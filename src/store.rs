//! Persistent state: user config, card layout and screenshot files.
//!
//! Everything lives under `%APPDATA%\Window3D`:
//! - `config.json`  user settings (hotkey, speeds, shortcut folders)
//! - `layout.json`  every card ever seen, its 3D placement and the camera
//! - `shots\<id>.qoi` last screenshot of each window card (QOI-style compressed BGRA)
//! - `shortcuts\`    drop .lnk/.url/.exe files here to get launcher cards

use serde::{Deserialize, Serialize};
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use crate::image::Image;

#[derive(Serialize, Deserialize, Clone, Copy, PartialEq, Eq, Debug)]
pub enum Kind {
    Window,
    Shortcut,
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct Entry {
    pub id: u64,
    pub kind: Kind,
    /// Window: full image path of the owning process. Shortcut: path of the shortcut file.
    pub exe: String,
    #[serde(default)]
    pub class: String,
    #[serde(default)]
    pub title: String,
    pub pos: [f32; 3],
    /// Heading of the card: the camera yaw from which the card is seen head-on.
    pub yaw: f32,
    /// Unix seconds when the window was last seen alive (used to evict old ghosts).
    #[serde(default)]
    pub last_seen: u64,
    /// Window: show the window live instead of its last screenshot.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub live: bool,
    /// Window: let its sound through while the 3D view is open (muted otherwise).
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub sound: bool,
    /// Card size relative to the default.
    #[serde(default = "one", skip_serializing_if = "is_one")]
    pub scale: f32,
    /// Glue the card edge to edge to its neighbours while it is moved (on unless turned off,
    /// saved as `"no_snap": true`; the old `"snap"` key, off by default, is ignored).
    #[serde(rename = "no_snap", default = "yes", skip_serializing_if = "is_yes", with = "inverted")]
    pub snap: bool,
    /// Where the user put the card's toolbar (card-local position of its top button), if moved.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub toolbar: Option<[f32; 2]>,
}

pub(crate) fn yes() -> bool {
    true
}

fn is_yes(v: &bool) -> bool {
    *v
}

/// A bool saved as its opposite.
mod inverted {
    use serde::{Deserialize, Deserializer, Serializer};

    pub fn serialize<S: Serializer>(v: &bool, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_bool(!*v)
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<bool, D::Error> {
        bool::deserialize(d).map(|v| !v)
    }
}

fn one() -> f32 {
    1.0
}

fn is_one(v: &f32) -> bool {
    *v == 1.0
}

#[derive(Serialize, Deserialize, Clone, Copy, Debug)]
pub struct CameraState {
    pub pos: [f32; 3],
    pub yaw: f32,
    pub pitch: f32,
}

/// An empty frame in the room that a dropped card is fitted into (a "template slot").
#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct Slot {
    pub pos: [f32; 3],
    pub yaw: f32,
    pub hw: f32,
    pub hh: f32,
    /// Open workflow the slot came with (empty: made by hand).
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub wf: String,
}

/// A loaded workflow and the cards it placed.
#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct OpenWorkflow {
    pub name: String,
    pub ids: Vec<u64>,
}

#[derive(Serialize, Deserialize, Default)]
pub struct Layout {
    pub next_id: u64,
    pub entries: Vec<Entry>,
    pub camera: Option<CameraState>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub slots: Vec<Slot>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub workflows: Vec<OpenWorkflow>,
    /// The shortcut wall: the Desktop folder card sits at `pos`, the shortcuts fill rows below.
    #[serde(default)]
    pub wall: Option<Wall>,
}

#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq)]
pub struct Wall {
    /// Centre of the Desktop folder card at the top of the wall.
    pub pos: [f32; 3],
    pub yaw: f32,
    pub cols: usize,
    pub spacing: f32,
}

impl Default for Wall {
    fn default() -> Self {
        Self {
            pos: [0.0, crate::render::ROOM_HEIGHT - 1.3, 15.6],
            yaw: std::f32::consts::PI,
            cols: 14,
            spacing: 1.25,
        }
    }
}

#[derive(Serialize, Deserialize, Clone)]
#[serde(default)]
pub struct Config {
    /// e.g. "Ctrl+Alt+Space", "Win+Shift+D", "Alt+F1"
    pub hotkey: String,
    pub mouse_sensitivity: f32,
    pub move_speed: f32,
    pub fov_degrees: f32,
    pub show_ghosts: bool,
    pub max_ghosts: usize,
    /// Extra folders scanned for shortcuts (the built-in `shortcuts` folder is always scanned).
    pub shortcut_dirs: Vec<String>,
    pub include_desktop_shortcuts: bool,
    /// Programs (exe names) whose cards are never used in place: a click focuses the real window.
    pub focus_apps: Vec<String>,
    pub max_shortcuts: usize,
    /// Longest side of stored screenshots, in pixels.
    pub screenshot_size: u32,
    /// 4x MSAA. Renamed from `msaa`, whose saved `true` was the old default: card edges are
    /// now smoothed in the shader, at less than half the GPU time.
    #[serde(rename = "msaa_4x")]
    pub msaa: bool,
    /// "low-power" (integrated GPU, fastest to wake), "high-performance" or "default".
    pub gpu: String,
    /// Keep the (resource-free) GPU adapter between opens: much faster open, a little idle RAM.
    pub keep_gpu_adapter: bool,
    /// Keep the GPU device, pipelines and card textures while hidden: opens in ~100 ms instead
    /// of 1-2 s, at the cost of some idle (mostly GPU) memory. `false` releases everything.
    pub keep_warm: bool,
    /// With `keep_warm`, also keep the hidden overlay window and its swapchain: opens take a
    /// few tens of ms instead of ~100 ms, for the memory of the swapchain and render targets.
    pub keep_surface: bool,
    /// How a window is sized when its card is clicked: "borderless" (fill the monitor without
    /// a frame, else maximize), "maximize" or "none" (leave it as it is).
    pub focus_mode: String,
    /// Window classes or exe file names that are never made borderless (only maximized).
    pub borderless_exclude: Vec<String>,
    /// Monitor the 3D view opens on: "cursor" (the one under the mouse), "primary", or a
    /// monitor number from 1 (as listed in the tray menu).
    pub monitor: String,
    /// Show a live card of the real desktop beside the shortcut wall.
    pub desktop_card: bool,
    /// Show the shortcut wall. Shortcuts are still scanned and kept in the layout when off.
    pub show_shortcuts: bool,
    /// Lines of the stats window (its right-click menu picks them): "own", "app", "switch",
    /// "input", "live", "view".
    pub stats_fields: Vec<String>,
    /// Title strip (app icon and window title) above each window card's picture.
    pub card_titles: bool,
    /// On-screen help in the 3D view: the key list at the bottom and the hints shown over
    /// cards and tools.
    pub show_hints: bool,
    /// Most frames per second the 3D view draws (0: as many as the monitor shows). Frames
    /// that would look the same as the last one are not drawn at all.
    pub max_fps: u32,
    /// Most frames per second shown by a live card that is not in use (0: all of them). The
    /// card used in place or in theater mode always shows every frame.
    pub live_fps: u32,
    /// Behind the cards: "room" (the grid room) or a plain colour "#rrggbb" (no walls, only a
    /// faint floor grid).
    pub background: String,
}

/// Every line the stats window can show, in order, with its menu label.
pub const STATS_FIELDS: [(&str, &str); 6] = [
    ("own", "Window3D CPU / RAM"),
    ("app", "Foreground app CPU / RAM"),
    ("switch", "Switch latency"),
    ("input", "Input pass-on delay"),
    ("live", "Live cards: fps / delay"),
    ("view", "3D view frame rate"),
];

impl Default for Config {
    fn default() -> Self {
        Self {
            hotkey: "Ctrl+Alt+Z".into(),
            mouse_sensitivity: 0.0022,
            move_speed: 6.0,
            fov_degrees: 70.0,
            show_ghosts: false,
            max_ghosts: 24,
            shortcut_dirs: Vec::new(),
            include_desktop_shortcuts: true,
            focus_apps: Vec::new(),
            max_shortcuts: 200,
            screenshot_size: 800,
            msaa: false,
            gpu: "low-power".into(),
            keep_gpu_adapter: false,
            keep_warm: true,
            keep_surface: true,
            focus_mode: "none".into(),
            borderless_exclude: Vec::new(),
            monitor: "cursor".into(),
            desktop_card: true,
            show_shortcuts: false,
            stats_fields: STATS_FIELDS.iter().map(|f| f.0.to_string()).collect(),
            card_titles: false,
            show_hints: true,
            max_fps: 60,
            live_fps: 20,
            background: "room".into(),
        }
    }
}

pub fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

pub fn app_dir() -> PathBuf {
    // Override for testing / portable use.
    if let Some(d) = std::env::var_os("WINDOW3D_DATA") {
        return PathBuf::from(d);
    }
    let base = std::env::var_os("APPDATA")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."));
    base.join("Window3D")
}

pub fn shots_dir() -> PathBuf {
    app_dir().join("shots")
}

pub fn shortcuts_dir() -> PathBuf {
    app_dir().join("shortcuts")
}

pub fn ensure_dirs() {
    let _ = fs::create_dir_all(shots_dir());
    let _ = fs::create_dir_all(shortcuts_dir());
}

/// Appends a line to `%APPDATA%\Window3D\log.txt` (the app has no console).
pub fn log(msg: &str) {
    let path = app_dir().join("log.txt");
    // Keep the log from growing without bound.
    if fs::metadata(&path)
        .map(|m| m.len() > 1024 * 1024)
        .unwrap_or(false)
    {
        let _ = fs::remove_file(&path);
    }
    if let Ok(mut f) = fs::OpenOptions::new().create(true).append(true).open(&path) {
        let _ = writeln!(f, "[{}] {}", now_secs(), msg);
    }
}

fn write_atomic(path: &Path, data: &[u8]) -> std::io::Result<()> {
    let tmp = path.with_extension("tmp");
    fs::write(&tmp, data)?;
    fs::rename(&tmp, path)
}

pub fn load_config() -> Config {
    let path = app_dir().join("config.json");
    match fs::read_to_string(&path) {
        Ok(s) => {
            let mut cfg: Config = serde_json::from_str(s.trim_start_matches('\u{feff}')).unwrap_or_else(|e| {
                log(&format!("config.json is invalid ({e}); using defaults"));
                Config::default()
            });
            // 48 was the old default, too small for a full desktop: move it to the new one.
            if cfg.max_shortcuts == 48 {
                cfg.max_shortcuts = Config::default().max_shortcuts;
                save_config(&cfg);
            }
            cfg
        }
        Err(_) => {
            // First run: write the defaults so the user can discover and edit them.
            let cfg = Config::default();
            if let Ok(s) = serde_json::to_string_pretty(&cfg) {
                let _ = write_atomic(&path, s.as_bytes());
            }
            cfg
        }
    }
}

pub fn save_config(cfg: &Config) {
    if let Ok(s) = serde_json::to_string_pretty(cfg) {
        let _ = write_atomic(&app_dir().join("config.json"), s.as_bytes());
    }
}

pub fn load_layout() -> Layout {
    fs::read_to_string(app_dir().join("layout.json"))
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default()
}

pub fn save_layout(layout: &Layout) {
    match serde_json::to_string(layout) {
        Ok(s) => {
            if let Err(e) = write_atomic(&app_dir().join("layout.json"), s.as_bytes()) {
                log(&format!("failed to save layout: {e}"));
            }
        }
        Err(e) => log(&format!("failed to serialize layout: {e}")),
    }
}

fn shot_path(id: u64) -> PathBuf {
    shots_dir().join(format!("{id}.qoi"))
}

pub fn has_shot(id: u64) -> bool {
    shot_path(id).exists()
}

/// Changes whenever the screenshot of `id` is rewritten (0 if there is none).
pub fn shot_version(id: u64) -> u64 {
    fs::metadata(shot_path(id))
        .and_then(|m| m.modified())
        .ok()
        .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
        .map_or(0, |d| d.as_nanos() as u64)
}

pub fn save_shot(id: u64, img: &Image) {
    let data = crate::image::qoi_encode(img);
    if let Err(e) = write_atomic(&shot_path(id), &data) {
        log(&format!("failed to save screenshot {id}: {e}"));
    }
}

pub fn load_shot(id: u64) -> Option<Image> {
    let data = fs::read(shot_path(id)).ok()?;
    crate::image::qoi_decode(&data)
}

pub fn delete_shot(id: u64) {
    let _ = fs::remove_file(shot_path(id));
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn snap_is_on_unless_saved_off() {
        let base = r#"{"id":1,"kind":"Window","exe":"x","title":"t","pos":[0,0,0],"yaw":0"#;
        let load = |extra: &str| serde_json::from_str::<Entry>(&format!("{base}{extra}}}")).unwrap();
        assert!(load("").snap);
        // The old key (off by default) no longer matters.
        assert!(load(r#","snap":false"#).snap);
        assert!(load(r#","snap":true"#).snap);
        let off = load(r#","no_snap":true"#);
        assert!(!off.snap);
        assert!(serde_json::to_string(&off).unwrap().contains(r#""no_snap":true"#));
        assert!(!serde_json::to_string(&load("")).unwrap().contains("snap"));
    }
}
