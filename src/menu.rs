//! The options menu (Esc while flying): a settings panel drawn on the CPU, used with the mouse
//! (the cursor shows over it) or the keyboard.

use std::path::PathBuf;
use std::sync::mpsc::Receiver;

use windows::Win32::Foundation::RECT;
use windows::Win32::Graphics::Gdi::{
    DRAW_TEXT_FORMAT, DT_CENTER, DT_END_ELLIPSIS, DT_LEFT, DT_RIGHT, DT_SINGLELINE, DT_VCENTER,
};

use crate::image::Image;
use crate::win::Dib;

/// Settings the options menu (Esc) changes.
#[derive(Clone, PartialEq, Debug)]
pub struct MenuSettings {
    /// As `gpu` in the config: a keyword or a graphics card name.
    pub gpu: String,
    pub msaa: bool,
    pub fov_degrees: f32,
    pub mouse_sensitivity: f32,
    pub move_speed: f32,
    pub card_titles: bool,
    pub show_ghosts: bool,
    pub show_hints: bool,
    /// As `max_fps` and `live_fps` in the config (0: no cap).
    pub max_fps: u32,
    pub live_fps: u32,
    /// As `background` in the config.
    pub background: String,
}

/// Backgrounds offered first: config value and label.
const PRESETS: [(&str, &str); 7] = [
    ("room", "Grid room"),
    ("#05070d", "Night"),
    ("#000000", "Black"),
    ("#141a2e", "Navy"),
    ("#2b2f36", "Slate"),
    ("#3a332c", "Warm gray"),
    ("#c9ced6", "Light"),
];

/// Rows: the settings, the background grid, Done and Close the 3D view.
pub const ROWS: usize = 13;
const SETTINGS: usize = 10;
const BG_ROW: usize = 10;
pub const DONE_ROW: usize = 11;
pub const CLOSE_ROW: usize = 12;
/// Frame rate caps offered (0: none).
const MAX_FPS: [u32; 8] = [30, 60, 90, 120, 144, 165, 240, 0];
const LIVE_FPS: [u32; 7] = [5, 10, 15, 20, 30, 60, 0];

/// The next (or previous) of `steps` after `v` (a value not offered goes to the nearest).
fn cycle(steps: &[u32], v: u32, up: bool) -> u32 {
    let key = |x: u32| if x == 0 { u32::MAX } else { x };
    let n = steps.len();
    let i = steps
        .iter()
        .position(|&x| x == v)
        .unwrap_or_else(|| steps.iter().position(|&x| key(x) > key(v)).unwrap_or(n - 1));
    steps[if up { (i + 1) % n } else { (i + n - 1) % n }]
}

fn fps_label(v: u32, none: &str) -> String {
    if v == 0 { none.to_string() } else { format!("{v} fps") }
}
const COLS: usize = 5;
/// Tile rows shown at once (the wheel scrolls the rest).
const VIS_ROWS: usize = 2;

const PANEL: (u8, u8, u8) = (24, 27, 34);
const EDGE: (u8, u8, u8) = (70, 78, 96);
const TEXT: (u8, u8, u8) = (232, 234, 240);
const DIM: (u8, u8, u8) = (150, 156, 170);
const ROW_HOT: (u8, u8, u8) = (36, 41, 52);
const BOX: (u8, u8, u8) = (44, 50, 64);
const BOX_HOT: (u8, u8, u8) = (62, 70, 90);
const ACCENT: (u8, u8, u8) = (80, 150, 255);

/// What a background tile shows.
enum Preview {
    Room,
    Color((u8, u8, u8)),
    Picture(PathBuf),
    /// A 360° panorama, downloaded when picked.
    Pano(&'static crate::background::Pano),
    /// An animated scene drawn by a shader.
    Scene(&'static crate::background::Scene),
}

struct Tile {
    value: String,
    label: String,
    preview: Preview,
}

/// Something in the panel the mouse can be on.
#[derive(Clone, Copy, PartialEq, Debug)]
pub enum Target {
    /// A setting row; `true` on its back arrow.
    Row(usize, bool),
    Tile(usize),
    Done,
    Close,
}

/// What a click asks of the overlay.
#[derive(PartialEq, Debug)]
pub enum Click {
    Nothing,
    Changed,
    Done,
    Close,
}

/// The options menu: the row picked, the settings as opened and as changed, the renderer
/// choices, the background tiles and the last drawn panel.
pub struct Menu {
    pub sel: usize,
    pub start: MenuSettings,
    pub now: MenuSettings,
    gpus: Vec<String>,
    tiles: Vec<Tile>,
    /// First tile row shown.
    scroll: usize,
    hover: Option<Target>,
    /// Size factor (1 at 17 px text).
    k: f32,
    tile: (u32, u32),
    thumbs: Option<Receiver<PathBuf>>,
    /// The panel must be drawn again.
    pub dirty: bool,
    hits: Vec<(RECT, Target)>,
}

fn hex(s: &str) -> Option<(u8, u8, u8)> {
    let h = s.strip_prefix('#')?;
    if h.len() != 6 {
        return None;
    }
    let v = u32::from_str_radix(h, 16).ok()?;
    Some(((v >> 16) as u8, (v >> 8) as u8, v as u8))
}

fn stem(p: &std::path::Path) -> String {
    p.file_stem()
        .map_or_else(|| p.to_string_lossy().into_owned(), |s| s.to_string_lossy().into_owned())
}

fn preview_of(value: &str) -> Preview {
    use crate::background::Kind;
    match crate::background::parse(value) {
        Kind::Room => Preview::Room,
        Kind::Plain(_) => Preview::Color(
            hex(value).or_else(|| hex(&crate::background::desktop_color())).unwrap_or((0, 0, 0)),
        ),
        Kind::Picture(p) => Preview::Picture(p),
        Kind::Scene(x) => Preview::Scene(x),
    }
}

/// The thumbnail cache key of a picture tile.
fn thumb_path(p: &Preview) -> PathBuf {
    match p {
        Preview::Picture(p) => p.clone(),
        Preview::Pano(p) => crate::background::pano_thumb(p),
        Preview::Scene(x) => crate::background::scene_thumb(x),
        _ => PathBuf::new(),
    }
}

/// Darkens `r` of a `w`-wide panel, keeping 1/2^`bits` of it.
fn shade(dib: &mut Dib, w: i32, r: RECT, bits: u32) {
    let px = dib.pixels();
    for y in r.top.max(0)..r.bottom {
        let row = y as usize * w as usize * 4;
        for x in r.left.max(0)..r.right.min(w) {
            let i = row + x as usize * 4;
            for c in &mut px[i..i + 3] {
                *c >>= bits;
            }
        }
    }
}

fn gpu_label(gpu: &str) -> String {
    match gpu {
        "default" => "Automatic (Windows picks)".into(),
        "low-power" => "Power-saving GPU (integrated)".into(),
        "high-performance" => "High-performance GPU (dedicated)".into(),
        "software" => "Software renderer (WARP, slow)".into(),
        name => name.to_string(),
    }
}

fn rect(x: i32, y: i32, w: i32, h: i32) -> RECT {
    RECT {
        left: x,
        top: y,
        right: x + w,
        bottom: y + h,
    }
}

fn inside(r: &RECT, x: i32, y: i32) -> bool {
    x >= r.left && x < r.right && y >= r.top && y < r.bottom
}

impl Menu {
    /// A menu on `settings` with renderer choices `gpus`, sized for `text_px` text on a
    /// `screen`-sized overlay.
    pub fn new(settings: MenuSettings, gpus: Vec<String>, text_px: i32, screen: (u32, u32)) -> Menu {
        // The panel is about 940 x 890 at k = 1; it must fit the screen.
        let k = (text_px as f32 / 17.0)
            .min(screen.0 as f32 * 0.95 / 940.0)
            .min(screen.1 as f32 * 0.94 / 890.0)
            .max(0.5);
        let mut tiles: Vec<Tile> = PRESETS
            .iter()
            .map(|(v, l)| Tile {
                value: v.to_string(),
                label: l.to_string(),
                preview: preview_of(v),
            })
            .collect();
        for x in crate::background::SCENES {
            tiles.push(Tile {
                value: crate::background::scene_value(x),
                label: x.name.into(),
                preview: Preview::Scene(x),
            });
        }
        for p in crate::background::PANORAMAS {
            tiles.push(Tile {
                value: crate::background::pano_file(p),
                label: p.name.into(),
                preview: Preview::Pano(p),
            });
        }
        for name in crate::background::pictures() {
            let path = crate::background::folder().join(&name);
            // Only 360° ones: flat pictures (and the wallpaper) can still be set in the config.
            if crate::background::pano_of(&path).is_some() || !crate::background::is_panorama(&path) {
                continue;
            }
            tiles.push(Tile {
                value: name,
                label: stem(&path),
                preview: Preview::Picture(path),
            });
        }
        let (pad, gap) = (32.0 * k, 14.0 * k);
        let w = (940.0 * k) as i32;
        let tw = ((w as f32 - 2.0 * pad - (COLS - 1) as f32 * gap) / COLS as f32) as u32;
        let tile = (tw, tw * 9 / 16);
        let paths: Vec<PathBuf> = tiles
            .iter()
            .filter_map(|t| match &t.preview {
                Preview::Picture(p) => Some(p.clone()),
                Preview::Pano(p) => Some(crate::background::pano_thumb(p)),
                Preview::Scene(x) => Some(crate::background::scene_thumb(x)),
                _ => None,
            })
            .collect();
        let thumbs = (!paths.is_empty()).then(|| crate::background::make_thumbs(paths, tile.0, tile.1));
        let mut m = Menu {
            sel: 0,
            start: settings.clone(),
            now: settings,
            gpus,
            tiles,
            scroll: 0,
            hover: None,
            k,
            tile,
            thumbs,
            dirty: true,
            hits: Vec::new(),
        };
        m.show_current();
        m
    }

    fn current_tile(&self) -> Option<usize> {
        self.tiles
            .iter()
            .position(|t| t.value.eq_ignore_ascii_case(&self.now.background))
    }

    /// Scrolls the grid so the current background's tile shows.
    fn show_current(&mut self) {
        if let Some(i) = self.current_tile() {
            let r = i / COLS;
            if r < self.scroll {
                self.scroll = r;
            } else if r >= self.scroll + VIS_ROWS {
                self.scroll = r + 1 - VIS_ROWS;
            }
        }
    }

    fn tile_rows(&self) -> usize {
        self.tiles.len().div_ceil(COLS)
    }

    /// Picks up finished thumbnails; true when the panel needs drawing.
    pub fn poll(&mut self) -> bool {
        if let Some(rx) = &self.thumbs {
            loop {
                match rx.try_recv() {
                    Ok(_) => self.dirty = true,
                    Err(std::sync::mpsc::TryRecvError::Empty) => break,
                    Err(_) => {
                        self.thumbs = None;
                        break;
                    }
                }
            }
        }
        self.dirty
    }

    /// Wheel notches over the menu: scrolls the background grid.
    pub fn wheel(&mut self, delta: i32) {
        let max = self.tile_rows().saturating_sub(VIS_ROWS);
        let s = if delta > 0 {
            self.scroll.saturating_sub(1)
        } else {
            (self.scroll + 1).min(max)
        };
        if s != self.scroll {
            self.scroll = s;
            self.dirty = true;
        }
    }

    /// Up (`true`) or down a row.
    pub fn move_sel(&mut self, up: bool) {
        self.sel = if up { (self.sel + ROWS - 1) % ROWS } else { (self.sel + 1) % ROWS };
        self.dirty = true;
    }

    /// Changes the picked row by one step (`up`: forward).
    pub fn step(&mut self, up: bool) {
        self.dirty = true;
        let s = &mut self.now;
        let d = if up { 1.0 } else { -1.0 };
        match self.sel {
            0 => {
                let n = self.gpus.len().max(1);
                let i = self.gpus.iter().position(|g| *g == s.gpu).unwrap_or(0);
                let j = if up { (i + 1) % n } else { (i + n - 1) % n };
                if let Some(g) = self.gpus.get(j) {
                    s.gpu = g.clone();
                }
            }
            1 => s.msaa = !s.msaa,
            2 => s.fov_degrees = (s.fov_degrees + 5.0 * d).clamp(40.0, 120.0),
            3 => {
                let v = (s.mouse_sensitivity / 0.0022 + 0.1 * d).clamp(0.1, 5.0);
                s.mouse_sensitivity = (v * 10.0).round() / 10.0 * 0.0022;
            }
            4 => s.move_speed = (s.move_speed + d).clamp(1.0, 30.0),
            5 => s.card_titles = !s.card_titles,
            6 => s.show_ghosts = !s.show_ghosts,
            7 => s.show_hints = !s.show_hints,
            8 => s.max_fps = cycle(&MAX_FPS, s.max_fps, up),
            9 => s.live_fps = cycle(&LIVE_FPS, s.live_fps, up),
            _ => {}
        }
        if self.sel == BG_ROW {
            let n = self.tiles.len();
            let i = self.current_tile().unwrap_or(0);
            let j = if up { (i + 1) % n } else { (i + n - 1) % n };
            self.now.background = self.tiles[j].value.clone();
            self.show_current();
        }
    }

    /// The mouse at `(x, y)` in panel pixels, clicked or not.
    pub fn pointer(&mut self, x: i32, y: i32, click: bool) -> Click {
        let t = self.hits.iter().find(|(r, _)| inside(r, x, y)).map(|h| h.1);
        if t != self.hover {
            self.hover = t;
            self.dirty = true;
        }
        if !click {
            return Click::Nothing;
        }
        match t {
            Some(Target::Row(i, back)) => {
                self.sel = i;
                self.step(!back);
                Click::Changed
            }
            Some(Target::Tile(j)) => {
                self.sel = BG_ROW;
                if let Some(t) = self.tiles.get(j) {
                    self.now.background = t.value.clone();
                }
                self.dirty = true;
                Click::Changed
            }
            Some(Target::Done) => Click::Done,
            Some(Target::Close) => Click::Close,
            None => Click::Nothing,
        }
    }

    fn row(&self, i: usize) -> (&'static str, String, bool) {
        let on = |b: bool| if b { "On" } else { "Off" }.to_string();
        let s = &self.now;
        match i {
            0 => ("Renderer", gpu_label(&s.gpu), false),
            1 => ("4x MSAA (smoother edges, slower)", on(s.msaa), true),
            2 => ("Field of view", format!("{:.0}\u{00b0}", s.fov_degrees), false),
            3 => ("Mouse sensitivity", format!("{:.1}", s.mouse_sensitivity / 0.0022), false),
            4 => ("Move speed", format!("{:.0}", s.move_speed), false),
            5 => ("Card titles and icons", on(s.card_titles), true),
            6 => ("Cards of closed apps", on(s.show_ghosts), true),
            7 => ("Hints and tooltips", on(s.show_hints), true),
            8 => ("Frame rate cap", fps_label(s.max_fps, "None"), false),
            _ => ("Live cards not in use", fps_label(s.live_fps, "Every frame"), false),
        }
    }

    /// Draws the panel (and records where its parts are for `pointer`).
    pub fn compose(&mut self) -> Image {
        self.dirty = false;
        let k = self.k;
        let u = |v: f32| (v * k).round() as i32;
        let (w, pad, gap) = (u(940.0), u(32.0), u(14.0));
        let (tw, th) = (self.tile.0 as i32, self.tile.1 as i32);
        let lab = u(26.0);
        let rows_h = VIS_ROWS as i32 * (th + lab + gap);
        let h = pad + u(52.0) + SETTINGS as i32 * u(42.0) + u(14.0) + u(34.0) + rows_h + u(30.0) + u(44.0) + pad;
        let Some(mut dib) = Dib::new(w, h) else {
            return Image::new(4, 4);
        };
        let mut hits = Vec::new();
        let left = DT_LEFT | DT_SINGLELINE | DT_VCENTER | DT_END_ELLIPSIS;
        let center = DT_CENTER | DT_SINGLELINE | DT_VCENTER | DT_END_ELLIPSIS;
        let right = DT_RIGHT | DT_SINGLELINE | DT_VCENTER;
        let text = |d: &Dib, s: &str, r: RECT, px: f32, bold: bool, c: (u8, u8, u8), f: DRAW_TEXT_FORMAT| {
            d.text(s, r, u(px), bold, c, f)
        };
        dib.fill(0, 0, w, h, EDGE);
        dib.fill(1, 1, w - 2, h - 2, PANEL);

        let mut y = pad;
        text(&dib, "Options", rect(pad, y, w / 2, u(40.0)), 28.0, true, TEXT, left);
        text(
            &dib,
            "Esc: done  \u{00b7}  Esc Esc: close the 3D view",
            rect(w / 2, y, w / 2 - pad, u(40.0)),
            14.0,
            false,
            DIM,
            right,
        );
        y += u(52.0);

        let bw = u(400.0);
        for i in 0..SETTINGS {
            let r = rect(pad - u(10.0), y, w - 2 * pad + u(20.0), u(40.0));
            let hot = matches!(self.hover, Some(Target::Row(j, _)) if j == i);
            if hot || self.sel == i {
                dib.fill(r.left, r.top, r.right - r.left, r.bottom - r.top, ROW_HOT);
            }
            if self.sel == i {
                dib.fill(r.left, r.top, u(3.0), r.bottom - r.top, ACCENT);
            }
            let (label, value, toggle) = self.row(i);
            text(&dib, label, rect(pad + u(6.0), y, w - 2 * pad - bw, u(40.0)), 17.0, false, TEXT, left);
            let b = rect(w - pad - bw, y + u(5.0), bw, u(30.0));
            let back = rect(b.left, b.top, bw / 3, b.bottom - b.top);
            let bc = if hot { BOX_HOT } else { BOX };
            dib.fill(b.left, b.top, bw, b.bottom - b.top, bc);
            if toggle {
                let c = if value == "On" { ACCENT } else { DIM };
                text(&dib, &value, b, 16.0, true, c, center);
            } else {
                if self.hover == Some(Target::Row(i, true)) {
                    dib.fill(back.left, back.top, u(36.0), back.bottom - back.top, (80, 90, 112));
                }
                if self.hover == Some(Target::Row(i, false)) {
                    dib.fill(b.right - u(36.0), b.top, u(36.0), b.bottom - b.top, (80, 90, 112));
                }
                text(&dib, "\u{25c0}", rect(b.left, b.top, u(36.0), b.bottom - b.top), 13.0, false, DIM, center);
                text(&dib, "\u{25b6}", rect(b.right - u(36.0), b.top, u(36.0), b.bottom - b.top), 13.0, false, DIM, center);
                text(&dib, &value, rect(b.left + u(40.0), b.top, bw - u(80.0), b.bottom - b.top), 16.0, false, TEXT, center);
                hits.push((back, Target::Row(i, true)));
            }
            hits.push((r, Target::Row(i, false)));
            y += u(42.0);
        }
        y += u(14.0);

        // Background tiles.
        let cur = self.current_tile();
        let head = rect(pad - u(10.0), y, w - 2 * pad + u(20.0), u(30.0));
        if self.sel == BG_ROW {
            dib.fill(head.left, head.top, u(3.0), head.bottom - head.top, ACCENT);
        }
        text(&dib, "Background", rect(pad + u(6.0), y, w / 2, u(30.0)), 17.0, true, TEXT, left);
        let more = self.tile_rows() > VIS_ROWS;
        let note = format!(
            "{}Your pictures: tray menu \u{2192} Open backgrounds folder",
            if more { "Scroll for more  \u{00b7}  " } else { "" }
        );
        text(&dib, &note, rect(w / 2, y, w / 2 - pad, u(30.0)), 13.0, false, DIM, right);
        y += u(34.0);
        let grid_top = y;
        for r in 0..VIS_ROWS {
            for c in 0..COLS {
                let i = (self.scroll + r) * COLS + c;
                let Some(t) = self.tiles.get(i) else { break };
                let x0 = pad + c as i32 * (tw + gap);
                let y0 = grid_top + r as i32 * (th + lab + gap);
                match &t.preview {
                    Preview::Room => {
                        dib.fill(x0, y0, tw, th, (18, 22, 32));
                        let hz = y0 + th * 2 / 5;
                        dib.fill(x0, hz, tw, th - (hz - y0), (10, 12, 18));
                        let line = (52, 78, 120);
                        let mut gy = 2.0f32;
                        while hz + (gy as i32) < y0 + th {
                            dib.fill(x0, hz + gy as i32, tw, 1, line);
                            gy *= 1.6;
                        }
                        for g in 1..8 {
                            dib.fill(x0 + tw * g / 8, hz, 1, th - (hz - y0), line);
                        }
                    }
                    Preview::Color(c) => dib.fill(x0, y0, tw, th, *c),
                    Preview::Picture(_) | Preview::Pano(_) | Preview::Scene(_) => match crate::background::thumb_cached(&thumb_path(&t.preview), tw as u32, th as u32) {
                        Some(img) => {
                            let stride = w as usize * 4;
                            let px = dib.pixels();
                            for row in 0..th as usize {
                                let s = row * tw as usize * 4;
                                let d = (y0 as usize + row) * stride + x0 as usize * 4;
                                px[d..d + tw as usize * 4].copy_from_slice(&img.px[s..s + tw as usize * 4]);
                            }
                        }
                        None => {
                            dib.fill(x0, y0, tw, th, (34, 38, 48));
                            text(&dib, "\u{2026}", rect(x0, y0, tw, th), 20.0, false, DIM, center);
                        }
                    },
                }
                if let Preview::Pano(p) = &t.preview {
                    use crate::background::PanoState;
                    let strip = |dib: &mut Dib, s: &str, c: (u8, u8, u8)| {
                        let sh = u(22.0);
                        shade(dib, w, rect(x0, y0 + th - sh, tw, sh), 2);
                        text(dib, s, rect(x0, y0 + th - sh, tw, sh), 12.0, false, c, center);
                    };
                    match crate::background::pano_state(p) {
                        PanoState::Ready => {}
                        PanoState::Missing => strip(&mut dib, "\u{2913} downloads when picked", TEXT),
                        PanoState::Downloading => {
                            shade(&mut dib, w, rect(x0, y0, tw, th), 2);
                            text(&dib, "Downloading\u{2026}", rect(x0, y0, tw, th), 14.0, true, TEXT, center);
                        }
                        PanoState::Failed(_) => strip(&mut dib, "Download failed: pick to retry", (255, 150, 140)),
                    }
                    let b = rect(x0 + u(5.0), y0 + u(5.0), u(38.0), u(18.0));
                    shade(&mut dib, w, b, 2);
                    text(&dib, "360\u{00b0}", b, 11.0, true, TEXT, center);
                }
                if let Preview::Scene(_) = &t.preview {
                    let b = rect(x0 + u(5.0), y0 + u(5.0), u(46.0), u(18.0));
                    shade(&mut dib, w, b, 2);
                    text(&dib, "Live", b, 11.0, true, TEXT, center);
                }
                let picked = cur == Some(i);
                let hot = self.hover == Some(Target::Tile(i));
                {
                    // Dark tiles need an outline to stand out from the panel.
                    let (c, b) = if picked {
                        (ACCENT, u(3.0))
                    } else if hot {
                        ((175, 180, 195), u(2.0))
                    } else {
                        (EDGE, 1)
                    };
                    dib.fill(x0 - b, y0 - b, tw + 2 * b, b, c);
                    dib.fill(x0 - b, y0 + th, tw + 2 * b, b, c);
                    dib.fill(x0 - b, y0, b, th, c);
                    dib.fill(x0 + tw, y0, b, th, c);
                }
                let lc = if picked { TEXT } else { DIM };
                text(&dib, &t.label, rect(x0, y0 + th + u(3.0), tw, lab - u(3.0)), 13.0, picked, lc, center);
                hits.push((rect(x0, y0, tw, th + lab), Target::Tile(i)));
            }
        }
        if more {
            // Scroll bar.
            let total = self.tile_rows() as i32;
            let x = w - pad + u(12.0);
            dib.fill(x, grid_top, u(4.0), rows_h - gap, (40, 45, 56));
            let bar_h = (rows_h - gap) * VIS_ROWS as i32 / total;
            let top = grid_top + (rows_h - gap - bar_h) * self.scroll as i32 / (total - VIS_ROWS as i32).max(1);
            dib.fill(x, top, u(4.0), bar_h, DIM);
        }
        y += rows_h;

        text(
            &dib,
            "Renderer, anti-aliasing, titles and closed apps reopen the 3D view.  \u{00b7}  360\u{00b0} panoramas: Poly Haven (CC0)",
            rect(pad, y, w - 2 * pad, u(24.0)),
            13.0,
            false,
            DIM,
            left,
        );
        y += u(30.0);
        text(
            &dib,
            "Keys: \u{2191}\u{2193} pick  \u{00b7}  \u{2190}\u{2192} change  \u{00b7}  Enter",
            rect(pad, y, w / 2, u(44.0)),
            13.0,
            false,
            DIM,
            left,
        );
        let close = rect(w - pad - u(130.0) - u(12.0) - u(200.0), y, u(200.0), u(44.0));
        let done = rect(w - pad - u(130.0), y, u(130.0), u(44.0));
        for (r, t, label, base) in [
            (close, Target::Close, "Close the 3D view", (58u8, 44u8, 50u8)),
            (done, Target::Done, "Done", (46, 104, 210)),
        ] {
            let hot = self.hover == Some(t);
            let row = if t == Target::Done { DONE_ROW } else { CLOSE_ROW };
            let c = if hot {
                (base.0.saturating_add(24), base.1.saturating_add(24), base.2.saturating_add(24))
            } else {
                base
            };
            if self.sel == row {
                dib.fill(r.left - u(3.0), r.top - u(3.0), r.right - r.left + u(6.0), r.bottom - r.top + u(6.0), ACCENT);
            }
            dib.fill(r.left, r.top, r.right - r.left, r.bottom - r.top, c);
            text(&dib, label, r, 16.0, true, TEXT, center);
            hits.push((r, t));
        }
        self.hits = hits;
        let mut img = dib.to_image();
        img.set_opaque();
        img
    }
}
