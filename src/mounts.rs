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
    /// The subtree of the source this mount shows (`/` for a whole
    /// filesystem, `/@home` for a btrfs subvolume).
    pub root: String,
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
        let root = fields.next().unwrap_or("/");
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
            source: unescape(source),
            root: unescape(root),
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

/// Undo the kernel's `\ooo` octal escapes (space, tab, newline,
/// backslash) in a mountinfo field.
fn unescape(field: &str) -> String {
    let b = field.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'\\'
            && i + 3 < b.len()
            && b[i + 1..i + 4].iter().all(|c| (b'0'..=b'7').contains(c))
        {
            let v = (b[i + 1] - b'0') as u32 * 64
                + (b[i + 2] - b'0') as u32 * 8
                + (b[i + 3] - b'0') as u32;
            if let Ok(byte) = u8::try_from(v) {
                out.push(byte);
                i += 4;
                continue;
            }
        }
        out.push(b[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
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
    /// A drive meant to be unplugged: a USB stick, an SD card.
    fn removable(&self, volume: &Volume) -> bool;
}

/// The desktop automount a path sits on, as (mount point, label), read
/// from the path alone: `/media/$USER/X` or `/run/media/$USER/X`. Lets
/// the breadcrumb start at the device before the mount table is read.
pub fn automount_of(path: &Path, user: Option<&str>) -> Option<(PathBuf, String)> {
    let user = user.filter(|u| !u.is_empty() && !u.contains('/'))?;
    [
        Path::new("/media").join(user),
        Path::new("/run/media").join(user),
    ]
    .into_iter()
    .find_map(|base| {
        let name = path.strip_prefix(&base).ok()?.components().next()?;
        let name = name.as_os_str();
        Some((base.join(name), name.to_string_lossy().into_owned()))
    })
}

/// True when the kernel marks the block device (or the disk a partition
/// belongs to) removable, or the desktop automounted it under
/// `/media/$USER` or `/run/media/$USER`.
pub fn system_removable(volume: &Volume, user: Option<&str>) -> bool {
    if let Some(user) = user.filter(|u| !u.is_empty()) {
        let mount = Path::new(&volume.mount_point);
        if mount.starts_with(Path::new("/media").join(user))
            || mount.starts_with(Path::new("/run/media").join(user))
        {
            return true;
        }
    }
    let Some(name) = volume.source.strip_prefix("/dev/") else {
        return false;
    };
    let block = Path::new("/sys/class/block").join(name);
    let flag =
        |dir: &Path| std::fs::read_to_string(dir.join("removable")).is_ok_and(|t| t.trim() == "1");
    // A partition's flag lives on its disk, one folder up.
    flag(&block)
        || std::fs::canonicalize(&block)
            .ok()
            .and_then(|p| p.parent().map(Path::to_path_buf))
            .is_some_and(|disk| flag(&disk))
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

    fn removable(&self, volume: &Volume) -> bool {
        let user = std::env::var("USER").ok();
        system_removable(volume, user.as_deref())
    }
}

/// A seeded mount table: `mountinfo` in the kernel's format and `space`
/// with one `<mount point> <total> <free>` line per mount.
pub struct FixtureMounts {
    mountinfo: String,
    space: Vec<(PathBuf, Space)>,
    /// Sources listed in `removable`, one per line.
    removable: Vec<String>,
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
        let removable = read("removable")
            .unwrap_or_default()
            .lines()
            .map(str::to_string)
            .collect();
        Ok(Self {
            mountinfo,
            space,
            removable,
        })
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

    fn removable(&self, volume: &Volume) -> bool {
        self.removable.contains(&volume.source)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mountinfo_octal_escapes_are_decoded() {
        let text = "40 22 8:1 / /media/My\\040Disk\\134x\\012y rw - exfat /dev/disk\\040a rw\n";
        let v = volumes_from_mountinfo(text);
        assert_eq!(v[0].mount_point, "/media/My Disk\\x\ny");
        assert_eq!(v[0].source, "/dev/disk a");
        assert_eq!(
            unescape("a\\0b\\"),
            "a\\0b\\",
            "short escapes stay as typed"
        );
    }

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
        let sandisk = root.join("media/sky/SANDISK");
        assert!(vols
            .iter()
            .any(|v| Path::new(&v.mount_point) == sandisk && v.fs_type == "exfat"));
        let s = m.space(&sandisk).unwrap();
        assert!(s.free < s.total);
        assert!(m.space(Path::new("/nowhere")).is_err());
        let stick = vols.iter().find(|v| v.source == "/dev/sda1").unwrap();
        assert!(m.removable(stick));
        assert!(!m.removable(&vols[0]));
        crate::fixture::remove(&root).unwrap();
    }

    #[test]
    fn media_user_mounts_are_removable() {
        let v = |mount: &str| Volume {
            mount_point: mount.into(),
            fs_type: "ext4".into(),
            source: "/dev/robin-none".into(),
            root: "/".into(),
        };
        assert!(system_removable(&v("/media/sky/STICK"), Some("sky")));
        assert_eq!(
            automount_of(Path::new("/run/media/sky/CARD/DCIM"), Some("sky")),
            Some((PathBuf::from("/run/media/sky/CARD"), "CARD".into()))
        );
        assert_eq!(
            automount_of(Path::new("/media/sky/STICK"), Some("sky")),
            Some((PathBuf::from("/media/sky/STICK"), "STICK".into()))
        );
        assert_eq!(automount_of(Path::new("/run/media/sky"), Some("sky")), None);
        assert_eq!(automount_of(Path::new("/media/ann/X"), Some("sky")), None);
        assert_eq!(automount_of(Path::new("/media/sky/X"), None), None);
        assert!(system_removable(&v("/run/media/sky/CARD"), Some("sky")));
        assert!(!system_removable(&v("/srv/media/disk"), Some("sky")));
        assert!(!system_removable(&v("/media/other/STICK"), Some("sky")));
        assert!(!system_removable(&v("/home/sky/media/x"), Some("sky")));
    }

    #[test]
    fn mountinfo_keeps_the_subvolume_root() {
        let v = volumes_from_mountinfo(
            "30 1 0:40 /@ / rw - btrfs /dev/sda2 rw\n31 1 0:40 /@home /home rw - btrfs /dev/sda2 rw\n",
        );
        assert_eq!((v[0].root.as_str(), v[1].root.as_str()), ("/@", "/@home"));
    }

    #[test]
    fn system_space_reads_statvfs() {
        let s = SystemMounts.space(Path::new("/")).unwrap();
        assert!(s.total > 0 && s.free <= s.total);
    }
}
