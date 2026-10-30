//! The controller behind the window's file operations: the clipboard, the
//! drag-and-drop choice between copy and move, jobs that run off the UI
//! thread, and undo for trash. Nothing here touches Slint, so it is tested
//! directly.

use std::ffi::{OsStr, OsString};
use std::path::{Path, PathBuf};

use crate::error::{Error, Result};
use crate::fsops::{
    self, Cancel, Conflict, ConflictChoice, ConflictPolicy, CopyOpts, KeptEntry, MoveOpts,
    TransferReport, TrashChoice,
};
use crate::safety::DirFd;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Op {
    Copy,
    Move,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ClipMode {
    Copy,
    Cut,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Clip {
    pub mode: ClipMode,
    pub dir: PathBuf,
    pub names: Vec<OsString>,
}

/// Ctrl+X / Ctrl+C / Ctrl+V. A cut is used up by a successful move; a copy can be
/// pasted again.
#[derive(Debug, Default)]
pub struct Clipboard {
    clip: Option<Clip>,
}

impl Clipboard {
    pub fn cut(&mut self, dir: &Path, names: Vec<OsString>) {
        self.set(ClipMode::Cut, dir, names);
    }

    pub fn copy(&mut self, dir: &Path, names: Vec<OsString>) {
        self.set(ClipMode::Copy, dir, names);
    }

    fn set(&mut self, mode: ClipMode, dir: &Path, names: Vec<OsString>) {
        if names.is_empty() {
            return;
        }
        self.clip = Some(Clip {
            mode,
            dir: dir.to_path_buf(),
            names,
        });
    }

    pub fn clip(&self) -> Option<&Clip> {
        self.clip.as_ref()
    }

    /// Cut rows are drawn dimmed until they are pasted.
    pub fn is_cut(&self, dir: &Path, name: &OsStr) -> bool {
        self.clip.as_ref().is_some_and(|c| {
            c.mode == ClipMode::Cut && c.dir == dir && c.names.iter().any(|n| n == name)
        })
    }

    /// The job a paste into `dest` starts. Pasting a cut into the folder it
    /// came from does nothing and drops the cut, like Thunar.
    pub fn paste(&mut self, dest: &Path) -> Option<Job> {
        let clip = self.clip.clone()?;
        match clip.mode {
            // The cut stays until a move succeeds; see `cut_moved`.
            ClipMode::Cut => {
                if clip.dir == dest {
                    return None;
                }
                Some(Job::new(JobKind::Move, &clip.dir, clip.names, dest))
            }
            ClipMode::Copy => Some(Job::new(JobKind::Copy, &clip.dir, clip.names, dest)),
        }
    }

    /// A move job finished cleanly: drop the cut if it is the one moved.
    pub fn cut_moved(&mut self, job: &Job) {
        if let Some(clip) = &self.clip {
            if clip.mode == ClipMode::Cut && clip.dir == job.src && clip.names == job.names {
                self.clip = None;
            }
        }
    }

    pub fn clear(&mut self) {
        self.clip = None;
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Mods {
    pub ctrl: bool,
    pub shift: bool,
}

/// Thunar's drop rule: the same filesystem moves and another one copies.
/// Ctrl forces a copy and Shift forces a move. Ctrl+Shift makes a link in
/// Thunar; Robin doesn't make links, so that drop does nothing.
pub fn drop_op(mods: Mods, same_fs: bool) -> Option<Op> {
    match (mods.ctrl, mods.shift) {
        (true, true) => None,
        (true, false) => Some(Op::Copy),
        (false, true) => Some(Op::Move),
        (false, false) if same_fs => Some(Op::Move),
        (false, false) => Some(Op::Copy),
    }
}

pub fn same_filesystem(a: &Path, b: &Path) -> bool {
    match (rustix::fs::stat(a), rustix::fs::stat(b)) {
        (Ok(a), Ok(b)) => a.st_dev == b.st_dev,
        _ => false,
    }
}

/// A folder can't go into itself or anything under it, and a move into the
/// folder it is already in does nothing.
pub fn drop_allowed(op: Op, src: &Path, names: &[OsString], dest: &Path) -> bool {
    if names.is_empty() || (op == Op::Move && src == dest) {
        return false;
    }
    !names.iter().any(|n| dest.starts_with(src.join(n)))
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum JobKind {
    Copy,
    Move,
    Trash,
    Delete,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Job {
    pub kind: JobKind,
    pub src: PathBuf,
    pub names: Vec<OsString>,
    /// Destination folder for copies and moves. Unused for trash and delete.
    pub dest: PathBuf,
}

impl Job {
    pub fn new(kind: JobKind, src: &Path, names: Vec<OsString>, dest: &Path) -> Self {
        Self {
            kind,
            src: src.to_path_buf(),
            names,
            dest: dest.to_path_buf(),
        }
    }

    pub fn transfer(op: Op, src: &Path, names: Vec<OsString>, dest: &Path) -> Self {
        let kind = match op {
            Op::Copy => JobKind::Copy,
            Op::Move => JobKind::Move,
        };
        Self::new(kind, src, names, dest)
    }

    /// The progress strip's glyph: copy, move, or trash.
    pub fn glyph(&self) -> &'static str {
        match self.kind {
            JobKind::Copy => "󰆏",
            JobKind::Move => "󰪹",
            JobKind::Trash | JobKind::Delete => "󰩺",
        }
    }

    /// "Copying 3 items to Pictures".
    pub fn label(&self) -> String {
        let n = count(self.names.len());
        let to = folder_name(&self.dest);
        match self.kind {
            JobKind::Copy => format!("Copying {n} to {to}"),
            JobKind::Move => format!("Moving {n} to {to}"),
            JobKind::Trash => format!("Moving {n} to trash"),
            JobKind::Delete => format!("Deleting {n}"),
        }
    }
}

pub fn count(n: usize) -> String {
    if n == 1 {
        "1 item".into()
    } else {
        format!("{n} items")
    }
}

pub fn folder_name(path: &Path) -> String {
    path.file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| path.display().to_string())
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Progress {
    pub done: usize,
    pub total: usize,
    pub current: String,
}

impl Progress {
    pub fn amount(&self) -> f32 {
        if self.total == 0 {
            1.0
        } else {
            self.done as f32 / self.total as f32
        }
    }

    /// "2 of 5 items · dune-0413.jpg".
    pub fn detail(&self) -> String {
        let of = format!("{} of {}", self.done, count(self.total));
        if self.current.is_empty() {
            of
        } else {
            format!("{of} · {}", self.current)
        }
    }
}

/// One entry put in a trash, enough to put it back.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Trashed {
    /// The trash folder holding `files/` and `info/`.
    pub trash: PathBuf,
    pub stored: OsString,
    pub original: PathBuf,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Outcome {
    /// Top-level items finished.
    pub done: usize,
    pub cancelled: bool,
    pub report: TransferReport,
    /// "name: reason" for items that failed.
    pub failed: Vec<String>,
    /// Moves refused up front, for the move-refused dialog.
    pub refused: Vec<(OsString, Vec<KeptEntry>)>,
    pub trashed: Vec<Trashed>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Toast {
    /// "" for neutral, "warning", or "danger".
    pub tone: &'static str,
    pub text: String,
    pub details: Vec<String>,
    /// Undo after trash.
    pub undo: bool,
}

impl Outcome {
    pub fn toast(&self, kind: JobKind) -> Toast {
        let n = count(self.done);
        let mut text = match kind {
            JobKind::Copy => format!("Copied {n}"),
            JobKind::Move => format!("Moved {n}"),
            JobKind::Trash => format!("Moved {n} to trash"),
            JobKind::Delete => format!("Deleted {n}"),
        };
        if !self.report.left_behind.is_empty() {
            text = format!(
                "{text}, but couldn't remove {} original{}",
                self.report.left_behind.len(),
                if self.report.left_behind.len() == 1 {
                    ""
                } else {
                    "s"
                }
            );
        }
        if !self.report.skipped.is_empty() {
            text = format!("{text}, skipped {}", self.report.skipped.len());
        }
        if !self.failed.is_empty() {
            text = format!("{text}, {} failed", self.failed.len());
        }
        if self.cancelled {
            text = format!("{text}, then cancelled");
        }
        let mut details = self.report.details();
        details.extend(self.failed.iter().cloned());
        let tone = if !self.failed.is_empty() {
            "danger"
        } else if !self.report.left_behind.is_empty() || self.cancelled {
            "warning"
        } else {
            ""
        };
        Toast {
            tone,
            text,
            details,
            undo: kind == JobKind::Trash && !self.trashed.is_empty(),
        }
    }
}

/// Where trash goes for folders on the home filesystem.
#[derive(Clone, Debug)]
pub struct Env {
    pub home: PathBuf,
    pub home_trash: PathBuf,
}

impl Env {
    /// `$XDG_DATA_HOME/Trash`, or `~/.local/share/Trash`.
    pub fn from_system() -> Self {
        let home = std::env::var_os("HOME")
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from("/"));
        let data = std::env::var_os("XDG_DATA_HOME")
            .filter(|v| !v.is_empty())
            .map(PathBuf::from)
            .unwrap_or_else(|| home.join(".local/share"));
        Self {
            home_trash: data.join("Trash"),
            home,
        }
    }
}

/// Whether Del can trash things in `dir`, or must offer a permanent delete.
pub fn trash_choice(dir: &Path, env: &Env) -> TrashChoice {
    if same_filesystem(dir, &env.home) {
        return TrashChoice::Home;
    }
    let creatable = fsops::mount_top(dir).is_ok_and(|top| {
        rustix::fs::access(&top, rustix::fs::Access::WRITE_OK).is_ok()
            || top.join(".Trash").is_dir()
            || has_private_trash(&top)
    });
    fsops::choose_trash(false, creatable)
}

fn has_private_trash(top: &Path) -> bool {
    let uid = rustix::process::getuid().as_raw();
    top.join(format!(".Trash-{uid}")).is_dir()
}

struct TrashTarget {
    root: PathBuf,
    fd: DirFd,
    choice: TrashChoice,
    /// Volume trash records paths relative to the top of the volume.
    top: Option<PathBuf>,
}

fn trash_target(dir: &Path, env: &Env) -> Result<TrashTarget> {
    match trash_choice(dir, env) {
        TrashChoice::Home => {
            std::fs::create_dir_all(&env.home_trash).map_err(|e| Error::io(&env.home_trash, e))?;
            Ok(TrashTarget {
                fd: DirFd::open(&env.home_trash)?,
                root: env.home_trash.clone(),
                choice: TrashChoice::Home,
                top: None,
            })
        }
        TrashChoice::Volume => {
            let top = fsops::mount_top(dir)?;
            let top_fd = DirFd::open(&top)?;
            let root = match fsops::volume_trash_plan(&top_fd)? {
                fsops::VolumeTrash::Sticky { uid_dir } => top.join(uid_dir),
                fsops::VolumeTrash::Private { dir } => top.join(dir),
            };
            Ok(TrashTarget {
                fd: fsops::open_volume_trash(&top_fd)?,
                root,
                choice: TrashChoice::Volume,
                top: Some(top),
            })
        }
        TrashChoice::Ask => Err(Error::Message(
            "this drive has no trash and files are not copied home".into(),
        )),
    }
}

/// Run `job` one top-level item at a time. `cancel` is checked between
/// items, so an item is never left half-done. `progress` sees each item
/// before it starts and once at the end.
pub fn run(job: &Job, env: &Env, cancel: &Cancel, progress: &mut dyn FnMut(&Progress)) -> Outcome {
    let mut out = Outcome::default();
    let total = job.names.len();
    let src = match DirFd::open(&job.src) {
        Ok(fd) => fd,
        Err(e) => {
            out.failed.push(e.to_string());
            return out;
        }
    };
    let dest = match job.kind {
        JobKind::Copy | JobKind::Move => match DirFd::open(&job.dest) {
            Ok(fd) => Some(fd),
            Err(e) => {
                out.failed.push(e.to_string());
                return out;
            }
        },
        _ => None,
    };
    let trash = match job.kind {
        JobKind::Trash => match trash_target(&job.src, env) {
            Ok(t) => Some(t),
            Err(e) => {
                out.failed.push(e.to_string());
                return out;
            }
        },
        _ => None,
    };
    for (i, name) in job.names.iter().enumerate() {
        if cancel.is_cancelled() {
            out.cancelled = true;
            break;
        }
        progress(&Progress {
            done: i,
            total,
            current: name.to_string_lossy().into_owned(),
        });
        // Each item finishes once started; Cancel takes effect before the next.
        let item = Cancel::new();
        let result = match job.kind {
            JobKind::Copy => copy_one(job, &src, dest.as_ref().unwrap(), name, &item, &mut out),
            JobKind::Move => move_one(job, &src, dest.as_ref().unwrap(), name, &item, &mut out),
            JobKind::Trash => trash_one(job, &src, trash.as_ref().unwrap(), name, &mut out),
            JobKind::Delete => fsops::remove_child(&src, name, &mut |_| {}, &item),
        };
        match result {
            Ok(()) => out.done += 1,
            Err(Error::NotRemovable(kept)) => out.refused.push((name.clone(), kept)),
            Err(e) => out.failed.push(format!("{}: {e}", name.to_string_lossy())),
        }
    }
    if cancel.is_cancelled() && out.done + out.failed.len() + out.refused.len() < total {
        out.cancelled = true;
    }
    progress(&Progress {
        done: out.done,
        total,
        current: String::new(),
    });
    out
}

fn keep_both(_: &fsops::ConflictPrompt) -> Result<ConflictChoice> {
    Ok(ConflictChoice {
        action: Conflict::Rename,
        apply_all: true,
    })
}

/// Refuse when the destination is the item itself or anywhere inside it.
/// Compares device and inode up the destination's real parent chain, so a
/// symlinked or `..`-laden path into the folder is still caught.
fn into_itself(job: &Job, name: &OsStr) -> Result<()> {
    use std::os::unix::fs::MetadataExt;
    let Ok(item) = std::fs::symlink_metadata(job.src.join(name)) else {
        return Ok(()); // gone; the operation reports it
    };
    if !item.is_dir() {
        return Ok(());
    }
    let dest = std::fs::canonicalize(&job.dest).map_err(|e| Error::Message(e.to_string()))?;
    for dir in dest.ancestors() {
        if let Ok(meta) = std::fs::metadata(dir) {
            if meta.dev() == item.dev() && meta.ino() == item.ino() {
                return Err(Error::Message("can't put a folder inside itself".into()));
            }
        }
    }
    Ok(())
}

fn merge(into: &mut TransferReport, from: TransferReport) {
    into.moved += from.moved;
    into.skipped.extend(from.skipped);
    into.left_behind.extend(from.left_behind);
}

fn copy_one(
    job: &Job,
    src: &DirFd,
    dest: &DirFd,
    name: &OsStr,
    item: &Cancel,
    out: &mut Outcome,
) -> Result<()> {
    into_itself(job, name)?;
    let mut policy = ConflictPolicy::new();
    let mut ask = keep_both;
    let (_, report) = fsops::copy_instead(
        src,
        name,
        dest,
        name,
        CopyOpts {
            cancel: item,
            policy: &mut policy,
            ask: &mut ask,
        },
    )?;
    merge(&mut out.report, report);
    Ok(())
}

fn move_one(
    job: &Job,
    src: &DirFd,
    dest: &DirFd,
    name: &OsStr,
    item: &Cancel,
    out: &mut Outcome,
) -> Result<()> {
    if job.src == job.dest {
        return Ok(());
    }
    into_itself(job, name)?;
    let mut policy = ConflictPolicy::new();
    let mut ask = keep_both;
    let mut opts = MoveOpts {
        force_copy: false,
        fail_before_unlink: false,
        copy: CopyOpts {
            cancel: item,
            policy: &mut policy,
            ask: &mut ask,
        },
        report: TransferReport::default(),
        check_removable: true,
        keep_source: false,
    };
    let result = fsops::move_child(src, name, dest, name, &mut opts);
    let report = std::mem::take(&mut opts.report);
    merge(&mut out.report, report);
    result.map(|_| ())
}

fn trash_one(
    job: &Job,
    src: &DirFd,
    trash: &TrashTarget,
    name: &OsStr,
    out: &mut Outcome,
) -> Result<()> {
    let original = job.src.join(name);
    let info_path = match &trash.top {
        Some(top) => original
            .strip_prefix(top)
            .unwrap_or(&original)
            .to_string_lossy()
            .into_owned(),
        None => original.to_string_lossy().into_owned(),
    };
    let stored = fsops::place_in_trash(src, name, &trash.fd, trash.choice, &info_path)?;
    out.trashed.push(Trashed {
        trash: trash.root.clone(),
        stored,
        original,
    });
    Ok(())
}

/// Put trashed entries back where they were. Returns how many came back
/// and a line for each that couldn't.
pub fn undo(items: &[Trashed]) -> (usize, Vec<String>) {
    let mut restored = 0;
    let mut failed = Vec::new();
    for t in items {
        let result = (|| {
            let parent = t
                .original
                .parent()
                .ok_or_else(|| Error::Message("no parent folder".into()))?;
            let name = t.original.file_name().ok_or(Error::InvalidName)?;
            let trash = DirFd::open(&t.trash)?;
            let dest = DirFd::open(parent)?;
            fsops::restore_from_trash(&trash, &t.stored, &dest, name)
        })();
        match result {
            Ok(()) => restored += 1,
            Err(e) => failed.push(format!("{}: {e}", t.original.display())),
        }
    }
    (restored, failed)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn scratch(label: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "robin-ops-{label}-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn names(list: &[&str]) -> Vec<OsString> {
        list.iter().map(OsString::from).collect()
    }

    fn env(root: &Path) -> Env {
        Env {
            home: root.to_path_buf(),
            home_trash: root.join("trash"),
        }
    }

    #[test]
    fn cut_dims_until_pasted_and_is_used_up() {
        let mut clip = Clipboard::default();
        let src = Path::new("/a");
        clip.cut(src, names(&["x", "y"]));
        assert!(clip.is_cut(src, OsStr::new("x")));
        assert!(!clip.is_cut(src, OsStr::new("z")));
        assert!(!clip.is_cut(Path::new("/b"), OsStr::new("x")));
        let job = clip.paste(Path::new("/b")).unwrap();
        assert_eq!(job.kind, JobKind::Move);
        assert_eq!(job.names, names(&["x", "y"]));
        assert!(
            clip.is_cut(src, OsStr::new("x")),
            "kept until the move succeeds"
        );
        // A different cut is not dropped by an older job finishing.
        clip.cut(src, names(&["x"]));
        clip.cut_moved(&job);
        assert!(clip.is_cut(src, OsStr::new("x")));
        clip.cut(src, names(&["x", "y"]));
        clip.cut_moved(&job);
        assert!(!clip.is_cut(src, OsStr::new("x")));
        assert!(clip.paste(Path::new("/c")).is_none());
    }

    #[test]
    fn copy_pastes_again_and_cut_into_the_same_folder_does_nothing() {
        let mut clip = Clipboard::default();
        clip.copy(Path::new("/a"), names(&["x"]));
        assert!(!clip.is_cut(Path::new("/a"), OsStr::new("x")));
        assert_eq!(clip.paste(Path::new("/b")).unwrap().kind, JobKind::Copy);
        assert_eq!(clip.paste(Path::new("/c")).unwrap().kind, JobKind::Copy);
        clip.cut(Path::new("/a"), names(&["x"]));
        assert!(clip.paste(Path::new("/a")).is_none());
        assert!(clip.clip().is_some(), "a same-folder paste keeps the cut");
        clip.clear();
        clip.cut(Path::new("/a"), Vec::new());
        assert!(clip.clip().is_none(), "an empty cut keeps nothing");
    }

    #[test]
    fn modifiers_pick_the_drop_op_like_thunar() {
        let m = |ctrl, shift| Mods { ctrl, shift };
        assert_eq!(drop_op(m(false, false), true), Some(Op::Move));
        assert_eq!(drop_op(m(false, false), false), Some(Op::Copy));
        assert_eq!(drop_op(m(true, false), true), Some(Op::Copy));
        assert_eq!(drop_op(m(true, false), false), Some(Op::Copy));
        assert_eq!(drop_op(m(false, true), true), Some(Op::Move));
        assert_eq!(drop_op(m(false, true), false), Some(Op::Move));
        assert_eq!(drop_op(m(true, true), true), None);
    }

    #[test]
    fn a_folder_cannot_be_dropped_into_itself() {
        let src = Path::new("/a");
        assert!(!drop_allowed(
            Op::Move,
            src,
            &names(&["x"]),
            Path::new("/a/x")
        ));
        assert!(!drop_allowed(
            Op::Copy,
            src,
            &names(&["x"]),
            Path::new("/a/x/deep")
        ));
        assert!(!drop_allowed(Op::Move, src, &names(&["x"]), src));
        assert!(drop_allowed(Op::Copy, src, &names(&["x"]), src));
        assert!(drop_allowed(
            Op::Move,
            src,
            &names(&["x"]),
            Path::new("/a/xy")
        ));
    }

    #[test]
    fn copy_and_move_jobs_run() {
        let root = scratch("transfer");
        let (a, b) = (root.join("a"), root.join("b"));
        fs::create_dir_all(a.join("dir")).unwrap();
        fs::create_dir_all(&b).unwrap();
        fs::write(a.join("one.txt"), "1").unwrap();
        fs::write(a.join("dir/two.txt"), "2").unwrap();
        let cancel = Cancel::new();
        let job = Job::new(JobKind::Copy, &a, names(&["one.txt", "dir"]), &b);
        let out = run(&job, &env(&root), &cancel, &mut |_| {});
        assert_eq!(out.done, 2, "{out:?}");
        assert!(a.join("one.txt").exists() && b.join("dir/two.txt").exists());
        // A second copy into the same folder keeps both.
        let again = Job::new(JobKind::Copy, &a, names(&["one.txt"]), &a);
        assert_eq!(run(&again, &env(&root), &cancel, &mut |_| {}).done, 1);
        assert_eq!(fs::read_dir(&a).unwrap().count(), 3);
        let mv = Job::new(JobKind::Move, &a, names(&["one.txt"]), &root);
        let out = run(&mv, &env(&root), &cancel, &mut |_| {});
        assert_eq!(out.toast(JobKind::Move).text, "Moved 1 item");
        assert!(!a.join("one.txt").exists() && root.join("one.txt").exists());
        let into = Job::new(JobKind::Move, &root, names(&["a"]), &a.join("dir"));
        let out = run(&into, &env(&root), &cancel, &mut |_| {});
        assert_eq!(out.failed.len(), 1);
        assert!(a.exists());
        fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn a_symlinked_path_into_the_folder_is_still_itself() {
        let root = scratch("into-link");
        let a = root.join("a");
        fs::create_dir_all(a.join("sub/deep")).unwrap();
        std::os::unix::fs::symlink(a.join("sub"), root.join("link")).unwrap();
        let cancel = Cancel::new();
        for (kind, dest) in [
            (JobKind::Move, root.join("link")),
            (JobKind::Copy, root.join("link/deep")),
            (JobKind::Move, root.join("link/../sub")),
        ] {
            let job = Job::new(kind, &root, names(&["a"]), &dest);
            let out = run(&job, &env(&root), &cancel, &mut |_| {});
            assert_eq!(out.failed.len(), 1, "{kind:?} into {dest:?}: {out:?}");
            assert!(out.failed[0].contains("inside itself"), "{out:?}");
        }
        assert!(a.join("sub/deep").is_dir());
        assert_eq!(fs::read_dir(a.join("sub/deep")).unwrap().count(), 0);
        fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn cancel_stops_between_items() {
        let root = scratch("cancel");
        let (a, b) = (root.join("a"), root.join("b"));
        fs::create_dir_all(&a).unwrap();
        fs::create_dir_all(&b).unwrap();
        for n in ["1", "2", "3"] {
            fs::write(a.join(n), n).unwrap();
        }
        let cancel = Cancel::new();
        let mut seen = Vec::new();
        let job = Job::new(JobKind::Move, &a, names(&["1", "2", "3"]), &b);
        let out = run(&job, &env(&root), &cancel, &mut |p| {
            seen.push(p.clone());
            // Cancel while the first item is starting: it still finishes.
            if p.done == 0 && !p.current.is_empty() {
                cancel.cancel();
            }
        });
        assert!(out.cancelled);
        assert_eq!(out.done, 1);
        assert!(b.join("1").exists());
        assert!(a.join("2").exists() && a.join("3").exists());
        assert!(!b.join("2").exists());
        assert_eq!(seen.last().unwrap().done, 1);
        assert_eq!(seen.last().unwrap().detail(), "1 of 3 items");
        assert_eq!(out.toast(JobKind::Move).tone, "warning");
        fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn trash_then_undo_restores() {
        let root = scratch("undo");
        let dir = root.join("work");
        fs::create_dir_all(dir.join("folder")).unwrap();
        fs::write(dir.join("note.md"), "hi").unwrap();
        fs::write(dir.join("folder/inner"), "x").unwrap();
        let cancel = Cancel::new();
        let job = Job::new(JobKind::Trash, &dir, names(&["note.md", "folder"]), &dir);
        let out = run(&job, &env(&root), &cancel, &mut |_| {});
        assert_eq!(out.done, 2, "{out:?}");
        let toast = out.toast(JobKind::Trash);
        assert_eq!(toast.text, "Moved 2 items to trash");
        assert!(toast.undo);
        assert!(!dir.join("note.md").exists());
        assert!(root.join("trash/files/note.md").exists());
        assert!(root.join("trash/info/note.md.trashinfo").exists());
        let (restored, failed) = undo(&out.trashed);
        assert_eq!((restored, failed.len()), (2, 0));
        assert_eq!(fs::read_to_string(dir.join("note.md")).unwrap(), "hi");
        assert!(dir.join("folder/inner").exists());
        assert!(!root.join("trash/files/note.md").exists());
        assert!(!root.join("trash/info/note.md.trashinfo").exists());
        fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn undo_never_replaces_a_new_file() {
        let root = scratch("undo-clash");
        let dir = root.join("work");
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join("a"), "old").unwrap();
        let job = Job::new(JobKind::Trash, &dir, names(&["a"]), &dir);
        let out = run(&job, &env(&root), &Cancel::new(), &mut |_| {});
        fs::write(dir.join("a"), "new").unwrap();
        let (restored, failed) = undo(&out.trashed);
        assert_eq!((restored, failed.len()), (0, 1));
        assert_eq!(fs::read_to_string(dir.join("a")).unwrap(), "new");
        assert!(root.join("trash/files/a").exists());
        fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn delete_removes_for_good() {
        let root = scratch("delete");
        fs::create_dir_all(root.join("d/e")).unwrap();
        fs::write(root.join("d/e/f"), "x").unwrap();
        let job = Job::new(JobKind::Delete, &root, names(&["d"]), &root);
        let out = run(&job, &env(&root), &Cancel::new(), &mut |_| {});
        assert_eq!(out.done, 1);
        assert!(!root.join("d").exists());
        assert!(!root.join("trash").exists());
        assert_eq!(out.toast(JobKind::Delete).text, "Deleted 1 item");
        fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn labels_name_the_destination() {
        let job = Job::new(
            JobKind::Copy,
            Path::new("/a"),
            names(&["x"]),
            Path::new("/m/SANDISK"),
        );
        assert_eq!(job.label(), "Copying 1 item to SANDISK");
        assert_eq!(job.glyph(), "󰆏");
    }
}
