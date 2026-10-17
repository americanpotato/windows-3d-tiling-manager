Window3D
========

Fly through your open windows in 3D. Every window becomes a card floating in a room; click one
to watch it live and use it right there, or jump to it.

Requirements: Windows 10 (version 2004 or later) or Windows 11, 64-bit, a DirectX 12 graphics card.


Getting started
---------------

1. Run window3d.exe. Nothing opens: Window3D waits in the tray (the three-cards icon by the
   clock) and a balloon tells you the hotkey.
2. Press Ctrl+Alt+Z to open the 3D view. Press it again (or Esc twice) to close it.
3. Right-click the tray icon for settings, "Start with Windows" and Quit.

No installer: keep window3d.exe wherever you like. Settings and screenshots are kept in
%APPDATA%\Window3D.


In the 3D view
--------------

  Mouse                 look around (the crosshair picks a card)
  W A S D / arrows      fly           Space / E   up        Q / C   down
  Shift                 faster        Mouse wheel  move forward / back
  Click a card          watch it live and use the window right there
                        (click outside it, or press the hotkey, to fly again)
  Shift+click / Enter   close the 3D view and switch to that window
  Hold and drag         move a card (the wheel pushes it away / pulls it closer)
  /  then a name        search; Tab next match, Enter opens, Esc back
  F                     fly up to the card until it fills the view
  L                     live view on/off for the card
  Home                  back to the middle of the room
  Shift+T               tidy the window cards
  G                     a 3x3 grid of slots; drop cards in to line them up
  M                     move the 3D view to the next monitor
  Tab                   free the mouse
  Esc                   options (renderer / GPU, anti-aliasing, field of view...)
  Esc Esc               close

Hover a card to get its toolbar: live view, sound, size, move, snap, rotate and close.

Save and restore whole arrangements of windows from the tray menu (Workflows).


Settings
--------

Tray icon > Edit settings opens config.json in Notepad. Save it and the changes apply the next
time you open the 3D view (a balloon confirms, or tells you what is wrong in the file).
Delete config.json to get the defaults back.


Uninstalling
------------

Untick "Start with Windows" in the tray menu, quit, delete window3d.exe and the folder
%APPDATA%\Window3D.
