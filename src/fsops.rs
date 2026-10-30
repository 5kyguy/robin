//! Explicit writes. Browsing never calls this module.
//!
//! Deletes and copies go through a directory file descriptor and do not
//! follow the final symlink. A same-filesystem move is a rename. Across
//! filesystems the file is copied to a temporary name, fsynced, renamed
//! into place, and only then is the source removed.

use std::fs::File;
use std::io::{Read, Write};
use std::os::unix::ffi::OsStrExt;
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use rustix::fs::{AtFlags, Mode, OFlags};

use crate::error::{Error, Result};
use crate::safety::DirFd;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TrashChoice {
    /// Same filesystem as the home data directory.
    Home,
    /// The volume's own `.Trash/$uid` or `.Trash-$uid`.
    Volume,
    /// Volume trash cannot be created. Do not copy onto the home trash
    /// unless the caller has already confirmed that fallback.
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

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Conflict {
    Replace,
    Rename,
    Skip,
}

pub struct Cancel {
    flag: AtomicBool,
}

impl Cancel {
    pub fn new() -> Self {
        Self {
            flag: AtomicBool::new(false),
        }
    }

    pub fn cancel(&self) {
        self.flag.store(true, Ordering::Relaxed);
    }

    pub fn is_cancelled(&self) -> bool {
        self.flag.load(Ordering::Relaxed)
    }
}

impl Default for Cancel {
    fn default() -> Self {
        Self::new()
    }
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
    rustix::fs::renameat(
        parent.as_fd(),
        from_c.as_c_str(),
        parent.as_fd(),
        to_c.as_c_str(),
    )
    .map_err(|e| Error::from_errno(from, e))
}

/// Remove `name` inside `parent`. Symlinks are unlinked, not followed.
/// `before` runs after the name is known and before it is unlinked, so a
/// test can swap the entry for a symlink.
pub fn remove_child(
    parent: &DirFd,
    name: &std::ffi::OsStr,
    before: &mut dyn FnMut(&std::ffi::OsStr),
) -> Result<()> {
    let c = component(name)?;
    before(name);
    let stat = rustix::fs::statat(parent.as_fd(), c.as_c_str(), AtFlags::SYMLINK_NOFOLLOW)
        .map_err(|e| Error::from_errno(name, e))?;
    let mode = stat.st_mode;
    let symlink = is_lnk(mode);
    let dir = is_dir(mode);
    if symlink || !dir {
        rustix::fs::unlinkat(parent.as_fd(), c.as_c_str(), AtFlags::empty())
            .map_err(|e| Error::from_errno(name, e))?;
        return Ok(());
    }
    match parent.open_dir_nofollow(name) {
        Ok(child) => {
            for entry in child.list()? {
                remove_child(&child, &entry.name, before)?;
            }
            rustix::fs::unlinkat(parent.as_fd(), c.as_c_str(), AtFlags::REMOVEDIR)
                .map_err(|e| Error::from_errno(name, e))
        }
        Err(Error::Io { source, .. })
            if source.raw_os_error() == Some(rustix::io::Errno::LOOP.raw_os_error())
                || source.raw_os_error() == Some(rustix::io::Errno::NOTDIR.raw_os_error()) =>
        {
            rustix::fs::unlinkat(parent.as_fd(), c.as_c_str(), AtFlags::empty())
                .map_err(|e| Error::from_errno(name, e))
        }
        Err(other) => Err(other),
    }
}

pub fn copy_file(
    parent: &DirFd,
    from: &std::ffi::OsStr,
    dest_parent: &DirFd,
    to: &std::ffi::OsStr,
    cancel: &Cancel,
) -> Result<()> {
    if cancel.is_cancelled() {
        return Err(Error::Cancelled);
    }
    let from_c = component(from)?;
    let to_c = component(to)?;
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
    let tmp_name = format!(".robin-partial-{}", std::process::id());
    let tmp = std::ffi::OsString::from(&tmp_name);
    let tmp_c = component(tmp.as_os_str())?;
    let dest = rustix::fs::openat(
        dest_parent.as_fd(),
        tmp_c.as_c_str(),
        OFlags::WRONLY | OFlags::CREATE | OFlags::EXCL | OFlags::NOFOLLOW | OFlags::CLOEXEC,
        Mode::from_raw_mode(0o644),
    )
    .map_err(|e| Error::from_errno(&tmp, e))?;
    let mut input = File::from(src);
    let mut output = File::from(dest);
    let mut buf = [0u8; 8192];
    loop {
        if cancel.is_cancelled() {
            let _ = rustix::fs::unlinkat(dest_parent.as_fd(), tmp_c.as_c_str(), AtFlags::empty());
            return Err(Error::Cancelled);
        }
        let n = input.read(&mut buf).map_err(|e| Error::io(from, e))?;
        if n == 0 {
            break;
        }
        output.write_all(&buf[..n]).map_err(|e| Error::io(to, e))?;
    }
    output.sync_all().map_err(|e| Error::io(to, e))?;
    rustix::fs::fchmod(&output, Mode::from_raw_mode(stat.st_mode & 0o777))
        .map_err(|e| Error::from_errno(to, e))?;
    let mtime = rustix::fs::Timespec {
        tv_sec: stat.st_mtime,
        tv_nsec: stat.st_mtime_nsec as i64,
    };
    let times = rustix::fs::Timestamps {
        last_access: mtime,
        last_modification: mtime,
    };
    rustix::fs::futimens(&output, &times).map_err(|e| Error::from_errno(to, e))?;
    drop(output);
    rustix::fs::renameat(
        dest_parent.as_fd(),
        tmp_c.as_c_str(),
        dest_parent.as_fd(),
        to_c.as_c_str(),
    )
    .map_err(|e| Error::from_errno(to, e))?;
    Ok(())
}

/// Move by rename when it works. `force_copy` exercises the cross-device path.
pub fn move_child(
    parent: &DirFd,
    from: &std::ffi::OsStr,
    dest_parent: &DirFd,
    to: &std::ffi::OsStr,
    force_copy: bool,
    cancel: &Cancel,
) -> Result<()> {
    if !force_copy {
        let from_c = component(from)?;
        let to_c = component(to)?;
        match rustix::fs::renameat(
            parent.as_fd(),
            from_c.as_c_str(),
            dest_parent.as_fd(),
            to_c.as_c_str(),
        ) {
            Ok(()) => return Ok(()),
            Err(err) if err == rustix::io::Errno::XDEV => {}
            Err(err) => return Err(Error::from_errno(from, err)),
        }
    }
    copy_file(parent, from, dest_parent, to, cancel)?;
    remove_child(parent, from, &mut |_| {})?;
    Ok(())
}

pub fn place_in_trash(
    parent: &DirFd,
    name: &std::ffi::OsStr,
    trash: &DirFd,
    choice: TrashChoice,
    allow_home_fallback: bool,
) -> Result<()> {
    match choice {
        TrashChoice::Ask if !allow_home_fallback => {
            return Err(Error::Message(
                "this drive has no trash; confirmation is required".into(),
            ));
        }
        TrashChoice::Home | TrashChoice::Volume | TrashChoice::Ask => {}
    }
    let info_name = format!("{}.trashinfo", name.to_string_lossy());
    let info = std::ffi::OsString::from(info_name);
    let body = format!(
        "[Trash Info]\nPath={}\nDeletionDate={}\n",
        percent_encode(name.as_bytes()),
        deletion_date()
    );
    {
        let c = component(info.as_os_str())?;
        let fd = rustix::fs::openat(
            trash.as_fd(),
            c.as_c_str(),
            OFlags::WRONLY | OFlags::CREATE | OFlags::EXCL | OFlags::CLOEXEC,
            Mode::from_raw_mode(0o644),
        )
        .map_err(|e| Error::from_errno(&info, e))?;
        let mut file = File::from(fd);
        file.write_all(body.as_bytes())
            .map_err(|e| Error::io(&info, e))?;
    }
    rename_across(parent, name, trash, name)
}

fn rename_across(
    from_dir: &DirFd,
    from: &std::ffi::OsStr,
    to_dir: &DirFd,
    to: &std::ffi::OsStr,
) -> Result<()> {
    let from_c = component(from)?;
    let to_c = component(to)?;
    rustix::fs::renameat(
        from_dir.as_fd(),
        from_c.as_c_str(),
        to_dir.as_fd(),
        to_c.as_c_str(),
    )
    .map_err(|e| Error::from_errno(from, e))
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
        .as_secs();
    format!("{secs}")
}

/// Refuse to operate if `path`'s directory is a pseudo filesystem.
pub fn guard_root(path: &Path) -> Result<DirFd> {
    DirFd::open(path)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;
    use std::sync::atomic::AtomicBool;

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

    #[test]
    fn symlink_swap_does_not_delete_the_target() {
        let root = scratch("swap");
        let box_dir = root.join("box");
        std::fs::create_dir(&box_dir).unwrap();
        std::fs::write(box_dir.join("victim"), b"x").unwrap();
        std::fs::write(root.join("precious"), b"secret").unwrap();
        let parent = DirFd::open(&root).unwrap();
        let swapped = AtomicBool::new(false);
        remove_child(&parent, std::ffi::OsStr::new("box"), &mut |name| {
            if name == "victim" && !swapped.swap(true, Ordering::Relaxed) {
                let victim = box_dir.join("victim");
                std::fs::remove_file(&victim).unwrap();
                std::os::unix::fs::symlink(root.join("precious"), &victim).unwrap();
            }
        })
        .unwrap();
        assert_eq!(std::fs::read(root.join("precious")).unwrap(), b"secret");
        assert!(!box_dir.exists());
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn deleting_a_symlink_leaves_the_target() {
        let root = scratch("link");
        std::fs::write(root.join("target"), b"keep").unwrap();
        std::os::unix::fs::symlink("target", root.join("link")).unwrap();
        let parent = DirFd::open(&root).unwrap();
        remove_child(&parent, std::ffi::OsStr::new("link"), &mut |_| {}).unwrap();
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
    fn copy_preserves_bytes_mode_and_mtime_and_cancel_removes_the_partial() {
        let root = scratch("copy");
        let src = root.join("src");
        let dst = root.join("dst");
        std::fs::create_dir(&src).unwrap();
        std::fs::create_dir(&dst).unwrap();
        let file = src.join("note.txt");
        std::fs::write(&file, b"hello robin").unwrap();
        std::fs::set_permissions(&file, std::fs::Permissions::from_mode(0o640)).unwrap();
        let parent = DirFd::open(&src).unwrap();
        let dest = DirFd::open(&dst).unwrap();
        copy_file(
            &parent,
            std::ffi::OsStr::new("note.txt"),
            &dest,
            std::ffi::OsStr::new("note.txt"),
            &Cancel::new(),
        )
        .unwrap();
        assert_eq!(std::fs::read(dst.join("note.txt")).unwrap(), b"hello robin");
        let mode = std::fs::metadata(dst.join("note.txt"))
            .unwrap()
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(mode, 0o640);
        assert!(dst.read_dir().unwrap().all(|e| !e
            .unwrap()
            .file_name()
            .to_string_lossy()
            .starts_with(".robin-partial")));

        let cancel = Cancel::new();
        cancel.cancel();
        let err = copy_file(
            &parent,
            std::ffi::OsStr::new("note.txt"),
            &dest,
            std::ffi::OsStr::new("other.txt"),
            &cancel,
        )
        .unwrap_err();
        assert!(matches!(err, Error::Cancelled));
        assert!(!dst.join("other.txt").exists());
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn same_filesystem_move_is_a_rename() {
        let root = scratch("move");
        std::fs::write(root.join("a"), b"z").unwrap();
        let dir = DirFd::open(&root).unwrap();
        move_child(
            &dir,
            std::ffi::OsStr::new("a"),
            &dir,
            std::ffi::OsStr::new("b"),
            false,
            &Cancel::new(),
        )
        .unwrap();
        assert!(!root.join("a").exists());
        assert_eq!(std::fs::read(root.join("b")).unwrap(), b"z");
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn forced_copy_move_removes_the_source_only_after_the_copy() {
        let root = scratch("exdev");
        std::fs::write(root.join("a"), b"z").unwrap();
        let dir = DirFd::open(&root).unwrap();
        move_child(
            &dir,
            std::ffi::OsStr::new("a"),
            &dir,
            std::ffi::OsStr::new("b"),
            true,
            &Cancel::new(),
        )
        .unwrap();
        assert!(!root.join("a").exists());
        assert_eq!(std::fs::read(root.join("b")).unwrap(), b"z");
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
            false,
        )
        .unwrap_err();
        assert!(err.to_string().contains("confirmation"));
        assert!(files.join("song.flac").is_file());
        place_in_trash(
            &src,
            std::ffi::OsStr::new("song.flac"),
            &bin,
            TrashChoice::Volume,
            false,
        )
        .unwrap();
        assert!(!files.join("song.flac").exists());
        assert!(trash.join("song.flac").is_file());
        assert!(trash.join("song.flac.trashinfo").is_file());
        let _ = std::fs::remove_dir_all(&root);
    }
}
