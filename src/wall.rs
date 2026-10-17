//! While a card is used in place, the real cursor may only be where the real window is what is
//! there, so every edge of it feels the same (a screen edge, the taskbar, another window). A
//! low-level mouse hook on its own thread (so a busy frame never delays the mouse) holds the
//! cursor back at other windows like a wall: it slides along it, and the motion pushing into it
//! is summed up (the pointer goes that far off the card). Pushed off, a click off the overlay
//! is swallowed and ends using the window.
//!
//! The desktop card is the whole primary screen, so windows on it are no walls there: the
//! cursor moves freely over the whole primary monitor, and clicks go to whatever is there.

use std::sync::atomic::{AtomicBool, AtomicI32, AtomicIsize, AtomicU32, Ordering};

use windows::Win32::Foundation::*;
use windows::Win32::System::Threading::GetCurrentThreadId;
use windows::Win32::UI::WindowsAndMessaging::*;

static OVERLAY: AtomicIsize = AtomicIsize::new(0);
static TARGET: AtomicIsize = AtomicIsize::new(0);
static PUSH_X: AtomicI32 = AtomicI32::new(0);
static PUSH_Y: AtomicI32 = AtomicI32::new(0);
/// The pointer is off the window (set each frame): a click off the overlay ends using it.
static OFF: AtomicBool = AtomicBool::new(false);
/// A click was swallowed to end using the window.
static END: AtomicBool = AtomicBool::new(false);
/// Button-ups still to swallow (of swallowed downs).
static UPS: AtomicU32 = AtomicU32::new(0);
static THREAD: AtomicU32 = AtomicU32::new(0);
/// The target is the desktop: the primary monitor's rectangle, else empty.
static DESK: std::sync::Mutex<Option<RECT>> = std::sync::Mutex::new(None);
/// The cursor went over to another monitor: no walls, every click goes where it is.
static AWAY: AtomicBool = AtomicBool::new(false);
/// When a mouse button was last pressed by the user (`now_ms`; 0: not yet).
static PRESSED: AtomicU32 = AtomicU32::new(0);

/// Milliseconds since first asked, from 1 (0 marks "never" in the times kept from it).
fn now_ms() -> u32 {
    static T0: std::sync::OnceLock<std::time::Instant> = std::sync::OnceLock::new();
    (T0.get_or_init(std::time::Instant::now).elapsed().as_millis() as u32).wrapping_add(1)
}

/// The overlay the cursor is held to (0: none yet).
pub fn overlay() -> isize {
    OVERLAY.load(Ordering::Relaxed)
}

/// For the self-test: `overlay` is the overlay, if none was used in place yet.
pub fn selftest_overlay(overlay: isize) {
    let _ = OVERLAY.compare_exchange(0, overlay, Ordering::SeqCst, Ordering::SeqCst);
}

/// Starts holding the cursor to the overlay `overlay` and window `target`.
pub fn start(overlay: isize, target: isize) {
    stop();
    OVERLAY.store(overlay, Ordering::SeqCst);
    TARGET.store(target, Ordering::SeqCst);
    PUSH_X.store(0, Ordering::SeqCst);
    PUSH_Y.store(0, Ordering::SeqCst);
    OFF.store(false, Ordering::SeqCst);
    END.store(false, Ordering::SeqCst);
    UPS.store(0, Ordering::SeqCst);
    AWAY.store(false, Ordering::SeqCst);
    *DESK.lock().unwrap_or_else(|e| e.into_inner()) = crate::win::is_desktop(crate::win::hwnd_of(target))
        .then(crate::win::primary_rect)
        .flatten();
    let (tx, rx) = std::sync::mpsc::channel();
    let _ = std::thread::Builder::new().name("wall".into()).spawn(move || unsafe {
        let _ = tx.send(GetCurrentThreadId());
        let Ok(hook) = SetWindowsHookExW(WH_MOUSE_LL, Some(hook), None, 0) else {
            return;
        };
        let mut msg = MSG::default();
        while GetMessageW(&mut msg, None, 0, 0).as_bool() {}
        let _ = UnhookWindowsHookEx(hook);
    });
    if let Ok(id) = rx.recv() {
        THREAD.store(id, Ordering::SeqCst);
    }
}

pub fn stop() {
    let id = THREAD.swap(0, Ordering::SeqCst);
    if id != 0 {
        unsafe {
            let _ = PostThreadMessageW(id, WM_QUIT, WPARAM(0), LPARAM(0));
        }
    }
}

/// Motion pushed into a wall so far (pixels).
pub fn push() -> (f32, f32) {
    (PUSH_X.load(Ordering::Relaxed) as f32, PUSH_Y.load(Ordering::Relaxed) as f32)
}

/// The cursor is off the card, free over the 3D view and other monitors.
pub fn away() -> bool {
    AWAY.load(Ordering::Relaxed)
}

/// The cursor went off the card (or came back onto it): drops the push.
pub fn set_away(away: bool) {
    AWAY.store(away, Ordering::SeqCst);
    PUSH_X.store(0, Ordering::SeqCst);
    PUSH_Y.store(0, Ordering::SeqCst);
}

pub fn set_off(off: bool) {
    OFF.store(off, Ordering::Relaxed);
}

/// Whether a click pushed off the window asked to end using it (once).
pub fn take_end() -> bool {
    END.swap(false, Ordering::Relaxed)
}

/// Using the desktop, whether `pt` is on the primary monitor (all of it is the desktop card).
pub fn desk_free(pt: POINT) -> bool {
    DESK.lock()
        .unwrap_or_else(|e| e.into_inner())
        .is_some_and(|r| pt.x >= r.left && pt.x < r.right && pt.y >= r.top && pt.y < r.bottom)
}

/// The slowest button press the hook took since the last [`take_slowest`], in microseconds.
static SLOWEST: AtomicU32 = AtomicU32::new(0);

/// The slowest button press the hook took since the last call, in microseconds.
pub fn take_slowest() -> u32 {
    SLOWEST.swap(0, Ordering::Relaxed)
}

/// The cursor may be at `pt`: the window (or one of its menus/dialogs) is what is there, inside
/// its visible frame.
fn allowed(pt: POINT) -> bool {
    if DESK.lock().unwrap_or_else(|e| e.into_inner()).is_some() {
        return desk_free(pt);
    }
    let target = TARGET.load(Ordering::Relaxed);
    let hit = crate::win::window_at(pt);
    crate::win::part_of(hit, target)
        && (hit.0 as isize != target || crate::win::in_frame(target, pt))
}

const PUSH_MAX: i32 = 160;

/// For the self-test: as if the user had just pressed a mouse button.
pub fn selftest_press() {
    PRESSED.store(now_ms().max(1), Ordering::Relaxed);
}

/// Whether the user pressed a mouse button in the last `ms` milliseconds (while a card is used
/// in place).
pub fn pressed_within(ms: u32) -> bool {
    let t = PRESSED.load(Ordering::Relaxed);
    t != 0 && now_ms().wrapping_sub(t) < ms
}

unsafe extern "system" fn hook(code: i32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    if code == HC_ACTION as i32
        && matches!(wp.0 as u32, WM_LBUTTONDOWN | WM_RBUTTONDOWN | WM_MBUTTONDOWN | WM_XBUTTONDOWN)
    {
        let m = unsafe { &*(lp.0 as *const MSLLHOOKSTRUCT) };
        if m.flags & LLMHF_INJECTED == 0 {
            PRESSED.store(now_ms().max(1), Ordering::Relaxed);
        }
        let t = std::time::Instant::now();
        let r = unsafe { hook_on(code, wp, lp) };
        SLOWEST.fetch_max(t.elapsed().as_micros() as u32, Ordering::Relaxed);
        return r;
    }
    unsafe { hook_on(code, wp, lp) }
}

unsafe fn hook_on(code: i32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    if code == HC_ACTION as i32 && !AWAY.load(Ordering::Relaxed) {
        let m = unsafe { &*(lp.0 as *const MSLLHOOKSTRUCT) };
        let injected = m.flags & LLMHF_INJECTED != 0;
        match wp.0 as u32 {
            WM_MOUSEMOVE if !injected => {
                let mut cur = POINT::default();
                unsafe {
                    let _ = GetCursorPos(&mut cur);
                }
                let pt = m.pt;
                // Already off the window (it moved, or a menu closed): let it go back freely.
                if allowed(pt) || !allowed(cur) {
                    // Moving back away from the wall takes the push back; along it keeps it.
                    let ease = |a: &AtomicI32, d: i32| {
                        let v = a.load(Ordering::Relaxed);
                        if v * d < 0 {
                            let v = if v > 0 { (v + d).max(0) } else { (v + d).min(0) };
                            a.store(v, Ordering::Relaxed);
                        }
                    };
                    ease(&PUSH_X, pt.x - cur.x);
                    ease(&PUSH_Y, pt.y - cur.y);
                } else {
                    // Slide along the wall where one axis alone is free.
                    let slide = [POINT { x: pt.x, y: cur.y }, POINT { x: cur.x, y: pt.y }]
                        .into_iter()
                        .find(|&p| p != cur && allowed(p))
                        .unwrap_or(cur);
                    let add = |a: &AtomicI32, d: i32| {
                        let v = (a.load(Ordering::Relaxed) + d).clamp(-PUSH_MAX, PUSH_MAX);
                        a.store(v, Ordering::Relaxed);
                    };
                    add(&PUSH_X, pt.x - slide.x);
                    add(&PUSH_Y, pt.y - slide.y);
                    if slide != cur {
                        unsafe {
                            let _ = SetCursorPos(slide.x, slide.y);
                        }
                    }
                    return LRESULT(1);
                }
            }
            WM_LBUTTONDOWN | WM_RBUTTONDOWN | WM_MBUTTONDOWN
                if !injected
                    && OFF.load(Ordering::Relaxed)
                    && !crate::win::contains(OVERLAY.load(Ordering::Relaxed), m.pt) =>
            {
                UPS.fetch_add(1, Ordering::Relaxed);
                END.store(true, Ordering::Relaxed);
                return LRESULT(1);
            }
            WM_LBUTTONUP | WM_RBUTTONUP | WM_MBUTTONUP
                if !injected && UPS.load(Ordering::Relaxed) > 0 =>
            {
                UPS.fetch_sub(1, Ordering::Relaxed);
                return LRESULT(1);
            }
            _ => {}
        }
    }
    unsafe { CallNextHookEx(None, code, wp, lp) }
}
