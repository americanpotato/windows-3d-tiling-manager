//! The 3D scene shown while the overlay is open: camera, cards, picking and dragging.

use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::Receiver;
use std::time::Instant;

use glam::{Mat4, Vec2, Vec3};
use windows::Win32::Foundation::{HWND, POINT};
use windows::Win32::Graphics::Gdi::ClientToScreen;
use windows::Win32::UI::Input::KeyboardAndMouse::*;
use windows::Win32::UI::WindowsAndMessaging::*;

use crate::live::{self, Live};

use crate::image::Image;
use crate::model::Model;
use crate::render::{Backdrop, CardDraw, Gpu, HudDraw, ROOM_HALF, ROOM_HEIGHT, Renderer};
use crate::store::{CameraState, Config, Kind, Slot};
use crate::tools::{self, Quad, Rect3, Tool};

pub struct Card {
    pub id: u64,
    pub kind: Kind,
    pub ghost: bool,
    pub hwnd: Option<isize>,
    half_w: f32,
    half_h: f32,
    tex: usize,
}

#[derive(Clone, Copy)]
pub struct Camera {
    pub pos: Vec3,
    pub yaw: f32,
    pub pitch: f32,
}

impl Camera {
    pub fn forward(&self) -> Vec3 {
        Vec3::new(
            self.yaw.sin() * self.pitch.cos(),
            self.pitch.sin(),
            -self.yaw.cos() * self.pitch.cos(),
        )
    }
}

pub const DEFAULT_CAMERA: CameraState = CameraState {
    pos: [0.0, 1.7, 0.0],
    yaw: 0.0,
    pitch: 0.0,
};

/// What the app should do in response to input.
pub enum Action {
    None,
    Close,
    Focus(isize),
    /// Theater mode flew into the window: make the real one fullscreen and step aside.
    Theater(isize),
    Launch(String),
    Forget(u64),
    LayoutChanged,
    /// Turn live view of a card on or off.
    SetLive(u64, bool),
    /// Let the sound of a card's app through (or mute it) while the 3D view is open.
    SetSound(u64, bool),
    /// Watch a card in the 3D view: live view and sound on.
    Watch(u64),
    /// Sound settings of several cards changed (saved and applied).
    AudioChanged,
    /// Close the card's real window (`true`: end its process).
    CloseWindow(u64, bool),
    /// Esc while flying: open the options menu (the app fills in the current settings).
    OpenMenu,
    /// The options menu closed with changed settings: save and apply them.
    ApplySettings(MenuSettings),
}

pub use crate::menu::MenuSettings;

/// The key list and the hints over cards and tools are drawn (`show_hints` in the config).
pub static SHOW_HINTS: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(true);
use crate::menu::{Click, Menu};

/// What the crosshair is on.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Hit {
    Card,
    Tool(Tool),
    /// Move gizmo axis 0 (X, along the card), 1 (Y, up) or 2 (Z, towards the viewer).
    Axis(usize),
}

/// A toolbar button or gizmo axis held down: mouse moves operate it instead of looking.
struct ToolDrag {
    card: usize,
    hit: Hit,
    moved: f32,
    /// Mouse movement not applied yet.
    acc: (f32, f32),
    /// Unsnapped card centre while moving along an axis (so a snapped card can be pulled off).
    raw: Option<Vec3>,
}

/// Texture slots of the toolbar icons, by `Tool` index (off / on state) and gizmo axis.
struct Icons {
    off: [usize; Tool::ALL.len()],
    on: [usize; Tool::ALL.len()],
    axis: [usize; 3],
}

struct Press {
    card: usize,
    dist: f32,
    local: (f32, f32),
    moved: f32,
    dragging: bool,
}

/// Set while a live card is being used in place: the overlay window then shows the cursor and
/// sends mouse and keyboard messages as `Event::Pass` instead of driving the camera.
pub static INTERACT: AtomicBool = AtomicBool::new(false);

/// Set while the overlay gets raw mouse motion (the mouse is grabbed for flying).
pub static RAW_INPUT: AtomicBool = AtomicBool::new(false);

/// State of using a live card in place. The same for every app: the real window has the real
/// focus, the overlay lets the real mouse through (click-through) while the real cursor is on
/// the real window, which owns it as in normal use (hover, clicks, wheel, games' mouse-look);
/// the card shows where that cursor is. Nothing is posted or held.
struct Interact {
    card: usize,
    /// Where the pointer is on the overlay (client pixels).
    pos: (f32, f32),
    since: Instant,
    /// The overlay is click-through now (the real cursor is on the real window).
    through: bool,
    /// Mouse motion (raw, pixels) pushing on past the edge the real cursor is held at (the
    /// window fills the screen, or the app clips the cursor to itself): the pointer goes
    /// that far off the card, so there is always somewhere to click out.
    push: (f32, f32),
    /// Raw motion since the last frame.
    raw: (f32, f32),
    /// A mouse button was down last frame (clicks are logged).
    button: bool,
    /// The real cursor went over to another monitor (pushed off the side of the card): it is
    /// free there, and comes back onto the window when it comes back onto this monitor.
    away: bool,
}

/// How far (pixels) a held cursor's push past the window's edge is counted.
const PUSH_MAX: f32 = 160.0;

/// Two `Esc` presses within this close the overlay or end using a window in place.
const DOUBLE_ESC: std::time::Duration = std::time::Duration::from_millis(500);

/// Pseudo-fullscreen: the camera parked in front of a card so it fills the view.
struct Theater {
    card: usize,
    /// Where the camera goes back to.
    back: Camera,
    /// Hand over to the real window once the flight ends (and a live frame is up): Shift+F.
    auto: bool,
    t0: Instant,
}

/// The stats panel (I): its texture slot and the samples it compares against.
struct Stats {
    slot: Option<usize>,
    size: (u32, u32),
    next: Instant,
    own: Option<crate::stats::ProcSample>,
    app: Option<crate::stats::ProcSample>,
    /// Smoothed frame time in milliseconds.
    frame_ms: f32,
}

/// A short camera flight (into or out of theater mode).
struct Fly {
    from: Camera,
    to: Camera,
    t0: Instant,
}

const FLY_SECS: f32 = 0.28;

/// How far in front of its card a live frame is drawn (so the screenshot never shows through).
const LIVE_NUDGE: f32 = 0.003;

/// A clicked card whose live view shows no frame by then is focused instead of used in place.
const PENDING_SECS: f32 = 2.5;

/// Texture cache ids above this are HUD elements, not cards.
const HUD_IDS: u64 = u64::MAX - 64;

/// Type-to-search (`/`): matching cards, the selected one is flown to.
struct Search {
    query: String,
    /// Matching card indices, best first.
    hits: Vec<usize>,
    sel: usize,
    /// Camera before the search, flown back to on `Esc`.
    back: Camera,
    /// Label at the top of the screen, for its text.
    hud: Option<(String, Hud)>,
}

struct Hud {
    tex: usize,
    w: f32,
    h: f32,
}

pub struct Overlay {
    cards: Vec<Card>,
    pub cam: Camera,
    vel: Vec3,
    hover: Option<usize>,
    press: Option<Press>,
    mouse: (f32, f32),
    sensitivity: f32,
    /// `Config::focus_apps`.
    focus_apps: Vec<String>,
    /// Last `Esc` press: a second one within `DOUBLE_ESC` closes / ends the in-place use (a
    /// single `Esc` goes to the window, e.g. a game's pause menu).
    last_esc: Option<Instant>,
    /// When an Esc was sent to close Start or search on a click out of them.
    shell_esc: Option<Instant>,
    /// `Esc` held at the last poll (games used in place have the real focus: polled).
    esc_down: bool,
    speed: f32,
    fov: f32,
    start: Instant,
    last: Instant,
    /// What the last drawn frame showed (a hash) and when it was drawn: unchanged frames are
    /// skipped, which saves most of the GPU and CPU time while nothing moves.
    drawn: (u64, Instant),
    /// Whether the last `frame` drew (else the caller can sleep until the next one).
    pub rendered: bool,
    /// Hash of the camera, cards and HUD of the last frame, and whether it differed from the
    /// one before (something moves: frames should come at the full rate).
    scene: u64,
    pub moving: bool,
    /// When the scene last changed: a hover pulse is only animated for a few seconds after.
    scene_at: std::time::Instant,
    cross: Hud,
    help: Hud,
    hints: [Hud; 7],
    /// Last check of whether the hovered live card's window is a covered Chromium window
    /// (card id, when, result); rechecked twice a second.
    occluded: (u64, Instant, bool),
    /// Windows given a shaped region so covered live Chromium windows keep drawing
    /// (`win::uncover_chromium`), and when that is checked next.
    shaped: Vec<isize>,
    shape_check: Instant,
    /// Texture slots used by the HUD (they come first).
    hud_slots: usize,
    hwnd: isize,
    live: Option<Live>,
    /// Live capture can't run here (no D3D11 / Windows.Graphics.Capture).
    live_broken: bool,
    /// The live capture device being made on a worker (it takes ~60 ms).
    live_making: Option<std::sync::mpsc::Receiver<Result<live::Sendable, String>>>,
    /// Cards whose window refused to be captured.
    live_failed: HashSet<u64>,
    /// Smoothed frame time (s) and when the stats window gets the next report.
    frame_avg: f32,
    view_report: Instant,
    interact: Option<Interact>,
    /// The card last used in place and when its use ended: a window launched from it (Start
    /// shown on it) comes up by it, though the launch ended its use.
    left: Option<(u64, Instant)>,
    /// A clicked card to use in place as soon as its live view has a frame.
    pending_interact: Option<(u64, Instant)>,
    /// The wait for `pending_interact` was written to the log.
    pending_traced: bool,
    /// The shell's popups were last made invisible then (see `sync_shell`).
    shell_hidden: Option<Instant>,
    theater: Option<Theater>,
    fly: Option<Fly>,
    stats: Option<Stats>,
    /// The cursor is released (Tab): no mouse-look or picking until it is taken back.
    free: bool,
    /// What the crosshair is on (for the hovered card).
    hit: Hit,
    /// Card whose toolbar is shown, and when the crosshair last was on it or its toolbar.
    bar: Option<(usize, Instant)>,
    /// Card (id) showing the move gizmo.
    gizmo: Option<u64>,
    tool: Option<ToolDrag>,
    icons: Icons,
    tips: [Hud; Tool::ALL.len()],
    /// Last frame's view-projection, to turn mouse moves into moves along an axis.
    vp: Mat4,
    /// Template slot under the crosshair while a card is dragged (it is fitted there on drop).
    slot_hot: Option<usize>,
    slot_tex: usize,
    /// Open workflow picked with `K` (index into `layout.workflows`); its cards are highlighted.
    wf_sel: Option<usize>,
    /// Label of the picked workflow, for (index, name).
    wf_hud: Option<(usize, String, Hud)>,
    search: Option<Search>,
    /// The options menu (Esc while flying).
    menu: Option<Menu>,
    /// The options menu's texture slot and size.
    menu_slot: Option<(usize, (u32, u32))>,
    text_px: i32,
    /// The background picture being decoded: its content key and where it arrives.
    sky_load: Option<(u64, Receiver<Result<crate::background::Loaded, String>>)>,
    /// The `background` setting shown when the options menu is closed.
    background: String,
    /// `background::downloads_ended` when last looked at.
    downloads_seen: u32,
    /// Last, so it is dropped after the live captures that share its device.
    renderer: Renderer,
}

/// Shown when a background picture can't be read: the "night" plain colour.
const FALLBACK_SKY: [f32; 3] = [0.0015, 0.0021, 0.004];

/// Sets the background from a `background` setting. A picture not yet on the GPU starts
/// decoding (unless `load` already is that one) and is returned to be polled.
fn apply_background(
    renderer: &mut Renderer,
    setting: &str,
    load: Option<(u64, Receiver<Result<crate::background::Loaded, String>>)>,
) -> Option<(u64, Receiver<Result<crate::background::Loaded, String>>)> {
    use crate::background::Kind;
    match crate::background::parse(setting) {
        Kind::Room => renderer.set_background(Backdrop::Room),
        Kind::Plain(c) => renderer.set_background(Backdrop::Plain(c)),
        Kind::Scene(x) => renderer.set_background(Backdrop::Scene { index: x.index, dim: 0.15 }),
        Kind::Picture(path) => {
            renderer.set_background(Backdrop::Picture { dim: 0.2 });
            // A panorama not downloaded yet: it shows when the download ends.
            if let Some(p) = crate::background::pano_of(&path).filter(|_| !path.is_file()) {
                if crate::background::pano_state(p) != crate::background::PanoState::Downloading {
                    crate::background::fetch(p);
                }
                return load;
            }
            let key = crate::background::key(&path);
            if renderer.gpu().sky_key() != Some(key) {
                if load.as_ref().is_some_and(|l| l.0 == key) {
                    return load;
                }
                let max = renderer.gpu().max_texture_dim();
                return Some((key, crate::background::load(path, max)));
            }
        }
    }
    None
}

fn slot_quad(s: &Slot) -> Quad {
    let (r, _) = card_axes(s.yaw);
    Quad {
        center: Vec3::from(s.pos),
        right: r * s.hw,
        up: Vec3::Y * s.hh,
    }
}

/// The nearest template slot on the ray (`o`, `d`).
fn slot_at(o: Vec3, d: Vec3, slots: &[Slot]) -> Option<usize> {
    slots
        .iter()
        .enumerate()
        .filter_map(|(k, s)| tools::ray_quad(o, d, &slot_quad(s), 1.0).map(|t| (k, t)))
        .min_by(|a, b| a.1.total_cmp(&b.1))
        .map(|(k, _)| k)
}

/// A 3x3 grid of slots about 4.5 m in front of the camera, facing it.
fn slot_grid(cam: &Camera) -> Vec<Slot> {
    let (hw, hh, gap) = (1.0f32, 0.6f32, 0.1f32);
    let (r, _) = card_axes(cam.yaw);
    let ahead = Vec3::new(cam.yaw.sin(), 0.0, -cam.yaw.cos());
    let (sx, sy) = (2.0 * hw + gap, 2.0 * hh + gap);
    let mut c = cam.pos + ahead * 4.5;
    // Keep the whole grid inside the room.
    let mx = (1.5 * sx + 0.05).min(ROOM_HALF - 0.1);
    c.x = c.x.clamp(-ROOM_HALF + mx, ROOM_HALF - mx);
    c.z = c.z.clamp(-ROOM_HALF + mx, ROOM_HALF - mx);
    c.y = c.y.clamp(1.5 * sy + 0.05, ROOM_HEIGHT - 1.5 * sy - 0.05);
    let mut out = Vec::with_capacity(9);
    for row in [1.0f32, 0.0, -1.0] {
        for col in [-1.0f32, 0.0, 1.0] {
            out.push(Slot {
                pos: (c + r * col * sx + Vec3::Y * row * sy).to_array(),
                yaw: cam.yaw,
                hw,
                hh,
                wf: String::new(),
            });
        }
    }
    out
}

fn card_axes(yaw: f32) -> (Vec3, Vec3) {
    (
        Vec3::new(yaw.cos(), 0.0, yaw.sin()),
        Vec3::new(-yaw.sin(), 0.0, yaw.cos()),
    )
}

fn clamp_in_room(p: Vec3, margin: f32) -> Vec3 {
    Vec3::new(
        p.x.clamp(-ROOM_HALF + margin, ROOM_HALF - margin),
        p.y.clamp(margin, ROOM_HEIGHT - margin),
        p.z.clamp(-ROOM_HALF + margin, ROOM_HALF - margin),
    )
}

fn crosshair() -> Image {
    let n = 32u32;
    let mut img = Image::new(n, n);
    for y in 0..n {
        for x in 0..n {
            let (dx, dy) = ((x as i32 - 16).abs(), (y as i32 - 16).abs());
            let arm = |a: i32, b: i32| a <= 1 && (4..=12).contains(&b);
            let core = arm(dx, dy) || arm(dy, dx);
            let outline =
                (dx <= 2 && (3..=13).contains(&dy)) || (dy <= 2 && (3..=13).contains(&dx));
            let o = ((y * n + x) * 4) as usize;
            if core {
                img.px[o..o + 4].copy_from_slice(&[255, 255, 255, 235]);
            } else if outline {
                img.px[o..o + 4].copy_from_slice(&[0, 0, 0, 150]);
            }
        }
    }
    img
}

impl Overlay {
    pub fn new(
        gpu: Option<Gpu>,
        hwnd: isize,
        hinstance: isize,
        w: u32,
        h: u32,
        cfg: &Config,
        cam: Option<CameraState>,
    ) -> Result<Self, String> {
        let mut renderer = Renderer::new(
            gpu,
            hwnd,
            hinstance,
            w,
            h,
            cfg.msaa,
            &cfg.gpu,
            cfg.keep_gpu_adapter,
        )?;
        let sky_load = apply_background(&mut renderer, &cfg.background, None);
        let scale = (h as f32 / 1080.0).clamp(0.75, 2.5);
        let px = (17.0 * scale) as i32;
        // HUD textures are cached like cards, under reserved ids, keyed by the text size.
        let mut next_id = HUD_IDS;
        let mut hud = |make: &dyn Fn() -> Image| {
            next_id += 1;
            let tex = match renderer.cached(next_id, px as u64) {
                Some(t) => t,
                None => renderer.upload_cached(next_id, px as u64, &make(), false),
            };
            let (w, h) = renderer.tex_size(tex);
            Hud {
                tex,
                w: w as f32,
                h: h as f32,
            }
        };
        let cross = hud(&crosshair);
        let help = hud(&|| {
            crate::win::compose_hud_text(
                &[
                    "WASD fly \u{00b7} mouse look \u{00b7} Space/E up \u{00b7} Q/C down \u{00b7} Shift faster \u{00b7} wheel dolly",
                    "Click a card to watch it \u{00b7} hold & drag to move it (wheel pushes/pulls) \u{00b7} Home recenters \u{00b7} / search \u{00b7} Esc options \u{00b7} Esc Esc closes",
                    "Tab frees the mouse \u{00b7} M next monitor \u{00b7} I stats \u{00b7} G template grid (drop cards in; Del removes a slot) \u{00b7} K open workflows",
                ],
                px,
            )
        });
        let hints = [
            hud(&|| {
                crate::win::compose_hud_text(
                    &[
                        "Click: use it here  \u{00b7}  Shift+click: switch  \u{00b7}  Drag: move  \u{00b7}  L: live view  \u{00b7}  F: fullscreen here",
                    ],
                    px,
                )
            }),
            hud(&|| {
                crate::win::compose_hud_text(
                    &[
                        "Closed app  \u{00b7}  Click: relaunch  \u{00b7}  Drag: move  \u{00b7}  Del: forget",
                    ],
                    px,
                )
            }),
            hud(&|| {
                crate::win::compose_hud_text(
                    &["Shortcut  \u{00b7}  Click: launch  \u{00b7}  Drag: move"],
                    px,
                )
            }),
            hud(&|| {
                crate::win::compose_hud_text(
                    &[
                        "Live  \u{00b7}  Shift+click: switch  \u{00b7}  F: fullscreen here  \u{00b7}  Click: use it here  \u{00b7}  L: stop live",
                    ],
                    px,
                )
            }),
            hud(&|| {
                crate::win::compose_hud_text(
                    &[
                        "Using the window: it has the keyboard, the mouse is passed on  \u{00b7}  Hotkey or click outside it: back to flying",
                    ],
                    px,
                )
            }),
            hud(&|| {
                crate::win::compose_hud_text(
                    &[
                        "Cursor free: move it to another monitor  \u{00b7}  Click here or Tab: fly again  \u{00b7}  M: next monitor",
                    ],
                    px,
                )
            }),
            hud(&|| {
                crate::win::compose_hud_text(
                    &[
                        "Live  \u{00b7}  Another window covers this one, so the browser stopped drawing it (black or frozen)",
                        "Fix: start it with --disable-features=CalculateNativeWinOcclusion (see README)",
                    ],
                    px,
                )
            }),
        ];
        let glyphs = [
            ('\u{E76F}', '\u{E76F}'),
            ('\u{ED1A}', '\u{E890}'),
            ('\u{E74F}', '\u{E767}'),
            ('\u{E740}', '\u{E740}'),
            ('\u{E7C2}', '\u{E7C2}'),
            ('\u{E71B}', '\u{E71B}'),
            ('\u{E7AD}', '\u{E7AD}'),
            ('\u{E711}', '\u{E711}'),
        ];
        let toggles = [Tool::Eye, Tool::Sound, Tool::Move, Tool::Snap];
        let off = glyphs.map(|(g, _)| hud(&|| crate::win::compose_icon(g, false)).tex);
        let mut on = off;
        for t in toggles {
            let g = glyphs[t as usize].1;
            on[t as usize] = hud(&|| crate::win::compose_icon(g, true)).tex;
        }
        let axis = [[235u8, 70, 70], [90, 210, 80], [70, 130, 245]].map(|[r, g, b]| {
            hud(&|| {
                let mut img = Image::new(8, 8);
                for p in img.px.chunks_exact_mut(4) {
                    p.copy_from_slice(&[b, g, r, 255]);
                }
                img
            })
            .tex
        });
        let icons = Icons { off, on, axis };
        let slot_tex = hud(&|| {
            // Light blue with a brighter frame.
            let n = 32u32;
            let mut img = Image::new(n, n);
            for y in 0..n {
                for x in 0..n {
                    let edge = x < 2 || y < 2 || x >= n - 2 || y >= n - 2;
                    let o = ((y * n + x) * 4) as usize;
                    let px = if edge {
                        [255, 240, 200, 255]
                    } else {
                        [250, 200, 120, 150]
                    };
                    img.px[o..o + 4].copy_from_slice(&px);
                }
            }
            img
        })
        .tex;
        let tip = |s: &'static str| move || crate::win::compose_hud_text(&[s], px);
        let tips = [
            hud(&tip(
                "Drag: move this toolbar  \u{00b7}  Click: put it back beside the card",
            )),
            hud(&tip("Live view on/off (L)")),
            hud(&tip(
                "Sound in the 3D view on/off (apps are muted here by default)",
            )),
            hud(&tip("Drag: resize the card  \u{00b7}  Click: default size")),
            hud(&tip(
                "Move: drag the red / green / blue arrows to move along one axis  \u{00b7}  Click: show/hide them",
            )),
            hud(&tip(
                "Snap: glue the card edge to edge to its neighbours while it moves",
            )),
            hud(&tip(
                "Drag: turn the card (around the shared edge when glued)  \u{00b7}  Click: face me",
            )),
            hud(&tip(
                "Close the window  \u{00b7}  Shift+click: end the app (unsaved work is lost)",
            )),
        ];
        let hud_slots = renderer.texture_count();
        let c = cam.unwrap_or(DEFAULT_CAMERA);
        let now = Instant::now();
        Ok(Self {
            renderer,
            sky_load,
            background: cfg.background.clone(),
            downloads_seen: crate::background::downloads_ended(),
            cards: Vec::new(),
            cam: Camera {
                pos: clamp_in_room(Vec3::from(c.pos), 0.3),
                yaw: c.yaw,
                pitch: c.pitch,
            },
            vel: Vec3::ZERO,
            hover: None,
            press: None,
            mouse: (0.0, 0.0),
            sensitivity: cfg.mouse_sensitivity,
            focus_apps: cfg.focus_apps.clone(),
            last_esc: None,
            shell_esc: None,
            esc_down: false,
            speed: cfg.move_speed,
            fov: cfg.fov_degrees.clamp(30.0, 120.0).to_radians(),
            start: now,
            last: now,
            drawn: (0, now),
            rendered: false,
            scene: 0,
            moving: true,
            scene_at: now,
            cross,
            help,
            hints,
            hud_slots,
            hwnd,
            live: None,
            live_broken: false,
            live_making: None,
            live_failed: HashSet::new(),
            frame_avg: 0.0,
            view_report: Instant::now(),
            interact: None,
            left: None,
            pending_interact: None,
            pending_traced: false,
            shell_hidden: None,
            theater: None,
            stats: None,
            free: false,
            fly: None,
            hit: Hit::Card,
            bar: None,
            gizmo: None,
            tool: None,
            icons,
            tips,
            vp: Mat4::IDENTITY,
            slot_hot: None,
            slot_tex,
            wf_sel: None,
            wf_hud: None,
            search: None,
            menu: None,
            menu_slot: None,
            occluded: (0, Instant::now(), false),
            shaped: Vec::new(),
            shape_check: Instant::now(),
            text_px: px,
        })
    }

    /// Whether the texture of card `id` with content `key` is cached.
    pub fn has_cached(&self, id: u64, key: u64) -> bool {
        self.renderer.gpu().has_cached(id, key)
    }

    /// Uploads card `id` with content `key` into the texture cache for the next open.
    pub fn preload(&mut self, id: u64, key: u64, img: &Image) {
        self.renderer.gpu_mut().preload(id, key, img);
    }

    /// Empties the scene but keeps the swapchain, so the hidden overlay can reopen at once.
    /// Cached textures of cards no longer in the scene are released.
    pub fn park(&mut self) {
        let ids: std::collections::HashSet<u64> = self.card_ids().collect();
        self.renderer
            .gpu_mut()
            .retain_cache(|id| id > HUD_IDS || ids.contains(&id));
        self.end_interact("overlay parked");
        if let Some(l) = &mut self.live {
            l.stop_all();
        }
        self.unshape();
        self.live_failed.clear();
        self.cards.clear();
        self.renderer.truncate_textures(self.hud_slots);
        (self.hover, self.press) = (None, None);
        (self.theater, self.fly) = (None, None);
        (self.bar, self.tool, self.gizmo) = (None, None, None);
        self.search = None;
        self.discard_menu();
        self.menu_slot = None;
        if let Some(st) = &mut self.stats {
            st.slot = None;
        }
        self.renderer.finish();
    }

    /// Prepares a parked overlay for a new open at size `w`x`h`.
    pub fn reopen(&mut self, w: u32, h: u32, cam: Option<CameraState>) {
        self.resize(w, h);
        self.free = false;
        let c = cam.unwrap_or(DEFAULT_CAMERA);
        self.cam = Camera {
            pos: clamp_in_room(Vec3::from(c.pos), 0.3),
            yaw: c.yaw,
            pitch: c.pitch,
        };
        self.vel = Vec3::ZERO;
        self.mouse = (0.0, 0.0);
        let now = Instant::now();
        (self.start, self.last) = (now, now);
        // Not in the frame drawn before showing: that one should be quick.
        self.shape_check = now + std::time::Duration::from_millis(300);
    }

    /// Adds a card textured with `img`, or with the cached texture for content `key` when `img`
    /// is `None`. Returns false if there is no such cached texture.
    pub fn add_card(
        &mut self,
        id: u64,
        kind: Kind,
        ghost: bool,
        hwnd: Option<isize>,
        key: u64,
        img: Option<&Image>,
    ) -> bool {
        let tex = match img {
            Some(img) => self.renderer.upload_cached(id, key, img, true),
            None => match self.renderer.cached(id, key) {
                Some(t) => t,
                None => return false,
            },
        };
        let (half_w, half_h) = self.card_half(kind, tex);
        self.cards.push(Card {
            id,
            kind,
            ghost,
            hwnd,
            half_w,
            half_h,
            tex,
        });
        true
    }

    /// Replaces the texture of card `id` (e.g. with a screenshot that arrived after opening).
    pub fn update_card(&mut self, id: u64, key: u64, img: &Image) {
        let Some(i) = self.cards.iter().position(|c| c.id == id) else {
            return;
        };
        let tex = self.renderer.upload_cached(id, key, img, true);
        let (half_w, half_h) = self.card_half(self.cards[i].kind, tex);
        let c = &mut self.cards[i];
        (c.tex, c.half_w, c.half_h) = (tex, half_w, half_h);
    }

    fn card_half(&self, kind: Kind, tex: usize) -> (f32, f32) {
        let (w, h) = self.renderer.tex_size(tex);
        let aspect = h as f32 / w.max(1) as f32;
        match kind {
            Kind::Shortcut => (0.475, 0.475 * aspect),
            Kind::Window => {
                let hw = 1.3f32;
                let hh = hw * aspect;
                if hh > 1.25 {
                    (1.25 / aspect, 1.25)
                } else {
                    (hw, hh)
                }
            }
        }
    }

    /// Ids of the cards in the scene.
    pub fn card_ids(&self) -> impl Iterator<Item = u64> + '_ {
        self.cards.iter().map(|c| c.id)
    }

    /// Gives back the regions set by `win::uncover_chromium`.
    pub fn unshape(&mut self) {
        crate::win::unshape(&std::mem::take(&mut self.shaped));
    }

    /// Closes the swapchain, keeping the device and texture cache for the next open. Cached
    /// textures of cards no longer in the scene are released.
    pub fn into_gpu(mut self) -> Gpu {
        self.end_interact("overlay closed");
        self.live = None;
        self.unshape();
        let ids: std::collections::HashSet<u64> = self.card_ids().collect();
        self.renderer
            .gpu_mut()
            .retain_cache(|id| id > HUD_IDS || ids.contains(&id));
        self.renderer.into_gpu()
    }

    pub fn camera_state(&self) -> CameraState {
        let c = match (&self.fly, &self.theater) {
            (_, Some(t)) => t.back,
            (Some(f), _) => f.to,
            _ => self.cam,
        };
        CameraState {
            pos: c.pos.to_array(),
            yaw: c.yaw,
            pitch: c.pitch,
        }
    }

    pub fn resize(&mut self, w: u32, h: u32) {
        self.renderer.resize(w, h);
    }

    pub fn renderer_mut(&mut self) -> &mut Renderer {
        &mut self.renderer
    }

    pub fn finish(&self) {
        self.renderer.finish();
    }

    pub fn set_free(&mut self, free: bool) {
        self.free = free;
        self.press = None;
        self.tool = None;
        self.mouse = (0.0, 0.0);
    }

    pub fn mouse_delta(&mut self, dx: i32, dy: i32) {
        if self.menu.is_some() {
            return;
        }
        if let Some(it) = &mut self.interact {
            it.raw.0 += dx as f32;
            it.raw.1 += dy as f32;
            return;
        }
        if self.free || self.interact.is_some() || self.theater.is_some() || self.fly.is_some() {
            return;
        }
        if let Some(td) = &mut self.tool {
            td.acc.0 += dx as f32;
            td.acc.1 += dy as f32;
            td.moved += ((dx * dx + dy * dy) as f32).sqrt();
            return;
        }
        self.mouse.0 += dx as f32;
        self.mouse.1 += dy as f32;
        if let Some(p) = &mut self.press {
            p.moved += ((dx * dx + dy * dy) as f32).sqrt();
            if p.moved > 6.0 {
                p.dragging = true;
            }
        }
    }

    pub fn wheel(&mut self, delta: i32) {
        if let Some(m) = &mut self.menu {
            m.wheel(delta);
            return;
        }
        if self.theater.is_some() || self.fly.is_some() || self.tool.is_some() {
            return;
        }
        let notches = delta as f32 / 120.0;
        match &mut self.press {
            Some(p) if p.dragging || p.moved > 0.0 => {
                p.dragging = true;
                p.dist = (p.dist * 1.12f32.powf(notches)).clamp(0.8, 30.0);
            }
            _ => {
                let f = self.cam.forward();
                self.cam.pos = clamp_in_room(self.cam.pos + f * notches * 1.2, 0.3);
            }
        }
    }

    pub fn button(&mut self, down: bool, model: &mut Model) -> Action {
        if self.menu.is_some() {
            return Action::None;
        }
        if down {
            let (o, d) = (self.cam.pos, self.cam.forward());
            match self.pick_hit(o, d, model) {
                Some((i, Hit::Card, t, local)) => {
                    self.press = Some(Press {
                        card: i,
                        dist: t,
                        local,
                        moved: 0.0,
                        dragging: false,
                    })
                }
                Some((i, hit, _, _)) => {
                    self.tool = Some(ToolDrag {
                        card: i,
                        hit,
                        moved: 0.0,
                        acc: (0.0, 0.0),
                        raw: None,
                    })
                }
                None => {}
            }
            return Action::None;
        }
        if let Some(td) = self.tool.take() {
            return match td.hit {
                _ if td.moved > 6.0 => Action::LayoutChanged,
                Hit::Tool(t) => self.tool_click(td.card, t, model),
                _ => Action::None,
            };
        }
        match self.press.take() {
            Some(p) if p.dragging => {
                let (o, d) = (self.cam.pos, self.cam.forward());
                if let Some(k) = slot_at(o, d, &model.layout.slots) {
                    self.fit_to_slot(p.card, k, model);
                }
                self.slot_hot = None;
                Action::LayoutChanged
            }
            Some(p) => self.activate(p.card, model, shift_down()),
            None => Action::None,
        }
    }

    /// Puts card `i` in template slot `k`: centred, facing the same way, scaled to fit inside.
    fn fit_to_slot(&mut self, i: usize, k: usize, model: &mut Model) {
        let Some(s) = model.layout.slots.get(k).cloned() else {
            return;
        };
        let Some(c) = self.cards.get_mut(i) else {
            return;
        };
        let Some(e) = model.get_mut(c.id) else {
            return;
        };
        let old = e.scale.max(1e-3);
        let (bw, bh) = (c.half_w / old, c.half_h / old);
        let scale = (s.hw / bw.max(1e-3))
            .min(s.hh / bh.max(1e-3))
            .clamp(tools::MIN_SCALE, tools::MAX_SCALE);
        e.scale = scale;
        (c.half_w, c.half_h) = (bw * scale, bh * scale);
        e.pos = s.pos;
        e.yaw = s.yaw;
    }

    /// A toolbar button clicked (pressed and released without dragging).
    fn tool_click(&mut self, i: usize, t: Tool, model: &mut Model) -> Action {
        let Some(id) = self.cards.get(i).map(|c| c.id) else {
            return Action::None;
        };
        let Some(e) = model.get_mut(id) else {
            return Action::None;
        };
        match t {
            Tool::Grip => {
                e.toolbar = None;
                Action::LayoutChanged
            }
            Tool::Eye => {
                let on = !e.live;
                self.live_failed.remove(&id);
                Action::SetLive(id, on)
            }
            Tool::Sound => Action::SetSound(id, !e.sound),
            Tool::Resize => {
                self.set_scale(i, 1.0, model);
                Action::LayoutChanged
            }
            Tool::Move => {
                self.gizmo = if self.gizmo == Some(id) {
                    None
                } else {
                    Some(id)
                };
                Action::None
            }
            Tool::Snap => {
                e.snap = !e.snap;
                self.apply_snap(i, model);
                Action::LayoutChanged
            }
            Tool::Rotate => {
                // Face the camera.
                let d = self.cam.pos - Vec3::from(e.pos);
                self.set_yaw(i, (-d.x).atan2(d.z), model);
                Action::LayoutChanged
            }
            Tool::Close => {
                Action::CloseWindow(id, unsafe { GetAsyncKeyState(VK_SHIFT.0 as i32) } < 0)
            }
        }
    }

    fn rect(&self, i: usize, model: &Model) -> Option<Rect3> {
        let c = self.cards.get(i)?;
        let e = model.get(c.id)?;
        Some(Rect3 {
            center: Vec3::from(e.pos),
            yaw: e.yaw,
            hw: c.half_w,
            hh: c.half_h,
        })
    }

    /// All cards but `i`, for snapping and toolbar placement.
    fn others(&self, i: usize, model: &Model) -> Vec<Rect3> {
        (0..self.cards.len())
            .filter(|&k| k != i)
            .filter_map(|k| self.rect(k, model))
            .collect()
    }

    fn tools_of(&self, i: usize) -> &'static [Tool] {
        let window = self
            .cards
            .get(i)
            .is_some_and(|c| c.kind == Kind::Window && !c.ghost && c.hwnd.is_some());
        if window {
            &Tool::ALL
        } else {
            &Tool::PLAIN
        }
    }

    /// The side of `me` facing the camera: its normal there and the matching right direction.
    fn facing(&self, me: &Rect3) -> (Vec3, Vec3) {
        if me.normal().dot(self.cam.pos - me.center) >= 0.0 {
            (me.normal(), me.right())
        } else {
            (-me.normal(), -me.right())
        }
    }

    /// The toolbar buttons of card `i`, just in front of it on the camera's side.
    fn bar_quads(&self, i: usize, model: &Model) -> Vec<(Tool, Quad)> {
        let Some(me) = self.rect(i, model) else {
            return Vec::new();
        };
        let manual = self.cards.get(i).and_then(|c| model.get(c.id)?.toolbar);
        let origin = tools::bar_origin(&me, &self.others(i, model), manual);
        let (n, fr) = self.facing(&me);
        tools::bar_buttons(origin, self.tools_of(i))
            .map(|(t, p)| {
                let q = Quad {
                    center: me.center + me.right() * p.x + Vec3::Y * p.y + n * 0.01,
                    right: fr * tools::BTN,
                    up: Vec3::Y * tools::BTN,
                };
                (t, q)
            })
            .collect()
    }

    /// Camera right and up directions.
    fn cam_axes(&self) -> (Vec3, Vec3) {
        let r = Vec3::new(self.cam.yaw.cos(), 0.0, self.cam.yaw.sin());
        (r, r.cross(self.cam.forward()))
    }

    /// The card showing the move gizmo and the gizmo's axes.
    #[allow(clippy::type_complexity)]
    fn gizmo_of(&self, model: &Model) -> Option<(usize, [(Vec3, Option<Quad>, Quad); 3])> {
        let id = self.gizmo?;
        let i = self.cards.iter().position(|c| c.id == id)?;
        let me = self.rect(i, model)?;
        let (r, u) = self.cam_axes();
        Some((i, tools::gizmo(&me, self.cam.pos, r, u)))
    }

    /// What the ray hits: a button of the shown toolbar, a move gizmo arrow or a card.
    /// Returns (card index, what, distance, hit point in card-local units).
    fn pick_hit(&self, o: Vec3, d: Vec3, model: &Model) -> Option<(usize, Hit, f32, (f32, f32))> {
        let mut best: Option<(usize, Hit, f32)> = None;
        let mut consider = |i: usize, hit: Hit, t: Option<f32>| {
            if let Some(t) = t.filter(|&t| best.is_none_or(|b| t < b.2)) {
                best = Some((i, hit, t));
            }
        };
        if let Some((b, _)) = self.bar {
            for (t, q) in self.bar_quads(b, model) {
                consider(b, Hit::Tool(t), tools::ray_quad(o, d, &q, 1.0));
            }
        }
        if let Some((g, axes)) = self.gizmo_of(model) {
            for (k, (_, shaft, knob)) in axes.iter().enumerate() {
                let on_shaft = shaft.as_ref().and_then(|s| tools::ray_quad(o, d, s, 3.0));
                consider(
                    g,
                    Hit::Axis(k),
                    on_shaft.or(tools::ray_quad(o, d, knob, 1.0)),
                );
            }
        }
        let card = self.pick(o, d, model);
        match (best, card) {
            // Buttons and arrows float just in front of the cards: they win unless a card is
            // clearly nearer.
            (Some(b), c) if c.is_none_or(|c| c.1 > b.2 - 0.05) => Some((b.0, b.1, b.2, (0.0, 0.0))),
            (_, c) => c.map(|(i, t, local)| (i, Hit::Card, t, local)),
        }
    }

    /// Pixel position of world point `p` in the last frame.
    fn to_screen(&self, p: Vec3) -> Option<Vec2> {
        let c = self.vp * p.extend(1.0);
        if c.w <= 1e-4 {
            return None;
        }
        let (sw, sh) = self.renderer.size();
        Some(Vec2::new(
            (c.x / c.w * 0.5 + 0.5) * sw as f32,
            (0.5 - c.y / c.w * 0.5) * sh as f32,
        ))
    }

    /// Metres per screen pixel at the distance of `p`.
    fn metres_per_px(&self, p: Vec3) -> f32 {
        let sh = self.renderer.size().1.max(1) as f32;
        p.distance(self.cam.pos) * 2.0 * (self.fov * 0.5).tan() / sh
    }

    /// Sets the size of card `i` (1 = default), keeping an edge glued to a neighbour in place.
    fn set_scale(&mut self, i: usize, scale: f32, model: &mut Model) {
        let Some(me) = self.rect(i, model) else {
            return;
        };
        let edge = tools::shared_edge(&me, &self.others(i, model));
        let Some(e) = model.get_mut(self.cards[i].id) else {
            return;
        };
        let scale = scale.clamp(tools::MIN_SCALE, tools::MAX_SCALE);
        let k = scale / e.scale.max(1e-3);
        e.scale = scale;
        let c = &mut self.cards[i];
        (c.half_w, c.half_h) = (c.half_w * k, c.half_h * k);
        e.pos = tools::keep_edge(&me, me.yaw, c.half_w, edge).to_array();
    }

    /// Turns card `i` to heading `yaw`, about an edge glued to a neighbour if there is one.
    fn set_yaw(&mut self, i: usize, yaw: f32, model: &mut Model) {
        let Some(me) = self.rect(i, model) else {
            return;
        };
        let edge = tools::shared_edge(&me, &self.others(i, model));
        let center = tools::keep_edge(&me, yaw, me.hw, edge);
        if let Some(e) = model.get_mut(self.cards[i].id) {
            e.yaw = yaw.rem_euclid(std::f32::consts::TAU);
            e.pos = center.to_array();
        }
    }

    /// Glues card `i` to the nearest neighbour edge, if it has snapping on and one is close.
    fn apply_snap(&mut self, i: usize, model: &mut Model) {
        let Some(me) = self.rect(i, model) else {
            return;
        };
        if !model.get(self.cards[i].id).is_some_and(|e| e.snap) {
            return;
        }
        if let Some((c, yaw)) = tools::snap(&me, &self.others(i, model)) {
            if let Some(e) = model.get_mut(self.cards[i].id) {
                (e.pos, e.yaw) = (c.to_array(), yaw);
            }
        }
    }

    /// Applies mouse movement (`dx`, `dy` pixels) to a held toolbar button or gizmo arrow of
    /// card `i`. Returns the new unsnapped centre for axis moves.
    fn apply_tool(
        &mut self,
        i: usize,
        hit: Hit,
        (dx, dy): (f32, f32),
        raw: Option<Vec3>,
        model: &mut Model,
    ) -> Option<Vec3> {
        let me = self.rect(i, model)?;
        let id = self.cards[i].id;
        match hit {
            Hit::Tool(Tool::Grip) => {
                let manual = model.get(id)?.toolbar;
                let origin = tools::bar_origin(&me, &self.others(i, model), manual);
                let (cr, cu) = self.cam_axes();
                let w = (cr * dx - cu * dy) * self.metres_per_px(me.center);
                let lim = Vec2::new(me.hw + 1.5, me.hh + 1.5);
                let local = (origin + Vec2::new(w.dot(me.right()), w.y)).clamp(-lim, lim);
                model.get_mut(id)?.toolbar = Some(local.to_array());
            }
            Hit::Tool(Tool::Resize) => {
                let s = model.get(id)?.scale;
                self.set_scale(i, s * ((dx - dy) * 0.004).exp(), model);
            }
            Hit::Tool(Tool::Rotate) => self.set_yaw(i, me.yaw + dx * 0.004, model),
            Hit::Axis(k) => {
                let (_, axes) = self.gizmo_of(model)?;
                let a = axes[k].0;
                let from = raw.unwrap_or(me.center);
                let screen = (
                    self.to_screen(me.center),
                    self.to_screen(me.center + a * tools::AXIS_LEN),
                );
                // Along the arrow as drawn on screen; vertical mouse moves when seen end-on.
                let along = match screen {
                    (Some(p0), Some(p1)) if (p1 - p0).length() > 24.0 => {
                        let s = p1 - p0;
                        Vec2::new(dx, dy).dot(s) / s.length_squared() * tools::AXIS_LEN
                    }
                    _ => -dy * self.metres_per_px(me.center),
                };
                let margin = (me.hw.max(me.hh) * 0.5).min(1.0);
                let to = clamp_in_room(from + a * along, margin);
                model.get_mut(id)?.pos = to.to_array();
                self.apply_snap(i, model);
                return Some(to);
            }
            _ => {}
        }
        None
    }

    /// Self test of the toolbar: aims at the resize button of the card under the crosshair,
    /// drags it, then puts card and camera back. Returns a report line.
    /// Adds a template grid, drops the first card into its centre slot, then undoes it all.
    /// Captures our own window `hwnd` live for a while: time to the first frame, frames read and
    /// the slowest poll (a poll must not wait for the GPU).
    /// With `restore`, the window starts minimized and is restored quietly after 0.3 s, as a
    /// click on a minimized window's card does.
    pub fn selftest_live(&mut self, hwnd: isize, restore: bool) -> String {
        let mut live = match Live::new(self.renderer.share()) {
            Ok(l) => l,
            Err(e) => return format!("live test: unavailable: {e}"),
        };
        const ID: u64 = u64::MAX - 7;
        let t0 = Instant::now();
        let cpu0 = crate::stats::cpu_time();
        live.start(ID, hwnd);
        let (mut first, mut frames, mut slowest, mut size) = (None, 0, 0.0f32, (0, 0));
        let mut to_restore = restore;
        while t0.elapsed().as_secs_f32() < 2.0 {
            crate::selftest::pump();
            if to_restore && t0.elapsed().as_secs_f32() > 0.3 {
                to_restore = false;
                crate::win::restore_quietly(HWND(hwnd as *mut core::ffi::c_void));
            }
            unsafe {
                let _ = windows::Win32::Graphics::Gdi::InvalidateRect(
                    Some(HWND(hwnd as *mut core::ffi::c_void)),
                    None,
                    true,
                );
            }
            let t = Instant::now();
            let got = live.poll(&mut self.renderer);
            slowest = slowest.max(t.elapsed().as_secs_f32() * 1000.0);
            if let Some(f) = got.get(&ID) {
                first.get_or_insert(t0.elapsed().as_millis());
                size = (f.w, f.h);
                frames += 1;
            }
            std::thread::sleep(std::time::Duration::from_millis(16));
        }
        let cpu_ms = (crate::stats::cpu_time() - cpu0) as f32 / 10_000.0;
        let failed = live.failed();
        let shared = live.is_shared();
        live.stop(ID, &mut self.renderer);
        let what = if restore { "live test (minimized, restored at 300 ms)" } else { "live test" };
        format!(
            "{what}: first frame {first:?} ms, polls with a frame {frames}, size {size:?}, slowest poll {slowest:.1} ms, GPU shared {shared}, CPU {:.2} ms per frame, failed {failed:?}",
            cpu_ms / frames.max(1) as f32
        )
    }

    /// Pictures of every animated scene (looking ahead and down at the ground, at a fixed
    /// time) and the time per frame; restores the background after.
    pub fn selftest_scenes(&mut self, dir: &std::path::Path) -> String {
        let (sw, sh) = self.renderer.size();
        let aspect = sw as f32 / sh.max(1) as f32;
        let proj = glam::camera::rh::proj::directx::perspective(self.fov, aspect, 0.05, 200.0);
        let o = Vec3::new(0.0, 1.7, 0.0);
        let mut out = Vec::new();
        for x in crate::background::SCENES {
            self.sky_load = apply_background(&mut self.renderer, &crate::background::scene_value(x), self.sky_load.take());
            let views = [(0.0f32, 0.12f32), (2.4, -0.3), (4.2, 0.6)];
            for (n, (yaw, pitch)) in views.iter().enumerate() {
                let f = Vec3::new(yaw.sin() * pitch.cos(), pitch.sin(), -yaw.cos() * pitch.cos());
                let vp = proj * glam::camera::rh::view::look_to_mat4(o, f, Vec3::Y);
                self.renderer.want_capture = true;
                self.renderer.render(vp, o, 40.0, &[], &[]);
                if let Some(mut img) = self.renderer.captured.take() {
                    while img.w > 1600 {
                        img = img.half();
                    }
                    let _ = crate::selftest::write_png(&dir.join(format!("scene_{}_{}.png", x.id, n + 1)), &img);
                }
            }
            // 20 frames, then one read back so the GPU has finished them.
            let vp = proj * glam::camera::rh::view::look_to_mat4(o, Vec3::new(0.0, 0.1, -1.0).normalize(), Vec3::Y);
            let t = std::time::Instant::now();
            for i in 0..21 {
                self.renderer.want_capture = i == 20;
                self.renderer.render(vp, o, 40.0 + i as f32 / 30.0, &[], &[]);
            }
            self.renderer.captured = None;
            out.push(format!("{} {:.1} ms", x.id, t.elapsed().as_secs_f32() * 1000.0 / 21.0));
        }
        self.sky_load = apply_background(&mut self.renderer, &self.background.clone(), self.sky_load.take());
        format!("scenes ({}x{}, per frame incl. readback): {}", sw, sh, out.join(", "))
    }

    /// Streams the desktop card as the card in use for two seconds: how long its polls take
    /// (it is put together from Progman's, the taskbar's and the shell popups' frames) and
    /// whether the desktop and the taskbar both show in it.
    pub fn selftest_desktop(&mut self) -> String {
        let Some(d) = crate::win::desktop_window() else {
            return "desktop test: no desktop".into();
        };
        let mut live = match Live::new(self.renderer.share()) {
            Ok(l) => l,
            Err(e) => return format!("desktop test: unavailable: {e}"),
        };
        const ID: u64 = u64::MAX - 9;
        live.start(ID, d.hwnd);
        live.priority = Some(ID);
        let t0 = Instant::now();
        let (mut first, mut polls, mut total, mut slowest, mut size) = (None, 0, 0.0f32, 0.0f32, (0, 0));
        while t0.elapsed().as_secs_f32() < 2.0 {
            crate::selftest::pump();
            let t = Instant::now();
            let got = live.poll(&mut self.renderer);
            let ms = t.elapsed().as_secs_f32() * 1000.0;
            if let Some(f) = got.get(&ID) {
                first.get_or_insert(t0.elapsed().as_millis());
                size = (f.w, f.h);
                // Past the first frame: the textures are made by then.
                if t0.elapsed().as_secs_f32() > 0.5 {
                    (polls, total, slowest) = (polls + 1, total + ms, slowest.max(ms));
                }
            }
            std::thread::sleep(std::time::Duration::from_millis(8));
        }
        let (w, h) = size;
        let desk = live.selftest_pixel(ID, w / 2, h / 3);
        let tray = crate::win::desktop_layout(d.hwnd)
            .filter(|l| l.tray.is_some())
            .and_then(|l| live.selftest_pixel(ID, w / 2, (l.tray_at.y + 4).max(0) as u32));
        let failed = live.failed();
        live.stop(ID, &mut self.renderer);
        format!(
            "desktop test: first frame {first:?} ms, size {size:?}, poll {:.2} ms average {slowest:.1} slowest, desktop pixel {desk:?}, taskbar pixel {tray:?}, failed {failed:?}",
            total / polls.max(1) as f32
        )
    }

    /// Uses window `hwnd` (its popup `pop` showing over it at (`x`, `y`) of its frame, black)
    /// in place for a while: the popup must be drawn on its card and hidden on the screen while
    /// in use, and given back after.
    pub fn selftest_popup(&mut self, hwnd: isize, pop: isize, x: u32, y: u32) -> String {
        let mut live = match Live::new(self.renderer.share()) {
            Ok(l) => l,
            Err(e) => return format!("popup test: unavailable: {e}"),
        };
        const ID: u64 = u64::MAX - 8;
        crate::wall::selftest_overlay(self.hwnd);
        let hidden = || {
            let p = HWND(pop as *mut core::ffi::c_void);
            let ex = unsafe { GetWindowLongW(p, GWL_EXSTYLE) } as u32;
            ex & WS_EX_LAYERED.0 != 0
        };
        live.start(ID, hwnd);
        live.priority = Some(ID);
        let t0 = Instant::now();
        let mut drawn = None;
        while t0.elapsed().as_secs_f32() < 2.0 && drawn.is_none() {
            crate::selftest::pump();
            let _ = live.poll(&mut self.renderer);
            let px = live.selftest_pixel(ID, x + 20, y + 20);
            if px.is_some_and(|p| p[0] < 160 && p[1] < 160 && p[2] < 160) {
                drawn = Some(t0.elapsed().as_millis());
            }
            std::thread::sleep(std::time::Duration::from_millis(16));
        }
        let beside = live.selftest_pixel(ID, x.saturating_sub(20), y.saturating_sub(20));
        let in_use_hidden = hidden();
        live.priority = None;
        for _ in 0..10 {
            crate::selftest::pump();
            let _ = live.poll(&mut self.renderer);
            std::thread::sleep(std::time::Duration::from_millis(16));
        }
        let cleared = live
            .selftest_pixel(ID, x + 20, y + 20)
            .map(|p| p[0] >= 160 || p[1] >= 160 || p[2] >= 160);
        crate::win::give_back_popups(true);
        let given_back = !hidden();
        let shared = live.is_shared();
        live.stop(ID, &mut self.renderer);
        format!(
            "popup test: drawn on the card after {drawn:?} ms, next to it {beside:?}, hidden in use {in_use_hidden}, gone from the card after use {cleared:?}, given back {given_back}, GPU shared {shared}"
        )
    }

    /// Where F puts the corners of a live frame on screen, for frames of a few shapes: the
    /// frame should fill the view to the pixel (no scaling of a screen-sized window) with its
    /// edges on whole pixels.
    pub fn selftest_theater(&mut self, model: &Model) -> String {
        let Some(i) = self.cards.iter().position(|c| c.kind == Kind::Window && model.get(c.id).is_some()) else {
            return "theater test: no window card".into();
        };
        let (sw, sh) = self.renderer.size();
        let (fw, fh) = (sw as f32, sh as f32);
        let keep = (self.cards[i].half_w, self.cards[i].half_h);
        let mut out = format!("theater test ({sw}x{sh} view):");
        for (w, h) in [(sw, sh), (sw, sh - 96), (sw, sh - 97), (sw - 41, sh), (1920, 1080)] {
            let frac = self.content_frac(&self.cards[i]);
            let c = &mut self.cards[i];
            c.half_h = c.half_w * h as f32 / w as f32 / frac;
            let Some(cam) = self.theater_camera(i, model) else { break };
            let (c, e) = (&self.cards[i], model.get(self.cards[i].id).unwrap());
            let f = self.content_frac(c);
            let (r, n) = card_axes(e.yaw);
            let mid = Vec3::from(e.pos) - Vec3::Y * c.half_h * (1.0 - f)
                + n * LIVE_NUDGE * n.dot(cam.pos - Vec3::from(e.pos)).signum();
            let proj = glam::camera::rh::proj::directx::perspective(self.fov, fw / fh, 0.05, 200.0);
            let vp = proj * glam::camera::rh::view::look_to_mat4(cam.pos, cam.forward(), Vec3::Y);
            let at = |p: Vec3| {
                let q = vp.project_point3(p);
                ((q.x + 1.0) * 0.5 * fw, (1.0 - q.y) * 0.5 * fh)
            };
            let (l, t) = at(mid - r * c.half_w + Vec3::Y * c.half_h * f);
            let (rr, b) = at(mid + r * c.half_w - Vec3::Y * c.half_h * f);
            let off = [l, t, rr, b].iter().map(|v| (v - v.round()).abs()).fold(0.0f32, f32::max);
            out += &format!(
                " {w}x{h} at ({l:.2}, {t:.2})-({rr:.2}, {b:.2}) off the pixels {off:.3};"
            );
        }
        let c = &mut self.cards[i];
        (c.half_w, c.half_h) = keep;
        out
    }

    /// F on window `hwnd` (about as big as the screen) live: each pixel of the view must show
    /// exactly the texel of the live frame under it.
    pub fn selftest_sharp(&mut self, model: &Model, hwnd: isize) -> String {
        let Some(i) = self.cards.iter().position(|c| c.kind == Kind::Window && model.get(c.id).is_some()) else {
            return "sharp test: no window card".into();
        };
        let mut live = match Live::new(self.renderer.share()) {
            Ok(l) => l,
            Err(e) => return format!("sharp test: unavailable: {e}"),
        };
        const ID: u64 = u64::MAX - 9;
        live.start(ID, hwnd);
        let t0 = Instant::now();
        let mut frame = None;
        while t0.elapsed().as_secs_f32() < 2.0 {
            crate::selftest::pump();
            if let Some(f) = live.poll(&mut self.renderer).remove(&ID) {
                frame = Some(f);
            }
            std::thread::sleep(std::time::Duration::from_millis(16));
        }
        let Some(fr) = frame else {
            live.stop(ID, &mut self.renderer);
            return "sharp test: no frame".into();
        };
        let keep = (self.cards[i].half_w, self.cards[i].half_h);
        let frac = self.content_frac(&self.cards[i]);
        // The card starts in the shape of a screenshot of this window (its height rounded to whole
        // pixels at the default screenshot size) and is fitted to the frame as in the app.
        let (c, k) = (&self.cards[i], model.get(self.cards[i].id).map_or(1.0, |e| e.scale));
        let (bw, _) = self.card_half(c.kind, c.tex);
        let s = (crate::store::Config::default().screenshot_size as f32 / fr.w.max(fr.h) as f32).min(1.0);
        let shot = (fr.w as f32 * s).round() / (fr.h as f32 * s).round();
        (self.cards[i].half_w, self.cards[i].half_h) = (bw * k, bw * k / shot / frac);
        let frames = HashMap::from([(self.cards[i].id, live::Frame { slot: fr.slot, w: fr.w, h: fr.h })]);
        self.fit_live_cards(&frames, model);
        let c = &self.cards[i];
        let stretch = c.half_h * frac / (c.half_w * fr.h as f32 / fr.w as f32) - 1.0;
        let (sw, sh) = self.renderer.size();
        let (fw, fh) = (sw as f32, sh as f32);
        let mut out = format!(
            "sharp test ({sw}x{sh} view, {}x{} frame, {}x msaa, card stretch {:.5}%):",
            fr.w,
            fr.h,
            self.renderer.samples(),
            stretch * 100.0
        );
        if let Some(cam) = self.theater_camera(i, model) {
            let (c, e) = (&self.cards[i], model.get(self.cards[i].id).unwrap());
            let (r, n) = card_axes(e.yaw);
            let center = Vec3::from(e.pos);
            let nudge = n * LIVE_NUDGE * n.dot(cam.pos - center).signum();
            let draw = CardDraw {
                center: center + Vec3::Y * c.half_h * (frac - 1.0) + nudge,
                right: r * c.half_w,
                up: Vec3::Y * c.half_h * frac,
                alpha: 1.0,
                ghost: 0.0,
                highlight: -1.0,
                tex: fr.slot,
            };
            let proj = glam::camera::rh::proj::directx::perspective(self.fov, fw / fh, 0.05, 200.0);
            let vp = proj * glam::camera::rh::view::look_to_mat4(cam.pos, cam.forward(), Vec3::Y);
            let at = |p: Vec3| {
                let q = vp.project_point3(p);
                ((q.x + 1.0) * 0.5 * fw, (1.0 - q.y) * 0.5 * fh)
            };
            let (l, t) = at(draw.center - draw.right + draw.up);
            let (rr, b) = at(draw.center + draw.right - draw.up);
            out += &format!(" drawn at ({l:.2}, {t:.2})-({rr:.2}, {b:.2});");
            self.renderer.want_capture = true;
            self.renderer.render(vp, cam.pos, 0.0, &[draw], &[]);
            let shot = self.renderer.captured.take();
            match (shot, live.selftest_frame(ID)) {
                (Some(shot), Some(src)) => {
                    let (l, t) = (l.round() as i64, t.round() as i64);
                    // Every pixel, edges included.
                    let (mut n, mut same, mut sum, mut max, mut busy) = (0u64, 0u64, 0u64, 0u8, 0u64);
                    for y in 0..fr.h {
                        for x in 0..fr.w {
                            let (px, py) = (x as i64 + l, y as i64 + t);
                            if px < 0 || py < 0 || px >= shot.w as i64 || py >= shot.h as i64 {
                                continue;
                            }
                            let a = &shot.px[(py as usize * shot.w as usize + px as usize) * 4..][..3];
                            let s = &src.px[(y as usize * src.w as usize + x as usize) * 4..][..3];
                            let d = a.iter().zip(s).map(|(a, s)| a.abs_diff(*s)).max().unwrap_or(0);
                            n += 1;
                            same += (d == 0) as u64;
                            sum += d as u64;
                            max = max.max(d);
                            busy += (s[1] < 200) as u64;
                        }
                    }
                    out += &format!(
                        " {n} pixels, {:.2}% exact, mean diff {:.3}, max diff {max}, {:.1}% of the frame dark (text)",
                        same as f64 * 100.0 / n.max(1) as f64,
                        sum as f64 / n.max(1) as f64,
                        busy as f64 * 100.0 / n.max(1) as f64,
                    );
                }
                (a, b) => out += &format!(" no readback (view {}, frame {})", a.is_some(), b.is_some()),
            }
        }
        (self.cards[i].half_w, self.cards[i].half_h) = keep;
        live.stop(ID, &mut self.renderer);
        out
    }

    pub fn selftest_slots(&mut self, model: &mut Model) -> String {
        let Some(id) = self.cards.first().map(|c| c.id) else {
            return "slot test: no cards".into();
        };
        let Some(before) = model.get(id).map(|e| (e.pos, e.yaw, e.scale)) else {
            return "slot test: no entry".into();
        };
        let (hw0, hh0) = (self.cards[0].half_w, self.cards[0].half_h);
        let n0 = model.layout.slots.len();
        let added = matches!(self.key(VK_G.0, model), Action::LayoutChanged);
        let n = model.layout.slots.len() - n0;
        let (o, f) = (self.cam.pos, self.cam.forward());
        let aimed = slot_at(o, f, &model.layout.slots);
        let centre = n0 + 4;
        if let Some(k) = aimed {
            self.fit_to_slot(0, k, model);
        }
        let s = model.layout.slots.get(centre).cloned();
        let c = &self.cards[0];
        let fits = s.is_some_and(|s| {
            model
                .get(id)
                .is_some_and(|e| Vec3::from(e.pos).distance(Vec3::from(s.pos)) < 1e-3)
                && c.half_w <= s.hw + 1e-3
                && c.half_h <= s.hh + 1e-3
        });
        let fitted = (c.half_w, c.half_h);
        model.layout.slots.truncate(n0);
        if let Some(e) = model.get_mut(id) {
            (e.pos, e.yaw, e.scale) = before;
        }
        (self.cards[0].half_w, self.cards[0].half_h) = (hw0, hh0);
        format!(
            "slot test: added={added} slots={n} aimed-at-centre={} fitted={fits} half {:.2}x{:.2} -> {:.2}x{:.2}",
            aimed == Some(centre),
            hw0,
            hh0,
            fitted.0,
            fitted.1
        )
    }

    pub fn selftest_tools(&mut self, model: &mut Model) -> String {
        let (o, f) = (self.cam.pos, self.cam.forward());
        let Some((i, _, _)) = self.pick(o, f, model) else {
            return "toolbar test: no card in view".into();
        };
        let id = self.cards[i].id;
        let Some(before) = model.get(id).map(|e| (e.pos, e.yaw, e.scale)) else {
            return "toolbar test: no entry".into();
        };
        let cam = self.cam;
        self.bar = Some((i, Instant::now()));
        let Some((_, q)) = self
            .bar_quads(i, model)
            .into_iter()
            .find(|b| b.0 == Tool::Resize)
        else {
            return "toolbar test: no resize button".into();
        };
        let d = (q.center - self.cam.pos).normalize();
        (self.cam.yaw, self.cam.pitch) = (d.x.atan2(-d.z), d.y.asin());
        self.frame(model, false);
        let aimed = self.hit == Hit::Tool(Tool::Resize);
        let pressed = matches!(self.button(true, model), Action::None) && self.tool.is_some();
        self.mouse_delta(200, 0);
        self.frame(model, false);
        let changed = matches!(self.button(false, model), Action::LayoutChanged);
        let scale = model.get(id).map_or(0.0, |e| e.scale);
        let half_w = self.cards[i].half_w;
        if let Some(e) = model.get_mut(id) {
            (e.pos, e.yaw, e.scale) = before;
        }
        self.cam = cam;
        self.frame(model, false);
        format!(
            "toolbar test: aimed-at-resize={aimed} pressed={pressed} layout-changed={changed} scale {:.2} -> {scale:.2} (half width {half_w:.2})",
            before.2
        )
    }

    /// Click (or `Enter`) on a card: a window card is watched and used in place (live view and
    /// sound on, input passed on), or with `focus` its real window is focused; others launch.
    fn activate(&mut self, i: usize, model: &Model, focus: bool) -> Action {
        self.search = None;
        let Some(c) = self.cards.get(i) else {
            return Action::None;
        };
        match (c.kind, c.hwnd) {
            (Kind::Window, Some(h)) if !c.ghost && focus => Action::Focus(h),
            (Kind::Window, Some(h))
                if !c.ghost && {
                    let info = crate::win::win_info(HWND(h as *mut core::ffi::c_void));
                    let real = crate::win::is_focus_app(&info, &self.focus_apps);
                    crate::store::log(&format!(
                        "card click: class {:?}, exe {:?}{}",
                        info.class,
                        info.exe,
                        if real {
                            ": in focus_apps, focusing the real window"
                        } else {
                            ""
                        }
                    ));
                    real
                } =>
            {
                Action::Focus(h)
            }
            (Kind::Window, Some(h)) if !c.ghost => {
                let id = c.id;
                if crate::win::restore_quietly(HWND(h as *mut core::ffi::c_void)) {
                    crate::store::log("card click: the window was minimized, restored behind the overlay");
                }
                self.live_failed.remove(&id);
                // Used in place once its live view shows a frame (see `frame`).
                self.pending_interact = Some((id, Instant::now()));
                self.pending_traced = false;
                Action::Watch(id)
            }
            _ => model
                .get(c.id)
                .map(|e| Action::Launch(e.exe.clone()))
                .unwrap_or(Action::None),
        }
    }

    /// Opens the options menu on `settings`, with renderer choices `gpus` (graphics card names).
    pub fn open_menu(&mut self, settings: MenuSettings, gpus: Vec<String>) {
        let mut choices: Vec<String> = ["default", "low-power", "high-performance"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        choices.extend(gpus);
        choices.push("software".into());
        if !choices.contains(&settings.gpu) {
            choices.insert(0, settings.gpu.clone());
        }
        self.press = None;
        self.vel = Vec3::ZERO;
        let size = self.renderer.size();
        self.menu = Some(Menu::new(settings, choices, self.text_px, size));
    }

    /// The options menu is open.
    pub fn menu_open(&self) -> bool {
        self.menu.is_some()
    }

    /// Field of view, mouse sensitivity and move speed, applied at once.
    pub fn set_view(&mut self, fov_degrees: f32, sensitivity: f32, speed: f32) {
        self.fov = fov_degrees.clamp(30.0, 120.0).to_radians();
        self.sensitivity = sensitivity;
        self.speed = speed;
    }

    /// A key while the options menu is open. Esc Esc still closes the 3D view.
    fn menu_key(&mut self, vk: u16) -> Action {
        let Some(m) = &mut self.menu else {
            return Action::None;
        };
        match VIRTUAL_KEY(vk) {
            VK_UP | VK_W => m.move_sel(true),
            VK_DOWN | VK_S => m.move_sel(false),
            VK_LEFT | VK_A => m.step(false),
            VK_RIGHT | VK_D => m.step(true),
            VK_RETURN | VK_SPACE if m.sel == crate::menu::DONE_ROW => return self.close_menu(),
            VK_RETURN | VK_SPACE if m.sel == crate::menu::CLOSE_ROW => {
                self.discard_menu();
                return Action::Close;
            }
            VK_RETURN | VK_SPACE => m.step(true),
            VK_ESCAPE if self.double_esc() => {
                self.discard_menu();
                return Action::Close;
            }
            VK_ESCAPE => return self.close_menu(),
            _ => {}
        }
        // The background shows at once, behind the menu.
        if let Some(m) = &self.menu {
            let load = self.sky_load.take();
            self.sky_load = apply_background(&mut self.renderer, &m.now.background, load);
        }
        Action::None
    }

    /// The mouse at `(x, y)` (overlay pixels) while the options menu is open, clicked or not.
    pub fn menu_pointer(&mut self, x: i32, y: i32, click: bool) -> Action {
        let (sw, sh) = self.renderer.size();
        let (w, h) = self.menu_slot.map_or((0, 0), |s| s.1);
        let Some(m) = &mut self.menu else {
            return Action::None;
        };
        let (px, py) = (x - (sw as i32 - w as i32) / 2, y - (sh as i32 - h as i32) / 2);
        match m.pointer(px, py, click) {
            Click::Nothing => return Action::None,
            Click::Done => return self.close_menu(),
            Click::Close => {
                self.discard_menu();
                return Action::Close;
            }
            Click::Changed => {}
        }
        // The background shows at once, behind the menu.
        if let Some(m) = &self.menu {
            let load = self.sky_load.take();
            self.sky_load = apply_background(&mut self.renderer, &m.now.background, load);
        }
        Action::None
    }

    /// The options menu as last drawn (for the selftest).
    pub fn menu_image(&mut self) -> Option<Image> {
        self.menu.as_mut().map(|m| m.compose())
    }

    /// Uploads the background picture once it is decoded.
    fn poll_sky(&mut self) {
        let Some((_, rx)) = &self.sky_load else { return };
        let r = match rx.try_recv() {
            Ok(r) => r,
            Err(std::sync::mpsc::TryRecvError::Empty) => return,
            Err(_) => Err("decoder stopped".into()),
        };
        self.sky_load = None;
        match r {
            Ok(l) => {
                let (w, h) = (l.levels[0].w, l.levels[0].h);
                crate::store::log(&format!(
                    "background: {}x{} (file {}x{}) pano={} ms={}",
                    w, h, l.source.0, l.source.1, l.pano, l.ms
                ));
                self.renderer.gpu_mut().set_sky(&l);
            }
            Err(e) => {
                crate::store::log(&format!("background: {e}"));
                self.renderer.set_background(Backdrop::Plain(FALLBACK_SKY));
            }
        }
    }

    /// Closes the options menu without keeping its changes (the 3D view is closing).
    fn discard_menu(&mut self) {
        if let Some(m) = self.menu.take() {
            let load = self.sky_load.take();
            self.sky_load = apply_background(&mut self.renderer, &m.start.background, load);
            self.background = m.start.background;
        }
    }

    /// Closes the options menu, handing on the settings if they changed.
    fn close_menu(&mut self) -> Action {
        let m = self.menu.take();
        if let Some(m) = &m {
            self.background = m.now.background.clone();
        }
        match m {
            Some(m) if m.now != m.start => Action::ApplySettings(m.now),
            _ => Action::None,
        }
    }

    /// An `Esc` press: true when it is the second within `DOUBLE_ESC`.
    fn double_esc(&mut self) -> bool {
        let now = Instant::now();
        if self.last_esc.is_some_and(|t| now - t <= DOUBLE_ESC) {
            self.last_esc = None;
            true
        } else {
            self.last_esc = Some(now);
            false
        }
    }

    /// Whether type-to-search is open (keys are text then).
    pub fn searching(&self) -> bool {
        self.search.is_some()
    }

    /// Search hits, the selected card id and whether the camera is flying (for the selftest).
    pub fn search_state(&self) -> Option<(usize, Option<u64>, bool)> {
        let s = self.search.as_ref()?;
        let sel = s.hits.get(s.sel).and_then(|&i| self.cards.get(i)).map(|c| c.id);
        Some((s.hits.len(), sel, self.fly.is_some()))
    }

    /// A typed character (`WM_CHAR`): `/` starts the search, then text goes into it.
    pub fn char_typed(&mut self, c: u16, model: &Model) {
        let Some(ch) = char::from_u32(c as u32).filter(|ch| !ch.is_control()) else {
            return;
        };
        if self.menu.is_some() {
            return;
        }
        match &mut self.search {
            Some(s) => s.query.push(ch),
            None if ch == '/' && self.interact.is_none() && self.theater.is_none() => {
                let back = self.fly.as_ref().map_or(self.cam, |f| f.to);
                self.search = Some(Search {
                    query: String::new(),
                    hits: Vec::new(),
                    sel: 0,
                    back,
                    hud: None,
                });
                (self.press, self.gizmo) = (None, None);
                return;
            }
            None => return,
        }
        self.update_search(model);
    }

    /// Keys while searching: `Esc` cancels, `Backspace` edits, `Tab`/arrows pick, `Enter` opens.
    fn search_key(&mut self, vk: u16, model: &Model) -> Action {
        let Some(s) = &mut self.search else {
            return Action::None;
        };
        let n = s.hits.len();
        let step = match VIRTUAL_KEY(vk) {
            VK_ESCAPE => {
                let back = s.back;
                self.search = None;
                self.fly_to(back);
                return Action::None;
            }
            VK_BACK => {
                s.query.pop();
                self.update_search(model);
                return Action::None;
            }
            VK_RETURN => {
                return match s.hits.get(s.sel).copied() {
                    Some(i) => self.activate(i, model, shift_down()),
                    None => Action::None,
                };
            }
            VK_TAB if shift_down() => n.saturating_sub(1),
            VK_TAB | VK_DOWN | VK_RIGHT => 1,
            VK_UP | VK_LEFT => n.saturating_sub(1),
            _ => return Action::None,
        };
        if n > 1 {
            s.sel = (s.sel + step) % n;
            let i = s.hits[s.sel];
            self.fly_to_hit(i, model);
        }
        Action::None
    }

    /// Ranks the cards against the query and flies to the best one.
    fn update_search(&mut self, model: &Model) {
        let Some(s) = &self.search else { return };
        let q = s.query.trim().to_lowercase();
        let from = s.back.pos;
        let mut hits: Vec<(u8, bool, f32, usize)> = Vec::new();
        if !q.is_empty() {
            for (i, c) in self.cards.iter().enumerate() {
                let Some(e) = model.get(c.id) else { continue };
                if let Some(r) = search_rank(&q, &e.title, &e.exe) {
                    let d = Vec3::from(e.pos).distance_squared(from);
                    hits.push((r, c.ghost, d, i));
                }
            }
        }
        hits.sort_by(|a, b| (a.0, a.1).cmp(&(b.0, b.1)).then(a.2.total_cmp(&b.2)));
        let hits: Vec<usize> = hits.into_iter().map(|h| h.3).collect();
        let first = hits.first().copied();
        if let Some(s) = &mut self.search {
            (s.hits, s.sel) = (hits, 0);
        }
        if let Some(i) = first {
            self.fly_to_hit(i, model);
        }
    }

    /// Flies to where card `i` is seen whole, a bit back from filling the view.
    fn fly_to_hit(&mut self, i: usize, model: &Model) {
        let (Some(mut to), Some(e)) = (
            self.theater_camera(i, model),
            self.cards.get(i).and_then(|c| model.get(c.id)),
        ) else {
            return;
        };
        let center = Vec3::from(e.pos);
        to.pos = center + (to.pos - center) * 1.6;
        self.fly_to(to);
    }

    fn fly_to(&mut self, to: Camera) {
        self.fly = Some(Fly {
            from: self.cam,
            to,
            t0: Instant::now(),
        });
        self.vel = Vec3::ZERO;
    }

    pub fn key(&mut self, vk: u16, model: &mut Model) -> Action {
        if self.search.is_some() {
            return self.search_key(vk, model);
        }
        if self.menu.is_some() {
            return self.menu_key(vk);
        }
        match VIRTUAL_KEY(vk) {
            VK_ESCAPE if self.theater.is_some() => {
                self.leave_theater();
                Action::None
            }
            VK_ESCAPE if self.gizmo.is_some() => {
                self.gizmo = None;
                Action::None
            }
            VK_ESCAPE if self.double_esc() => Action::Close,
            VK_ESCAPE if self.interact.is_none() => Action::OpenMenu,
            VK_ESCAPE => Action::None,
            VK_F if self.theater.is_some() => {
                self.leave_theater();
                Action::None
            }
            VK_I => {
                self.stats = match self.stats.take() {
                    Some(st) => {
                        if let Some(slot) = st.slot {
                            self.renderer.live_slot(Some(slot), 1, 1);
                        }
                        None
                    }
                    None => Some(Stats {
                        slot: None,
                        size: (0, 0),
                        next: Instant::now(),
                        own: None,
                        app: None,
                        frame_ms: 0.0,
                    }),
                };
                Action::None
            }
            VK_F => match self.hover {
                Some(i) => self.enter_theater(i, model, shift_down()),
                None => Action::None,
            },
            VK_HOME => {
                self.cam = Camera {
                    pos: Vec3::from(DEFAULT_CAMERA.pos),
                    yaw: 0.0,
                    pitch: 0.0,
                };
                self.vel = Vec3::ZERO;
                Action::None
            }
            VK_RETURN => match self.hover {
                Some(i) => self.activate(i, model, true),
                None => Action::None,
            },
            VK_DELETE if self.hover.is_none() => {
                let (o, d) = (self.cam.pos, self.cam.forward());
                match slot_at(o, d, &model.layout.slots) {
                    Some(k) => {
                        model.layout.slots.remove(k);
                        Action::LayoutChanged
                    }
                    None => Action::None,
                }
            }
            VK_DELETE => match self.hover.and_then(|i| self.cards.get(i)) {
                Some(c) if c.ghost => Action::Forget(c.id),
                _ => Action::None,
            },
            VK_K => {
                let n = model.layout.workflows.len();
                self.wf_sel = match self.wf_sel {
                    None if n > 0 => Some(0),
                    Some(i) if i + 1 < n => Some(i + 1),
                    _ => None,
                };
                Action::None
            }
            VK_X | VK_N | VK_P
                if self
                    .wf_sel
                    .is_some_and(|i| i < model.layout.workflows.len()) =>
            {
                let w = model.layout.workflows[self.wf_sel.unwrap_or(0)].clone();
                match VIRTUAL_KEY(vk) {
                    VK_X => {
                        crate::workflow::close(model, &w.name);
                        self.wf_sel = None;
                        Action::LayoutChanged
                    }
                    VK_N => {
                        for id in &w.ids {
                            if let Some(e) = model.get_mut(*id) {
                                e.sound = false;
                            }
                        }
                        Action::AudioChanged
                    }
                    _ => {
                        let windows: Vec<u64> = w
                            .ids
                            .iter()
                            .copied()
                            .filter(|id| {
                                self.cards
                                    .iter()
                                    .any(|c| c.id == *id && c.kind == Kind::Window && !c.ghost)
                            })
                            .collect();
                        let on = !windows
                            .iter()
                            .all(|id| model.get(*id).is_some_and(|e| e.live));
                        for id in windows {
                            if let Some(e) = model.get_mut(id) {
                                e.live = on;
                            }
                            self.live_failed.remove(&id);
                        }
                        Action::LayoutChanged
                    }
                }
            }
            VK_T if self.interact.is_none() && self.theater.is_none() && shift_down() => {
                // Open windows first, most recently used first; ghosts after them.
                let mut ids: Vec<(bool, u64, u64)> = self
                    .cards
                    .iter()
                    .filter(|c| c.kind == Kind::Window)
                    .filter_map(|c| model.get(c.id).map(|e| (c, e)))
                    .filter(|(_, e)| e.class != crate::model::DESKTOP_CLASS)
                    .map(|(c, e)| (c.ghost, u64::MAX - e.last_seen, c.id))
                    .collect();
                ids.sort();
                let ids: Vec<u64> = ids.into_iter().map(|t| t.2).collect();
                model.tidy_windows(&ids);
                Action::LayoutChanged
            }
            VK_T if self.interact.is_none() && self.theater.is_none() => {
                model.tidy_wall();
                Action::LayoutChanged
            }
            VK_OEM_4 | VK_OEM_6 if self.interact.is_none() && self.theater.is_none() => {
                model.wall_columns(if VIRTUAL_KEY(vk) == VK_OEM_4 { -1 } else { 1 });
                Action::LayoutChanged
            }
            VK_G if self.interact.is_none() && self.theater.is_none() => {
                model.layout.slots.extend(slot_grid(&self.cam));
                Action::LayoutChanged
            }
            VK_L => match self.hover.and_then(|i| self.cards.get(i)) {
                Some(c) if c.kind == Kind::Window && !c.ghost => {
                    let id = c.id;
                    self.live_failed.remove(&id);
                    match model.get(id) {
                        Some(e) => Action::SetLive(id, !e.live),
                        None => Action::None,
                    }
                }
                _ => Action::None,
            },
            _ => Action::None,
        }
    }

    /// Removes a card from the scene (its GPU texture is released when the overlay closes).
    pub fn remove_card(&mut self, id: u64) {
        let Some(i) = self.cards.iter().position(|c| c.id == id) else {
            return;
        };
        // Using another card in place goes on (the indices after this card shift down).
        if self.interact.as_ref().is_some_and(|it| it.card == i)
            || self.theater.as_ref().is_some_and(|t| t.card == i)
            || self.pending_interact.is_some_and(|(p, _)| p == id)
        {
            self.end_interact("card removed");
        }
        if let Some(it) = &mut self.interact {
            it.card -= (it.card > i) as usize;
        }
        if let Some(t) = &mut self.theater {
            t.card -= (t.card > i) as usize;
        }
        if let Some(s) = &mut self.search {
            s.hits.retain(|&h| h != i);
            for h in &mut s.hits {
                *h -= (*h > i) as usize;
            }
            s.sel = s.sel.min(s.hits.len().saturating_sub(1));
        }
        if let Some(l) = &mut self.live {
            l.stop(id, &mut self.renderer);
        }
        self.cards.retain(|c| c.id != id);
        self.hover = None;
        self.press = None;
        (self.bar, self.tool) = (None, None);
    }

    /// Ray vs. card rectangles; returns (card index, distance, hit point in card-local units).
    fn pick(&self, o: Vec3, d: Vec3, model: &Model) -> Option<(usize, f32, (f32, f32))> {
        let mut best: Option<(usize, f32, (f32, f32))> = None;
        for (i, c) in self.cards.iter().enumerate() {
            let Some(e) = model.get(c.id) else { continue };
            let center = Vec3::from(e.pos);
            let (r, n) = card_axes(e.yaw);
            let denom = n.dot(d);
            if denom.abs() < 1e-5 {
                continue;
            }
            let t = n.dot(center - o) / denom;
            if t <= 0.05 || best.is_some_and(|b| t >= b.1) {
                continue;
            }
            let q = o + d * t - center;
            let (x, y) = (q.dot(r), q.y);
            if x.abs() <= c.half_w && y.abs() <= c.half_h {
                best = Some((i, t, (x, y)));
            }
        }
        best
    }

    /// Fraction of a window card's height taken by the screenshot (below the title strip).
    fn content_frac(&self, c: &Card) -> f32 {
        let (tw, th) = self.renderer.tex_size(c.tex);
        let strip = crate::win::card_strip(tw as i32) as f32;
        ((th as f32 - strip) / th.max(1) as f32).clamp(0.05, 1.0)
    }

    /// Whether card `c` at `pos` may be on screen (with a margin, so turning back to it finds the
    /// stream still running). Live cards out of view stop streaming; their app keeps its sound.
    fn in_view(&self, c: &Card, pos: [f32; 3]) -> bool {
        let to = Vec3::from(pos) - self.cam.pos;
        let dist = to.length();
        let r = c.half_w.max(c.half_h);
        if dist <= r {
            return true;
        }
        let (sw, sh) = self.renderer.size();
        let t = (self.fov * 0.5).tan();
        let half_diag = (t * (1.0 + (sw as f32 / sh.max(1) as f32).powi(2)).sqrt()).atan();
        let angle = (to / dist).dot(self.cam.forward()).clamp(-1.0, 1.0).acos();
        angle < half_diag + (r / dist).atan() + 0.35
    }

    /// Whether any of the card is on screen now (no margin, unlike `in_view`): the card's
    /// bounding sphere against the four sides of the view.
    fn on_screen(&self, c: &Card, pos: [f32; 3]) -> bool {
        let to = Vec3::from(pos) - self.cam.pos;
        let r = c.half_w.hypot(c.half_h);
        let f = self.cam.forward();
        let right = f.cross(Vec3::Y).normalize_or_zero();
        let up = right.cross(f);
        let (x, y, z) = (to.dot(right), to.dot(up), to.dot(f));
        let (sw, sh) = self.renderer.size();
        let tv = (self.fov * 0.5).tan();
        let th = tv * sw as f32 / sh.max(1) as f32;
        // Distance from the side plane through the eye whose slope is `t` (outside: positive).
        let side = |a: f32, t: f32| (a.abs() - z * t) / (1.0 + t * t).sqrt();
        z > -r && side(x, th) < r && side(y, tv) < r
    }

    /// Starts and stops capture streams to match the live flags; returns the newest frames.
    fn sync_live(&mut self, model: &Model) -> HashMap<u64, live::Frame> {
        let want: Vec<(u64, isize)> = self
            .cards
            .iter()
            .filter(|c| c.kind == Kind::Window && !c.ghost && !self.live_failed.contains(&c.id))
            .filter(|c| {
                model
                    .get(c.id)
                    .is_some_and(|e| e.live && self.in_view(c, e.pos))
            })
            .filter_map(|c| Some((c.id, c.hwnd?)))
            .collect();
        let visible: HashSet<u64> = want
            .iter()
            .filter(|w| {
                self.cards
                    .iter()
                    .find(|c| c.id == w.0)
                    .zip(model.get(w.0))
                    .is_some_and(|(c, e)| self.on_screen(c, e.pos))
            })
            .map(|w| w.0)
            .collect();
        if self.live.is_none() {
            if want.is_empty() || self.live_broken {
                return HashMap::new();
            }
            let rx = self.live_making.get_or_insert_with(|| {
                let (tx, rx) = std::sync::mpsc::channel();
                let share = self.renderer.share();
                std::thread::spawn(move || {
                    let t = Instant::now();
                    unsafe {
                        let _ = windows::Win32::System::Com::CoInitializeEx(
                            None,
                            windows::Win32::System::Com::COINIT_MULTITHREADED,
                        );
                    }
                    let r = Live::new(share).map(live::Sendable).map_err(|e| e.to_string());
                    crate::store::log(&format!(
                        "live capture device made in {} ms",
                        t.elapsed().as_millis()
                    ));
                    let _ = tx.send(r);
                });
                rx
            });
            match rx.try_recv() {
                Ok(Ok(l)) => {
                    self.live = Some(l.0);
                    self.live_making = None;
                }
                Ok(Err(e)) => {
                    crate::store::log(&format!("live view unavailable: {e}"));
                    self.live_broken = true;
                    self.live_making = None;
                    return HashMap::new();
                }
                Err(std::sync::mpsc::TryRecvError::Empty) => return HashMap::new(),
                Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                    self.live_broken = true;
                    self.live_making = None;
                    return HashMap::new();
                }
            }
        }
        let Some(live) = &mut self.live else {
            return HashMap::new();
        };
        for id in live.ids() {
            if !want.iter().any(|w| w.0 == id) {
                live.stop(id, &mut self.renderer);
            }
        }
        live.visible = visible;
        for (id, h) in want {
            live.start(id, h);
        }
        live.priority = self
            .interact
            .as_ref()
            .map(|it| it.card)
            .or(self.theater.as_ref().map(|t| t.card))
            .and_then(|i| self.cards.get(i))
            .map(|c| c.id);
        let frames = live.poll(&mut self.renderer);
        for (id, e) in live.failed() {
            crate::store::log(&format!("live view of card {id} failed: {e}"));
            self.live_failed.insert(id);
        }
        frames
    }

    /// Starts using live card `i` in place, with the cursor at the centre of the view.
    fn begin_interact(&mut self, i: usize, model: &Model) {
        let Some(c) = self.cards.get(i) else { return };
        let frame = self.live.as_ref().is_some_and(|l| l.has_frame(c.id));
        if self.interact.is_some() || c.hwnd.is_none() || !frame {
            crate::store::log(&format!(
                "in-place use not started: already {}, window {}, live frame {frame}",
                self.interact.is_some(),
                c.hwnd.is_some()
            ));
            return;
        }
        self.press = None;
        let target = c.hwnd.unwrap_or_default();
        let (sw, sh) = self.renderer.size();
        let pos = (sw as f32 / 2.0, sh as f32 / 2.0);
        self.interact = Some(Interact {
            card: i,
            pos,
            since: Instant::now(),
            through: false,
            push: (0.0, 0.0),
            raw: (0.0, 0.0),
            button: false,
            away: false,
        });
        crate::pointer::hide_cursors();
        INTERACT.store(true, Ordering::Relaxed);
        crate::wall::start(self.hwnd, target);
        crate::stats::switch_start(target);
        // The window gets the real focus (keys, shortcuts, IMEs and games work as in normal
        // use); the overlay stays on top. `Esc Esc` is polled (see `frame`). Should the focus
        // not move, the overlay keeps it and posts keys on (`pass_input`).
        // The desktop keeps a Start menu or flyout that has the focus open.
        let fg = unsafe { GetForegroundWindow() };
        if crate::win::is_desktop(crate::win::hwnd_of(target)) && crate::win::part_of(fg, target) {
            crate::store::log("desktop used in place; the shell keeps the focus");
        } else {
            let ok = crate::win::force_foreground(HWND(target as *mut core::ffi::c_void));
            crate::store::log(&format!("window used in place gets the real focus: {ok}"));
        }
        unsafe {
            self.esc_down = GetAsyncKeyState(VK_ESCAPE.0 as i32) < 0;
            let _ = ClipCursor(None);
        }
        // The real cursor goes to the spot of the window under the crosshair.
        let p = match self.content_uv(i, pos, model) {
            Some((u, v, true)) => crate::win::card_point(target, u, v),
            _ => None,
        }
        .unwrap_or_else(|| self.screen_point(pos));
        unsafe {
            let _ = SetCursorPos(p.x, p.y);
        }
        self.interact_tick(model);
    }

    /// The real window of card `id`, if it has one.
    pub fn card_hwnd(&self, id: u64) -> Option<isize> {
        self.cards.iter().find(|c| c.id == id)?.hwnd
    }

    /// Whether a live card is on screen (its frames come as fast as the view draws).
    pub fn live_on_screen(&self) -> bool {
        self.live.as_ref().is_some_and(|l| !l.visible.is_empty())
    }

    /// The window being used in place, if any.
    pub fn interact_hwnd(&self) -> Option<isize> {
        self.cards.get(self.interact.as_ref()?.card)?.hwnd
    }

    /// The window used in place, if Start, search and the flyouts show on its card (as they
    /// show over it): not the desktop, not one filling its monitor (a game, a video: they go to
    /// the desktop card, see `show_desktop`), and the cursor on the card.
    pub fn shell_on_card(&self) -> Option<isize> {
        let h = self.interact_hwnd()?;
        let hw = crate::win::hwnd_of(h);
        (!crate::win::is_desktop(hw) && !crate::win::covers_monitor(hw) && !crate::wall::away()).then_some(h)
    }

    /// Back to flying, taking the keyboard focus back from the window that was used.
    pub fn end_interact_refocus(&mut self, why: &str) {
        if self.interact.is_some() {
            self.end_interact(why);
            crate::win::force_foreground(HWND(self.hwnd as *mut core::ffi::c_void));
        }
    }

    /// Using the desktop card in place, puts the 3D view back over the windows that came up
    /// over it (they still show on the card, see `win::raise_over`).
    pub fn keep_on_top(&self) {
        let desktop = self
            .interact_hwnd()
            .is_some_and(|h| crate::win::is_desktop(crate::win::hwnd_of(h)));
        if !desktop {
            return;
        }
        let over = crate::win::raise_over(self.hwnd);
        if !over.is_empty() {
            crate::store::log(&format!("desktop used in place: back over {}", over.join(", ")));
        }
    }

    /// The card showing real window `h` (not a ghost), if any.
    pub fn card_of(&self, h: isize) -> Option<u64> {
        let c = self.cards.iter().find(|c| c.hwnd == Some(h) && !c.ghost)?;
        Some(c.id)
    }

    /// Where a new window's card goes: the camera to put it in front of, and how far ahead.
    /// In theater mode, between the camera and the card filling the view.
    pub fn ahead(&self, model: &Model) -> (CameraState, f32) {
        // The card in theater mode, or else the one used in place: right by it, the new card
        // would come up behind it.
        let Some(card) = self
            .theater
            .as_ref()
            .map(|t| t.card)
            .or(self.interact.as_ref().map(|it| it.card))
            .or_else(|| self.just_left())
        else {
            return (self.camera_state(), 3.2);
        };
        let cam = self.fly.as_ref().map_or(self.cam, |f| f.to);
        let d = self
            .cards
            .get(card)
            .and_then(|c| model.get(c.id))
            .map_or(3.2, |e| cam.pos.distance(Vec3::from(e.pos)));
        let state = CameraState {
            pos: cam.pos.to_array(),
            yaw: cam.yaw,
            pitch: cam.pitch,
        };
        (state, (d * 0.5).clamp(0.4, 3.2))
    }

    /// Brings the card of window `to` right in front of the camera, filling the view (in
    /// theater mode, in front of the card in view), and uses it in place there, leaving
    /// whatever card was used. The camera stays where it is. None if it has no card.
    pub fn bring_window(&mut self, to: isize, model: &mut Model) -> Option<Action> {
        let i = self
            .cards
            .iter()
            .position(|c| c.hwnd == Some(to) && !c.ghost)?;
        if self.interact.as_ref().is_some_and(|it| it.card == i)
            || self.pending_interact.is_some_and(|(id, _)| id == self.cards[i].id)
        {
            return Some(Action::None);
        }
        let cam = self.fly.as_ref().map_or(self.cam, |f| f.to);
        if self.theater.is_none() {
            return self.bring_near(i, cam, model, true);
        }
        let c = &self.cards[i];
        let d = self.fill_distance(c);
        // In theater mode the card in view is about `d` away too: the new one goes in front of
        // it (the camera then backs off a little to fit it).
        let ahead = match &self.theater {
            Some(t) if t.card != i => self
                .cards
                .get(t.card)
                .and_then(|o| model.get(o.id))
                .map_or(d, |e| d.min(cam.pos.distance(Vec3::from(e.pos)) * 0.6)),
            _ => d,
        };
        let lift = c.half_h * (1.0 - self.content_frac(c));
        let fwd = Vec3::new(cam.yaw.sin(), 0.0, -cam.yaw.cos());
        let id = c.id;
        let e = model.get_mut(id)?;
        e.pos = (cam.pos + fwd * ahead + Vec3::Y * lift).to_array();
        e.yaw = cam.yaw;
        // Back to flying from where the camera is now, not from before theater mode.
        self.end_interact("over to another window");
        self.theater = None;
        self.fly = None;
        Some(self.enter_theater(i, model, false))
    }

    /// A launch (from Start shown on the card used in place, say) took the foreground: the use
    /// ends, but theater mode stays, so the camera doesn't fly back out and the new window's
    /// card comes up in front of this one.
    pub fn end_interact_for_launch(&mut self) {
        let theater = self.theater.take();
        self.end_interact("a launched app took the foreground");
        self.theater = theater;
    }

    /// The card whose use in place ended in the last 8 seconds (a launch may take that long).
    fn just_left(&self) -> Option<usize> {
        let (id, at) = self.left?;
        if at.elapsed() > std::time::Duration::from_secs(8) {
            return None;
        }
        self.cards.iter().position(|c| c.id == id && !c.ghost)
    }

    /// The user is on a card (using one in place, about to, or in theater mode) rather than
    /// flying around.
    pub fn on_card(&self) -> bool {
        self.interact.is_some() || self.pending_interact.is_some() || self.theater.is_some()
    }

    /// While flying: the card of window `to` goes ahead of the camera, its live view starting,
    /// without being used in place (the user clicks it for that). None if it has no card.
    pub fn show_window(&mut self, to: isize, model: &mut Model) -> Option<Action> {
        let i = self
            .cards
            .iter()
            .position(|c| c.hwnd == Some(to) && !c.ghost)?;
        let cam = self.fly.as_ref().map_or(self.cam, |f| f.to);
        self.bring_near(i, cam, model, false)
    }

    /// Outside theater mode: card `i` goes just in front of the card used in place (or looked
    /// at), a little down and to the right so the two read as separate cards, at about the
    /// same distance, and is used in place there (`use_it`). With no such card, or not to be
    /// used, it goes ahead of camera `cam`, filling most of the view.
    fn bring_near(
        &mut self,
        i: usize,
        cam: Camera,
        model: &mut Model,
        use_it: bool,
    ) -> Option<Action> {
        let near = self
            .interact
            .as_ref()
            .map(|it| it.card)
            .or_else(|| self.just_left())
            .or(self.hover)
            .filter(|&r| use_it && r != i)
            .and_then(|r| {
                let o = self.cards.get(r)?;
                Some((o.half_w, o.half_h, model.get(o.id)?))
            });
        let (pos, yaw) = match near {
            Some((hw, hh, e)) => {
                let center = Vec3::from(e.pos);
                let (right, n) = card_axes(e.yaw);
                let s = if n.dot(cam.pos - center) >= 0.0 {
                    1.0
                } else {
                    -1.0
                };
                let shift = right * s * (hw * 0.06) - Vec3::Y * (hh * 0.06);
                (center + n * s * 0.12 + shift, e.yaw)
            }
            None => {
                let c = &self.cards[i];
                let d = self.fill_distance(c) * 1.4;
                let lift = c.half_h * (1.0 - self.content_frac(c));
                let fwd = Vec3::new(cam.yaw.sin(), 0.0, -cam.yaw.cos());
                (cam.pos + fwd * d + Vec3::Y * lift, cam.yaw)
            }
        };
        let id = self.cards[i].id;
        let e = model.get_mut(id)?;
        e.pos = pos.to_array();
        e.yaw = yaw;
        self.live_failed.remove(&id);
        if use_it {
            self.end_interact("over to another window");
            // Used in place once its live view shows a frame (see `frame`).
            self.pending_interact = Some((id, Instant::now()));
            self.pending_traced = false;
        }
        Some(match model.get(id) {
            Some(e) if !e.live || !e.sound => Action::Watch(id),
            _ => Action::None,
        })
    }

    /// A shell popup (Start, search, a flyout) came up while the overlay shows: over to the
    /// desktop card to use it in place there, so the popup is seen and used on the card. False
    /// if there is no desktop card.
    pub fn show_desktop(&mut self, model: &Model) -> bool {
        let desktop = |c: &Card| {
            c.hwnd
                .is_some_and(|h| crate::win::is_desktop(crate::win::hwnd_of(h)))
        };
        let Some(i) = self.cards.iter().position(|c| desktop(c) && !c.ghost) else {
            return false;
        };
        if self.interact.as_ref().is_some_and(|it| it.card == i) {
            return true;
        }
        if self
            .pending_interact
            .is_some_and(|(id, _)| id == self.cards[i].id)
        {
            return true;
        }
        self.end_interact("a shell popup came up");
        self.enter_theater(i, model, false);
        self.sync_shell();
        true
    }

    /// While an app's card is used in place, the shell popups over it are made invisible (see
    /// `win::hide_shell_on_app`), and given back afterwards. The desktop card is the whole
    /// screen 1:1, so its popups stay where they are, over their own image. Called every frame.
    fn sync_shell(&mut self) {
        let on_app = self.shell_on_card().is_some();
        match (on_app, self.shell_hidden) {
            (true, last) if last.is_none_or(|t| t.elapsed().as_millis() >= 250) => {
                crate::win::hide_shell_on_app();
                self.shell_hidden = Some(Instant::now());
            }
            (false, Some(_)) => {
                crate::win::show_shell(false);
                self.shell_hidden = None;
            }
            _ => crate::win::rehide_shell(),
        }
    }

    /// Back to flying (without touching the focus: the overlay is closing).
    pub fn end_interact(&mut self, why: &str) {
        if self.pending_interact.take().is_some() && self.interact.is_none() {
            crate::store::log(&format!("in-place use dropped before it started: {why}"));
        }
        if let Some(it) = self.interact.take() {
            self.left = self.cards.get(it.card).map(|c| (c.id, Instant::now()));
            let hwnd = self.cards.get(it.card).and_then(|c| c.hwnd).unwrap_or(0);
            crate::store::log(&format!(
                "in-place use ended: {why}; {}",
                crate::win::cursor_report(hwnd, it.push, it.through)
            ));
            if it.through {
                crate::win::click_through(self.hwnd, false);
            }
            crate::pointer::hide();
            let p = self.screen_point(it.pos);
            crate::win::unpark_cursor(self.hwnd, p, RAW_INPUT.load(Ordering::Relaxed));
            crate::pointer::restore_cursors();
            INTERACT.store(false, Ordering::Relaxed);
            crate::wall::stop();
            self.mouse = (0.0, 0.0);
            unsafe {
                SetCursor(None);
            }
        }
        self.leave_theater();
    }

    /// Camera that makes the window image of card `i` fill the view, seen from our side.
    fn theater_camera(&self, i: usize, model: &Model) -> Option<Camera> {
        let c = self.cards.get(i)?;
        let e = model.get(c.id)?;
        let center = Vec3::from(e.pos);
        let (_, n) = card_axes(e.yaw);
        let s = if n.dot(self.cam.pos - center) >= 0.0 {
            1.0
        } else {
            -1.0
        };
        let frac = self.content_frac(c);
        // The live frame (drawn `LIVE_NUDGE` in front of the card) fills the view.
        let d = self.fill_distance(c);
        let mid = center - Vec3::Y * (c.half_h * (1.0 - frac));
        let yaw = (-s * n.x).atan2(s * n.z);
        // Its edges on whole screen pixels, so each pixel shows one texel of a window as big as
        // the screen instead of a blend of two.
        let (sw, sh) = self.renderer.size();
        let (sw, sh) = (sw.max(1) as f32, sh.max(1) as f32);
        let t = (self.fov * 0.5).tan();
        let px = 2.0 * d * t / sh;
        let ox = (sw - 2.0 * c.half_w / px) * 0.5;
        let oy = (sh - 2.0 * c.half_h * frac / px) * 0.5;
        let right = Vec3::new(yaw.cos(), 0.0, yaw.sin());
        let snap = right * (ox - ox.round()) * px + Vec3::Y * (oy.round() - oy) * px;
        Some(Camera {
            pos: mid + n * s * (d + LIVE_NUDGE) + snap,
            yaw,
            pitch: 0.0,
        })
    }

    /// How far from card `c` the camera is when its window image fills the view.
    fn fill_distance(&self, c: &Card) -> f32 {
        let frac = self.content_frac(c);
        let (sw, sh) = self.renderer.size();
        let aspect = sw.max(1) as f32 / sh.max(1) as f32;
        let t = (self.fov * 0.5).tan();
        (c.half_h * frac / t).max(c.half_w / (t * aspect))
    }

    /// F: fly up to a window card until it fills the view and use it there (live view and
    /// sound on, input passed on). With `real` (Shift+F) the real window then takes over,
    /// borderless fullscreen.
    fn enter_theater(&mut self, i: usize, model: &Model, real: bool) -> Action {
        let Some(c) = self.cards.get(i) else {
            return Action::None;
        };
        if c.kind != Kind::Window || c.ghost || c.hwnd.is_none() {
            return Action::None;
        }
        let Some(to) = self.theater_camera(i, model) else {
            return Action::None;
        };
        let id = c.id;
        self.press = None;
        let back = self.fly.take().map_or(self.cam, |f| f.to);
        self.theater = Some(Theater {
            card: i,
            back,
            auto: real,
            t0: Instant::now(),
        });
        self.fly = Some(Fly {
            from: self.cam,
            to,
            t0: Instant::now(),
        });
        self.vel = Vec3::ZERO;
        self.live_failed.remove(&id);
        if !real {
            // Used in place once the flight ends and its live view shows a frame.
            self.pending_interact = Some((id, Instant::now()));
            self.pending_traced = false;
        }
        match model.get(id) {
            Some(e) if !e.live || !e.sound => Action::Watch(id),
            _ => Action::None,
        }
    }

    /// Flies back to where the camera was before theater mode.
    fn leave_theater(&mut self) {
        if let Some(t) = self.theater.take() {
            self.fly = Some(Fly {
                from: self.cam,
                to: t.back,
                t0: Instant::now(),
            });
        }
    }

    /// Where the ray through overlay pixel `p` meets the content of card `i`: (u, v) across the
    /// window image, and whether that is inside it.
    fn content_uv(&self, i: usize, p: (f32, f32), model: &Model) -> Option<(f32, f32, bool)> {
        let c = self.cards.get(i)?;
        let e = model.get(c.id)?;
        let (sw, sh) = self.renderer.size();
        let (sw, sh) = (sw.max(1) as f32, sh.max(1) as f32);
        let t = (self.fov * 0.5).tan();
        let (x, y) = (p.0 / sw * 2.0 - 1.0, 1.0 - p.1 / sh * 2.0);
        let f = self.cam.forward();
        let r = Vec3::new(self.cam.yaw.cos(), 0.0, self.cam.yaw.sin());
        let d = (f + r * x * t * sw / sh + r.cross(f) * y * t).normalize();
        let center = Vec3::from(e.pos);
        let (cr, n) = card_axes(e.yaw);
        let denom = n.dot(d);
        if denom.abs() < 1e-5 {
            return None;
        }
        let dist = n.dot(center - self.cam.pos) / denom;
        let q = self.cam.pos + d * dist - center;
        let u = (q.dot(cr) + c.half_w) / (2.0 * c.half_w);
        let frac = self.content_frac(c);
        let v = ((c.half_h - q.y) / (2.0 * c.half_h) - (1.0 - frac)) / frac;
        let inside = dist > 0.0 && (0.0..=1.0).contains(&u) && (0.0..=1.0).contains(&v);
        Some((u, v, inside))
    }

    /// Overlay client pixels to screen pixels.
    fn screen_point(&self, p: (f32, f32)) -> POINT {
        let mut pt = POINT {
            x: p.0 as i32,
            y: p.1 as i32,
        };
        unsafe {
            let _ = ClientToScreen(HWND(self.hwnd as *mut core::ffi::c_void), &mut pt);
        }
        pt
    }

    /// Where (`u`, `v`) across the window image of card `i` shows on the overlay (client
    /// pixels); `u`, `v` may lie outside [0, 1]. The inverse of `content_uv`.
    fn card_screen(&self, i: usize, u: f32, v: f32, model: &Model) -> Option<(f32, f32)> {
        let c = self.cards.get(i)?;
        let e = model.get(c.id)?;
        let (sw, sh) = self.renderer.size();
        let (sw, sh) = (sw.max(1) as f32, sh.max(1) as f32);
        let t = (self.fov * 0.5).tan();
        let f = self.cam.forward();
        let r = Vec3::new(self.cam.yaw.cos(), 0.0, self.cam.yaw.sin());
        let up = r.cross(f);
        let (cr, _) = card_axes(e.yaw);
        let frac = self.content_frac(c);
        let qy = c.half_h - (v * frac + 1.0 - frac) * 2.0 * c.half_h;
        let q = Vec3::from(e.pos) + cr * (u * 2.0 * c.half_w - c.half_w) + Vec3::Y * qy;
        let d = q - self.cam.pos;
        let z = d.dot(f);
        if z <= 1e-4 {
            return None;
        }
        let x = d.dot(r) / (z * t * sw / sh);
        let y = d.dot(up) / (z * t);
        Some(((x + 1.0) * 0.5 * sw, (1.0 - y) * 0.5 * sh))
    }

    /// Once a frame while a card is used in place: follows the real cursor on the real window
    /// with the pointer on the card. While the cursor is on the real window the overlay is
    /// click-through (the window gets the real mouse, as in normal use); off it, the overlay
    /// takes the mouse back so a click there ends using the window.
    fn interact_tick(&mut self, model: &Model) {
        let mut slow = SlowTick::new();
        let Some(it) = &self.interact else { return };
        let card = it.card;
        let Some(hwnd) = self.cards.get(card).and_then(|c| c.hwnd) else {
            return;
        };
        self.keep_on_top();
        let away = it.away;
        let mut cur = POINT::default();
        unsafe {
            let _ = GetCursorPos(&mut cur);
        }
        let mon = crate::win::monitor_of(HWND(self.hwnd as *mut core::ffi::c_void));
        let here = |p: POINT| {
            mon.is_some_and(|m| p.x >= m.left && p.x < m.right && p.y >= m.top && p.y < m.bottom)
        };
        if away {
            let Some(it) = self.interact.as_mut() else {
                return;
            };
            it.raw = (0.0, 0.0);
            let mut c = cur;
            unsafe {
                let _ = windows::Win32::Graphics::Gdi::ScreenToClient(
                    HWND(self.hwnd as *mut core::ffi::c_void),
                    &mut c,
                );
            }
            let p = (c.x as f32, c.y as f32);
            // Over the card again: onto the window at that point.
            let back = here(cur)
                .then(|| self.content_uv(card, p, model))
                .flatten()
                .filter(|&(_, _, inside)| inside)
                .and_then(|(u, v, _)| crate::win::card_point(hwnd, u, v));
            let Some(w) = back else {
                if here(cur) {
                    if let Some(it) = self.interact.as_mut() {
                        it.pos = p;
                    }
                }
                return;
            };
            crate::win::free_cursor_at(w);
            cur = w;
            crate::wall::set_away(false);
            crate::pointer::hide_cursors();
            if let Some(it) = self.interact.as_mut() {
                it.away = false;
                it.push = (0.0, 0.0);
            }
            crate::store::log(&format!(
                "in-place cursor back onto the card at ({},{}) (window point ({},{}))",
                c.x, c.y, w.x, w.y
            ));
        }
        slow.mark("away");
        crate::win::confine_cursor(self.hwnd, hwnd);
        slow.mark("confine");
        let clip = crate::win::cursor_clip();
        if crate::wall::take_end() {
            self.end_interact_refocus("click pushed off the window (off the overlay)");
            return;
        }
        // On the window only where the window is what is really there; elsewhere off the
        // overlay `wall` holds the cursor back.
        slow.mark("clip");
        let under = crate::win::window_at(cur);
        let mine = crate::win::part_of(under, hwnd) || crate::wall::desk_free(cur);
        slow.mark("under");
        let hook = crate::wall::take_slowest();
        if hook > 20_000 {
            crate::store::log(&format!("in-place: a button press took the mouse hook {} ms", hook / 1000));
        }
        let Some(it) = self.interact.as_mut() else {
            return;
        };
        let raw = std::mem::take(&mut it.raw);
        let button = [VK_LBUTTON, VK_RBUTTON, VK_MBUTTON]
            .iter()
            .any(|k| unsafe { GetAsyncKeyState(k.0 as i32) } < 0);
        if button && !it.button {
            crate::store::log(&format!(
                "in-place click: {}",
                crate::win::cursor_report(hwnd, it.push, it.through)
            ));
        }
        // Per axis: held at the low edge, pushing on (negative) goes past it; at the high
        // edge, positive. Leaving the edge drops the push.
        let axis = |push: f32, d: f32, at: i32, lo: i32, hi: i32| {
            if at <= lo {
                (push + d).clamp(-PUSH_MAX, 0.0)
            } else if at >= hi - 1 {
                (push + d).clamp(0.0, PUSH_MAX)
            } else {
                0.0
            }
        };
        if let Some(c) = clip {
            it.push.0 = axis(it.push.0, raw.0, cur.x, c.left, c.right);
            it.push.1 = axis(it.push.1, raw.1, cur.y, c.top, c.bottom);
        }
        it.button = button;
        // The wall hook and the held cursor see the same motion: where the hook holds the
        // cursor its push counts (and the held push follows it, so nothing jumps once the hook
        // lets go); the held push is for cursors the app itself holds.
        let wall = crate::wall::push();
        if wall.0 != 0.0 {
            it.push.0 = wall.0;
        }
        if wall.1 != 0.0 {
            it.push.1 = wall.1;
        }
        let push = (
            it.push.0.clamp(-PUSH_MAX, PUSH_MAX),
            it.push.1.clamp(-PUSH_MAX, PUSH_MAX),
        );
        slow.mark("shell+push");
        let at = crate::win::window_uv(hwnd, cur);
        let on =
            mine && at.is_some_and(|(u, v)| (0.0..=1.0).contains(&u) && (0.0..=1.0).contains(&v));
        slow.mark("uv");
        let pos = at
            .and_then(|(u, v)| self.card_screen(card, u.clamp(0.0, 1.0), v.clamp(0.0, 1.0), model));
        let screen = pos.map(|p| self.screen_point(p));
        // Pushed past the window's edge: the cursor goes off the card where the pointer is,
        // free over the 3D view (and on to other monitors) until it comes back onto the card.
        if push != (0.0, 0.0) {
            if let (Some(p), Some(m)) = (pos, mon) {
                let step = |d: f32| {
                    if d > 0.0 {
                        3.0
                    } else if d < 0.0 {
                        -3.0
                    } else {
                        0.0
                    }
                };
                let s = self.screen_point((p.0 + step(push.0), p.1 + step(push.1)));
                let s = POINT {
                    x: s.x.clamp(m.left, m.right - 1),
                    y: s.y.clamp(m.top, m.bottom - 1),
                };
                crate::wall::set_away(true);
                crate::wall::set_off(false);
                crate::win::free_cursor_at(s);
                crate::pointer::hide();
                crate::pointer::restore_cursors();
                let Some(it) = self.interact.as_mut() else {
                    return;
                };
                it.away = true;
                it.push = (0.0, 0.0);
                it.pos = p;
                if it.through {
                    it.through = false;
                    crate::win::click_through(self.hwnd, false);
                }
                crate::store::log(&format!(
                    "in-place cursor off the card at ({},{})",
                    s.x, s.y
                ));
                return;
            }
        }
        slow.mark("pos");
        crate::wall::set_off(!on);
        let Some(it) = self.interact.as_mut() else {
            return;
        };
        let through = on;
        if through != it.through {
            crate::store::log(&format!(
                "in-place cursor {} the window: {}",
                if through { "onto" } else { "off" },
                crate::win::cursor_report(hwnd, push, it.through)
            ));
            it.through = through;
            crate::win::click_through(self.hwnd, through);
        }
        slow.mark("through");
        match (pos, screen) {
            (Some(p), Some(screen)) => {
                it.pos = p;
                crate::pointer::show(screen);
            }
            _ => crate::pointer::hide(),
        }
    }

    /// Handles a mouse or keyboard message the overlay got while a card is used in place: keys
    /// (only when the window could not get the real focus) are sent on; a click (the cursor is
    /// off the real window then) ends using it. Returns false when that ended.
    pub fn pass_input(&mut self, msg: u32, wp: usize, lp: isize) -> bool {
        let Some(hwnd) = self.interact_hwnd() else {
            self.end_interact("card has no window");
            return false;
        };
        match msg {
            WM_KEYDOWN | WM_SYSKEYDOWN
                if wp as u16 == VK_ESCAPE.0 && lp & (1 << 30) == 0 && self.double_esc() =>
            {
                self.end_interact_refocus("Esc Esc (overlay focused)");
                false
            }
            // The target's own TranslateMessage turns the posted key-downs into characters;
            // sending ours too would type everything twice.
            WM_CHAR | WM_SYSCHAR => true,
            // `force_foreground`'s own unlock key.
            WM_KEYDOWN | WM_KEYUP if wp == 0xE8 => true,
            WM_KEYDOWN | WM_KEYUP | WM_SYSKEYDOWN | WM_SYSKEYUP => {
                crate::pointer::typed();
                crate::win::forward_key(hwnd, msg, wp, lp);
                true
            }
            WM_LBUTTONDOWN | WM_RBUTTONDOWN | WM_MBUTTONDOWN => {
                self.end_interact_refocus("click on the overlay");
                false
            }
            _ => true,
        }
    }

    /// Live cards take the shape of their window's current content (a video going
    /// fullscreen, a resized window); other cards the shape of their screenshot. A theater
    /// flight into a card that changed shape is re-aimed.
    fn fit_live_cards(&mut self, frames: &HashMap<u64, live::Frame>, model: &Model) {
        let mut changed = None;
        for i in 0..self.cards.len() {
            let c = &self.cards[i];
            let (bw, bh) = self.card_half(c.kind, c.tex);
            let (w, h) = match frames.get(&c.id) {
                Some(fr) if fr.w > 0 && fr.h > 0 => {
                    let hh = bw * fr.h as f32 / fr.w as f32 / self.content_frac(c);
                    if hh > 1.6 {
                        (bw * 1.6 / hh, 1.6)
                    } else {
                        (bw, hh)
                    }
                }
                _ => (bw, bh),
            };
            let k = model.get(c.id).map_or(1.0, |e| e.scale);
            let (w, h) = (w * k, h * k);
            // Exact: a card off by a fraction of a texel (the screenshot's rounded height) would
            // stretch its frame by a row when it fills the view, blurring every line of text.
            if (w - c.half_w).abs() > w * 1e-6 || (h - c.half_h).abs() > h * 1e-6 {
                let c = &mut self.cards[i];
                (c.half_w, c.half_h) = (w, h);
                changed = Some(i);
            }
        }
        let Some(t) = &self.theater else { return };
        if changed.is_none()
            || !self
                .cards
                .get(t.card)
                .is_some_and(|c| frames.contains_key(&c.id))
        {
            return;
        }
        if let Some(to) = self.theater_camera(t.card, model) {
            match &mut self.fly {
                Some(f) => f.to = to,
                None => self.cam = to,
            }
        }
    }

    /// Tells the stats window the frame rate and the live cards' fps and delay, twice a second.
    fn report_view(&mut self, dt: f32) {
        self.frame_avg = if self.frame_avg == 0.0 {
            dt
        } else {
            self.frame_avg * 0.95 + dt * 0.05
        };
        let now = Instant::now();
        if now < self.view_report {
            return;
        }
        self.view_report = now + std::time::Duration::from_millis(500);
        let ls: Vec<_> = self
            .live
            .as_ref()
            .map(|l| l.ids().into_iter().filter_map(|id| l.stats(id)).collect())
            .unwrap_or_default();
        // Dragging on a card used in place: how fast its frames come and are drawn, for the log.
        if let Some(id) = self.interact.as_ref().and_then(|it| self.cards.get(it.card)).map(|c| c.id) {
            if unsafe { GetAsyncKeyState(VK_LBUTTON.0 as i32) } < 0 {
                if let Some(s) = self.live.as_ref().and_then(|l| l.stats(id)) {
                    crate::store::log(&format!(
                        "in-place drag: card {id} {:.0} fps, delay {:.0} ms, frame {:.1} ms",
                        s.fps,
                        s.delay_ms,
                        self.frame_avg * 1000.0
                    ));
                }
            }
        }
        let n = ls.len().max(1) as f32;
        crate::stats::set_view(crate::stats::ViewStats {
            frame_ms: self.frame_avg * 1000.0,
            live: ls.len(),
            fps: ls.iter().map(|s| s.fps).sum::<f32>() / n,
            delay_ms: ls.iter().map(|s| s.delay_ms).sum::<f32>() / n,
            delay_max: ls.iter().map(|s| s.delay_ms).fold(0.0, f32::max),
            at: now,
        });
    }

    /// Redraws the stats panel about twice a second.
    fn update_stats(&mut self, dt: f32, frames: &HashMap<u64, live::Frame>, model: &Model) {
        let Some(st) = &mut self.stats else { return };
        st.frame_ms = if st.frame_ms == 0.0 {
            dt * 1000.0
        } else {
            st.frame_ms * 0.95 + dt * 50.0
        };
        let now = Instant::now();
        if now < st.next && st.slot.is_some() {
            return;
        }
        st.next = now + std::time::Duration::from_millis(500);
        let card = self
            .theater
            .as_ref()
            .map(|t| t.card)
            .or(self.interact.as_ref().map(|it| it.card))
            .or(self.hover)
            .and_then(|i| self.cards.get(i));
        let mut lines: Vec<String> = Vec::new();
        let mb = |b: u64| b as f32 / 1048576.0;
        match card {
            Some(c) => {
                let title = model.get(c.id).map(|e| e.title.clone()).unwrap_or_default();
                let title: String = title.chars().take(48).collect();
                lines.push(format!("\u{25b8} {title}"));
                if let Some(h) = c.hwnd.filter(|_| !c.ghost) {
                    let mut pid = 0u32;
                    unsafe {
                        GetWindowThreadProcessId(HWND(h as *mut core::ffi::c_void), Some(&mut pid));
                    }
                    st.app = crate::stats::sample(pid, st.app.as_ref());
                    match &st.app {
                        Some(a) => lines.push(format!(
                            "App CPU {}   RAM {:.0} MB (private {:.0} MB)",
                            a.cpu_pct.map_or("\u{2026}".into(), |p| format!("{p:.1} %")),
                            mb(a.working_set),
                            mb(a.private)
                        )),
                        None => lines.push("App CPU/RAM: not readable (elevated?)".into()),
                    }
                    lines.push(match crate::stats::switch_ms(h) {
                        Some(ms) => format!("Switch to app: {ms:.0} ms (last time)"),
                        None => "Switch to app: not measured yet".into(),
                    });
                    let ls = self.live.as_ref().and_then(|l| l.stats(c.id));
                    lines.push(match (ls, frames.get(&c.id)) {
                        (Some(ls), Some(fr)) => format!(
                            "Live {}x{}: {:.0} fps   delay {:.1} ms   copy {:.1} ms",
                            fr.w, fr.h, ls.fps, ls.delay_ms, ls.copy_ms
                        ),
                        _ => "Live: off (L turns it on)".into(),
                    });
                    lines.push(match crate::stats::input_ms() {
                        Some(ms) => format!("Input pass-on delay: {ms:.2} ms"),
                        None => "Input pass-on delay: click a card to use it".into(),
                    });
                } else {
                    st.app = None;
                }
            }
            None => lines.push("Aim at a card for its stats".into()),
        }
        st.own = crate::stats::sample(0, st.own.as_ref());
        if let Some(o) = &st.own {
            lines.push(format!(
                "Window3D: frame {:.1} ms ({:.0} fps)   CPU {}   RAM {:.0} MB",
                st.frame_ms,
                1000.0 / st.frame_ms.max(0.1),
                o.cpu_pct.map_or("\u{2026}".into(), |p| format!("{p:.1} %")),
                mb(o.working_set)
            ));
        }
        let refs: Vec<&str> = lines.iter().map(|l| l.as_str()).collect();
        let img = crate::win::compose_hud_text(&refs, 15);
        if st.slot.is_none() || st.size != (img.w, img.h) {
            st.slot = Some(self.renderer.live_slot(st.slot, img.w, img.h));
            st.size = (img.w, img.h);
        }
        if let Some(slot) = st.slot {
            self.renderer
                .write_live(slot, &img.px, img.w * 4, img.w, img.h);
        }
    }

    /// Advances the simulation and draws one frame. `focused` gates keyboard polling.
    pub fn frame(&mut self, model: &mut Model, focused: bool) -> Action {
        let mut action = Action::None;
        let now = Instant::now();
        let dt = (now - self.last).as_secs_f32().min(0.1);
        self.last = now;
        // A panorama download ended: show it if it is the background.
        let ended = crate::background::downloads_ended();
        if ended != self.downloads_seen {
            self.downloads_seen = ended;
            let bg = self.menu.as_ref().map_or_else(|| self.background.clone(), |m| m.now.background.clone());
            let load = self.sky_load.take();
            self.sky_load = apply_background(&mut self.renderer, &bg, load);
            if let Some(m) = &mut self.menu {
                m.dirty = true;
            }
        }
        self.poll_sky();
        self.sync_shell();
        // Start or search opened off the window used in place (a small one): its card can't
        // show it, the desktop card can.
        if let Some(h) = self.shell_on_card() {
            let fg = unsafe { GetForegroundWindow() };
            if crate::win::is_shell_popup(fg)
                && crate::win::shows_off(fg, crate::win::hwnd_of(h))
                && self.show_desktop(model)
            {
                crate::store::log(&format!(
                    "a shell popup showed off the window used in place ({}): to the desktop card",
                    crate::win::describe(fg)
                ));
            }
        }
        let t_shell = now.elapsed().as_micros();

        if self.interact.is_some() && !focused {
            let down = unsafe { GetAsyncKeyState(VK_ESCAPE.0 as i32) } < 0;
            let pressed = down && !self.esc_down;
            self.esc_down = down;
            // The Esc sent to close Start or search is not one of the user's.
            let ours = self.shell_esc.is_some_and(|t| t.elapsed().as_millis() < 300);
            if pressed && !ours && self.double_esc() {
                self.end_interact_refocus("Esc Esc");
            }
        }
        self.interact_tick(model);
        let t_tick = now.elapsed().as_micros();

        // Mouse look.
        let (mx, my) = std::mem::take(&mut self.mouse);
        self.cam.yaw = (self.cam.yaw + mx * self.sensitivity).rem_euclid(std::f32::consts::TAU);
        self.cam.pitch = (self.cam.pitch - my * self.sensitivity).clamp(-1.5, 1.5);

        // Theater flights.
        if let Some(fl) = &self.fly {
            let k = ((now - fl.t0).as_secs_f32() / FLY_SECS).min(1.0);
            let e = k * k * (3.0 - 2.0 * k);
            let dyaw = (fl.to.yaw - fl.from.yaw + std::f32::consts::PI)
                .rem_euclid(std::f32::consts::TAU)
                - std::f32::consts::PI;
            self.cam = Camera {
                pos: fl.from.pos.lerp(fl.to.pos, e),
                yaw: (fl.from.yaw + dyaw * e).rem_euclid(std::f32::consts::TAU),
                pitch: fl.from.pitch + (fl.to.pitch - fl.from.pitch) * e,
            };
            self.vel = Vec3::ZERO;
            if k >= 1.0 {
                self.fly = None;
            }
        }
        let parked = self.theater.is_some() || self.fly.is_some();

        // Movement (polled so held keys move smoothly).
        let focused = focused
            && self.interact.is_none()
            && !parked
            && self.search.is_none()
            && self.menu.is_none();
        let down = |vk: VIRTUAL_KEY| {
            focused && unsafe { GetAsyncKeyState(vk.0 as i32) } as u16 & 0x8000 != 0
        };
        let flat_fwd = Vec3::new(self.cam.yaw.sin(), 0.0, -self.cam.yaw.cos());
        let right = Vec3::new(self.cam.yaw.cos(), 0.0, self.cam.yaw.sin());
        let mut dir = Vec3::ZERO;
        if down(VK_W) || down(VK_UP) {
            dir += flat_fwd;
        }
        if down(VK_S) || down(VK_DOWN) {
            dir -= flat_fwd;
        }
        if down(VK_D) || down(VK_RIGHT) {
            dir += right;
        }
        if down(VK_A) || down(VK_LEFT) {
            dir -= right;
        }
        if down(VK_SPACE) || down(VK_E) {
            dir += Vec3::Y;
        }
        if down(VK_Q) || down(VK_C) {
            dir -= Vec3::Y;
        }
        let boost = if down(VK_SHIFT) { 2.5 } else { 1.0 };
        let target = dir.normalize_or_zero() * self.speed * boost;
        self.vel = self.vel.lerp(target, 1.0 - (-dt * 12.0).exp());
        if !parked {
            self.cam.pos = clamp_in_room(self.cam.pos + self.vel * dt, 0.3);
        }

        let (o, f) = (self.cam.pos, self.cam.forward());

        // Dragging: the grabbed point stays under the crosshair at a fixed distance and the card turns to face us.
        let mut snap_card = None;
        if let Some(p) = &self.press {
            if p.dragging {
                snap_card = Some(p.card);
                if let Some(card) = self.cards.get(p.card) {
                    let yaw = self.cam.yaw;
                    let (r, _) = card_axes(yaw);
                    let margin = card.half_w.max(card.half_h) * 0.5;
                    let center = clamp_in_room(
                        o + f * p.dist - r * p.local.0 - Vec3::Y * p.local.1,
                        margin.min(1.0),
                    );
                    if let Some(e) = model.get_mut(card.id) {
                        e.pos = center.to_array();
                        e.yaw = yaw;
                    }
                }
            }
        }
        if let Some(i) = snap_card {
            self.apply_snap(i, model);
        }
        self.slot_hot = snap_card.and_then(|_| slot_at(o, f, &model.layout.slots));
        // A held toolbar button or gizmo arrow.
        if let Some(td) = &mut self.tool {
            if td.moved > 6.0 {
                let acc = std::mem::take(&mut td.acc);
                let (i, hit, raw) = (td.card, td.hit, td.raw);
                let raw = self.apply_tool(i, hit, acc, raw, model);
                if let Some(td) = &mut self.tool {
                    td.raw = raw;
                }
            }
        }
        let mut hit = Hit::Card;
        self.hover = match (&self.interact, &self.press, &self.tool) {
            (Some(it), _, _) => Some(it.card),
            (_, Some(p), _) => Some(p.card),
            (_, _, Some(td)) => {
                hit = td.hit;
                Some(td.card)
            }
            _ => self.pick_hit(o, f, model).map(|h| {
                hit = h.1;
                h.0
            }),
        };
        self.hit = hit;
        // The toolbar shows beside the card under the crosshair and stays a moment after the
        // crosshair leaves, so it can be reached.
        if self.interact.is_some() || parked || self.free {
            self.bar = None;
        } else if let Some(i) = self.hover {
            self.bar = Some((i, now));
        } else if self.bar.is_some_and(|(_, t)| (now - t).as_secs_f32() > 0.6) {
            self.bar = None;
        }
        let t_pre = now.elapsed().as_micros();
        let frames = self.sync_live(model);
        let t_live = now.elapsed().as_micros();
        if now >= self.shape_check {
            self.shape_check = now + std::time::Duration::from_millis(500);
            let ids = self.live.as_ref().map(|l| l.ids()).unwrap_or_default();
            let hwnds: Vec<isize> = ids.iter().filter_map(|&id| self.card_hwnd(id)).collect();
            self.shaped.extend(crate::win::pre_uncover_chromium(&hwnds));
            for id in ids {
                if let Some(h) = self.card_hwnd(id) {
                    self.shaped.extend(crate::win::uncover_chromium(h));
                }
            }
        }
        if self.interact.as_ref().is_some_and(|it| {
            self.cards
                .get(it.card)
                .is_none_or(|c| !frames.contains_key(&c.id))
        }) {
            self.end_interact_refocus("live view of the card stopped");
        }
        let t_shape = now.elapsed().as_micros();
        self.fit_live_cards(&frames, model);
        if let Some((id, t0)) = self.pending_interact {
            if t0.elapsed() >= std::time::Duration::from_millis(1000) && !self.pending_traced {
                self.pending_traced = true;
                crate::store::log(&format!(
                    "in-place use waiting 1 s: live {:?}, streams {:?}, frame {}, flying {}, in view {:?}",
                    model.get(id).map(|e| e.live),
                    self.live.as_ref().map(|l| l.ids()),
                    frames.contains_key(&id),
                    self.fly.is_some(),
                    self.cards
                        .iter()
                        .find(|c| c.id == id)
                        .zip(model.get(id))
                        .map(|(c, e)| self.in_view(c, e.pos))
                ));
            }
            if t0.elapsed().as_secs_f32() > PENDING_SECS || self.live_failed.contains(&id) {
                crate::store::log(&format!(
                    "in-place use not started: live view failed {}, frame {}, flying {}; focusing the real window",
                    self.live_failed.contains(&id),
                    frames.contains_key(&id),
                    self.fly.is_some()
                ));
                self.pending_interact = None;
                // The click still does something: the real window takes over.
                if let Some(h) = self
                    .cards
                    .iter()
                    .find(|c| c.id == id && !c.ghost)
                    .and_then(|c| c.hwnd)
                {
                    action = Action::Focus(h);
                }
            } else if frames.contains_key(&id) && self.fly.is_none() {
                self.pending_interact = None;
                if let Some(i) = self.cards.iter().position(|c| c.id == id) {
                    self.begin_interact(i, model);
                }
            }
        }
        if let Some(t) = &mut self.theater {
            let has_frame = self
                .cards
                .get(t.card)
                .is_some_and(|c| frames.contains_key(&c.id));
            // Give the live frame a moment to show up, so the hand-over is seamless.
            let waited = t.t0.elapsed().as_secs_f32() > FLY_SECS + 0.5;
            if t.auto && self.fly.is_none() && (has_frame || waited) {
                t.auto = false;
                if let Some(h) = self.cards.get(t.card).and_then(|c| c.hwnd) {
                    action = Action::Theater(h);
                }
            }
        }
        self.update_stats(dt, &frames, model);
        self.report_view(dt);

        // Build draw lists, far to near so translucent ghosts blend correctly.
        // The open workflow picked with `K`: its cards and slots are highlighted.
        if self
            .wf_sel
            .is_some_and(|i| i >= model.layout.workflows.len())
        {
            self.wf_sel = None;
        }
        let wf = self.wf_sel.and_then(|i| model.layout.workflows.get(i));
        let wf_ids: HashSet<u64> = wf
            .map(|w| w.ids.iter().copied().collect())
            .unwrap_or_default();
        let wf_name = wf.map(|w| w.name.clone());
        let wf_name = wf_name.as_ref();
        let found = self.search.as_ref().and_then(|s| s.hits.get(s.sel).copied());
        let mut draws: Vec<(f32, CardDraw)> = Vec::with_capacity(self.cards.len());
        for (i, c) in self.cards.iter().enumerate() {
            let Some(e) = model.get(c.id) else { continue };
            let center = Vec3::from(e.pos);
            let (r, n) = card_axes(e.yaw);
            let at_limit = e.scale >= tools::MAX_SCALE - 1e-3 || e.scale <= tools::MIN_SCALE + 1e-3;
            let picked = wf_ids.contains(&c.id) || found == Some(i);
            let hl = match self.hover == Some(i) && self.interact.is_none() {
                true if at_limit => 2.0,
                true => 1.0,
                false if picked => 1.0,
                false => 0.0,
            };
            let dist = center.distance_squared(o);
            if let Some(fr) = frames.get(&c.id) {
                // The live frame covers the screenshot area, just in front of the card.
                let f = self.content_frac(c);
                let nudge = n * LIVE_NUDGE * n.dot(o - center).signum();
                // Viewed with F or used in place it fills the view: no outline over its edge pixels.
                let full = self.theater.as_ref().is_some_and(|t| t.card == i)
                    || self.interact.as_ref().is_some_and(|t| t.card == i);
                draws.push((
                    dist - 1e-3,
                    CardDraw {
                        center: center + Vec3::Y * c.half_h * (f - 1.0) + nudge,
                        right: r * c.half_w,
                        up: Vec3::Y * c.half_h * f,
                        alpha: 1.0,
                        ghost: 0.0,
                        highlight: if full { -1.0 } else { 0.0 },
                        tex: fr.slot,
                    },
                ));
            }
            draws.push((
                dist,
                CardDraw {
                    center,
                    right: r * c.half_w,
                    up: Vec3::Y * c.half_h,
                    alpha: if c.ghost { 0.55 } else { 1.0 },
                    ghost: if c.ghost { 1.0 } else { 0.0 },
                    highlight: hl,
                    tex: c.tex,
                },
            ));
        }
        if !parked {
            for (k, s) in model.layout.slots.iter().enumerate() {
                let q = slot_quad(s);
                let hot = self.slot_hot == Some(k) || (!s.wf.is_empty() && Some(&s.wf) == wf_name);
                draws.push((
                    q.center.distance_squared(o) + 1e-3,
                    CardDraw {
                        center: q.center,
                        right: q.right,
                        up: q.up,
                        alpha: if hot { 0.6 } else { 0.3 },
                        ghost: 0.0,
                        highlight: if hot { 1.0 } else { 0.0 },
                        tex: self.slot_tex,
                    },
                ));
            }
        }
        if let Some((b, _)) = self.bar {
            if let Some(e) = self.cards.get(b).and_then(|c| model.get(c.id)) {
                let on = |t: Tool| match t {
                    Tool::Eye => e.live,
                    Tool::Sound => e.sound,
                    Tool::Move => self.gizmo == Some(e.id),
                    Tool::Snap => e.snap,
                    _ => false,
                };
                for (t, q) in self.bar_quads(b, model) {
                    let hot = self.hover == Some(b) && self.hit == Hit::Tool(t);
                    let icons = if on(t) {
                        &self.icons.on
                    } else {
                        &self.icons.off
                    };
                    draws.push((
                        q.center.distance_squared(o) - 2e-3,
                        CardDraw {
                            center: q.center,
                            right: q.right,
                            up: q.up,
                            alpha: 0.95,
                            ghost: 0.0,
                            highlight: if hot { 1.0 } else { 0.0 },
                            tex: icons[t as usize],
                        },
                    ));
                }
            }
        }
        if let Some((g, axes)) = self.gizmo_of(model).filter(|_| !parked) {
            for (k, (_, shaft, knob)) in axes.iter().enumerate() {
                let hot = self.hover == Some(g) && self.hit == Hit::Axis(k);
                for q in shaft.iter().chain([knob]) {
                    draws.push((
                        q.center.distance_squared(o) - 2e-3,
                        CardDraw {
                            center: q.center,
                            right: q.right,
                            up: q.up,
                            alpha: 1.0,
                            ghost: 0.0,
                            highlight: if hot { 1.0 } else { 0.0 },
                            tex: self.icons.axis[k],
                        },
                    ));
                }
            }
        }
        draws.sort_by(|a, b| b.0.total_cmp(&a.0));
        let draws: Vec<CardDraw> = draws.into_iter().map(|d| d.1).collect();

        let (sw, sh) = self.renderer.size();
        let (sw, sh) = (sw as f32, sh as f32);
        let rect = |cx: f32, cy: f32, w: f32, h: f32| {
            // Pixel-space centre/size -> NDC [left, bottom, right, top], snapped to whole pixels.
            let (l, t) = ((cx - w / 2.0).round(), (cy - h / 2.0).round());
            [
                l / sw * 2.0 - 1.0,
                1.0 - (t + h) / sh * 2.0,
                (l + w) / sw * 2.0 - 1.0,
                1.0 - t / sh * 2.0,
            ]
        };
        if let (Some(i), Some(name)) = (self.wf_sel, wf_name) {
            if self
                .wf_hud
                .as_ref()
                .is_none_or(|h| h.0 != i || &h.1 != name)
            {
                let n = model.layout.workflows.len();
                let text = format!(
                    "Workflow \u{201c}{name}\u{201d} ({}/{n})  \u{00b7}  K: next  \u{00b7}  X: close  \u{00b7}  N: mute all  \u{00b7}  P: live view on/off",
                    i + 1
                );
                let img = crate::win::compose_hud_text(&[text.as_str()], self.text_px);
                let key = text.bytes().fold(1469598103934665603u64, |h, b| {
                    (h ^ b as u64).wrapping_mul(1099511628211)
                });
                let tex = self.renderer.upload_cached(HUD_IDS + 60, key, &img, false);
                let (w, h) = self.renderer.tex_size(tex);
                self.wf_hud = Some((
                    i,
                    name.clone(),
                    Hud {
                        tex,
                        w: w as f32,
                        h: h as f32,
                    },
                ));
            }
        }
        let hints_on = SHOW_HINTS.load(std::sync::atomic::Ordering::Relaxed);
        let mut hud = vec![
            HudDraw {
                rect: rect(sw / 2.0, sh / 2.0, self.cross.w, self.cross.h),
                alpha: if self.interact.is_some() || parked || self.menu.is_some() {
                    0.0
                } else {
                    1.0
                },
                tex: self.cross.tex,
            },
            HudDraw {
                rect: rect(
                    sw / 2.0,
                    sh - self.help.h / 2.0 - sh * 0.03,
                    self.help.w,
                    self.help.h,
                ),
                // Theater mode keeps the screen for the window.
                alpha: if parked || !hints_on { 0.0 } else { 0.85 },
                tex: self.help.tex,
            },
        ];
        // The options menu is drawn again when something on it changed.
        if let Some(img) = self.menu.as_mut().and_then(|m| m.poll().then(|| m.compose())) {
            let slot = match self.menu_slot {
                Some((slot, size)) if size == (img.w, img.h) => slot,
                old => self.renderer.live_slot(old.map(|o| o.0), img.w, img.h),
            };
            self.renderer.write_live(slot, &img.px, img.w * 4, img.w, img.h);
            self.menu_slot = Some((slot, (img.w, img.h)));
        }
        if let Some(s) = &self.search {
            let n = s.hits.len();
            let found = match s.hits.get(s.sel).and_then(|&i| self.cards.get(i)) {
                Some(c) => {
                    let name = model.get(c.id).map_or(String::new(), |e| {
                        if e.title.is_empty() {
                            crate::model::shortcut_name(std::path::Path::new(&e.exe))
                        } else {
                            e.title.chars().take(60).collect()
                        }
                    });
                    format!("{}/{n}: {name}", s.sel + 1)
                }
                None if s.query.is_empty() => "type a window or app name".into(),
                None => "no match".into(),
            };
            let text = format!(
                "Search: {}_  \u{00b7}  {found}  \u{00b7}  Tab: next  \u{00b7}  Enter: use  \u{00b7}  Shift+Enter: focus  \u{00b7}  Esc: cancel",
                s.query
            );
            if s.hud.as_ref().is_none_or(|h| h.0 != text) {
                let img = crate::win::compose_hud_text(&[text.as_str()], self.text_px);
                let key = text.bytes().fold(1469598103934665603u64, |h, b| {
                    (h ^ b as u64).wrapping_mul(1099511628211)
                });
                let tex = self.renderer.upload_cached(HUD_IDS + 61, key, &img, false);
                let (w, h) = self.renderer.tex_size(tex);
                let hud = Hud {
                    tex,
                    w: w as f32,
                    h: h as f32,
                };
                if let Some(s) = &mut self.search {
                    s.hud = Some((text, hud));
                }
            }
        }
        if let Some((slot, (w, h))) = self.menu_slot.filter(|_| self.menu.is_some()) {
            let (w, h) = (w as f32, h as f32);
            hud.push(HudDraw {
                rect: rect(sw / 2.0, sh / 2.0, w, h),
                alpha: 0.97,
                tex: slot,
            });
        }
        if let Some((_, h)) = self.search.as_ref().and_then(|s| s.hud.as_ref()) {
            hud.push(HudDraw {
                rect: rect(sw / 2.0, sh * 0.03 + h.h / 2.0, h.w, h.h),
                alpha: 0.95,
                tex: h.tex,
            });
        } else if let Some((_, _, h)) = self
            .wf_hud
            .as_ref()
            .filter(|_| wf_name.is_some() && !parked)
        {
            hud.push(HudDraw {
                rect: rect(sw / 2.0, sh * 0.03 + h.h / 2.0, h.w, h.h),
                alpha: 0.9,
                tex: h.tex,
            });
        }
        if let Some(st) = &self.stats {
            if let Some(slot) = st.slot {
                let (w, h) = (st.size.0 as f32, st.size.1 as f32);
                hud.push(HudDraw {
                    rect: rect(16.0 + w / 2.0, 16.0 + h / 2.0, w, h),
                    alpha: 0.92,
                    tex: slot,
                });
            }
        }
        if !hints_on {
        } else if self.free {
            let hint = &self.hints[5];
            hud.push(HudDraw {
                rect: rect(sw / 2.0, sh * 0.03 + hint.h / 2.0, hint.w, hint.h),
                alpha: 0.9,
                tex: hint.tex,
            });
        } else if let Some(it) = &self.interact {
            let hint = &self.hints[4];
            // In theater mode the hint fades away after a few seconds.
            let alpha = if self.theater.is_some() {
                (0.9 * (4.0 - (now - it.since).as_secs_f32())).clamp(0.0, 0.9)
            } else {
                0.9
            };
            hud.push(HudDraw {
                rect: rect(sw / 2.0, sh * 0.03 + hint.h / 2.0, hint.w, hint.h),
                alpha,
                tex: hint.tex,
            });
        } else if let Some(c) = self.hover.and_then(|i| self.cards.get(i)) {
            let live = self.live.as_ref().is_some_and(|l| l.is_live(c.id));
            if live
                && (self.occluded.0 != c.id
                    || now - self.occluded.1 > std::time::Duration::from_millis(500))
            {
                let covered = c.hwnd.is_some_and(crate::win::chromium_occluded);
                self.occluded = (c.id, now, covered);
            }
            let covered = live && self.occluded.0 == c.id && self.occluded.2;
            if !parked && !self.press.as_ref().is_some_and(|p| p.dragging) {
                let hint = match self.hit {
                    Hit::Tool(t) => &self.tips[t as usize],
                    Hit::Axis(_) => &self.tips[Tool::Move as usize],
                    Hit::Card => {
                        &self.hints[match (c.kind, c.ghost) {
                            (Kind::Shortcut, _) => 2,
                            (_, true) => 1,
                            _ if covered => 6,
                            _ if frames.contains_key(&c.id) => 3,
                            _ => 0,
                        }]
                    }
                };
                hud.push(HudDraw {
                    rect: rect(sw / 2.0, sh / 2.0 + 30.0 + hint.h / 2.0, hint.w, hint.h),
                    alpha: 0.9,
                    tex: hint.tex,
                });
            }
        }

        let aspect = sw / sh.max(1.0);
        let proj = glam::camera::rh::proj::directx::perspective(self.fov, aspect, 0.05, 200.0);
        let view = glam::camera::rh::view::look_to_mat4(o, f, Vec3::Y);
        let time = (now - self.start).as_secs_f32();
        self.vp = proj * view;
        let t_draws = now.elapsed().as_micros();
        // Skip the frame if it would look the same as the last one. The hovered card's border
        // pulses: ~20 fps is plenty for that.
        let sig = {
            use std::hash::{Hash, Hasher};
            let mut v: Vec<u32> = Vec::with_capacity(20 + draws.len() * 13 + hud.len() * 6);
            let mut f = |x: f32| v.push(x.to_bits());
            self.vp.to_cols_array().into_iter().for_each(&mut f);
            o.to_array().into_iter().for_each(&mut f);
            for d in &draws {
                for p in [d.center, d.right, d.up] {
                    p.to_array().into_iter().for_each(&mut f);
                }
                [d.alpha, d.ghost, d.highlight, d.tex as f32].into_iter().for_each(&mut f);
            }
            for d in &hud {
                d.rect.into_iter().for_each(&mut f);
                [d.alpha, d.tex as f32].into_iter().for_each(&mut f);
            }
            let mut hs = std::collections::hash_map::DefaultHasher::new();
            v.hash(&mut hs);
            let scene = hs.finish();
            self.moving = scene != self.scene;
            if self.moving {
                self.scene_at = now;
            }
            self.scene = scene;
            hs.write_u64(self.live.as_ref().map_or(0, |l| l.fresh));
            hs.finish()
        };
        let since = now - self.drawn.1;
        let pulsing = draws.iter().any(|d| d.highlight > 0.0)
            && now - self.scene_at < std::time::Duration::from_secs(3);
        let dirty = self.renderer.take_dirty();
        self.rendered = dirty
            || sig != self.drawn.0
            || self.renderer.want_capture
            || since >= std::time::Duration::from_secs(1)
            || (pulsing && since >= std::time::Duration::from_millis(50))
            // An animated scene: about 30 frames a second while nothing else moves.
            || (self.renderer.animated() && since >= std::time::Duration::from_millis(30));
        if self.rendered {
            self.renderer.render(self.vp, o, time, &draws, &hud);
            self.drawn = (sig, now);
        }
        let t_all = now.elapsed().as_millis();
        if t_all > 60 {
            crate::store::log(&format!(
                "slow frame {t_all} ms (us): shell={t_shell} tick={t_tick} pre={t_pre} live={t_live} shape={t_shape} draws={t_draws}"
            ));
        }
        action
    }
}

/// Whether a Shift key is held right now.
/// How well lowercase query `q` matches a card (lower is better): the title, then the exe name;
/// a prefix, a word start, anywhere, then the letters in order.
fn search_rank(q: &str, title: &str, exe: &str) -> Option<u8> {
    let title = title.to_lowercase();
    let stem = crate::model::shortcut_name(std::path::Path::new(exe)).to_lowercase();
    let word_start = |s: &str| {
        s.match_indices(q).any(|(k, _)| {
            s[..k]
                .chars()
                .next_back()
                .is_none_or(|c| !c.is_alphanumeric())
        })
    };
    let subseq = |s: &str| {
        let mut it = s.chars();
        q.chars().filter(|c| !c.is_whitespace()).all(|c| it.any(|x| x == c))
    };
    for (k, s) in [&title, &stem].into_iter().enumerate() {
        let k = k as u8 * 3;
        if s.is_empty() {
            continue;
        }
        if s.starts_with(q) {
            return Some(k);
        }
        if word_start(s) {
            return Some(k + 1);
        }
        if s.contains(q) {
            return Some(k + 2);
        }
    }
    [&title, &stem]
        .into_iter()
        .position(|s| !s.is_empty() && subseq(s))
        .map(|k| 6 + k as u8)
}

fn shift_down() -> bool {
    (unsafe { GetAsyncKeyState(VK_SHIFT.0 as i32) } as u16 & 0x8000) != 0
}

/// Times the parts of a slow in-place tick (over 20 ms), logged when it ends.
struct SlowTick(Instant, Vec<(&'static str, u128)>);

impl SlowTick {
    fn new() -> Self {
        SlowTick(Instant::now(), Vec::new())
    }

    fn mark(&mut self, what: &'static str) {
        self.1.push((what, self.0.elapsed().as_micros()));
    }
}

impl Drop for SlowTick {
    fn drop(&mut self) {
        let all = self.0.elapsed().as_micros();
        if all > 20_000 {
            let parts: Vec<String> = self.1.iter().map(|(w, t)| format!("{w}={t}")).collect();
            crate::store::log(&format!("slow in-place tick {all} us: {}", parts.join(" ")));
        }
    }
}
