//! Block devices.
//!
//! Sidebar rows can be built from `/proc/self/mountinfo` without D-Bus.
//! Mount and unmount go through UDisks2 (`org.freedesktop.UDisks2`). The
//! system bus is contacted only by [`list_udisks`]; tests use the pure
//! mapping and a mountinfo fixture.

use std::path::Path;

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
}
