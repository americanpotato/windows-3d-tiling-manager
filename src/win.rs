//! Win32 helpers: window enumeration, screenshots, icons, GDI card composition,
//! focusing windows and launching programs.

use std::ffi::c_void;
use std::mem::size_of;

use windows::Win32::Foundation::*;
use windows::Win32::Graphics::Dwm::*;
use windows::Win32::Graphics::Gdi::*;
use windows::Win32::Storage::FileSystem::FILE_FLAGS_AND_ATTRIBUTES;
use windows::Win32::Storage::Xps::{PRINT_WINDOW_FLAGS, PrintWindow};
use windows::Win32::System::Threading::*;
use windows::Win32::UI::Controls::IImageList;
use windows::Win32::UI::Shell::*;
use windows::Win32::UI::WindowsAndMessaging::*;
use windows::core::{BOOL, PCWSTR, PWSTR, w};

use crate::image::Image;

pub fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

/// Identity of a top-level window as far as the layout is concerned.
#[derive(Clone, Debug)]
pub struct WinInfo {
    pub hwnd: isize,
    pub exe: String,
    pub class: String,
    pub title: String,
    pub minimized: bool,
}

pub fn hwnd_of(h: isize) -> HWND {
    HWND(h as *mut c_void)
}

pub fn window_title(hwnd: HWND) -> String {
    let mut buf = [0u16; 512];
    let n = unsafe { GetWindowTextW(hwnd, &mut buf) };
    String::from_utf16_lossy(&buf[..n.max(0) as usize])
}

pub fn window_class(hwnd: HWND) -> String {
    let mut buf = [0u16; 256];
    let n = unsafe { GetClassNameW(hwnd, &mut buf) };
    String::from_utf16_lossy(&buf[..n.max(0) as usize])
}

pub fn window_pid(hwnd: HWND) -> u32 {
    let mut pid = 0u32;
    unsafe { GetWindowThreadProcessId(hwnd, Some(&mut pid)) };
    pid
}

/// Whether window `h` belongs to a process running with more rights than Window3D (one run as
/// administrator, like Task Manager). Windows keeps such windows out of reach: they can't be
/// moved under other windows, sent messages, and the mouse hook doesn't see clicks on them.
/// So is one of a process Window3D can't even open (a system one, or another app's elevated
/// helper): its tray menus couldn't be hidden either.
pub fn elevated(h: HWND) -> bool {
    use windows::Win32::Security::{GetTokenInformation, TOKEN_ELEVATION, TOKEN_QUERY, TokenElevation};
    fn is_elevated(p: HANDLE) -> Option<bool> {
        unsafe {
            let mut t = HANDLE::default();
            OpenProcessToken(p, TOKEN_QUERY, &mut t).ok()?;
            let mut e = TOKEN_ELEVATION::default();
            let mut len = 0u32;
            let ok = GetTokenInformation(
                t,
                TokenElevation,
                Some(&mut e as *mut TOKEN_ELEVATION as *mut c_void),
                size_of::<TOKEN_ELEVATION>() as u32,
                &mut len,
            )
            .is_ok();
            let _ = CloseHandle(t);
            ok.then_some(e.TokenIsElevated != 0)
        }
    }
    static CACHE: std::sync::Mutex<Vec<(u32, bool)>> = std::sync::Mutex::new(Vec::new());
    static ME: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    let pid = window_pid(h);
    if pid == 0 || *ME.get_or_init(|| is_elevated(unsafe { GetCurrentProcess() }).unwrap_or(false)) {
        return false;
    }
    let mut cache = CACHE.lock().unwrap_or_else(|e| e.into_inner());
    if let Some(&(_, e)) = cache.iter().find(|(p, _)| *p == pid) {
        return e;
    }
    let e = unsafe {
        match OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid) {
            Ok(p) => {
                let e = is_elevated(p);
                let _ = CloseHandle(p);
                e.unwrap_or(false)
            }
            Err(_) => true,
        }
    };
    if cache.len() > 256 {
        cache.clear();
    }
    cache.push((pid, e));
    e
}

pub fn process_path(pid: u32) -> String {
    unsafe {
        let Ok(h) = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid) else {
            return String::new();
        };
        let mut buf = [0u16; 1024];
        let mut len = buf.len() as u32;
        let ok =
            QueryFullProcessImageNameW(h, PROCESS_NAME_WIN32, PWSTR(buf.as_mut_ptr()), &mut len)
                .is_ok();
        let _ = CloseHandle(h);
        if ok {
            String::from_utf16_lossy(&buf[..len as usize])
        } else {
            String::new()
        }
    }
}

/// Asks `hwnd` to close (`WM_CLOSE`), or with `kill` ends its process (never Explorer's, which
/// is the shell, nor ours: those get the polite close). True once the window is gone, waiting
/// up to half a second.
pub fn close_window(hwnd: HWND, kill: bool) -> bool {
    let pid = window_pid(hwnd);
    let exe = process_path(pid).to_ascii_lowercase();
    let killable =
        kill && pid != 0 && pid != std::process::id() && !exe.ends_with("\\explorer.exe");
    unsafe {
        let mut done = false;
        if killable {
            if let Ok(p) = OpenProcess(PROCESS_TERMINATE, false, pid) {
                done = TerminateProcess(p, 1).is_ok();
                let _ = CloseHandle(p);
            }
        }
        if !done {
            let _ = PostMessageW(Some(hwnd), WM_CLOSE, WPARAM(0), LPARAM(0));
        }
        for _ in 0..25 {
            if !IsWindow(Some(hwnd)).as_bool() {
                return true;
            }
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        !IsWindow(Some(hwnd)).as_bool()
    }
}

fn is_cloaked(hwnd: HWND) -> bool {
    let mut cloaked = 0u32;
    unsafe {
        DwmGetWindowAttribute(
            hwnd,
            DWMWA_CLOAKED,
            &mut cloaked as *mut u32 as *mut c_void,
            4,
        )
        .is_ok()
            && cloaked != 0
    }
}

/// Windows that stayed over the 3D view after [`raise_over`] put it on top (in a band of the
/// shell's above every app): not tried again.
static OVER_BAND: std::sync::Mutex<Vec<isize>> = std::sync::Mutex::new(Vec::new());

/// The other apps' windows over the 3D view `overlay` that it could go over: visible, not
/// cloaked, overlapping it, not the shell's (Start, search, the taskbar and the flyouts are in
/// bands above every app), not given up on (`OVER_BAND`).
fn over(overlay: HWND) -> Vec<HWND> {
    let mut out = Vec::new();
    unsafe {
        let mut r = RECT::default();
        if GetWindowRect(overlay, &mut r).is_err() {
            return out;
        }
        let me = GetCurrentProcessId();
        let band = OVER_BAND.lock().unwrap_or_else(|e| e.into_inner()).clone();
        let mut w = GetWindow(overlay, GW_HWNDPREV).ok();
        while let Some(h) = w {
            w = GetWindow(h, GW_HWNDPREV).ok();
            let mut o = RECT::default();
            if IsWindowVisible(h).as_bool()
                && !is_cloaked(h)
                && window_pid(h) != me
                && !band.contains(&(h.0 as isize))
                && !is_shell_popup(h)
                && !matches!(window_class(h).as_str(), "Shell_TrayWnd" | "Shell_SecondaryTrayWnd")
                && GetWindowRect(h, &mut o).is_ok()
                && o.left < r.right
                && r.left < o.right
                && o.top < r.bottom
                && r.top < o.bottom
            {
                out.push(h);
            }
        }
    }
    out
}

/// Using the desktop card in place, windows that come up over the 3D view (tray and context
/// menus, tooltips, Task Manager and other "always on top" windows) are put back under it: the
/// 3D view is left out of the screen capture (see [`no_capture`]), so they still show on the
/// desktop card, just not twice. Our own windows over it (the pointer) go back over it. Windows
/// still over it afterwards are in a band above every app and are given up on. Returns the
/// windows it went over, for the log.
pub fn raise_over(overlay: isize) -> Vec<String> {
    let overlay = hwnd_of(overlay);
    let above = over(overlay);
    if above.is_empty() {
        return Vec::new();
    }
    unsafe {
        let me = GetCurrentProcessId();
        let mut mine = Vec::new();
        let mut w = GetWindow(overlay, GW_HWNDPREV).ok();
        while let Some(h) = w {
            w = GetWindow(h, GW_HWNDPREV).ok();
            if window_pid(h) == me && IsWindowVisible(h).as_bool() {
                mine.push(h);
            }
        }
        let flags = SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE;
        let _ = SetWindowPos(overlay, Some(HWND_TOPMOST), 0, 0, 0, 0, flags);
        // Bottom to top, each to the top: in the order they were.
        for h in mine {
            let _ = SetWindowPos(h, Some(HWND_TOPMOST), 0, 0, 0, 0, flags);
        }
    }
    let stuck = over(overlay);
    let mut out = Vec::new();
    for h in above {
        if stuck.contains(&h) {
            OVER_BAND.lock().unwrap_or_else(|e| e.into_inner()).push(h.0 as isize);
            out.push(format!("{} (stays over it, given up on)", describe(h)));
        } else {
            out.push(describe(h));
        }
    }
    out
}

/// Whether `hwnd` is a Chromium window (Chrome, Edge, Electron apps) that other windows fully
/// cover, judged the way Chromium's own occlusion tracking does. Chromium then stops painting it
/// to save power, so its live card shows black or freezes. Window3D's overlay doesn't count (it's
/// a tool window, which Chromium ignores).
pub fn chromium_occluded(hwnd: isize) -> bool {
    let hwnd = HWND(hwnd as *mut c_void);
    if window_class(hwnd) != "Chrome_WidgetWin_1" {
        return false;
    }
    unsafe {
        let mut r = RECT::default();
        if GetWindowRect(hwnd, &mut r).is_err() || IsIconic(hwnd).as_bool() {
            return false;
        }
        let left = CreateRectRgn(r.left, r.top, r.right, r.bottom);
        let mut covered = false;
        let mut w = GetWindow(hwnd, GW_HWNDPREV).ok();
        while let Some(h) = w {
            let mut o = RECT::default();
            if chromium_opaque(h) && GetWindowRect(h, &mut o).is_ok() {
                let rgn = CreateRectRgn(o.left, o.top, o.right, o.bottom);
                let kind = CombineRgn(Some(left), Some(left), Some(rgn), RGN_DIFF);
                let _ = DeleteObject(rgn.into());
                if kind == NULLREGION {
                    covered = true;
                    break;
                }
            }
            w = GetWindow(h, GW_HWNDPREV).ok();
        }
        let _ = DeleteObject(left.into());
        covered
    }
}

/// Whether Chromium's occlusion tracking counts window `h` as an opaque cover.
fn chromium_opaque(h: HWND) -> bool {
    unsafe {
        let ex = GetWindowLongW(h, GWL_EXSTYLE) as u32;
        let probe = CreateRectRgn(0, 0, 0, 0);
        let shaped = GetWindowRgn(h, probe) == COMPLEXREGION;
        let _ = DeleteObject(probe.into());
        IsWindowVisible(h).as_bool()
            && !IsIconic(h).as_bool()
            && !is_cloaked(h)
            && !shaped
            && ex & WS_EX_TRANSPARENT.0 == 0
            && (ex & WS_EX_TOOLWINDOW.0 == 0 || window_class(h) == "Shell_TrayWnd")
            && ex & WS_EX_LAYERED.0 == 0
    }
}

/// Makes a covered Chromium window (see [`chromium_occluded`]) draw again without moving
/// anything: every window over it gets a window region with a one-pixel hole in its top-left
/// corner. Chromium doesn't count windows with such a shaped region as covers, so it marks the
/// window visible again. The region is far larger than the window, so it clips nothing else
/// even if the window grows. Windows that already have a region, the taskbar and our own are
/// left alone. A maximized window loses such a region at once: it is marked a tool window
/// instead (with `WS_EX_APPWINDOW`, so it keeps its taskbar button and Alt+Tab entry), which
/// Chromium doesn't count as a cover either. Returns the windows changed; give them back with
/// [`unshape`].
pub fn uncover_chromium(hwnd: isize) -> Vec<isize> {
    let mut done = Vec::new();
    if !chromium_occluded(hwnd) {
        return done;
    }
    let hwnd = HWND(hwnd as *mut c_void);
    unsafe {
        let mut r = RECT::default();
        let _ = GetWindowRect(hwnd, &mut r);
        let mut w = GetWindow(hwnd, GW_HWNDPREV).ok();
        while let Some(h) = w {
            w = GetWindow(h, GW_HWNDPREV).ok();
            let mut o = RECT::default();
            let overlaps = GetWindowRect(h, &mut o).is_ok()
                && o.left < r.right
                && r.left < o.right
                && o.top < r.bottom
                && r.top < o.bottom;
            if !overlaps
                || !chromium_opaque(h)
                || window_pid(h) == GetCurrentProcessId()
                || window_class(h) == "Shell_TrayWnd"
            {
                continue;
            }
            if IsZoomed(h).as_bool() {
                done.extend(make_tool(h));
                continue;
            }
            let probe = CreateRectRgn(0, 0, 0, 0);
            let has_region = GetWindowRgn(h, probe) != GDI_REGION_TYPE(0);
            let _ = DeleteObject(probe.into());
            if has_region {
                continue;
            }
            let rgn = CreateRectRgn(0, 0, 0x7fff, 0x7fff);
            let hole = CreateRectRgn(0, 0, 1, 1);
            let _ = CombineRgn(Some(rgn), Some(rgn), Some(hole), RGN_DIFF);
            let _ = DeleteObject(hole.into());
            // The system owns the region once it is set.
            if SetWindowRgn(h, Some(rgn), false) != 0 {
                done.push(h.0 as isize);
            } else {
                let _ = DeleteObject(rgn.into());
            }
        }
    }
    done
}

/// The windows Chromium counts as covering `hwnd` (class, title, has a region), for the self-test.
pub fn chromium_covers(hwnd: isize) -> Vec<String> {
    let mut out = Vec::new();
    unsafe {
        let hwnd = HWND(hwnd as *mut c_void);
        let mut r = RECT::default();
        let _ = GetWindowRect(hwnd, &mut r);
        let mut w = GetWindow(hwnd, GW_HWNDPREV).ok();
        while let Some(h) = w {
            w = GetWindow(h, GW_HWNDPREV).ok();
            let mut o = RECT::default();
            if chromium_opaque(h)
                && GetWindowRect(h, &mut o).is_ok()
                && o.left < r.right
                && r.left < o.right
                && o.top < r.bottom
                && r.top < o.bottom
            {
                let probe = CreateRectRgn(0, 0, 0, 0);
                let kind = GetWindowRgn(h, probe).0;
                let _ = DeleteObject(probe.into());
                out.push(format!("{}|{}|rgn{kind}|pid{}", window_class(h), window_title(h), window_pid(h)));
            }
        }
    }
    out
}

/// Marks maximized window `h` a tool window (see [`uncover_chromium`]). Returns it if changed.
unsafe fn make_tool(h: HWND) -> Option<isize> {
    unsafe {
        let ex = GetWindowLongPtrW(h, GWL_EXSTYLE) as u32;
        let add = WS_EX_TOOLWINDOW.0 | WS_EX_APPWINDOW.0;
        SetWindowLongPtrW(h, GWL_EXSTYLE, (ex | add) as isize);
        let got = GetWindowLongPtrW(h, GWL_EXSTYLE) as u32;
        if got & WS_EX_TOOLWINDOW.0 == 0 {
            return None;
        }
        TOOLED
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .push((h.0 as isize, add & !ex));
        // Chromium looks again after a window move ends.
        windows::Win32::UI::Accessibility::NotifyWinEvent(
            EVENT_SYSTEM_MOVESIZEEND,
            h,
            OBJID_WINDOW.0,
            CHILDID_SELF as i32,
        );
        Some(h.0 as isize)
    }
}

/// Ahead of [`uncover_chromium`]: a maximized live window that overlaps another live Chromium
/// window is marked a tool window before it ever covers it (using it in place brings it to
/// the top), since Chromium may take seconds to notice a cover is gone. Returns the windows
/// changed; give them back with [`unshape`].
pub fn pre_uncover_chromium(live: &[isize]) -> Vec<isize> {
    let mut done = Vec::new();
    let rect = |h: HWND| {
        let mut r = RECT::default();
        unsafe { GetWindowRect(h, &mut r) }.ok().map(|_| r)
    };
    for &a in live {
        let h = HWND(a as *mut c_void);
        if !unsafe { IsZoomed(h) }.as_bool()
            || tooled(h)
            || !chromium_opaque(h)
            || window_pid(h) == unsafe { GetCurrentProcessId() }
        {
            continue;
        }
        let Some(r) = rect(h) else { continue };
        let overlaps = live.iter().any(|&b| {
            let c = HWND(b as *mut c_void);
            b != a
                && window_class(c) == "Chrome_WidgetWin_1"
                && !unsafe { IsIconic(c) }.as_bool()
                && rect(c).is_some_and(|o| {
                    o.left < r.right && r.left < o.right && o.top < r.bottom && r.top < o.bottom
                })
        });
        if overlaps {
            done.extend(unsafe { make_tool(h) });
        }
    }
    done
}

/// Windows [`uncover_chromium`] made tool windows, with the extended style bits it added.
static TOOLED: std::sync::Mutex<Vec<(isize, u32)>> = std::sync::Mutex::new(Vec::new());

/// Whether `h` is a tool window only because [`uncover_chromium`] made it one.
pub fn tooled(h: HWND) -> bool {
    TOOLED
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .iter()
        .any(|t| t.0 == h.0 as isize)
}

/// Removes the regions and tool window styles [`uncover_chromium`] gave these windows.
pub fn unshape(windows: &[isize]) {
    if windows.is_empty() {
        return;
    }
    // SetWindowRgn and SetWindowLongPtrW wait for each (other process's) window to handle the change: off the UI
    // thread, so hiding the overlay never waits for a busy browser.
    let windows = windows.to_vec();
    let t = std::thread::spawn(move || {
        for h in windows {
            let h = HWND(h as *mut c_void);
            let tool = {
                let mut t = TOOLED.lock().unwrap_or_else(|e| e.into_inner());
                t.iter().position(|t| t.0 == h.0 as isize).map(|i| t.remove(i).1)
            };
            if !unsafe { IsWindow(Some(h)) }.as_bool() {
                continue;
            }
            match tool {
                Some(added) => unsafe {
                    let ex = GetWindowLongPtrW(h, GWL_EXSTYLE) as u32;
                    SetWindowLongPtrW(h, GWL_EXSTYLE, (ex & !added) as isize);
                },
                None => unsafe {
                    SetWindowRgn(h, None, true);
                },
            }
        }
    });
    UNSHAPING.lock().unwrap_or_else(|e| e.into_inner()).push(t);
}

static UNSHAPING: std::sync::Mutex<Vec<std::thread::JoinHandle<()>>> =
    std::sync::Mutex::new(Vec::new());

/// Waits for `unshape` to finish (before exiting, so no window keeps its hole).
pub fn unshape_wait() {
    let all = std::mem::take(&mut *UNSHAPING.lock().unwrap_or_else(|e| e.into_inner()));
    for t in all {
        let _ = t.join();
    }
}

const SKIP_CLASSES: &[&str] = &[
    "Progman",
    "WorkerW",
    "Shell_TrayWnd",
    "Shell_SecondaryTrayWnd",
    "Windows.UI.Core.CoreWindow",
    crate::OVERLAY_CLASS,
    crate::MSG_CLASS,
];

/// Would this window appear in Alt+Tab? (visible, unowned, not a tool window, not cloaked, has a title)
pub fn is_switchable(hwnd: HWND) -> bool {
    unsafe {
        if hwnd.0.is_null() || !IsWindow(Some(hwnd)).as_bool() || !IsWindowVisible(hwnd).as_bool() {
            return false;
        }
        if GetWindow(hwnd, GW_OWNER)
            .map(|o| !o.0.is_null())
            .unwrap_or(false)
        {
            return false;
        }
        let ex = GetWindowLongPtrW(hwnd, GWL_EXSTYLE) as u32;
        if ex & WS_EX_TOOLWINDOW.0 != 0 && ex & WS_EX_APPWINDOW.0 == 0 {
            return false;
        }
        if ex & WS_EX_NOACTIVATE.0 != 0 && ex & WS_EX_APPWINDOW.0 == 0 {
            return false;
        }
        if is_cloaked(hwnd) {
            return false;
        }
    }
    if window_pid(hwnd) == std::process::id() {
        return false;
    }
    if window_title(hwnd).trim().is_empty() {
        return false;
    }
    let class = window_class(hwnd);
    !SKIP_CLASSES.contains(&class.as_str())
}

/// UWP apps are hosted by ApplicationFrameHost; the real app lives in a child CoreWindow.
fn uwp_real_exe(hwnd: HWND, host_pid: u32) -> Option<String> {
    struct Ctx {
        host: u32,
        found: Option<u32>,
    }
    unsafe extern "system" fn cb(child: HWND, lp: LPARAM) -> BOOL {
        let ctx = unsafe { &mut *(lp.0 as *mut Ctx) };
        let pid = window_pid(child);
        if pid != ctx.host {
            ctx.found = Some(pid);
            return FALSE;
        }
        TRUE
    }
    let mut ctx = Ctx {
        host: host_pid,
        found: None,
    };
    unsafe {
        let _ = EnumChildWindows(Some(hwnd), Some(cb), LPARAM(&mut ctx as *mut Ctx as isize));
    }
    ctx.found.map(process_path).filter(|s| !s.is_empty())
}

/// True for a program listed in `focus_apps` (never used in place).
pub fn is_focus_app(info: &WinInfo, focus_apps: &[String]) -> bool {
    let exe = std::path::Path::new(&info.exe)
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    focus_apps.iter().any(|a| a.eq_ignore_ascii_case(&exe))
}

/// Makes the overlay let real mouse input through to whatever is under it (`on`), still drawn
/// as before (layered at full opacity), or takes it back. Used while a window used in place holds
/// the mouse (mouse-look): it has the real focus and reads the real mouse as in normal use.
pub fn click_through(overlay: isize, on: bool) {
    let h = hwnd_of(overlay);
    unsafe {
        let ex = GetWindowLongPtrW(h, GWL_EXSTYLE);
        let bits = (WS_EX_LAYERED.0 | WS_EX_TRANSPARENT.0) as isize;
        if on {
            SetWindowRgn(h, None, false);
            SetWindowLongPtrW(h, GWL_EXSTYLE, ex | bits);
            let _ = SetLayeredWindowAttributes(h, COLORREF(0), 255, LWA_ALPHA);
        } else {
            SetWindowLongPtrW(h, GWL_EXSTYLE, ex & !bits);
        }
    }
}

pub fn win_info(hwnd: HWND) -> WinInfo {
    let pid = window_pid(hwnd);
    let mut exe = process_path(pid);
    if exe
        .to_ascii_lowercase()
        .ends_with("\\applicationframehost.exe")
    {
        if let Some(real) = uwp_real_exe(hwnd, pid) {
            exe = real;
        }
    }
    WinInfo {
        hwnd: hwnd.0 as isize,
        exe,
        class: window_class(hwnd),
        title: window_title(hwnd),
        minimized: unsafe { IsIconic(hwnd).as_bool() },
    }
}

/// All Alt+Tab-style windows, in z-order (topmost first).
pub fn enumerate_windows() -> Vec<WinInfo> {
    unsafe extern "system" fn cb(hwnd: HWND, lp: LPARAM) -> BOOL {
        let v = unsafe { &mut *(lp.0 as *mut Vec<HWND>) };
        if is_switchable(hwnd) {
            v.push(hwnd);
        }
        TRUE
    }
    let mut hwnds: Vec<HWND> = Vec::new();
    unsafe {
        let _ = EnumWindows(Some(cb), LPARAM(&mut hwnds as *mut Vec<HWND> as isize));
    }
    hwnds.into_iter().map(win_info).collect()
}

// --- GDI memory bitmaps ----------------------------------------------------

/// A top-down 32-bit DIB section selected into a memory DC.
pub struct Dib {
    pub dc: HDC,
    bmp: HBITMAP,
    old: HGDIOBJ,
    bits: *mut u8,
    pub w: i32,
    pub h: i32,
}

impl Dib {
    pub fn new(w: i32, h: i32) -> Option<Dib> {
        unsafe {
            let screen = GetDC(None);
            let dc = CreateCompatibleDC(Some(screen));
            ReleaseDC(None, screen);
            if dc.is_invalid() {
                return None;
            }
            let bmi = BITMAPINFO {
                bmiHeader: BITMAPINFOHEADER {
                    biSize: size_of::<BITMAPINFOHEADER>() as u32,
                    biWidth: w,
                    biHeight: -h,
                    biPlanes: 1,
                    biBitCount: 32,
                    biCompression: BI_RGB.0,
                    ..Default::default()
                },
                ..Default::default()
            };
            let mut bits: *mut c_void = std::ptr::null_mut();
            let Ok(bmp) = CreateDIBSection(Some(dc), &bmi, DIB_RGB_COLORS, &mut bits, None, 0)
            else {
                let _ = DeleteDC(dc);
                return None;
            };
            if bits.is_null() {
                let _ = DeleteObject(bmp.into());
                let _ = DeleteDC(dc);
                return None;
            }
            let old = SelectObject(dc, bmp.into());
            Some(Dib {
                dc,
                bmp,
                old,
                bits: bits as *mut u8,
                w,
                h,
            })
        }
    }

    pub fn pixels(&mut self) -> &mut [u8] {
        unsafe {
            let _ = GdiFlush();
            std::slice::from_raw_parts_mut(self.bits, (self.w * self.h * 4) as usize)
        }
    }

    pub fn to_image(mut self) -> Image {
        let (w, h) = (self.w as u32, self.h as u32);
        Image {
            w,
            h,
            px: self.pixels().to_vec(),
        }
    }

    pub fn fill(&self, x: i32, y: i32, w: i32, h: i32, rgb: (u8, u8, u8)) {
        unsafe {
            let brush = CreateSolidBrush(COLORREF(
                rgb.0 as u32 | (rgb.1 as u32) << 8 | (rgb.2 as u32) << 16,
            ));
            let r = RECT {
                left: x,
                top: y,
                right: x + w,
                bottom: y + h,
            };
            FillRect(self.dc, &r, brush);
            let _ = DeleteObject(brush.into());
        }
    }

    /// Draws text into `r` with an anti-aliased Segoe UI font of `px` pixels.
    pub fn text(
        &self,
        s: &str,
        r: RECT,
        px: i32,
        bold: bool,
        rgb: (u8, u8, u8),
        flags: DRAW_TEXT_FORMAT,
    ) {
        self.text_in(s, r, px, bold, rgb, flags, w!("Segoe UI"));
    }

    /// Like `text`, in font `face`.
    #[allow(clippy::too_many_arguments)]
    pub fn text_in(
        &self,
        s: &str,
        r: RECT,
        px: i32,
        bold: bool,
        rgb: (u8, u8, u8),
        flags: DRAW_TEXT_FORMAT,
        face: PCWSTR,
    ) {
        // DrawTextW reads through the (dangling) pointer of an empty buffer.
        if s.is_empty() {
            return;
        }
        unsafe {
            let font = CreateFontW(
                -px,
                0,
                0,
                0,
                if bold { 600 } else { 400 },
                0,
                0,
                0,
                DEFAULT_CHARSET,
                OUT_DEFAULT_PRECIS,
                CLIP_DEFAULT_PRECIS,
                ANTIALIASED_QUALITY,
                (DEFAULT_PITCH.0 | FF_SWISS.0) as u32,
                face,
            );
            let old = SelectObject(self.dc, font.into());
            SetBkMode(self.dc, TRANSPARENT);
            SetTextColor(
                self.dc,
                COLORREF(rgb.0 as u32 | (rgb.1 as u32) << 8 | (rgb.2 as u32) << 16),
            );
            let mut buf: Vec<u16> = s.encode_utf16().collect();
            let mut r = r;
            DrawTextW(self.dc, &mut buf, &mut r, flags | DT_NOPREFIX);
            SelectObject(self.dc, old);
            let _ = DeleteObject(font.into());
        }
    }

    pub fn icon(&self, icon: HICON, x: i32, y: i32, size: i32) {
        unsafe {
            let _ = DrawIconEx(self.dc, x, y, icon, size, size, 0, None, DI_NORMAL);
        }
    }
}

impl Drop for Dib {
    fn drop(&mut self) {
        unsafe {
            SelectObject(self.dc, self.old);
            let _ = DeleteObject(self.bmp.into());
            let _ = DeleteDC(self.dc);
        }
    }
}

// --- Screenshots -----------------------------------------------------------

/// Captures a window's own pixels (works while occluded, via DWM) and scales it so that
/// its longest side is at most `max_side`. Returns None for minimized/hung/black windows.
pub fn capture_window(hwnd: HWND, max_side: u32) -> Option<Image> {
    unsafe {
        if !IsWindow(Some(hwnd)).as_bool()
            || IsIconic(hwnd).as_bool()
            || IsHungAppWindow(hwnd).as_bool()
        {
            return None;
        }
        let mut wr = RECT::default();
        GetWindowRect(hwnd, &mut wr).ok()?;
        // Crop the invisible resize borders by using the visible frame bounds.
        let mut fr = RECT::default();
        let frame = DwmGetWindowAttribute(
            hwnd,
            DWMWA_EXTENDED_FRAME_BOUNDS,
            &mut fr as *mut RECT as *mut c_void,
            size_of::<RECT>() as u32,
        )
        .is_ok();
        if !frame || fr.right <= fr.left {
            fr = wr;
        }
        // The desktop's card is the whole primary screen, as it is (windows, taskbar, popups);
        // the 3D view is left out of screen captures meanwhile (`snip.rs`).
        let desktop = is_desktop(hwnd);
        if desktop {
            fr = primary_rect()?;
            wr = fr;
        }
        let (ww, wh) = (wr.right - wr.left, wr.bottom - wr.top);
        if ww < 16 || wh < 16 || ww > 16384 || wh > 16384 {
            return None;
        }
        let full = Dib::new(ww, wh)?;
        if desktop {
            // Left out of captures meanwhile; given a frame to take effect.
            if crate::snip::desktop_capture(true) {
                std::thread::sleep(std::time::Duration::from_millis(40));
            }
            let screen = GetDC(None);
            let ok = BitBlt(full.dc, 0, 0, ww, wh, Some(screen), wr.left, wr.top, SRCCOPY | CAPTUREBLT).is_ok();
            ReleaseDC(None, screen);
            let _ = crate::snip::desktop_capture(false);
            if !ok {
                return None;
            }
        // PW_RENDERFULLCONTENT (2) captures DirectX/accelerated content too.
        } else if !PrintWindow(hwnd, full.dc, PRINT_WINDOW_FLAGS(2)).as_bool() {
            return None;
        }
        let (cx, cy) = (
            (fr.left - wr.left).clamp(0, ww - 1),
            (fr.top - wr.top).clamp(0, wh - 1),
        );
        let cw = (fr.right - fr.left).min(ww - cx).max(1);
        let ch = (fr.bottom - fr.top).min(wh - cy).max(1);
        let scale = (max_side as f32 / cw.max(ch) as f32).min(1.0);
        let (tw, th) = (
            ((cw as f32 * scale).round() as i32).max(1),
            ((ch as f32 * scale).round() as i32).max(1),
        );
        let small = Dib::new(tw, th)?;
        SetStretchBltMode(small.dc, HALFTONE);
        let _ = SetBrushOrgEx(small.dc, 0, 0, None);
        let _ = StretchBlt(
            small.dc,
            0,
            0,
            tw,
            th,
            Some(full.dc),
            cx,
            cy,
            cw,
            ch,
            SRCCOPY,
        );
        drop(full);
        let mut img = small.to_image();
        img.set_opaque();
        // A fully black capture means the app refused to render (e.g. protected content); keep the old shot.
        let dark = img
            .px
            .chunks_exact(4)
            .step_by(7)
            .filter(|p| p[0] < 4 && p[1] < 4 && p[2] < 4)
            .count();
        let total = img.px.len() / 4 / 7 + 1;
        if dark * 100 > total * 98 {
            return None;
        }
        Some(img)
    }
}

// --- Icons -----------------------------------------------------------------

pub struct Icon(pub HICON);

impl Drop for Icon {
    fn drop(&mut self) {
        unsafe {
            let _ = DestroyIcon(self.0);
        }
    }
}

/// Large shell icon for a file (exe, lnk, url...). Picks the 256px "jumbo" icon unless the
/// file only provides a small one (in which case jumbo is a tiny icon in a big empty square).
pub fn file_icon(path: &str) -> Option<Icon> {
    if path.is_empty() {
        return None;
    }
    unsafe {
        let wpath = wide(path);
        let mut sfi = SHFILEINFOW::default();
        let r = SHGetFileInfoW(
            PCWSTR(wpath.as_ptr()),
            FILE_FLAGS_AND_ATTRIBUTES(0),
            Some(&mut sfi),
            size_of::<SHFILEINFOW>() as u32,
            SHGFI_SYSICONINDEX,
        );
        if r == 0 {
            return None;
        }
        let idx = sfi.iIcon;
        if let Ok(list) = SHGetImageList::<IImageList>(SHIL_JUMBO as i32) {
            if let Ok(h) = list.GetIcon(idx, 0x1 /* ILD_TRANSPARENT */) {
                let icon = Icon(h);
                if !icon_is_small_in_jumbo(&icon) {
                    return Some(icon);
                }
            }
        }
        let list = SHGetImageList::<IImageList>(SHIL_EXTRALARGE as i32).ok()?;
        list.GetIcon(idx, 0x1).ok().map(Icon)
    }
}

fn icon_is_small_in_jumbo(icon: &Icon) -> bool {
    let Some(mut dib) = Dib::new(256, 256) else {
        return false;
    };
    dib.icon(icon.0, 0, 0, 256);
    let px = dib.pixels();
    for y in 0..256usize {
        for x in 0..256usize {
            if (x >= 64 || y >= 64) && px[(y * 256 + x) * 4 + 3] != 0 {
                return false;
            }
            if (x >= 64 || y >= 64)
                && (px[(y * 256 + x) * 4] | px[(y * 256 + x) * 4 + 1] | px[(y * 256 + x) * 4 + 2])
                    != 0
            {
                return false;
            }
        }
    }
    true
}

// --- Card textures ----------------------------------------------------------

const STRIP_BG: (u8, u8, u8) = (34, 38, 50);
const BODY_BG: (u8, u8, u8) = (20, 23, 32);

/// Window cards have a title strip (`card_titles` in the config).
pub static CARD_TITLES: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// Height of the title strip above a window card's picture `w` pixels wide (0 without titles).
pub fn card_strip(w: i32) -> i32 {
    if CARD_TITLES.load(std::sync::atomic::Ordering::Relaxed) {
        (w / 18).clamp(26, 44)
    } else {
        0
    }
}

/// A window card: title strip with icon on top (`card_titles`), screenshot (or a big icon
/// placeholder) below.
pub fn compose_window_card(title: &str, exe: &str, shot: Option<&Image>, ghost: bool) -> Image {
    let (bw, bh) = shot.map(|s| (s.w as i32, s.h as i32)).unwrap_or((640, 400));
    let strip = card_strip(bw);
    let (w, h) = (bw, bh + strip);
    let Some(mut dib) = Dib::new(w, h) else {
        return Image::new(4, 4);
    };
    let icon = file_icon(exe);
    if strip > 0 {
        dib.fill(0, 0, w, strip, STRIP_BG);
        let isz = strip - 8;
        if let Some(i) = &icon {
            dib.icon(i.0, 6, 4, isz);
        }
        let label = if ghost {
            format!("{title}   \u{2014}   closed \u{00b7} click to relaunch")
        } else {
            title.to_string()
        };
        dib.text(
            &label,
            RECT {
                left: isz + 14,
                top: 0,
                right: w - 8,
                bottom: strip,
            },
            (strip as f32 * 0.52) as i32,
            false,
            (235, 238, 245),
            DT_SINGLELINE | DT_VCENTER | DT_END_ELLIPSIS,
        );
    }
    match shot {
        Some(s) => {
            let px = dib.pixels();
            let off = (strip * w * 4) as usize;
            px[off..off + s.px.len()].copy_from_slice(&s.px);
        }
        None => {
            dib.fill(0, strip, w, bh, BODY_BG);
            let size = (bh / 3).min(256);
            match &icon {
                Some(i) => dib.icon(i.0, (w - size) / 2, strip + (bh - size) / 2, size),
                None => dib.text(
                    "no preview yet",
                    RECT {
                        left: 0,
                        top: strip,
                        right: w,
                        bottom: h,
                    },
                    28,
                    false,
                    (130, 136, 150),
                    DT_SINGLELINE | DT_VCENTER | DT_CENTER,
                ),
            }
        }
    }
    let mut img = dib.to_image();
    img.set_opaque();
    img
}

/// A launcher card: big icon with the shortcut name underneath.
pub fn compose_shortcut_card(name: &str, path: &str) -> Image {
    let (w, h) = (256, 300);
    let Some(dib) = Dib::new(w, h) else {
        return Image::new(4, 4);
    };
    dib.fill(0, 0, w, h, (28, 32, 44));
    dib.fill(0, 0, w, 4, (90, 140, 255));
    if let Some(i) = file_icon(path) {
        dib.icon(i.0, (w - 160) / 2, 30, 160);
    }
    dib.text(
        name,
        RECT {
            left: 10,
            top: 204,
            right: w - 10,
            bottom: h - 8,
        },
        26,
        true,
        (235, 238, 245),
        DT_CENTER | DT_WORDBREAK | DT_END_ELLIPSIS,
    );
    let mut img = dib.to_image();
    img.set_opaque();
    img
}

/// White anti-aliased text on a translucent dark pill, for the heads-up display.
pub fn compose_hud_text(lines: &[&str], px: i32) -> Image {
    let line_h = (px as f32 * 1.45) as i32;
    let pad = px;
    let mut w = 0;
    // Measure the widest line.
    if let Some(dib) = Dib::new(4, 4) {
        for l in lines.iter().filter(|l| !l.is_empty()) {
            let mut r = RECT {
                left: 0,
                top: 0,
                right: 4000,
                bottom: 200,
            };
            unsafe {
                let font = CreateFontW(
                    -px,
                    0,
                    0,
                    0,
                    400,
                    0,
                    0,
                    0,
                    DEFAULT_CHARSET,
                    OUT_DEFAULT_PRECIS,
                    CLIP_DEFAULT_PRECIS,
                    ANTIALIASED_QUALITY,
                    (DEFAULT_PITCH.0 | FF_SWISS.0) as u32,
                    w!("Segoe UI"),
                );
                let old = SelectObject(dib.dc, font.into());
                let mut buf: Vec<u16> = l.encode_utf16().collect();
                DrawTextW(
                    dib.dc,
                    &mut buf,
                    &mut r,
                    DT_CALCRECT | DT_SINGLELINE | DT_NOPREFIX,
                );
                SelectObject(dib.dc, old);
                let _ = DeleteObject(font.into());
            }
            w = w.max(r.right);
        }
    }
    let (w, h) = (w + pad * 2, line_h * lines.len() as i32 + pad);
    let Some(dib) = Dib::new(w.max(4), h.max(4)) else {
        return Image::new(4, 4);
    };
    for (i, l) in lines.iter().enumerate() {
        let top = pad / 2 + i as i32 * line_h;
        dib.text(
            l,
            RECT {
                left: pad,
                top,
                right: w,
                bottom: top + line_h,
            },
            px,
            false,
            (255, 255, 255),
            DT_SINGLELINE | DT_VCENTER,
        );
    }
    let mut img = dib.to_image();
    for p in img.px.chunks_exact_mut(4) {
        let cov = p[0].max(p[1]).max(p[2]);
        p[0] = cov;
        p[1] = cov;
        p[2] = cov;
        p[3] = cov.max(150);
    }
    img
}

/// A square toolbar button: a "Segoe MDL2 Assets" glyph on a dark (or, when `on`, accent) tile.
pub fn compose_icon(glyph: char, on: bool) -> Image {
    let n = 64;
    let Some(dib) = Dib::new(n, n) else {
        return Image::new(4, 4);
    };
    dib.fill(0, 0, n, n, if on { (36, 112, 196) } else { (28, 32, 42) });
    let r = RECT {
        left: 0,
        top: 0,
        right: n,
        bottom: n,
    };
    dib.text_in(
        &glyph.to_string(),
        r,
        n * 9 / 16,
        false,
        (255, 255, 255),
        DT_SINGLELINE | DT_VCENTER | DT_CENTER,
        w!("Segoe MDL2 Assets"),
    );
    let mut img = dib.to_image();
    for p in img.px.chunks_exact_mut(4) {
        p[3] = 255;
    }
    img
}

// --- Focus & launch --------------------------------------------------------

/// How a window is sized when it is focused from the overlay (`focus_mode`).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum FocusMode {
    /// Leave the window as it is.
    Keep,
    /// Maximize it (if it can be maximized).
    Maximize,
    /// Make it fill its monitor without a frame, falling back to maximizing.
    Borderless,
}

impl FocusMode {
    pub fn parse(s: &str) -> FocusMode {
        match s.trim().to_ascii_lowercase().as_str() {
            "maximize" | "maximized" => FocusMode::Maximize,
            "borderless" | "fullscreen" => FocusMode::Borderless,
            _ => FocusMode::Keep,
        }
    }
}

/// The frame of a window made borderless, so it can be put back.
#[derive(Clone, Copy)]
pub struct SavedFrame {
    style: isize,
    ex_style: isize,
    placement: WINDOWPLACEMENT,
}

/// Window classes whose frame is drawn by someone else (UWP hosts) or that handle frame
/// changes badly: these are only ever maximized.
const NO_BORDERLESS_CLASSES: &[&str] = &[
    "ApplicationFrameWindow",
    "Windows.UI.Core.CoreWindow",
    "CASCADIA_HOSTING_WINDOW_CLASS",
];

/// Whether `hwnd` is the desktop (Progman), whose card shows the primary monitor only.
pub fn is_desktop(hwnd: HWND) -> bool {
    window_class(hwnd) == "Progman"
}

/// The primary monitor's rectangle on the virtual screen.
/// Leaves window `h` out of every screen capture (it still shows on the monitor), so the
/// desktop card's capture of the whole screen shows what is under the 3D view, not the view.
/// Leaves window `h` out of screen captures while the desktop card captures the monitor
/// (see `snip.rs`).
pub fn no_capture(h: isize, what: &'static str) {
    crate::snip::register(h, what);
}

pub fn primary_rect() -> Option<RECT> {
    unsafe {
        let mon = MonitorFromPoint(POINT::default(), MONITOR_DEFAULTTOPRIMARY);
        let mut mi = MONITORINFO {
            cbSize: size_of::<MONITORINFO>() as u32,
            ..Default::default()
        };
        GetMonitorInfoW(mon, &mut mi)
            .as_bool()
            .then_some(mi.rcMonitor)
    }
}

/// The shell's popups showing on the primary monitor, topmost first: Start, search, the
/// taskbar's hover previews, menus and flyouts (not the taskbar or the desktop themselves).
/// Each comes with its rectangle on the screen.
pub fn shell_popups() -> Vec<(isize, RECT)> {
    unsafe extern "system" fn each(h: HWND, l: LPARAM) -> BOOL {
        let out = unsafe { &mut *(l.0 as *mut Vec<(isize, RECT)>) };
        let class = window_class(h);
        let mut r = RECT::default();
        if unsafe { IsWindowVisible(h).as_bool() }
            && !is_cloaked(h)
            && shell_window(h, &class)
            && unsafe { GetWindowRect(h, &mut r).is_ok() }
            && r.right > r.left
            && r.bottom > r.top
        {
            out.push((h.0 as isize, r));
        }
        true.into()
    }
    let mut out: Vec<(isize, RECT)> = Vec::new();
    let Some(m) = primary_rect() else {
        return out;
    };
    for_each_top(each, LPARAM(&mut out as *mut _ as isize));
    out.retain(|(_, r)| r.left < m.right && r.right > m.left && r.top < m.bottom && r.bottom > m.top);
    // A tray icon's menu (taken in by `note_desktop_popup`) goes over the shell's flyouts: the
    // tray overflow is listed above it (a higher band than menus), which drew the menu under
    // the overflow on the card.
    let noted = DESKTOP_POPUPS.lock().unwrap_or_else(|e| e.into_inner()).clone();
    out.sort_by_key(|(h, _)| !noted.contains(h));
    out
}

/// Calls `f` on every top-level window, topmost first. Unlike `EnumWindows`, this also reaches
/// the shell's immersive windows (Start, search, the taskbar's XAML popups).
fn for_each_top(f: unsafe extern "system" fn(HWND, LPARAM) -> BOOL, l: LPARAM) {
    let mut h = None;
    for _ in 0..10_000 {
        match unsafe { FindWindowExW(None, h, PCWSTR::null(), PCWSTR::null()) } {
            Ok(next) if !next.is_invalid() => {
                if !unsafe { f(next, l) }.as_bool() {
                    return;
                }
                h = Some(next);
            }
            _ => return,
        }
    }
}

/// Whether `h` is one of the shell's popups (Start, search, the taskbar's previews, menus and
/// flyouts), not the taskbar or the desktop themselves.
fn shell_window(h: HWND, class: &str) -> bool {
    !matches!(
        class,
        "Progman" | "WorkerW" | "Shell_TrayWnd" | "Shell_SecondaryTrayWnd" | "EdgeUiInputWndClass"
    ) && desktop_part(h, class)
}

/// Whether `h` is a shell popup (see [`shell_popups`]).
pub fn is_shell_popup(h: HWND) -> bool {
    shell_window(h, &window_class(h))
}

/// A shell popup made invisible, with what to give back: its extended style and, if it was
/// layered already, its layered attributes.
struct ShellHidden {
    hwnd: isize,
    ex: i32,
    attrs: Option<(u32, u8, u32)>,
}

/// Whether the desktop is used in place (between [`hide_shell`] and [`show_shell`]): only then
/// do Explorer's context menus count as shell popups (File Explorer windows have the same ones).
static DESKTOP_IN_PLACE: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
/// Whether a window is used in place with the shell popups shown on its card (between
/// [`hide_shell_on_app`] and [`show_shell`]).
static SHELL_ON_APP: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// Popups of the desktop's process that came up right after a click passed on to its icons
/// (see [`note_desktop_popup`]), counted as shell popups until the desktop is given back.
static DESKTOP_POPUPS: std::sync::Mutex<Vec<isize>> = std::sync::Mutex::new(Vec::new());

/// How long [`hide_shell`]'s search for shell popups takes (it runs every 50 ms while the
/// desktop is used in place), without hiding anything: for the self-test.
pub fn shell_scan_report() -> String {
    unsafe extern "system" fn each(h: HWND, l: LPARAM) -> BOOL {
        let out = unsafe { &mut *(l.0 as *mut (u32, u32)) };
        out.0 += 1;
        if shell_window(h, &window_class(h)) {
            out.1 += 1;
        }
        true.into()
    }
    let mut ms = Vec::new();
    let mut n = (0u32, 0u32);
    for _ in 0..3 {
        let t = std::time::Instant::now();
        n = (0, 0);
        for_each_top(each, LPARAM(&mut n as *mut _ as isize));
        ms.push(format!("{:.1}", t.elapsed().as_secs_f32() * 1000.0));
    }
    let mut app = Vec::new();
    if let Ok(t) = unsafe { FindWindowW(w!("Shell_TrayWnd"), PCWSTR::null()) } {
        for _ in 0..3 {
            let t0 = std::time::Instant::now();
            find_popups(t.0 as isize);
            app.push(format!("{:.2}", t0.elapsed().as_secs_f32() * 1000.0));
        }
    }
    format!(
        "shell popup search: {} top-level windows, {} shell popups, {} ms; app popup search {} ms",
        n.0,
        n.1,
        ms.join(" / "),
        app.join(" / ")
    )
}

/// Class, program, styles and owner of window `h`, for the log.
pub fn details(h: HWND) -> String {
    unsafe {
        let owner = GetAncestor(h, GA_ROOTOWNER);
        format!(
            "{} style {:#x} ex {:#x} minimized {} owner {}",
            describe(h),
            GetWindowLongW(h, GWL_STYLE) as u32,
            GetWindowLongW(h, GWL_EXSTYLE) as u32,
            IsIconic(h).as_bool(),
            if owner == h || owner.is_invalid() { "none".to_string() } else { describe(owner) }
        )
    }
}

/// Shell popups hidden from the screen while the desktop is used in place, and ones being given
/// back (they stay hidden until they close, so they don't flash up as the 3D view takes over).
static SHELL_HIDDEN: std::sync::Mutex<(Vec<ShellHidden>, Vec<ShellHidden>)> =
    std::sync::Mutex::new((Vec::new(), Vec::new()));

/// The window with the foreground as the desktop's use in place began: it goes under the desktop
/// like the rest (only one picked since is left on top).
static FG_AT_START: std::sync::atomic::AtomicIsize = std::sync::atomic::AtomicIsize::new(0);

/// Windows pushed under the desktop while it is used in place, topmost first, each with the
/// window that was just above it (0: none).
static UNDER: std::sync::Mutex<Vec<(isize, isize)>> = std::sync::Mutex::new(Vec::new());

/// While the desktop is used in place, the windows over it on the primary monitor go under it
/// (behind the 3D view, unseen), so the real cursor and buttons reach its icons: dragging them
/// and dragging a selection box work as on the real desktop, which clicks passed on to them
/// (see `wall`) can't do well. Topmost windows stay (clicks on them are passed on). Put back
/// with [`restore_under_desktop`].
fn push_under_desktop() {
    let (Some(m), Ok(progman)) = (primary_rect(), unsafe { FindWindowW(w!("Progman"), PCWSTR::null()) }) else {
        return;
    };
    let me = std::process::id();
    // A window just picked (on the taskbar, say) is left on top: the desktop's use is ending.
    let mut fg = unsafe { GetAncestor(GetForegroundWindow(), GA_ROOTOWNER) };
    if fg.0 as isize == FG_AT_START.load(std::sync::atomic::Ordering::Relaxed) {
        fg = HWND::default();
    }
    let mut over = Vec::new();
    unsafe {
        let mut prev = 0isize;
        let mut h = GetTopWindow(None).unwrap_or_default();
        while !h.is_invalid() && h != progman {
            let mut r = RECT::default();
            let class = window_class(h);
            if IsWindowVisible(h).as_bool()
                && GetWindowLongW(h, GWL_EXSTYLE) as u32 & WS_EX_TOPMOST.0 == 0
                && window_pid(h) != me
                && !is_cloaked(h)
                && GetWindowRect(h, &mut r).is_ok()
                && r.left < m.right
                && r.right > m.left
                && r.top < m.bottom
                && r.bottom > m.top
                && !matches!(class.as_str(), "WorkerW" | "Shell_TrayWnd" | "Shell_SecondaryTrayWnd")
                && !shell_window(h, &class)
                && !IsHungAppWindow(h).as_bool()
                && GetAncestor(h, GA_ROOTOWNER) != fg
                && !elevated(h)
            {
                over.push((h.0 as isize, prev));
            }
            prev = h.0 as isize;
            h = GetWindow(h, GW_HWNDNEXT).unwrap_or_default();
        }
    }
    let mut under = UNDER.lock().unwrap_or_else(|e| e.into_inner());
    // One pushed already that is over the desktop again won't go (an elevated window, say) or
    // came back up by itself: it is left there, not fought over every sweep.
    over.retain(|&(h, _)| !under.iter().any(|&(u, _)| u == h));
    if over.is_empty() {
        return;
    }
    // Topmost first, each to the bottom: they keep their order under the desktop.
    for &(h, prev) in &over {
        unsafe {
            let _ = SetWindowPos(
                hwnd_of(h),
                Some(HWND_BOTTOM),
                0,
                0,
                0,
                0,
                SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE | SWP_NOOWNERZORDER,
            );
        }
        under.push((h, prev));
    }
    crate::store::log(&format!("in-place desktop: {} window(s) over it went under it", over.len()));
}

/// For the self-test: the windows over the desktop go under it and come back in their order.
pub fn selftest_under_desktop() -> String {
    let order = || {
        let mut v = Vec::new();
        unsafe {
            let mut h = GetTopWindow(None).unwrap_or_default();
            while !h.is_invalid() {
                if IsWindowVisible(h).as_bool() && is_switchable(h) {
                    v.push(h.0 as isize);
                }
                h = GetWindow(h, GW_HWNDNEXT).unwrap_or_default();
            }
        }
        v
    };
    let before = order();
    // As the desktop's use in place begins: the window with the foreground goes under it too.
    let fg = unsafe { GetAncestor(GetForegroundWindow(), GA_ROOTOWNER) };
    FG_AT_START.store(fg.0 as isize, std::sync::atomic::Ordering::Relaxed);
    let t = std::time::Instant::now();
    push_under_desktop();
    let pushed = UNDER.lock().unwrap_or_else(|e| e.into_inner()).len();
    let push_ms = t.elapsed().as_secs_f32() * 1000.0;
    let center = primary_rect().map(|m| POINT { x: (m.left + m.right) / 2, y: (m.top + m.bottom) / 2 });
    let progman = unsafe { FindWindowW(w!("Progman"), PCWSTR::null()) }.unwrap_or_default();
    let at = center.map(|c| describe(window_at(c))).unwrap_or_default();
    let reaches = center.is_some_and(|c| part_of(window_at(c), progman.0 as isize));
    let fg = unsafe { GetAncestor(GetForegroundWindow(), GA_ROOTOWNER) };
    let at_fg = center.is_some_and(|c| part_of(window_at(c), fg.0 as isize));
    let t = std::time::Instant::now();
    restore_under_desktop();
    let restore_ms = t.elapsed().as_secs_f32() * 1000.0;
    let after = order();
    format!(
        "under-desktop test: {pushed} window(s) pushed in {push_ms:.1} ms, desktop reached at the center {reaches} ({at}, the foreground {at_fg}), put back in {restore_ms:.1} ms, order kept {}",
        before == after
    )
}

/// Puts the windows [`push_under_desktop`] pushed back where they were (except one given the
/// foreground since: it came up on top by itself).
pub fn restore_under_desktop() {
    let list = match UNDER.try_lock() {
        Ok(mut l) => std::mem::take(&mut *l),
        Err(_) => return,
    };
    if list.is_empty() {
        return;
    }
    let mut fg = unsafe { GetAncestor(GetForegroundWindow(), GA_ROOTOWNER) };
    if fg.0 as isize == FG_AT_START.load(std::sync::atomic::Ordering::Relaxed) {
        fg = HWND::default();
    }
    for &(h, prev) in &list {
        let hw = hwnd_of(h);
        unsafe {
            if !IsWindow(Some(hw)).as_bool() || hw == fg || IsHungAppWindow(hw).as_bool() {
                continue;
            }
            let p = hwnd_of(prev);
            let after = if prev != 0
                && IsWindow(Some(p)).as_bool()
                && GetWindowLongW(p, GWL_EXSTYLE) as u32 & WS_EX_TOPMOST.0 == 0
            {
                p
            } else {
                HWND_TOP
            };
            let _ = SetWindowPos(
                hw,
                Some(after),
                0,
                0,
                0,
                0,
                SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE | SWP_NOOWNERZORDER,
            );
        }
    }
    crate::store::log(&format!("in-place desktop over: {} window(s) put back over it", list.len()));
}

/// Like [`hide_shell`], while a window is used in place: Start, search and the flyouts are drawn
/// on its card where they show over it (see [`popups_of`]). Explorer's menus are not the shell's.
pub fn hide_shell_on_app() {
    restore_under_desktop();
    DESKTOP_IN_PLACE.store(false, std::sync::atomic::Ordering::Relaxed);
    SHELL_ON_APP.store(true, std::sync::atomic::Ordering::Relaxed);
    hide_shell_again();
}

/// Whether the shell popups are being hidden (see [`hide_shell`] and [`hide_shell_on_app`]).
pub fn shell_hiding() -> bool {
    DESKTOP_IN_PLACE.load(std::sync::atomic::Ordering::Relaxed) || SHELL_ON_APP.load(std::sync::atomic::Ordering::Relaxed)
}

/// Hides the shell popups again (new ones), without changing what they are hidden for.
pub fn hide_shell_again() {
    unsafe extern "system" fn each(h: HWND, l: LPARAM) -> BOOL {
        let out = unsafe { &mut *(l.0 as *mut Vec<isize>) };
        if shell_window(h, &window_class(h)) {
            out.push(h.0 as isize);
        }
        true.into()
    }
    let mut all = Vec::new();
    for_each_top(each, LPARAM(&mut all as *mut _ as isize));
    // The taskbar too: opening Start raises it over the 3D view.
    if let Ok(t) = unsafe { FindWindowW(w!("Shell_TrayWnd"), PCWSTR::null()) } {
        all.push(t.0 as isize);
    }
    let mut st = SHELL_HIDDEN.lock().unwrap_or_else(|e| e.into_inner());
    let (hidden, releasing) = &mut *st;
    for h in all {
        hide(hidden, releasing, h);
    }
}

/// Makes window `h` invisible on the screen (layered with an alpha of 1) and notes it in
/// `hidden`, unless it is there already; one in `releasing` goes back to `hidden`.
/// An off-screen test window hidden like a shell popup, then dropping its layered style the way
/// a menu does when its fade-in is done: whether the next look hides it again, and whether it
/// is given back without the style.
pub fn menu_fade_test() -> String {
    unsafe {
        let Ok(h) = CreateWindowExW(
            WS_EX_TOOLWINDOW | WS_EX_NOACTIVATE | WS_EX_LAYERED,
            w!("STATIC"),
            w!("window3d menu fade test"),
            WS_POPUP,
            -24000,
            -24000,
            200,
            200,
            None,
            None,
            None,
            None,
        ) else {
            return "menu fade test: could not create the test window".into();
        };
        let _ = SetLayeredWindowAttributes(h, COLORREF(0), 128, LWA_ALPHA);
        let _ = ShowWindow(h, SW_SHOWNOACTIVATE);
        let (mut hidden, mut releasing) = (Vec::new(), Vec::new());
        let alpha = |h: HWND| {
            let mut a = 0u8;
            let ex = GetWindowLongW(h, GWL_EXSTYLE) & WS_EX_LAYERED.0 as i32 != 0;
            (ex && GetLayeredWindowAttributes(h, None, Some(&mut a), None).is_ok()).then_some(a)
        };
        hide(&mut hidden, &mut releasing, h.0 as isize);
        let first = alpha(h);
        let ex = GetWindowLongW(h, GWL_EXSTYLE);
        SetWindowLongW(h, GWL_EXSTYLE, ex & !(WS_EX_LAYERED.0 as i32));
        hide(&mut hidden, &mut releasing, h.0 as isize);
        let again = alpha(h);
        let gone = hidden.iter().all(|s| s.ex & WS_EX_LAYERED.0 as i32 == 0);
        // Fading in by its own alpha, the way a tooltip does.
        let _ = SetLayeredWindowAttributes(h, COLORREF(0), 200, LWA_ALPHA);
        hide(&mut hidden, &mut releasing, h.0 as isize);
        let faded = alpha(h);
        let kept = hidden.iter().any(|s| s.attrs.is_some_and(|a| a.1 == 200));
        let _ = DestroyWindow(h);
        // A menu ("#32768") of an app other than the shell, taken in as a tray icon's menu.
        use std::sync::atomic::Ordering;
        let menu = CreateWindowExW(WS_EX_TOOLWINDOW | WS_EX_NOACTIVATE, w!("#32768"), w!(""), WS_POPUP, -24000, -24000, 100, 100, None, None, None, None);
        let tray = match menu {
            Ok(m) if !DESKTOP_IN_PLACE.load(Ordering::Relaxed) => {
                let plain = is_shell_popup(m);
                DESKTOP_IN_PLACE.store(true, Ordering::Relaxed);
                DESKTOP_POPUPS.lock().unwrap_or_else(|e| e.into_inner()).push(m.0 as isize);
                let taken = is_shell_popup(m);
                DESKTOP_POPUPS.lock().unwrap_or_else(|e| e.into_inner()).retain(|&p| p != m.0 as isize);
                DESKTOP_IN_PLACE.store(false, Ordering::Relaxed);
                let _ = DestroyWindow(m);
                format!("{plain} (taken in {taken})")
            }
            Ok(m) => {
                let _ = DestroyWindow(m);
                "not tried (the desktop is in use)".into()
            }
            Err(e) => format!("not tried ({e})"),
        };
        format!("menu fade test: hidden {first:?}, after the fade {again:?}, given back unlayered {gone}, after an alpha fade {faded:?} (to give back 200 {kept}); an app's menu a shell popup {tray}")
    }
}

/// Checks the shell popups hidden already (no search for new ones, so it is cheap enough for
/// every frame): a menu's fade-in dropping its layered style, or a tooltip raising its own
/// alpha, showed it on the real screen until the next sweep (every 250 ms). Also notes how
/// the followed popups look now in the log (see [`trace_pop`]).
pub fn rehide_shell() {
    if !shell_hiding() {
        return;
    }
    let mut st = SHELL_HIDDEN.lock().unwrap_or_else(|e| e.into_inner());
    let (hidden, releasing) = &mut *st;
    let list: Vec<isize> = hidden.iter().map(|s| s.hwnd).collect();
    for h in list {
        hide(hidden, releasing, h);
    }
    drop(st);
    trace_look();
}

/// A popup followed in the log while the shell popups are hidden: when it was first seen, the
/// last line logged of each kind (only changes are logged) and how many lines it got.
struct PopTrace {
    hwnd: isize,
    t0: std::time::Instant,
    last: Vec<(&'static str, String)>,
    lines: u32,
}

static POP_TRACE: std::sync::Mutex<Vec<PopTrace>> = std::sync::Mutex::new(Vec::new());
/// Windows that just showed, with the time of the event (`GetTickCount`), to log how late
/// the app took them in.
static SHOWN_AT: std::sync::Mutex<Vec<(isize, u32)>> = std::sync::Mutex::new(Vec::new());

/// Window `h` showed at `ms` (the event's time): kept for [`trace_pop`] while popups are hidden.
pub fn trace_shown(h: HWND, ms: u32) {
    if shell_hiding() {
        let mut s = SHOWN_AT.lock().unwrap_or_else(|e| e.into_inner());
        s.retain(|(o, _)| *o != h.0 as isize);
        s.push((h.0 as isize, ms));
        if s.len() > 64 {
            s.remove(0);
        }
    }
}

/// Logs `what` about popup `h` ("popup trace") while the shell popups are hidden, when it
/// differs from the last line of the same `kind`; at most 80 lines a popup. The taskbar is
/// left out (it is hidden at every sweep).
pub fn trace_pop(h: isize, kind: &'static str, what: &str) {
    if !shell_hiding() {
        return;
    }
    let mut t = POP_TRACE.lock().unwrap_or_else(|e| e.into_inner());
    if !t.iter().any(|p| p.hwnd == h) {
        // Only a popup that is on screen now is followed: the sweeps also hide dozens of
        // invisible or cloaked shell windows, which would bury the trace.
        let hw = hwnd_of(h);
        if window_class(hw) == "Shell_TrayWnd" || unsafe { !IsWindowVisible(hw).as_bool() } || is_cloaked(hw) {
            return;
        }
        t.push(PopTrace { hwnd: h, t0: std::time::Instant::now(), last: Vec::new(), lines: 0 });
        drop(t);
        let shown = SHOWN_AT.lock().unwrap_or_else(|e| e.into_inner()).iter().find(|(o, _)| *o == h).map(|s| s.1);
        let now = unsafe { windows::Win32::System::SystemInformation::GetTickCount() };
        crate::store::log(&format!(
            "popup trace {h:#x}: followed from now on ({}); {}",
            details(hwnd_of(h)),
            match shown {
                Some(ms) => format!("it showed {} ms ago", now.wrapping_sub(ms)),
                None => "no show event seen for it".into(),
            }
        ));
        t = POP_TRACE.lock().unwrap_or_else(|e| e.into_inner());
    }
    let Some(p) = t.iter_mut().find(|p| p.hwnd == h) else {
        return;
    };
    if p.lines >= 80 || p.last.iter().any(|(k, l)| *k == kind && l == what) {
        return;
    }
    p.last.retain(|(k, _)| *k != kind);
    p.last.push((kind, what.to_string()));
    p.lines += 1;
    let (ms, last) = (p.t0.elapsed().as_millis(), p.lines == 80);
    drop(t);
    crate::store::log(&format!(
        "popup trace {h:#x} +{ms} ms {kind}: {what}{}",
        if last { " (no more lines for this one)" } else { "" }
    ));
}

/// How popup `h` looks now, for [`trace_pop`]: shown, where, its layered alpha, and whether
/// it is over the 3D view.
fn pop_state(h: HWND) -> String {
    unsafe {
        let mut r = RECT::default();
        let _ = GetWindowRect(h, &mut r);
        let ex = GetWindowLongW(h, GWL_EXSTYLE);
        let (mut alpha, mut flags) = (0u8, LAYERED_WINDOW_ATTRIBUTES_FLAGS(0));
        let attrs = if ex & WS_EX_LAYERED.0 as i32 == 0 {
            "not layered".to_string()
        } else if GetLayeredWindowAttributes(h, None, Some(&mut alpha), Some(&mut flags)).is_ok() {
            format!("alpha {alpha} flags {:#x}", flags.0)
        } else {
            "layered by UpdateLayeredWindow".to_string()
        };
        let view = crate::wall::overlay();
        let mut over = false;
        let mut w = GetWindow(h, GW_HWNDNEXT).ok();
        for _ in 0..2000 {
            match w {
                Some(x) if view != 0 && !x.is_invalid() => {
                    if x.0 as isize == view {
                        over = true;
                        break;
                    }
                    w = GetWindow(x, GW_HWNDNEXT).ok();
                }
                _ => break,
            }
        }
        format!(
            "visible {} cloaked {} rect ({},{})-({},{}) {attrs}, {} the 3D view",
            IsWindowVisible(h).as_bool(),
            is_cloaked(h),
            r.left,
            r.top,
            r.right,
            r.bottom,
            if over { "over" } else { "under" }
        )
    }
}

/// Logs the followed popups' looks that changed, and drops the ones destroyed.
fn trace_look() {
    let hs: Vec<isize> = POP_TRACE.lock().unwrap_or_else(|e| e.into_inner()).iter().map(|p| p.hwnd).collect();
    for h in hs {
        if unsafe { IsWindow(Some(hwnd_of(h))).as_bool() } {
            trace_pop(h, "look", &pop_state(hwnd_of(h)));
        } else {
            trace_pop(h, "look", "destroyed");
            POP_TRACE.lock().unwrap_or_else(|e| e.into_inner()).retain(|p| p.hwnd != h);
        }
    }
}

/// Puts `hw` right under the 3D view, without activating it.
fn under_view(hw: HWND) {
    let view = crate::wall::overlay();
    if view != 0 {
        unsafe {
            let _ = SetWindowPos(
                hw,
                Some(hwnd_of(view)),
                0,
                0,
                0,
                0,
                SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE | SWP_NOOWNERZORDER | SWP_ASYNCWINDOWPOS,
            );
        }
    }
}

fn hide(hidden: &mut Vec<ShellHidden>, releasing: &mut Vec<ShellHidden>, h: isize) {
    if let Some(s) = hidden.iter_mut().find(|s| s.hwnd == h) {
        let mut under = false;
        // A menu fading in is layered by itself, and drops the style (with the alpha of 1)
        // once the fade is done: it showed on the real screen, away from the card. Hidden
        // again, to give back without the style.
        let hw = hwnd_of(h);
        unsafe {
            let ex = GetWindowLongW(hw, GWL_EXSTYLE);
            let (mut key, mut alpha, mut flags) = (COLORREF(0), 0u8, LAYERED_WINDOW_ATTRIBUTES_FLAGS(0));
            if ex & WS_EX_LAYERED.0 as i32 == 0 {
                SetWindowLongW(hw, GWL_EXSTYLE, ex | WS_EX_LAYERED.0 as i32);
                if SetLayeredWindowAttributes(hw, COLORREF(0), 1, LWA_ALPHA).is_ok() {
                    (s.ex, s.attrs) = (ex, None);
                    trace_pop(h, "hide", "dropped its layered style (its fade-in is done): hidden again");
                } else {
                    SetWindowLongW(hw, GWL_EXSTYLE, ex);
                }
            } else if GetLayeredWindowAttributes(hw, Some(&mut key), Some(&mut alpha), Some(&mut flags)).is_ok()
                && (flags.0 & LWA_ALPHA.0 == 0 || alpha > 1)
            {
                // A tooltip fading in sets its own alpha, step by step up to 255: it showed on
                // the real screen as well as on the card. Its newest alpha is what it gets back.
                if SetLayeredWindowAttributes(hw, COLORREF(0), 1, LWA_ALPHA).is_ok() {
                    s.attrs = Some((key.0, alpha, flags.0));
                    under = true;
                    trace_pop(h, "hide", &format!("set its own alpha {alpha} (flags {:#x}): back to 1", flags.0));
                }
            }
            // A tooltip fading itself in again showed flat on the real screen too, over the 3D
            // view ("mirrored"): under the 3D view with it. (Not a menu: it is clicked through
            // the card.)
            if under {
                under_view(hw);
            }
        }
        return;
    }
    if let Some(i) = releasing.iter().position(|s| s.hwnd == h) {
        hidden.push(releasing.swap_remove(i));
        return;
    }
    let hw = hwnd_of(h);
    unsafe {
        let ex = GetWindowLongW(hw, GWL_EXSTYLE);
        let (mut key, mut alpha, mut flags) = (COLORREF(0), 0u8, LAYERED_WINDOW_ATTRIBUTES_FLAGS(0));
        let layered = ex & WS_EX_LAYERED.0 as i32 != 0;
        if layered && GetLayeredWindowAttributes(hw, Some(&mut key), Some(&mut alpha), Some(&mut flags)).is_err() {
            // Drawn with UpdateLayeredWindow (a drag image, a tooltip): an alpha of its own would
            // stop it drawing, so it goes under the 3D view instead, at every look (it may come
            // back on top).
            under_view(hw);
            trace_pop(
                h,
                "hide",
                if elevated(hw) {
                    "drawn with UpdateLayeredWindow, out of reach: stays over the 3D view"
                } else {
                    "drawn with UpdateLayeredWindow: pushed under the 3D view"
                },
            );
            return;
        }
        let attrs = layered.then_some((key.0, alpha, flags.0));
        if attrs.is_some_and(|a| a.2 & LWA_ALPHA.0 != 0 && a.1 <= 1) {
            trace_pop(h, "hide", &format!("invisible already (alpha {alpha}): left be"));
            return; // Left behind hidden, or invisible by itself: leave it be.
        }
        SetWindowLongW(hw, GWL_EXSTYLE, ex | WS_EX_LAYERED.0 as i32);
        if SetLayeredWindowAttributes(hw, COLORREF(0), 1, LWA_ALPHA).is_ok() {
            hidden.push(ShellHidden { hwnd: h, ex, attrs });
            trace_pop(h, "hide", &format!("hidden (alpha 1; it had ex {ex:#x}, layered {attrs:?})"));
        } else {
            SetWindowLongW(hw, GWL_EXSTYLE, ex);
            trace_pop(h, "hide", &format!("can't be hidden (out of reach: {}): left on the screen", elevated(hw)));
        }
    }
}

fn unhide(s: &ShellHidden) {
    let hw = hwnd_of(s.hwnd);
    unsafe {
        if !IsWindow(Some(hw)).as_bool() {
            return;
        }
        match s.attrs {
            Some((key, alpha, flags)) => {
                let _ = SetLayeredWindowAttributes(
                    hw,
                    COLORREF(key),
                    alpha,
                    LAYERED_WINDOW_ATTRIBUTES_FLAGS(flags),
                );
            }
            None => {
                SetWindowLongW(hw, GWL_EXSTYLE, s.ex & !(WS_EX_LAYERED.0 as i32));
            }
        }
    }
}

/// Gives the hidden shell popups back: closed ones at once, open ones once they close (or after
/// a second and a half). With `now`, all at once (quitting).
/// Whether a hidden shell window is still showing. The taskbar always is, so it counts as
/// open only while a popup that would raise it (Start, search) still is.
fn open(s: &ShellHidden, all: &[ShellHidden]) -> bool {
    let h = hwnd_of(s.hwnd);
    if window_class(h) == "Shell_TrayWnd" {
        return all.iter().any(|o| o.hwnd != s.hwnd && open(o, &[]));
    }
    (unsafe { IsWindowVisible(h).as_bool() }) && !is_cloaked(h)
}

/// Gives back the windows of `list` that are no longer open, or all of them if `all`.
fn release(list: &mut Vec<ShellHidden>, all: bool) {
    let keep: Vec<bool> = list.iter().map(|s| !all && open(s, list)).collect();
    let mut i = 0;
    list.retain(|s| {
        i += 1;
        if !keep[i - 1] {
            unhide(s);
        }
        keep[i - 1]
    });
}

pub fn show_shell(now: bool) {
    DESKTOP_POPUPS.lock().unwrap_or_else(|e| e.into_inner()).clear();
    POP_TRACE.lock().unwrap_or_else(|e| e.into_inner()).clear();
    DESKTOP_IN_PLACE.store(false, std::sync::atomic::Ordering::Relaxed);
    SHELL_ON_APP.store(false, std::sync::atomic::Ordering::Relaxed);
    restore_under_desktop();
    give_back(&SHELL_HIDDEN, now);
    give_back_popups(now);
}

/// Gives back the windows hidden in `list`: closed ones at once, open ones once they close (or
/// after a second and a half). With `now`, all at once.
fn give_back(list: &'static std::sync::Mutex<(Vec<ShellHidden>, Vec<ShellHidden>)>, now: bool) {
    let mut st = list.lock().unwrap_or_else(|e| e.into_inner());
    let (hidden, releasing) = &mut *st;
    releasing.append(hidden);
    release(releasing, now);
    if releasing.is_empty() {
        return;
    }
    drop(st);
    std::thread::spawn(move || {
        let t0 = std::time::Instant::now();
        loop {
            std::thread::sleep(std::time::Duration::from_millis(30));
            let mut st = list.lock().unwrap_or_else(|e| e.into_inner());
            let late = t0.elapsed().as_millis() > 1500;
            release(&mut st.1, late);
            if st.1.is_empty() {
                return;
            }
        }
    });
}

/// From the panic hook: gives the hidden shell popups back unless the lock is held.
pub fn panic_show_shell() {
    restore_under_desktop();
    for list in [&SHELL_HIDDEN, &POPUPS_HIDDEN] {
        if let Ok(mut st) = list.try_lock() {
            let (hidden, releasing) = &mut *st;
            for s in hidden.drain(..).chain(releasing.drain(..)) {
                unhide(&s);
            }
        }
    }
}

/// The window used in place whose popups are hidden from the screen (0: none).
static POPUP_OWNER: std::sync::atomic::AtomicIsize = std::sync::atomic::AtomicIsize::new(0);
/// Popups of the window used in place hidden from the screen, and ones being given back.
static POPUPS_HIDDEN: std::sync::Mutex<(Vec<ShellHidden>, Vec<ShellHidden>)> =
    std::sync::Mutex::new((Vec::new(), Vec::new()));

/// Whether `h` is a popup of window `top` showing over (part of) `frame`, its frame bounds:
/// a menu, drop-down, tooltip or dialog of it, not `top` itself.
fn popup_of(h: HWND, top: isize, frame: &RECT) -> bool {
    let over = |r: &RECT| r.left < frame.right && r.right > frame.left && r.top < frame.bottom && r.bottom > frame.top;
    let mut r = RECT::default();
    h != hwnd_of(top)
        && unsafe { IsWindowVisible(h).as_bool() }
        && unsafe { GetWindowRect(h, &mut r).is_ok() }
        && over(&r)
        // Its frame too: a maximized window's rect reaches into the next monitor by its border.
        && frame_bounds(h).is_some_and(|f| over(&f))
        && (window_pid(h) != unsafe { GetCurrentProcessId() } || own_popup(h, top))
        && !is_cloaked(h)
        && part_of(h, top)
}

/// Whether `h`, one of this app's windows, is a popup owned by `top`, also this app's (only
/// the selftest's window is used in place this way).
fn own_popup(h: HWND, top: isize) -> bool {
    let top = hwnd_of(top);
    window_pid(top) == unsafe { GetCurrentProcessId() }
        && root_owner(h) == root_owner(top)
}

/// The window at the top of `h`'s owner chain (`h` if it has no owner). `GA_ROOTOWNER` only
/// follows the owners of `WS_POPUP` windows, so it stops at an owned dialog with a caption.
fn root_owner(h: HWND) -> HWND {
    let mut r = unsafe { GetAncestor(h, GA_ROOT) };
    if r.is_invalid() {
        r = h;
    }
    for _ in 0..16 {
        match unsafe { GetWindow(r, GW_OWNER) } {
            Ok(o) if !o.is_invalid() && o != r => r = o,
            _ => break,
        }
    }
    r
}

/// The popups of window `top` (used in place) showing over it, topmost first, with their top
/// left relative to its frame bounds (where they go in its captured image). Windows.Graphics
/// .Capture of a window leaves them out, so the card draws each from a capture of its own:
/// they are made invisible on the screen (still taking clicks and keys, still captured in
/// full), where they would show at the window's real place, not on the card. Popups that
/// closed are given back. [`give_back_popups`] gives back the rest.
pub fn popups_of(top: isize) -> Vec<(isize, POINT)> {
    POPUP_OWNER.store(top, std::sync::atomic::Ordering::Relaxed);
    let out = find_popups(top);
    let mut st = POPUPS_HIDDEN.lock().unwrap_or_else(|e| e.into_inner());
    let (hidden, releasing) = &mut *st;
    for (h, _) in &out {
        hide(hidden, releasing, *h);
    }
    release(hidden, false);
    drop(st);
    // Start, search and the flyouts over it (above its own popups), hidden by `hide_shell_on_app`.
    match frame_bounds(hwnd_of(top)) {
        Some(f) if SHELL_ON_APP.load(std::sync::atomic::Ordering::Relaxed) && !is_desktop(hwnd_of(top)) => shell_popups()
            .into_iter()
            .filter(|(_, r)| r.left < f.right && r.right > f.left && r.top < f.bottom && r.bottom > f.top)
            .map(|(h, r)| {
                let r = frame_bounds(hwnd_of(h)).unwrap_or(r);
                (h, POINT { x: r.left - f.left, y: r.top - f.top })
            })
            .chain(out)
            .collect(),
        _ => out,
    }
}

/// Whether shell popup `pop` (showing) lies partly off window `top`, so its card can't show it
/// all. Not yet showing (opening), it doesn't.
pub fn shows_off(pop: HWND, top: HWND) -> bool {
    let mut r = RECT::default();
    if !unsafe { IsWindowVisible(pop).as_bool() } || is_cloaked(pop) || unsafe { GetWindowRect(pop, &mut r) }.is_err() {
        return false;
    }
    let Some(f) = frame_bounds(top) else {
        return true;
    };
    let area = |l: i32, t: i32, rr: i32, b: i32| ((rr - l).max(0) as i64) * ((b - t).max(0) as i64);
    let inside = area(r.left.max(f.left), r.top.max(f.top), r.right.min(f.right), r.bottom.min(f.bottom));
    // A shadow or rounding may stick out a little.
    inside * 10 < area(r.left, r.top, r.right, r.bottom) * 9
}

/// The popups of window `top` (see [`popups_of`]), without hiding them.
fn find_popups(top: isize) -> Vec<(isize, POINT)> {
    let out = Vec::new();
    let Some(frame) = frame_bounds(hwnd_of(top)) else {
        return out;
    };
    if is_desktop(hwnd_of(top)) {
        return out;
    }
    struct Scan {
        top: isize,
        frame: RECT,
        out: Vec<(isize, POINT)>,
    }
    unsafe extern "system" fn each(h: HWND, l: LPARAM) -> BOOL {
        let s = unsafe { &mut *(l.0 as *mut Scan) };
        if popup_of(h, s.top, &s.frame) {
            if let Some(r) = frame_bounds(h) {
                let at = POINT {
                    x: r.left - s.frame.left,
                    y: r.top - s.frame.top,
                };
                s.out.push((h.0 as isize, at));
            }
        }
        true.into()
    }
    let mut scan = Scan { top, frame, out };
    // EnumWindows (topmost first) takes a fraction of `for_each_top`'s time, and app popups
    // are not immersive windows.
    unsafe {
        let _ = EnumWindows(Some(each), LPARAM(&mut scan as *mut _ as isize));
    }
    scan.out
}

/// Window `h` just came up: a popup of the window used in place is hidden from the screen at
/// once (the card's next look for popups would leave it showing for a moment).
pub fn note_popup(h: HWND) {
    let top = POPUP_OWNER.load(std::sync::atomic::Ordering::Relaxed);
    if top == 0 {
        return;
    }
    if frame_bounds(hwnd_of(top)).is_some_and(|f| !is_desktop(hwnd_of(top)) && popup_of(h, top, &f)) {
        let mut st = POPUPS_HIDDEN.lock().unwrap_or_else(|e| e.into_inner());
        let (hidden, releasing) = &mut *st;
        hide(hidden, releasing, h.0 as isize);
    }
}

/// Gives back the popups hidden by [`popups_of`] (open ones once they close, unless `now`).
pub fn give_back_popups(now: bool) {
    POPUP_OWNER.store(0, std::sync::atomic::Ordering::Relaxed);
    give_back(&POPUPS_HIDDEN, now);
}

/// At start: shell popups left invisible by a Window3D that ended while the desktop was in use
/// are made visible again.
pub fn recover_shell() {
    unsafe extern "system" fn each(h: HWND, _: LPARAM) -> BOOL {
        unsafe {
            let ex = GetWindowLongW(h, GWL_EXSTYLE);
            let class = window_class(h);
            if ex & WS_EX_LAYERED.0 as i32 != 0
                && (class == "Shell_TrayWnd" || shell_window(h, &class))
            {
                let (mut alpha, mut flags) = (0u8, LAYERED_WINDOW_ATTRIBUTES_FLAGS(0));
                let _ = GetLayeredWindowAttributes(h, None, Some(&mut alpha), Some(&mut flags));
                if flags.0 & LWA_ALPHA.0 != 0 && alpha == 1 {
                    crate::store::log(&format!("shell window made visible again: {}", window_class(h)));
                    if window_class(h) == "Windows.UI.Core.CoreWindow" {
                        SetWindowLongW(h, GWL_EXSTYLE, ex & !(WS_EX_LAYERED.0 as i32));
                    } else {
                        let _ = SetLayeredWindowAttributes(h, COLORREF(0), 255, LWA_ALPHA);
                    }
                }
            }
        }
        true.into()
    }
    for_each_top(each, LPARAM(0));
    // Windows left under the desktop (see `push_under_desktop`) come back over it, in order.
    let Ok(progman) = (unsafe { FindWindowW(w!("Progman"), PCWSTR::null()) }) else {
        return;
    };
    let mut below = Vec::new();
    unsafe {
        let mut h = GetWindow(progman, GW_HWNDNEXT).unwrap_or_default();
        while !h.is_invalid() {
            if is_switchable(h) {
                below.push(h);
            }
            h = GetWindow(h, GW_HWNDNEXT).unwrap_or_default();
        }
        let above = GetWindow(progman, GW_HWNDPREV).unwrap_or_default();
        for &h in below.iter().rev() {
            let after = if above.is_invalid() { HWND_TOP } else { above };
            let _ = SetWindowPos(h, Some(after), 0, 0, 0, 0, SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE | SWP_NOOWNERZORDER);
        }
    }
    if !below.is_empty() {
        crate::store::log(&format!("{} window(s) left under the desktop put back over it", below.len()));
    }
}

/// How the desktop card is put together: Progman spans every monitor, so its image is cropped
/// to the primary monitor (`crop`, relative to Progman's top left), and the taskbar, a window of
/// its own, is drawn over it at `tray_at` (relative to the primary monitor's top left).
pub struct DesktopLayout {
    pub crop: RECT,
    pub tray: Option<isize>,
    pub tray_at: POINT,
}

pub fn desktop_layout(progman: isize) -> Option<DesktopLayout> {
    unsafe {
        let mut pr = RECT::default();
        GetWindowRect(hwnd_of(progman), &mut pr).ok()?;
        let m = primary_rect()?;
        let crop = RECT {
            left: m.left - pr.left,
            top: m.top - pr.top,
            right: m.right - pr.left,
            bottom: m.bottom - pr.top,
        };
        let tray = FindWindowW(w!("Shell_TrayWnd"), PCWSTR::null()).ok();
        let mut tr = RECT::default();
        if let Some(t) = tray {
            let _ = GetWindowRect(t, &mut tr);
        }
        Some(DesktopLayout {
            crop,
            tray: tray.map(|t| t.0 as isize),
            tray_at: POINT {
                x: tr.left - m.left,
                y: tr.top - m.top,
            },
        })
    }
}

fn monitor_rect_of(hwnd: HWND) -> Option<RECT> {
    unsafe {
        let mon = MonitorFromWindow(hwnd, MONITOR_DEFAULTTONEAREST);
        let mut mi = MONITORINFO {
            cbSize: size_of::<MONITORINFO>() as u32,
            ..Default::default()
        };
        GetMonitorInfoW(mon, &mut mi)
            .as_bool()
            .then_some(mi.rcMonitor)
    }
}

/// Whether the window already covers its whole monitor (borderless or exclusive fullscreen).
pub fn covers_monitor(hwnd: HWND) -> bool {
    unsafe {
        let mut r = RECT::default();
        if GetWindowRect(hwnd, &mut r).is_err() {
            return false;
        }
        monitor_rect_of(hwnd).is_some_and(|m| {
            r.left <= m.left && r.top <= m.top && r.right >= m.right && r.bottom >= m.bottom
        })
    }
}

/// Removes the frame of a resizable window and makes it fill its monitor. Returns the old frame
/// on success; `None` if the window is not suitable (then the caller maximizes it instead).
pub fn make_borderless(hwnd: HWND, class: &str) -> Option<SavedFrame> {
    if NO_BORDERLESS_CLASSES.contains(&class) {
        return None;
    }
    unsafe {
        let style = GetWindowLongPtrW(hwnd, GWL_STYLE);
        let ex_style = GetWindowLongPtrW(hwnd, GWL_EXSTYLE);
        let need = (WS_CAPTION.0 | WS_THICKFRAME.0) as isize;
        if style & need != need || style & WS_CHILD.0 as isize != 0 {
            return None;
        }
        let mut placement = WINDOWPLACEMENT {
            length: size_of::<WINDOWPLACEMENT>() as u32,
            ..Default::default()
        };
        GetWindowPlacement(hwnd, &mut placement).ok()?;
        let m = monitor_rect_of(hwnd)?;
        if IsIconic(hwnd).as_bool() || IsZoomed(hwnd).as_bool() {
            // A maximized window keeps snapping back to the work area; un-maximize it first.
            let _ = ShowWindow(hwnd, SW_RESTORE);
        }
        let new_style =
            style & !(need | (WS_MAXIMIZEBOX.0 | WS_MINIMIZEBOX.0 | WS_SYSMENU.0) as isize);
        let new_ex = ex_style
            & !((WS_EX_DLGMODALFRAME.0
                | WS_EX_WINDOWEDGE.0
                | WS_EX_CLIENTEDGE.0
                | WS_EX_STATICEDGE.0) as isize);
        SetLastError(WIN32_ERROR(0));
        SetWindowLongPtrW(hwnd, GWL_STYLE, new_style);
        if GetWindowLongPtrW(hwnd, GWL_STYLE) != new_style {
            // Refused (e.g. an elevated window): put back whatever changed.
            SetWindowLongPtrW(hwnd, GWL_STYLE, style);
            return None;
        }
        SetWindowLongPtrW(hwnd, GWL_EXSTYLE, new_ex);
        let _ = SetWindowPos(
            hwnd,
            None,
            m.left,
            m.top,
            m.right - m.left,
            m.bottom - m.top,
            SWP_FRAMECHANGED | SWP_NOZORDER | SWP_NOOWNERZORDER | SWP_ASYNCWINDOWPOS,
        );
        Some(SavedFrame {
            style,
            ex_style,
            placement,
        })
    }
}

/// Puts back the frame, size and maximized state saved by `make_borderless`.
pub fn restore_frame(hwnd: HWND, f: &SavedFrame) {
    unsafe {
        if !IsWindow(Some(hwnd)).as_bool() || IsHungAppWindow(hwnd).as_bool() {
            return;
        }
        SetWindowLongPtrW(hwnd, GWL_STYLE, f.style);
        SetWindowLongPtrW(hwnd, GWL_EXSTYLE, f.ex_style);
        let mut p = f.placement;
        if p.showCmd == SW_SHOWMINIMIZED.0 as u32 || p.showCmd == SW_MINIMIZE.0 as u32 {
            p.showCmd = SW_SHOWNORMAL.0 as u32;
        }
        let _ = SetWindowPlacement(hwnd, &p);
        let _ = SetWindowPos(
            hwnd,
            None,
            0,
            0,
            0,
            0,
            SWP_FRAMECHANGED
                | SWP_NOMOVE
                | SWP_NOSIZE
                | SWP_NOZORDER
                | SWP_NOOWNERZORDER
                | SWP_NOACTIVATE,
        );
    }
}

/// Brings a real window to the foreground and sizes it according to `mode`: borderless
/// fullscreen if possible, else maximized; windows that already fill their monitor are left
/// alone. Returns the saved frame if the window was made borderless.
pub fn focus_and_size(hwnd: HWND, class: &str, mode: FocusMode) -> (bool, Option<SavedFrame>) {
    // Focus first, while the caller still owns the foreground.
    let ok = focus_window(hwnd);
    let mut saved = None;
    unsafe {
        if mode != FocusMode::Keep
            && IsWindow(Some(hwnd)).as_bool()
            && !IsHungAppWindow(hwnd).as_bool()
            && !covers_monitor(hwnd)
        {
            if mode == FocusMode::Borderless {
                saved = make_borderless(hwnd, class);
            }
            if saved.is_none() {
                let style = GetWindowLongPtrW(hwnd, GWL_STYLE);
                if style & WS_MAXIMIZEBOX.0 as isize != 0 && !IsZoomed(hwnd).as_bool() {
                    let _ = ShowWindowAsync(hwnd, SW_MAXIMIZE);
                }
            }
        }
    }
    (ok, saved)
}

/// Brings a real window to the foreground (restoring it if minimized). Never moves or resizes it.
pub fn focus_window(hwnd: HWND) -> bool {
    unsafe {
        if !IsWindow(Some(hwnd)).as_bool() {
            return false;
        }
        if IsIconic(hwnd).as_bool() {
            let _ = ShowWindowAsync(hwnd, SW_RESTORE);
        }
        let ok = SetForegroundWindow(hwnd).as_bool();
        let _ = BringWindowToTop(hwnd);
        ok
    }
}

/// Gives `hwnd` the keyboard focus even when Windows' foreground lock would refuse it: first
/// plainly, then sharing the input state of the current foreground thread, then after a
/// synthetic press of an unassigned key (which counts as our own latest input). Nothing is
/// moved, resized or restored. Returns whether `hwnd` ended up in the foreground.
pub fn force_foreground(hwnd: HWND) -> bool {
    use windows::Win32::UI::Input::KeyboardAndMouse::*;
    unsafe {
        if !IsWindow(Some(hwnd)).as_bool() {
            return false;
        }
        if SetForegroundWindow(hwnd).as_bool() && GetForegroundWindow() == hwnd {
            return true;
        }
        let me = GetCurrentThreadId();
        let fg = GetForegroundWindow();
        let fg_tid = if fg.is_invalid() {
            0
        } else {
            GetWindowThreadProcessId(fg, None)
        };
        if fg_tid != 0 && fg_tid != me && AttachThreadInput(me, fg_tid, true).as_bool() {
            let _ = SetForegroundWindow(hwnd);
            let _ = AttachThreadInput(me, fg_tid, false);
        }
        if GetForegroundWindow() == hwnd {
            return true;
        }
        // 0xE8 is unassigned, so whoever receives it ignores it.
        let key = |flags| INPUT {
            r#type: INPUT_KEYBOARD,
            Anonymous: INPUT_0 {
                ki: KEYBDINPUT {
                    wVk: VIRTUAL_KEY(0xE8),
                    dwFlags: flags,
                    ..Default::default()
                },
            },
        };
        let _ = SendInput(
            &[key(KEYBD_EVENT_FLAGS(0)), key(KEYEVENTF_KEYUP)],
            size_of::<INPUT>() as i32,
        );
        let _ = SetForegroundWindow(hwnd);
        GetForegroundWindow() == hwnd
    }
}

/// Path of a known folder (`FOLDERID_Desktop`, ...), following redirections such as OneDrive.
pub fn known_folder(id: &windows::core::GUID) -> Option<std::path::PathBuf> {
    unsafe {
        let p = SHGetKnownFolderPath(id, KF_FLAG_DEFAULT, None).ok()?;
        let s = p.to_string().ok();
        windows::Win32::System::Com::CoTaskMemFree(Some(p.0 as *const c_void));
        s.filter(|s| !s.is_empty()).map(std::path::PathBuf::from)
    }
}

pub fn desktop_dirs() -> (Option<std::path::PathBuf>, Option<std::path::PathBuf>) {
    (
        known_folder(&FOLDERID_Desktop),
        known_folder(&FOLDERID_PublicDesktop),
    )
}

/// The desktop (the shell's `Progman` window), as a window for the live desktop card.
pub fn desktop_window() -> Option<WinInfo> {
    let hwnd = unsafe { FindWindowW(w!("Progman"), PCWSTR::null()).ok()? };
    let mut pid = 0u32;
    unsafe { GetWindowThreadProcessId(hwnd, Some(&mut pid)) };
    Some(WinInfo {
        hwnd: hwnd.0 as isize,
        exe: process_path(pid),
        class: "Progman".into(),
        title: "Desktop".into(),
        minimized: false,
    })
}

pub fn launch(path: &str) -> bool {
    launch_shown(path, false).is_some()
}

/// Launches `path` without letting it take the focus from the overlay: it is asked to open
/// inactive and other processes' `SetForegroundWindow` is locked out (Windows lifts the lock
/// itself on Alt or a click elsewhere; `unlock_foreground` lifts it sooner). Returns the id of
/// the started process (0 when the shell handed the file to an app already running), or None if
/// it failed.
pub fn launch_behind(path: &str) -> Option<u32> {
    unsafe {
        let _ = LockSetForegroundWindow(LSFW_LOCK);
    }
    launch_shown(path, true)
}

/// Ids of process `root` and of all processes it started, directly or not (by the parent ids
/// Windows recorded, which stay even after a launcher stub exits).
pub fn process_tree(root: u32) -> std::collections::HashSet<u32> {
    use windows::Win32::System::Diagnostics::ToolHelp::*;
    let mut tree = std::collections::HashSet::from([root]);
    let mut pairs = Vec::new();
    unsafe {
        let Ok(snap) = CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0) else {
            return tree;
        };
        let mut e = PROCESSENTRY32W {
            dwSize: size_of::<PROCESSENTRY32W>() as u32,
            ..Default::default()
        };
        let mut ok = Process32FirstW(snap, &mut e).is_ok();
        while ok {
            pairs.push((e.th32ProcessID, e.th32ParentProcessID));
            ok = Process32NextW(snap, &mut e).is_ok();
        }
        let _ = CloseHandle(snap);
    }
    loop {
        let before = tree.len();
        for &(pid, parent) in &pairs {
            if pid != parent && tree.contains(&parent) {
                tree.insert(pid);
            }
        }
        if tree.len() == before {
            return tree;
        }
    }
}

pub fn unlock_foreground() {
    unsafe {
        let _ = LockSetForegroundWindow(LSFW_UNLOCK);
    }
}

fn launch_shown(path: &str, behind: bool) -> Option<u32> {
    if path.is_empty() {
        return None;
    }
    // PowerShell scripts open in an editor by default: run them instead.
    if path.to_lowercase().ends_with(".ps1") {
        let args = format!("-NoProfile -ExecutionPolicy Bypass -File \"{path}\"");
        return launch_with("powershell.exe", &args, path, behind);
    }
    launch_with(path, "", path, behind)
}

/// `ShellExecuteEx` "open" of `file` with `args`, in the folder of `path`. Returns the started
/// process id (0 if none was started), or None on failure.
fn launch_with(file: &str, args: &str, path: &str, behind: bool) -> Option<u32> {
    unsafe {
        if !behind {
            let _ = AllowSetForegroundWindow(ASFW_ANY);
        }
        let wpath = wide(file);
        let wargs = wide(args);
        let dir = std::path::Path::new(path)
            .parent()
            .map(|p| wide(&p.to_string_lossy()));
        let mut info = SHELLEXECUTEINFOW {
            cbSize: size_of::<SHELLEXECUTEINFOW>() as u32,
            fMask: SEE_MASK_NOCLOSEPROCESS,
            lpVerb: w!("open"),
            lpFile: PCWSTR(wpath.as_ptr()),
            lpParameters: if args.is_empty() {
                PCWSTR::null()
            } else {
                PCWSTR(wargs.as_ptr())
            },
            lpDirectory: dir
                .as_ref()
                .map(|d| PCWSTR(d.as_ptr()))
                .unwrap_or(PCWSTR::null()),
            nShow: if behind { SW_SHOWNOACTIVATE } else { SW_SHOWNORMAL }.0,
            ..Default::default()
        };
        ShellExecuteExW(&mut info).ok()?;
        if info.hProcess.is_invalid() {
            return Some(0);
        }
        let pid = GetProcessId(info.hProcess);
        let _ = CloseHandle(info.hProcess);
        Some(pid)
    }
}

pub fn message_box(text: &str) {
    let t = wide(text);
    unsafe {
        MessageBoxW(
            None,
            PCWSTR(t.as_ptr()),
            w!("Window3D"),
            MB_OK | MB_ICONWARNING,
        );
    }
}

/// Builds an HICON from a BGRA image with alpha (used for the tray icon).
pub fn make_icon(img: &Image) -> HICON {
    unsafe {
        let color = CreateBitmap(
            img.w as i32,
            img.h as i32,
            1,
            32,
            Some(img.px.as_ptr() as *const c_void),
        );
        let mask_bits = vec![0u8; (img.w.div_ceil(16) * 2 * img.h) as usize];
        let mask = CreateBitmap(
            img.w as i32,
            img.h as i32,
            1,
            1,
            Some(mask_bits.as_ptr() as *const c_void),
        );
        let info = ICONINFO {
            fIcon: TRUE,
            xHotspot: 0,
            yHotspot: 0,
            hbmMask: mask,
            hbmColor: color,
        };
        let icon = CreateIconIndirect(&info).unwrap_or_default();
        let _ = DeleteObject(color.into());
        let _ = DeleteObject(mask.into());
        icon
    }
}

/// The app icon (`icon.rs`) as an image, for the tray.
pub fn app_icon_image(n: u32) -> Image {
    Image {
        w: n,
        h: n,
        px: crate::icon::bgra(n),
    }
}

/// Window bounds as seen by Windows.Graphics.Capture (the visible frame, without the shadow).
pub fn frame_bounds(hwnd: HWND) -> Option<RECT> {
    // The desktop card shows only the primary monitor (see `desktop_layout`).
    if is_desktop(hwnd) {
        return primary_rect();
    }
    let mut r = RECT::default();
    unsafe {
        DwmGetWindowAttribute(
            hwnd,
            DWMWA_EXTENDED_FRAME_BOUNDS,
            &mut r as *mut RECT as *mut c_void,
            size_of::<RECT>() as u32,
        )
        .or_else(|_| GetWindowRect(hwnd, &mut r))
        .ok()?;
    }
    Some(r)
}

/// The screen point of window `top` at (`u`, `v`) in [0, 1] across its captured image.
pub fn card_point(top: isize, u: f32, v: f32) -> Option<POINT> {
    let r = frame_bounds(hwnd_of(top))?;
    Some(POINT {
        x: r.left + (u.clamp(0.0, 1.0) * (r.right - r.left - 1) as f32).round() as i32,
        y: r.top + (v.clamp(0.0, 1.0) * (r.bottom - r.top - 1) as f32).round() as i32,
    })
}

/// Keeps the cursor within the overlay (and window `top`, if it reaches past it), so it can't
/// stray onto another monitor and click something there. A tighter clip (an app's own) is left
/// alone; a looser one or none gets ours.
pub fn confine_cursor(overlay: isize, top: isize) {
    unsafe {
        let mut want = RECT::default();
        if GetWindowRect(hwnd_of(overlay), &mut want).is_err() {
            return;
        }
        if let Some(w) = frame_bounds(hwnd_of(top)) {
            want.left = want.left.min(w.left);
            want.top = want.top.min(w.top);
            want.right = want.right.max(w.right);
            want.bottom = want.bottom.max(w.bottom);
        }
        let mut c = RECT::default();
        if GetClipCursor(&mut c).is_err() {
            return;
        }
        let looser = c.left <= want.left
            && c.top <= want.top
            && c.right >= want.right
            && c.bottom >= want.bottom;
        if looser && c != want {
            let _ = ClipCursor(Some(&want));
        }
    }
}

/// For the log: where the cursor is, its clip, window `top`'s bounds, what is under the
/// cursor and which window has the focus.
pub fn cursor_report(top: isize, push: (f32, f32), through: bool) -> String {
    let mut cur = POINT::default();
    unsafe {
        let _ = GetCursorPos(&mut cur);
    }
    let rect = |r: Option<RECT>| match r {
        Some(r) => format!("({},{})-({},{})", r.left, r.top, r.right, r.bottom),
        None => "?".into(),
    };
    let under = unsafe { WindowFromPoint(cur) };
    let under = unsafe { GetAncestor(under, GA_ROOT) };
    let fg = unsafe { GetForegroundWindow() };
    format!(
        "cursor ({},{}) clip {} window {} push ({:.0},{:.0}) through {through}; under: {}; focus: {}",
        cur.x,
        cur.y,
        rect(cursor_clip()),
        rect(frame_bounds(hwnd_of(top))),
        push.0,
        push.1,
        describe(under),
        describe(fg),
    )
}

/// `class "exe"` of window `h`, for the log.
pub fn describe(h: HWND) -> String {
    let i = win_info(h);
    format!("{:?} {:?}", i.class, i.exe)
}

/// The top-level window the user sees at `pt`: the first one down the z-order that is
/// visible there, skipping Window3D's own windows (the overlay, the pointer) and windows that
/// let the mouse through.
pub fn window_at(pt: POINT) -> HWND {
    let me = std::process::id();
    unsafe {
        let mut h = GetTopWindow(None).unwrap_or_default();
        while !h.is_invalid() {
            let mut r = RECT::default();
            let ex = GetWindowLongW(h, GWL_EXSTYLE) as u32;
            // WS_EX_TRANSPARENT lets the mouse through only with WS_EX_LAYERED.
            let through = WS_EX_TRANSPARENT.0 | WS_EX_LAYERED.0;
            if IsWindowVisible(h).as_bool()
                && ex & through != through
                && GetWindowRect(h, &mut r).is_ok()
                && pt.x >= r.left
                && pt.x < r.right
                && pt.y >= r.top
                && pt.y < r.bottom
                && window_pid(h) != me
                && !is_cloaked(h)
            {
                return h;
            }
            h = GetWindow(h, GW_HWNDNEXT).unwrap_or_default();
        }
    }
    HWND::default()
}

/// Whether window `hit` is part of window `top` as the user sees it: `top` itself, a window
/// it owns (dialogs, drop-downs), a menu or tooltip of its process, or for the desktop the
/// window that draws its icons.
pub fn part_of(hit: HWND, top: isize) -> bool {
    let top = hwnd_of(top);
    if hit == top {
        return true;
    }
    if !hit.is_invalid() && root_owner(hit) == root_owner(top) {
        return true;
    }
    if adopted(hit, top) {
        return true;
    }
    let class = window_class(hit);
    if window_class(top) == "Progman" && desktop_part(hit, &class) {
        return true;
    }
    // Another main window of the same app (a second browser window, maybe on another monitor)
    // is not part of it, though `uncover_chromium` made it a tool window: it would be hidden
    // from the screen as a popup of this one (see `popups_of`).
    let style = unsafe { GetWindowLongW(hit, GWL_STYLE) } as u32;
    let ex = unsafe { GetWindowLongW(hit, GWL_EXSTYLE) } as u32;
    if tooled(hit) || style & WS_CAPTION.0 == WS_CAPTION.0 || ex & WS_EX_APPWINDOW.0 != 0 {
        return false;
    }
    // Menus, drop-downs and bubbles of its process, owned or not (a popup without a caption
    // that Alt+Tab doesn't list).
    window_pid(hit) == window_pid(top)
        && !class.starts_with("Shell_")
        && (class == "#32768"
            || ex & (WS_EX_TOOLWINDOW.0 | WS_EX_TOPMOST.0) != 0
            || style & WS_POPUP.0 != 0 && !is_switchable(hit))
}

/// Windows of the app used in place taken as popups of it (see [`adopt_popup`]), with the
/// window each belongs to.
static ADOPTED: std::sync::Mutex<Vec<(isize, isize)>> = std::sync::Mutex::new(Vec::new());

fn adopted(h: HWND, top: HWND) -> bool {
    let (h, top) = (h.0 as isize, top.0 as isize);
    ADOPTED.lock().unwrap_or_else(|e| e.into_inner()).iter().any(|&(p, t)| p == h && t == top)
}

/// Whether window `h`, which just came up or took the foreground, is a popup of window `top`
/// (used in place) though it looks like a main window: a sign-in window a page opened (Google,
/// Microsoft, GitHub...), captioned and with a taskbar button but not owned by `top`. It is
/// taken for one if it is `top`'s process, came up within 3 s of a click on `top`, and lies
/// over `top`, well inside it and smaller (a new main window is about as large as `top`, or
/// reaches past it). From then on it is part of `top` (see [`part_of`]): it shows on `top`'s
/// card where it is over `top`, and taking the foreground it doesn't end the use. Owned
/// dialogs, menus and bubbles are part of `top` anyway (true for them too).
pub fn adopt_popup(h: HWND, top: isize) -> bool {
    let t = hwnd_of(top);
    if h == t || h.is_invalid() {
        return false;
    }
    if part_of(h, top) {
        return true;
    }
    let ok = unsafe { IsWindowVisible(h).as_bool() }
        && !is_cloaked(h)
        && !is_desktop(t)
        && !tooled(h)
        && window_pid(h) == window_pid(t)
        && crate::wall::pressed_within(3000);
    let (Some(f), Some(p)) = (frame_bounds(t), frame_bounds(h)) else {
        return false;
    };
    let area = |r: &RECT| ((r.right - r.left).max(0) as i64) * ((r.bottom - r.top).max(0) as i64);
    const M: i32 = 24;
    let inside = p.left >= f.left - M && p.top >= f.top - M && p.right <= f.right + M && p.bottom <= f.bottom + M;
    if !ok || !inside || area(&p) == 0 || area(&p) * 10 > area(&f) * 6 {
        return false;
    }
    let mut list = ADOPTED.lock().unwrap_or_else(|e| e.into_inner());
    list.retain(|&(p, _)| unsafe { IsWindow(Some(hwnd_of(p))).as_bool() });
    list.push((h.0 as isize, top));
    true
}

/// Dialog `h` of window `top` (used in place) that lies partly off `top`: `top`'s card can't
/// show it all, so it is moved over `top`'s middle (a dialog centred on the monitor over a
/// small window, or one remembered from elsewhere). Menus and bubbles (no caption) stay put.
pub fn fit_dialog(h: HWND, top: isize) -> bool {
    let style = unsafe { GetWindowLongW(h, GWL_STYLE) } as u32;
    if style & WS_CAPTION.0 != WS_CAPTION.0 || !shows_off(h, hwnd_of(top)) {
        return false;
    }
    let (Some(f), Some(p)) = (frame_bounds(hwnd_of(top)), frame_bounds(h)) else {
        return false;
    };
    let (w, ht) = (p.right - p.left, p.bottom - p.top);
    if w > f.right - f.left || ht > f.bottom - f.top {
        return false;
    }
    let (x, y) = (f.left + (f.right - f.left - w) / 2, f.top + (f.bottom - f.top - ht) / 2);
    let mut r = RECT::default();
    unsafe {
        GetWindowRect(h, &mut r).is_ok()
            && SetWindowPos(
                h,
                None,
                r.left + x - p.left,
                r.top + y - p.top,
                0,
                0,
                SWP_NOSIZE | SWP_NOZORDER | SWP_NOACTIVATE | SWP_NOOWNERZORDER,
            )
            .is_ok()
    }
}

/// Whether `hit` belongs with the desktop when it is used in place: its wallpaper, the taskbars
/// and the shell's Start menu, search, flyouts and tray overflow.
fn desktop_part(hit: HWND, class: &str) -> bool {
    const SHELL: &[&str] = &["StartMenuExperienceHost.exe", "SearchHost.exe", "ShellExperienceHost.exe", "ShellHost.exe"];
    // Popups taken in by `note_desktop_popup` (a tray icon's menu), whatever their class: a
    // "#32768" menu of another app is no shell popup below.
    if DESKTOP_IN_PLACE.load(std::sync::atomic::Ordering::Relaxed)
        && DESKTOP_POPUPS.lock().unwrap_or_else(|e| e.into_inner()).contains(&(hit.0 as isize))
    {
        return true;
    }
    match class {
        "WorkerW"
        | "Shell_TrayWnd"
        | "Shell_SecondaryTrayWnd"
        | "XamlExplorerHostIslandWindow"
        | "TopLevelWindowForOverflowXamlIsland"
        | "NotifyIconOverflowWindow" => true,
        "Windows.UI.Core.CoreWindow" => {
            let exe = process_path_cached(window_pid(hit));
            SHELL.iter().any(|s| exe.ends_with(&s.to_ascii_lowercase()))
        }
        // The Widgets board (the weather button) and its WebView2 popups.
        "WindowsDashboard" => true,
        // Quick settings (the sound, network and battery icons) of ShellHost.exe.
        "ControlCenterWindow" => true,
        "Chrome_WidgetWin_0" | "Chrome_WidgetWin_1" => widgets_process(window_pid(hit), SHELL),
        // The desktop's right-click menu (WinUI, or the classic one under "Show more options").
        "Microsoft.UI.Content.PopupWindowSiteBridge" | "XamlExplorerHostIslandWindow_WASDK" | "#32768" => {
            DESKTOP_IN_PLACE.load(std::sync::atomic::Ordering::Relaxed)
                && process_path_cached(window_pid(hit)).ends_with("\\explorer.exe")
                // The Widgets board's and the shell hosts' menus (a widget's "..." menu).
                || shell_host(window_pid(hit), SHELL)
        }
        // The drag image of icons being dragged and the icons' tooltips, which show by the
        // real cursor (taskbar tooltips are owned by it, below).
        "SysDragImage" | "tooltips_class32"
            if DESKTOP_IN_PLACE.load(std::sync::atomic::Ordering::Relaxed)
                && unsafe { GetWindow(hit, GW_OWNER) }.map_or(true, |o| o.is_invalid())
                && process_path_cached(window_pid(hit)).ends_with("\\explorer.exe") =>
        {
            true
        }
        _ => {
            // Menus and flyouts of the taskbar (owned by it), and whatever else the shell's
            // hosts show: the value bubble of a Quick Settings slider, tooltips of Start.
            let owner = unsafe { GetAncestor(hit, GA_ROOTOWNER) };
            // The Widgets board's dialogs and popups too (Widgets.exe, its WebView2), and
            // whatever a shell popup owns.
            (window_class(owner).starts_with("Shell_") && owner != hit)
                || (owner != hit && !owner.is_invalid() && desktop_part(owner, &window_class(owner)))
                || shell_host(window_pid(hit), SHELL)
                    && unsafe { GetWindowLongW(hit, GWL_STYLE) } as u32 & WS_CHILD.0 == 0
        }
    }
}

/// Whether process `pid` is one of the shell's hosts (`shell`: Start, search...), the Widgets
/// board (Widgets.exe), or the WebView2 of one of them.
fn shell_host(pid: u32, shell: &[&str]) -> bool {
    let exe = process_path_cached(pid);
    exe.ends_with("\\widgets.exe")
        || shell.iter().any(|s| exe.ends_with(&s.to_ascii_lowercase()))
        || widgets_process(pid, shell)
}

/// [`process_path`] in lower case, cached per process (the shell popup search runs often).
fn process_path_cached(pid: u32) -> String {
    static CACHE: std::sync::Mutex<Vec<(u32, String)>> = std::sync::Mutex::new(Vec::new());
    let mut cache = CACHE.lock().unwrap_or_else(|e| e.into_inner());
    if let Some((_, p)) = cache.iter().find(|c| c.0 == pid) {
        return p.clone();
    }
    let p = process_path(pid).to_ascii_lowercase();
    if cache.len() > 512 {
        cache.clear();
    }
    cache.push((pid, p.clone()));
    p
}

/// Whether process `pid` is the WebView2 of the Widgets board or of a shell host in `shell`
/// (started by it, directly or through the WebView2 browser process). Cached per process.
fn widgets_process(pid: u32, shell: &[&str]) -> bool {
    use windows::Win32::System::Diagnostics::ToolHelp::*;
    static CACHE: std::sync::Mutex<Vec<(u32, bool)>> = std::sync::Mutex::new(Vec::new());
    let mut cache = CACHE.lock().unwrap_or_else(|e| e.into_inner());
    if let Some(&(_, w)) = cache.iter().find(|c| c.0 == pid) {
        return w;
    }
    let name = |p: u32| process_path(p).rsplit('\\').next().unwrap_or("").to_ascii_lowercase();
    let mut w = false;
    if name(pid) == "msedgewebview2.exe" {
        let mut parents = std::collections::HashMap::new();
        unsafe {
            if let Ok(snap) = CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0) {
                let mut e = PROCESSENTRY32W {
                    dwSize: size_of::<PROCESSENTRY32W>() as u32,
                    ..Default::default()
                };
                let mut ok = Process32FirstW(snap, &mut e).is_ok();
                while ok {
                    parents.insert(e.th32ProcessID, e.th32ParentProcessID);
                    ok = Process32NextW(snap, &mut e).is_ok();
                }
                let _ = CloseHandle(snap);
            }
        }
        let mut p = pid;
        for _ in 0..3 {
            match parents.get(&p) {
                Some(&up) if up != p => p = up,
                _ => break,
            }
            let n = name(p);
            if n == "widgets.exe" || shell.iter().any(|s| s.eq_ignore_ascii_case(&n)) {
                w = true;
                break;
            }
        }
    }
    if cache.len() > 256 {
        cache.clear();
    }
    cache.push((pid, w));
    w
}

/// Whether screen point `pt` is on window `h`.
pub fn contains(h: isize, pt: POINT) -> bool {
    let mut r = RECT::default();
    (unsafe { GetWindowRect(hwnd_of(h), &mut r).is_ok() })
        && pt.x >= r.left
        && pt.x < r.right
        && pt.y >= r.top
        && pt.y < r.bottom
}

/// Whether screen point `pt` is inside window `top`'s visible frame (not its invisible
/// resize borders).
pub fn in_frame(top: isize, pt: POINT) -> bool {
    frame_bounds(hwnd_of(top)).is_none_or(|r| {
        pt.x >= r.left && pt.x < r.right && pt.y >= r.top && pt.y < r.bottom
    })
}

/// The rectangle the cursor is clipped to now (the whole desktop when unclipped).
pub fn cursor_clip() -> Option<RECT> {
    let mut c = RECT::default();
    unsafe { GetClipCursor(&mut c).ok().map(|_| c) }
}

/// Where `screen` is across window `top`'s captured image: (`u`, `v`) in [0, 1] inside it,
/// outside that range off it. The inverse of `card_point`.
pub fn window_uv(top: isize, screen: POINT) -> Option<(f32, f32)> {
    let r = frame_bounds(hwnd_of(top))?;
    let (w, h) = ((r.right - r.left - 1).max(1) as f32, (r.bottom - r.top - 1).max(1) as f32);
    Some(((screen.x - r.left) as f32 / w, (screen.y - r.top) as f32 / h))
}

/// The rectangle of the monitor window `h` is (mostly) on.
pub fn monitor_of(h: HWND) -> Option<RECT> {
    unsafe {
        let mon = MonitorFromWindow(h, MONITOR_DEFAULTTONEAREST);
        let mut mi = MONITORINFO {
            cbSize: size_of::<MONITORINFO>() as u32,
            ..Default::default()
        };
        GetMonitorInfoW(mon, &mut mi).as_bool().then_some(mi.rcMonitor)
    }
}

/// The work area of the monitor window `h` is (mostly) on.
fn work_area_of(h: HWND) -> Option<RECT> {
    unsafe {
        let mon = MonitorFromWindow(h, MONITOR_DEFAULTTONEAREST);
        let mut mi = MONITORINFO {
            cbSize: size_of::<MONITORINFO>() as u32,
            ..Default::default()
        };
        GetMonitorInfoW(mon, &mut mi).as_bool().then_some(mi.rcWork)
    }
}

/// The rectangle of the monitor under the cursor.
pub fn cursor_monitor() -> Option<RECT> {
    unsafe {
        let mut pt = POINT::default();
        GetCursorPos(&mut pt).ok()?;
        let mon = MonitorFromPoint(pt, MONITOR_DEFAULTTONEAREST);
        let mut mi = MONITORINFO {
            cbSize: size_of::<MONITORINFO>() as u32,
            ..Default::default()
        };
        GetMonitorInfoW(mon, &mut mi).as_bool().then_some(mi.rcMonitor)
    }
}

/// Moves window `h` onto the monitor of window `to`, at the same place in its work area (kept
/// inside it), still maximized if it was. False if it was already there.
pub fn move_to_monitor_of(h: HWND, to: HWND) -> bool {
    let (Some(from), Some(dst)) = (work_area_of(h), work_area_of(to)) else {
        return false;
    };
    if from == dst {
        return false;
    }
    let (dx, dy) = (dst.left - from.left, dst.top - from.top);
    let fit = |r: RECT| {
        let w = (r.right - r.left).min(dst.right - dst.left);
        let h = (r.bottom - r.top).min(dst.bottom - dst.top);
        (r.left + dx, r.top + dy, w, h)
    };
    unsafe {
        if IsZoomed(h).as_bool() {
            // Maximized: its normal position (workspace coordinates, so only shifted, not
            // clamped) moves over for un-maximizing later, and the maximized window itself
            // is moved onto the other work area.
            let mut p = WINDOWPLACEMENT {
                length: size_of::<WINDOWPLACEMENT>() as u32,
                ..Default::default()
            };
            if GetWindowPlacement(h, &mut p).is_err() {
                return false;
            }
            let (x, y, w, ht) = fit(p.rcNormalPosition);
            p.rcNormalPosition = RECT {
                left: x,
                top: y,
                right: x + w,
                bottom: y + ht,
            };
            p.showCmd = SW_SHOWMAXIMIZED.0 as u32;
            let _ = SetWindowPlacement(h, &p);
            return SetWindowPos(
                h,
                None,
                dst.left,
                dst.top,
                dst.right - dst.left,
                dst.bottom - dst.top,
                SWP_NOZORDER | SWP_NOOWNERZORDER | SWP_NOACTIVATE,
            )
            .is_ok();
        }
        let mut r = RECT::default();
        if GetWindowRect(h, &mut r).is_err() {
            return false;
        }
        let (x, y, w, ht) = fit(r);
        let x = x.clamp(dst.left, dst.right - w);
        let y = y.clamp(dst.top, dst.bottom - ht);
        SetWindowPos(
            h,
            None,
            x,
            y,
            w,
            ht,
            SWP_NOZORDER | SWP_NOOWNERZORDER | SWP_NOACTIVATE,
        )
        .is_ok()
    }
}

/// Lets the cursor go anywhere and puts it at `pt`.
pub fn free_cursor_at(pt: POINT) {
    unsafe {
        let _ = ClipCursor(None);
        let _ = SetCursorPos(pt.x, pt.y);
    }
}

/// Brings a parked cursor back onto the overlay at `screen` (clipped to it with `clip`).
pub fn unpark_cursor(overlay: isize, screen: POINT, clip: bool) {
    let overlay = hwnd_of(overlay);
    unsafe {
        SetWindowRgn(overlay, None, false);
        let _ = ClipCursor(None);
        let _ = SetCursorPos(screen.x, screen.y);
        let mut r = RECT::default();
        if clip && GetWindowRect(overlay, &mut r).is_ok() {
            let _ = ClipCursor(Some(&r));
        }
    }
}

/// Posts a keyboard message to the focused control of window `top`'s thread (or to `top`).
pub fn forward_key(top: isize, msg: u32, wp: usize, lp: isize) {
    let top = hwnd_of(top);
    unsafe {
        let tid = GetWindowThreadProcessId(top, None);
        let mut gui = GUITHREADINFO {
            cbSize: size_of::<GUITHREADINFO>() as u32,
            ..Default::default()
        };
        let target = if GetGUIThreadInfo(tid, &mut gui).is_ok() && !gui.hwndFocus.is_invalid() {
            gui.hwndFocus
        } else {
            top
        };
        let _ = PostMessageW(Some(target), msg, WPARAM(wp), LPARAM(lp));
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn process_tree_finds_children() {
        let me = std::process::id();
        let mut child = std::process::Command::new("cmd")
            .args(["/c", "ping -n 3 127.0.0.1 >nul"])
            .spawn()
            .unwrap();
        let tree = super::process_tree(me);
        let _ = child.kill();
        assert!(tree.contains(&me));
        assert!(tree.contains(&child.id()));
    }
}


/// Whether window `h` is minimized.
pub fn is_minimized(h: HWND) -> bool {
    unsafe { IsIconic(h).as_bool() }
}

/// Restores a minimized window without activating it (a minimized window gives no live
/// frames). Asynchronous, so a hung window can't block us. Returns whether it was minimized.
pub fn restore_quietly(h: HWND) -> bool {
    unsafe {
        if !IsIconic(h).as_bool() {
            return false;
        }
        let _ = ShowWindowAsync(h, SW_SHOWNOACTIVATE);
        true
    }
}

const RUN_KEY: PCWSTR = w!(r"Software\Microsoft\Windows\CurrentVersion\Run");
const RUN_VALUE: PCWSTR = w!("Window3D");

/// Whether Window3D starts with Windows (a value under the user's `Run` key).
pub fn autostart() -> bool {
    use windows::Win32::System::Registry::*;
    unsafe {
        RegGetValueW(HKEY_CURRENT_USER, RUN_KEY, RUN_VALUE, RRF_RT_REG_SZ, None, None, None)
            .is_ok()
    }
}

/// Makes Window3D start with Windows (quietly: no start-up balloon), or not.
pub fn set_autostart(on: bool) {
    use windows::Win32::System::Registry::*;
    unsafe {
        let r = if on {
            let Ok(exe) = std::env::current_exe() else { return };
            let cmd = wide(&format!("\"{}\" --quiet", exe.display()));
            RegSetKeyValueW(
                HKEY_CURRENT_USER,
                RUN_KEY,
                RUN_VALUE,
                REG_SZ.0,
                Some(cmd.as_ptr() as *const c_void),
                (cmd.len() * 2) as u32,
            )
        } else {
            RegDeleteKeyValueW(HKEY_CURRENT_USER, RUN_KEY, RUN_VALUE)
        };
        if r.is_err() {
            crate::store::log(&format!("could not change start with Windows: {r:?}"));
        }
    }
}
