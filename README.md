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

**Drag and drop with other apps doesn't work on Wayland yet, including
Hyprland.** Dropping files from another app into Robin does nothing
there, and dragging files out of Robin into another app doesn't work
on any platform.

Why, and what does work (Robin uses Slint 1.17.1 on winit 0.30):

- **Dragging out of Robin: not supported anywhere.** Slint's
  `DragArea` only works inside the window, its `DataTransfer` carries
  plain text, an image or in-app data but no `text/uri-list`, and the
  winit backend can't start a system drag.
- **Dropping into Robin on Wayland: not supported.** winit 0.30 has no
  Wayland drag and drop
  ([winit#1881](https://github.com/rust-windowing/winit/issues/1881)).
  It was added in winit 0.31
  ([winit#4571](https://github.com/rust-windowing/winit/pull/4571),
  merged July 2026, in the 0.31 betas), but Slint still builds on winit
  0.30 (`slint::winit_030`), so Robin can't use it until Slint moves to
  0.31.
- **Dropping into Robin on X11: works, with limits.** Files land in the
  open folder, and are always **copied**, never moved: winit hands Robin
  paths with symlinks already resolved, so moving could relocate a
  link's target instead of the link. Dropped items that are still
  links are refused. See `docs/DESIGN.md` for the details. winit 0.30 reports only the paths, with no pointer position
  and no modifier keys, so a drop always lands in the open folder (not
  the folder row under the pointer), Ctrl/Shift can't pick copy or
  move, and nothing highlights while dragging. Items already in the
  open folder, and folders dropped into themselves, are ignored.

### Opt-in workaround on Wayland: run Robin through XWayland

winit picks Wayland when `WAYLAND_DISPLAY` is set and falls back to X11
when it is empty and `DISPLAY` is set. Starting Robin with an empty
`WAYLAND_DISPLAY` makes it an XWayland window, where the X11 drop path
above applies:

```sh
WAYLAND_DISPLAY= robin
```

This needs XWayland running (on Hyprland, `xwayland { enabled = true }`,
the default). Whether a drop from a native Wayland app reaches an
XWayland window depends on the compositor bridging Wayland drags to
XDND; drops from other XWayland apps go through X11 directly. This has
not been tested on Hyprland. Tradeoffs:

- It may look blurry on scaled outputs, since XWayland windows are
  upscaled unless `xwayland { force_zero_scaling = true }` is set (which
  then needs app-side scaling, e.g. `SLINT_SCALE_FACTOR`).
- Robin loses native Wayland: no fractional scaling, and Wayland
  clipboard and input go through XWayland.
- The X11 drop limits above still apply.

It isn't the default and Robin doesn't add a flag for it; the variable
is all it takes.
