//! The controller behind the window's file operations: the clipboard, the
//! drag-and-drop choice between copy and move, jobs that run off the UI
//! thread, and undo for trash. Nothing here touches Slint, so it is tested
//! directly.

use std::ffi::{OsStr, OsString};
use std::os::unix::ffi::OsStrExt;
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
/// Why a drop on a folder is or isn't possible.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DropCheck {
    Ok,
    /// Nothing to do: no names, or a move into the folder they are in.
    Nothing,
    /// The folder is one of the dragged folders or inside one.
    IntoItself,
}

pub fn drop_check(op: Op, src: &Path, names: &[OsString], dest: &Path) -> DropCheck {
    if names.is_empty() || (op == Op::Move && src == dest) {
        return DropCheck::Nothing;
    }
    if names.iter().any(|n| inside(&src.join(n), dest)) {
        return DropCheck::IntoItself;
    }
    DropCheck::Ok
}

/// What a drop from another app turns into.
#[derive(Debug, Default)]
pub struct ExternalDrop {
    pub jobs: Vec<Job>,
    /// Items skipped because they are already in the destination, or
    /// are the destination's own ancestors.
    pub already_here: usize,
}

/// Files dropped on the window by another app, as copy jobs into `dest`,
/// one per source folder.
///
/// Always a copy, never a move: winit 0.30's X11 handler canonicalizes
/// every dropped path before Robin sees it, so a dropped symlink arrives
/// as its target, and moving would take the target away from wherever it
/// really lives. Slint doesn't expose the raw `text/uri-list`, so the link
/// itself can't be recovered; copying at worst duplicates the target.
/// winit resolves links before Robin sees a path, so a dropped symlink
/// copies its target. Items already
/// in `dest` (compared after canonicalizing it) and folders dropped into
/// themselves are skipped.
pub fn external_drop(paths: &[PathBuf], dest: &Path) -> ExternalDrop {
    let real_dest = std::fs::canonicalize(dest).unwrap_or_else(|_| dest.to_path_buf());
    let mut out = ExternalDrop::default();
    let mut groups: Vec<(PathBuf, Vec<OsString>)> = Vec::new();
    for path in paths {
        let (Some(parent), Some(name)) = (path.parent(), path.file_name()) else {
            continue;
        };
        match groups.iter_mut().find(|(p, _)| p == parent) {
            Some((_, names)) => {
                if !names.iter().any(|n| n == name) {
                    names.push(name.to_os_string());
                }
            }
            None => groups.push((parent.to_path_buf(), vec![name.to_os_string()])),
        }
    }
    for (src, names) in groups {
        let real_src = std::fs::canonicalize(&src).unwrap_or_else(|_| src.clone());
        if real_src == real_dest || !drop_allowed(Op::Copy, &src, &names, dest) {
            out.already_here += names.len();
            continue;
        }
        out.jobs.push(Job::transfer(Op::Copy, &src, names, dest));
    }
    out
}

pub fn drop_allowed(op: Op, src: &Path, names: &[OsString], dest: &Path) -> bool {
    drop_check(op, src, names, dest) == DropCheck::Ok
}

/// Whether `dest` is the folder `item` or anywhere inside it. Compares
/// device and inode up the destination's real parent chain, so a symlinked
/// or `..`-laden path into the folder is still caught.
pub fn inside(item: &Path, dest: &Path) -> bool {
    use std::os::unix::fs::MetadataExt;
    let Ok(meta) = std::fs::symlink_metadata(item) else {
        return dest.starts_with(item);
    };
    if !meta.is_dir() {
        return false;
    }
    let Ok(real) = std::fs::canonicalize(dest) else {
        return dest.starts_with(item);
    };
    real.ancestors().any(|dir| {
        std::fs::metadata(dir).is_ok_and(|m| m.dev() == meta.dev() && m.ino() == meta.ino())
    })
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum JobKind {
    Copy,
    Move,
    Trash,
    Delete,
    /// Undo of a trash: put `Job::restore` entries back.
    Restore,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Job {
    pub kind: JobKind,
    pub src: PathBuf,
    pub names: Vec<OsString>,
    /// Destination folder for copies and moves. Unused for trash and delete.
    pub dest: PathBuf,
    /// What a Restore job puts back; empty otherwise.
    pub restore: Vec<Trashed>,
    /// Empty Trash: after the deletes, prune orphan trashinfo and stale
    /// directorysizes lines.
    pub empty_trash: bool,
}

impl Job {
    pub fn new(kind: JobKind, src: &Path, names: Vec<OsString>, dest: &Path) -> Self {
        Self {
            kind,
            src: src.to_path_buf(),
            names,
            dest: dest.to_path_buf(),
            restore: Vec::new(),
            empty_trash: false,
        }
    }

    /// Undo a trash job on the job thread.
    pub fn restore(items: Vec<Trashed>) -> Self {
        let first = items
            .first()
            .and_then(|t| t.original.parent())
            .map(Path::to_path_buf)
            .unwrap_or_default();
        let names = items
            .iter()
            .map(|t| t.original.file_name().unwrap_or_default().to_os_string())
            .collect();
        Self {
            kind: JobKind::Restore,
            src: first.clone(),
            names,
            dest: first,
            restore: items,
            empty_trash: false,
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
            JobKind::Restore => "󰕌",
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
            JobKind::Restore => format!("Restoring {n}"),
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
    /// Folders a restore had to recreate, as shown to the user (`~/…`).
    pub recreated: Vec<String>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Toast {
    /// "" for neutral, "warning", or "danger".
    pub tone: &'static str,
    pub text: String,
    pub details: Vec<String>,
    /// The first detail stays at the top of the list, above any "+N more".
    pub pinned: bool,
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
            JobKind::Restore => format!("Restored {n}"),
        };
        if kind == JobKind::Restore {
            let stayed = self.failed.len();
            if let [folder] = self.recreated.as_slice() {
                text = if self.done == 1 {
                    format!("Restored to {folder} (folder recreated)")
                } else {
                    format!("{text}, recreated {folder}")
                };
            } else if self.recreated.len() > 1 {
                text = format!("{text}, recreated {} folders", self.recreated.len());
            }
            return Toast {
                tone: if stayed == 0 { "" } else { "warning" },
                text: if stayed == 0 {
                    text
                } else {
                    format!("{text}, {stayed} stayed in the trash")
                },
                details: self.failed.clone(),
                pinned: false,
                undo: false,
            };
        }
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
        if !self.refused.is_empty() {
            text = format!("{text}, {} left where they were", self.refused.len());
        }
        if !self.failed.is_empty() {
            text = format!("{text}, {} failed", self.failed.len());
        }
        if self.cancelled {
            text = format!("{text}, then cancelled");
        }
        // An error that stopped a merge partway says how far the merge
        // it happened in got; the error is pinned above what didn't make
        // it, then the rest of the job's details.
        if let Some(m) = self.report.merges.first() {
            let mut details = vec![m.error.clone()];
            for level in &self.report.merges {
                details.extend(level.partly.iter().map(|p| format!("{p} — partly merged")));
                details.extend(level.missing.iter().map(|p| format!("{p} — not merged")));
                details.extend(level.skipped.iter().cloned());
            }
            // Job-level skips a merge already showed aren't repeated.
            let mut rest = self.report.clone();
            let covered: Vec<_> = self.report.merges.iter().flat_map(|m| &m.covers).collect();
            rest.skipped.retain(|s| !covered.contains(&&s.path));
            details.extend(rest.details());
            details.extend(self.report.leftovers.iter().cloned());
            // The failure of the merged item itself repeats the error.
            details.extend(
                self.failed
                    .iter()
                    .filter(|f| !f.ends_with(&m.error))
                    .cloned(),
            );
            return Toast {
                tone: "warning",
                text: format!(
                    "Merged {} of {} into {} before an error",
                    m.merged,
                    count(m.total),
                    m.into
                ),
                details,
                pinned: true,
                undo: false,
            };
        }
        let mut details = self.report.details();
        details.extend(self.report.leftovers.iter().cloned());
        details.extend(self.failed.iter().cloned());
        details.extend(
            self.refused
                .iter()
                .map(|(n, _)| format!("{}: not moved, see the dialog", n.to_string_lossy())),
        );
        let tone = if !self.failed.is_empty() {
            "danger"
        } else if !self.report.left_behind.is_empty() || !self.refused.is_empty() || self.cancelled
        {
            "warning"
        } else {
            ""
        };
        Toast {
            tone,
            text,
            details,
            pinned: false,
            undo: kind == JobKind::Trash && !self.trashed.is_empty(),
        }
    }
}

/// Where trash goes for folders on the home filesystem.
#[derive(Clone, Debug)]
pub struct Env {
    /// `None` when `$HOME` is unset: nothing is shown as `~`.
    pub home: Option<PathBuf>,
    pub home_trash: PathBuf,
    /// `$XDG_CONFIG_HOME` when set to an absolute path; else `~/.config`.
    pub config_home: Option<PathBuf>,
}

impl Env {
    /// `$XDG_DATA_HOME/Trash`, or `~/.local/share/Trash`.
    pub fn from_system() -> Self {
        let mut env = Self::from_vars(std::env::var_os("HOME"), std::env::var_os("XDG_DATA_HOME"));
        env.config_home = std::env::var_os("XDG_CONFIG_HOME")
            .map(PathBuf::from)
            .filter(|p| p.is_absolute());
        env
    }

    /// `from_system` with the variables passed in.
    pub fn from_vars(home: Option<std::ffi::OsString>, data: Option<std::ffi::OsString>) -> Self {
        // `/` as home would show every path as `~`: treat it as unset.
        let home = home
            .filter(|v| !v.is_empty() && Path::new(v) != Path::new("/"))
            .map(PathBuf::from);
        let data = data
            .filter(|v| !v.is_empty())
            .map(PathBuf::from)
            .unwrap_or_else(|| {
                home.as_deref()
                    .unwrap_or(Path::new("/"))
                    .join(".local/share")
            });
        Self {
            home_trash: data.join("Trash"),
            home,
            config_home: None,
        }
    }
}

impl Env {
    /// Where `user-dirs.dirs` and friends live.
    pub fn config_dir(&self) -> Option<PathBuf> {
        self.config_home
            .clone()
            .or_else(|| self.home.as_ref().map(|h| h.join(".config")))
    }

    /// A seeded home: trash under its own `.local/share`, never the host's.
    pub fn for_home(home: &Path) -> Self {
        Self {
            home: Some(home.to_path_buf()),
            home_trash: home.join(".local/share/Trash"),
            config_home: None,
        }
    }
}

/// Whether Del can trash things in `dir`, or must offer a permanent delete.
pub fn trash_choice(dir: &Path, env: &Env) -> TrashChoice {
    if env.home.as_deref().is_some_and(|h| same_filesystem(dir, h)) {
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
    run_asking(job, env, cancel, progress, &mut keep_both)
}

/// The conflict question a job asks: answer with a choice, or
/// `Err(Error::Cancelled)` to stop the job.
pub type Ask<'a> = dyn FnMut(&fsops::ConflictPrompt) -> Result<ConflictChoice> + 'a;

/// [`run`], asking `ask` about each name clash. An "apply to all" answer
/// holds for the rest of the job.
pub fn run_asking(
    job: &Job,
    env: &Env,
    cancel: &Cancel,
    progress: &mut dyn FnMut(&Progress),
    ask: &mut Ask<'_>,
) -> Outcome {
    let mut out = Outcome::default();
    let total = job.names.len();
    if job.kind == JobKind::Restore {
        let mut policy = ConflictPolicy::new();
        for (i, t) in job.restore.iter().enumerate() {
            if cancel.is_cancelled() {
                out.cancelled = true;
                break;
            }
            progress(&Progress {
                done: i,
                total,
                current: job.names[i].to_string_lossy().into_owned(),
            });
            let item = Cancel::new();
            let mut opts = CopyOpts {
                cancel: &item,
                policy: &mut policy,
                ask: &mut *ask,
            };
            match restore_one(t, env, &mut opts, &mut out.recreated) {
                Ok(fsops::Restored::Restored) => out.done += 1,
                Ok(fsops::Restored::Skipped) => {}
                Err(Error::Cancelled) => {
                    out.cancelled = true;
                    break;
                }
                Err(e) => out.failed.push(format!("{}: {e}", t.original.display())),
            }
        }
        progress(&Progress {
            done: out.done,
            total,
            current: String::new(),
        });
        return out;
    }
    let mut policy = ConflictPolicy::new();
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
    // Deleting inside a trash's files/ also drops each item's trashinfo.
    let info_dir = match job.kind {
        JobKind::Delete => trash_of(&job.src, env),
        _ => None,
    };
    let info_dir = info_dir.and_then(|fd| {
        let info = fd.open_dir_nofollow(OsStr::new("info")).ok()?;
        Some((fd, info))
    });
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
            JobKind::Copy | JobKind::Move => {
                let mut opts = CopyOpts {
                    cancel: &item,
                    policy: &mut policy,
                    ask: &mut *ask,
                };
                let dest = dest.as_ref().unwrap();
                if job.kind == JobKind::Copy {
                    copy_one(job, &src, dest, name, &mut opts, &mut out)
                } else {
                    move_one(job, &src, dest, name, opts, &mut out)
                }
            }
            JobKind::Trash => trash_one(job, &src, trash.as_ref().unwrap(), name, &mut out),
            JobKind::Delete => fsops::remove_child(&src, name, &mut |_| {}, &item).and_then(|()| {
                info_dir
                    .as_ref()
                    .map_or(Ok(()), |d| fsops::forget_trashinfo(&d.1, name))
            }),
            JobKind::Restore => unreachable!("handled above"),
        };
        match result {
            Ok(()) => out.done += 1,
            // Cancel in the conflict dialog stops the whole job.
            Err(Error::Cancelled) => {
                out.cancelled = true;
                break;
            }
            Err(Error::NotRemovable(kept)) => out.refused.push((name.clone(), kept)),
            Err(e) => out.failed.push(format!("{}: {e}", name.to_string_lossy())),
        }
    }
    if job.empty_trash {
        if let Some((trash, _)) = &info_dir {
            if let Err(e) = tidy_trash(trash, &job.names) {
                out.failed.push(format!("trash info: {e}"));
            }
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

/// The trash folder holding `dir` when `dir` is a trash's `files/`,
/// opened without following a symlink at the trash itself.
fn trash_of(dir: &Path, env: &Env) -> Option<DirFd> {
    let trash = dir.parent()?;
    let uid = rustix::process::getuid().as_raw();
    if dir.file_name()? != "files" || !is_trash_dir(trash, &env.home_trash, uid) {
        return None;
    }
    open_trash(trash, &env.home_trash)
}

/// Open a trash the spec way: `$topdir/.Trash/$uid` only when `.Trash`
/// is a real sticky folder, and never through a symlink at the last
/// step, so a planted link can't point Empty Trash elsewhere.
fn open_trash(trash: &Path, home_trash: &Path) -> Option<DirFd> {
    let name = trash.file_name()?;
    let parent = trash.parent()?;
    if trash != home_trash && parent.file_name().is_some_and(|p| p == ".Trash") {
        let top = DirFd::open(parent.parent()?).ok()?;
        let dot = fsops::open_sticky_trash(&top).ok()??;
        return dot.open_dir_nofollow(name).ok();
    }
    DirFd::open(parent).ok()?.open_dir_nofollow(name).ok()
}

/// After Empty Trash: drop the trashinfo and directorysizes lines of the
/// items this job deleted, and nothing else. Another program's entries,
/// even ones that look orphaned, are left alone.
fn tidy_trash(trash: &DirFd, deleted: &[OsString]) -> Result<()> {
    let files = trash.open_dir_nofollow(OsStr::new("files"))?;
    let gone = |n: &OsStr| {
        crate::fsops::component(n).is_ok_and(|c| {
            rustix::fs::statat(
                files.as_fd(),
                c.as_c_str(),
                rustix::fs::AtFlags::SYMLINK_NOFOLLOW,
            )
            .is_err()
        })
    };
    let gone: Vec<&OsString> = deleted.iter().filter(|n| gone(n)).collect();
    if let Ok(info) = trash.open_dir_nofollow(OsStr::new("info")) {
        for n in &gone {
            fsops::forget_trashinfo(&info, n)?;
        }
    }
    let Some(text) = fsops::read_small(trash, OsStr::new("directorysizes"))? else {
        return Ok(());
    };
    let lines: Vec<&[u8]> = text
        .split(|b| *b == b'\n')
        .filter(|l| !l.is_empty())
        .collect();
    let kept: Vec<&[u8]> = lines
        .iter()
        .copied()
        .filter(|line| {
            let name = line
                .rsplit(|b| *b == b' ')
                .next()
                .and_then(|enc| std::str::from_utf8(enc).ok())
                .and_then(crate::trash::unescape);
            !name.is_some_and(|n| gone.iter().any(|g| g.as_bytes() == n.as_slice()))
        })
        .collect();
    if kept.len() == lines.len() {
        return Ok(());
    }
    if kept.is_empty() {
        return fsops::unlink_child(trash, OsStr::new("directorysizes"));
    }
    let mut out = kept.join(&b'\n');
    out.push(b'\n');
    fsops::replace_file(trash, OsStr::new("directorysizes"), &out)
}

/// `~/…` under home, the full path elsewhere.
fn shown_path(path: &Path, home: Option<&Path>) -> String {
    match home.and_then(|h| path.strip_prefix(h).ok()) {
        Some(rel) if rel.as_os_str().is_empty() => "~".into(),
        Some(rel) => format!("~/{}", rel.display()),
        None => path.display().to_string(),
    }
}

/// Only the home trash and the spec's per-device trashes,
/// `$topdir/.Trash/$uid` and `$topdir/.Trash-$uid`. A user's own folder
/// called `Trash` is just a folder.
fn is_trash_dir(trash: &Path, home_trash: &Path, uid: u32) -> bool {
    if trash == home_trash {
        return true;
    }
    let Some(name) = trash.file_name() else {
        return false;
    };
    if name.to_str() == Some(&format!(".Trash-{uid}")) {
        return true;
    }
    name.to_str() == Some(&uid.to_string())
        && trash
            .parent()
            .and_then(Path::file_name)
            .is_some_and(|p| p == ".Trash")
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
    if inside(&job.src.join(name), &job.dest) {
        return Err(Error::Message("can't put a folder inside itself".into()));
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
    opts: &mut CopyOpts<'_>,
    out: &mut Outcome,
) -> Result<()> {
    into_itself(job, name)?;
    let (_, report) = fsops::copy_instead(
        src,
        name,
        dest,
        name,
        CopyOpts {
            cancel: opts.cancel,
            policy: &mut *opts.policy,
            ask: &mut *opts.ask,
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
    copy: CopyOpts<'_>,
    out: &mut Outcome,
) -> Result<()> {
    if job.src == job.dest {
        return Ok(());
    }
    into_itself(job, name)?;
    let mut opts = MoveOpts {
        force_copy: false,
        fail_before_unlink: false,
        copy,
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

/// Run an undo the way the window does, as a Restore job.
#[cfg(test)]
pub(crate) fn undo(items: &[Trashed]) -> (usize, Vec<String>) {
    let env = Env {
        home: Some(PathBuf::from("/")),
        home_trash: PathBuf::from("/nonexistent"),
        config_home: None,
    };
    let out = run(
        &Job::restore(items.to_vec()),
        &env,
        &Cancel::new(),
        &mut |_| {},
    );
    (out.done, out.failed)
}

/// Put one trashed entry back where it was, asking when the name is taken.
fn restore_one(
    t: &Trashed,
    env: &Env,
    opts: &mut CopyOpts<'_>,
    recreated: &mut Vec<String>,
) -> Result<fsops::Restored> {
    let parent = t
        .original
        .parent()
        .ok_or_else(|| Error::Message("no parent folder".into()))?;
    let name = t.original.file_name().ok_or(Error::InvalidName)?;
    let trash = DirFd::open(&t.trash)?;
    // Like Nautilus: a folder deleted since is recreated, not asked about.
    if std::fs::symlink_metadata(parent).is_err() {
        std::fs::create_dir_all(parent)
            .map_err(|e| Error::Message(format!("{}: {e}", parent.display())))?;
        let shown = shown_path(parent, env.home.as_deref());
        if !recreated.contains(&shown) {
            recreated.push(shown);
        }
    }
    let dest = DirFd::open(parent)?;
    // Replace moves what is there now to the trash rather than deleting it.
    let mut make_room = |dir: &DirFd, name: &OsStr| -> Result<()> {
        let target = trash_target(parent, env)?;
        let original = parent.join(name);
        let info = match &target.top {
            Some(top) => original
                .strip_prefix(top)
                .unwrap_or(&original)
                .to_string_lossy()
                .into_owned(),
            None => original.to_string_lossy().into_owned(),
        };
        fsops::place_in_trash(dir, name, &target.fd, target.choice, &info).map(|_| ())
    };
    fsops::restore_asking(&trash, &t.stored, &dest, name, opts, &mut make_room)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_merge_stopped_by_an_error_says_how_far_it_got() {
        use crate::fsops::PartialMerge;
        let out = Outcome {
            failed: vec![
                "raw-exports: IMG_9.CR3: disk full".into(),
                "notes.txt: Permission denied".into(),
            ],
            report: crate::fsops::TransferReport {
                // Innermost first, as the job records them.
                merges: vec![
                    PartialMerge {
                        into: "raw-exports/day2".into(),
                        merged: 3,
                        total: 6,
                        error: "IMG_9.CR3: disk full".into(),
                        skipped: vec!["raw-exports/day2/pipe — named pipes are not copied".into()],
                        covers: vec!["raw-exports/day2/pipe".into()],
                        missing: vec![
                            "raw-exports/day2/IMG_9.CR3".into(),
                            "raw-exports/day2/IMG_10.CR3".into(),
                        ],
                        ..Default::default()
                    },
                    PartialMerge {
                        into: "raw-exports".into(),
                        merged: 31,
                        total: 33,
                        error: "IMG_9.CR3: disk full".into(),
                        partly: vec!["raw-exports/day2".into()],
                        missing: vec!["raw-exports/day3".into()],
                        ..Default::default()
                    },
                ],
                leftovers: vec!["raw-exports/x — not removed: denied".into()],
                skipped: vec![crate::fsops::SkippedEntry {
                    path: "raw-exports/day2/pipe".into(),
                    reason: crate::fsops::SkipReason::Fifo,
                }],
                ..Default::default()
            },
            ..Default::default()
        };
        let t = out.toast(JobKind::Copy);
        assert_eq!(t.tone, "warning");
        assert!(t.pinned);
        assert_eq!(
            t.text,
            "Merged 3 of 6 items into raw-exports/day2 before an error"
        );
        assert_eq!(
            t.details,
            [
                "IMG_9.CR3: disk full",
                "raw-exports/day2/IMG_9.CR3 — not merged",
                "raw-exports/day2/IMG_10.CR3 — not merged",
                "raw-exports/day2/pipe — named pipes are not copied",
                "raw-exports/day2 — partly merged",
                "raw-exports/day3 — not merged",
                "raw-exports/x — not removed: denied",
                "notes.txt: Permission denied",
            ]
        );
    }
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

    #[test]
    fn a_fixture_home_keeps_trash_inside_it() {
        let env = Env::for_home(Path::new("/tmp/fx"));
        assert_eq!(env.home_trash, Path::new("/tmp/fx/.local/share/Trash"));
    }

    #[test]
    fn an_unset_home_is_no_home_not_root() {
        let env = Env::from_vars(None, None);
        assert_eq!(env.home, None);
        assert_eq!(env.home_trash, PathBuf::from("/.local/share/Trash"));
        let env = Env::from_vars(Some("".into()), Some("/x/data".into()));
        assert_eq!(env.home, None);
        assert_eq!(env.home_trash, PathBuf::from("/x/data/Trash"));
        let env = Env::from_vars(Some("/".into()), None);
        assert_eq!(env.home, None, "HOME=/ is no home");
        let env = Env::from_vars(Some("/home/sky".into()), None);
        assert_eq!(env.home.as_deref(), Some(Path::new("/home/sky")));
    }

    fn env(root: &Path) -> Env {
        Env {
            home: Some(root.to_path_buf()),
            home_trash: root.join("trash"),
            config_home: None,
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

    /// Trash `a` ("old"), write a new `a` ("new"), then restore answering `pick`.
    fn restore_over_new(label: &str, pick: fsops::Conflict) -> (PathBuf, Outcome, Vec<String>) {
        let root = scratch(label);
        let dir = root.join("work");
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join("a"), "old").unwrap();
        let e = env(&root);
        let out = run(
            &Job::new(JobKind::Trash, &dir, names(&["a"]), &dir),
            &e,
            &Cancel::new(),
            &mut |_| {},
        );
        fs::write(dir.join("a"), "new").unwrap();
        let mut asked = Vec::new();
        let back = run_asking(
            &Job::restore(out.trashed),
            &e,
            &Cancel::new(),
            &mut |_| {},
            &mut |p| {
                asked.push(p.name.clone());
                Ok(ConflictChoice {
                    action: pick,
                    apply_all: false,
                })
            },
        );
        (root, back, asked)
    }

    #[test]
    fn deleting_in_the_trash_drops_the_trashinfo_too() {
        let root = scratch("trash-delete");
        fs::write(root.join("a"), "x").unwrap();
        let e = env(&root);
        run(
            &Job::new(JobKind::Trash, &root, names(&["a"]), &root),
            &e,
            &Cancel::new(),
            &mut |_| {},
        );
        let files = root.join("trash/files");
        let out = run(
            &Job::new(JobKind::Delete, &files, names(&["a"]), &files),
            &e,
            &Cancel::new(),
            &mut |_| {},
        );
        assert_eq!(out.done, 1, "{out:?}");
        assert!(!files.join("a").exists());
        assert!(!root.join("trash/info/a.trashinfo").exists());
        fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn deleting_in_a_folder_merely_named_files_keeps_its_neighbours() {
        let root = scratch("files-delete");
        fs::create_dir_all(root.join("proj/files")).unwrap();
        fs::create_dir_all(root.join("proj/info")).unwrap();
        fs::write(root.join("proj/files/a"), "x").unwrap();
        fs::write(root.join("proj/info/a.trashinfo"), "mine").unwrap();
        let files = root.join("proj/files");
        run(
            &Job::new(JobKind::Delete, &files, names(&["a"]), &files),
            &env(&root),
            &Cancel::new(),
            &mut |_| {},
        );
        assert!(root.join("proj/info/a.trashinfo").exists());
        fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn restore_recreates_a_missing_folder_and_says_so() {
        let root = scratch("restore-parent");
        fs::create_dir_all(root.join("Documents/Finance")).unwrap();
        fs::write(root.join("Documents/Finance/q3.ods"), b"q3").unwrap();
        let e = env(&root);
        let out = run(
            &Job::new(
                JobKind::Trash,
                &root.join("Documents/Finance"),
                names(&["q3.ods"]),
                &root,
            ),
            &e,
            &Cancel::new(),
            &mut |_| {},
        );
        fs::remove_dir_all(root.join("Documents")).unwrap();
        let back = run(
            &Job::restore(out.trashed.clone()),
            &e,
            &Cancel::new(),
            &mut |_| {},
        );
        assert_eq!((back.done, back.failed.len()), (1, 0), "{back:?}");
        assert_eq!(
            fs::read(root.join("Documents/Finance/q3.ods")).unwrap(),
            b"q3"
        );
        assert_eq!(
            back.toast(JobKind::Restore).text,
            "Restored to ~/Documents/Finance (folder recreated)"
        );
        fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn empty_trash_tidies_only_what_it_deleted() {
        use std::os::unix::fs::PermissionsExt;
        let root = scratch("empty-tidy");
        let t = root.join("trash");
        fs::create_dir_all(t.join("files/stuck/inner")).unwrap();
        fs::create_dir_all(t.join("info")).unwrap();
        fs::write(t.join("files/a"), "a").unwrap();
        fs::write(t.join("files/stuck/inner/x"), "x").unwrap();
        fs::write(t.join("info/a.trashinfo"), "i").unwrap();
        fs::write(t.join("info/stuck.trashinfo"), "i").unwrap();
        fs::write(t.join("info/ghost.trashinfo"), "orphan").unwrap();
        fs::write(
            t.join("directorysizes"),
            "4096 1 stuck\n4096 1 gone%20dir\n",
        )
        .unwrap();
        fs::set_permissions(
            t.join("files/stuck/inner"),
            fs::Permissions::from_mode(0o555),
        )
        .unwrap();
        let files = t.join("files");
        let mut job = Job::new(JobKind::Delete, &files, names(&["a", "stuck"]), &files);
        job.empty_trash = true;
        let out = run(&job, &env(&root), &Cancel::new(), &mut |_| {});
        fs::set_permissions(
            t.join("files/stuck/inner"),
            fs::Permissions::from_mode(0o755),
        )
        .unwrap();
        assert_eq!(out.done, 1);
        let toast = out.toast(JobKind::Delete);
        assert_eq!(toast.text, "Deleted 1 item, 1 failed");
        assert!(
            toast.details.iter().any(|d| d.starts_with("stuck: ")),
            "{:?}",
            toast.details
        );
        assert!(t.join("info/ghost.trashinfo").exists(), "not ours to prune");
        assert!(!t.join("info/a.trashinfo").exists());
        assert!(
            t.join("info/stuck.trashinfo").exists(),
            "kept with its item"
        );
        assert_eq!(
            fs::read_to_string(t.join("directorysizes")).unwrap(),
            "4096 1 stuck\n4096 1 gone%20dir\n"
        );
        fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn empty_trash_never_writes_through_a_directorysizes_link() {
        let root = scratch("dirsizes");
        let t = root.join("trash");
        fs::create_dir_all(t.join("files/d")).unwrap();
        fs::create_dir_all(t.join("info")).unwrap();
        fs::write(t.join("info/d.trashinfo"), "i").unwrap();
        fs::write(root.join("victim"), "4096 1 d\n4096 1 keep\n").unwrap();
        std::os::unix::fs::symlink(root.join("victim"), t.join("directorysizes")).unwrap();
        let files = t.join("files");
        let mut job = Job::new(JobKind::Delete, &files, names(&["d"]), &files);
        job.empty_trash = true;
        let _ = run(&job, &env(&root), &Cancel::new(), &mut |_| {});
        assert_eq!(
            fs::read_to_string(root.join("victim")).unwrap(),
            "4096 1 d\n4096 1 keep\n"
        );
        assert!(
            t.join("directorysizes").is_symlink(),
            "refused, not followed"
        );
        fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn a_volume_trash_counts_only_under_a_real_sticky_dot_trash() {
        use std::os::unix::fs::PermissionsExt;
        let uid = rustix::process::getuid().as_raw().to_string();
        let root = scratch("sticky");
        let home_trash = root.join("home-trash");
        let make = |top: &Path| {
            let t = top.join(".Trash").join(&uid);
            fs::create_dir_all(t.join("info")).unwrap();
            t
        };
        let real = make(&root.join("ok"));
        assert!(open_trash(&real, &home_trash).is_none(), "not sticky");
        fs::set_permissions(root.join("ok/.Trash"), fs::Permissions::from_mode(0o1777)).unwrap();
        assert!(open_trash(&real, &home_trash).is_some());
        fs::create_dir_all(root.join("ln")).unwrap();
        std::os::unix::fs::symlink(root.join("ok/.Trash"), root.join("ln/.Trash")).unwrap();
        assert!(open_trash(&root.join("ln/.Trash").join(&uid), &home_trash).is_none());
        let dash = root.join("vol").join(format!(".Trash-{uid}"));
        fs::create_dir_all(root.join("vol")).unwrap();
        std::os::unix::fs::symlink(&real, &dash).unwrap();
        assert!(open_trash(&dash, &home_trash).is_none(), "symlinked trash");
        fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn a_projects_trash_folder_is_not_a_trash() {
        let root = scratch("projects-trash");
        fs::create_dir_all(root.join("Projects/Trash/files")).unwrap();
        fs::create_dir_all(root.join("Projects/Trash/info")).unwrap();
        fs::write(root.join("Projects/Trash/files/a"), "x").unwrap();
        fs::write(root.join("Projects/Trash/info/a.trashinfo"), "mine").unwrap();
        let files = root.join("Projects/Trash/files");
        run(
            &Job::new(JobKind::Delete, &files, names(&["a"]), &files),
            &env(&root),
            &Cancel::new(),
            &mut |_| {},
        );
        assert!(root.join("Projects/Trash/info/a.trashinfo").exists());
        fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn only_the_spec_trashes_count() {
        let home = Path::new("/h/.local/share/Trash");
        assert!(is_trash_dir(home, home, 1000));
        assert!(is_trash_dir(Path::new("/mnt/d/.Trash-1000"), home, 1000));
        assert!(is_trash_dir(Path::new("/mnt/d/.Trash/1000"), home, 1000));
        assert!(!is_trash_dir(Path::new("/mnt/d/.Trash-1001"), home, 1000));
        assert!(!is_trash_dir(Path::new("/mnt/d/.Trash/1001"), home, 1000));
        assert!(!is_trash_dir(Path::new("/h/Projects/Trash"), home, 1000));
        assert!(!is_trash_dir(Path::new("/h/.Trashy"), home, 1000));
    }

    #[test]
    fn restore_onto_a_taken_name_asks_and_skip_leaves_it_in_the_trash() {
        let (root, back, asked) = restore_over_new("restore-skip", fsops::Conflict::Skip);
        assert_eq!(asked, ["a"]);
        assert_eq!((back.done, back.failed.len()), (0, 0));
        assert_eq!(fs::read_to_string(root.join("work/a")).unwrap(), "new");
        assert!(root.join("trash/files/a").exists());
        fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn restore_keep_both_lands_under_a_fresh_name() {
        let (root, back, _) = restore_over_new("restore-keep", fsops::Conflict::Rename);
        assert_eq!(back.done, 1, "{back:?}");
        assert_eq!(fs::read_to_string(root.join("work/a")).unwrap(), "new");
        assert_eq!(fs::read_to_string(root.join("work/a (2)")).unwrap(), "old");
        assert!(!root.join("trash/info/a.trashinfo").exists());
        fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn restore_replace_trashes_the_newer_file_instead_of_deleting_it() {
        let (root, back, _) = restore_over_new("restore-replace", fsops::Conflict::Replace);
        assert_eq!(back.done, 1, "{back:?}");
        assert_eq!(fs::read_to_string(root.join("work/a")).unwrap(), "old");
        let kept: Vec<_> = fs::read_dir(root.join("trash/files"))
            .unwrap()
            .flatten()
            .collect();
        assert_eq!(kept.len(), 1);
        assert_eq!(fs::read_to_string(kept[0].path()).unwrap(), "new");
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

    fn clash(label: &str, files: &[&str]) -> (PathBuf, PathBuf) {
        let root = scratch(label);
        let (src, dest) = (root.join("src"), root.join("dest"));
        fs::create_dir_all(&src).unwrap();
        fs::create_dir_all(&dest).unwrap();
        for f in files {
            fs::write(src.join(f), b"new").unwrap();
            fs::write(dest.join(f), b"old").unwrap();
        }
        (root, dest)
    }

    #[test]
    fn conflict_answers_and_apply_to_all() {
        let (root, dest) = clash("conflict-all", &["a", "b", "c"]);
        let job = Job::new(
            JobKind::Copy,
            &root.join("src"),
            names(&["a", "b", "c"]),
            &dest,
        );
        let mut asked = Vec::new();
        let mut script = vec![
            ConflictChoice {
                action: fsops::Conflict::Replace,
                apply_all: false,
            },
            ConflictChoice {
                action: fsops::Conflict::Skip,
                apply_all: true,
            },
        ]
        .into_iter();
        let out = run_asking(&job, &env(&root), &Cancel::new(), &mut |_| {}, &mut |p| {
            asked.push(p.name.clone());
            Ok(script.next().expect("asked once too often"))
        });
        assert_eq!(asked, ["a", "b"], "apply to all answers c without asking");
        assert_eq!(fs::read(dest.join("a")).unwrap(), b"new");
        assert_eq!(fs::read(dest.join("b")).unwrap(), b"old");
        assert_eq!(fs::read(dest.join("c")).unwrap(), b"old");
        assert_eq!(out.done, 3, "skipped items still finish");
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn keep_both_never_overwrites() {
        let (root, dest) = clash("conflict-keep", &["a.txt"]);
        for kind in [JobKind::Copy, JobKind::Move] {
            let job = Job::new(kind, &root.join("src"), names(&["a.txt"]), &dest);
            if kind == JobKind::Move {
                fs::write(root.join("src/a.txt"), b"moved").unwrap();
            }
            run_asking(&job, &env(&root), &Cancel::new(), &mut |_| {}, &mut |_| {
                Ok(ConflictChoice {
                    action: fsops::Conflict::Rename,
                    apply_all: false,
                })
            });
        }
        assert_eq!(fs::read(dest.join("a.txt")).unwrap(), b"old");
        assert_eq!(fs::read(dest.join("a (2).txt")).unwrap(), b"new");
        assert_eq!(fs::read(dest.join("a (3).txt")).unwrap(), b"moved");
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn cancelling_a_conflict_stops_the_job() {
        let (root, dest) = clash("conflict-cancel", &["a", "b"]);
        let job = Job::new(JobKind::Move, &root.join("src"), names(&["a", "b"]), &dest);
        let mut n = 0;
        let out = run_asking(&job, &env(&root), &Cancel::new(), &mut |_| {}, &mut |_| {
            n += 1;
            Err(Error::Cancelled)
        });
        assert!(out.cancelled);
        assert_eq!(n, 1, "nothing is asked after Cancel");
        assert!(root.join("src/a").exists() && root.join("src/b").exists());
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn drop_through_a_symlink_into_itself_is_refused() {
        let root = scratch("drop-link");
        fs::create_dir_all(root.join("Projects/inner")).unwrap();
        std::os::unix::fs::symlink(root.join("Projects/inner"), root.join("shortcut")).unwrap();
        let check = drop_check(
            Op::Move,
            &root,
            &names(&["Projects"]),
            &root.join("shortcut"),
        );
        assert_eq!(check, DropCheck::IntoItself);
        assert!(!drop_allowed(
            Op::Copy,
            &root,
            &names(&["Projects"]),
            &root.join("shortcut")
        ));
        fs::create_dir_all(root.join("Other")).unwrap();
        assert!(drop_allowed(
            Op::Move,
            &root,
            &names(&["Projects"]),
            &root.join("Other")
        ));
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn restore_job_puts_trashed_files_back() {
        let root = scratch("restore");
        fs::write(root.join("a"), b"a").unwrap();
        let e = env(&root);
        let out = run(
            &Job::new(JobKind::Trash, &root, names(&["a"]), &root),
            &e,
            &Cancel::new(),
            &mut |_| {},
        );
        assert!(!root.join("a").exists());
        let back = run(
            &Job::restore(out.trashed.clone()),
            &e,
            &Cancel::new(),
            &mut |_| {},
        );
        assert_eq!((back.done, back.failed.len()), (1, 0));
        assert_eq!(fs::read(root.join("a")).unwrap(), b"a");
        assert!(back.toast(JobKind::Restore).text.starts_with("Restored"));
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn outside_drops_group_by_folder_and_skip_what_is_already_here() {
        let root = std::env::temp_dir().join(format!("robin-xdrop-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let (a, b, dest) = (root.join("a"), root.join("b"), root.join("dest"));
        for d in [&a, &b, &dest, &dest.join("sub")] {
            std::fs::create_dir_all(d).unwrap();
        }
        let paths = vec![
            a.join("1.jpg"),
            b.join("2.jpg"),
            a.join("3.jpg"),
            a.join("1.jpg"),
            dest.join("here.txt"),
            PathBuf::from("/"),
        ];
        let jobs = external_drop(&paths, &dest).jobs;
        assert_eq!(jobs.len(), 2);
        assert_eq!(jobs[0].src, a);
        assert_eq!(jobs[0].names, ["1.jpg", "3.jpg"]);
        assert_eq!(jobs[1].src, b);
        assert_eq!(jobs[1].names, ["2.jpg"]);
        assert!(
            jobs.iter()
                .all(|j| j.dest == dest && j.kind == JobKind::Copy),
            "outside drops copy even on the same filesystem"
        );
        assert!(
            external_drop(std::slice::from_ref(&root), &dest.join("sub"))
                .jobs
                .is_empty()
        );
        // The same folder through a symlinked path is still "already here".
        std::os::unix::fs::symlink(&dest, root.join("dest-link")).unwrap();
        let via_link = external_drop(&[dest.join("here.txt")], &root.join("dest-link"));
        assert!(via_link.jobs.is_empty());
        assert_eq!(via_link.already_here, 1);
        std::fs::remove_dir_all(&root).unwrap();
    }
}
