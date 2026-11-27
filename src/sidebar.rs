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
    /// False for a drive plugged in but not mounted: its name is muted,
    /// it has no bar, and a click mounts it. `path` is empty then.
    pub mounted: bool,
    /// Mounted read-only: the row gets a "Read-only" pill.
    pub read_only: bool,
    /// The device backend's id, when the backend knows this drive: it
    /// can be mounted and ejected.
    pub drive: Option<String>,
    /// The mount's filesystem, for the FAT32 file-size check.
    pub fs_type: String,
    /// A drive, phone or camera. Phones and cameras are slow: no trash,
    /// no counts, no thumbnails.
    pub kind: crate::devices::Kind,
    /// A phone mounted but not giving access yet: muted like an unmounted
    /// drive, and a click opens its waiting state.
    pub waiting: bool,
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
    /// Removable drives the device backend lists (none without one):
    /// unmounted ones get their own rows, mounted ones their id.
    pub drives: Vec<crate::devices::Drive>,
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

    /// A job that will never land (it panicked, or the event loop is
    /// gone): free the gate, rerunning if a request came in meanwhile.
    pub fn abandon(&mut self, gen: u64) -> SideLanding {
        self.land(gen)
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

/// Runs its callback on drop unless disarmed: a side job holds one so
/// the gate is freed even when the job panics or can't post its result.
pub struct SideGuard<F: FnOnce()>(Option<F>);

impl<F: FnOnce()> SideGuard<F> {
    pub fn new(on_abandon: F) -> Self {
        Self(Some(on_abandon))
    }
    pub fn disarm(mut self) {
        self.0 = None;
    }
}

impl<F: FnOnce()> Drop for SideGuard<F> {
    fn drop(&mut self) {
        if let Some(f) = self.0.take() {
            f();
        }
    }
}

/// Disk calls with a deadline: statvfs for a device, and stat plus a
/// listing for a folder's count. Any of these can hang on a dead network
/// share or a stuck drive. A probe still running from an earlier job is
/// not repeated: the last answer for that path (or none) is used, so a
/// hung mount never piles up threads.
///
/// A timed-out thread is left to finish on its own rather than pruned:
/// a thread blocked in the kernel can't be cancelled, and the `busy`
/// flag already caps it at one per path. When it does return, its answer
/// becomes the cached `last`. Cache entries for paths that went away are
/// kept too: there is one small entry per mount or pinned folder ever
/// seen, and dropping one while its thread still runs would let the next
/// job start a second thread on the same dead path.
pub struct SpaceProbe {
    timeout: Duration,
    cache: Mutex<HashMap<(Kind, PathBuf), Probe>>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
enum Kind {
    Space,
    Folder,
}

/// A folder as the sidebar sees it: a real directory (never a link) with
/// its device and visible item count.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FolderInfo {
    pub dev: u64,
    /// Read only when on the expected device; blank otherwise.
    pub count: String,
}

#[derive(Clone, Debug, PartialEq)]
enum Answer {
    Space(Space),
    /// `None`: missing, a link or not a folder.
    Folder(Option<FolderInfo>),
}

#[derive(Default)]
struct Probe {
    last: Option<Answer>,
    busy: bool,
}

impl SpaceProbe {
    pub fn new(timeout: Duration) -> Arc<Self> {
        Arc::new(Self {
            timeout,
            cache: Mutex::new(HashMap::new()),
        })
    }

    fn timed(
        self: &Arc<Self>,
        key: (Kind, PathBuf),
        work: impl FnOnce() -> Option<Answer> + Send + 'static,
    ) -> Option<Answer> {
        {
            let mut cache = self.cache.lock().unwrap_or_else(|e| e.into_inner());
            let entry = cache.entry(key.clone()).or_default();
            if entry.busy {
                return entry.last.clone();
            }
            entry.busy = true;
        }
        let (tx, rx) = mpsc::channel();
        let (me, k) = (self.clone(), key.clone());
        std::thread::spawn(move || {
            let got = work();
            {
                let mut cache = me.cache.lock().unwrap_or_else(|e| e.into_inner());
                let entry = cache.entry(k).or_default();
                entry.busy = false;
                if got.is_some() {
                    entry.last = got.clone();
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
                .get(&key)
                .and_then(|e| e.last.clone()),
        }
    }

    pub fn space(self: &Arc<Self>, mounts: &Arc<dyn MountSource>, path: &Path) -> Option<Space> {
        let m = mounts.clone();
        let p = path.to_path_buf();
        match self.timed((Kind::Space, path.to_path_buf()), move || {
            m.space(&p).ok().map(Answer::Space)
        }) {
            Some(Answer::Space(s)) => Some(s),
            _ => None,
        }
    }

    /// `path` as a real folder, counted when it is on `dev` (or always,
    /// with no `dev`). The outer `None` means the probe timed out with no
    /// earlier answer, so whether it is a folder is unknown.
    pub fn folder(self: &Arc<Self>, path: &Path, dev: Option<u64>) -> Option<Option<FolderInfo>> {
        // With no known device there is nothing to check a pinned folder
        // against, but the trash is always counted; callers that need the
        // same-device rule compare `FolderInfo::dev` themselves.
        let p = path.to_path_buf();
        match self.timed((Kind::Folder, path.to_path_buf()), move || {
            Some(Answer::Folder(folder_info(&p, dev)))
        }) {
            Some(Answer::Folder(f)) => Some(f),
            _ => None,
        }
    }
}

/// A pinned folder's count, only when it is on Home's known device.
fn pinned_count(f: FolderInfo, home_dev: Option<u64>) -> String {
    if home_dev == Some(f.dev) {
        f.count
    } else {
        String::new()
    }
}

fn folder_info(path: &Path, dev: Option<u64>) -> Option<FolderInfo> {
    let meta = path.symlink_metadata().ok()?;
    if !meta.is_dir() {
        return None;
    }
    Some(FolderInfo {
        dev: meta.dev(),
        count: visible_count(path, dev),
    })
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
        let home_info = self.probe.folder(home, None);
        let dev = home_info.clone().flatten().map(|f| f.dev);
        let count = home_info.flatten().map(|f| f.count).unwrap_or_default();
        let mut out = vec![folder("󰋜", "Home".into(), home.to_path_buf(), count)];
        for (key, default, glyph) in XDG {
            let path = dirs
                .iter()
                .find(|(k, _)| k == key)
                .map(|(_, p)| p.clone())
                .unwrap_or_else(|| home.join(default));
            // xdg-user-dirs points a disabled dir at $HOME itself.
            if path == home {
                continue;
            }
            // Only a real folder; a hung probe keeps the row, uncounted.
            // The count is shown only when the folder is on Home's own
            // device. If Home's probe timed out its device is unknown, so
            // no pinned folder is counted rather than one being read on
            // another drive.
            let count = match self.probe.folder(&path, dev) {
                Some(None) => continue,
                Some(Some(f)) => pinned_count(f, dev),
                None => String::new(),
            };
            let name = path
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_else(|| default.to_string());
            out.push(folder(glyph, name, path, count));
        }
        out
    }

    fn devices(&self, volumes: &[Volume]) -> Vec<SideEntry> {
        let removable = |v: &Volume| self.mounts.removable(v);
        let mut out: Vec<SideEntry> = devices(volumes, self.env.home.as_deref(), &removable)
            .into_iter()
            .map(|(v, glyph, name, removable)| {
                let path = PathBuf::from(&v.mount_point);
                let drive = self.drives.iter().find(|d| d.mount_points.contains(&path));
                let usage = self.probe.space(&self.mounts, &path).and_then(used);
                SideEntry {
                    glyph: if drive.is_some() { "󰕓" } else { glyph },
                    name,
                    path,
                    count: percent(usage),
                    usage,
                    removable: removable || drive.is_some(),
                    mounted: true,
                    read_only: v.read_only || drive.is_some_and(|d| d.read_only),
                    drive: drive.map(|d| d.id.clone()),
                    fs_type: v.fs_type.clone(),
                    kind: crate::devices::Kind::Drive,
                    waiting: false,
                }
            })
            .collect();
        // Drives the mount table doesn't show: plugged in but not mounted,
        // or mounted where this process's mountinfo can't see.
        for d in &self.drives {
            if out
                .iter()
                .any(|r| r.drive.as_deref() == Some(d.id.as_str()))
            {
                continue;
            }
            let row = match d.mounted() {
                // A phone or camera: named by the device, not its gvfs
                // folder ("mtp:host=…"), and never probed for space, since
                // every call crosses USB.
                Some(m) if d.media() => SideEntry {
                    glyph: d.kind.glyph(),
                    name: d.name(),
                    path: m.to_path_buf(),
                    count: String::new(),
                    usage: None,
                    removable: true,
                    mounted: true,
                    read_only: d.read_only,
                    drive: Some(d.id.clone()),
                    fs_type: d.fs_type.clone(),
                    kind: d.kind,
                    waiting: d.waiting,
                },
                Some(m) => {
                    let usage = self.probe.space(&self.mounts, m).and_then(used);
                    SideEntry {
                        glyph: drive_glyph(d),
                        name: m
                            .file_name()
                            .map(|n| n.to_string_lossy().into_owned())
                            .unwrap_or_else(|| d.name()),
                        path: m.to_path_buf(),
                        count: percent(usage),
                        usage,
                        removable: true,
                        mounted: true,
                        read_only: d.read_only,
                        drive: Some(d.id.clone()),
                        fs_type: d.fs_type.clone(),
                        kind: crate::devices::Kind::Drive,
                        waiting: false,
                    }
                }
                None => SideEntry {
                    glyph: drive_glyph(d),
                    name: d.name(),
                    path: PathBuf::new(),
                    count: String::new(),
                    usage: None,
                    removable: true,
                    mounted: false,
                    read_only: false,
                    drive: Some(d.id.clone()),
                    fs_type: d.fs_type.clone(),
                    kind: d.kind,
                    waiting: false,
                },
            };
            out.push(row);
        }
        out
    }

    fn trash(&self) -> SideEntry {
        let files = self.env.home_trash.join("files");
        SideEntry {
            glyph: "󰩺",
            name: "Trash".into(),
            count: self
                .probe
                .folder(&files, None)
                .flatten()
                .map(|f| f.count)
                .unwrap_or_default(),
            path: files,
            usage: None,
            removable: false,
            mounted: true,
            read_only: false,
            drive: None,
            fs_type: String::new(),
            kind: crate::devices::Kind::Drive,
            waiting: false,
        }
    }

    fn space(&self, volumes: &[Volume]) -> String {
        // On a phone or camera: only inside a storage, and that storage's
        // own numbers. gvfsd-fuse answers statfs per path from gvfs's
        // filesystem::free and ::size, which gvfs-mtp reads per storage
        // (the top folder would sum every storage). This runs on the side
        // job, behind the probe's timeout, so a slow phone can't stall it.
        let media_root = crate::devices::device_of(&self.drives, &self.dir)
            .filter(|d| d.media())
            .and_then(|d| d.mounted().map(Path::to_path_buf))
            .or_else(|| crate::devices::gvfs_media_root(&self.dir));
        if let Some(root) = media_root {
            return storage_of(&root, &self.dir)
                .and_then(|storage| self.probe.space(&self.mounts, &storage))
                .filter(|s| s.total > 0)
                .map(space_text)
                .unwrap_or_default();
        }
        mount_of(&self.dir, volumes)
            .and_then(|m| self.probe.space(&self.mounts, Path::new(m)))
            .map(space_text)
            .unwrap_or_default()
    }
}

/// The storage `dir` is in on a phone mounted at `root`: the first
/// folder under it. `None` at the top folder itself.
pub fn storage_of(root: &Path, dir: &Path) -> Option<PathBuf> {
    let first = dir.strip_prefix(root).ok()?.components().next()?;
    Some(root.join(first))
}

fn folder(glyph: &'static str, name: String, path: PathBuf, count: String) -> SideEntry {
    SideEntry {
        glyph,
        name,
        path,
        count,
        usage: None,
        removable: false,
        mounted: true,
        read_only: false,
        drive: None,
        fs_type: String::new(),
        kind: crate::devices::Kind::Drive,
        waiting: false,
    }
}

/// "42%" for a device's count column; blank when its space is unknown.
/// A drive row's glyph: the lock on a locked encrypted partition, the
/// open lock once it is unlocked (mounted or not), else its kind's.
fn drive_glyph(d: &crate::devices::Drive) -> &'static str {
    if d.locked {
        crate::devices::LOCK_GLYPH
    } else if !d.crypto.is_empty() {
        crate::devices::UNLOCKED_GLYPH
    } else {
        d.kind.glyph()
    }
}

fn percent(usage: Option<f32>) -> String {
    usage
        .map(|u| format!("{}%", (u * 100.0).round() as u32))
        .unwrap_or_default()
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
            // An unmounted drive has no folder to be in.
            if r.path.as_os_str().is_empty() || !dir.starts_with(&r.path) {
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

    #[test]
    fn an_encrypted_row_shows_a_lock_then_an_open_lock() {
        use crate::devices::{DeviceBackend, FakeDevices, FakeDrive, Passphrase};
        let fake = FakeDevices::new();
        fake.plug(FakeDrive::encrypted(
            "sdb1",
            "SANDISK",
            1 << 30,
            Path::new("/media/sky/SANDISK"),
            "pass",
        ));
        let glyph = |fake: &FakeDevices| drive_glyph(&fake.drives().unwrap()[0]);
        assert_eq!(glyph(&fake), crate::devices::LOCK_GLYPH);
        fake.unlock("sdb1", &Passphrase::new("pass")).unwrap();
        assert_eq!(glyph(&fake), crate::devices::UNLOCKED_GLYPH);
        fake.lock("sdb1").unwrap();
        assert_eq!(glyph(&fake), crate::devices::LOCK_GLYPH);
    }

    fn vol(mount: &str, fs: &str, source: &str) -> Volume {
        Volume {
            mount_point: mount.into(),
            fs_type: fs.into(),
            source: source.into(),
            root: "/".into(),
            read_only: false,
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
        // The rail dot's edges: amber from exactly 85%, red from 95%.
        assert_eq!((level(0.849), level(0.85)), (0, 1));
        assert_eq!((level(0.949), level(0.95)), (1, 2));
    }

    #[test]
    fn every_fixture_trash_is_on_the_home_mount() {
        let top = std::env::temp_dir().join(format!("robin-trashmount-{}", std::process::id()));
        let _ = crate::fixture::remove(&top);
        crate::fixture::materialize(&top).unwrap();
        let mounts = FixtureMounts::load(&crate::fixture::mounts_dir(&top)).unwrap();
        let volumes = mounts.volumes().unwrap();
        let home = crate::fixture::home(&top);
        let on = |p: &Path| mount_of(p, &volumes).map(str::to_string);
        let want = on(&home);
        assert!(want.is_some());
        assert_eq!(on(&Env::for_home(&home).home_trash.join("files")), want);
        assert_eq!(on(&crate::fixture::empty_trash(&top).join("files")), want);
        crate::fixture::remove(&top).unwrap();
    }

    #[test]
    fn a_phone_shows_its_storage_free_space_only_inside_a_storage() {
        let top = std::env::temp_dir().join(format!("robin-side-phone-{}", std::process::id()));
        let _ = crate::fixture::remove(&top);
        crate::fixture::materialize(&top).unwrap();
        let home = crate::fixture::home(&top);
        let at = top.join("run/user/1000/gvfs/mtp:host=Pixel");
        let storage = at.join("Internal shared storage");
        std::fs::create_dir_all(storage.join("DCIM")).unwrap();
        let mut phone = crate::devices::FakeDrive::phone("pixel", "Pixel 8", &at).drive;
        phone.mount_points = vec![at.clone()];
        let mounts = FixtureMounts::load(&crate::fixture::mounts_dir(&top))
            .unwrap()
            .with_space(&storage, 128_000_000_000, 12_000_000_000)
            .with_space(&at, 256_000_000_000, 40_000_000_000);
        let mounts: Arc<dyn MountSource> = Arc::new(mounts);
        let job = |dir: &Path, drives: Vec<crate::devices::Drive>| SideJob {
            env: Env::for_home(&home),
            mounts: mounts.clone(),
            dir: dir.to_path_buf(),
            probe: SpaceProbe::new(Duration::from_secs(5)),
            drives,
        };
        let space = |dir: &Path| job(dir, vec![phone.clone()]).perform().space;
        assert_eq!(space(&storage.join("DCIM")), "12 GB free of 128 GB");
        assert_eq!(space(&storage), "12 GB free of 128 GB");
        assert_eq!(
            space(&at),
            "",
            "not at the top folder, which sums every storage"
        );
        // A gvfs path with no backend row reads the same way.
        assert_eq!(
            job(&storage, Vec::new()).perform().space,
            "12 GB free of 128 GB"
        );
        let d = job(&storage, vec![phone.clone()]).perform();
        let row = d.devices.iter().find(|e| e.name == "Pixel 8").unwrap();
        assert_eq!(
            (row.usage, row.count.as_str()),
            (None, ""),
            "no usage bar on the row"
        );
        let _ = crate::fixture::remove(&top);
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
            drives: Vec::new(),
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
            [("System", "97%"), ("Home disk", "55%"), ("SANDISK", "91%")]
        );
        assert_eq!(d.trash.len(), 1);
        assert_eq!(
            (d.trash[0].name.as_str(), d.trash[0].count.as_str()),
            ("Trash", "4")
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
    fn backend_drives_mark_their_rows_and_add_unmounted_ones() {
        let root = std::env::temp_dir().join(format!("robin-side-drives-{}", std::process::id()));
        let _ = crate::fixture::remove(&root);
        crate::fixture::materialize(&root).unwrap();
        let top = root.clone();
        let home = crate::fixture::home(&top);
        let stick = top.join("media/sky/SANDISK");
        let mut mounted =
            crate::devices::FakeDrive::stick("sda1", "SANDISK", 64 << 30, &stick).drive;
        mounted.mount_points = vec![stick.clone()];
        mounted.read_only = true;
        let mut card = crate::devices::FakeDrive::stick("sdb1", "CARD", 32 << 30, &stick).drive;
        card.mount_points.clear();
        let job = SideJob {
            env: Env::for_home(&home),
            mounts: Arc::new(FixtureMounts::load(&crate::fixture::mounts_dir(&top)).unwrap()),
            dir: home.clone(),
            probe: SpaceProbe::new(Duration::from_secs(5)),
            drives: vec![mounted, card],
        };
        let d = job.perform();
        let sandisk = d.devices.iter().find(|e| e.name == "SANDISK").unwrap();
        assert_eq!(sandisk.drive.as_deref(), Some("sda1"));
        assert!(sandisk.mounted && sandisk.read_only && sandisk.removable);
        let card = d.devices.iter().find(|e| e.name == "CARD").unwrap();
        assert!(!card.mounted && card.path.as_os_str().is_empty());
        assert_eq!(
            (card.count.as_str(), card.drive.as_deref()),
            ("", Some("sdb1"))
        );
        assert_eq!(
            picked(&d, Path::new("/")),
            Some((1, 0)),
            "an unmounted row never lights for a folder"
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
            drives: Vec::new(),
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
            drives: Vec::new(),
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

    #[test]
    fn pinned_counts_need_homes_device() {
        let f = |dev| FolderInfo {
            dev,
            count: "4".into(),
        };
        assert_eq!(pinned_count(f(7), Some(7)), "4");
        assert_eq!(pinned_count(f(8), Some(7)), "", "another drive");
        assert_eq!(pinned_count(f(7), None), "", "home timed out: unknown");
    }

    #[test]
    fn a_pinned_link_or_file_is_not_a_folder() {
        let root = std::env::temp_dir().join(format!("robin-pinlink-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let home = root.join("home");
        let cfg = root.join("cfg");
        std::fs::create_dir_all(home.join("real")).unwrap();
        std::fs::create_dir_all(&cfg).unwrap();
        std::os::unix::fs::symlink("real", home.join("Snaps")).unwrap();
        std::fs::write(home.join("Tunes"), b"").unwrap();
        std::fs::write(
            cfg.join("user-dirs.dirs"),
            "XDG_PICTURES_DIR=\"$HOME/Snaps\"\nXDG_MUSIC_DIR=\"$HOME/Tunes\"\n",
        )
        .unwrap();
        let mut env = Env::for_home(&home);
        env.config_home = Some(cfg);
        let job = SideJob {
            env,
            mounts: Arc::new(crate::mounts::SystemMounts),
            dir: home.clone(),
            probe: SpaceProbe::new(Duration::from_secs(5)),
            drives: Vec::new(),
        };
        let names: Vec<String> = job.pinned().into_iter().map(|e| e.name).collect();
        assert_eq!(names, ["Home"], "a link and a file are skipped");
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
            if !probe.cache.lock().unwrap()[&(Kind::Space, p.to_path_buf())].busy {
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

    #[test]
    fn a_panicked_side_job_frees_the_gate() {
        let gate = Arc::new(Mutex::new(SideGate::default()));
        let gen = gate.lock().unwrap().start().unwrap();
        let g = gate.clone();
        let joined = std::thread::spawn(move || {
            let _guard = SideGuard::new(move || {
                g.lock().unwrap_or_else(|e| e.into_inner()).abandon(gen);
            });
            panic!("side job blew up");
        })
        .join();
        assert!(joined.is_err());
        let mut g = gate.lock().unwrap_or_else(|e| e.into_inner());
        assert!(g.start().is_some(), "busy was cleared");
    }

    #[test]
    fn a_disarmed_guard_does_nothing() {
        let hit = std::cell::Cell::new(false);
        SideGuard::new(|| hit.set(true)).disarm();
        assert!(!hit.get());
    }
}
