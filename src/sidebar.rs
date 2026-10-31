//! The sidebar's data: pinned folders, devices and trash.
//!
//! Everything here touches the disk (item counts, the mount table,
//! statvfs), so [`SideJob::perform`] runs on a worker thread and the
//! window only ever gets a finished [`SideData`].

use std::collections::{HashMap, HashSet};
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::sync::{mpsc, Arc, Mutex};
use std::time::Duration;

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
    /// A drive meant to be unplugged; breadcrumbs root at its label.
    pub removable: bool,
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
    /// Shared across jobs so a hung drive is probed by one thread at most.
    pub probe: Arc<SpaceProbe>,
}

/// One side job in flight at a time. A request while one runs marks the
/// gate dirty; when the running job lands, its now-stale result is
/// dropped and one fresh job starts in its place.
#[derive(Debug, Default)]
pub struct SideGate {
    gen: u64,
    busy: bool,
    dirty: bool,
}

/// What to do with a landed side job.
#[derive(Debug, PartialEq, Eq)]
pub enum SideLanding {
    /// Current: draw it.
    Apply,
    /// Superseded while it ran: drop it and start a fresh job.
    Rerun,
    /// From a generation already replaced: ignore.
    Drop,
}

impl SideGate {
    /// The generation to start, or `None` when a job is already running
    /// (it will be rerun when it lands).
    pub fn start(&mut self) -> Option<u64> {
        if self.busy {
            self.dirty = true;
            return None;
        }
        self.busy = true;
        self.dirty = false;
        self.gen += 1;
        Some(self.gen)
    }

    pub fn land(&mut self, gen: u64) -> SideLanding {
        if gen != self.gen || !self.busy {
            return SideLanding::Drop;
        }
        self.busy = false;
        if std::mem::take(&mut self.dirty) {
            SideLanding::Rerun
        } else {
            SideLanding::Apply
        }
    }
}

/// statvfs with a deadline. A probe still running from an earlier job is
/// not repeated: the last answer for that mount (or none) is used, so a
/// dead network share or a stuck drive never piles up threads.
pub struct SpaceProbe {
    timeout: Duration,
    cache: Mutex<HashMap<PathBuf, Probe>>,
}

#[derive(Default)]
struct Probe {
    last: Option<Space>,
    busy: bool,
}

impl SpaceProbe {
    pub fn new(timeout: Duration) -> Arc<Self> {
        Arc::new(Self {
            timeout,
            cache: Mutex::new(HashMap::new()),
        })
    }

    pub fn space(self: &Arc<Self>, mounts: &Arc<dyn MountSource>, path: &Path) -> Option<Space> {
        {
            let mut cache = self.cache.lock().unwrap_or_else(|e| e.into_inner());
            let entry = cache.entry(path.to_path_buf()).or_default();
            if entry.busy {
                return entry.last;
            }
            entry.busy = true;
        }
        let (tx, rx) = mpsc::channel();
        let (me, m, p) = (self.clone(), mounts.clone(), path.to_path_buf());
        std::thread::spawn(move || {
            let got = m.space(&p).ok();
            {
                let mut cache = me.cache.lock().unwrap_or_else(|e| e.into_inner());
                let entry = cache.entry(p).or_default();
                entry.busy = false;
                if got.is_some() {
                    entry.last = got;
                }
            }
            let _ = tx.send(got);
        });
        match rx.recv_timeout(self.timeout) {
            Ok(got) => got,
            Err(_) => self
                .cache
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .get(path)
                .and_then(|e| e.last),
        }
    }
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
        let config = self
            .env
            .config_dir()
            .unwrap_or_else(|| home.join(".config"));
        let dirs = std::fs::read_to_string(config.join("user-dirs.dirs"))
            .map(|text| user_dirs(&text, home))
            .unwrap_or_default();
        let dev = home.symlink_metadata().ok().map(|m| m.dev());
        let mut out = vec![folder("󰋜", "Home".into(), home.to_path_buf(), dev)];
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
            out.push(folder(glyph, name, path, dev));
        }
        out
    }

    fn devices(&self, volumes: &[Volume]) -> Vec<SideEntry> {
        let removable = |v: &Volume| self.mounts.removable(v);
        devices(volumes, self.env.home.as_deref(), &removable)
            .into_iter()
            .map(|(v, glyph, name, removable)| {
                let path = PathBuf::from(&v.mount_point);
                let space = self.probe.space(&self.mounts, &path);
                let usage = space.and_then(used);
                SideEntry {
                    glyph,
                    name,
                    path,
                    count: usage
                        .map(|u| format!("{}%", (u * 100.0).round() as u32))
                        .unwrap_or_default(),
                    usage,
                    removable,
                }
            })
            .collect()
    }

    fn trash(&self) -> SideEntry {
        let files = self.env.home_trash.join("files");
        SideEntry {
            glyph: "󰩺",
            name: "Trash".into(),
            count: visible_count(&files, None),
            path: files,
            usage: None,
            removable: false,
        }
    }

    fn space(&self, volumes: &[Volume]) -> String {
        mount_of(&self.dir, volumes)
            .and_then(|m| self.probe.space(&self.mounts, Path::new(m)))
            .map(space_text)
            .unwrap_or_default()
    }
}

fn folder(glyph: &'static str, name: String, path: PathBuf, home_dev: Option<u64>) -> SideEntry {
    let count = visible_count(&path, home_dev);
    SideEntry {
        glyph,
        name,
        path,
        count,
        usage: None,
        removable: false,
    }
}

/// Entries not starting with `.`, counted only for a real folder (not a
/// link) on `dev` when given: a pinned folder that points at another
/// drive is shown without a count rather than read.
pub fn visible_count(path: &Path, dev: Option<u64>) -> String {
    let Ok(meta) = path.symlink_metadata() else {
        return String::new();
    };
    if !meta.is_dir() || dev.is_some_and(|d| d != meta.dev()) {
        return String::new();
    }
    std::fs::read_dir(path)
        .map(|d| {
            d.flatten()
                .filter(|e| !e.file_name().to_string_lossy().starts_with('.'))
                .count()
                .to_string()
        })
        .unwrap_or_default()
}

/// `XDG_*_DIR="$HOME/..."` lines from `user-dirs.dirs`.
pub fn user_dirs(text: &str, home: &Path) -> Vec<(String, PathBuf)> {
    text.lines()
        .filter_map(|line| {
            let (key, value) = line.trim().split_once('=')?;
            let key = key.strip_prefix("XDG_")?.strip_suffix("_DIR")?;
            let value = value.trim().strip_prefix('"')?.strip_suffix('"')?;
            let value = shell_unquote(value);
            // Only `$HOME` itself or `$HOME/...`: `$HOMEX` is another name.
            let rest = value
                .strip_prefix("$HOME")
                .filter(|r| r.is_empty() || r.starts_with('/'));
            let path = match rest {
                Some(rest) => home.join(rest.trim_start_matches('/')),
                None if value.starts_with('/') => PathBuf::from(&value),
                None => return None,
            };
            Some((key.to_string(), path))
        })
        .collect()
}

/// Undo the `\"` and `\\` escapes xdg-user-dirs writes inside quotes.
fn shell_unquote(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    let mut chars = value.chars();
    while let Some(c) = chars.next() {
        match (c, chars.clone().next()) {
            ('\\', Some(n @ ('"' | '\\'))) => {
                out.push(n);
                chars.next();
            }
            _ => out.push(c),
        }
    }
    out
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

/// The device rows: real block filesystems, once per source and subtree
/// (a bind mount of the same tree is dropped, the shortest mount point
/// wins; btrfs subvolumes each keep a row), skipping loop devices and
/// system-only mounts like /boot. `(volume, glyph, name, removable)`.
pub fn devices<'a>(
    volumes: &'a [Volume],
    home: Option<&Path>,
    removable: &dyn Fn(&Volume) -> bool,
) -> Vec<(&'a Volume, &'static str, String, bool)> {
    let mut real: Vec<&Volume> = volumes
        .iter()
        .filter(|v| {
            !pseudo(&v.fs_type)
                && v.source.starts_with("/dev/")
                && !v.source.starts_with("/dev/loop")
        })
        .filter(|v| {
            !["/boot", "/snap", "/var", "/usr", "/opt", "/srv", "/nix"]
                .iter()
                .any(|p| Path::new(&v.mount_point).starts_with(p))
        })
        .collect();
    real.sort_by_key(|v| v.mount_point.len());
    let mut seen = HashSet::new();
    real.retain(|v| seen.insert((v.source.clone(), v.root.clone())));
    real.sort_by(|a, b| a.mount_point.cmp(&b.mount_point));
    real.into_iter()
        .map(|v| {
            let mount = Path::new(&v.mount_point);
            let removable = removable(v);
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
            (v, if removable { "󰕓" } else { "󰋊" }, name, removable)
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

/// Which row the open folder lights: the deepest entry that contains it,
/// so Pictures stays lit in Pictures/raw-exports, Home anywhere else under
/// ~, and a device anywhere on its drive. Ties go to the earlier section.
/// `(section, index)`.
pub fn picked(data: &SideData, dir: &Path) -> Option<(usize, usize)> {
    let mut best: Option<((usize, usize), usize)> = None;
    for (s, rows) in [&data.pinned, &data.devices, &data.trash]
        .iter()
        .enumerate()
    {
        for (i, r) in rows.iter().enumerate() {
            if !dir.starts_with(&r.path) {
                continue;
            }
            let depth = r.path.components().count();
            if best.is_none_or(|(_, d)| depth > d) {
                best = Some(((s, i), depth));
            }
        }
    }
    best.map(|(at, _)| at)
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
            root: "/".into(),
        }
    }

    fn on_media(v: &Volume) -> bool {
        crate::mounts::system_removable(v, Some("sky")) && v.source != "/dev/robin-none"
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
        let rows = devices(&v, Some(Path::new("/home/sky")), &on_media);
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
        let top = root.clone();
        let root = crate::fixture::home(&top);
        let stick = top.join("media/sky/SANDISK");
        let job = SideJob {
            env: Env::for_home(&root),
            mounts: Arc::new(FixtureMounts::load(&crate::fixture::mounts_dir(&top)).unwrap()),
            dir: stick.clone(),
            probe: SpaceProbe::new(Duration::from_secs(5)),
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
        assert!(d.devices[2].removable && !d.devices[1].removable);
        assert_eq!(picked(&d, &stick), Some((1, 2)));
        assert_eq!(
            picked(&d, &stick.join("DCIM/100")),
            Some((1, 2)),
            "anywhere on the drive"
        );
        let pictures = d.pinned.iter().position(|e| e.name == "Pictures").unwrap();
        assert_eq!(picked(&d, &root.join("Pictures")), Some((0, pictures)));
        assert_eq!(
            picked(&d, &root.join("Pictures/raw-exports")),
            Some((0, pictures)),
            "Pictures stays lit below it"
        );
        assert_eq!(
            picked(&d, &root.join("secret")),
            Some((0, 0)),
            "Home under ~"
        );
        assert_eq!(picked(&d, Path::new("/etc")), Some((1, 0)), "System");
        assert_eq!(
            picked(&d, &root.join(".local/share/Trash/files")),
            Some((2, 0))
        );
        crate::fixture::remove(&top).unwrap();
    }

    #[test]
    fn no_home_means_no_pinned_but_still_trash() {
        let job = SideJob {
            env: Env::from_vars(None, Some("/nonexistent-data".into())),
            mounts: Arc::new(crate::mounts::SystemMounts),
            dir: PathBuf::from("/"),
            probe: SpaceProbe::new(Duration::from_secs(5)),
        };
        let d = job.perform();
        assert!(d.pinned.is_empty());
        assert_eq!(d.trash.len(), 1);
    }

    #[test]
    fn devices_keep_subvolumes_and_drop_loops() {
        let mut home = vol("/home", "btrfs", "/dev/sda2");
        home.root = "/@home".into();
        let mut sys = vol("/", "btrfs", "/dev/sda2");
        sys.root = "/@".into();
        let mut bind = vol("/srv/again", "btrfs", "/dev/sda2");
        bind.root = "/@home".into();
        let v = [sys, home, bind, vol("/mnt/iso", "iso9660", "/dev/loop3")];
        let rows = devices(&v, Some(Path::new("/home/sky")), &|_| false);
        let names: Vec<_> = rows.iter().map(|r| r.2.as_str()).collect();
        assert_eq!(names, ["System", "Home disk"]);
    }

    #[test]
    fn user_dirs_unescape_and_only_expand_home_itself() {
        let text = "XDG_A_DIR=\"$HOME/My \\\"Pics\\\"\"\nXDG_B_DIR=\"$HOMEWORK/x\"\nXDG_C_DIR=\"$HOME\"\nXDG_D_DIR=\"/data/back\\\\slash\"\n";
        let d = user_dirs(text, Path::new("/home/sky"));
        assert_eq!(
            d,
            [
                ("A".to_string(), PathBuf::from("/home/sky/My \"Pics\"")),
                ("C".to_string(), PathBuf::from("/home/sky")),
                ("D".to_string(), PathBuf::from("/data/back\\slash")),
            ]
        );
    }

    #[test]
    fn user_dirs_come_from_xdg_config_home() {
        let root = std::env::temp_dir().join(format!("robin-xdgcfg-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let home = root.join("home");
        let cfg = root.join("cfg");
        std::fs::create_dir_all(home.join("Snaps")).unwrap();
        std::fs::create_dir_all(&cfg).unwrap();
        std::fs::write(
            cfg.join("user-dirs.dirs"),
            "XDG_PICTURES_DIR=\"$HOME/Snaps\"\n",
        )
        .unwrap();
        let mut env = Env::for_home(&home);
        env.config_home = Some(cfg);
        let job = SideJob {
            env,
            mounts: Arc::new(crate::mounts::SystemMounts),
            dir: home.clone(),
            probe: SpaceProbe::new(Duration::from_secs(5)),
        };
        assert!(job.pinned().iter().any(|e| e.name == "Snaps"));
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn counts_skip_hidden_links_and_other_drives() {
        let root = std::env::temp_dir().join(format!("robin-counts-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join("d")).unwrap();
        std::fs::write(root.join("d/a"), b"").unwrap();
        std::fs::write(root.join("d/.hidden"), b"").unwrap();
        std::os::unix::fs::symlink("d", root.join("link")).unwrap();
        let dev = root.symlink_metadata().unwrap().dev();
        assert_eq!(visible_count(&root.join("d"), Some(dev)), "1");
        assert_eq!(
            visible_count(&root.join("link"), Some(dev)),
            "",
            "a link isn't followed"
        );
        assert_eq!(
            visible_count(&root.join("d"), Some(dev + 1)),
            "",
            "another drive"
        );
        std::fs::remove_dir_all(&root).unwrap();
    }

    /// A drive whose statvfs never returns until released.
    struct Hung {
        calls: std::sync::atomic::AtomicUsize,
        gate: Mutex<mpsc::Receiver<()>>,
    }

    impl MountSource for Hung {
        fn volumes(&self) -> crate::error::Result<Vec<Volume>> {
            Ok(vec![])
        }
        fn space(&self, _: &Path) -> crate::error::Result<Space> {
            self.calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            let _ = self.gate.lock().unwrap().recv();
            Ok(Space { total: 10, free: 5 })
        }
        fn removable(&self, _: &Volume) -> bool {
            false
        }
    }

    #[test]
    fn a_hung_drive_is_probed_by_one_thread_at_most() {
        let (tx, rx) = mpsc::channel();
        let hung = Arc::new(Hung {
            calls: Default::default(),
            gate: Mutex::new(rx),
        });
        let mounts: Arc<dyn MountSource> = hung.clone();
        let probe = SpaceProbe::new(Duration::from_millis(50));
        let p = Path::new("/mnt/dead");
        assert_eq!(probe.space(&mounts, p), None, "timed out");
        assert_eq!(
            probe.space(&mounts, p),
            None,
            "still hung: cached, no new probe"
        );
        assert_eq!(hung.calls.load(std::sync::atomic::Ordering::SeqCst), 1);
        tx.send(()).unwrap();
        for _ in 0..100 {
            if !probe.cache.lock().unwrap()[p].busy {
                break;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        tx.send(()).unwrap();
        assert_eq!(probe.space(&mounts, p), Some(Space { total: 10, free: 5 }));
        assert_eq!(hung.calls.load(std::sync::atomic::Ordering::SeqCst), 2);
    }

    #[test]
    fn one_side_job_runs_and_a_stale_one_is_rerun() {
        let mut g = SideGate::default();
        let first = g.start().unwrap();
        assert_eq!(g.start(), None, "one in flight");
        assert_eq!(g.start(), None, "still one");
        assert_eq!(g.land(first), SideLanding::Rerun, "dirty: dropped, rerun");
        let second = g.start().unwrap();
        assert_eq!(g.land(first), SideLanding::Drop, "an old generation");
        assert_eq!(g.land(second), SideLanding::Apply);
        assert_eq!(g.land(second), SideLanding::Drop, "landing twice");
    }
}
