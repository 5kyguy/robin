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

const MAX_DEPTH: u32 = 32;

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
/// `before` runs after the entry is stat'd, so a test can swap it.
pub fn remove_child(
    parent: &DirFd,
    name: &std::ffi::OsStr,
    before: &mut dyn FnMut(&std::ffi::OsStr),
    cancel: &Cancel,
) -> Result<()> {
    let root_dev = rustix::fs::fstat(parent.as_fd())
        .map_err(|e| Error::from_errno(name, e))?
        .st_dev;
    remove_tree(parent, name, before, cancel, root_dev, 0)
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
    if depth > MAX_DEPTH {
        return Err(Error::Message("directory is too deep".into()));
    }
    let c = component(name)?;
    rustix::fs::statat(parent.as_fd(), c.as_c_str(), AtFlags::SYMLINK_NOFOLLOW)
        .map_err(|e| Error::from_errno(name, e))?;
    before(name);
    let stat = rustix::fs::statat(parent.as_fd(), c.as_c_str(), AtFlags::SYMLINK_NOFOLLOW)
        .map_err(|e| Error::from_errno(name, e))?;
    if stat.st_dev != root_dev {
        return Err(Error::Message(
            "refusing to cross filesystems while deleting".into(),
        ));
    }
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
) -> Result<()> {
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
    let dest_name = resolve_dest_name(dest_parent, to, &stat, opts)?;
    let Some(dest_name) = dest_name else {
        return Ok(());
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
        set_mtime(&output, &stat).map_err(|e| Error::from_errno(&dest_name, e))?;
        output.sync_all().map_err(|e| Error::io(&dest_name, e))?;
        copy_xattrs(&input, &output);
        drop(output);
    } else {
        let output = File::from(dest);
        rustix::fs::fchmod(&output, Mode::from_raw_mode(stat.st_mode & 0o777))
            .map_err(|e| Error::from_errno(&dest_name, e))?;
        set_mtime(&output, &stat).map_err(|e| Error::from_errno(&dest_name, e))?;
        output.sync_all().map_err(|e| Error::io(&dest_name, e))?;
        copy_xattrs(&src, output.as_fd());
        drop(output);
    }
    let replace = exists(dest_parent, dest_name.as_os_str())?;
    if replace {
        rustix::fs::renameat(
            dest_parent.as_fd(),
            guard.name.as_c_str(),
            dest_parent.as_fd(),
            to_c.as_c_str(),
        )
        .map_err(|e| Error::from_errno(&dest_name, e))?;
    } else {
        rename_noreplace(dest_parent.as_fd(), &guard.name, dest_parent.as_fd(), &to_c)
            .map_err(|e| Error::from_errno(&dest_name, e))?;
    }
    guard.live = false;
    Ok(())
}

pub struct MoveOpts<'a> {
    pub force_copy: bool,
    pub fail_before_unlink: bool,
    pub copy: CopyOpts<'a>,
}

/// Move by rename when it works. `force_copy` exercises the cross-device path.
pub fn move_child(
    parent: &DirFd,
    from: &std::ffi::OsStr,
    dest_parent: &DirFd,
    to: &std::ffi::OsStr,
    opts: &mut MoveOpts<'_>,
) -> Result<()> {
    if !opts.force_copy {
        let from_c = component(from)?;
        let to_c = component(to)?;
        if exists(dest_parent, to)? {
            let _ = settle_conflict(parent, from, dest_parent, to, &mut opts.copy)?;
        }
        match rename_noreplace(parent.as_fd(), &from_c, dest_parent.as_fd(), &to_c) {
            Ok(()) => return Ok(()),
            Err(err) if err == Errno::XDEV => {}
            Err(err) if err == Errno::EXIST => {
                return replace_or_rename(parent, from, dest_parent, to, &mut opts.copy);
            }
            Err(err) if err == Errno::ISDIR => {
                return Err(Error::Message(
                    "refusing to move a directory across devices".into(),
                ));
            }
            Err(err) => return Err(Error::from_errno(to, err)),
        }
    }
    if is_directory(parent, from)? {
        return Err(Error::Message(
            "refusing to move a directory across devices".into(),
        ));
    }
    copy_file(parent, from, dest_parent, to, &mut opts.copy)?;
    rustix::fs::fsync(dest_parent.as_fd()).map_err(|e| Error::from_errno(to, e))?;
    if opts.fail_before_unlink {
        return Err(Error::Message(
            "injected failure before unlinking the source".into(),
        ));
    }
    remove_child(parent, from, &mut |_| {}, opts.copy.cancel)
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

pub fn place_in_trash(
    parent: &DirFd,
    name: &std::ffi::OsStr,
    trash: &DirFd,
    choice: TrashChoice,
    info_path: &str,
) -> Result<()> {
    if matches!(choice, TrashChoice::Ask) {
        return Err(Error::Message(
            "this drive has no trash and files are not copied home".into(),
        ));
    }
    let files = ensure_subdir(trash, "files")?;
    let info_dir = ensure_subdir(trash, "info")?;
    let unique = unique_trash_name(&files, name)?;
    let info_name = format!("{}.trashinfo", unique.to_string_lossy());
    let info = std::ffi::OsString::from(&info_name);
    let body = format!(
        "[Trash Info]\nPath={}\nDeletionDate={}\n",
        percent_encode(info_path.as_bytes()),
        deletion_date()
    );
    let info_c = component(info.as_os_str())?;
    let fd = rustix::fs::openat(
        info_dir.as_fd(),
        info_c.as_c_str(),
        OFlags::WRONLY | OFlags::CREATE | OFlags::EXCL | OFlags::NOFOLLOW | OFlags::CLOEXEC,
        Mode::from_raw_mode(0o600),
    )
    .map_err(|e| Error::from_errno(&info, e))?;
    {
        let mut file = File::from(fd);
        file.write_all(body.as_bytes())
            .map_err(|e| Error::io(&info, e))?;
        file.sync_all().map_err(|e| Error::io(&info, e))?;
    }
    let data_c = component(unique.as_os_str())?;
    let from_c = component(name)?;
    if let Err(err) = rename_noreplace(parent.as_fd(), &from_c, files.as_fd(), &data_c) {
        let _ = rustix::fs::unlinkat(info_dir.as_fd(), info_c.as_c_str(), AtFlags::empty());
        return Err(Error::from_errno(name, err));
    }
    rustix::fs::fsync(files.as_fd()).map_err(|e| Error::from_errno(name, e))?;
    Ok(())
}

fn ensure_subdir(parent: &DirFd, name: &str) -> Result<DirFd> {
    let os = std::ffi::OsStr::new(name);
    match parent.open_dir_nofollow(os) {
        Ok(dir) => Ok(dir),
        Err(_) => {
            let c = component(os)?;
            match rustix::fs::mkdirat(parent.as_fd(), c.as_c_str(), Mode::from_raw_mode(0o700)) {
                Ok(()) | Err(Errno::EXIST) => {}
                Err(err) => return Err(Error::from_errno(os, err)),
            }
            parent.open_dir_nofollow(os)
        }
    }
}

fn unique_trash_name(files: &DirFd, name: &std::ffi::OsStr) -> Result<std::ffi::OsString> {
    if !exists(files, name)? {
        return Ok(name.to_os_string());
    }
    for n in 1..1000 {
        let candidate = std::ffi::OsString::from(format!("{}.{n}", name.to_string_lossy()));
        if !exists(files, &candidate)? {
            return Ok(candidate);
        }
    }
    Err(Error::Message("trash name is exhausted".into()))
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

fn resolve_dest_name(
    dest_parent: &DirFd,
    to: &std::ffi::OsStr,
    incoming: &rustix::fs::Stat,
    opts: &mut CopyOpts<'_>,
) -> Result<Option<std::ffi::OsString>> {
    if !exists(dest_parent, to)? {
        return Ok(Some(to.to_os_string()));
    }
    let action = settle_conflict_stat(dest_parent, to, incoming, opts)?;
    match action {
        Conflict::Skip => Ok(None),
        Conflict::Replace => Ok(Some(to.to_os_string())),
        Conflict::Rename => fresh_name(dest_parent, to).map(Some),
    }
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
) -> Result<()> {
    match settle_conflict(parent, from, dest_parent, to, opts)? {
        Conflict::Skip => Ok(()),
        Conflict::Replace => {
            let from_c = component(from)?;
            let to_c = component(to)?;
            rustix::fs::renameat(
                parent.as_fd(),
                from_c.as_c_str(),
                dest_parent.as_fd(),
                to_c.as_c_str(),
            )
            .map_err(|e| Error::from_errno(to, e))
        }
        Conflict::Rename => {
            let name = fresh_name(dest_parent, to)?;
            let from_c = component(from)?;
            let to_c = component(name.as_os_str())?;
            rename_noreplace(parent.as_fd(), &from_c, dest_parent.as_fd(), &to_c)
                .map_err(|e| Error::from_errno(&name, e))
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

fn copy_xattrs(src: impl AsFd, dst: impl AsFd) {
    let len = match rustix::fs::flistxattr_query(&src) {
        Ok(0) | Err(_) => return,
        Ok(n) => n,
    };
    let mut buf = vec![0u8; len];
    if rustix::fs::flistxattr(&src, buf.as_mut_slice()).is_err() {
        return;
    }
    for name in buf.split(|b| *b == 0) {
        if name.is_empty() {
            continue;
        }
        let Ok(size) = rustix::fs::fgetxattr_query(&src, name) else {
            continue;
        };
        let mut value = vec![0u8; size];
        if rustix::fs::fgetxattr(&src, name, value.as_mut_slice()).is_err() {
            continue;
        }
        let _ = rustix::fs::fsetxattr(&dst, name, &value, rustix::fs::XattrFlags::empty());
    }
}

fn set_mtime(file: impl AsFd, stat: &rustix::fs::Stat) -> std::result::Result<(), Errno> {
    let mtime = rustix::fs::Timespec {
        tv_sec: stat.st_mtime,
        tv_nsec: stat.st_mtime_nsec as _,
    };
    let times = rustix::fs::Timestamps {
        last_access: mtime,
        last_modification: mtime,
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
    format_deletion_date(secs)
}

fn format_deletion_date(secs: i64) -> String {
    let days = secs.div_euclid(86_400);
    let tod = secs.rem_euclid(86_400) as u32;
    let (year, month, day) = civil_from_days(days);
    let hour = tod / 3600;
    let minute = (tod % 3600) / 60;
    let second = tod % 60;
    format!("{year:04}-{month:02}-{day:02}T{hour:02}:{minute:02}:{second:02}")
}

/// Howard Hinnant's civil_from_days.
fn civil_from_days(days: i64) -> (i32, u32, u32) {
    let z = days + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = (z - era * 146_097) as u64;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146_096) / 365;
    let y = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if m <= 2 { y + 1 } else { y };
    (y as i32, m as u32, d as u32)
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
    fn devtmpfs_is_refused_and_a_normal_dir_is_not() {
        let root = scratch("allow");
        DirFd::open(&root).unwrap();
        match DirFd::open(Path::new("/dev")) {
            Ok(_) => {}
            Err(Error::Pseudo { kind: "devtmpfs" }) => {}
            Err(err) => panic!("unexpected /dev error: {err}"),
        }
        let _ = std::fs::remove_dir_all(&root);
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
            },
        )
        .unwrap_err();
        assert!(err.to_string().contains("injected"));
        assert_eq!(std::fs::read(root.join("a")).unwrap(), b"z");
        assert_eq!(std::fs::read(root.join("b")).unwrap(), b"z");
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn a_directory_is_not_moved_across_devices_with_eisdir() {
        let root = scratch("dirmove");
        std::fs::create_dir(root.join("folder")).unwrap();
        let dir = DirFd::open(&root).unwrap();
        let cancel = Cancel::new();
        let mut policy = ConflictPolicy::new();
        let mut ask = replace_ask;
        let err = move_child(
            &dir,
            std::ffi::OsStr::new("folder"),
            &dir,
            std::ffi::OsStr::new("elsewhere"),
            &mut MoveOpts {
                force_copy: true,
                fail_before_unlink: false,
                copy: opts(&cancel, &mut policy, &mut ask),
            },
        )
        .unwrap_err();
        assert!(err.to_string().contains("directory"), "{err}");
        assert!(!err.to_string().contains("EISDIR"));
        assert!(root.join("folder").is_dir());
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
    fn deletion_date_is_iso_local() {
        assert_eq!(format_deletion_date(1_700_000_000), "2023-11-14T22:13:20");
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
