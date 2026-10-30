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
| | Compact | Comfortable (default) |
|---|---|---|
| list row | 24 | 30 |
| sidebar row | 26 | 30 |
| toolbar | 40 | 46 (ViewerToolbar implicitHeight 46) |
| icon button | 28×28 | 34×34 (IconButton 34) |
| grid tile (S/M/L) | 96/128/176 | 112/152/208 |
| status bar | 22 | 26 |

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

### Places sidebar (FolderSidebar.qml)
- Width 232. Drag the 6px handle on the right edge (FolderSidebar width 6) to resize it between 180 and 360. `Ctrl+B` collapses it to a 52px glyph rail, and below a 760px window width it auto-collapses (Main.qml `width >= 760`). Collapsing and expanding use the spring.
- Fill `surface`. Right edge is a 1px `divider` line.
- Padding is 8. Each section has a header row 24px tall in `t-label` style.
- Sections, in order:
  - **PINNED**: Home, Desktop, Documents, Downloads, Pictures, Videos, plus user pins added by drag-in or the context menu "Pin".
  - **DEVICES**: mounts from `/proc/self/mountinfo` with real filesystems. Each row has a 3px usage bar beneath it: `accent`, or `warning` at ≥85%, or `danger` at ≥95%. Removable devices show an eject glyph `󰕓` button 22×22 on hover.
  - **NETWORK** appears only when gvfs or sshfs mounts exist.
  - **TRASH** has its count in `muted`.
  - The bottom-pinned row "󰄨 Statistics" opens the statistics view.
- Row layout: glyph 18-box/14px `muted` → 8 gap → name `t-body` `text` (ellipsised) → count `t-small` `muted`, right-aligned. Radius `r-md`. Selected rows use `selected`, and the keyboard cursor is the 1px accent ring.
- Glyphs: home `󰋜`, desktop `󰇄`, documents `󰈙`, downloads `󰇚`, pictures `󰉏`, videos `󰕧`, music `󰝚`, folder `󰉋`, drive `󰋊`, USB `󰕓`, network `󰛳`, trash `󰩺` (full: `󰩹`), stats `󰄨`.

### Toolbar (ViewerToolbar.qml, IconButton.qml)
- Height 46, fill `bg`, bottom 1px `divider`.
- Contents: back `󰁍`, forward `󰁔`, up `󰁝` (34×34 icon buttons, 2px gap) → 12 gap → breadcrumb field (flex) → 12 gap → search `󰍉` → view switch segmented [list `󰈚` | grid `󰕰`] → overflow `󰇘`.
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
- Right: free space on the current mount (`812 GB free of 1.8 TB`), then `Ctrl+H` hidden toggle state as a `󰈈` glyph that is `accent` when shown, then the density toggle.

## 3. States
All empty and error states are centred in the content area: glyph 64 `muted@60%` → 12 → `t-h1` `text` → 6 → `t-small` `muted` (max 360 wide) → 16 → optional buttons.

| State | Glyph | Title / body | Action | Accent |
|---|---|---|---|---|
| Empty folder | `󰉖` | "This folder is empty" / "Drop files here or press Ctrl+N" | [New folder] | — (the whole area is a drop target) |
| Permission denied | `󰌾` | "No permission to open ~/x" / "Owned by root (drwx------)" | [Open as admin…] (spawns `pkexec` only on click) [Go back] | glyph `warning` |
| Unmounted device | `󰕓` | "SANDISK is not mounted" | [Mount] (primary) | — |
| Ejected while open | `󰕓` | "SANDISK was removed" | [Go to Home]; a toast appears too | glyph `warning` |
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

### Inline rename (`F2`)
- The name cell becomes a 1px-ring `focus_ring` input in place, at the same height as the row.
- The stem is pre-selected (extension excluded).
- Enter commits, and Esc or a blur commits only if the name is valid.
- When a name collides, the ring turns `danger` and a 22px tooltip appears below: "A file named x already exists". Invalid characters (`/`, NUL) get the same treatment.
- Batch rename (more than one selected) opens a dialog with find/replace and `{n}` counter, plus a live preview list.

### New folder and file
- `Ctrl+Shift+N` inserts a new row at the sort position with the name "New folder" selected inline, the same as rename.
- `Ctrl+Alt+N` opens a 3-option popover (Empty file, Text `.md`, From template in `~/Templates`), followed by inline naming.
- The new row enters with the slide-in, from a 24px offset over 180ms.

### Delete
- `Del` → **neutral** dialog: "Move 3 items to trash?" with a list of up to 5 names in `t-small` and "+12 more". Buttons are [Cancel] [Move to Trash] (primary accent). A "Don't ask again" checkbox is included and is on by default after the first use. On a mount without a usable `.Trash-$uid`, the dialog title becomes "This drive has no trash" with a `warning` glyph and offers [Delete permanently] in danger style. Robin never silently copies to home.
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
- Bottom-centre, 16 above the status bar (or the strip). Width auto, from 280 to 520. Height 40, radius `r-lg`, fill `panel_float`, 1px `hairline`.
- Content: glyph 16 → `t-body` → optional action link in `accent` ("Undo"), then a close `󰅖`.
- They last 4s, or 8s with an action. Hover pauses the timer.
- Up to 3 stack with an 8px gap. New toasts slide in 24px up over 180ms, and dismissal uses the 120ms exit.
- Variants change only the glyph colour: info `accent`, warning `warning`, error `danger`.
- "Moved 3 items to trash · Undo" is the canonical toast.

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

1. **DISKS:** one row per mount, 56 tall. Drive glyph → name and mountpoint → a 10px pill bar → `t-display` percent on the right and `t-small` "812 GB free of 1.8 TB". The bar fill is `accent`, or `warning` at ≥85%, or `danger` at ≥95%. Removable devices get an eject button.
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
  - File menu: Open · Open with › · Open in viewer · ─ · Cut · Copy · Copy path · Paste · ─ · Rename · Duplicate · Compress · Pin to sidebar · ─ · Properties · ─ · Move to trash · Delete permanently.
  - Background menu: New folder · New file › · Paste · Select all · View › · Sort by › · Show hidden · Open terminal here · Properties.
- **Shortcut overlay (`?`, ViewerShortcutHelp.qml):** a centred 760×520 panel, `panel_float`, radius r-lg, on a scrim. It has 3 columns (NAVIGATE / FILES / VIEW, with the viewer's keys when opened from the viewer) under `t-label` headers. Each row shows key caps (`t-small`, a 1px `line` border, radius r-xs, padding 4/1, min width 20) then the description. A filter input sits at the top, and Esc closes it.
- **Settings** (`Ctrl+,`): a full page with a left category list 200 wide using sidebar row style, and a form column max 640.
  - Rows are 44 tall: label `t-body` with a `t-small` `muted` hint, and the control on the right.
  - Toggles are 32×18 pills (on: `accent` track with an `on_accent` knob; off: a `text@20%` track with a `text` knob). The knob moves with the spring.
  - Categories:
    - General: default view, density, show hidden, sort, folders first, single-click open.
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
10. device-unmounted and device-ejected
11. loading-large-dir (skeleton)
12. hidden-files-shown
13. rename-inline and rename-collision
14. new-folder inline and new-file popover
15. confirm-trash (neutral) and confirm-trash-no-trash-on-drive
16. confirm-delete-permanent (danger)
17. progress-strip with 1 job, 3 jobs, and error
18. conflict-dialog (file vs file and folder merge)
19. toasts with 3 stacked
20. drop-target valid and invalid
21. context-menu file and background
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
  or relocated file. Paths that are still symlinks under a no-follow
  stat are refused with a warning.
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
