//! Directory-fd operations and the pseudo-filesystem guard.
//!
//! A mutation names a child of an already-opened directory. `openat` uses
//! `O_NOFOLLOW`, so a symlink swapped in after the listing is not descended.
//! Pseudo filesystems are classified from `f_type`, not from a path prefix.

use std::ffi::{CString, OsStr, OsString};
use std::os::fd::{AsFd, OwnedFd};
use std::os::unix::ffi::{OsStrExt, OsStringExt};
use std::path::Path;

use rustix::fs::{AtFlags, Dir, Mode, OFlags};
use rustix::io::Errno;

use crate::error::{Error, Result};

/// procfs and sysfs have their own magics. devtmpfs shares `0x01021994` with
/// tmpfs, so that number alone must not refuse a directory: it would refuse
/// every tmpfs, including the private `/tmp` a bubblewrap test uses. devtmpfs
/// is told apart by the filesystem type string in mountinfo.
const PROC_SUPER_MAGIC: u64 = 0x9fa0;
const SYSFS_MAGIC: u64 = 0x6265_6572;
const TMPFS_MAGIC: u64 = 0x102_1994;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FsClass {
    Normal,
    Proc,
    Sysfs,
    DevTmp,
}

impl FsClass {
    pub fn name(self) -> &'static str {
        match self {
            Self::Normal => "normal",
            Self::Proc => "procfs",
            Self::Sysfs => "sysfs",
            Self::DevTmp => "devtmpfs",
        }
    }

    pub fn is_pseudo(self) -> bool {
        !matches!(self, Self::Normal)
    }
}

pub fn classify_magic(magic: u64) -> FsClass {
    match magic {
        PROC_SUPER_MAGIC => FsClass::Proc,
        SYSFS_MAGIC => FsClass::Sysfs,
        _ => FsClass::Normal,
    }
}

/// Filesystem type string for `major:minor`, from `/proc/self/mountinfo`.
pub fn fstype_in_mountinfo(text: &str, major: u32, minor: u32) -> Option<&str> {
    for line in text.lines() {
        let Some((left, right)) = line.split_once(" - ") else {
            continue;
        };
        let mut fields = left.split_whitespace();
        let Some(device) = fields.nth(2) else {
            continue;
        };
        let Some((maj, min)) = device.split_once(':') else {
            continue;
        };
        let Ok(maj) = maj.parse::<u32>() else {
            continue;
        };
        let Ok(min) = min.parse::<u32>() else {
            continue;
        };
        if maj == major && min == minor {
            return right.split_whitespace().next();
        }
    }
    None
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Dirent {
    pub name: OsString,
    pub is_dir: bool,
    pub is_symlink: bool,
    pub len: u64,
    pub mtime_secs: i64,
    pub mode: u32,
}

/// An owned directory file descriptor.
#[derive(Debug)]
pub struct DirFd {
    fd: OwnedFd,
}

impl DirFd {
    /// Open `path` as a directory. The leaf may be a symlink to a directory
    /// so browsing can enter a linked folder. The filesystem type is checked
    /// after the open.
    pub fn open(path: &Path) -> Result<Self> {
        let fd = rustix::fs::open(
            path,
            OFlags::RDONLY | OFlags::DIRECTORY | OFlags::CLOEXEC,
            Mode::empty(),
        )
        .map_err(|e| Error::from_errno(path, e))?;
        let dir = Self { fd };
        dir.reject_pseudo(path)?;
        Ok(dir)
    }

    /// Open a child directory. A symlink child fails with `ELOOP` instead of
    /// being followed.
    pub fn open_dir_nofollow(&self, name: &OsStr) -> Result<Self> {
        let name = component(name)?;
        let fd = rustix::fs::openat(
            &self.fd,
            name.as_c_str(),
            OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
            Mode::empty(),
        )
        .map_err(|e| Error::from_errno(OsStr::from_bytes(name.as_bytes()), e))?;
        let dir = Self { fd };
        dir.reject_pseudo(Path::new(OsStr::from_bytes(name.as_bytes())))?;
        Ok(dir)
    }

    pub fn list(&self) -> Result<Vec<Dirent>> {
        let dup = rustix::io::dup(&self.fd).map_err(|e| Error::from_errno(".", e))?;
        let reader = Dir::read_from(dup).map_err(|e| Error::from_errno(".", e))?;
        let mut out = Vec::new();
        for item in reader {
            let entry = item.map_err(|e| Error::from_errno(".", e))?;
            let bytes = entry.file_name().to_bytes();
            if bytes == b"." || bytes == b".." {
                continue;
            }
            let os_name = OsString::from_vec(bytes.to_vec());
            let stat =
                match rustix::fs::statat(&self.fd, entry.file_name(), AtFlags::SYMLINK_NOFOLLOW) {
                    Ok(stat) => stat,
                    Err(_) => continue,
                };
            let mode = stat.st_mode;
            let is_symlink = (mode & libc_s_ifmt()) == libc_s_iflnk();
            let is_dir = (mode & libc_s_ifmt()) == libc_s_ifdir();
            out.push(Dirent {
                name: os_name,
                is_dir,
                is_symlink,
                len: stat.st_size as u64,
                mtime_secs: stat.st_mtime as i64,
                mode,
            });
        }
        Ok(out)
    }

    pub fn as_fd(&self) -> std::os::fd::BorrowedFd<'_> {
        self.fd.as_fd()
    }

    fn reject_pseudo(&self, path: &Path) -> Result<()> {
        let class = class_of(self.fd.as_fd()).map_err(|e| Error::from_errno(path, e))?;
        if class.is_pseudo() {
            Err(Error::Pseudo { kind: class.name() })
        } else {
            Ok(())
        }
    }
}

fn class_of(fd: impl AsFd) -> std::result::Result<FsClass, Errno> {
    let stat = rustix::fs::fstatfs(&fd)?;
    let magic = stat.f_type as u64;
    let class = classify_magic(magic);
    if class.is_pseudo() {
        return Ok(class);
    }
    if magic == TMPFS_MAGIC {
        let file = rustix::fs::fstat(&fd)?;
        let (major, minor) = dev_parts(file.st_dev);
        if let Ok(text) = std::fs::read_to_string("/proc/self/mountinfo") {
            if fstype_in_mountinfo(&text, major, minor) == Some("devtmpfs") {
                return Ok(FsClass::DevTmp);
            }
        }
    }
    Ok(FsClass::Normal)
}

pub fn dev_parts(dev: u64) -> (u32, u32) {
    let major = (((dev >> 32) & 0xffff_f000) | ((dev >> 8) & 0xfff)) as u32;
    let minor = (((dev >> 12) & 0xffff_ff00) | (dev & 0xff)) as u32;
    (major, minor)
}

fn component(name: &OsStr) -> Result<CString> {
    let bytes = name.as_bytes();
    if bytes.is_empty() || bytes.contains(&b'/') || bytes == b"." || bytes == b".." {
        return Err(Error::InvalidName);
    }
    CString::new(bytes).map_err(|_| Error::InvalidName)
}

fn libc_s_ifmt() -> u32 {
    0o170000
}
fn libc_s_ifdir() -> u32 {
    0o040000
}
fn libc_s_iflnk() -> u32 {
    0o120000
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    #[test]
    fn magic_numbers_match_the_review() {
        assert_eq!(classify_magic(0x9fa0), FsClass::Proc);
        assert_eq!(classify_magic(0x6265_6572), FsClass::Sysfs);
        // Shared with tmpfs. The fd check consults mountinfo before refusing.
        assert_eq!(classify_magic(0x102_1994), FsClass::Normal);
        assert_eq!(classify_magic(0xef53), FsClass::Normal);
    }

    #[test]
    fn mountinfo_distinguishes_devtmpfs_from_tmpfs() {
        let text = "\
203 201 0:6 / /dev rw - devtmpfs devtmpfs rw\n\
36 35 0:46 / /tmp rw - tmpfs tmpfs rw\n";
        assert_eq!(fstype_in_mountinfo(text, 0, 6), Some("devtmpfs"));
        assert_eq!(fstype_in_mountinfo(text, 0, 46), Some("tmpfs"));
    }

    #[test]
    fn dev_is_refused_when_it_is_devtmpfs() {
        let text = fs::read_to_string("/proc/self/mountinfo").unwrap();
        let stat = rustix::fs::stat("/dev").unwrap();
        let (major, minor) = dev_parts(stat.st_dev);
        if fstype_in_mountinfo(&text, major, minor) != Some("devtmpfs") {
            // bubblewrap's `--dev` is a plain tmpfs, which is safe to open.
            return;
        }
        let err = DirFd::open(Path::new("/dev")).unwrap_err();
        assert!(matches!(err, Error::Pseudo { kind: "devtmpfs" }), "{err}");
    }

    #[test]
    fn proc_is_refused_and_a_temp_dir_is_not() {
        let err = DirFd::open(Path::new("/proc")).unwrap_err();
        assert!(matches!(err, Error::Pseudo { kind: "procfs" }), "{err}");

        let dir = tempfile_dir("list");
        let opened = DirFd::open(&dir).unwrap();
        fs::write(dir.join("note.txt"), b"hello").unwrap();
        fs::create_dir(dir.join("sub")).unwrap();
        std::os::unix::fs::symlink("note.txt", dir.join("link")).unwrap();
        let mut names: Vec<_> = opened
            .list()
            .unwrap()
            .into_iter()
            .map(|e| e.name.to_string_lossy().into_owned())
            .collect();
        names.sort();
        assert_eq!(names, ["link", "note.txt", "sub"]);
        let link = opened
            .list()
            .unwrap()
            .into_iter()
            .find(|e| e.name == "link")
            .unwrap();
        assert!(link.is_symlink);
        assert!(!link.is_dir);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn nofollow_does_not_descend_a_symlink() {
        let dir = tempfile_dir("nofollow");
        let real = dir.join("real");
        fs::create_dir(&real).unwrap();
        fs::write(real.join("secret"), b"nope").unwrap();
        std::os::unix::fs::symlink("real", dir.join("via")).unwrap();
        let root = DirFd::open(&dir).unwrap();
        let err = root.open_dir_nofollow(OsStr::new("via")).unwrap_err();
        let Error::Io { source, .. } = err else {
            panic!("expected io error, got {err}");
        };
        // Linux reports ENOTDIR when O_DIRECTORY|O_NOFOLLOW hits a symlink.
        // ELOOP is the other acceptable refusal. Either way the target stays.
        let code = source.raw_os_error();
        assert!(
            code == Some(Errno::NOTDIR.raw_os_error()) || code == Some(Errno::LOOP.raw_os_error()),
            "unexpected errno {code:?}"
        );
        assert_eq!(fs::read(real.join("secret")).unwrap(), b"nope");
        let _ = fs::remove_dir_all(&dir);
    }

    fn tempfile_dir(label: &str) -> std::path::PathBuf {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("clock")
            .as_nanos();
        let path =
            std::env::temp_dir().join(format!("robin-{label}-{}-{nanos}", std::process::id()));
        fs::create_dir_all(&path).unwrap();
        path
    }
}
