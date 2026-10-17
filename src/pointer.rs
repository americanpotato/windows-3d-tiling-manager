//! The pointer while a card is used in place. For real hover the real cursor is parked on the
//! real window (behind a 1-pixel hole in the overlay), so it must not be seen there: the system
//! cursors are swapped for blank ones meanwhile and a small window of ours draws the shape the
//! app asked for where the pointer is on the card. The overlay shows the (blank) system arrow, so
//! only the pointer is seen. A marker file lets the next start put the cursors back after a crash.

use std::cell::RefCell;
use std::ffi::c_void;
use std::mem::size_of;
use std::sync::atomic::{AtomicBool, AtomicU8, AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use windows::Win32::Foundation::*;
use windows::Win32::Graphics::Gdi::*;
use windows::Win32::System::LibraryLoader::{GetModuleHandleW, GetProcAddress, LoadLibraryW};
use windows::Win32::UI::WindowsAndMessaging::*;
use windows::core::{BOOL, PCWSTR, s, w};

/// System cursors that apps pick by id (`LoadCursor(NULL, IDC_*)`).
const IDS: [u32; 16] = [
    32512, 32513, 32514, 32515, 32516, 32642, 32643, 32644, 32645, 32646, 32648, 32649, 32650,
    32651, 32671, 32672,
];

static HIDDEN: AtomicBool = AtomicBool::new(false);
/// The user's "hide pointer while typing" setting while it is turned off: 0 = untouched,
/// 1 = was off, 2 = was on. With it on, typing into a card clears `CURSOR_SHOWING` and the
/// drawn pointer flickers.
static VANISH: AtomicU8 = AtomicU8::new(0);
/// When a key was last passed to a card (ms since the epoch).
static TYPED: AtomicU64 = AtomicU64::new(0);

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_millis() as u64)
}

/// Notes a key passed to a card, so the pointer stays drawn while typing.
pub fn typed() {
    TYPED.store(now_ms(), Ordering::Relaxed);
}

fn get_vanish() -> bool {
    let mut on = BOOL(0);
    unsafe {
        let _ = SystemParametersInfoW(
            SPI_GETMOUSEVANISH,
            0,
            Some(&mut on as *mut BOOL as *mut c_void),
            SYSTEM_PARAMETERS_INFO_UPDATE_FLAGS(0),
        );
    }
    on.as_bool()
}

/// Sets "hide pointer while typing" for this session only (flags 0: not written to the profile).
fn set_vanish(on: bool) {
    unsafe {
        let _ = SystemParametersInfoW(
            SPI_SETMOUSEVANISH,
            0,
            Some(on as usize as *mut c_void),
            SYSTEM_PARAMETERS_INFO_UPDATE_FLAGS(0),
        );
    }
}

/// Turns "hide pointer while typing" back on if it was turned off.
fn restore_vanish() {
    if VANISH.swap(0, Ordering::SeqCst) == 2 {
        set_vanish(true);
    }
}

#[derive(Default)]
struct State {
    /// (shared handle before, shared handle after blanking, copy of the original image).
    shapes: Vec<(isize, isize, isize)>,
    hwnd: isize,
    /// Cursor drawn in the window and its hotspot.
    drawn: isize,
    hot: (i32, i32),
    shown: bool,
    /// Where the window was last put, and when (ms).
    placed: ((i32, i32), u64),
}

thread_local! {
    static STATE: RefCell<State> = RefCell::new(State::default());
}

fn marker() -> std::path::PathBuf {
    crate::store::app_dir().join("cursors-hidden")
}

fn reload_cursors() {
    unsafe {
        let _ = SystemParametersInfoW(
            SPI_SETCURSORS,
            0,
            None,
            SYSTEM_PARAMETERS_INFO_UPDATE_FLAGS(0),
        );
    }
}

/// Hides (or shows again) the real cursor whatever its shape, through the Magnification API.
/// Blank system cursors are not enough: an app's own cursors (Chrome's middle-click scroll
/// arrows, grab hands, zoom, drag-copy, a page's CSS cursor) are not system cursors and would
/// stay seen where the real cursor is parked, a second pointer beside the drawn one. The
/// cursor shows again when the process ends.
fn mag_cursor(show: bool) {
    type Show = unsafe extern "system" fn(BOOL) -> BOOL;
    static SHOW: std::sync::OnceLock<Option<Show>> = std::sync::OnceLock::new();
    let f = SHOW.get_or_init(|| unsafe {
        let lib = LoadLibraryW(w!("Magnification.dll")).ok()?;
        let init: unsafe extern "system" fn() -> BOOL = std::mem::transmute(GetProcAddress(lib, s!("MagInitialize"))?);
        if !init().as_bool() {
            return None;
        }
        Some(std::mem::transmute::<_, Show>(GetProcAddress(lib, s!("MagShowSystemCursor"))?))
    });
    if let Some(f) = f {
        unsafe {
            let _ = f(BOOL::from(show));
        }
    }
}

/// At start: puts the cursors back if Window3D ended while they were blank.
pub fn recover() {
    if marker().exists() {
        reload_cursors();
        if std::fs::read(marker()).is_ok_and(|m| m.contains(&b'v')) {
            set_vanish(true);
        }
        unsafe {
            let _ = ClipCursor(None);
        }
        let _ = std::fs::remove_file(marker());
        crate::store::log("cursors restored after an unclean exit");
    }
}

/// From the panic hook (any thread).
pub fn panic_restore() {
    if HIDDEN.swap(false, Ordering::SeqCst) {
        reload_cursors();
        restore_vanish();
        unsafe {
            let _ = ClipCursor(None);
        }
        let _ = std::fs::remove_file(marker());
    }
}

/// Swaps the system cursors for blank ones (keeping copies to draw).
pub fn hide_cursors() {
    if HIDDEN.load(Ordering::SeqCst) {
        return;
    }
    let vanish = get_vanish();
    let _ = std::fs::write(marker(), if vanish { b"1v".as_slice() } else { b"1" });
    HIDDEN.store(true, Ordering::SeqCst);
    VANISH.store(if vanish { 2 } else { 1 }, Ordering::SeqCst);
    if vanish {
        set_vanish(false);
    }
    STATE.with_borrow_mut(|s| unsafe {
        for (_, _, copy) in s.shapes.drain(..) {
            let _ = DestroyCursor(HCURSOR(copy as *mut c_void));
        }
        let and = [0xffu8; 128];
        let xor = [0u8; 128];
        let Ok(blank) = CreateCursor(
            None,
            0,
            0,
            32,
            32,
            and.as_ptr() as *const c_void,
            xor.as_ptr() as *const c_void,
        ) else {
            return;
        };
        for id in IDS {
            let Ok(h) = LoadCursorW(None, PCWSTR(id as usize as *const u16)) else {
                continue;
            };
            let Ok(copy) = CopyIcon(HICON(h.0)) else {
                continue;
            };
            if let Ok(b) = CopyIcon(HICON(blank.0)) {
                let _ = SetSystemCursor(HCURSOR(b.0), SYSTEM_CURSOR_ID(id));
            }
            let after = LoadCursorW(None, PCWSTR(id as usize as *const u16)).map_or(0, |a| a.0 as isize);
            s.shapes.push((h.0 as isize, after, copy.0 as isize));
        }
        let _ = DestroyCursor(blank);
    });
    mag_cursor(false);
}

/// Puts the user's cursors back.
pub fn restore_cursors() {
    hide();
    if HIDDEN.swap(false, Ordering::SeqCst) {
        mag_cursor(true);
        reload_cursors();
        restore_vanish();
        let _ = std::fs::remove_file(marker());
    }
}

/// Shows the pointer at `screen` with the shape the window under the real cursor chose.
pub fn show(screen: POINT) {
    let mut ci = CURSORINFO {
        cbSize: size_of::<CURSORINFO>() as u32,
        ..Default::default()
    };
    unsafe {
        if GetCursorInfo(&mut ci).is_err() || ci.flags.0 & CURSOR_SHOWING.0 == 0 || ci.hCursor.is_invalid() {
            // Hidden only because of typing (some apps hide it themselves): keep the last shape.
            let recent = now_ms().saturating_sub(TYPED.load(Ordering::Relaxed)) < 1500;
            let drawn = STATE.with_borrow(|s| s.drawn);
            if !recent || drawn == 0 {
                hide();
                return;
            }
            ci.hCursor = HCURSOR(drawn as *mut c_void);
        }
    }
    STATE.with_borrow_mut(|s| unsafe {
        if s.hwnd == 0 {
            s.hwnd = create_window();
            if s.hwnd == 0 {
                return;
            }
        }
        let cur = ci.hCursor.0 as isize;
        let image = s
            .shapes
            .iter()
            .find(|(b, a, _)| *b == cur || *a == cur)
            .map_or(cur, |x| x.2);
        let hwnd = HWND(s.hwnd as *mut c_void);
        if s.drawn != image {
            match render(hwnd, HICON(image as *mut c_void)) {
                Some(hot) => {
                    s.drawn = image;
                    s.hot = hot;
                }
                None => return,
            }
        }
        // Moving a topmost window is not free: skipped while nothing changed, apart from
        // re-asserting the top of the z-order now and then.
        let at = (screen.x - s.hot.0, screen.y - s.hot.1);
        let t = now_ms();
        if s.shown && s.placed.0 == at && t.saturating_sub(s.placed.1) < 250 {
            return;
        }
        let raise = !s.shown || t.saturating_sub(s.placed.1) >= 250;
        if raise {
            s.placed.1 = t;
        }
        s.placed.0 = at;
        let _ = SetWindowPos(
            hwnd,
            Some(HWND_TOPMOST),
            at.0,
            at.1,
            0,
            0,
            SWP_NOSIZE
                | SWP_NOACTIVATE
                | if raise { SWP_SHOWWINDOW } else { SWP_NOZORDER },
        );
        s.shown = true;
    });
}

/// Hides the pointer window.
pub fn hide() {
    STATE.with_borrow_mut(|s| {
        if std::mem::take(&mut s.shown) {
            unsafe {
                let _ = ShowWindow(HWND(s.hwnd as *mut c_void), SW_HIDE);
            }
        }
    });
}

extern "system" fn wndproc(hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    unsafe { DefWindowProcW(hwnd, msg, wp, lp) }
}

unsafe fn create_window() -> isize {
    unsafe {
        let hinst: HINSTANCE = GetModuleHandleW(None).map(|h| h.into()).unwrap_or_default();
        let wc = WNDCLASSEXW {
            cbSize: size_of::<WNDCLASSEXW>() as u32,
            lpfnWndProc: Some(wndproc),
            hInstance: hinst,
            lpszClassName: w!("Window3D.Pointer"),
            ..Default::default()
        };
        RegisterClassExW(&wc);
        CreateWindowExW(
            WS_EX_LAYERED | WS_EX_TRANSPARENT | WS_EX_TOPMOST | WS_EX_TOOLWINDOW | WS_EX_NOACTIVATE,
            w!("Window3D.Pointer"),
            None,
            WS_POPUP,
            0,
            0,
            1,
            1,
            None,
            None,
            Some(hinst),
            None,
        )
        .map_or(0, |h| {
            crate::win::no_capture(h.0 as isize, "the pointer");
            h.0 as isize
        })
    }
}

/// Draws cursor `cur` into the layered window; returns its hotspot. The cursor is drawn on
/// black and on white to recover its alpha; inverting parts (the I-beam) become black with a
/// white rim so they show on any background.
unsafe fn render(hwnd: HWND, cur: HICON) -> Option<(i32, i32)> {
    unsafe {
        let mut ii = ICONINFO::default();
        GetIconInfo(cur, &mut ii).ok()?;
        let mono = ii.hbmColor.is_invalid();
        let src = if mono { ii.hbmMask } else { ii.hbmColor };
        let mut bm = BITMAP::default();
        GetObjectW(
            HGDIOBJ(src.0),
            size_of::<BITMAP>() as i32,
            Some(&mut bm as *mut BITMAP as *mut c_void),
        );
        for b in [ii.hbmColor, ii.hbmMask] {
            if !b.is_invalid() {
                let _ = DeleteObject(HGDIOBJ(b.0));
            }
        }
        let (cw, ch) = (bm.bmWidth, if mono { bm.bmHeight / 2 } else { bm.bmHeight });
        if cw <= 0 || ch <= 0 || cw > 512 || ch > 512 {
            return None;
        }
        let (w, h) = (cw + 2, ch + 2);
        let screen = GetDC(None);
        let mem = CreateCompatibleDC(Some(screen));
        let bmi = BITMAPINFO {
            bmiHeader: BITMAPINFOHEADER {
                biSize: size_of::<BITMAPINFOHEADER>() as u32,
                biWidth: w,
                biHeight: -h,
                biPlanes: 1,
                biBitCount: 32,
                ..Default::default()
            },
            ..Default::default()
        };
        let mut bits: *mut c_void = std::ptr::null_mut();
        let Ok(dib) = CreateDIBSection(Some(mem), &bmi, DIB_RGB_COLORS, &mut bits, None, 0) else {
            let _ = DeleteDC(mem);
            ReleaseDC(None, screen);
            return None;
        };
        let old = SelectObject(mem, HGDIOBJ(dib.0));
        let n = (w * h) as usize;
        let px = std::slice::from_raw_parts_mut(bits as *mut u32, n);
        let mut pass = |bg: u32| {
            px.fill(bg);
            let _ = GdiFlush();
            let _ = DrawIconEx(mem, 1, 1, cur, cw, ch, 0, None, DI_NORMAL);
            let _ = GdiFlush();
            px.to_vec()
        };
        let black = pass(0);
        let white = pass(0x00ff_ffff);
        let mut out = vec![0u32; n];
        let mut inv = vec![false; n];
        for i in 0..n {
            let (b, wt) = (black[i], white[i]);
            let mut maxd = 0i32;
            let mut neg = false;
            for sh in [0, 8, 16] {
                let d = ((wt >> sh) & 0xff) as i32 - ((b >> sh) & 0xff) as i32;
                neg |= d < -64;
                maxd = maxd.max(d);
            }
            if neg {
                inv[i] = true;
                out[i] = 0xff00_0000;
                continue;
            }
            let a = (255 - maxd).clamp(0, 255) as u32;
            let ch = |sh: u32| ((b >> sh) & 0xff).min(a) << sh;
            out[i] = (a << 24) | ch(0) | ch(8) | ch(16);
        }
        let (wu, hu) = (w as usize, h as usize);
        for y in 0..hu {
            for x in 0..wu {
                let i = y * wu + x;
                if out[i] >> 24 != 0 {
                    continue;
                }
                let near = (x > 0 && inv[i - 1])
                    || (x + 1 < wu && inv[i + 1])
                    || (y > 0 && inv[i - wu])
                    || (y + 1 < hu && inv[i + wu]);
                if near {
                    out[i] = 0xffff_ffff;
                }
            }
        }
        px.copy_from_slice(&out);
        let blend = BLENDFUNCTION {
            BlendOp: AC_SRC_OVER as u8,
            BlendFlags: 0,
            SourceConstantAlpha: 255,
            AlphaFormat: AC_SRC_ALPHA as u8,
        };
        let ok = UpdateLayeredWindow(
            hwnd,
            Some(screen),
            None,
            Some(&SIZE { cx: w, cy: h }),
            Some(mem),
            Some(&POINT::default()),
            COLORREF(0),
            Some(&blend),
            ULW_ALPHA,
        )
        .is_ok();
        SelectObject(mem, old);
        let _ = DeleteObject(HGDIOBJ(dib.0));
        let _ = DeleteDC(mem);
        ReleaseDC(None, screen);
        ok.then_some((ii.xHotspot as i32 + 1, ii.yHotspot as i32 + 1))
    }
}
