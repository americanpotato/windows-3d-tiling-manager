//! Maps live windows and shortcut files onto persistent layout entries.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use crate::store::{self, Entry, Kind, Layout, Wall};
use crate::win::WinInfo;

pub struct Model {
    pub layout: Layout,
    /// Live window handle -> entry id. Rebuilt as windows come and go; never persisted.
    pub bindings: HashMap<isize, u64>,
}

const RING_RADIUS: f32 = 9.0;
const RING_SLOTS: usize = 12;
const ROW_HEIGHT: f32 = 1.9;
/// Vertical distance between shortcut rows, and from the Desktop folder card to the first row.
const WALL_ROW: f32 = 1.6;
const WALL_HEADER_GAP: f32 = 2.2;
/// Scale of the Desktop folder card at the top of the wall.
const WALL_HEADER_SCALE: f32 = 1.6;
pub const DESKTOP_CLASS: &str = "Progman";

fn lower(s: &str) -> String {
    s.to_lowercase()
}

impl Model {
    pub fn load() -> Self {
        let mut layout = store::load_layout();
        let max_id = layout.entries.iter().map(|e| e.id).max().unwrap_or(0);
        layout.next_id = layout.next_id.max(max_id + 1);
        Self {
            layout,
            bindings: HashMap::new(),
        }
    }

    pub fn get(&self, id: u64) -> Option<&Entry> {
        self.layout.entries.iter().find(|e| e.id == id)
    }

    pub fn get_mut(&mut self, id: u64) -> Option<&mut Entry> {
        self.layout.entries.iter_mut().find(|e| e.id == id)
    }

    pub fn remove(&mut self, id: u64) {
        self.layout.entries.retain(|e| e.id != id);
        self.bindings.retain(|_, v| *v != id);
        store::delete_shot(id);
    }

    fn bound_ids(&self) -> std::collections::HashSet<u64> {
        self.bindings.values().copied().collect()
    }

    /// Finds (or creates) the entry for a live window and binds the handle to it.
    /// Matching order: existing binding, exact exe+class+title of an unbound entry,
    /// then exe+class of an unbound entry (reclaims the ghost of a reopened app).
    pub fn entry_for_window(&mut self, w: &WinInfo) -> u64 {
        let now = store::now_secs();
        if let Some(&id) = self.bindings.get(&w.hwnd) {
            if let Some(e) = self.get_mut(id) {
                e.title = w.title.clone();
                e.last_seen = now;
                return id;
            }
        }
        let bound = self.bound_ids();
        let (exe, class) = (lower(&w.exe), w.class.as_str());
        let candidates = || {
            self.layout.entries.iter().filter(|e| {
                e.kind == Kind::Window
                    && !bound.contains(&e.id)
                    && lower(&e.exe) == exe
                    && e.class == class
            })
        };
        let found = candidates()
            .find(|e| e.title == w.title)
            .or_else(|| candidates().max_by_key(|e| e.last_seen))
            .map(|e| e.id);
        let id = match found {
            Some(id) => id,
            None => {
                let (pos, yaw) = if w.class == DESKTOP_CLASS {
                    desktop_card_pose(&self.wall())
                } else {
                    self.free_window_slot()
                };
                let id = self.layout.next_id;
                self.layout.next_id += 1;
                self.layout.entries.push(Entry {
                    id,
                    kind: Kind::Window,
                    exe: w.exe.clone(),
                    class: w.class.clone(),
                    title: w.title.clone(),
                    pos,
                    yaw,
                    last_seen: now,
                    live: w.class == DESKTOP_CLASS,
                    sound: false,
                    scale: 1.0,
                    snap: true,
                    toolbar: None,
                });
                id
            }
        };
        if let Some(e) = self.get_mut(id) {
            e.title = w.title.clone();
            e.last_seen = now;
        }
        self.bindings.insert(w.hwnd, id);
        id
    }

    /// Re-synchronizes bindings with the current set of live windows.
    pub fn sync_live(&mut self, live: &[WinInfo]) {
        let alive: std::collections::HashSet<isize> = live.iter().map(|w| w.hwnd).collect();
        self.bindings.retain(|h, _| alive.contains(h));
        for w in live {
            self.entry_for_window(w);
        }
    }

    /// Ghosts = window entries without a live window. Keeps only the `max` most recent.
    pub fn evict_ghosts(&mut self, max: usize) {
        let bound = self.bound_ids();
        let mut ghosts: Vec<(u64, u64)> = self
            .layout
            .entries
            .iter()
            .filter(|e| e.kind == Kind::Window && !bound.contains(&e.id))
            .map(|e| (e.last_seen, e.id))
            .collect();
        if ghosts.len() <= max {
            return;
        }
        ghosts.sort_unstable_by(|a, b| b.cmp(a));
        for (_, id) in ghosts.split_off(max) {
            self.remove(id);
        }
    }

    pub fn is_ghost(&self, id: u64) -> bool {
        self.get(id)
            .map(|e| e.kind == Kind::Window)
            .unwrap_or(false)
            && !self.bindings.values().any(|v| *v == id)
    }

    /// Adds entries for new shortcut files and drops entries whose files disappeared.
    pub fn sync_shortcuts(&mut self, files: &[PathBuf]) {
        let wanted: std::collections::HashSet<String> =
            files.iter().map(|p| lower(&p.to_string_lossy())).collect();
        let stale: Vec<u64> = self
            .layout
            .entries
            .iter()
            .filter(|e| e.kind == Kind::Shortcut && !wanted.contains(&lower(&e.exe)))
            .map(|e| e.id)
            .collect();
        for id in stale {
            self.remove(id);
        }
        for f in files {
            let path = f.to_string_lossy().to_string();
            let key = lower(&path);
            if self
                .layout
                .entries
                .iter()
                .any(|e| e.kind == Kind::Shortcut && lower(&e.exe) == key)
            {
                continue;
            }
            let header = is_wall_header(&path);
            let (pos, yaw) = if header {
                let w = self.wall();
                (w.pos, w.yaw)
            } else {
                self.free_shortcut_slot()
            };
            let id = self.layout.next_id;
            self.layout.next_id += 1;
            self.layout.entries.push(Entry {
                id,
                kind: Kind::Shortcut,
                exe: path,
                class: String::new(),
                title: shortcut_name(f),
                pos,
                yaw,
                last_seen: 0,
                live: false,
                sound: false,
                scale: if header { WALL_HEADER_SCALE } else { 1.0 },
                snap: true,
                toolbar: None,
            });
        }
        // First run with the wall: line the existing shortcuts up under the Desktop card.
        if self.layout.wall.is_none() {
            self.layout.wall = Some(Wall::default());
            self.tidy_wall();
        }
    }

    pub fn wall(&self) -> Wall {
        self.layout.wall.unwrap_or_default()
    }

    /// The Desktop folder card at the top of the wall, if it has one.
    pub fn wall_header(&self) -> Option<u64> {
        self.layout
            .entries
            .iter()
            .find(|e| e.kind == Kind::Shortcut && is_wall_header(&e.exe))
            .map(|e| e.id)
    }

    /// Puts the Desktop card at the top of the wall, every other shortcut in rows below it (in
    /// file order) and the live desktop card beside it.
    pub fn tidy_wall(&mut self) {
        let w = self.wall();
        let n = self.shortcut_count();
        let mut i = 0;
        for e in self.layout.entries.iter_mut() {
            if e.kind == Kind::Shortcut {
                if is_wall_header(&e.exe) {
                    (e.pos, e.yaw) = (w.pos, w.yaw);
                } else {
                    (e.pos, e.yaw) = wall_cell(&w, i, n);
                    e.scale = 1.0;
                    i += 1;
                }
            } else if e.class == DESKTOP_CLASS {
                (e.pos, e.yaw) = desktop_card_pose(&w);
            }
        }
    }

    /// After a drag: when the Desktop card moved, the whole wall follows it. Returns whether
    /// the wall moved.
    pub fn follow_wall(&mut self) -> bool {
        let Some(h) = self.wall_header().and_then(|id| self.get(id)) else {
            return false;
        };
        let (pos, yaw) = (h.pos, h.yaw);
        let mut w = self.wall();
        if w.pos == pos && w.yaw == yaw {
            return false;
        }
        (w.pos, w.yaw) = (pos, yaw);
        self.layout.wall = Some(w);
        self.tidy_wall();
        true
    }

    /// Puts the window cards `ids` (in that order) back on the rings around the room centre,
    /// from in front of the default camera outward; their sizes are kept.
    pub fn tidy_windows(&mut self, ids: &[u64]) {
        for e in self.layout.entries.iter_mut() {
            if ids.contains(&e.id) {
                e.pos = [1e6, 1e6, 1e6];
            }
        }
        for &id in ids {
            let (pos, yaw) = self.free_window_slot();
            if let Some(e) = self.get_mut(id) {
                (e.pos, e.yaw) = (pos, yaw);
            }
        }
    }

    /// Changes the number of wall columns by `d` (2..=30) and re-arranges the wall.
    pub fn wall_columns(&mut self, d: i32) {
        let mut w = self.wall();
        w.cols = (w.cols as i32 + d).clamp(2, 30) as usize;
        self.layout.wall = Some(w);
        self.tidy_wall();
    }

    fn occupied(&self, p: [f32; 3], radius: f32) -> bool {
        self.layout.entries.iter().any(|e| {
            let d = [e.pos[0] - p[0], e.pos[1] - p[1], e.pos[2] - p[2]];
            d[0] * d[0] + d[1] * d[1] + d[2] * d[2] < radius * radius
        })
    }

    /// Next free spot on the rings of cards around the room centre, all facing inward.
    fn free_window_slot(&self) -> ([f32; 3], f32) {
        for i in 0..400 {
            let (row, j) = (i / RING_SLOTS, i % RING_SLOTS);
            // Start in front of the default camera (looking toward -Z) and alternate right/left.
            let step = std::f32::consts::TAU / RING_SLOTS as f32;
            let idx = if j % 2 == 1 {
                ((j + 1) / 2) as f32
            } else {
                -((j / 2) as f32)
            };
            // Stagger alternate rows so cards above don't sit exactly on top of each other.
            let angle = idx * step + if row % 2 == 1 { step * 0.5 } else { 0.0 };
            let radius = RING_RADIUS + (row / 4) as f32 * 2.5;
            let y = 1.7 + (row % 4) as f32 * ROW_HEIGHT;
            // Card in direction `angle` from the centre: forward = (sin a, 0, -cos a).
            let p = [
                angle.sin() * radius,
                y.min(crate::render::ROOM_HEIGHT - 1.0),
                -angle.cos() * radius,
            ];
            if !self.occupied(p, 1.2) {
                return (p, angle);
            }
        }
        ([0.0, 1.7, -RING_RADIUS], 0.0)
    }

    /// Self-test of the shortcut wall: tidy, column change and dragging the Desktop card.
    pub fn selftest_wall(&mut self, files: &[PathBuf]) -> String {
        let saved: Vec<_> = self.layout.entries.iter().map(|e| (e.pos, e.yaw)).collect();
        let wall = self.layout.wall;
        let ext = |x: &str| {
            files
                .iter()
                .filter(|p| p.extension().is_some_and(|e| e.eq_ignore_ascii_case(x)))
                .count()
        };
        let folders = files.iter().filter(|p| p.is_dir()).count();
        self.tidy_wall();
        let shortcuts = |m: &Self| -> Vec<[f32; 3]> {
            m.layout
                .entries
                .iter()
                .filter(|e| e.kind == Kind::Shortcut)
                .map(|e| e.pos)
                .collect()
        };
        let a = shortcuts(self);
        let distinct = a.iter().enumerate().all(|(i, p)| {
            a[i + 1..]
                .iter()
                .all(|q| (p[0] - q[0]).abs() + (p[1] - q[1]).abs() + (p[2] - q[2]).abs() > 0.1)
        });
        let in_room = a
            .iter()
            .all(|p| p[1] > 0.3 && p[1] < crate::render::ROOM_HEIGHT);
        self.wall_columns(-4);
        let cols_ok = self.wall().cols + 4 == wall.unwrap_or_default().cols;
        let header = self.wall_header();
        let mut follows = false;
        if let Some(h) = header.and_then(|id| self.get_mut(id)) {
            h.pos[0] += 1.0;
            let before = shortcuts(self);
            follows = self.follow_wall()
                && shortcuts(self)
                    .iter()
                    .zip(&before)
                    .filter(|(p, b)| (p[0] - b[0]).abs() > 0.5)
                    .count()
                    >= before.len().saturating_sub(2);
        }
        let desktop = self.layout.entries.iter().any(|e| e.class == DESKTOP_CLASS);
        let ok = distinct && in_room && cols_ok && (header.is_none() || follows);
        self.layout.wall = wall;
        for (e, s) in self.layout.entries.iter_mut().zip(saved) {
            (e.pos, e.yaw) = s;
        }
        format!(
            "wall test: items={} (lnk={} url={} exe={} bat={} cmd={} ps1={} folders={folders}) header={} desktop-card={desktop} distinct={distinct} in-room={in_room} columns={cols_ok} follows={follows} ok={ok}",
            files.len(),
            ext("lnk"),
            ext("url"),
            ext("exe"),
            ext("bat"),
            ext("cmd"),
            ext("ps1"),
            header.is_some(),
        )
    }

    fn shortcut_count(&self) -> usize {
        self.layout
            .entries
            .iter()
            .filter(|e| e.kind == Kind::Shortcut && !is_wall_header(&e.exe))
            .count()
    }

    /// Next free cell of the shortcut wall.
    fn free_shortcut_slot(&self) -> ([f32; 3], f32) {
        let w = self.wall();
        let n = self.shortcut_count() + 1;
        (0..1000)
            .map(|i| wall_cell(&w, i, n))
            .find(|(p, _)| !self.occupied(*p, 0.5))
            .unwrap_or((w.pos, w.yaw))
    }
}

/// Cell `i` of a wall of `n` shortcuts: rows below the Desktop card, left to right as seen from
/// the room. When `n` doesn't fit above the floor, the wall gets wider than `cols`.
fn wall_cell(w: &Wall, i: usize, n: usize) -> ([f32; 3], f32) {
    let rows = (((w.pos[1] - WALL_HEADER_GAP - 0.7) / WALL_ROW).floor() as usize + 1).max(1);
    let cols = w.cols.max(n.div_ceil(rows)).max(1);
    let (row, col) = (i / cols, i % cols);
    // Seen from the room (looking at the wall), right is (cos yaw, 0, sin yaw).
    let right = [w.yaw.cos(), w.yaw.sin()];
    // Very full walls squeeze together rather than leave the room.
    let spacing = w
        .spacing
        .min(2.0 * (crate::render::ROOM_HALF - 1.5) / cols as f32);
    let x = (col as f32 - (cols as f32 - 1.0) / 2.0) * spacing;
    (
        [
            w.pos[0] + right[0] * x,
            w.pos[1] - WALL_HEADER_GAP - row as f32 * WALL_ROW,
            w.pos[2] + right[1] * x,
        ],
        w.yaw,
    )
}

/// The live desktop card: left of the Desktop folder card, above the rows.
fn desktop_card_pose(w: &Wall) -> ([f32; 3], f32) {
    let x = -3.3;
    let p = [
        w.pos[0] + w.yaw.cos() * x,
        w.pos[1] - 0.2,
        w.pos[2] + w.yaw.sin() * x,
    ];
    (p, w.yaw)
}

/// The user's Desktop folder.

pub fn desktop_dir() -> Option<PathBuf> {
    static DIR: std::sync::OnceLock<Option<PathBuf>> = std::sync::OnceLock::new();
    DIR.get_or_init(|| {
        crate::win::desktop_dirs()
            .0
            .or_else(|| std::env::var_os("USERPROFILE").map(|h| PathBuf::from(h).join("Desktop")))
    })
    .clone()
}

/// Whether a shortcut path is the Desktop folder card that heads the wall.
fn is_wall_header(path: &str) -> bool {
    desktop_dir().is_some_and(|d| lower(&d.to_string_lossy()) == lower(path))
}

/// Whether a folder item becomes a card: everything but hidden/system files (`desktop.ini`),
/// Office lock files and temporary files.
fn wanted_item(p: &Path) -> bool {
    use std::os::windows::fs::MetadataExt;
    const HIDDEN: u32 = 0x2;
    const SYSTEM: u32 = 0x4;
    let name = p
        .file_name()
        .map(|n| n.to_string_lossy().to_lowercase())
        .unwrap_or_default();
    if name.is_empty() || name == "desktop.ini" || name == "thumbs.db" || name.starts_with("~$") {
        return false;
    }
    if name.ends_with(".tmp") || name.ends_with(".crdownload") || name.ends_with(".part") {
        return false;
    }
    std::fs::metadata(p).is_ok_and(|m| m.file_attributes() & (HIDDEN | SYSTEM) == 0)
}

pub fn shortcut_name(p: &Path) -> String {
    p.file_stem()
        .map(|s| s.to_string_lossy().to_string())
        .unwrap_or_default()
}

/// Items of the built-in folder, the desktops (optional) and configured folders: shortcuts,
/// programs, scripts (`.bat`, `.cmd`, `.ps1`), documents and folders. With the desktops, the
/// Desktop folder itself comes first (the card at the top of the wall).
pub fn scan_shortcuts(cfg: &store::Config) -> Vec<PathBuf> {
    let mut dirs: Vec<PathBuf> = vec![store::shortcuts_dir()];
    let mut out = Vec::new();
    if cfg.include_desktop_shortcuts {
        if let Some(d) = desktop_dir() {
            if d.is_dir() {
                out.push(d.clone());
            }
            dirs.push(d);
        }
        let public = crate::win::desktop_dirs()
            .1
            .or_else(|| std::env::var_os("PUBLIC").map(|p| PathBuf::from(p).join("Desktop")));
        dirs.extend(public);
    }
    dirs.extend(cfg.shortcut_dirs.iter().map(PathBuf::from));
    let limit = cfg.max_shortcuts + out.len();
    for d in dirs {
        let Ok(rd) = std::fs::read_dir(&d) else {
            continue;
        };
        let mut files: Vec<PathBuf> = rd
            .filter_map(|e| e.ok().map(|e| e.path()))
            .filter(|p| wanted_item(p))
            .collect();
        files.sort_by_key(|p| p.to_string_lossy().to_lowercase());
        out.extend(files);
        if out.len() > limit {
            store::log(&format!(
                "more than max_shortcuts ({}) items; the rest are not shown",
                cfg.max_shortcuts
            ));
            out.truncate(limit);
            break;
        }
    }
    out
}
