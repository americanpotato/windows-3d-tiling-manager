//! Window3D: a lightweight 3D window navigator for Windows.
//!
//! Idle: a hidden message window, a global hotkey, a tray icon and a foreground-change hook.
//! No GPU objects exist and the process blocks in GetMessage (~0% CPU).
//!
//! Hotkey: a fullscreen overlay is created with a fresh wgpu device. Every window is shown as
//! a card textured with its last saved screenshot (nothing is captured live while flying).
//! Clicking a card focuses the real window and tears the overlay and all GPU state down.

#![windows_subsystem = "windows"]

mod audio;
mod background;
mod icon;
mod image;
mod live;
mod menu;
mod model;
mod overlay;
mod pointer;
mod prompt;
mod render;
mod selftest;
mod snip;
mod stats;
mod statswin;
mod store;
mod tools;
mod wall;
mod win;
mod workflow;

use std::cell::RefCell;
use std::collections::{HashMap, HashSet, VecDeque};
use std::ffi::c_void;
use std::mem::size_of;
use std::sync::mpsc;

use windows::Win32::Foundation::*;
use windows::Win32::Graphics::Gdi::*;
use windows::Win32::System::Com::{COINIT_APARTMENTTHREADED, CoInitializeEx};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::System::ProcessStatus::K32EmptyWorkingSet;
use windows::Win32::System::Threading::{
    CREATE_WAITABLE_TIMER_HIGH_RESOLUTION, CreateMutexW, CreateWaitableTimerExW, GetCurrentProcess,
    SetWaitableTimer, TIMER_ALL_ACCESS,
};
use windows::Win32::UI::Accessibility::{HWINEVENTHOOK, SetWinEventHook, UnhookWinEvent};
use windows::Win32::UI::HiDpi::{
    DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2, SetProcessDpiAwarenessContext,
};
use windows::Win32::UI::Input::KeyboardAndMouse::*;
use windows::Win32::UI::Input::*;
use windows::Win32::UI::Shell::*;
use windows::Win32::UI::WindowsAndMessaging::*;
use windows::core::{PCWSTR, w};

use crate::model::Model;
use crate::overlay::{Action, Overlay};
use crate::render::Gpu;
use crate::store::{Config, Kind};
use crate::win::{WinInfo, hwnd_of};

pub const OVERLAY_CLASS: &str = "Window3DOverlay";
pub const MSG_CLASS: &str = "Window3DMsg";

const WM_APP_CAPTURED: u32 = WM_APP + 1;
const WM_APP_TRAY: u32 = WM_APP + 2;
const WM_APP_WARM: u32 = WM_APP + 3;
const HOTKEY_ID: i32 = 1;
/// Timer on the message window that gives back the sound of apps closed while muted (see
/// `audio`).
const AUDIO_TIMER: usize = 7;
const MENU_SHOW: usize = 1;
const MENU_FOLDER: usize = 2;
const MENU_CONFIG: usize = 3;
const MENU_QUIT: usize = 4;
const MENU_FRAMES: usize = 5;
const MENU_STATS: usize = 6;
const MENU_AUTOSTART: usize = 7;
const MENU_WF_SAVE: usize = 10;
const MENU_WF_SAVE_WINDOWS: usize = 11;
const MENU_WF_SAVE_TEMPLATES: usize = 12;
const MENU_WF_FOLDER: usize = 13;
const MENU_BG_FOLDER: usize = 14;
/// Saved workflow `n` in the tray menu (by its place in the menu).
const MENU_WF_LOAD: usize = 1000;
/// "Open on monitor": `MENU_MONITOR` = under the cursor, `+ n` = monitor n (1-based).
const MENU_MONITOR: usize = 100;

enum Event {
    Toggle,
    Quit,
    OpenFolder,
    OpenConfig,
    RestoreFrames,
    TrayMenu,
    TaskbarCreated,
    Foreground(isize),
    /// A window starts minimizing (only watched while the overlay is open).
    Minimized(isize),
    /// A top-level window was shown or uncloaked (only watched while the overlay is open).
    Shown(isize),
    /// A window was destroyed or hidden (only watched while the overlay is open).
    Gone(isize),
    Captured,
    Warm,
    AudioTimer,
    Close,
    Activated(bool),
    Resize(u32, u32),
    MouseDelta(i32, i32),
    Button(bool),
    Wheel(i32),
    /// The mouse on the options menu (client pixels), clicked or not.
    MenuMouse(i32, i32, bool),
    Key(u16),
    /// A typed character (`WM_CHAR`), for type-to-search.
    Char(u16),
    /// Input for the window used in place, with the time it arrived.
    Pass(u32, usize, isize, std::time::Instant),
    ToggleStats,
    SetMonitor(String),
    /// The stats window's right-click menu changed which lines it shows.
    StatsFields(Vec<String>),
    SaveWorkflow(workflow::Save),
    LoadWorkflow(std::path::PathBuf),
    OpenWorkflowFolder,
    OpenBackgroundFolder,
    /// A click on the overlay while the cursor is free: take the mouse back for flying.
    Regrab,
    System(Sys),
}

/// Session, power and display changes (from the message window).
#[derive(Debug, Clone, Copy, PartialEq)]
enum Sys {
    Lock,
    Suspend,
    Resume,
    /// Monitors added or removed, a resolution or DPI change.
    Display,
}

/// The cursor is released (Tab): no mouse-look, the mouse can leave for another monitor, and the
/// overlay stays open while another window has the focus.
static FREE: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
/// The options menu is open: the cursor shows and the mouse uses the menu.
static MENU: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

thread_local! {
    static EVENTS: RefCell<VecDeque<Event>> = const { RefCell::new(VecDeque::new()) };
    static TASKBAR_CREATED: std::cell::Cell<u32> = const { std::cell::Cell::new(0) };
}

fn push(e: Event) {
    EVENTS.with(|q| q.borrow_mut().push_back(e));
}

fn pop() -> Option<Event> {
    EVENTS.with(|q| q.borrow_mut().pop_front())
}

// --- Window procedures (they only record events; all logic runs in App) ------

extern "system" fn msg_wndproc(hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    match msg {
        WM_HOTKEY if wp.0 as i32 == HOTKEY_ID => push(Event::Toggle),
        WM_APP_CAPTURED => push(Event::Captured),
        WM_APP_WARM => push(Event::Warm),
        WM_TIMER if wp.0 == AUDIO_TIMER => push(Event::AudioTimer),
        WM_APP_TRAY => match lp.0 as u32 {
            WM_LBUTTONUP => push(Event::Toggle),
            WM_RBUTTONUP | WM_CONTEXTMENU => push(Event::TrayMenu),
            _ => {}
        },
        m if m != 0 && m == TASKBAR_CREATED.with(|c| c.get()) => push(Event::TaskbarCreated),
        WM_WTSSESSION_CHANGE if wp.0 as u32 == WTS_SESSION_LOCK => push(Event::System(Sys::Lock)),
        WM_POWERBROADCAST => {
            match wp.0 as u32 {
                PBT_APMSUSPEND => push(Event::System(Sys::Suspend)),
                PBT_APMRESUMEAUTOMATIC => push(Event::System(Sys::Resume)),
                _ => {}
            }
            return LRESULT(1);
        }
        WM_DISPLAYCHANGE => push(Event::System(Sys::Display)),
        // Logging off or shutting down: the process ends when this returns, before the
        // message loop runs again, so put the shell and the cursors back right here.
        WM_ENDSESSION if wp.0 != 0 => {
            store::log("session ending");
            audio::emergency_restore();
            win::show_shell(true);
            pointer::restore_cursors();
        }
        _ => return unsafe { DefWindowProcW(hwnd, msg, wp, lp) },
    }
    LRESULT(0)
}

extern "system" fn overlay_wndproc(hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    if overlay::INTERACT.load(std::sync::atomic::Ordering::Relaxed) {
        match msg {
            WM_MOUSEMOVE | WM_LBUTTONDOWN | WM_LBUTTONUP | WM_RBUTTONDOWN | WM_RBUTTONUP
            | WM_MBUTTONDOWN | WM_MBUTTONUP | WM_MOUSEWHEEL | WM_MOUSEHWHEEL | WM_KEYDOWN
            | WM_KEYUP | WM_CHAR | WM_SYSKEYDOWN | WM_SYSKEYUP | WM_SYSCHAR => {
                push(Event::Pass(msg, wp.0, lp.0, std::time::Instant::now()));
                return LRESULT(0);
            }
            // Clicks must not take the keyboard focus back from the window being used.
            WM_MOUSEACTIVATE => return LRESULT(MA_NOACTIVATE as isize),
            // The system arrow (blanked meanwhile): the pointer on the card is the only one seen.
            WM_SETCURSOR => unsafe {
                SetCursor(LoadCursorW(None, IDC_ARROW).ok());
                return LRESULT(1);
            },
            _ => {}
        }
    }
    if MENU.load(std::sync::atomic::Ordering::Relaxed) {
        match msg {
            WM_SETCURSOR => unsafe {
                SetCursor(LoadCursorW(None, IDC_ARROW).ok());
                return LRESULT(1);
            },
            WM_MOUSEMOVE | WM_LBUTTONDOWN => {
                let x = (lp.0 & 0xffff) as u16 as i16 as i32;
                let y = ((lp.0 >> 16) & 0xffff) as u16 as i16 as i32;
                push(Event::MenuMouse(x, y, msg == WM_LBUTTONDOWN));
                return LRESULT(0);
            }
            WM_LBUTTONUP | WM_MBUTTONDOWN => return LRESULT(0),
            _ => {}
        }
    }
    if FREE.load(std::sync::atomic::Ordering::Relaxed) {
        match msg {
            WM_SETCURSOR => unsafe {
                SetCursor(LoadCursorW(None, IDC_ARROW).ok());
                return LRESULT(1);
            },
            WM_LBUTTONDOWN => {
                push(Event::Regrab);
                return LRESULT(0);
            }
            WM_LBUTTONUP | WM_MBUTTONDOWN => return LRESULT(0),
            _ => {}
        }
    }
    match msg {
        WM_INPUT => {
            let mut raw = RAWINPUT::default();
            let mut size = size_of::<RAWINPUT>() as u32;
            let n = unsafe {
                GetRawInputData(
                    HRAWINPUT(lp.0 as *mut c_void),
                    RID_INPUT,
                    Some(&mut raw as *mut RAWINPUT as *mut c_void),
                    &mut size,
                    size_of::<RAWINPUTHEADER>() as u32,
                )
            };
            if n != u32::MAX && raw.header.dwType == RIM_TYPEMOUSE.0 {
                let m = unsafe { raw.data.mouse };
                // Absolute devices (remote desktop, tablets) don't give usable relative motion.
                if m.usFlags.0 & MOUSE_MOVE_ABSOLUTE.0 == 0 && (m.lLastX != 0 || m.lLastY != 0) {
                    push(Event::MouseDelta(m.lLastX, m.lLastY));
                }
            }
            return unsafe { DefWindowProcW(hwnd, msg, wp, lp) };
        }
        WM_LBUTTONDOWN => push(Event::Button(true)),
        WM_LBUTTONUP => push(Event::Button(false)),
        WM_MOUSEWHEEL => push(Event::Wheel(((wp.0 >> 16) & 0xffff) as u16 as i16 as i32)),
        WM_KEYDOWN => push(Event::Key(wp.0 as u16)),
        WM_CHAR => push(Event::Char(wp.0 as u16)),
        WM_SYSKEYDOWN => {
            push(Event::Key(wp.0 as u16));
            return unsafe { DefWindowProcW(hwnd, msg, wp, lp) };
        }
        WM_SETCURSOR => unsafe {
            // Using a card in place with the cursor off it, the cursor is the normal one.
            let free = overlay::INTERACT.load(std::sync::atomic::Ordering::Relaxed) && wall::away();
            SetCursor(if free {
                LoadCursorW(None, IDC_ARROW).ok()
            } else {
                None
            });
            return LRESULT(1);
        },
        WM_ACTIVATE => push(Event::Activated((wp.0 & 0xffff) as u32 != WA_INACTIVE)),
        WM_CLOSE => push(Event::Close),
        // Moved to a monitor with another scale (opening there, `M`): the window is sized
        // to the monitor's pixels by `show`, so nothing changes. Not a display change: that
        // closed the 3D view and remade the GPU (10 s) every time it went to the other monitor.
        WM_DPICHANGED => {
            store::log("3D view: on a monitor with another scale (DPI change ignored)");
            return LRESULT(0);
        }
        WM_SIZE => push(Event::Resize(
            (lp.0 & 0xffff) as u32,
            ((lp.0 >> 16) & 0xffff) as u32,
        )),
        _ => return unsafe { DefWindowProcW(hwnd, msg, wp, lp) },
    }
    LRESULT(0)
}

unsafe extern "system" fn foreground_hook(
    _: HWINEVENTHOOK,
    _event: u32,
    hwnd: HWND,
    id_object: i32,
    _: i32,
    _: u32,
    _: u32,
) {
    if id_object == OBJID_WINDOW.0 && !hwnd.0.is_null() {
        push(Event::Foreground(hwnd.0 as isize));
    }
}

/// Window events watched while the overlay is open: minimizing and top-level windows showing.
/// Handled as they happen instead of guessed from the foreground change that follows.
unsafe extern "system" fn window_events_hook(
    _: HWINEVENTHOOK,
    event: u32,
    hwnd: HWND,
    id_object: i32,
    _: i32,
    _: u32,
    ms: u32,
) {
    if id_object != OBJID_WINDOW.0 || hwnd.0.is_null() {
        return;
    }
    if event == EVENT_SYSTEM_MINIMIZESTART {
        push(Event::Minimized(hwnd.0 as isize));
    } else if (event == EVENT_OBJECT_SHOW || event == EVENT_OBJECT_UNCLOAKED)
        && unsafe { GetAncestor(hwnd, GA_ROOT) } == hwnd
    {
        win::note_popup(hwnd);
        win::trace_shown(hwnd, ms);
        push(Event::Shown(hwnd.0 as isize));
    } else if event == EVENT_OBJECT_DESTROY || event == EVENT_OBJECT_HIDE {
        push(Event::Gone(hwnd.0 as isize));
    }
}

thread_local! {
    static WINDOW_HOOKS: RefCell<Vec<HWINEVENTHOOK>> = const { RefCell::new(Vec::new()) };
}

/// Starts (or stops) watching the window events of [`window_events_hook`]. Off while the
/// overlay is hidden, so idling costs nothing.
fn watch_windows(on: bool) {
    WINDOW_HOOKS.with(|hooks| {
        let mut hooks = hooks.borrow_mut();
        for h in hooks.drain(..) {
            unsafe {
                let _ = UnhookWinEvent(h);
            }
        }
        if on {
            for e in [
                EVENT_SYSTEM_MINIMIZESTART,
                EVENT_OBJECT_SHOW,
                EVENT_OBJECT_UNCLOAKED,
                EVENT_OBJECT_DESTROY,
                EVENT_OBJECT_HIDE,
            ] {
                let h = unsafe {
                    SetWinEventHook(
                        e,
                        e,
                        None,
                        Some(window_events_hook),
                        0,
                        0,
                        WINEVENT_OUTOFCONTEXT,
                    )
                };
                if !h.is_invalid() {
                    hooks.push(h);
                }
            }
        }
    });
}

// --- Background screenshot worker -------------------------------------------

struct CaptureJob {
    info: WinInfo,
    /// Also compose the card image (the overlay is open and waiting for it).
    card: bool,
}

/// Apps launched from the overlay (and apps used in place on a card): until the overlay hides,
/// new windows of theirs or of processes they start (a game started from its launcher) are shown
/// as cards in front of the camera, and the overlay keeps the focus.
struct Launching {
    /// Until when any new window counts (`any`).
    until: std::time::Instant,
    next_poll: std::time::Instant,
    /// Windows that existed before the launch: the user switching to one of them is let be.
    before: HashSet<isize>,
    /// Windows seen so far (`before` and the placed ones).
    known: HashSet<isize>,
    /// Started processes (their windows are the ones wanted), and whether some launch was
    /// handed to an app already running (then any new window counts).
    pids: Vec<u32>,
    any: bool,
    placed: usize,
    /// Second screenshots of placed windows (their first paint may not be done yet).
    recapture: Vec<(std::time::Instant, isize)>,
    /// A launched window took the foreground from the window used in place (which ended):
    /// the user was on a card, so the launched one is used in place too.
    from_card: bool,
}

struct Captured {
    info: WinInfo,
    img: image::Image,
    card: Option<image::Image>,
}

fn spawn_capture_worker(
    msg_hwnd: isize,
    max_side: u32,
) -> (mpsc::Sender<CaptureJob>, mpsc::Receiver<Captured>) {
    let (job_tx, job_rx) = mpsc::channel::<CaptureJob>();
    let (done_tx, done_rx) = mpsc::channel::<Captured>();
    std::thread::Builder::new()
        .name("capture".into())
        .stack_size(1024 * 1024)
        .spawn(move || {
            unsafe {
                let _ = CoInitializeEx(None, COINIT_APARTMENTTHREADED);
            }
            for CaptureJob { info, card } in job_rx {
                if let Some(img) = win::capture_window(hwnd_of(info.hwnd), max_side) {
                    let card = card.then(|| {
                        win::compose_window_card(&info.title, &info.exe, Some(&img), false)
                    });
                    if done_tx.send(Captured { info, img, card }).is_err() {
                        break;
                    }
                    unsafe {
                        let _ = PostMessageW(
                            Some(hwnd_of(msg_hwnd)),
                            WM_APP_CAPTURED,
                            WPARAM(0),
                            LPARAM(0),
                        );
                    }
                }
            }
        })
        .expect("spawn capture thread");
    (job_tx, done_rx)
}

/// A card to show. `key` identifies its image content in the GPU texture cache.
struct CardJob {
    id: u64,
    kind: Kind,
    ghost: bool,
    hwnd: Option<isize>,
    title: String,
    exe: String,
    key: u64,
}

fn monitor_rect(mon: HMONITOR) -> (i32, i32, i32, i32) {
    let mut mi = MONITORINFO {
        cbSize: size_of::<MONITORINFO>() as u32,
        ..Default::default()
    };
    let _ = unsafe { GetMonitorInfoW(mon, &mut mi) };
    let r = mi.rcMonitor;
    (r.left, r.top, r.right - r.left, r.bottom - r.top)
}

/// The monitors as (x, y, w, h, primary), primary first, then left to right.
fn monitors() -> Vec<(i32, i32, i32, i32, bool)> {
    unsafe extern "system" fn each(
        m: HMONITOR,
        _: HDC,
        _: *mut RECT,
        lp: LPARAM,
    ) -> windows::core::BOOL {
        let v = unsafe { &mut *(lp.0 as *mut Vec<HMONITOR>) };
        v.push(m);
        true.into()
    }
    let mut v: Vec<HMONITOR> = Vec::new();
    unsafe {
        let _ = EnumDisplayMonitors(None, None, Some(each), LPARAM(&mut v as *mut _ as isize));
    }
    let mut out: Vec<_> = v
        .into_iter()
        .map(|m| {
            let (x, y, w, h) = monitor_rect(m);
            (x, y, w, h, x == 0 && y == 0)
        })
        .collect();
    out.sort_by_key(|m| (!m.4, m.0, m.1));
    out
}

/// Where the overlay opens: the configured monitor, else the one under the cursor.
fn overlay_monitor(choice: &str) -> (i32, i32, i32, i32) {
    let list = monitors();
    let pick = match choice.trim().to_ascii_lowercase().as_str() {
        "primary" => list.first(),
        n => n
            .parse::<usize>()
            .ok()
            .and_then(|n| list.get(n.wrapping_sub(1))),
    };
    if let Some(&(x, y, w, h, _)) = pick {
        return (x, y, w, h);
    }
    unsafe {
        let mut pt = POINT::default();
        let _ = GetCursorPos(&mut pt);
        monitor_rect(MonitorFromPoint(pt, MONITOR_DEFAULTTONEAREST))
    }
}

fn show_overlay_window(hwnd: HWND) {
    unsafe {
        let _ = ShowWindow(hwnd, SW_SHOW);
        let _ = SetForegroundWindow(hwnd);
        let _ = UpdateWindow(hwnd);
    }
}

// --- Hotkey parsing ---------------------------------------------------------

/// Composes the card image of a window, ghost or shortcut.
fn compose_card(kind: Kind, ghost: bool, title: &str, exe: &str, id: u64) -> image::Image {
    match kind {
        Kind::Shortcut => win::compose_shortcut_card(title, exe),
        Kind::Window => win::compose_window_card(title, exe, store::load_shot(id).as_ref(), ghost),
    }
}

/// Identifies the content of a card image, for the GPU texture cache.
fn card_key(kind: Kind, ghost: bool, title: &str, exe: &str, id: u64) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut h = std::collections::hash_map::DefaultHasher::new();
    (kind == Kind::Shortcut, ghost, title, exe).hash(&mut h);
    win::CARD_TITLES
        .load(std::sync::atomic::Ordering::Relaxed)
        .hash(&mut h);
    if kind == Kind::Window {
        store::shot_version(id).hash(&mut h);
    }
    h.finish()
}

/// When `config.json` was last written.
fn config_stamp() -> Option<std::time::SystemTime> {
    std::fs::metadata(store::app_dir().join("config.json"))
        .and_then(|m| m.modified())
        .ok()
}

fn parse_hotkey(s: &str) -> Option<(HOT_KEY_MODIFIERS, u32)> {
    let mut mods = MOD_NOREPEAT;
    let mut key = None;
    for part in s.split('+').map(|p| p.trim().to_ascii_lowercase()) {
        match part.as_str() {
            "ctrl" | "control" => mods |= MOD_CONTROL,
            "alt" => mods |= MOD_ALT,
            "shift" => mods |= MOD_SHIFT,
            "win" | "super" => mods |= MOD_WIN,
            "space" => key = Some(VK_SPACE.0 as u32),
            "tab" => key = Some(VK_TAB.0 as u32),
            "enter" | "return" => key = Some(VK_RETURN.0 as u32),
            "esc" | "escape" => key = Some(VK_ESCAPE.0 as u32),
            "`" | "backquote" | "grave" | "tilde" => key = Some(VK_OEM_3.0 as u32),
            "insert" => key = Some(VK_INSERT.0 as u32),
            "home" => key = Some(VK_HOME.0 as u32),
            "end" => key = Some(VK_END.0 as u32),
            "pause" => key = Some(VK_PAUSE.0 as u32),
            p if p.len() == 1 && p.chars().all(|c| c.is_ascii_alphanumeric()) => {
                key = Some(p.to_ascii_uppercase().as_bytes()[0] as u32)
            }
            p if p.starts_with('f')
                && p[1..].parse::<u32>().is_ok_and(|n| (1..=24).contains(&n)) =>
            {
                key = Some(VK_F1.0 as u32 + p[1..].parse::<u32>().unwrap() - 1)
            }
            _ => return None,
        }
    }
    key.map(|k| (mods, k))
}

// --- Application --------------------------------------------------------------

struct App {
    cfg: Config,
    /// When `config.json` was last read, to apply edits to it (`reload_config`).
    cfg_stamp: Option<std::time::SystemTime>,
    model: Model,
    hinstance: HINSTANCE,
    msg_hwnd: HWND,
    tray_icon: HICON,
    overlay: Option<Overlay>,
    /// The hidden overlay kept between opens (`keep_surface`), on `overlay_hwnd`.
    parked: Option<Overlay>,
    overlay_hwnd: HWND,
    /// Raw mouse input and the cursor clip are active.
    grabbed: bool,
    /// Windows made borderless by `focus_mode`, with their original frames.
    frames: HashMap<isize, win::SavedFrame>,
    /// Window made fullscreen by theater mode, given its frame back when the overlay returns.
    theater_frame: Option<isize>,
    /// Mutes apps whose card speaker is off while the overlay is open.
    muter: audio::Muter,
    /// When the mutes are next refreshed (apps start new audio sessions at any time).
    audio_next: std::time::Instant,
    focused: bool,
    last_fg: Option<isize>,
    capture_tx: mpsc::Sender<CaptureJob>,
    captured_rx: mpsc::Receiver<Captured>,
    /// Device and card textures kept between opens (`keep_warm`).
    gpu: Option<Gpu>,
    /// Background GPU start-up launched at program start (`keep_warm`).
    warmup: Option<mpsc::Receiver<Option<Gpu>>>,
    quit: bool,
    /// Selftest: the overlay window is never shown and never takes focus or the cursor.
    headless: bool,
    launching: Option<Launching>,
    /// Windows that came up or went away while the overlay shows, to look at shortly (when
    /// a new window has its title, or a hidden one is surely gone): (when, window, shown,
    /// until when a shown window is retried).
    window_checks: Vec<(std::time::Instant, isize, bool, std::time::Instant)>,
    /// Since when the foreground is a window without a card (the Alt+Tab switcher, a
    /// notification...): the focus is taken back if that lasts.
    lost: Option<std::time::Instant>,
}

impl Launching {
    /// The launched processes and everything they started.
    fn tree(&self) -> HashSet<u32> {
        self.pids
            .iter()
            .flat_map(|&p| win::process_tree(p))
            .collect()
    }

    /// Whether window `h` (not there before the launch) comes from a launched app.
    fn owns(&self, h: isize, tree: &HashSet<u32>) -> bool {
        tree.contains(&win::window_pid(hwnd_of(h)))
            || (self.any
                && std::time::Instant::now() < self.until
                && win::is_switchable(hwnd_of(h)))
    }
}

impl App {
    fn add_tray(&self) {
        let mut nid = NOTIFYICONDATAW {
            cbSize: size_of::<NOTIFYICONDATAW>() as u32,
            hWnd: self.msg_hwnd,
            uID: 1,
            uFlags: NIF_ICON | NIF_MESSAGE | NIF_TIP,
            uCallbackMessage: WM_APP_TRAY,
            hIcon: self.tray_icon,
            ..Default::default()
        };
        let tip: Vec<u16> = format!("Window3D \u{2014} {} opens the 3D view", self.cfg.hotkey)
            .encode_utf16()
            .collect();
        let n = tip.len().min(nid.szTip.len() - 1);
        nid.szTip[..n].copy_from_slice(&tip[..n]);
        unsafe {
            let _ = Shell_NotifyIconW(NIM_ADD, &nid);
        }
    }

    /// Applies edits to `config.json` made since it was last read (the overlay must be hidden):
    /// registers the new hotkey and rebuilds the parked overlay and the GPU state from the new
    /// settings. A file that doesn't parse keeps the old settings.
    fn reload_config(&mut self) {
        let stamp = config_stamp();
        if stamp == self.cfg_stamp || self.overlay.is_some() {
            return;
        }
        self.cfg_stamp = stamp;
        let text = std::fs::read_to_string(store::app_dir().join("config.json")).unwrap_or_default();
        let cfg: Config = match serde_json::from_str(text.trim_start_matches('\u{feff}')) {
            Ok(c) => c,
            Err(e) => {
                store::log(&format!("config.json changed but is invalid ({e}): not applied"));
                self.notify(
                    "Settings not applied",
                    &format!("config.json has an error ({e}); keeping the previous settings."),
                );
                return;
            }
        };
        let same = |a: &Config, b: &Config| serde_json::to_string(a).ok() == serde_json::to_string(b).ok();
        if same(&cfg, &self.cfg) {
            return;
        }
        let mut note = String::from("Window3D is using the new settings.");
        if cfg.hotkey != self.cfg.hotkey {
            unsafe {
                let _ = UnregisterHotKey(Some(self.msg_hwnd), HOTKEY_ID);
                let ok = parse_hotkey(&cfg.hotkey).is_some_and(|(mods, vk)| {
                    RegisterHotKey(Some(self.msg_hwnd), HOTKEY_ID, mods, vk).is_ok()
                });
                if ok {
                    note = format!("{note} The hotkey is now {}.", cfg.hotkey);
                } else {
                    note = format!(
                        "{note} The hotkey {} is not valid or already used by another program; use the tray icon.",
                        cfg.hotkey
                    );
                }
            }
        }
        if cfg.screenshot_size != self.cfg.screenshot_size {
            note += " The screenshot size applies after a restart.";
        }
        self.cfg = cfg;
        win::CARD_TITLES.store(self.cfg.card_titles, std::sync::atomic::Ordering::Relaxed);
        overlay::SHOW_HINTS.store(self.cfg.show_hints, std::sync::atomic::Ordering::Relaxed);
        live::LIVE_FPS.store(self.cfg.live_fps, std::sync::atomic::Ordering::Relaxed);
        live::MAX_FPS.store(self.cfg.max_fps, std::sync::atomic::Ordering::Relaxed);
        self.drop_gpu();
        self.remove_tray();
        self.add_tray();
        self.notify("Settings applied", &note);
        store::log("config.json changed: settings reloaded");
    }

    /// Saves the settings changed in the options menu and applies them: the view ones at once,
    /// the others by reopening the 3D view (with a new GPU device for renderer changes).
    fn apply_settings(&mut self, s: overlay::MenuSettings) {
        let reopen = s.gpu != self.cfg.gpu
            || s.msaa != self.cfg.msaa
            || s.card_titles != self.cfg.card_titles
            || s.show_ghosts != self.cfg.show_ghosts;
        self.cfg.gpu = s.gpu;
        self.cfg.msaa = s.msaa;
        self.cfg.fov_degrees = s.fov_degrees;
        self.cfg.mouse_sensitivity = s.mouse_sensitivity;
        self.cfg.move_speed = s.move_speed;
        self.cfg.card_titles = s.card_titles;
        self.cfg.show_ghosts = s.show_ghosts;
        self.cfg.show_hints = s.show_hints;
        self.cfg.max_fps = s.max_fps;
        self.cfg.live_fps = s.live_fps;
        self.cfg.background = s.background;
        store::save_config(&self.cfg);
        self.cfg_stamp = config_stamp();
        win::CARD_TITLES.store(self.cfg.card_titles, std::sync::atomic::Ordering::Relaxed);
        overlay::SHOW_HINTS.store(self.cfg.show_hints, std::sync::atomic::Ordering::Relaxed);
        live::LIVE_FPS.store(self.cfg.live_fps, std::sync::atomic::Ordering::Relaxed);
        live::MAX_FPS.store(self.cfg.max_fps, std::sync::atomic::Ordering::Relaxed);
        store::log(&format!(
            "options menu: settings saved (renderer {}, msaa {}){}",
            self.cfg.gpu,
            self.cfg.msaa,
            if reopen { ", reopening the 3D view" } else { "" }
        ));
        if reopen {
            self.drop_gpu();
            self.show();
        } else if let Some(ov) = &mut self.overlay {
            ov.set_view(s.fov_degrees, s.mouse_sensitivity, s.move_speed);
        }
    }

    /// Shows a balloon from the tray icon.
    fn notify(&self, title: &str, text: &str) {
        let mut nid = NOTIFYICONDATAW {
            cbSize: size_of::<NOTIFYICONDATAW>() as u32,
            hWnd: self.msg_hwnd,
            uID: 1,
            uFlags: NIF_INFO,
            dwInfoFlags: NIIF_USER | NIIF_LARGE_ICON,
            hBalloonIcon: self.tray_icon,
            ..Default::default()
        };
        let (title, text): (Vec<u16>, Vec<u16>) =
            (title.encode_utf16().collect(), text.encode_utf16().collect());
        let n = title.len().min(nid.szInfoTitle.len() - 1);
        nid.szInfoTitle[..n].copy_from_slice(&title[..n]);
        let n = text.len().min(nid.szInfo.len() - 1);
        nid.szInfo[..n].copy_from_slice(&text[..n]);
        unsafe {
            let _ = Shell_NotifyIconW(NIM_MODIFY, &nid);
        }
    }

    /// Tells a user who just started Window3D that it runs in the tray and how to open it; the
    /// first time with a few more words.
    fn greet(&self) {
        let marker = store::app_dir().join("welcomed");
        if marker.exists() {
            self.notify(
                "Window3D is running",
                &format!("Press {} to open the 3D view.", self.cfg.hotkey),
            );
        } else {
            let _ = std::fs::write(&marker, b"");
            self.notify(
                "Welcome to Window3D",
                &format!(
                    "Press {} to fly through your windows in 3D (press it again or Esc to close). \
                     It waits here in the tray; right-click the icon for settings.",
                    self.cfg.hotkey
                ),
            );
        }
    }

    fn remove_tray(&self) {
        let nid = NOTIFYICONDATAW {
            cbSize: size_of::<NOTIFYICONDATAW>() as u32,
            hWnd: self.msg_hwnd,
            uID: 1,
            ..Default::default()
        };
        unsafe {
            let _ = Shell_NotifyIconW(NIM_DELETE, &nid);
        }
    }

    fn save_workflow(&mut self, what: workflow::Save) {
        let label = "Name of the workflow. Use folder\\name to put it in a folder\n(new folders are created).";
        let Some(name) = prompt::ask(self.hinstance, "Save workflow", label, "") else {
            return;
        };
        let Some(path) = workflow::path_for(&name) else {
            return;
        };
        if path.exists()
            && !prompt::confirm(
                "Save workflow",
                &format!(
                    "Replace the workflow \u{201c}{}\u{201d}?",
                    workflow::name_of(&path)
                ),
            )
        {
            return;
        }
        if let Err(e) = workflow::save(&self.model, &path, what) {
            win::message_box(&format!("Could not save the workflow:\n{e}"));
        }
    }

    fn load_workflow(&mut self, path: &std::path::Path) {
        let name = workflow::name_of(path);
        if !prompt::confirm(
            "Open workflow",
            &format!(
                "Open the workflow \u{201c}{name}\u{201d}?\n\nCards move to their saved places and apps that are not running are started."
            ),
        ) {
            return;
        }
        // Cards are rebuilt from the layout when the 3D view opens.
        if self.overlay.is_some() {
            self.hide();
        }
        match workflow::load(&mut self.model, path) {
            Ok(launch) => {
                store::save_layout(&self.model.layout);
                for exe in launch {
                    if !win::launch(&exe) {
                        store::log(&format!("workflow {name}: failed to launch {exe}"));
                    }
                }
            }
            Err(e) => win::message_box(&format!("Could not open the workflow:\n{e}")),
        }
    }

    fn tray_menu(&mut self) {
        self.reload_config();
        unsafe {
            let Ok(menu) = CreatePopupMenu() else { return };
            let show = win::wide(&format!("Open 3D view\t{}", self.cfg.hotkey));
            let _ = AppendMenuW(menu, MF_STRING, MENU_SHOW, PCWSTR(show.as_ptr()));
            let _ = AppendMenuW(
                menu,
                MF_STRING,
                MENU_FOLDER,
                w!("Open data / shortcuts folder"),
            );
            let _ = AppendMenuW(
                menu,
                MF_STRING,
                MENU_CONFIG,
                w!("Edit settings"),
            );
            if !self.frames.is_empty() {
                let _ = AppendMenuW(menu, MF_STRING, MENU_FRAMES, w!("Restore window frames"));
            }
            let list = monitors();
            if let Ok(sub) = CreatePopupMenu() {
                let cur = self.cfg.monitor.trim().to_ascii_lowercase();
                let check = |on: bool| if on { MF_CHECKED } else { MF_UNCHECKED };
                let _ = AppendMenuW(
                    sub,
                    MF_STRING | check(cur.parse::<usize>().is_err() && cur != "primary"),
                    MENU_MONITOR,
                    w!("Monitor under the mouse"),
                );
                for (i, m) in list.iter().enumerate() {
                    let label = win::wide(&format!(
                        "Monitor {}: {}\u{d7}{}{}",
                        i + 1,
                        m.2,
                        m.3,
                        if m.4 { " (primary)" } else { "" }
                    ));
                    let on = cur == (i + 1).to_string() || (cur == "primary" && m.4);
                    let _ = AppendMenuW(
                        sub,
                        MF_STRING | check(on),
                        MENU_MONITOR + i + 1,
                        PCWSTR(label.as_ptr()),
                    );
                }
                let _ = AppendMenuW(menu, MF_POPUP, sub.0 as usize, w!("Open the 3D view on"));
            }
            let _ = AppendMenuW(
                menu,
                MF_STRING
                    | if statswin::is_open() {
                        MF_CHECKED
                    } else {
                        MF_UNCHECKED
                    },
                MENU_STATS,
                w!("Stats window"),
            );
            let _ = AppendMenuW(
                menu,
                MF_STRING
                    | if win::autostart() {
                        MF_CHECKED
                    } else {
                        MF_UNCHECKED
                    },
                MENU_AUTOSTART,
                w!("Start with Windows"),
            );
            let _ = AppendMenuW(menu, MF_STRING, MENU_BG_FOLDER, w!("Open backgrounds folder"));
            let mut wf_paths = Vec::new();
            if let Ok(sub) = CreatePopupMenu() {
                let _ = AppendMenuW(
                    sub,
                    MF_STRING,
                    MENU_WF_SAVE,
                    w!("Save arrangement as\u{2026}"),
                );
                let _ = AppendMenuW(
                    sub,
                    MF_STRING,
                    MENU_WF_SAVE_WINDOWS,
                    w!("Save windows only as\u{2026}"),
                );
                let _ = AppendMenuW(
                    sub,
                    MF_STRING,
                    MENU_WF_SAVE_TEMPLATES,
                    w!("Save template slots only as\u{2026}"),
                );
                let tree = workflow::tree();
                if !tree.is_empty() {
                    let _ = AppendMenuW(sub, MF_SEPARATOR, 0, PCWSTR::null());
                }
                let open: Vec<String> = self
                    .model
                    .layout
                    .workflows
                    .iter()
                    .map(|w| w.name.clone())
                    .collect();
                fn add(
                    menu: HMENU,
                    nodes: &[workflow::Node],
                    paths: &mut Vec<std::path::PathBuf>,
                    open: &[String],
                ) {
                    for n in nodes {
                        match n {
                            workflow::Node::Folder(name, kids) => unsafe {
                                if let Ok(sub) = CreatePopupMenu() {
                                    add(sub, kids, paths, open);
                                    let label = win::wide(name);
                                    let _ = AppendMenuW(
                                        menu,
                                        MF_POPUP,
                                        sub.0 as usize,
                                        PCWSTR(label.as_ptr()),
                                    );
                                }
                            },
                            workflow::Node::File(name, p) => unsafe {
                                let on = open.contains(&workflow::name_of(p));
                                let label = win::wide(name);
                                let _ = AppendMenuW(
                                    menu,
                                    MF_STRING | if on { MF_CHECKED } else { MF_UNCHECKED },
                                    MENU_WF_LOAD + paths.len(),
                                    PCWSTR(label.as_ptr()),
                                );
                                paths.push(p.clone());
                            },
                        }
                    }
                }
                add(sub, &tree, &mut wf_paths, &open);
                let _ = AppendMenuW(sub, MF_SEPARATOR, 0, PCWSTR::null());
                let _ = AppendMenuW(sub, MF_STRING, MENU_WF_FOLDER, w!("Open workflows folder"));
                let _ = AppendMenuW(menu, MF_POPUP, sub.0 as usize, w!("Workflows"));
            }
            let _ = AppendMenuW(menu, MF_SEPARATOR, 0, PCWSTR::null());
            let _ = AppendMenuW(menu, MF_STRING, MENU_QUIT, w!("Quit"));
            let mut pt = POINT::default();
            let _ = GetCursorPos(&mut pt);
            let _ = SetForegroundWindow(self.msg_hwnd);
            let cmd = TrackPopupMenu(
                menu,
                TPM_RIGHTBUTTON | TPM_RETURNCMD | TPM_NONOTIFY,
                pt.x,
                pt.y,
                None,
                self.msg_hwnd,
                None,
            );
            let _ = PostMessageW(Some(self.msg_hwnd), WM_NULL, WPARAM(0), LPARAM(0));
            let _ = DestroyMenu(menu);
            match cmd.0 as usize {
                MENU_SHOW => push(Event::Toggle),
                MENU_FOLDER => push(Event::OpenFolder),
                MENU_CONFIG => push(Event::OpenConfig),
                MENU_FRAMES => push(Event::RestoreFrames),
                MENU_QUIT => push(Event::Quit),
                MENU_STATS => push(Event::ToggleStats),
                MENU_AUTOSTART => win::set_autostart(!win::autostart()),
                MENU_MONITOR => push(Event::SetMonitor("cursor".into())),
                MENU_WF_SAVE => push(Event::SaveWorkflow(workflow::Save::All)),
                MENU_WF_SAVE_WINDOWS => push(Event::SaveWorkflow(workflow::Save::Windows)),
                MENU_WF_SAVE_TEMPLATES => push(Event::SaveWorkflow(workflow::Save::Templates)),
                MENU_WF_FOLDER => push(Event::OpenWorkflowFolder),
                MENU_BG_FOLDER => push(Event::OpenBackgroundFolder),
                n if n >= MENU_WF_LOAD && n < MENU_WF_LOAD + wf_paths.len() => {
                    push(Event::LoadWorkflow(wf_paths[n - MENU_WF_LOAD].clone()))
                }
                n if n > MENU_MONITOR && n <= MENU_MONITOR + list.len() => {
                    push(Event::SetMonitor((n - MENU_MONITOR).to_string()))
                }
                _ => {}
            }
        }
    }

    fn store_capture(&mut self, info: &WinInfo, img: &image::Image) -> u64 {
        let before = (self.model.layout.next_id, self.model.layout.entries.len());
        let id = self.model.entry_for_window(info);
        store::save_shot(id, img);
        if before != (self.model.layout.next_id, self.model.layout.entries.len()) {
            // Persist new ids right away so shot files can never be orphaned or reused.
            store::save_layout(&self.model.layout);
        }
        id
    }

    /// Stores a finished background capture and, if the overlay shows that window, swaps the
    /// fresh card in.
    fn on_captured(&mut self, c: Captured) {
        let id = self.store_capture(&c.info, &c.img);
        if let (Some(card), Some(e)) = (&c.card, self.model.get(id)) {
            let key = card_key(Kind::Window, false, &e.title, &e.exe, id);
            if let Some(ov) = &mut self.overlay {
                ov.update_card(id, key, card);
            } else if let Some(ov) = &mut self.parked {
                // Ready for the next open: it won't have to compose this card.
                ov.preload(id, key, card);
                unsafe {
                    let _ = K32EmptyWorkingSet(GetCurrentProcess());
                }
            }
        }
    }

    /// Whether foreground window `fg` may be a just-launched app taking the focus (rather than
    /// the user switching to a window that was already there).
    fn launch_stole(&self, fg: isize) -> bool {
        self.launching.as_ref().is_some_and(|l| {
            fg != self.overlay_hwnd.0 as isize && !l.before.contains(&fg) && l.owns(fg, &l.tree())
        })
    }

    /// Gives the focus back to the overlay after a launched app took it.
    fn retake_focus(&mut self) {
        if self.overlay.is_none()
            || self.headless
            || overlay::INTERACT.load(std::sync::atomic::Ordering::Relaxed)
            || FREE.load(std::sync::atomic::Ordering::Relaxed)
        {
            return;
        }
        let fg = unsafe { GetForegroundWindow() };
        let ok = win::force_foreground(self.overlay_hwnd);
        store::log(&format!(
            "launched app took the focus (\"{}\"): {}",
            win::win_info(fg).title,
            if ok {
                "taken back"
            } else {
                "could not take it back"
            }
        ));
        if ok {
            self.focused = true;
            self.grab();
            unsafe {
                let _ = LockSetForegroundWindow(LSFW_LOCK);
            }
        }
    }

    /// Starts watching for windows started from the app used in place on a card (only windows
    /// opened from now on count).
    fn adopt_interact(&self, l: &mut Launching) {
        let Some(h) = self.overlay.as_ref().and_then(|ov| ov.interact_hwnd()) else {
            return;
        };
        let pid = win::window_pid(hwnd_of(h));
        if pid != 0 && !l.pids.contains(&pid) {
            l.pids.push(pid);
            for w in win::enumerate_windows() {
                l.before.insert(w.hwnd);
                l.known.insert(w.hwnd);
            }
        }
    }

    /// Looks for the windows of launched apps and adds them as cards in front of the camera.
    fn poll_launch(&mut self) {
        let now = std::time::Instant::now();
        let Some(mut l) = self.launching.take() else {
            return;
        };
        if now < l.next_poll || self.overlay.is_none() {
            self.launching = Some(l);
            return;
        }
        l.next_poll = now + std::time::Duration::from_millis(250);
        self.adopt_interact(&mut l);
        l.recapture.retain(|&(t, h)| {
            if t > now {
                return true;
            }
            if win::is_switchable(hwnd_of(h)) {
                let _ = self.capture_tx.send(CaptureJob {
                    info: win::win_info(hwnd_of(h)),
                    card: true,
                });
            }
            false
        });
        let fresh: Vec<WinInfo> = win::enumerate_windows()
            .into_iter()
            .filter(|w| !w.minimized && !l.known.contains(&w.hwnd))
            .collect();
        let tree = if fresh.is_empty() {
            HashSet::new()
        } else {
            l.tree()
        };
        let mut last = None;
        let used = self.overlay.as_ref().and_then(|ov| ov.interact_hwnd());
        for w in fresh {
            // Windows of other apps opening meanwhile are left alone (seen again next poll).
            if !l.owns(w.hwnd, &tree) {
                continue;
            }
            // A sign-in window opened over the window used in place: shown on its card.
            if used.is_some_and(|t| self.popup_of_used(w.hwnd, t)) {
                l.known.insert(w.hwnd);
                continue;
            }
            let w = if self.pull_to_view(w.hwnd) {
                win::win_info(hwnd_of(w.hwnd))
            } else {
                w
            };
            last = Some(w.hwnd);
            l.known.insert(w.hwnd);
            self.place_launched(&w, l.placed);
            l.placed += 1;
            l.recapture
                .push((now + std::time::Duration::from_millis(1500), w.hwnd));
            // Splash screens give way to the main window: keep taking any new window a little
            // longer when the process is unknown.
            l.until = l.until.min(now + std::time::Duration::from_secs(4));
        }
        let use_it = std::mem::take(&mut l.from_card)
            || self.overlay.as_ref().is_some_and(|ov| ov.on_card());
        self.launching = Some(l);
        if let Some(h) = last {
            self.present_new(h, use_it);
        }
    }

    /// Puts the card of new window `w` in front of the camera (the `n`-th placed, side by side).
    fn place_launched(&mut self, w: &WinInfo, n: usize) {
        let Some(ov) = &mut self.overlay else {
            return;
        };
        let (cam, ahead) = ov.ahead(&self.model);
        let id = self.model.entry_for_window(w);
        let side = if n % 2 == 1 {
            (n / 2 + 1) as f32
        } else {
            -((n / 2) as f32)
        } * 2.8;
        let (fwd, right) = (
            [cam.yaw.sin(), -cam.yaw.cos()],
            [cam.yaw.cos(), cam.yaw.sin()],
        );
        let Some(e) = self.model.get_mut(id) else {
            return;
        };
        e.pos = [
            cam.pos[0] + fwd[0] * ahead + right[0] * side,
            cam.pos[1].clamp(1.4, render::ROOM_HEIGHT - 1.4),
            cam.pos[2] + fwd[1] * ahead + right[1] * side,
        ];
        e.yaw = cam.yaw;
        let (title, exe) = (e.title.clone(), e.exe.clone());
        store::save_layout(&self.model.layout);
        // A ghost of the same app may have been reclaimed: it becomes this card.
        ov.remove_card(id);
        let key = card_key(Kind::Window, false, &title, &exe, id);
        let img = compose_card(Kind::Window, false, &title, &exe, id);
        ov.add_card(id, Kind::Window, false, Some(w.hwnd), key, Some(&img));
        let _ = self.capture_tx.send(CaptureJob {
            info: w.clone(),
            card: true,
        });
        store::log(&format!("launched window \"{title}\" added as card {id}"));
    }

    /// Takes the warm GPU, waiting for the start-up warm-up if it is still running.
    fn take_gpu(&mut self) -> Option<Gpu> {
        if let Some(rx) = self.warmup.take() {
            if let Ok(Some(g)) = rx.recv() {
                self.gpu = Some(g);
            }
        }
        self.gpu.take()
    }

    /// The window used in place is minimized (its minimize button, Win+Down...): back to
    /// flying with the focus, before Windows hands the foreground to the next window.
    fn on_minimized(&mut self, h: isize) {
        if let Some(ov) = &mut self.overlay {
            if ov.interact_hwnd() == Some(h) {
                ov.end_interact_refocus("the window was minimized");
            }
        }
    }

    /// Whether window `h` (not the window `used` in place) is a popup of it: a menu, bubble or
    /// dialog, or a window it opened over itself that looks like a main window (see
    /// `win::adopt_popup`). Windows with a card of their own are not.
    fn popup_of_used(&self, h: isize, used: isize) -> bool {
        h != used
            && !win::is_desktop(hwnd_of(used))
            && self.overlay.as_ref().is_some_and(|ov| ov.card_of(h).is_none())
            && win::adopt_popup(hwnd_of(h), used)
    }

    /// A window came up: one of the window used in place is shown on its card.
    fn on_shown(&mut self, h: isize) {
        if let Some(ov) = &self.overlay {
            ov.keep_on_top();
        }
        let used = self.overlay.as_ref().and_then(|ov| ov.interact_hwnd());
        if let Some(t) = used.filter(|&t| win::is_switchable(hwnd_of(h)) && self.popup_of_used(h, t)) {
            // Captioned, Alt+Tab-listed, but opened by the window used in place over itself
            // (a sign-in window): on its card, not a card of its own.
            let moved = win::fit_dialog(hwnd_of(h), t);
            win::note_popup(hwnd_of(h));
            if let Some(l) = &mut self.launching {
                l.known.insert(h);
            }
            store::log(&format!(
                "a window of the window used in place came up over it ({}): shown on its card{}",
                win::details(hwnd_of(h)),
                if moved { ", moved over the window" } else { "" }
            ));
        } else if win::shell_hiding() && win::is_shell_popup(hwnd_of(h)) {
            // Hidden as it shows rather than at the next sweep, so it doesn't flash up.
            win::hide_shell_again();
        } else {
            // Looked at shortly, once it has its title and size.
            let now = std::time::Instant::now();
            self.window_checks.push((
                now + std::time::Duration::from_millis(150),
                h,
                true,
                now + std::time::Duration::from_secs(2),
            ));
        }
    }

    /// A window was destroyed or hidden: if it has a card, the card goes (looked at again
    /// shortly when it still exists, as apps briefly hide windows to restyle them).
    fn on_gone(&mut self, h: isize) {
        if !self.overlay.as_ref().is_some_and(|ov| ov.card_of(h).is_some()) {
            return;
        }
        if unsafe { IsWindow(Some(hwnd_of(h))) }.as_bool() {
            let at = std::time::Instant::now() + std::time::Duration::from_millis(300);
            self.window_checks.push((at, h, false, at));
        } else {
            self.window_gone(h);
        }
    }

    /// Removes the card of window `h` if it was closed or hidden (its close button, closed
    /// to the tray...); when it was used in place, back to flying with the focus.
    fn window_gone(&mut self, h: isize) {
        let hw = hwnd_of(h);
        if unsafe { IsWindow(Some(hw)).as_bool() && IsWindowVisible(hw).as_bool() } {
            return;
        }
        let Some(ov) = &mut self.overlay else {
            return;
        };
        let Some(id) = ov.card_of(h) else {
            return;
        };
        if ov.interact_hwnd() == Some(h) {
            ov.end_interact_refocus("the window was closed");
            self.focused = true;
        }
        ov.remove_card(id);
        store::log(&format!("a window with a card was closed or hidden: card {id} removed"));
    }

    /// A window came up while the overlay shows (opened from anywhere): it gets a card in
    /// front of the camera and is used in place. False to look again later (no title yet).
    fn window_appeared(&mut self, h: isize) -> bool {
        let hw = hwnd_of(h);
        if self.overlay.is_none()
            || self.headless
            || FREE.load(std::sync::atomic::Ordering::Relaxed)
            || !unsafe { IsWindow(Some(hw)) }.as_bool()
        {
            return true;
        }
        if !win::is_switchable(hw) {
            return false;
        }
        if win::is_minimized(hw) || self.overlay.as_ref().is_some_and(|ov| ov.card_of(h).is_some())
        {
            return true;
        }
        // Popups of the window used in place show on its card; windows of a launch are placed
        // by `poll_launch`.
        if self
            .overlay
            .as_ref()
            .and_then(|ov| ov.interact_hwnd())
            .is_some_and(|t| self.popup_of_used(h, t))
        {
            return true;
        }
        if self
            .launching
            .as_ref()
            .is_some_and(|l| l.known.contains(&h) || l.owns(h, &l.tree()))
        {
            return true;
        }
        if win::monitor_of(hw) != win::monitor_of(self.overlay_hwnd) && !self.pull_to_view(h) {
            return true;
        }
        store::log(&format!(
            "a window came up ({}): added as a card",
            win::describe(hw)
        ));
        self.adopt_new(h);
        true
    }

    /// Gives new window `h` a card in front of the camera (see `present_new`).
    fn adopt_new(&mut self, h: isize) {
        // Launched from a card (Start shown on it), whose use the launch ended: used in place too.
        let from_card = self.launching.as_mut().is_some_and(|l| std::mem::take(&mut l.from_card));
        let use_it = from_card || self.overlay.as_ref().is_some_and(|ov| ov.on_card());
        let w = win::win_info(hwnd_of(h));
        self.place_launched(&w, 0);
        if let Some(l) = &mut self.launching {
            l.known.insert(h);
        }
        self.present_new(h, use_it);
    }

    /// The card of new window `h` comes in front of the user: used in place if the user was
    /// on a card (`use_it`), else only shown there while the 3D view keeps the focus, so a slow
    /// launch doesn't pull the user out of flying around.
    fn present_new(&mut self, h: isize, use_it: bool) {
        if use_it {
            self.use_window(h);
            return;
        }
        let a = match &mut self.overlay {
            Some(ov) => ov.show_window(h, &mut self.model),
            None => None,
        };
        let Some(a) = a else {
            return;
        };
        store::save_layout(&self.model.layout);
        store::log("new window shown in front of the camera (flying: not used in place)");
        self.lost = None;
        if !self.headless
            && unsafe { GetForegroundWindow() } != self.overlay_hwnd
            && win::force_foreground(self.overlay_hwnd)
        {
            self.focused = true;
            self.grab();
        }
        self.handle_action(a);
    }

    /// New window `h` opened on another monitor while the user is in the 3D view (the cursor
    /// on its monitor, not pushed over to another one): it is moved over to the 3D view's
    /// monitor. True if it was moved.
    fn pull_to_view(&self, h: isize) -> bool {
        if self.headless || win::cursor_monitor() != win::monitor_of(self.overlay_hwnd) {
            return false;
        }
        let moved = win::move_to_monitor_of(hwnd_of(h), self.overlay_hwnd);
        if moved {
            store::log(&format!(
                "a new window opened on another monitor ({}): moved to the 3D view's monitor",
                win::describe(hwnd_of(h))
            ));
        }
        moved
    }

    /// Brings the card of window `h` in front of the camera to use it in place there (the
    /// overlay takes the focus meanwhile). False if it has no card here.
    fn use_window(&mut self, h: isize) -> bool {
        let a = match &mut self.overlay {
            Some(ov) => ov.bring_window(h, &mut self.model),
            None => None,
        };
        let Some(a) = a else {
            return false;
        };
        store::save_layout(&self.model.layout);
        self.lost = None;
        if !self.headless && win::force_foreground(self.overlay_hwnd) {
            self.focused = true;
            self.grab();
        }
        self.handle_action(a);
        true
    }

    /// Another window took the foreground from the 3D view (Alt+Tab, a window of the app
    /// used in place, an app bringing itself up...): instead of closing, the 3D view goes to
    /// its card and uses it in place, giving it a card first if it is new. Anything else
    /// (the Alt+Tab switcher, a notification) gets the focus taken back if it keeps it.
    fn follow(&mut self, new: isize) {
        let hw = hwnd_of(new);
        if self.overlay.is_none()
            || self.headless
            || new == self.overlay_hwnd.0 as isize
            || FREE.load(std::sync::atomic::Ordering::Relaxed)
            || win::window_pid(hw) == std::process::id()
        {
            return;
        }
        // A dialog of the app used in place (Save as, a prompt): it isn't on the card, so the
        // user is let through to it.
        let used = self.overlay.as_ref().and_then(|ov| ov.interact_hwnd());
        if used.is_some_and(|t| {
            !win::is_switchable(hw) && win::window_pid(hw) == win::window_pid(hwnd_of(t))
        }) {
            store::log(&format!(
                "a dialog of the window used in place came up ({}): hiding",
                win::describe(hw)
            ));
            self.hide();
            return;
        }
        if !overlay::INTERACT.load(std::sync::atomic::Ordering::Relaxed)
            && win::monitor_of(hw) != win::monitor_of(self.overlay_hwnd)
        {
            // A new window: brought over to this monitor as a card.
            if self.overlay.as_ref().is_some_and(|ov| ov.card_of(new).is_none())
                && win::is_switchable(hw)
                && !win::is_minimized(hw)
                && self.pull_to_view(new)
            {
                self.adopt_new(new);
                return;
            }
            store::log(&format!(
                "a window on another monitor took the focus ({}): hiding",
                win::describe(hw)
            ));
            self.hide();
            return;
        }
        if self.use_window(new) {
            store::log(&format!(
                "{} took the foreground: over to its card",
                win::describe(hw)
            ));
        } else if win::is_switchable(hw) && !win::is_minimized(hw) {
            store::log(&format!(
                "{} took the foreground: added as a card and used in place",
                win::describe(hw)
            ));
            self.adopt_new(new);
        } else if self.lost.is_none() {
            store::log(&format!(
                "{} took the foreground: staying, the focus is taken back if it keeps it",
                win::describe(hw)
            ));
            self.lost = Some(std::time::Instant::now());
        }
    }

    /// Looks at the windows that came up or went away, and takes the focus back from a window
    /// without a card that kept it.
    fn poll_windows(&mut self) {
        let now = std::time::Instant::now();
        if self.overlay.is_none() {
            self.window_checks.clear();
            self.lost = None;
            return;
        }
        if !self.window_checks.is_empty() {
            for (at, h, shown, until) in std::mem::take(&mut self.window_checks) {
                if at > now {
                    self.window_checks.push((at, h, shown, until));
                } else if !shown {
                    self.window_gone(h);
                } else if !self.window_appeared(h) && now < until {
                    let at = now + std::time::Duration::from_millis(250);
                    self.window_checks.push((at, h, shown, until));
                }
            }
        }
        if let Some(t) = self.lost {
            if self.focused || overlay::INTERACT.load(std::sync::atomic::Ordering::Relaxed) {
                self.lost = None;
            } else if snip::active() {
                // Not taken back from the Snipping Tool.
            } else if t.elapsed() >= std::time::Duration::from_millis(1200)
                && unsafe { GetAsyncKeyState(VK_MENU.0 as i32) } >= 0
            {
                self.lost = None;
                let fg = unsafe { GetForegroundWindow() }.0 as isize;
                if win::is_switchable(hwnd_of(fg)) {
                    self.follow(fg);
                } else if win::force_foreground(self.overlay_hwnd) {
                    store::log("the focus was taken back from a window without a card");
                    self.focused = true;
                    self.grab();
                }
            }
        }
    }

    fn on_foreground(&mut self, new: isize) {
        stats::switch_done(new);
        // Win+Shift+S: the Snipping Tool takes the foreground over the 3D view for a moment;
        // the user stays where they were (flying, or using a window in place).
        if self.overlay.is_some() && snip::is_tool(new) {
            store::log(&format!("the Snipping Tool took the foreground ({}), staying", win::describe(hwnd_of(new))));
            return;
        }
        if self.launch_stole(new) {
            self.retake_focus();
        }
        // Start, search or a flyout came up over the 3D view: it is shown and used on the
        // desktop card instead.
        let on_desktop = self
            .overlay
            .as_ref()
            .and_then(|ov| ov.interact_hwnd())
            .is_some_and(|t| win::is_desktop(hwnd_of(t)));
        // While using a card, the user may push over to another monitor and work there.
        let elsewhere = self
            .overlay
            .as_ref()
            .is_some_and(|ov| ov.interact_hwnd().is_some())
            && win::monitor_of(hwnd_of(new)) != win::monitor_of(self.overlay_hwnd);
        if elsewhere {
            store::log(&format!(
                "in-place use: a window on another monitor took the foreground ({}), staying",
                win::describe(hwnd_of(new))
            ));
        }
        // Using a window (not filling its monitor) in place, they show on its card instead, as
        // they show over it (see `Overlay::shell_on_card`).
        let on_app = !on_desktop && self.overlay.as_ref().is_some_and(|ov| ov.shell_on_card().is_some());
        if on_app && !elsewhere && win::is_shell_popup(hwnd_of(new)) {
            store::log(&format!(
                "a shell popup came up ({}): shown on the card of the window used in place",
                win::describe(hwnd_of(new))
            ));
        } else if !on_desktop && !elsewhere && win::is_shell_popup(hwnd_of(new)) {
            if let Some(ov) = &mut self.overlay {
                if ov.show_desktop(&self.model) {
                    store::log(&format!(
                        "a shell popup came up ({}): to the desktop card",
                        win::describe(hwnd_of(new))
                    ));
                }
            }
        }
        // Using a window in place gives it the focus behind the overlay; anything else taking
        // the foreground (Alt+Tab, Start...) means the user went elsewhere.
        let used = self.overlay.as_ref().and_then(|ov| ov.interact_hwnd());
        if let Some(t) = used.filter(|&t| {
            !unsafe { IsWindow(Some(hwnd_of(t))).as_bool() && IsWindowVisible(hwnd_of(t)).as_bool() }
        }) {
            // Closed (or hidden to the tray) with its own button: Windows hands the foreground
            // to the next window, but the user stays in the 3D view.
            self.window_gone(t);
        } else if let Some(t) = used {
            let desktop = win::is_desktop(hwnd_of(t));
            // Using the desktop in place, the card is the whole screen 1:1: whatever takes the
            // foreground on it (a window clicked, the taskbar, Start, a tray menu, Task Manager)
            // is part of it, still used through the card.
            if desktop && !elsewhere {
                if new != t && new != self.overlay_hwnd.0 as isize {
                    store::log(&format!(
                        "in-place use (desktop): {} took the foreground, staying",
                        win::describe(hwnd_of(new))
                    ));
                }
            } else if !desktop && !elsewhere && new != t && self.popup_of_used(new, t) {
                // A dialog, bubble or menu of the window used in place (Save as, a sign-in
                // window a page opened, Chrome's profile menu): part of it, shown on its card.
                let moved = win::fit_dialog(hwnd_of(new), t);
                win::note_popup(hwnd_of(new));
                store::log(&format!(
                    "in-place use: a popup of the window used in place took the foreground ({}), staying{}",
                    win::details(hwnd_of(new)),
                    if moved { "; moved over the window" } else { "" }
                ));
            } else if !elsewhere
                && new != t
                && new != self.overlay_hwnd.0 as isize
                && !(on_app && win::is_shell_popup(hwnd_of(new)))
            {
                store::log(&format!(
                    "in-place use: another window took the foreground ({}), {}",
                    win::describe(hwnd_of(new)),
                    if self.launch_stole(new) || win::is_minimized(hwnd_of(t)) {
                        "back to flying"
                    } else {
                        "following it"
                    }
                ));
                if self.launch_stole(new) {
                    // E.g. a game started from the launcher used in place: back to flying, and
                    // the game comes in as a card, used in place.
                    if let Some(ov) = &mut self.overlay {
                        ov.end_interact_for_launch();
                    }
                    if let Some(l) = &mut self.launching {
                        l.from_card = true;
                    }
                } else if win::is_minimized(hwnd_of(t)) {
                    // Its minimize button: Windows hands the foreground to the next window,
                    // but the user stays in the 3D view.
                    if let Some(ov) = &mut self.overlay {
                        ov.end_interact_refocus("the window was minimized");
                    }
                } else {
                    // E.g. another window of the app used in place, or Alt+Tab: over to it.
                    store::log(&format!(
                        "in-place use: following {}; the window used in place: {}",
                        win::details(hwnd_of(new)),
                        win::details(hwnd_of(t))
                    ));
                    self.follow(new);
                }
            }
        } else if self.overlay.is_some()
            && !self.focused
            && new != self.overlay_hwnd.0 as isize
            && !self.launch_stole(new)
            && !win::is_shell_popup(hwnd_of(new))
        {
            // E.g. the window picked in the Alt+Tab switcher.
            self.follow(new);
        }
        // The window that just lost focus is still fully rendered by DWM: snapshot it now.
        if let Some(prev) = self.last_fg.take() {
            if prev != new && self.overlay.is_none() && win::is_switchable(hwnd_of(prev)) {
                let _ = self.capture_tx.send(CaptureJob {
                    info: win::win_info(hwnd_of(prev)),
                    card: self.parked.is_some(),
                });
            }
        }
        self.last_fg = win::is_switchable(hwnd_of(new)).then_some(new);
    }

    /// Syncs the model with the open windows and shortcuts. Returns the captures to run (the
    /// window `fg` and windows without any screenshot yet) and the cards to show, in order.
    fn scan(&mut self, fg: Option<isize>) -> (Vec<CaptureJob>, Vec<CardJob>) {
        let mut live = win::enumerate_windows();
        if self.cfg.desktop_card {
            live.extend(win::desktop_window());
        }
        self.model.sync_live(&live);
        let mut captures = Vec::new();
        for w in &live {
            let id = self.model.entry_for_window(w);
            // The desktop never loses the focus like other windows: shoot it on every open.
            let desktop = w.class == "Progman";
            if Some(w.hwnd) == fg || desktop || (!w.minimized && !store::has_shot(id)) {
                captures.push(CaptureJob {
                    info: w.clone(),
                    card: true,
                });
            }
        }
        self.model.sync_shortcuts(&model::scan_shortcuts(&self.cfg));
        self.model.evict_ghosts(if self.cfg.show_ghosts {
            self.cfg.max_ghosts
        } else {
            usize::MAX
        });
        store::save_layout(&self.model.layout);

        // Cards: live windows, then ghosts of closed apps, then shortcuts.
        let mut jobs: Vec<CardJob> = Vec::new();
        for w in &live {
            if let Some(e) = self
                .model
                .bindings
                .get(&w.hwnd)
                .and_then(|id| self.model.get(*id))
            {
                jobs.push(CardJob {
                    id: e.id,
                    kind: Kind::Window,
                    ghost: false,
                    hwnd: Some(w.hwnd),
                    title: e.title.clone(),
                    exe: e.exe.clone(),
                    key: card_key(Kind::Window, false, &e.title, &e.exe, e.id),
                });
            }
        }
        for e in &self.model.layout.entries {
            let ghost = self.cfg.show_ghosts && self.model.is_ghost(e.id);
            let shortcut = e.kind == Kind::Shortcut && self.cfg.show_shortcuts;
            if ghost || shortcut {
                jobs.push(CardJob {
                    id: e.id,
                    kind: e.kind,
                    ghost,
                    hwnd: None,
                    title: e.title.clone(),
                    exe: e.exe.clone(),
                    key: card_key(e.kind, ghost, &e.title, &e.exe, e.id),
                });
            }
        }
        (captures, jobs)
    }

    fn show(&mut self) {
        self.show_with(self.headless);
    }

    /// Opens the overlay. `hidden` builds it without showing it or touching focus and input
    /// (the self-test, and pre-building the parked overlay at start-up).
    fn show_with(&mut self, hidden: bool) {
        if self.overlay.is_some() {
            return;
        }
        // The parked overlay or warm GPU died while hidden (e.g. a driver update).
        if render::take_lost() {
            store::log("GPU lost while hidden: making a new one");
            self.drop_gpu();
        }
        if let Some(h) = self.theater_frame.take() {
            if let Some(f) = self.frames.remove(&h) {
                win::restore_frame(hwnd_of(h), &f);
            }
        }
        let t0 = std::time::Instant::now();
        let mut phases = String::new();
        let mut lap = |name: &str| {
            use std::fmt::Write as _;
            let _ = write!(phases, " {name}={}", t0.elapsed().as_millis());
        };
        // Drain screenshots the worker finished meanwhile.
        while let Ok(c) = self.captured_rx.try_recv() {
            self.store_capture(&c.info, &c.img);
        }
        // The window being left is the freshest thing we can snapshot. It and windows without
        // any screenshot yet are captured in the background: their cards show the previous
        // screenshot (or a placeholder) and are swapped once the capture arrives.
        let fg = unsafe { GetForegroundWindow() };
        let fg = (!hidden && win::is_switchable(fg)).then_some(fg.0 as isize);
        if !hidden {
            self.last_fg = None;
        }
        // Fullscreen on the chosen monitor (by default the one under the cursor); every
        // monitor shows the same cards and camera.
        let (mx, my, mw, mh) = overlay_monitor(&self.cfg.monitor);
        let (captures, jobs) = self.scan(fg);
        lap("scan");

        // Cards whose texture is still on the warm GPU need no composing at all.
        let parked = self.parked.take();
        let gpu = if self.cfg.keep_warm && parked.is_none() {
            self.take_gpu()
        } else {
            None
        };
        lap("gpu-wait");
        let order: HashMap<u64, usize> = jobs.iter().enumerate().map(|(i, j)| (j.id, i)).collect();
        let (cached, jobs): (Vec<CardJob>, Vec<CardJob>) =
            jobs.into_iter().partition(|j| match &parked {
                Some(ov) => ov.has_cached(j.id, j.key),
                None => gpu.as_ref().is_some_and(|g| g.has_cached(j.id, j.key)),
            });
        let composed = jobs.len();
        let cards = composed + cached.len();
        let threads = std::thread::available_parallelism()
            .map_or(2, |n| n.get())
            .clamp(1, 4);
        let per = jobs.len().div_ceil(threads).max(1);
        let mut workers = Vec::new();
        let mut rest = jobs;
        while !rest.is_empty() {
            let chunk: Vec<CardJob> = rest.drain(..per.min(rest.len())).collect();
            workers.push(std::thread::spawn(move || {
                unsafe {
                    let _ = CoInitializeEx(None, COINIT_APARTMENTTHREADED);
                }
                chunk
                    .into_iter()
                    .map(|j| {
                        let img = compose_card(j.kind, j.ghost, &j.title, &j.exe, j.id);
                        (j, img)
                    })
                    .collect::<Vec<_>>()
            }));
        }

        let reused = parked.is_some();
        let mut ov = if let Some(mut ov) = parked {
            unsafe {
                let _ = SetWindowPos(
                    self.overlay_hwnd,
                    None,
                    mx,
                    my,
                    mw,
                    mh,
                    SWP_NOACTIVATE | SWP_NOZORDER,
                );
            }
            ov.reopen(mw as u32, mh as u32, self.model.layout.camera);
            ov
        } else {
            let Some(hwnd) = self.create_overlay_window(mx, my, mw, mh) else {
                return;
            };
            if !hidden {
                show_overlay_window(hwnd);
            }
            self.overlay_hwnd = hwnd;
            lap("window");
            match Overlay::new(
                gpu,
                hwnd.0 as isize,
                self.hinstance.0 as isize,
                mw as u32,
                mh as u32,
                &self.cfg,
                self.model.layout.camera,
            ) {
                Ok(o) => o,
                Err(e) => {
                    store::log(&format!("GPU init failed: {e}"));
                    self.destroy_overlay_window();
                    win::message_box(&format!("Window3D could not initialise the GPU:\n{e}"));
                    return;
                }
            }
        };
        lap("gpu");
        // Keep the scene order: cached cards and composed ones interleave by original position.
        let mut all: Vec<(CardJob, Option<image::Image>)> =
            cached.into_iter().map(|j| (j, None)).collect();
        for wk in workers {
            all.extend(
                wk.join()
                    .unwrap_or_default()
                    .into_iter()
                    .map(|(j, img)| (j, Some(img))),
            );
        }
        all.sort_by_key(|(j, _)| order.get(&j.id).copied().unwrap_or(usize::MAX));
        for (j, img) in all {
            if !ov.add_card(j.id, j.kind, j.ghost, j.hwnd, j.key, img.as_ref()) {
                // The kept GPU had to be recreated, losing its cache: compose now.
                let img = compose_card(j.kind, j.ghost, &j.title, &j.exe, j.id);
                ov.add_card(j.id, j.kind, j.ghost, j.hwnd, j.key, Some(&img));
            }
        }
        for c in captures {
            let _ = self.capture_tx.send(c);
        }
        lap("cards");

        let hwnd = self.overlay_hwnd;
        if reused {
            // Draw the new scene into the hidden swapchain first: no stale frame shows up.
            ov.frame(&mut self.model, false);
            lap("frame");
            if !hidden {
                show_overlay_window(hwnd);
            }
        }
        self.focused = unsafe { GetForegroundWindow() } == hwnd;
        self.overlay = Some(ov);
        if !hidden {
            watch_windows(true);
            // Warms the shell popup search's caches (its first run is slow), off this thread.
            std::thread::spawn(win::shell_scan_report);
            self.grab();
            statswin::raise();
            self.apply_audio();
        }
        lap("done");
        store::log(&format!(
            "overlay opened in {} ms ({cards} cards, {composed} composed):{phases}",
            t0.elapsed().as_millis(),
        ));
    }

    fn create_overlay_window(&self, x: i32, y: i32, w: i32, h: i32) -> Option<HWND> {
        let class = win::wide(OVERLAY_CLASS);
        let hwnd = unsafe {
            CreateWindowExW(
                if self.headless {
                    WS_EX_TOOLWINDOW | WS_EX_NOACTIVATE
                } else {
                    WS_EX_TOPMOST | WS_EX_TOOLWINDOW
                },
                PCWSTR(class.as_ptr()),
                w!("Window3D"),
                WS_POPUP,
                x,
                y,
                w,
                h,
                None,
                None,
                Some(self.hinstance),
                None,
            )
        };
        match hwnd {
            Ok(h) => win::no_capture(h.0 as isize, "the 3D view"),
            Err(_) => store::log("failed to create overlay window"),
        }
        hwnd.ok()
    }

    /// Lock, sleep and display changes. The overlay closes (its monitor, the GPU or the
    /// desktop it is on may be gone when it comes back); sleep and display changes also drop
    /// every GPU object, made again on the next open (or warm-up).
    fn on_system(&mut self, what: Sys) {
        store::log(&format!(
            "system event {what:?} (overlay {})",
            if self.overlay.is_some() { "open" } else { "closed" }
        ));
        self.hide();
        if what == Sys::Lock {
            return;
        }
        self.drop_gpu();
        if what != Sys::Suspend && self.cfg.keep_surface {
            push(Event::Warm);
        }
    }

    /// Releases the parked overlay, the warm GPU and the kept adapter.
    fn drop_gpu(&mut self) {
        self.hide();
        if self.parked.take().is_some() {
            self.destroy_overlay_window();
        }
        if let Some(rx) = self.warmup.take() {
            let _ = rx.recv();
        }
        self.gpu = None;
        render::forget_adapter();
        unsafe {
            let _ = K32EmptyWorkingSet(GetCurrentProcess());
        }
    }

    fn destroy_overlay_window(&mut self) {
        if !self.overlay_hwnd.0.is_null() {
            unsafe {
                let _ = DestroyWindow(self.overlay_hwnd);
            }
            self.overlay_hwnd = HWND::default();
        }
    }

    /// Tears down the overlay (and, unless `keep_warm`, every GPU object), then trims the
    /// working set.
    /// Mutes the apps of window cards whose speaker is off (an app with any card speaker on
    /// keeps its sound).
    fn apply_audio(&mut self) {
        self.audio_next = std::time::Instant::now() + std::time::Duration::from_secs(2);
        if self.headless || self.overlay.is_none() {
            return;
        }
        let mut on = HashSet::new();
        let mut off = HashSet::new();
        for e in &self.model.layout.entries {
            if e.kind == store::Kind::Window && !e.exe.is_empty() {
                let exe = e.exe.to_lowercase();
                if e.sound {
                    on.insert(exe);
                } else {
                    off.insert(exe);
                }
            }
        }
        off.retain(|x| !on.contains(x));
        self.muter.apply(&off);
    }

    /// Unmutes everything muted here, and keeps checking every few seconds while some muted
    /// app is closed (it comes back muted when started again).
    fn give_back_sound(&mut self) {
        self.muter.restore();
        unsafe {
            if self.muter.pending() {
                SetTimer(Some(self.msg_hwnd), AUDIO_TIMER, 3000, None);
            } else {
                let _ = KillTimer(Some(self.msg_hwnd), AUDIO_TIMER);
            }
        }
    }

    fn hide(&mut self) {
        watch_windows(false);
        self.give_back_sound();
        if self.launching.take().is_some() {
            win::unlock_foreground();
        }
        let Some(ov) = self.overlay.take() else {
            return;
        };
        let mut ov = ov;
        ov.end_interact("overlay hidden");
        win::show_shell(false);
        ov.unshape();
        overlay::INTERACT.store(false, std::sync::atomic::Ordering::Relaxed);
        self.model.layout.camera = Some(ov.camera_state());
        let park = self.cfg.keep_warm && self.cfg.keep_surface;
        if park {
            ov.park();
            unsafe {
                let _ = ShowWindow(self.overlay_hwnd, SW_HIDE);
            }
            self.parked = Some(ov);
        } else if self.cfg.keep_warm {
            let gpu = ov.into_gpu();
            store::log(&format!(
                "kept GPU warm with {} MB of card textures",
                gpu.cache_bytes() >> 20
            ));
            self.gpu = Some(gpu);
        } else {
            ov.finish();
            drop(ov);
        }
        self.ungrab();
        FREE.store(false, std::sync::atomic::Ordering::Relaxed);
        if !park {
            self.destroy_overlay_window();
        }
        self.focused = false;
        store::save_layout(&self.model.layout);
        unsafe {
            let _ = K32EmptyWorkingSet(GetCurrentProcess());
        }
    }

    /// Takes the mouse for flying: raw mouse input, the cursor clipped to the overlay.
    fn grab(&mut self) {
        let hwnd = self.overlay_hwnd;
        unsafe {
            let dev = RAWINPUTDEVICE {
                usUsagePage: 0x01,
                usUsage: 0x02,
                // Also in the background: a window used in place may take the focus.
                dwFlags: RIDEV_INPUTSINK,
                hwndTarget: hwnd,
            };
            let _ = RegisterRawInputDevices(&[dev], size_of::<RAWINPUTDEVICE>() as u32);
            let mut clip = RECT::default();
            if GetWindowRect(hwnd, &mut clip).is_ok() {
                let _ = ClipCursor(Some(&clip));
            }
        }
        self.grabbed = true;
        overlay::RAW_INPUT.store(true, std::sync::atomic::Ordering::Relaxed);
    }

    fn ungrab(&mut self) {
        overlay::RAW_INPUT.store(false, std::sync::atomic::Ordering::Relaxed);
        if std::mem::take(&mut self.grabbed) {
            unsafe {
                let dev = RAWINPUTDEVICE {
                    usUsagePage: 0x01,
                    usUsage: 0x02,
                    dwFlags: RIDEV_REMOVE,
                    hwndTarget: HWND::default(),
                };
                let _ = RegisterRawInputDevices(&[dev], size_of::<RAWINPUTDEVICE>() as u32);
                let _ = ClipCursor(None);
            }
        }
    }

    /// Tab: frees the cursor (mouse-look off, the mouse can go to another monitor and use other
    /// windows while the overlay stays) or takes it back.
    fn set_free(&mut self, free: bool) {
        if self.overlay.is_none() || self.headless {
            return;
        }
        FREE.store(free, std::sync::atomic::Ordering::Relaxed);
        if let Some(ov) = &mut self.overlay {
            ov.set_free(free);
        }
        if free {
            self.ungrab();
            unsafe {
                // Show the cursor where the crosshair is.
                let mut r = RECT::default();
                if GetWindowRect(self.overlay_hwnd, &mut r).is_ok() {
                    let _ = SetCursorPos((r.left + r.right) / 2, (r.top + r.bottom) / 2);
                }
            }
        } else {
            unsafe {
                let _ = SetForegroundWindow(self.overlay_hwnd);
            }
            self.grab();
        }
    }

    /// M: moves the open overlay to the next monitor and makes that the configured one.
    fn next_monitor(&mut self) {
        let list = monitors();
        if self.overlay.is_none() || list.len() < 2 {
            return;
        }
        let mut cur = RECT::default();
        unsafe {
            let _ = GetWindowRect(self.overlay_hwnd, &mut cur);
        }
        let i = list
            .iter()
            .position(|m| m.0 == cur.left && m.1 == cur.top)
            .map_or(0, |i| (i + 1) % list.len());
        self.cfg.monitor = (i + 1).to_string();
        store::save_config(&self.cfg);
        // Reopen there: the same cards, the camera where it was.
        let last = self.last_fg;
        self.hide();
        self.show();
        self.last_fg = last;
    }

    /// Focuses a real window and sizes it according to `focus_mode` (or borderless fullscreen
    /// when `fullscreen`). Shift+click focuses it as it is (and gives back the frame if
    /// Window3D removed it).
    fn focus_card_window(&mut self, h: isize, fullscreen: bool) {
        let hwnd = hwnd_of(h);
        self.frames
            .retain(|&k, _| unsafe { IsWindow(Some(hwnd_of(k))).as_bool() });
        let shift = unsafe { GetAsyncKeyState(VK_SHIFT.0 as i32) } < 0;
        let mut mode = if fullscreen {
            win::FocusMode::Borderless
        } else {
            win::FocusMode::parse(&self.cfg.focus_mode)
        };
        if shift && !fullscreen {
            mode = win::FocusMode::Keep;
            if let Some(f) = self.frames.remove(&h) {
                win::restore_frame(hwnd, &f);
            }
        }
        let info = win::win_info(hwnd);
        if mode == win::FocusMode::Borderless {
            let exe = std::path::Path::new(&info.exe)
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_default();
            let excluded = self
                .cfg
                .borderless_exclude
                .iter()
                .any(|e| e.eq_ignore_ascii_case(&exe) || e.eq_ignore_ascii_case(&info.class));
            if excluded {
                mode = win::FocusMode::Maximize;
            }
        }
        let (_, saved) = win::focus_and_size(hwnd, &info.class, mode);
        if let Some(f) = saved {
            self.frames.entry(h).or_insert(f);
        }
    }

    fn restore_frames(&mut self) {
        for (h, f) in self.frames.drain() {
            win::restore_frame(hwnd_of(h), &f);
        }
    }

    fn handle_action(&mut self, a: Action) {
        self.do_action(a);
        self.sync_menu_mouse();
    }

    /// While the options menu is open the cursor is free and shown over it; it is taken
    /// back for flying when the menu closes.
    fn sync_menu_mouse(&mut self) {
        let open = !self.headless && self.overlay.as_ref().is_some_and(|o| o.menu_open());
        if open == MENU.load(std::sync::atomic::Ordering::Relaxed) {
            return;
        }
        MENU.store(open, std::sync::atomic::Ordering::Relaxed);
        let free = FREE.load(std::sync::atomic::Ordering::Relaxed);
        if open {
            self.ungrab();
            unsafe {
                let _ = SetForegroundWindow(self.overlay_hwnd);
                let mut r = RECT::default();
                if GetWindowRect(self.overlay_hwnd, &mut r).is_ok() {
                    // With a free cursor (Tab) it may still leave the monitor.
                    if !free {
                        let _ = ClipCursor(Some(&r));
                    }
                    let _ = SetCursorPos((r.left + r.right) / 2, (r.top + r.bottom) / 2);
                }
            }
        } else {
            unsafe {
                let _ = ClipCursor(None);
            }
            if self.overlay.is_some() && !free {
                self.grab();
            }
        }
    }

    /// The settings the options menu shows.
    fn menu_settings(&self) -> overlay::MenuSettings {
        overlay::MenuSettings {
            gpu: self.cfg.gpu.clone(),
            msaa: self.cfg.msaa,
            fov_degrees: self.cfg.fov_degrees,
            mouse_sensitivity: self.cfg.mouse_sensitivity,
            move_speed: self.cfg.move_speed,
            card_titles: self.cfg.card_titles,
            show_ghosts: self.cfg.show_ghosts,
            show_hints: self.cfg.show_hints,
            max_fps: self.cfg.max_fps,
            live_fps: self.cfg.live_fps,
            background: self.cfg.background.clone(),
        }
    }

    fn do_action(&mut self, a: Action) {
        match a {
            Action::None => {}
            Action::Close => self.hide(),
            Action::OpenMenu => {
                let settings = self.menu_settings();
                let gpus = render::adapter_names();
                if let Some(ov) = &mut self.overlay {
                    ov.open_menu(settings, gpus);
                }
            }
            Action::ApplySettings(s) => self.apply_settings(s),
            Action::Focus(h) => {
                // Focus first while we still own the foreground, then vanish.
                stats::switch_start(h);
                self.focus_card_window(h, false);
                self.hide();
                self.last_fg = Some(h);
            }
            Action::Theater(h) if win::is_desktop(hwnd_of(h)) => {
                // Flown into the desktop card (the whole screen as it is): the overlay steps
                // aside and the real screen is used. The hotkey brings the 3D view back.
                store::log("desktop card: handed over to the real screen");
                self.hide();
            }
            Action::Theater(h) => {
                // The camera has flown into the card: the real window takes its place,
                // borderless fullscreen, and the overlay steps aside (staying warm). The hotkey
                // brings the overlay back and gives the window its frame again.
                let had = self.frames.contains_key(&h);
                stats::switch_start(h);
                self.focus_card_window(h, true);
                if !had && self.frames.contains_key(&h) {
                    self.theater_frame = Some(h);
                }
                self.hide();
                self.last_fg = Some(h);
            }
            Action::Launch(path) => {
                // The app opens behind the overlay and comes in as a card in front of the user.
                let known: HashSet<isize> =
                    win::enumerate_windows().iter().map(|w| w.hwnd).collect();
                if let Some(pid) = win::launch_behind(&path) {
                    store::log(&format!("launched {path} (pid {pid}) behind the overlay"));
                    let now = std::time::Instant::now();
                    let l = self.launching.get_or_insert(Launching {
                        until: now,
                        next_poll: now,
                        before: known.clone(),
                        known,
                        pids: Vec::new(),
                        any: false,
                        placed: 0,
                        recapture: Vec::new(),
                        from_card: false,
                    });
                    l.until = now + std::time::Duration::from_secs(60);
                    if pid == 0 {
                        l.any = true;
                    } else {
                        l.pids.push(pid);
                    }
                } else {
                    store::log(&format!("failed to launch {path}"));
                    win::unlock_foreground();
                }
            }
            Action::CloseWindow(id, kill) => {
                let h = self.overlay.as_ref().and_then(|ov| ov.card_hwnd(id));
                if let Some(h) = h {
                    if win::close_window(hwnd_of(h), kill) {
                        // Gone: the card turns into a ghost the next time the room opens.
                        if let Some(ov) = &mut self.overlay {
                            ov.remove_card(id);
                        }
                    } else {
                        // Still there, most likely asking to save: show it.
                        let popup = unsafe { GetLastActivePopup(hwnd_of(h)) };
                        let p = if popup.is_invalid() {
                            h
                        } else {
                            popup.0 as isize
                        };
                        self.focus_card_window(p, false);
                        self.hide();
                        self.last_fg = Some(p);
                    }
                }
            }
            Action::Forget(id) => {
                self.model.remove(id);
                if let Some(ov) = &mut self.overlay {
                    ov.remove_card(id);
                }
                store::save_layout(&self.model.layout);
            }
            Action::LayoutChanged => {
                self.model.follow_wall();
                store::save_layout(&self.model.layout);
            }
            Action::SetLive(id, on) => {
                if let Some(e) = self.model.get_mut(id) {
                    e.live = on;
                }
                store::save_layout(&self.model.layout);
            }
            Action::Watch(id) => {
                if let Some(e) = self.model.get_mut(id) {
                    (e.live, e.sound) = (true, true);
                }
                store::save_layout(&self.model.layout);
                self.apply_audio();
            }
            Action::AudioChanged => {
                store::save_layout(&self.model.layout);
                self.apply_audio();
            }
            Action::SetSound(id, on) => {
                if let Some(e) = self.model.get_mut(id) {
                    e.sound = on;
                }
                store::save_layout(&self.model.layout);
                self.apply_audio();
            }
        }
    }

    fn process_events(&mut self) {
        self.handle_events();
        // The menu may have closed with the overlay (lock, display change).
        self.sync_menu_mouse();

        // Screenshots: the 3D view back in screen captures once the Snipping Tool is done.
        let _ = snip::sync();
    }

    fn handle_events(&mut self) {
        while let Some(e) = pop() {
            match e {
                Event::Toggle => {
                    // While a window is used in place, the hotkey goes back to flying.
                    if let Some(ov) = self
                        .overlay
                        .as_mut()
                        .filter(|o| o.interact_hwnd().is_some())
                    {
                        ov.end_interact_refocus("hotkey");
                    } else if self.overlay.is_some() {
                        self.hide()
                    } else {
                        self.reload_config();
                        self.show()
                    }
                }
                Event::Quit => self.quit = true,
                Event::OpenFolder => {
                    win::launch(&store::app_dir().to_string_lossy());
                }
                Event::RestoreFrames => self.restore_frames(),
                Event::SaveWorkflow(what) => self.save_workflow(what),
                Event::LoadWorkflow(p) => self.load_workflow(&p),
                Event::OpenWorkflowFolder => {
                    let d = workflow::dir();
                    let _ = std::fs::create_dir_all(&d);
                    win::launch(&d.to_string_lossy());
                }
                Event::OpenBackgroundFolder => {
                    let d = background::folder();
                    let _ = std::fs::create_dir_all(&d);
                    win::launch(&d.to_string_lossy());
                }
                Event::OpenConfig => {
                    let p = store::app_dir().join("config.json");
                    let t = win::wide(&p.to_string_lossy());
                    unsafe {
                        ShellExecuteW(
                            None,
                            w!("open"),
                            w!("notepad.exe"),
                            PCWSTR(t.as_ptr()),
                            PCWSTR::null(),
                            SW_SHOWNORMAL,
                        );
                    }
                }
                Event::TrayMenu => self.tray_menu(),
                Event::TaskbarCreated => self.add_tray(),
                Event::Foreground(h) => self.on_foreground(h),
                Event::Minimized(h) => self.on_minimized(h),
                Event::Shown(h) => self.on_shown(h),
                Event::Gone(h) => self.on_gone(h),
                // While the view is open, `apply_audio` does this.
                Event::AudioTimer if self.overlay.is_none() => self.give_back_sound(),
                Event::AudioTimer => {}
                Event::Captured => {
                    while let Ok(c) = self.captured_rx.try_recv() {
                        self.on_captured(c);
                    }
                }
                Event::Warm => {
                    // Build the overlay hidden and park it, so even the first open is instant.
                    if self.cfg.keep_surface && self.overlay.is_none() && self.parked.is_none() {
                        self.show_with(true);
                        self.hide();
                    }
                }
                Event::Close => self.hide(),
                Event::Activated(active) => {
                    self.focused = active;
                    // Alt+Tab, Win key, a notification stealing focus...: get out of the way.
                    // Using a window in place hands it the focus on purpose, and with a free
                    // cursor (Tab) the user may be working on another monitor.
                    if !active
                        && !overlay::INTERACT.load(std::sync::atomic::Ordering::Relaxed)
                        && !FREE.load(std::sync::atomic::Ordering::Relaxed)
                    {
                        // ...unless it is an app just launched from here grabbing it.
                        let fg = unsafe { GetForegroundWindow() }.0 as isize;
                        let shell = win::is_shell_popup(hwnd_of(fg))
                            && self
                                .overlay
                                .as_mut()
                                .is_some_and(|ov| ov.show_desktop(&self.model));
                        if shell {
                            // Start, search or a flyout: shown and used on the desktop card.
                            store::log(&format!(
                                "a shell popup took the focus ({}): to the desktop card",
                                win::describe(hwnd_of(fg))
                            ));
                        } else if self.launch_stole(fg) {
                            self.retake_focus();
                        } else {
                            self.follow(fg);
                        }
                    }
                }
                Event::ToggleStats => statswin::toggle(self.hinstance, &self.cfg.stats_fields),
                Event::StatsFields(f) => {
                    self.cfg.stats_fields = f;
                    store::save_config(&self.cfg);
                }
                Event::SetMonitor(m) => {
                    self.cfg.monitor = m;
                    store::save_config(&self.cfg);
                }
                Event::Regrab => self.set_free(false),
                Event::System(what) => self.on_system(what),
                Event::Resize(w, h) => {
                    if let Some(ov) = &mut self.overlay {
                        ov.resize(w, h);
                    }
                }
                Event::MouseDelta(dx, dy) => {
                    if let Some(ov) = &mut self.overlay {
                        ov.mouse_delta(dx, dy);
                    }
                }
                Event::Button(down) => {
                    if let Some(ov) = &mut self.overlay {
                        let a = ov.button(down, &mut self.model);
                        self.handle_action(a);
                    }
                }
                Event::Wheel(d) => {
                    if let Some(ov) = &mut self.overlay {
                        ov.wheel(d);
                    }
                }
                Event::MenuMouse(x, y, click) => {
                    if let Some(ov) = &mut self.overlay {
                        let a = ov.menu_pointer(x, y, click);
                        self.handle_action(a);
                    }
                }
                Event::Char(c) => {
                    if let Some(ov) = &mut self.overlay {
                        ov.char_typed(c, &self.model);
                    }
                }
                Event::Key(vk)
                    if vk == VK_TAB.0
                        && self
                            .overlay
                            .as_ref()
                            .is_some_and(|o| !o.searching() && !o.menu_open()) =>
                {
                    let free = !FREE.load(std::sync::atomic::Ordering::Relaxed);
                    self.set_free(free);
                }
                Event::Key(vk)
                    if vk == VK_M.0
                        && self
                            .overlay
                            .as_ref()
                            .is_some_and(|o| !o.searching() && !o.menu_open()) =>
                {
                    self.next_monitor()
                }
                Event::Key(vk) => {
                    if let Some(ov) = &mut self.overlay {
                        let a = ov.key(vk, &mut self.model);
                        self.handle_action(a);
                    }
                }
                Event::Pass(msg, wp, lp, t) => {
                    if let Some(ov) = &mut self.overlay {
                        ov.pass_input(msg, wp, lp);
                        stats::record_input(t.elapsed());
                    }
                }
            }
        }
    }
}

fn register_class(hinstance: HINSTANCE, name: &str, proc_: WNDPROC, bg: HBRUSH) -> bool {
    let cname = win::wide(name);
    let wc = WNDCLASSEXW {
        cbSize: size_of::<WNDCLASSEXW>() as u32,
        lpfnWndProc: proc_,
        hInstance: hinstance,
        lpszClassName: PCWSTR(cname.as_ptr()),
        hbrBackground: bg,
        ..Default::default()
    };
    unsafe { RegisterClassExW(&wc) != 0 }
}

/// A black 200x100 popup of `owner` at (`x`, `y`) drawn with UpdateLayeredWindow, shown.
fn ulw_popup(owner: HWND, x: i32, y: i32) -> Option<HWND> {
    use windows::Win32::Graphics::Gdi::*;
    let (w, h) = (200, 100);
    unsafe {
        let pop = CreateWindowExW(
            WS_EX_LAYERED | WS_EX_TOOLWINDOW | WS_EX_NOACTIVATE,
            w!("STATIC"),
            w!(""),
            WS_POPUP,
            x,
            y,
            w,
            h,
            Some(owner),
            None,
            None,
            None,
        )
        .ok()?;
        let dc = CreateCompatibleDC(None);
        let bi = BITMAPINFO {
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
        let mut bits = std::ptr::null_mut();
        let Ok(bmp) = CreateDIBSection(Some(dc), &bi, DIB_RGB_COLORS, &mut bits, None, 0) else {
            let _ = DeleteDC(dc);
            let _ = DestroyWindow(pop);
            return None;
        };
        // Opaque black (premultiplied BGRA).
        std::slice::from_raw_parts_mut(bits as *mut u32, (w * h) as usize).fill(0xff00_0000);
        let old = SelectObject(dc, bmp.into());
        let size = windows::Win32::Foundation::SIZE { cx: w, cy: h };
        let src = windows::Win32::Foundation::POINT::default();
        let dst = windows::Win32::Foundation::POINT { x, y };
        let blend = BLENDFUNCTION {
            BlendOp: AC_SRC_OVER as u8,
            SourceConstantAlpha: 255,
            AlphaFormat: AC_SRC_ALPHA as u8,
            ..Default::default()
        };
        let ok = UpdateLayeredWindow(pop, None, Some(&dst), Some(&size), Some(dc), Some(&src), Default::default(), Some(&blend), ULW_ALPHA);
        SelectObject(dc, old);
        let _ = DeleteObject(bmp.into());
        let _ = DeleteDC(dc);
        if ok.is_err() {
            let _ = DestroyWindow(pop);
            return None;
        }
        let _ = ShowWindow(pop, SW_SHOWNOACTIVATE);
        Some(pop)
    }
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let selftest_dir = args
        .iter()
        .position(|a| a == "--selftest")
        .map(|i| std::path::PathBuf::from(args.get(i + 1).cloned().unwrap_or_else(|| ".".into())));
    unsafe {
        let _ = SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2);
        let _mutex = CreateMutexW(None, true, w!("Local\\Window3D.SingleInstance"));
        if GetLastError() == ERROR_ALREADY_EXISTS && selftest_dir.is_none() {
            win::message_box("Window3D is already running (see the tray icon).");
            return;
        }
        let _ = CoInitializeEx(None, COINIT_APARTMENTTHREADED);
    }
    store::ensure_dirs();
    // Release builds abort on panic with no console: leave the reason in the log.
    pointer::recover();
    win::recover_shell();
    snip::start();
    std::panic::set_hook(Box::new(|info| {
        pointer::panic_restore();
        win::panic_show_shell();
        audio::emergency_restore();
        store::log(&format!(
            "PANIC: {info}
{}",
            std::backtrace::Backtrace::force_capture()
        ));
    }));
    let cfg = store::load_config();
    win::CARD_TITLES.store(cfg.card_titles, std::sync::atomic::Ordering::Relaxed);
    overlay::SHOW_HINTS.store(cfg.show_hints, std::sync::atomic::Ordering::Relaxed);
    live::LIVE_FPS.store(cfg.live_fps, std::sync::atomic::Ordering::Relaxed);
    live::MAX_FPS.store(cfg.max_fps, std::sync::atomic::Ordering::Relaxed);

    let hinstance: HINSTANCE =
        unsafe { GetModuleHandleW(None).map(|h| h.into()).unwrap_or_default() };
    let black = unsafe { HBRUSH(GetStockObject(BLACK_BRUSH).0) };
    if !register_class(hinstance, MSG_CLASS, Some(msg_wndproc), HBRUSH::default())
        || !register_class(hinstance, OVERLAY_CLASS, Some(overlay_wndproc), black)
    {
        win::message_box("Window3D: failed to register window classes.");
        return;
    }
    let msg_class = win::wide(MSG_CLASS);
    // A hidden top-level window (not HWND_MESSAGE) so it also receives the TaskbarCreated broadcast.
    let Ok(msg_hwnd) = (unsafe {
        CreateWindowExW(
            WS_EX_TOOLWINDOW,
            PCWSTR(msg_class.as_ptr()),
            w!("Window3D"),
            WS_POPUP,
            0,
            0,
            0,
            0,
            None,
            None,
            Some(hinstance),
            None,
        )
    }) else {
        win::message_box("Window3D: failed to create message window.");
        return;
    };
    TASKBAR_CREATED.with(|c| c.set(unsafe { RegisterWindowMessageW(w!("TaskbarCreated")) }));
    // Win+L: WM_WTSSESSION_CHANGE closes an open overlay.
    unsafe {
        let _ = windows::Win32::System::RemoteDesktop::WTSRegisterSessionNotification(
            msg_hwnd,
            windows::Win32::System::RemoteDesktop::NOTIFY_FOR_THIS_SESSION,
        );
    }

    match parse_hotkey(&cfg.hotkey).filter(|_| selftest_dir.is_none()) {
        Some((mods, vk)) => unsafe {
            if RegisterHotKey(Some(msg_hwnd), HOTKEY_ID, mods, vk).is_err() {
                win::message_box(&format!(
                    "Window3D: the hotkey {} is already used by another program.\nChange \"hotkey\" in {}\\config.json. The tray icon still works.",
                    cfg.hotkey,
                    store::app_dir().display()
                ));
            }
        },
        None if selftest_dir.is_some() => {}
        None => win::message_box(&format!(
            "Window3D: cannot understand hotkey \"{}\" in config.json.",
            cfg.hotkey
        )),
    }

    let hook = unsafe {
        SetWinEventHook(
            EVENT_SYSTEM_FOREGROUND,
            EVENT_SYSTEM_FOREGROUND,
            None,
            Some(foreground_hook),
            0,
            0,
            WINEVENT_OUTOFCONTEXT,
        )
    };

    let (capture_tx, captured_rx) = spawn_capture_worker(msg_hwnd.0 as isize, cfg.screenshot_size);
    let tray_icon = win::make_icon(&win::app_icon_image(32));
    let mut app = App {
        model: Model::load(),
        cfg,
        cfg_stamp: config_stamp(),
        hinstance,
        msg_hwnd,
        tray_icon,
        overlay: None,
        parked: None,
        overlay_hwnd: HWND::default(),
        grabbed: false,
        frames: HashMap::new(),
        theater_frame: None,
        muter: if selftest_dir.is_some() { audio::Muter::default() } else { audio::Muter::load() },
        audio_next: std::time::Instant::now(),
        focused: false,
        last_fg: None,
        capture_tx,
        captured_rx,
        gpu: None,
        warmup: None,
        quit: false,
        headless: selftest_dir.is_some(),
        launching: None,
        window_checks: Vec::new(),
        lost: None,
    };
    // Give back the sound of apps an earlier run left muted (killed, crashed, powered off).
    app.give_back_sound();
    // Start the GPU and upload the cards in the background so even the first open is fast.
    if app.cfg.keep_warm {
        let (_, jobs) = app.scan(None);
        let (tx, rx) = mpsc::channel();
        let (msaa, gpu, keep) = (app.cfg.msaa, app.cfg.gpu.clone(), app.cfg.keep_gpu_adapter);
        let msg = app.msg_hwnd.0 as isize;
        std::thread::spawn(move || {
            let t = std::time::Instant::now();
            let g = Gpu::new(msaa, &gpu, keep)
                .map_err(|e| store::log(&format!("GPU warm-up failed: {e}")))
                .ok()
                .map(|mut g| {
                    let t_gpu = t.elapsed().as_millis();
                    unsafe {
                        let _ = CoInitializeEx(None, COINIT_APARTMENTTHREADED);
                    }
                    for j in &jobs {
                        let img = compose_card(j.kind, j.ghost, &j.title, &j.exe, j.id);
                        g.preload(j.id, j.key, &img);
                    }
                    g.poll();
                    // Page the start-up allocations out: idle stays below 1 MB working set.
                    unsafe {
                        let _ = K32EmptyWorkingSet(GetCurrentProcess());
                    }
                    store::log(&format!(
                        "warm-up: gpu {t_gpu} ms, {} cards {} ms",
                        jobs.len(),
                        t.elapsed().as_millis()
                    ));
                    g
                });
            let _ = tx.send(g);
            unsafe {
                let _ = PostMessageW(Some(hwnd_of(msg)), WM_APP_WARM, WPARAM(0), LPARAM(0));
            }
        });
        app.warmup = Some(rx);
    }
    if let Some(dir) = selftest_dir {
        run_selftest(&mut app, &dir);
        return;
    }
    app.add_tray();
    if !args.iter().any(|a| a == "--quiet") {
        app.greet();
    }
    let fg = unsafe { GetForegroundWindow() };
    app.last_fg = win::is_switchable(fg).then_some(fg.0 as isize);
    unsafe {
        let _ = K32EmptyWorkingSet(GetCurrentProcess());
    }

    let mut msg = MSG::default();
    // Frame pacing: frames are at most `max_fps` apart; after a second with nothing to draw
    // the view only checks for changes ~15 times a second, until input arrives.
    let timer = unsafe {
        CreateWaitableTimerExW(
            None,
            None,
            CREATE_WAITABLE_TIMER_HIGH_RESOLUTION,
            TIMER_ALL_ACCESS.0,
        )
    }
    .ok();
    let mut next_frame = std::time::Instant::now();
    let mut last_drawn = next_frame;
    let mut last_frame = next_frame;
    let mut last_input = next_frame;
    'outer: while !app.quit {
        if app.overlay.is_some() {
            // Overlay open: drain input, then (when due) one frame.
            let mut input = false;
            while unsafe { PeekMessageW(&mut msg, None, 0, 0, PM_REMOVE) }.as_bool() {
                if msg.message == WM_QUIT {
                    break 'outer;
                }
                input |= matches!(
                    msg.message,
                    WM_INPUT | WM_KEYDOWN | WM_SYSKEYDOWN | WM_MOUSEMOVE | WM_LBUTTONDOWN
                        | WM_RBUTTONDOWN | WM_MOUSEWHEEL
                );
                unsafe {
                    let _ = TranslateMessage(&msg);
                    DispatchMessageW(&msg);
                }
            }
            let min_gap = match app.cfg.max_fps {
                0 => std::time::Duration::ZERO,
                n => std::time::Duration::from_secs_f32(1.0 / n as f32),
            };
            if input {
                last_input = std::time::Instant::now();
                next_frame = next_frame.min(last_frame + min_gap);
            }
            let now = std::time::Instant::now();
            if now < next_frame && app.overlay.is_some() {
                let wait = next_frame - now;
                unsafe {
                    match &timer {
                        Some(t) => {
                            let due = -((wait.as_nanos() / 100) as i64).max(1);
                            let _ = SetWaitableTimer(*t, &due, 0, None, None, false);
                            MsgWaitForMultipleObjects(Some(&[*t]), false, 1000, QS_ALLINPUT);
                        }
                        None => {
                            MsgWaitForMultipleObjects(
                                None,
                                false,
                                wait.as_millis().max(1) as u32,
                                QS_ALLINPUT,
                            );
                        }
                    }
                }
                continue;
            }
            app.process_events();
            if app.launching.is_none()
                && app
                    .overlay
                    .as_ref()
                    .is_some_and(|ov| ov.interact_hwnd().is_some())
            {
                let now = std::time::Instant::now();
                app.launching = Some(Launching {
                    until: now,
                    next_poll: now,
                    before: HashSet::new(),
                    known: HashSet::new(),
                    pids: Vec::new(),
                    any: false,
                    placed: 0,
                    recapture: Vec::new(),
                    from_card: false,
                });
            }
            app.poll_launch();
            app.poll_windows();
            if let Some(ov) = &mut app.overlay {
                let a = ov.frame(&mut app.model, app.focused);
                let now = std::time::Instant::now();
                last_frame = now;
                if ov.rendered {
                    last_drawn = now;
                }
                next_frame = if !app.focused && FREE.load(std::sync::atomic::Ordering::Relaxed) {
                    // Left open on its monitor while the user works elsewhere: ~10 fps is plenty.
                    now + std::time::Duration::from_millis(100)
                } else if now - last_drawn > std::time::Duration::from_secs(1) {
                    now + std::time::Duration::from_millis(66)
                } else if !ov.moving
                    && !ov.live_on_screen()
                    && now - last_input > std::time::Duration::from_millis(250)
                {
                    // Only live cards off screen (capped at `live_fps`) or a hover pulse change:
                    // ~30 checks a second keep up with them.
                    (next_frame + min_gap.max(std::time::Duration::from_millis(33))).max(now)
                } else {
                    // Paced from the frame's start so the rate holds.
                    (next_frame + min_gap).max(now)
                };
                app.handle_action(a);
            }
            if render::take_lost() {
                store::log("GPU lost: closing the overlay and dropping the GPU");
                app.drop_gpu();
            }
            if std::time::Instant::now() >= app.audio_next {
                app.apply_audio();
            }
        } else {
            // Idle: block until the hotkey, tray or a focus change wakes us.
            if unsafe { GetMessageW(&mut msg, None, 0, 0) }.0 <= 0 {
                break;
            }
            unsafe {
                let _ = TranslateMessage(&msg);
                DispatchMessageW(&msg);
            }
            app.process_events();
        }
    }

    app.hide();
    win::show_shell(true);
    pointer::restore_cursors();
    win::unshape_wait();
    app.restore_frames();
    app.parked = None;
    app.destroy_overlay_window();
    store::save_layout(&app.model.layout);
    app.remove_tray();
    unsafe {
        let _ = UnregisterHotKey(Some(msg_hwnd), HOTKEY_ID);
        let _ = windows::Win32::System::RemoteDesktop::WTSUnRegisterSessionNotification(msg_hwnd);
        if !hook.is_invalid() {
            let _ = UnhookWinEvent(hook);
        }
        let _ = DestroyWindow(msg_hwnd);
    }
}

/// Lock, display change and a lost GPU: the overlay closes, GPU state is dropped and made
/// again, and the overlay still opens and renders afterwards.
fn selftest_system(app: &mut App) -> String {
    let frames_ok = |app: &mut App| {
        app.show_with(true);
        let ok = app.overlay.is_some();
        for _ in 0..3 {
            selftest::pump();
            while pop().is_some() {}
            if let Some(ov) = &mut app.overlay {
                ov.frame(&mut app.model, false);
            }
        }
        ok && !render::take_lost()
    };
    let opened = frames_ok(app);
    // The options menu (Esc) draws without crashing.
    app.handle_action(Action::OpenMenu);
    for _ in 0..2 {
        if let Some(ov) = &mut app.overlay {
            ov.frame(&mut app.model, false);
        }
    }
    let menu = app.overlay.as_ref().is_some_and(|o| o.menu_open());
    app.on_system(Sys::Lock);
    let lock_closes = app.overlay.is_none();
    frames_ok(app);
    app.on_system(Sys::Display);
    let display_drops = app.overlay.is_none() && app.parked.is_none() && app.gpu.is_none();
    // The warm-up queued by the display change re-parks the overlay (with keep_surface).
    app.process_events();
    let rewarmed = !app.cfg.keep_surface || app.parked.is_some();
    render::mark_lost();
    let reopen_after_lost = frames_ok(app);
    app.hide();
    format!(
        "system events test: {} (opened={opened} menu={menu} lock-closes={lock_closes} display-drops-gpu={display_drops} rewarmed={rewarmed} reopen-after-lost={reopen_after_lost})",
        opened && menu && lock_closes && display_drops && rewarmed && reopen_after_lost
    )
}

/// Shift+T: window cards piled up in one spot go back on the rings, apart from each other.
fn selftest_tidy_windows(app: &mut App) -> String {
    let ids: Vec<u64> = app
        .model
        .layout
        .entries
        .iter()
        .filter(|e| e.kind == Kind::Window && e.class != model::DESKTOP_CLASS)
        .map(|e| e.id)
        .take(30)
        .collect();
    let saved: Vec<([f32; 3], f32)> = ids
        .iter()
        .filter_map(|&id| app.model.get(id).map(|e| (e.pos, e.yaw)))
        .collect();
    for &id in &ids {
        if let Some(e) = app.model.get_mut(id) {
            e.pos = [0.0, 1.7, 0.0];
        }
    }
    app.model.tidy_windows(&ids);
    let pos: Vec<glam::Vec3> = ids
        .iter()
        .filter_map(|&id| app.model.get(id).map(|e| glam::Vec3::from(e.pos)))
        .collect();
    let apart = pos
        .iter()
        .enumerate()
        .all(|(i, a)| pos[i + 1..].iter().all(|b| a.distance(*b) >= 1.2));
    let in_room = pos.iter().all(|p| {
        p.x.abs() < render::ROOM_HALF && p.z.abs() < render::ROOM_HALF && p.y < render::ROOM_HEIGHT
    });
    for (&id, &(p, y)) in ids.iter().zip(&saved) {
        if let Some(e) = app.model.get_mut(id) {
            (e.pos, e.yaw) = (p, y);
        }
    }
    format!(
        "tidy windows test: {} (cards={} apart={apart} in-room={in_room})",
        apart && in_room && !ids.is_empty(),
        ids.len()
    )
}

/// Type-to-search: `/` and part of a card's title fly to that card; `M` is text then, `Esc`
/// flies back.
fn selftest_search(app: &mut App) -> String {
    app.show_with(true);
    let Some(ov) = &app.overlay else {
        return "search test: no overlay".into();
    };
    let ids: Vec<u64> = ov.card_ids().collect();
    let Some((id, word)) = ids.iter().find_map(|&id| {
        let e = app.model.get(id)?;
        let w = e
            .title
            .split(|c: char| !c.is_alphanumeric())
            .find(|w| w.chars().count() >= 4)?;
        Some((id, w.chars().take(5).collect::<String>()))
    }) else {
        app.hide();
        return "search test: no card with a title".into();
    };
    let monitor = app.cfg.monitor.clone();
    for c in format!("/{word}m").encode_utf16() {
        push(Event::Char(c));
    }
    push(Event::Key(VK_M.0));
    push(Event::Key(VK_BACK.0));
    app.process_events();
    let state = app.overlay.as_ref().and_then(|o| o.search_state());
    let found = state.is_some_and(|(n, sel, fly)| {
        n >= 1
            && fly
            && sel.and_then(|s| app.model.get(s)).is_some_and(|e| {
                (e.title.to_lowercase() + " " + &e.exe.to_lowercase())
                    .contains(&word.to_lowercase())
                    || e.id == id
            })
    });
    let m_is_text = app.cfg.monitor == monitor;
    push(Event::Key(VK_ESCAPE.0));
    app.process_events();
    let closed = app
        .overlay
        .as_ref()
        .is_some_and(|o| !o.searching() && o.search_state().is_none());
    app.hide();
    format!(
        "search test: {} (query={word:?} found={found} m-is-text={m_is_text} esc-closes={closed} state={state:?})",
        found && m_is_text && closed
    )
}

/// Whether the window events watched while the overlay is open arrive: a test window of our
/// own (off screen) is shown, then minimized.
fn selftest_window_events() -> String {
    use windows::core::w;
    let h = unsafe {
        CreateWindowExW(
            WS_EX_TOOLWINDOW,
            w!("STATIC"),
            w!("window3d events test"),
            WS_OVERLAPPEDWINDOW,
            -20000,
            -20000,
            200,
            100,
            None,
            None,
            None,
            None,
        )
    };
    let Ok(h) = h else {
        return "window events test: could not create the test window".into();
    };
    let me = h.0 as isize;
    let others: Vec<Event> = std::iter::from_fn(pop).collect();
    watch_windows(true);
    let (mut shown, mut minimized) = (false, false);
    let wait = |shown: &mut bool, minimized: &mut bool| {
        for _ in 0..20 {
            selftest::pump();
            while let Some(e) = pop() {
                match e {
                    Event::Shown(x) if x == me => *shown = true,
                    Event::Minimized(x) if x == me => *minimized = true,
                    _ => {}
                }
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
    };
    unsafe {
        let _ = ShowWindow(h, SW_SHOWNOACTIVATE);
    }
    wait(&mut shown, &mut minimized);
    unsafe {
        let _ = ShowWindow(h, SW_SHOWMINNOACTIVE);
    }
    wait(&mut shown, &mut minimized);
    // A click on a minimized window's card restores it without taking the focus.
    let was_min = win::restore_quietly(h);
    wait(&mut shown, &mut minimized);
    let restored = was_min
        && !win::is_minimized(h)
        && unsafe { GetForegroundWindow() } != h;
    watch_windows(false);
    unsafe {
        let _ = DestroyWindow(h);
    }
    others.into_iter().for_each(push);
    format!("window events test: shown {shown} minimized {minimized} restored-quietly {restored}")
}

fn selftest_live(ov: &mut overlay::Overlay, model: &model::Model) -> String {
    use windows::core::w;
    let h = unsafe {
        CreateWindowExW(
            WS_EX_TOOLWINDOW,
            w!("STATIC"),
            w!("window3d live test"),
            WS_OVERLAPPEDWINDOW,
            -20000,
            -20000,
            1920,
            1080,
            None,
            None,
            None,
            None,
        )
    };
    let Ok(h) = h else {
        return "live test: could not create the test window".into();
    };
    unsafe {
        let _ = ShowWindow(h, SW_SHOWNOACTIVATE);
    }
    let mut out = ov.selftest_live(h.0 as isize, false);
    unsafe {
        let _ = ShowWindow(h, SW_SHOWMINNOACTIVE);
    }
    out += "
";
    out += &ov.selftest_live(h.0 as isize, true);
    let mut r = windows::Win32::Foundation::RECT::default();
    unsafe {
        let _ = GetWindowRect(h, &mut r);
    }
    out += &format!(", window {}x{} minimized {}", r.right - r.left, r.bottom - r.top, win::is_minimized(h));
    // An owned popup (a dark box, like a menu) over the window used in place.
    unsafe {
        let _ = ShowWindow(h, SW_SHOWNOACTIVATE);
    }
    // On the primary screen (Windows draws no popups off the screen), made invisible.
    unsafe {
        let ex = GetWindowLongW(h, GWL_EXSTYLE);
        SetWindowLongW(h, GWL_EXSTYLE, ex | WS_EX_LAYERED.0 as i32);
        let _ = SetLayeredWindowAttributes(h, windows::Win32::Foundation::COLORREF(0), 1, LWA_ALPHA);
        let _ = SetWindowPos(h, None, 0, 0, 0, 0, SWP_NOSIZE | SWP_NOZORDER | SWP_NOACTIVATE);
    }
    let at = win::card_point(h.0 as isize, 0.0, 0.0).unwrap_or_default();
    let (x, y) = (300, 200);
    let pop = unsafe {
        CreateWindowExW(
            WS_EX_TOOLWINDOW | WS_EX_NOACTIVATE,
            w!("STATIC"),
            w!(""),
            WS_POPUP | WINDOW_STYLE(4), // SS_BLACKRECT
            at.x + x,
            at.y + y,
            200,
            100,
            Some(h),
            None,
            None,
            None,
        )
    };
    if let Ok(pop) = pop {
        unsafe {
            let _ = ShowWindow(pop, SW_SHOWNOACTIVATE);
        }
        out += "
";
        out += &ov.selftest_popup(h.0 as isize, pop.0 as isize, x as u32, y as u32);
        unsafe {
            let _ = DestroyWindow(pop);
        }
    }
    // The same drawn with UpdateLayeredWindow, like a drag image or a tooltip: it must stay
    // drawn by itself (no alpha of ours) and go under the 3D view instead.
    if let Some(pop) = ulw_popup(h, at.x + x, at.y + y) {
        out += "
ULW ";
        out += &ov.selftest_popup(h.0 as isize, pop.0 as isize, x as u32, y as u32);
        let own = unsafe {
            let (mut k, mut a, mut f) = (windows::Win32::Foundation::COLORREF(0), 0u8, LAYERED_WINDOW_ATTRIBUTES_FLAGS(0));
            GetLayeredWindowAttributes(pop, Some(&mut k), Some(&mut a), Some(&mut f)).is_err()
        };
        let view = crate::wall::overlay();
        let mut under = view != 0 && view != pop.0 as isize;
        if under {
            under = false;
            let mut n = Some(hwnd_of(view));
            for _ in 0..10_000 {
                n = n.and_then(|w| unsafe { GetWindow(w, GW_HWNDNEXT) }.ok()).filter(|w| !w.is_invalid());
                match n {
                    Some(w) if w == pop => {
                        under = true;
                        break;
                    }
                    Some(_) => {}
                    None => break,
                }
            }
        }
        out += &format!(", drawn by itself {own}, under the view {under} (view {view:#x})");
        unsafe {
            let _ = DestroyWindow(pop);
        }
    }
    // As wide as the primary screen and full of text, like a maximized browser.
    unsafe {
        let mut wr = windows::Win32::Foundation::RECT::default();
        let _ = GetWindowRect(h, &mut wr);
        if let Some(fb) = win::frame_bounds(h) {
            let (sw, sh) = (GetSystemMetrics(SM_CXSCREEN), GetSystemMetrics(SM_CYSCREEN));
            let (dl, dt) = (fb.left - wr.left, fb.top - wr.top);
            let (dr, db) = (wr.right - fb.right, wr.bottom - fb.bottom);
            let (w, h2) = (sw + dl + dr, sh - sh / 18 + dt + db);
            let _ = SetWindowPos(h, None, -dl, -dt, w, h2, SWP_NOZORDER | SWP_NOACTIVATE);
            let text: Vec<u16> = "The quick brown fox jumps over the lazy dog 0123456789. "
                .repeat(400)
                .encode_utf16()
                .chain([0])
                .collect();
            let _ = SetWindowTextW(h, PCWSTR(text.as_ptr()));
        }
    }
    out += "\n";
    out += &ov.selftest_sharp(model, h.0 as isize);
    unsafe {
        let _ = DestroyWindow(h);
    }
    out
}

/// GPU time of a full redraw from each selftest camera view (median of 64 frames).
fn gpu_bench(
    ov: &mut overlay::Overlay,
    model: &mut model::Model,
    shots: &[(usize, [f32; 3], f32, f32)],
) -> String {
    let (w, h) = ov.renderer_mut().size();
    let samples = ov.renderer_mut().samples();
    let mb = ov.renderer_mut().gpu().cache_bytes() >> 20;
    let mut cpu_out = String::new();
    let mut out = format!("gpu bench ({w}x{h}, {samples}x msaa, {mb} MB of card textures):");
    for &(_, pos, yaw, pitch) in shots {
        ov.cam.pos = glam::Vec3::from(pos);
        ov.cam.yaw = yaw;
        ov.cam.pitch = pitch;
        // Untimed frames first so the GPU clocks are up.
        for _ in 0..90 {
            ov.renderer_mut().mark_dirty();
            ov.frame(model, false);
        }
        ov.renderer_mut().gpu_timing = true;
        ov.renderer_mut().gpu_ms.clear();
        let mut cpu = Vec::new();
        for _ in 0..64 {
            ov.renderer_mut().mark_dirty();
            let t = std::time::Instant::now();
            ov.frame(model, false);
            cpu.push(t.elapsed().as_secs_f32() * 1000.0);
        }
        cpu.sort_by(f32::total_cmp);
        cpu_out += &format!(" {:.2}", cpu[cpu.len() / 2]);
        let r = ov.renderer_mut();
        r.gpu_timing = false;
        let mut v = std::mem::take(&mut r.gpu_ms);
        if v.is_empty() {
            out += " n/a";
            continue;
        }
        v.sort_by(f32::total_cmp);
        out += &format!(" {:.2}", v[v.len() / 2]);
    }
    out + " ms; cpu per drawn frame" + &cpu_out + " ms"
}

fn run_selftest(app: &mut App, dir: &std::path::Path) {
    use std::time::Instant;
    let _ = std::fs::create_dir_all(dir);
    // The layout tests use shortcut cards.
    app.cfg.show_shortcuts = true;
    let mut r = selftest::Report(String::new());
    r.mem("idle (before first open)");
    if app.warmup.is_some() {
        let t = Instant::now();
        app.gpu = app.take_gpu();
        r.line(format!("gpu warm-up wait: {} ms", t.elapsed().as_millis()));
        if app.cfg.keep_surface {
            let t = Instant::now();
            app.show_with(true);
            app.hide();
            r.line(format!("pre-park: {} ms", t.elapsed().as_millis()));
        }
        std::thread::sleep(std::time::Duration::from_secs(1));
        r.mem("idle (gpu warm, cards preloaded)");
    }
    let t = Instant::now();
    app.show();
    r.line(format!("open: {} ms", t.elapsed().as_millis()));
    if app.overlay.is_none() {
        r.line("overlay failed to open (see log.txt)");
        let _ = std::fs::write(dir.join("selftest.txt"), &r.0);
        return;
    }
    if let Some(ov) = &mut app.overlay {
        r.line(ov.selftest_scenes(dir));
    }
    // Only the scene pictures, to work on their shaders.
    if std::env::var_os("WINDOW3D_SCENES_ONLY").is_some() {
        app.hide();
        let _ = std::fs::write(dir.join("selftest.txt"), &r.0);
        return;
    }
    let shots: [(usize, [f32; 3], f32, f32); 3] = [
        (60, [0.0, 1.7, 0.0], 0.0, 0.0),
        (120, [0.0, 2.2, 0.0], std::f32::consts::PI, 0.0),
        (180, [0.0, 3.0, 2.0], 0.0, 0.05),
    ];
    let mut frame_times = Vec::new();
    for i in 0..200 {
        selftest::pump();
        while pop().is_some() {}
        let Some(ov) = &mut app.overlay else { break };
        if let Some(&(_, pos, yaw, pitch)) = shots.iter().find(|s| i < s.0) {
            ov.cam.pos = glam::Vec3::from(pos);
            ov.cam.yaw = yaw;
            ov.cam.pitch = pitch;
        }
        // Exercise picking + drag on the card in front of the camera, then put it back.
        if i == 20 {
            let before = app
                .model
                .layout
                .entries
                .iter()
                .map(|e| (e.id, e.pos))
                .collect::<Vec<_>>();
            if matches!(ov.button(true, &mut app.model), Action::None) {
                ov.mouse_delta(40, 0);
                ov.frame(&mut app.model, false);
                let dragged = matches!(ov.button(false, &mut app.model), Action::LayoutChanged);
                let moved = app
                    .model
                    .layout
                    .entries
                    .iter()
                    .zip(&before)
                    .filter(|(e, b)| e.pos != b.1)
                    .count();
                r.line(format!(
                    "drag test: layout-changed={dragged}, cards moved={moved}"
                ));
                for (e, b) in app.model.layout.entries.iter_mut().zip(&before) {
                    e.pos = b.1;
                }
            }
            r.line(ov.selftest_tools(&mut app.model));
            r.line(ov.selftest_theater(&app.model));
            r.line(ov.selftest_slots(&mut app.model));
            r.line(app.model.selftest_wall(&model::scan_shortcuts(&app.cfg)));
            r.line(workflow::selftest(&mut app.model));
            r.line(selftest::frame_test());
            r.line(selftest::popup_kinds_test());
            r.line(win::menu_fade_test());
            r.line(win::shell_scan_report());
            r.line(win::selftest_under_desktop());
            r.line(selftest_window_events());
            r.line(selftest_live(ov, &app.model));
            r.line(ov.selftest_desktop());
            // Touches the user's real browser windows (briefly), so only on request.
            if std::env::var_os("WINDOW3D_OCCLUSION_TEST").is_some() {
                r.line(selftest::occlusion_test());
            }
        }
        let shot = shots.iter().position(|s| s.0 - 1 == i);
        ov.renderer_mut().want_capture = shot.is_some();
        let t = Instant::now();
        ov.frame(&mut app.model, false);
        if shot.is_none() {
            frame_times.push(t.elapsed().as_secs_f32() * 1000.0);
        }
        if let Some(n) = shot {
            if let Some(mut img) = ov.renderer_mut().captured.take() {
                while img.w > 1600 {
                    img = img.half();
                }
                let p = dir.join(format!("selftest_{}.png", n + 1));
                let _ = selftest::write_png(&p, &img);
            }
        }
    }
    if let Some(ov) = &mut app.overlay {
        r.line(gpu_bench(ov, &mut app.model, &shots));
    }
    frame_times.sort_by(f32::total_cmp);
    if !frame_times.is_empty() {
        r.line(format!(
            "frame time (hidden window, no vsync): median {:.2} ms, p95 {:.2} ms",
            frame_times[frame_times.len() / 2],
            frame_times[frame_times.len() * 95 / 100]
        ));
    }
    let all: Vec<&str> = store::STATS_FIELDS.iter().map(|f| f.0).collect();
    let full = statswin::lines_for(&all);
    let only_view = statswin::lines_for(&["view"]);
    let reported = full.iter().any(|l| l.starts_with("Live cards"))
        && full
            .iter()
            .any(|l| l.starts_with("3D view: ") && l.contains("fps"));
    r.line(format!(
        "stats window test: {} (all fields {} lines, view only {:?})",
        reported && only_view.len() == 1 && only_view[0].contains("fps"),
        full.len(),
        only_view
    ));
    // Head-on close-ups of every window card, to verify textures as seen in 3D.
    let targets: Vec<_> = app
        .model
        .layout
        .entries
        .iter()
        .filter(|e| e.kind == Kind::Window)
        .map(|e| (e.id, e.pos, e.yaw))
        .collect();
    for (id, pos, yaw) in targets {
        let Some(ov) = &mut app.overlay else { break };
        let fwd = glam::Vec3::new(yaw.sin(), 0.0, -yaw.cos());
        ov.cam.pos = glam::Vec3::from(pos) - fwd * 2.5;
        ov.cam.yaw = yaw;
        ov.cam.pitch = 0.0;
        for _ in 0..3 {
            selftest::pump();
            while pop().is_some() {}
            ov.frame(&mut app.model, false);
        }
        ov.renderer_mut().want_capture = true;
        ov.frame(&mut app.model, false);
        if let Some(mut img) = ov.renderer_mut().captured.take() {
            while img.w > 800 {
                img = img.half();
            }
            let _ = selftest::write_png(&dir.join(format!("closeup_{id}.png")), &img);
        }
    }
    r.line(format!(
        "cards: {} entries in layout",
        app.model.layout.entries.len()
    ));
    for e in app
        .model
        .layout
        .entries
        .iter()
        .filter(|e| e.kind == Kind::Window)
    {
        let shot = store::load_shot(e.id);
        r.line(format!(
            "window card {}: shot={} exe={} title={}",
            e.id,
            shot.is_some(),
            e.exe,
            e.title
        ));
        let img =
            win::compose_window_card(&e.title, &e.exe, shot.as_ref(), app.model.is_ghost(e.id));
        let _ = selftest::write_png(&dir.join(format!("card_{}.png", e.id)), &img);
    }
    // The options menu as drawn, once its picture thumbnails are made.
    let settings = app.menu_settings();
    if let Some(ov) = &mut app.overlay {
        ov.open_menu(settings, render::adapter_names());
        let t = Instant::now();
        for _ in 0..30 {
            ov.frame(&mut app.model, false);
            std::thread::sleep(std::time::Duration::from_millis(100));
        }
        if let Some(img) = ov.menu_image() {
            let _ = selftest::write_png(&dir.join("menu.png"), &img);
            r.line(format!("options menu: {}x{} drawn ({} ms with thumbnails)", img.w, img.h, t.elapsed().as_millis()));
        }
        // The top of the background grid: presets, wallpaper and the 360° panoramas.
        for _ in 0..40 {
            ov.wheel(120);
        }
        if let Some(img) = ov.menu_image() {
            let _ = selftest::write_png(&dir.join("menu_top.png"), &img);
        }
        let ready = crate::background::PANORAMAS
            .iter()
            .filter(|p| crate::background::pano_state(p) == crate::background::PanoState::Ready)
            .count();
        r.line(format!("options menu: {} panoramas offered, {} downloaded", crate::background::PANORAMAS.len(), ready));
        let a = ov.key(VK_ESCAPE.0, &mut app.model);
        r.line(format!("options menu: Esc closes it={} settings-unchanged={}", !ov.menu_open(), matches!(a, Action::None)));
    }
    r.mem("overlay open");
    let t = Instant::now();
    app.hide();
    r.line(format!("close: {} ms", t.elapsed().as_millis()));
    selftest::pump();
    r.mem("after close");
    for n in 2..=3 {
        let t = Instant::now();
        app.show();
        r.line(format!("open #{n}: {} ms", t.elapsed().as_millis()));
        for _ in 0..90 {
            selftest::pump();
            while pop().is_some() {}
            if let Some(ov) = &mut app.overlay {
                ov.frame(&mut app.model, false);
            }
        }
        r.mem(&format!("open #{n}"));
        app.hide();
        selftest::pump();
        r.mem(&format!("after close #{n}"));
    }
    r.line(selftest_system(app));
    r.line(selftest_search(app));
    r.line(selftest_tidy_windows(app));
    for s in [1, 3, 6] {
        std::thread::sleep(std::time::Duration::from_secs(s));
        selftest::pump();
        r.mem(&format!("{s}s+ later"));
    }
    let _ = std::fs::write(dir.join("selftest.txt"), &r.0);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hotkeys() {
        assert_eq!(
            parse_hotkey("Ctrl+Alt+Space"),
            Some((MOD_NOREPEAT | MOD_CONTROL | MOD_ALT, VK_SPACE.0 as u32))
        );
        assert_eq!(
            parse_hotkey("win+shift+d"),
            Some((MOD_NOREPEAT | MOD_WIN | MOD_SHIFT, 'D' as u32))
        );
        assert_eq!(
            parse_hotkey("Alt+F12"),
            Some((MOD_NOREPEAT | MOD_ALT, VK_F12.0 as u32))
        );
        assert_eq!(parse_hotkey("Ctrl+Banana"), None);
        assert_eq!(parse_hotkey("Ctrl+Alt"), None);
    }
}
