# Windows 3D Tiling Manager (Window3D)

Press a hotkey and your open windows become cards floating in a 3D room. Fly around, arrange
them, and use any window right on its card. Written in Rust (`windows-rs` + `wgpu`/DX12).

**Who it's for:** people with one screen who'd like more room than one monitor gives. It's
handy now and then, for keeping an eye on several things or laying out a task, but honestly
Alt+Tab and normal windows are faster for most everyday switching. Treat it as a fun extra
screen, not a replacement for your desktop.

## What it does

- **Hotkey overlay** (Ctrl+Alt+Z, or click the tray icon): every window is a card with its last
  screenshot. WASD + mouse to fly, wheel to dolly, `/` to search.
- **Use windows in place:** click a card to see it live and type/click into it inside the 3D
  view. Shift+click or Enter jumps to the real window.
- **Arrange:** drag cards, resize, rotate, snap edge to edge (bend a row into a "curved
  monitor"), drop cards into a 3x3 template grid (`G`), tidy (`Shift+T`).
- **Per-card toolbar:** live view, per-app sound, size, move, snap, rotate, close.
- **Workflows:** save and reload whole arrangements; missing apps are relaunched.
- **Closed apps** keep a dimmed card you can click to relaunch. A shortcut wall launches apps
  without leaving the view.
- **Backgrounds:** grid room, plain colours, your wallpaper, 360° panoramas or animated skies.
- **Options menu** (Esc): renderer/GPU, anti-aliasing, field of view, speed, card titles,
  hints on/off, frame rate caps. Esc twice closes the 3D view.

## Performance

Measured on a laptop with a Ryzen 780M (integrated GPU), 2880x1800.

- **Idle:** 0% CPU (purely event-driven, no polling). Working set about 2 MB.
- **Opening:** about 10 ms with the default warm mode. The catch: that keeps about 470-500 MB of
  GPU/driver memory reserved while idle. `keep_surface: false` gives about 100 ms opens for
  about 250 MB; `keep_warm: false` frees everything when closed (about 20 MB idle) and opens in
  about 1 s.
- **In the view:** about 5% CPU while watching a live card (a YouTube video, say). The GPU
  only draws frames that change, so a still view costs almost nothing. The frame rate caps are
  in the options menu (Esc).
- **Screenshots** are taken once when a window loses focus, downscaled and saved compressed.

## Known issues

- **The desktop card is the weakest part.** It shows the whole primary screen live and passes
  clicks through to it. But "always on top" popups (tray menus, tooltips, Task Manager) appear
  twice, at their real spot over the 3D view and on the card. Start, search and the taskbar
  can't be covered at all; Windows only lets signed system-level apps above them.
- Apps run as administrator can't be controlled from a normal app, so some of their windows and
  menus misbehave in the 3D view.
- Using a live card in place swaps the system cursors for blank ones and draws the pointer
  on the card. Apps with their own custom cursors show them at the real spot instead. Parts of
  a window that are off-screen can't be reached.
- Locking the PC, sleep, monitor or resolution changes, and GPU driver resets close the 3D view
  and rebuild it on the next open.
- Windows only (10 version 2004+ or 11, 64-bit, DirectX 12 GPU). It's a hobby project; expect
  rough edges.
- You'll run into lots of small, unexpected issues (every app behaves a bit differently in the
  3D view), so I recommend forking it and fixing the ones you hit. [PROBLEMS.md](PROBLEMS.md)
  records how each past one was tracked down.

## Build and run

```
cargo build --release
target\release\window3d.exe
```

It sits in the tray. Right-click the tray icon for settings, "Start with Windows" and Quit.
Settings live in `%APPDATA%\Window3D\config.json`.

More detail (every key, every setting, how it works inside): [docs/GUIDE.md](docs/GUIDE.md).
Fixes and dead ends: [PROBLEMS.md](PROBLEMS.md).

## License

[MIT](LICENSE)
