//! The sidebar's data: pinned folders, devices and trash.
//!
//! Everything here touches the disk (item counts, the mount table,
//! statvfs), so [`SideJob::perform`] runs on a worker thread and the
//! window only ever gets a finished [`SideData`].

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use crate::mounts::{MountSource, Space, Volume};
use crate::ops::Env;

/// One sidebar row and the folder it opens.
#[derive(Clone, Debug, PartialEq)]
pub struct SideEntry {
    pub glyph: &'static str,
    pub name: String,
    pub path: PathBuf,
    /// Item count, or "NN%" for a device; blank when unknown.
    pub count: String,
    /// Fraction used, for a device's bar.
    pub usage: Option<f32>,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct SideData {
    pub pinned: Vec<SideEntry>,
    pub devices: Vec<SideEntry>,
    /// Always one entry, so Trash is never missing from the sidebar.
    pub trash: Vec<SideEntry>,
    /// Free space of the open folder's mount, for status-right.
    pub space: String,
}

/// What a worker needs to build [`SideData`].
#[derive(Clone)]
pub struct SideJob {
    pub env: Env,
    pub mounts: Arc<dyn MountSource>,
    pub dir: PathBuf,
}

/// The XDG user dirs shown under Pinned, in order, with their keys in
/// `user-dirs.dirs` and their default names.
const XDG: &[(&str, &str, &str)] = &[
    ("DESKTOP", "Desktop", "󰇄"),
    ("DOCUMENTS", "Documents", "󰈙"),
    ("DOWNLOAD", "Downloads", "󰇚"),
    ("MUSIC", "Music", "󰝚"),
    ("PICTURES", "Pictures", "󰉏"),
    ("VIDEOS", "Videos", "󰕧"),
];

impl SideJob {
    pub fn perform(&self) -> SideData {
        let volumes = self.mounts.volumes().unwrap_or_default();
        SideData {
            pinned: self.pinned(),
            devices: self.devices(&volumes),
            trash: vec![self.trash()],
            space: self.space(&volumes),
        }
    }

    fn pinned(&self) -> Vec<SideEntry> {
        let Some(home) = self.env.home.as_deref() else {
            return Vec::new();
        };
        let dirs = std::fs::read_to_string(home.join(".config/user-dirs.dirs"))
            .map(|text| user_dirs(&text, home))
            .unwrap_or_default();
        let mut out = vec![folder("󰋜", "Home".into(), home.to_path_buf())];
        for (key, default, glyph) in XDG {
            let path = dirs
                .iter()
                .find(|(k, _)| k == key)
                .map(|(_, p)| p.clone())
                .unwrap_or_else(|| home.join(default));
            // xdg-user-dirs points a disabled dir at $HOME itself.
            if path == home || !path.is_dir() {
                continue;
            }
            let name = path
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_else(|| default.to_string());
            out.push(folder(glyph, name, path));
        }
        out
    }

    fn devices(&self, volumes: &[Volume]) -> Vec<SideEntry> {
        devices(volumes, self.env.home.as_deref())
            .into_iter()
            .map(|(v, glyph, name)| {
                let path = PathBuf::from(&v.mount_point);
                let space = self.mounts.space(&path).ok();
                let usage = space.and_then(used);
                SideEntry {
                    glyph,
                    name,
                    path,
                    count: usage
                        .map(|u| format!("{}%", (u * 100.0).round() as u32))
                        .unwrap_or_default(),
                    usage,
                }
            })
            .collect()
    }

    fn trash(&self) -> SideEntry {
        let files = self.env.home_trash.join("files");
        SideEntry {
            glyph: "󰩺",
            name: "Trash".into(),
            count: std::fs::read_dir(&files)
                .map(|d| d.count().to_string())
                .unwrap_or_default(),
            path: files,
            usage: None,
        }
    }

    fn space(&self, volumes: &[Volume]) -> String {
        mount_of(&self.dir, volumes)
            .and_then(|m| self.mounts.space(Path::new(m)).ok())
            .map(space_text)
            .unwrap_or_default()
    }
}

fn folder(glyph: &'static str, name: String, path: PathBuf) -> SideEntry {
    let count = std::fs::read_dir(&path)
        .map(|d| {
            d.flatten()
                .filter(|e| !e.file_name().to_string_lossy().starts_with('.'))
                .count()
                .to_string()
        })
        .unwrap_or_default();
    SideEntry {
        glyph,
        name,
        path,
        count,
        usage: None,
    }
}

/// `XDG_*_DIR="$HOME/..."` lines from `user-dirs.dirs`.
pub fn user_dirs(text: &str, home: &Path) -> Vec<(String, PathBuf)> {
    text.lines()
        .filter_map(|line| {
            let (key, value) = line.trim().split_once('=')?;
            let key = key.strip_prefix("XDG_")?.strip_suffix("_DIR")?;
            let value = value.trim().strip_prefix('"')?.strip_suffix('"')?;
            let path = match value.strip_prefix("$HOME") {
                Some(rest) => home.join(rest.trim_start_matches('/')),
                None if value.starts_with('/') => PathBuf::from(value),
                None => return None,
            };
            Some((key.to_string(), path))
        })
        .collect()
}

/// Filesystems that are never a disk a person would open.
fn pseudo(fs_type: &str) -> bool {
    matches!(
        fs_type,
        "tmpfs"
            | "devtmpfs"
            | "ramfs"
            | "proc"
            | "sysfs"
            | "cgroup"
            | "cgroup2"
            | "devpts"
            | "mqueue"
            | "hugetlbfs"
            | "debugfs"
            | "tracefs"
            | "securityfs"
            | "pstore"
            | "bpf"
            | "configfs"
            | "fusectl"
            | "binfmt_misc"
            | "autofs"
            | "efivarfs"
            | "nsfs"
            | "rpc_pipefs"
            | "overlay"
            | "squashfs"
            | "fuse.portal"
            | "fuse.gvfsd-fuse"
    )
}

/// The device rows: real block filesystems, once per source (a bind
/// mount of the same device is dropped; the shortest mount point wins),
/// skipping system-only mounts like /boot.
pub fn devices<'a>(
    volumes: &'a [Volume],
    home: Option<&Path>,
) -> Vec<(&'a Volume, &'static str, String)> {
    let mut real: Vec<&Volume> = volumes
        .iter()
        .filter(|v| !pseudo(&v.fs_type) && v.source.starts_with("/dev/"))
        .filter(|v| {
            !["/boot", "/snap", "/var", "/usr", "/opt", "/srv", "/nix"]
                .iter()
                .any(|p| Path::new(&v.mount_point).starts_with(p))
        })
        .collect();
    real.sort_by_key(|v| v.mount_point.len());
    let mut seen = HashSet::new();
    real.retain(|v| seen.insert(v.source.clone()));
    real.sort_by(|a, b| a.mount_point.cmp(&b.mount_point));
    real.into_iter()
        .map(|v| {
            let mount = Path::new(&v.mount_point);
            let removable = ["/media", "/run/media", "/mnt"]
                .iter()
                .any(|p| mount.starts_with(p))
                || mount.components().any(|c| c.as_os_str() == "media")
                || matches!(
                    v.fs_type.as_str(),
                    "vfat" | "exfat" | "ntfs" | "ntfs3" | "fuseblk"
                );
            let name = if v.mount_point == "/" {
                "System".to_string()
            } else if home.is_some_and(|h| h.starts_with(mount)) {
                "Home disk".to_string()
            } else {
                mount
                    .file_name()
                    .map(|n| n.to_string_lossy().into_owned())
                    .unwrap_or_else(|| v.mount_point.clone())
            };
            (v, if removable { "󰕓" } else { "󰋊" }, name)
        })
        .collect()
}

/// The mount `dir` lives on: the longest mount point that contains it.
pub fn mount_of<'a>(dir: &Path, volumes: &'a [Volume]) -> Option<&'a str> {
    volumes
        .iter()
        .map(|v| v.mount_point.as_str())
        .filter(|m| dir.starts_with(m))
        .max_by_key(|m| m.len())
}

fn used(s: Space) -> Option<f32> {
    (s.total > 0).then(|| (s.total.saturating_sub(s.free)) as f32 / s.total as f32)
}

/// Bar colour level: 0 normal, 1 warning from 85% used, 2 danger from 95%.
pub fn level(usage: f32) -> i32 {
    if usage >= 0.95 {
        2
    } else if usage >= 0.85 {
        1
    } else {
        0
    }
}

/// "812 GB free of 1.8 TB": whole numbers from 10 up or when round,
/// otherwise one decimal.
pub fn space_text(s: Space) -> String {
    format!("{} free of {}", size(s.free), size(s.total))
}

fn size(n: u64) -> String {
    let units = ["B", "KB", "MB", "GB", "TB", "PB"];
    let mut v = n as f64;
    let mut u = 0;
    while v >= 1000.0 && u < units.len() - 1 {
        v /= 1000.0;
        u += 1;
    }
    if u == 0 || v >= 10.0 || v.fract() < 0.05 {
        format!("{v:.0} {}", units[u])
    } else {
        format!("{v:.1} {}", units[u])
    }
}

/// Which row the open folder lights: a pinned folder first, then a
/// device's mount point, then trash. `(section, index)`.
pub fn picked(data: &SideData, dir: &Path) -> Option<(usize, usize)> {
    [&data.pinned, &data.devices, &data.trash]
        .iter()
        .enumerate()
        .find_map(|(s, rows)| rows.iter().position(|r| r.path == dir).map(|i| (s, i)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mounts::FixtureMounts;

    fn vol(mount: &str, fs: &str, source: &str) -> Volume {
        Volume {
            mount_point: mount.into(),
            fs_type: fs.into(),
            source: source.into(),
        }
    }

    #[test]
    fn devices_drop_pseudo_filesystems_and_bind_mounts() {
        let v = [
            vol("/", "ext4", "/dev/nvme0n1p2"),
            vol("/home", "ext4", "/dev/nvme0n1p3"),
            vol("/srv/bound-home", "ext4", "/dev/nvme0n1p3"),
            vol("/home/sky/bind", "ext4", "/dev/nvme0n1p3"),
            vol("/run", "tmpfs", "tmpfs"),
            vol("/snap/core/1", "squashfs", "/dev/loop0"),
            vol("/boot/efi", "vfat", "/dev/nvme0n1p1"),
            vol("/dev/shm", "tmpfs", "/dev/shm"),
            vol("/media/sky/SANDISK", "exfat", "/dev/sda1"),
            vol("/sys/fs/cgroup", "cgroup2", "cgroup2"),
        ];
        let rows = devices(&v, Some(Path::new("/home/sky")));
        let names: Vec<_> = rows.iter().map(|r| (r.2.as_str(), r.1)).collect();
        assert_eq!(
            names,
            [("System", "󰋊"), ("Home disk", "󰋊"), ("SANDISK", "󰕓")]
        );
        assert_eq!(
            rows[1].0.mount_point, "/home",
            "the bind mounts are dropped"
        );
    }

    #[test]
    fn user_dirs_follow_the_file_and_home() {
        let text = "# comment\nXDG_PICTURES_DIR=\"$HOME/Photos\"\nXDG_MUSIC_DIR=\"/data/music\"\nXDG_DESKTOP_DIR=\"$HOME/\"\nbad line\n";
        let d = user_dirs(text, Path::new("/home/sky"));
        assert_eq!(
            d,
            [
                ("PICTURES".to_string(), PathBuf::from("/home/sky/Photos")),
                ("MUSIC".to_string(), PathBuf::from("/data/music")),
                ("DESKTOP".to_string(), PathBuf::from("/home/sky/")),
            ]
        );
    }

    #[test]
    fn space_reads_like_a_person_would_say_it() {
        let s = |total, free| space_text(Space { total, free });
        assert_eq!(
            s(1_800_000_000_000, 812_000_000_000),
            "812 GB free of 1.8 TB"
        );
        assert_eq!(s(64_000_000_000, 6_000_000_000), "6 GB free of 64 GB");
        assert_eq!(level(0.5), 0);
        assert_eq!(level(0.9), 1);
        assert_eq!(level(0.97), 2);
    }

    #[test]
    fn the_fixture_sidebar_is_read_from_disk() {
        let root = std::env::temp_dir().join(format!("robin-side-{}", std::process::id()));
        let _ = crate::fixture::remove(&root);
        crate::fixture::materialize(&root).unwrap();
        let job = SideJob {
            env: Env::for_home(&root),
            mounts: Arc::new(FixtureMounts::load(&crate::fixture::mounts_dir(&root)).unwrap()),
            dir: root.join("media/SANDISK"),
        };
        let d = job.perform();
        let pinned: Vec<_> = d
            .pinned
            .iter()
            .map(|e| (e.name.as_str(), e.count.as_str()))
            .collect();
        assert_eq!(pinned[0].0, "Home");
        assert!(pinned.contains(&("Pictures", "7")), "{pinned:?}");
        let devices: Vec<_> = d
            .devices
            .iter()
            .map(|e| (e.name.as_str(), e.count.as_str()))
            .collect();
        assert_eq!(
            devices,
            [("System", "75%"), ("Home disk", "55%"), ("SANDISK", "91%")]
        );
        assert_eq!(d.trash.len(), 1);
        assert_eq!(
            (d.trash[0].name.as_str(), d.trash[0].count.as_str()),
            ("Trash", "1")
        );
        assert_eq!(d.space, "6 GB free of 64 GB", "the open folder's mount");
        assert_eq!(picked(&d, &root.join("media/SANDISK")), Some((1, 2)));
        assert_eq!(picked(&d, &root.join("Pictures")).map(|p| p.0), Some(0));
        assert_eq!(picked(&d, &root.join("secret")), None);
        crate::fixture::remove(&root).unwrap();
    }

    #[test]
    fn no_home_means_no_pinned_but_still_trash() {
        let job = SideJob {
            env: Env::from_vars(None, Some("/nonexistent-data".into())),
            mounts: Arc::new(crate::mounts::SystemMounts),
            dir: PathBuf::from("/"),
        };
        let d = job.perform();
        assert!(d.pinned.is_empty());
        assert_eq!(d.trash.len(), 1);
    }
}
