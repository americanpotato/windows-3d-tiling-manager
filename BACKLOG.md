# Backlog

Features requested, in implementation order (each step builds on the previous ones). Done items
move to the README.

## Next up

1. **Card toolbar** — DONE (all buttons; per-card sound/scale/snap/toolbar position saved) Out-of-view live cards stop streaming (sound unaffected). (Replaces most single-key card commands). A small column of icon buttons beside
   the hovered card: on the left by default, on the right when a neighbour card sits on the left. When
   neither side is free (e.g. the centre of a 3x3 block) the toolbar can be dragged somewhere else.
   Hovering a button highlights the button and its card.
   - **Eye**: closed = screenshot, open = live video (same as `L`).
   - **Speaker** (muted by default): real per-app mute through Core Audio (`ISimpleAudioVolume` for
     every audio session of the window's process). Independent of the eye, so audio-only works.
   - **Resize**: drag to scale the card, clamped to a maximum; the outline highlights at the maximum.
   - **Move gizmo**: Blender-style X/Y/Z arrows (red/green/blue quads); dragging an axis moves the card
     along that axis only.
   - **Snap** toggle: a snapping card glues edge to edge to the nearest card while dragged.
   - **Rotate**: drag to turn the card (yaw). When snapped to a neighbour it pivots on the shared edge,
     so a row of cards can be bent into a "curved monitor".
   - Optimisation: only live cards inside the view stream video; out-of-view live cards keep only
     their audio.
2. **Template slots** — DONE (`G` grid, drop-to-fit, `Del` removes): transparent placeholder frames (e.g. a 3x3 grid). Dropping a card on a slot fits
   the card to it (position, angle, size).
3. **Workflows** — DONE (tray submenu; `K`/`X`/`N`/`P` in the overlay; tab URLs not feasible): save the current arrangement under a name (positions, angles, sizes, button states,
   template slots, and Chromium tab URLs if feasible) into a folder tree (name prompt, new folders,
   drag workflows into folders). Load = expand the tree, click, accept: missing apps are relaunched and
   all cards go to their saved places. Several workflows can be open at once; each has an X button
   (hovering it highlights every card of that workflow) and "toggle all passthrough" / "mute all".
   Save options: templates only, windows only, or both.
4. **Desktop passthrough card** — DONE (live `Progman` card left of the Desktop folder card; `desktop_card` setting) next to the desktop shortcut wall (a live card of the real desktop).
5. **Desktop wall layout** — DONE (Desktop folder card heads the wall; dragging it moves the wall; `T` tidy, `[`/`]` columns): make the desktop shortcuts wall adjustable, with the Desktop folder
   itself shown as a card above the icons.
6. **Desktop shortcuts** — DONE (all items but hidden/system; known-folder paths incl. OneDrive/Public; `.ps1` runs in PowerShell; `max_shortcuts` 200, old 48 migrated): some desktop `.bat` files don't show up; check every desktop item type
   (`.bat`, `.cmd`, `.ps1`, folders, other files) and the `max_shortcuts` limit.
7. **Multi-monitor** — DONE, then simplified: per-monitor rooms were removed (the user wants one
   3D view, the same on any monitor, open on one monitor at a time; `M` moves it).
8. **Stats window extras** — DONE (live-card count/fps/delay and the 3D view frame rate in the stats
   window; right-click menu ticks the lines, saved as `stats_fields`).

## Ongoing / paused

- **Live cards + input passthrough** (first version done, needs feel testing): left click (formerly middle click) uses a live
  card in place. Open: mouse input for raw-input apps and games. See PROBLEMS.md for what has been tried for keys, scroll and clicks.
- **Fullscreen from the 3D view** (done): `F` flies a card up to fill the view, then hides the overlay
  and makes the real window borderless fullscreen.
- **Real window sizing on focus (paused):** `focus_mode` = `"borderless"` / `"maximize"` / `"none"`.
  Code is in place (`win::focus_and_size`, tray "Restore window frames") but the default is `"none"`.
