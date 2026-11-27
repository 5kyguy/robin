# Robin: visual design spec v1

Robin is a Rust and Slint file manager and media viewer for R2-D2 on Hyprland. It replaces both Thunar and Omaroll.
Author: Designr. Implementer: Developr. Every value below is final unless a `--render` review changes it.

**Grounding.** The look comes from Omaroll (`/workspace/omaroll-ref`, btsouth/omaroll). Each section cites the QML file it draws on. Omaroll's matte style has these traits: flat fills, `shade(bg, 0.84–0.92)` panels, a 1px border at `text @ 12%`, a 4px radius fallback, no gradients, no drop shadows, and glyph icons.

---

## 0. Theme contract

- Robin reads `~/.config/r2-d2/theme.json` (generated from `config/theme/palette.toml`) at startup, and reloads it live through an `inotify` watch on that file. A reload crossfades the old colours to the new ones over 120ms.
- **Dark only.** Robin ignores `[light]`.
- Roles: `bg surface line accent on_accent text muted danger on_danger warning on_warning` (plus `deep` and `spark`, which are optional and used only by the charts in section 6).
- Mockup reference values (cyan dark): bg `#061216`, surface `#0E1E26`, line `#1E6A78`, accent `#00E5FF`, on_accent `#042028`, text `#E7FBFF`, muted `#7EC8D4`, danger `#FF2A4A`, warning `#FFB000`.
- **Derived tints.** No colour may be hard-coded. Every tint is a role with alpha, written `role@NN%`:

| Token | Value | Source |
|---|---|---|
| `panel` | `bg` mixed toward `surface`; use `surface` | FolderSidebar `shade(bg,0.85)` |
| `panel_float` (viewer chrome) | `bg@86%` | ViewerToolbar/Transport `shade(bg,0.84)` |
| `hairline` | `text@12%` | all Omaroll panels, border `shade(fg,0.12)` |
| `divider` | `line` (1px) | r2-d2 |
| `hover` | `text@6%` | IconButton |
| `selected` | `accent@18%` fill + `text` label | FolderSidebar `shade(accent,0.18)` |
| `selected_active` (focused list) | `accent@28%` | IconButton `active` `shade(accent,0.30)` |
| `focus_ring` | 2px inside stroke `accent` | IconButton `activeFocus ? 2 : …` |
| `keyboard_cursor` (not selected) | 1px stroke `accent` | FolderSidebar `keyboardCurrent ? 1` |
| `drop_target` | `accent@12%` fill + 2px dashed `accent` stroke (dash 6 gap 4), radius md | new |
| `drop_target_invalid` | `danger@10%` fill + 2px `danger` stroke, cursor "not-allowed" | new |
| `disabled` | opacity 0.42 | IconButton `enabled ? 1 : 0.42` |
| `scrim` | `#000@60%` | viewer and dialogs |

## 1. Tokens

### Spacing (4px base)
`s0 0 · s1 2 · s2 4 · s3 6 · s4 8 · s5 12 · s6 16 · s7 24 · s8 32 · s9 48`. Gaps inside a panel are 4 (filmstrip spacing). Panel padding is 8 (FolderSidebar margins) and info-panel padding is 16 (ViewerInfoPanel).

### Radii
`r-xs 2` (badges, RAW chip: ViewerFilmstrip) · `r-sm 3` (thumb tiles, icon buttons: IconButton `min(h/2,3)`) · `r-icon 4` (glyph tiles, house rule from den-den-mushi) · `r-md 6` (rows, inputs, sidebar items: FolderSidebar `min(6, …)`) · `r-lg 10` (floating viewer chrome, dialogs, cards: ViewerFilmstrip `min(r,10)`) · `r-pill h/2` (scrubber, progress bar, toggles).

### Type: JetBrainsMono Nerd Font only, sizes in px
| Token | Size/weight | Use |
|---|---|---|
| `t-caption` | 9 / 600, uppercase, +0.8px tracking | badges (RAW, EXT) |
| `t-small` | 11 / 400 | metadata, status bar, column values, sidebar counts (FolderSidebar, ViewerInfoPanel) |
| `t-label` | 11 / 700, UPPERCASE, +1px tracking, `muted` | section headers (den-den-mushi house style) |
| `t-body` | 12 / 400 | rows, buttons, menus (IconButton, FolderSidebar) |
| `t-title` | 13 / 700 | viewer filename, dialog titles (ViewerInfoPanel) |
| `t-h1` | 18 / 700 | statistics hero numbers, empty-state titles |
| `t-display` | 28 / 700 | disk-usage percent |
Line height is 1.35 times the size. Columns of numbers use tabular figures (JetBrainsMono figures are already fixed-width).

### Density
| | Compact | Comfortable (the app) |
|---|---|---|
| list row | 24 | 30 |
| sidebar row | 26 | 30 |
| toolbar | 40 | 46 (ViewerToolbar implicitHeight 46) |
| icon button | 28×28 | 34×34 (IconButton 34) |
| grid tile (S/M/L) | 96/128/176 | 112/152/208 |
| status bar | 22 | 26 |

The app is always comfortable and has no density control. Compact exists only for the render harness (`--density compact`, `browser-list-compact`).

### Icon sizes
Glyph sizes: 14 in rows and the sidebar (FolderSidebar glyph 14 in an 18px box) · 16 on toolbar buttons · 20 in menus at comfortable density · 48 as the grid type glyph on a medium tile (0.32 times the tile width) · 64 in empty states. Icon boxes use `r-icon 4`.

## 2. Window layout (1280×800 reference)

```
┌ sidebar 232 ┬──────────── toolbar 46 ────────────────────────┐
│ PINNED      │ ◀ ▶ ▲  [ ~ › Pictures › 2026 ]   ⌕  ☰ ▦  ⋯    │
│ DEVICES     ├────────────────────────────────────────────────┤
│ TRASH       │ content (list | grid)                          │
│             │                                                │
│ [stats]     ├──────── progress strip 44 (when active) ───────┤
└─────────────┴──────── status bar 26 ───────────────────────────┘
```

### Resizing
1280×800 is the reference: every frame at that size is drawn as specified above. Smaller windows keep the same parts and shrink, elide, wrap, scroll, or move them; nothing new is drawn for them.
- **Floor.** The window asks for at least 480×360. Every view below is laid out to stay usable down to that size.
- **Shell.** The sidebar folds to its rail on its own; list columns shrink and Type drops out before a name is cut; the breadcrumb field takes what the toolbar buttons leave; the status bar shows the short free-space text. In the progress strip the label (280) and the figures (228) shrink and elide, and the bar is dropped once it would be under 48.
- **Dialogs** are their spec width or the window − 32, whichever is smaller, and never taller than the window − 32. Name lists and the refused-move list scroll; the buttons always show. Conflict cards share the dialog's width and the buttons stay right-aligned.
- **Context menus** that would run off the right or bottom edge open left of or above the pointer, 8 inside the window, and scroll when taller than it. Toasts, the drag label and the path completion dropdown are capped to the window; an expanded toast's list scrolls in the room above the status bar.
- **Shortcut overlay:** min(760, window − 32) × min(520, window − 32). Its sections sit in three columns from a 700px panel, two from 420, otherwise one, and scroll under the filter.
- **Viewer.** The top bar is min(560, window − 32) and the toolbar min(300, window − 32). A filmstrip wider than the window − 32 is cut at its ends and slid so the current tile stays centred. From a 720px window the info panel docks and the picture fits beside it; narrower, it floats at the right below the top bar and above the toolbar, or above the filmstrip while the strip shows so the current tile stays visible (min(300, window − 32) wide), and its facts scroll. Where they would touch, the zoom chip drops under the top bar and the minimap rises above the toolbar or filmstrip. The video transport is window − 120, or window − 32 below 600 (the width the picture has, when the info panel docks).
- **Statistics** scrolls. FILE TYPES sits beside the 655px treemap card while it keeps 300, otherwise under it at full width; the treemap stretches to its card. CACHE and TRASH sit side by side while each keeps 360, otherwise stack. A disk row under 560 moves its free-space line under the bar.

### Places sidebar (FolderSidebar.qml)
- Width 232. Drag the 6px handle on the right edge (FolderSidebar width 6) to resize it between 180 and 360. `Ctrl+B` collapses it to a 52px glyph rail, and below a 720px window width it auto-collapses. Collapsing and expanding use the spring.
- Fill `surface`. Right edge is a 1px `divider` line.
- Padding is 8. Each section has a header row 24px tall in `t-label` style.
- Sections, in order:
  - **PINNED**: Home, Desktop, Documents, Downloads, Pictures, Videos, plus user pins added by drag-in or the context menu "Pin".
  - **DEVICES**: mounts from `/proc/self/mountinfo` with real filesystems, plus the removable drives UDisks2 lists (USB sticks, SD cards, external disks). Each mounted row has a 3px usage bar beneath it: `accent`, or `warning` at ≥85%, or `danger` at ≥95%. Removable devices keep the USB glyph `󰕓` as their row icon and show an eject button `󰇪` (nf-md-eject, U+F01EA) 22×22 on hover, in place of the count.
    - **Plugged in, not mounted:** the row's name is `muted`, with no count and no usage bar. One click mounts the drive (UDisks2 `Filesystem.Mount`, no `pkexec`) and opens it. A mount that fails shows a `danger` toast, "Couldn't mount SANDISK: <reason>". The hover eject button still shows, and only powers the drive off; hovering it shows the tooltip "Power off" (`t-small` `text` on `bg`, 1px `hairline`, radius `r-sm`, 22 tall, left of the button).
    - **Read-only:** a small `muted` pill "Read-only" (`t-small`, `text@8%` fill, radius `r-sm`) sits left of the count; the usage bar stays. Pastes and drops onto it get the existing refused-move state. An empty folder on it shows "SANDISK is read-only" / "Files can be opened but not changed" (section 3), with no drop hint and no [New folder].
    - **Arrival:** a drive plugged in while Robin runs shows an info toast "SANDISK connected · 29.8 GB" with [Open], gone after 4s. Drives already present at startup get no toast. A drive with no label is named by its size, "29.8 GB drive".
    - **Eject:** the hover button, the row's right-click menu (Open, a separator, Eject) and the statistics DISKS card all eject. Eject acts on the whole physical drive: every mounted partition on it is unmounted in turn, then the drive is powered off, or only unmounted when it can't be powered off. If every partition came off but the power-off is refused, a `warning` toast says "SANDISK is unmounted but couldn't power off: <reason>", with no [Force] (a forced unmount can't help). Success shows "SANDISK can be unplugged", naming every partition on the drive ("SANDISK and BACKUP can be unplugged"; three or more read "A, B and C"), and all of the drive's rows disappear together in one redraw, never one before another. If the open folder is on the drive, Robin goes Home first.
    - **Busy on eject**, as a `warning` toast:
      - Robin's own job on any partition of the drive: "Copying 3 items to BACKUP", naming the partition the job touches, with [Wait] [Cancel copy] and no Force. [Wait] ejects as soon as the job finishes ("SANDISK will eject when the copy finishes"); [Cancel copy] cancels it and then ejects.
      - Another program holding one of its partitions: "SANDISK is in use by vlc", naming that partition, with [Retry] [Force] (Force in `danger`). The program is found by scanning `/proc/*/fd` and `cwd` for the mount, else "another program". Force is a UDisks2 unmount with `force`.
    - **Pulled out without eject:** the row disappears; if the open folder was on it, the pane shows "SANDISK was removed" (section 3) and no toast, since the pane already says it. Pulled out while another folder is open, a `warning` toast says "SANDISK was removed without being ejected". A drive with several partitions gets ONE toast naming them all, "SANDISK and BACKUP were removed without being ejected" (three or more: "A, B and C were removed…"). A Robin job on the drive stops with a `danger` toast, "SANDISK was removed: copied 3 of 8 items".
    - **Copies onto a drive** check free space first ("Not enough space on SANDISK: 7.0 GB needed, 6.0 GB free", `danger`; Details: "Files replacing ones on SANDISK count in full. Nothing was copied.", since the check doesn't net out overwrites) and, on FAT32, files over 4 GB ("holo.img is too big for SANDISK", or "2 files are too big for SANDISK", `warning`; Details pins "SANDISK is FAT32: files must be under 4 GB. Nothing was copied." and then names every file over 4 GB with its size). Nothing is copied in either case.
    - **Without UDisks2** (no system bus): rows come from mountinfo alone, so there are no unmounted rows and no eject buttons; a mount that vanishes still shows the removed state.
    - **Phones and cameras (MTP/PTP through gvfs):** an Android phone is listed under DEVICES as soon as gvfs's MTP volume monitor sees it, and a camera when the gPhoto2 monitor does. A phone row uses `󰄜` (nf-md-cellphone, U+F011C) and a camera row uses `󰄀` (nf-md-camera, U+F0100). The row is named as gvfs names the device ("Pixel 8"). It has no count, no usage bar and no "% used", because each of these would be another round trip over USB. Inside a storage, the status bar shows that storage's free space, "12 GB free of 128 GB". It is read off the UI thread with statvfs on the gvfs folder, which gvfsd-fuse answers per storage from gvfs's `filesystem::free` and `filesystem::size`. The phone's top folder shows none, because there gvfs sums every storage. The hover eject button shows as on a stick.
      - **Plugged in:** the row is `muted`, as for an unmounted drive. Arrival shows an info toast "Pixel 8 connected" with [Open], with no size because gvfs gives none. One click mounts the phone through gvfs (`gio mount`) and opens its top folder, which lists its storages ("Internal shared storage", "SD card").
      - **Waiting for access:** a locked phone, or one where "Allow" hasn't been tapped, either fails to open or opens empty. Either way the pane shows `󰄜` "Pixel 8" / "Unlock your phone and allow file access" [Retry], and the row stays `muted`. [Retry] opens the phone again. If it is still locked, an info toast says "Pixel 8 still hasn't allowed file access" and the pane keeps waiting. Once access is allowed, the storages list.
      - **Camera waiting:** a camera that is off, asleep or not in PTP mode fails gvfs-gphoto2's setup ("Error initializing camera") or opens empty. The pane uses the same layout: `󰄀` "Canon EOS R6" / "Turn on Canon EOS R6 and set its USB mode to PTP" [Retry]. A retry that still can't open it gives an info toast, "Canon EOS R6 still isn't ready".
      - **What a phone can't do:** each refusal comes up front with its reason, as a `warning` toast, and nothing is attempted. At the phone's top folder, a paste or drop gives "Files can only go inside a storage on Pixel 8", and Del on a storage gives "Pixel 8's storages can't be deleted". An empty folder the phone takes nothing in shows `󰄜` "Nothing can be added here" / the reason, with no drop hint and no [New folder]. A phone has no trash, so Del asks with "This phone has no trash" ("This camera has no trash" on a camera), the note "Files deleted on Pixel 8 can't be recovered" and the file list, in the drive dialog's layout, and offers only [Delete permanently]. A drive uses the same note, "Files deleted on SANDISK can't be recovered".
      - **New folder** on a phone or camera, or on any gvfs path, is made on a worker: the name probe and the `mkdir` are both round trips over USB. The window stays responsive. The reread then lists the new folder selected. A failure is a `danger` toast, "Couldn't create a folder in DCIM: <reason>".
      - **Slow listings:** folders on a phone are listed off the UI thread, as everywhere, but they are never counted, never thumbnailed and never probed for free space. Their folder sizes stay blank, never "0".
      - **Copies onto a phone** go through the op queue with progress, as any copy does. A new file is written straight to its final name, and is removed again if the copy fails or is cancelled. A Replace never puts the original at risk:
        - The new file is written beside the original under a hidden `.robin-partial-…` name in the same folder. A cancel, failure or unplug partway removes it and leaves the original untouched.
        - Only once it is fully written is it renamed over the original. A same-folder rename on MTP is a name change, not a re-upload.
        - If the phone refuses that rename, the temp file is renamed once more to learn whether the folder renames at all. If it doesn't, the temp file is removed, the original stays, and the item fails with "this folder doesn't allow renaming, so photo.jpg wasn't replaced. The original is unchanged".
        - If the probe rename works, the original is removed and the new file renamed into place. If that last rename fails, the new copy is kept under its hidden name, and the error names it: "…It is saved as .robin-partial-…". Modes and times are not kept, since MTP keeps neither. A move onto or off a phone is a copy and then a delete. A free-space check runs only when gvfs reports a size.
      - **Eject** unmounts through gvfs (`gio mount -u`). A phone never powers off. Busy, unplug and removed toasts and states are as for drives: "Copying 3 items to Pixel 8" with [Wait] [Cancel copy], "Pixel 8 can be unplugged", "Pixel 8 was removed" with `󰄜` in `warning`, and "Pixel 8 was removed without being ejected".
      - **Without gvfs** (no session bus, or no MTP or gPhoto2 monitor running): no phone rows appear, and DEVICES behaves exactly as it does with UDisks2 alone. A phone mounted by something else still browses at `/run/user/<uid>/gvfs/mtp:host=…`, and Robin still applies the phone rules (no trash, no counts, plain copies) to that path.
      - iPhones (AFC) are out of scope.
  - **NETWORK** appears only when gvfs or sshfs mounts exist.
  - **TRASH** has its count in `muted`.
  - The bottom-pinned row "󰄨 Statistics" opens the statistics view.
- Row layout: glyph 18-box/14px `muted` → 8 gap → name `t-body` `text` (ellipsised) → count `t-small` `muted`, right-aligned. Radius `r-md`. Selected rows use `selected`, and the keyboard cursor is the 1px accent ring.
- Glyphs: home `󰋜`, desktop `󰇄`, documents `󰈙`, downloads `󰇚`, pictures `󰉏`, videos `󰕧`, music `󰝚`, folder `󰉋`, drive `󰋊`, USB `󰕓`, eject `󰇪`, network `󰛳`, trash `󰩺` (full: `󰩹`), stats `󰄨`.

### Toolbar (ViewerToolbar.qml, IconButton.qml)
- Height 46, fill `bg`, bottom 1px `divider`.
- Contents: back `󰁍`, forward `󰁔`, up `󰁝` (34×34 icon buttons, 2px gap) → 12 gap → breadcrumb field (flex) → 12 gap → search `󰍉` → view switch segmented [list `󰈚` | grid `󰕰`] → overflow `󰇘`.
- Back and forward step through the folder history: every navigation (a click, Enter, Backspace, up, the sidebar, a path) pushes the folder it left, and a new one clears forward. A search only filters the open folder and adds nothing. A refresh that walks up out of a vanished folder adds nothing either, and a step into a folder that has since vanished fails with the usual toast and drops that entry. Back and forward sit at opacity 0.42 and ignore clicks when there is nowhere to go. Up opens the parent, the same as Backspace, and at `/` it sits at opacity 0.42 and ignores clicks too.
- `Alt+←` / `Alt+→` and the mouse's side buttons (back / forward) run the same history steps as the two buttons, with the same no-op when there is nowhere to go. They do nothing while a dialog, a menu, the viewer or the rename field is open.
- Groups are split by a 1px `text@14%` divider 20px tall (ViewerToolbar separator 9×20).
- **Breadcrumbs:** a field 30px tall with radius `r-md`, fill `surface`, and a 1px `hairline` border. Segments are `t-body` `muted`, the last one is `text` 700, and the separator is `›` in `muted`. Hovering a segment applies `hover` with radius `r-sm`. When segments overflow, the leftmost ones collapse into `…`, which opens a dropdown.
- **Path edit:** `Ctrl+L`, or a click on empty field space, turns the field into a text input with a monospace path, `focus_ring`, and all text selected. Tab-completion shows a dropdown of matching dirs (max 8 rows, 28px). Enter navigates and Esc reverts. An invalid path shakes the field ±4px (3 cycles in 180ms) and turns the ring to `danger`.
- **Search** (`Ctrl+F` or typing in the list) replaces the breadcrumb field in place with a 120ms crossfade. The query is fuzzy, recursive within the current dir, and shows a `t-small` count chip on the right.

### List view
- The header row is 26px, `t-label`, with a bottom `divider`. The sorted column shows `󰁅`/`󰁝` in `accent`.
- Columns: [glyph 14 / thumbnail 20×20 r-sm] Name (flex, min 240) · Size 88, right-aligned · Type 120 · Modified 140 (`2026-10-30 18:58`; within 24h this becomes relative, like `3h ago`) · optional Permissions 96 and Owner 96 (off by default).
- Column padding is 12. Zebra striping is off. Rows use `hover`, `selected`, or `selected_active`, with radius `r-md` and a 4px inset from the content edges.
- Folders show their item count in the Size column as `t-small` `muted` text.
- Hidden files render their name at 60% opacity.
- Symlinks get a `󰌹` overlay at the bottom-right of the glyph, 9px, `muted`. A broken link turns the name `danger` and adds the tooltip "Broken link → target".
- Cut items render at 50% opacity until pasted.

### Grid view (CaptureGrid.qml, CaptureCard.qml)
- Tile sizes come from the density table. `Ctrl+=`/`Ctrl+-` cycle S/M/L. The cell gap is 8 and the outer padding is 16.
- A tile is a 1:1 thumbnail box plus a 2-line name below it (`t-body`, centred, ellipsised in the middle). The tile has radius `r-md` and 6 padding. Hover is `hover` and selection is `selected` covering the whole tile.
- **Media thumbnails** are cover-cropped into the box, with radius `r-sm` and a fill of `text@8%` while loading (ViewerFilmstrip tile bg `shade(fg,0.08)`). They fade in over 120ms.
  - Video thumbnails get a duration badge in the bottom-right: `t-caption`, fill `#000@78%`, radius `r-xs`, padding 3/1 (the same treatment as the ViewerFilmstrip RAW badge). A `󰐊` play glyph 20px in white, 80% opacity, sits in the centre.
  - RAW, HEIC and other badges use the same chip at the top-left.
  - Thumbnails come from the freedesktop cache `~/.cache/thumbnails/{normal,large,x-large}`, so they are shared with other apps.
- **Non-media** files show a glyph centred on a `surface` square with radius `r-icon 4`, at 0.32 of the tile width, coloured by role:

| Kind | Glyph | Colour |
|---|---|---|
| folder | `󰉋` (open `󰝰`) | accent |
| text / md | `󰈙` / `` | text |
| code (rs, py, js, ts, sh, toml, json) | `` `` `` `` `` `` `` | accent |
| pdf | `󰈦` | danger |
| archive (zip, tar, 7z, gz, zst) | `󰗄` | warning |
| audio | `󰎆` | accent |
| font | `` | text |
| office doc / sheet / slides | `󰈬` `󰈛` `󰈧` | text |
| iso / img | `󰗮` | muted |
| executable | `` | warning |
| unknown | `󰈔` | muted |

Only palette roles are used, never per-type hues. The extension shows as a `t-caption` chip at the bottom of the glyph square.

### Status bar
- Height 26, fill `bg`, top `divider`, `t-small` `muted`, horizontal padding 12.
- Left: `142 items · 3 hidden` or `4 selected (1.2 GB)`.
- Right: free space on the current mount (`812 GB free of 1.8 TB`), then `Ctrl+H` hidden toggle state as a `󰈈` glyph that is `accent` when shown.

## 3. States
All empty and error states are centred in the content area: glyph 64 `muted@60%` → 12 → `t-h1` `text` → 6 → `t-small` `muted` (max 360 wide) → 16 → optional buttons.

| State | Glyph | Title / body | Action | Accent |
|---|---|---|---|---|
| Empty folder | `󰉖` | "This folder is empty" / "Drop files here or press Ctrl+N" | [New folder] | — (the whole area is a drop target) |
| Permission denied | `󰌾` | "No permission to open ~/x" / "Owned by root (drwx------)" | [Open as admin…] (spawns `pkexec` only on click) [Go back] | glyph `warning` |
| Unmounted device | `󰕓` | "SANDISK is not mounted" | [Mount] (primary) | — |
| Ejected while open | `󰕓` | "SANDISK was removed" | [Go to Home]; no toast (the pane says it) | glyph `warning` |
| Empty folder on a read-only drive | `󰕓` | "SANDISK is read-only" / "Files can be opened but not changed" | none (no drop hint, no [New folder]) | — |
| Phone waiting for access | `󰄜` | "Pixel 8" / "Unlock your phone and allow file access" | [Retry]; a retry that still can't list gives an info toast "Pixel 8 still hasn't allowed file access" | — |
| Folder a phone takes nothing in (empty) | `󰄜` | "Nothing can be added here" / the reason, e.g. "Files can only go inside a storage on Pixel 8" | none (no drop hint, no [New folder]) | — |
| Phone removed while open | `󰄜` | "Pixel 8 was removed" | [Go to Home] | glyph `warning` |
| Camera waiting | `󰄀` | "Canon EOS R6" / "Turn on Canon EOS R6 and set its USB mode to PTP" | [Retry]; a retry that still can't open it gives an info toast "Canon EOS R6 still isn't ready" | — |
| Loading a large dir | — | Rows stream in. Show 12 skeleton rows of `text@6%` bars that pulse between 4% and 8% opacity over 900ms (no pulse under reduced motion). A 2px indeterminate `accent` bar sits under the toolbar. The status bar reads `Reading… 18 240 items`. Over 300ms, the first batch of 200 renders. | Esc cancels | |
| Search, no results | `󰍉` | "Nothing matches "kotlin"" / "Searched 4 812 items in ~/Projects" | [Search hidden files] [Clear] | |
| Hidden files shown | — | Hidden files have 60% name opacity, and the status bar eye glyph is `accent`. There is no banner. | | |
| Device error / I/O | `󰀦` | "Couldn't read /mnt/x" + errno text | [Retry] | `danger` |

## 4. Operations UI

### Dialogs (shared)
- Width 440, padding 20, radius `r-lg`, fill `surface`, 1px `hairline` border, on a `scrim`.
- Title `t-title`, body `t-body` `muted`.
- Buttons are 32px tall, radius `r-md`, min width 88, right-aligned with a 8px gap.
  - Primary: `accent` fill with `on_accent` text.
  - Secondary: transparent with a 1px `line` border and `text`.
  - Danger: `danger` fill with `on_danger` text.
- Enter triggers the default button and Esc cancels. Dialogs enter with a scale from 0.96 to 1 plus opacity (enter curve) and exit with fade only.

### Unlock dialog (400 wide, encrypted drives)
- Lock glyph `󰌾` in `muted` + title "Unlock SANDISK"; body "SANDISK is encrypted. Enter its passphrase to open it."
- A 36px passphrase field (dots), focused. A wrong passphrase turns its ring `danger` and shows "Wrong passphrase. Try again." in `danger` under it, keeping the dialog open.
- Buttons [Cancel] [Unlock] (primary). Unlock mounts and opens the drive like a click on its row.
- Phase 1 has the mock only (`device-unlock`, `device-unlock-wrong`); encrypted drives aren't listed yet.

### Inline rename (`F2`, or Rename in the file menu)
- The name cell becomes a 1px-ring `focus_ring` input in place, at the same height as the row (list) or over the name under the tile (grid), on a `surface` fill.
- The stem is pre-selected: everything before the last dot, with `.tar.gz`, `.tar.xz`, `.tar.zst`, `.tar.bz2` and `.tar.lz` counted as one extension. Folders, dotfiles (`.bashrc`) and names ending in a dot select the whole name.
- Enter saves and Esc cancels. Clicking away (a click elsewhere in the list, the grid, the sidebar, or the window losing focus) saves a valid, changed name and otherwise cancels, so a half-typed bad name is never kept and never nags.
- The name is checked as you type. Problems show in `danger` in a box right under the field (above it on the last visible rows; wrapped under a grid tile), never in a dialog, and the ring turns `danger`. Enter does nothing while one shows:
  - "A file named x already exists" / "A folder named x already exists"
  - "Enter a name", "\".\" and \"..\" can't be used as names", "Names can't contain \"/\"", "Names can't contain a null character", "Names can be at most 255 bytes long".
- A change of case only (`photo.JPG` → `photo.jpg`) is allowed; the filesystem decides whether that's the same name.
- The rename runs off the UI thread (a phone answers each call over USB). While it runs the field dims to 0.6 and ignores typing and Esc.
- It never overwrites. `renameat2(RENAME_NOREPLACE)` is used, so a file that appears after the check still turns into the inline clash message. Where the flag is unsupported (gvfs FUSE, FAT, old kernels) a file is hard-linked to the new name (which also refuses an existing name) and the old link removed; where links aren't supported either, or for a folder, the name is checked at the last moment before a plain rename. That last case leaves a window of microseconds in which a file created by another program could be replaced.
- On FAT/exFAT a case-only rename finds "the new name" is the same file (same device and inode). It goes in two steps through a hidden `.robin-rename-…` name in the same folder; if the second step fails, the original name is put back. If even that fails, the edit closes and a `danger` toast says "Couldn't finish renaming. It's now this hidden file:" with the hidden name on its pinned line and a [Dismiss] action. It has no 4s/8s timer: it stays until [Dismiss]. A newer toast sits on top of it rather than replacing it; once that toast closes, this one is visible again with the hidden name. Only [Dismiss] removes it, so the file can be found (hidden files are off by default).
- What the location refuses shows under the field too ("Pixel 8 doesn't allow renaming here", "SANDISK is read-only", "You don't have permission to rename it"), and the field stays open on the typed name.
- After the rename the item keeps its selection under the new name and is scrolled into view.
- Batch rename (more than one selected) opens a dialog with find/replace and `{n}` counter, plus a live preview list.

### Properties (file menu)
- A dialog `min(440, window − 32)` wide, in the shared dialog style: the item's glyph and name as the title, then 22px fact rows with a 96-wide `muted` key and a `text` value, and one [Close] (primary, focused). Enter and Esc close it.
- One item: Kind, Size ("6.2 MB (6 204 811 bytes)"; a folder shows its item count), Location (`~`-shortened), Modified (absolute date and time), Permissions (`-rw-r--r--`).
- Several: "N items" as the title, Kind as counts ("3 files, 1 folder"), Size of the files only ("… in files; folders not counted"), Location, the newest Modified, and Permissions or "Mixed".
- No recursive folder size: it would mean walking the tree on open.

### New folder and file
- `Ctrl+Shift+N` inserts a new row at the sort position with the name "New folder" selected inline, the same as rename.
- `Ctrl+Alt+N` opens a 3-option popover (Empty file, Text `.md`, From template in `~/Templates`), followed by inline naming.
- The new row enters with the slide-in, from a 24px offset over 180ms.

### Delete
- `Del` → **neutral** dialog: "Move 3 items to trash?" with a list of up to 5 names in `t-small` and "+12 more". Buttons are [Cancel] [Move to Trash] (primary accent). A "Don't ask again" checkbox is included and is on by default after the first use. On a mount without a usable `.Trash-$uid`, the dialog title becomes "This drive has no trash" with a `warning` glyph, the note "Files deleted on SANDISK can't be recovered" and the file list, and offers only [Delete permanently] in danger style. Robin never silently copies to home.
- `Shift+Del` → **danger** dialog: a 3px `danger` top bar inside the radius, a `󰀦` glyph 20 in `danger`, the title "Permanently delete 3 items?", the body "This can't be undone." Buttons are [Cancel] (default focus) [Delete permanently] (danger). The danger button is disabled for 600ms after the dialog opens.

### Progress strip
- 44px tall, sits above the status bar, fill `surface`, top `divider`. It slides up 44px with the spring.
- Row layout: glyph (copy `󰆏` / move `󰪹` / trash `󰩺`) → `t-body` "Copying 1 204 files to SANDISK" → a 4px pill bar (track `text@12%`, fill `accent`, flex, min 200) → `t-small` `muted` "2.1 / 6.4 GB · 84 MB/s · 52s left" → pause `󰏤` → cancel `󰅖` (hover `danger@18%`).
- Multiple jobs stack as additional 36px rows, up to 3, followed by "+2 more" that expands.
- On finish, the bar turns full `accent` and holds for 1s, then the strip collapses.
- On error, the bar becomes `danger` and shows [Retry][Skip].

### Conflict dialog (480 wide)
- Title "SANDISK already has "IMG_0412.jpg"".
- Two compare cards side by side: a 64px thumbnail or glyph plus size, date, and type in `t-small`. The newer one gets a `accent` "Newer" chip and the larger one gets a "Larger" chip.
- Buttons: [Skip] [Keep both] (auto-rename to `IMG_0412 (2).jpg`, previewed in muted below) [Replace] (primary, but `warning` fill when the target is newer).
- Checkbox "Apply to all 14 conflicts" sits in the bottom-left.
- Folder-vs-folder conflicts offer [Merge] instead of Replace.
- A folder's size line shows its item count ("48 items"), as the list view does.
- The muted line under the cards also says that a replaced file isn't restored on cancel. Cancel takes back only what the job added; a file a Replace or Merge overwrote keeps the new contents.
- An error that stops a merge partway shows a `warning` toast, "Merged 31 of 48 items into raw-exports before an error". Details lists the error, each item that wasn't merged, and anything the cleanup couldn't remove.

### Toasts
- Bottom-centre, 16 above the status bar, or 16 above the progress strip while it shows, so no toast ever covers a job's progress or its Cancel. This holds for the sticky stack, which sits 16 above the status bar or the progress strip; a timed toast sits 8px above that stack. Width auto, from 280 to 520. Height 40, radius `r-lg`, fill `panel_float`, 1px `hairline`.
- Content: glyph 16 → `t-body` → optional action link in `accent` ("Undo"), then a close `󰅖`.
- They last 4s, or 8s with an action. Hover pauses the timer. A sticky toast (one with no timer, such as the stranded-rename toast) has no 4s/8s timer.
- A sticky toast is not replaced by a newer one. The stack keeps every sticky toast, newest on top. Sticky toasts stack upward from 16px above the status bar or the copy progress strip, with 8px gaps. A timed toast always sits 8px above the whole sticky stack. At most 3 sticky toasts are shown in full. Older ones collapse into a single line in `muted` under the stack that reads "+N more" (for example "+2 more"); clicking that line expands the full list, and a `muted` "Show fewer" line in the same place collapses it again. When the window is under 600px tall, only 1 sticky toast is shown in full and the rest go into the "+N more" line. The expanded stack, including the timed toast, is capped 16px below the toolbar (window height minus the toolbar minus 16px, less the 16px above the status bar or progress strip); the parked list scrolls inside that cap, and the timed toast stays visible above it. Resizing the window re-applies the cap. Each toast keeps its own [Dismiss], and each hidden filename stays in `text` (the same white as when the toast is on its own). When the current toast closes, the newest parked sticky toast comes back. Only [Dismiss] (or that toast's own action) removes its own toast. Ordinary timed toasts still replace only the current timed toast, and leave every parked sticky toast in place. New toasts slide in 24px up over 180ms, and dismissal uses the 120ms exit.
- Variants change only the glyph colour: info `accent`, warning `warning`, error `danger`.
- "Moved 3 items to trash · Undo" is the canonical toast.
- A toast can carry two actions ("Wait · Cancel copy", "Retry · Force"); a destructive second action is drawn in `danger`.

## 5. Media viewer (Viewer.qml, ViewerToolbar, ViewerFilmstrip, ViewerTransport, ViewerInfoPanel, ViewerKeys, ViewerShortcutHelp)

- **Full-bleed.** The viewer takes over the window. The canvas is `#000` blended 70% toward `bg` (i.e. bg at 30% over black). The image is fit-to-window with a 0 margin.
- **Floating chrome** (all `panel_float`, 1px `hairline`, radius `r-lg`, with 16 from the edges):
  - Top bar: 40 tall. Back `󰁍`, filename `t-title`, a counter `12 / 248` in `t-small` `muted` (Viewer counterText), and on the right info `󰋽`, slideshow `󰐊`, overflow.
  - Bottom toolbar: 46 tall, centred. Prev, next, zoom −/fit/1:1/+, rotate, favourite, delete, and open-with.
  - Filmstrip: tiles 72×48, gap 4, padding 6, radius 3. Non-current tiles are at 62% opacity and the current tile gets a 2px `accent` ring. It sits 16 above the toolbar and auto-centres on the current item (ViewerFilmstrip `preferredHighlight`). `F` toggles it.
  - **Auto-hide:** chrome fades out after 2500ms of no pointer movement (220ms out-quad, Viewer.qml line 1567) and returns on any move or key. The cursor hides with it.
- **Zoom and pan:**
  - Wheel or `+`/`-` zoom by ×1.25 around the cursor, between fit and 16×. `0` fits and `1` shows 100%.
  - Double-click toggles fit and 100% at the point clicked. Each zoom step animates with the spring.
  - Drag pans with inertia (the spring). The pan is clamped so an image edge can't move past the centre.
  - A zoom HUD chip "200%" (`t-small`, `panel_float`, radius r-xs) appears top-right for 800ms.
  - At ≥200% a 120×80 minimap appears bottom-right with the viewport rectangle drawn as a 1px `accent` stroke.
- **Video:** libmpv renders into a Slint texture. The transport (ViewerTransport) replaces the toolbar:
  - Height 46, panel width = window − 120.
  - Contents: play `󰐊`/`󰏤`, time `t-small` `01:12 / 04:30`, a scrub track (3px, or 5px when lifted on hover over 100ms, track `text@20%`, played `accent`, 12px knob in `text` with a 2px `accent` ring), a hover time tooltip, volume `󰕾` with a slider popover, speed chip `1.0×`, loop, subtitles `󰨖`, and fullscreen.
  - Space toggles play. Resume prompt: if the saved position is between 5s and duration − 3s, show a toast "Resume at 02:14?" (Viewer.qml line 1305 logic).
- **Metadata panel** (`I`, ViewerInfoPanel): 300 wide, docked right with 16 inset, radius `r-lg`, fill `bg@92%`, padding 16.
  - Filename `t-title`, then path `t-small` `muted`.
  - Sections in `t-label`: FILE (size, type, dimensions, modified), CAMERA (make/model, lens, ISO, f, shutter, focal), LOCATION (coordinates and a [Copy] link), VIDEO (codec, fps, bitrate, audio).
  - Key/value rows are 20 tall with key `muted` and value `text`, aligned right.
  - It slides in from 24px over 180ms.
- **Slideshow** (`S`): chrome hides, and each slide crossfades over 400ms (a longer, slideshow-only crossfade; 120ms under reduced motion is replaced by a cut). The interval is 5s and can be set from 2 to 30s. A 2px `accent` progress hairline runs along the bottom edge. Space pauses.
- **Keyboard map** (ViewerKeys.qml):
  - `←/→` or `h/l`: prev/next. `Home/End`: first/last.
  - `+ - 0 1`: zoom. `r` / `R`: rotate cw/ccw. `f`: filmstrip. `i`: info. `s`: slideshow.
  - Space: play/pause, or next in images. `,`/`.`: frame step. `j/k`: seek ∓5s. `m`: mute.
  - `Del` / `Shift+Del`: same dialogs as section 4. `Ctrl+C`: copy image. `Enter` / `F11`: fullscreen. `?`: help. `Esc`/`q`: back to the browser.
- **Browser↔viewer transition:** opening runs a shared-element zoom. The thumbnail rect animates to the fitted image rect with the spring, the canvas fades in over 180ms (enter curve), and the chrome fades in 60ms later. Closing reverses this to the current item's tile (scrolling it into view first) with the 120ms exit, and the selection lands on the last viewed item. Under reduced motion it is a 120ms crossfade.

## 6. Statistics view (sidebar "Statistics", or `Ctrl+I`)
- Single scroll page, padding 24, max content width 1120.
- Cards use `surface` fill, 1px `hairline`, radius `r-lg`, padding 16, and a `t-label` header with `muted` glyph (den-den-mushi dashboard card style). Card gap is 16.

1. **DISKS:** one row per mount, 56 tall. Drive glyph → name and mountpoint → a 10px pill bar → `t-display` percent on the right and `t-small` "812 GB free of 1.8 TB". The bar fill is `accent`, or `warning` at ≥85%, or `danger` at ≥95%. Removable devices get a 22×22 eject button right of the percent, which ejects as the sidebar does.
2. **LARGEST FOLDERS — treemap** (squarified), 2/3 width × 320.
   - **Why a treemap over a sunburst:** rectangles let names and sizes sit inside the cells as plain text. Area is easier to compare than arc angle. It fills the card's rectangle with no wasted corners, and it is simpler to hit-test and lay out in Slint (no arc text).
   - Cell colouring: depth-1 cells are `accent@(70→25)%` by rank, and their children nest at −10% alpha.
   - Cells have 2px `bg` gutters and radius `r-sm`. Each label is `t-small` in `on_accent` when the fill alpha is ≥50% and `text` otherwise, shown only if the cell is ≥64×28.
   - Click drills in, and a breadcrumb above the treemap steps back out. Hover shows a tooltip with the full path and size.
   - A right-side list 1/3 wide shows the top 10 with a size bar.
   - Scanning happens incrementally in the background with a "Scanning… 41%" line, and the cells grow with the spring.
3. **FILE TYPES:** a horizontal stacked bar 16px tall, with categories (Images, Video, Audio, Docs, Code, Archives, Other) as `accent` at 100/80/60/45/32/20/10% alpha, then a table below it: swatch 10×10 radius r-xs · category · count · size · % (sort toggles count/size).
4. **CACHE:** thumbnails (`~/.cache/thumbnails`, shared), Robin cache, and mpv watch-later, each with its size. [Clear thumbnail cache] is a secondary button that opens a neutral confirm, and then a toast reports "Freed 1.4 GB".
5. **TRASH:** item count and size, the oldest item date, and a per-device breakdown. [Open trash] is secondary. [Empty trash] is the danger button, and it opens the danger dialog from section 4 that names the size: "Permanently delete 2 318 items (4.7 GB)?".
- Charts use only palette roles and alpha, with no gradients. Under reduced motion the bars render at their final size.

## 7. Menus, overlay, settings
- **Context menu:** min width 220, radius `r-lg`, fill `surface`, 1px `hairline`, padding 4.
  - Items are 28 tall (24 at compact) with radius `r-md` and hover `selected`. Each item has a glyph 14 `muted`, a label `t-body`, and a shortcut `t-small` `muted` aligned right. Separators are a 1px `divider` with 4px margins. Submenus are marked `󰅂`.
  - Destructive items ("Delete permanently") have a `danger` label and glyph, and are always last.
  - Opening scales from 0.98 to 1 plus opacity over 120ms from the pointer. Closing is a 80ms fade.
  - File menu (right-click on an item): Open · Open with… · ─ · Cut · Copy · Paste · Rename `F2` · ─ · Move to Trash `Del` (or **Delete permanently** in `danger` where there is no trash, through the no-trash dialog) · ─ · Copy path · Properties.
    - Right-clicking an unselected item selects it first; on a selected one the menu acts on the whole selection.
    - Open opens the right-clicked item (a folder in Robin, media in the viewer, anything else with its default app). Open with… hands one file to the desktop's opener. Paste pastes into the open folder. Copy path puts the absolute paths, one per line, on the clipboard through a helper, off the UI thread: `wl-copy` on Wayland, else `xclip -selection clipboard`, then `xsel --clipboard --input` wherever `DISPLAY` is set. Only once a helper has taken the text does the toast say "Copied 1 path" / "Copied 3 paths". With no helper installed it is a `warning` toast, "Couldn't copy: install wl-clipboard or xclip"; a helper that fails gives a `danger` "Couldn't copy the path: …".
    - Lines the location or selection can't do stay in place at opacity 0.42, ignore clicks and hover, and show their reason in a tooltip (`surface` box, 1px `hairline`, `t-small`) to the right of the line on hover, flipped left near the window edge: "Select one item to rename", "Select one item to open with another app", "Folders open in Robin", "Nothing to paste: cut or copy something first", "Restore it from the Trash first", "SANDISK is read-only", "Pixel 8 doesn't allow renaming in DCIM", "You don't have permission to change …".
    - The menu opens at the pointer and flips left and/or up when it would cross the window's right or bottom edge; when the window is shorter than the menu it is clamped to 8px margins and scrolls.
  - Background menu: New folder · New file › · Paste · Select all · View › · Sort by › · Show hidden · Open terminal here · Properties.
- **Shortcut overlay (`?`, ViewerShortcutHelp.qml):** a centred 760×520 panel, `panel_float`, radius r-lg, on a scrim. It has 3 columns (NAVIGATE / FILES / VIEW, with the viewer's keys when opened from the viewer) under `t-label` headers. Each row shows key caps (`t-small`, a 1px `line` border, radius r-xs, padding 4/1, min width 20) then the description. A filter input sits at the top, and Esc closes it. NAVIGATE lists `Alt ← →` back / forward and `M4 M5` mouse back / forward; FILES lists `F2` rename.
- **Settings** (`Ctrl+,`): a full page with a left category list 200 wide using sidebar row style, and a form column max 640.
  - Rows are 44 tall: label `t-body` with a `t-small` `muted` hint, and the control on the right.
  - Toggles are 32×18 pills (on: `accent` track with an `on_accent` knob; off: a `text@20%` track with a `text` knob). The knob moves with the spring.
  - Categories:
    - General: default view, show hidden, sort, folders first, single-click open.
    - Viewer: chrome timeout, slideshow interval, filmstrip default, video resume, loop short videos under 15s.
    - Motion: **Reduce motion** (follows `gtk-enable-animations=false` by default; when on, every enter, exit, and spring becomes a 120ms crossfade or a cut, and the skeleton pulse is off).
    - Files: confirm trash, trash on external drives policy, thumbnail sizes, and the max thumbnail file size (default 200 MB).
    - Default apps: per-MIME handoff for anything Robin doesn't view itself, such as PDF → zathura.
    - Keyboard: read-only list.
    - About.
  - Theme is not configurable (it follows R2-D2), and the page says so with a `muted` note.

## Motion (shared with den-den-mushi)
| Name | Spec | Used for |
|---|---|---|
| enter | 180ms `cubic-bezier(0.2,0.8,0.2,1)`, 24px offset | toasts, panels, dialogs, new rows |
| exit | 120ms ease-in `cubic-bezier(0.4,0,1,1)` | dismissals |
| spring | stiffness 400, damping 40, mass 1 (critically damped, settles in about 250ms) | sidebar collapse, zoom and pan, restack, viewer shared-element, progress strip |
| crossfade | 120ms linear | replaces: view switch, search field, theme reload |
| hover / press | 120ms out-quad colour (IconButton) | all controls |
Things that never animate: scrolling content, list sorting (it cuts), and typing.
**Reduced motion:** the spring and enter/exit become a 120ms crossfade, slides become fades, and the skeleton pulse, zoom HUD fade, and slideshow crossfade are cuts.

## 8. `--render` checklist
`robin --render <view> --fixture tests/fixtures/tree --size 1280x800 --density {comfortable,compact} --out /workspace/robin-shots/<n>.png`. Use the cyan palette, plus one pass in `black` to check monochrome. Each frame must be deterministic: fixed date 2026-10-30 18:58, and animations snapped to their end state.
1. browser-list (Pictures, 1 selected, 1 hovered)
2. browser-list-compact
3. browser-grid-M with mixed media and non-media glyphs
4. browser-grid-S and grid-L
5. sidebar-collapsed rail
6. breadcrumb-overflow and path-edit with completion dropdown, plus invalid path
7. search-results and search-no-results
8. empty-folder
9. permission-denied
10. device-unmounted (muted row, no bar) and device-ejected (row gone)
10a. device-plugged (arrival toast), device-read-only, device-menu, device-busy-copy, device-busy-other, device-safe-unplug, device-unlock and device-unlock-wrong, phone-waiting, device-read-only-empty, device-removed-elsewhere (toast only when another folder is open), device-power-off (tooltip), device-partition-unplug (one toast names both partitions, both rows gone), device-partitions-removed-elsewhere (one merged removed toast)
10b. Phones, as mocks (`--render-mock`): phone-row (phone and camera glyphs), phone-plugged, phone-waiting, phone-still-waiting, phone-top-refused, phone-no-write, phone-no-trash, phone-busy-copy, phone-safe-unplug, camera-waiting. As real scenes against the fake backend (`--render`): phones-row, phones-plugged, phones-waiting, phones-still-waiting, phones-allowed, phones-storage, phones-loading, phones-top-refused, phones-no-write, phones-no-trash, phones-copying, phones-copied (no partial file left), phones-busy-copy, phones-safe-unplug (unmount only, never a power-off), phones-removed, phones-removed-elsewhere, phones-menu, phones-camera-waiting, phones-camera-still-waiting, phones-new-folder (made off the UI thread, then selected). phones-storage also checks the status bar reads "12 GB free of 128 GB", and phones-no-trash checks the note
11. loading-large-dir (skeleton)
12. hidden-files-shown
13. rename-inline, rename-collision, rename-invalid and rename-grid (mocks); rename-editing, rename-clash, rename-invalid, rename-grid, rename-done (selected and in view after), phones-rename-refused-reached (the refusal inline) as real scenes
14. new-folder inline and new-file popover
15. confirm-trash (neutral) and confirm-trash-no-trash-on-drive
16. confirm-delete-permanent (danger)
17. progress-strip with 1 job, 3 jobs, and error
18. conflict-dialog (file vs file and folder merge)
19. toasts with 3 stacked
20. drop-target valid and invalid
21. context-menu file and background: menu-file, menu-file-edge (flipped up and left in the bottom-right corner), menu-file-greyed (two selected, Rename's tooltip), menu-file-phone, menu-bg (mocks); context-menu, context-menu-edge, context-menu-multi, phones-rename-refused (Rename greyed with the phone's reason), devices-menu-read-only (real scenes)
21a. properties (mock and real) and properties-multi (real)
21b. copy-path-done (2 selected), copy-path-no-tool, rename-stranded (mocks); copy-path (wl-copy), copy-path-xclip (X11), copy-path-no-tool (real scenes against a stand-in clipboard); rename-stranded and rename-stranded-dismissed (real: the toast stays, [Dismiss] closes it)
21c. toast-stack and toast-stack-restored (mock and real): a newer "Copied 1 path" toast sits 8px above the sticky hidden-file toast, which stays fully visible with its filename in `text` (the same white as when it is on its own) and [Dismiss]; when the newer toast closes, the sticky one is the current toast again with the same filename. toast-stack-two (mock and real): two parked hidden-file toasts plus a newer "Copied 1 path", stacked 8px apart, newest nearest the top, each filename white and fully readable. toast-stack-more (mock and real): five parked sticky toasts with a newer timed toast; three sticky toasts are shown in full and a muted "+2 more" line sits under the stack. toast-stack-expanded (mock and real): eight parked sticky toasts with a newer timed toast; the stack is capped 16px below the toolbar, the list scrolls under the timed toast, and a muted "Show fewer" line sits under the stack. toast-stack-short (mock and real): five parked sticky toasts in a window under 600px tall; one sticky toast is shown in full and a muted "+4 more" line sits under the stack
22. shortcut-overlay for browser and viewer
23. settings for each category
24. viewer-image with chrome, viewer-image with chrome hidden, and viewer-zoom-400 with minimap
25. viewer-video transport, scrub-hover, and resume toast
26. viewer-info-panel
27. viewer-slideshow
28. stats-overview, stats-treemap-drilled, and stats-empty-trash-confirm
29. reduce-motion has no visual delta and only needs a sanity check of one frame
30. theme-black for frames 1, 3, 24, and 28

Mockups: `/workspace/robin-design/mockups/{01-list,02-grid,03-viewer,04-stats,05-dialogs}.png`. They are a direction only, and this spec wins over them.

## External drag and drop (Slint 1.17.1)

Checked against the 1.17.1 sources rather than assumed.

- `DragArea`/`DropArea` are in-window only. `DataTransfer` holds
  `plain_text`, `image` and `user_data`; there is no MIME/uri-list slot.
- `i-slint-backend-winit` 1.17.1 neither starts native drags nor
  forwards `WindowEvent::DroppedFile` into Slint's drop machinery.
- So outbound drags to other apps are impossible without patching
  Slint; we do not fake them (e.g. by copying paths to the clipboard).
- Inbound: the `unstable-winit-030` feature exposes
  `WinitWindowAccessor::on_winit_window_event`, and winit 0.30 emits
  `DroppedFile(path)` on X11 (once per path). `app::run` collects those
  together with the folder open at that moment (the destination is
  frozen at drop time, not at flush time). The next 100ms tick turns
  them into jobs via `ops::external_drop`, unless a dialog is open, in
  which case they wait; jobs queue behind a running one. Jobs are
  grouped per source folder, items already in the (canonicalized)
  destination and into-itself drops are skipped.
- Inbound drops always **copy**, never move. winit 0.30.13's X11
  handler (`platform_impl/linux/x11/dnd.rs`) canonicalizes every path
  (:166), so a dropped symlink arrives as its target and a move would
  take the target away from where it really lives. It also
  percent-decodes the whole `text/uri-list` before splitting on CRLF
  (:149-150), so names containing an encoded CR/LF split wrongly; strips
  `file://` with a `replace` over the whole string, so a name containing
  `file://` is mangled; and drops the entire drop if any name isn't
  UTF-8. Slint doesn't expose the raw uri-list, so none of this can be
  undone. Copying makes the worst case a duplicate rather than a lost
  or relocated file. A dropped symlink copies its target: winit resolved
  it, and a path it can't resolve fails the whole drop inside winit, so
  Robin never sees a link. The notices for a drop that copied nothing
  and for an unwatchable folder use the neutral toast.
- Subfolder item counts open each child without following links and
  skip a child on another device (a mount point), leaving it blank.
- The drop API depends on Slint's `unstable-winit-030` feature, which
  has no semver guarantee; that is why `slint` is pinned to `=1.17.1`.
  Any Slint bump must recheck `on_winit_window_event` and `DroppedFile`. `DroppedFile` has no position, no
  modifiers and no hover events, so the target is always the open
  folder. winit 0.30 has no Wayland implementation, so nothing arrives
  there.
- Upstream status (checked 2026-10-31): winit 0.31 replaces
  `DroppedFile` with `DragEntered`/`DragMoved`/`DragDropped`/`DragLeft`
  carrying all paths plus the pointer position, and implements them on
  Wayland ([winit#4571](https://github.com/rust-windowing/winit/pull/4571),
  merged 2026-07-16, shipped in 0.31 betas). Slint 1.17's release notes
  say cross-app drag and drop is "in development upstream in winit", and
  Slint's docs still expose only `winit_030`. Once Slint moves to winit
  0.31, Robin can switch to `DragDropped` and target the row under the
  pointer.
- XWayland: winit 0.30 chooses X11 when `WAYLAND_DISPLAY` is empty and
  `DISPLAY` is set, so `WAYLAND_DISPLAY= robin` is a documented opt-in,
  not the default.

## Live refresh

- `watch::Watch` is a non-recursive inotify watch on the open folder.
  Its callback only flips two `AtomicBool`s: `dirty` (the listing
  changed) and `stale` (the folder was deleted, moved or replaced, i.e.
  IN_IGNORED / delete-self / move-self, or the watcher errored). No
  channel, so a burst of events can't grow memory.
- A 100ms UI timer reads the flags. `Debounce` waits for 200ms of quiet
  but fires at most 1.5s after the first event, so a folder that never
  goes quiet still updates.
- A stale watch is dropped and re-armed on the next tick, followed by a
  reread.
- The reread (`Snapshot::read`: list, stat, subfolder counts) runs on a
  worker thread, one at a time; the UI thread only sorts and builds
  rows, and ignores a result for a folder it has left. An unchanged
  result doesn't redraw.
- If a watch can't be set up, the folder is reread every 2s instead,
  setup is retried with backoff (2s doubling to 60s), and a muted toast
  says so once per folder.
- Only ENOENT/ENOTDIR on reread falls back to the nearest existing
  ancestor; other errors (e.g. EACCES) keep the folder and its last
  listing and show the error once.
- Measured on a 100k-file folder (release build, ext4, warm cache): worker
  read+stat ~116ms, UI sort/apply ~18ms, UI row build ~242ms.
