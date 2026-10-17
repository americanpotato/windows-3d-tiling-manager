//! `window3d.exe --selftest <dir>`: opens the overlay, flies the camera around, saves screen
//! captures as PNG, closes it again and writes timings and memory figures to `<dir>\selftest.txt`.

use std::fmt::Write as _;
use std::path::Path;

use windows::Win32::System::ProcessStatus::{GetProcessMemoryInfo, PROCESS_MEMORY_COUNTERS};
use windows::Win32::System::Threading::GetCurrentProcess;
use windows::Win32::UI::WindowsAndMessaging::*;

use crate::image::Image;

pub fn memory() -> (usize, usize) {
    let mut c = PROCESS_MEMORY_COUNTERS {
        cb: size_of::<PROCESS_MEMORY_COUNTERS>() as u32,
        ..Default::default()
    };
    unsafe {
        let _ = GetProcessMemoryInfo(GetCurrentProcess(), &mut c, c.cb);
    }
    (c.WorkingSetSize / 1024, c.PagefileUsage / 1024)
}

pub fn pump() {
    let mut msg = MSG::default();
    unsafe {
        while PeekMessageW(&mut msg, None, 0, 0, PM_REMOVE).as_bool() {
            let _ = TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
    }
}

pub fn write_png(path: &Path, img: &Image) -> std::io::Result<()> {
    fn crc32(data: &[u8]) -> u32 {
        let mut c = 0xffff_ffffu32;
        for &b in data {
            c ^= b as u32;
            for _ in 0..8 {
                c = if c & 1 != 0 {
                    0xedb8_8320 ^ (c >> 1)
                } else {
                    c >> 1
                };
            }
        }
        !c
    }
    fn chunk(out: &mut Vec<u8>, kind: &[u8], data: &[u8]) {
        out.extend_from_slice(&(data.len() as u32).to_be_bytes());
        let start = out.len();
        out.extend_from_slice(kind);
        out.extend_from_slice(data);
        let crc = crc32(&out[start..]);
        out.extend_from_slice(&crc.to_be_bytes());
    }
    // Raw scanlines (filter 0, RGB), wrapped in stored (uncompressed) deflate blocks.
    let mut raw = Vec::with_capacity((img.w * 3 + 1) as usize * img.h as usize);
    for row in img.px.chunks_exact(img.w as usize * 4) {
        raw.push(0);
        for p in row.chunks_exact(4) {
            raw.extend_from_slice(&[p[2], p[1], p[0]]);
        }
    }
    let mut z = vec![0x78, 0x01];
    let blocks: Vec<&[u8]> = raw.chunks(65535).collect();
    for (i, b) in blocks.iter().enumerate() {
        z.push((i == blocks.len() - 1) as u8);
        z.extend_from_slice(&(b.len() as u16).to_le_bytes());
        z.extend_from_slice(&(!(b.len() as u16)).to_le_bytes());
        z.extend_from_slice(b);
    }
    let (mut a, mut bb) = (1u32, 0u32);
    for &x in &raw {
        a = (a + x as u32) % 65521;
        bb = (bb + a) % 65521;
    }
    z.extend_from_slice(&((bb << 16) | a).to_be_bytes());

    let mut out = b"\x89PNG\r\n\x1a\n".to_vec();
    let mut ihdr = Vec::new();
    ihdr.extend_from_slice(&img.w.to_be_bytes());
    ihdr.extend_from_slice(&img.h.to_be_bytes());
    ihdr.extend_from_slice(&[8, 2, 0, 0, 0]);
    chunk(&mut out, b"IHDR", &ihdr);
    chunk(&mut out, b"IDAT", &z);
    chunk(&mut out, b"IEND", &[]);
    std::fs::write(path, out)
}

pub struct Report(pub String);

impl Report {
    pub fn line(&mut self, s: impl AsRef<str>) {
        let _ = writeln!(self.0, "{}", s.as_ref());
    }
    pub fn mem(&mut self, label: &str) {
        let (ws, private) = memory();
        self.line(format!(
            "{label}: working set {} KB, private {} KB",
            ws, private
        ));
    }
}

/// Uncovers covered Chromium windows (if any are open) for a moment and gives the regions back,
/// reporting their titles (a test page can put `document.visibilityState` there).
pub fn occlusion_test() -> String {
    use crate::win;
    let covered: Vec<_> = win::enumerate_windows()
        .into_iter()
        .filter(|w| win::chromium_occluded(w.hwnd))
        .collect();
    let titles = || {
        covered
            .iter()
            .map(|w| win::window_title(windows::Win32::Foundation::HWND(w.hwnd as _)))
            .collect::<Vec<_>>()
    };
    let before = titles();
    let mut shaped = Vec::new();
    for w in &covered {
        shaped.extend(win::uncover_chromium(w.hwnd));
    }
    let wait = || {
        let t = std::time::Instant::now();
        while t.elapsed().as_millis() < 1500 {
            pump();
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
    };
    wait();
    let after = titles();
    let still = covered.iter().filter(|w| win::chromium_occluded(w.hwnd)).count();
    for w in covered.iter().filter(|w| win::chromium_occluded(w.hwnd)) {
        crate::store::log(&format!("still covered {:?}: {:?}", w.title, win::chromium_covers(w.hwnd)));
    }
    win::unshape(&shaped);
    win::unshape_wait();
    wait();
    format!(
        "occlusion test: covered={} shaped={} still-covered={still} before={before:?} uncovered={after:?} restored={:?}",
        covered.len(),
        shaped.len(),
        titles()
    )
}

/// Makes a hidden test window (never a real one) borderless and restores it again.
pub fn frame_test() -> String {
    use windows::Win32::Foundation::RECT;
    use windows::core::w;
    unsafe {
        let Ok(h) = CreateWindowExW(
            WINDOW_EX_STYLE(0),
            w!("STATIC"),
            w!("window3d frame test"),
            WS_OVERLAPPEDWINDOW,
            100,
            100,
            640,
            480,
            None,
            None,
            None,
            None,
        ) else {
            return "frame test: could not create the test window".into();
        };
        let rect = |h| {
            let mut r = RECT::default();
            let _ = GetWindowRect(h, &mut r);
            (r.left, r.top, r.right, r.bottom)
        };
        let style0 = GetWindowLongPtrW(h, GWL_STYLE);
        let rect0 = rect(h);
        let saved = crate::win::make_borderless(h, "Static");
        pump();
        let cap = GetWindowLongPtrW(h, GWL_STYLE) & WS_CAPTION.0 as isize != 0;
        let covers = crate::win::covers_monitor(h);
        let mut restored = false;
        if let Some(f) = &saved {
            crate::win::restore_frame(h, f);
            pump();
            // Restoring shows the window; the hidden test window was never visible.
            let vis = WS_VISIBLE.0 as isize;
            restored = GetWindowLongPtrW(h, GWL_STYLE) & !vis == style0 & !vis && rect(h) == rect0;
            if !restored {
                crate::store::log(&format!(
                    "frame test: style {:x} -> {:x}, rect {rect0:?} -> {:?}",
                    style0,
                    GetWindowLongPtrW(h, GWL_STYLE),
                    rect(h)
                ));
            }
        }
        let _ = DestroyWindow(h);
        format!(
            "frame test: borderless={} caption-removed={} covers-monitor={covers} restored={restored}",
            saved.is_some(),
            !cap
        )
    }
}

/// Off-screen test windows (never real ones) standing for a window used in place and what it
/// opens: an owned dialog, a sign-in window opened from a page (captioned, unowned, small,
/// over it), a captionless popup, a second main window as large as it, and a dialog partly
/// off it. Reports which count as part of the window, and whether the dialog was moved over it.
pub fn popup_kinds_test() -> String {
    use crate::win;
    use windows::Win32::Foundation::HWND;
    use windows::core::w;
    const X: i32 = -24000;
    const Y: i32 = -24000;
    unsafe {
        let make = |ex: WINDOW_EX_STYLE, style: WINDOW_STYLE, x: i32, y: i32, cx: i32, cy: i32, owner: Option<HWND>| {
            CreateWindowExW(ex | WS_EX_NOACTIVATE, w!("STATIC"), w!("window3d popup kinds test"), style, X + x, Y + y, cx, cy, owner, None, None, None)
                .ok()
                .inspect(|&h| {
                    let _ = ShowWindow(h, SW_SHOWNOACTIVATE);
                })
        };
        let Some(main) = make(WINDOW_EX_STYLE(0), WS_OVERLAPPEDWINDOW, 0, 0, 1200, 900, None) else {
            return "popup kinds test: could not create the test window".into();
        };
        let dialog = make(WINDOW_EX_STYLE(0), WS_OVERLAPPED | WS_CAPTION | WS_SYSMENU, 300, 200, 500, 400, Some(main));
        let signin = make(WINDOW_EX_STYLE(0), WS_OVERLAPPEDWINDOW, 350, 150, 500, 600, None);
        let bubble = make(WINDOW_EX_STYLE(0), WS_POPUP, 900, 40, 280, 300, None);
        let second = make(WINDOW_EX_STYLE(0), WS_OVERLAPPEDWINDOW, 30, 30, 1200, 900, None);
        let off = make(WINDOW_EX_STYLE(0), WS_OVERLAPPED | WS_CAPTION | WS_SYSMENU, 1000, 700, 500, 400, Some(main));
        pump();
        let top = main.0 as isize;
        let part = |h: Option<HWND>| h.is_some_and(|h| win::part_of(h, top));
        let adopt = |h: Option<HWND>| h.is_some_and(|h| win::adopt_popup(h, top));
        let (dialog_part, bubble_part, second_part) = (part(dialog), part(bubble), part(second));
        // A sign-in window counts only right after a click on the window.
        let signin_unclicked = adopt(signin);
        crate::wall::selftest_press();
        let signin_clicked = adopt(signin) && part(signin);
        let second_clicked = adopt(second);
        let moved = off.is_some_and(|h| win::fit_dialog(h, top) && !win::shows_off(h, main));
        for h in [dialog, signin, bubble, second, off].into_iter().flatten() {
            let _ = DestroyWindow(h);
        }
        let _ = DestroyWindow(main);
        format!(
            "popup kinds test: dialog {dialog_part}, bubble {bubble_part}, second window {second_part} (after a click {second_clicked}), sign-in window {signin_unclicked} (after a click {signin_clicked}), dialog off the window moved over it {moved}"
        )
    }
}
