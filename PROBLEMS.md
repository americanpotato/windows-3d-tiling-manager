# Known problems and what has been tried

Notes from hand-testing, so the same dead ends aren't tried again. Newest status first in each
section. Code pointers: `src/overlay.rs` (`begin_interact`, `pass_input`), `src/win.rs`
(`forward_key`, `forward_mouse`, `punch_hole`, `real_button`, `force_foreground`, `real_key`).

## Background: why "use in place" is hard

The overlay is a fullscreen topmost window. The real window sits behind it, so by default
Windows sends all real input to the overlay. The overlay has to pass input on, and apps differ
in what they accept:

- **Classic Win32 apps** (Notepad, Explorer's file list) accept posted messages (`PostMessage` of
  `WM_KEYDOWN`, `WM_LBUTTONDOWN`, ...).
- **Chromium apps** (Chrome, Edge, Electron, VS Code) drop a scroll if `WindowFromPoint` at
  the scroll point isn't their own window. Here that window would be the overlay.
- **WinUI/XAML parts** (Windows 11 Explorer's command bar, address bar, tabs, menus) read input
  from the system input stack and ignore posted mouse messages.
- **Apps that check `GetKeyState`** (many Ctrl/Alt shortcuts), **games and raw-input apps**
  never see posted keys as held keys.

> **Past problems (obsolete, kept for reference).** The sections Keys, Scroll, Clicks, Hover
> and "Options for input overall" below describe the old approach of posting input to the
> window. It was replaced by giving the window used in place the real focus and making the
> overlay click-through while the cursor is on it (see "Games that grab the mouse, used in
> place"), so real keys, clicks, wheel and hover reach every app natively. The open items there
> (real-keys mode, XAML clicks, Chromium scroll through a hole) no longer apply.

## Keys (typing)

| # | Approach | Result |
|---|----------|--------|
| 1 | Overlay keeps the keyboard focus. Each key is posted to the focused control of the target's thread (`forward_key`); the app's own `TranslateMessage` turns key-downs into characters, so our `WM_CHAR` is dropped (posting it too would type everything twice). | **Works** (Notepad, Explorer). Ctrl/Alt shortcuts that read `GetKeyState` may fail. **Current approach.** |
| 2 | `begin_interact` gives the real window the focus with a plain `SetForegroundWindow`; keys go to it natively; posting (1) as the fallback. | In practice `SetForegroundWindow` was usually refused (log: "could not give the window the focus"), so the fallback (1) ran and keys worked. |
| 3 | Force the focus switch (`force_foreground`: `AttachThreadInput` with the foreground thread, plus a `SendInput` of unassigned VK `0xE8` to unlock the foreground switch), then `SendInput` keys (`real_key`). | **Broke typing.** The switch succeeded, but keys going to the window behind the topmost overlay didn't register. Reverted; `real_key` is kept unused. |

Possible next steps: an opt-in "real keys" mode per app (option C below), or hiding the overlay
while a window is used (option D).

## Scroll

| # | Approach | Result |
|---|----------|--------|
| 1 | `SendMessageTimeout` of `WM_MOUSEWHEEL` (screen coordinates) to the child under the point. | Worked for classic apps; Chromium ignored it (the point is over the overlay). |
| 2 | While the target had the focus (keys approach 2/3), the overlay `EnableWindow(false)`'d itself during the synchronous wheel send, so `WindowFromPoint` skips it. | **Worked.** Only ran when the overlay was *not* foreground; disabling the focused overlay would take its keyboard focus. |
| 3 | After going back to keys approach 1 the overlay is always foreground, so (2) never ran. | **Scroll broke.** |
| 4 | `punch_hole`: during the wheel send the overlay gets a 5 px hole at the scroll point (`SetWindowRgn`), then the region is cleared. The focus is untouched. | **Current approach.** Not hand-tested yet. Watch for flicker under the cursor. |

## Clicks

| # | Approach | Result |
|---|----------|--------|
| 1 | `PostMessage` of button messages to the deepest child that real input would hit (`ChildWindowFromPointEx`, skipping `HTTRANSPARENT` ones such as Chrome's compositor child). Double-clicks are sent as `WM_*DBLCLK` to classes with `CS_DBLCLKS`. | Works for classic controls and Explorer's file list. Explorer's command bar and menus (XAML) ignore it. |
| 2 | `real_button` for XAML targets (`is_xaml`: class contains InputSite / SiteBridge / ContentBridge / Xaml, or starts with `Windows.UI.`, or its parent's does): punch a hole, check that the `WindowFromPoint` root is the target, `SendInput` an absolute move + button, wait 15 ms, move the cursor back, restore the clip, close the hole. | Only runs when the target is foreground. Since keys went back to approach 1, the target isn't foreground, so it **doesn't run and XAML clicks don't register**. |

Option B (proposed, not done): for an XAML click only, hand the target the focus, do the real
click, then take the focus back to the overlay.

## Hover

Posted `WM_MOUSEMOVE` doesn't make Chromium show hover states or change the cursor. Done
(`pointer.rs`, `win::park_cursor`): while a card is used in place the real cursor is parked on the
real window behind a 1-pixel hole in the overlay (checked with `WindowFromPoint`, then held with a
1x1 `ClipCursor`); raw input (`RIDEV_INPUTSINK`) moves the card pointer, and each move re-parks the
cursor. The system cursors are blanked with `SetSystemCursor` (copies kept), and a layered topmost
window draws the shape from `GetCursorInfo` at the card point. Restore: `SPI_SETCURSORS` on leaving,
on exit, in the panic hook, and at start if the `cursors-hidden` marker file exists.
Known gaps: no pointer acceleration; covered spots fall back to messages; custom app cursors aren't blanked.

Flicker while typing: with "hide pointer while typing" (`SPI_GETMOUSEVANISH`) on, each key clears
`CURSOR_SHOWING`, so `pointer::show` hid the drawn pointer until the next mouse move. Fix:
`hide_cursors` saves the setting and turns it off with flags 0 (session only, not persisted);
`restore_cursors`/`panic_restore` put it back, and the `cursors-hidden` marker holds a `v` so
`recover` does too after a crash. Belt and braces: `pointer::typed()` (called for every key in
`pass_input`) keeps the last drawn shape for 1.5 s even if an app hides the cursor itself.

## Options for input overall

- **A.** Posting for keys and clicks (current). Typing works; some shortcuts, Chromium and XAML
  parts may ignore it.
- **B.** As A, but give the window the focus just for XAML clicks, then take it back.
- **C.** A per-app setting for "real input" apps.
- **D.** Hide the overlay while a window is used, so it gets real input natively (the 3D view
  isn't visible meanwhile; `F` / `Shift+F` fullscreen already works this way).

## Live cards go black (Chrome/Edge)

Reported: two YouTube videos as live cards; the one whose window isn't in front goes black.

Cause: Chromium's native window occlusion tracking. It walks the real z-order and, when other
opaque windows fully cover a browser window, marks it hidden and stops drawing it (videos may
keep playing, but no frames are produced, so Windows.Graphics.Capture delivers black or a
frozen picture). It skips tool windows, `WS_EX_TRANSPARENT` and see-through layered windows, so
the overlay itself doesn't count; the other browser window does.

What's done: `win::chromium_occluded` repeats that test for the hovered live card (class
`Chrome_WidgetWin_1`, twice a second) and the hover hint then explains it and names the fix.
Also twice a second while the overlay is open, `win::uncover_chromium` gives every window above
a covered live browser window a huge window region with a 1-pixel hole at (0,0). Chromium skips
windows with a complex region, so the browser counts as visible again and keeps drawing. Own
windows, the taskbar and windows that already have a region are left alone. `Overlay::unshape`
removes the regions in `park`, `into_gpu` and on hide. Verified: a test page in a non-maximized
covered Chrome window went hidden → visible → hidden on restore.

Not solved: a **maximized** covering window drops the region at once (`GetWindowRgn` returns
nothing right after `SetWindowRgn`), so a browser covered by a maximized window stays black.
Untested idea: temporarily add `WS_EX_TRANSPARENT` to the covering window (Chromium skips those)
plus `SetWindowPos(…FRAMECHANGED)`, and restore the style on hide. Risky, because it changes
input behaviour of a real window if the restore is ever missed (crash).

Options not taken: raising or moving the covered window (the app never moves real windows).
The real fix is the browser flag `--disable-features=CalculateNativeWinOcclusion` (README).

Self-test: `selftest::occlusion_test` uncovers covered Chromium windows and reports their titles
(a test page can put `document.visibilityState` there). It touches real windows, so it only runs
with `WINDOW3D_OCCLUSION_TEST=1` set.

## Crashes

The release build uses `panic = "abort"`. A panic shows up in Windows Error Reporting as
`0xc0000409` (fast-fail 7). `main.rs` installs a panic hook that writes `PANIC: <info>` and a
backtrace to `%APPDATA%\Window3D\log.txt`. Check there first.

| Crash | Cause | Fix |
|-------|-------|-----|
| Index out of bounds in `Overlay::frame` (len 7, index 7) | Hovering the toolbar's "Room" button (shown only with several monitors), which had no tooltip | Added the tooltip; `tips` is now `[Hud; Tool::ALL.len()]`, so a missing tooltip fails to compile |

## Debugging tips

- The log is `%APPDATA%\Window3D\log.txt` (or the folder in `WINDOW3D_DATA`).
- Self-test: `window3d.exe --selftest <dir>` (with `WINDOW3D_DATA=<dir>`), results in
  `<dir>\selftest.txt`. It doesn't cover in-place input; that needs testing by hand.

## Slow hotkey with live cards

Opening took 0.4–1 s with 6 live cards: the frame drawn before showing the parked overlay called
`StartCapture` for each live window (~150 ms each), and hiding closed the sessions and gave back
the `uncover_chromium` regions (each `SetWindowRgn` waits for the other process). Now captures are
opened on worker threads (`Live::start`, cards show their screenshot until the first frame), closed
on a worker when dropped, regions are given back on a worker (`win::unshape_wait` at exit), and the
first region check waits 300 ms after opening. Frames over 60 ms are logged as `slow frame` with
their phases.

## Launching from the overlay

Wanted: a shortcut launched from the 3D view should show up in the room, not take the user out of
it. Done (`App::poll_launch`, `win::launch_behind`): `ShellExecuteExW` with `SW_SHOWNOACTIVATE`,
no `AllowSetForegroundWindow`, and `LockSetForegroundWindow(LSFW_LOCK)` before it (lifted with
`LSFW_UNLOCK` when the overlay hides; Windows also lifts it on Alt/click). The
started process id is kept (`SEE_MASK_NOCLOSEPROCESS`). Until the overlay hides (see the MultiMC note below)
the window list is polled every 250 ms; each new top-level window of
the launched process or its descendants (`win::process_tree`, Toolhelp parent ids, so launcher
stubs that exit still count) gets its entry moved 3.2 m in front of the camera (side by side if
several), a placeholder card, a capture, and a second capture 1.5 s later. When the shell gave no
process (file handed to an app already running) any new switchable window counts. If such a window
takes the foreground anyway (Electron/Chromium force it with a synthetic Alt, which lifts the
lock), `on_foreground`/`Activated(false)` call `force_foreground` on the overlay instead of hiding.
Other windows (Alt+Tab to an existing window, Start) still hide the overlay.

First try (10 s watch, any new window) failed with GDLauncher: no window was placed and the overlay
hid. Most likely the launcher took longer than 10 s to show its window, so the watch had ended when
it grabbed the focus. The log now records the launch pid, every focus take-back, a hide during a
launch (with the window title) and the end of the watch, so a repeat can be diagnosed.
Known gaps: a splash screen gets its own card (it becomes a ghost when it closes).

MultiMC report: MultiMC came in as a card fine, but launching Minecraft from it took the user out
of the overlay. The watch had ended 4 s after MultiMC's window was placed, and using a card in
place always hid the overlay when another window took the foreground. Now the watch lasts until
the overlay hides: for a known pid the whole process tree counts for the whole session (only the
"any new window" fallback keeps the 60 s limit). Using a card in place adopts that window's process
(all windows that already exist are marked as known), so windows it starts later become cards too.
If such a window takes the foreground while a card is used in place, the in-place use ends and the
overlay takes the focus back instead of hiding; after a take-back the foreground lock is set again.
MultiMC's real window opened on the second monitor: that is its own saved position, Window3D never
moves real windows.

## Games that grab the mouse, used in place

Report: Minecraft (1.8.9, LWJGL 2) used in place on its card; once in a world the view span and
was pulled downwards. In mouse-look mode the game puts the cursor back at its window centre (and
clips it) every frame and reads the offset as motion, while the in-place mode holds the real cursor
at the parked point on the window (`win::hold_cursor`): each side kept undoing the other, so the
game saw a constant offset. Now `hold_cursor` reports when it had to put the cursor back; 8 times
within 1 s (no normal window moves the cursor) means a game is grabbing the mouse, and the overlay
hands over to the real window like Shift+click (`Action::Focus`: overlay hides, the game gets the
focus, the hotkey brings the overlay back). Known gap: with the cursor freed (Tab) the cursor is not
parked, so this check does not run.

Second report: still span; the check never fired. Cause, most likely: LWJGL 2 in mouse-look reads
every WM_MOUSEMOVE as motion from its client centre (it puts its own cursor back there after each
poll), but in place the cursor sat at the pointer's spot (parked) or moves were posted at the
pointer's spot, so it saw the same offset every frame. A first fix (game classes always focus the
real window) took the user out of the 3D view, which they did not want. Now: windows of
`win::GAME_CLASSES` (LWJGL, GLFW30, SDL_app, UnityWndClass, UnrealWindow) used in place watch the
cursor (`win::foreign_cursor`: hidden, or not a system cursor; ours are the blanked system ones).
While the game hides it (mouse-look), the real cursor is parked at the game's client centre and
each frame's raw motion is posted as a move to centre + motion (`win::game_motion`); real clicks
go through the hole at the centre. When the cursor is a system one again (menus) the normal pointer
comes back. Log lines: "card click: class ..., exe ...", "game in place: mouse-look on/off".
Untested guesses: other engines may read raw input instead (then motion would not arrive at all);
`focus_apps` lists exes to always focus for real.

Third report: the game's pause menu "Back to Game" did nothing. Likely cause: the game was not the
foreground window (the overlay was), so it paused itself again as soon as it resumed (Minecraft
pauses on focus loss). Now a game used in place gets the real foreground focus at the start
(`win::force_foreground`, log "game used in place: it gets the real focus"); the overlay polls `Esc`
with `GetAsyncKeyState` since it no longer receives keys. To let `Esc` reach the game, leaving needs
`Esc` twice within 0.5 s (`DOUBLE_ESC`), everywhere except theater and the move gizmo. Watch for:
mouse messages landing on the topmost overlay instead of the game, and LWJGL's `ClipCursor`
fighting `hold_cursor`.

Fourth report: mouse-look worked but felt slow and jittery, and leaving the inventory jolted the
view before mouse-look was noticed. Cause: posted WM_MOUSEMOVEs cannot be kept in step with the
game's frames (LWJGL reads each move as an offset from its last position and re-centres only once
per game frame), so motion was lost unevenly; a feed thread sending one move per game frame was
tried and was still not right. The user wanted one mouse approach for every app, no game hacks.

Now (one rule for every app, no game detection, no grab detection): the window used in place gets
the real foreground focus (`force_foreground`), so keys reach it natively; `Esc Esc` is polled with
`GetAsyncKeyState`. The real cursor is never parked, held or moved by us after the start (it starts
at the spot under the crosshair): while it is on the real window, the overlay is click-through
(`win::click_through`: WS_EX_LAYERED | WS_EX_TRANSPARENT at alpha 255) and the window gets the real
mouse as in normal use - hover, clicks, wheel, and games' mouse-look with their own sensitivity.
Each frame the overlay maps the cursor to the card (`win::window_uv` -> `card_screen`) and draws the
pointer there. Off the real window the overlay takes the mouse back; a click there ends using it.
Removed: posting mouse messages, the 1-pixel hole/parking, `hold_cursor`, game classes.
Risks to watch: a flip-model DX12 swapchain on a window made WS_EX_LAYERED may stop presenting or
go black (fallback: wgpu's Dx12 DxgiFromVisual presentation); focus not moving (log line "window
used in place gets the real focus: false"; the overlay then still posts keys).

### Using the Desktop card in place clicked windows on the second monitor
The log (`in-place click` lines) showed the Desktop card's window (`Progman`) spans the whole
virtual screen `(0,0)-(5440,1800)`, so the cursor clip let the cursor onto the second monitor, and
"on the window" was a rectangle test: over the desktop's rectangle the overlay let clicks through,
and they landed in Chrome sitting on top of the desktop. Fix: the cursor is on the window only when
the top window under it (`win::window_at`, skipping Window3D's own) is part of it
(`win::part_of`: itself, owned windows, its menus/tooltips, `WorkerW` for the desktop); off the
overlay's monitor, other windows are a wall: the cursor is held at its last spot on the window
(`Interact::last_ok`) and pushing on moves the pointer off the card; a click then ends using it.
First try warped the cursor onto the overlay's nearest point instead: for a window on the second
monitor that bounced the cursor between monitors (jumping, two cursors).

### The per-frame wall was janky and let clicks through
On the second monitor, at the bottom edge of a Chrome window, the cursor slipped onto a taskbar
flyout (`XamlExplorerHostIslandWindow`) and clicked it: the wall only put the cursor back once per
frame, so between frames it could cross the edge and a click landed there, and the snapping back
felt jerky. Fix (`src/wall.rs`): while a card is used in place, a `WH_MOUSE_LL` hook on its own
thread blocks real moves to points that are neither on the overlay nor part of the window, slides
the cursor along the edge instead, and sums the blocked motion as the push that moves the pointer
off the card. A click off the overlay while the pointer is off the card is swallowed (with its
button-up) and ends using the window. The hook is installed only while a card is in use.

### The bottom edge jerked while the other edges were hard walls
With Chrome maximized on the overlay's monitor, left/right/top are screen edges (the cursor stops
and the push builds up), but below the window the taskbar lies under the overlay, and the wall let
the cursor onto any point on the overlay: it crossed into the taskbar strip (and Chrome's invisible
resize border), the overlay flipped from click-through to solid a frame later, and the pointer ran
off the card in a different way than at the other edges. Fix: the wall allows only points where
the window (or its menus/dialogs) is what is there, inside its visible frame (`win::in_frame`),
overlay or not, so every edge works the same; a cursor already off the window moves freely.

### The pointer flickered on and off at every edge
The log showed `in-place cursor off/onto the window` many times a second at an edge, with a push of
1 to 3 pixels: one pixel bumped into the wall pushed the pointer's spot past the edge (off), and the
next move along the wall reset the push (on), each time switching the overlay's click-through and
the pointer. Fix: being on the window no longer depends on the push below `PUSH_OFF` (the pointer
stays at the edge until pushed past it), and the hook only takes the push back as the cursor moves
away from the wall, not along it.

### The Desktop card was stretched over both monitors, without its taskbar
`Progman` spans the whole virtual screen (5440x1800 here), so the card covered both monitors, with a
white dead area where the second monitor is shorter, and the taskbar (`Shell_TrayWnd`, a window of
its own) was missing. Windows captures one window at a time; a monitor capture would contain both,
but also the overlay itself and every open window, so it was not used. Fix: the desktop's live
stream crops Progman's frames to the primary monitor and draws a second capture, of the taskbar,
over them at its real place (`live.rs`, `Desktop`); the saved screenshot does the same with two
`PrintWindow` calls (drawing the taskbar through `SetViewportOrgEx` doesn't work: PrintWindow
ignores the offset, so it is printed apart and copied in). `frame_bounds` gives the primary monitor
for Progman, so pointer and click mapping match the card. The desktop is shot again on every open
(it never loses the focus like other windows, so old shots stayed). The black background was not a
bug: the wallpaper setting is a solid black colour (`BackgroundType` 1, `Background` `0 0 0`).

### Using the desktop in place, a click on the taskbar closed the 3D view
The taskbar, Start menu and search take the foreground, which ended in-place use by hiding the
overlay. Now `part_of` counts the shell windows (`Shell_TrayWnd`, `Shell_SecondaryTrayWnd`,
`XamlExplorerHostIslandWindow`, `TopLevelWindowForOverflowXamlIsland`, the `CoreWindow`s of
StartMenuExperienceHost/SearchHost/ShellExperienceHost, the taskbar's own menus) as part of the
desktop, and any other window taking the foreground meanwhile ends in-place use, takes the focus
back and flies to that window's card (`Overlay::switch_from_desktop`) instead of hiding.

### Start, search and taskbar previews showed over the 3D view, not in the desktop card
The shell's popups are topmost windows of other processes, so they drew over the overlay, and
the taskbar was raised over it whenever Start opened. Findings: `EnumWindows` from this process
does not list the immersive windows (Start/search `CoreWindow`s, even `Shell_TrayWnd`); a
`FindWindowExW(None, prev)` walk does (`win::for_each_top`). Making a shell window
`WS_EX_LAYERED` with alpha 1 from outside (`SetWindowLongW` + `SetLayeredWindowAttributes`) hides
it on screen while Windows.Graphics.Capture still gets its full content (premultiplied alpha,
transparent around the menu). So while the desktop card is in use, `win::hide_shell` makes every
shell popup and the taskbar alpha 1 every 50 ms, `live` captures the visible popups on the primary
monitor and composites them over the desktop and taskbar in the card, and `win::show_shell` gives
them back afterwards (open popups once they cloak, the taskbar once Start/search are gone, at most
1.5 s). A panic hook and `win::recover_shell` at startup undo a hide left behind by a crash.
Pressing Win while flying flies to the desktop card instead of hiding the overlay.

### Invisible walls on the desktop card; the Widgets board opened on the real screen
The desktop card shows the bare desktop, but the wall hook held the cursor to the parts of the
real desktop no window covers, so a maximized app above the taskbar was a wall nobody could see.
Now, using the desktop, the whole primary monitor is free (`wall::desk_free`) and clicks and the
wheel on covered parts are swallowed instead (logged as "swallowed on a covered part of the
desktop"). The Widgets board (`WindowsDashboard` of Widgets.exe, and the windows of its WebView2,
found by the process' parents) counts as a shell popup, so it shows in the card too. So does
quick settings (`ControlCenterWindow` of ShellHost.exe, behind the sound/network icons).

### Using a card in place kept the cursor off the second monitor

The cursor was confined to the overlay's monitor while a card was used in place, and a window on another monitor taking the foreground ended the in-place use. Now pushing the mouse off the left or right side of the card (the pointer travels on to the screen edge first, so it doesn't visibly skip; where no monitor is beyond, it stops at the edge) moves the cursor onto the monitor on that side: the wall hook stands down, the confinement is released and the normal cursor is back, and a window on another monitor taking the foreground leaves the overlay and the in-place use alone. Once the cursor comes back onto the overlay's monitor it is put on the window's nearest edge and the walls are back. The log records "in-place cursor over to the monitor on the ...", "in-place cursor back from another monitor" and "a window on another monitor took the foreground".

### Pointer off the desktop card: clicks swallowed, top/bottom stop short

With the Desktop card in use, a click with the pointer pushed off the card landed where another window covers the real desktop, and the covered-desktop rule swallowed it instead of letting it end using the card. Off the card a click is now the overlay's again. Pushing off the top or bottom of a card now also takes the pointer on to the top or bottom of the screen (where it stops) instead of only ~150 pixels past the card.

### Pointer jumping near the card's edges

Past a card's edge two pushes were added up: the wall hook's and the held cursor's (from raw mouse input), which see the same motion, so the pointer ran off the card at double speed, and coming back the cursor left the edge at once, dropping one push and making the pointer jump back. Once the pointer could go on to the screen edge the jumps got large. Now the hook's push counts wherever the hook holds the cursor (the held push follows it), and moving back keeps the cursor at the wall until the push is taken back, so the pointer comes back onto the card steadily before the cursor moves again.

### Pointer locking up near the card's edges

Letting the push past a card's edge grow until the pointer reached the screen edge (and holding the cursor at the wall until the push was taken back) left large pushes behind that held the cursor in place. The push is back to its small range; instead it is spread over the distance from the card's edge to the screen edge, so a full push puts the pointer at the screen edge (and sideways on to the next monitor). While the pointer is off the card the log records "in-place push: ..." lines (cursor, push, wall push, pointer) five times a second.

### Resistance at a card's edge

Even spread over the distance to the screen edge, the push past a card's edge still felt like the pointer was held back. The push is gone: as soon as the mouse moves past the window's edge, the cursor is freed just outside the card where the pointer was (the wall hook stands down, the confinement is released, the normal cursor is back), so it moves freely over the 3D view and on to other monitors. A click on the 3D view ends using the window; moving back over the card puts the cursor back on the window at that point. The log records "in-place cursor off the card at ..." and "in-place cursor back onto the card at ...".

### Desktop icons not clickable in place

Using the Desktop card in place, the taskbar worked but the desktop's shortcut icons and the background's right-click menu didn't: the real desktop is usually covered by other windows, and the mouse hook swallowed every click on a covered point. Now a left or right press (or a double click, by the system's double-click time and distance) on a covered point is posted to the desktop's icon list view (the SysListView32 under SHELLDLL_DefView, under Progman or a WorkerW) in its client coordinates, along with the matching release and the moves in between (for selecting and dragging); the real click is still swallowed, so the covering window never gets it. The log says "in-place: N click(s) passed on to the desktop icons".

### Desktop menu over the 3D view, minimize leaving the 3D view

The desktop's right-click menu is a WinUI popup of explorer.exe ("Microsoft.UI.Content.PopupWindowSiteBridge", focus on "XamlExplorerHostIslandWindow_WASDK"), which wasn't on the list of shell popups, so it showed over the 3D view instead of in the desktop card. While the desktop is used in place, those windows and explorer's classic "#32768" menus now count as shell popups (made invisible on the screen, shown in the card); at other times they don't, since File Explorer windows use the same menus. Minimizing a window used in place made Windows hand the foreground to the next window, which counted as the user going elsewhere and hid the overlay; now, if the window used in place is minimized, it is back to flying in the 3D view (log: "in-place use ended: the window was minimized").

### Guessing from the foreground change; stutter using the desktop in place

The app only listened for the foreground changing and guessed what had happened, so a minimize or an unknown popup taking the foreground looked like the user leaving and hid the overlay. While the overlay is open it now also listens for windows starting to minimize and for top-level windows showing or uncloaking (the hooks are removed when it hides, so idling costs nothing). Minimizing the window used in place goes back to flying as it starts; a popup (no caption) of the desktop's process that comes up within 1.5 s of a click passed on to the desktop icons counts as part of the desktop whatever its class (log: "a desktop popup came up"), and shell popups are hidden as they show instead of at the next sweep. The one remaining guess, hiding the overlay when another window takes the foreground, logs both windows' class, program, styles, minimized state and owner, so a wrong guess can be spotted and fixed from the log.

Slow frames (75-100 ms) while using the desktop in place came from the search for shell popups, run every 50 ms on the render thread: about 80 ms the first time (process lookups) and 4-7 ms after that. Process paths are now cached, the caches are warmed on a background thread when the overlay opens, and the sweep runs every 250 ms (about 2-3 ms). Slow-frame log lines now also give shell= and tick= times. The self-test reports the search time ("shell popup search") and checks that the window events arrive, using its own off-screen test window ("window events test").

### Slow frames with live cards

The remaining slow frames (60-200 ms, logged as live=...) came from the live view, all on the render thread: each frame was copied to a staging texture and read back at once, which waited for the GPU; the first frame of a big window (2880x1704) made a new D3D11 staging texture and a new wgpu texture (about 30-50 ms each); and the D3D11 capture device was made on the first frame that needed it (about 60-110 ms). Now the copy is read on the next poll without waiting (if the GPU isn't done, the poll skips it), the staging and wgpu textures for a new size are made on worker threads while the frame waits, and the capture device is made on a worker (with COM initialised there: without it the captures it opened never delivered a frame). A live poll over 20 ms is logged ("slow live poll N ms: card ... upload ..."). Still on the render thread: writing each new frame into its texture (about 8-20 ms for a 2880x1704 window) and composing the desktop card (about 20 ms per new frame). The self-test captures its own window live ("live test": first frame, frames read, slowest poll).

### Lock, sleep, monitor changes and a lost GPU

Nothing handled these. A lost device (driver update, TDR, sometimes after sleep) went to wgpu's default error handler, which panics, so the app aborted; the parked overlay kept the size and room of a monitor that might be gone; and Win+L left the overlay open (and the cursor clipped) behind the lock screen. Now the message window gets WM_WTSSESSION_CHANGE (lock: close the overlay), WM_POWERBROADCAST (suspend: close and drop all GPU state; resume: warm up again), WM_DISPLAYCHANGE and the overlay's WM_DPICHANGED (close, drop, warm up again) and WM_ENDSESSION (show the taskbar, restore the cursors). The device has a lost callback and an error handler that log (validation errors at most 20 times) and set a flag. The flag is checked each frame and on open, and the overlay is rebuilt on a new device. The self-test's "system events test" runs a lock, a display change and a simulated lost device.

### Shortcut cards hidden
The shortcut wall is hidden by default (`show_shortcuts`); shortcuts are still scanned and kept in
the layout so it can be turned back on.

### Desktop card: the selection box trailed the cursor (relayed drags)

Symptom: on the desktop card used in place, the cursor moved smoothly but the blue selection box
followed slowly. Other cards were fine.

Cause: most of the desktop is covered by other windows (Chrome nearly maximized), so a press there
is not real input to the desktop. The mouse hook (`wall.rs`) swallows it and posts it to the icon
view (`win::post_desktop_mouse`), and while the button is held it posted every mouse move. Posted
moves are not merged the way real ones are, and the icon view redraws the box for each one. A
fast mouse sends about 1000 moves a second, so they piled up. A scratch test that posted moves to
the icon view: at 60/s the box was 2 ms behind; at 1000/s it was 160 ms behind after one second,
and the gap kept growing. Earlier builds felt fast because drags on bare desktop get real input.

Fix (8eb011b): `wall::relay_move` posts a move at most every 12 ms, holding back only the newest
point. `wall::flush_relay` (called every frame from the overlay tick) sends a held-back point
once its turn comes, and a button-up first sends the final point. Log: "in-place drag: card N
fps, delay, relayed true".

Dead end: `win::push_under_desktop` (`SetWindowPos(HWND_BOTTOM)`) never puts windows under the
desktop. Windows keeps Progman at the very bottom, so pushed windows land just above it and still
cover it, and `SetWindowPos(Progman, HWND_TOP)` does nothing. `WS_EX_TRANSPARENT` without
`WS_EX_LAYERED` doesn't make a window click-through either. So relayed drags are still the path
over covered parts; getting real input there is unsolved.

Before that (8b58789): the live capture's shared texture is a pair (one shown, one written,
swapped). The D3D11 wait on the free fence had stalled polls by 15-50 ms.

### Desktop card: a click on empty desktop brought up Chrome's card

Symptom: sometimes a click-drag on bare desktop on the desktop card switched to the Chrome card.
Log: "in-place use: another window took the foreground (Chrome...)", and "in-place click: ...
under: Chrome" with no "passed on to the desktop icons" line.

Cause: Chrome's window had `WS_EX_TRANSPARENT` (ex style 0x2401a0), left there by a scratch test
that didn't restore it properly. `win::window_at` skipped any window with that style, so
`wall::covered` saw bare desktop and let the real click through. But the style alone doesn't make
a window click-through, so the click reached Chrome, which took the foreground.

Fix (17cdf33): `window_at` skips a window only if it has both `WS_EX_TRANSPARENT` and
`WS_EX_LAYERED`. Also, the hook no longer looks up the shell popups on each press (that goes over
every top-level window). `wall::note_popups` keeps them from the frame loop every 50 ms, and a
press the hook took over 20 ms is logged ("a button press took the mouse hook N ms"; Windows
passes input on past a hook that is late).

To check by hand which window covers a point (class, pid, ex style), list the visible top-level
windows down the z-order with `GetTopWindow`/`GetWindow(GW_HWNDNEXT)`. Scripts that change other
apps' windows must save each original once and verify the restore.

### Plain backgrounds (no visible room box)

Request: drop the visible box around the 3D view and allow other backgrounds; first step plain
colours, images later.

How it works: `background` in `config.json` is "room" or "#rrggbb" (`render::background_color`
turns it into linear rgb). `Renderer::set_background` sets the clear colour and `Globals.bg`;
with `bg.w` = 1, `fs_room` in `shader.wgsl` discards the walls and ceiling and draws the floor
in the background colour with a faint grid that fades before the room's edges and away from the
camera, so the box's outline never shows. The room's limits (`ROOM_HALF`, `ROOM_HEIGHT`) are
unchanged, so the camera and cards stay inside an invisible box. The options menu's
"Background" row (`overlay::BACKGROUNDS`) previews at once; `Overlay::discard_menu` undoes the
preview when the menu is left by closing the 3D view (the overlay can be kept and reused).

How to check: `--selftest` with `"background": "#141a2e"` in the config: `selftest_*.png` show
navy with a fading floor grid and no walls.

Pictures (added after): `background` can also be "wallpaper" (`SPI_GETDESKWALLPAPER`) or a
picture path (relative = `%APPDATA%\Window3D\backgrounds`), see `background.rs`. `parse` turns
the setting into `Kind`; `load` decodes with WIC on a worker thread (no new crate), scales to at
most 8192 (panorama) / 4096 (backdrop) and the GPU limit, and builds the mip chain.
`overlay::apply_background` sets `Backdrop::Picture { dim: 0.2 }` and starts the load unless the
GPU already holds that picture (`Gpu::sky_key`, a hash of path + size + mtime; the sky texture
lives in `Gpu`, so it survives reopening the 3D view); `Overlay::poll_sky` uploads it each frame
when ready (`Gpu::set_sky`). Until then only the clear colour shows. Aspect 1.9-2.1 = panorama:
`fs_sky` turns each pixel's ray (`inv_view_proj`) into equirect uv, with gradients taken from
a seam-free copy of u so there is no 1-pixel line where u wraps; else a cover-fit backdrop.
Picture mode draws no room at all (no floor grid).

How to check: log line `background: 8192x4096 (file 8192x4096) pano=true ms=695` (a Poly Haven
8K JPG) or `... pano=false ms=43` (1920x1080 PNG); `selftest_*.png` show the sky / the backdrop
cropped to the screen's aspect. A bad file logs `background: <path>: can't open (...)`.


### "Windows wallpaper" background showed the grid room

- **Symptom**: picking "Windows wallpaper" in the options menu showed no picture, just the room.
- **Cause**: the user's desktop is a solid colour (black), not a picture: the registry has
  `HKCU\Control Panel\Desktop\WallPaper` empty and `...\Explorer\Wallpapers\BackgroundType` = 1.
  `background::wallpaper()` found no file and `background::parse("wallpaper")` fell back to `Kind::Room`.
- **Fix**: `parse` now falls back to `Kind::Plain` with the desktop colour (`background::desktop_color`,
  `GetSysColor(COLOR_DESKTOP)`); the menu tile shows that colour.
- **How to check**: with a solid-colour desktop, `background: "wallpaper"` gives a plain background of
  that colour; `menu.png` from `--selftest` shows the "Windows wallpaper" tile filled with it.

### Options menu was keyboard-only text

- **Symptom**: the Esc menu was a list of text lines driven by arrow keys; backgrounds could only be
  cycled through by name, and the mouse did nothing (the cursor stayed hidden for mouse-look).
- **Fix**: `src/menu.rs` draws a panel on the CPU (`Menu::compose`, GDI text in a `win::Dib`) with
  clickable value boxes, Done / Close buttons and background tiles with thumbnails
  (`background::make_thumbs`: 3 worker threads, WIC scale-to-cover + centre crop in `decode_cover`,
  cached per path and size in `THUMBS`). Extra tiles: `background::windows_pictures` (the
  `BackgroundHistoryPath0-4` registry values, then `%SystemRoot%\Web\Wallpaper\*\*`). The panel is
  uploaded to a live texture slot (`Overlay::menu_slot`, reset in `park`) only when `Menu::dirty`
  (hover, change, scroll, a thumbnail done). While it is open `main.rs` sets the `MENU` flag
  (`App::sync_menu_mouse`, run after every action and event batch): raw mouse input is released,
  the overlay is put in front, the cursor is clipped to it (unless Tab freed it) and shown at the
  centre; `overlay_wndproc` turns `WM_MOUSEMOVE`/`WM_LBUTTONDOWN` into `Event::MenuMouse` for
  `Overlay::menu_pointer`, and the wheel scrolls the tiles. Closing the menu grabs the mouse again.
- **How to check**: `--selftest` writes `menu.png` and the lines `options menu: WxH drawn (N ms with
  thumbnails)` and `options menu: Esc closes it=true settings-unchanged=true`.

### No 360° panoramas to pick in the options menu

- **Symptom**: "there's no panorama or 360 degree images backgrounds": the Esc panel only offered
  the room, colours, the wallpaper and Windows' own (flat) pictures; panoramas worked only if the
  user found and dropped one in the backgrounds folder.
- **Cause**: nothing shipped any. Panoramas are big (8192x4096 JPGs are 3-22 MB), so bundling them
  in the exe is not an option.
- **Fix**: `background::PANORAMAS`: 13 (now 28, see below) Poly Haven HDRIs (CC0) as their "Tonemapped JPG"
  (`https://dl.polyhaven.org/file/ph-assets/HDRIs/extra/Tonemapped%20JPG/{id}.jpg`); only a
  384x216 thumbnail of each is embedded (`assets/panoramas/*.jpg`, ~15 KB each, decoded from memory
  by `decode_cover` via `IWICStream::InitializeFromMemory` under the cache key `pano:{id}`). Their
  tiles (`menu::Preview::Pano`, after the wallpaper, badge "360°", strip "downloads when picked" /
  "Downloading…" / "Download failed") set `background` to `"<name> (360).jpg"`.
  `overlay::apply_background` sees that file is missing and calls `background::fetch`: a thread
  runs `URLDownloadToFileW` (Cargo feature `Win32_System_Com_Urlmon`) to `.part`, checks with WIC
  that it is a 2:1 picture, renames it. `background::downloads_ended` counts finished downloads; the
  overlay frame re-applies the current background (`Overlay::background`, or the menu's) when it
  changes, so the panorama appears without reopening anything. A config naming a missing panorama
  downloads it when the 3D view opens. Folder pictures that are panorama downloads are not listed
  twice.
- **How to check**: `--selftest` writes `menu_top.png` (top of the grid, panorama tiles) and
  `options menu: 28 panoramas offered, N downloaded`. log.txt: `panorama <id>: downloaded in N ms`
  (or `download failed: ...`), then `background: 8192x4096 (file 8192x4096) pano=true`. Tested with
  `"background": "Sky on fire (360).jpg"`: downloaded and drawn in the selftest.

### Using one Chrome window's card hides the other Chrome window on the second monitor

- **Symptom**: "when i focus on the other chrome window it takes this chrome window off the
  second monitor where it's supposed to be": with one Chrome window maximized on the primary
  monitor (used in place on its card) and another maximized on the second monitor, the second one
  vanished from that monitor while the first was used.
- **Cause**: not a move (log.txt never had "moved to the 3D view's monitor"). Listing the windows
  showed the primary one at `layered=True alpha=1` while the other was used in place: it had been
  made invisible as a *popup* of the window used in place (`win::popups_of`, which hides an app's
  menus on the screen and draws them on the card). `popup_of` took it for one because (1) a
  maximized window's `GetWindowRect` reaches 8 px into the next monitor (`(2872,-8)`) and so
  "overlapped" the other's frame, and (2) `part_of` counts a same-process window as part of the
  window if it has `WS_EX_TOOLWINDOW`, which `uncover_chromium`/`make_tool` give maximized Chrome
  windows (style 0x240180). Both Chrome windows are one process, so each hid the other.
- **Fix**: `win::part_of`: a window that has a caption (`WS_CAPTION`), `WS_EX_APPWINDOW`, or was
  made a tool window by us (`tooled`) is another main window, never part of the one used in place.
  `win::popup_of` also tests the overlap with the frame bounds (`frame_bounds`), not only the window
  rect.
- **How to check**: with two Chrome windows maximized on two monitors, use either card in place: the
  other stays on screen (PowerShell `GetLayeredWindowAttributes` on it: not layered, or alpha 255).
  `--selftest`: `popup test: ... hidden in use true ... given back true` still shows real popups
  being hidden. Side note: both Chrome windows kept the tool-window bits (0x40080) after Window3D
  was killed with `taskkill /F` (the `unshape` that clears them never ran); harmless, and gone
  when Chrome restarts.

### Flat pictures in the background menu; more open-sky panoramas wanted

- **Symptom**: "remove the image backgrounds that aren't 360" and "search for more 360 images i
  really like the clear blue sky and the partly cloudy sky because there's nothing in the
  foreground so i can walk around and feel like im in the setting".
- **Cause**: the options menu also listed the Windows wallpaper and Windows' own/recent pictures
  (flat backdrops), and only two of the 13 panoramas were pure skies.
- **Fix**: `background::PANORAMAS` gained 15 Poly Haven "Pure Sky" HDRIs (ids ending `_puresky`:
  clear midday, noon, afternoon, deep blue, little/fleecy/big white clouds, cloud bank, stormy,
  golden sunset, clear sunrise, pale dawn, overcast, misty morning, moonlit), listed right after
  the two existing skies. Their thumbnails (`assets/panoramas/<id>.jpg`, 384x216) are centre crops
  of `https://cdn.polyhaven.com/asset_img/primary/<id>.png?height=432`; not the
  `/asset_img/thumbs/` URL, which shows test-render balls. `windows_pictures`, `history_path` and
  `is_picture` are gone; `menu::Menu::new` drops the "Windows wallpaper" tile and lists folder
  pictures only if `background::is_panorama` (2:1 header). Flat pictures and `"wallpaper"` still
  work when set in `config.json` (a config value not in the list shows as its own first tile).
- **How to check**: `--selftest`: `options menu: 28 panoramas offered, N downloaded`;
  `menu_top.png` shows colours, then "Partly cloudy sky", "Clear blue sky", "Clear midday sky".

### Animated "cozy alien" skies as shader backgrounds

- **Symptom**: request: animated backgrounds that are alien but cozy ("not too alien that it feels
  uncomfortable"), done as sky-dome shaders rather than pictures.
- **Cause**: backgrounds could only be the room, a colour or a still picture.
- **Fix**: `src/scene.wgsl` (appended to `shader.wgsl` in `render.rs` with `concat!`):
  `fs_scene` draws one of 6 scenes per pixel from the view direction: a sky at infinity above the
  horizon and a ground plane at a real height below (`ground_at`), so walking moves you over it.
  first 6 scenes; the set was later changed (see "Scene horizons looked like a clipped image"). `render::Backdrop::Scene{index,dim}` with `scene_pipe`; the
  index goes in `globals.bg.y`, time in `misc.x`. `Renderer::animated()` makes `overlay.rs` redraw
  every 30 ms. `background::SCENES` / `Kind::Scene`, config value `"scene:<id>"`; `menu.rs` shows
  them as "Live" tiles after the colours. Thumbnails `assets/scenes/<id>.jpg` are 384x216 centre
  crops of the selftest shots. First renders were tuned by eye from a contact sheet: thin
  aurora squiggles became broad `smoothstep(sin(band))` curtains over a brighter sky; the nebula
  was all teal and dark (now a lighter base, teal/pink by a warp field, gold cores); islands were
  flat grey triangles (now a rounded hanging rock, grass dome and some trees); lanterns were
  confetti (fewer cells, a higher threshold, taller bodies, halos masked inside their cell to avoid
  square edges) and the sea under them is calmer (`water_dir` amplitude 0.015).
- **How to check**: `--selftest` writes `scene_<id>_{1,2,3}.png` and `scenes (2880x1800, per frame
  incl. readback): twin_moons 9.1 ms ...` (about 8-9 ms each on the integrated GPU).
  `WINDOW3D_SCENES_ONLY=1` stops the selftest after the scene shots for quick shader work.

### Scene horizons looked like a clipped image; scenes remade per mood

- **Symptom**: "twin moons and aurora meadow look the best, some of the others have issues where
  the horizon looks flat and then it's like a clear separation between that and the background
  ... like its just clipping an image at the edge". Wanted: more scenes, each evoking a feeling.
- **Cause**: the grounds of ringed_world, floating_isles and nebula_drift faded with distance into
  one fixed colour (`grad(0.0)`), but the sky just above the horizon was not that colour
  everywhere (sun glow, planet, clouds, the nebula has no horizon gradient at all), so a hard line
  showed. The mirror grid's lines also ran right up to the horizon. Twin moons (water that
  reflects the sky) and aurora meadow (its sky at y=0 is exactly `grad(0.0)`, the aurora fades
  out before the horizon) had no seam.
- **Fix**: `scene.wgsl`: every ground now fades into the sky in its own direction, `sky(flat(d))`
  (`flat` puts the direction on the horizon), or is water reflecting the sky. ringed_world,
  nebula_drift, floating_isles and lantern_sea are gone (their ids now fall back to the room).
  New, each aimed at a mood: 2 amber_sea (big low sun over a warm sea: nostalgia), 3 rain_veil
  (rain streaks, drop rings on a lake, warm glow far off: cozy), 4 deep_blue (underwater: surface
  window, rays, motes, sand with caustics at y=-5, all fogged into the water colour: calm),
  5 salt_mirror (puffy clouds mirrored by a salt flat, pink petals: joy), 6 galaxy_rise (spiral
  galaxy over a glassy black sea: awe), 7 snowfall (snow plain at night, three layers of falling
  flakes, moon in thin cloud, warm light on the horizon: quiet, safe). aurora_meadow is index 1.
  Things in the air (`drift` for snow, petals, motes; `rain`) are drawn in `fs_scene` over sky and
  ground alike, so they cross the horizon too. Dead ends while tuning: grid-like salt crust
  (looked like floor tiles, now faint and patchy), square sand/snow glints (one-pixel cells shown
  whole; now round dots), rain streaks fanning out overhead (faded above |y| 0.25-0.65), a
  choppy amber sea (amplitude 0.09 to 0.03).
- **How to check**: `WINDOW3D_SCENES_ONLY=1 window3d.exe --selftest <dir>`: look at
  `scene_<id>_1.png` (ahead, slightly up) and `_2` (down at the ground) for a line at the
  horizon. `scenes (...)`: all 8 about 8.5-9 ms per frame at 2880x1800 on the integrated GPU.

### Popups of the window used in place end the use (Chrome sign-in, Widgets, tray menus)

- **Symptom**: on a Chrome card used in place, Google's sign-in window, the profile menu and
  extension bubbles came up but couldn't be clicked or focused: the card dropped out of use and
  the popup was left under the opaque 3D view. On the desktop card, the Widgets board's popups
  and some tray menus were hard to click the same way.
- **Cause**: from the log (`in-place use ended: a launched app took the foreground`): during
  in-place use `launching` always exists and `adopt_interact` adds the used app's pid to it, so
  any new window of that app taking the foreground counted as `launch_stole`. The non-desktop
  branch of `on_foreground` never asked `part_of`. `part_of` compared `GA_ROOTOWNER`, which only
  follows owners of `WS_POPUP` windows, so an owned dialog with a caption wasn't part of its
  window (found by the selftest's `popup kinds test`: "dialog false"). On the desktop, a tray
  app's hidden window (OpenVPN's `tooltips_class32`) taking the foreground ended the use, and
  Widgets.exe's WinUI popups (`PopupWindowSiteBridge`) only counted for explorer.exe. Also
  `wall::now_ms` started at 0, so a press stored in the first millisecond (`max(1)`) lay "in the
  future" and `pressed_within` was false (found by the same test).
- **Fix**: `win.rs`: `root_owner` walks `GW_OWNER` (used by `part_of`, `own_popup`); `part_of`
  also takes same-process non-switchable `WS_POPUP`s and windows taken by `adopt_popup` (same
  process, came up within 3 s of a click, inside the frame ±24 px, at most 60% of its area: a
  sign-in window); `fit_dialog` moves a captioned popup lying off the window over its middle;
  `desktop_part` counts `PopupWindowSiteBridge`/`#32768` of shell hosts (`shell_host`) and
  windows they own; `note_desktop_popup` takes any captionless non-switchable popup within
  1.5 s of a click (tray menus). `main.rs`: `on_foreground` stays in use for a popup of the used
  window ("a popup of the window used in place took the foreground") and on the desktop for a
  non-switchable window right after a click; `on_shown` adopts switchable popups ("a window of
  the window used in place came up over it"); `poll_launch`/`window_appeared` skip them
  (`popup_of_used`). `wall.rs`: `now_ms` counts from 1.
- **How to check**: selftest line `popup kinds test: dialog true, bubble true, second window
  false (after a click false), sign-in window false (after a click true), dialog off the window
  moved over it true`. In the log, opening Chrome's sign-in or profile menu on a card in use
  shows one of the two lines above, not `in-place use ended: a launched app took the foreground`.

### Desktop card: a tray icon's menu showed on the real screen, away from the card

- **Symptom**: using the desktop in place, right-clicking a tray icon (OpenVPN in the tray
  overflow) brought its menu up in the wrong place: on the real screen where the cursor really
  is, not over the card.
- **Cause**: the log said the menu was taken in ("a desktop popup came up ("#32768" ...openvpn-gui.exe):
  shown in the card"), and `hide` did make it layered with an alpha of 1. A scratch test
  (`target/scratch/wd/menu.py`: `TrackPopupMenu` plus a thread setting alpha 1 on the `#32768`)
  showed why that didn't last: a menu fades in layered by itself (ex style 0x80189), and once the
  fade is done it drops `WS_EX_LAYERED` (ex 0x189), and the alpha of 1 goes with it. `hide`
  skipped windows it had hidden already, so the menu stayed visible on top of the 3D view.
  That was only half of it: with that fixed the menu still showed flat on the real screen (the
  user's screenshot). `desktop_part` matched the class `"#32768"` before it looked at
  `DESKTOP_POPUPS`, and that arm only takes Explorer's and the shell hosts' menus, so the menu
  `note_desktop_popup` had taken in (and logged as "shown in the card") was never a shell popup
  and never hidden at all.
- **Fix**: `win.rs` `hide`: for a window it hid already that is no longer layered, it makes it
  layered with alpha 1 again (at the next look: every 250 ms then, every frame now) and saves the style it has now, so
  `show_shell` gives it back without the layered style. It can still flash for up to one 50 ms
  look as the fade ends. `desktop_part` now looks at `DESKTOP_POPUPS` first, whatever the class.
- **How to check**: selftest line `menu fade test: hidden Some(1), after the fade Some(1), given
  back unlayered true; an app's menu a shell popup false (taken in true)` (`win::menu_fade_test`;
  before the fixes "after the fade" was None and "taken in" false). A tray menu should show
  tilted with the card, not flat on the screen by the real cursor.
- **Then**: the menu showed on the card, but under the tray overflow. The card stacks shell
  popups in the order `for_each_top` lists them, and the overflow
  (`TopLevelWindowForOverflowXamlIsland`, in a band over menus) is listed above the menu.
  `win::shell_popups` now puts the popups in `DESKTOP_POPUPS` (what came up after a click: the
  tray menu, its tooltip) first, so they are drawn on top; `live::sync_pops` also counts a
  change of order as a change, so the card is drawn again when only the stacking changes.
  Not checked by the selftest (its test windows are off the screen, which `shell_popups` leaves
  out); check by right-clicking a tray icon in the overflow on the desktop card.

### Desktop card: a tray icon's hover tooltip showed twice (on the card and on the real screen)

- **Symptom**: hovering a tray icon (OpenVPN) on the desktop card, its tooltip showed twice.
- **Cause**: the tooltip (`tooltips_class32` of openvpn-gui.exe, taken in by `note_desktop_popup`)
  was drawn on the card, but `hide`'s alpha of 1 didn't last. A scratch test
  (`target/scratch/wd/tip.py`, a tracking tooltip polled with `GetLayeredWindowAttributes`)
  shows a tooltip fading in by its own `SetLayeredWindowAttributes`: alpha 40, 120, 200, 255
  over about 0.3 s, staying layered. `hide` only looked again at windows that had dropped the
  layered style (a menu's fade), so the real tooltip came back on the screen over the 3D view.
- **Fix**: `win.rs` `hide`: for a window it hid already that is layered with an alpha over 1,
  the alpha goes back to 1 at the next look (every frame since the "re-hidden only every 250 ms" entry below), and the window's newest attributes are what
  `show_shell` gives back (not the faint ones from the start of the fade).
- **How to check**: selftest line `menu fade test: ... after an alpha fade Some(1) (to give back
  200 true) ...`. Hovering a tray icon on the desktop card shows one tooltip, tilted with the card.

### Tray menus and tooltips glitchy on the desktop card: re-hidden only every 250 ms; popup trace in the log

- **Symptom**: opening tray icon menus (OpenVPN, Tailscale) on the desktop card used in place
  was "very glitchy"; the log only said "a desktop popup came up", not what happened next.
- **Cause**: popups hidden already were only looked at again by the sweep in
  `overlay.rs` `sync_shell`, every 250 ms. A menu drops `WS_EX_LAYERED` when its fade-in ends and
  a tooltip raises its own alpha step by step, so each showed on the real screen (over the 3D
  view) for up to 250 ms before `win.rs` `hide` hid it again. New popups are hidden from the
  window-shown event, but that goes through the main loop's event queue, which a slow frame
  (89-113 ms frames were logged) delays.
- **Fix**: `win::rehide_shell` runs `hide` over the hidden list (no window search, so it's cheap)
  on every frame in `sync_shell`. Added a popup trace to the log so the next report shows
  what really happens. `win::trace_pop` logs changes only, at most 80 lines a popup, and
  only while shell popups are hidden. The log now rolls over at 1 MB instead of 256 KB.
- **How to check**: `popup trace 0x...: followed from now on (...); it showed N ms ago`.
  N is how late the app took the popup in, so the time it could flash on the real screen.
  Then `+ms hide: ...` shows each hiding step (hidden, its fade dropped the layered style, its
  own alpha set back to 1, pushed under for UpdateLayeredWindow ones). `+ms look: visible ..
  rect .. alpha .., over/under the 3D view` shows its state on the real screen; `over` with
  alpha above 1 means it is visible on the real screen. `+ms card: on the card at ..: capture
  opening / capture opened / gone from the card` and `+ms frame: first frame on the card` give
  how long the card took to draw it.

### Desktop card: popup trace buried in hidden shell windows; a tooltip "mirrored" on the real screen

- **Symptom**: the user saw a tray tooltip "mirror": shown flat on the real screen as well as
  tilted on the card. The popup trace in their log was hard to read.
- **Cause**: (1) the sweep in `hide_shell_again` hides about 50 shell windows that are
  invisible or cloaked, and `win::trace_pop` started following each one. (2) The OpenVPN
  tooltip fades itself in again and again (alpha 225 down to 15, back to 255). `hide` set the
  alpha back to 1, but one trace look showed `alpha 255 ... over the 3D view`. The tooltip had
  come back on top, so between its own alpha change and our next look it was on the real screen.
- **Fix**: `win.rs` `trace_pop` follows only a popup that is visible and not cloaked. In `hide`,
  when a hidden window set its own alpha again, it also goes right under the 3D view
  (`under_view`, the same call the UpdateLayeredWindow branch already used). Menus whose fade
  ended are not moved under it: they are clicked through the card.
- **How to check**: no `popup trace` lines for invisible windows. For a tooltip, a `hide: set
  its own alpha N ...: back to 1` line followed by `look: ... under the 3D view`.

### Desktop card: a click near Task Manager (run as administrator) ended using the desktop and flew to its card

- **Symptom**: "sometimes clicking on the widgets brings task manager to focus". Using the
  desktop card in place, after a tray menu closed, the next click flew the camera to Task
  Manager's card.
- **Cause**: Task Manager runs elevated. `SetWindowPos` on it, and raising Progman over it,
  fail with access denied (5); tested, nothing moved. So it can't be put under the desktop. When a
  tray popup closes, Windows gives Task Manager the foreground. The log showed `the desktop
  takes it back (false)`, Task Manager over the desktop's rect, `in-place cursor off the
  window ... under: TaskManagerWindow`, then `in-place use ended: click on the overlay`.
  Over an elevated window the overlay stops letting the mouse through (`through` in
  `overlay.rs`) so that `wall`'s hook can pass the click on to the desktop icons. But the hook
  didn't take the click: it either missed it, or `covered` said no. So `pass_input` saw a click
  on the overlay and ended the use.
- **Fix**: `wall::pass_elevated`, called first in `overlay.rs` `pass_input`: using the desktop,
  a press the overlay gets over an elevated window on the desktop's monitor goes to the desktop's
  icons (`post_desktop_mouse`, with double clicks, drags and the button-up), like the hook's
  covered path. A middle press there is swallowed. It doesn't end the use.
- **How to check**: with Task Manager (as administrator) on the primary monitor, use the desktop
  card and open and close a tray menu. Then click the desktop where Task Manager is: the log
  shows `in-place: 1 click(s) passed on to the desktop icons (under: ...Taskmgr...)` and no
  `in-place use ended: click on the overlay`.

### Desktop card: tray menus of apps run as administrator showed twice; Task Manager still ended the desktop's use

- **Symptom**: "a bunch of glitchy stuff". The menus of tray icons for Task Manager, G-Helper and
  some system tools showed flat on the real screen and also on the card. After a tray menu
  closed, the desktop's use ended and the camera flew to Task Manager.
- **Cause**: from the popup trace in the user's log. These popups belong to elevated processes,
  or to processes Window3D can't open (exe ""). `hide` can't set their alpha (no `hidden` line
  for them). `under_view` can't move them either: `look: ... over the 3D view` after `pushed
  under`. So they stayed on the real screen while the card drew them as well. Task Manager: the
  shell-popup branch tried to take the foreground back (`the desktop takes it back (false)`),
  then the next foreground event fell through to `another window took the foreground ...
  back to flying`.
- **Fix**: `win::elevated` also counts a process it can't open as out of reach. `live.rs`
  `shell_pops_now` leaves popups that are out of reach off the card: the real one shows over
  the 3D view, once. `hide` traces `can't be hidden (out of reach: ...)` /
  `drawn with UpdateLayeredWindow, out of reach: stays over the 3D view`. `main.rs`: when the
  takeback fails, the time goes in `TAKEBACK_FAILED`. An elevated window taking the
  foreground within 3 s of that logs `kept the foreground the desktop couldn't take back (run
  as administrator), staying`, and the desktop stays in use.
- **How to check**: right-click the Task Manager tray icon on the desktop card. The trace shows
  `out of reach`, and the menu shows only once. After it closes, the `staying` line appears
  and the camera doesn't fly to Task Manager.

### Desktop card: the real screen instead of a bare desktop used in place

- **Symptom**: using the desktop card in place stayed glitchy however many popup kinds were
  handled: tray menus and tooltips flashing or doubled, a tooltip "mirrored" on the real screen,
  Task Manager and other admin apps ending the use, laggy drag selection on covered parts.
- **Cause**: the design. The card showed the bare desktop (Progman, the taskbar and every shell
  popup captured one by one and composed), while the real desktop is covered by windows. So
  every shell popup had to be hidden on the real screen (alpha 1, which menus and tooltips undo
  with their own fades), clicks on covered parts were relayed to the icon view by hand, and
  the foreground was guessed at. Windows of elevated apps can't be hidden or moved at all.
  All the entries above about the desktop used in place were patches on this.
- **Fix**: the desktop card is now the whole primary screen as it is, windows and all.
  `win::no_capture` gives the 3D view (`main.rs` `create_overlay_window`) and the pointer window
  (`pointer.rs`) `WDA_EXCLUDEFROMCAPTURE`, so screen captures show what is under them.
  `live.rs` `open_capture` captures the primary monitor (`CreateForMonitor`) for the desktop's
  card (no taskbar or popup captures any more); `win::capture_window` shoots the desktop card
  with a `BitBlt` of the primary screen; `win::frame_bounds` of the desktop is the primary
  monitor, so card points map 1:1 to the screen.
  First (143d1fb) clicking it handed over to the real screen; the user wanted it used like any
  card. Next (a62c232) a click flew into it like F; the user wanted a click used in place where
  the card is, like the other cards (`activate` has no desktop arm any more). Used in place,
  the overlay is click-through over all of it (`interact_tick`: `through = on`), so every click
  goes to the real window under the cursor. Real popups (tray menus, Start, Task Manager) are
  not hidden: unzoomed they show twice, for real at their real spot over the 3D view and on
  the card (the user chose to try that first); after F (1:1) they sit over their image. Shift+F still hands over to the real screen
  (`main.rs` `Action::Theater` desktop arm). `main.rs` `on_foreground`: while the desktop is
  used, any window taking the foreground stays in use ("in-place use (desktop): ... took the
  foreground, staying"); Esc Esc ends it. Removed: the click relay to the icon view and
  `pass_elevated` (`wall.rs`), hiding the shell popups for the desktop (`sync_shell` only hides
  them over an app card), `note_desktop_popup`, the desktop takeback after Start closed,
  `switch_from_desktop`.
- **How to check**: selftest `desktop test: first frame ..., size (2880, 1800)` (the monitor's
  size). A screenshot (PrtScn, Snipping Tool) taken with the 3D view open shows the windows
  under it, not the 3D view. In the log, clicking the desktop card gives "in-place use" lines and
  "in-place use (desktop): ... staying" when a window clicked on it takes the foreground; Shift+F
  gives "desktop card: handed over to the real screen".

### Desktop card used in place: menus and Task Manager showed twice (over the 3D view and on the card)

- **Symptom**: using the desktop card in place unzoomed, a tray or context menu, a tooltip or
  Task Manager came up for real over the 3D view at its real spot, and again on the card.
- **Cause**: the card is a capture of the whole screen, used where it sits in 3D, so the real
  screen isn't under its image; windows that are "always on top" open over the 3D view.
- **Fix**: the 3D view goes back on top of them instead of hiding them (`win::raise_over`,
  called by `Overlay::keep_on_top` every in-place frame and from `main.rs` `on_shown`). The 3D
  view is left out of captures (`win::no_capture`), so they still show on the card. Our own
  windows above it (the pointer) are raised again after it. Start, search, the taskbar and the
  shell's flyouts are skipped: they are in bands above every app (only `uiAccess`, a signed app
  in Program Files, could go over them), so they still show twice; a window still over it after
  a raise is put on `OVER_BAND` and not tried again.
- **How to check**: log "desktop used in place: back over <window>" when a menu or Task Manager
  comes up; "(stays over it, given up on)" marks one in a higher band.

### Per-monitor rooms removed: the 3D view is the same on every monitor

- **Symptom**: opened on another monitor, the 3D view showed other cards (that monitor's
  "room") and its own camera; cards had a toolbar button sending them to the next monitor.
  The user doesn't want that: one 3D view, the same wherever it opens, one open at a time.
- **Cause**: the multi-monitor rooms feature (backlog item 7): `Entry::room`,
  `Layout::room_cameras`, `App::room`/`rooms`, `model::room_of`, `Tool::Room` /
  `Action::NextRoom`.
- **Fix**: all of that is gone. `main.rs` `scan` shows every card on any monitor; `show` and
  `hide` use `layout.camera` only; the toolbar is `Tool::ALL` for a running window's card and
  `Tool::PLAIN` for the others. `M` and the tray's "Open the 3D view on" still pick the monitor.
  Old `layout.json` files with `room` / `room_cameras` still load (serde skips the unknown
  fields; they are dropped on the next save), so cards that were sent to another room show
  again everywhere. The selftest `room test` line is gone; the search test's `m-is-text` now
  checks that `M` typed in the search box leaves `monitor` in the config unchanged.
- **How to check**: open the 3D view on two monitors in turn (`M`): the same cards, the camera
  where it was left. No monitor button on card toolbars.

### The released exe carried the Windows user name (build paths)

- **Symptom**: `dist/Window3D-0.1.0.zip`'s `window3d.exe` held 64 copies of
  `C:\Users\<name>\.cargo\registry\src\...` (crate source paths in panic messages) and the full
  path of its PDB, so anyone with the exe could read the builder's Windows user name.
- **Cause**: rustc writes the source path of every dependency into the binary, and the MSVC
  linker writes the full PDB path; neither is stripped by a plain `cargo build --release`.
  Found by searching the exe's bytes for the user name.
- **Fix**: `packaging/package.ps1` builds with `CARGO_ENCODED_RUSTFLAGS` set to
  `--remap-path-prefix=<home>=~` and `-Clink-arg=/PDBALTPATH:%_PDB%` (only the file name), into
  `target\dist` so the everyday `target\release` isn't rebuilt, and fails if the exe still holds
  the user name. The git history was checked too: no log, selftest output or screenshot was
  ever committed, and no file version holds the user name, home paths or window titles.
- **How to check**: the script stops with "the exe still contains '<name>'" if it comes back.

### Opening the 3D view on the other monitor closed it and took 10 s to come back

- **Symptom**: with two monitors at different scales (here 1440x900 at 200% and 2048x1152),
  opening the 3D view on the second one glitched: it vanished, and the next open took ~10 s.
- **Cause**: the log had `system event Display (overlay open)` right after the open, then
  `surface ready in 10383 ms (gpu cold)`. The kept-warm overlay window is moved to the
  monitor it opens on; on a monitor of another scale Windows sends it `WM_DPICHANGED`, which
  `main.rs` treated as a display change (`Sys::Display`: close the overlay, drop every GPU
  object, warm up again). It showed now because, with per-monitor rooms gone, the user opened
  the same view on the second monitor.
- **Fix**: the overlay's window procedure answers `WM_DPICHANGED` itself (returns 0, so the
  window keeps the size `show` gave it: the monitor's pixels) and logs it. Real monitor changes
  still come as `WM_DISPLAYCHANGE`.
- **How to check**: opening on the other monitor (or `M`) logs "3D view: on a monitor with
  another scale (DPI change ignored)" and no `system event Display`; the open stays fast.

### Tray menu "doesn't show up properly" with the 3D view closed (not a bug)
- **Symptom**: the user thought the tray icon's right-click menu looked wrong with the 3D
  view closed.
- **Cause**: none found. Posting `WM_APP_TRAY` with `lParam = WM_RBUTTONUP` to the
  `Window3DMsg` window showed the full menu within about 60 ms. It opens at the cursor
  (`tray_menu` uses `GetCursorPos`), so if the mouse moves just after the click, the menu
  looks offset. The user confirmed it works.
- **How to check**: from a test script, look for the `#32768` window with
  `SetProcessDpiAwarenessContext(-4)` set. Without per-monitor awareness, its coordinates and
  screenshots on the 2nd monitor (different scale) are wrong: that was a dead end here.

### Option to hide the on-screen hints (options menu: "Hints and tooltips")
- **Symptom**: the user wanted a setting in the Esc options menu to remove the key list at
  the bottom and the tooltips over cards and tools.
- **Fix**: new config key `show_hints` (default `true`, `store.rs`) and a row in `menu.rs`
  (`ROWS`/`SETTINGS` grew by one; `BG_ROW`, `DONE_ROW`, `CLOSE_ROW` moved down). `main.rs`
  copies it to `overlay::SHOW_HINTS` at startup, on config reload and in `apply_settings`.
  When it is off, the overlay's frame code gives the help strip alpha 0 and skips the hint
  block (hover hints, tool tips, the in-place and free-mouse hints). Search and workflow
  bars still show.
- **How to check**: Esc, then turn "Hints and tooltips" off, then Esc. No text at the bottom
  and nothing when hovering a card. `config.json` has `"show_hints": false`.

### ULTRAKILL used in place: the cursor flew everywhere while looking around

- **Symptom**: ULTRAKILL (Unity, full screen on the primary monitor) used in place on its card: in
  mouse-look the cursor appeared and jumped around the screen, and the view stuttered.
- **Cause**: the edge push in `interact_tick` (`overlay.rs`). The game hides the cursor and pulls it
  back to its centre itself, but a quick turn reaches the screen edge first; the motion pushing on
  past it counted as "pushed off the card", so Window3D moved the real cursor (`free_cursor_at`),
  showed the system cursors again and took the mouse back from the game; coming back onto the card
  moved it again to the mapped window point. Seen in the log as pairs of "in-place cursor off the
  card at (1545,976)" / "in-place cursor back onto the card at ... (window point (2879,1200))"
  every second or two. (So "the real cursor is never moved by us" in "Games that grab the mouse"
  is not quite true: the edge push moves it.)
- **Fix**: `pointer::app_hidden` (`GetCursorInfo`: not showing, or no cursor). While the overlay is
  click-through and the app hides the cursor, `interact_tick` drops the push (its own and the
  wall's, `wall::set_away(false)`), so the cursor is never moved or freed; `pointer::show` already
  draws no pointer for a hidden cursor. Leaving is `Esc Esc` as always (or the game's menu, which
  shows the cursor again).
- **How to check**: log lines "in-place: the app hides the cursor" / "... shows the cursor"; no
  "in-place cursor off the card" lines while mouse-looking.

### Frame rate settings in the options menu

- **Symptom**: the frame rate caps (`max_fps`, `live_fps`) could only be changed in config.json,
  and the README explained them at length.
- **Fix**: two rows in the options menu (`menu.rs`: "Frame rate cap" cycles `MAX_FPS` 30-240 or
  none, "Live cards not in use" cycles `LIVE_FPS` 5-60 or every frame; a value not offered steps
  to the nearest). `App::apply_settings` saves them and stores `live::MAX_FPS` / `live::LIVE_FPS`
  at once (the frame pacing reads `cfg.max_fps` each frame); a config.json reload stores them too.
  The panel is taller (fits the screen at 890 units instead of 800).
- **How to check**: `--selftest` `menu.png` shows the two rows; change them with Esc, the log
  line "options menu: settings saved" and config.json show the new values.

### Win+Shift+S in the 3D view

- **Symptom**: a Win+Shift+S screenshot in the 3D view showed the windows under it, not the 3D
  view, and the Snipping Tool taking the foreground ended using a window in place ("in-place
  use: another window took the foreground (\"XamlWindow\" ...SnippingTool.exe), back to flying").
- **Cause**: `win::no_capture` gave the 3D view and the pointer window `WDA_EXCLUDEFROMCAPTURE`
  for the whole run, so the desktop card's monitor capture wouldn't show the 3D view (see
  "Desktop card: the real screen instead of a bare desktop"). Every screen capture left it out.
  The Snipping Tool's capture overlay was treated like any other window taking the foreground.
- **Fix**: `snip.rs`. The windows registered there (`win::no_capture` now calls
  `snip::register`) are left out of captures only while a desktop card capture is open
  (`live.rs` `open_capture` / `Capture::drop` call `snip::desktop_capture`; the desktop's
  `BitBlt` shot in `win::capture_window` does too, waiting 40 ms when that changed something).
  A low-level keyboard hook on its own thread (`snip::start`, started in `main`) sees Win+Shift+S,
  PrtScn and Win+PrtScn before the shell does and drops the exclusion at once (from the hook
  thread: `SetWindowDisplayAffinity` works there). It comes back 2 s later, once the Snipping Tool
  no longer has the foreground (`snip::active`, checked each loop by `snip::sync` in
  `App::process_events`). `App::on_foreground` ignores the Snipping Tool taking the foreground
  (`snip::is_tool`: the process is SnippingTool.exe), and the "focus taken back" timer waits for
  it. A fullscreen game used in place (ULTRAKILL) still minimizes itself when the Snipping Tool
  takes the foreground; nothing to do about that from here.
- **How to check**: log lines "Win+Shift+S: screen captures show the 3D view", "the Snipping Tool
  took the foreground (...), staying", "screenshot done: the 3D view is left out of captures
  again while needed". Tested by sending Win+Shift+S with the exclusion forced on (a temporary
  switch, removed): a screen grab before showed the windows under the 3D view, the Snipping
  Tool's frozen screen showed the 3D view, and after Esc the grab left it out again.
