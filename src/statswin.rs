//! A small always-on-top stats window, independent of the 3D view: Window3D's own CPU and
//! memory, the foreground app's, and the switch and input latencies. Drag it anywhere (also to
//! another monitor); its right-click menu picks the lines to show (saved as `stats_fields`) or
//! closes it, as does the tray menu. It only exists while shown, and then wakes twice a second.

use std::cell::RefCell;

use windows::Win32::Foundation::*;
use windows::Win32::Graphics::Gdi::*;
use windows::Win32::UI::WindowsAndMessaging::*;
use windows::core::PCWSTR;

use crate::stats::{self, ProcSample};
use crate::store::STATS_FIELDS;
use crate::win;

const CLASS: &str = "Window3DStats";
const TIMER: usize = 1;
const W: i32 = 340;
const LINE: i32 = 20;
/// Menu ids: `MENU_FIELD + i` toggles `STATS_FIELDS[i]`.
const MENU_CLOSE: usize = 1;
const MENU_FIELD: usize = 100;

#[derive(Default)]
struct State {
    hwnd: Option<isize>,
    own: Option<ProcSample>,
    app: Option<ProcSample>,
    /// Foreground window we last measured (the stats window itself never is one).
    fg: Option<isize>,
    lines: Vec<String>,
    /// The lines to show (keys of `STATS_FIELDS`).
    fields: Vec<String>,
}

thread_local! {
    static STATE: RefCell<State> = RefCell::new(State::default());
}

pub fn is_open() -> bool {
    STATE.with(|s| s.borrow().hwnd.is_some())
}

/// Opens the window at the top right of the primary monitor, or closes it.
pub fn toggle(hinstance: HINSTANCE, fields: &[String]) {
    if let Some(h) = STATE.with(|s| s.borrow().hwnd) {
        unsafe {
            let _ = DestroyWindow(win::hwnd_of(h));
        }
        return;
    }
    let class = win::wide(CLASS);
    unsafe {
        let wc = WNDCLASSEXW {
            cbSize: size_of::<WNDCLASSEXW>() as u32,
            lpfnWndProc: Some(wndproc),
            hInstance: hinstance,
            lpszClassName: PCWSTR(class.as_ptr()),
            hCursor: LoadCursorW(None, IDC_SIZEALL).unwrap_or_default(),
            ..Default::default()
        };
        // Fails harmlessly when already registered by an earlier open.
        RegisterClassExW(&wc);
        let x = GetSystemMetrics(SM_CXSCREEN) - W - 24;
        let Ok(hwnd) = CreateWindowExW(
            WS_EX_TOPMOST | WS_EX_TOOLWINDOW | WS_EX_NOACTIVATE,
            PCWSTR(class.as_ptr()),
            windows::core::w!("Window3D stats"),
            WS_POPUP | WS_BORDER,
            x,
            24,
            W,
            LINE * 2,
            None,
            None,
            Some(hinstance),
            None,
        ) else {
            return;
        };
        STATE.with(|s| {
            let mut s = s.borrow_mut();
            s.hwnd = Some(hwnd.0 as isize);
            s.fields = fields.to_vec();
        });
        refresh();
        let _ = ShowWindow(hwnd, SW_SHOWNOACTIVATE);
        SetTimer(Some(hwnd), TIMER, 500, None);
    }
}

/// Keeps the window above a topmost overlay that was just shown.
pub fn raise() {
    if let Some(h) = STATE.with(|s| s.borrow().hwnd) {
        unsafe {
            let _ = SetWindowPos(
                win::hwnd_of(h),
                Some(HWND_TOPMOST),
                0,
                0,
                0,
                0,
                SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE,
            );
        }
    }
}

/// The lines the window would show with `fields` (self-test; needs no window).
pub fn lines_for(fields: &[&str]) -> Vec<String> {
    STATE.with(|s| s.borrow_mut().fields = fields.iter().map(|f| f.to_string()).collect());
    refresh();
    STATE.with(|s| std::mem::take(&mut *s.borrow_mut()).lines)
}

fn mb(b: u64) -> f64 {
    b as f64 / (1024.0 * 1024.0)
}

fn pct(p: Option<f32>) -> String {
    p.map_or("-".into(), |p| format!("{p:.1}%"))
}

fn ms(v: Option<f32>) -> String {
    v.map_or("-".into(), |v| format!("{v:.1} ms"))
}

fn refresh() {
    STATE.with(|s| {
        let s = &mut *s.borrow_mut();
        s.own = stats::sample(0, s.own.as_ref());
        let fg = unsafe { GetForegroundWindow() };
        let fgi = fg.0 as isize;
        if !fg.0.is_null() && Some(fgi) != s.hwnd {
            s.fg = Some(fgi);
        }
        let on = |f: &str| s.fields.iter().any(|x| x == f);
        let (own, app, switch, input) = (on("own"), on("app"), on("switch"), on("input"));
        let (live, view) = (on("live"), on("view"));
        let mut lines = Vec::new();
        if let Some(o) = s.own.as_ref().filter(|_| own) {
            lines.push(format!(
                "Window3D   CPU {}   RAM {:.1} MB (private {:.1})",
                pct(o.cpu_pct),
                mb(o.working_set),
                mb(o.private)
            ));
        }
        match s.fg.filter(|_| app || switch) {
            Some(h) => {
                let hw = win::hwnd_of(h);
                let pid = win::window_pid(hw);
                let prev = s.app.as_ref().filter(|a| a.pid == pid);
                s.app = stats::sample(pid, prev);
                let mut title = win::window_title(hw);
                if title.chars().count() > 38 {
                    title = title.chars().take(37).collect::<String>() + "\u{2026}";
                }
                lines.push(format!("Foreground: {title}"));
                if app {
                    match &s.app {
                        Some(a) => lines.push(format!(
                            "   CPU {}   RAM {:.1} MB (private {:.1})",
                            pct(a.cpu_pct),
                            mb(a.working_set),
                            mb(a.private)
                        )),
                        None => lines.push("   (no access to this process)".into()),
                    }
                }
                if switch {
                    lines.push(format!(
                        "Switch to it from the 3D view: {}",
                        ms(stats::switch_ms(h))
                    ));
                }
            }
            None => s.app = None,
        }
        if input {
            lines.push(format!(
                "Input passed to a live card: {}",
                ms(stats::input_ms())
            ));
        }
        let v = stats::view();
        if live {
            lines.push(match v {
                Some(v) if v.live > 0 => format!(
                    "Live cards: {}   {:.0} fps   delay {:.1} ms (max {:.1})",
                    v.live, v.fps, v.delay_ms, v.delay_max
                ),
                Some(_) => "Live cards: none".into(),
                None => "Live cards: - (3D view closed)".into(),
            });
        }
        if view {
            lines.push(match v {
                Some(v) => format!(
                    "3D view: {:.0} fps (frame {:.1} ms)",
                    1000.0 / v.frame_ms.max(0.1),
                    v.frame_ms
                ),
                None => "3D view: closed".into(),
            });
        }
        if lines.is_empty() {
            lines.push("Right-click to pick what to show".into());
        }
        // Fit the height to the lines.
        if let Some(h) = s.hwnd {
            let want = 16 + lines.len() as i32 * LINE + 2;
            let mut r = RECT::default();
            unsafe {
                let _ = GetWindowRect(win::hwnd_of(h), &mut r);
                if r.bottom - r.top != want {
                    let _ = SetWindowPos(
                        win::hwnd_of(h),
                        None,
                        0,
                        0,
                        W,
                        want,
                        SWP_NOMOVE | SWP_NOZORDER | SWP_NOACTIVATE,
                    );
                }
            }
        }
        s.lines = lines;
    });
}

/// Right-click menu: a check item per line, and Close.
fn menu(hwnd: HWND) {
    let fields = STATE.with(|s| s.borrow().fields.clone());
    unsafe {
        let Ok(m) = CreatePopupMenu() else { return };
        for (i, (key, label)) in STATS_FIELDS.iter().enumerate() {
            let flags = if fields.iter().any(|f| f == key) {
                MF_STRING | MF_CHECKED
            } else {
                MF_STRING
            };
            let label = win::wide(label);
            let _ = AppendMenuW(m, flags, MENU_FIELD + i, PCWSTR(label.as_ptr()));
        }
        let _ = AppendMenuW(m, MF_SEPARATOR, 0, None);
        let _ = AppendMenuW(m, MF_STRING, MENU_CLOSE, windows::core::w!("Close"));
        let mut pt = POINT::default();
        let _ = GetCursorPos(&mut pt);
        let _ = SetForegroundWindow(hwnd);
        let cmd = TrackPopupMenu(
            m,
            TPM_RETURNCMD | TPM_RIGHTBUTTON,
            pt.x,
            pt.y,
            None,
            hwnd,
            None,
        )
        .0 as usize;
        let _ = DestroyMenu(m);
        match cmd {
            MENU_CLOSE => {
                let _ = DestroyWindow(hwnd);
            }
            n if (MENU_FIELD..MENU_FIELD + STATS_FIELDS.len()).contains(&n) => {
                let key = STATS_FIELDS[n - MENU_FIELD].0;
                let mut fields = fields;
                if fields.iter().any(|f| f == key) {
                    fields.retain(|f| f != key);
                } else {
                    fields.push(key.to_string());
                }
                STATE.with(|s| s.borrow_mut().fields = fields.clone());
                crate::push(crate::Event::StatsFields(fields));
                refresh();
                let _ = InvalidateRect(Some(hwnd), None, false);
            }
            _ => {}
        }
    }
}

extern "system" fn wndproc(hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    unsafe {
        match msg {
            WM_TIMER => {
                refresh();
                let _ = InvalidateRect(Some(hwnd), None, false);
            }
            // Drag anywhere to move it.
            WM_NCHITTEST => return LRESULT(HTCAPTION as isize),
            WM_NCRBUTTONUP => menu(hwnd),
            WM_PAINT => {
                let mut ps = PAINTSTRUCT::default();
                let dc = BeginPaint(hwnd, &mut ps);
                let mut rc = RECT::default();
                let _ = GetClientRect(hwnd, &mut rc);
                let bg = CreateSolidBrush(COLORREF(0x00201a18));
                FillRect(dc, &rc, bg);
                let _ = DeleteObject(bg.into());
                let font = GetStockObject(DEFAULT_GUI_FONT);
                let old = SelectObject(dc, font);
                SetBkMode(dc, TRANSPARENT);
                SetTextColor(dc, COLORREF(0x00f0e8e0));
                let lines = STATE.with(|s| s.borrow().lines.clone());
                for (i, l) in lines.iter().enumerate().filter(|(_, l)| !l.is_empty()) {
                    let mut t: Vec<u16> = l.encode_utf16().collect();
                    let mut r = RECT {
                        left: 10,
                        top: 8 + i as i32 * LINE,
                        right: rc.right - 8,
                        bottom: 8 + (i as i32 + 1) * LINE,
                    };
                    DrawTextW(dc, &mut t, &mut r, DT_SINGLELINE | DT_END_ELLIPSIS);
                }
                SelectObject(dc, old);
                let _ = EndPaint(hwnd, &ps);
            }
            WM_DESTROY => {
                let _ = KillTimer(Some(hwnd), TIMER);
                STATE.with(|s| *s.borrow_mut() = State::default());
            }
            _ => return DefWindowProcW(hwnd, msg, wp, lp),
        }
    }
    LRESULT(0)
}
