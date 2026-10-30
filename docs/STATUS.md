# Status

Robin is in development. The visual spec is `docs/DESIGN.md` (Designr's spec, verbatim). The spec wins over the mockups.

## Design assets

`docs/DESIGN.md` was copied byte-for-byte from Designr's spec.

Designr's mockups are committed at `docs/mockups/{01-list,02-grid,03-viewer,04-stats,05-dialogs}.png`. The spec still cites `/workspace/robin-design/mockups/` and wins over the pictures.

## Milestones

| Milestone | State |
|---|---|
| Docs (plan, design spec, status) | in review |
| Spikes | recorded below |
| Skeleton, CI, `--demo`, `--render` harness | in review |
| Directory model (sort, selection, search, grid rows) | in review |
| Browser UI | in review |
| Thumbnails | in review |
| Fsops | in review |
| Viewer | in review |
| Statistics | in review |
| Mounts (UDisks2) | in review |
| Delegation | in review |
| Single instance and `--select` | in review |
| Packaging | in review |
| Thumbnails | not started |
| Fsops | not started |
| Viewer | not started |
| Statistics | not started |
| Mounts (UDisks2) | not started |
| Delegation | not started |
| Single instance and `--select` | not started |
| Packaging | not started |

UI screens have not been built yet.

## Spikes

Recorded on this machine (Linux 6.12, Rust 1.99.1, libmpv 0.37, Slint 1.17 docs). Hyprland was not available: `DISPLAY=:1`, `WAYLAND_DISPLAY` unset.

### Wayland app id

`slint::set_xdg_app_id` exists in Slint 1.17 and sets the xdg app id for Wayland and X11. It has to run before the window is shown. Robin will call it with `dev.skyguy.robin`. Not exercised against a compositor here. No API gap.

### OS drag-out

Slint 1.17 adds `DragArea`, `DropArea`, and `data-transfer` for drag and drop inside the window. The 1.17 release notes say dragging to other applications, and receiving drops from them, is still in development in winit. In-app drags can be built. Dragging a file out to another Wayland client cannot, until winit grows that. Paste and "copy path" cover the gap. This is a product blocker for OS drag-out only.

### 50k-file directory

Created 50,000 small files in a temp directory and timed a cold listing:

| Step | Time |
|---|---|
| create | 0.801 s |
| `listdir` | 0.013 s |
| `lstat` of every name | 0.115 s |
| sort by name | 0.016 s |
| sort by size | 0.011 s |

List plus stat plus sort is about 144 ms. Reading the directory is not the risk. Drawing 50,000 widgets would be. The grid stays a virtualized list of rows.

### ARGB translucency

Slint 1.17's FemtoVG WGPU backend selects a non-opaque swapchain composite alpha mode when the window is transparent. That path was not run on Hyprland. Robin's scrim, `panel_float`, and selection tints are alpha inside an opaque window, so they do not need a translucent surface. A transparent top-level window on Hyprland is unverified. Not a blocker for the matte UI.

### GPL and Slint

Slint 1.17 is usable under GPL-3.0-only, a royalty-free proprietary license, or a paid license. Robin is GPL-3.0-only and takes the GPL-3.0-only option. libmpv 0.37 is GPL-2.0-or-later, so a combined binary can be shipped as GPL-3.0-only. `ffmpegthumbnailer` and `pdftoppm` are subprocesses, not linked. No license blocker.

### libmpv into a Slint texture

Executed against libmpv 0.37. A 32×24 red clip was loaded with `vo=libmpv` and rendered through `MPV_RENDER_API_TYPE_SW`.

- `mpv_render_context_create` returned 0.
- The first `mpv_render_context_render` ran before a frame existed and produced a black buffer. Rendering after `mpv_render_context_update` reported `MPV_RENDER_UPDATE_FRAME` produced a frame.
- The center pixel was R=255, G=0, B=0. The fourth byte of `rgb0` was 0.

`rgb0`'s fourth byte is padding, not alpha, and libmpv documents it as uninitialized. `slint::Image::from_rgba8` treats that byte as alpha, so Robin sets it to 255 after every frame. Otherwise the video is invisible.

The buffer was not uploaded into a Slint window in this spike. The v1 mapping is a `SharedPixelBuffer<Rgba8Pixel>` handed to `Image::from_rgba8` on each frame. The OpenGL render API plus Slint's borrowed GL texture is faster and is deferred: sharing Slint's GL context has a history of breaking the UI's own drawing. GStreamer is not required.

## Glyphs

Checked against `JetBrainsMonoNerdFont-Regular.ttf` (name ID 16: "JetBrainsMono Nerd Font", Nerd Fonts 3.4.0). Every codepoint named in the spec is in the cmap.

The spec's grid table left markdown, the code-language cells, font, and executable blank, and the list mockup's TOML glyph does not draw. These are the stand-ins, each present in that font:

| Kind | Glyph | Codepoint | Nerd Font name |
|---|---|---|---|
| markdown | `󰍔` | U+F0354 | `md-language_markdown` |
| rust | `󱘗` | U+F1617 | `md-language_rust` |
| python | `󰌠` | U+F0320 | `md-language_python` |
| javascript | `󰌞` | U+F031E | `md-language_javascript` |
| typescript | `󰛦` | U+F06E6 | `md-language_typescript` |
| shell | `󱆃` | U+F1183 | `md-bash` |
| toml | `` | U+E6B2 | `custom-toml` |
| json | `󰘦` | U+F0626 | `md-code_json` |
| font | `󰛖` | U+F06D6 | `md-format_font` |
| executable | `󰆍` | U+F018D | `md-console` |

TOML is U+E6B2. It is the only nerd-font glyph whose name is toml, and it is in the font.

## Blockers

- OS drag-out waits on winit. In-app drag does not.
- A translucent top-level window was not verified on Hyprland.
- libmpv software render works. Uploading that buffer into a Slint `Image` is still to be wired when the viewer is built.
