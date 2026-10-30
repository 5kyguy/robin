//! Explicit writes. Browsing never calls this module.
//!
//! Deletes and copies go through a directory file descriptor and do not
//! follow the final symlink. A same-filesystem move is `renameat2` with
//! `RENAME_NOREPLACE`. Across filesystems the file is copied, fsynced,
//! renamed, the destination directory is fsynced, and only then is the
//! source removed.

use std::fs::File;
use std::io::{Read, Write};
use std::os::fd::AsFd;
use std::os::unix::ffi::OsStrExt;
use std::path::Path;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use rustix::fs::{AtFlags, Mode, OFlags, RenameFlags};
use rustix::io::Errno;

use crate::error::{Error, Result};
use crate::safety::DirFd;

const MAX_DEPTH: u32 = 1024;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TrashChoice {
    /// Same filesystem as the home data directory.
    Home,
    /// The volume's own `.Trash/$uid` or `.Trash-$uid`.
    Volume,
    /// Volume trash cannot be created. Never copy onto the home trash.
    Ask,
}

pub fn choose_trash(same_filesystem_as_home: bool, volume_trash_creatable: bool) -> TrashChoice {
    if same_filesystem_as_home {
        TrashChoice::Home
    } else if volume_trash_creatable {
        TrashChoice::Volume
    } else {
        TrashChoice::Ask
    }
}

/// Whether a copy transferred bytes. A move deletes the source only for [`Copied`](CopyOutcome::Copied).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CopyOutcome {
    Copied,
    Skipped,
}

/// What the user picked in the conflict dialog.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Conflict {
    Replace,
    Rename,
    Skip,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ConflictChoice {
    pub action: Conflict,
    pub apply_all: bool,
}

/// Map the dialog buttons onto [`Conflict`]. Skip is the focused default.
pub fn choice_from_buttons(
    skip: bool,
    rename: bool,
    replace: bool,
    apply_all: bool,
) -> Result<ConflictChoice> {
    let action = match (skip, rename, replace) {
        (true, false, false) => Conflict::Skip,
        (false, true, false) => Conflict::Rename,
        (false, false, true) => Conflict::Replace,
        _ => {
            return Err(Error::Message(
                "choose one of skip, rename, or replace".into(),
            ))
        }
    };
    Ok(ConflictChoice { action, apply_all })
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ConflictPrompt {
    pub name: String,
    pub existing_size: u64,
    pub existing_mtime: i64,
    pub incoming_size: u64,
    pub incoming_mtime: i64,
}

/// Remembers an "apply to all" answer for the rest of a transfer.
#[derive(Debug, Default)]
pub struct ConflictPolicy {
    remembered: Option<Conflict>,
}

impl ConflictPolicy {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn choose(
        &mut self,
        prompt: &ConflictPrompt,
        ask: &mut dyn FnMut(&ConflictPrompt) -> Result<ConflictChoice>,
    ) -> Result<Conflict> {
        if let Some(action) = self.remembered {
            return Ok(action);
        }
        let choice = ask(prompt)?;
        if choice.apply_all {
            self.remembered = Some(choice.action);
        }
        Ok(choice.action)
    }
}

pub struct Cancel {
    flag: AtomicBool,
    stop_after: AtomicU64,
    written: AtomicU64,
}

impl Cancel {
    pub fn new() -> Self {
        Self {
            flag: AtomicBool::new(false),
            stop_after: AtomicU64::new(u64::MAX),
            written: AtomicU64::new(0),
        }
    }

    pub fn cancel(&self) {
        self.flag.store(true, Ordering::Relaxed);
    }

    pub fn is_cancelled(&self) -> bool {
        self.flag.load(Ordering::Relaxed)
    }

    /// Cancel once at least `n` bytes have been written. Tests use this to
    /// stop in the middle of a copy.
    pub fn stop_after_bytes(&self, n: u64) {
        self.stop_after.store(n, Ordering::Relaxed);
    }

    fn note_written(&self, n: u64) {
        let total = self.written.fetch_add(n, Ordering::Relaxed) + n;
        if total >= self.stop_after.load(Ordering::Relaxed) {
            self.cancel();
        }
    }
}

impl Default for Cancel {
    fn default() -> Self {
        Self::new()
    }
}

pub struct CopyOpts<'a> {
    pub cancel: &'a Cancel,
    pub policy: &'a mut ConflictPolicy,
    pub ask: &'a mut dyn FnMut(&ConflictPrompt) -> Result<ConflictChoice>,
}

pub fn create_file(parent: &DirFd, name: &std::ffi::OsStr) -> Result<()> {
    let c = component(name)?;
    let fd = rustix::fs::openat(
        parent.as_fd(),
        c.as_c_str(),
        OFlags::WRONLY | OFlags::CREATE | OFlags::EXCL | OFlags::NOFOLLOW | OFlags::CLOEXEC,
        Mode::from_raw_mode(0o644),
    )
    .map_err(|e| Error::from_errno(name, e))?;
    let _ = fd;
    Ok(())
}

pub fn create_dir(parent: &DirFd, name: &std::ffi::OsStr) -> Result<()> {
    let c = component(name)?;
    rustix::fs::mkdirat(parent.as_fd(), c.as_c_str(), Mode::from_raw_mode(0o755))
        .map_err(|e| Error::from_errno(name, e))
}

pub fn rename_child(parent: &DirFd, from: &std::ffi::OsStr, to: &std::ffi::OsStr) -> Result<()> {
    let from_c = component(from)?;
    let to_c = component(to)?;
    rename_noreplace(parent.as_fd(), &from_c, parent.as_fd(), &to_c).map_err(|e| {
        if e == Errno::EXIST {
            Error::Message(format!("not overwriting {}", to.to_string_lossy()))
        } else {
            Error::from_errno(to, e)
        }
    })
}

/// Remove `name` inside `parent`. Symlinks are unlinked, not followed.
/// `before` runs after the stat that decides how to delete, so a test can
/// swap the entry before it is opened or unlinked.
pub fn remove_child(
    parent: &DirFd,
    name: &std::ffi::OsStr,
    before: &mut dyn FnMut(&std::ffi::OsStr),
    cancel: &Cancel,
) -> Result<()> {
    let root_dev = rustix::fs::fstat(parent.as_fd())
        .map_err(|e| Error::from_errno(name, e))?
        .st_dev;
    // Depth is known before the first unlink, so a too-deep tree is left whole.
    ensure_depth(parent, name, root_dev, 0)?;
    remove_tree(parent, name, before, cancel, root_dev, 0)
}

fn ensure_depth(parent: &DirFd, name: &std::ffi::OsStr, root_dev: u64, depth: u32) -> Result<()> {
    if depth >= MAX_DEPTH {
        return Err(Error::Message("directory is too deep".into()));
    }
    let c = component(name)?;
    let stat = rustix::fs::statat(parent.as_fd(), c.as_c_str(), AtFlags::SYMLINK_NOFOLLOW)
        .map_err(|e| Error::from_errno(name, e))?;
    if stat.st_dev != root_dev {
        return Err(Error::Message(
            "refusing to cross filesystems while deleting".into(),
        ));
    }
    if is_lnk(stat.st_mode) || !is_dir(stat.st_mode) {
        return Ok(());
    }
    let child = parent.open_dir_nofollow(name)?;
    for entry in child.list()? {
        ensure_depth(&child, &entry.name, root_dev, depth + 1)?;
    }
    Ok(())
}

fn remove_tree(
    parent: &DirFd,
    name: &std::ffi::OsStr,
    before: &mut dyn FnMut(&std::ffi::OsStr),
    cancel: &Cancel,
    root_dev: u64,
    depth: u32,
) -> Result<()> {
    if cancel.is_cancelled() {
        return Err(Error::Cancelled);
    }
    if depth >= MAX_DEPTH {
        return Err(Error::Message("directory is too deep".into()));
    }
    let c = component(name)?;
    let stat = rustix::fs::statat(parent.as_fd(), c.as_c_str(), AtFlags::SYMLINK_NOFOLLOW)
        .map_err(|e| Error::from_errno(name, e))?;
    if stat.st_dev != root_dev {
        return Err(Error::Message(
            "refusing to cross filesystems while deleting".into(),
        ));
    }
    before(name);
    if is_lnk(stat.st_mode) || !is_dir(stat.st_mode) {
        rustix::fs::unlinkat(parent.as_fd(), c.as_c_str(), AtFlags::empty())
            .map_err(|e| Error::from_errno(name, e))?;
        return Ok(());
    }
    let child = parent.open_dir_nofollow(name)?;
    if cancel.is_cancelled() {
        return Err(Error::Cancelled);
    }
    for entry in child.list()? {
        remove_tree(&child, &entry.name, before, cancel, root_dev, depth + 1)?;
    }
    rustix::fs::unlinkat(parent.as_fd(), c.as_c_str(), AtFlags::REMOVEDIR)
        .map_err(|e| Error::from_errno(name, e))
}

pub fn copy_file(
    parent: &DirFd,
    from: &std::ffi::OsStr,
    dest_parent: &DirFd,
    to: &std::ffi::OsStr,
    opts: &mut CopyOpts<'_>,
) -> Result<CopyOutcome> {
    if opts.cancel.is_cancelled() {
        return Err(Error::Cancelled);
    }
    let from_c = component(from)?;
    let src = rustix::fs::openat(
        parent.as_fd(),
        from_c.as_c_str(),
        OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
        Mode::empty(),
    )
    .map_err(|e| Error::from_errno(from, e))?;
    let stat = rustix::fs::fstat(&src).map_err(|e| Error::from_errno(from, e))?;
    if is_lnk(stat.st_mode) {
        return Err(Error::Message("refusing to copy through a symlink".into()));
    }
    if is_dir(stat.st_mode) {
        return Err(Error::Message("refusing to copy a directory".into()));
    }
    let dest = resolve_dest_name(dest_parent, to, &stat, opts)?;
    let Some(DestName {
        name: dest_name,
        replace,
    }) = dest
    else {
        return Ok(CopyOutcome::Skipped);
    };
    let to_c = component(dest_name.as_os_str())?;
    let tmp_name = partial_name();
    let tmp = std::ffi::OsString::from(&tmp_name);
    let tmp_c = component(tmp.as_os_str())?;
    let dest = rustix::fs::openat(
        dest_parent.as_fd(),
        tmp_c.as_c_str(),
        OFlags::WRONLY | OFlags::CREATE | OFlags::EXCL | OFlags::NOFOLLOW | OFlags::CLOEXEC,
        Mode::from_raw_mode(0o600),
    )
    .map_err(|e| Error::from_errno(&tmp, e))?;
    let mut guard = UnlinkOnDrop {
        dir: dest_parent.as_fd(),
        name: tmp_c,
        live: true,
    };
    let copied = if opts.cancel.stop_after.load(Ordering::Relaxed) == u64::MAX {
        try_copy_range(&src, &dest, opts.cancel)?
    } else {
        false
    };
    if !copied {
        let mut input = File::from(src);
        let mut output = File::from(dest);
        let mut buf = [0u8; 8192];
        loop {
            if opts.cancel.is_cancelled() {
                return Err(Error::Cancelled);
            }
            let n = input.read(&mut buf).map_err(|e| Error::io(from, e))?;
            if n == 0 {
                break;
            }
            output
                .write_all(&buf[..n])
                .map_err(|e| Error::io(&dest_name, e))?;
            opts.cancel.note_written(n as u64);
            if opts.cancel.is_cancelled() {
                return Err(Error::Cancelled);
            }
        }
        rustix::fs::fchmod(&output, Mode::from_raw_mode(stat.st_mode & 0o777))
            .map_err(|e| Error::from_errno(&dest_name, e))?;
        set_times(&output, &stat).map_err(|e| Error::from_errno(&dest_name, e))?;
        output.sync_all().map_err(|e| Error::io(&dest_name, e))?;
        copy_xattrs(&input, &output)?;
        drop(output);
    } else {
        let output = File::from(dest);
        rustix::fs::fchmod(&output, Mode::from_raw_mode(stat.st_mode & 0o777))
            .map_err(|e| Error::from_errno(&dest_name, e))?;
        set_times(&output, &stat).map_err(|e| Error::from_errno(&dest_name, e))?;
        output.sync_all().map_err(|e| Error::io(&dest_name, e))?;
        copy_xattrs(&src, output.as_fd())?;
        drop(output);
    }
    if replace {
        rustix::fs::renameat(
            dest_parent.as_fd(),
            guard.name.as_c_str(),
            dest_parent.as_fd(),
            to_c.as_c_str(),
        )
        .map_err(|e| Error::from_errno(&dest_name, e))?;
    } else {
        land_keep_both(dest_parent, to, dest_name, |c| {
            rename_noreplace(dest_parent.as_fd(), &guard.name, dest_parent.as_fd(), c)
        })?;
    }
    guard.live = false;
    Ok(CopyOutcome::Copied)
}

pub struct MoveOpts<'a> {
    pub force_copy: bool,
    pub fail_before_unlink: bool,
    pub copy: CopyOpts<'a>,
    /// What moved and what was left behind, for the completion toast.
    pub report: TransferReport,
    /// Walk the source first and refuse the move if anything in it could
    /// not be removed after copying. Tests turn it off to reach the
    /// fallback for removals that fail after the check.
    pub check_removable: bool,
    /// Copy only: never remove the source. Set by [`copy_instead`].
    pub keep_source: bool,
}

/// Why a copied original could not be removed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum KeepReason {
    /// The folder holding the entry is not writable by us.
    ReadOnlyFolder,
    /// The folder is sticky and neither it nor the entry is ours.
    StickyFolder,
    ReadOnlyFilesystem,
    /// A folder we can't open, so nothing inside it can be removed.
    UnreadableFolder,
}

impl KeepReason {
    pub fn describe(self) -> &'static str {
        match self {
            KeepReason::ReadOnlyFolder => "read-only folder",
            KeepReason::StickyFolder => "shared folder owned by someone else",
            KeepReason::ReadOnlyFilesystem => "read-only drive",
            KeepReason::UnreadableFolder => "folder can't be opened",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct KeptEntry {
    /// Relative to the moved item, starting with its own name.
    pub path: std::path::PathBuf,
    pub reason: KeepReason,
}

/// How a transfer ended, for picking the toast.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TransferStatus {
    Complete,
    /// Some entries were skipped on purpose and stay in the source.
    Skipped,
    /// Everything was copied but some originals could not be removed.
    Partial,
}

fn keep_reason(err: &Error, sticky: bool) -> Option<KeepReason> {
    if io_is(err, Errno::PERM) && sticky {
        Some(KeepReason::StickyFolder)
    } else if io_is(err, Errno::ACCESS) || io_is(err, Errno::PERM) {
        Some(KeepReason::ReadOnlyFolder)
    } else if io_is(err, Errno::ROFS) {
        Some(KeepReason::ReadOnlyFilesystem)
    } else {
        None
    }
}

/// Why a cross-device move left an entry where it was.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SkipReason {
    /// Another filesystem is mounted here. Moving would empty that volume.
    MountPoint,
    Fifo,
    Socket,
    Device,
}

impl SkipReason {
    pub fn describe(self) -> &'static str {
        match self {
            SkipReason::MountPoint => "another drive is mounted here",
            SkipReason::Fifo => "named pipes are not copied",
            SkipReason::Socket => "sockets are not copied",
            SkipReason::Device => "device files are not copied",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SkippedEntry {
    /// Relative to the moved item, starting with its own name.
    pub path: std::path::PathBuf,
    pub reason: SkipReason,
}

/// The result of a transfer. Skipped entries stay in the source and are
/// listed so they never vanish silently.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct TransferReport {
    /// Files and symlinks copied to the destination.
    pub moved: usize,
    pub skipped: Vec<SkippedEntry>,
    /// Copied, but the original is still in the source.
    pub left_behind: Vec<KeptEntry>,
}

impl TransferReport {
    pub fn status(&self) -> TransferStatus {
        if !self.left_behind.is_empty() {
            TransferStatus::Partial
        } else if !self.skipped.is_empty() {
            TransferStatus::Skipped
        } else {
            TransferStatus::Complete
        }
    }

    /// The toast line, e.g. "Moved 212 items, skipped 3".
    pub fn summary(&self) -> String {
        let items = if self.moved == 1 { "item" } else { "items" };
        if let Some(first) = self.left_behind.first() {
            let mixed = self.left_behind.iter().any(|k| k.reason != first.reason);
            let why = if mixed {
                "see details"
            } else {
                first.reason.describe()
            };
            return format!(
                "Copied {} {items}, but couldn't remove the originals ({why})",
                self.moved
            );
        }
        if self.skipped.is_empty() {
            format!("Moved {} {items}", self.moved)
        } else {
            format!(
                "Moved {} {items}, skipped {}",
                self.moved,
                self.skipped.len()
            )
        }
    }

    /// One line per skipped entry for the Details list.
    pub fn details(&self) -> Vec<String> {
        let kept = self.left_behind.iter().map(|k| {
            format!(
                "{} — left behind: {}",
                k.path.display(),
                k.reason.describe()
            )
        });
        let skipped = self
            .skipped
            .iter()
            .map(|s| format!("{} — {}", s.path.display(), s.reason.describe()));
        kept.chain(skipped).collect()
    }
}

/// The up-front refusal, worded for the move-refused dialog.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Refusal {
    /// "Can't move Projects: 1 folder inside is read-only" for one reason,
    /// "Can't move Projects: 2 items can't be removed afterwards" for several.
    pub title: String,
    /// The blocking paths: the read-only or unreadable folders themselves,
    /// and the items in shared folders or on read-only drives.
    pub lines: Vec<RefusalLine>,
    /// False when a folder can't be read: it can't be copied either, so the
    /// dialog offers a single Close button.
    pub can_copy_instead: bool,
    /// Label of the button that dismisses the dialog: "Cancel" next to
    /// Copy instead, "Close" when it is the only button.
    pub dismiss_label: &'static str,
}

/// One blocking path in the dialog, with a note shown under it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RefusalLine {
    pub path: String,
    pub note: Option<String>,
}

pub const UNREADABLE_NOTE: &str = "Robin can't read this folder, so it can't be copied either.";

/// What the move-refused dialog offers. Copy instead is the default.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum RefusalChoice {
    #[default]
    CopyInstead,
    Cancel,
}

fn plural(n: usize, one: &str, many: &str) -> String {
    format!("{n} {}", if n == 1 { one } else { many })
}

pub fn refusal(name: &str, blocked: &[KeptEntry]) -> Refusal {
    let mut folders: Vec<std::path::PathBuf> = Vec::new();
    let mut shared = Vec::new();
    let mut drive = Vec::new();
    let mut locked = Vec::new();
    for k in blocked {
        match k.reason {
            KeepReason::ReadOnlyFolder => {
                let dir = k.path.parent().unwrap_or(&k.path).to_path_buf();
                if !folders.contains(&dir) {
                    folders.push(dir);
                }
            }
            KeepReason::StickyFolder => shared.push(k.path.clone()),
            KeepReason::ReadOnlyFilesystem => drive.push(k.path.clone()),
            KeepReason::UnreadableFolder => locked.push(k.path.clone()),
        }
    }
    let mut parts = Vec::new();
    if !folders.is_empty() {
        let verb = if folders.len() == 1 { "is" } else { "are" };
        parts.push(format!(
            "{} inside {verb} read-only",
            plural(folders.len(), "folder", "folders")
        ));
    }
    if !locked.is_empty() {
        parts.push(format!(
            "{} inside can't be opened",
            plural(locked.len(), "folder", "folders")
        ));
    }
    if !shared.is_empty() {
        let verb = if shared.len() == 1 {
            "belongs"
        } else {
            "belong"
        };
        parts.push(format!(
            "{} {verb} to someone else",
            plural(shared.len(), "item", "items")
        ));
    }
    if !drive.is_empty() {
        parts.push("the drive is read-only".to_string());
    }
    let line = |p: &std::path::PathBuf, note: Option<&str>| RefusalLine {
        path: p.display().to_string(),
        note: note.map(str::to_string),
    };
    let lines = folders
        .iter()
        .map(|p| line(p, None))
        .chain(locked.iter().map(|p| line(p, Some(UNREADABLE_NOTE))))
        .chain(shared.iter().chain(&drive).map(|p| line(p, None)))
        .collect();
    let can_copy_instead = locked.is_empty();
    let why = if parts.len() == 1 {
        parts.remove(0)
    } else {
        format!(
            "{} can't be removed afterwards",
            plural(blocked.len(), "item", "items")
        )
    };
    Refusal {
        title: format!("Can't move {name}: {why}"),
        lines,
        can_copy_instead,
        dismiss_label: if can_copy_instead { "Cancel" } else { "Close" },
    }
}

impl Refusal {
    /// Lines that carry a note; the dialog grows one row for each.
    pub fn note_count(&self) -> usize {
        self.lines.iter().filter(|l| l.note.is_some()).count()
    }
}

/// Copy `from` to `to` without touching the source: the move-refused
/// dialog's Copy instead.
pub fn copy_instead(
    parent: &DirFd,
    from: &std::ffi::OsStr,
    dest_parent: &DirFd,
    to: &std::ffi::OsStr,
    copy: CopyOpts<'_>,
) -> Result<(CopyOutcome, TransferReport)> {
    // A folder we can't open can't be copied either: refuse before copying.
    let from_c = component(from)?;
    let stat = rustix::fs::statat(parent.as_fd(), from_c.as_c_str(), AtFlags::SYMLINK_NOFOLLOW)
        .map_err(|e| Error::from_errno(from, e))?;
    let mut blocked = Vec::new();
    removal_blockers(parent, from, stat.st_dev, Path::new(from), 0, &mut blocked)?;
    blocked.retain(|k| k.reason == KeepReason::UnreadableFolder);
    if !blocked.is_empty() {
        return Err(Error::Unreadable(blocked));
    }
    let mut opts = MoveOpts {
        force_copy: true,
        fail_before_unlink: false,
        copy,
        report: TransferReport::default(),
        check_removable: false,
        keep_source: true,
    };
    let outcome = move_child(parent, from, dest_parent, to, &mut opts)?;
    Ok((outcome, opts.report))
}

/// Act on the move-refused dialog. Cancel does nothing and returns `None`.
pub fn resolve_refusal(
    choice: RefusalChoice,
    parent: &DirFd,
    from: &std::ffi::OsStr,
    dest_parent: &DirFd,
    to: &std::ffi::OsStr,
    copy: CopyOpts<'_>,
) -> Result<Option<(CopyOutcome, TransferReport)>> {
    match choice {
        RefusalChoice::Cancel => Ok(None),
        RefusalChoice::CopyInstead => copy_instead(parent, from, dest_parent, to, copy).map(Some),
    }
}

/// Collect every entry under `from` that a move could copy but not remove.
fn removal_blockers(
    parent: &DirFd,
    from: &std::ffi::OsStr,
    root_dev: u64,
    rel: &Path,
    depth: u32,
    out: &mut Vec<KeptEntry>,
) -> Result<()> {
    if depth >= MAX_DEPTH {
        return Err(Error::Message("directory is too deep".into()));
    }
    let c = component(from)?;
    let stat = rustix::fs::statat(parent.as_fd(), c.as_c_str(), AtFlags::SYMLINK_NOFOLLOW)
        .map_err(|e| Error::from_errno(from, e))?;
    let kind = classify(stat.st_mode, stat.st_dev, root_dev);
    if let EntryKind::Skip(_) = kind {
        return Ok(());
    }
    if let Some(reason) = removal_denied(parent, &stat, rel)? {
        out.push(KeptEntry {
            path: rel.to_path_buf(),
            reason,
        });
    }
    if kind == EntryKind::Dir {
        let opened = parent.open_dir_nofollow(from).and_then(|child| {
            let entries = child.list()?;
            Ok((child, entries))
        });
        let (child, entries) = match opened {
            Ok(v) => v,
            Err(e) if io_is(&e, Errno::ACCESS) || io_is(&e, Errno::PERM) => {
                out.push(KeptEntry {
                    path: rel.to_path_buf(),
                    reason: KeepReason::UnreadableFolder,
                });
                return Ok(());
            }
            Err(e) => return Err(e),
        };
        for entry in entries {
            removal_blockers(
                &child,
                &entry.name,
                root_dev,
                &rel.join(&entry.name),
                depth + 1,
                out,
            )?;
        }
    }
    Ok(())
}

/// Whether unlinking an entry with `stat` from `parent` would be refused.
/// Only a permission answer is a reason; any other failure is an error.
fn removal_denied(
    parent: &DirFd,
    stat: &rustix::fs::Stat,
    rel: &Path,
) -> Result<Option<KeepReason>> {
    match rustix::fs::accessat(
        parent.as_fd(),
        c".",
        rustix::fs::Access::WRITE_OK | rustix::fs::Access::EXEC_OK,
        AtFlags::EACCESS,
    ) {
        Ok(()) => {}
        Err(Errno::ROFS) => return Ok(Some(KeepReason::ReadOnlyFilesystem)),
        Err(Errno::ACCESS | Errno::PERM) => return Ok(Some(KeepReason::ReadOnlyFolder)),
        Err(e) => return Err(Error::from_errno(rel, e)),
    }
    let dir = rustix::fs::fstat(parent.as_fd()).map_err(|e| Error::from_errno(rel, e))?;
    if sticky_blocks(&dir, stat) {
        return Ok(Some(KeepReason::StickyFolder));
    }
    Ok(None)
}

/// A sticky folder lets only the entry's owner, the folder's owner, or root
/// remove an entry.
fn sticky_blocks(dir: &rustix::fs::Stat, entry: &rustix::fs::Stat) -> bool {
    let uid = rustix::process::geteuid().as_raw();
    dir.st_mode & 0o1000 != 0 && uid != 0 && uid != entry.st_uid && uid != dir.st_uid
}

/// Whether `from` in `parent` is held by sticky-folder rules.
fn sticky_denies(parent: &DirFd, from: &std::ffi::OsStr) -> bool {
    let Ok(c) = component(from) else {
        return false;
    };
    let (Ok(dir), Ok(entry)) = (
        rustix::fs::fstat(parent.as_fd()),
        rustix::fs::statat(parent.as_fd(), c.as_c_str(), AtFlags::SYMLINK_NOFOLLOW),
    ) else {
        return false;
    };
    sticky_blocks(&dir, &entry)
}

impl TransferReport {
    /// Record a removal that failed after the copy. `parent` and `from`
    /// are used to tell a sticky folder's EPERM from a read-only one.
    fn keep(
        &mut self,
        parent: &DirFd,
        from: &std::ffi::OsStr,
        rel: &Path,
        err: Error,
    ) -> Result<CopyOutcome> {
        match keep_reason(&err, sticky_denies(parent, from)) {
            Some(reason) => {
                self.left_behind.push(KeptEntry {
                    path: rel.to_path_buf(),
                    reason,
                });
                Ok(CopyOutcome::Skipped)
            }
            None => Err(err),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum EntryKind {
    Dir,
    File,
    Symlink,
    Skip(SkipReason),
}

/// Decide how a cross-device move treats an entry. Anything on a different
/// device than the moved root is a mount point and is never descended into.
fn classify(mode: u32, dev: u64, root_dev: u64) -> EntryKind {
    if dev != root_dev {
        return EntryKind::Skip(SkipReason::MountPoint);
    }
    match mode & 0o170000 {
        0o040000 => EntryKind::Dir,
        0o100000 => EntryKind::File,
        0o120000 => EntryKind::Symlink,
        0o010000 => EntryKind::Skip(SkipReason::Fifo),
        0o140000 => EntryKind::Skip(SkipReason::Socket),
        _ => EntryKind::Skip(SkipReason::Device),
    }
}

/// Move by rename when it works. `force_copy` exercises the cross-device path.
/// The source is removed only when the transfer completed. Skip leaves it.
pub fn move_child(
    parent: &DirFd,
    from: &std::ffi::OsStr,
    dest_parent: &DirFd,
    to: &std::ffi::OsStr,
    opts: &mut MoveOpts<'_>,
) -> Result<CopyOutcome> {
    if !opts.force_copy {
        let from_c = component(from)?;
        let to_c = component(to)?;
        let outcome = match rename_noreplace(parent.as_fd(), &from_c, dest_parent.as_fd(), &to_c) {
            Ok(()) => Some(CopyOutcome::Copied),
            Err(err) if err == Errno::XDEV => None,
            Err(err) if err == Errno::EXIST => Some(replace_or_rename(
                parent,
                from,
                dest_parent,
                to,
                &mut opts.copy,
            )?),
            Err(err) => return Err(Error::from_errno(to, err)),
        };
        if let Some(outcome) = outcome {
            if outcome == CopyOutcome::Copied {
                opts.report.moved += 1;
            }
            return Ok(outcome);
        }
    }
    let from_c = component(from)?;
    let stat = rustix::fs::statat(parent.as_fd(), from_c.as_c_str(), AtFlags::SYMLINK_NOFOLLOW)
        .map_err(|e| Error::from_errno(from, e))?;
    let rel = std::path::PathBuf::from(from);
    if opts.check_removable && !opts.keep_source {
        let mut blocked = Vec::new();
        removal_blockers(parent, from, stat.st_dev, &rel, 0, &mut blocked)?;
        if !blocked.is_empty() {
            return Err(Error::NotRemovable(blocked));
        }
    }
    move_entry(parent, from, dest_parent, to, opts, stat.st_dev, &rel, 0)
}

#[allow(clippy::too_many_arguments)]
fn move_entry(
    parent: &DirFd,
    from: &std::ffi::OsStr,
    dest_parent: &DirFd,
    to: &std::ffi::OsStr,
    opts: &mut MoveOpts<'_>,
    root_dev: u64,
    rel: &Path,
    depth: u32,
) -> Result<CopyOutcome> {
    if opts.copy.cancel.is_cancelled() {
        return Err(Error::Cancelled);
    }
    let from_c = component(from)?;
    let stat = rustix::fs::statat(parent.as_fd(), from_c.as_c_str(), AtFlags::SYMLINK_NOFOLLOW)
        .map_err(|e| Error::from_errno(from, e))?;
    match classify(stat.st_mode, stat.st_dev, root_dev) {
        EntryKind::Skip(reason) => {
            opts.report.skipped.push(SkippedEntry {
                path: rel.to_path_buf(),
                reason,
            });
            Ok(CopyOutcome::Skipped)
        }
        EntryKind::Dir => move_directory(parent, from, dest_parent, to, opts, root_dev, rel, depth),
        EntryKind::Symlink => move_symlink(parent, from, dest_parent, to, &stat, opts, rel),
        EntryKind::File => {
            if copy_file(parent, from, dest_parent, to, &mut opts.copy)? == CopyOutcome::Skipped {
                return Ok(CopyOutcome::Skipped);
            }
            rustix::fs::fsync(dest_parent.as_fd()).map_err(|e| Error::from_errno(to, e))?;
            if opts.fail_before_unlink {
                return Err(Error::Message(
                    "injected failure before unlinking the source".into(),
                ));
            }
            opts.report.moved += 1;
            if opts.keep_source {
                return Ok(CopyOutcome::Copied);
            }
            match remove_child(parent, from, &mut |_| {}, opts.copy.cancel) {
                Ok(()) => Ok(CopyOutcome::Copied),
                Err(err) => opts.report.keep(parent, from, rel, err),
            }
        }
    }
}

/// Recreate a symlink on the destination with the same target, then unlink
/// the source link. The target is never followed.
fn move_symlink(
    parent: &DirFd,
    from: &std::ffi::OsStr,
    dest_parent: &DirFd,
    to: &std::ffi::OsStr,
    stat: &rustix::fs::Stat,
    opts: &mut MoveOpts<'_>,
    rel: &Path,
) -> Result<CopyOutcome> {
    let from_c = component(from)?;
    let target = rustix::fs::readlinkat(parent.as_fd(), from_c.as_c_str(), Vec::new())
        .map_err(|e| Error::from_errno(from, e))?;
    let Some(DestName {
        name: dest_name,
        replace,
    }) = resolve_dest_name(dest_parent, to, stat, &mut opts.copy)?
    else {
        return Ok(CopyOutcome::Skipped);
    };
    if replace
        && exists(dest_parent, dest_name.as_os_str())?
        && is_directory(dest_parent, dest_name.as_os_str())?
    {
        return Err(Error::Message(format!(
            "refusing to replace directory {} with a symlink",
            dest_name.to_string_lossy()
        )));
    }
    let to_c = component(dest_name.as_os_str())?;
    let tmp = std::ffi::OsString::from(partial_name());
    let tmp_c = component(tmp.as_os_str())?;
    rustix::fs::symlinkat(target.as_c_str(), dest_parent.as_fd(), tmp_c.as_c_str())
        .map_err(|e| Error::from_errno(&tmp, e))?;
    let mut guard = UnlinkOnDrop {
        dir: dest_parent.as_fd(),
        name: tmp_c,
        live: true,
    };
    let dest_name = if replace {
        rustix::fs::renameat(
            dest_parent.as_fd(),
            guard.name.as_c_str(),
            dest_parent.as_fd(),
            to_c.as_c_str(),
        )
        .map_err(|e| Error::from_errno(&dest_name, e))?;
        dest_name
    } else {
        land_keep_both(dest_parent, to, dest_name, |c| {
            rename_noreplace(dest_parent.as_fd(), &guard.name, dest_parent.as_fd(), c)
        })?
    };
    guard.live = false;
    rustix::fs::fsync(dest_parent.as_fd()).map_err(|e| Error::from_errno(&dest_name, e))?;
    if opts.fail_before_unlink {
        return Err(Error::Message(
            "injected failure before unlinking the source".into(),
        ));
    }
    opts.report.moved += 1;
    if opts.keep_source {
        return Ok(CopyOutcome::Copied);
    }
    match rustix::fs::unlinkat(parent.as_fd(), from_c.as_c_str(), AtFlags::empty()) {
        Ok(()) => Ok(CopyOutcome::Copied),
        Err(e) => opts
            .report
            .keep(parent, from, rel, Error::from_errno(from, e)),
    }
}

/// Copy a directory across devices. The source directory is removed only when
/// every child was [`CopyOutcome::Copied`]. A new destination directory is
/// created private and writable, and gets the source mode and times only
/// after its children are in, so a read-only source still copies.
#[allow(clippy::too_many_arguments)]
fn move_directory(
    parent: &DirFd,
    from: &std::ffi::OsStr,
    dest_parent: &DirFd,
    to: &std::ffi::OsStr,
    opts: &mut MoveOpts<'_>,
    root_dev: u64,
    rel: &Path,
    depth: u32,
) -> Result<CopyOutcome> {
    if opts.copy.cancel.is_cancelled() {
        return Err(Error::Cancelled);
    }
    if depth >= MAX_DEPTH {
        return Err(Error::Message("directory is too deep".into()));
    }
    let from_c = component(from)?;
    let stat = rustix::fs::statat(parent.as_fd(), from_c.as_c_str(), AtFlags::SYMLINK_NOFOLLOW)
        .map_err(|e| Error::from_errno(from, e))?;
    let dest = resolve_dest_name(dest_parent, to, &stat, &mut opts.copy)?;
    let Some(DestName {
        name: dest_name,
        replace,
    }) = dest
    else {
        return Ok(CopyOutcome::Skipped);
    };
    // Replace merges into the existing folder; anything else makes a new
    // one and never merges into a folder that appeared meanwhile.
    let (dest_name, created) = if replace {
        if !is_directory(dest_parent, dest_name.as_os_str())? {
            return Err(Error::Message(format!(
                "refusing to replace a file with directory {}",
                dest_name.to_string_lossy()
            )));
        }
        (dest_name, false)
    } else {
        let name = land_keep_both(dest_parent, to, dest_name, |c| {
            rustix::fs::mkdirat(
                dest_parent.as_fd(),
                c.as_c_str(),
                Mode::from_raw_mode(0o700),
            )
        })?;
        (name, true)
    };
    let src = parent.open_dir_nofollow(from)?;
    let dst = dest_parent.open_dir_nofollow(dest_name.as_os_str())?;
    let mut all_copied = true;
    // Children first; the mode and times are applied even if a child fails,
    // so a partial copy does not stay 0700 or carry the copy time.
    let children = (|| -> Result<()> {
        for entry in src.list()? {
            let child_rel = rel.join(&entry.name);
            let outcome = move_entry(
                &src,
                &entry.name,
                &dst,
                &entry.name,
                opts,
                root_dev,
                &child_rel,
                depth + 1,
            )?;
            if outcome == CopyOutcome::Skipped {
                all_copied = false;
            }
        }
        Ok(())
    })();
    if created {
        rustix::fs::fchmod(dst.as_fd(), Mode::from_raw_mode(stat.st_mode & 0o777))
            .map_err(|e| Error::from_errno(&dest_name, e))?;
        set_times(dst.as_fd(), &stat).map_err(|e| Error::from_errno(&dest_name, e))?;
    }
    children?;
    rustix::fs::fsync(dst.as_fd()).map_err(|e| Error::from_errno(&dest_name, e))?;
    if !all_copied {
        return Ok(CopyOutcome::Skipped);
    }
    if opts.keep_source {
        return Ok(CopyOutcome::Copied);
    }
    if let Err(e) = rustix::fs::unlinkat(parent.as_fd(), from_c.as_c_str(), AtFlags::REMOVEDIR) {
        return opts
            .report
            .keep(parent, from, rel, Error::from_errno(from, e));
    }
    rustix::fs::fsync(dest_parent.as_fd()).map_err(|e| Error::from_errno(&dest_name, e))?;
    Ok(CopyOutcome::Copied)
}

/// Where a removable volume keeps its trash.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum VolumeTrash {
    /// `$topdir/.Trash/$uid` — `.Trash` is a sticky directory and not a symlink.
    Sticky { uid_dir: String },
    /// `$topdir/.Trash-$uid`, created mode `0700`.
    Private { dir: String },
}

pub fn volume_trash_plan(top: &DirFd) -> Result<VolumeTrash> {
    let uid = rustix::process::getuid().as_raw();
    let name = std::ffi::OsStr::new(".Trash");
    let c = component(name)?;
    match rustix::fs::statat(top.as_fd(), c.as_c_str(), AtFlags::SYMLINK_NOFOLLOW) {
        Ok(stat) if is_dir(stat.st_mode) && !is_lnk(stat.st_mode) && stat.st_mode & 0o1000 != 0 => {
            Ok(VolumeTrash::Sticky {
                uid_dir: format!(".Trash/{uid}"),
            })
        }
        Ok(_) | Err(Errno::NOENT) => Ok(VolumeTrash::Private {
            dir: format!(".Trash-{uid}"),
        }),
        Err(err) => Err(Error::from_errno(name, err)),
    }
}

/// Open the volume trash, creating `$topdir/.Trash/$uid` or `$topdir/.Trash-$uid`.
/// `files/` and `info/` are created the same way: `O_NOFOLLOW`, mode `0700`, owned by the user.
pub fn open_volume_trash(top: &DirFd) -> Result<DirFd> {
    let uid = rustix::process::getuid().as_raw();
    let root = match open_sticky_trash(top)? {
        Some(dot) => open_or_create_owned(&dot, &std::ffi::OsString::from(uid.to_string()), 0o700)?,
        None => open_or_create_owned(
            top,
            &std::ffi::OsString::from(format!(".Trash-{uid}")),
            0o700,
        )?,
    };
    let _files = open_or_create_owned(&root, std::ffi::OsStr::new("files"), 0o700)?;
    let _info = open_or_create_owned(&root, std::ffi::OsStr::new("info"), 0o700)?;
    Ok(root)
}

fn open_sticky_trash(top: &DirFd) -> Result<Option<DirFd>> {
    let name = std::ffi::OsStr::new(".Trash");
    match top.open_dir_nofollow(name) {
        Ok(dir) => {
            let stat = rustix::fs::fstat(dir.as_fd()).map_err(|e| Error::from_errno(name, e))?;
            if is_dir(stat.st_mode) && !is_lnk(stat.st_mode) && stat.st_mode & 0o1000 != 0 {
                Ok(Some(dir))
            } else {
                Ok(None)
            }
        }
        Err(err)
            if io_is(&err, Errno::NOENT)
                || io_is(&err, Errno::LOOP)
                || io_is(&err, Errno::NOTDIR) =>
        {
            Ok(None)
        }
        Err(err) => Err(err),
    }
}

fn open_or_create_owned(parent: &DirFd, name: &std::ffi::OsStr, mode: u32) -> Result<DirFd> {
    match parent.open_dir_nofollow(name) {
        Ok(dir) => {
            verify_owned(&dir, name, mode)?;
            Ok(dir)
        }
        Err(err) if io_is(&err, Errno::NOENT) => {
            let c = component(name)?;
            let created = match rustix::fs::mkdirat(
                parent.as_fd(),
                c.as_c_str(),
                Mode::from_raw_mode(mode),
            ) {
                Ok(()) => true,
                Err(Errno::EXIST) => false,
                Err(err) => return Err(Error::from_errno(name, err)),
            };
            let dir = parent.open_dir_nofollow(name)?;
            if created {
                rustix::fs::fchmod(dir.as_fd(), Mode::from_raw_mode(mode))
                    .map_err(|e| Error::from_errno(name, e))?;
            }
            verify_owned(&dir, name, mode)?;
            Ok(dir)
        }
        Err(err) if io_is(&err, Errno::LOOP) => Err(Error::Message(format!(
            "{} is a symlink",
            name.to_string_lossy()
        ))),
        Err(err) => Err(err),
    }
}

fn verify_owned(dir: &DirFd, name: &std::ffi::OsStr, mode: u32) -> Result<()> {
    let stat = rustix::fs::fstat(dir.as_fd()).map_err(|e| Error::from_errno(name, e))?;
    let uid = rustix::process::getuid().as_raw();
    if !is_dir(stat.st_mode) || is_lnk(stat.st_mode) {
        return Err(Error::Message(format!(
            "{} is not a directory",
            name.to_string_lossy()
        )));
    }
    if stat.st_uid != uid {
        return Err(Error::Message(format!(
            "{} is not owned by uid {uid}",
            name.to_string_lossy()
        )));
    }
    if stat.st_mode & 0o777 != mode {
        return Err(Error::Message(format!(
            "{} mode is {:o}, want {mode:o}",
            name.to_string_lossy(),
            stat.st_mode & 0o777
        )));
    }
    Ok(())
}

fn io_is(err: &Error, errno: Errno) -> bool {
    match err {
        Error::Io { source, .. } => source.raw_os_error() == Some(errno.raw_os_error()),
        _ => false,
    }
}

pub fn place_in_trash(
    parent: &DirFd,
    name: &std::ffi::OsStr,
    trash: &DirFd,
    choice: TrashChoice,
    info_path: &str,
) -> Result<std::ffi::OsString> {
    if matches!(choice, TrashChoice::Ask) {
        return Err(Error::Message(
            "this drive has no trash and files are not copied home".into(),
        ));
    }
    let files = open_or_create_owned(trash, std::ffi::OsStr::new("files"), 0o700)?;
    let info_dir = open_or_create_owned(trash, std::ffi::OsStr::new("info"), 0o700)?;
    let body = format!(
        "[Trash Info]\nPath={}\nDeletionDate={}\n",
        percent_encode(info_path.as_bytes()),
        deletion_date()
    );
    let from_c = component(name)?;
    for candidate in trash_candidates(name) {
        let info_label = format!("{}.trashinfo", candidate.to_string_lossy());
        let info_name = std::ffi::OsString::from(&info_label);
        let info_c = component(info_name.as_os_str())?;
        let fd = match rustix::fs::openat(
            info_dir.as_fd(),
            info_c.as_c_str(),
            OFlags::WRONLY | OFlags::CREATE | OFlags::EXCL | OFlags::NOFOLLOW | OFlags::CLOEXEC,
            Mode::from_raw_mode(0o600),
        ) {
            Ok(fd) => fd,
            Err(Errno::EXIST) => continue,
            Err(err) => return Err(Error::from_errno(&info_name, err)),
        };
        {
            let mut file = File::from(fd);
            file.write_all(body.as_bytes())
                .map_err(|e| Error::io(&info_name, e))?;
            file.sync_all().map_err(|e| Error::io(&info_name, e))?;
        }
        rustix::fs::fsync(info_dir.as_fd()).map_err(|e| Error::from_errno(&info_name, e))?;
        let data_c = component(candidate.as_os_str())?;
        match rename_noreplace(parent.as_fd(), &from_c, files.as_fd(), &data_c) {
            Ok(()) => {
                rustix::fs::fsync(files.as_fd()).map_err(|e| Error::from_errno(name, e))?;
                return Ok(candidate);
            }
            Err(Errno::EXIST) => {
                let _ = rustix::fs::unlinkat(info_dir.as_fd(), info_c.as_c_str(), AtFlags::empty());
            }
            Err(err) => {
                let _ = rustix::fs::unlinkat(info_dir.as_fd(), info_c.as_c_str(), AtFlags::empty());
                return Err(Error::from_errno(name, err));
            }
        }
    }
    Err(Error::Message("trash name is exhausted".into()))
}

/// Put a trashed entry back: `files/<stored>` is renamed to `name` in
/// `dest_parent` without replacing anything there, then its `.trashinfo` is
/// removed. An existing `name` leaves the entry in the trash.
pub fn restore_from_trash(
    trash: &DirFd,
    stored: &std::ffi::OsStr,
    dest_parent: &DirFd,
    name: &std::ffi::OsStr,
) -> Result<()> {
    let files = trash.open_dir_nofollow(std::ffi::OsStr::new("files"))?;
    let info_dir = trash.open_dir_nofollow(std::ffi::OsStr::new("info"))?;
    let stored_c = component(stored)?;
    let name_c = component(name)?;
    rename_noreplace(files.as_fd(), &stored_c, dest_parent.as_fd(), &name_c).map_err(|e| {
        if e == Errno::EXIST {
            Error::Message(format!(
                "{} already exists, left it in the trash",
                name.to_string_lossy()
            ))
        } else {
            Error::from_errno(name, e)
        }
    })?;
    rustix::fs::fsync(dest_parent.as_fd()).map_err(|e| Error::from_errno(name, e))?;
    let info = std::ffi::OsString::from(format!("{}.trashinfo", stored.to_string_lossy()));
    let info_c = component(&info)?;
    match rustix::fs::unlinkat(info_dir.as_fd(), info_c.as_c_str(), AtFlags::empty()) {
        Ok(()) | Err(Errno::NOENT) => Ok(()),
        Err(err) => Err(Error::from_errno(&info, err)),
    }
}

/// Walk up from `path` to the top of its filesystem.
pub fn mount_top(path: &Path) -> Result<std::path::PathBuf> {
    let dev = |p: &Path| {
        rustix::fs::stat(p)
            .map(|s| s.st_dev)
            .map_err(|e| Error::from_errno(p, e))
    };
    let own = dev(path)?;
    let mut top = path.to_path_buf();
    while let Some(parent) = top.parent() {
        if dev(parent)? != own {
            break;
        }
        top = parent.to_path_buf();
    }
    Ok(top)
}

fn trash_candidates(name: &std::ffi::OsStr) -> Vec<std::ffi::OsString> {
    let mut out = Vec::with_capacity(1000);
    out.push(name.to_os_string());
    for n in 1..1000 {
        out.push(std::ffi::OsString::from(format!(
            "{}.{n}",
            name.to_string_lossy()
        )));
    }
    out
}

struct UnlinkOnDrop<'a> {
    dir: rustix::fd::BorrowedFd<'a>,
    name: std::ffi::CString,
    live: bool,
}

impl Drop for UnlinkOnDrop<'_> {
    fn drop(&mut self) {
        if self.live {
            let _ = rustix::fs::unlinkat(self.dir, self.name.as_c_str(), AtFlags::empty());
        }
    }
}

/// Where an entry lands. Only an explicit Replace may overwrite; every
/// other landing is no-replace and picks a new keep-both name if the slot
/// was taken after we looked.
struct DestName {
    name: std::ffi::OsString,
    replace: bool,
}

fn resolve_dest_name(
    dest_parent: &DirFd,
    to: &std::ffi::OsStr,
    incoming: &rustix::fs::Stat,
    opts: &mut CopyOpts<'_>,
) -> Result<Option<DestName>> {
    if !exists(dest_parent, to)? {
        return Ok(Some(DestName {
            name: to.to_os_string(),
            replace: false,
        }));
    }
    let action = settle_conflict_stat(dest_parent, to, incoming, opts)?;
    match action {
        Conflict::Skip => Ok(None),
        Conflict::Replace => Ok(Some(DestName {
            name: to.to_os_string(),
            replace: true,
        })),
        Conflict::Rename => Ok(Some(DestName {
            name: fresh_name(dest_parent, to)?,
            replace: false,
        })),
    }
}

/// Retries when the chosen name appears between choosing it and landing.
const KEEP_BOTH_TRIES: usize = 16;

#[cfg(test)]
type LandingHook = Box<dyn FnMut(&DirFd, &std::ffi::OsStr)>;

#[cfg(test)]
thread_local! {
    /// Runs right before each final no-replace landing, to simulate
    /// another program creating the target in that window.
    pub(crate) static BEFORE_LANDING: std::cell::RefCell<Option<LandingHook>> =
        std::cell::RefCell::new(None);
}

/// Land an entry under `first` in `dest_parent` with `op`, which must fail
/// with EEXIST rather than replace. On EEXIST pick the next keep-both name
/// for `base` and try again, a bounded number of times.
fn land_keep_both(
    dest_parent: &DirFd,
    base: &std::ffi::OsStr,
    first: std::ffi::OsString,
    mut op: impl FnMut(&std::ffi::CString) -> std::result::Result<(), Errno>,
) -> Result<std::ffi::OsString> {
    let mut name = first;
    for _ in 0..KEEP_BOTH_TRIES {
        #[cfg(test)]
        BEFORE_LANDING.with(|h| {
            if let Some(hook) = h.borrow_mut().as_mut() {
                hook(dest_parent, &name);
            }
        });
        let c = component(name.as_os_str())?;
        match op(&c) {
            Ok(()) => return Ok(name),
            Err(Errno::EXIST) => name = fresh_name(dest_parent, base)?,
            Err(err) => return Err(Error::from_errno(&name, err)),
        }
    }
    Err(Error::Message(format!(
        "{} kept being taken; nothing was replaced",
        base.to_string_lossy()
    )))
}

fn settle_conflict(
    parent: &DirFd,
    from: &std::ffi::OsStr,
    dest_parent: &DirFd,
    to: &std::ffi::OsStr,
    opts: &mut CopyOpts<'_>,
) -> Result<Conflict> {
    let from_c = component(from)?;
    let stat = rustix::fs::statat(parent.as_fd(), from_c.as_c_str(), AtFlags::SYMLINK_NOFOLLOW)
        .map_err(|e| Error::from_errno(from, e))?;
    settle_conflict_stat(dest_parent, to, &stat, opts)
}

fn settle_conflict_stat(
    dest_parent: &DirFd,
    to: &std::ffi::OsStr,
    incoming: &rustix::fs::Stat,
    opts: &mut CopyOpts<'_>,
) -> Result<Conflict> {
    let to_c = component(to)?;
    let existing = rustix::fs::statat(
        dest_parent.as_fd(),
        to_c.as_c_str(),
        AtFlags::SYMLINK_NOFOLLOW,
    )
    .map_err(|e| Error::from_errno(to, e))?;
    let prompt = ConflictPrompt {
        name: to.to_string_lossy().into_owned(),
        existing_size: existing.st_size as u64,
        existing_mtime: existing.st_mtime,
        incoming_size: incoming.st_size as u64,
        incoming_mtime: incoming.st_mtime,
    };
    opts.policy.choose(&prompt, opts.ask)
}

fn replace_or_rename(
    parent: &DirFd,
    from: &std::ffi::OsStr,
    dest_parent: &DirFd,
    to: &std::ffi::OsStr,
    opts: &mut CopyOpts<'_>,
) -> Result<CopyOutcome> {
    match settle_conflict(parent, from, dest_parent, to, opts)? {
        Conflict::Skip => Ok(CopyOutcome::Skipped),
        Conflict::Replace => {
            let from_c = component(from)?;
            let to_c = component(to)?;
            rustix::fs::renameat(
                parent.as_fd(),
                from_c.as_c_str(),
                dest_parent.as_fd(),
                to_c.as_c_str(),
            )
            .map_err(|e| Error::from_errno(to, e))?;
            Ok(CopyOutcome::Copied)
        }
        Conflict::Rename => {
            let from_c = component(from)?;
            land_keep_both(dest_parent, to, fresh_name(dest_parent, to)?, |c| {
                rename_noreplace(parent.as_fd(), &from_c, dest_parent.as_fd(), c)
            })?;
            Ok(CopyOutcome::Copied)
        }
    }
}

fn fresh_name(dir: &DirFd, name: &std::ffi::OsStr) -> Result<std::ffi::OsString> {
    for n in 2..1000 {
        let candidate = numbered(name, n);
        if !exists(dir, &candidate)? {
            return Ok(candidate);
        }
    }
    Err(Error::Message(format!(
        "no free name for {}",
        name.to_string_lossy()
    )))
}

fn numbered(name: &std::ffi::OsStr, n: u32) -> std::ffi::OsString {
    let text = name.to_string_lossy();
    if let Some((stem, ext)) = text.rsplit_once('.') {
        if !stem.is_empty() {
            return std::ffi::OsString::from(format!("{stem} ({n}).{ext}"));
        }
    }
    std::ffi::OsString::from(format!("{text} ({n})"))
}

fn exists(dir: &DirFd, name: &std::ffi::OsStr) -> Result<bool> {
    let c = component(name)?;
    match rustix::fs::statat(dir.as_fd(), c.as_c_str(), AtFlags::SYMLINK_NOFOLLOW) {
        Ok(_) => Ok(true),
        Err(Errno::NOENT) => Ok(false),
        Err(err) => Err(Error::from_errno(name, err)),
    }
}

fn is_directory(dir: &DirFd, name: &std::ffi::OsStr) -> Result<bool> {
    let c = component(name)?;
    let stat = rustix::fs::statat(dir.as_fd(), c.as_c_str(), AtFlags::SYMLINK_NOFOLLOW)
        .map_err(|e| Error::from_errno(name, e))?;
    Ok(is_dir(stat.st_mode) && !is_lnk(stat.st_mode))
}

fn rename_noreplace(
    from_dir: impl AsFd,
    from: &std::ffi::CString,
    to_dir: impl AsFd,
    to: &std::ffi::CString,
) -> std::result::Result<(), Errno> {
    rustix::fs::renameat_with(
        from_dir,
        from.as_c_str(),
        to_dir,
        to.as_c_str(),
        RenameFlags::NOREPLACE,
    )
}

fn try_copy_range(src: impl AsFd, dst: impl AsFd, cancel: &Cancel) -> Result<bool> {
    let mut off_in = 0u64;
    let mut off_out = 0u64;
    loop {
        if cancel.is_cancelled() {
            return Err(Error::Cancelled);
        }
        match rustix::fs::copy_file_range(
            &src,
            Some(&mut off_in),
            &dst,
            Some(&mut off_out),
            1 << 20,
        ) {
            Ok(0) => return Ok(true),
            Ok(n) => cancel.note_written(n as u64),
            Err(Errno::NOSYS | Errno::XDEV | Errno::INVAL | Errno::OPNOTSUPP | Errno::PERM) => {
                return Ok(false);
            }
            Err(err) => return Err(Error::from_errno("copy_file_range", err)),
        }
    }
}

/// `ENOTSUP` means this filesystem has no xattrs. Every other error is returned.
fn xattr_errno(err: Errno) -> Result<()> {
    if err == Errno::NOTSUP {
        Ok(())
    } else {
        Err(Error::from_errno("xattr", err))
    }
}

fn copy_xattrs(src: impl AsFd, dst: impl AsFd) -> Result<()> {
    let len = match rustix::fs::flistxattr_query(&src) {
        Ok(0) => return Ok(()),
        Ok(n) => n,
        Err(err) => return xattr_errno(err),
    };
    let mut buf = vec![0u8; len];
    if let Err(err) = rustix::fs::flistxattr(&src, buf.as_mut_slice()) {
        return xattr_errno(err);
    }
    for name in buf.split(|b| *b == 0) {
        if name.is_empty() {
            continue;
        }
        let size = match rustix::fs::fgetxattr_query(&src, name) {
            Ok(n) => n,
            Err(err) => return xattr_errno(err),
        };
        let mut value = vec![0u8; size];
        if size > 0 {
            if let Err(err) = rustix::fs::fgetxattr(&src, name, value.as_mut_slice()) {
                return xattr_errno(err);
            }
        }
        if let Err(err) = rustix::fs::fsetxattr(&dst, name, &value, rustix::fs::XattrFlags::empty())
        {
            return xattr_errno(err);
        }
    }
    Ok(())
}

fn set_times(file: impl AsFd, stat: &rustix::fs::Stat) -> std::result::Result<(), Errno> {
    let times = rustix::fs::Timestamps {
        last_access: rustix::fs::Timespec {
            tv_sec: stat.st_atime,
            tv_nsec: stat.st_atime_nsec as _,
        },
        last_modification: rustix::fs::Timespec {
            tv_sec: stat.st_mtime,
            tv_nsec: stat.st_mtime_nsec as _,
        },
    };
    rustix::fs::futimens(file, &times)
}

fn partial_name() -> String {
    static SEQ: AtomicU64 = AtomicU64::new(1);
    let n = SEQ.fetch_add(1, Ordering::Relaxed);
    let jitter = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.subsec_nanos())
        .unwrap_or(0);
    format!(".robin-partial-{}-{n}-{jitter:08x}", std::process::id())
}

fn component(name: &std::ffi::OsStr) -> Result<std::ffi::CString> {
    let bytes = name.as_bytes();
    if bytes.is_empty() || bytes.contains(&b'/') || bytes == b"." || bytes == b".." {
        return Err(Error::InvalidName);
    }
    std::ffi::CString::new(bytes).map_err(|_| Error::InvalidName)
}

fn is_dir(mode: u32) -> bool {
    mode & 0o170000 == 0o040000
}
fn is_lnk(mode: u32) -> bool {
    mode & 0o170000 == 0o120000
}

fn percent_encode(bytes: &[u8]) -> String {
    let mut out = String::new();
    for byte in bytes {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(*byte as char)
            }
            _ => out.push_str(&format!("%{byte:02X}")),
        }
    }
    out
}

fn deletion_date() -> String {
    let secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or(Duration::from_secs(0))
        .as_secs() as i64;
    format_deletion_date(secs, local_offset(secs))
}

/// A modification time in local time for the conflict dialog:
/// "2026-10-30 18:58".
pub fn short_date(secs: i64) -> String {
    let full = format_deletion_date(secs, local_offset(secs));
    full[..16].replace('T', " ")
}

/// Seconds east of UTC for the local zone at `secs`.
fn local_offset(secs: i64) -> i64 {
    let t = secs as libc::time_t;
    let mut tm = unsafe { std::mem::zeroed::<libc::tm>() };
    let ok = unsafe { !libc::localtime_r(&t, &mut tm).is_null() };
    if ok {
        tm.tm_gmtoff as i64
    } else {
        0
    }
}

/// `DeletionDate` is local time without a zone, so the offset is applied
/// here. Pure, so tests do not touch `TZ`.
fn format_deletion_date(secs: i64, offset: i64) -> String {
    let t = secs + offset;
    let days = t.div_euclid(86_400);
    let rem = t.rem_euclid(86_400);
    // Civil date from days since 1970-01-01 (Howard Hinnant's algorithm).
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + i64::from(month <= 2);
    format!(
        "{year:04}-{month:02}-{day:02}T{:02}:{:02}:{:02}",
        rem / 3600,
        rem % 3600 / 60,
        rem % 60
    )
}

/// Refuse to operate if `path`'s directory is a pseudo filesystem.
pub fn guard_root(path: &Path) -> Result<DirFd> {
    DirFd::open(path)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;
    use std::sync::atomic::AtomicUsize;

    fn scratch(label: &str) -> std::path::PathBuf {
        let path = std::env::temp_dir().join(format!(
            "robin-fs-{label}-{}",
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&path).unwrap();
        path
    }

    fn replace_ask(prompt: &ConflictPrompt) -> Result<ConflictChoice> {
        let _ = prompt;
        Ok(ConflictChoice {
            action: Conflict::Replace,
            apply_all: false,
        })
    }

    fn opts<'a>(
        cancel: &'a Cancel,
        policy: &'a mut ConflictPolicy,
        ask: &'a mut dyn FnMut(&ConflictPrompt) -> Result<ConflictChoice>,
    ) -> CopyOpts<'a> {
        CopyOpts {
            cancel,
            policy,
            ask,
        }
    }

    #[test]
    fn symlink_swap_after_stat_leaves_outside_the_tree() {
        let root = scratch("swap");
        let outside = scratch("outside");
        std::fs::write(outside.join("keep"), b"safe").unwrap();
        let box_dir = root.join("box");
        std::fs::create_dir(&box_dir).unwrap();
        std::fs::write(box_dir.join("victim"), b"x").unwrap();
        std::fs::write(root.join("precious"), b"secret").unwrap();
        let parent = DirFd::open(&root).unwrap();
        let swapped = AtomicBool::new(false);
        remove_child(
            &parent,
            std::ffi::OsStr::new("box"),
            &mut |name| {
                if name == "victim" && !swapped.swap(true, Ordering::Relaxed) {
                    let victim = box_dir.join("victim");
                    std::fs::remove_file(&victim).unwrap();
                    std::os::unix::fs::symlink(&outside, &victim).unwrap();
                }
            },
            &Cancel::new(),
        )
        .unwrap();
        assert_eq!(std::fs::read(root.join("precious")).unwrap(), b"secret");
        assert_eq!(std::fs::read(outside.join("keep")).unwrap(), b"safe");
        assert!(outside.is_dir());
        assert!(!box_dir.exists());
        let _ = std::fs::remove_dir_all(&root);
        let _ = std::fs::remove_dir_all(&outside);
    }

    #[test]
    fn deleting_a_symlink_leaves_the_target() {
        let root = scratch("link");
        std::fs::write(root.join("target"), b"keep").unwrap();
        std::os::unix::fs::symlink("target", root.join("link")).unwrap();
        let parent = DirFd::open(&root).unwrap();
        remove_child(
            &parent,
            std::ffi::OsStr::new("link"),
            &mut |_| {},
            &Cancel::new(),
        )
        .unwrap();
        assert_eq!(std::fs::read(root.join("target")).unwrap(), b"keep");
        assert!(!root.join("link").exists());
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn proc_is_not_a_place_we_can_delete_from() {
        let err = DirFd::open(Path::new("/proc")).unwrap_err();
        assert!(matches!(err, Error::Pseudo { .. }), "{err}");
    }

    #[test]
    fn dev_shm_opens_and_dev_is_refused() {
        let fixture = "\
203 201 0:6 / /dev rw - devtmpfs devtmpfs rw\n\
204 203 0:23 / /dev/shm rw - tmpfs tmpfs rw\n";
        assert_eq!(
            crate::safety::fstype_in_mountinfo(fixture, 0, 6),
            Some("devtmpfs")
        );
        assert_eq!(
            crate::safety::fstype_in_mountinfo(fixture, 0, 23),
            Some("tmpfs")
        );

        DirFd::open(Path::new("/dev/shm")).expect("/dev/shm is tmpfs and must open");
        let text = std::fs::read_to_string("/proc/self/mountinfo").unwrap();
        let dev = rustix::fs::stat("/dev").unwrap();
        let (major, minor) = crate::safety::dev_parts(dev.st_dev);
        match crate::safety::fstype_in_mountinfo(&text, major, minor) {
            Some("devtmpfs") => match DirFd::open(Path::new("/dev")) {
                Err(Error::Pseudo { kind: "devtmpfs" }) => {}
                Ok(_) => panic!("/dev opened; devtmpfs must be refused"),
                Err(err) => panic!("/dev failed for the wrong reason: {err}"),
            },
            _ => {
                DirFd::open(Path::new("/dev")).expect("a /dev that is not devtmpfs must open");
            }
        }
    }

    #[test]
    fn copy_preserves_bytes_mode_and_mtime_and_cancel_removes_the_partial() {
        let root = scratch("copy");
        let src = root.join("src");
        let dst = root.join("dst");
        std::fs::create_dir(&src).unwrap();
        std::fs::create_dir(&dst).unwrap();
        let file = src.join("note.txt");
        std::fs::write(&file, b"hello robin").unwrap();
        std::fs::set_permissions(&file, std::fs::Permissions::from_mode(0o640)).unwrap();
        let opened = File::open(&file).unwrap();
        let stamp = rustix::fs::Timespec {
            tv_sec: 1_700_000_000,
            tv_nsec: 0,
        };
        rustix::fs::futimens(
            &opened,
            &rustix::fs::Timestamps {
                last_access: stamp,
                last_modification: stamp,
            },
        )
        .unwrap();
        drop(opened);
        let parent = DirFd::open(&src).unwrap();
        let dest = DirFd::open(&dst).unwrap();
        let cancel = Cancel::new();
        let mut policy = ConflictPolicy::new();
        let mut ask = replace_ask;
        copy_file(
            &parent,
            std::ffi::OsStr::new("note.txt"),
            &dest,
            std::ffi::OsStr::new("note.txt"),
            &mut opts(&cancel, &mut policy, &mut ask),
        )
        .unwrap();
        assert_eq!(std::fs::read(dst.join("note.txt")).unwrap(), b"hello robin");
        let meta = std::fs::metadata(dst.join("note.txt")).unwrap();
        assert_eq!(meta.permissions().mode() & 0o777, 0o640);
        let copied = rustix::fs::stat(dst.join("note.txt")).unwrap();
        assert_eq!(copied.st_mtime, 1_700_000_000);
        assert!(dst.read_dir().unwrap().all(|e| !e
            .unwrap()
            .file_name()
            .to_string_lossy()
            .starts_with(".robin-partial")));

        let cancel = Cancel::new();
        cancel.stop_after_bytes(1);
        let mut policy = ConflictPolicy::new();
        let err = copy_file(
            &parent,
            std::ffi::OsStr::new("note.txt"),
            &dest,
            std::ffi::OsStr::new("other.txt"),
            &mut opts(&cancel, &mut policy, &mut ask),
        )
        .unwrap_err();
        assert!(matches!(err, Error::Cancelled));
        assert!(!dst.join("other.txt").exists());
        assert!(dst.read_dir().unwrap().all(|e| !e
            .unwrap()
            .file_name()
            .to_string_lossy()
            .starts_with(".robin-partial")));
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn conflict_choices_do_not_silently_overwrite() {
        let root = scratch("conflict");
        std::fs::write(root.join("src"), b"new").unwrap();
        std::fs::write(root.join("dest"), b"old").unwrap();
        let dir = DirFd::open(&root).unwrap();
        let cancel = Cancel::new();
        let mut policy = ConflictPolicy::new();
        let mut skip = |_: &ConflictPrompt| {
            Ok(ConflictChoice {
                action: Conflict::Skip,
                apply_all: false,
            })
        };
        copy_file(
            &dir,
            std::ffi::OsStr::new("src"),
            &dir,
            std::ffi::OsStr::new("dest"),
            &mut opts(&cancel, &mut policy, &mut skip),
        )
        .unwrap();
        assert_eq!(std::fs::read(root.join("dest")).unwrap(), b"old");

        let mut policy = ConflictPolicy::new();
        let mut rename = |_: &ConflictPrompt| choice_from_buttons(false, true, false, false);
        copy_file(
            &dir,
            std::ffi::OsStr::new("src"),
            &dir,
            std::ffi::OsStr::new("dest"),
            &mut opts(&cancel, &mut policy, &mut rename),
        )
        .unwrap();
        assert_eq!(std::fs::read(root.join("dest")).unwrap(), b"old");
        assert_eq!(std::fs::read(root.join("dest (2)")).unwrap(), b"new");

        let mut policy = ConflictPolicy::new();
        let asks = AtomicUsize::new(0);
        let mut replace_all = |_: &ConflictPrompt| {
            asks.fetch_add(1, Ordering::Relaxed);
            choice_from_buttons(false, false, true, true)
        };
        copy_file(
            &dir,
            std::ffi::OsStr::new("src"),
            &dir,
            std::ffi::OsStr::new("dest"),
            &mut opts(&cancel, &mut policy, &mut replace_all),
        )
        .unwrap();
        std::fs::write(root.join("src2"), b"again").unwrap();
        copy_file(
            &dir,
            std::ffi::OsStr::new("src2"),
            &dir,
            std::ffi::OsStr::new("dest"),
            &mut opts(&cancel, &mut policy, &mut replace_all),
        )
        .unwrap();
        assert_eq!(asks.load(Ordering::Relaxed), 1);
        assert_eq!(std::fs::read(root.join("dest")).unwrap(), b"again");
        let blocked = rename_child(
            &dir,
            std::ffi::OsStr::new("src"),
            std::ffi::OsStr::new("dest"),
        )
        .unwrap_err();
        assert!(blocked.to_string().contains("dest"), "{blocked}");
        let _ = std::fs::remove_dir_all(&root);
    }

    fn rename_ask(prompt: &ConflictPrompt) -> Result<ConflictChoice> {
        let _ = prompt;
        Ok(ConflictChoice {
            action: Conflict::Rename,
            apply_all: false,
        })
    }

    /// Before each landing, plant `marker` under the chosen name the first
    /// `times` times, like another program racing us.
    fn race_landing(times: usize) {
        let mut left = times;
        BEFORE_LANDING.with(|h| {
            *h.borrow_mut() = Some(Box::new(move |dir: &DirFd, name: &std::ffi::OsStr| {
                if left > 0 {
                    left -= 1;
                    let path = dir_path(dir).join(name);
                    std::fs::write(path, b"racer").unwrap();
                }
            }));
        });
    }

    fn dir_path(dir: &DirFd) -> std::path::PathBuf {
        std::fs::read_link(format!(
            "/proc/self/fd/{}",
            std::os::fd::AsRawFd::as_raw_fd(&dir.as_fd())
        ))
        .unwrap()
    }

    fn racy_move(
        root: &Path,
        from: &str,
        ask: fn(&ConflictPrompt) -> Result<ConflictChoice>,
    ) -> Result<CopyOutcome> {
        let dir = DirFd::open(root).unwrap();
        let cancel = Cancel::new();
        let mut policy = ConflictPolicy::new();
        let mut ask = ask;
        let out = move_child(
            &dir,
            std::ffi::OsStr::new(from),
            &dir,
            std::ffi::OsStr::new("dest"),
            &mut MoveOpts {
                force_copy: true,
                fail_before_unlink: false,
                copy: opts(&cancel, &mut policy, &mut ask),
                report: TransferReport::default(),
                check_removable: true,
                keep_source: false,
            },
        );
        BEFORE_LANDING.with(|h| *h.borrow_mut() = None);
        out
    }

    fn racers(root: &Path) -> usize {
        std::fs::read_dir(root)
            .unwrap()
            .filter(|e| std::fs::read(e.as_ref().unwrap().path()).ok().as_deref() == Some(b"racer"))
            .count()
    }

    #[test]
    fn keep_both_never_overwrites_a_name_taken_before_landing() {
        // Plain file, fresh destination name.
        let root = scratch("race-file");
        std::fs::write(root.join("src"), b"mine").unwrap();
        race_landing(1);
        racy_move(&root, "src", rename_ask).unwrap();
        assert_eq!(racers(&root), 1, "the racer's file survives");
        assert_eq!(std::fs::read(root.join("dest (2)")).unwrap(), b"mine");
        let _ = std::fs::remove_dir_all(&root);

        // Keep-both chosen on a real conflict, and the new name is taken too.
        let root = scratch("race-keep");
        std::fs::write(root.join("src"), b"mine").unwrap();
        std::fs::write(root.join("dest"), b"old").unwrap();
        race_landing(2);
        racy_move(&root, "src", rename_ask).unwrap();
        assert_eq!(std::fs::read(root.join("dest")).unwrap(), b"old");
        assert_eq!(racers(&root), 2);
        assert!(std::fs::read_dir(&root)
            .unwrap()
            .any(|e| std::fs::read(e.unwrap().path()).unwrap() == b"mine"));
        let _ = std::fs::remove_dir_all(&root);

        // Symlink move.
        let root = scratch("race-link");
        std::os::unix::fs::symlink("target", root.join("src")).unwrap();
        race_landing(1);
        racy_move(&root, "src", rename_ask).unwrap();
        assert_eq!(std::fs::read(root.join("dest")).unwrap(), b"racer");
        assert_eq!(
            std::fs::read_link(root.join("dest (2)")).unwrap(),
            Path::new("target")
        );
        let _ = std::fs::remove_dir_all(&root);

        // Folder: never merges into one that appeared meanwhile.
        let root = scratch("race-dir");
        std::fs::create_dir(root.join("src")).unwrap();
        std::fs::write(root.join("src/f"), b"mine").unwrap();
        race_landing(1);
        racy_move(&root, "src", rename_ask).unwrap();
        assert_eq!(std::fs::read(root.join("dest")).unwrap(), b"racer");
        assert_eq!(std::fs::read(root.join("dest (2)/f")).unwrap(), b"mine");
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn keep_both_gives_up_without_overwriting() {
        let root = scratch("race-giveup");
        std::fs::write(root.join("src"), b"mine").unwrap();
        race_landing(usize::MAX);
        let err = racy_move(&root, "src", rename_ask).unwrap_err();
        assert!(err.to_string().contains("nothing was replaced"), "{err}");
        assert_eq!(std::fs::read(root.join("src")).unwrap(), b"mine");
        assert_eq!(racers(&root), KEEP_BOTH_TRIES);
        let leftovers: Vec<_> = std::fs::read_dir(&root)
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .filter(|n| n.starts_with(".robin-partial-"))
            .collect();
        assert!(
            leftovers.is_empty(),
            "temp files left behind: {leftovers:?}"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn same_filesystem_move_is_a_rename() {
        let root = scratch("move");
        std::fs::write(root.join("a"), b"z").unwrap();
        let dir = DirFd::open(&root).unwrap();
        let cancel = Cancel::new();
        let mut policy = ConflictPolicy::new();
        let mut ask = replace_ask;
        move_child(
            &dir,
            std::ffi::OsStr::new("a"),
            &dir,
            std::ffi::OsStr::new("b"),
            &mut MoveOpts {
                force_copy: false,
                fail_before_unlink: false,
                copy: opts(&cancel, &mut policy, &mut ask),
                report: TransferReport::default(),
                check_removable: true,
                keep_source: false,
            },
        )
        .unwrap();
        assert!(!root.join("a").exists());
        assert_eq!(std::fs::read(root.join("b")).unwrap(), b"z");
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn injected_exdev_failure_leaves_the_source() {
        let root = scratch("exdev");
        std::fs::write(root.join("a"), b"z").unwrap();
        let dir = DirFd::open(&root).unwrap();
        let cancel = Cancel::new();
        let mut policy = ConflictPolicy::new();
        let mut ask = replace_ask;
        let err = move_child(
            &dir,
            std::ffi::OsStr::new("a"),
            &dir,
            std::ffi::OsStr::new("b"),
            &mut MoveOpts {
                force_copy: true,
                fail_before_unlink: true,
                copy: opts(&cancel, &mut policy, &mut ask),
                report: TransferReport::default(),
                check_removable: true,
                keep_source: false,
            },
        )
        .unwrap_err();
        assert!(err.to_string().contains("injected"));
        assert_eq!(std::fs::read(root.join("a")).unwrap(), b"z");
        assert_eq!(std::fs::read(root.join("b")).unwrap(), b"z");
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn skip_on_exdev_move_leaves_the_source() {
        let root = scratch("skip-exdev");
        std::fs::write(root.join("src"), b"keep").unwrap();
        std::fs::write(root.join("dest"), b"old").unwrap();
        let dir = DirFd::open(&root).unwrap();
        let cancel = Cancel::new();
        let mut policy = ConflictPolicy::new();
        let mut skip = |_: &ConflictPrompt| {
            Ok(ConflictChoice {
                action: Conflict::Skip,
                apply_all: false,
            })
        };
        let outcome = move_child(
            &dir,
            std::ffi::OsStr::new("src"),
            &dir,
            std::ffi::OsStr::new("dest"),
            &mut MoveOpts {
                force_copy: true,
                fail_before_unlink: false,
                copy: opts(&cancel, &mut policy, &mut skip),
                report: TransferReport::default(),
                check_removable: true,
                keep_source: false,
            },
        )
        .unwrap();
        assert_eq!(outcome, CopyOutcome::Skipped);
        assert_eq!(std::fs::read(root.join("src")).unwrap(), b"keep");
        assert_eq!(std::fs::read(root.join("dest")).unwrap(), b"old");
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn directory_move_deletes_the_source_only_when_every_child_is_copied() {
        let root = scratch("dirmove");
        let src = root.join("src");
        let dst = root.join("dst");
        std::fs::create_dir_all(src.join("folder")).unwrap();
        std::fs::create_dir_all(dst.join("folder")).unwrap();
        std::fs::write(src.join("folder").join("keep.txt"), b"stay").unwrap();
        std::fs::write(src.join("folder").join("go.txt"), b"move").unwrap();
        std::fs::write(dst.join("folder").join("keep.txt"), b"old").unwrap();
        let parent = DirFd::open(&src).unwrap();
        let dest = DirFd::open(&dst).unwrap();
        let cancel = Cancel::new();
        let mut policy = ConflictPolicy::new();
        let mut ask = |prompt: &ConflictPrompt| {
            let action = if prompt.name == "keep.txt" {
                Conflict::Skip
            } else {
                Conflict::Replace
            };
            Ok(ConflictChoice {
                action,
                apply_all: false,
            })
        };
        let outcome = move_child(
            &parent,
            std::ffi::OsStr::new("folder"),
            &dest,
            std::ffi::OsStr::new("folder"),
            &mut MoveOpts {
                force_copy: true,
                fail_before_unlink: false,
                copy: opts(&cancel, &mut policy, &mut ask),
                report: TransferReport::default(),
                check_removable: true,
                keep_source: false,
            },
        )
        .unwrap();
        assert_eq!(outcome, CopyOutcome::Skipped);
        assert_eq!(
            std::fs::read(src.join("folder").join("keep.txt")).unwrap(),
            b"stay"
        );
        assert!(!src.join("folder").join("go.txt").exists());
        assert!(src.join("folder").is_dir());
        assert_eq!(
            std::fs::read(dst.join("folder").join("keep.txt")).unwrap(),
            b"old"
        );
        assert_eq!(
            std::fs::read(dst.join("folder").join("go.txt")).unwrap(),
            b"move"
        );

        std::fs::create_dir(src.join("whole")).unwrap();
        std::fs::write(src.join("whole").join("a.txt"), b"a").unwrap();
        let mut policy = ConflictPolicy::new();
        let mut ask = replace_ask;
        let outcome = move_child(
            &parent,
            std::ffi::OsStr::new("whole"),
            &dest,
            std::ffi::OsStr::new("whole"),
            &mut MoveOpts {
                force_copy: true,
                fail_before_unlink: false,
                copy: opts(&cancel, &mut policy, &mut ask),
                report: TransferReport::default(),
                check_removable: true,
                keep_source: false,
            },
        )
        .unwrap();
        assert_eq!(outcome, CopyOutcome::Copied);
        assert!(!src.join("whole").exists());
        assert_eq!(
            std::fs::read(dst.join("whole").join("a.txt")).unwrap(),
            b"a"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn same_fs_move_asks_about_a_conflict_once() {
        let root = scratch("once");
        std::fs::write(root.join("a"), b"new").unwrap();
        std::fs::write(root.join("b"), b"old").unwrap();
        std::fs::write(root.join("c"), b"c").unwrap();
        let dir = DirFd::open(&root).unwrap();
        let cancel = Cancel::new();
        let mut policy = ConflictPolicy::new();
        let asks = AtomicUsize::new(0);
        let mut ask = |_: &ConflictPrompt| {
            asks.fetch_add(1, Ordering::Relaxed);
            Ok(ConflictChoice {
                action: Conflict::Replace,
                apply_all: false,
            })
        };
        move_child(
            &dir,
            std::ffi::OsStr::new("c"),
            &dir,
            std::ffi::OsStr::new("d"),
            &mut MoveOpts {
                force_copy: false,
                fail_before_unlink: false,
                copy: opts(&cancel, &mut policy, &mut ask),
                report: TransferReport::default(),
                check_removable: true,
                keep_source: false,
            },
        )
        .unwrap();
        assert_eq!(asks.load(Ordering::Relaxed), 0);
        move_child(
            &dir,
            std::ffi::OsStr::new("a"),
            &dir,
            std::ffi::OsStr::new("b"),
            &mut MoveOpts {
                force_copy: false,
                fail_before_unlink: false,
                copy: opts(&cancel, &mut policy, &mut ask),
                report: TransferReport::default(),
                check_removable: true,
                keep_source: false,
            },
        )
        .unwrap();
        assert_eq!(asks.load(Ordering::Relaxed), 1);
        assert_eq!(std::fs::read(root.join("b")).unwrap(), b"new");
        assert!(!root.join("a").exists());
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn external_drive_without_a_trash_does_not_fall_home() {
        assert_eq!(choose_trash(true, false), TrashChoice::Home);
        assert_eq!(choose_trash(false, true), TrashChoice::Volume);
        assert_eq!(choose_trash(false, false), TrashChoice::Ask);
        let root = scratch("trash");
        let files = root.join("files");
        let trash = root.join("trash");
        std::fs::create_dir(&files).unwrap();
        std::fs::create_dir(&trash).unwrap();
        std::fs::write(files.join("song.flac"), b"music").unwrap();
        let src = DirFd::open(&files).unwrap();
        let bin = DirFd::open(&trash).unwrap();
        let err = place_in_trash(
            &src,
            std::ffi::OsStr::new("song.flac"),
            &bin,
            TrashChoice::Ask,
            "files/song.flac",
        )
        .unwrap_err();
        assert!(err.to_string().contains("not copied home"));
        assert!(files.join("song.flac").is_file());
        place_in_trash(
            &src,
            std::ffi::OsStr::new("song.flac"),
            &bin,
            TrashChoice::Volume,
            "files/song.flac",
        )
        .unwrap();
        assert!(!files.join("song.flac").exists());
        assert!(trash.join("files").join("song.flac").is_file());
        let info = std::fs::read_to_string(trash.join("info").join("song.flac.trashinfo")).unwrap();
        assert!(info.contains("Path=files%2Fsong.flac"), "{info}");
        assert!(!info.contains("Path=%2F"), "{info}");
        let date = info
            .lines()
            .find_map(|line| line.strip_prefix("DeletionDate="))
            .unwrap();
        assert_eq!(date.len(), 19, "{date}");
        assert_eq!(&date[4..5], "-");
        assert_eq!(&date[10..11], "T");
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn home_trash_path_is_absolute_and_sticky_trash_is_preferred() {
        let root = scratch("home-trash");
        let files = root.join("files");
        let trash = root.join("trash");
        std::fs::create_dir(&files).unwrap();
        std::fs::create_dir(&trash).unwrap();
        std::fs::write(files.join("song.flac"), b"music").unwrap();
        let src = DirFd::open(&files).unwrap();
        let bin = DirFd::open(&trash).unwrap();
        let absolute = files.join("song.flac");
        place_in_trash(
            &src,
            std::ffi::OsStr::new("song.flac"),
            &bin,
            TrashChoice::Home,
            absolute.to_str().unwrap(),
        )
        .unwrap();
        let info = std::fs::read_to_string(trash.join("info").join("song.flac.trashinfo")).unwrap();
        assert!(info.contains("Path=%2F"), "{info}");

        let sticky = scratch("sticky");
        let dot = sticky.join(".Trash");
        std::fs::create_dir(&dot).unwrap();
        let mut perms = std::fs::metadata(&dot).unwrap().permissions();
        perms.set_mode(0o1777);
        std::fs::set_permissions(&dot, perms).unwrap();
        let top = DirFd::open(&sticky).unwrap();
        assert!(matches!(
            volume_trash_plan(&top).unwrap(),
            VolumeTrash::Sticky { .. }
        ));

        let plain = scratch("plain");
        let top = DirFd::open(&plain).unwrap();
        assert!(matches!(
            volume_trash_plan(&top).unwrap(),
            VolumeTrash::Private { .. }
        ));

        std::os::unix::fs::symlink("/tmp", plain.join(".Trash")).unwrap();
        assert!(matches!(
            volume_trash_plan(&top).unwrap(),
            VolumeTrash::Private { .. }
        ));
        let _ = std::fs::remove_dir_all(&root);
        let _ = std::fs::remove_dir_all(&sticky);
        let _ = std::fs::remove_dir_all(&plain);
    }

    #[test]
    fn deletion_date_is_local_not_utc() {
        assert_eq!(
            format_deletion_date(1_700_000_000, -5 * 3600),
            "2023-11-14T17:13:20",
            "DeletionDate must be local time"
        );
        assert_eq!(
            format_deletion_date(1_700_000_000, 0),
            "2023-11-14T22:13:20"
        );
        assert_eq!(
            format_deletion_date(1_700_000_000, 5 * 3600 + 1800),
            "2023-11-15T03:43:20"
        );
        assert_eq!(format_deletion_date(951_782_400, 0), "2000-02-29T00:00:00");
        assert_eq!(format_deletion_date(0, -3600), "1969-12-31T23:00:00");
    }

    #[test]
    fn depth_limit_is_1024_and_checked_before_deletion() {
        let root = scratch("depth-ok");
        let mut deep = root.join("top");
        std::fs::create_dir(&deep).unwrap();
        for i in 0..40 {
            deep.push(format!("d{i}"));
            std::fs::create_dir(&deep).unwrap();
        }
        std::fs::write(deep.join("leaf"), b"x").unwrap();
        let parent = DirFd::open(&root).unwrap();
        remove_child(
            &parent,
            std::ffi::OsStr::new("top"),
            &mut |_| {},
            &Cancel::new(),
        )
        .unwrap();
        assert!(!root.join("top").exists());
        let _ = std::fs::remove_dir_all(&root);

        let root = std::env::temp_dir().join(format!("rd{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir(&root).unwrap();
        let top = root.join("t");
        std::fs::create_dir(&top).unwrap();
        std::fs::write(top.join("keep"), b"stay").unwrap();
        let mut deep = top.clone();
        for _ in 0..1024 {
            deep.push("a");
            std::fs::create_dir(&deep).unwrap();
        }
        let parent = DirFd::open(&root).unwrap();
        let err = remove_child(
            &parent,
            std::ffi::OsStr::new("t"),
            &mut |_| {},
            &Cancel::new(),
        )
        .unwrap_err();
        assert!(err.to_string().contains("too deep"), "{err}");
        assert_eq!(std::fs::read(top.join("keep")).unwrap(), b"stay");
        assert!(deep.is_dir());
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn swapping_a_directory_for_a_symlink_does_not_follow_it() {
        let root = scratch("swapdir");
        let outside = scratch("swapout");
        std::fs::write(outside.join("keep"), b"safe").unwrap();
        let box_dir = root.join("box");
        std::fs::create_dir(&box_dir).unwrap();
        std::fs::create_dir(box_dir.join("nested")).unwrap();
        let parent = DirFd::open(&root).unwrap();
        let err = remove_child(
            &parent,
            std::ffi::OsStr::new("box"),
            &mut |name| {
                if name == "nested" {
                    let nested = box_dir.join("nested");
                    std::fs::remove_dir(&nested).unwrap();
                    std::os::unix::fs::symlink(&outside, &nested).unwrap();
                }
            },
            &Cancel::new(),
        )
        .unwrap_err();
        assert!(
            io_is(&err, Errno::LOOP) || io_is(&err, Errno::NOTDIR),
            "{err}"
        );
        assert_eq!(std::fs::read(outside.join("keep")).unwrap(), b"safe");
        assert!(outside.is_dir());
        let _ = std::fs::remove_dir_all(&root);
        let _ = std::fs::remove_dir_all(&outside);
    }

    #[test]
    fn volume_trash_dirs_are_created_owned_and_private() {
        let uid = rustix::process::getuid().as_raw();
        let sticky = scratch("sticky-open");
        let dot = sticky.join(".Trash");
        std::fs::create_dir(&dot).unwrap();
        std::fs::set_permissions(&dot, std::fs::Permissions::from_mode(0o1777)).unwrap();
        let top = DirFd::open(&sticky).unwrap();
        open_volume_trash(&top).unwrap();
        let uid_dir = dot.join(uid.to_string());
        assert_eq!(mode_bits(&uid_dir), 0o700);
        assert_eq!(owner_uid(&uid_dir), uid);
        assert_eq!(mode_bits(&uid_dir.join("files")), 0o700);
        assert_eq!(mode_bits(&uid_dir.join("info")), 0o700);
        assert_eq!(owner_uid(&uid_dir.join("files")), uid);
        assert_eq!(owner_uid(&uid_dir.join("info")), uid);

        let plain = scratch("private-open");
        let outside = scratch("trash-outside");
        std::os::unix::fs::symlink(&outside, plain.join(".Trash")).unwrap();
        let top = DirFd::open(&plain).unwrap();
        open_volume_trash(&top).unwrap();
        let private = plain.join(format!(".Trash-{uid}"));
        assert_eq!(mode_bits(&private), 0o700);
        assert_eq!(owner_uid(&private), uid);
        assert!(private.join("files").is_dir());
        assert!(private.join("info").is_dir());
        assert!(!outside.join("files").exists());
        assert!(!outside.join(uid.to_string()).exists());

        let bad = scratch("bad-mode");
        let wrong = bad.join(format!(".Trash-{uid}"));
        std::fs::create_dir(&wrong).unwrap();
        std::fs::set_permissions(&wrong, std::fs::Permissions::from_mode(0o755)).unwrap();
        let top = DirFd::open(&bad).unwrap();
        let err = open_volume_trash(&top).unwrap_err();
        assert!(err.to_string().contains("mode"), "{err}");
        let _ = std::fs::remove_dir_all(&sticky);
        let _ = std::fs::remove_dir_all(&plain);
        let _ = std::fs::remove_dir_all(&outside);
        let _ = std::fs::remove_dir_all(&bad);
    }

    #[test]
    fn trashinfo_is_reserved_exclusively() {
        let root = scratch("reserve");
        let files = root.join("files");
        let trash = root.join("trash");
        std::fs::create_dir(&files).unwrap();
        std::fs::create_dir(&trash).unwrap();
        std::fs::write(files.join("song.flac"), b"music").unwrap();
        let info = trash.join("info");
        std::fs::create_dir(&info).unwrap();
        std::fs::set_permissions(&info, std::fs::Permissions::from_mode(0o700)).unwrap();
        std::fs::write(info.join("song.flac.trashinfo"), b"original").unwrap();
        let src = DirFd::open(&files).unwrap();
        let bin = DirFd::open(&trash).unwrap();
        place_in_trash(
            &src,
            std::ffi::OsStr::new("song.flac"),
            &bin,
            TrashChoice::Volume,
            "files/song.flac",
        )
        .unwrap();
        assert_eq!(
            std::fs::read(info.join("song.flac.trashinfo")).unwrap(),
            b"original"
        );
        assert!(trash.join("files").join("song.flac.1").is_file());
        assert!(info.join("song.flac.1.trashinfo").is_file());
        assert!(!trash.join("files").join("song.flac").exists());
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn copy_keeps_atime_separate_from_mtime() {
        let root = scratch("atime");
        std::fs::create_dir(root.join("src")).unwrap();
        std::fs::create_dir(root.join("dst")).unwrap();
        let file = root.join("src").join("note.txt");
        std::fs::write(&file, b"hello robin").unwrap();
        let opened = File::open(&file).unwrap();
        let atime = rustix::fs::Timespec {
            tv_sec: 1_600_000_000,
            tv_nsec: 0,
        };
        let mtime = rustix::fs::Timespec {
            tv_sec: 1_700_000_000,
            tv_nsec: 0,
        };
        rustix::fs::futimens(
            &opened,
            &rustix::fs::Timestamps {
                last_access: atime,
                last_modification: mtime,
            },
        )
        .unwrap();
        drop(opened);
        let parent = DirFd::open(&root.join("src")).unwrap();
        let dest = DirFd::open(&root.join("dst")).unwrap();
        let cancel = Cancel::new();
        let mut policy = ConflictPolicy::new();
        let mut ask = replace_ask;
        copy_file(
            &parent,
            std::ffi::OsStr::new("note.txt"),
            &dest,
            std::ffi::OsStr::new("note.txt"),
            &mut opts(&cancel, &mut policy, &mut ask),
        )
        .unwrap();
        let copied = rustix::fs::stat(root.join("dst").join("note.txt")).unwrap();
        assert_eq!(copied.st_atime, 1_600_000_000);
        assert_eq!(copied.st_mtime, 1_700_000_000);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn copy_preserves_user_xattr() {
        let root = scratch("xattr");
        std::fs::create_dir(root.join("src")).unwrap();
        std::fs::create_dir(root.join("dst")).unwrap();
        let file = root.join("src").join("note.txt");
        std::fs::write(&file, b"hello robin").unwrap();
        let opened = File::options().read(true).write(true).open(&file).unwrap();
        rustix::fs::fsetxattr(
            &opened,
            "user.robin",
            b"amber",
            rustix::fs::XattrFlags::empty(),
        )
        .unwrap();
        drop(opened);
        let parent = DirFd::open(&root.join("src")).unwrap();
        let dest = DirFd::open(&root.join("dst")).unwrap();
        let cancel = Cancel::new();
        let mut policy = ConflictPolicy::new();
        let mut ask = replace_ask;
        copy_file(
            &parent,
            std::ffi::OsStr::new("note.txt"),
            &dest,
            std::ffi::OsStr::new("note.txt"),
            &mut opts(&cancel, &mut policy, &mut ask),
        )
        .unwrap();
        let copied = File::open(root.join("dst").join("note.txt")).unwrap();
        let mut value = [0u8; 8];
        let n = rustix::fs::fgetxattr(&copied, "user.robin", &mut value).unwrap();
        assert_eq!(&value[..n], b"amber");
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn xattr_enotsup_is_tolerated_and_other_errors_propagate() {
        assert!(xattr_errno(Errno::NOTSUP).is_ok());
        let err = xattr_errno(Errno::PERM).unwrap_err();
        assert!(err.to_string().contains("xattr"), "{err}");
    }

    fn cross_device_move(root: &Path, from: &str, to: &str) -> (CopyOutcome, TransferReport) {
        let dir = DirFd::open(root).unwrap();
        let cancel = Cancel::new();
        let mut policy = ConflictPolicy::new();
        let mut ask = replace_ask;
        let mut opts = MoveOpts {
            force_copy: true,
            fail_before_unlink: false,
            copy: CopyOpts {
                cancel: &cancel,
                policy: &mut policy,
                ask: &mut ask,
            },
            report: TransferReport::default(),
            check_removable: true,
            keep_source: false,
        };
        let outcome = move_child(
            &dir,
            std::ffi::OsStr::new(from),
            &dir,
            std::ffi::OsStr::new(to),
            &mut opts,
        )
        .unwrap();
        (outcome, opts.report)
    }

    #[test]
    fn mount_points_are_skipped_not_descended() {
        let dir_mode = 0o040755;
        assert_eq!(classify(dir_mode, 7, 7), EntryKind::Dir);
        assert_eq!(
            classify(dir_mode, 9, 7),
            EntryKind::Skip(SkipReason::MountPoint),
            "a child on another device is a mount point"
        );
        assert_eq!(
            classify(0o100644, 9, 7),
            EntryKind::Skip(SkipReason::MountPoint)
        );
        assert_eq!(classify(0o120777, 7, 7), EntryKind::Symlink);
        assert_eq!(classify(0o100644, 7, 7), EntryKind::File);
        assert_eq!(classify(0o010644, 7, 7), EntryKind::Skip(SkipReason::Fifo));
        assert_eq!(
            classify(0o140755, 7, 7),
            EntryKind::Skip(SkipReason::Socket)
        );
        assert_eq!(
            classify(0o020644, 7, 7),
            EntryKind::Skip(SkipReason::Device)
        );

        // /proc is its own mount under /. Its device differs from the root's,
        // so a move rooted at / would skip it instead of copying it.
        let root = rustix::fs::stat("/").unwrap();
        let proc_ = rustix::fs::stat("/proc").unwrap();
        if proc_.st_dev != root.st_dev {
            assert_eq!(
                classify(proc_.st_mode, proc_.st_dev, root.st_dev),
                EntryKind::Skip(SkipReason::MountPoint)
            );
        }
    }

    #[test]
    fn cross_device_move_copies_symlinks_and_skips_fifos_and_sockets() {
        let root = scratch("special");
        let src = root.join("src");
        std::fs::create_dir(&src).unwrap();
        std::fs::write(src.join("a.txt"), b"a").unwrap();
        std::os::unix::fs::symlink("../elsewhere/target", src.join("link")).unwrap();
        let fifo = std::ffi::CString::new(src.join("pipe").as_os_str().as_bytes()).unwrap();
        assert_eq!(unsafe { libc::mkfifo(fifo.as_ptr(), 0o644) }, 0);
        let _sock = std::os::unix::net::UnixListener::bind(src.join("sock")).unwrap();

        let (outcome, report) = cross_device_move(&root, "src", "dest");
        assert_eq!(
            outcome,
            CopyOutcome::Skipped,
            "skipped children keep the source"
        );
        let dest = root.join("dest");
        assert_eq!(std::fs::read(dest.join("a.txt")).unwrap(), b"a");
        assert_eq!(
            std::fs::read_link(dest.join("link")).unwrap(),
            Path::new("../elsewhere/target"),
            "the link target is copied, not followed"
        );
        assert!(!dest.join("pipe").exists());
        assert!(!dest.join("sock").exists());
        // Copied entries leave the source; skipped ones stay put.
        assert!(!src.join("a.txt").exists());
        assert!(std::fs::symlink_metadata(src.join("link")).is_err());
        assert!(std::fs::symlink_metadata(src.join("pipe")).is_ok());
        assert!(std::fs::symlink_metadata(src.join("sock")).is_ok());
        assert_eq!(report.moved, 2);
        let mut skipped: Vec<_> = report
            .skipped
            .iter()
            .map(|s| (s.path.clone(), s.reason))
            .collect();
        skipped.sort_by(|a, b| a.0.cmp(&b.0));
        assert_eq!(
            skipped,
            vec![
                (Path::new("src/pipe").to_path_buf(), SkipReason::Fifo),
                (Path::new("src/sock").to_path_buf(), SkipReason::Socket),
            ]
        );
        assert_eq!(report.summary(), "Moved 2 items, skipped 2");
        assert_eq!(report.details().len(), 2);

        // A top-level symlink moves on its own too.
        std::os::unix::fs::symlink("a.txt", root.join("top-link")).unwrap();
        let (outcome, _) = cross_device_move(&root, "top-link", "moved-link");
        assert_eq!(outcome, CopyOutcome::Copied);
        assert_eq!(
            std::fs::read_link(root.join("moved-link")).unwrap(),
            Path::new("a.txt")
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    /// `src/inner` is 0500 with a file in it, and dated 2020-09-13.
    fn read_only_tree(label: &str) -> (std::path::PathBuf, SystemTime) {
        let root = scratch(label);
        let src = root.join("src");
        std::fs::create_dir_all(src.join("inner")).unwrap();
        std::fs::write(src.join("inner/f"), b"x").unwrap();
        std::fs::set_permissions(src.join("inner"), std::fs::Permissions::from_mode(0o500))
            .unwrap();
        let mtime = UNIX_EPOCH + Duration::from_secs(1_600_000_000);
        std::fs::File::open(src.join("inner"))
            .unwrap()
            .set_modified(mtime)
            .unwrap();
        (root, mtime)
    }

    fn cleanup_read_only(root: &Path) {
        for dir in ["src/inner", "dest/inner"] {
            let _ =
                std::fs::set_permissions(root.join(dir), std::fs::Permissions::from_mode(0o700));
        }
        let _ = std::fs::remove_dir_all(root);
    }

    fn move_src(root: &Path, check_removable: bool) -> (Result<CopyOutcome>, TransferReport) {
        let dir = DirFd::open(root).unwrap();
        let cancel = Cancel::new();
        let mut policy = ConflictPolicy::new();
        let mut ask = replace_ask;
        let mut opts = MoveOpts {
            force_copy: true,
            fail_before_unlink: false,
            copy: CopyOpts {
                cancel: &cancel,
                policy: &mut policy,
                ask: &mut ask,
            },
            report: TransferReport::default(),
            check_removable,
            keep_source: false,
        };
        let result = move_child(
            &dir,
            std::ffi::OsStr::new("src"),
            &dir,
            std::ffi::OsStr::new("dest"),
            &mut opts,
        );
        (result, opts.report)
    }

    fn running_as_root() -> bool {
        rustix::process::geteuid().is_root()
    }

    #[test]
    fn move_is_refused_up_front_when_originals_cannot_be_removed() {
        if running_as_root() {
            return;
        }
        let (root, _) = read_only_tree("preflight");
        let (result, report) = move_src(&root, true);
        match result {
            Err(Error::NotRemovable(blocked)) => {
                assert_eq!(
                    blocked,
                    vec![KeptEntry {
                        path: Path::new("src/inner/f").to_path_buf(),
                        reason: KeepReason::ReadOnlyFolder,
                    }]
                );
                assert_eq!(
                    refusal("src", &blocked),
                    Refusal {
                        title: "Can't move src: 1 folder inside is read-only".into(),
                        lines: vec![RefusalLine {
                            path: "src/inner".into(),
                            note: None,
                        }],
                        can_copy_instead: true,
                        dismiss_label: "Cancel",
                    }
                );
            }
            other => panic!("expected an up-front refusal, got {other:?}"),
        }
        assert!(!root.join("dest").exists(), "nothing is copied");
        assert_eq!(report, TransferReport::default());
        assert_eq!(std::fs::read(root.join("src/inner/f")).unwrap(), b"x");
        cleanup_read_only(&root);
    }

    #[test]
    fn removal_failing_after_the_copy_is_a_partial_move() {
        if running_as_root() {
            return;
        }
        // The preflight is off to stand in for a folder that turns read-only
        // between the check and the unlink.
        let (root, mtime) = read_only_tree("partial");
        let (result, report) = move_src(&root, false);
        assert_eq!(result.unwrap(), CopyOutcome::Skipped, "the source is kept");
        let inner = root.join("dest/inner");
        assert_eq!(std::fs::read(inner.join("f")).unwrap(), b"x");
        assert_eq!(mode_bits(&inner), 0o500, "mode is set after the children");
        assert_eq!(
            std::fs::metadata(&inner).unwrap().modified().unwrap(),
            mtime,
            "times are set after the children"
        );
        assert_eq!(std::fs::read(root.join("src/inner/f")).unwrap(), b"x");
        assert_eq!(report.status(), TransferStatus::Partial);
        assert_eq!(
            report.left_behind,
            vec![KeptEntry {
                path: Path::new("src/inner/f").to_path_buf(),
                reason: KeepReason::ReadOnlyFolder,
            }]
        );
        assert_eq!(
            report.summary(),
            "Copied 1 item, but couldn't remove the originals (read-only folder)"
        );
        assert_eq!(
            report.details(),
            vec!["src/inner/f — left behind: read-only folder".to_string()]
        );
        cleanup_read_only(&root);
    }

    #[test]
    fn sticky_folder_blocks_removal_of_someone_elses_file() {
        if running_as_root() {
            return;
        }
        // /tmp is sticky and owned by root. A file there owned by root (or
        // anyone else) cannot be removed by us. Use a file we don't own if
        // one exists; otherwise check our own file passes.
        let tmp = DirFd::open(Path::new("/tmp")).unwrap();
        let mut ours = rustix::fs::fstat(tmp.as_fd()).unwrap();
        ours.st_uid = rustix::process::geteuid().as_raw();
        let rel = Path::new("x");
        assert_eq!(removal_denied(&tmp, &ours, rel).unwrap(), None);
        let mut theirs = ours;
        theirs.st_uid = ours.st_uid.wrapping_add(4242);
        let dir = rustix::fs::fstat(tmp.as_fd()).unwrap();
        if dir.st_mode & 0o1000 != 0 && dir.st_uid != ours.st_uid {
            assert_eq!(
                removal_denied(&tmp, &theirs, rel).unwrap(),
                Some(KeepReason::StickyFolder)
            );
        }
    }

    #[test]
    fn refusal_title_counts_by_reason() {
        let k = |p: &str, reason| KeptEntry {
            path: p.into(),
            reason,
        };
        let one = |p: &str, reason| refusal("Projects", &[k(p, reason)]).title;
        assert_eq!(
            one("Projects/x/1", KeepReason::StickyFolder),
            "Can't move Projects: 1 item belongs to someone else"
        );
        assert_eq!(
            refusal(
                "Projects",
                &[
                    k("Projects/x/1", KeepReason::ReadOnlyFolder),
                    k("Projects/y/2", KeepReason::ReadOnlyFolder),
                ]
            )
            .title,
            "Can't move Projects: 2 folders inside are read-only"
        );
        let r = refusal(
            "a",
            &[
                k("a/x/1", KeepReason::ReadOnlyFolder),
                k("a/x/2", KeepReason::ReadOnlyFolder),
                k("a/y/3", KeepReason::ReadOnlyFolder),
                k("a/s/4", KeepReason::StickyFolder),
            ],
        );
        assert_eq!(r.title, "Can't move a: 4 items can't be removed afterwards");
        let paths: Vec<_> = r.lines.iter().map(|l| l.path.as_str()).collect();
        assert_eq!(paths, vec!["a/x", "a/y", "a/s/4"]);
        assert!(r.lines.iter().all(|l| l.note.is_none()));
        assert!(
            r.can_copy_instead,
            "read-only and sticky can still be copied"
        );
        assert_eq!(r.dismiss_label, "Cancel");
        assert_eq!(r.note_count(), 0);
    }

    #[test]
    fn copy_instead_copies_and_keeps_the_source() {
        let (root, mtime) = read_only_tree("copy-instead");
        assert_eq!(RefusalChoice::default(), RefusalChoice::CopyInstead);
        let dir = DirFd::open(&root).unwrap();
        let cancel = Cancel::new();
        let mut policy = ConflictPolicy::new();
        let mut ask = replace_ask;
        let result = resolve_refusal(
            RefusalChoice::default(),
            &dir,
            std::ffi::OsStr::new("src"),
            &dir,
            std::ffi::OsStr::new("dest"),
            CopyOpts {
                cancel: &cancel,
                policy: &mut policy,
                ask: &mut ask,
            },
        )
        .unwrap();
        let (outcome, report) = result.expect("copy instead runs a copy");
        assert_eq!(outcome, CopyOutcome::Copied);
        assert_eq!(report.status(), TransferStatus::Complete);
        assert!(report.left_behind.is_empty());
        let inner = root.join("dest/inner");
        assert_eq!(std::fs::read(inner.join("f")).unwrap(), b"x");
        assert_eq!(mode_bits(&inner), 0o500);
        assert_eq!(
            std::fs::metadata(&inner).unwrap().modified().unwrap(),
            mtime
        );
        assert_eq!(std::fs::read(root.join("src/inner/f")).unwrap(), b"x");
        cleanup_read_only(&root);
    }

    #[test]
    fn cancel_on_the_refusal_copies_nothing() {
        let (root, _) = read_only_tree("refusal-cancel");
        let dir = DirFd::open(&root).unwrap();
        let cancel = Cancel::new();
        let mut policy = ConflictPolicy::new();
        let mut ask = replace_ask;
        let result = resolve_refusal(
            RefusalChoice::Cancel,
            &dir,
            std::ffi::OsStr::new("src"),
            &dir,
            std::ffi::OsStr::new("dest"),
            CopyOpts {
                cancel: &cancel,
                policy: &mut policy,
                ask: &mut ask,
            },
        )
        .unwrap();
        assert!(result.is_none());
        assert!(!root.join("dest").exists());
        cleanup_read_only(&root);
    }

    #[test]
    fn unopenable_subfolder_is_a_refusal_not_an_io_error() {
        if running_as_root() {
            return;
        }
        for mode in [0o300, 0o000] {
            let root = scratch("unopenable");
            let src = root.join("src");
            std::fs::create_dir_all(src.join("locked")).unwrap();
            std::fs::write(src.join("locked/f"), b"x").unwrap();
            std::fs::set_permissions(src.join("locked"), std::fs::Permissions::from_mode(mode))
                .unwrap();
            let (result, _) = move_src(&root, true);
            std::fs::set_permissions(src.join("locked"), std::fs::Permissions::from_mode(0o700))
                .unwrap();
            match result {
                Err(Error::NotRemovable(blocked)) => {
                    assert_eq!(
                        blocked,
                        vec![KeptEntry {
                            path: "src/locked".into(),
                            reason: KeepReason::UnreadableFolder,
                        }],
                        "mode {mode:o}"
                    );
                    assert_eq!(
                        refusal("src", &blocked).title,
                        "Can't move src: 1 folder inside can't be opened"
                    );
                }
                other => panic!("mode {mode:o}: expected a refusal, got {other:?}"),
            }
            assert!(!root.join("dest").exists());
            let _ = std::fs::remove_dir_all(&root);
        }
    }

    #[test]
    fn sticky_eperm_after_copy_is_a_sticky_reason() {
        let err = Error::from_errno("x", Errno::PERM);
        assert_eq!(keep_reason(&err, true), Some(KeepReason::StickyFolder));
        assert_eq!(keep_reason(&err, false), Some(KeepReason::ReadOnlyFolder));
        let acc = Error::from_errno("x", Errno::ACCESS);
        assert_eq!(keep_reason(&acc, true), Some(KeepReason::ReadOnlyFolder));
        assert_eq!(keep_reason(&Error::from_errno("x", Errno::IO), false), None);
    }

    #[test]
    fn each_unreadable_folder_adds_a_note_row() {
        let k = |p: &str, reason| KeptEntry {
            path: p.into(),
            reason,
        };
        let r = refusal(
            "Projects",
            &[
                k("Projects/a", KeepReason::UnreadableFolder),
                k("Projects/b", KeepReason::UnreadableFolder),
                k("Projects/ro/f", KeepReason::ReadOnlyFolder),
            ],
        );
        assert_eq!(r.note_count(), 2);
        assert_eq!(r.dismiss_label, "Close");
    }

    #[test]
    fn unreadable_folder_offers_only_close() {
        let k = |p: &str, reason| KeptEntry {
            path: p.into(),
            reason,
        };
        let r = refusal(
            "Projects",
            &[
                k("Projects/ro/f", KeepReason::ReadOnlyFolder),
                k("Projects/locked", KeepReason::UnreadableFolder),
            ],
        );
        assert!(!r.can_copy_instead);
        assert_eq!(r.dismiss_label, "Close");
        assert_eq!(r.note_count(), 1);
        assert_eq!(
            r.lines,
            vec![
                RefusalLine {
                    path: "Projects/ro".into(),
                    note: None,
                },
                RefusalLine {
                    path: "Projects/locked".into(),
                    note: Some(UNREADABLE_NOTE.into()),
                },
            ]
        );
    }

    #[test]
    fn copy_instead_refuses_an_unreadable_folder() {
        if running_as_root() {
            return;
        }
        let root = scratch("copy-unreadable");
        let src = root.join("src");
        std::fs::create_dir_all(src.join("locked")).unwrap();
        std::fs::write(src.join("locked/f"), b"x").unwrap();
        std::fs::write(src.join("a"), b"a").unwrap();
        std::fs::set_permissions(src.join("locked"), std::fs::Permissions::from_mode(0o000))
            .unwrap();
        let dir = DirFd::open(&root).unwrap();
        let cancel = Cancel::new();
        let mut policy = ConflictPolicy::new();
        let mut ask = replace_ask;
        let result = copy_instead(
            &dir,
            std::ffi::OsStr::new("src"),
            &dir,
            std::ffi::OsStr::new("dest"),
            CopyOpts {
                cancel: &cancel,
                policy: &mut policy,
                ask: &mut ask,
            },
        );
        std::fs::set_permissions(src.join("locked"), std::fs::Permissions::from_mode(0o700))
            .unwrap();
        match result {
            Err(Error::Unreadable(blocked)) => assert_eq!(
                blocked,
                vec![KeptEntry {
                    path: "src/locked".into(),
                    reason: KeepReason::UnreadableFolder,
                }]
            ),
            other => panic!("expected an unreadable refusal, got {other:?}"),
        }
        assert!(!root.join("dest").exists(), "nothing is copied");
        let _ = std::fs::remove_dir_all(&root);
    }

    fn mode_bits(path: &Path) -> u32 {
        std::fs::metadata(path).unwrap().permissions().mode() & 0o777
    }

    fn owner_uid(path: &Path) -> u32 {
        rustix::fs::stat(path).unwrap().st_uid
    }

    #[test]
    fn buttons_map_onto_conflict() {
        assert_eq!(
            choice_from_buttons(true, false, false, true).unwrap(),
            ConflictChoice {
                action: Conflict::Skip,
                apply_all: true,
            }
        );
        assert!(choice_from_buttons(true, true, false, false).is_err());
    }
}
