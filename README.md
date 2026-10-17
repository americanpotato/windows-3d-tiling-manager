# Window3D

A lightweight 3D window navigator for Windows, written in Rust (`windows-rs` + `wgpu`/DX12).

Window3D runs as a tiny background process with a global hotkey and a tray icon. Press the
hotkey to open a fullscreen overlay. In it you fly around a 3D room, and every open window
appears as a floating card textured with a saved screenshot. Click a window card to use
it in place (live view and sound on, your mouse and keyboard go to it; click outside it to fly
again); Shift+click or `Enter` hides the overlay and focuses the
real window. Press the hotkey again to return.

It is a navigator, not a window manager: real windows stay where they are on the desktop
(except in the opt-in fullscreen modes); the cards are its own 3D view of them.

## How it works

- **Idle:** a hidden message window, a `RegisterHotKey` hotkey, a tray icon and a
  `SetWinEventHook(EVENT_SYSTEM_FOREGROUND)` hook, with 0% CPU (purely event-driven) and a
  working set below 1 MB. With the default `keep_warm`, the GPU device, the card textures and the
  hidden overlay stay allocated (see below). With `keep_warm` off, no GPU device exists while idle.
- **Screenshots:** when a window loses focus, it is captured once (`PrintWindow` with
  `PW_RENDERFULLCONTENT`), downscaled to `screenshot_size`, and
  saved as QOI-compressed files under `shots`. Minimized windows that have no screenshot get a card with the app icon.
- **Overlay:** opening creates the swapchain on a fullscreen window, adds the card textures and
  draws the room. The window being left, and windows that don't have a screenshot yet, are
  captured in the background. Their cards show the previous screenshot (or the app icon) and
  are swapped in when the capture arrives, so opening never waits for a capture.
- **Open speed and GPU memory (`keep_warm`, `keep_surface`):** three tiers, measured on a
  Ryzen 780M with 54 cards (headless timings; showing the window adds about one frame):
  - `keep_warm` + `keep_surface` (default): at start-up, a background thread creates the wgpu
    device, uploads every card texture and builds the overlay once while hidden. Closing only
    hides ("parks") the overlay window and keeps its swapchain. New screenshots are composed
    into the parked overlay as they arrive. Opens take about 10 ms. The cost is about
    470-500 MB of idle private memory (mostly driver allocations for the device and the
    fullscreen swapchain), while the working set stays at about 2 MB.
  - `keep_warm` only (`keep_surface: false`): closing releases the swapchain and render targets
    but keeps the device and the texture cache. Only changed cards are recomposed. Opens take
    about 100 ms, for about 250-270 MB of idle private memory.
  - `keep_warm: false`: this matches "release all GPU memory when hidden". Every open creates
    the device and composes all cards, and closing drops everything. Opening takes about 1 s
    (mostly adapter and device creation). A few seconds after closing, the process is back to
    about 1 MB working set and about 20 MB private memory.
- **Ghost cards:** windows that have closed keep their card, dimmed and labelled "closed".
  Clicking a ghost relaunches the app. `Del` on a ghost forgets it. Only the newest `max_ghosts`
  ghosts are kept.
- **Shortcuts:** every item (shortcuts, programs, `.bat`/`.cmd`/`.ps1` scripts, documents, folders; not hidden or system files) from the data folder's
  `shortcuts\` directory, the Desktop (optional) and any `shortcut_dirs` appear on their own wall.
  Clicking one launches it without leaving the 3D view: the app opens behind the overlay
  (inactive, with other apps' focus-stealing locked out for a moment) and its window appears as a
  card right in front of you, first as a placeholder, then with its screenshot. New windows of the app and of
  anything it starts later (e.g. a game started from a launcher) keep coming in as cards until the
  overlay hides, and if one grabs the focus the overlay takes it back. The same goes for windows
  started from an app you are using in place on a card. In fact any window that comes up while the
  overlay shows gets a card in front of you (in theater mode, in front of the card you are looking
  at). If you were using a card in place, the new window is used in place there; if you were
  flying around (e.g. while a slow app starts), its card just appears ahead of you and the 3D view
  keeps the focus: click it to use it. Switching to another window (Alt+Tab, another window of the app
  you are using) doesn't hide the overlay: the camera flies to that window's card and uses it in
  place. Closing a window with its own close button removes its card. A new window that opens on
  another monitor while you are in the 3D view (cursor on its monitor) is moved to the 3D view's
  monitor, at the same spot (still maximized if it was); other windows are never moved.
- **Layout:** card positions and headings (and the last camera pose) are saved to `layout.json`.
- **Lock, sleep, monitors, GPU loss:** locking the PC (Win+L) closes an open overlay. Sleep,
  a monitor or resolution change and a DPI change also drop the parked overlay, the warm GPU and
  the kept adapter, which are made again on the next open (or, with `keep_surface`, by a new
  warm-up right after a monitor change or on resume). A lost GPU device (driver update or reset)
  is logged instead of crashing: the overlay closes and the next open uses a new device. At
  log-off or shutdown the taskbar and the cursors are put back. UAC prompts and
  virtual-desktop switches take the focus; if the focus stays on a window without a card for
  over a second (and Alt isn't held), the overlay takes it back.

## Build

```
cargo build --release
```

The binary is `target\release\window3d.exe`. It is a GUI-subsystem app (no console window).
Only one instance runs at a time. To start it with Windows, tick "Start with Windows" in the
tray menu.

`packaging\package.ps1` builds the release exe and writes `dist\Window3D-<version>.zip` (the exe and
`packaging\README.txt`, the short guide for players) for itch.io.

## Controls

| Input | Action |
|---|---|
| `Ctrl+Alt+Z` (configurable) | open/close the overlay |
| Tray icon: left click / right click | open the overlay / menu (open the data folder, edit settings, pick the monitor, stats window, start with Windows, quit). On start a tray balloon names the hotkey (a longer welcome the first time); `--quiet` (used by start with Windows) skips it |
| Mouse | look around (the cursor is locked; a crosshair selects cards) |
| `W` `A` `S` `D` / arrow keys | fly |
| `Space` / `E` | up |
| `Q` / `C` | down |
| `Shift` | faster |
| Mouse wheel | dolly forward/back |
| Left click on a card | window card: live view and sound on, then use the window in place: the real window gets the real keyboard focus (behind the overlay) and the real mouse: the overlay lets the mouse through while the cursor is on the real window, so every app (games' mouse-look included) gets real hover, clicks, wheel and motion exactly as in normal use, and the card shows a pointer where the cursor is on the window; you keep seeing the 3D view. Move the cursor off the window and click to fly again; minimizing the window also goes back to flying. The cursor is kept on the overlay's monitor meanwhile (unless the app confines it more tightly itself), so it doesn't stray onto another monitor by accident; moving the mouse on past any edge of the window frees it at once, with no resistance: the normal cursor appears just off the card over the 3D view and moves freely there and onto other monitors (where clicks work as usual, while the overlay stays up and you stay in the window). A click on the 3D view ends using the window; moving the cursor back over the card puts it back on the window at that spot. The cursor counts as on the window only where that window (or one of its menus/dialogs) is really what's under it; anywhere another window covers it, a click ends using the card instead of reaching the other window, and every edge of the window is a wall, enforced by a low-level mouse hook (the screen edge, the taskbar, other windows alike): the cursor doesn't cross onto what lies beyond the window; moving past the edge frees the cursor over the 3D view instead, and a click there ends using the window and never reaches what is under it. The Desktop card shows the primary monitor with its taskbar; the cursor moves freely over all of it, and where other windows cover the real desktop, left and right clicks and double-clicks are passed on to the desktop icons (so shortcuts open, icons can be selected and the background's right-click menu comes up); the middle button and the wheel do nothing there. While the desktop is used in place, its taskbar, Start menu, search, tray flyouts, quick settings, the Widgets board, taskbar previews and the desktop's right-click menu count as part of it and show inside the card instead of over the 3D view (pressing the Windows key while flying goes to the desktop card with Start open), and an app brought up from the taskbar (or any other window taking the focus) doesn't leave the 3D view: the overlay takes the focus back and flies to that app's card to use it there. Windows opened from the desktop come in as new cards. Programs listed in `focus_apps` are never used in place: the click hides the overlay and focuses the real window (or launch the shortcut / relaunch a ghost) |
| Shift + left click on a card | hide the overlay and focus that window |
| Hold left button and move | drag the card; the wheel pushes it away or pulls it closer while dragging |
| Card toolbar (icons beside the card under the crosshair; it stays 0.6 s so you can reach it) | grip: drag to move the toolbar, click to put it back · eye: live view on/off · speaker: let the app's sound through (window apps are muted while the 3D view is open, unmuted when it hides; Windows keeps an app's mute across restarts, so the apps muted are listed in `muted.json` until their sound is given back, also after a crash or power cut, and for an app closed while muted, as soon as it plays again) · resize: drag to scale (orange outline at the limit), click for the default size · move: shows red/green/blue arrows, drag one to move the card along that axis only (`Esc` hides them) · snap (on by default): the card glues edge to edge to its neighbours while it moves · rotate: drag to turn it (around the shared edge when glued, so a row bends into a curved wall), click to face you. All saved per card · close (✕, at the bottom): closes the window like its own close button (if it asks to save, the overlay hides and the prompt gets the focus); Shift+click ends the app (never Explorer). The card disappears (closing a window with its own close button does the same) |
| `Enter` | focus the window under the crosshair (like Shift+click; launches shortcuts and ghosts) |
| `Del` | forget the ghost card under the crosshair, or remove the empty template slot under it |
| `T` | tidy the shortcut wall: the Desktop folder card at the top, every item in rows below it, the live desktop card beside it. Dragging the Desktop folder card moves the whole wall |
| `Shift+T` | tidy the window cards: they go back on the rings around the room centre, open windows first (most recently used in front of you), ghosts after them; sizes are kept |
| `[` / `]` | fewer / more columns on the shortcut wall |
| `G` | add a 3x3 grid of template slots in front of you (transparent frames). Dropping a card on a slot fits the card to it (position, angle, size); the slot lights up while you drag over it |
| `K` | cycle through the open workflows: their cards and slots are highlighted and the name shows at the top (again past the last one = none) |
| `X` / `N` / `P` (with a workflow picked by `K`) | close the workflow (its slots and grouping go away, windows are untouched) / mute all its cards / toggle live view on all its cards |
| `/` then text | search: cards whose title or app name matches are ranked (title start, word start, anywhere, then letters in order; closer cards first) and the camera flies to the best one, highlighted, with the match shown at the top. `Tab` / `Down` / `Right` next match, `Shift+Tab` / `Up` / `Left` previous, `Backspace` edits, `Enter` uses it in place (like a click), `Shift+Enter` focuses the real window, `Esc` flies back. Flying keys, `Tab` and `M` are text while searching |
| `Home` | recentre the camera |
| `Esc` `Esc` (twice within 0.5 s) | close the overlay (a single `Esc` opens the options menu, so a stray press cannot drop you out) |
| `Esc` (once, while flying) | options panel: the cursor shows over it and everything is clickable. Renderer (automatic, power-saving or high-performance GPU, a graphics card by name, or the software renderer), anti-aliasing, field of view, mouse sensitivity, move speed, card titles, cards of closed apps (click a value, or its left arrow to go back), and the background as picture tiles: the grid room, plain colours, 8 animated skies, each meant for a mood (marked "Live": Twin moons, serene; Aurora meadow, wonder; Amber sea, warm nostalgia; Rain veil, cozy rain; Deep blue, calm under the sea; Salt mirror, light and joyful; Galaxy rise, awe; Snowfall, quiet and safe), 28 360° panoramas (first 15 open skies with nothing on the ground, then sunsets, a beach, cities, rooms; marked "360°", downloaded from Poly Haven, CC0, into the backgrounds folder the first time one is picked, 3-23 MB, and shown as soon as the download ends) and the 360° pictures in the backgrounds folder (the wheel scrolls them); flat pictures and the Windows wallpaper are only set in `config.json`; the background shows at once. The keys work too: `Up`/`Down` pick, `Left`/`Right`/`Enter` change. `Esc` or Done closes it and saves to `config.json` (renderer, anti-aliasing, titles and closed apps reopen the 3D view); `Esc` `Esc` or "Close the 3D view" closes the 3D view |
| `L` | make the window card under the crosshair live (streams the real window, e.g. a playing video); again to go back to the screenshot. Saved per card |
| `F` on a window card | theater: the camera flies up until the card's live view fills the 3D view, sound on, and you use the window there (keys, clicks and scrolling go to it); you stay in the app. `Esc`, the hotkey or a click outside it flies back |
| `Shift+F` on a window card | the same flight, then the overlay steps aside and the real window is made borderless fullscreen (real input, no added latency). The hotkey brings the 3D view back and gives the window its frame back |
| Hotkey, click outside the card or `Esc` `Esc` (while using in place) | back to flying; a single `Esc` goes to the window |
| `I` | stats panel (CPU/RAM of Window3D and the app, live-card delay and fps, switch and input latency) |
| `Tab` | free the mouse: mouse-look stops, the cursor shows and can go to another monitor; the overlay stays open there even when another window has the focus. Click the overlay or press `Tab` to fly again |
| `M` | move the open overlay to the next monitor (and remember it); it shows the same cards there |

**Multiple monitors.** The tray menu's "Open the 3D view on" picks a monitor (or the one under the
mouse, the default); it is saved as `monitor` in `config.json`. Whichever monitor it opens on, it
shows the same 3D view: every card, with the camera where it was left. One overlay is open at a time
(`M` moves it), so the other monitors show the real desktop.

**Stats window**: the tray menu's "Stats window" opens a small always-on-top window that you can drag to
any monitor. It shows Window3D's and the foreground app's CPU/RAM, the switch and input latencies, the
live cards' fps and capture delay (average and worst), and the 3D view's frame rate. Its right-click menu
ticks the lines to show (saved as `stats_fields`) or closes it.

**Workflows.** The tray menu's "Workflows" submenu saves the current arrangement under a name:
"Save windows + templates", "Save windows only" or "Save templates only". The name prompt accepts
`folder\name` to file it into a (new) folder. Saved workflows appear as a folder tree of submenus
(open ones are ticked); clicking one asks for confirmation, hides the overlay, relaunches missing apps
and moves every card to its saved place, angle, size and button state (live, sound, snap). Several
workflows can be open at once. "Open workflows folder" opens the folder in Explorer, where workflows
can be moved between folders. Chromium tab URLs are not saved (that would need the remote debugging
protocol).

**Live cards and in-place input (experimental).** Live cards use Windows.Graphics.Capture; only cards
you mark live are captured, and capture stops when the overlay hides (GPU memory is still released).
In-place input is posted to the window as messages (keys are posted as key-downs/ups, and the app turns
them into characters itself; mouse messages go to the child window real input would hit, skipping
input-transparent ones such as Chrome's compositor window). It is instant but not universal: `Ctrl`/`Alt`
shortcuts that rely on `GetKeyState`, games and other raw-input apps, and some browser content may
ignore it. The window used in place gets the real keyboard focus (the overlay only posts keys if that fails). Minimized windows produce no live frames, so they keep showing the screenshot.

**Mouse on live cards (experimental).** While you use a live card in place, the overlay is click-through whenever the real cursor is on the real window (hidden behind the overlay): the app owns the real cursor as in normal use, with no special cases for games or anything else. The system cursors are swapped for blank ones meanwhile, and a small window of Window3D's draws the shape the app chose at the matching spot on the card; leaving puts the cursors back (and after a crash, the next start does). Caveats: the pointer on the card moves at the real window's scale (faster on a card smaller than the window); parts of the real window off-screen or under another monitor's edge can't be reached; apps with their own (non-system) cursors show them at the real spot. While a card is in use, Windows' "hide pointer while typing" option is switched off for the session (not saved; put back on leaving, on exit and after a crash).

**Black or frozen live cards of Chrome/Edge.** Chromium browsers (and Electron apps) stop drawing a
window that other windows fully cover, to save power. Window3D's overlay doesn't count, but the other
real windows do: with two YouTube windows live, the one behind goes black. While the overlay is open,
Window3D works around it without moving anything: twice a second it gives each normal window that
covers a live browser card a window region with a 1-pixel hole, which Chromium treats as "not opaque",
so the browser keeps drawing. A maximized window can't keep such a region, so it is marked a tool window
instead (it keeps its taskbar button and Alt+Tab entry), which Chromium doesn't count as a cover either;
a maximized live card that overlaps a live browser card is marked ahead of time, so using it in place
never covers the browser (Chromium can take seconds to notice a cover is gone).
Both are undone as soon as the overlay hides. If a card is still covered, hovering it says so. The full
fix is on the browser side: start it with
`--disable-features=CalculateNativeWinOcclusion` (add it to the shortcut's target, after the closing
quote of `chrome.exe`/`msedge.exe`, and quit every browser window first so it takes effect).

## Configuration

Settings live in `config.json` in the data folder, which is `%APPDATA%\Window3D` (override it
with the `WINDOW3D_DATA` environment variable, for example for a portable setup). The file is
written with defaults on first run. Saved edits apply the next time the 3D view or the tray menu opens (a tray balloon confirms, or names the error and keeps the old settings); only `screenshot_size` needs a restart.

| Key | Default | Meaning |
|---|---|---|
| `hotkey` | `"Ctrl+Alt+Z"` | modifiers `Ctrl`/`Alt`/`Shift`/`Win` plus a key (`A`–`Z`, `0`–`9`, `F1`–`F24`, `Space`, `Tab`, …) |
| `mouse_sensitivity` | `0.0022` | radians per mouse count |
| `move_speed` | `6.0` | metres per second |
| `fov_degrees` | `70` | vertical field of view |
| `card_titles` | `false` | title strip (app icon and window title) above window cards |
| `show_hints` | `true` | on-screen help in the 3D view: the key list at the bottom and the hints over cards and tools (options menu: "Hints and tooltips") |
| `show_ghosts` | `false` | show cards for closed apps (their spots are remembered either way) |
| `max_ghosts` | `24` | ghost cards kept (oldest are evicted) |
| `shortcut_dirs` | `[]` | extra folders scanned for shortcuts |
| `focus_apps` | `[]` | exe names (e.g. `"javaw.exe"`) whose cards always focus the real window instead of being used in place |
| `include_desktop_shortcuts` | `true` | also scan the user and public Desktop |
| `max_shortcuts` | `200` | maximum number of shortcut cards (a saved 48, the old default, is raised to 200) |
| `screenshot_size` | `800` | longest side of stored screenshots, in pixels |
| `msaa_4x` | `false` | 4x MSAA (if supported; replaces `msaa`, which is ignored). Card edges are smoothed in the shader either way; MSAA also smooths room corners but takes about twice the GPU time and over 100 MB more memory on a large screen |
| `max_fps` | `60` | frame cap for the 3D view (`0` = uncapped); frames are only drawn when something changed, so a still view draws about 1 frame per second |
| `live_fps` | `20` | refresh rate of live window cards just off screen (their capture keeps running so they show at once when turned to; `0` = every frame); cards on screen and the card in use update at full rate (`max_fps`) |
| `background` | `"room"` | behind the cards: `"room"` (the grid room) or a plain colour `"#rrggbb"` (no walls or ceiling, only a faint floor grid that fades out; the room's limits still hold the camera and cards). Or a picture: `"wallpaper"` (the Windows desktop picture, or its solid colour when there is none) or a JPG/PNG/BMP/WebP path; a bare file name is looked for in `%APPDATA%\Window3D\backgrounds` (tray menu: "Open backgrounds folder"), and 360° pictures put there are offered in the options menu. A picture about twice as wide as high is a 360° panorama (equirectangular, as from phone "photo sphere" modes or Poly Haven) drawn all around you; any other picture is a fixed backdrop filling the screen. Or an animated sky drawn by a shader, all around you with a ground or sea below: `"scene:twin_moons"`, `"scene:aurora_meadow"`, `"scene:amber_sea"`, `"scene:rain_veil"`, `"scene:deep_blue"`, `"scene:salt_mirror"`, `"scene:galaxy_rise"` or `"scene:snowfall"` (an unknown scene falls back to the grid room) (it keeps the 3D view redrawing about 33 times a second, about 9 ms of GPU time per frame at 2880x1800 on an integrated GPU). Pictures are slightly darkened so cards stand out, and load in the background (an unreadable one falls back to a dark plain colour). The options menu offers a few colours, the 8 animated skies and 28 360° panoramas from Poly Haven (CC0), saved in the backgrounds folder as `"<name> (360).jpg"` when first picked |
| `gpu` | `"low-power"` | `"low-power"` (integrated GPU, wakes fastest), `"high-performance"`, `"default"` or `"software"` (Windows' software renderer, slow; used anyway when no DirectX 12 GPU is found) |
| `keep_warm` | `true` | keep the GPU device and card textures between opens: about 100 ms opens instead of about 1 s, at the cost of about 250 MB of idle private memory (see above) |
| `keep_surface` | `true` | only used with `keep_warm`: keep the hidden overlay window and its swapchain between opens, giving about 10 ms opens for about 220 MB more idle private memory |
| `desktop_card` | `true` | a live card of the real desktop (the primary monitor: wallpaper, icons and taskbar) next to the Desktop folder card; clicking it uses it in place |
| `show_shortcuts` | `false` | show the wall of shortcut cards (shortcuts are still scanned while it is hidden) |
| `stats_fields` | all | lines of the stats window: `own`, `app`, `switch`, `input`, `live`, `view` (its right-click menu edits this) |
| `monitor` | `"cursor"` | monitor the overlay opens on: `"cursor"`, `"primary"` or a number from 1 (as in the tray menu) |
| `focus_mode` | `"none"` | how a clicked window is sized: `"borderless"`, `"maximize"` or `"none"` |
| `keep_gpu_adapter` | `false` | only used when `keep_warm` is off: keep the (resource-free) adapter between opens, giving about 0.4 s opens for about 30 MB of idle RAM |

Data folder contents: `config.json`, `layout.json`, `shots\<id>.qoi` (screenshots),
`shortcuts\` (drop shortcuts here), `workflows\<folders>\<name>.json` (saved workflows), `muted.json` (apps to unmute, while any) and `log.txt`.

## Self-test

```
window3d.exe --selftest <dir>
```

This runs headless: the overlay window is created but never shown, and the self-test never
takes focus or locks the cursor. It opens and closes the 3D view three times, flies the camera,
exercises picking and dragging, and writes the following to `<dir>`:

- `selftest.txt`: timings and memory readings
- `selftest_*.png`: rendered frames
- `closeup_<id>.png`: rendered close-ups of each window card
- `card_<id>.png`: card textures

Combine it with `WINDOW3D_DATA=<dir>` so that it doesn't touch your real layout.
