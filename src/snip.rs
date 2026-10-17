//! Screenshots of the 3D view (Win+Shift+S, PrtScn, Win+PrtScn).
//!
//! The 3D view and the pointer window are left out of screen captures only while the desktop
//! card captures the primary monitor (else that capture would show the 3D view inside itself).
//! A low-level keyboard hook on its own thread sees the screenshot keys before the shell does,
//! and lets captures see the 3D view at once, until the Snipping Tool is done (the desktop card
//! may show itself for that while).

use std::sync::Mutex;
use std::sync::atomic::{AtomicI32, AtomicU64, AtomicUsize, Ordering};
use std::time::Instant;

use windows::Win32::Foundation::*;
use windows::Win32::UI::Input::KeyboardAndMouse::*;
use windows::Win32::UI::WindowsAndMessaging::*;

/// Windows of Window3D left out of captures while the desktop card captures the monitor.
static WINDOWS: Mutex<Vec<(isize, &'static str)>> = Mutex::new(Vec::new());
/// Desktop card captures open (`live.rs`).
static DESKTOP_CAPTURES: AtomicUsize = AtomicUsize::new(0);
/// When a screenshot key was last pressed (milliseconds since `START`, 0: none).
static SNIP_AT: AtomicU64 = AtomicU64::new(0);
/// What the windows were last given: 0 nothing yet, 1 left out, 2 captured.
static APPLIED: AtomicI32 = AtomicI32::new(0);
static START: Mutex<Option<Instant>> = Mutex::new(None);

fn now_ms() -> u64 {
    let mut s = START.lock().unwrap_or_else(|e| e.into_inner());
    s.get_or_insert_with(Instant::now).elapsed().as_millis() as u64 + 1
}

/// Window `h` (the 3D view, the pointer) is left out of screen captures while needed.
pub fn register(h: isize, what: &'static str) {
    WINDOWS.lock().unwrap_or_else(|e| e.into_inner()).push((h, what));
    APPLIED.store(0, Ordering::SeqCst);
    let _ = sync();
}

/// A desktop card capture of the primary monitor was opened (`true`) or closed. Returns whether
/// that changed what the windows were given.
pub fn desktop_capture(open: bool) -> bool {
    if open {
        DESKTOP_CAPTURES.fetch_add(1, Ordering::SeqCst);
    } else {
        let _ = DESKTOP_CAPTURES.try_update(Ordering::SeqCst, Ordering::SeqCst, |n| n.checked_sub(1));
    }
    sync()
}

/// The Snipping Tool (its capture overlay and editor).
pub fn is_tool(h: isize) -> bool {
    let fg = crate::win::hwnd_of(h);
    if fg.is_invalid() {
        return false;
    }
    crate::win::process_path(crate::win::window_pid(fg))
        .to_ascii_lowercase()
        .ends_with("snippingtool.exe")
}

/// A screenshot is being taken: a key was pressed in the last 2 s, or the Snipping Tool it
/// opened still has the foreground.
pub fn active() -> bool {
    let at = SNIP_AT.load(Ordering::SeqCst);
    if at == 0 {
        return false;
    }
    if now_ms().saturating_sub(at) < 2000 || is_tool(unsafe { GetForegroundWindow() }.0 as isize) {
        return true;
    }
    SNIP_AT.store(0, Ordering::SeqCst);
    crate::store::log("screenshot done: the 3D view is left out of captures again while needed");
    false
}

/// Gives the windows what they need now. Cheap when nothing changed; called every frame.
/// Returns whether that changed anything.
pub fn sync() -> bool {
    let exclude = DESKTOP_CAPTURES.load(Ordering::SeqCst) > 0 && !active();
    let want = if exclude { 1 } else { 2 };
    if APPLIED.swap(want, Ordering::SeqCst) == want {
        return false;
    }
    let mut wins = WINDOWS.lock().unwrap_or_else(|e| e.into_inner());
    wins.retain(|&(h, _)| unsafe { IsWindow(Some(crate::win::hwnd_of(h))).as_bool() });
    for &(h, what) in wins.iter() {
        let aff = if exclude { WDA_EXCLUDEFROMCAPTURE } else { WDA_NONE };
        if let Err(e) = unsafe { SetWindowDisplayAffinity(crate::win::hwnd_of(h), aff) } {
            crate::store::log(&format!("{what}: screen capture setting not changed: {e}"));
        }
    }
    !wins.is_empty()
}

unsafe extern "system" fn hook(code: i32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    unsafe {
        if code >= 0 && (wp.0 == WM_KEYDOWN as usize || wp.0 == WM_SYSKEYDOWN as usize) {
            let k = &*(lp.0 as *const KBDLLHOOKSTRUCT);
            let down = |vk: VIRTUAL_KEY| GetAsyncKeyState(vk.0 as i32) < 0;
            let win = down(VK_LWIN) || down(VK_RWIN);
            let key = if k.vkCode == VK_SNAPSHOT.0 as u32 {
                Some(if win { "Win+PrtScn" } else { "PrtScn" })
            } else if k.vkCode == 'S' as u32 && win && down(VK_SHIFT) {
                Some("Win+Shift+S")
            } else {
                None
            };
            if let Some(key) = key {
                let first = SNIP_AT.swap(now_ms(), Ordering::SeqCst) == 0;
                let _ = sync();
                if first && !WINDOWS.lock().unwrap_or_else(|e| e.into_inner()).is_empty() {
                    crate::store::log(&format!("{key}: screen captures show the 3D view"));
                }
            }
        }
        CallNextHookEx(None, code, wp, lp)
    }
}

/// Starts the keyboard hook thread (for the whole run).
pub fn start() {
    let _ = std::thread::Builder::new().name("snip".into()).spawn(|| unsafe {
        let Ok(hook) = SetWindowsHookExW(WH_KEYBOARD_LL, Some(hook), None, 0) else {
            crate::store::log("screenshot key hook not set");
            return;
        };
        let mut msg = MSG::default();
        while GetMessageW(&mut msg, None, 0, 0).as_bool() {}
        let _ = UnhookWindowsHookEx(hook);
    });
}
