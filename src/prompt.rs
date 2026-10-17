//! A tiny modal text prompt (Windows has no stock input box): a label, an edit field and
//! OK / Cancel. Enter accepts, Esc cancels.

use std::cell::RefCell;

use windows::Win32::Foundation::*;
use windows::Win32::Graphics::Gdi::*;
use windows::Win32::UI::Input::KeyboardAndMouse::{EnableWindow, SetFocus};
use windows::Win32::UI::WindowsAndMessaging::*;
use windows::core::{PCWSTR, w};

use crate::win;

const CLASS: &str = "Window3DPrompt";
const ID_EDIT: i32 = 100;

#[derive(Default)]
struct State {
    edit: isize,
    done: bool,
    result: Option<String>,
}

thread_local! {
    static STATE: RefCell<State> = RefCell::new(State::default());
}

/// Asks for a line of text. `None` when cancelled.
pub fn ask(hinstance: HINSTANCE, title: &str, label: &str, initial: &str) -> Option<String> {
    let class = win::wide(CLASS);
    let (title, label, initial) = (win::wide(title), win::wide(label), win::wide(initial));
    STATE.with(|s| *s.borrow_mut() = State::default());
    unsafe {
        let wc = WNDCLASSEXW {
            cbSize: size_of::<WNDCLASSEXW>() as u32,
            lpfnWndProc: Some(wndproc),
            hInstance: hinstance,
            lpszClassName: PCWSTR(class.as_ptr()),
            hCursor: LoadCursorW(None, IDC_ARROW).unwrap_or_default(),
            hbrBackground: HBRUSH((COLOR_BTNFACE.0 + 1) as isize as *mut _),
            ..Default::default()
        };
        RegisterClassExW(&wc);
        let (w, h) = (440, 170);
        let x = (GetSystemMetrics(SM_CXSCREEN) - w) / 2;
        let y = (GetSystemMetrics(SM_CYSCREEN) - h) / 3;
        let Ok(hwnd) = CreateWindowExW(
            WS_EX_TOPMOST | WS_EX_DLGMODALFRAME,
            PCWSTR(class.as_ptr()),
            PCWSTR(title.as_ptr()),
            WS_POPUP | WS_CAPTION | WS_SYSMENU,
            x,
            y,
            w,
            h,
            None,
            None,
            Some(hinstance),
            None,
        ) else {
            return None;
        };
        let font = WPARAM(GetStockObject(DEFAULT_GUI_FONT).0 as usize);
        let child = |cls: PCWSTR, text: PCWSTR, style: WINDOW_STYLE, ex, x, y, w, h, id: i32| {
            let c = CreateWindowExW(
                ex,
                cls,
                text,
                WS_CHILD | WS_VISIBLE | style,
                x,
                y,
                w,
                h,
                Some(hwnd),
                Some(HMENU(id as isize as *mut _)),
                Some(hinstance),
                None,
            )
            .unwrap_or_default();
            SendMessageW(c, WM_SETFONT, Some(font), Some(LPARAM(1)));
            c
        };
        let none = WINDOW_EX_STYLE::default();
        child(
            w!("STATIC"),
            PCWSTR(label.as_ptr()),
            WINDOW_STYLE(0),
            none,
            14,
            12,
            400,
            36,
            -1,
        );
        let edit = child(
            w!("EDIT"),
            PCWSTR(initial.as_ptr()),
            WS_TABSTOP | WINDOW_STYLE(ES_AUTOHSCROLL as u32),
            WS_EX_CLIENTEDGE,
            14,
            52,
            400,
            24,
            ID_EDIT,
        );
        child(
            w!("BUTTON"),
            w!("OK"),
            WS_TABSTOP | WINDOW_STYLE(BS_DEFPUSHBUTTON as u32),
            none,
            234,
            88,
            86,
            28,
            IDOK.0,
        );
        child(
            w!("BUTTON"),
            w!("Cancel"),
            WS_TABSTOP,
            none,
            328,
            88,
            86,
            28,
            IDCANCEL.0,
        );
        STATE.with(|s| s.borrow_mut().edit = edit.0 as isize);
        let _ = ShowWindow(hwnd, SW_SHOW);
        let _ = SetForegroundWindow(hwnd);
        let _ = SetFocus(Some(edit));
        SendMessageW(
            edit,
            0x00B1, /* EM_SETSEL */
            Some(WPARAM(0)),
            Some(LPARAM(-1)),
        );
        let _ = EnableWindow(hwnd, true);
        let mut msg = MSG::default();
        while !STATE.with(|s| s.borrow().done) && GetMessageW(&mut msg, None, 0, 0).as_bool() {
            if !IsDialogMessageW(hwnd, &msg).as_bool() {
                let _ = TranslateMessage(&msg);
                DispatchMessageW(&msg);
            }
        }
        let _ = DestroyWindow(hwnd);
    }
    STATE.with(|s| s.borrow_mut().result.take())
}

extern "system" fn wndproc(hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    unsafe {
        match msg {
            WM_COMMAND => {
                let id = (wp.0 & 0xffff) as i32;
                if id == IDOK.0 || id == IDCANCEL.0 {
                    STATE.with(|s| {
                        let mut s = s.borrow_mut();
                        if id == IDOK.0 {
                            let edit = win::hwnd_of(s.edit);
                            let mut buf = [0u16; 512];
                            let n = GetWindowTextW(edit, &mut buf).max(0) as usize;
                            s.result = Some(String::from_utf16_lossy(&buf[..n]));
                        }
                        s.done = true;
                    });
                }
                LRESULT(0)
            }
            WM_CLOSE => {
                STATE.with(|s| s.borrow_mut().done = true);
                LRESULT(0)
            }
            _ => DefWindowProcW(hwnd, msg, wp, lp),
        }
    }
}

/// Yes / No question, on top of everything.
pub fn confirm(title: &str, text: &str) -> bool {
    let (t, x) = (win::wide(title), win::wide(text));
    unsafe {
        MessageBoxW(
            None,
            PCWSTR(x.as_ptr()),
            PCWSTR(t.as_ptr()),
            MB_YESNO | MB_ICONQUESTION | MB_TOPMOST | MB_SETFOREGROUND,
        ) == IDYES
    }
}
