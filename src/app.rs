//! The interactive window: listing, keyboard, drag and drop, and file jobs.
//!
//! [`Controller`] holds everything that decides what a key or a drop does,
//! so it is tested without a window. [`run`] binds it to Slint and runs jobs
//! on a worker thread that reports back through the event loop.

use std::cell::RefCell;
use std::collections::hash_map::DefaultHasher;
use std::collections::VecDeque;
use std::ffi::OsString;
use std::hash::{Hash, Hasher};
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::{mpsc, Arc};
use std::time::Duration;

use slint::{ComponentHandle, Model, ModelRc, SharedString, VecModel};

use crate::fsops::{
    self, Cancel, Conflict, ConflictChoice, ConflictKind, ConflictPrompt, KeptEntry, TrashChoice,
};
use crate::model::{Listing, Nav, SortDir, SortKey};
use crate::ops::{self, Clipboard, Env, Job, JobKind, Mods, Op, Outcome, Progress, Trashed};
use crate::safety::DirFd;
use crate::{AppWindow, Columns, Error, GridLine, Mark, MarkRun, Result, Row};

/// What the trash/delete dialog is asking about.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Dialog {
    Trash(Vec<OsString>),
    /// The drive has no trash: Delete permanently is the only way.
    TrashNo(Vec<OsString>),
    Delete(Vec<OsString>),
    /// Empty Trash: everything in the Trash view, deleted for good.
    EmptyTrash(Vec<OsString>),
    /// Clear thumbnail cache: Robin's own thumbnails under this cache root.
    ClearThumbs(PathBuf),
    /// A move left `names` where they were. Button 1 is Copy instead when
    /// any of them can be copied; it copies `copy`, the ones that can.
    Refused {
        job: Job,
        names: Vec<OsString>,
        copy: Vec<OsString>,
        can_copy: bool,
    },
    /// A paste, drop or move found the name taken. Buttons: 0 Cancel,
    /// 1 Skip, 2 Keep both, 3 Replace, 4 the apply-to-all toggle.
    Conflict {
        ask: ConflictAsk,
        apply_all: bool,
    },
}

/// One name clash the job thread is waiting on.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ConflictAsk {
    pub prompt: ConflictPrompt,
    /// The destination folder's name for the title.
    pub dest: String,
    /// Items left in the job, including this one; the toggle offers to
    /// answer them all.
    pub left: usize,
}

impl Dialog {
    pub fn overlay(&self) -> &'static str {
        match self {
            Dialog::Trash(_) => "trash",
            Dialog::TrashNo(_) => "trash-no",
            Dialog::Delete(_) | Dialog::EmptyTrash(_) | Dialog::ClearThumbs(_) => "delete",
            Dialog::Refused { .. } => "move-refused",
            Dialog::Conflict { .. } => "conflict-live",
        }
    }

    fn buttons(&self) -> i32 {
        match self {
            Dialog::Refused {
                can_copy: false, ..
            } => 1,
            Dialog::Conflict { .. } => 5,
            // Cancel, Move to Trash, and the "Don't ask again" box.
            Dialog::Trash(_) => 3,
            _ => 2,
        }
    }

    /// The button Enter presses when the dialog opens. Delete starts on
    /// Cancel so a held Enter can't destroy anything.
    fn default_focus(&self) -> i32 {
        match self {
            Dialog::Delete(_)
            | Dialog::EmptyTrash(_)
            | Dialog::ClearThumbs(_)
            | Dialog::TrashNo(_) => 0,
            Dialog::Trash(_) => 1,
            Dialog::Refused { can_copy, .. } => i32::from(*can_copy),
            Dialog::Conflict { .. } => 1,
        }
    }
}

/// What the window should do after the controller handled an input.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Action {
    None,
    /// Repaint rows and overlays.
    Redraw,
    Run(Job),
    Open(Dialog),
    Navigate(PathBuf),
    CancelJob,
    /// Reply to the job thread's conflict question; `None` cancels the job.
    Answer(Option<ConflictChoice>),
    /// Start a program detached, e.g. xdg-open on a video.
    Launch(Vec<OsString>),
    /// Delete Robin's thumbnails under this cache root, then rescan stats.
    ClearThumbs(PathBuf),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Drag {
    pub names: Vec<OsString>,
    pub target: Option<usize>,
    pub op: Option<Op>,
    /// A folder under the pointer that is one of the dragged items or
    /// inside one: shown muted, never dropped on.
    pub refused: Option<usize>,
}

/// One read of a folder: its entries and, once the second pass finishes,
/// each subfolder's item count.
/// Built off the UI thread for live refresh.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Snapshot {
    pub entries: Vec<crate::model::Entry>,
    pub counts: std::collections::HashMap<OsString, usize>,
    /// False until the second pass has filled `counts`. A pending map
    /// must not replace numbers already shown, so a reread doesn't flash
    /// blank (or "0") before the new counts arrive. Once ready, a missing
    /// key is a folder left uncounted (another device, or unreadable).
    pub counts_ready: bool,
}

impl Snapshot {
    /// Entries only, so a large folder can list before its counts arrive.
    pub fn read_entries(dir: &Path) -> Result<Self> {
        let fd = DirFd::open(dir)?;
        let entries: Vec<crate::model::Entry> = fd
            .list()?
            .into_iter()
            .map(crate::model::Entry::from_dirent)
            .collect();
        Ok(Self {
            entries,
            counts: std::collections::HashMap::new(),
            counts_ready: false,
        })
    }

    /// Entries and subfolder counts in one go, for tests and a synchronous
    /// refresh that should look finished.
    pub fn read(dir: &Path) -> Result<Self> {
        let fd = DirFd::open(dir)?;
        let entries: Vec<crate::model::Entry> = fd
            .list()?
            .into_iter()
            .map(crate::model::Entry::from_dirent)
            .collect();
        let counts = subfolder_counts(&fd, &entries, |a, b| device(a) == device(b));
        Ok(Self {
            entries,
            counts,
            counts_ready: true,
        })
    }
}

fn device(fd: &DirFd) -> Option<u64> {
    rustix::fs::fstat(fd.as_fd()).ok().map(|st| st.st_dev)
}

/// Item counts for the subfolders in `entries`. Each is opened without
/// following links, and one on another device than `parent` (a mount
/// point, which may be slow or asleep) is left uncounted, so blank.
fn subfolder_counts(
    parent: &DirFd,
    entries: &[crate::model::Entry],
    same_device: impl Fn(&DirFd, &DirFd) -> bool,
) -> std::collections::HashMap<OsString, usize> {
    entries
        .iter()
        .filter(|e| e.is_dir)
        .filter_map(|e| {
            let child = parent.open_dir_nofollow(&e.name).ok()?;
            if !same_device(parent, &child) {
                return None;
            }
            Some((e.name.clone(), child.list().ok()?.len()))
        })
        .collect()
}

/// Second pass over a listing already shown: reopen `dir` and count its
/// subfolders. `None` if the folder went away before the pass ran.
fn count_subfolders(
    dir: &Path,
    entries: &[crate::model::Entry],
) -> Option<std::collections::HashMap<OsString, usize>> {
    let fd = DirFd::open(dir).ok()?;
    Some(subfolder_counts(&fd, entries, |a, b| {
        device(a) == device(b)
    }))
}

pub struct Controller {
    /// The type-to-filter field is open (Ctrl+F or `/`).
    pub searching: bool,
    pub dir: PathBuf,
    pub listing: Listing,
    pub clip: Clipboard,
    pub env: Env,
    pub dialog: Option<Dialog>,
    pub focus: i32,
    pub armed: bool,
    pub drag: Option<Drag>,
    /// Mounts and free space; the system's, or a fixture's in renders.
    pub mounts: std::sync::Arc<dyn crate::mounts::MountSource>,
    pressed: Option<usize>,
    /// A Ctrl-press on an already selected item: it is toggled off on
    /// release, and only if no drag started, so Ctrl-drag can carry it.
    ctrl_untoggle: Option<OsString>,
    pub job_running: bool,
    /// A fixed (now, UTC offset) for tests and renders; the system clock
    /// and zone otherwise.
    pub clock: Option<(i64, i64)>,
    /// Item counts of the folders in the open folder, taken on each reread.
    pub counts: std::collections::HashMap<OsString, usize>,
    /// False until the first read of the open folder lands.
    pub loaded: bool,
    /// Bumped by every read request; a result from an older one is dropped.
    read_gen: u64,
    /// The read in flight, if any, and whether it is a navigation.
    in_flight: Option<ReadKind>,
    /// The navigation in flight: its target, when it started, and whether
    /// it has been slow enough to show as pending.
    nav: Option<PendingNav>,
    /// Set when the first read of the open folder failed: the empty state
    /// shows why instead of claiming the folder is empty.
    /// `Some(denied)`; denied is true for EACCES/EPERM.
    pub unreadable: Option<bool>,
    /// A navigation failed; reread the folder still open.
    refresh_due: bool,
    /// The Trash view's trashinfo, by stored name; empty elsewhere.
    pub trash: TrashInfo,
    /// Tiles instead of rows.
    pub grid: bool,
    /// Grid tile size: 0 small, 1 medium, 2 large.
    pub tile_step: usize,
    /// Compact density: shorter rows, bars and tiles. Only the render
    /// harness sets it (`--density compact`); the app is always
    /// comfortable and offers no way to switch.
    pub compact: bool,
    /// Tiles per grid line, refit to the content width on every redraw.
    pub columns: std::cell::Cell<usize>,
    /// The list's middle-cut name budget the rows were last built for.
    pub name_budget: std::cell::Cell<usize>,
    /// The media viewer, while it is open over the browser.
    pub viewer: Option<crate::viewer::Viewer>,
    /// The open image's pixel size and the viewer's canvas size, from the
    /// last redraw; zoom steps are taken against them.
    pub view_geom: std::cell::Cell<((f32, f32), (f32, f32))>,
    /// The Move-to-trash confirm was answered before, so its "Don't ask
    /// again" box starts ticked.
    pub trash_asked: bool,
    /// "Don't ask again" was ticked: Del trashes without asking.
    pub skip_trash_confirm: bool,
    /// The open trash confirm's "Don't ask again" box.
    pub dont_ask: bool,
}

/// Grid tile widths (S/M/L) per density, from the spec's density table.
const TILES: [[f32; 3]; 2] = [[112.0, 152.0, 208.0], [96.0, 128.0, 176.0]];
/// The most tiles a grid line holds; `GridLine` has six slots.
pub const MAX_COLUMNS: usize = 6;

/// How many tiles fit a content area `width` wide: 16 padding each
/// side and an 8 gap between tiles.
/// `18240` as `18 240`, as the sidebar and status bar print numbers.
fn group(n: usize) -> String {
    crate::stats::group_digits(n as u64)
}

pub fn fit_columns(width: f32, tile: f32) -> usize {
    let n = ((width - 32.0 + 8.0) / (tile + 8.0)).floor();
    (n.max(1.0) as usize).min(MAX_COLUMNS)
}

/// The list drops its Type column under this content width
/// (`Columns.no-type` in app.slint).
/// One cut, at the narrowest wide-column list that leaves Name 220px
/// beside Type. Below it the columns only shrink and Name only grows
/// as Type goes, so Type never flickers back on the way down.
pub const TYPE_MIN_LIST: f32 = 668.0;
/// Name never gets narrower than this while Type shows.
pub const NAME_MIN_W: f32 = 220.0;

/// How many characters the list's Name column holds at `width`,
/// mirroring `Columns` in app.slint (monospace, ~7.3px a glyph at 12px).
/// How many tiles a drag from the tile in column `col` moved, as an
/// index offset: `dx` is clamped to the line so a drag past the grid's
/// right or left edge can't wrap onto the next or previous line.
pub fn tile_offset(col: usize, dx: i32, dy: i32, columns: usize) -> i32 {
    let (col, columns) = (col as i32, columns.max(1) as i32);
    let dx = dx.clamp(-col, (columns - 1 - col).max(0));
    dy * columns + dx
}

pub fn name_chars(width: f32) -> usize {
    ((name_w(width) / 7.3).floor().max(6.0)) as usize
}

/// The Name column's width in a list `width` px wide, and whether
/// Type is dropped: mirrors `Columns` in app.slint.
pub fn columns(width: f32) -> (f32, bool) {
    let (size, modified) = if width < 480.0 {
        (64.0, 92.0)
    } else if width < 640.0 {
        (72.0, 104.0)
    } else {
        (88.0, 140.0)
    };
    let without = width - 12.0 - modified - 16.0 - size - 16.0 - 40.0;
    let with = without - 120.0 - 16.0;
    let no_type = width < TYPE_MIN_LIST;
    (if no_type { without } else { with }, no_type)
}

fn name_w(width: f32) -> f32 {
    columns(width).0
}

/// Printable text, as typed into a search: not a key name
/// like Escape or an arrow, which arrive as control or private-use chars.
fn is_typed(text: &str) -> bool {
    // An IME commit or a dead key can arrive as several chars at once.
    !text.is_empty()
        && text
            .chars()
            .all(|c| !c.is_control() && !('\u{f700}'..='\u{f8ff}').contains(&c))
}

/// "812 GB free of 1.8 TB" as "812 GB free", for narrow windows.
pub fn short_free(space: &str) -> String {
    space.split(" of ").next().unwrap_or_default().to_string()
}

/// Cut `name` in the middle so its ending stays visible:
/// `dune-sunset-0412.jpg` at 17 becomes `dune-sun…0412.jpg`.
pub fn elide_middle(name: &str, max: usize) -> String {
    use unicode_segmentation::UnicodeSegmentation;
    use unicode_width::UnicodeWidthStr;
    // Cut between graphemes, never inside one (é as e + accent, flags,
    // emoji families), and count wide CJK glyphs as two cells.
    if name.width() <= max || max < 3 {
        return name.to_string();
    }
    let parts: Vec<&str> = name.graphemes(true).collect();
    let tail_budget = (max - 1) / 2;
    let head_budget = max - 1 - tail_budget;
    let take = |iter: &mut dyn Iterator<Item = &&str>, budget: usize| {
        let mut used = 0;
        let mut n = 0;
        for g in iter {
            let w = g.width();
            if used + w > budget {
                break;
            }
            used += w;
            n += 1;
        }
        n
    };
    let head = take(&mut parts.iter(), head_budget);
    let tail = take(&mut parts.iter().rev(), tail_budget).min(parts.len() - head);
    let mut out: String = parts[..head].concat();
    out.push('…');
    out.push_str(&parts[parts.len() - tail..].concat());
    out
}

/// How long a navigation read may take before the breadcrumb jumps to the
/// target and the toolbar shows a progress bar.
pub const NAV_PENDING_AFTER: Duration = Duration::from_millis(150);

#[derive(Debug)]
struct PendingNav {
    gen: u64,
    target: PathBuf,
    started: std::time::Instant,
    shown: bool,
}

/// Why a folder is read: a navigation keeps the old folder on screen and
/// never falls back; a refresh rereads the open folder and, when it is
/// gone, walks up to the nearest ancestor that still exists.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ReadKind {
    Navigate,
    Refresh,
}

/// A read to run off the UI thread.
#[derive(Debug)]
pub struct ReadTicket {
    gen: u64,
    kind: ReadKind,
    dir: PathBuf,
    /// Set when `dir` is the home trash's files/: its trash folder, whose
    /// info/ is read alongside.
    trash: Option<PathBuf>,
}

/// A finished read: the folder actually read (an ancestor after a
/// fallback) and its snapshot, or the error.
#[derive(Debug)]
pub struct ReadResult {
    gen: u64,
    kind: ReadKind,
    result: Result<(PathBuf, Snapshot)>,
    trash: Option<TrashInfo>,
}

/// Where each item in the trash came from, by stored name.
pub type TrashInfo = std::collections::HashMap<OsString, crate::trash::Info>;

impl ReadTicket {
    /// The request's generation number.
    pub fn gen(&self) -> u64 {
        self.gen
    }

    /// Do the blocking read, including subfolder counts. Only
    /// ENOENT/ENOTDIR on a refresh walk up. Tests and the scripted
    /// harness use this so a finished read looks complete.
    pub fn perform(self) -> ReadResult {
        let (mut read, follow) = self.perform_listing();
        if let Some((_, dir, entries)) = follow {
            if let Ok((_, snap)) = &mut read.result {
                if let Some(counts) = count_subfolders(&dir, &entries) {
                    snap.counts = counts;
                    snap.counts_ready = true;
                }
            }
        }
        read
    }

    /// List the folder (walking up on a gone refresh) without counting
    /// subfolders. The optional follow-up is the generation, the folder
    /// actually listed, and its entries, so a second pass can fill counts.
    pub fn perform_listing(self) -> (ReadResult, Option<(u64, PathBuf, Vec<crate::model::Entry>)>) {
        let result = match Snapshot::read_entries(&self.dir) {
            Ok(snap) => Ok((self.dir, snap)),
            Err(e) if e.is_gone() && self.kind == ReadKind::Refresh => {
                let mut dir = self.dir;
                let mut found = None;
                while dir.pop() {
                    if let Ok(snap) = Snapshot::read_entries(&dir) {
                        found = Some((dir, snap));
                        break;
                    }
                }
                found.ok_or(e)
            }
            Err(e) => Err(e),
        };
        let follow = result
            .as_ref()
            .ok()
            .map(|(dir, snap)| (self.gen, dir.clone(), snap.entries.clone()));
        let trash = self
            .trash
            .as_deref()
            .map(|t| crate::trash::read_all(t, None));
        (
            ReadResult {
                gen: self.gen,
                kind: self.kind,
                result,
                trash,
            },
            follow,
        )
    }
}

/// What landing a read did to the controller.
#[derive(Debug)]
pub enum Landed {
    /// A newer request superseded it; nothing changed.
    Stale,
    /// Same entries and counts as shown.
    Unchanged,
    /// Applied; `moved` when the open folder changed.
    Shown { moved: bool },
    /// The read failed; the open folder and listing are kept.
    Failed(Error),
}

pub mod keys {
    pub const DELETE: &str = "\u{7f}";
    pub const RETURN: &str = "\n";
    pub const BACKSPACE: &str = "\u{8}";
    pub const ESCAPE: &str = "\u{1b}";
    pub const TAB: &str = "\t";
    pub const BACKTAB: &str = "\u{19}";
    pub const UP: &str = "\u{F700}";
    pub const DOWN: &str = "\u{F701}";
    pub const LEFT: &str = "\u{F702}";
    pub const RIGHT: &str = "\u{F703}";
    pub const HOME: &str = "\u{F729}";
    pub const END: &str = "\u{F72B}";
}

/// Ctrl+letter arrives either as the letter or as its control character.
fn ctrl_letter(text: &str, letter: char) -> bool {
    let ctl = char::from(letter.to_ascii_lowercase() as u8 - b'a' + 1);
    text.eq_ignore_ascii_case(&letter.to_string()) || text == ctl.to_string()
}

impl Controller {
    /// Open the viewer on `name` if it is media, browsing the folder's
    /// media in view order.
    pub fn open_viewer(&mut self, name: &OsString) -> Action {
        let names: Vec<OsString> = self
            .listing
            .view()
            .iter()
            .filter(|e| !e.is_dir)
            .map(|e| e.name.clone())
            .collect();
        match crate::viewer::Viewer::open(&names, name) {
            Some(v) => {
                self.viewer = Some(v);
                Action::Redraw
            }
            None => Action::None,
        }
    }

    /// Jump to the filmstrip's `tile`th tile.
    pub fn viewer_go(&mut self, tile: usize) -> Action {
        match self.viewer.as_mut() {
            Some(v) => {
                let index = v.strip_window(STRIP_TILES).start + tile;
                v.go(index);
                Action::Redraw
            }
            None => Action::None,
        }
    }

    /// Hand the open item to the desktop's default app (xdg-open): the
    /// way out for videos and for pictures Robin can't show.
    pub fn viewer_external(&self) -> Action {
        match self.viewer.as_ref() {
            Some(v) => Action::Launch(crate::delegate::argv(
                crate::delegate::Handoff::Default,
                &self.dir.join(v.current()),
            )),
            None => Action::None,
        }
    }

    /// A drag on the picture moved (`dx`, `dy`) screen pixels.
    pub fn viewer_pan(&mut self, dx: f32, dy: f32) -> Action {
        let (image, view) = self.view_geom.get();
        match self.viewer.as_mut() {
            Some(v) if v.zoomed(image, view) => {
                v.pan_by((dx, dy), image, view);
                Action::Redraw
            }
            _ => Action::None,
        }
    }

    /// The window is now `window` big: keep the pan inside the new
    /// limits, so growing the window never leaves a gap at an edge.
    pub fn viewer_resized(&mut self, window: (f32, f32)) -> Action {
        let (image, _) = self.view_geom.get();
        let Some(v) = self.viewer.as_mut() else {
            return Action::None;
        };
        let info = if v.info { 332.0 } else { 0.0 };
        let view = ((window.0 - info).max(1.0), window.1.max(1.0));
        self.view_geom.set((image, view));
        let before = v.pan;
        v.pan_by((0.0, 0.0), image, view);
        if v.pan == before {
            Action::None
        } else {
            Action::Redraw
        }
    }

    /// The mouse moved over the viewer: bring the chrome back.
    pub fn viewer_wake(&mut self) -> Action {
        match self.viewer.as_mut() {
            Some(v) if v.chrome_hidden => {
                v.chrome_hidden = false;
                Action::Redraw
            }
            _ => Action::None,
        }
    }

    /// The chrome's idle timer ran out: hide it again if still zoomed.
    pub fn viewer_idle(&mut self) -> Action {
        let (image, view) = self.view_geom.get();
        match self.viewer.as_mut() {
            Some(v) if !v.chrome_hidden && v.zoomed(image, view) => {
                v.chrome_hidden = true;
                Action::Redraw
            }
            _ => Action::None,
        }
    }

    /// Close the viewer; the browser's selection lands on the last item
    /// viewed, so Esc after browsing ten photos leaves you on the tenth.
    pub fn close_viewer(&mut self) {
        if let Some(v) = self.viewer.take() {
            self.listing.select_only(v.current().clone());
        }
    }

    /// The item, selection and clipboard summary. Blank when the folder
    /// can't be read: "0 items" would claim it is empty.
    /// The status bar's left side: `Reading…` until the first read lands,
    /// `Reading… 18 240 items` while rows are still being built, then the
    /// usual count and selection.
    pub fn status_line(&self, filling: bool) -> String {
        if !self.loaded {
            return "Reading…".into();
        }
        if filling && self.unreadable.is_none() {
            return format!("Reading… {}", ops::count(self.listing.view().len()));
        }
        self.status_left()
    }

    pub fn status_left(&self) -> String {
        if self.unreadable.is_some() {
            return String::new();
        }
        let n = self.listing.view().len();
        let sel = self.listing.selection().len();
        let searching = !self.listing.query().is_empty();
        let mut left = if searching {
            format!(
                "{} of {}",
                group(n),
                ops::count(self.listing.searchable_count())
            )
        } else {
            ops::count(n)
        };
        let hidden = self.listing.hidden_count();
        if !searching && !self.listing.show_hidden() && hidden > 0 {
            left = format!("{left} · {} hidden", group(hidden));
        }
        if sel > 0 {
            left = format!("{left} · {} selected", group(sel));
        }
        if let Some(clip) = self.clip.clip() {
            let verb = if clip.mode == ops::ClipMode::Cut {
                "cut"
            } else {
                "copied"
            };
            left = format!("{left} · {} {verb}", ops::count(clip.names.len()));
        }
        left
    }

    pub fn new(dir: PathBuf, env: Env) -> Self {
        Self {
            dir,
            listing: Listing::default(),
            clip: Clipboard::default(),
            env,
            dialog: None,
            focus: 0,
            armed: false,
            drag: None,
            mounts: std::sync::Arc::new(crate::mounts::SystemMounts),
            pressed: None,
            ctrl_untoggle: None,
            job_running: false,
            clock: None,
            counts: Default::default(),
            loaded: true,
            read_gen: 0,
            in_flight: None,
            nav: None,
            unreadable: None,
            refresh_due: false,
            trash: TrashInfo::new(),
            grid: false,
            searching: false,
            tile_step: 1,
            compact: false,
            columns: std::cell::Cell::new(1),
            name_budget: std::cell::Cell::new(0),
            viewer: None,
            view_geom: std::cell::Cell::new(((0.0, 0.0), (0.0, 0.0))),
            trash_asked: false,
            skip_trash_confirm: false,
            dont_ask: false,
        }
    }

    /// Ask to open `dir`. The current folder stays until the read lands.
    pub fn begin_navigate(&mut self, dir: PathBuf) -> ReadTicket {
        self.begin_navigate_at(dir, std::time::Instant::now())
    }

    /// `begin_navigate` at a given time. A navigation that supersedes one
    /// already shown as pending retargets the breadcrumb right away.
    pub fn begin_navigate_at(&mut self, dir: PathBuf, now: std::time::Instant) -> ReadTicket {
        let shown = self.nav.as_ref().is_some_and(|n| n.shown);
        let ticket = self.ticket(dir.clone(), ReadKind::Navigate);
        self.nav = Some(PendingNav {
            gen: ticket.gen,
            target: dir,
            started: now,
            shown,
        });
        ticket
    }

    /// Mark navigation `gen` as pending once it has taken
    /// `NAV_PENDING_AFTER`, if it is still the one in flight; true when
    /// that changed what is shown.
    pub fn tick_nav(&mut self, gen: u64, now: std::time::Instant) -> bool {
        match &mut self.nav {
            Some(n)
                if n.gen == gen
                    && self.read_gen == gen
                    && !n.shown
                    && now.duration_since(n.started) >= NAV_PENDING_AFTER =>
            {
                n.shown = true;
                true
            }
            _ => false,
        }
    }

    /// Whether a navigation's read is still in flight, slow or not.
    pub fn nav_in_flight(&self) -> bool {
        self.nav.is_some()
    }

    /// The folder a navigation in flight is opening, if any.
    pub fn nav_target(&self) -> Option<&Path> {
        self.nav.as_ref().map(|n| n.target.as_path())
    }

    /// Whether a slow navigation is showing its progress bar.
    pub fn nav_pending(&self) -> bool {
        self.nav.as_ref().is_some_and(|n| n.shown)
    }

    /// The folder the breadcrumb names: a slow navigation's target, or the
    /// open folder.
    pub fn crumb_dir(&self) -> &Path {
        match &self.nav {
            Some(n) if n.shown => &n.target,
            _ => &self.dir,
        }
    }

    /// Ask to reread the open folder; `None` while a navigation is in
    /// flight, since its result will replace this folder anyway.
    pub fn begin_refresh(&mut self) -> Option<ReadTicket> {
        if self.in_flight == Some(ReadKind::Navigate) {
            return None;
        }
        Some(self.ticket(self.dir.clone(), ReadKind::Refresh))
    }

    /// Whether a failed navigation asked for the open folder to be
    /// reread; clears the request.
    pub fn take_refresh_due(&mut self) -> bool {
        std::mem::take(&mut self.refresh_due)
    }

    /// Whether a failed read of `kind` should toast: a reread of a folder
    /// already showing the denied state says it there instead.
    pub fn toasts_failure(&self, kind: ReadKind) -> bool {
        !(kind == ReadKind::Refresh && self.unreadable.is_some())
    }

    /// Whether no read is in flight.
    pub fn idle(&self) -> bool {
        self.in_flight.is_none()
    }

    fn ticket(&mut self, dir: PathBuf, kind: ReadKind) -> ReadTicket {
        self.read_gen += 1;
        self.in_flight = Some(kind);
        let trash = (dir == self.trash_files()).then(|| self.env.home_trash.clone());
        ReadTicket {
            gen: self.read_gen,
            kind,
            dir,
            trash,
        }
    }

    /// The home trash's files/, the folder the Trash view shows.
    pub fn trash_files(&self) -> PathBuf {
        self.env.home_trash.join("files")
    }

    /// Whether the open folder is the Trash.
    pub fn in_trash(&self) -> bool {
        self.dir == self.trash_files()
    }

    /// Restore the selected trash items to where they came from. Items
    /// with no trashinfo have nowhere to go and stay put.
    pub fn restore_selected(&self) -> Action {
        if !self.in_trash() {
            return Action::None;
        }
        let items: Vec<ops::Trashed> = self
            .names()
            .into_iter()
            .filter_map(|n| {
                let info = self.trash.get(&n)?;
                Some(ops::Trashed {
                    trash: self.env.home_trash.clone(),
                    stored: n,
                    original: info.original.clone(),
                })
            })
            .collect();
        if items.is_empty() {
            return Action::None;
        }
        Action::Run(Job::restore(items))
    }

    /// The banner over the trash list when the selection holds items with
    /// no trashinfo: Restore skips them, so say how to keep them.
    pub fn trash_note(&self) -> String {
        if !self.in_trash() {
            return String::new();
        }
        let lost = self
            .names()
            .iter()
            .filter(|n| !self.trash.contains_key(*n))
            .count();
        match lost {
            0 => String::new(),
            1 => "1 item has no saved location and can't be restored. Drag it out to keep it."
                .into(),
            n => format!(
                "{n} items have no saved location and can't be restored. Drag them out to keep them."
            ),
        }
    }

    /// Empty Trash: ask first, naming how many items go.
    pub fn empty_trash(&self) -> Action {
        let names: Vec<OsString> = self
            .listing
            .entries()
            .iter()
            .map(|e| e.name.clone())
            .collect();
        if !self.in_trash() || names.is_empty() {
            return Action::None;
        }
        Action::Open(Dialog::EmptyTrash(names))
    }

    /// Apply a finished read unless a newer request superseded it.
    pub fn land(&mut self, read: ReadResult) -> Landed {
        if read.gen != self.read_gen {
            return Landed::Stale;
        }
        self.in_flight = None;
        self.nav = None;
        if read.result.is_ok() {
            self.trash = read.trash.unwrap_or_default();
        }
        let was_loaded = std::mem::replace(&mut self.loaded, true);
        let was_unreadable = self.unreadable.take().is_some();
        if let Err(e) = &read.result {
            if !was_loaded || was_unreadable {
                self.unreadable = Some(e.is_denied());
            }
            if read.kind == ReadKind::Navigate {
                self.refresh_due = true;
            }
        }
        match read.result {
            Ok((dir, snap)) if dir != self.dir => {
                let (key, order) = self.listing.sort();
                let hidden = self.listing.show_hidden();
                self.dir = dir;
                // A new folder starts unfiltered.
                self.searching = false;
                self.listing = Listing::default();
                self.listing.set_sort(key, order);
                self.listing.set_show_hidden(hidden);
                // Counts from the previous folder must not carry over,
                // even when a child has the same name.
                self.counts.clear();
                self.apply_snapshot(snap);
                // The viewer browses one folder; leaving it closes it.
                self.viewer = None;
                Landed::Shown { moved: true }
            }
            Ok((_, snap)) => {
                if was_loaded
                    && !was_unreadable
                    && snap.entries == self.listing.entries()
                    && (!snap.counts_ready || snap.counts == self.counts)
                {
                    return Landed::Unchanged;
                }
                self.apply_snapshot(snap);
                self.prune_viewer();
                Landed::Shown { moved: false }
            }
            Err(e) => Landed::Failed(e),
        }
    }

    /// Reread after an outside change, synchronously, through the same
    /// path as the worker: only a gone folder falls back to an ancestor.
    pub fn refresh(&mut self) -> Result<()> {
        let ticket = self.ticket(self.dir.clone(), ReadKind::Refresh);
        match self.land(ticket.perform()) {
            Landed::Failed(e) => Err(e),
            _ => Ok(()),
        }
    }

    /// After a reread: drop viewer items that are gone, closing the
    /// viewer when none are left.
    fn prune_viewer(&mut self) {
        let Some(v) = self.viewer.as_mut() else {
            return;
        };
        let present: std::collections::HashSet<&OsString> =
            self.listing.entries().iter().map(|e| &e.name).collect();
        if !v.retain(|n| present.contains(n)) {
            self.viewer = None;
        }
    }

    pub fn apply_snapshot(&mut self, snap: Snapshot) {
        if !snap.counts_ready {
            // Keep numbers already shown for folders that are still
            // here, so a reread doesn't blank them before the second
            // pass. New folders stay blank, never "0".
            self.counts
                .retain(|name, _| snap.entries.iter().any(|e| e.is_dir && e.name == *name));
        }
        self.listing.set_entries(snap.entries);
        if snap.counts_ready {
            self.counts = snap.counts;
        }
    }

    /// Apply a finished count pass when it still matches the open
    /// folder and the request that listed it. True when the numbers
    /// changed and the window should redraw, keeping scroll.
    pub fn apply_counts(
        &mut self,
        gen: u64,
        dir: &Path,
        counts: std::collections::HashMap<OsString, usize>,
    ) -> bool {
        if gen != self.read_gen || dir != self.dir || self.counts == counts {
            return false;
        }
        self.counts = counts;
        true
    }

    pub fn reload(&mut self) -> Result<()> {
        let snap = Snapshot::read(&self.dir)?;
        self.trash = if self.in_trash() {
            crate::trash::read_all(&self.env.home_trash, None)
        } else {
            TrashInfo::new()
        };
        self.apply_snapshot(snap);
        Ok(())
    }

    /// What an operation acts on: the selection as far as the view
    /// shows it, never a name hidden by the filter or the dotfile toggle.
    fn names(&self) -> Vec<OsString> {
        self.listing.visible_selection()
    }

    fn row_name(&self, index: usize) -> Option<(OsString, bool)> {
        self.listing
            .view()
            .get(index)
            .map(|e| (e.name.clone(), e.is_dir))
    }

    pub fn open_dialog(&mut self, dialog: Dialog) {
        self.focus = dialog.default_focus();
        // On by default once the confirm has been answered before.
        self.dont_ask = self.trash_asked;
        self.armed = !matches!(
            dialog,
            Dialog::Delete(_) | Dialog::EmptyTrash(_) | Dialog::ClearThumbs(_)
        );
        self.dialog = Some(dialog);
    }

    /// Del or Shift+Del on the selection.
    pub fn remove(&self, permanent: bool) -> Action {
        let names = self.names();
        if names.is_empty() {
            return Action::None;
        }
        // The trash has no trash: Del there deletes for good, and says so.
        if permanent || self.in_trash() {
            return Action::Open(Dialog::Delete(names));
        }
        match ops::trash_choice(&self.dir, &self.env) {
            TrashChoice::Ask => Action::Open(Dialog::TrashNo(names)),
            // "Don't ask again" was ticked: trash straight away; the
            // toast's Undo is the way back.
            _ if self.skip_trash_confirm => {
                Action::Run(Job::new(JobKind::Trash, &self.dir, names, &self.dir))
            }
            _ => Action::Open(Dialog::Trash(names)),
        }
    }

    /// A button under an empty or error state (DESIGN §3).
    pub fn state_action(&mut self, action: &str) -> Action {
        match action {
            "new-folder" => self.new_folder(),
            "admin" => self.open_as_admin(),
            "back" => match self.dir.parent() {
                Some(p) => Action::Navigate(p.to_path_buf()),
                None => Action::None,
            },
            "home" => match self.env.home.clone() {
                Some(home) => Action::Navigate(home),
                None => Action::None,
            },
            "search-hidden" => {
                if !self.listing.show_hidden() {
                    self.toggle_hidden();
                }
                Action::Redraw
            }
            "clear" => self.clear_search(),
            _ => Action::None,
        }
    }

    /// The empty folder's [New folder]: "New folder", or "New folder 2"
    /// and so on when that is taken, created and selected.
    pub fn new_folder(&mut self) -> Action {
        if self.nav_pending() || self.in_trash() {
            return Action::None;
        }
        let name = (1..1000)
            .map(|n| {
                if n == 1 {
                    "New folder".to_string()
                } else {
                    format!("New folder {n}")
                }
            })
            .find(|n| self.dir.join(n).symlink_metadata().is_err());
        let Some(name) = name else {
            return Action::None;
        };
        let made = DirFd::open(&self.dir).and_then(|fd| {
            rustix::fs::mkdirat(fd.as_fd(), name.as_str(), rustix::fs::Mode::from(0o777))
                .map_err(|e| Error::io(self.dir.join(&name), std::io::Error::from(e)))
        });
        if made.is_err() {
            return Action::None;
        }
        let _ = self.refresh();
        self.listing.select_only(name.into());
        Action::Redraw
    }

    /// [Open as admin…]: a second Robin on this folder through pkexec,
    /// which asks for the password itself. Nothing runs until clicked.
    pub fn open_as_admin(&self) -> Action {
        let robin = std::env::current_exe()
            .map(PathBuf::into_os_string)
            .unwrap_or_else(|_| "robin".into());
        Action::Launch(vec!["pkexec".into(), robin, self.dir.clone().into()])
    }

    /// Ctrl+H. Hidden items that disappear leave the selection, so a
    /// later Del can't act on something no longer shown.
    pub fn toggle_hidden(&mut self) {
        let show = !self.listing.show_hidden();
        self.listing.set_show_hidden(show);
        if !show {
            self.listing.drop_unseen();
        }
    }

    /// The view settings worth remembering.
    pub fn prefs(&self) -> crate::prefs::ViewPrefs {
        crate::prefs::ViewPrefs {
            grid: self.grid,
            tile_step: self.tile_step,
            show_hidden: self.listing.show_hidden(),
            trash_asked: self.trash_asked,
            skip_trash_confirm: self.skip_trash_confirm,
        }
    }

    pub fn apply_prefs(&mut self, p: crate::prefs::ViewPrefs) {
        self.grid = p.grid;
        self.tile_step = p.tile_step.min(2);
        self.listing.set_show_hidden(p.show_hidden);
        self.trash_asked = p.trash_asked;
        self.skip_trash_confirm = p.skip_trash_confirm;
    }

    pub fn set_grid(&mut self, grid: bool) -> Action {
        if self.grid == grid {
            return Action::None;
        }
        self.grid = grid;
        Action::Redraw
    }

    /// Ctrl+= and Ctrl+- move between S, M and L, stopping at either end.
    pub fn step_tile(&mut self, by: i32) -> Action {
        let next = (self.tile_step as i32 + by).clamp(0, 2) as usize;
        if next == self.tile_step {
            return Action::None;
        }
        self.tile_step = next;
        Action::Redraw
    }

    pub fn tile(&self) -> f32 {
        TILES[usize::from(self.compact)][self.tile_step]
    }

    /// Fit the grid to a content area `width` wide. True when the
    /// column count changed and the lines must be rebuilt.
    pub fn fit(&self, width: f32) -> bool {
        let columns = fit_columns(width, self.tile());
        self.columns.replace(columns) != columns
    }

    /// Fit the list to a content area `width` wide. True only when the
    /// middle-cut name budget changed, so a resize rebuilds the rows
    /// only when some label would read differently.
    pub fn refit_list(&self, width: f32) -> bool {
        let budget = name_chars(width);
        self.name_budget.replace(budget) != budget
    }

    /// The grid as lines of up to `columns` tiles.
    pub fn grid_lines(&self, rows: &[Row]) -> Vec<GridLine> {
        let columns = self.columns.get().max(1);
        rows.chunks(columns)
            .enumerate()
            .map(|(i, chunk)| {
                let mut line = crate::shots::empty_line();
                line.n = chunk.len() as i32;
                line.first = (i * columns) as i32;
                for (k, row) in chunk.iter().enumerate() {
                    crate::shots::set_cell(
                        &mut line,
                        k,
                        &row.glyph,
                        &row.name,
                        row.role,
                        &row.badge,
                        row.selected || row.drop,
                        false,
                    );
                    let faded = row.hidden || row.cut;
                    match k {
                        0 => line.f0 = faded,
                        1 => line.f1 = faded,
                        2 => line.f2 = faded,
                        3 => line.f3 = faded,
                        4 => line.f4 = faded,
                        _ => line.f5 = faded,
                    }
                }
                line
            })
            .collect()
    }

    pub fn key(&mut self, text: &str, ctrl: bool, shift: bool) -> Action {
        if self.dialog.is_some() {
            return self.dialog_key(text, shift);
        }
        if let Some(v) = self.viewer.as_mut() {
            if ctrl {
                // Ctrl+R, Ctrl+Q and friends are the app's, not the viewer's.
                return Action::None;
            }
            let video =
                crate::viewer::media(Path::new(v.current())) == Some(crate::viewer::Media::Video);
            if video && text == keys::RETURN {
                return self.viewer_external();
            }
            // Shift+Space steps back, as Space steps on.
            let text = if shift && text == " " { "h" } else { text };
            let (image, view) = self.view_geom.get();
            return match v.key(text, image, view) {
                crate::viewer::Step::Ignored => Action::None,
                crate::viewer::Step::Close => {
                    self.close_viewer();
                    Action::Redraw
                }
                _ => Action::Redraw,
            };
        }
        if let Some(action) = self.search_key(text, ctrl) {
            return action;
        }
        if ctrl {
            if ctrl_letter(text, 'x') {
                self.clip.cut(&self.dir, self.names());
                return Action::Redraw;
            }
            if ctrl_letter(text, 'c') {
                self.clip.copy(&self.dir, self.names());
                return Action::Redraw;
            }
            if ctrl_letter(text, 'v') {
                if self.nav_pending() {
                    return Action::None; // the destination is changing
                }
                return self.clip.paste(&self.dir).map_or(Action::None, Action::Run);
            }
            if ctrl_letter(text, 'h') {
                self.toggle_hidden();
                return Action::Redraw;
            }
            match text {
                // Tile size only means something in the grid.
                "=" | "+" if self.grid => return self.step_tile(1),
                "-" | "_" if self.grid => return self.step_tile(-1),
                "1" => return self.set_grid(false),
                "2" => return self.set_grid(true),
                _ => {}
            }
        }
        let columns = if self.grid { self.columns.get() } else { 1 };
        let nav = match text {
            keys::UP => Some(Nav::Up),
            keys::DOWN => Some(Nav::Down),
            keys::LEFT if self.grid => Some(Nav::Left),
            keys::RIGHT if self.grid => Some(Nav::Right),
            keys::HOME => Some(Nav::Home),
            keys::END => Some(Nav::End),
            _ => None,
        };
        if let Some(nav) = nav {
            self.listing.move_cursor(nav, shift, columns);
            return Action::Redraw;
        }
        match text {
            keys::DELETE => self.remove(shift),
            keys::ESCAPE if self.clip.clip().is_some() => {
                self.clip.clear();
                Action::Redraw
            }
            keys::ESCAPE if self.job_running => Action::CancelJob,
            keys::RETURN => match self.listing.cursor().cloned() {
                Some(name) if self.dir.join(&name).is_dir() => {
                    Action::Navigate(self.dir.join(name))
                }
                Some(name) => self.open_viewer(&name),
                None => Action::None,
            },
            keys::BACKSPACE => match self.dir.parent() {
                Some(p) => Action::Navigate(p.to_path_buf()),
                None => Action::None,
            },
            _ => Action::None,
        }
    }

    /// Search keys, or None to fall through to the normal ones. Arrows,
    /// Return and Ctrl shortcuts keep working while searching.
    fn search_key(&mut self, text: &str, ctrl: bool) -> Option<Action> {
        let opens = (ctrl && ctrl_letter(text, 'f')) || (!ctrl && text == "/");
        if !self.searching {
            if opens {
                self.searching = true;
                return Some(Action::Redraw);
            }
            return None;
        }
        if ctrl {
            return ctrl_letter(text, 'f').then_some(Action::None);
        }
        match text {
            keys::ESCAPE => {
                self.close_search();
            }
            keys::BACKSPACE => {
                use unicode_segmentation::UnicodeSegmentation;
                // Remove what the user sees as one character (é, a flag).
                let q = self.listing.query();
                let Some((at, _)) = q.grapheme_indices(true).next_back() else {
                    return Some(self.close_search());
                };
                let q = q[..at].to_string();
                self.listing.set_query(&q);
            }
            // No name holds a slash, so "/" is the open key, never text.
            "/" => return Some(Action::None),
            _ if is_typed(text) => {
                let q = format!("{}{text}", self.listing.query());
                self.listing.set_query(&q);
            }
            _ => return None,
        }
        Some(Action::Redraw)
    }

    /// The empty-search state's title and note, when a query is set:
    /// what was searched for, how much, where, and whether hidden files
    /// were left out.
    pub fn no_match_text(&self) -> Option<(String, String)> {
        let query = self.listing.query();
        if query.is_empty() {
            return None;
        }
        let hidden = !self.listing.show_hidden() && self.listing.hidden_count() > 0;
        Some((
            format!("Nothing matches \"{query}\""),
            format!(
                "Searched {} in {}{}",
                ops::count(self.listing.searchable_count()),
                self.short_dir(),
                if hidden {
                    " (hidden files excluded)"
                } else {
                    ""
                }
            ),
        ))
    }

    fn close_search(&mut self) -> Action {
        self.searching = false;
        self.listing.set_query("");
        Action::Redraw
    }

    /// The folder's own name, for the search field's scope label.
    pub fn dir_name(&self) -> String {
        self.dir
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| self.dir.display().to_string())
    }

    /// Opens the search, as the toolbar button does.
    pub fn open_search(&mut self) -> Action {
        self.searching = true;
        Action::Redraw
    }

    /// The field's clear button: empty the query, keep the field open.
    pub fn clear_search(&mut self) -> Action {
        self.listing.set_query("");
        Action::Redraw
    }

    /// "~/Pictures" for a folder under home, the full path otherwise.
    pub fn short_dir(&self) -> String {
        match self
            .env
            .home
            .as_deref()
            .and_then(|h| self.dir.strip_prefix(h).ok())
        {
            Some(rest) if rest.as_os_str().is_empty() => "~".into(),
            Some(rest) => format!("~/{}", rest.display()),
            None => self.dir.display().to_string(),
        }
    }

    fn dialog_key(&mut self, text: &str, shift: bool) -> Action {
        let n = self.dialog.as_ref().map_or(1, Dialog::buttons);
        match text {
            keys::TAB | keys::RIGHT if !shift => {
                self.focus = (self.focus + 1) % n;
                Action::Redraw
            }
            keys::TAB | keys::BACKTAB | keys::LEFT => {
                self.focus = (self.focus + n - 1) % n;
                Action::Redraw
            }
            keys::RETURN | " " => self.button(self.focus),
            keys::ESCAPE => self.button(0),
            _ if matches!(self.dialog, Some(Dialog::Conflict { .. })) => {
                match text.to_ascii_lowercase().as_str() {
                    "s" => self.button(1),
                    "k" => self.button(2),
                    "r" => self.button(3),
                    "a" => self.button(4),
                    _ => Action::None,
                }
            }
            _ => Action::None,
        }
    }

    /// A dialog button: 0 is always Cancel/Close, 1 confirms.
    pub fn button(&mut self, index: i32) -> Action {
        let Some(dialog) = self.dialog.clone() else {
            return Action::None;
        };
        if let Dialog::Conflict { ask, apply_all } = dialog {
            let action = match index {
                0 => None,
                1 => Some(Conflict::Skip),
                2 => Some(Conflict::Rename),
                3 => Some(Conflict::Replace),
                4 => {
                    self.focus = 4;
                    self.dialog = Some(Dialog::Conflict {
                        ask,
                        apply_all: !apply_all,
                    });
                    return Action::Redraw;
                }
                _ => return Action::None,
            };
            self.dialog = None;
            return Action::Answer(action.map(|action| ConflictChoice { action, apply_all }));
        }
        if index == 0 {
            self.dialog = None;
            return Action::Redraw;
        }
        if index == 2 && matches!(dialog, Dialog::Trash(_)) {
            self.focus = 2;
            self.dont_ask = !self.dont_ask;
            return Action::Redraw;
        }
        if !self.armed {
            return Action::None;
        }
        self.dialog = None;
        match dialog {
            Dialog::Trash(names) => {
                self.trash_asked = true;
                self.skip_trash_confirm = self.dont_ask;
                Action::Run(Job::new(JobKind::Trash, &self.dir, names, &self.dir))
            }
            Dialog::TrashNo(names) | Dialog::Delete(names) => {
                Action::Run(Job::new(JobKind::Delete, &self.dir, names, &self.dir))
            }
            Dialog::EmptyTrash(names) => {
                let mut job = Job::new(JobKind::Delete, &self.dir, names, &self.dir);
                job.empty_trash = true;
                Action::Run(job)
            }
            Dialog::ClearThumbs(cache) => Action::ClearThumbs(cache),
            Dialog::Refused {
                job,
                copy,
                can_copy,
                ..
            } if can_copy => Action::Run(Job::new(JobKind::Copy, &job.src, copy, &job.dest)),
            Dialog::Refused { .. } | Dialog::Conflict { .. } => Action::Redraw,
        }
    }

    /// Pointer on row `index`: `kind` 0 down, 1 move, 2 up. `dy` is how many
    /// items the pointer is from the pressed one (rows in the list, row
    /// offset times columns plus tiles across in the grid).
    pub fn pointer(&mut self, index: usize, kind: i32, dy: i32, far: bool, mods: Mods) -> Action {
        match kind {
            0 => {
                self.pressed = Some(index);
                self.ctrl_untoggle = None;
                // Modifier clicks act on press, as in Thunar and Nautilus,
                // so the selection is right before any drag starts.
                if let Some((name, _)) = self.row_name(index) {
                    if mods.ctrl && self.listing.selection().contains(&name) {
                        self.ctrl_untoggle = Some(name);
                    } else if mods.ctrl {
                        self.listing.toggle(name);
                    } else if mods.shift {
                        self.listing.select_range_to(name);
                    } else if !self.listing.selection().contains(&name) {
                        self.listing.select_only(name);
                    }
                }
                Action::Redraw
            }
            1 => {
                let Some(start) = self.pressed else {
                    return Action::None;
                };
                if self.drag.is_none() && !far {
                    return Action::None;
                }
                let names = match &self.drag {
                    Some(d) => d.names.clone(),
                    None => self.names(),
                };
                let at = start as i64 + i64::from(dy);
                let folder = usize::try_from(at).ok().and_then(|t| {
                    let (name, is_dir) = self.row_name(t)?;
                    is_dir.then_some((t, name))
                });
                let mut refused = None;
                let blocked = self.nav_pending();
                let op = folder.as_ref().filter(|_| !blocked).and_then(|(t, name)| {
                    if names.contains(name) {
                        refused = Some(*t);
                        return None;
                    }
                    let dest = self.dir.join(name);
                    let op = ops::drop_op(mods, ops::same_filesystem(&self.dir, &dest))?;
                    match ops::drop_check(op, &self.dir, &names, &dest) {
                        ops::DropCheck::Ok => Some(op),
                        ops::DropCheck::IntoItself => {
                            refused = Some(*t);
                            None
                        }
                        ops::DropCheck::Nothing => None,
                    }
                });
                let next = Drag {
                    names,
                    target: op.and(folder.map(|f| f.0)),
                    op,
                    refused,
                };
                let changed = self.drag.as_ref() != Some(&next);
                self.drag = Some(next);
                if changed {
                    Action::Redraw
                } else {
                    Action::None
                }
            }
            _ => {
                self.pressed = None;
                let untoggle = self.ctrl_untoggle.take();
                let Some(drag) = self.drag.take() else {
                    if let Some(name) = untoggle {
                        self.listing.toggle(name);
                    } else if !far && !mods.ctrl && !mods.shift {
                        if let Some((name, _)) = self.row_name(index) {
                            self.listing.select_only(name);
                        }
                    }
                    return Action::Redraw;
                };
                match (drag.target, drag.op) {
                    (Some(t), Some(op)) if !self.nav_pending() => {
                        let dest = self
                            .dir
                            .join(self.row_name(t).map(|r| r.0).unwrap_or_default());
                        Action::Run(Job::transfer(op, &self.dir, drag.names, &dest))
                    }
                    _ => Action::Redraw,
                }
            }
        }
    }

    pub fn drag_label(&self) -> Option<(String, &'static str)> {
        let drag = self.drag.as_ref()?;
        if self.nav_pending() {
            return Some(("Wait for the folder to open".into(), "󰜺"));
        }
        if drag.refused.is_some() {
            return Some(("Can't drop into itself".into(), "󰜺"));
        }
        let (Some(t), Some(op)) = (drag.target, drag.op) else {
            return Some(("Drop on a folder".into(), "󰆏"));
        };
        let to = self.row_name(t)?.0.to_string_lossy().into_owned();
        let n = ops::count(drag.names.len());
        Some(match op {
            Op::Copy => (format!("Copy {n} to {to}"), "󰆏"),
            Op::Move => (format!("Move {n} to {to}"), "󰪹"),
        })
    }

    /// A finished job: the refused moves come back as a dialog.
    pub fn finish(&mut self, job: &Job, outcome: &Outcome) -> Option<Dialog> {
        // A cut lives until its move fully succeeds; a refused, failed,
        // cancelled or partial move keeps it so the user can try again.
        if job.kind == JobKind::Move
            && !outcome.cancelled
            && outcome.failed.is_empty()
            && outcome.refused.is_empty()
            && outcome.done == job.names.len()
        {
            self.clip.cut_moved(job);
        }
        if outcome.refused.is_empty() {
            return None;
        }
        let view = refusal_view(&outcome.refused);
        Some(Dialog::Refused {
            job: job.clone(),
            names: outcome.refused.iter().map(|r| r.0.clone()).collect(),
            copy: outcome
                .refused
                .iter()
                .filter(|(n, k)| fsops::refusal(&n.to_string_lossy(), k).can_copy_instead)
                .map(|r| r.0.clone())
                .collect(),
            can_copy: view.can_copy,
        })
    }

    /// A column header click: 0 Name, 1 Size, 2 Type, 3 Modified.
    pub fn sort_by(&mut self, column: i32) -> Action {
        let key = match column {
            0 => SortKey::Name,
            1 => SortKey::Size,
            2 => SortKey::Type,
            3 => SortKey::Date,
            _ => return Action::None,
        };
        self.listing.toggle_sort(key);
        Action::Redraw
    }

    /// (column, descending) for the header arrow.
    pub fn sort_header(&self) -> (i32, bool) {
        let (key, dir) = self.listing.sort();
        let column = match key {
            SortKey::Name => 0,
            SortKey::Size => 1,
            SortKey::Type => 2,
            SortKey::Date => 3,
        };
        (column, dir == SortDir::Desc)
    }

    pub fn rows(&self) -> Vec<Row> {
        let view = self.listing.view();
        self.rows_range(&view, 0, view.len())
    }

    /// Build `view[start..end]` as rows, so the window can fill a large
    /// listing in chunks without stalling the UI thread.
    pub fn rows_range(&self, view: &[&crate::model::Entry], start: usize, end: usize) -> Vec<Row> {
        let end = end.min(view.len());
        let start = start.min(end);
        let now = self
            .clock
            .map_or_else(|| secs(std::time::SystemTime::now()), |c| c.0);
        let offset = |t| {
            self.clock
                .map_or_else(|| crate::model::local_offset(t), |c| c.1)
        };
        let target = self.drag.as_ref().and_then(|d| d.target);
        let refused = self.drag.as_ref().and_then(|d| d.refused);
        let in_trash = self.in_trash();
        let home = self.env.home.as_deref();
        let rows: Vec<Row> = view[start..end]
            .iter()
            .enumerate()
            .map(|(off, e)| {
                let i = start + off;
                Row {
                    glyph: file_icon(e.is_dir, &e.extension()).0.into(),
                    name: e.name.to_string_lossy().as_ref().into(),
                    size: size_line(e.is_dir, e.len, self.counts.get(&e.name).copied()).into(),
                    kind: if e.is_dir {
                        "Folder".into()
                    } else {
                        kind_name(&e.extension())
                    }
                    .into(),
                    modified: crate::model::format_modified(secs(e.mtime), now, offset).into(),
                    selected: self.listing.selection().contains(&e.name),
                    hovered: false,
                    hidden: e.is_hidden(),
                    role: file_icon(e.is_dir, &e.extension()).1,
                    badge: SharedString::new(),
                    cut: self.clip.is_cut(&self.dir, &e.name),
                    drop: target == Some(i),
                    drop_refused: refused == Some(i),
                }
            })
            .collect();
        if !in_trash {
            return rows;
        }
        // Trash columns: Name, Original location, Deleted.
        rows.into_iter()
            .zip(view[start..end].iter())
            .map(|(mut row, e)| {
                let info = self.trash.get(&e.name);
                row.size = SharedString::new();
                row.kind = info
                    .map(|i| crate::trash::location_text(&i.original, home))
                    .unwrap_or_else(|| "Unknown".into())
                    .into();
                row.modified = info
                    .map(|i| crate::trash::deleted_text(&i.deleted))
                    .unwrap_or_default()
                    .into();
                row
            })
            .collect()
    }
}

/// The move-refused dialog's contents for every refused item. One item
/// reads as before; several get a "Can't move N items" title with each
/// item's reason as a heading over its blocking paths.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RefusalView {
    pub title: String,
    /// (path, note, heading) rows.
    pub lines: Vec<(String, String, bool)>,
    pub can_copy: bool,
    /// Set when only some items can be copied: says which stay behind.
    pub body: String,
    pub dismiss: &'static str,
    pub notes: usize,
    pub items: usize,
}

pub fn refusal_view(refused: &[(OsString, Vec<KeptEntry>)]) -> RefusalView {
    let parts: Vec<fsops::Refusal> = refused
        .iter()
        .map(|(n, k)| fsops::refusal(&n.to_string_lossy(), k))
        .collect();
    let can_copy = parts.iter().any(|r| r.can_copy_instead);
    let stuck: Vec<String> = refused
        .iter()
        .zip(&parts)
        .filter(|(_, r)| !r.can_copy_instead)
        .map(|((n, _), _)| n.to_string_lossy().into_owned())
        .collect();
    let body = if can_copy && !stuck.is_empty() {
        let n = parts.len() - stuck.len();
        format!(
            "Robin can copy the other {} there instead. {} can't be read, so {} not be copied either.",
            ops::count(n),
            stuck.join(", "),
            if stuck.len() == 1 { "it will" } else { "they will" },
        )
    } else {
        String::new()
    };
    let rows = |r: &fsops::Refusal| {
        r.lines
            .iter()
            .map(|l| (l.path.clone(), l.note.clone().unwrap_or_default(), false))
            .collect::<Vec<_>>()
    };
    if let [one] = parts.as_slice() {
        return RefusalView {
            title: one.title.clone(),
            lines: rows(one),
            can_copy,
            body,
            dismiss: one.dismiss_label,
            notes: one.note_count(),
            items: 1,
        };
    }
    let mut lines = Vec::new();
    let mut notes = 0;
    for r in &parts {
        let head = r.title.strip_prefix("Can't move ").unwrap_or(&r.title);
        lines.push((head.to_string(), String::new(), true));
        lines.extend(rows(r));
        notes += r.note_count();
    }
    RefusalView {
        title: format!("Can't move {}", ops::count(parts.len())),
        lines,
        can_copy,
        body,
        dismiss: if can_copy { "Cancel" } else { "Close" },
        notes,
        items: parts.len(),
    }
}

fn secs(t: std::time::SystemTime) -> i64 {
    match t.duration_since(std::time::UNIX_EPOCH) {
        Ok(d) => d.as_secs() as i64,
        Err(e) => -(e.duration().as_secs() as i64),
    }
}

fn human(n: u64) -> String {
    let units = ["B", "KB", "MB", "GB", "TB"];
    let mut v = n as f64;
    let mut u = 0;
    while v >= 1000.0 && u < units.len() - 1 {
        v /= 1000.0;
        u += 1;
    }
    if u == 0 {
        format!("{n} B")
    } else {
        format!("{v:.1} {}", units[u])
    }
}

/// The names line under a confirm title: up to five, then "and N more".
pub fn names_text(names: &[OsString]) -> String {
    let mut lines: Vec<String> = names
        .iter()
        .take(5)
        .map(|n| n.to_string_lossy().into_owned())
        .collect();
    if names.len() > 5 {
        lines.push(format!("and {} more", names.len() - 5));
    }
    lines.join("\n")
}

fn noun(names: &[OsString]) -> String {
    match names {
        [one] => format!("“{}”", one.to_string_lossy()),
        _ => ops::count(names.len()),
    }
}

// ---- window binding -------------------------------------------------------

struct State {
    ctl: Controller,
    queue: VecDeque<Job>,
    cancel: Option<Arc<Cancel>>,
    undo: Vec<Trashed>,
    /// Where the waiting job thread's conflict answer goes.
    reply: Option<mpsc::Sender<Option<ConflictChoice>>>,
    toast_open: bool,
    toast_gen: u64,
    /// Every detail line of the toast, its pinned flag, and whether the
    /// "+N more" was expanded.
    toast_all: Vec<String>,
    toast_pinned: bool,
    arm_timer: slint::Timer,
    /// Hides the viewer's chrome again after the mouse stops, when zoomed.
    chrome_timer: slint::Timer,
    /// Live refresh: the watch on `ctl.dir`, its debounce and poll timer.
    watch: Option<crate::watch::Watch>,
    /// Set while the open folder can't be watched.
    fallback: Option<crate::watch::Fallback>,
    debounce: crate::model::Debounce,
    watch_timer: slint::Timer,
    /// The last reread error shown, so it isn't repeated every tick.
    read_error: Option<String>,
    /// Paths another app dropped on the window, each with the folder that
    /// was open when it arrived.
    /// `None` while a navigation is in flight: resolved when it lands.
    dropped: Rc<RefCell<Vec<DroppedPath>>>,
    /// The open folder as the drop handler sees it; `None` while a
    /// navigation is in flight.
    drop_dest: Rc<RefCell<Option<PathBuf>>>,
    /// A drop held by a dialog or an in-flight navigation, and the toast
    /// generation of the 2s "waiting" notice if that is what is showing.
    drop_wait: Option<DropWait>,
    drop_wait_toast_gen: u64,
    /// The sidebar rows and status-right, as the last side job read them.
    side: crate::sidebar::SideData,
    side_gate: crate::sidebar::SideGate,
    /// Statistics: the latest report, and the newest scan's number so an
    /// older scan landing late is dropped.
    stats: Option<crate::stats::Report>,
    stats_gen: u64,
    stats_at: i64,
    /// The scan in flight, if any: a second open reuses it instead of
    /// starting another walk.
    stats_scan: Option<crate::stats::ScanCtl>,
    stats_tick: Option<slint::Timer>,
    /// statvfs with a timeout, shared by every side job.
    probe: Arc<crate::sidebar::SpaceProbe>,
    /// Where view settings are remembered, and what was last written.
    prefs_path: Option<PathBuf>,
    prefs_saved: std::cell::Cell<crate::prefs::ViewPrefs>,
    /// Decoded viewer images and filmstrip thumbnails, by path.
    pics: RefCell<Pics>,
    /// Rows already built for the current listing, filled in chunks so a
    /// 100k-entry folder doesn't stall the UI thread, flash an empty pane,
    /// or drop the scroll position and selection.
    rows_fill: RefCell<RowsFill>,
    files_model: RefCell<Option<Rc<VecModel<Row>>>>,
    labels_model: RefCell<Option<Rc<VecModel<SharedString>>>>,
    marks_model: RefCell<Option<Rc<VecModel<Mark>>>>,
    grid_model: RefCell<Option<Rc<VecModel<GridLine>>>>,
    row_fill_queued: std::cell::Cell<bool>,
}

type Shared = Rc<RefCell<State>>;

thread_local! {
    static STATE: RefCell<Option<(Shared, slint::Weak<AppWindow>)>> = const { RefCell::new(None) };
    /// Set by the render harness: reads wait in `PENDING_READS` until the
    /// script lands them, instead of running on a worker thread.
    static SCRIPTED: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
    static PENDING_READS: RefCell<VecDeque<ReadTicket>> = const { RefCell::new(VecDeque::new()) };
    static PENDING_DECODES: RefCell<VecDeque<DecodeJob>> = const { RefCell::new(VecDeque::new()) };
    static LAUNCHED: RefCell<Vec<Vec<OsString>>> = const { RefCell::new(Vec::new()) };
    static PENDING_STATS: RefCell<VecDeque<(u64, crate::stats::StatsJob)>> = const { RefCell::new(VecDeque::new()) };
    static PENDING_SIDE: RefCell<VecDeque<(u64, crate::sidebar::SideJob)>> = const { RefCell::new(VecDeque::new()) };
}

/// `build` for the render harness: the same window and callbacks, with
/// folder reads held for [`land_reads`] so a script decides when each
/// one arrives.
pub fn build_scripted(launch: Launch) -> Result<AppWindow> {
    SCRIPTED.with(|s| s.set(true));
    build(launch)
}

/// Perform and land every held read in the order it was started,
/// including rereads they queue.
pub fn land_reads() {
    loop {
        if let Some(ticket) = PENDING_READS.with(|p| p.borrow_mut().pop_front()) {
            let read = ticket.perform();
            with_state(|ui, st| landed(&ui, st, read));
        } else if let Some(job) = PENDING_DECODES.with(|p| p.borrow_mut().pop_front()) {
            let done = job.perform();
            with_state(|ui, st| decode_landed(&ui, st, done));
        } else if let Some((gen, job)) = PENDING_STATS.with(|p| p.borrow_mut().pop_front()) {
            let report = job.perform();
            with_state(|ui, st| stats_landed(&ui, st, gen, report));
        } else if let Some((gen, job)) = PENDING_SIDE.with(|p| p.borrow_mut().pop_front()) {
            let data = job.perform();
            with_state(|ui, st| side_landed(&ui, st, gen, data));
        } else {
            break;
        }
    }
}

/// Reread the sidebar and the open folder's free space on a worker:
/// item counts and statvfs can block on a slow or dead mount.
/// One job runs at a time; a request meanwhile reruns it once it lands.
fn start_side(s: &mut State) {
    let Some(gen) = s.side_gate.start() else {
        return;
    };
    let job = crate::sidebar::SideJob {
        env: s.ctl.env.clone(),
        mounts: s.ctl.mounts.clone(),
        dir: s.ctl.dir.clone(),
        probe: s.probe.clone(),
    };
    if SCRIPTED.with(|s| s.get()) {
        PENDING_SIDE.with(|p| p.borrow_mut().push_back((gen, job)));
        return;
    }
    std::thread::spawn(move || {
        // Freed on any path that doesn't land: a panic in perform, or a
        // failed post. If the post itself fails the loop is gone and this
        // one fails too, which is fine: there is no window left to free.
        let guard = crate::sidebar::SideGuard::new(move || {
            let _ = slint::invoke_from_event_loop(move || {
                with_state(|ui, st| side_abandoned(&ui, st, gen));
            });
        });
        let data = job.perform();
        if slint::invoke_from_event_loop(move || {
            with_state(|ui, st| side_landed(&ui, st, gen, data));
        })
        .is_ok()
        {
            guard.disarm();
        }
    });
}

/// Where the cache lives: `$XDG_CACHE_HOME` when absolute, else
/// `~/.cache`. Renders and tests always use the fixture's `~/.cache`.
fn cache_home(home: &Path) -> PathBuf {
    if !SCRIPTED.with(|s| s.get()) {
        if let Some(dir) = std::env::var_os("XDG_CACHE_HOME").map(PathBuf::from) {
            if dir.is_absolute() {
                return dir;
            }
        }
    }
    home.join(".cache")
}

/// Ask before clearing Robin's thumbnails from the cache root in use.
fn ask_clear_thumbs(ui: &AppWindow, st: &Shared) {
    let Some(home) = st.borrow().ctl.env.home.clone() else {
        return;
    };
    apply(ui, st, Action::Open(Dialog::ClearThumbs(cache_home(&home))));
}

/// Delete Robin's own thumbnails off the UI thread, then say how many
/// went and rescan so the cache card shows the new size.
fn clear_thumbs(ui: &AppWindow, st: &Shared, cache: PathBuf) {
    let home = st.borrow().ctl.env.home.clone();
    redraw(ui, &st.borrow());
    let work = move || crate::thumbs::clear_robins(&cache, home.as_deref());
    let finish = |ui: &AppWindow, st: &Shared, (n, bytes): (usize, u64)| {
        let text = match n {
            0 => "No Robin thumbnails to clear".to_string(),
            1 => format!("Cleared 1 thumbnail ({})", human(bytes)),
            n => format!("Cleared {n} thumbnails ({})", human(bytes)),
        };
        show_toast(ui, st, "", text, Vec::new(), false, "");
        if ui.get_stats_mode() != "" {
            open_stats(ui, st);
        }
    };
    if SCRIPTED.with(|s| s.get()) {
        finish(ui, st, work());
        return;
    }
    std::thread::spawn(move || {
        let gone = work();
        let _ = slint::invoke_from_event_loop(move || {
            with_state(|ui, st| finish(&ui, st, gone));
        });
    });
}

/// Open the Statistics view and (re)scan on a worker: a home walk can
/// take seconds. The last report shows meanwhile, dimmed. A scan already
/// running is reused, never doubled.
fn open_stats(ui: &AppWindow, st: &Shared) {
    ui.set_stats_mode("overview".into());
    let job = {
        let mut s = st.borrow_mut();
        if let Some(r) = &s.stats {
            show_stats(
                ui,
                r,
                &scanned_line(&s.ctl, s.stats_at, true),
                s.ctl.listing.show_hidden(),
            );
        }
        ui.set_stat_has_results(s.stats.is_some());
        if s.stats_scan.is_some() {
            ui.set_stat_scanning(true);
            return;
        }
        let Some(home) = s.ctl.env.home.clone() else {
            ui.set_stat_scanning(false);
            ui.set_stat_scanned("No home folder to scan".into());
            return;
        };
        s.stats_gen += 1;
        let ctl = crate::stats::ScanCtl::default();
        s.stats_scan = Some(ctl.clone());
        ui.set_stat_scanning(true);
        ui.set_stat_scan_count("0".into());
        if !SCRIPTED.with(|c| c.get()) {
            let timer = slint::Timer::default();
            let weak = ui.as_weak();
            let files = ctl.files.clone();
            timer.start(
                slint::TimerMode::Repeated,
                std::time::Duration::from_millis(200),
                move || {
                    if let Some(ui) = weak.upgrade() {
                        let n = files.load(std::sync::atomic::Ordering::Relaxed);
                        ui.set_stat_scan_count(crate::stats::group_digits(n).into());
                    }
                },
            );
            s.stats_tick = Some(timer);
        }
        (
            s.stats_gen,
            crate::stats::StatsJob {
                cache: cache_home(&home),
                home,
                trash: s.ctl.env.home_trash.clone(),
                mounts: s.ctl.mounts.clone(),
                ctl,
            },
        )
    };
    if SCRIPTED.with(|s| s.get()) {
        PENDING_STATS.with(|p| p.borrow_mut().push_back(job));
        return;
    }
    std::thread::spawn(move || {
        let (gen, job) = job;
        let report = job.perform();
        let _ = slint::invoke_from_event_loop(move || {
            with_state(|ui, st| stats_landed(&ui, st, gen, report));
        });
    });
}

/// Stop the running scan. Its report lands marked cancelled and is
/// dropped; the last full one stays.
fn cancel_stats(st: &Shared) {
    if let Some(ctl) = &st.borrow().stats_scan {
        ctl.cancel.store(true, std::sync::atomic::Ordering::Relaxed);
    }
}

fn stats_landed(ui: &AppWindow, st: &Shared, gen: u64, report: crate::stats::Report) {
    let mut s = st.borrow_mut();
    if gen != s.stats_gen {
        return;
    }
    s.stats_scan = None;
    s.stats_tick = None;
    ui.set_stat_scanning(false);
    if report.home.cancelled {
        ui.set_stat_has_results(s.stats.is_some());
        match &s.stats {
            Some(r) => show_stats(
                ui,
                r,
                &scanned_line(&s.ctl, s.stats_at, false),
                s.ctl.listing.show_hidden(),
            ),
            None => ui.set_stat_scanned("Scan cancelled · Rescan".into()),
        }
        return;
    }
    s.stats_at = ctl_now(&s.ctl);
    ui.set_stat_has_results(true);
    if ui.get_stats_mode() != "" {
        show_stats(
            ui,
            &report,
            &scanned_line(&s.ctl, s.stats_at, false),
            s.ctl.listing.show_hidden(),
        );
    }
    s.stats = Some(report);
}

/// The muted line under the treemap title and its tooltip, or empty when
/// nothing was left out.
pub fn skipped_line(scan: &crate::stats::Scan) -> (String, String) {
    let plural = |n: usize, one: &str, many: &str| {
        if n == 1 {
            format!("1 {one}")
        } else {
            format!("{n} {many}")
        }
    };
    let mut parts = Vec::new();
    let mut tip = Vec::new();
    if !scan.skipped.is_empty() {
        parts.push(plural(scan.skipped.len(), "folder", "folders") + " skipped (no access)");
        tip.push("Skipped (no access):".to_string());
        tip.extend(scan.skipped.iter().map(|p| format!("  {}", p.display())));
    }
    if !scan.other_drives.is_empty() {
        parts.push(plural(scan.other_drives.len(), "other drive", "other drives") + " not counted");
        tip.push("Other drives, not counted:".to_string());
        tip.extend(
            scan.other_drives
                .iter()
                .map(|p| format!("  {}", p.display())),
        );
    }
    (parts.join(" · "), tip.join("\n"))
}

/// Fill the Statistics view from a report. `stale` marks a rescan running.
pub fn show_stats(ui: &AppWindow, r: &crate::stats::Report, scanned: &str, hidden: bool) {
    use crate::stats::UsageLevel;
    let (skipped, tip) = skipped_line(&r.home);
    ui.set_stat_skipped(skipped.into());
    ui.set_stat_skipped_tip(tip.into());
    let disks: Vec<crate::StatDisk> = r
        .disks
        .iter()
        .map(|d| {
            let ratio = d.used_ratio();
            crate::StatDisk {
                name: d.name.clone().into(),
                mount: d.mount.clone().into(),
                amount: ratio,
                level: match crate::stats::usage_level(ratio) {
                    UsageLevel::Accent => 0,
                    UsageLevel::Warning => 1,
                    UsageLevel::Danger => 2,
                },
                pct: format!("{:.0}%", ratio * 100.0).into(),
                free: format!("{} free of {}", human(d.free), human(d.total)).into(),
            }
        })
        .collect();
    ui.set_stat_disks(ModelRc::new(VecModel::from(disks)));
    let total: u64 = r.home.kinds.iter().map(|k| k.1).sum();
    let mut start = 0.0;
    let types: Vec<crate::StatType> = crate::stats::KINDS
        .iter()
        .zip(r.home.kinds.iter())
        .enumerate()
        .map(|(i, (kind, (files, bytes)))| {
            let share = if total == 0 {
                0.0
            } else {
                *bytes as f32 / total as f32
            };
            let t = crate::StatType {
                name: kind.name().into(),
                count: crate::stats::group_digits(*files).into(),
                size: human(*bytes).into(),
                start,
                share,
                alpha: [1.0, 0.8, 0.6, 0.45, 0.32, 0.2, 0.1][i],
            };
            start += share;
            t
        })
        .collect();
    ui.set_stat_types(ModelRc::new(VecModel::from(types)));
    let weights = crate::stats::treemap_weights(&r.home.top, 9, hidden);
    ui.set_map_cells(ModelRc::new(VecModel::from(map_cells(
        &weights, 623.0, 300.0,
    ))));
    ui.set_stat_tree_title(
        format!(
            "LARGEST FOLDERS   ·   {} in home",
            human(r.home.total_bytes)
        )
        .into(),
    );
    ui.set_stat_cache_total(human(r.cache_bytes).into());
    ui.set_stat_cache_detail(
        if r.cache_parts.is_empty() {
            "Nothing cached".to_string()
        } else {
            r.cache_parts
                .iter()
                .map(|(n, b)| {
                    // Short labels so the line fits; it wraps if not.
                    let n = if n == "thumbnails" {
                        "thumbs"
                    } else {
                        n.as_str()
                    };
                    format!("{} {n}", human(*b))
                })
                .collect::<Vec<_>>()
                .join("  ·  ")
        }
        .into(),
    );
    ui.set_stat_trash_size(human(r.trash_bytes).into());
    ui.set_stat_trash_count(crate::ops::count(r.trash_items as usize).into());
    ui.set_stat_scanned(scanned.into());
}

/// "Scanned 16:10  ·  Rescan", or "Rescanning…" while a rescan runs.
fn scanned_line(ctl: &Controller, at: i64, stale: bool) -> String {
    let offset = |t| {
        ctl.clock
            .map_or_else(|| crate::model::local_offset(t), |c| c.1)
    };
    let when = crate::model::format_clock_or_date(at, ctl_now(ctl), offset);
    let tail = if stale { "Rescanning…" } else { "Rescan" };
    format!("Scanned {when}  ·  {tail}")
}

fn ctl_now(ctl: &Controller) -> i64 {
    ctl.clock
        .map_or_else(|| secs(std::time::SystemTime::now()), |c| c.0)
}

/// Treemap cells for (name, bytes) weights in a `w` by `h` box: the
/// largest is full accent and each lower rank, or deeper row, is darker.
/// Full accent marks the biggest of several: other tiles stay below it,
/// and a lone tile is a surface tint with an accent outline.
pub fn map_cells(weights: &[(String, u64)], w: f32, h: f32) -> Vec<crate::MapCell> {
    let rank_of: std::collections::HashMap<&str, usize> = weights
        .iter()
        .enumerate()
        .map(|(i, (n, _))| (n.as_str(), i))
        .collect();
    let bytes: std::collections::HashMap<&str, u64> =
        weights.iter().map(|(n, b)| (n.as_str(), *b)).collect();
    // 1px off each shared edge is a 2px surface gutter.
    const GUTTER: f32 = 1.0;
    const LONE_TINT: f32 = 0.12;
    let lone = weights.len() == 1;
    crate::stats::squarify(weights, w, h)
        .into_iter()
        .map(|rect| {
            let rank = rank_of.get(rect.name.as_str()).copied().unwrap_or(0) as f32;
            let top = rank == 0.0 && rect.depth == 0;
            let shade = if lone {
                LONE_TINT
            } else if top {
                1.0
            } else {
                (0.8 - rank * 0.07 - rect.depth as f32 * 0.12).clamp(0.18, 0.8)
            };
            crate::MapCell {
                x: rect.x + GUTTER,
                y: rect.y + GUTTER,
                w: (rect.w - 2.0 * GUTTER).max(0.0),
                h: (rect.h - 2.0 * GUTTER).max(0.0),
                detail: human(bytes.get(rect.name.as_str()).copied().unwrap_or(0)).into(),
                name: rect.name.into(),
                shade,
                outline: lone,
            }
        })
        .collect()
}

fn side_abandoned(_ui: &AppWindow, st: &Shared, gen: u64) {
    let rerun = {
        let mut s = st.borrow_mut();
        s.side_gate.abandon(gen) == crate::sidebar::SideLanding::Rerun
    };
    if rerun {
        start_side(&mut st.borrow_mut());
    }
}

/// The breadcrumb, rooted at a device's label when the folder is on one.
fn draw_crumbs(ui: &AppWindow, s: &State) -> Vec<String> {
    let user = std::env::var("USER").ok();
    let home = s.ctl.env.home.as_deref();
    if s.ctl.crumb_dir() == s.ctl.trash_files() {
        ui.set_crumbs("Trash".into());
        return vec!["Trash".into()];
    }
    let crumbs: Vec<String> = crumbs_for(s.ctl.crumb_dir(), home, &s.side.devices, user.as_deref())
        .iter()
        .map(|c| c.label.clone())
        .collect();
    ui.set_crumbs(crumbs.join("  ›  ").into());
    crumbs
}

/// The breadcrumb parts. Removable devices come from the side job, but a
/// desktop automount is known from its path alone, so its crumb starts at
/// the device name from the first frame instead of `/ › run › media`.
fn crumbs_for(
    dir: &Path,
    home: Option<&Path>,
    side: &[crate::sidebar::SideEntry],
    user: Option<&str>,
) -> Vec<crate::model::Crumb> {
    let mut devices: Vec<(PathBuf, String)> = side
        .iter()
        .filter(|d| d.removable && !home.is_some_and(|h| h.starts_with(&d.path)))
        .map(|d| (d.path.clone(), d.name.clone()))
        .collect();
    if let Some(auto) = crate::mounts::automount_of(dir, user) {
        if !devices.iter().any(|(p, _)| *p == auto.0) {
            devices.push(auto);
        }
    }
    crate::model::breadcrumbs_on(dir, home, &devices)
}

/// The permission-denied body: who owns the folder and its mode, as
/// `Owned by root (drwx------)`. Blank when the folder can't be stat'ed.
pub fn denied_note(dir: &Path) -> String {
    use std::os::unix::fs::MetadataExt;
    match std::fs::metadata(dir) {
        Ok(m) => format!("Owned by {} ({})", user_name(m.uid()), mode_text(m.mode())),
        Err(_) => String::new(),
    }
}

/// A uid's login name, or the number when it has none.
fn user_name(uid: u32) -> String {
    let mut buf = vec![0 as libc::c_char; 4096];
    let mut pwd: libc::passwd = unsafe { std::mem::zeroed() };
    let mut out: *mut libc::passwd = std::ptr::null_mut();
    // SAFETY: getpwuid_r writes only into pwd and buf, both ours and
    // sized as passed; out is null or points at pwd.
    let rc = unsafe { libc::getpwuid_r(uid, &mut pwd, buf.as_mut_ptr(), buf.len(), &mut out) };
    if rc != 0 || out.is_null() || pwd.pw_name.is_null() {
        return uid.to_string();
    }
    // SAFETY: on success pw_name is a NUL-terminated string inside buf.
    unsafe { std::ffi::CStr::from_ptr(pwd.pw_name) }
        .to_string_lossy()
        .into_owned()
}

/// `ls -l` style mode: `drwx------`, with setuid, setgid and sticky.
pub fn mode_text(mode: u32) -> String {
    let kind = match mode & 0o170_000 {
        0o040_000 => 'd',
        0o120_000 => 'l',
        _ => '-',
    };
    let bit = |m: u32, c: char| if mode & m != 0 { c } else { '-' };
    let special = |x: u32, s: u32, on: char, off: char| match (mode & x != 0, mode & s != 0) {
        (true, true) => on,
        (false, true) => off,
        (true, false) => 'x',
        (false, false) => '-',
    };
    [
        kind,
        bit(0o400, 'r'),
        bit(0o200, 'w'),
        special(0o100, 0o4000, 's', 'S'),
        bit(0o040, 'r'),
        bit(0o020, 'w'),
        special(0o010, 0o2000, 's', 'S'),
        bit(0o004, 'r'),
        bit(0o002, 'w'),
        special(0o001, 0o1000, 't', 'T'),
    ]
    .iter()
    .collect()
}

/// A folder's name for a message, rebuilt from its crumbs: the root as
/// shown (`/`, `~`, a device), then the rest joined with `/`.
fn crumb_name(crumbs: &[String]) -> String {
    let Some((root, rest)) = crumbs.split_first() else {
        return String::new();
    };
    if rest.is_empty() {
        return root.clone();
    }
    let sep = if root.ends_with('/') { "" } else { "/" };
    format!("{root}{sep}{}", rest.join("/"))
}

fn side_landed(ui: &AppWindow, st: &Shared, gen: u64, data: crate::sidebar::SideData) {
    use crate::sidebar::SideLanding;
    let mut s = st.borrow_mut();
    match s.side_gate.land(gen) {
        SideLanding::Apply => {}
        SideLanding::Drop => return,
        SideLanding::Rerun => {
            start_side(&mut s);
            return;
        }
    }
    s.side = data;
    draw_side(ui, &s);
    draw_crumbs(ui, &s);
}

/// The sidebar sections and status-right from the last side job, with
/// the open folder's row lit.
fn draw_side(ui: &AppWindow, s: &State) {
    let side = &s.side;
    let lit = crate::sidebar::picked(side, &s.ctl.dir);
    let rows = |section: usize, entries: &[crate::sidebar::SideEntry]| {
        let places: Vec<crate::Place> = entries
            .iter()
            .enumerate()
            .map(|(i, e)| crate::Place {
                glyph: e.glyph.into(),
                name: e.name.clone().into(),
                count: e.count.clone().into(),
                selected: lit == Some((section, i)),
                usage: e.usage.unwrap_or(-1.0),
                level: e.usage.map(crate::sidebar::level).unwrap_or(0),
                section: section as i32,
            })
            .collect();
        ModelRc::new(Rc::new(VecModel::from(places)))
    };
    ui.set_pinned_places(rows(0, &side.pinned));
    ui.set_device_places(rows(1, &side.devices));
    ui.set_trash_places(rows(2, &side.trash));
    ui.set_status_right(side.space.clone().into());
    ui.set_status_free_short(short_free(&side.space).into());
}

/// Start a navigation to `target` at `t0` and hold its read, then move the
/// clock past `NAV_PENDING_AFTER` so the pending state shows. `phase`
/// freezes the progress bar's sweep for a still frame.
pub fn navigate_pending(target: PathBuf, t0: std::time::Instant, phase: f32) {
    with_state(|ui, st| {
        let ticket = st.borrow_mut().ctl.begin_navigate_at(target, t0);
        sync_drop_dest(&st.borrow());
        let gen = ticket.gen();
        start_read(Some(ticket));
        st.borrow_mut().ctl.tick_nav(gen, t0 + NAV_PENDING_AFTER);
        redraw(&ui, &st.borrow());
        ui.set_nav_phase(phase);
    });
}

/// What a window starts with: the folder, where trash and home are, the
/// look, and an optional frozen clock for renders.
pub struct Launch {
    pub dir: PathBuf,
    pub env: Env,
    pub theme: crate::theme::Theme,
    pub compact: bool,
    pub clock: Option<(i64, i64)>,
    pub mounts: std::sync::Arc<dyn crate::mounts::MountSource>,
    /// Where the view settings are remembered; None never reads or
    /// writes them (renders, tests).
    pub prefs: Option<PathBuf>,
}

impl Launch {
    /// The interactive app: the system's home and trash, cyan, comfortable.
    pub fn system(dir: PathBuf) -> Self {
        let env = Env::from_system();
        Self {
            prefs: crate::prefs::ViewPrefs::path(env.config_dir()),
            dir,
            env,
            theme: crate::theme::Theme::cyan(),
            compact: false,
            clock: None,
            mounts: std::sync::Arc::new(crate::mounts::SystemMounts),
        }
    }
}

pub fn run(dir: PathBuf) -> Result<i32> {
    let ui = build(Launch::system(dir))?;
    ui.show().map_err(|e| Error::Message(e.to_string()))?;
    slint::run_event_loop().map_err(|e| Error::Message(e.to_string()))?;
    Ok(0)
}

pub fn apply_theme(ui: &AppWindow, theme: &crate::theme::Theme) {
    ui.set_bg(theme.bg.to_slint());
    ui.set_surface(theme.surface.to_slint());
    ui.set_text(theme.text.to_slint());
    ui.set_muted(theme.muted.to_slint());
    ui.set_accent(theme.accent.to_slint());
    ui.set_on_accent(theme.on_accent.to_slint());
    ui.set_line(theme.line.to_slint());
    ui.set_danger(theme.danger.to_slint());
    ui.set_on_danger(theme.on_danger.to_slint());
    ui.set_warning(theme.warning.to_slint());
    ui.set_on_warning(theme.on_warning.to_slint());
}

/// Make the window and wire every callback, with the startup read
/// already requested, but don't show it or run the event loop: `run`
/// does that interactively and the render harness drives it by script.
pub fn build(launch: Launch) -> Result<AppWindow> {
    let ui = AppWindow::new().map_err(|e| Error::Message(e.to_string()))?;
    apply_theme(&ui, &launch.theme);
    ui.set_harness(false);
    ui.set_compact(launch.compact);
    let mut ctl = Controller::new(launch.dir, launch.env);
    ctl.compact = launch.compact;
    let saved = launch.prefs.as_deref().map(crate::prefs::ViewPrefs::load);
    if let Some(p) = saved {
        ctl.apply_prefs(p);
    }
    ctl.clock = launch.clock;
    ctl.mounts = launch.mounts;
    ctl.loaded = false;
    let startup = ctl.begin_refresh();
    let state: Shared = Rc::new(RefCell::new(State {
        ctl,
        queue: VecDeque::new(),
        cancel: None,
        undo: Vec::new(),
        reply: None,
        toast_open: false,
        toast_gen: 0,
        toast_all: Vec::new(),
        toast_pinned: false,
        arm_timer: slint::Timer::default(),
        chrome_timer: slint::Timer::default(),
        watch: None,
        fallback: None,
        debounce: debounce(),
        watch_timer: slint::Timer::default(),
        read_error: None,
        dropped: Rc::default(),
        drop_dest: Rc::default(),
        drop_wait: None,
        drop_wait_toast_gen: 0,
        side: Default::default(),
        side_gate: Default::default(),
        stats: None,
        stats_gen: 0,
        stats_at: 0,
        stats_scan: None,
        stats_tick: None,
        probe: crate::sidebar::SpaceProbe::new(std::time::Duration::from_millis(400)),
        prefs_saved: std::cell::Cell::new(saved.unwrap_or_else(crate::prefs::ViewPrefs::fresh)),
        prefs_path: launch.prefs,
        pics: RefCell::default(),
        rows_fill: RefCell::default(),
        files_model: RefCell::new(None),
        labels_model: RefCell::new(None),
        marks_model: RefCell::new(None),
        grid_model: RefCell::new(None),
        row_fill_queued: std::cell::Cell::new(false),
    }));
    STATE.with(|s| *s.borrow_mut() = Some((state.clone(), ui.as_weak())));
    redraw(&ui, &state.borrow());
    start_read(startup);
    start_side(&mut state.borrow_mut());
    {
        // Inbound drops from other apps. winit reports one DroppedFile per
        // path (X11 only in winit 0.30) and no pointer position, so they
        // land in the open folder.
        use slint::winit_030::{winit::event::WindowEvent, EventResult, WinitWindowAccessor};
        let dropped = state.borrow().dropped.clone();
        let dest = state.borrow().drop_dest.clone();
        ui.window()
            .on_winit_window_event(move |_, event| match event {
                WindowEvent::DroppedFile(path) => {
                    // Freeze the destination now: a navigation before the
                    // batch is flushed must not redirect it. During one,
                    // leave it open until the read lands.
                    dropped
                        .borrow_mut()
                        .push((path.clone(), dest.borrow().clone()));
                    EventResult::PreventDefault
                }
                _ => EventResult::Propagate,
            });
    }
    let w = ui.as_weak();
    let st = state.clone();
    state.borrow().watch_timer.start(
        slint::TimerMode::Repeated,
        Duration::from_millis(100),
        move || {
            if let Some(ui) = w.upgrade() {
                poll_watch(&ui, &st);
            }
        },
    );

    let w = ui.as_weak();
    let st = state.clone();
    ui.on_set_grid(move |grid| {
        let action = st.borrow_mut().ctl.set_grid(grid);
        if let Some(ui) = w.upgrade() {
            apply(&ui, &st, action);
        }
    });
    let w = ui.as_weak();
    let st = state.clone();
    ui.on_search_open(move || {
        let action = st.borrow_mut().ctl.open_search();
        if let Some(ui) = w.upgrade() {
            apply(&ui, &st, action);
        }
    });
    let w = ui.as_weak();
    let st = state.clone();
    ui.on_state_action(move |which| {
        let action = st.borrow_mut().ctl.state_action(&which);
        if let Some(ui) = w.upgrade() {
            apply(&ui, &st, action);
        }
    });
    let w = ui.as_weak();
    let st = state.clone();
    ui.on_search_clear(move || {
        let action = st.borrow_mut().ctl.clear_search();
        if let Some(ui) = w.upgrade() {
            apply(&ui, &st, action);
        }
    });
    let w = ui.as_weak();
    let st = state.clone();
    ui.on_toggle_hidden(move || {
        st.borrow_mut().ctl.toggle_hidden();
        if let Some(ui) = w.upgrade() {
            apply(&ui, &st, Action::Redraw);
        }
    });
    // A resize can change how many tiles fit a grid line.
    let w = ui.as_weak();
    let st = state.clone();
    ui.on_content_resized(move || {
        let Some(ui) = w.upgrade() else { return };
        if st.try_borrow().is_ok_and(|s| s.ctl.viewer.is_some()) {
            let size = ui.window().size().to_logical(ui.window().scale_factor());
            let action = st
                .borrow_mut()
                .ctl
                .viewer_resized((size.width, size.height));
            apply(&ui, &st, action);
        }
        let Ok(s) = st.try_borrow() else { return };
        // Grid columns and the list's middle-cut names both follow width;
        // rebuild only when one of them changed.
        let width = ui.get_content_w();
        ui.global::<Columns>().set_row_w(width);
        let changed = if s.ctl.grid {
            s.ctl.fit(width)
        } else {
            s.ctl.refit_list(width)
        };
        if changed {
            redraw(&ui, &s);
        }
    });
    let w = ui.as_weak();
    let st = state.clone();
    let (w_vc, st_vc) = (ui.as_weak(), state.clone());
    let (w_vg, st_vg) = (ui.as_weak(), state.clone());
    let (w_vk, st_vk) = (ui.as_weak(), state.clone());
    ui.on_view_close(move || {
        st_vc.borrow_mut().ctl.close_viewer();
        if let Some(ui) = w_vc.upgrade() {
            apply(&ui, &st_vc, Action::Redraw);
        }
    });
    ui.on_view_go(move |tile| {
        let action = st_vg.borrow_mut().ctl.viewer_go(tile.max(0) as usize);
        if let Some(ui) = w_vg.upgrade() {
            apply(&ui, &st_vg, action);
        }
    });
    ui.on_view_key(move |text| {
        let action = st_vk.borrow_mut().ctl.key(&text, false, false);
        if let Some(ui) = w_vk.upgrade() {
            apply(&ui, &st_vk, action);
        }
    });
    let (w_ve, st_ve) = (ui.as_weak(), state.clone());
    let (w_vp, st_vp) = (ui.as_weak(), state.clone());
    let (w_vw, st_vw) = (ui.as_weak(), state.clone());
    ui.on_view_pan(move |dx, dy| {
        let action = st_vp.borrow_mut().ctl.viewer_pan(dx, dy);
        if let Some(ui) = w_vp.upgrade() {
            apply(&ui, &st_vp, action);
        }
    });
    ui.on_view_wake(move || {
        let action = st_vw.borrow_mut().ctl.viewer_wake();
        if let Some(ui) = w_vw.upgrade() {
            if action != Action::None {
                apply(&ui, &st_vw, action);
            }
            // Each move restarts the idle clock.
            let (w, st) = (ui.as_weak(), st_vw.clone());
            st_vw.borrow().chrome_timer.start(
                slint::TimerMode::SingleShot,
                Duration::from_millis(2500),
                move || {
                    let action = st.borrow_mut().ctl.viewer_idle();
                    if let Some(ui) = w.upgrade() {
                        apply(&ui, &st, action);
                    }
                },
            );
        }
    });
    ui.on_view_external(move || {
        let action = st_ve.borrow().ctl.viewer_external();
        if let Some(ui) = w_ve.upgrade() {
            apply(&ui, &st_ve, action);
        }
    });
    ui.on_key(move |text, ctrl, shift, _alt| {
        let action = st.borrow_mut().ctl.key(&text, ctrl, shift);
        let handled = action != Action::None;
        if let Some(ui) = w.upgrade() {
            apply(&ui, &st, action);
        }
        handled
    });
    let w = ui.as_weak();
    let st = state.clone();
    ui.on_tile_pointer(move |first, col, kind, dx, dy, far, ctrl, shift| {
        let action = {
            let mut s = st.borrow_mut();
            let columns = s.ctl.columns.get();
            let offset = tile_offset(col.max(0) as usize, dx, dy, columns);
            let index = (first + col).max(0) as usize;
            s.ctl
                .pointer(index, kind, offset, far, Mods { ctrl, shift })
        };
        if let Some(ui) = w.upgrade() {
            apply(&ui, &st, action);
        }
    });
    let w = ui.as_weak();
    let st = state.clone();
    ui.on_row_pointer(move |index, kind, dy, far, ctrl, shift| {
        let action =
            st.borrow_mut()
                .ctl
                .pointer(index.max(0) as usize, kind, dy, far, Mods { ctrl, shift });
        if let Some(ui) = w.upgrade() {
            apply(&ui, &st, action);
        }
    });
    let (w_so, st_so) = (ui.as_weak(), state.clone());
    ui.on_stats_open(move || {
        if let Some(ui) = w_so.upgrade() {
            open_stats(&ui, &st_so);
        }
    });
    let (w_sr, st_sr) = (ui.as_weak(), state.clone());
    {
        let st = state.clone();
        ui.on_stats_cancel(move || cancel_stats(&st));
    }
    ui.on_stats_rescan(move || {
        if let Some(ui) = w_sr.upgrade() {
            open_stats(&ui, &st_sr);
        }
    });
    let (w_ct, st_ct) = (ui.as_weak(), state.clone());
    ui.on_stats_clear_thumbs(move || {
        if let Some(ui) = w_ct.upgrade() {
            ask_clear_thumbs(&ui, &st_ct);
        }
    });
    let w = ui.as_weak();
    let st = state.clone();
    ui.on_place_open(move |section, index| {
        let target = {
            let s = st.borrow();
            let rows = match section {
                0 => &s.side.pinned,
                1 => &s.side.devices,
                _ => &s.side.trash,
            };
            rows.get(index.max(0) as usize).map(|e| e.path.clone())
        };
        if let (Some(ui), Some(t)) = (w.upgrade(), target) {
            apply(&ui, &st, Action::Navigate(t));
        }
    });
    let w = ui.as_weak();
    let st = state.clone();
    ui.on_row_open(move |index| {
        let row = st.borrow().ctl.row_name(index.max(0) as usize);
        let action = match row {
            Some((name, true)) => Action::Navigate(st.borrow().ctl.dir.join(name)),
            Some((name, false)) => st.borrow_mut().ctl.open_viewer(&name),
            None => Action::None,
        };
        if let Some(ui) = w.upgrade() {
            apply(&ui, &st, action);
        }
    });
    let w = ui.as_weak();
    let st = state.clone();
    ui.on_trash_restore(move || {
        let action = st.borrow().ctl.restore_selected();
        if let Some(ui) = w.upgrade() {
            apply(&ui, &st, action);
        }
    });
    let w = ui.as_weak();
    let st = state.clone();
    ui.on_trash_empty(move || {
        let action = st.borrow().ctl.empty_trash();
        if let Some(ui) = w.upgrade() {
            apply(&ui, &st, action);
        }
    });
    let w = ui.as_weak();
    let st = state.clone();
    ui.on_sort_by(move |column| {
        let action = st.borrow_mut().ctl.sort_by(column);
        if let Some(ui) = w.upgrade() {
            apply(&ui, &st, action);
        }
    });
    let w = ui.as_weak();
    let st = state.clone();
    ui.on_dialog_button(move |i| {
        let action = st.borrow_mut().ctl.button(i);
        if let Some(ui) = w.upgrade() {
            apply(&ui, &st, action);
        }
    });
    let w = ui.as_weak();
    let st = state.clone();
    ui.on_job_cancel(move || {
        if let Some(ui) = w.upgrade() {
            apply(&ui, &st, Action::CancelJob);
        }
    });
    let w = ui.as_weak();
    let st = state.clone();
    ui.on_toast_expand(move || {
        let Some(ui) = w.upgrade() else { return };
        let s = st.borrow();
        set_toast_rows(&ui, &detail_rows(&s.toast_all, s.toast_pinned, true));
    });
    let w = ui.as_weak();
    let st = state.clone();
    ui.on_toast_link(move || {
        let Some(ui) = w.upgrade() else { return };
        if ui.get_toast_details().row_count() > 0 || !ui.get_toast_pin().is_empty() {
            let open = {
                let mut s = st.borrow_mut();
                s.toast_open = !s.toast_open;
                s.toast_open
            };
            ui.set_toast_open(open);
            return;
        }
        if ui.get_toast_action() == DROP_WAIT_CANCEL {
            cancel_drop_wait(&ui, &st);
            return;
        }
        let items = std::mem::take(&mut st.borrow_mut().undo);
        if items.is_empty() {
            return;
        }
        // Restoring renames on the job thread like any other job, so a
        // slow drive never stalls the window.
        ui.set_toast_on(false);
        apply(&ui, &st, Action::Run(Job::restore(items)));
    });
    Ok(ui)
}

const WATCH_DEBOUNCE_MS: u64 = 200;
/// A folder that never goes quiet still refreshes this often.
const WATCH_MAX_WAIT_MS: u64 = 1500;

fn debounce() -> crate::model::Debounce {
    crate::model::Debounce::new(Duration::from_millis(WATCH_DEBOUNCE_MS))
        .max_wait(Duration::from_millis(WATCH_MAX_WAIT_MS))
}

/// Whether a finished refresh walked the open folder up to an ancestor.
/// Queued drops must be flushed against `open` before [`Controller::land`]
/// applies such a result, or an unresolved drop is grouped into the parent.
fn refresh_walks_up(read: &ReadResult, open: &Path) -> bool {
    read.kind == ReadKind::Refresh && matches!(&read.result, Ok((dir, _)) if dir.as_path() != open)
}

/// Drain queued external drops into copy jobs once nothing is holding
/// them back: no dialog is open and no navigation is in flight. Each path
/// keeps the destination frozen when it arrived; one still open (dropped
/// mid-navigation) goes to the folder that is open now.
fn flush_drops(ui: &AppWindow, st: &Shared) {
    if st.borrow().ctl.dialog.is_some() || st.borrow().ctl.nav_in_flight() {
        return; // keep them queued until the dialog closes or the folder lands
    }
    let dropped = std::mem::take(&mut *st.borrow().dropped.borrow_mut());
    if dropped.is_empty() {
        return;
    }
    let by_dest = group_drops(dropped, &st.borrow().ctl.dir);
    let (mut here, mut itself, mut jobs) = (0, 0, 0);
    for (dest, paths) in by_dest {
        let plan = ops::external_drop(&paths, &dest);
        here += plan.already_here;
        itself += plan.into_itself;
        jobs += plan.jobs.len();
        for job in plan.jobs {
            apply(ui, st, Action::Run(job)); // queued behind a running job
        }
    }
    if let Some((text, details)) = drop_notice(jobs, here, itself) {
        show_toast(ui, st, "", text, details, false, "");
    }
}

/// A path another app dropped, with its frozen destination if any.
pub type DroppedPath = (PathBuf, Option<PathBuf>);

/// Group dropped paths into one batch per destination. A path dropped
/// during a navigation has none yet and goes to `open`: the new folder if
/// the read landed, the old one if it failed.
pub fn group_drops(dropped: Vec<DroppedPath>, open: &Path) -> Vec<(PathBuf, Vec<PathBuf>)> {
    let mut by_dest: Vec<(PathBuf, Vec<PathBuf>)> = Vec::new();
    for (path, dest) in dropped {
        let dest = dest.unwrap_or_else(|| open.to_path_buf());
        match by_dest.iter_mut().find(|(d, _)| *d == dest) {
            Some((_, paths)) => paths.push(path),
            None => by_dest.push((dest, vec![path])),
        }
    }
    by_dest
}

/// The notice after a drop that copied nothing, or that skipped a
/// folder dropped into itself.
pub fn drop_notice(jobs: usize, here: usize, itself: usize) -> Option<(String, Vec<String>)> {
    const SELF: &str = "Can't copy a folder into itself";
    if itself == 0 {
        return (jobs == 0 && here > 0).then(|| {
            (
                "Nothing to copy".into(),
                vec![format!("{} already in this folder", ops::count(here))],
            )
        });
    }
    if jobs == 0 && here == 0 {
        return Some((SELF.into(), Vec::new()));
    }
    if jobs == 0 {
        return Some((
            "Nothing to copy".into(),
            vec![
                format!("{} already in this folder", ops::count(here)),
                SELF.into(),
            ],
        ));
    }
    // Some items are copying; mention the self-drop on its own.
    Some((SELF.into(), Vec::new()))
}

/// The toast's leading glyph: warning, error, info, or a check for a
/// finished action.
pub fn toast_glyph(tone: &str) -> &'static str {
    match tone {
        "warning" => "󰀦",
        "danger" => "󰅙",
        "info" => "󰋽",
        _ => "󰄬",
    }
}

/// Whether a failed watch should toast that live refresh is unavailable.
/// A path that is already gone (the folder was just deleted, or is no
/// longer a directory) walks up to a parent on the next refresh; don't
/// claim it can't be watched.
pub fn watch_warns_on_failure(dir: &Path) -> bool {
    !watch_path_is_gone(dir)
}

/// The open folder is gone or is no longer a directory, matching
/// [`Error::is_gone`]: ENOENT, or ENOTDIR when an ancestor was replaced
/// by a file.
fn watch_path_is_gone(dir: &Path) -> bool {
    match std::fs::symlink_metadata(dir) {
        Err(e) => matches!(e.raw_os_error(), Some(libc::ENOENT) | Some(libc::ENOTDIR)),
        Ok(m) => !m.is_dir(),
    }
}

/// How long a drop may wait on a dialog or an in-flight navigation before
/// the user is told, and before the drop is cancelled.
pub const DROP_WAIT_TOAST: Duration = Duration::from_secs(2);
pub const DROP_WAIT_LIMIT: Duration = Duration::from_secs(15);

/// The action on the hung-drop toast. Clicking it drops the queue so a
/// later flush cannot complete the copy.
pub const DROP_WAIT_CANCEL: &str = "Cancel";

/// 2s notice: typographic ellipsis; `X` is the drop destination.
pub fn drop_wait_toast(name: &str) -> String {
    format!("Waiting to drop into {name}…")
}

/// 15s notice after the drop is cancelled because it never proceeded.
pub fn drop_wait_cancelled(name: &str) -> String {
    format!("Cancelled the drop into {name}")
}

/// The folder name used as `X` in hung-drop toasts: the first frozen
/// drop destination if any, otherwise the in-flight navigation's target
/// (an unresolved mid-navigation drop lands there if the read succeeds),
/// otherwise the open folder.
pub fn drop_wait_name(dropped: &[DroppedPath], open: &Path, nav_target: Option<&Path>) -> String {
    let dest = dropped
        .iter()
        .find_map(|(_, d)| d.as_deref())
        .or(nav_target)
        .unwrap_or(open);
    crate::ops::folder_name(dest)
}

/// A drop queue that cannot be flushed yet, and how long it has waited.
#[derive(Clone, Debug)]
pub struct DropWait {
    since: std::time::Instant,
    toasted: bool,
}

impl DropWait {
    pub fn start(now: std::time::Instant) -> Self {
        Self {
            since: now,
            toasted: false,
        }
    }

    pub fn tick(&mut self, now: std::time::Instant) -> DropWaitTick {
        let waited = now.saturating_duration_since(self.since);
        if waited >= DROP_WAIT_LIMIT {
            DropWaitTick::AutoCancel
        } else if waited >= DROP_WAIT_TOAST && !self.toasted {
            self.toasted = true;
            DropWaitTick::Toast
        } else {
            DropWaitTick::Hold
        }
    }
}

/// What to do for a drop still held at `now`, or once it is no longer held.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DropWaitTick {
    /// Still held, and the 2s toast is not due (or already showing).
    Hold,
    /// The wait ended because the drop flushed or the user cancelled it.
    Clear,
    /// Show "Waiting to drop into X…" with Cancel, and keep it up.
    Toast,
    /// 15s elapsed: drop the queue and tell the user.
    AutoCancel,
}

/// Advance a held-drop wait with a controllable clock. `held` is true
/// while the queue is non-empty and a dialog or navigation is blocking
/// [`flush_drops`].
pub fn tick_held_drop(
    wait: &mut Option<DropWait>,
    held: bool,
    now: std::time::Instant,
) -> DropWaitTick {
    if !held {
        return if wait.take().is_some() {
            DropWaitTick::Clear
        } else {
            DropWaitTick::Hold
        };
    }
    wait.get_or_insert_with(|| DropWait::start(now)).tick(now)
}

fn drops_held(st: &Shared) -> bool {
    let s = st.borrow();
    !s.dropped.borrow().is_empty() && (s.ctl.dialog.is_some() || s.ctl.nav_in_flight())
}

fn drop_wait_name_from(st: &Shared) -> String {
    let s = st.borrow();
    let dropped = s.dropped.borrow();
    drop_wait_name(&dropped, &s.ctl.dir, s.ctl.nav_target())
}

fn tick_drop_wait(ui: &AppWindow, st: &Shared, now: std::time::Instant) {
    let held = drops_held(st);
    let tick = {
        let mut s = st.borrow_mut();
        tick_held_drop(&mut s.drop_wait, held, now)
    };
    match tick {
        DropWaitTick::Hold => {}
        DropWaitTick::Clear => {
            let hide = {
                let s = st.borrow();
                s.drop_wait_toast_gen != 0 && s.toast_gen == s.drop_wait_toast_gen
            };
            st.borrow_mut().drop_wait_toast_gen = 0;
            if hide {
                ui.set_toast_on(false);
                ui.set_toast_action("".into());
            }
        }
        DropWaitTick::Toast => {
            let name = drop_wait_name_from(st);
            show_toast_sticky(
                ui,
                st,
                "info",
                drop_wait_toast(&name),
                Vec::new(),
                false,
                DROP_WAIT_CANCEL,
            );
            let gen = st.borrow().toast_gen;
            st.borrow_mut().drop_wait_toast_gen = gen;
        }
        DropWaitTick::AutoCancel => {
            let name = drop_wait_name_from(st);
            st.borrow().dropped.borrow_mut().clear();
            let mut s = st.borrow_mut();
            s.drop_wait = None;
            s.drop_wait_toast_gen = 0;
            drop(s);
            show_toast(
                ui,
                st,
                "info",
                drop_wait_cancelled(&name),
                Vec::new(),
                false,
                "",
            );
        }
    }
}

/// Stop a hung drop so a later flush cannot complete it. The 2s toast
/// goes away; the 15s cancelled notice is not shown, because the user
/// already chose Cancel.
fn cancel_drop_wait(ui: &AppWindow, st: &Shared) {
    st.borrow().dropped.borrow_mut().clear();
    let mut s = st.borrow_mut();
    s.drop_wait = None;
    s.drop_wait_toast_gen = 0;
    drop(s);
    ui.set_toast_on(false);
    ui.set_toast_action("".into());
}

/// The one-time neutral notice for a folder without a live watch.
pub fn watch_fallback_notice(dir: &Path) -> (String, Vec<String>) {
    (
        "Live refresh is unavailable here".into(),
        vec![format!(
            "{} can't be watched; checking it every {} s instead.",
            dir.display(),
            crate::watch::SLOW_POLL.as_secs()
        )],
    )
}

/// Follow the open folder and reread it, off the UI thread, once outside
/// changes settle, keeping the selection (by name) and the scroll offset.
fn poll_watch(ui: &AppWindow, st: &Shared) {
    flush_drops(ui, st);
    let now = std::time::Instant::now();
    tick_drop_wait(ui, st, now);
    let warn = arm_watch(st, now);
    if let Some(dir) = warn {
        show_toast(
            ui,
            st,
            "info",
            watch_fallback_notice(&dir).0,
            watch_fallback_notice(&dir).1,
            false,
            "",
        );
    }
    let mut s = st.borrow_mut();
    let s = &mut *s;
    if let Some(w) = &s.watch {
        if w.pending() {
            s.debounce.push(now);
        }
    } else if let Some(f) = &mut s.fallback {
        if f.poll_due(now) {
            s.debounce.push(now);
        }
    }
    if s.ctl.idle() && s.debounce.ready(now) {
        start_read(s.ctl.begin_refresh());
    }
}

/// Run a read on a worker thread and land it on the UI thread as soon as
/// it finishes.
fn start_read(ticket: Option<ReadTicket>) {
    let Some(ticket) = ticket else { return };
    if SCRIPTED.with(|s| s.get()) {
        PENDING_READS.with(|p| p.borrow_mut().push_back(ticket));
        return;
    }
    std::thread::spawn(move || {
        let (read, follow) = ticket.perform_listing();
        let _ = slint::invoke_from_event_loop(move || {
            with_state(|ui, st| landed(&ui, st, read));
        });
        if let Some((gen, dir, entries)) = follow {
            if let Some(counts) = count_subfolders(&dir, &entries) {
                let _ = slint::invoke_from_event_loop(move || {
                    with_state(|ui, st| land_counts(&ui, st, gen, dir, counts));
                });
            }
        }
    });
}

/// Apply a finished count pass: keep the listing and scroll, fill in
/// folder sizes that were left blank (or kept from a previous reread).
fn land_counts(
    ui: &AppWindow,
    st: &Shared,
    gen: u64,
    dir: PathBuf,
    counts: std::collections::HashMap<OsString, usize>,
) {
    let mut s = st.borrow_mut();
    if !s.ctl.apply_counts(gen, &dir, counts) {
        return;
    }
    let (list_y, grid_y) = (ui.get_list_scroll(), ui.get_grid_scroll());
    redraw(ui, &s);
    ui.set_list_scroll(list_y);
    ui.set_grid_scroll(grid_y);
}

/// Show a finished read: a navigation or fallback starts at the top, a
/// reread keeps the scroll offset; errors keep the folder and toast.
fn landed(ui: &AppWindow, st: &Shared, read: ReadResult) {
    let kind = read.kind;
    // A refresh that walked up because the open folder is gone must flush
    // queued drops against that folder first. Otherwise a drop still open
    // from a mid-navigation wait resolves into the parent once ctl.dir
    // moves, and a frozen dest aimed at the vanished folder sits until a
    // later tick. Flushing now starts the copies at their intended target
    // (they fail cleanly if it is already gone) instead of misdirecting
    // them into the ancestor.
    if refresh_walks_up(&read, &st.borrow().ctl.dir) {
        flush_drops(ui, st);
    }
    let mut s = st.borrow_mut();
    let landed = s.ctl.land(read);
    sync_drop_dest(&s);
    match landed {
        Landed::Stale | Landed::Unchanged => {}
        Landed::Shown { moved } => {
            s.read_error = None;
            start_side(&mut s);
            let (list_y, grid_y) = (ui.get_list_scroll(), ui.get_grid_scroll());
            redraw(ui, &s);
            if moved {
                ui.set_list_scroll(0.0);
                ui.set_grid_scroll(0.0);
            } else {
                ui.set_list_scroll(list_y);
                ui.set_grid_scroll(grid_y);
            }
        }
        Landed::Failed(e) => {
            let text = e.to_string();
            redraw(ui, &s);
            if kind == ReadKind::Refresh {
                if s.read_error.as_deref() == Some(text.as_str()) {
                    return;
                }
                s.read_error = Some(text.clone());
            }
            if s.ctl.take_refresh_due() {
                start_read(s.ctl.begin_refresh());
            }
            if !s.ctl.toasts_failure(kind) {
                return;
            }
            drop(s);
            show_toast(ui, st, "danger", text, Vec::new(), false, "");
        }
    }
}

/// Keep a watch on the open folder: re-arm after navigation or when the
/// folder was replaced, back off when it can't be watched. Returns the
/// folder to warn about the first time its watch fails.
fn arm_watch(st: &Shared, now: std::time::Instant) -> Option<PathBuf> {
    let mut s = st.borrow_mut();
    let s = &mut *s;
    let dir = s.ctl.dir.clone();
    if s.watch
        .as_ref()
        .is_some_and(|w| w.dir() == dir && !w.stale())
    {
        return None;
    }
    if s.watch
        .as_ref()
        .is_some_and(|w| w.dir() == dir && w.stale())
    {
        // Deleted, renamed or replaced: reread whatever is there now.
        s.debounce.push(now);
    } else if s.fallback.as_ref().map(|f| f.dir()) != Some(dir.as_path()) {
        s.debounce = debounce();
        s.fallback = None;
        s.read_error = None;
    }
    s.watch = None;
    if s.fallback.as_ref().is_some_and(|f| !f.may_retry(now)) {
        return None;
    }
    match crate::watch::Watch::new(&dir) {
        Ok(w) => {
            s.watch = Some(w);
            s.fallback = None;
            None
        }
        Err(_) => {
            if watch_path_is_gone(&dir) {
                // Just deleted or replaced: the refresh walks up (or
                // rereads whatever is here now). Don't claim live refresh
                // is unavailable.
                s.debounce.push(now);
                return None;
            }
            let f = match &mut s.fallback {
                Some(f) => {
                    f.failed_again(now);
                    f
                }
                None => s.fallback.insert(crate::watch::Fallback::new(&dir, now)),
            };
            f.take_warning().then_some(dir)
        }
    }
}

/// Point inbound drops at the open folder, or at nothing yet while a
/// navigation is in flight.
fn sync_drop_dest(s: &State) {
    let want = (!s.ctl.nav_in_flight()).then(|| s.ctl.dir.clone());
    if *s.drop_dest.borrow() != want {
        *s.drop_dest.borrow_mut() = want;
    }
}

/// Write the view settings when one changed. A failed write only
/// means they aren't remembered next launch.
fn remember_prefs(s: &State) {
    let Some(path) = &s.prefs_path else { return };
    let now = s.ctl.prefs();
    if now != s.prefs_saved.get() {
        let _ = now.save(path);
        s.prefs_saved.set(now);
    }
}

/// Filmstrip tiles shown at once (DESIGN §5).
pub const STRIP_TILES: usize = 11;

/// A decoded picture, or why there is none.
type Decoded = std::result::Result<Arc<image::RgbaImage>, crate::picture::Why>;

/// Pixels ready for Slint, made on a worker: Slint images can't cross
/// threads, but their buffers can.
type Px = slint::SharedPixelBuffer<slint::Rgba8Pixel>;

/// Longest side of the pixels handed to Slint for the open picture.
/// Bigger pictures are shrunk on the worker; zoom still uses their real
/// size, so past this they are drawn upscaled.
const SHOWN_SIDE: u32 = 4096;

/// Filmstrip tiles kept decoded; older ones are dropped past this.
const TILE_CACHE: usize = 64;
/// Longest side of a filmstrip tile's pixels.
const TILE_SIDE: u32 = 144;

/// A tile: its blurred stand-in and it as shown, or `None` when it has
/// no picture (videos without a cached frame).
type Tile = Option<(slint::Image, slint::Image)>;

#[derive(Default)]
struct Pics {
    /// Bumped whenever the viewer closes: work started before lands stale.
    gen: u64,
    /// A turned copy being made: (path, turns).
    turning: Option<(PathBuf, u8)>,
    /// The last picture decoded; kept on screen while the next loads.
    full: Option<(PathBuf, Decoded)>,
    /// It as shown, turned: (path, turns, image).
    shown: Option<(PathBuf, u8, slint::Image)>,
    /// The picture asked for, while it decodes.
    want: Option<PathBuf>,
    decodes: crate::picture::Decodes<PathBuf>,
    tiles: std::collections::HashMap<PathBuf, Tile>,
    /// Tile paths, oldest first, for evicting.
    tile_order: VecDeque<PathBuf>,
    /// Tiles being made now (one batch at a time).
    tiles_pending: std::collections::HashSet<PathBuf>,
    spinner: Option<slint::Timer>,
}

impl Pics {
    /// The viewer closed: drop every picture, and make whatever is still
    /// decoding land stale instead of starting what waited behind it.
    fn close(&mut self) {
        let mut decodes = std::mem::take(&mut self.decodes);
        decodes.cancel();
        *self = Pics {
            gen: self.gen + 1,
            decodes,
            ..Pics::default()
        };
    }
}

/// `img` turned `turns` quarter turns clockwise and shrunk to fit
/// `max`×`max`, as pixels for Slint. Runs on a worker.
fn prepare(img: &image::RgbaImage, turns: u8, max: u32) -> Px {
    let (w, h) = img.dimensions();
    let shrunk;
    let img = if w > max || h > max {
        shrunk = image::DynamicImage::ImageRgba8(img.clone())
            .resize(max, max, image::imageops::FilterType::Triangle)
            .into_rgba8();
        &shrunk
    } else {
        img
    };
    let turned = match turns % 4 {
        0 => None,
        1 => Some(image::imageops::rotate90(img)),
        2 => Some(image::imageops::rotate180(img)),
        _ => Some(image::imageops::rotate270(img)),
    };
    let img = turned.as_ref().unwrap_or(img);
    let (w, h) = img.dimensions();
    Px::clone_from_slice(img.as_raw(), w, h)
}

/// Work for a decode thread.
enum DecodeJob {
    Full {
        gen: u64,
        ticket: u64,
        path: PathBuf,
    },
    Turn {
        gen: u64,
        path: PathBuf,
        img: Arc<image::RgbaImage>,
        turns: u8,
    },
    Tiles {
        gen: u64,
        paths: Vec<PathBuf>,
        cache: Option<PathBuf>,
    },
}

/// A finished [`DecodeJob`], sent back to the UI thread. Plain pixels:
/// Slint images are made on the UI thread.
enum DecodeDone {
    Full {
        gen: u64,
        ticket: u64,
        path: PathBuf,
        pic: std::result::Result<(Arc<image::RgbaImage>, Px), crate::picture::Why>,
    },
    Turn {
        gen: u64,
        path: PathBuf,
        turns: u8,
        px: Px,
    },
    /// Per path: the tile and its blurred stand-in.
    Tiles(u64, Vec<(PathBuf, Option<(Px, Px)>)>),
}

impl DecodeJob {
    fn perform(self) -> DecodeDone {
        match self {
            DecodeJob::Full { gen, ticket, path } => {
                let pic = crate::picture::full(&path).map(|img| {
                    let px = prepare(&img, 0, SHOWN_SIDE);
                    (Arc::new(img), px)
                });
                DecodeDone::Full {
                    gen,
                    ticket,
                    path,
                    pic,
                }
            }
            DecodeJob::Turn {
                gen,
                path,
                img,
                turns,
            } => DecodeDone::Turn {
                gen,
                px: prepare(&img, turns, SHOWN_SIDE),
                path,
                turns,
            },
            DecodeJob::Tiles { gen, paths, cache } => DecodeDone::Tiles(
                gen,
                paths
                    .into_iter()
                    .map(|p| {
                        let t = crate::picture::tile(&p, TILE_SIDE, cache.as_deref()).ok();
                        let t = t.map(|t| {
                            let blurred = image::imageops::blur(&t, 4.0);
                            (prepare(&t, 0, TILE_SIDE), prepare(&blurred, 0, TILE_SIDE))
                        });
                        (p, t)
                    })
                    .collect(),
            ),
        }
    }
}

/// Run a decode on a worker thread (or hold it for the render harness).
fn start_decode(job: DecodeJob) {
    if SCRIPTED.with(|s| s.get()) {
        PENDING_DECODES.with(|p| p.borrow_mut().push_back(job));
        return;
    }
    std::thread::spawn(move || {
        let done = job.perform();
        let _ = slint::invoke_from_event_loop(move || {
            with_state(|ui, st| decode_landed(&ui, st, done));
        });
    });
}

/// A decode finished: keep it if it is still wanted, start what waited.
fn decode_landed(ui: &AppWindow, st: &Shared, done: DecodeDone) {
    let s = st.borrow();
    {
        let mut pics = s.pics.borrow_mut();
        let current = pics.gen;
        match done {
            DecodeDone::Full {
                gen,
                ticket,
                path,
                pic,
            } => {
                // Always counted off, so a close can't leave a slot taken.
                let (wanted, next) = pics.decodes.finish(ticket);
                if let Some((ticket, path)) = next {
                    start_decode(DecodeJob::Full {
                        gen: current,
                        ticket,
                        path,
                    });
                }
                // Started before a close: nothing waits for it now.
                if gen != current || !wanted || pics.want.as_ref() != Some(&path) {
                    return;
                }
                pics.want = None;
                pics.spinner = None;
                pics.turning = None;
                pics.shown = None;
                let pic = pic.map(|(img, px)| {
                    pics.shown = Some((path.clone(), 0, slint::Image::from_rgba8(px)));
                    img
                });
                pics.full = Some((path, pic));
            }
            DecodeDone::Turn {
                gen,
                path,
                turns,
                px,
            } => {
                if gen != current || pics.turning.as_ref() != Some(&(path.clone(), turns)) {
                    return;
                }
                pics.turning = None;
                if pics.full.as_ref().map(|f| &f.0) == Some(&path) {
                    pics.shown = Some((path, turns, slint::Image::from_rgba8(px)));
                }
            }
            DecodeDone::Tiles(gen, done) => {
                if gen != current {
                    return;
                }
                for (path, pic) in done {
                    pics.tiles_pending.remove(&path);
                    let tile = pic.map(|(shown, blurred)| {
                        (
                            slint::Image::from_rgba8(blurred),
                            slint::Image::from_rgba8(shown),
                        )
                    });
                    if pics.tiles.insert(path.clone(), tile).is_none() {
                        pics.tile_order.push_back(path);
                    }
                }
                while pics.tiles.len() > TILE_CACHE {
                    let Some(old) = pics.tile_order.pop_front() else {
                        break;
                    };
                    pics.tiles.remove(&old);
                }
            }
        }
    }
    sync_viewer(ui, &s);
}

/// Start a program detached from Robin: no shared stdio, and a thread
/// reaps it so it never lingers as a zombie. Renders and tests record
/// the argv instead of running anything.
fn launch(ui: &AppWindow, st: &Shared, argv: Vec<OsString>) {
    if SCRIPTED.with(|s| s.get()) {
        LAUNCHED.with(|l| l.borrow_mut().push(argv));
        return;
    }
    use std::process::{Command, Stdio};
    let Some((program, args)) = argv.split_first() else {
        return;
    };
    let mut cmd = Command::new(program);
    cmd.args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    match own_session(&mut cmd).spawn() {
        Ok(mut child) => {
            std::thread::spawn(move || child.wait());
        }
        Err(e) => {
            let text = format!("Couldn't start {}: {e}", program.to_string_lossy());
            show_toast(ui, st, "", text, Vec::new(), false, "");
        }
    }
}

/// Run `cmd` in a session of its own, so closing Robin's terminal (or
/// Robin) never sends the player a hangup.
fn own_session(cmd: &mut std::process::Command) -> &mut std::process::Command {
    use std::os::unix::process::CommandExt;
    // SAFETY: setsid is async-signal-safe and touches no Rust state.
    unsafe {
        cmd.pre_exec(|| {
            if libc::setsid() == -1 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        })
    }
}

/// What the render harness was asked to launch, oldest first.
pub fn launched() -> Vec<Vec<OsString>> {
    LAUNCHED.with(|l| l.borrow().clone())
}

/// `path` with the home folder written as `~`.
pub fn tilde(path: &Path, home: Option<&Path>) -> String {
    match home.and_then(|h| path.strip_prefix(h).ok()) {
        Some(rest) if rest.as_os_str().is_empty() => "~".into(),
        Some(rest) => format!("~/{}", rest.display()),
        None => path.display().to_string(),
    }
}

/// The FILE section of the info panel.
pub fn file_facts(
    name: &str,
    len: u64,
    dims: Option<(u32, u32)>,
    modified: String,
) -> Vec<(String, String)> {
    let ext = Path::new(name)
        .extension()
        .map(|e| e.to_string_lossy().into_owned())
        .unwrap_or_default();
    let mut facts = vec![
        ("Size".into(), human(len)),
        ("Type".into(), kind_name(&ext)),
    ];
    if let Some((w, h)) = dims {
        facts.push(("Dimensions".into(), format!("{w} × {h}")));
    }
    facts.push(("Modified".into(), modified));
    facts
}

fn sync_viewer(ui: &AppWindow, s: &State) {
    let ctl = &s.ctl;
    let Some(v) = ctl.viewer.as_ref() else {
        ui.set_view_on(false);
        let mut pics = s.pics.borrow_mut();
        if pics.full.is_some() || pics.want.is_some() || !pics.tiles.is_empty() {
            pics.close();
        }
        return;
    };
    let path = ctl.dir.join(v.current());
    let mut pics = s.pics.borrow_mut();
    let ready = pics.full.as_ref().map(|f| &f.0) == Some(&path);
    if !ready && pics.want.as_ref() != Some(&path) {
        pics.want = Some(path.clone());
        if let Some((ticket, path)) = pics.decodes.request(path.clone()) {
            start_decode(DecodeJob::Full {
                gen: pics.gen,
                ticket,
                path,
            });
        }
        // A spinner only for a slow decode.
        let w = ui.as_weak();
        let timer = slint::Timer::default();
        timer.start(
            slint::TimerMode::SingleShot,
            Duration::from_millis(300),
            move || {
                if let Some(ui) = w.upgrade() {
                    ui.set_view_spinner(true);
                }
            },
        );
        pics.spinner = Some(timer);
    }
    if pics.want.is_none() {
        ui.set_view_spinner(false);
    }
    // Until the new picture lands, the last one stays on screen; on a
    // first open, a blurred tile (or a plain box) stands in.
    let loading_first = !ready && pics.full.is_none();
    let placeholder = if loading_first {
        pics.tiles
            .get(&path)
            .and_then(|t| t.as_ref())
            .map(|(blurred, _)| blurred.clone())
            .unwrap_or_default()
    } else {
        slint::Image::default()
    };
    ui.set_view_loading(loading_first);
    ui.set_view_placeholder(placeholder);
    let want_turns = if ready { v.turns } else { 0 };
    let pic = pics.full.as_ref().map(|f| f.1.clone());
    if let (Some((full_path, _)), Some(Ok(img))) = (pics.full.as_ref(), &pic) {
        let want = (full_path.clone(), want_turns);
        let have = pics.shown.as_ref().map(|s| (s.0.clone(), s.1));
        if have.as_ref() != Some(&want) && pics.turning.as_ref() != Some(&want) {
            // Turned off the UI thread; the old turn stays until it lands.
            pics.turning = Some(want.clone());
            start_decode(DecodeJob::Turn {
                gen: pics.gen,
                path: want.0,
                img: img.clone(),
                turns: want_turns,
            });
        }
    }
    // Sizes follow what is on screen, so a pending turn never stretches.
    let turns = pics.shown.as_ref().map_or(0, |s| s.1);
    // A picture still loading shows nothing for zoom and facts yet.
    let pic = match pic {
        Some(p) if ready || p.is_ok() => p,
        _ => Err(crate::picture::Why::Unreadable),
    };
    let quiet = !ready;
    let size = ui.window().size().to_logical(ui.window().scale_factor());
    let view_w = size.width - if v.info { 332.0 } else { 0.0 };
    let view = (view_w.max(1.0), size.height.max(1.0));
    let name = v.current().to_string_lossy().into_owned();
    ui.set_view_on(true);
    ui.set_view_name(name.clone().into());
    ui.set_view_counter(v.counter().into());
    ui.set_view_info(v.info);
    ui.set_view_strip(v.strip);
    ui.set_view_video(crate::viewer::media(&path) == Some(crate::viewer::Media::Video));
    ui.set_view_chrome(!v.chrome_hidden);
    ui.set_view_pan_x(v.pan.0);
    ui.set_view_pan_y(v.pan.1);
    let dims = match &pic {
        Ok(img) => {
            let (w, h) = img.dimensions();
            ctl.view_geom.set(((w as f32, h as f32), view));
            let (dw, dh) = if turns % 2 == 1 { (h, w) } else { (w, h) };
            ui.set_view_image(pics.shown.as_ref().map(|s| s.2.clone()).unwrap_or_default());
            ui.set_view_px_w(dw as f32);
            ui.set_view_px_h(dh as f32);
            // Fit is left to the UI, so it follows window resizes.
            ui.set_view_scale(match v.zoom {
                crate::viewer::Zoom::Fit => 0.0,
                crate::viewer::Zoom::Scale(z) => z,
            });
            ui.set_view_note(SharedString::new());
            ui.set_view_zoom(match v.zoom {
                crate::viewer::Zoom::Fit => SharedString::new(),
                crate::viewer::Zoom::Scale(z) => crate::viewer::percent(z).into(),
            });
            Some((w, h))
        }
        Err(why) => {
            ctl.view_geom.set(((0.0, 0.0), view));
            ui.set_view_image(slint::Image::default());
            ui.set_view_px_w(0.0);
            ui.set_view_px_h(0.0);
            ui.set_view_note(if quiet {
                SharedString::new()
            } else {
                why.line().into()
            });
            ui.set_view_zoom(SharedString::new());
            None
        }
    };
    let window = v.strip_window(STRIP_TILES);
    let mut tiles = Vec::new();
    let mut missing = Vec::new();
    for i in window {
        let p = ctl.dir.join(&v.items[i]);
        let shot = match pics.tiles.get(&p) {
            Some(t) => t.as_ref().map(|t| t.1.clone()).unwrap_or_default(),
            None => {
                if !pics.tiles_pending.contains(&p) {
                    missing.push(p.clone());
                }
                slint::Image::default()
            }
        };
        tiles.push(crate::ViewTile {
            shot,
            current: i == v.index,
            video: crate::viewer::media(&p) == Some(crate::viewer::Media::Video),
        });
    }
    // One batch of tiles at a time, so scrubbing can't pile them up.
    if pics.tiles_pending.is_empty() && !missing.is_empty() {
        pics.tiles_pending.extend(missing.iter().cloned());
        start_decode(DecodeJob::Tiles {
            gen: pics.gen,
            paths: missing,
            cache: ctl.env.cache_dir(),
        });
    }
    ui.set_view_tiles(Rc::new(VecModel::from(tiles)).into());
    let entry = ctl
        .listing
        .entries()
        .iter()
        .find(|e| &e.name == v.current());
    let now = ctl
        .clock
        .map_or_else(|| secs(std::time::SystemTime::now()), |c| c.0);
    let modified = entry.map_or_else(String::new, |e| {
        let off = |t| {
            ctl.clock
                .map_or_else(|| crate::model::local_offset(t), |c| c.1)
        };
        crate::model::format_modified(secs(e.mtime), now, off)
    });
    let facts: Vec<crate::Fact> = file_facts(&name, entry.map_or(0, |e| e.len), dims, modified)
        .into_iter()
        .map(|(key, value)| crate::Fact {
            key: key.into(),
            value: value.into(),
        })
        .collect();
    ui.set_view_facts(Rc::new(VecModel::from(facts)).into());
    ui.set_view_dir(tilde(ctl.crumb_dir(), ctl.env.home.as_deref()).into());
}

/// How many rows to build on the UI thread before yielding. Scripted
/// renders and tests fill the whole list in one go so a screenshot is
/// complete and scroll tests stay deterministic.
const ROW_CHUNK: usize = 1024;

#[derive(Default)]
struct RowsFill {
    key: u64,
    dir: PathBuf,
    filled: usize,
    total: usize,
    rows: Vec<Row>,
    labels: Vec<SharedString>,
    marks: Vec<Mark>,
}

fn row_chunk_limit() -> usize {
    if SCRIPTED.with(|s| s.get()) {
        usize::MAX
    } else {
        ROW_CHUNK
    }
}

/// A fingerprint of everything [`Controller::rows`] and the name labels
/// depend on, so a toast or sidebar land can reuse a finished list.
fn rows_key(ctl: &Controller, budget: usize) -> u64 {
    let mut h = DefaultHasher::new();
    ctl.dir.hash(&mut h);
    ctl.grid.hash(&mut h);
    ctl.compact.hash(&mut h);
    ctl.tile_step.hash(&mut h);
    budget.hash(&mut h);
    {
        let (key, dir) = ctl.listing.sort();
        std::mem::discriminant(&key).hash(&mut h);
        std::mem::discriminant(&dir).hash(&mut h);
    }
    ctl.listing.show_hidden().hash(&mut h);
    ctl.listing.query().hash(&mut h);
    for e in ctl.listing.view() {
        e.name.hash(&mut h);
        e.is_dir.hash(&mut h);
        e.is_symlink.hash(&mut h);
        e.len.hash(&mut h);
        e.mtime.hash(&mut h);
        e.mode.hash(&mut h);
    }
    ctl.counts.len().hash(&mut h);
    let mut counts: Vec<_> = ctl.counts.iter().collect();
    counts.sort_by_key(|(name, _)| *name);
    for (name, n) in counts {
        name.hash(&mut h);
        n.hash(&mut h);
    }
    for name in ctl.listing.selection() {
        name.hash(&mut h);
    }
    ctl.clock.hash(&mut h);
    if let Some(d) = &ctl.drag {
        d.target.hash(&mut h);
        d.refused.hash(&mut h);
    }
    if let Some(clip) = ctl.clip.clip() {
        clip.dir.hash(&mut h);
        clip.names.hash(&mut h);
        std::mem::discriminant(&clip.mode).hash(&mut h);
    }
    ctl.in_trash().hash(&mut h);
    let mut trash: Vec<_> = ctl.trash.iter().collect();
    trash.sort_by_key(|(name, _)| *name);
    for (name, info) in trash {
        name.hash(&mut h);
        info.original.hash(&mut h);
        info.deleted.hash(&mut h);
    }
    h.finish()
}

fn row_label(name: &str, budget: usize) -> SharedString {
    let cut = elide_middle(name, budget);
    if cut == name {
        SharedString::new()
    } else {
        cut.into()
    }
}

fn row_mark(name: &str, label: &SharedString, query: &str) -> Mark {
    match crate::model::match_runs(name, query) {
        Some(runs) if label.is_empty() => {
            let runs: Vec<MarkRun> = runs
                .into_iter()
                .map(|(text, hit)| MarkRun {
                    text: text.into(),
                    hit,
                })
                .collect();
            Mark {
                runs: ModelRc::new(Rc::new(VecModel::from(runs))),
            }
        }
        _ => Mark::default(),
    }
}

/// Merge a rebuilt prefix into an already-shown list. The tail past
/// this chunk stays until `complete`, so a same-folder restart (counts
/// landing, a selection change, a sort) doesn't shrink a list longer
/// than one chunk or jump the scroll.
fn merge_shown<T: Clone>(
    dest: &mut Vec<T>,
    start: usize,
    part: Vec<T>,
    total: usize,
    complete: bool,
) {
    if dest.len() > start {
        let overwrite = (dest.len() - start).min(part.len());
        dest[start..start + overwrite].clone_from_slice(&part[..overwrite]);
        if overwrite < part.len() {
            dest.extend_from_slice(&part[overwrite..]);
        }
    } else {
        dest.extend(part);
    }
    if complete {
        dest.truncate(total);
    }
}

fn sync_model<T: Clone + 'static>(model: &VecModel<T>, data: &[T]) {
    let n = data.len();
    let m = model.row_count();
    let share = n.min(m);
    for (i, item) in data.iter().take(share).enumerate() {
        model.set_row_data(i, item.clone());
    }
    for item in data.iter().skip(share) {
        model.push(item.clone());
    }
    for i in (n..m).rev() {
        model.remove(i);
    }
}

fn bind_model<T: Clone + 'static>(
    slot: &RefCell<Option<Rc<VecModel<T>>>>,
    data: &[T],
    replace: bool,
    set: impl FnOnce(ModelRc<T>),
) {
    let mut slot = slot.borrow_mut();
    if let Some(model) = slot.as_ref() {
        if replace {
            model.set_vec(data.to_vec());
        } else {
            sync_model(model, data);
        }
    } else {
        let model = Rc::new(VecModel::from(data.to_vec()));
        set(ModelRc::from(model.clone()));
        *slot = Some(model);
    }
}

fn clear_model<T: Clone + 'static>(
    slot: &RefCell<Option<Rc<VecModel<T>>>>,
    set: impl FnOnce(ModelRc<T>),
) {
    if let Some(model) = slot.borrow().as_ref() {
        if model.row_count() > 0 {
            model.set_vec(Vec::new());
        }
    } else {
        set(ModelRc::default());
    }
}

/// Keep filling the list on a later event-loop turn, preserving scroll.
fn schedule_row_fill(s: &State) {
    if s.row_fill_queued.get() {
        return;
    }
    s.row_fill_queued.set(true);
    let _ = slint::invoke_from_event_loop(|| {
        with_state(|ui, st| {
            st.borrow().row_fill_queued.set(false);
            let (list_y, grid_y) = (ui.get_list_scroll(), ui.get_grid_scroll());
            redraw(&ui, &st.borrow());
            ui.set_list_scroll(list_y);
            ui.set_grid_scroll(grid_y);
        });
    });
}

/// Build the next chunk of rows into `s.rows_fill` and push them to the
/// window's models without clearing a list that is already on screen.
/// True while rows are still being built (more chunks queued).
fn fill_rows(ui: &AppWindow, s: &State, budget: usize) -> bool {
    let ctl = &s.ctl;
    let key = rows_key(ctl, budget);
    let view = ctl.listing.view();
    let total = view.len();
    let mut fill = s.rows_fill.borrow_mut();
    let same = fill.key == key && fill.dir == ctl.dir;
    if same && fill.filled == fill.total && fill.total == total {
        return false;
    }
    let replace = fill.dir != ctl.dir || s.files_model.borrow().is_none();
    let start = if same { fill.filled } else { 0 };
    let end = (start.saturating_add(row_chunk_limit())).min(total);
    let chunk = ctl.rows_range(&view, start, end);
    let query = ctl.listing.query();
    let (labels, marks): (Vec<SharedString>, Vec<Mark>) = if ctl.grid {
        (Vec::new(), Vec::new())
    } else {
        chunk
            .iter()
            .map(|r| {
                let label = row_label(&r.name, budget);
                let mark = row_mark(&r.name, &label, query);
                (label, mark)
            })
            .unzip()
    };
    if replace && start == 0 {
        fill.rows.clear();
        fill.labels.clear();
        fill.marks.clear();
    }
    let complete = end == total;
    // On a same-folder rebuild, keep already-shown rows past this
    // chunk until the pass is complete, so the list doesn't shrink
    // and jump whatever was scrolled below the first chunk.
    merge_shown(&mut fill.rows, start, chunk, total, complete);
    if !ctl.grid {
        merge_shown(&mut fill.labels, start, labels, total, complete);
        merge_shown(&mut fill.marks, start, marks, total, complete);
    } else {
        fill.labels.clear();
        fill.marks.clear();
    }
    fill.key = key;
    fill.dir = ctl.dir.clone();
    fill.filled = end;
    fill.total = total;
    if ctl.grid {
        let lines = ctl.grid_lines(&fill.rows);
        bind_model(&s.grid_model, &lines, replace && start == 0, |m| {
            ui.set_grid_rows(m)
        });
        clear_model(&s.files_model, |m| ui.set_files(m));
        clear_model(&s.labels_model, |m| ui.set_labels(m));
        clear_model(&s.marks_model, |m| ui.set_marks(m));
    } else {
        bind_model(&s.files_model, &fill.rows, replace && start == 0, |m| {
            ui.set_files(m)
        });
        bind_model(&s.labels_model, &fill.labels, replace && start == 0, |m| {
            ui.set_labels(m)
        });
        bind_model(&s.marks_model, &fill.marks, replace && start == 0, |m| {
            ui.set_marks(m)
        });
        clear_model(&s.grid_model, |m| ui.set_grid_rows(m));
    }
    drop(fill);
    if end < total {
        schedule_row_fill(s);
    }
    end < total
}

/// What the file pane draws. Until the first read lands it is `loading`,
/// which draws placeholder rows: never a listing or a selection.
fn pane_state(ctl: &Controller) -> &'static str {
    let n = ctl.listing.view().len();
    if !ctl.loaded {
        "loading"
    } else if ctl.unreadable.is_some() {
        "denied"
    } else if n == 0 && !ctl.listing.query().is_empty() {
        "nosearch"
    } else if n == 0 && ctl.in_trash() {
        "trash-empty"
    } else if n == 0 {
        "empty"
    } else {
        "files"
    }
}

fn redraw(ui: &AppWindow, s: &State) {
    sync_viewer(ui, s);
    let ctl = &s.ctl;
    sync_drop_dest(s);
    remember_prefs(s);
    ui.set_grid(ctl.grid);
    ui.set_compact(ctl.compact);
    ui.set_hidden_on(ctl.listing.show_hidden());
    ui.set_tile(ctl.tile());
    let content_w = ui.get_content_w();
    ctl.fit(content_w);
    ui.set_grid_columns(ctl.columns.get() as i32);
    ui.global::<Columns>().set_row_w(content_w);
    // Before the first layout the width is 0; show full names then.
    let budget = if content_w > 0.0 {
        ctl.refit_list(content_w);
        name_chars(content_w)
    } else {
        usize::MAX
    };
    // Fill the visible model in chunks, keeping whatever is already on
    // screen so scroll and selection don't reset and the pane never
    // flashes empty while a large folder streams in.
    let filling = fill_rows(ui, s, budget);
    let (column, desc) = ctl.sort_header();
    ui.set_sort_column(column);
    ui.set_sort_desc(desc);
    let crumbs = draw_crumbs(ui, s);
    ui.set_path_text(ctl.crumb_dir().display().to_string().into());
    ui.set_nav_busy(ctl.nav_pending());
    draw_side(ui, s);
    if let Some(denied) = ctl.unreadable {
        let name = crumb_name(&crumbs);
        ui.set_state_title(
            if denied {
                format!("No permission to open {name}")
            } else {
                format!("Can't open {name}")
            }
            .into(),
        );
        ui.set_state_note(
            if denied {
                denied_note(&ctl.dir)
            } else {
                String::new()
            }
            .into(),
        );
    }
    // Rows still streaming in: the bar under the toolbar runs and the
    // status bar says so instead of showing a count that is still growing.
    ui.set_reading(!ctl.loaded || filling);
    ui.set_status_left(ctl.status_line(filling).into());
    ui.set_trash_view(ctl.in_trash());
    ui.set_trash_can_restore(matches!(ctl.restore_selected(), Action::Run(_)));
    ui.set_trash_note(ctl.trash_note().into());
    let n = ctl.listing.view().len();
    ui.set_search_query(ctl.listing.query().into());
    ui.set_search_scope(format!("in {}", ctl.dir_name()).into());
    ui.set_search_count(if ctl.listing.query().is_empty() {
        SharedString::new()
    } else {
        n.to_string().into()
    });
    if n == 0 {
        if let Some((title, note)) = ctl.no_match_text() {
            ui.set_state_title(title.into());
            ui.set_state_note(note.into());
        }
    }
    ui.set_state(pane_state(ctl).into());
    match ctl.drag_label() {
        Some((label, glyph)) => {
            ui.set_drag_label(label.into());
            ui.set_drag_glyph(glyph.into());
        }
        None => ui.set_drag_label(SharedString::new()),
    }
    ui.set_dialog_focus(ctl.focus);
    ui.set_danger_armed(ctl.armed);
    ui.set_trash_dont_ask(ctl.dont_ask);
    // [Search hidden files] only when hidden files were left out.
    ui.set_search_hidden_offer(!ctl.listing.show_hidden() && ctl.listing.hidden_count() > 0);
    match &ctl.dialog {
        Some(d) => {
            ui.set_overlay(d.overlay().into());
            match d {
                Dialog::EmptyTrash(names) => {
                    ui.set_confirm_noun(format!("{} in Trash", ops::count(names.len())).into());
                    ui.set_confirm_names(names_text(names).into());
                    ui.set_confirm_names_count(names.len().min(6) as i32);
                }
                Dialog::Trash(names) | Dialog::TrashNo(names) | Dialog::Delete(names) => {
                    ui.set_confirm_noun(noun(names).into());
                    ui.set_confirm_names(names_text(names).into());
                    ui.set_confirm_names_count(names.len().min(6) as i32);
                }
                Dialog::ClearThumbs(cache) => {
                    let home = ctl.env.home.as_deref();
                    ui.set_confirm_noun("Robin's thumbnails".into());
                    ui.set_confirm_names(
                        format!(
                            "{}\nOther apps' thumbnails stay",
                            tilde(&cache.join("thumbnails"), home)
                        )
                        .into(),
                    );
                    ui.set_confirm_names_count(2);
                }
                Dialog::Refused { .. } => {}
                Dialog::Conflict { ask, apply_all } => conflict_props(ui, ask, *apply_all),
            }
        }
        None if ctl.searching => ui.set_overlay("search".into()),
        None => ui.set_overlay(SharedString::new()),
    }
}

fn apply(ui: &AppWindow, st: &Shared, action: Action) {
    match action {
        Action::None => {}
        Action::Redraw => redraw(ui, &st.borrow()),
        Action::Open(d) => {
            // Every danger dialog arms after a beat, not just Delete:
            // Empty Trash used to stay disarmed for good.
            let delete = matches!(
                d,
                Dialog::Delete(_) | Dialog::EmptyTrash(_) | Dialog::ClearThumbs(_)
            );
            st.borrow_mut().ctl.open_dialog(d);
            redraw(ui, &st.borrow());
            if delete {
                let w = ui.as_weak();
                let st2 = st.clone();
                st.borrow().arm_timer.start(
                    slint::TimerMode::SingleShot,
                    Duration::from_millis(600),
                    move || {
                        st2.borrow_mut().ctl.armed = true;
                        if let Some(ui) = w.upgrade() {
                            ui.set_danger_armed(true);
                        }
                    },
                );
            }
        }
        Action::Navigate(dir) => {
            ui.set_stats_mode("".into());
            let ticket = st.borrow_mut().ctl.begin_navigate(dir);
            sync_drop_dest(&st.borrow());
            let gen = ticket.gen();
            let retargeted = st.borrow().ctl.nav_pending();
            if retargeted {
                redraw(ui, &st.borrow());
            }
            start_read(Some(ticket));
            let st = st.clone();
            let ui = ui.as_weak();
            slint::Timer::single_shot(NAV_PENDING_AFTER, move || {
                let Some(ui) = ui.upgrade() else { return };
                let mut s = st.borrow_mut();
                if s.ctl.tick_nav(gen, std::time::Instant::now()) {
                    redraw(&ui, &s);
                }
            });
        }
        Action::Launch(argv) => launch(ui, st, argv),
        Action::ClearThumbs(cache) => clear_thumbs(ui, st, cache),
        Action::Answer(choice) => {
            if let Some(tx) = st.borrow_mut().reply.take() {
                let _ = tx.send(choice);
            }
            redraw(ui, &st.borrow());
        }
        Action::CancelJob => {
            if let Some(c) = &st.borrow().cancel {
                c.cancel();
            }
            let pending = {
                let mut s = st.borrow_mut();
                if matches!(s.ctl.dialog, Some(Dialog::Conflict { .. })) {
                    s.ctl.dialog = None;
                }
                s.reply.take()
            };
            if let Some(tx) = pending {
                let _ = tx.send(None);
                redraw(ui, &st.borrow());
            }
            st.borrow_mut().queue.clear();
            ui.set_job_detail("Cancelling after this item…".into());
        }
        Action::Run(job) => {
            st.borrow_mut().queue.push_back(job);
            redraw(ui, &st.borrow());
            pump(ui, st);
        }
    }
}

/// Start the next queued job if none is running.
fn pump(ui: &AppWindow, st: &Shared) {
    let (job, env, cancel) = {
        let mut s = st.borrow_mut();
        if s.ctl.job_running {
            return;
        }
        let Some(job) = s.queue.pop_front() else {
            return;
        };
        s.ctl.job_running = true;
        let cancel = Arc::new(Cancel::new());
        s.cancel = Some(cancel.clone());
        (job, s.ctl.env.clone(), cancel)
    };
    ui.set_job_on(true);
    ui.set_job_glyph(job.glyph().into());
    ui.set_job_label(job.label().into());
    ui.set_job_detail(format!("0 of {}", ops::count(job.names.len())).into());
    ui.set_job_amount(0.0);
    // Renders have no event loop to post back to: run the job here.
    if SCRIPTED.with(|s| s.get()) {
        let outcome = ops::run(&job, &env, &cancel, &mut |_| {});
        done(ui, st, job, outcome);
        return;
    }
    let dest = job.dest.file_name().map_or_else(
        || job.dest.display().to_string(),
        |n| n.to_string_lossy().into_owned(),
    );
    std::thread::spawn(move || {
        let total = job.names.len();
        let mut seen = 0usize;
        let mut ask = |prompt: &ConflictPrompt| -> Result<ConflictChoice> {
            seen += 1;
            let (tx, rx) = mpsc::channel();
            let ask = ConflictAsk {
                prompt: prompt.clone(),
                dest: dest.clone(),
                left: total.saturating_sub(seen - 1).max(1),
            };
            let _ = slint::invoke_from_event_loop(move || {
                with_state(|ui, st| ask_conflict(&ui, st, ask, tx));
            });
            // The window answers on the event loop; this thread waits so
            // the transfer resumes exactly where it stopped.
            match rx.recv() {
                Ok(Some(choice)) => Ok(choice),
                _ => Err(Error::Cancelled),
            }
        };
        let outcome = ops::run_asking(
            &job,
            &env,
            &cancel,
            &mut |p: &Progress| {
                let (detail, amount) = (p.detail(), p.amount());
                let _ = slint::invoke_from_event_loop(move || {
                    with_state(|ui, _| {
                        if !ui.get_job_detail().starts_with("Cancelling") {
                            ui.set_job_detail(detail.into());
                        }
                        ui.set_job_amount(amount);
                    });
                });
            },
            &mut ask,
        );
        let _ = slint::invoke_from_event_loop(move || {
            with_state(|ui, st| done(&ui, st, job, outcome));
        });
    });
}

/// Show the job thread's conflict question. Another dialog open means the
/// user is mid-answer elsewhere, so the question waits for it to close.
fn ask_conflict(
    ui: &AppWindow,
    st: &Shared,
    ask: ConflictAsk,
    tx: mpsc::Sender<Option<ConflictChoice>>,
) {
    if st.borrow().ctl.dialog.is_some() {
        let w = ui.as_weak();
        let st = st.clone();
        slint::Timer::single_shot(Duration::from_millis(200), move || {
            if let Some(ui) = w.upgrade() {
                ask_conflict(&ui, &st, ask, tx);
            }
        });
        return;
    }
    st.borrow_mut().reply = Some(tx);
    apply(
        ui,
        st,
        Action::Open(Dialog::Conflict {
            ask,
            apply_all: false,
        }),
    );
}

pub fn conflict_props(ui: &AppWindow, ask: &ConflictAsk, apply_all: bool) {
    let p = &ask.prompt;
    ui.set_conflict_title(format!("{} already has “{}”", ask.dest, p.name).into());
    ui.set_conflict_name(p.name.as_str().into());
    let folders = p.kind == ConflictKind::FolderFolder;
    let ext = if folders {
        "Folder".into()
    } else {
        Path::new(&p.name)
            .extension()
            .map(|e| format!("{} file", e.to_string_lossy().to_uppercase()))
            .unwrap_or_else(|| "File".into())
    };
    ui.set_conflict_kind(ext.into());
    let (existing_dir, incoming_dir) = match p.kind {
        ConflictKind::FileFile => (false, false),
        ConflictKind::FolderFolder => (true, true),
        ConflictKind::FileFolder => (false, true),
        ConflictKind::FolderFile => (true, false),
    };
    ui.set_conflict_existing_size(
        size_line(existing_dir, p.existing_size, p.existing_items).into(),
    );
    ui.set_conflict_incoming_size(
        size_line(incoming_dir, p.incoming_size, p.incoming_items).into(),
    );
    ui.set_conflict_existing_folder(existing_dir);
    ui.set_conflict_incoming_folder(incoming_dir);
    // Replacing a folder with a folder merges into it; the button says so.
    ui.set_conflict_replace_label(if folders { "Merge" } else { "Replace" }.into());
    ui.set_conflict_existing_date(fsops::short_date(p.existing_mtime).into());
    ui.set_conflict_incoming_date(fsops::short_date(p.incoming_mtime).into());
    let (existing, incoming) = side_badges(p);
    let at = |v: &[&'static str], i: usize| SharedString::from(v.get(i).copied().unwrap_or(""));
    ui.set_conflict_existing_badge(at(&existing, 0));
    ui.set_conflict_existing_badge2(at(&existing, 1));
    ui.set_conflict_incoming_badge(at(&incoming, 0));
    ui.set_conflict_incoming_badge2(at(&incoming, 1));
    let both = if existing_dir {
        format!("{} (2)", p.name)
    } else {
        keep_both_name(&p.name)
    };
    ui.set_conflict_hint(format!("Keep both saves it as {both}").into());
    ui.set_conflict_note(cancel_note(folders).into());
    // Replace (or Merge) is the primary button, amber when what it
    // overwrites is newer than what replaces it.
    ui.set_conflict_replace_warn(p.existing_mtime > p.incoming_mtime);
    ui.set_conflict_apply_label(apply_label(p.kind, ask.left).into());
    ui.set_conflict_apply(apply_all);
}

/// A conflict side's size line: a file's size, or a folder's item count
/// as the list view shows it.
/// Thunar-style type names for the Type column; unknown extensions read
/// as "XYZ file" and files without one as "File".
/// The glyph and colour role for a row. Roles: 0 ink, 1 accent (cyan),
/// 2 danger (red), 3 warning (amber), 4 muted, as in `ui/app.slint`.
pub fn file_icon(is_dir: bool, ext: &str) -> (&'static str, i32) {
    if is_dir {
        return ("󰉋", 1);
    }
    match ext.to_ascii_lowercase().as_str() {
        "jpg" | "jpeg" | "png" | "gif" | "webp" | "heic" | "heif" | "svg" | "tif" | "tiff"
        | "raw" | "cr2" | "cr3" | "nef" | "arw" | "dng" => ("󰈟", 0),
        "mp4" | "m4v" | "mkv" | "webm" | "mov" | "avi" => ("󰕧", 1),
        "mp3" | "flac" | "ogg" | "opus" | "wav" | "m4a" => ("󰎆", 1),
        "pdf" => ("󰈦", 2),
        "zip" | "tar" | "gz" | "zst" | "xz" | "bz2" | "7z" | "rar" => ("󰗄", 3),
        "toml" | "json" | "yaml" | "yml" => ("\u{e6b2}", 1),
        "ini" | "conf" | "cfg" => ("\u{f0493}", 0), // 󰒓 settings
        "md" | "txt" => ("\u{f0354}", 0),
        _ => ("󰈔", 4),
    }
}

pub fn kind_name(ext: &str) -> String {
    let ext = ext.to_ascii_lowercase();
    let known = match ext.as_str() {
        "jpg" | "jpeg" => "JPEG image",
        "png" => "PNG image",
        "gif" => "GIF image",
        "webp" => "WebP image",
        "heic" | "heif" => "HEIC image",
        "svg" => "SVG image",
        "tif" | "tiff" => "TIFF image",
        "raw" | "cr2" | "nef" | "arw" | "dng" => "Raw image",
        "mp4" | "m4v" => "MPEG-4 video",
        "mkv" => "Matroska video",
        "webm" => "WebM video",
        "mov" => "QuickTime video",
        "mp3" => "MP3 audio",
        "flac" => "FLAC audio",
        "ogg" | "opus" => "Ogg audio",
        "wav" => "WAV audio",
        "md" => "Markdown",
        "txt" => "Plain text",
        "toml" => "TOML",
        "json" => "JSON",
        "pdf" => "PDF document",
        "zip" | "tar" | "gz" | "xz" | "zst" | "7z" => "Archive",
        "rs" => "Rust source",
        "sh" => "Shell script",
        "" => "File",
        _ => return format!("{} file", ext.to_uppercase()),
    };
    known.to_string()
}

pub fn size_line(dir: bool, size: u64, items: Option<usize>) -> String {
    match (dir, items) {
        (false, _) => human(size),
        (true, Some(i)) => crate::ops::count(i),
        (true, None) => String::new(),
    }
}

/// Neither a Replace nor the overwrites inside a Merge are taken back by
/// Cancel; the dialog says so where the choice is made.
pub fn cancel_note(folders: bool) -> &'static str {
    if folders {
        "Replaced files aren't restored on cancel."
    } else {
        "A replaced file isn't restored on cancel."
    }
}

/// The apply-to-all toggle names the kind of clash it covers, since an
/// answer only carries over to clashes of that kind.
pub fn apply_label(kind: ConflictKind, left: usize) -> String {
    let what = match kind {
        ConflictKind::FileFile => "file conflicts",
        ConflictKind::FolderFolder => "folder conflicts",
        ConflictKind::FileFolder | ConflictKind::FolderFile => "file-and-folder conflicts",
    };
    if left > 1 {
        format!("Apply to the rest of the {what} ({})", left - 1)
    } else {
        format!("Apply to the rest of the {what}")
    }
}

/// ("Larger", "Newer") badges for the (existing, incoming) side.
pub fn conflict_badges(
    p: &ConflictPrompt,
) -> ((&'static str, &'static str), (&'static str, &'static str)) {
    let sized = p.kind == ConflictKind::FileFile;
    let larger = match p.existing_size.cmp(&p.incoming_size) {
        _ if !sized => ("", ""),
        std::cmp::Ordering::Greater => ("Larger", ""),
        std::cmp::Ordering::Less => ("", "Larger"),
        std::cmp::Ordering::Equal => ("", ""),
    };
    let newer = match p.existing_mtime.cmp(&p.incoming_mtime) {
        std::cmp::Ordering::Greater => ("Newer", ""),
        std::cmp::Ordering::Less => ("", "Newer"),
        std::cmp::Ordering::Equal => ("", ""),
    };
    (larger, newer)
}

/// Every badge each side earns, in reading order: a file both newer and
/// larger shows "Newer" and "Larger" side by side.
pub fn side_badges(p: &ConflictPrompt) -> (Vec<&'static str>, Vec<&'static str>) {
    let (larger, newer) = conflict_badges(p);
    let keep = |v: [&'static str; 2]| v.into_iter().filter(|b| !b.is_empty()).collect();
    (keep([newer.0, larger.0]), keep([newer.1, larger.1]))
}

/// The first name Keep both tries: "photo (2).jpg".
pub fn keep_both_name(name: &str) -> String {
    match name.rfind('.') {
        Some(i) if i > 0 => format!("{} (2){}", &name[..i], &name[i..]),
        _ => format!("{name} (2)"),
    }
}

fn with_state(f: impl FnOnce(AppWindow, &Shared)) {
    STATE.with(|s| {
        if let Some((st, w)) = s.borrow().as_ref() {
            if let Some(ui) = w.upgrade() {
                f(ui, st);
            }
        }
    });
}

fn done(ui: &AppWindow, st: &Shared, job: Job, outcome: Outcome) {
    ui.set_job_on(false);
    let dialog = {
        let mut s = st.borrow_mut();
        s.ctl.job_running = false;
        s.cancel = None;
        let ticket = s.ctl.begin_refresh();
        start_read(ticket);
        s.ctl.finish(&job, &outcome)
    };
    redraw(ui, &st.borrow());
    let toast = outcome.toast(job.kind);
    if outcome.done > 0 || !toast.details.is_empty() || outcome.cancelled {
        if toast.undo {
            st.borrow_mut().undo = outcome.trashed.clone();
        }
        show_toast(
            ui,
            st,
            toast.tone,
            toast.text,
            toast.details,
            toast.pinned,
            if toast.undo { "Undo" } else { "" },
        );
    }
    if let Some(d) = dialog {
        if matches!(d, Dialog::Refused { .. }) {
            set_refusal(ui, &refusal_view(&outcome.refused));
        }
        apply(ui, st, Action::Open(d));
    }
    pump(ui, st);
}

pub fn set_refusal(ui: &AppWindow, r: &RefusalView) {
    ui.set_refusal_title(r.title.as_str().into());
    ui.set_refusal_can_copy(r.can_copy);
    ui.set_refusal_body(r.body.as_str().into());
    ui.set_refusal_dismiss(r.dismiss.into());
    ui.set_refusal_notes(r.notes as i32);
    ui.set_refusal_lines_items(r.items as i32);
    ui.set_refusal_lines(ModelRc::new(Rc::new(VecModel::from(
        r.lines
            .iter()
            .map(|(path, note, head)| crate::RefusalLine {
                path: path.as_str().into(),
                note: note.as_str().into(),
                head: *head,
            })
            .collect::<Vec<_>>(),
    ))));
}

fn show_toast(
    ui: &AppWindow,
    st: &Shared,
    tone: &str,
    text: String,
    details: Vec<String>,
    pinned: bool,
    action: &str,
) {
    bind_toast(ui, st, tone, text, details, pinned, action);
    let secs = if action.is_empty() && st.borrow().toast_all.is_empty() {
        4
    } else {
        8
    };
    let gen = st.borrow().toast_gen;
    let w = ui.as_weak();
    let st = st.clone();
    slint::Timer::single_shot(Duration::from_secs(secs), move || {
        let mut s = st.borrow_mut();
        if s.toast_gen == gen && !s.toast_open {
            s.undo.clear();
            if let Some(ui) = w.upgrade() {
                ui.set_toast_on(false);
            }
        }
    });
}

/// A toast that stays until something else replaces it or the caller hides it.
fn show_toast_sticky(
    ui: &AppWindow,
    st: &Shared,
    tone: &str,
    text: String,
    details: Vec<String>,
    pinned: bool,
    action: &str,
) {
    bind_toast(ui, st, tone, text, details, pinned, action);
}

fn bind_toast(
    ui: &AppWindow,
    st: &Shared,
    tone: &str,
    text: String,
    details: Vec<String>,
    pinned: bool,
    action: &str,
) {
    {
        let mut s = st.borrow_mut();
        s.toast_gen += 1;
        s.toast_open = false;
        s.toast_all = details.clone();
        s.toast_pinned = pinned;
    }
    ui.set_toast_tone(tone.into());
    ui.set_toast_glyph(toast_glyph(tone).into());
    ui.set_toast_text(text.into());
    set_toast_rows(ui, &detail_rows(&details, pinned, false));
    ui.set_toast_action(action.into());
    ui.set_toast_open(false);
    ui.set_toast_on(true);
}

/// Details shown before "+N more"; the pinned line doesn't count.
pub const DETAILS_SHOWN: usize = 7;

/// What the toast's Details list shows.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct DetailRows {
    /// Stays at the top, outside the scrolling list.
    pub pin: String,
    pub rows: Vec<String>,
    /// Lines behind "+N more"; 0 once expanded or when all fit.
    pub more: usize,
    pub expanded: bool,
}

/// Collapsed, the list shows the pinned line, the first seven items and
/// "+N more"; expanded, every item, scrolling past 240px.
pub fn detail_rows(details: &[String], pinned: bool, expanded: bool) -> DetailRows {
    let (pin, items) = match details.split_first() {
        Some((first, rest)) if pinned => (first.clone(), rest),
        _ => (String::new(), details),
    };
    // "+1 more" would take the same room as the line itself.
    let cut = !expanded && items.len() > DETAILS_SHOWN + 1;
    let shown = if cut { DETAILS_SHOWN } else { items.len() };
    DetailRows {
        pin,
        rows: items[..shown].to_vec(),
        more: items.len() - shown,
        expanded: expanded && items.len() > DETAILS_SHOWN + 1,
    }
}

impl DetailRows {
    /// Whether anything sits under the headline, so the divider shows.
    pub fn has_details(&self) -> bool {
        !self.pin.is_empty() || !self.rows.is_empty() || self.more > 0
    }
}

pub fn set_toast_rows(ui: &AppWindow, d: &DetailRows) {
    ui.set_toast_has_details(d.has_details());
    ui.set_toast_pin(d.pin.as_str().into());
    ui.set_toast_more(d.more as i32);
    ui.set_toast_expanded(d.expanded);
    ui.set_toast_details(ModelRc::new(Rc::new(VecModel::from(
        d.rows
            .iter()
            .map(|r| SharedString::from(r.as_str()))
            .collect::<Vec<_>>(),
    ))));
}

pub fn start_dir(arg: Option<&Path>) -> PathBuf {
    match arg {
        Some(p) => p.to_path_buf(),
        None => std::env::var_os("HOME")
            .map(PathBuf::from)
            .unwrap_or_else(|| "/".into()),
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn an_automount_crumb_starts_at_the_device_before_the_side_job() {
        let dir = Path::new("/run/media/sky/CARD/DCIM");
        let labels = |side: &[crate::sidebar::SideEntry]| -> Vec<String> {
            super::crumbs_for(dir, Some(Path::new("/home/sky")), side, Some("sky"))
                .into_iter()
                .map(|c| c.label)
                .collect()
        };
        assert_eq!(labels(&[]), ["CARD", "DCIM"], "first frame, no devices yet");
        let named = crate::sidebar::SideEntry {
            glyph: "",
            name: "Camera card".into(),
            path: "/run/media/sky/CARD".into(),
            count: String::new(),
            usage: None,
            removable: true,
        };
        assert_eq!(
            labels(&[named]),
            ["Camera card", "DCIM"],
            "the side job's label wins"
        );
    }

    #[test]
    fn a_crumb_name_is_built_from_its_parts() {
        let v = |a: &[&str]| a.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        assert_eq!(super::crumb_name(&v(&["/", "srv", "x"])), "/srv/x");
        assert_eq!(super::crumb_name(&v(&["~", "Documents"])), "~/Documents");
        assert_eq!(super::crumb_name(&v(&["CARD", "a//b"])), "CARD/a//b");
        assert_eq!(super::crumb_name(&v(&["/"])), "/");
    }

    use super::*;
    use std::fs;

    fn setup(label: &str) -> Controller {
        let root = std::env::temp_dir().join(format!("robin-app-{label}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        let dir = root.join("Pictures");
        fs::create_dir_all(dir.join("raw-exports")).unwrap();
        fs::write(dir.join("a.jpg"), b"a").unwrap();
        fs::write(dir.join("b.jpg"), b"b").unwrap();
        let env = Env {
            home: Some(root.clone()),
            home_trash: root.join("trash"),
            config_home: None,
            cache_home: None,
        };
        let mut c = Controller::new(dir, env);
        c.reload().unwrap();
        c
    }

    #[test]
    fn grid_columns_fit_the_width_within_one_to_six() {
        assert_eq!(fit_columns(100.0, 152.0), 1);
        assert_eq!(fit_columns(32.0 + 152.0 * 3.0 + 16.0, 152.0), 3);
        assert_eq!(fit_columns(5000.0, 96.0), MAX_COLUMNS);
    }

    #[test]
    fn tile_size_steps_stop_at_either_end_and_follow_density() {
        let mut c = setup("tiles");
        assert_eq!(c.tile(), 152.0);
        assert_eq!(c.step_tile(1), Action::Redraw);
        assert_eq!(c.step_tile(1), Action::None);
        assert_eq!(c.tile(), 208.0);
        c.compact = true;
        assert_eq!(c.tile(), 176.0);
        c.step_tile(-5);
        assert_eq!(c.tile(), 96.0);
    }

    #[test]
    fn grid_lines_chunk_by_the_fitted_columns() {
        let mut c = setup("lines");
        assert_eq!(c.set_grid(true), Action::Redraw);
        assert_eq!(c.set_grid(true), Action::None);
        assert!(c.fit(32.0 + 152.0 * 2.0 + 8.0));
        let lines = c.grid_lines(&c.rows());
        assert_eq!(lines.len(), 2);
        assert_eq!((lines[0].n, lines[0].first), (2, 0));
        assert_eq!((lines[1].n, lines[1].first), (1, 2));
        assert!(!c.fit(32.0 + 152.0 * 2.0 + 8.0), "same width, no rebuild");
    }

    #[test]
    fn shift_down_in_the_grid_selects_the_whole_range() {
        let mut c = setup("shift-range");
        for n in ["c.jpg", "d.jpg", "e.jpg"] {
            fs::write(c.dir.join(n), b"x").unwrap();
        }
        c.reload().unwrap();
        c.set_grid(true);
        c.columns.set(3);
        c.key(keys::HOME, false, false);
        c.key(keys::RIGHT, false, false);
        c.key(keys::DOWN, false, true);
        // raw-exports a b / c d e: a..d is a, b, c, d.
        let picked: Vec<_> = c.names();
        assert_eq!(
            picked,
            ["a.jpg", "b.jpg", "c.jpg", "d.jpg"].map(OsString::from)
        );
        c.key(keys::UP, false, true);
        assert_eq!(c.names(), vec![OsString::from("a.jpg")]);
    }

    #[test]
    fn dragging_a_tile_onto_a_folder_targets_it() {
        let mut c = setup("tile-drag");
        c.set_grid(true);
        c.columns.set(3);
        let none = Mods {
            ctrl: false,
            shift: false,
        };
        c.pointer(1, 0, 0, false, none);
        // One tile to the left of a.jpg is raw-exports.
        assert_eq!(c.pointer(1, 1, -1, true, none), Action::Redraw);
        assert_eq!(c.drag.as_ref().and_then(|d| d.target), Some(0));
    }

    #[test]
    fn grid_arrows_move_by_the_column_count() {
        let mut c = setup("grid-arrows");
        for n in ["c.jpg", "d.jpg", "e.jpg"] {
            fs::write(c.dir.join(n), b"x").unwrap();
        }
        c.reload().unwrap();
        c.set_grid(true);
        c.columns.set(3);
        c.key(keys::HOME, false, false);
        c.key(keys::DOWN, false, false);
        assert_eq!(c.listing.cursor(), Some(&OsString::from("c.jpg")));
        c.key(keys::RIGHT, false, false);
        assert_eq!(c.listing.cursor(), Some(&OsString::from("d.jpg")));
        c.key(keys::UP, false, false);
        assert_eq!(c.listing.cursor(), Some(&OsString::from("a.jpg")));
        // The list moves one row at a time and ignores left/right.
        c.set_grid(false);
        c.key(keys::DOWN, false, false);
        assert_eq!(c.listing.cursor(), Some(&OsString::from("b.jpg")));
        assert_eq!(c.key(keys::RIGHT, false, false), Action::None);
    }

    #[test]
    fn ctrl_1_ctrl_2_and_ctrl_plus_minus_drive_the_view() {
        let mut c = setup("ctrl-keys");
        c.key("2", true, false);
        assert!(c.grid);
        let start = c.tile_step;
        assert_eq!(c.key("=", true, false), Action::Redraw);
        assert_eq!(c.tile_step, (start + 1).min(2));
        c.key("-", true, false);
        c.key("-", true, false);
        assert_eq!(c.tile_step, start.saturating_sub(1));
        c.key("1", true, false);
        assert!(!c.grid);
    }

    #[test]
    fn a_resize_refits_the_columns() {
        let c = setup("refit");
        let tile = c.tile();
        assert!(c.fit(tile * 3.0 + 60.0));
        let wide = c.columns.get();
        assert!(!c.fit(tile * 3.0 + 60.0), "same width keeps the lines");
        assert!(c.fit(tile * 6.0 + 120.0));
        assert!(c.columns.get() > wide);
        assert!(c.fit(tile + 40.0));
        assert_eq!(c.columns.get(), 1);
    }

    #[test]
    fn narrow_free_space_drops_the_drive_size() {
        assert_eq!(short_free("812 GB free of 1.8 TB"), "812 GB free");
        assert_eq!(short_free(""), "");
    }

    #[test]
    fn narrow_lists_give_names_the_type_columns_width() {
        // Just under the cut Type is gone and Name gets its 136px.
        let (above, below) = (columns(668.0), columns(667.0));
        assert!(!above.1 && below.1);
        assert!(below.0 > above.0);
    }

    #[test]
    fn type_hides_before_name_drops_below_220px() {
        for w in 300..1400 {
            let (name, no_type) = columns(w as f32);
            if !no_type {
                assert!(name >= NAME_MIN_W, "{w}px keeps Type with a {name}px Name");
                assert!(w as f32 >= TYPE_MIN_LIST);
            }
        }
        // Sweeping narrower, Type goes once and never comes back.
        let mut gone = false;
        for w in (200..1600).rev() {
            let no_type = columns(w as f32).1;
            assert!(!gone || no_type, "Type came back at {w}px");
            gone = no_type;
        }
        // 913px window with the full sidebar: Type shows.
        assert!(!columns(913.0 - 232.0).1);
        assert!(columns(500.0).1);
    }

    #[test]
    fn a_search_narrows_what_operations_act_on() {
        let mut c = setup("search-ops");
        c.listing.move_cursor(Nav::Home, false, 1);
        c.listing.move_cursor(Nav::Down, true, 1);
        c.listing.move_cursor(Nav::Down, true, 1);
        assert_eq!(c.names().len(), 3);
        c.listing.set_query("a.jpg");
        assert_eq!(c.names(), vec![OsString::from("a.jpg")]);
        assert_eq!(c.listing.selection().len(), 1);
    }

    #[test]
    fn hiding_dotfiles_drops_them_from_the_selection() {
        let mut c = setup("hidden");
        fs::write(c.dir.join(".secret"), b"s").unwrap();
        c.reload().unwrap();
        c.toggle_hidden();
        select(&mut c, ".secret");
        c.toggle_hidden();
        assert!(c.rows().iter().all(|r| r.name != ".secret"));
        assert!(c.listing.selection().is_empty());
    }

    fn typed(c: &mut Controller, text: &str) {
        for ch in text.chars() {
            c.key(&ch.to_string(), false, false);
        }
    }

    #[test]
    fn slash_opens_search_and_typing_filters() {
        let mut c = setup("search-type");
        assert_eq!(c.key("/", false, false), Action::Redraw);
        assert!(c.searching);
        typed(&mut c, "b.j");
        assert_eq!(c.listing.query(), "b.j");
        let names: Vec<String> = c.rows().iter().map(|r| r.name.to_string()).collect();
        assert_eq!(names, ["b.jpg"]);
        // Arrows still move through the matches.
        c.key(keys::DOWN, false, false);
        assert_eq!(c.listing.cursor(), Some(&OsString::from("b.jpg")));
    }

    #[test]
    fn backspace_pops_then_closes_and_escape_clears() {
        let mut c = setup("search-keys");
        c.key("f", true, false);
        assert!(c.searching);
        typed(&mut c, "ab");
        c.key(keys::BACKSPACE, false, false);
        assert_eq!(c.listing.query(), "a");
        c.key(keys::BACKSPACE, false, false);
        assert_eq!(c.listing.query(), "");
        assert!(c.searching, "an empty field stays open");
        c.key(keys::BACKSPACE, false, false);
        assert!(!c.searching);
        c.key("/", false, false);
        typed(&mut c, "zz");
        c.key(keys::ESCAPE, false, false);
        assert!(!c.searching);
        assert_eq!(c.listing.query(), "");
        assert_eq!(c.rows().len(), 3);
    }

    #[test]
    fn typing_drops_selected_names_the_search_hides() {
        let mut c = setup("search-drop");
        select(&mut c, "a.jpg");
        c.key("/", false, false);
        typed(&mut c, "b.jpg");
        assert!(c.listing.selection().is_empty());
        assert!(c.names().is_empty());
    }

    #[test]
    fn the_status_bar_counts_matches_of_searchable_items() {
        let mut c = setup("search-status");
        c.key("/", false, false);
        typed(&mut c, "b.j");
        let total = c.listing.searchable_count();
        assert_eq!(c.status_left(), format!("1 of {}", ops::count(total)));
        c.listing.set_show_hidden(true);
        assert_eq!(
            c.listing.searchable_count(),
            total + c.listing.hidden_count()
        );
    }

    #[test]
    fn the_clear_button_keeps_the_field_open() {
        let mut c = setup("search-clear");
        c.open_search();
        typed(&mut c, "dune");
        c.clear_search();
        assert!(c.searching);
        assert_eq!(c.listing.query(), "");
        assert_eq!(c.dir_name(), "Pictures");
    }

    #[test]
    fn navigating_clears_the_search() {
        let mut c = setup("search-nav");
        c.key("/", false, false);
        typed(&mut c, "raw");
        let ticket = c.begin_navigate(c.dir.join("raw-exports"));
        c.land(ticket.perform());
        assert!(!c.searching);
        assert_eq!(c.listing.query(), "");
    }

    #[test]
    fn a_search_with_no_matches_says_where_it_looked() {
        let mut c = setup("search-nomatch");
        fs::write(c.dir.join(".secret.jpg"), b"s").unwrap();
        c.reload().unwrap();
        assert_eq!(c.no_match_text(), None);
        c.key("/", false, false);
        typed(&mut c, "zz");
        assert!(c.rows().is_empty());
        let (title, note) = c.no_match_text().unwrap();
        assert_eq!(title, "Nothing matches \"zz\"");
        assert!(note.contains("~/Pictures"), "{note}");
        assert!(note.ends_with(" (hidden files excluded)"), "{note}");
        c.toggle_hidden();
        assert!(!c.no_match_text().unwrap().1.contains("hidden"));
    }

    #[test]
    fn search_reaches_hidden_files_only_when_they_show() {
        let mut c = setup("search-hidden");
        fs::write(c.dir.join(".b-notes"), b"s").unwrap();
        c.reload().unwrap();
        c.key("/", false, false);
        typed(&mut c, "b");
        let names = |c: &Controller| -> Vec<String> {
            c.rows().iter().map(|r| r.name.to_string()).collect()
        };
        assert_eq!(names(&c), ["b.jpg"]);
        c.toggle_hidden();
        assert_eq!(names(&c), [".b-notes", "b.jpg"]);
        assert_eq!(c.listing.query(), "b", "showing hidden keeps the query");
    }

    #[test]
    fn search_filters_the_grid_too() {
        let mut c = setup("search-grid");
        c.grid = true;
        c.columns.set(4);
        c.key("/", false, false);
        typed(&mut c, "a.j");
        let rows = c.rows();
        let lines = c.grid_lines(&rows);
        assert_eq!(lines.len(), 1);
        assert_eq!(lines[0].n, 1);
        assert_eq!(rows[0].name, "a.jpg");
    }

    #[test]
    fn slash_and_ime_text_while_searching() {
        let mut c = setup("search-ime");
        c.key("/", false, false);
        // "/" can't be in a name; a second one is swallowed, not typed.
        assert_eq!(c.key("/", false, false), Action::None);
        assert_eq!(c.listing.query(), "");
        // An IME or dead key commits several characters in one event.
        assert_eq!(c.key("写真", false, false), Action::Redraw);
        assert_eq!(c.key("e\u{301}", false, false), Action::Redraw);
        assert_eq!(c.listing.query(), "写真e\u{301}");
        // Backspace takes back the accented letter whole.
        c.key(keys::BACKSPACE, false, false);
        assert_eq!(c.listing.query(), "写真");
    }

    #[test]
    fn only_printable_keys_count_as_typing() {
        let c = setup("search-where");
        assert_eq!(c.short_dir(), "~/Pictures");
        assert!(!is_typed(keys::UP));
        assert!(!is_typed(keys::ESCAPE));
        assert!(is_typed("k"));
    }

    #[test]
    fn shown_hidden_files_stay_shown_after_navigating() {
        let mut c = setup("hidden-nav");
        c.toggle_hidden();
        let sub = c.dir.join("raw-exports");
        fs::write(sub.join(".x"), b"x").unwrap();
        let ticket = c.begin_navigate(sub);
        c.land(ticket.perform());
        assert!(c.listing.show_hidden());
        assert!(c.rows().iter().any(|r| r.name == ".x"));
    }

    #[test]
    fn long_names_lose_their_middle_not_their_ending() {
        assert_eq!(
            elide_middle("dune-sunset-0412.jpg", 17),
            "dune-sun…0412.jpg"
        );
        assert_eq!(elide_middle("short.jpg", 17), "short.jpg");
    }

    #[test]
    fn eliding_keeps_graphemes_whole_and_counts_wide_glyphs_twice() {
        use unicode_width::UnicodeWidthStr;
        // e + combining acute is one grapheme; it is never split.
        let name = "e\u{301}".repeat(20) + ".txt";
        let cut = elide_middle(&name, 11);
        assert!(!cut.contains("…\u{301}"));
        assert!(cut.width() <= 11);
        // Ten CJK glyphs are 20 cells wide.
        let cjk = "写真写真写真写真写真.jpg";
        let cut = elide_middle(cjk, 12);
        assert!(cut.width() <= 12, "{cut}");
        assert!(cut.ends_with(".jpg"));
        assert_eq!(elide_middle("写真.jpg", 12), "写真.jpg");
        assert!(name_chars(567.0) > name_chars(500.0));
    }

    #[test]
    fn tile_drags_stay_on_their_line_sideways() {
        // Column 1 of 4: dx may reach columns 0..=3, no further.
        assert_eq!(tile_offset(1, 9, 0, 4), 2);
        assert_eq!(tile_offset(1, -9, 0, 4), -1);
        assert_eq!(tile_offset(1, 1, 2, 4), 9);
        assert_eq!(tile_offset(0, 3, 0, 1), 0);
    }

    #[test]
    fn list_resizes_rebuild_only_when_the_name_budget_moves() {
        let c = setup("refit");
        assert!(c.refit_list(800.0));
        assert!(!c.refit_list(800.0));
        let same = (800..900)
            .find(|w| name_chars(*w as f32) != name_chars(800.0))
            .unwrap();
        assert!(!c.refit_list(same as f32 - 1.0));
        assert!(c.refit_list(same as f32));
    }

    #[test]
    fn modifier_clicks_act_on_press() {
        let mut c = setup("mod-press");
        let at = |c: &Controller, n: &str| c.rows().iter().position(|r| r.name == n).unwrap();
        let (a, b) = (at(&c, "a.jpg"), at(&c, "b.jpg"));
        c.pointer(a, 0, 0, false, Mods::default());
        c.pointer(a, 2, 0, false, Mods::default());
        let ctrl = Mods {
            ctrl: true,
            shift: false,
        };
        c.pointer(b, 0, 0, false, ctrl);
        assert_eq!(
            c.listing.selection().len(),
            2,
            "ctrl press adds before release"
        );
        c.pointer(b, 2, 0, false, ctrl);
        assert_eq!(c.listing.selection().len(), 2, "release keeps it");
        c.pointer(b, 0, 0, false, ctrl);
        assert_eq!(c.listing.selection().len(), 2, "off waits for release");
        c.pointer(b, 2, 0, false, ctrl);
        assert_eq!(c.listing.selection().len(), 1, "ctrl release toggles off");
        let first = at(&c, "raw-exports");
        let shift = Mods {
            ctrl: false,
            shift: true,
        };
        c.pointer(first, 0, 0, false, shift);
        assert_eq!(
            c.listing.selection().len(),
            3,
            "shift press selects the range"
        );
    }

    #[test]
    fn ctrl_plus_does_nothing_in_the_list() {
        let mut c = setup("tile-list");
        assert_eq!(c.key("=", true, false), Action::None);
        c.set_grid(true);
        assert_eq!(c.key("=", true, false), Action::Redraw);
    }

    #[test]
    fn remembered_settings_restore_the_whole_view() {
        let mut c = setup("prefs-restore");
        let p = crate::prefs::ViewPrefs {
            grid: true,
            tile_step: 2,
            show_hidden: true,
            trash_asked: true,
            skip_trash_confirm: true,
        };
        c.apply_prefs(p);
        assert_eq!(c.prefs(), p);
        c.set_grid(false);
        assert!(!c.prefs().grid);
    }

    #[test]
    fn remembered_settings_never_make_the_app_compact() {
        let mut c = setup("prefs-comfortable");
        let old = "view = list\ntile = medium\ndensity = compact\n";
        c.apply_prefs(crate::prefs::ViewPrefs::parse(old));
        assert!(!c.compact, "a density line from an older build is ignored");
        assert_eq!(c.tile(), 152.0, "comfortable medium tiles");
    }

    fn select(c: &mut Controller, name: &str) {
        c.listing.select_only(name.into());
    }

    #[test]
    fn ctrl_x_dims_rows_and_ctrl_v_moves_once() {
        let mut c = setup("cut");
        select(&mut c, "a.jpg");
        assert_eq!(c.key("x", true, false), Action::Redraw);
        let rows = c.rows();
        assert!(rows.iter().find(|r| r.name == "a.jpg").unwrap().cut);
        assert!(!rows.iter().find(|r| r.name == "b.jpg").unwrap().cut);
        c.dir = c.dir.join("raw-exports");
        let Action::Run(job) = c.key("\u{16}", true, false) else {
            panic!("paste should run a job")
        };
        assert_eq!(job.kind, JobKind::Move);
        assert!(c.clip.clip().is_some(), "paste start keeps the cut");
        c.finish(
            &job,
            &Outcome {
                done: 1,
                ..Outcome::default()
            },
        );
        assert!(c.clip.clip().is_none(), "a successful move uses the cut up");
        assert_eq!(c.key("v", true, false), Action::None);
    }

    #[test]
    fn a_cut_survives_refused_failed_and_same_folder_pastes() {
        let mut c = setup("cut-kept");
        select(&mut c, "a.jpg");
        c.key("x", true, false);
        // Same folder: nothing runs, the cut stays.
        assert_eq!(c.key("v", true, false), Action::None);
        assert!(c.clip.clip().is_some());
        c.dir = c.dir.join("raw-exports");
        let Action::Run(job) = c.key("v", true, false) else {
            panic!("paste should run a job")
        };
        let failed = Outcome {
            failed: vec!["a.jpg: disk full".into()],
            ..Outcome::default()
        };
        c.finish(&job, &failed);
        assert!(c.clip.clip().is_some(), "a failed move keeps the cut");
        let refused = Outcome {
            refused: vec![("a.jpg".into(), Vec::new())],
            ..Outcome::default()
        };
        c.finish(&job, &refused);
        assert!(c.clip.clip().is_some(), "a refused move keeps the cut");
        let cancelled = Outcome {
            cancelled: true,
            ..Outcome::default()
        };
        c.finish(&job, &cancelled);
        assert!(c.clip.clip().is_some(), "a cancelled move keeps the cut");
    }

    #[test]
    fn ctrl_c_pastes_again_and_esc_clears_a_cut() {
        let mut c = setup("copy");
        select(&mut c, "b.jpg");
        c.key("c", true, false);
        c.dir = c.dir.join("raw-exports");
        assert!(matches!(
            c.key("v", true, false),
            Action::Run(Job {
                kind: JobKind::Copy,
                ..
            })
        ));
        assert!(matches!(c.key("v", true, false), Action::Run(_)));
        c.key("x", true, false);
        c.key(keys::ESCAPE, false, false);
        assert!(c.clip.clip().is_none());
    }

    #[test]
    fn drag_onto_a_folder_follows_modifiers() {
        let mut c = setup("dnd");
        // Rows: raw-exports, a.jpg, b.jpg (folders first).
        let none = Mods::default();
        c.pointer(1, 0, 0, false, none);
        assert_eq!(
            c.pointer(1, 1, 0, false, none),
            Action::None,
            "below threshold"
        );
        c.pointer(1, 1, -1, true, none);
        assert!(c.rows()[0].drop);
        assert_eq!(c.drag_label().unwrap().0, "Move 1 item to raw-exports");
        let ctrl = Mods {
            ctrl: true,
            shift: false,
        };
        c.pointer(1, 1, -1, true, ctrl);
        assert_eq!(c.drag_label().unwrap().0, "Copy 1 item to raw-exports");
        let Action::Run(job) = c.pointer(1, 2, -1, true, ctrl) else {
            panic!("drop should run")
        };
        assert_eq!(job.kind, JobKind::Copy);
        assert!(job.dest.ends_with("raw-exports"));
        assert!(c.drag.is_none());
    }

    #[test]
    fn ctrl_on_a_selected_item_unselects_on_release_unless_it_dragged() {
        let ctrl = Mods {
            ctrl: true,
            shift: false,
        };
        let mut c = setup("ctrl-untoggle");
        let sel = |c: &Controller| c.listing.selection().iter().cloned().collect::<Vec<_>>();
        c.pointer(1, 0, 0, false, ctrl);
        c.pointer(1, 2, 0, false, ctrl);
        c.pointer(2, 0, 0, false, ctrl);
        c.pointer(2, 2, 0, false, ctrl);
        assert_eq!(sel(&c), [OsString::from("a.jpg"), "b.jpg".into()]);
        // Ctrl-drag the selected a.jpg onto raw-exports: both go, and a.jpg
        // stays selected.
        c.pointer(1, 0, 0, false, ctrl);
        assert_eq!(sel(&c).len(), 2, "not dropped on press");
        c.pointer(1, 1, -1, true, ctrl);
        let Action::Run(job) = c.pointer(1, 2, -1, true, ctrl) else {
            panic!("the drag runs")
        };
        assert_eq!(job.kind, JobKind::Copy);
        assert_eq!(job.names.len(), 2, "the Ctrl-pressed item went too");
        assert_eq!(sel(&c).len(), 2);
        // A plain Ctrl-click on it still unselects, on release.
        c.pointer(1, 0, 0, false, ctrl);
        assert_eq!(sel(&c).len(), 2);
        c.pointer(1, 2, 0, false, ctrl);
        assert_eq!(sel(&c), [OsString::from("b.jpg")]);
    }

    #[test]
    fn dropping_on_a_file_does_nothing() {
        let mut c = setup("dnd-file");
        c.pointer(1, 0, 0, false, Mods::default());
        c.pointer(1, 1, 1, true, Mods::default());
        assert!(c.rows().iter().all(|r| !r.drop));
        assert_eq!(c.pointer(1, 2, 1, true, Mods::default()), Action::Redraw);
    }

    #[test]
    fn return_on_a_photo_opens_the_viewer_and_esc_lands_on_the_last_one() {
        let mut c = setup("viewer-open");
        fs::write(c.dir.join("notes.txt"), b"n").unwrap();
        c.reload().unwrap();
        select(&mut c, "notes.txt");
        assert_eq!(c.key(keys::RETURN, false, false), Action::None, "not media");
        assert!(c.viewer.is_none());
        select(&mut c, "a.jpg");
        assert_eq!(c.key(keys::RETURN, false, false), Action::Redraw);
        let v = c.viewer.as_ref().unwrap();
        assert_eq!(v.items, vec![OsString::from("a.jpg"), "b.jpg".into()]);
        assert_eq!(c.key("l", false, false), Action::Redraw);
        assert_eq!(
            c.key(keys::DELETE, false, false),
            Action::None,
            "browser keys sleep"
        );
        assert_eq!(c.key("q", false, false), Action::Redraw);
        assert!(c.viewer.is_none());
        assert_eq!(c.listing.cursor(), Some(&OsString::from("b.jpg")));
        assert!(c.listing.selection().contains(&OsString::from("b.jpg")));
    }

    #[test]
    fn filmstrip_tiles_jump_to_their_item() {
        let mut c = setup("viewer-strip");
        select(&mut c, "a.jpg");
        c.key(keys::RETURN, false, false);
        assert_eq!(c.viewer_go(1), Action::Redraw);
        assert_eq!(c.viewer.as_ref().unwrap().current(), "b.jpg");
        assert_eq!(c.viewer_go(9), Action::Redraw, "past the end clamps");
        assert_eq!(c.viewer.as_ref().unwrap().current(), "b.jpg");
    }

    #[test]
    fn the_info_panel_lists_the_file_facts() {
        let f = file_facts("a.jpg", 4_600, Some((320, 180)), "15:30".into());
        let f: Vec<(&str, &str)> = f.iter().map(|(k, v)| (k.as_str(), v.as_str())).collect();
        assert_eq!(
            f,
            [
                ("Size", "4.6 KB"),
                ("Type", "JPEG image"),
                ("Dimensions", "320 × 180"),
                ("Modified", "15:30")
            ]
        );
        assert_eq!(file_facts("x.mp4", 0, None, String::new()).len(), 3);
        let home = Path::new("/home/sky");
        assert_eq!(
            tilde(Path::new("/home/sky/Pictures"), Some(home)),
            "~/Pictures"
        );
        assert_eq!(tilde(home, Some(home)), "~");
        assert_eq!(tilde(Path::new("/media/x"), Some(home)), "/media/x");
    }

    #[test]
    fn decode_reads_real_stills_and_explains_the_rest() {
        use crate::picture::{full, tile, Why};
        let dir = std::env::temp_dir().join(format!("robin-decode-{}", std::process::id()));
        crate::thumbs::write_fixture_stills(&dir).unwrap();
        let img = full(&dir.join("dune-sunset-0412.jpg")).unwrap();
        assert_eq!(img.dimensions(), (320, 180));
        let small = tile(&dir.join("speeder-frame.png"), 144, None).unwrap();
        assert_eq!(small.dimensions(), (144, 81));
        fs::write(dir.join("x.heic"), b"ftypheic").unwrap();
        assert_eq!(full(&dir.join("x.heic")).unwrap_err(), Why::Unreadable);
        fs::write(dir.join("x.mp4"), b"v").unwrap();
        assert_eq!(full(&dir.join("x.mp4")).unwrap_err(), Why::Video);
        assert_eq!(Why::Video.line(), "No built-in video player yet");
        assert_eq!(Why::Unreadable.line(), "Can't read this file");
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn drags_pan_only_past_fit_and_the_chrome_hides_until_the_mouse_moves() {
        let mut c = setup("viewer-pan");
        select(&mut c, "a.jpg");
        c.key(keys::RETURN, false, false);
        c.view_geom.set(((1000.0, 800.0), (500.0, 400.0)));
        assert_eq!(c.viewer_pan(30.0, 0.0), Action::None, "at fit");
        assert!(!c.viewer.as_ref().unwrap().chrome_hidden);
        for _ in 0..4 {
            c.key("+", false, false);
        }
        assert!(c.viewer.as_ref().unwrap().chrome_hidden, "zoomed hides it");
        assert_eq!(c.viewer_pan(30.0, -10.0), Action::Redraw);
        assert_eq!(c.viewer.as_ref().unwrap().pan, (30.0, -10.0));
        assert_eq!(c.viewer_wake(), Action::Redraw);
        assert!(!c.viewer.as_ref().unwrap().chrome_hidden);
        assert_eq!(c.viewer_wake(), Action::None, "already shown");
        assert_eq!(c.viewer_idle(), Action::Redraw);
        assert!(c.viewer.as_ref().unwrap().chrome_hidden);
        c.key("0", false, false);
        c.viewer_wake();
        assert_eq!(c.viewer_idle(), Action::None, "at fit it stays");
    }

    #[test]
    fn a_file_deleted_while_open_leaves_the_viewer() {
        let mut c = setup("viewer-deleted");
        select(&mut c, "a.jpg");
        c.key(keys::RETURN, false, false);
        fs::remove_file(c.dir.join("a.jpg")).unwrap();
        c.refresh().unwrap();
        let v = c.viewer.as_ref().unwrap();
        assert_eq!(v.items, vec![OsString::from("b.jpg")], "not cached");
        assert_eq!(v.current(), "b.jpg");
        fs::remove_file(c.dir.join("b.jpg")).unwrap();
        c.refresh().unwrap();
        assert!(c.viewer.is_none(), "nothing left to show");
    }

    #[test]
    fn viewer_keys_leave_ctrl_to_the_app_and_shift_space_goes_back() {
        let mut c = setup("viewer-keys");
        select(&mut c, "a.jpg");
        c.key(keys::RETURN, false, false);
        c.key(" ", false, false);
        assert_eq!(c.viewer.as_ref().unwrap().current(), "b.jpg");
        c.key(" ", false, true);
        assert_eq!(c.viewer.as_ref().unwrap().current(), "a.jpg");
        assert_eq!(
            c.key("r", true, false),
            Action::None,
            "Ctrl+R falls through"
        );
        assert_eq!(
            c.key("q", true, false),
            Action::None,
            "Ctrl+Q falls through"
        );
        assert_eq!(c.viewer.as_ref().unwrap().turns, 0);
        assert!(c.viewer.is_some());
    }

    #[test]
    fn navigating_away_closes_the_viewer() {
        let mut c = setup("viewer-nav");
        fs::create_dir(c.dir.join("sub")).unwrap();
        select(&mut c, "a.jpg");
        c.key(keys::RETURN, false, false);
        let t = c.ticket(c.dir.join("sub"), ReadKind::Navigate);
        c.land(t.perform());
        assert!(c.viewer.is_none());
    }

    #[test]
    fn return_on_a_video_hands_it_to_the_default_player() {
        let mut c = setup("viewer-video");
        fs::write(c.dir.join("clip.mp4"), b"v").unwrap();
        c.reload().unwrap();
        select(&mut c, "clip.mp4");
        c.key(keys::RETURN, false, false);
        let want = Action::Launch(vec!["xdg-open".into(), c.dir.join("clip.mp4").into()]);
        assert_eq!(c.viewer_external(), want);
        assert_eq!(c.key(keys::RETURN, false, false), want);
    }

    #[test]
    fn dont_ask_again_starts_ticked_after_the_first_answer_and_then_skips_the_confirm() {
        let mut c = setup("dont-ask");
        select(&mut c, "a.jpg");
        let Action::Open(d) = c.key(keys::DELETE, false, false) else {
            panic!("asks the first time")
        };
        c.open_dialog(d);
        assert!(!c.dont_ask, "off before the first use");
        assert!(matches!(c.key(keys::RETURN, false, false), Action::Run(_)));
        assert!(c.trash_asked && !c.skip_trash_confirm);
        // Second time: the box starts ticked; Tab reaches it and Space
        // unticks it, so this answer keeps asking.
        let Action::Open(d) = c.key(keys::DELETE, false, false) else {
            panic!("still asks")
        };
        c.open_dialog(d);
        assert!(c.dont_ask, "on by default after the first use");
        c.key(keys::TAB, false, false);
        assert_eq!(c.focus, 2, "the box is the third stop");
        c.key(" ", false, false);
        assert!(!c.dont_ask);
        assert_eq!(c.button(2), Action::Redraw, "a click ticks it again");
        assert!(c.dont_ask);
        assert!(matches!(c.button(1), Action::Run(_)));
        assert!(c.skip_trash_confirm);
        assert!(c.prefs().skip_trash_confirm, "remembered");
        // Now Del trashes straight away; Shift+Del still asks.
        assert!(matches!(
            c.key(keys::DELETE, false, false),
            Action::Run(Job {
                kind: JobKind::Trash,
                ..
            })
        ));
        assert!(matches!(
            c.key(keys::DELETE, false, true),
            Action::Open(Dialog::Delete(_))
        ));
        // Cancel never records an answer.
        let mut fresh = setup("dont-ask-cancel");
        select(&mut fresh, "a.jpg");
        let Action::Open(d) = fresh.key(keys::DELETE, false, false) else {
            panic!()
        };
        fresh.open_dialog(d);
        fresh.button(2);
        fresh.button(0);
        assert!(!fresh.trash_asked && !fresh.skip_trash_confirm);
    }

    #[test]
    fn state_buttons_act_on_the_folder() {
        let mut c = setup("state-actions");
        let dir = c.dir.clone();
        assert_eq!(
            c.state_action("back"),
            Action::Navigate(dir.parent().unwrap().to_path_buf())
        );
        assert_eq!(
            c.state_action("home"),
            Action::Navigate(c.env.home.clone().unwrap())
        );
        // Open as admin only builds the command; nothing runs here.
        let Action::Launch(argv) = c.state_action("admin") else {
            panic!("admin launches")
        };
        assert_eq!(argv[0], "pkexec");
        assert_eq!(argv.last().unwrap(), dir.as_os_str());
        assert_eq!(c.state_action("mount"), Action::None);
        // New folder: made, listed and selected; a second one is numbered.
        assert_eq!(c.state_action("new-folder"), Action::Redraw);
        assert!(dir.join("New folder").is_dir());
        assert_eq!(c.state_action("new-folder"), Action::Redraw);
        assert!(dir.join("New folder 2").is_dir());
        let picked: Vec<OsString> = c.listing.selection().iter().cloned().collect();
        assert_eq!(picked, [OsString::from("New folder 2")]);
        // No-results: search hidden files keeps the query; Clear empties it.
        fs::write(dir.join(".kotlin-notes"), b"k").unwrap();
        c.reload().unwrap();
        c.open_search();
        c.listing.set_query("kotlin");
        assert!(c.listing.view().is_empty());
        c.state_action("search-hidden");
        assert!(c.listing.show_hidden());
        assert_eq!(c.listing.query(), "kotlin");
        assert_eq!(c.listing.view().len(), 1);
        c.state_action("clear");
        assert_eq!(c.listing.query(), "");
        let _ = fs::remove_dir_all(c.env.home.unwrap());
    }

    #[test]
    fn an_unread_folder_draws_placeholders_even_with_rows_and_a_selection() {
        let mut c = setup("placeholders");
        let first = c.listing.view()[0].name.clone();
        c.listing.select_only(first);
        assert_eq!(super::pane_state(&c), "files");
        c.loaded = false;
        assert_eq!(super::pane_state(&c), "loading");
        let _ = fs::remove_dir_all(c.env.home.unwrap());
    }

    #[test]
    fn the_status_bar_says_reading_while_rows_stream_in() {
        let mut c = setup("reading");
        assert_eq!(c.status_line(false), "3 items");
        assert_eq!(c.status_line(true), "Reading… 3 items");
        c.loaded = false;
        assert_eq!(c.status_line(false), "Reading…");
        c.loaded = true;
        let many: Vec<crate::model::Entry> = (0..18_240)
            .map(|i| crate::model::Entry {
                name: format!("f{i}").into(),
                is_dir: false,
                is_symlink: false,
                len: 1,
                mtime: std::time::UNIX_EPOCH,
                mode: 0o100644,
            })
            .collect();
        c.listing.set_entries(many);
        assert_eq!(c.status_line(true), "Reading… 18 240 items");
        assert_eq!(c.status_line(false), "18 240 items");
        let _ = fs::remove_dir_all(c.env.home.unwrap());
    }

    #[test]
    fn a_denied_folder_names_its_owner_and_mode() {
        assert_eq!(mode_text(0o040700), "drwx------");
        assert_eq!(mode_text(0o041777), "drwxrwxrwt");
        assert_eq!(mode_text(0o100644), "-rw-r--r--");
        assert_eq!(mode_text(0o106755), "-rwsr-sr-x");
        assert_eq!(mode_text(0o040000), "d---------");
        let c = setup("denied-note");
        let locked = c.dir.join("raw-exports");
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&locked, fs::Permissions::from_mode(0o700)).unwrap();
        let note = denied_note(&locked);
        assert!(note.starts_with("Owned by "), "{note}");
        assert!(note.ends_with("(drwx------)"), "{note}");
        assert_eq!(user_name(0), "root");
        assert_eq!(denied_note(&c.dir.join("missing")), "");
        let _ = fs::remove_dir_all(c.env.home.unwrap());
    }

    #[test]
    fn del_confirms_from_the_keyboard() {
        let mut c = setup("del");
        select(&mut c, "a.jpg");
        let Action::Open(d) = c.key(keys::DELETE, false, false) else {
            panic!()
        };
        assert_eq!(d, Dialog::Trash(vec!["a.jpg".into()]));
        c.open_dialog(d);
        assert_eq!(c.focus, 1, "Trash is the default");
        assert!(matches!(
            c.key(keys::RETURN, false, false),
            Action::Run(Job {
                kind: JobKind::Trash,
                ..
            })
        ));
        assert!(c.dialog.is_none());
    }

    #[test]
    fn shift_del_needs_tab_and_the_arm_delay() {
        let mut c = setup("shiftdel");
        select(&mut c, "a.jpg");
        let Action::Open(d) = c.key(keys::DELETE, false, true) else {
            panic!()
        };
        c.open_dialog(d);
        assert_eq!(c.focus, 0, "Cancel is the default");
        c.key(keys::TAB, false, false);
        assert_eq!(c.focus, 1);
        assert_eq!(
            c.key(keys::RETURN, false, false),
            Action::None,
            "not armed yet"
        );
        c.armed = true;
        assert!(matches!(
            c.key(keys::RETURN, false, false),
            Action::Run(Job {
                kind: JobKind::Delete,
                ..
            })
        ));
    }

    #[test]
    fn escape_cancels_a_dialog() {
        let mut c = setup("esc");
        select(&mut c, "a.jpg");
        c.open_dialog(Dialog::Trash(vec!["a.jpg".into()]));
        assert_eq!(c.key(keys::ESCAPE, false, false), Action::Redraw);
        assert!(c.dialog.is_none());
    }

    /// Trash a.jpg from Pictures and open the Trash view.
    fn in_trash_view(label: &str) -> Controller {
        let mut c = setup(label);
        let job = Job::new(JobKind::Trash, &c.dir, vec!["a.jpg".into()], &c.dir);
        ops::run(&job, &c.env, &Cancel::new(), &mut |_| {});
        c.dir = c.trash_files();
        c.reload().unwrap();
        c
    }

    #[test]
    fn the_trash_view_shows_where_items_came_from_and_when() {
        let c = in_trash_view("trash-view");
        assert!(c.in_trash());
        let rows = c.rows();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].name, "a.jpg");
        assert_eq!(rows[0].kind, "~/Pictures");
        assert_eq!(rows[0].size, "");
        assert_eq!(
            rows[0].modified.len(),
            "2026-10-29 10:00".len(),
            "{}",
            rows[0].modified
        );
        let _ = fs::remove_dir_all(c.env.home.unwrap());
    }

    #[test]
    fn restore_puts_the_selection_back_and_del_in_the_trash_is_permanent() {
        let mut c = in_trash_view("trash-restore");
        assert_eq!(c.restore_selected(), Action::None, "nothing selected");
        c.listing.select_only("a.jpg".into());
        let Action::Run(job) = c.restore_selected() else {
            panic!("no restore job");
        };
        assert_eq!(job.kind, JobKind::Restore);
        assert_eq!(
            job.restore[0].original,
            c.env.home.clone().unwrap().join("Pictures/a.jpg")
        );
        assert!(matches!(c.remove(false), Action::Open(Dialog::Delete(_))));
        let _ = fs::remove_dir_all(c.env.home.unwrap());
    }

    #[test]
    fn items_without_trashinfo_say_unknown_and_restore_skips_them() {
        let mut c = in_trash_view("trash-noinfo");
        fs::write(c.trash_files().join("stray.txt"), "s").unwrap();
        fs::write(c.trash_files().join("stray2.txt"), "s").unwrap();
        c.reload().unwrap();
        let rows = c.rows();
        let stray = rows.iter().find(|r| r.name == "stray.txt").unwrap();
        assert_eq!(
            (stray.kind.as_str(), stray.modified.as_str()),
            ("Unknown", "")
        );
        assert!(!stray.cut, "not greyed");
        c.listing.select_only("a.jpg".into());
        assert_eq!(c.trash_note(), "");
        c.listing.select_only("stray.txt".into());
        assert_eq!(
            c.trash_note(),
            "1 item has no saved location and can't be restored. Drag it out to keep it."
        );
        assert_eq!(c.restore_selected(), Action::None);
        c.listing.move_cursor(crate::model::Nav::Home, false, 1);
        c.listing.move_cursor(crate::model::Nav::Down, true, 1);
        c.listing.move_cursor(crate::model::Nav::Down, true, 1);
        assert!(c.trash_note().starts_with("2 items have no saved location"));
        let Action::Run(job) = c.restore_selected() else {
            panic!("restore should still run for the rest");
        };
        let names: Vec<_> = job.restore.iter().map(|t| t.stored.clone()).collect();
        assert_eq!(names, vec![OsString::from("a.jpg")]);
        assert!(matches!(c.remove(false), Action::Open(Dialog::Delete(_))));
        let _ = fs::remove_dir_all(c.env.home.unwrap());
    }

    #[test]
    fn the_skipped_line_counts_both_kinds_and_lists_paths_in_the_tip() {
        let mut scan = crate::stats::Scan::default();
        assert_eq!(skipped_line(&scan), (String::new(), String::new()));
        scan.skipped = vec!["/h/a".into(), "/h/b".into(), "/h/c".into()];
        scan.other_drives = vec!["/h/mnt".into(), "/h/ssd".into()];
        let (line, tip) = skipped_line(&scan);
        assert_eq!(
            line,
            "3 folders skipped (no access) · 2 other drives not counted"
        );
        assert!(tip.contains("/h/b") && tip.contains("/h/ssd"));
        scan.skipped.truncate(1);
        scan.other_drives.clear();
        assert_eq!(skipped_line(&scan).0, "1 folder skipped (no access)");
    }

    #[test]
    fn clear_thumbnail_cache_asks_first_and_hands_over_the_cache_root() {
        let mut c = in_trash_view("thumbs-clear");
        let cache = c.env.home.clone().unwrap().join(".cache");
        let d = Dialog::ClearThumbs(cache.clone());
        assert_eq!(d.overlay(), "delete", "the red confirm dialog");
        c.open_dialog(d);
        assert_eq!(c.focus, 0, "Cancel is the default");
        assert_eq!(c.button(1), Action::None, "not armed yet");
        assert_eq!(c.button(0), Action::Redraw, "Cancel deletes nothing");
        assert!(c.dialog.is_none());
        c.open_dialog(Dialog::ClearThumbs(cache.clone()));
        c.armed = true;
        assert_eq!(c.button(1), Action::ClearThumbs(cache));
        let _ = fs::remove_dir_all(c.env.home.unwrap());
    }

    #[test]
    fn empty_trash_asks_then_deletes_everything_and_its_info() {
        let mut c = in_trash_view("trash-empty");
        let Action::Open(d) = c.empty_trash() else {
            panic!("no dialog");
        };
        assert_eq!(d, Dialog::EmptyTrash(vec!["a.jpg".into()]));
        c.open_dialog(d);
        assert_eq!(c.focus, 0, "Cancel is the default");
        assert_eq!(c.button(1), Action::None, "not armed yet");
        c.armed = true;
        let Action::Run(job) = c.button(1) else {
            panic!("no job");
        };
        assert_eq!(
            (job.kind, job.src.clone()),
            (JobKind::Delete, c.trash_files())
        );
        assert!(
            job.empty_trash,
            "Empty Trash tidies info and directorysizes"
        );
        ops::run(&job, &c.env, &Cancel::new(), &mut |_| {});
        c.reload().unwrap();
        assert!(c.listing.entries().is_empty());
        assert!(!c.env.home_trash.join("info/a.jpg.trashinfo").exists());
        let _ = fs::remove_dir_all(c.env.home.unwrap());
    }

    #[test]
    fn empty_trash_does_nothing_outside_the_trash_or_when_empty() {
        let c = setup("trash-outside");
        assert_eq!(c.empty_trash(), Action::None);
        assert_eq!(c.restore_selected(), Action::None);
        let _ = fs::remove_dir_all(c.env.home.unwrap());
    }

    #[test]
    fn trash_then_undo_puts_the_file_back() {
        let mut c = setup("undo");
        let job = Job::new(JobKind::Trash, &c.dir, vec!["a.jpg".into()], &c.dir);
        let out = ops::run(&job, &c.env, &Cancel::new(), &mut |_| {});
        assert!(out.toast(JobKind::Trash).undo);
        assert!(!c.dir.join("a.jpg").exists());
        let back = ops::run(
            &Job::restore(out.trashed.clone()),
            &c.env,
            &Cancel::new(),
            &mut |_| {},
        );
        assert_eq!((back.done, back.failed.len()), (1, 0));
        assert_eq!(fs::read(c.dir.join("a.jpg")).unwrap(), b"a");
        c.reload().unwrap();
    }

    #[test]
    fn cancel_before_start_runs_nothing() {
        let c = setup("cancel");
        let job = Job::new(
            JobKind::Delete,
            &c.dir,
            vec!["a.jpg".into(), "b.jpg".into()],
            &c.dir,
        );
        let cancel = Cancel::new();
        cancel.cancel();
        let out = ops::run(&job, &c.env, &cancel, &mut |_| {});
        assert!(out.cancelled);
        assert!(c.dir.join("a.jpg").exists() && c.dir.join("b.jpg").exists());
    }

    #[test]
    fn copy_instead_from_a_refusal() {
        let mut c = setup("refused");
        let job = Job::new(
            JobKind::Move,
            &c.dir,
            vec!["a.jpg".into()],
            &c.dir.join("raw-exports"),
        );
        c.open_dialog(Dialog::Refused {
            job,
            names: vec!["a.jpg".into()],
            copy: vec!["a.jpg".into()],
            can_copy: true,
        });
        assert_eq!(c.focus, 1);
        assert!(matches!(
            c.button(1),
            Action::Run(Job {
                kind: JobKind::Copy,
                ..
            })
        ));
        c.open_dialog(Dialog::Refused {
            job: Job::new(JobKind::Move, &c.dir, vec![], &c.dir),
            names: vec!["a.jpg".into()],
            copy: vec![],
            can_copy: false,
        });
        assert_eq!(c.focus, 0);
        c.key(keys::TAB, false, false);
        assert_eq!(c.focus, 0, "Close is the only button");
    }

    #[test]
    fn names_list_is_capped() {
        let many: Vec<OsString> = (0..7).map(|i| format!("f{i}").into()).collect();
        assert_eq!(names_text(&many), "f0\nf1\nf2\nf3\nf4\nand 2 more");
        assert_eq!(
            names_text(&many[..5]),
            "f0\nf1\nf2\nf3\nf4",
            "five fit without a tail"
        );
        assert_eq!(names_text(&many[..6]).lines().count(), 6);
        assert_eq!(noun(&many[..1]), "“f0”");
    }

    #[test]
    fn folder_sides_show_item_counts_and_the_hint_names_replace() {
        assert_eq!(size_line(true, 4096, Some(48)), "48 items");
        assert_eq!(size_line(true, 4096, Some(1)), "1 item");
        assert_eq!(size_line(true, 4096, None), "");
        assert_eq!(size_line(false, 6_200_000, None), human(6_200_000));
        assert_eq!(
            cancel_note(true),
            "Replaced files aren't restored on cancel."
        );
        assert_eq!(
            cancel_note(false),
            "A replaced file isn't restored on cancel."
        );
    }

    fn lines(n: usize) -> Vec<String> {
        (0..n).map(|i| format!("l{i}")).collect()
    }

    #[test]
    fn details_pin_the_error_and_cut_after_seven() {
        let mut d = vec!["disk full".to_string()];
        d.extend(lines(18));
        let c = detail_rows(&d, true, false);
        assert_eq!(c.pin, "disk full");
        assert_eq!(c.rows, lines(7));
        assert_eq!(c.more, 11);
        assert!(!c.expanded);
        // Eight items fit without "+1 more"; unpinned, the first is an item.
        assert_eq!(detail_rows(&lines(8), false, false).more, 0);
        assert_eq!(detail_rows(&lines(9), false, false).more, 2);
        assert_eq!(detail_rows(&lines(9), false, false).pin, "");
    }

    #[test]
    fn more_expands_the_list_in_place() {
        let mut d = vec!["disk full".to_string()];
        d.extend(lines(18));
        let e = detail_rows(&d, true, true);
        assert_eq!(e.pin, "disk full");
        assert_eq!(e.rows, lines(18));
        assert_eq!(e.more, 0);
        assert!(e.expanded, "scrolls past 240px");
    }

    fn prompt() -> ConflictPrompt {
        ConflictPrompt {
            name: "dune.jpg".into(),
            kind: ConflictKind::FileFile,
            existing_size: 6_200_000,
            existing_mtime: 100,
            incoming_size: 4_100_000,
            incoming_mtime: 200,
            ..Default::default()
        }
    }

    fn conflict(c: &mut Controller) {
        c.open_dialog(Dialog::Conflict {
            ask: ConflictAsk {
                prompt: prompt(),
                dest: "SANDISK".into(),
                left: 3,
            },
            apply_all: false,
        });
    }

    #[test]
    fn conflict_dialog_is_keyboard_operable() {
        let mut c = setup("conflict-keys");
        conflict(&mut c);
        assert_eq!(c.focus, 1, "Skip is the default");
        assert_eq!(
            c.key(keys::RETURN, false, false),
            Action::Answer(Some(ConflictChoice {
                action: Conflict::Skip,
                apply_all: false
            }))
        );
        conflict(&mut c);
        c.key(keys::TAB, false, false);
        c.key(keys::TAB, false, false);
        c.key(keys::TAB, false, false);
        assert_eq!(c.focus, 4);
        c.key(" ", false, false);
        assert!(matches!(
            c.dialog,
            Some(Dialog::Conflict {
                apply_all: true,
                ..
            })
        ));
        c.key(keys::LEFT, false, false);
        assert_eq!(
            c.key(keys::RETURN, false, false),
            Action::Answer(Some(ConflictChoice {
                action: Conflict::Replace,
                apply_all: true
            }))
        );
        conflict(&mut c);
        assert_eq!(
            c.key("k", false, false),
            Action::Answer(Some(ConflictChoice {
                action: Conflict::Rename,
                apply_all: false
            }))
        );
        conflict(&mut c);
        assert_eq!(c.key(keys::ESCAPE, false, false), Action::Answer(None));
        assert!(c.dialog.is_none());
    }

    #[test]
    fn conflict_badges_and_names() {
        assert_eq!(conflict_badges(&prompt()), (("Larger", ""), ("", "Newer")));
        assert_eq!(side_badges(&prompt()), (vec!["Larger"], vec!["Newer"]));
        let both = ConflictPrompt {
            incoming_size: 9_000_000,
            ..prompt()
        };
        assert_eq!(side_badges(&both), (vec![], vec!["Newer", "Larger"]));
        let same = ConflictPrompt {
            incoming_size: 6_200_000,
            incoming_mtime: 100,
            ..prompt()
        };
        assert_eq!(side_badges(&same), (vec![], vec![]));
        assert_eq!(keep_both_name("dune.jpg"), "dune (2).jpg");
        assert_eq!(keep_both_name(".bashrc"), ".bashrc (2)");
    }

    #[test]
    fn copy_instead_copies_what_it_can_and_names_the_rest() {
        let mut c = setup("refused-some");
        let job = Job::new(
            JobKind::Move,
            &c.dir,
            vec!["a.jpg".into(), "locked".into()],
            &c.dir.join("raw-exports"),
        );
        let kept = |p: &str, reason| {
            vec![KeptEntry {
                path: p.into(),
                reason,
            }]
        };
        let outcome = Outcome {
            refused: vec![
                (
                    "a.jpg".into(),
                    kept("a.jpg", fsops::KeepReason::ReadOnlyFolder),
                ),
                (
                    "locked".into(),
                    kept("locked", fsops::KeepReason::UnreadableFolder),
                ),
            ],
            ..Outcome::default()
        };
        let view = refusal_view(&outcome.refused);
        assert!(view.can_copy, "one copyable item keeps Copy instead");
        assert!(view.body.contains("locked"), "{}", view.body);
        assert!(!view.body.contains("a.jpg"), "{}", view.body);
        let d = c.finish(&job, &outcome).unwrap();
        c.open_dialog(d);
        let Action::Run(copy) = c.button(1) else {
            panic!("Copy instead is offered")
        };
        assert_eq!(copy.kind, JobKind::Copy);
        assert_eq!(copy.names, vec![OsString::from("a.jpg")]);
    }

    #[test]
    fn toggle_names_the_kind_of_conflict() {
        assert_eq!(
            apply_label(ConflictKind::FileFile, 4),
            "Apply to the rest of the file conflicts (3)"
        );
        assert_eq!(
            apply_label(ConflictKind::FolderFolder, 3),
            "Apply to the rest of the folder conflicts (2)"
        );
        let folders = ConflictPrompt {
            kind: ConflictKind::FolderFolder,
            ..prompt()
        };
        assert_eq!(
            side_badges(&folders).1,
            vec!["Newer"],
            "no size badge on folders"
        );
    }

    #[test]
    fn every_refused_item_is_listed_and_copied() {
        let mut c = setup("refused-many");
        let job = Job::new(
            JobKind::Move,
            &c.dir,
            vec!["a.jpg".into(), "b.jpg".into()],
            &c.dir.join("raw-exports"),
        );
        let kept = |p: &str| {
            vec![KeptEntry {
                path: p.into(),
                reason: fsops::KeepReason::ReadOnlyFolder,
            }]
        };
        let outcome = Outcome {
            refused: vec![
                ("a.jpg".into(), kept("a.jpg")),
                ("b.jpg".into(), kept("b.jpg")),
            ],
            ..Outcome::default()
        };
        let view = refusal_view(&outcome.refused);
        assert_eq!(view.items, 2);
        assert!(view.title.contains("2 items"), "{}", view.title);
        assert_eq!(view.lines.iter().filter(|l| l.2).count(), 2);
        let d = c.finish(&job, &outcome).unwrap();
        let Dialog::Refused { names, .. } = &d else {
            panic!()
        };
        assert_eq!(names.len(), 2);
        c.open_dialog(d.clone());
        if let Dialog::Refused { can_copy: true, .. } = d {
            let Action::Run(copy) = c.button(1) else {
                panic!()
            };
            assert_eq!(copy.names.len(), 2);
        }
    }

    #[test]
    fn dragging_onto_itself_is_refused_and_labelled() {
        let mut c = setup("drag-self");
        let i = c
            .listing
            .view()
            .iter()
            .position(|e| e.name == "raw-exports")
            .unwrap();
        c.listing.select_only("raw-exports".into());
        c.pointer(i, 0, 0, false, Mods::default());
        c.pointer(i, 1, 0, true, Mods::default());
        let d = c.drag.clone().unwrap();
        assert_eq!((d.target, d.refused), (None, Some(i)));
        assert_eq!(c.drag_label().unwrap().0, "Can't drop into itself");
        let row = &c.rows()[i];
        assert!(row.drop_refused && !row.drop);
    }

    #[test]
    fn header_clicks_sort_and_reverse_with_folders_first() {
        let mut c = setup("sort-header");
        fs::write(c.dir.join("c.jpg"), b"ccccc").unwrap();
        c.reload().unwrap();
        let names = |c: &Controller| -> Vec<String> {
            c.rows().iter().map(|r| r.name.to_string()).collect()
        };
        assert_eq!(c.sort_header(), (0, false));
        assert_eq!(c.sort_by(1), Action::Redraw);
        assert_eq!(c.sort_header(), (1, false));
        assert_eq!(names(&c), ["raw-exports", "a.jpg", "b.jpg", "c.jpg"]);
        c.sort_by(1);
        assert_eq!(c.sort_header(), (1, true));
        assert_eq!(names(&c), ["raw-exports", "c.jpg", "b.jpg", "a.jpg"]);
        c.sort_by(0);
        assert_eq!(c.sort_header(), (0, false));
        c.sort_by(0);
        assert_eq!(names(&c), ["raw-exports", "c.jpg", "b.jpg", "a.jpg"]);
        assert_eq!(c.sort_by(9), Action::None);
    }

    #[test]
    fn modified_column_is_a_date_or_relative_within_a_day() {
        let mut c = setup("modified");
        let now = secs(std::time::SystemTime::now());
        let old = fs::File::options()
            .write(true)
            .open(c.dir.join("b.jpg"))
            .unwrap();
        old.set_modified(std::time::UNIX_EPOCH + std::time::Duration::from_secs(1_700_000_000))
            .unwrap();
        c.clock = Some((now, 0));
        c.reload().unwrap();
        let rows = c.rows();
        let get = |n: &str| {
            rows.iter()
                .find(|r| r.name == n)
                .unwrap()
                .modified
                .to_string()
        };
        assert_eq!(get("b.jpg"), "2023-11-14 22:13");
        assert_eq!(get("a.jpg"), "just now");
    }

    #[test]
    fn refresh_picks_up_outside_changes_and_keeps_the_selection() {
        let mut c = setup("refresh");
        c.listing.select_only("b.jpg".into());
        fs::write(c.dir.join("new.jpg"), b"n").unwrap();
        fs::remove_file(c.dir.join("a.jpg")).unwrap();
        c.refresh().unwrap();
        let names: Vec<String> = c.rows().iter().map(|r| r.name.to_string()).collect();
        assert_eq!(names, ["raw-exports", "b.jpg", "new.jpg"]);
        let selected: Vec<String> = c
            .rows()
            .iter()
            .filter(|r| r.selected)
            .map(|r| r.name.to_string())
            .collect();
        assert_eq!(selected, ["b.jpg"]);
        fs::remove_file(c.dir.join("b.jpg")).unwrap();
        c.refresh().unwrap();
        assert!(c.listing.selection().is_empty());
    }

    #[test]
    fn refresh_falls_back_to_the_parent_when_the_folder_goes_away() {
        let mut c = setup("refresh-gone");
        c.sort_by(1);
        let gone = c.dir.join("raw-exports");
        c.dir = gone.clone();
        c.reload().unwrap();
        fs::remove_dir(&gone).unwrap();
        c.refresh().unwrap();
        assert_eq!(c.dir, gone.parent().unwrap());
        assert_eq!(c.sort_header(), (1, false), "sort survives the fallback");
    }

    #[test]
    fn file_types_get_distinct_glyphs_and_colours() {
        let pdf = file_icon(false, "pdf");
        let zst = file_icon(false, "ZST");
        let mp4 = file_icon(false, "mp4");
        let flac = file_icon(false, "flac");
        let jpg = file_icon(false, "jpg");
        let other = file_icon(false, "xcf");
        assert_eq!(pdf.1, 2, "pdf is red");
        assert_eq!(zst.1, 3, "archives are amber");
        assert_eq!((mp4.1, flac.1), (1, 1), "video and audio are cyan");
        assert_eq!(other.1, 4, "unknown is muted");
        assert_eq!(file_icon(true, ""), ("󰉋", 1));
        let glyphs = [pdf.0, zst.0, mp4.0, flac.0, jpg.0, other.0];
        for (i, g) in glyphs.iter().enumerate() {
            assert!(!glyphs[i + 1..].contains(g), "{g} reused");
        }
        let mut c = setup("icons");
        fs::write(c.dir.join("m.pdf"), b"%").unwrap();
        c.reload().unwrap();
        let row = c.rows().into_iter().find(|r| r.name == "m.pdf").unwrap();
        assert_eq!((row.glyph.as_str(), row.role), pdf, "rows use the map");
    }

    #[test]
    fn counts_skip_other_devices_and_links() {
        let c = setup("count-dev");
        let dir = c.dir.clone();
        fs::create_dir(dir.join("mnt")).unwrap();
        fs::write(dir.join("mnt/x"), b"x").unwrap();
        std::os::unix::fs::symlink(dir.join("raw-exports"), dir.join("link")).unwrap();
        let fd = DirFd::open(&dir).unwrap();
        let entries: Vec<_> = fd
            .list()
            .unwrap()
            .into_iter()
            .map(crate::model::Entry::from_dirent)
            .collect();
        let key = |n: &str| OsString::from(n);
        let all = subfolder_counts(&fd, &entries, |_, _| true);
        assert_eq!(all.get(&key("mnt")), Some(&1));
        assert!(!all.contains_key(&key("link")), "links aren't followed");
        // A dev-mismatch seam: pretend "mnt" is a mount point.
        let mnt_ino = rustix::fs::fstat(fd.open_dir_nofollow("mnt".as_ref()).unwrap().as_fd())
            .unwrap()
            .st_ino;
        let split = subfolder_counts(&fd, &entries, |_, child| {
            rustix::fs::fstat(child.as_fd()).unwrap().st_ino != mnt_ino
        });
        assert!(!split.contains_key(&key("mnt")), "other device stays blank");
        assert_eq!(split.get(&key("raw-exports")), Some(&0));
    }

    #[test]
    fn the_watch_notice_uses_the_info_glyph_not_the_check() {
        assert_eq!(toast_glyph("info"), "󰋽");
        assert_eq!(toast_glyph(""), "󰄬");
        let glyphs = ["", "info", "warning", "danger"].map(toast_glyph);
        for (i, g) in glyphs.iter().enumerate() {
            assert!(!glyphs[i + 1..].contains(g));
        }
    }

    #[test]
    fn a_drop_onto_its_own_folder_says_nothing_was_copied() {
        assert_eq!(drop_notice(2, 0, 0), None);
        assert_eq!(
            drop_notice(2, 1, 0),
            None,
            "partial already-here stays quiet"
        );
        assert_eq!(drop_notice(0, 3, 0).unwrap().0, "Nothing to copy");
        let self_only = drop_notice(0, 0, 1).unwrap();
        assert_eq!(self_only.0, "Can't copy a folder into itself");
        assert!(self_only.1.is_empty());
        let both = drop_notice(0, 2, 1).unwrap();
        assert_eq!(both.0, "Nothing to copy");
        assert_eq!(
            both.1,
            [
                "2 items already in this folder",
                "Can't copy a folder into itself"
            ]
        );
        assert_eq!(
            drop_notice(1, 0, 1).unwrap().0,
            "Can't copy a folder into itself",
            "a self-drop is mentioned even when other items copy"
        );
        assert_eq!(drop_notice(1, 2, 0), None);
    }

    #[test]
    fn a_hung_drop_toasts_at_two_seconds_and_auto_cancels_at_fifteen() {
        let t0 = std::time::Instant::now();
        let mut wait = None;
        assert_eq!(tick_held_drop(&mut wait, true, t0), DropWaitTick::Hold);
        assert_eq!(
            tick_held_drop(
                &mut wait,
                true,
                t0 + DROP_WAIT_TOAST - Duration::from_millis(1)
            ),
            DropWaitTick::Hold
        );
        assert_eq!(
            tick_held_drop(&mut wait, true, t0 + DROP_WAIT_TOAST),
            DropWaitTick::Toast
        );
        assert_eq!(
            tick_held_drop(&mut wait, true, t0 + DROP_WAIT_TOAST),
            DropWaitTick::Hold,
            "the 2s toast is shown once"
        );
        assert_eq!(
            tick_held_drop(
                &mut wait,
                true,
                t0 + DROP_WAIT_LIMIT - Duration::from_millis(1)
            ),
            DropWaitTick::Hold
        );
        assert_eq!(
            tick_held_drop(&mut wait, true, t0 + DROP_WAIT_LIMIT),
            DropWaitTick::AutoCancel
        );
    }

    #[test]
    fn a_hung_drop_auto_cancels_even_if_the_two_second_tick_was_missed() {
        let t0 = std::time::Instant::now();
        // The wait started at t0; the first observed tick is already at 15s.
        let mut wait = Some(DropWait::start(t0));
        assert_eq!(
            tick_held_drop(&mut wait, true, t0 + DROP_WAIT_LIMIT),
            DropWaitTick::AutoCancel
        );
    }

    #[test]
    fn cancelling_a_hung_drop_clears_the_queue_and_does_not_auto_cancel() {
        let t0 = std::time::Instant::now();
        let mut wait = Some(DropWait::start(t0));
        let mut dropped: Vec<DroppedPath> = vec![(PathBuf::from("/tmp/shot.png"), None)];
        assert_eq!(
            tick_held_drop(&mut wait, true, t0 + DROP_WAIT_TOAST),
            DropWaitTick::Toast
        );
        // The Cancel action: empty the queue and end the wait so a later
        // flush cannot complete the drop, and 15s does not speak for us.
        dropped.clear();
        assert_eq!(
            tick_held_drop(&mut wait, false, t0 + DROP_WAIT_TOAST),
            DropWaitTick::Clear
        );
        assert!(dropped.is_empty());
        assert!(wait.is_none());
        assert_eq!(
            tick_held_drop(&mut wait, false, t0 + DROP_WAIT_LIMIT),
            DropWaitTick::Hold,
            "a cancelled drop is not auto-cancelled later"
        );
    }

    #[test]
    fn a_hung_drop_that_proceeds_clears_the_wait_without_cancelling() {
        let t0 = std::time::Instant::now();
        let mut wait = None;
        tick_held_drop(&mut wait, true, t0 + DROP_WAIT_TOAST);
        assert_eq!(
            tick_held_drop(
                &mut wait,
                false,
                t0 + DROP_WAIT_TOAST + Duration::from_secs(1)
            ),
            DropWaitTick::Clear
        );
        assert!(wait.is_none());
        assert_eq!(
            tick_held_drop(&mut wait, false, t0 + DROP_WAIT_LIMIT),
            DropWaitTick::Hold
        );
    }

    #[test]
    fn hung_drop_copy_names_the_drop_destination() {
        assert_eq!(
            drop_wait_toast("raw-exports"),
            "Waiting to drop into raw-exports…"
        );
        assert!(drop_wait_toast("raw-exports").contains('…'));
        assert_eq!(
            drop_wait_cancelled("raw-exports"),
            "Cancelled the drop into raw-exports"
        );
        assert_eq!(DROP_WAIT_CANCEL, "Cancel");

        let open = PathBuf::from("/home/sky/Pictures");
        let nav = PathBuf::from("/home/sky/raw-exports");
        let mid_nav = vec![(PathBuf::from("/tmp/shot.png"), None)];
        assert_eq!(
            drop_wait_name(&mid_nav, &open, Some(&nav)),
            "raw-exports",
            "an unresolved dest names the folder the drop will land in"
        );
        let frozen = vec![(
            PathBuf::from("/tmp/shot.png"),
            Some(PathBuf::from("/home/sky/Downloads")),
        )];
        assert_eq!(
            drop_wait_name(&frozen, &open, Some(&nav)),
            "Downloads",
            "a frozen dest wins over the folder being opened"
        );
        assert_eq!(
            drop_wait_name(&frozen, &open, None),
            "Downloads",
            "a dialog-held drop names its frozen destination"
        );
        assert_eq!(
            drop_wait_name(&mid_nav, &open, None),
            "Pictures",
            "an unresolved drop with no navigation names the open folder"
        );
    }

    #[test]
    fn a_just_deleted_folder_does_not_warn_that_live_refresh_is_unavailable() {
        let dir = std::env::temp_dir().join(format!("robin-watch-gone-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        assert!(
            watch_warns_on_failure(&dir),
            "a folder that is still here still warns"
        );
        fs::remove_dir(&dir).unwrap();
        assert!(
            !watch_warns_on_failure(&dir),
            "just deleted: walk up quietly"
        );
        fs::write(&dir, b"x").unwrap();
        assert!(
            !watch_warns_on_failure(&dir),
            "replaced by a file: also gone as a folder"
        );
        fs::remove_file(&dir).unwrap();
    }

    #[test]
    fn an_ancestor_replaced_by_a_file_does_not_warn_that_live_refresh_is_unavailable() {
        let root = std::env::temp_dir().join(format!("robin-watch-enotdir-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        let leaf = root.join("parent").join("leaf");
        fs::create_dir_all(&leaf).unwrap();
        assert!(
            watch_warns_on_failure(&leaf),
            "a folder that is still here still warns"
        );
        fs::remove_dir_all(root.join("parent")).unwrap();
        fs::write(root.join("parent"), b"x").unwrap();
        assert!(
            !watch_warns_on_failure(&leaf),
            "ENOTDIR from a file in the path: walk up quietly, like is_gone"
        );
        fs::remove_file(root.join("parent")).unwrap();
        fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn navigation_keeps_the_old_folder_until_its_read_lands() {
        let mut c = setup("nav-async");
        let old = c.dir.clone();
        let ticket = c.begin_navigate(old.join("raw-exports"));
        assert_eq!(c.dir, old, "no switch before the read lands");
        assert_eq!(c.listing.entries().len(), 3, "old listing still shown");
        assert!(c.begin_refresh().is_none(), "a reread can't cancel it");
        assert!(matches!(
            c.land(ticket.perform()),
            Landed::Shown { moved: true }
        ));
        assert_eq!(c.dir, old.join("raw-exports"));
        assert!(c.listing.entries().is_empty());
        assert!(c.idle());
    }

    #[test]
    fn a_slow_navigation_shows_its_target_after_150ms() {
        let mut c = setup("nav-slow");
        let old = c.dir.clone();
        let t0 = std::time::Instant::now();
        let ticket = c.begin_navigate_at(old.join("raw-exports"), t0);
        let g = ticket.gen;
        assert!(!c.tick_nav(g, t0 + Duration::from_millis(149)));
        assert_eq!(c.crumb_dir(), old, "a quick read never moves the crumb");
        assert!(!c.nav_pending());
        assert!(c.tick_nav(g, t0 + NAV_PENDING_AFTER));
        assert!(!c.tick_nav(g, t0 + Duration::from_millis(400)), "once");
        assert_eq!(c.crumb_dir(), old.join("raw-exports"));
        assert_eq!(c.dir, old, "listing not swapped yet");
        assert!(c.nav_pending());
        c.land(ticket.perform());
        assert!(!c.nav_pending(), "bar clears when the listing swaps in");
        assert_eq!(c.crumb_dir(), c.dir);
    }

    #[test]
    fn a_failed_slow_navigation_puts_the_crumb_back() {
        let mut c = setup("nav-slow-fail");
        let old = c.dir.clone();
        let t0 = std::time::Instant::now();
        let ticket = c.begin_navigate_at(old.join("missing"), t0);
        c.tick_nav(ticket.gen, t0 + NAV_PENDING_AFTER);
        assert_eq!(c.crumb_dir(), old.join("missing"));
        assert!(matches!(c.land(ticket.perform()), Landed::Failed(_)));
        assert_eq!(c.crumb_dir(), old);
        assert!(!c.nav_pending(), "bar clears on failure too");
    }

    #[test]
    fn a_superseding_navigation_retargets_the_crumb() {
        let mut c = setup("nav-retarget");
        let old = c.dir.clone();
        let t0 = std::time::Instant::now();
        let first = c.begin_navigate_at(old.join("raw-exports"), t0);
        c.tick_nav(first.gen, t0 + NAV_PENDING_AFTER);
        let up = old.parent().unwrap().to_path_buf();
        let second = c.begin_navigate_at(up.clone(), t0 + Duration::from_millis(200));
        assert_eq!(c.crumb_dir(), up, "retargeted at once");
        assert!(c.nav_pending());
        assert!(matches!(c.land(first.perform()), Landed::Stale));
        assert!(c.nav_pending(), "a stale result leaves the bar up");
        c.land(second.perform());
        assert!(!c.nav_pending());
        assert_eq!(c.dir, up);
    }

    #[test]
    fn a_superseded_navigations_timer_does_nothing() {
        let mut c = setup("nav-timer-stale");
        let old = c.dir.clone();
        let t0 = std::time::Instant::now();
        let first = c.begin_navigate_at(old.join("raw-exports"), t0);
        let second = c.begin_navigate_at(old.join("raw-exports"), t0 + Duration::from_millis(100));
        assert!(!c.tick_nav(first.gen, t0 + NAV_PENDING_AFTER), "old timer");
        assert!(!c.nav_pending());
        let g2 = second.gen;
        assert!(c.tick_nav(g2, t0 + Duration::from_millis(250)));
        c.land(second.perform());
        assert!(!c.tick_nav(g2, t0 + Duration::from_secs(1)), "landed");
        assert!(!c.nav_pending());
    }

    #[test]
    fn a_pending_navigation_blocks_paste_and_drops_onto_folders() {
        let mut c = setup("nav-block");
        let old = c.dir.clone();
        select(&mut c, "a.jpg");
        c.key("c", true, false);
        let t0 = std::time::Instant::now();
        let ticket = c.begin_navigate_at(old.parent().unwrap().to_path_buf(), t0);
        assert!(
            matches!(c.key("v", true, false), Action::Run(_)),
            "not yet pending"
        );
        c.tick_nav(ticket.gen, t0 + NAV_PENDING_AFTER);
        assert_eq!(c.key("v", true, false), Action::None);
        let rows = c.rows();
        let a = rows.iter().position(|r| r.name == "a.jpg").unwrap();
        let raw = rows.iter().position(|r| r.name == "raw-exports").unwrap();
        c.pointer(a, 0, 0, false, Mods::default());
        c.pointer(a, 1, raw as i32 - a as i32, true, Mods::default());
        assert!(!matches!(
            c.pointer(raw, 2, 0, true, Mods::default()),
            Action::Run(_)
        ));
        c.land(ticket.perform());
        assert!(matches!(c.key("v", true, false), Action::Run(_)));
    }

    #[test]
    fn a_drop_during_navigation_goes_where_the_read_lands() {
        for (target, lands_in) in [("raw-exports", "raw-exports"), ("missing", "")] {
            let mut c = setup(&format!("nav-drop-{target}"));
            let old = c.dir.clone();
            let src = old.parent().unwrap().to_path_buf();
            let paths: Vec<_> = ["x1", "x2", "x3"].map(|n| src.join(n)).into();
            for p in &paths {
                std::fs::write(p, b"x").unwrap();
            }
            let ticket = c.begin_navigate(old.join(target));
            assert!(c.nav_in_flight(), "even before the bar shows");
            let dropped: Vec<_> = paths.iter().map(|p| (p.clone(), None)).collect();
            c.land(ticket.perform());
            assert!(!c.nav_in_flight());
            let by_dest = group_drops(dropped, &c.dir);
            let want = if lands_in.is_empty() {
                old.clone()
            } else {
                old.join(lands_in)
            };
            assert_eq!(by_dest.len(), 1);
            assert_eq!(by_dest[0].0, want);
            let plan = ops::external_drop(&by_dest[0].1, &by_dest[0].0);
            let name = want.file_name().unwrap().to_string_lossy().into_owned();
            assert_eq!(plan.jobs[0].label(), format!("Copying 3 items to {name}"));
        }
    }

    #[test]
    fn a_drop_frozen_before_navigation_keeps_its_folder() {
        let by_dest = group_drops(
            vec![
                (PathBuf::from("/a/x"), Some(PathBuf::from("/old"))),
                (PathBuf::from("/a/y"), None),
            ],
            Path::new("/new"),
        );
        assert_eq!(by_dest[0].0, PathBuf::from("/old"));
        assert_eq!(by_dest[1].0, PathBuf::from("/new"));
    }

    #[test]
    fn drops_pending_when_a_folder_vanishes_keep_that_folder_not_the_parent() {
        let mut c = setup("drop-vanished");
        let parent = c.dir.clone();
        let gone = parent.join("raw-exports");
        let nav = c.begin_navigate(gone.clone());
        assert!(matches!(
            c.land(nav.perform()),
            Landed::Shown { moved: true }
        ));
        assert_eq!(&c.dir, &gone);

        let root = parent.parent().unwrap().to_path_buf();
        let paths: Vec<_> = ["dx", "dy"]
            .into_iter()
            .map(|n| {
                let p = root.join(n);
                fs::write(&p, b"x").unwrap();
                p
            })
            .collect();
        // One drop frozen to the open folder, one still open as during a
        // mid-navigation wait that has not flushed yet.
        let dropped = vec![
            (paths[0].clone(), Some(gone.clone())),
            (paths[1].clone(), None),
        ];

        fs::remove_dir(&gone).unwrap();
        let ticket = c.begin_refresh().expect("idle after nav");
        let read = ticket.perform();
        assert!(
            refresh_walks_up(&read, &c.dir),
            "the refresh walked up from the vanished folder"
        );

        // Resolve against the vanishing folder *before* land, matching
        // landed()'s flush-first order. Both drops must keep `gone`.
        let before = group_drops(dropped.clone(), &c.dir);
        assert_eq!(before.len(), 1, "{before:?}");
        assert_eq!(before[0].0, gone);
        assert_eq!(before[0].1.len(), 2);
        let plan = ops::external_drop(&before[0].1, &before[0].0);
        assert_eq!(plan.jobs.len(), 1);
        assert_eq!(plan.jobs[0].dest, gone, "intended target, not the parent");

        // Grouping after land would redirect the open drop into the parent.
        assert!(matches!(c.land(read), Landed::Shown { moved: true }));
        assert_eq!(&c.dir, &parent);
        let after = group_drops(dropped, &c.dir);
        let open_dest = after
            .iter()
            .find(|(_, ps)| ps.iter().any(|p| p == &paths[1]))
            .map(|(d, _)| d.clone());
        assert_eq!(
            open_dest,
            Some(parent.clone()),
            "post-walk-up grouping is what flush-first avoids"
        );
        // A successful navigation also moves dir, but must not flush-first:
        // mid-nav drops wait and take wherever that read lands.
        let elsewhere = parent.join("raw-exports");
        let nav_read = ReadResult {
            gen: 0,
            kind: ReadKind::Navigate,
            result: Ok((
                elsewhere.clone(),
                Snapshot {
                    entries: Vec::new(),
                    counts: Default::default(),
                    counts_ready: false,
                },
            )),
            trash: None,
        };
        assert!(!refresh_walks_up(&nav_read, &parent));
        assert!(refresh_walks_up(
            &ReadResult {
                gen: 0,
                kind: ReadKind::Refresh,
                result: Ok((
                    parent.clone(),
                    Snapshot {
                        entries: Vec::new(),
                        counts: Default::default(),
                        counts_ready: false,
                    },
                )),
                trash: None,
            },
            &elsewhere
        ));
    }

    #[test]
    fn row_drags_are_blocked_while_a_navigation_is_pending() {
        let mut c = setup("nav-drag");
        let t0 = std::time::Instant::now();
        let ticket = c.begin_navigate_at(c.dir.join("raw-exports"), t0);
        c.tick_nav(ticket.gen, t0 + NAV_PENDING_AFTER);
        let none = Mods::default();
        c.pointer(1, 0, 0, false, none);
        c.pointer(1, 1, 5, true, none); // past the rows: empty background
        assert_eq!(c.drag_label().unwrap().0, "Wait for the folder to open");
        c.pointer(1, 1, -1, true, none); // over raw-exports
        assert!(c.rows().iter().all(|r| !r.drop));
        assert_eq!(c.pointer(1, 2, 5, true, none), Action::Redraw, "no job");
        assert!(c.drag.is_none());
    }

    #[test]
    fn a_failed_navigation_queues_a_reread_of_the_old_folder() {
        let mut c = setup("nav-reread");
        let ticket = c.begin_navigate(c.dir.join("missing"));
        assert!(!c.take_refresh_due());
        c.land(ticket.perform());
        assert!(c.take_refresh_due());
        assert!(!c.take_refresh_due(), "once");
        let ticket = c.begin_refresh().expect("nothing in flight");
        assert_eq!(ticket.dir, c.dir);
    }

    #[test]
    fn an_unreadable_startup_folder_shows_why_not_empty() {
        use std::os::unix::fs::PermissionsExt;
        let mut c = setup("start-denied");
        let locked = c.dir.join("raw-exports");
        fs::set_permissions(&locked, fs::Permissions::from_mode(0o000)).unwrap();
        if fs::read_dir(&locked).is_ok() {
            return; // running as root
        }
        let mut s = Controller::new(locked.clone(), c.env.clone());
        s.loaded = false;
        let t = s.begin_refresh().unwrap();
        let Landed::Failed(e) = s.land(t.perform()) else {
            panic!("EACCES should fail")
        };
        assert!(e.is_denied());
        assert_eq!(s.unreadable, Some(true));
        assert_eq!(s.dir, locked, "no fallback on EACCES");
        fs::set_permissions(&locked, fs::Permissions::from_mode(0o755)).unwrap();
        let t = s.begin_refresh().unwrap();
        assert!(matches!(s.land(t.perform()), Landed::Shown { .. }));
        assert_eq!(s.unreadable, None, "a later read clears it");
        c.dir = locked;
    }

    #[test]
    fn the_denied_startup_state_replaces_the_error_toast() {
        let mut c = setup("start-denied-toast");
        assert!(c.toasts_failure(ReadKind::Refresh));
        c.unreadable = Some(true);
        assert!(!c.toasts_failure(ReadKind::Refresh), "the state says it");
        assert_eq!(
            c.status_left(),
            "",
            "no item count for a folder it can't read"
        );
        assert!(
            c.toasts_failure(ReadKind::Navigate),
            "a failed navigation still toasts"
        );
    }

    #[test]
    fn a_toast_without_details_has_no_divider() {
        assert!(!detail_rows(&[], false, false).has_details());
        assert!(!detail_rows(&[], true, true).has_details());
        assert!(detail_rows(&["one".into()], false, false).has_details());
        assert!(detail_rows(&["pinned".into()], true, false).has_details());
    }

    #[test]
    fn a_superseded_read_is_dropped() {
        let mut c = setup("nav-stale");
        let old = c.dir.clone();
        let first = c.begin_navigate(old.join("raw-exports"));
        let second = c.begin_navigate(old.parent().unwrap().to_path_buf());
        assert!(matches!(c.land(first.perform()), Landed::Stale));
        assert_eq!(c.dir, old, "the stale folder never shows");
        assert!(!c.idle());
        assert!(matches!(
            c.land(second.perform()),
            Landed::Shown { moved: true }
        ));
        assert_eq!(&c.dir, old.parent().unwrap());
        // A reread overtaken by a navigation is dropped too.
        let reread = c.begin_refresh().unwrap();
        let nav = c.begin_navigate(old.clone());
        assert!(matches!(c.land(reread.perform()), Landed::Stale));
        assert!(matches!(
            c.land(nav.perform()),
            Landed::Shown { moved: true }
        ));
        assert_eq!(c.dir, old);
    }

    #[test]
    fn a_failed_navigation_stays_put_and_never_falls_back() {
        let mut c = setup("nav-fail");
        let old = c.dir.clone();
        let ticket = c.begin_navigate(old.join("missing").join("deeper"));
        let Landed::Failed(e) = c.land(ticket.perform()) else {
            panic!("navigating to a missing folder should fail")
        };
        assert!(e.is_gone());
        assert_eq!(c.dir, old);
        assert_eq!(c.listing.entries().len(), 3);
        assert!(c.idle());
    }

    #[test]
    fn a_worker_reread_walks_up_only_when_gone() {
        let mut c = setup("reread-gone");
        let sub = c.dir.join("raw-exports");
        let nav = c.begin_navigate(sub.clone());
        c.land(nav.perform());
        let ticket = c.begin_refresh().unwrap();
        fs::remove_dir(&sub).unwrap();
        assert!(matches!(
            c.land(ticket.perform()),
            Landed::Shown { moved: true }
        ));
        assert_eq!(&c.dir, sub.parent().unwrap(), "fell back on the worker");
        let ticket = c.begin_refresh().unwrap();
        assert!(matches!(c.land(ticket.perform()), Landed::Unchanged));
    }

    #[test]
    fn startup_shows_loading_until_the_first_read_lands() {
        let mut c = setup("startup");
        c.loaded = false;
        let ticket = c.begin_refresh().unwrap();
        assert!(!c.loaded);
        assert!(matches!(
            c.land(ticket.perform()),
            Landed::Shown { moved: false }
        ));
        assert!(c.loaded);
    }

    #[test]
    fn a_reread_error_other_than_gone_keeps_the_folder() {
        let mut c = setup("refresh-error");
        let dir = c.dir.clone();
        let denied = Error::io(&dir, std::io::Error::from_raw_os_error(libc::EACCES));
        assert!(!denied.is_gone());
        let read = ReadResult {
            gen: c.ticket(dir.clone(), ReadKind::Refresh).gen,
            kind: ReadKind::Refresh,
            result: Err(denied),
            trash: None,
        };
        assert!(matches!(c.land(read), Landed::Failed(_)));
        assert_eq!(c.dir, dir, "no jump to the parent on EACCES");
        assert!(!c.listing.entries().is_empty(), "old listing kept");
        let gone = Error::io(&dir, std::io::Error::from_raw_os_error(libc::ENOTDIR));
        assert!(gone.is_gone());
        assert!(Error::io(&dir, std::io::Error::from_raw_os_error(libc::ENOENT)).is_gone());
    }

    /// `cargo test --release -- --ignored --nocapture big_folder`
    #[test]
    #[ignore]
    fn big_folder_reread_timing() {
        let c = setup("big");
        for i in 0..100_000 {
            fs::write(c.dir.join(format!("f{i:06}.jpg")), b"").unwrap();
        }
        let mut c = c;
        let big = c.dir.clone();
        c.dir = big.parent().unwrap().to_path_buf();
        c.reload().unwrap();
        let ticket = c.begin_navigate(big);
        let t = std::time::Instant::now();
        let done = ticket.perform();
        let read = t.elapsed();
        let t = std::time::Instant::now();
        assert!(matches!(c.land(done), Landed::Shown { moved: true }));
        let apply = t.elapsed();
        let t = std::time::Instant::now();
        let rows = c.rows();
        let rows_t = t.elapsed();
        eprintln!(
            "100k: read+stat (worker) {read:?}, sort/apply (UI) {apply:?}, rows (UI) {rows_t:?}, {} rows",
            rows.len()
        );
        fs::remove_dir_all(c.dir.parent().unwrap()).unwrap();
    }

    #[test]
    fn folders_show_item_counts_and_files_a_type_name() {
        let mut c = setup("counts");
        for n in ["1.raw", "2.raw", ".hidden"] {
            fs::write(c.dir.join("raw-exports").join(n), b"x").unwrap();
        }
        fs::write(c.dir.join("notes.md"), b"#").unwrap();
        fs::write(c.dir.join("blob.xyz"), b"?").unwrap();
        c.reload().unwrap();
        let rows = c.rows();
        let get = |n: &str| rows.iter().find(|r| r.name == n).unwrap().clone();
        assert_eq!(get("raw-exports").size, "3 items");
        assert_eq!(get("raw-exports").kind, "Folder");
        assert_eq!(get("a.jpg").kind, "JPEG image");
        assert_eq!(get("notes.md").kind, "Markdown");
        assert_eq!(get("blob.xyz").kind, "XYZ file");
        fs::write(c.dir.join("raw-exports/3.raw"), b"x").unwrap();
        c.refresh().unwrap();
        assert_eq!(c.rows()[0].size, "4 items", "counts are retaken on refresh");
    }

    #[test]
    fn a_listing_shows_entries_before_counts_without_flashing_zero() {
        let mut c = setup("lazy-counts");
        let inner = c.dir.join("raw-exports");
        fs::create_dir_all(inner.join("shots")).unwrap();
        let ticket = c.begin_navigate(inner);
        let (read, follow) = ticket.perform_listing();
        assert!(matches!(c.land(read), Landed::Shown { moved: true }));
        let rows = c.rows();
        let shots = rows.iter().find(|r| r.name == "shots").unwrap();
        assert_eq!(shots.size, "", "blank until the count pass, not 0");
        let (gen, dir, entries) = follow.expect("count pass");
        let counts = count_subfolders(&dir, &entries).expect("still there");
        assert!(c.apply_counts(gen, &dir, counts));
        assert_eq!(
            c.rows().iter().find(|r| r.name == "shots").unwrap().size,
            "0 items",
            "an empty folder is counted only once the pass lands"
        );
    }

    #[test]
    fn a_reread_keeps_folder_counts_until_the_second_pass() {
        let mut c = setup("lazy-counts-keep");
        for n in ["1.raw", "2.raw", "3.raw"] {
            fs::write(c.dir.join("raw-exports").join(n), b"x").unwrap();
        }
        c.reload().unwrap();
        assert_eq!(
            c.rows()
                .iter()
                .find(|r| r.name == "raw-exports")
                .unwrap()
                .size,
            "3 items"
        );
        let ticket = c.begin_refresh().unwrap();
        let (read, follow) = ticket.perform_listing();
        assert!(
            matches!(
                c.land(read),
                Landed::Unchanged | Landed::Shown { moved: false }
            ),
            "entries are already on screen"
        );
        assert_eq!(
            c.rows()
                .iter()
                .find(|r| r.name == "raw-exports")
                .unwrap()
                .size,
            "3 items",
            "pending counts must not blank a reread"
        );
        assert!(follow.is_some());
    }

    #[test]
    fn rows_can_be_built_in_chunks_without_dropping_selection() {
        let mut c = setup("row-chunks");
        for n in ["c.jpg", "d.jpg", "e.jpg", "f.jpg"] {
            fs::write(c.dir.join(n), b"x").unwrap();
        }
        c.reload().unwrap();
        c.listing.select_only("d.jpg".into());
        c.listing.toggle("f.jpg".into());
        let view = c.listing.view();
        let all = c.rows_range(&view, 0, view.len());
        assert_eq!(all, c.rows());
        let mut acc = Vec::new();
        let mut start = 0;
        while start < view.len() {
            let end = (start + 2).min(view.len());
            acc.extend(c.rows_range(&view, start, end));
            start = end;
        }
        assert_eq!(acc, all);
        let selected: Vec<_> = all
            .iter()
            .filter(|r| r.selected)
            .map(|r| r.name.to_string())
            .collect();
        assert_eq!(selected, ["d.jpg", "f.jpg"]);
    }

    #[test]
    fn a_same_folder_rebuild_keeps_rows_past_the_first_chunk() {
        let n = ROW_CHUNK + 200;
        let mut shown: Vec<u32> = (0..n as u32).collect();
        let first: Vec<u32> = (1_000..1_000 + ROW_CHUNK as u32).collect();
        merge_shown(&mut shown, 0, first, n, false);
        assert_eq!(
            shown.len(),
            n,
            "a same-folder restart must not drop the tail"
        );
        assert_eq!(shown[0], 1_000);
        assert_eq!(shown[ROW_CHUNK - 1], 1_000 + ROW_CHUNK as u32 - 1);
        assert_eq!(shown[ROW_CHUNK], ROW_CHUNK as u32, "old tail stays");
        assert_eq!(shown[n - 1], n as u32 - 1);
        let second: Vec<u32> = (2_000..2_000 + 200).collect();
        merge_shown(&mut shown, ROW_CHUNK, second, n, true);
        assert_eq!(shown.len(), n);
        assert_eq!(shown[ROW_CHUNK], 2_000);
        assert_eq!(shown[n - 1], 2_199);

        // A shorter listing must not shrink until the rebuild completes.
        let mut shown: Vec<u32> = (0..n as u32).collect();
        let shorter = ROW_CHUNK + 50;
        merge_shown(
            &mut shown,
            0,
            (0..ROW_CHUNK as u32).collect(),
            shorter,
            false,
        );
        assert_eq!(shown.len(), n, "don't shrink mid-rebuild");
        merge_shown(
            &mut shown,
            ROW_CHUNK,
            (ROW_CHUNK as u32..shorter as u32).collect(),
            shorter,
            true,
        );
        assert_eq!(shown.len(), shorter);
    }

    #[test]
    fn changing_tile_size_changes_the_rows_key() {
        let mut c = setup("tile-key");
        let before = rows_key(&c, 80);
        assert_eq!(c.step_tile(1), Action::Redraw);
        assert_ne!(
            rows_key(&c, 80),
            before,
            "S/M/L must invalidate a finished grid"
        );
        assert_eq!(c.step_tile(-1), Action::Redraw);
        assert_eq!(rows_key(&c, 80), before);
    }

    #[test]
    fn pictures_are_turned_and_shrunk_before_they_reach_the_ui() {
        let img = image::RgbaImage::from_pixel(400, 100, image::Rgba([9, 8, 7, 255]));
        let px = prepare(&img, 1, 4096);
        assert_eq!((px.width(), px.height()), (100, 400));
        let px = prepare(&img, 2, 200);
        assert_eq!((px.width(), px.height()), (200, 50));
        let px = prepare(&img, 3, 200);
        assert_eq!((px.width(), px.height()), (50, 200));
    }

    #[test]
    fn closing_the_viewer_strands_its_decodes_and_frees_their_slots() {
        let mut pics = Pics::default();
        let (old, _) = pics.decodes.request(PathBuf::from("/a.jpg")).unwrap();
        pics.decodes.request(PathBuf::from("/b.jpg")).unwrap();
        assert!(
            pics.decodes.request(PathBuf::from("/c.jpg")).is_none(),
            "c waits"
        );
        pics.want = Some("/c.jpg".into());
        let gen = pics.gen;
        pics.close();
        assert_ne!(pics.gen, gen, "tiles and turns from before land stale");
        assert!(pics.want.is_none());
        let (wanted, next) = pics.decodes.finish(old);
        assert!(!wanted && next.is_none(), "c was dropped with the viewer");
        // A reopen gets a fresh ticket the stale one can't match.
        let (new, _) = pics.decodes.request(PathBuf::from("/a.jpg")).unwrap();
        assert_ne!(new, old);
        assert!(pics.decodes.is_latest(new));
    }

    #[test]
    fn a_resize_pulls_the_pan_back_inside_the_picture() {
        let mut c = setup("viewer-resize");
        select(&mut c, "a.jpg");
        c.key(keys::RETURN, false, false);
        c.view_geom.set(((1000.0, 800.0), (500.0, 400.0)));
        let v = c.viewer.as_mut().unwrap();
        v.zoom = crate::viewer::Zoom::Scale(1.0);
        v.pan_by((-400.0, -400.0), (1000.0, 800.0), (500.0, 400.0));
        assert_eq!(v.pan, (-250.0, -200.0));
        assert_eq!(c.viewer_resized((900.0, 700.0)), Action::Redraw);
        assert_eq!(c.viewer.as_ref().unwrap().pan, (-50.0, -50.0));
        assert_eq!(
            c.viewer_resized((900.0, 700.0)),
            Action::None,
            "already inside"
        );
    }

    #[test]
    fn launched_apps_lead_their_own_session() {
        let mut cmd = std::process::Command::new("sh");
        // $$ is the shell's pid; a session leader's sid equals it.
        cmd.args(["-c", "test \"$(ps -o sid= -p $$ | tr -d ' ')\" = \"$$\""]);
        let status = own_session(&mut cmd).status().unwrap();
        assert!(status.success());
    }
}
