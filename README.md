# Robin

Robin is a native, offline file manager and media viewer for [R2-D2](https://github.com/5kyguy/r2-d2), sky's Hyprland desktop. It replaces both Thunar and Omaroll: file browsing, image viewing, and video playback live in one application. The backend is Rust and the interface is Slint. The application id is `dev.skyguy.robin`.

Robin is in development.

Robin is licensed under the GNU General Public License version 3 only (GPL-3.0-only). There is no "or later" option.

## Copy and move conflicts

Cancelling a copy or move removes the items that job added. It does not
restore a file that a Replace or Merge overwrote; that file keeps the
copied contents. Anything the cleanup can't remove is listed under the
job's Details.

## The list

Click a column header to sort by it, again to reverse; folders stay on
top and names sort naturally (`frame-2` before `frame-10`). Modified
shows the time for today and the date before that. The open folder
refreshes by itself when other apps change it, keeping the selection
and scroll position.

## Drag and drop with other apps

Robin is built on Slint 1.17.1, which limits what it can do:

- **Out of Robin (drag to another app): not supported.** Slint's
  `DragArea` only works inside the window, its `DataTransfer` carries
  plain text, an image or in-app data but no `text/uri-list`, and the
  winit backend has no way to start a system drag.
- **Into Robin (drop from another app): X11 only.** Files dropped on
  the window are copied or moved into the open folder: moved on the
  same filesystem, copied across, as in Thunar. The drop arrives through
  winit's `DroppedFile`, which has no pointer position and no modifier
  keys, so a drop always lands in the open folder (not the folder row
  under the pointer), Ctrl/Shift can't pick copy or move, and there is
  no hover highlight while dragging. Items already in the open folder,
  and folders dropped into themselves, are ignored.
- **On Wayland, drops from other apps do nothing.** winit 0.30 does not
  implement file drops there.
