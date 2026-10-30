# Robin plan

Robin is the file manager and media viewer for R2-D2, sky's Hyprland desktop (`github.com/5kyguy/r2-d2`). It replaces both Thunar and Omaroll. Browsing, image viewing, and video playback ship in this application. Rust backend, Slint 1.17 UI, application id `dev.skyguy.robin`, license GPL-3.0-only.

The visual contract is `docs/DESIGN.md`, copied verbatim from Designr's spec. That file wins over the mockups. Mockups are a direction only. This plan folds in sky's overrides (viewer and statistics are in scope, replace both Thunar and Omaroll) and the safety rules from review.

R2-D2 integration (keybinds, the bar, default MIME handlers) happens later in the r2-d2 repository. Sky does that. This repository does not touch r2-d2.

## Staged migration

Robin takes over in three steps, so the desktop is never without a file manager or a viewer:

1. **Browsing.** Robin becomes the daily file manager. Thunar is unpinned once list and grid, trash, and copy/move are in use.
2. **Images.** JPEG, PNG, HEIC, and the other still types open in Robin's viewer. The external image viewer stops being the default for those types.
3. **Video.** Local video opens in Robin through libmpv. Omaroll is retired for local files after the viewer has transport, resume, and the filmstrip.

Delegation remains for types Robin does not render itself (PDF, archives, annotation, wallpaper, share, terminal).

## Architecture

One crate, library plus `robin` binary.

| Module | Job |
|---|---|
| `theme` | Read `theme.json`, derive `role@NN%` tints, reload on inotify |
| `safety` | Directory fds, `O_NOFOLLOW`, pseudo-filesystem check by `f_type` |
| `model` | Directory listing, sort, hidden filter, search, selection, breadcrumbs, debounced notify, grid rows |
| `thumbs` | Freedesktop thumbnail cache, image / ffmpegthumbnailer / pdftoppm, LRU bound |
| `fsops` | Rename, create, trash, permanent delete, copy/move, clipboard |
| `stats` | Walk, largest files, type breakdown, squarified treemap, per-mount usage, cache sizes |
| `mounts` | UDisks2 block devices, plus `/proc/self/mountinfo` for the sidebar |
| `delegate` | Fixed argv for the apps Robin does not replace |
| `instance` | Unix-socket single instance and `--select` |
| `render` | Deterministic `--render` frames from the design checklist |

UI lives in `ui/*.slint` and is built with `slint-build`. The window loads JetBrainsMono Nerd Font by file, not by whatever the desktop has installed. Glyphs that the spec names are used at those codepoints. Where the spec's grid table left a cell blank, the codepoint is recorded in `docs/STATUS.md` after checking the font cmap. The TOML stand-in is U+E6B2 (`custom-toml`), which exists in JetBrainsMono Nerd Font. The list mockup draws that cell blank.

### Theme

Designr's spec sets the path: `$XDG_CONFIG_HOME/r2-d2/theme.json`, defaulting to `~/.config/r2-d2/theme.json`, generated from R2-D2's `config/theme/palette.toml`. This replaces the earlier note that pointed at `$XDG_STATE_HOME/r2-d2/`. Robin reads the `dark` object only and ignores `light`. Missing keys fall back to the cyan reference values in the spec. A reload crossfades over 120ms.

No chrome color is a literal hue. Rust computes every tint as a role plus alpha and passes the resulting colors into Slint. The spec's explicit exceptions stay exceptions: scrim `#000` at 60%, and the viewer canvas (black blended 70% toward `bg`). Charts use `accent` at the alphas in section 6, plus optional `deep` and `spark` when the theme provides them.

### Browsing

Places sidebar (pinned, devices, network only when gvfs or sshfs mounts exist, trash, statistics). List and grid. Grid is a virtualized list of rows of tiles, not one element per file. Sort by name, date, size, or type. Hidden files. Multi-select. Arrows and hjkl. Search-as-you-type. Editable breadcrumbs. Status bar with counts, selection size, and free space.

The listing is a Rust model over `openat` plus the `notify` crate. Notify events are debounced and applied incrementally. A rescan happens on overflow.

### Thumbnails

Images through the `image` crate. Video through `ffmpegthumbnailer`. PDFs through `pdftoppm`. Those subprocesses use a fixed argv and never a shell. The cache is the shared freedesktop directory `~/.cache/thumbnails/{normal,large}` (normal at 1x, large from 1.5x DPR upward), with `Thumb::URI` and `Thumb::MTime` metadata. Robin tracks the files it wrote and LRU-evicts those until its own entries fit in 256 MB. It does not delete another application's thumbnails to meet that cap.

### Writes

Writes are explicit.

- F2 renames in place. `Ctrl+Shift+N` creates a folder. `Ctrl+Alt+N` creates a file.
- Del trashes, with a confirm. On a volume with no usable `.Trash/$UID` or `.Trash-$UID`, the dialog says so and offers a permanent delete. Robin does not copy the file into the home trash unless the user confirms that fallback.
- Shift+Del permanently deletes after the danger dialog. Cancel is focused. The danger button stays disabled for 600 ms. The delete never follows symlinks.
- Copy and move run on a worker with progress and cancel. Conflicts resolve as replace, rename, or skip. A same-filesystem move is `rename`. On `EXDEV`, Robin copies to a temporary name, fsyncs, renames into place, and only then deletes the source. Cancel removes the partial file. Mode and mtime are preserved.
- Clipboard cut, copy, and paste. Symlinks are copied and moved as links, not as their targets.

The `trash` crate (5.2.9) falls back to a home-trash copy when a volume trash cannot be created. That is the silent copy this project is not allowed to do, so trash is implemented in-tree to the FreeDesktop trash spec, using the spec's "refuse, and warn" branch unless the caller passes an explicit home-fallback confirmation.

### Safety

- Browsing is read-only. There is no shell. Every external program is a fixed argv.
- Mutations go through a directory file descriptor. `openat` uses `O_NOFOLLOW`. A test swaps a directory entry for a symlink between lookup and delete and checks that the symlink target is untouched.
- Pseudo filesystems are refused by `f_type` (`procfs`, `sysfs`, `devtmpfs`), not by a path prefix.
- There is no undo in v1. The trash toast is the recovery path for Del.

### Statistics

A scroll page, not a modal: per-mount bars (warning at 85%, danger at 95%), a squarified treemap of the largest folders, a file-type breakdown, cache sizes with a confirmed clear, and trash size with a danger confirm that names the byte count. The treemap is rectangles on purpose: labels sit inside cells, area compares more honestly than arc angle, and hit testing stays simple in Slint.

### Viewer

Full-bleed image and video, filmstrip, zoom and pan, metadata panel, slideshow. Video is libmpv's software render API (`MPV_RENDER_API_TYPE_SW`, format `rgb0`) into a buffer, the padding byte forced to opaque, then `Image::from_rgba8` on a Slint texture. The OpenGL render API can target `BorrowedOpenGLTextureBuilder` later. It needs a shared GL context, which is fragile next to Slint's own context, so it is not the v1 path. GStreamer is the fallback only if the libmpv software path cannot be built.

### Mounts

Sidebar devices come from `/proc/self/mountinfo` so usage bars work without D-Bus. Mount and unmount go through UDisks2 (`zbus`).

### Delegation

Fixed argv, never a shell:

| Action | Program |
|---|---|
| PDF | `zathura` |
| Archives | `file-roller` |
| Annotation | `tensaku` |
| Set wallpaper | `r2-d2-theme-bg-set` |
| Share | `r2-d2-cmd-share` |
| Terminal here | `xdg-terminal-exec` |
| Open with | XDG portal via `ashpd` |
| Notifications | `r2-d2-notification-send` |
| Copy path | `wl-copy` |

### Single instance

A Unix socket under `$XDG_RUNTIME_DIR`. A second process forwards its path and exits. `robin --select <file>` reveals that file in the running instance.

### Interface

Slint 1.17. `slint::set_xdg_app_id("dev.skyguy.robin")` runs before the window is shown. Motion matches the spec: 180 ms enter `cubic-bezier(0.2, 0.8, 0.2, 1)`, 120 ms ease-in exit, spring stiffness 400 damping 40 mass 1. Reduced motion follows `gtk-enable-animations=false` and collapses those into a 120 ms crossfade or a cut. Flat fills only. No gradients. No drop shadows.

`--render` is headless-capable through the winit software renderer when a display or Xvfb is present, with `SLINT_SCALE_FACTOR=1`. The checklist in `docs/DESIGN.md` section 8 is the screenshot set. Frames use the cyan reference palette, a fixed clock of 2026-10-30 18:58, and animations at their end state. One pass uses the black theme for frames 1, 3, 24, and 28.

`--demo` materializes the fictional fixture tree used by tests and by `--render`.

## Testing

- `cargo fmt --check`, `cargo clippy --all-targets -- -D warnings`, `cargo test`.
- Filesystem mutations are tested against a temporary directory opened as a capability. The bubblewrap wrapper is `scripts/test-bwrap.sh`.
- The permanent-delete test replaces an entry with a symlink mid-walk.
- `--render` writes PNGs. A smoke test checks dimensions and that the background pixel matches the theme.

## Milestones

Each milestone is one pull request, stacked on the previous branch, with commits that contain only that milestone. Pull requests are squash-merged. After a parent lands, the child is rebased onto main and retargeted.

1. **Docs.** This plan, the verbatim design spec, status, and the five mockups in `docs/mockups/`.
2. **Spikes.** Wayland app id, OS drag-out, a 50k-file directory, ARGB translucency, the GPL/Slint license, libmpv software render into a buffer. Results live in `docs/STATUS.md`.
3. **Skeleton.** Crate, CI, theme loader, fixture, `--demo`, `--render` harness.
4. **Browser.** List, grid, virtualized rows, sort, selection, keyboard, search, breadcrumbs, sidebar, status bar, and the browser frames from the checklist.
5. **Thumbnails.** Generators and the freedesktop cache.
6. **Fsops.** Rename, create, trash, permanent delete, copy/move, clipboard, with the sandbox tests in the same PR.
7. **Viewer.** Images, video transport, filmstrip, zoom.
8. **Statistics.** Scanner, treemap, disk bars, cache clear.
9. **Mounts.** UDisks2 client.
10. **Delegation.** Fixed argv helpers.
11. **Single instance.** Socket lock and `--select`.
12. **Packaging.** PKGBUILD, desktop file, release tarball.

## Packaging

Release tarballs are `robin-vX.Y.Z.tar.gz` plus `SHA256SUMS`. The PKGBUILD builds with `cargo build --release --locked`. The desktop file uses `StartupWMClass=dev.skyguy.robin`.

## Out of scope for v1

Undo, a cloud account, thumbnailing inside `/proc`, and any commit to the r2-d2 repository.
