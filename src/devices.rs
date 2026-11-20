//! Removable drives: USB sticks, SD cards and external disks.
//!
//! The sidebar's mounted rows still come from `/proc/self/mountinfo`
//! ([`crate::mounts`]); this module adds what the mount table can't say:
//! a drive plugged in but not mounted, hotplug, and mount, unmount and
//! power-off without a password. The system's drives come from UDisks2
//! over the system bus ([`UDisks`]); tests and renders script a
//! [`FakeDevices`] instead, so plug, unplug, mount errors, read-only media
//! and a busy drive are all covered without hardware. With no UDisks2
//! (no system bus, or `ROBIN_DEVICES=none`) Robin keeps the mountinfo-only
//! sidebar: no unmounted rows, no hotplug, no eject.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{mpsc, Arc, Mutex};

/// One filesystem on a drive meant to be plugged in and out.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Drive {
    /// Stable while plugged in: the UDisks2 block object path, or the
    /// fake's own id.
    pub id: String,
    /// The UDisks2 drive object the filesystem sits on (what powers off);
    /// empty when there is none.
    pub drive: String,
    /// The filesystem label, or empty.
    pub label: String,
    /// `/dev/sdb1`.
    pub device: String,
    /// Bytes.
    pub size: u64,
    /// `vfat`, `exfat`, `ext4`…
    pub fs_type: String,
    pub mount_points: Vec<PathBuf>,
    pub read_only: bool,
    pub can_power_off: bool,
}

impl Drive {
    /// The name rows and toasts use: the label, or "29.8 GB drive".
    pub fn name(&self) -> String {
        if self.label.trim().is_empty() {
            format!("{} drive", size_text(self.size))
        } else {
            self.label.clone()
        }
    }

    pub fn mounted(&self) -> Option<&Path> {
        self.mount_points.first().map(PathBuf::as_path)
    }

    /// Whether `path` is on this drive's mounted filesystem.
    pub fn holds(&self, path: &Path) -> bool {
        self.mount_points.iter().any(|m| path.starts_with(m))
    }

    /// FAT32 can't hold a file of 4 GiB or more.
    pub fn fat(&self) -> bool {
        is_fat(&self.fs_type)
    }
}

pub fn is_fat(fs_type: &str) -> bool {
    matches!(fs_type, "vfat" | "msdos" | "fat" | "fat32")
}

/// The largest file FAT32 can store: 4 GiB less a byte.
pub const FAT_MAX_FILE: u64 = 4 * 1024 * 1024 * 1024 - 1;

/// "29.8 GB": one decimal under 100, whole numbers from there, 1000-based
/// like every other size in Robin.
pub fn size_text(n: u64) -> String {
    let units = ["B", "KB", "MB", "GB", "TB", "PB"];
    let mut v = n as f64;
    let mut u = 0;
    while v >= 1000.0 && u < units.len() - 1 {
        v /= 1000.0;
        u += 1;
    }
    if u == 0 || v >= 100.0 {
        format!("{:.0} {}", v, units[u])
    } else {
        format!("{:.1} {}", v, units[u])
    }
}

/// Why a device call didn't do what was asked.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum DeviceError {
    /// Unmount refused: something still has files open on it.
    Busy,
    /// The drive can't do this (power-off on a card reader).
    Unsupported,
    /// Polkit said no.
    NotAuthorized(String),
    /// Anything else, as UDisks2 put it.
    Failed(String),
}

impl std::fmt::Display for DeviceError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            DeviceError::Busy => write!(f, "it is in use"),
            DeviceError::Unsupported => write!(f, "the drive doesn't support it"),
            DeviceError::NotAuthorized(m) if m.is_empty() => write!(f, "not allowed"),
            DeviceError::NotAuthorized(m) | DeviceError::Failed(m) => write!(f, "{m}"),
        }
    }
}

/// Where drives come from. Every call may block (D-Bus, the kernel), so
/// the window calls these on a worker thread only.
pub trait DeviceBackend: Send + Sync {
    /// The removable drives' filesystems plugged in right now.
    fn drives(&self) -> Result<Vec<Drive>, DeviceError>;
    /// Mount without a password; returns where.
    fn mount(&self, id: &str) -> Result<PathBuf, DeviceError>;
    fn unmount(&self, id: &str, force: bool) -> Result<(), DeviceError>;
    /// Power the drive off so it can be pulled out.
    fn power_off(&self, id: &str) -> Result<(), DeviceError>;
    /// A channel that receives `()` whenever a drive may have arrived,
    /// left or changed. `None`: no hotplug, the list is read once.
    fn watch(&self) -> Option<mpsc::Receiver<()>>;
    /// The program keeping `mount` busy, for the busy toast.
    fn holder(&self, mount: &Path) -> Option<String> {
        holder(mount)
    }
}

/// How an eject ended.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Ejected {
    /// Unmounted and powered off: the drive is gone from the list.
    PoweredOff,
    /// Unmounted; the drive can't power off (a card reader), but it is
    /// safe to pull.
    Unmounted,
}

/// Unmount every mount of `id`, then power the drive off. A drive that
/// can't power off is still safe to unplug once unmounted.
pub fn eject(
    backend: &dyn DeviceBackend,
    drive: &Drive,
    force: bool,
) -> Result<Ejected, DeviceError> {
    if drive.mounted().is_some() {
        backend.unmount(&drive.id, force)?;
    }
    if !drive.can_power_off {
        return Ok(Ejected::Unmounted);
    }
    match backend.power_off(&drive.id) {
        Ok(()) => Ok(Ejected::PoweredOff),
        Err(DeviceError::Unsupported) => Ok(Ejected::Unmounted),
        Err(e) => Err(e),
    }
}

/// Drives in `now` that weren't in `prev`. With no `prev` (the first
/// listing, at startup) nothing counts as arriving: a drive already
/// plugged in when Robin starts gets no toast.
pub fn arrivals<'a>(prev: Option<&[Drive]>, now: &'a [Drive]) -> Vec<&'a Drive> {
    let Some(prev) = prev else {
        return Vec::new();
    };
    now.iter()
        .filter(|d| !prev.iter().any(|p| p.id == d.id))
        .collect()
}

/// A mount that went away between two listings: the drive as it was,
/// where it was mounted, and whether the drive is still plugged in
/// (unmounted by something else) or gone (pulled out).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Lost {
    pub drive: Drive,
    pub mount: PathBuf,
    pub still_plugged: bool,
}

pub fn lost_mounts(prev: &[Drive], now: &[Drive]) -> Vec<Lost> {
    let mut out = Vec::new();
    for p in prev {
        let after = now.iter().find(|d| d.id == p.id);
        for m in &p.mount_points {
            if after.is_some_and(|d| d.mount_points.contains(m)) {
                continue;
            }
            out.push(Lost {
                drive: p.clone(),
                mount: m.clone(),
                still_plugged: after.is_some(),
            });
        }
    }
    out
}

/// The program holding files open on `mount`, by `/proc` scan: the first
/// other process with a working directory or an open file under it.
/// `None` when none is visible (another user's, or the kernel's).
pub fn holder(mount: &Path) -> Option<String> {
    holder_in(Path::new("/proc"), mount, std::process::id())
}

pub fn holder_in(proc_root: &Path, mount: &Path, me: u32) -> Option<String> {
    let mut pids: Vec<u32> = std::fs::read_dir(proc_root)
        .ok()?
        .flatten()
        .filter_map(|e| e.file_name().to_str()?.parse().ok())
        .filter(|pid| *pid != me)
        .collect();
    pids.sort_unstable();
    pids.into_iter().find_map(|pid| {
        let dir = proc_root.join(pid.to_string());
        let on = |link: &Path| std::fs::read_link(link).is_ok_and(|t| t.starts_with(mount));
        let held = on(&dir.join("cwd"))
            || std::fs::read_dir(dir.join("fd"))
                .map(|fds| fds.flatten().any(|fd| on(&fd.path())))
                .unwrap_or(false);
        if !held {
            return None;
        }
        let comm = std::fs::read_to_string(dir.join("comm")).ok()?;
        let comm = comm.trim();
        (!comm.is_empty()).then(|| comm.to_string())
    })
}

/// Why a copy onto a drive shouldn't start.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Preflight {
    /// The items need more than the drive has free.
    NoSpace { need: u64, free: u64 },
    /// Files of 4 GiB or more going onto FAT32, with their sizes.
    TooBig(Vec<(String, u64)>),
}

/// Check a copy onto a drive before it starts: the bytes it will write
/// against the free space, and on FAT32 every file against the 4 GiB
/// limit. Symlinks are copied as links and count as nothing.
pub fn preflight(
    src: &Path,
    names: &[std::ffi::OsString],
    free: u64,
    fat: bool,
) -> Option<Preflight> {
    let mut need = 0u64;
    let mut big = Vec::new();
    let mut stack: Vec<(PathBuf, String)> = names
        .iter()
        .map(|n| (src.join(n), n.to_string_lossy().into_owned()))
        .collect();
    while let Some((path, shown)) = stack.pop() {
        let Ok(meta) = path.symlink_metadata() else {
            continue;
        };
        if meta.is_dir() {
            if let Ok(rd) = std::fs::read_dir(&path) {
                for e in rd.flatten() {
                    let n = e.file_name().to_string_lossy().into_owned();
                    stack.push((e.path(), format!("{shown}/{n}")));
                }
            }
        } else if meta.is_file() {
            need = need.saturating_add(meta.len());
            if fat && meta.len() > FAT_MAX_FILE {
                big.push((shown, meta.len()));
            }
        }
    }
    if !big.is_empty() {
        big.sort();
        return Some(Preflight::TooBig(big));
    }
    (need > free).then_some(Preflight::NoSpace { need, free })
}

/// The backend the app uses: UDisks2 on the system bus, unless
/// `ROBIN_DEVICES` says `none` (mountinfo only) or `fake` (a scripted
/// SANDISK stick that plugs in two seconds after launch, for trying the
/// device states without hardware).
pub fn from_env() -> Option<Arc<dyn DeviceBackend>> {
    match std::env::var("ROBIN_DEVICES").as_deref() {
        Ok("none") => None,
        Ok("fake") => {
            let fake = FakeDevices::new();
            let base = std::env::var_os("XDG_RUNTIME_DIR")
                .map(PathBuf::from)
                .filter(|p| p.is_absolute())
                .unwrap_or_else(std::env::temp_dir)
                .join("robin-fake-devices");
            let plug = fake.clone();
            std::thread::spawn(move || {
                std::thread::sleep(std::time::Duration::from_secs(2));
                plug.plug(FakeDrive::stick(
                    "fake-sandisk",
                    "SANDISK",
                    29_800_000_000,
                    &base.join("SANDISK"),
                ));
            });
            Some(fake)
        }
        _ => UDisks::connect()
            .ok()
            .map(|u| Arc::new(u) as Arc<dyn DeviceBackend>),
    }
}

// ---- UDisks2 --------------------------------------------------------------

const UDISKS: &str = "org.freedesktop.UDisks2";
const BLOCK: &str = "org.freedesktop.UDisks2.Block";
const FILESYSTEM: &str = "org.freedesktop.UDisks2.Filesystem";
const DRIVE: &str = "org.freedesktop.UDisks2.Drive";

type Props = HashMap<String, zbus::zvariant::OwnedValue>;
type Managed = HashMap<zbus::zvariant::OwnedObjectPath, HashMap<String, Props>>;

/// UDisks2 over the system bus, with blocking calls.
pub struct UDisks {
    conn: zbus::blocking::Connection,
}

fn prop<T: TryFrom<zbus::zvariant::OwnedValue>>(props: &Props, key: &str) -> Option<T> {
    let v = props.get(key)?.try_clone().ok()?;
    T::try_from(v).ok()
}

fn bytes_path(b: &[u8]) -> PathBuf {
    use std::os::unix::ffi::OsStrExt;
    let end = b.iter().position(|c| *c == 0).unwrap_or(b.len());
    PathBuf::from(std::ffi::OsStr::from_bytes(&b[..end]))
}

fn device_error(e: zbus::Error) -> DeviceError {
    match e {
        zbus::Error::MethodError(name, detail, _) => {
            let detail = detail.unwrap_or_default();
            let n = name.as_str();
            if n.ends_with(".DeviceBusy") {
                DeviceError::Busy
            } else if n.ends_with(".NotAuthorized")
                || n.ends_with(".NotAuthorizedCanObtain")
                || n.ends_with(".NotAuthorizedDismissed")
            {
                DeviceError::NotAuthorized(detail)
            } else if n.ends_with(".NotSupported") {
                DeviceError::Unsupported
            } else if detail.is_empty() {
                DeviceError::Failed(n.to_string())
            } else {
                DeviceError::Failed(detail)
            }
        }
        other => DeviceError::Failed(other.to_string()),
    }
}

/// Turn UDisks2's object tree into drives: filesystems on a block that
/// isn't hidden, on a drive that is removable or external.
pub fn drives_from_managed(objects: &Managed) -> Vec<Drive> {
    let mut out = Vec::new();
    for (path, ifaces) in objects {
        let (Some(block), Some(fs)) = (ifaces.get(BLOCK), ifaces.get(FILESYSTEM)) else {
            continue;
        };
        if prop::<bool>(block, "HintIgnore").unwrap_or(false) {
            continue;
        }
        let drive_path = prop::<zbus::zvariant::OwnedObjectPath>(block, "Drive")
            .map(|p| p.as_str().to_string())
            .filter(|p| p != "/")
            .unwrap_or_default();
        let drive = objects
            .iter()
            .find(|(p, _)| p.as_str() == drive_path)
            .and_then(|(_, i)| i.get(DRIVE));
        let removable = drive.is_some_and(|d| {
            prop::<bool>(d, "Removable").unwrap_or(false)
                || prop::<bool>(d, "MediaRemovable").unwrap_or(false)
                || matches!(
                    prop::<String>(d, "ConnectionBus").as_deref(),
                    Some("usb" | "sdio" | "ieee1394")
                )
        });
        if prop::<bool>(block, "HintSystem").unwrap_or(true) && !removable {
            continue;
        }
        let mount_points = prop::<Vec<Vec<u8>>>(fs, "MountPoints")
            .unwrap_or_default()
            .iter()
            .map(|b| bytes_path(b))
            .collect();
        out.push(Drive {
            id: path.as_str().to_string(),
            label: prop::<String>(block, "IdLabel").unwrap_or_default(),
            device: bytes_path(&prop::<Vec<u8>>(block, "Device").unwrap_or_default())
                .display()
                .to_string(),
            size: prop::<u64>(block, "Size").unwrap_or(0),
            fs_type: prop::<String>(block, "IdType").unwrap_or_default(),
            mount_points,
            read_only: prop::<bool>(block, "ReadOnly").unwrap_or(false),
            can_power_off: drive.is_some_and(|d| prop::<bool>(d, "CanPowerOff").unwrap_or(false)),
            drive: drive_path,
        });
    }
    out.sort_by(|a, b| a.device.cmp(&b.device));
    out
}

impl UDisks {
    /// Connect and check UDisks2 answers; an error means fall back.
    pub fn connect() -> Result<Self, DeviceError> {
        let conn = zbus::blocking::Connection::system().map_err(device_error)?;
        let u = Self { conn };
        u.managed()?;
        Ok(u)
    }

    fn managed(&self) -> Result<Managed, DeviceError> {
        let reply = self
            .conn
            .call_method(
                Some(UDISKS),
                "/org/freedesktop/UDisks2",
                Some("org.freedesktop.DBus.ObjectManager"),
                "GetManagedObjects",
                &(),
            )
            .map_err(device_error)?;
        reply.body().deserialize::<Managed>().map_err(device_error)
    }

    fn call(
        &self,
        path: &str,
        iface: &str,
        method: &str,
        force: bool,
    ) -> Result<zbus::Message, DeviceError> {
        let mut opts: HashMap<&str, zbus::zvariant::Value<'_>> = HashMap::new();
        if force {
            opts.insert("force", true.into());
        }
        let path = zbus::zvariant::ObjectPath::try_from(path)
            .map_err(|e| DeviceError::Failed(e.to_string()))?;
        self.conn
            .call_method(Some(UDISKS), path, Some(iface), method, &(opts,))
            .map_err(device_error)
    }

    fn drive_of(&self, id: &str) -> Result<String, DeviceError> {
        self.drives()?
            .into_iter()
            .find(|d| d.id == id)
            .map(|d| d.drive)
            .filter(|d| !d.is_empty())
            .ok_or(DeviceError::Unsupported)
    }
}

impl DeviceBackend for UDisks {
    fn drives(&self) -> Result<Vec<Drive>, DeviceError> {
        Ok(drives_from_managed(&self.managed()?))
    }

    fn mount(&self, id: &str) -> Result<PathBuf, DeviceError> {
        match self.call(id, FILESYSTEM, "Mount", false) {
            Ok(reply) => reply
                .body()
                .deserialize::<String>()
                .map(PathBuf::from)
                .map_err(|e| DeviceError::Failed(e.to_string())),
            // Mounted meanwhile (a desktop automounter): use that mount.
            Err(DeviceError::Failed(m)) if m.contains("already mounted") => self
                .drives()?
                .into_iter()
                .find(|d| d.id == id)
                .and_then(|d| d.mounted().map(Path::to_path_buf))
                .ok_or(DeviceError::Failed(m)),
            Err(e) => Err(e),
        }
    }

    fn unmount(&self, id: &str, force: bool) -> Result<(), DeviceError> {
        self.call(id, FILESYSTEM, "Unmount", force).map(|_| ())
    }

    fn power_off(&self, id: &str) -> Result<(), DeviceError> {
        let drive = self.drive_of(id)?;
        self.call(&drive, DRIVE, "PowerOff", false).map(|_| ())
    }

    fn watch(&self) -> Option<mpsc::Receiver<()>> {
        let rule = zbus::MatchRule::builder()
            .msg_type(zbus::message::Type::Signal)
            .sender(UDISKS)
            .ok()?
            .build();
        let iter =
            zbus::blocking::MessageIterator::for_match_rule(rule, &self.conn, Some(64)).ok()?;
        let (tx, rx) = mpsc::channel();
        std::thread::spawn(move || {
            for msg in iter {
                if msg.is_ok() && tx.send(()).is_err() {
                    break;
                }
            }
        });
        Some(rx)
    }
}

// ---- Fake -----------------------------------------------------------------

/// One scripted drive and how it behaves.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FakeDrive {
    pub drive: Drive,
    /// Where a mount puts it; created on mount.
    pub mount_at: PathBuf,
    /// A mount fails with this message.
    pub mount_error: Option<String>,
    /// Mounts read-only (write-protected card, or a dirty filesystem).
    pub read_only: bool,
    /// Unmount refuses with Busy unless forced.
    pub busy: bool,
}

impl FakeDrive {
    /// An exFAT USB stick, unmounted, that powers off.
    pub fn stick(id: &str, label: &str, size: u64, mount_at: &Path) -> Self {
        Self {
            drive: Drive {
                id: id.into(),
                drive: format!("{id}-drive"),
                label: label.into(),
                device: format!("/dev/fake-{id}"),
                size,
                fs_type: "exfat".into(),
                mount_points: Vec::new(),
                read_only: false,
                can_power_off: true,
            },
            mount_at: mount_at.to_path_buf(),
            mount_error: None,
            read_only: false,
            busy: false,
        }
    }
}

/// Drives a test or render plugs in and pulls out by hand. Every change
/// pings the watchers, as UDisks2's signals would.
#[derive(Default)]
pub struct FakeDevices {
    drives: Mutex<Vec<FakeDrive>>,
    subs: Mutex<Vec<mpsc::Sender<()>>>,
    /// Every call made, in order: `mount id`, `unmount id force`, …
    pub log: Mutex<Vec<String>>,
    /// Who the busy toast names; the `/proc` scan when unset.
    pub holder: Mutex<Option<String>>,
}

impl FakeDevices {
    pub fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Vec<FakeDrive>> {
        self.drives.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn notify(&self) {
        self.subs
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .retain(|tx| tx.send(()).is_ok());
    }

    fn note(&self, line: String) {
        self.log
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .push(line);
    }

    pub fn plug(&self, d: FakeDrive) {
        self.lock().push(d);
        self.notify();
    }

    /// Pulled out, mounted or not.
    pub fn unplug(&self, id: &str) {
        self.lock().retain(|d| d.drive.id != id);
        self.notify();
    }

    /// Something else unmounted it; it stays plugged in.
    pub fn unmount_elsewhere(&self, id: &str) {
        for d in self.lock().iter_mut().filter(|d| d.drive.id == id) {
            d.drive.mount_points.clear();
        }
        self.notify();
    }

    pub fn edit(&self, id: &str, f: impl FnOnce(&mut FakeDrive)) {
        if let Some(d) = self.lock().iter_mut().find(|d| d.drive.id == id) {
            f(d);
        }
        self.notify();
    }

    pub fn get(&self, id: &str) -> Option<FakeDrive> {
        self.lock().iter().find(|d| d.drive.id == id).cloned()
    }
}

impl DeviceBackend for FakeDevices {
    fn drives(&self) -> Result<Vec<Drive>, DeviceError> {
        Ok(self.lock().iter().map(|d| d.drive.clone()).collect())
    }

    fn mount(&self, id: &str) -> Result<PathBuf, DeviceError> {
        self.note(format!("mount {id}"));
        let at = {
            let mut drives = self.lock();
            let d = drives
                .iter_mut()
                .find(|d| d.drive.id == id)
                .ok_or_else(|| DeviceError::Failed("No such device".into()))?;
            if let Some(m) = &d.mount_error {
                return Err(DeviceError::Failed(m.clone()));
            }
            if let Some(m) = d.drive.mounted() {
                return Ok(m.to_path_buf());
            }
            std::fs::create_dir_all(&d.mount_at).map_err(|e| DeviceError::Failed(e.to_string()))?;
            d.drive.mount_points = vec![d.mount_at.clone()];
            d.drive.read_only = d.read_only;
            d.mount_at.clone()
        };
        self.notify();
        Ok(at)
    }

    fn unmount(&self, id: &str, force: bool) -> Result<(), DeviceError> {
        self.note(format!("unmount {id}{}", if force { " force" } else { "" }));
        {
            let mut drives = self.lock();
            let d = drives
                .iter_mut()
                .find(|d| d.drive.id == id)
                .ok_or_else(|| DeviceError::Failed("No such device".into()))?;
            if d.busy && !force {
                return Err(DeviceError::Busy);
            }
            d.drive.mount_points.clear();
            d.busy = false;
        }
        self.notify();
        Ok(())
    }

    fn power_off(&self, id: &str) -> Result<(), DeviceError> {
        self.note(format!("power-off {id}"));
        let can = self.get(id).is_some_and(|d| d.drive.can_power_off);
        if !can {
            return Err(DeviceError::Unsupported);
        }
        self.unplug(id);
        Ok(())
    }

    fn watch(&self) -> Option<mpsc::Receiver<()>> {
        let (tx, rx) = mpsc::channel();
        self.subs.lock().unwrap_or_else(|e| e.into_inner()).push(tx);
        Some(rx)
    }

    fn holder(&self, mount: &Path) -> Option<String> {
        let set = self
            .holder
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone();
        set.or_else(|| holder(mount))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("robin-devices-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    fn stick(root: &Path) -> FakeDrive {
        FakeDrive::stick("sdb1", "SANDISK", 29_800_000_000, &root.join("SANDISK"))
    }

    #[test]
    fn plug_and_unplug_ping_the_watchers_and_change_the_list() {
        let root = tmp("plug");
        let fake = FakeDevices::new();
        let rx = fake.watch().unwrap();
        assert!(fake.drives().unwrap().is_empty());
        fake.plug(stick(&root));
        rx.try_recv().unwrap();
        let now = fake.drives().unwrap();
        assert_eq!(now.len(), 1);
        assert_eq!(now[0].name(), "SANDISK");
        assert!(now[0].mounted().is_none(), "plugged in, not mounted");
        fake.unplug("sdb1");
        rx.try_recv().unwrap();
        assert!(fake.drives().unwrap().is_empty());
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn startup_drives_never_count_as_arriving() {
        let root = tmp("arrive");
        let a = stick(&root).drive;
        let mut b = a.clone();
        b.id = "sdc1".into();
        assert!(arrivals(None, std::slice::from_ref(&a)).is_empty());
        let prev = [a.clone()];
        let now = [a.clone(), b.clone()];
        let got: Vec<_> = arrivals(Some(&prev), &now)
            .iter()
            .map(|d| d.id.clone())
            .collect();
        assert_eq!(got, ["sdc1"]);
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn mounting_succeeds_fails_or_comes_up_read_only() {
        let root = tmp("mount");
        let fake = FakeDevices::new();
        fake.plug(stick(&root));
        let at = fake.mount("sdb1").unwrap();
        assert_eq!(at, root.join("SANDISK"));
        assert!(at.is_dir());
        assert_eq!(fake.drives().unwrap()[0].mounted(), Some(at.as_path()));
        fake.unmount("sdb1", false).unwrap();

        fake.edit("sdb1", |d| {
            d.mount_error = Some("Filesystem type ntfs3 not supported".into())
        });
        assert_eq!(
            fake.mount("sdb1"),
            Err(DeviceError::Failed(
                "Filesystem type ntfs3 not supported".into()
            ))
        );

        fake.edit("sdb1", |d| {
            d.mount_error = None;
            d.read_only = true;
        });
        fake.mount("sdb1").unwrap();
        assert!(fake.drives().unwrap()[0].read_only);
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn a_busy_drive_refuses_to_unmount_unless_forced() {
        let root = tmp("busy");
        let fake = FakeDevices::new();
        fake.plug(stick(&root));
        fake.mount("sdb1").unwrap();
        fake.edit("sdb1", |d| d.busy = true);
        let drive = fake.drives().unwrap()[0].clone();
        assert_eq!(eject(fake.as_ref(), &drive, false), Err(DeviceError::Busy));
        assert!(
            fake.drives().unwrap()[0].mounted().is_some(),
            "still mounted"
        );
        assert_eq!(eject(fake.as_ref(), &drive, true), Ok(Ejected::PoweredOff));
        assert!(fake.drives().unwrap().is_empty(), "powered off and gone");
        let log = fake.log.lock().unwrap().clone();
        assert_eq!(
            log,
            [
                "mount sdb1",
                "unmount sdb1",
                "unmount sdb1 force",
                "power-off sdb1"
            ]
        );
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn a_drive_that_cannot_power_off_is_still_safe_once_unmounted() {
        let root = tmp("nopower");
        let fake = FakeDevices::new();
        let mut card = stick(&root);
        card.drive.can_power_off = false;
        fake.plug(card);
        fake.mount("sdb1").unwrap();
        let drive = fake.drives().unwrap()[0].clone();
        assert_eq!(eject(fake.as_ref(), &drive, false), Ok(Ejected::Unmounted));
        let after = fake.drives().unwrap();
        assert_eq!(after.len(), 1, "the reader stays");
        assert!(after[0].mounted().is_none());
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn lost_mounts_tell_a_pulled_drive_from_an_unmounted_one() {
        let root = tmp("lost");
        let mut a = stick(&root).drive;
        a.mount_points = vec![root.join("SANDISK")];
        let mut unmounted = a.clone();
        unmounted.mount_points.clear();
        let lost = lost_mounts(std::slice::from_ref(&a), std::slice::from_ref(&unmounted));
        assert_eq!(lost.len(), 1);
        assert!(lost[0].still_plugged);
        let pulled = lost_mounts(std::slice::from_ref(&a), &[]);
        assert!(!pulled[0].still_plugged);
        assert_eq!(pulled[0].mount, root.join("SANDISK"));
        assert!(lost_mounts(std::slice::from_ref(&a), std::slice::from_ref(&a)).is_empty());
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn the_holder_is_found_by_its_working_directory() {
        let root = tmp("holder");
        let mut child = std::process::Command::new("sleep")
            .arg("30")
            .current_dir(&root)
            .spawn()
            .unwrap();
        let got = holder(&root);
        child.kill().unwrap();
        let _ = child.wait();
        assert_eq!(got.as_deref(), Some("sleep"));
        assert_eq!(holder(&root.join("nothing-here")), None);
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn preflight_checks_free_space_and_the_fat_file_limit() {
        let root = tmp("preflight");
        std::fs::create_dir_all(root.join("album")).unwrap();
        std::fs::write(root.join("album/a.jpg"), vec![0u8; 3000]).unwrap();
        std::fs::write(root.join("b.mp4"), vec![0u8; 2000]).unwrap();
        // A sparse file past 4 GiB costs no disk.
        let big = std::fs::File::create(root.join("holo.img")).unwrap();
        big.set_len(FAT_MAX_FILE + 1).unwrap();
        let names = |n: &[&str]| n.iter().map(std::ffi::OsString::from).collect::<Vec<_>>();
        assert_eq!(
            preflight(&root, &names(&["album", "b.mp4"]), 10_000, true),
            None
        );
        assert_eq!(
            preflight(&root, &names(&["album", "b.mp4"]), 4_000, false),
            Some(Preflight::NoSpace {
                need: 5000,
                free: 4000
            })
        );
        assert_eq!(
            preflight(&root, &names(&["holo.img", "b.mp4"]), u64::MAX, true),
            Some(Preflight::TooBig(vec![(
                "holo.img".into(),
                FAT_MAX_FILE + 1
            )]))
        );
        assert_eq!(
            preflight(&root, &names(&["holo.img"]), u64::MAX, false),
            None,
            "exFAT holds it"
        );
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn names_and_sizes_read_like_the_spec() {
        assert_eq!(size_text(29_800_000_000), "29.8 GB");
        assert_eq!(size_text(2_000_000_000_000), "2.0 TB");
        assert_eq!(size_text(128_000_000_000), "128 GB");
        let mut d = FakeDrive::stick("x", "", 15_500_000_000, Path::new("/x")).drive;
        assert_eq!(d.name(), "15.5 GB drive");
        d.fs_type = "vfat".into();
        assert!(d.fat());
    }

    #[test]
    fn udisks_errors_map_to_what_the_toast_says() {
        let err = |name: &str, detail: &str| {
            device_error(zbus::Error::MethodError(
                zbus::names::OwnedErrorName::try_from(name.to_string()).unwrap(),
                Some(detail.into()),
                zbus::message::Message::method_call("/", "X")
                    .unwrap()
                    .build(&())
                    .unwrap(),
            ))
        };
        assert_eq!(
            err("org.freedesktop.UDisks2.Error.DeviceBusy", "busy"),
            DeviceError::Busy
        );
        assert_eq!(
            err(
                "org.freedesktop.UDisks2.Error.NotAuthorizedCanObtain",
                "auth"
            ),
            DeviceError::NotAuthorized("auth".into())
        );
        assert_eq!(
            err("org.freedesktop.UDisks2.Error.Failed", "wrong fs"),
            DeviceError::Failed("wrong fs".into())
        );
    }
}
