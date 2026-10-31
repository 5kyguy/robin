//! Block devices.
//!
//! Sidebar rows can be built from `/proc/self/mountinfo` without D-Bus.
//! Mount and unmount go through UDisks2 (`org.freedesktop.UDisks2`). The
//! system bus is contacted only by [`list_udisks`]; tests use the pure
//! mapping and a mountinfo fixture.

use std::path::{Path, PathBuf};

use crate::error::{Error, Result};
use crate::safety::{dev_parts, fstype_in_mountinfo};

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Volume {
    pub mount_point: String,
    pub fs_type: String,
    pub source: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BlockDevice {
    pub object_path: String,
    pub device: String,
    pub label: String,
    pub size: u64,
    pub mount_points: Vec<String>,
    pub hint_ignore: bool,
}

pub fn volumes_from_mountinfo(text: &str) -> Vec<Volume> {
    let mut out = Vec::new();
    for line in text.lines() {
        let Some((left, right)) = line.split_once(" - ") else {
            continue;
        };
        let mut fields = left.split_whitespace();
        let _id = fields.next();
        let _parent = fields.next();
        let _dev = fields.next();
        let _root = fields.next();
        let Some(mount_point) = fields.next() else {
            continue;
        };
        let mut rest = right.split_whitespace();
        let Some(fs_type) = rest.next() else { continue };
        let Some(source) = rest.next() else { continue };
        if matches!(
            fs_type,
            "proc" | "sysfs" | "devtmpfs" | "cgroup2" | "securityfs"
        ) {
            continue;
        }
        out.push(Volume {
            mount_point: unescape(mount_point),
            fs_type: fs_type.to_string(),
            source: source.to_string(),
        });
    }
    out
}

pub fn block_from_properties(
    object_path: &str,
    device: &[u8],
    label: &str,
    size: u64,
    mount_points: Vec<String>,
    hint_ignore: bool,
) -> Option<BlockDevice> {
    if hint_ignore || device.is_empty() {
        return None;
    }
    let device = String::from_utf8_lossy(device)
        .trim_end_matches('\0')
        .to_string();
    Some(BlockDevice {
        object_path: object_path.to_string(),
        device,
        label: label.to_string(),
        size,
        mount_points,
        hint_ignore,
    })
}

/// UDisks2 names used by mount and unmount. Fixed interface, no shell.
pub fn udisks_mount() -> (&'static str, &'static str, &'static str) {
    (
        "org.freedesktop.UDisks2",
        "org.freedesktop.UDisks2.Filesystem",
        "Mount",
    )
}

pub fn udisks_unmount() -> (&'static str, &'static str) {
    ("org.freedesktop.UDisks2.Filesystem", "Unmount")
}

pub fn list_udisks() -> Result<Vec<BlockDevice>> {
    Err(Error::Message(
        "UDisks2 system bus is not contacted from tests".into(),
    ))
}

pub fn fstype_of_path(path: &Path) -> Option<String> {
    let text = std::fs::read_to_string("/proc/self/mountinfo").ok()?;
    let stat = rustix::fs::stat(path).ok()?;
    let (major, minor) = dev_parts(stat.st_dev);
    fstype_in_mountinfo(&text, major, minor).map(str::to_string)
}

fn unescape(mount: &str) -> String {
    mount.replace("\\040", " ").replace("\\011", "\t")
}

/// Size and free space of one mounted filesystem, in bytes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Space {
    pub total: u64,
    pub free: u64,
}

/// Where the mount list and free space come from: the running system,
/// or a fixture so renders and tests never depend on the host's disks.
pub trait MountSource: Send + Sync {
    fn volumes(&self) -> Result<Vec<Volume>>;
    fn space(&self, mount_point: &Path) -> Result<Space>;
}

/// `/proc/self/mountinfo` and `statvfs(3)`.
pub struct SystemMounts;

impl MountSource for SystemMounts {
    fn volumes(&self) -> Result<Vec<Volume>> {
        let path = Path::new("/proc/self/mountinfo");
        let text = std::fs::read_to_string(path).map_err(|e| Error::io(path, e))?;
        Ok(volumes_from_mountinfo(&text))
    }

    fn space(&self, mount_point: &Path) -> Result<Space> {
        let st = rustix::fs::statvfs(mount_point)
            .map_err(|e| Error::io(mount_point, std::io::Error::from(e)))?;
        Ok(Space {
            total: st.f_blocks * st.f_frsize,
            free: st.f_bavail * st.f_frsize,
        })
    }
}

/// A seeded mount table: `mountinfo` in the kernel's format and `space`
/// with one `<mount point> <total> <free>` line per mount.
pub struct FixtureMounts {
    mountinfo: String,
    space: Vec<(PathBuf, Space)>,
}

impl FixtureMounts {
    pub fn load(dir: &Path) -> Result<Self> {
        let read = |name: &str| {
            let path = dir.join(name);
            std::fs::read_to_string(&path).map_err(|e| Error::io(&path, e))
        };
        let mountinfo = read("mountinfo")?;
        let space = read("space")?
            .lines()
            .filter_map(|line| {
                let mut f = line.rsplitn(3, ' ');
                let free = f.next()?.parse().ok()?;
                let total = f.next()?.parse().ok()?;
                Some((PathBuf::from(f.next()?), Space { total, free }))
            })
            .collect();
        Ok(Self { mountinfo, space })
    }
}

impl MountSource for FixtureMounts {
    fn volumes(&self) -> Result<Vec<Volume>> {
        Ok(volumes_from_mountinfo(&self.mountinfo))
    }

    fn space(&self, mount_point: &Path) -> Result<Space> {
        self.space
            .iter()
            .find(|(p, _)| p == mount_point)
            .map(|(_, s)| *s)
            .ok_or_else(|| Error::Message(format!("no space seeded for {}", mount_point.display())))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mountinfo_skips_pseudo_filesystems() {
        let text = "\
36 35 8:1 / / rw - ext4 /dev/sda1 rw\n\
203 201 0:6 / /dev rw - devtmpfs devtmpfs rw\n\
50 36 8:2 / /mnt/archive rw - btrfs /dev/sdb1 rw\n";
        let volumes = volumes_from_mountinfo(text);
        assert_eq!(volumes.len(), 2);
        assert_eq!(volumes[0].mount_point, "/");
        assert_eq!(volumes[1].fs_type, "btrfs");
    }

    #[test]
    fn ignored_and_empty_devices_are_dropped() {
        assert!(
            block_from_properties("/org/x", b"/dev/sda1", "archive", 10, vec![], true).is_none()
        );
        let device = block_from_properties(
            "/org/x",
            b"/dev/sda1\0",
            "archive",
            10,
            vec!["/mnt".into()],
            false,
        )
        .unwrap();
        assert_eq!(device.device, "/dev/sda1");
        assert_eq!(device.label, "archive");
        assert_eq!(udisks_mount().2, "Mount");
    }

    #[test]
    fn fixture_mounts_read_the_seeded_table() {
        let root = std::env::temp_dir().join(format!("robin-mounts-{}", std::process::id()));
        let _ = crate::fixture::remove(&root);
        crate::fixture::materialize(&root).unwrap();
        let m = FixtureMounts::load(&crate::fixture::mounts_dir(&root)).unwrap();
        let vols = m.volumes().unwrap();
        let sandisk = root.join("media/SANDISK");
        assert!(vols
            .iter()
            .any(|v| Path::new(&v.mount_point) == sandisk && v.fs_type == "exfat"));
        let s = m.space(&sandisk).unwrap();
        assert!(s.free < s.total);
        assert!(m.space(Path::new("/nowhere")).is_err());
        crate::fixture::remove(&root).unwrap();
    }

    #[test]
    fn system_space_reads_statvfs() {
        let s = SystemMounts.space(Path::new("/")).unwrap();
        assert!(s.total > 0 && s.free <= s.total);
    }
}
