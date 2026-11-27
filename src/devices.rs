//! Removable drives (USB sticks, SD cards, external disks) and, since
//! phase 2, phones and cameras over USB.
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
//!
//! Phones (MTP) and cameras (PTP) come from gvfs's volume monitors on the
//! session bus ([`crate::gvfs::Gvfs`]); a mounted one is an ordinary folder
//! under `/run/user/<uid>/gvfs/`. [`Combined`] puts both sources behind
//! one backend. Without gvfs (no session bus, or no monitor answering)
//! DEVICES is exactly phase 1's.
//!
//! Encrypted (LUKS) partitions are listed while locked, with the lock
//! glyph. [`DeviceBackend::unlock`] opens one with a [`Passphrase`] that
//! never outlives the call, and eject locks it again before the drive
//! powers off. Polkit asks through UDisks2's own agent, never `pkexec`.

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
    /// A drive, or a phone or camera reached through gvfs.
    pub kind: Kind,
    /// A phone that is mounted but hasn't given Robin its files yet: it is
    /// locked, or "Allow" hasn't been tapped. Its top folder lists nothing.
    pub waiting: bool,
    /// Folders on it that take no new files, each with the reason the UI
    /// gives (a fake's scripted refusals; real phones use [`limits`]'s
    /// rules).
    pub no_write: Vec<(PathBuf, String)>,
    /// Folders on it whose items can't be renamed, each with the reason
    /// (a fake's scripted refusals, as some MTP folders refuse).
    pub no_rename: Vec<(PathBuf, String)>,
    /// An encrypted partition nobody has unlocked: there is no
    /// filesystem to mount yet, and a click asks for its passphrase.
    /// `id` is the encrypted block itself.
    pub locked: bool,
    /// The encrypted partition this filesystem was unlocked from (its
    /// UDisks2 block, or the fake's id); empty for a plain filesystem.
    /// Eject locks it again after unmounting.
    pub crypto: String,
}

/// nf-md-lock U+F033E, checked against the bundled font's cmap by a
/// test: the glyph of a locked row and of the Unlock dialog.
pub const LOCK_GLYPH: &str = "\u{f033e}";

/// nf-md-lock-open U+F033F: the row of an encrypted partition that is
/// unlocked, mounted or not.
pub const UNLOCKED_GLYPH: &str = "\u{f033f}";

/// nf-md-eye U+F0208 and nf-md-eye-off U+F0209: the Unlock field's
/// show / hide toggle (drawn by ui/app.slint; a test ties the two).
pub const EYE_GLYPH: &str = "\u{f0208}";
pub const EYE_OFF_GLYPH: &str = "\u{f0209}";

/// What a device row stands for.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Kind {
    /// A USB stick, SD card or disk, from UDisks2.
    #[default]
    Drive,
    /// An Android phone over MTP.
    Phone,
    /// A camera (or a phone in photo mode) over PTP.
    Camera,
}

impl Kind {
    /// The row's glyph from the bundled Nerd Font: nf-md-usb U+F0553,
    /// nf-md-cellphone U+F011C, nf-md-camera U+F0100.
    pub fn glyph(self) -> &'static str {
        match self {
            Kind::Drive => "\u{f0553}",
            Kind::Phone => "\u{f011c}",
            Kind::Camera => "\u{f0100}",
        }
    }

    /// Phones and cameras: every call crosses USB one object at a time,
    /// so counts, thumbnails and recursive sizes are skipped.
    pub fn slow(self) -> bool {
        self != Kind::Drive
    }
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

    /// A phone or camera rather than a drive.
    pub fn media(&self) -> bool {
        self.kind.slow()
    }

    /// The id eject and power-off can still reach once this filesystem
    /// is locked away: the encrypted partition under it, or itself.
    pub fn stable_id(&self) -> &str {
        if self.crypto.is_empty() {
            &self.id
        } else {
            &self.crypto
        }
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
    /// A phone that won't open: locked, or file access not allowed yet.
    Locked,
    /// An encrypted partition refused the passphrase.
    WrongPassphrase,
    /// Anything else, as UDisks2 put it.
    Failed(String),
}

impl std::fmt::Display for DeviceError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            DeviceError::Busy => write!(f, "it is in use"),
            DeviceError::Unsupported => write!(f, "the drive doesn't support it"),
            DeviceError::Locked => write!(f, "unlock it and allow file access"),
            DeviceError::WrongPassphrase => write!(f, "wrong passphrase"),
            DeviceError::NotAuthorized(m) if m.is_empty() => write!(f, "not allowed"),
            DeviceError::NotAuthorized(m) | DeviceError::Failed(m) => write!(f, "{m}"),
        }
    }
}

/// A passphrase on its way to an unlock: held only for that call, never
/// cloned, printed or logged (`Debug` says `<redacted>`), and zeroed when
/// dropped. It is never put in a process's argv: UDisks2 gets it over
/// D-Bus.
pub struct Passphrase(String);

impl Passphrase {
    /// Copy `typed` into a buffer of exactly its size, so the copy is
    /// never reallocated and left behind unzeroed.
    pub fn new(typed: &str) -> Self {
        let mut s = String::with_capacity(typed.len());
        s.push_str(typed);
        Self(s)
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    /// The passphrase itself, for the one call that needs it.
    pub fn expose(&self) -> &str {
        &self.0
    }
}

impl Passphrase {
    /// Overwrite the buffer with zeros, in place.
    fn wipe(&mut self) {
        // SAFETY: zero bytes are valid UTF-8, so the String stays valid.
        let bytes = unsafe { self.0.as_bytes_mut() };
        for b in bytes.iter_mut() {
            // Volatile, so the zeroing isn't optimised away as a dead
            // store before the free. SAFETY: `b` is a live, aligned &mut u8.
            unsafe { std::ptr::write_volatile(b, 0) };
        }
        std::sync::atomic::compiler_fence(std::sync::atomic::Ordering::SeqCst);
    }
}

impl Drop for Passphrase {
    fn drop(&mut self) {
        self.wipe();
    }
}

impl std::fmt::Debug for Passphrase {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Passphrase(<redacted>)")
    }
}

/// What the Unlock dialog says under the field when an unlock fails,
/// in plain words rather than UDisks2's ("Error unlocking /dev/sdb1:
/// Failed to activate device: …"). It never contains the passphrase:
/// the error UDisks2 sends doesn't either.
///
/// Only LUKS gets "Wrong passphrase". A TrueCrypt volume that refuses
/// the key may want a PIM, a hidden volume or a keyfile this dialog
/// can't give, so it says so; BitLocker (and anything else) gets a
/// neutral line.
pub fn unlock_error_text(name: &str, e: &DeviceError, crypto: Crypto) -> String {
    match e {
        DeviceError::WrongPassphrase => match crypto {
            Crypto::Luks => "Wrong passphrase. Try again.".into(),
            Crypto::TrueCrypt => format!(
                "Couldn't unlock {name}. This TrueCrypt volume may need options Robin doesn't support."
            ),
            Crypto::Other => format!("Couldn't unlock {name} with this passphrase"),
        },
        DeviceError::NotAuthorized(m) if m.to_lowercase().contains("dismissed") => {
            "Authentication was cancelled".into()
        }
        DeviceError::NotAuthorized(_) => format!("You aren't allowed to unlock {name}"),
        DeviceError::Busy => format!("{name} is busy. Try again in a moment"),
        DeviceError::Unsupported => format!("{name} can't be unlocked here"),
        DeviceError::Locked => format!("Couldn't unlock {name}"),
        DeviceError::Failed(m) => {
            let low = m.to_lowercase();
            if low.contains("no such device")
                || low.contains("unknownobject")
                || low.contains("doesn't exist")
                || low.contains("does not exist")
            {
                format!("{name} was removed")
            } else if low.contains("cannot allocate memory") || low.contains("out of memory") {
                format!("Not enough free memory to unlock {name}")
            } else if low.contains("unsupported luks2 requirements") {
                format!("{name} uses encryption features this system can't open")
            } else if low.contains("does not appear to be") {
                format!("{name} isn't an encrypted volume Robin can open")
            } else if low.contains("no key available") {
                "Enter the passphrase".into()
            } else {
                format!("Couldn't unlock {name}: {}", plain_reason(m))
            }
        }
    }
}

/// Which kind of encrypted volume a locked block is, by its IdType; it
/// decides what a refused key says.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Crypto {
    /// "crypto_LUKS", LUKS 1 or 2.
    #[default]
    Luks,
    /// "crypto_TCRYPT", TrueCrypt or VeraCrypt.
    TrueCrypt,
    /// "BitLocker", or a type UDisks2 has no name for.
    Other,
}

impl Crypto {
    pub fn of(id_type: &str) -> Self {
        match id_type {
            "crypto_LUKS" => Crypto::Luks,
            "crypto_TCRYPT" => Crypto::TrueCrypt,
            _ => Crypto::Other,
        }
    }
}

/// UDisks2's message less its "Error unlocking /dev/sdb1: " and
/// "Failed to activate device: " prefixes, without a trailing dot.
fn plain_reason(m: &str) -> String {
    let mut r = m.trim();
    if let Some(rest) = r.strip_prefix("Error unlocking ") {
        r = rest.split_once(": ").map_or(rest, |(_, t)| t);
    }
    if let Some(rest) = r.strip_prefix("Failed to activate device: ") {
        r = rest;
    }
    let r = r.trim().trim_end_matches('.');
    if r.is_empty() {
        "UDisks2 gave no reason".into()
    } else {
        r.to_string()
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
    /// Unlock the encrypted partition `id` (UDisks2 `Encrypted.Unlock`);
    /// returns the id of the filesystem it opens to, for [`mount`].
    /// Polkit, if it asks, asks through UDisks2's own agent.
    ///
    /// [`mount`]: DeviceBackend::mount
    fn unlock(&self, _id: &str, _passphrase: &Passphrase) -> Result<String, DeviceError> {
        Err(DeviceError::Unsupported)
    }
    /// Lock the encrypted partition `id` again (`Encrypted.Lock`); its
    /// filesystem must be unmounted first.
    fn lock(&self, _id: &str) -> Result<(), DeviceError> {
        Err(DeviceError::Unsupported)
    }
    /// A channel that receives `()` whenever a drive may have arrived,
    /// left or changed. `None`: no hotplug, the list is read once.
    fn watch(&self) -> Option<mpsc::Receiver<()>>;
    /// The program keeping `mount` busy, for the busy toast.
    fn holder(&self, mount: &Path) -> Option<String> {
        holder(mount)
    }
    /// Whether `id` is one of this backend's, for [`Combined`] to route.
    fn owns(&self, _id: &str) -> bool {
        true
    }
    /// [Retry] on a phone waiting for access: look again, remounting if
    /// it still shows nothing; returns where it is mounted.
    fn retry(&self, id: &str) -> Result<PathBuf, DeviceError> {
        self.mount(id)
    }
    /// Called on the reading thread before a folder on a slow device is
    /// listed. Real devices are slow by themselves; the fake sleeps here
    /// to script a slow phone.
    fn pace(&self, _dir: &Path) {}
}

/// What can't be done in a folder on a phone or camera, and why: the UI
/// greys these out (or refuses up front) instead of failing part-way.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Limits {
    /// Nothing can be created, pasted or dropped here.
    pub write: Option<String>,
    /// What is here can't be deleted (a phone's storages).
    pub remove: Option<String>,
    /// There is no trash: Del offers Delete permanently.
    pub no_trash: bool,
    /// Every read crosses USB: skip folder counts, recursive sizes and
    /// thumbnails, and copy moves instead of renaming them (MTP may have
    /// no move).
    pub slow: bool,
    /// What is here can't be renamed: a phone's storages, a folder the
    /// phone refuses renames in, or anything that takes no writes.
    pub rename: Option<String>,
    /// The drive is mounted read-only: nothing here can change.
    pub read_only: Option<String>,
}

/// The device `dir` is on (the deepest mount that holds it), if any.
pub fn device_of<'a>(drives: &'a [Drive], dir: &Path) -> Option<&'a Drive> {
    drives
        .iter()
        .filter(|d| d.holds(dir))
        .max_by_key(|d| d.mounted().map_or(0, |m| m.components().count()))
}

/// The limits on `dir`: a phone's or camera's (from the device list, or
/// its gvfs path when the list doesn't know it), or none for a drive.
/// A phone's top folder only lists its storages ("Internal shared
/// storage", an SD card): files can't go there and the storages can't
/// be deleted. Nothing on a phone has a trash.
pub fn limits(drives: &[Drive], dir: &Path) -> Limits {
    let (name, root) = match device_of(drives, dir) {
        Some(d) => {
            let scripted = d
                .no_write
                .iter()
                .find(|(p, _)| p == dir)
                .map(|(_, why)| why.clone());
            let no_rename = d
                .no_rename
                .iter()
                .find(|(p, _)| p == dir)
                .map(|(_, why)| why.clone());
            if !d.media() {
                let read_only = d.read_only.then(|| format!("{} is read-only", d.name()));
                return Limits {
                    rename: no_rename
                        .or_else(|| scripted.clone())
                        .or_else(|| read_only.clone()),
                    write: scripted,
                    read_only,
                    ..Limits::default()
                };
            }
            let root = d.mounted().is_some_and(|m| m == dir);
            let mut l = media_limits(&d.name(), root);
            if l.write.is_none() {
                l.write = scripted;
            }
            if l.rename.is_none() {
                l.rename = no_rename.or_else(|| l.write.clone());
            }
            return l;
        }
        None => match gvfs_media_root(dir) {
            Some(root) => (
                root.file_name()
                    .map(|n| crate::gvfs::fuse_name_host(&n.to_string_lossy()))
                    .unwrap_or_default(),
                root == dir,
            ),
            None => return Limits::default(),
        },
    };
    media_limits(if name.is_empty() { "the phone" } else { &name }, root)
}

fn media_limits(name: &str, root: bool) -> Limits {
    Limits {
        write: root.then(|| format!("Files can only go inside a storage on {name}")),
        remove: root.then(|| format!("{name}'s storages can't be deleted")),
        no_trash: true,
        slow: true,
        rename: root.then(|| format!("{name}'s storages can't be renamed")),
        read_only: None,
    }
}

/// The gvfs folder of the phone or camera `path` is on:
/// `/run/user/1000/gvfs/mtp:host=Google_Pixel_8_…` for anything under it.
pub fn gvfs_media_root(path: &Path) -> Option<PathBuf> {
    let mut out = PathBuf::new();
    let mut prev_gvfs = false;
    for c in path.components() {
        out.push(c);
        let name = c.as_os_str().to_string_lossy();
        if prev_gvfs && (name.starts_with("mtp:host=") || name.starts_with("gphoto2:host=")) {
            return Some(out);
        }
        prev_gvfs = name == "gvfs" || name == ".gvfs";
    }
    None
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

/// Why an eject stopped: the step and the filesystem it stopped on.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum EjectError {
    /// Unmounting `part` failed. A `Busy` here is a program holding that
    /// filesystem, which a forced unmount can get past.
    Unmount {
        part: Box<Drive>,
        error: DeviceError,
    },
    /// `part` came off, but its encrypted partition wouldn't lock.
    Lock {
        part: Box<Drive>,
        error: DeviceError,
    },
    /// Every filesystem came off, but the drive wouldn't power off; a
    /// forced unmount can't help with that.
    PowerOff(DeviceError),
}

/// The filesystems on the same physical drive as `d` (itself included),
/// in listing order. A filesystem with no drive behind it stands alone.
pub fn partitions<'a>(all: &'a [Drive], d: &Drive) -> Vec<&'a Drive> {
    if d.drive.is_empty() {
        return all.iter().filter(|p| p.id == d.id).collect();
    }
    all.iter().filter(|p| p.drive == d.drive).collect()
}

/// Eject a whole drive: unmount every mounted filesystem on it in turn
/// (stopping at the first that fails), lock every unlocked encrypted
/// partition (mounted or not), then power the drive off. A drive that
/// can't power off is still safe to unplug once unmounted and locked.
/// `parts` is [`partitions`] of the row that asked.
pub fn eject(
    backend: &dyn DeviceBackend,
    parts: &[Drive],
    force: bool,
) -> Result<Ejected, EjectError> {
    for part in parts.iter().filter(|p| p.mounted().is_some()) {
        backend
            .unmount(&part.id, force)
            .map_err(|error| EjectError::Unmount {
                part: Box::new(part.clone()),
                error,
            })?;
    }
    for part in parts.iter().filter(|p| !p.crypto.is_empty()) {
        backend
            .lock(&part.crypto)
            .map_err(|error| EjectError::Lock {
                part: Box::new(part.clone()),
                error,
            })?;
    }
    let Some(first) = parts.first() else {
        return Ok(Ejected::Unmounted);
    };
    if !parts.iter().any(|p| p.can_power_off) {
        return Ok(Ejected::Unmounted);
    }
    // A locked filesystem's own id is gone: power off through the
    // encrypted partition, which is still there.
    match backend.power_off(first.stable_id()) {
        Ok(()) => Ok(Ejected::PoweredOff),
        Err(DeviceError::Unsupported) => Ok(Ejected::Unmounted),
        Err(e) => Err(EjectError::PowerOff(e)),
    }
}

/// Drives in `now` that weren't in `prev`. With no `prev` (the first
/// listing, at startup) nothing counts as arriving: a drive already
/// plugged in when Robin starts gets no toast. Nor does a partition that
/// was there under its other face: an encrypted one just unlocked (its
/// filesystem is new) or just locked (its encrypted block is back).
pub fn arrivals<'a>(prev: Option<&[Drive]>, now: &'a [Drive]) -> Vec<&'a Drive> {
    let Some(prev) = prev else {
        return Vec::new();
    };
    let seen = |id: &str| prev.iter().any(|p| p.id == id || p.crypto == id);
    now.iter()
        .filter(|d| !seen(&d.id) && (d.crypto.is_empty() || !seen(&d.crypto)))
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

/// The backend the app uses: UDisks2 on the system bus and gvfs's phone
/// and camera monitors on the session bus, whichever answer, unless
/// `ROBIN_DEVICES` says `none` (mountinfo only), `fake` (a scripted
/// SANDISK stick that plugs in two seconds after launch, for trying the
/// device states without hardware) or `fake-phone` (that stick plus a
/// locked PIXEL 8 phone that allows access six seconds after it mounts).
pub fn from_env() -> Option<Arc<dyn DeviceBackend>> {
    match std::env::var("ROBIN_DEVICES").as_deref() {
        Ok("none") => None,
        Ok(v @ ("fake" | "fake-phone")) => {
            let fake = FakeDevices::new();
            let base = std::env::var_os("XDG_RUNTIME_DIR")
                .map(PathBuf::from)
                .filter(|p| p.is_absolute())
                .unwrap_or_else(std::env::temp_dir)
                .join("robin-fake-devices");
            let plug = fake.clone();
            let phone = v == "fake-phone";
            std::thread::spawn(move || {
                std::thread::sleep(std::time::Duration::from_secs(2));
                plug.plug(FakeDrive::stick(
                    "fake-sandisk",
                    "SANDISK",
                    29_800_000_000,
                    &base.join("SANDISK"),
                ));
                if phone {
                    std::thread::sleep(std::time::Duration::from_secs(1));
                    let mut p = FakeDrive::phone("fake-pixel", "PIXEL 8", &base.join("PIXEL 8"));
                    p.locked = true;
                    p.allow_after = Some(std::time::Duration::from_secs(6));
                    p.list_delay = Some(std::time::Duration::from_millis(700));
                    plug.plug(p);
                }
            });
            Some(fake)
        }
        _ => {
            let mut all: Vec<Arc<dyn DeviceBackend>> = Vec::new();
            if let Ok(u) = UDisks::connect() {
                all.push(Arc::new(u));
            }
            if let Some(g) = crate::gvfs::Gvfs::session() {
                all.push(Arc::new(g));
            }
            match all.len() {
                0 => None,
                1 => all.pop(),
                _ => Some(Arc::new(Combined::new(all))),
            }
        }
    }
}

// ---- Several backends -----------------------------------------------------

/// UDisks2's drives and gvfs's phones as one list. Calls on a device go
/// to the backend that [owns](DeviceBackend::owns) its id.
pub struct Combined {
    parts: Vec<Arc<dyn DeviceBackend>>,
}

impl Combined {
    pub fn new(parts: Vec<Arc<dyn DeviceBackend>>) -> Self {
        Self { parts }
    }

    fn of(&self, id: &str) -> Result<&Arc<dyn DeviceBackend>, DeviceError> {
        self.parts
            .iter()
            .find(|b| b.owns(id))
            .ok_or_else(|| DeviceError::Failed("No such device".into()))
    }
}

impl DeviceBackend for Combined {
    /// Every backend's devices. One that fails fails the whole listing, so
    /// the window keeps what it shows instead of reading its devices as
    /// pulled out.
    fn drives(&self) -> Result<Vec<Drive>, DeviceError> {
        let mut out = Vec::new();
        for b in &self.parts {
            out.extend(b.drives()?);
        }
        Ok(out)
    }

    fn mount(&self, id: &str) -> Result<PathBuf, DeviceError> {
        self.of(id)?.mount(id)
    }

    fn unmount(&self, id: &str, force: bool) -> Result<(), DeviceError> {
        self.of(id)?.unmount(id, force)
    }

    fn power_off(&self, id: &str) -> Result<(), DeviceError> {
        self.of(id)?.power_off(id)
    }

    fn unlock(&self, id: &str, passphrase: &Passphrase) -> Result<String, DeviceError> {
        self.of(id)?.unlock(id, passphrase)
    }

    fn lock(&self, id: &str) -> Result<(), DeviceError> {
        self.of(id)?.lock(id)
    }

    fn retry(&self, id: &str) -> Result<PathBuf, DeviceError> {
        self.of(id)?.retry(id)
    }

    fn owns(&self, id: &str) -> bool {
        self.parts.iter().any(|b| b.owns(id))
    }

    fn pace(&self, dir: &Path) {
        for b in &self.parts {
            b.pace(dir);
        }
    }

    /// One channel that pings when any backend's does.
    fn watch(&self) -> Option<mpsc::Receiver<()>> {
        let rxs: Vec<_> = self.parts.iter().filter_map(|b| b.watch()).collect();
        if rxs.is_empty() {
            return None;
        }
        let (tx, rx) = mpsc::channel();
        for r in rxs {
            let tx = tx.clone();
            std::thread::spawn(move || {
                while r.recv().is_ok() {
                    if tx.send(()).is_err() {
                        break;
                    }
                }
            });
        }
        Some(rx)
    }

    fn holder(&self, mount: &Path) -> Option<String> {
        self.parts.first().and_then(|b| b.holder(mount))
    }
}

// ---- UDisks2 --------------------------------------------------------------

const UDISKS: &str = "org.freedesktop.UDisks2";
const BLOCK: &str = "org.freedesktop.UDisks2.Block";
const FILESYSTEM: &str = "org.freedesktop.UDisks2.Filesystem";
const DRIVE: &str = "org.freedesktop.UDisks2.Drive";
const ENCRYPTED: &str = "org.freedesktop.UDisks2.Encrypted";

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

/// D-Bus's "No such interface" (GDBus) or an unknown method: the object
/// has no Filesystem interface (yet).
fn no_interface(m: &str) -> bool {
    m.contains("No such interface") || m.contains("UnknownMethod") || m.contains("UnknownInterface")
}

/// Whether UDisks2's unlock error is cryptsetup refusing the key:
/// libblockdev says "Incorrect passphrase." (older ones pass on EPERM's
/// "Operation not permitted").
pub fn wrong_passphrase(m: &str) -> bool {
    let low = m.to_lowercase();
    low.contains("incorrect passphrase")
        || low.contains("operation not permitted")
        || low.contains("no key available with this passphrase")
}

fn unlock_error(e: zbus::Error) -> DeviceError {
    match device_error(e) {
        DeviceError::Failed(m) if wrong_passphrase(&m) => DeviceError::WrongPassphrase,
        other => other,
    }
}

/// Turn UDisks2's object tree into drives: filesystems on a block that
/// isn't hidden, on a drive that is removable or external. An encrypted
/// partition is listed as itself while locked ([`Drive::locked`]), and
/// as the filesystem it opens to once unlocked ([`Drive::crypto`]). That
/// cleartext block is a device-mapper node with no drive of its own, so
/// its drive and hints come from the encrypted partition under it.
pub fn drives_from_managed(objects: &Managed) -> Vec<Drive> {
    let find = |path: &str| {
        objects
            .iter()
            .find(|(p, _)| p.as_str() == path)
            .map(|(_, i)| i)
    };
    let link = |props: &Props, key: &str| {
        prop::<zbus::zvariant::OwnedObjectPath>(props, key)
            .map(|p| p.as_str().to_string())
            .filter(|p| p != "/")
            .unwrap_or_default()
    };
    let unlocked = |path: &str, enc: &Props| {
        !link(enc, "CleartextDevice").is_empty()
            || objects.values().any(|i| {
                i.get(BLOCK)
                    .is_some_and(|b| link(b, "CryptoBackingDevice") == path)
            })
    };
    let mut out = Vec::new();
    for (path, ifaces) in objects {
        let Some(block) = ifaces.get(BLOCK) else {
            continue;
        };
        let fs = ifaces.get(FILESYSTEM);
        let crypto = link(block, "CryptoBackingDevice");
        // Whose drive and hints count: the encrypted partition under a
        // cleartext block, else the block itself.
        let base = if crypto.is_empty() {
            block
        } else {
            match find(&crypto).and_then(|i| i.get(BLOCK)) {
                Some(b) => b,
                None => continue,
            }
        };
        let locked = match ifaces.get(ENCRYPTED) {
            // Unlocked: its filesystem has the row.
            Some(enc) if unlocked(path.as_str(), enc) => continue,
            Some(_) => true,
            None => false,
        };
        // A plain block needs a filesystem; a cleartext one is listed
        // even before (or without) one, so it can still be locked.
        if fs.is_none() && !locked && crypto.is_empty() {
            continue;
        }
        if [block, base]
            .iter()
            .any(|b| prop::<bool>(b, "HintIgnore").unwrap_or(false))
        {
            continue;
        }
        let drive_path = link(base, "Drive");
        let drive = find(&drive_path).and_then(|i| i.get(DRIVE));
        let removable = drive.is_some_and(|d| {
            prop::<bool>(d, "Removable").unwrap_or(false)
                || prop::<bool>(d, "MediaRemovable").unwrap_or(false)
                || matches!(
                    prop::<String>(d, "ConnectionBus").as_deref(),
                    Some("usb" | "sdio" | "ieee1394")
                )
        });
        if prop::<bool>(base, "HintSystem").unwrap_or(true) && !removable {
            continue;
        }
        let mount_points = fs
            .and_then(|fs| prop::<Vec<Vec<u8>>>(fs, "MountPoints"))
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
            kind: Kind::Drive,
            waiting: false,
            no_write: Vec::new(),
            no_rename: Vec::new(),
            locked,
            crypto,
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

    fn mount_now(&self, id: &str) -> Result<PathBuf, DeviceError> {
        let got = self.call(id, FILESYSTEM, "Mount", false);
        self.mount_landed(id, got)
    }

    fn mount_landed(
        &self,
        id: &str,
        got: Result<zbus::Message, DeviceError>,
    ) -> Result<PathBuf, DeviceError> {
        match got {
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

    /// Up to five seconds for `id` to gain a Filesystem interface.
    fn wait_for_filesystem(&self, id: &str) -> bool {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        loop {
            let has = self.managed().is_ok_and(|m| {
                m.iter()
                    .any(|(p, i)| p.as_str() == id && i.contains_key(FILESYSTEM))
            });
            if has {
                return true;
            }
            if std::time::Instant::now() >= deadline {
                return false;
            }
            std::thread::sleep(std::time::Duration::from_millis(200));
        }
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

    /// UDisks2 ids are its object paths.
    fn owns(&self, id: &str) -> bool {
        id.starts_with('/')
    }

    fn mount(&self, id: &str) -> Result<PathBuf, DeviceError> {
        match self.call(id, FILESYSTEM, "Mount", false) {
            // A filesystem just unlocked: udev may not have probed it yet,
            // so its Filesystem interface isn't there. Wait for it.
            Err(DeviceError::Failed(m)) if no_interface(&m) => {
                if !self.wait_for_filesystem(id) {
                    return Err(DeviceError::Failed(
                        "it has no filesystem Robin can open".into(),
                    ));
                }
                self.mount_now(id)
            }
            other => self.mount_landed(id, other),
        }
    }

    fn unmount(&self, id: &str, force: bool) -> Result<(), DeviceError> {
        self.call(id, FILESYSTEM, "Unmount", force).map(|_| ())
    }

    fn power_off(&self, id: &str) -> Result<(), DeviceError> {
        let drive = self.drive_of(id)?;
        self.call(&drive, DRIVE, "PowerOff", false).map(|_| ())
    }

    /// `Encrypted.Unlock(passphrase, {})`. The passphrase goes only into
    /// the D-Bus message; UDisks2 checks polkit itself, which may show
    /// the session's password prompt. An already unlocked partition (a
    /// desktop automounter got there first) opens to its filesystem.
    fn unlock(&self, id: &str, passphrase: &Passphrase) -> Result<String, DeviceError> {
        let path = zbus::zvariant::ObjectPath::try_from(id)
            .map_err(|e| DeviceError::Failed(e.to_string()))?;
        let opts: HashMap<&str, zbus::zvariant::Value<'_>> = HashMap::new();
        let got = self.conn.call_method(
            Some(UDISKS),
            path,
            Some(ENCRYPTED),
            "Unlock",
            &(passphrase.expose(), opts),
        );
        match got.map_err(unlock_error) {
            Ok(reply) => reply
                .body()
                .deserialize::<zbus::zvariant::OwnedObjectPath>()
                .map(|p| p.as_str().to_string())
                .map_err(|e| DeviceError::Failed(e.to_string())),
            Err(DeviceError::Failed(m)) if m.contains("already unlocked") => self
                .drives()?
                .into_iter()
                .find(|d| d.crypto == id)
                .map(|d| d.id)
                .ok_or(DeviceError::Failed(m)),
            Err(e) => Err(e),
        }
    }

    fn lock(&self, id: &str) -> Result<(), DeviceError> {
        match self.call(id, ENCRYPTED, "Lock", false) {
            Ok(_) => Ok(()),
            Err(DeviceError::Failed(m)) if m.to_lowercase().contains("busy") => {
                Err(DeviceError::Busy)
            }
            Err(e) => Err(e),
        }
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
    /// A phone that mounts but lists nothing until [`FakeDevices::allow`]:
    /// locked, or "Allow access" not tapped yet.
    pub locked: bool,
    /// A locked phone allows access this long after it first mounts, as
    /// someone tapping Allow would (the `fake-phone` demo).
    pub allow_after: Option<std::time::Duration>,
    /// Listing any folder on it takes this long (a slow phone).
    pub list_delay: Option<std::time::Duration>,
    /// A phone's storages, made in its top folder once it allows access.
    pub storages: Vec<String>,
    /// An encrypted partition: `drive` is the filesystem it unlocks to.
    pub luks: Option<FakeLuks>,
}

/// A scripted LUKS partition: its own id and label while locked, the
/// passphrase that opens it, and an unlock that fails for another
/// reason.
#[derive(Clone, PartialEq, Eq)]
pub struct FakeLuks {
    /// The encrypted block's id; the filesystem keeps `drive.id`.
    pub id: String,
    /// What the locked row is called (a LUKS2 label).
    pub label: String,
    /// The one that unlocks it. Scripted by the test, never typed.
    pub passphrase: String,
    /// Unlock fails with this message (wrong passphrase aside).
    pub unlock_error: Option<String>,
    /// Lock refuses with this message.
    pub lock_error: Option<String>,
    pub unlocked: bool,
    /// The locked block's IdType: "crypto_LUKS", "BitLocker" or
    /// "crypto_TCRYPT".
    pub id_type: String,
}

impl std::fmt::Debug for FakeLuks {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("FakeLuks")
            .field("id", &self.id)
            .field("label", &self.label)
            .field("passphrase", &"<redacted>")
            .field("unlock_error", &self.unlock_error)
            .field("lock_error", &self.lock_error)
            .field("unlocked", &self.unlocked)
            .field("id_type", &self.id_type)
            .finish()
    }
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
                kind: Kind::Drive,
                waiting: false,
                no_write: Vec::new(),
                no_rename: Vec::new(),
                locked: false,
                crypto: String::new(),
            },
            mount_at: mount_at.to_path_buf(),
            mount_error: None,
            read_only: false,
            busy: false,
            locked: false,
            allow_after: None,
            list_delay: None,
            storages: Vec::new(),
            luks: None,
        }
    }

    /// A LUKS partition on a stick, locked: listed as `id` until the
    /// right passphrase unlocks it to an exFAT filesystem `{id}-clear`.
    pub fn encrypted(id: &str, label: &str, size: u64, mount_at: &Path, passphrase: &str) -> Self {
        let mut d = Self::stick(&format!("{id}-clear"), label, size, mount_at);
        d.drive.drive = format!("{id}-drive");
        d.luks = Some(FakeLuks {
            id: id.into(),
            label: label.into(),
            passphrase: passphrase.into(),
            unlock_error: None,
            lock_error: None,
            unlocked: false,
            id_type: "crypto_LUKS".into(),
        });
        d
    }

    /// Whether `id` names it: its filesystem, or its encrypted block.
    fn is(&self, id: &str) -> bool {
        self.drive.id == id || self.luks.as_ref().is_some_and(|l| l.id == id)
    }

    /// The row as UDisks2 would list it right now.
    fn listed(&self) -> Drive {
        match &self.luks {
            Some(l) if !l.unlocked => Drive {
                id: l.id.clone(),
                label: l.label.clone(),
                fs_type: l.id_type.clone(),
                mount_points: Vec::new(),
                read_only: false,
                locked: true,
                crypto: String::new(),
                ..self.drive.clone()
            },
            Some(l) => Drive {
                crypto: l.id.clone(),
                ..self.drive.clone()
            },
            None => self.drive.clone(),
        }
    }

    /// An Android phone over MTP, unmounted, with one storage. Like a real
    /// one it can't power off: eject only unmounts it.
    pub fn phone(id: &str, name: &str, mount_at: &Path) -> Self {
        let mut p = Self::stick(id, name, 0, mount_at);
        p.drive.drive = String::new();
        p.drive.device = format!("mtp://fake-{id}/");
        p.drive.fs_type = "mtp".into();
        p.drive.can_power_off = false;
        p.drive.kind = Kind::Phone;
        p.storages = vec!["Internal shared storage".into()];
        p
    }

    /// A camera over PTP (gvfs's gPhoto2 monitor), unmounted, with one
    /// card. Like a phone it waits while `locked` (off, or not in PTP mode).
    pub fn camera(id: &str, name: &str, mount_at: &Path) -> Self {
        let mut c = Self::phone(id, name, mount_at);
        c.drive.device = format!("gphoto2://fake-{id}/");
        c.drive.fs_type = "gphoto2".into();
        c.drive.kind = Kind::Camera;
        c.storages = vec!["store_00010001".into()];
        c
    }

    fn make_storages(&self) {
        for s in &self.storages {
            let _ = std::fs::create_dir_all(self.mount_at.join(s));
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
    /// Itself, for a scripted Allow that lands later on its own thread.
    me: Mutex<Option<std::sync::Weak<FakeDevices>>>,
}

impl FakeDevices {
    pub fn new() -> Arc<Self> {
        let me = Arc::new(Self::default());
        *me.me.lock().unwrap_or_else(|e| e.into_inner()) = Some(Arc::downgrade(&me));
        me
    }

    fn held(&self) -> std::sync::MutexGuard<'_, Vec<FakeDrive>> {
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
        self.held().push(d);
        self.notify();
    }

    /// Pulled out, mounted or not.
    pub fn unplug(&self, id: &str) {
        self.held().retain(|d| !d.is(id));
        self.notify();
    }

    /// Something else unmounted it; it stays plugged in.
    pub fn unmount_elsewhere(&self, id: &str) {
        for d in self.held().iter_mut().filter(|d| d.drive.id == id) {
            d.drive.mount_points.clear();
        }
        self.notify();
    }

    pub fn edit(&self, id: &str, f: impl FnOnce(&mut FakeDrive)) {
        if let Some(d) = self.held().iter_mut().find(|d| d.is(id)) {
            f(d);
        }
        self.notify();
    }

    pub fn get(&self, id: &str) -> Option<FakeDrive> {
        self.held().iter().find(|d| d.is(id)).cloned()
    }

    /// Someone unlocked the phone and tapped Allow: its storages show.
    pub fn allow(&self, id: &str) {
        if let Some(d) = self.held().iter_mut().find(|d| d.drive.id == id) {
            d.locked = false;
            if d.drive.mounted().is_some() {
                d.make_storages();
            }
            d.drive.waiting = false;
        }
        self.notify();
    }
}

impl DeviceBackend for FakeDevices {
    fn drives(&self) -> Result<Vec<Drive>, DeviceError> {
        Ok(self.held().iter().map(FakeDrive::listed).collect())
    }

    fn mount(&self, id: &str) -> Result<PathBuf, DeviceError> {
        self.note(format!("mount {id}"));
        let at = {
            let mut drives = self.held();
            let d = drives
                .iter_mut()
                .find(|d| d.is(id))
                .ok_or_else(|| DeviceError::Failed("No such device".into()))?;
            // Locked: there is no filesystem to mount, as on UDisks2.
            if d.luks.as_ref().is_some_and(|l| !l.unlocked) {
                return Err(DeviceError::Failed(
                    "No such interface 'org.freedesktop.UDisks2.Filesystem'".into(),
                ));
            }
            if let Some(m) = &d.mount_error {
                return Err(DeviceError::Failed(m.clone()));
            }
            if let Some(m) = d.drive.mounted() {
                return Ok(m.to_path_buf());
            }
            std::fs::create_dir_all(&d.mount_at).map_err(|e| DeviceError::Failed(e.to_string()))?;
            d.drive.mount_points = vec![d.mount_at.clone()];
            d.drive.read_only = d.read_only;
            if d.drive.media() {
                if d.locked {
                    d.drive.waiting = true;
                } else {
                    d.make_storages();
                }
            }
            if let (true, Some(after)) = (d.locked, d.allow_after.take()) {
                let me = self.me.lock().unwrap_or_else(|e| e.into_inner()).clone();
                let id = id.to_string();
                std::thread::spawn(move || {
                    std::thread::sleep(after);
                    if let Some(me) = me.and_then(|w| w.upgrade()) {
                        me.allow(&id);
                    }
                });
            }
            d.mount_at.clone()
        };
        self.notify();
        Ok(at)
    }

    fn retry(&self, id: &str) -> Result<PathBuf, DeviceError> {
        self.note(format!("retry {id}"));
        self.mount(id)
    }

    fn pace(&self, dir: &Path) {
        let delay = self
            .held()
            .iter()
            .find(|d| d.drive.holds(dir))
            .and_then(|d| d.list_delay);
        if let Some(t) = delay {
            std::thread::sleep(t);
        }
    }

    fn unmount(&self, id: &str, force: bool) -> Result<(), DeviceError> {
        self.note(format!("unmount {id}{}", if force { " force" } else { "" }));
        {
            let mut drives = self.held();
            let d = drives
                .iter_mut()
                .find(|d| d.drive.id == id)
                .ok_or_else(|| DeviceError::Failed("No such device".into()))?;
            if d.busy && !force {
                return Err(DeviceError::Busy);
            }
            d.drive.mount_points.clear();
            d.drive.waiting = false;
            d.busy = false;
        }
        self.notify();
        Ok(())
    }

    fn power_off(&self, id: &str) -> Result<(), DeviceError> {
        self.note(format!("power-off {id}"));
        let Some(me) = self.get(id) else {
            return Err(DeviceError::Failed("No such device".into()));
        };
        if !me.drive.can_power_off {
            return Err(DeviceError::Unsupported);
        }
        // As UDisks2 does: the whole drive goes, so every filesystem on it
        // must be unmounted first.
        let on_drive: Vec<FakeDrive> = self
            .held()
            .iter()
            .filter(|d| d.drive.drive == me.drive.drive)
            .cloned()
            .collect();
        // UDisks2 won't power off a drive with a partition still
        // mounted, or still unlocked.
        if on_drive
            .iter()
            .any(|d| d.drive.mounted().is_some() || d.luks.as_ref().is_some_and(|l| l.unlocked))
        {
            return Err(DeviceError::Busy);
        }
        for d in on_drive {
            self.unplug(&d.drive.id);
        }
        Ok(())
    }

    /// The log says which partition, never what was typed.
    fn unlock(&self, id: &str, passphrase: &Passphrase) -> Result<String, DeviceError> {
        self.note(format!("unlock {id}"));
        let clear = {
            let mut drives = self.held();
            let d = drives
                .iter_mut()
                .find(|d| d.luks.as_ref().is_some_and(|l| l.id == id))
                .ok_or_else(|| DeviceError::Failed("No such device".into()))?;
            let fs = d.drive.id.clone();
            let Some(l) = d.luks.as_mut() else {
                return Err(DeviceError::Unsupported);
            };
            if !l.unlocked {
                if let Some(m) = &l.unlock_error {
                    return Err(DeviceError::Failed(m.clone()));
                }
                if passphrase.expose() != l.passphrase {
                    return Err(DeviceError::WrongPassphrase);
                }
                l.unlocked = true;
            }
            fs
        };
        self.notify();
        Ok(clear)
    }

    fn lock(&self, id: &str) -> Result<(), DeviceError> {
        self.note(format!("lock {id}"));
        {
            let mut drives = self.held();
            let d = drives
                .iter_mut()
                .find(|d| d.luks.as_ref().is_some_and(|l| l.id == id))
                .ok_or_else(|| DeviceError::Failed("No such device".into()))?;
            if d.drive.mounted().is_some() {
                return Err(DeviceError::Busy);
            }
            let Some(l) = d.luks.as_mut() else {
                return Err(DeviceError::Unsupported);
            };
            if let Some(m) = &l.lock_error {
                return Err(DeviceError::Failed(m.clone()));
            }
            if !l.unlocked {
                return Err(DeviceError::Failed(format!("Device {id} is not unlocked")));
            }
            l.unlocked = false;
        }
        self.notify();
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
        let parts = std::slice::from_ref(&drive);
        assert_eq!(
            eject(fake.as_ref(), parts, false),
            Err(EjectError::Unmount {
                part: Box::new(drive.clone()),
                error: DeviceError::Busy
            })
        );
        assert!(
            fake.drives().unwrap()[0].mounted().is_some(),
            "still mounted"
        );
        assert_eq!(eject(fake.as_ref(), parts, true), Ok(Ejected::PoweredOff));
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

    /// One stick, two partitions: A and B share the drive.
    fn two_partitions(fake: &FakeDevices, root: &Path) {
        let mut a = FakeDrive::stick("sdc1", "PART-A", 16 << 30, &root.join("PART-A"));
        let mut b = FakeDrive::stick("sdc2", "PART-B", 16 << 30, &root.join("PART-B"));
        a.drive.drive = "stick-drive".into();
        b.drive.drive = "stick-drive".into();
        fake.plug(a);
        fake.plug(b);
        fake.mount("sdc1").unwrap();
        fake.mount("sdc2").unwrap();
    }

    #[test]
    fn ejecting_one_partition_unmounts_both_then_powers_the_drive_off() {
        let root = tmp("two-parts");
        let fake = FakeDevices::new();
        two_partitions(&fake, &root);
        let all = fake.drives().unwrap();
        let parts: Vec<Drive> = partitions(&all, &all[0]).into_iter().cloned().collect();
        assert_eq!(parts.len(), 2, "A's row finds B on the same drive");
        assert_eq!(eject(fake.as_ref(), &parts, false), Ok(Ejected::PoweredOff));
        assert!(
            fake.drives().unwrap().is_empty(),
            "both gone with the drive"
        );
        let log = fake.log.lock().unwrap().clone();
        assert_eq!(
            log[2..],
            ["unmount sdc1", "unmount sdc2", "power-off sdc1"],
            "every filesystem comes off before the power-off"
        );
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn a_busy_sibling_stops_the_eject_and_names_itself() {
        let root = tmp("two-parts-busy");
        let fake = FakeDevices::new();
        two_partitions(&fake, &root);
        fake.edit("sdc2", |d| d.busy = true);
        let all = fake.drives().unwrap();
        let parts: Vec<Drive> = partitions(&all, &all[0]).into_iter().cloned().collect();
        match eject(fake.as_ref(), &parts, false) {
            Err(EjectError::Unmount { part, error }) => {
                assert_eq!((part.id.as_str(), error), ("sdc2", DeviceError::Busy));
            }
            other => panic!("expected B's unmount to refuse, got {other:?}"),
        }
        assert_eq!(fake.drives().unwrap().len(), 2, "no power-off");
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn a_power_off_refused_after_unmounting_is_not_a_holder() {
        let root = tmp("two-parts-poweroff");
        let fake = FakeDevices::new();
        two_partitions(&fake, &root);
        let all = fake.drives().unwrap();
        // Only A asked: B stays mounted, so the drive can't go.
        let only_a = vec![all[0].clone()];
        assert_eq!(
            eject(fake.as_ref(), &only_a, false),
            Err(EjectError::PowerOff(DeviceError::Busy))
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
        assert_eq!(
            eject(fake.as_ref(), std::slice::from_ref(&drive), false),
            Ok(Ejected::Unmounted)
        );
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
        // Until the exec lands, the child still carries the test's name.
        let comm = format!("/proc/{}/comm", child.id());
        let started = std::time::Instant::now();
        while std::fs::read_to_string(&comm).map_or(true, |c| c.trim() != "sleep")
            && started.elapsed() < std::time::Duration::from_secs(5)
        {
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
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

    fn pixel(root: &Path) -> FakeDrive {
        FakeDrive::phone(
            "pixel",
            "Pixel 8",
            &root.join("gvfs/mtp:host=Google_Pixel_8_X"),
        )
    }

    #[test]
    fn a_phone_plugs_mounts_and_lists_its_storages() {
        let root = tmp("phone");
        let fake = FakeDevices::new();
        let rx = fake.watch().unwrap();
        fake.plug(pixel(&root));
        rx.try_recv().unwrap();
        let d = fake.drives().unwrap()[0].clone();
        assert_eq!(d.kind, Kind::Phone);
        assert!(d.media() && d.kind.slow());
        assert_eq!(d.kind.glyph(), "\u{f011c}", "md-cellphone");
        let at = fake.mount("pixel").unwrap();
        assert!(at.join("Internal shared storage").is_dir());
        assert!(!fake.drives().unwrap()[0].waiting);
        fake.unplug("pixel");
        rx.try_recv().unwrap();
        assert!(fake.drives().unwrap().is_empty());
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn a_locked_phone_waits_until_allowed() {
        let root = tmp("phone-locked");
        let fake = FakeDevices::new();
        let mut p = pixel(&root);
        p.locked = true;
        fake.plug(p);
        let at = fake.mount("pixel").unwrap();
        let d = fake.drives().unwrap()[0].clone();
        assert!(d.waiting, "mounted but nothing to list yet");
        assert_eq!(std::fs::read_dir(&at).unwrap().count(), 0);
        // [Retry] while still locked: still empty, still waiting.
        fake.retry("pixel").unwrap();
        assert!(fake.drives().unwrap()[0].waiting);
        fake.allow("pixel");
        assert!(!fake.drives().unwrap()[0].waiting);
        assert!(at.join("Internal shared storage").is_dir());
        let log = fake.log.lock().unwrap().clone();
        assert_eq!(log, ["mount pixel", "retry pixel", "mount pixel"]);
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn a_scripted_allow_lands_on_its_own() {
        let root = tmp("phone-allow-after");
        let fake = FakeDevices::new();
        let rx = fake.watch().unwrap();
        let mut p = pixel(&root);
        p.locked = true;
        p.allow_after = Some(std::time::Duration::from_millis(20));
        fake.plug(p);
        rx.recv().unwrap();
        let at = fake.mount("pixel").unwrap();
        rx.recv().unwrap();
        rx.recv_timeout(std::time::Duration::from_secs(5))
            .expect("the scripted allow pings the watchers");
        assert!(!fake.drives().unwrap()[0].waiting);
        assert!(at.join("Internal shared storage").is_dir());
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn ejecting_a_phone_only_unmounts_it() {
        let root = tmp("phone-eject");
        let fake = FakeDevices::new();
        fake.plug(pixel(&root));
        fake.plug(stick(&root));
        fake.mount("pixel").unwrap();
        let all = fake.drives().unwrap();
        let phone = all.iter().find(|d| d.id == "pixel").unwrap();
        let parts: Vec<Drive> = partitions(&all, phone).into_iter().cloned().collect();
        assert_eq!(parts.len(), 1, "a phone shares no drive with the stick");
        assert_eq!(eject(fake.as_ref(), &parts, false), Ok(Ejected::Unmounted));
        let log = fake.log.lock().unwrap().clone();
        assert_eq!(log, ["mount pixel", "unmount pixel"], "never powered off");
        assert_eq!(fake.drives().unwrap().len(), 2, "still plugged in");
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn a_slow_phone_paces_its_listings() {
        let root = tmp("phone-pace");
        let fake = FakeDevices::new();
        let mut p = pixel(&root);
        p.list_delay = Some(std::time::Duration::from_millis(60));
        fake.plug(p);
        let at = fake.mount("pixel").unwrap();
        let t = std::time::Instant::now();
        fake.pace(&at.join("Internal shared storage"));
        assert!(t.elapsed() >= std::time::Duration::from_millis(60));
        let t = std::time::Instant::now();
        fake.pace(&root);
        assert!(
            t.elapsed() < std::time::Duration::from_millis(60),
            "not on the phone"
        );
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn a_phone_top_folder_refuses_writes_and_nothing_has_a_trash() {
        let root = tmp("phone-limits");
        let fake = FakeDevices::new();
        let mut p = pixel(&root);
        let data = p.mount_at.join("Internal shared storage/Android/data");
        p.drive.no_write = vec![(data.clone(), "no changes here".into())];
        fake.plug(p);
        fake.plug(stick(&root));
        let at = fake.mount("pixel").unwrap();
        fake.mount("sdb1").unwrap();
        let all = fake.drives().unwrap();
        let top = limits(&all, &at);
        assert_eq!(
            top.write.as_deref(),
            Some("Files can only go inside a storage on Pixel 8")
        );
        assert_eq!(
            top.remove.as_deref(),
            Some("Pixel 8's storages can't be deleted")
        );
        assert!(top.no_trash && top.slow);
        let inside = limits(&all, &at.join("Internal shared storage"));
        assert_eq!(inside.write, None);
        assert_eq!(inside.remove, None);
        assert!(inside.no_trash && inside.slow);
        assert_eq!(
            limits(&all, &data).write.as_deref(),
            Some("no changes here")
        );
        let stick = limits(&all, &root.join("SANDISK"));
        assert_eq!(stick, Limits::default(), "a USB stick has no limits");
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn gvfs_paths_are_media_even_without_a_backend_row() {
        let p =
            Path::new("/run/user/1000/gvfs/mtp:host=Google_Pixel_8_X/Internal shared storage/DCIM");
        assert_eq!(
            gvfs_media_root(p),
            Some(PathBuf::from(
                "/run/user/1000/gvfs/mtp:host=Google_Pixel_8_X"
            ))
        );
        let cam = Path::new("/home/sky/.gvfs/gphoto2:host=Canon/DCIM");
        assert_eq!(
            gvfs_media_root(cam),
            Some(PathBuf::from("/home/sky/.gvfs/gphoto2:host=Canon"))
        );
        assert_eq!(
            gvfs_media_root(Path::new("/home/sky/mtp:host=x")),
            None,
            "not under gvfs"
        );
        assert_eq!(
            gvfs_media_root(Path::new("/run/user/1000/gvfs/smb-share:server=nas")),
            None
        );
        let l = limits(&[], p);
        assert!(l.no_trash && l.slow);
        assert_eq!(l.write, None);
    }

    /// A backend that owns ids with its own prefix and logs its calls.
    struct Tagged(&'static str, Mutex<Vec<String>>);

    impl DeviceBackend for Tagged {
        fn drives(&self) -> Result<Vec<Drive>, DeviceError> {
            let mut d = FakeDrive::stick(&format!("{}1", self.0), self.0, 1, Path::new("/x")).drive;
            d.drive = String::new();
            Ok(vec![d])
        }
        fn owns(&self, id: &str) -> bool {
            id.starts_with(self.0)
        }
        fn mount(&self, id: &str) -> Result<PathBuf, DeviceError> {
            self.1.lock().unwrap().push(format!("mount {id}"));
            Ok(PathBuf::from("/x"))
        }
        fn unmount(&self, id: &str, _force: bool) -> Result<(), DeviceError> {
            self.1.lock().unwrap().push(format!("unmount {id}"));
            Ok(())
        }
        fn power_off(&self, _id: &str) -> Result<(), DeviceError> {
            Err(DeviceError::Unsupported)
        }
        fn watch(&self) -> Option<mpsc::Receiver<()>> {
            None
        }
        fn holder(&self, _mount: &Path) -> Option<String> {
            None
        }
    }

    #[test]
    fn combined_routes_each_call_to_the_backend_that_owns_the_id() {
        let a = Arc::new(Tagged("/dev", Mutex::default()));
        let b = Arc::new(Tagged("gvfs:", Mutex::default()));
        let both = Combined::new(vec![a.clone(), b.clone()]);
        let ids: Vec<String> = both.drives().unwrap().into_iter().map(|d| d.id).collect();
        assert_eq!(ids, ["/dev1", "gvfs:1"]);
        both.mount("gvfs:1").unwrap();
        both.unmount("/dev1", false).unwrap();
        assert_eq!(*a.1.lock().unwrap(), ["unmount /dev1"]);
        assert_eq!(*b.1.lock().unwrap(), ["mount gvfs:1"]);
        assert!(both.mount("nobody").is_err(), "no owner, no call");
    }

    // ---- Encrypted (LUKS) partitions ----------------------------------

    const RIGHT: &str = "correct horse";
    const WRONG: &str = "hunter2 hunter2";

    fn luks(root: &Path) -> FakeDrive {
        FakeDrive::encrypted(
            "sdb1",
            "SANDISK",
            29_800_000_000,
            &root.join("SANDISK"),
            RIGHT,
        )
    }

    /// No passphrase in anything a test, log or toast can read.
    fn assert_clean(what: &str, text: &str) {
        assert!(
            !text.contains(RIGHT) && !text.contains(WRONG),
            "the passphrase leaked into {what}: {text}"
        );
    }

    #[test]
    fn a_locked_partition_is_listed_as_itself_with_nothing_to_mount() {
        let root = tmp("luks-list");
        let fake = FakeDevices::new();
        fake.plug(luks(&root));
        let all = fake.drives().unwrap();
        assert_eq!(all.len(), 1);
        let d = &all[0];
        assert_eq!((d.id.as_str(), d.name()), ("sdb1", "SANDISK".to_string()));
        assert!(d.locked && d.mounted().is_none() && d.crypto.is_empty());
        assert_eq!(d.fs_type, "crypto_LUKS");
        // Mounting the encrypted block itself fails, as on UDisks2.
        assert!(matches!(fake.mount("sdb1"), Err(DeviceError::Failed(_))));
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn the_right_passphrase_unlocks_to_a_filesystem_that_mounts() {
        let root = tmp("luks-right");
        let fake = FakeDevices::new();
        fake.plug(luks(&root));
        let rx = fake.watch().unwrap();
        let clear = fake.unlock("sdb1", &Passphrase::new(RIGHT)).unwrap();
        rx.try_recv().expect("an unlock pings the watchers");
        assert_eq!(clear, "sdb1-clear");
        let all = fake.drives().unwrap();
        assert_eq!(all.len(), 1, "the filesystem replaces the locked row");
        assert_eq!(
            (all[0].id.as_str(), all[0].crypto.as_str(), all[0].locked),
            ("sdb1-clear", "sdb1", false)
        );
        assert_eq!(all[0].stable_id(), "sdb1");
        let at = fake.mount(&clear).unwrap();
        assert_eq!(at, root.join("SANDISK"));
        // Unlocking again (an automounter got there first) opens to the
        // same filesystem.
        assert_eq!(fake.unlock("sdb1", &Passphrase::new(RIGHT)).unwrap(), clear);
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn a_wrong_passphrase_is_its_own_error_and_leaves_it_locked() {
        let root = tmp("luks-wrong");
        let fake = FakeDevices::new();
        fake.plug(luks(&root));
        let e = fake.unlock("sdb1", &Passphrase::new(WRONG)).unwrap_err();
        assert_eq!(e, DeviceError::WrongPassphrase);
        assert_eq!(
            unlock_error_text("SANDISK", &e, Crypto::Luks),
            "Wrong passphrase. Try again."
        );
        assert!(fake.drives().unwrap()[0].locked);
        let log = fake.log.lock().unwrap().join("\n");
        assert_eq!(log, "unlock sdb1");
        assert_clean("the call log", &log);
        assert_clean("the error", &format!("{e} {e:?}"));
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn an_unlock_failure_reads_in_plain_words() {
        let root = tmp("luks-fail");
        let fake = FakeDevices::new();
        let mut d = luks(&root);
        if let Some(l) = d.luks.as_mut() {
            l.unlock_error = Some(
                "Error unlocking /dev/sdb1: Failed to activate device: Cannot allocate memory"
                    .into(),
            );
        }
        fake.plug(d);
        let e = fake.unlock("sdb1", &Passphrase::new(RIGHT)).unwrap_err();
        assert_eq!(
            unlock_error_text("SANDISK", &e, Crypto::Luks),
            "Not enough free memory to unlock SANDISK"
        );
        assert!(fake.drives().unwrap()[0].locked, "still locked");
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn udisks_unlock_messages_map_to_plain_words() {
        let t =
            |m: &str| unlock_error_text("SANDISK", &DeviceError::Failed(m.into()), Crypto::Luks);
        assert!(wrong_passphrase(
            "Error unlocking /dev/sdb1: Failed to activate device: Incorrect passphrase."
        ));
        assert!(wrong_passphrase(
            "Error unlocking /dev/sdb1: Failed to activate device: Operation not permitted"
        ));
        assert!(!wrong_passphrase(
            "Error unlocking /dev/sdb1: Failed to activate device: Cannot allocate memory"
        ));
        assert_eq!(
            t("Error unlocking /dev/sdb1: Failed to activate device: Input/output error"),
            "Couldn't unlock SANDISK: Input/output error"
        );
        assert_eq!(
            t("Error unlocking /dev/sdb1: Failed to activate device: Unknown or unsupported LUKS2 requirements detected."),
            "SANDISK uses encryption features this system can't open"
        );
        assert_eq!(t("No such device"), "SANDISK was removed");
        assert_eq!(
            t("Device /dev/sdb1 does not appear to be a LUKS, BITLK or TCRYPT device"),
            "SANDISK isn't an encrypted volume Robin can open"
        );
        assert_eq!(t(""), "Couldn't unlock SANDISK: UDisks2 gave no reason");
        assert_eq!(
            unlock_error_text(
                "SANDISK",
                &DeviceError::NotAuthorized("dismissed".into()),
                Crypto::Luks
            ),
            "Authentication was cancelled"
        );
        assert_eq!(
            unlock_error_text(
                "SANDISK",
                &DeviceError::NotAuthorized(String::new()),
                Crypto::Luks
            ),
            "You aren't allowed to unlock SANDISK"
        );
    }

    #[test]
    fn a_passphrase_never_prints_and_is_zeroed() {
        let mut p = Passphrase::new(WRONG);
        assert_eq!(p.expose(), WRONG);
        assert_eq!(format!("{p:?}"), "Passphrase(<redacted>)");
        let mut luks = luks(Path::new("/nonexistent"));
        if let Some(l) = luks.luks.as_mut() {
            l.passphrase = WRONG.into();
        }
        assert_clean("the fake's Debug", &format!("{luks:?}"));
        p.wipe();
        assert!(p.expose().bytes().all(|b| b == 0));
        assert_eq!(p.expose().len(), WRONG.len(), "zeroed in place");
    }

    #[test]
    fn locking_needs_the_filesystem_unmounted_first() {
        let root = tmp("luks-lock");
        let fake = FakeDevices::new();
        fake.plug(luks(&root));
        let clear = fake.unlock("sdb1", &Passphrase::new(RIGHT)).unwrap();
        fake.mount(&clear).unwrap();
        assert_eq!(fake.lock("sdb1"), Err(DeviceError::Busy));
        fake.unmount(&clear, false).unwrap();
        fake.lock("sdb1").unwrap();
        let all = fake.drives().unwrap();
        assert!(
            all[0].locked && all[0].id == "sdb1",
            "back to the locked row"
        );
        assert!(matches!(fake.lock("sdb1"), Err(DeviceError::Failed(_))));
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn ejecting_an_unlocked_drive_unmounts_locks_then_powers_off() {
        let root = tmp("luks-eject");
        let fake = FakeDevices::new();
        fake.plug(luks(&root));
        let clear = fake.unlock("sdb1", &Passphrase::new(RIGHT)).unwrap();
        fake.mount(&clear).unwrap();
        let all = fake.drives().unwrap();
        let parts: Vec<Drive> = partitions(&all, &all[0]).into_iter().cloned().collect();
        assert_eq!(eject(fake.as_ref(), &parts, false), Ok(Ejected::PoweredOff));
        assert!(fake.drives().unwrap().is_empty());
        let log = fake.log.lock().unwrap().clone();
        assert_eq!(
            log,
            [
                "unlock sdb1",
                "mount sdb1-clear",
                "unmount sdb1-clear",
                "lock sdb1",
                "power-off sdb1"
            ]
        );
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn an_unlocked_drive_that_was_never_mounted_still_ejects() {
        let root = tmp("luks-eject-unmounted");
        let fake = FakeDevices::new();
        fake.plug(luks(&root));
        fake.unlock("sdb1", &Passphrase::new(RIGHT)).unwrap();
        let all = fake.drives().unwrap();
        assert!(all[0].mounted().is_none());
        // The power-off alone is refused while it is unlocked.
        assert_eq!(fake.power_off("sdb1"), Err(DeviceError::Busy));
        assert_eq!(eject(fake.as_ref(), &all, false), Ok(Ejected::PoweredOff));
        let log = fake.log.lock().unwrap().clone();
        assert_eq!(
            log[log.len() - 2..],
            ["lock sdb1", "power-off sdb1"],
            "nothing to unmount"
        );
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn a_refused_lock_stops_the_eject_before_the_power_off() {
        let root = tmp("luks-lock-refused");
        let fake = FakeDevices::new();
        let mut d = luks(&root);
        if let Some(l) = d.luks.as_mut() {
            l.lock_error = Some("Error locking /dev/dm-0 (/dev/sdb1): busy".into());
        }
        fake.plug(d);
        let clear = fake.unlock("sdb1", &Passphrase::new(RIGHT)).unwrap();
        fake.mount(&clear).unwrap();
        let all = fake.drives().unwrap();
        match eject(fake.as_ref(), &all, false) {
            Err(EjectError::Lock { part, .. }) => assert_eq!(part.id, "sdb1-clear"),
            other => panic!("expected the lock to stop it, got {other:?}"),
        }
        let log = fake.log.lock().unwrap().clone();
        assert!(!log.iter().any(|l| l.starts_with("power-off")), "{log:?}");
        assert_eq!(fake.drives().unwrap().len(), 1, "still plugged in");
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn a_locked_partition_beside_a_plain_one_powers_off_with_it() {
        let root = tmp("luks-sibling");
        let fake = FakeDevices::new();
        let mut plain = FakeDrive::stick("sdb2", "BACKUP", 16 << 30, &root.join("BACKUP"));
        plain.drive.drive = "sdb1-drive".into();
        fake.plug(luks(&root));
        fake.plug(plain);
        fake.mount("sdb2").unwrap();
        let all = fake.drives().unwrap();
        let backup = all.iter().find(|d| d.id == "sdb2").unwrap();
        let parts: Vec<Drive> = partitions(&all, backup).into_iter().cloned().collect();
        assert_eq!(parts.len(), 2, "the locked partition is on the same drive");
        assert_eq!(eject(fake.as_ref(), &parts, false), Ok(Ejected::PoweredOff));
        let log = fake.log.lock().unwrap().clone();
        assert!(!log.iter().any(|l| l.starts_with("lock")), "{log:?}");
        assert!(fake.drives().unwrap().is_empty());
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn unlocking_or_locking_is_no_arrival() {
        let root = tmp("luks-arrive");
        let fake = FakeDevices::new();
        fake.plug(luks(&root));
        let locked = fake.drives().unwrap();
        fake.unlock("sdb1", &Passphrase::new(RIGHT)).unwrap();
        let unlocked = fake.drives().unwrap();
        assert!(arrivals(Some(&locked), &unlocked).is_empty());
        fake.lock("sdb1").unwrap();
        let relocked = fake.drives().unwrap();
        assert!(arrivals(Some(&unlocked), &relocked).is_empty());
        // A different encrypted stick still toasts.
        let mut other = locked[0].clone();
        other.id = "sdc1".into();
        assert_eq!(
            arrivals(Some(&locked), &[locked[0].clone(), other]).len(),
            1
        );
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn the_lock_glyph_is_in_the_bundled_font() {
        let font = std::fs::read(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/assets/fonts/JetBrainsMonoNerdFont-Regular.ttf"
        ))
        .unwrap();
        let face = ttf_parser::Face::parse(&font, 0).unwrap();
        let ch = LOCK_GLYPH.chars().next().unwrap();
        assert_eq!(ch as u32, 0xF033E, "nf-md-lock");
        let id = face.glyph_index(ch).expect("the font has nf-md-lock");
        assert_eq!(face.glyph_name(id), Some("md-lock"));
    }

    #[test]
    fn the_unlock_glyphs_are_in_every_bundled_face() {
        let ui = include_str!("../ui/app.slint");
        for face in ["Regular", "Medium", "Bold"] {
            let font = std::fs::read(format!(
                "{}/assets/fonts/JetBrainsMonoNerdFont-{face}.ttf",
                env!("CARGO_MANIFEST_DIR")
            ))
            .unwrap();
            let f = ttf_parser::Face::parse(&font, 0).unwrap();
            for (glyph, cp, name) in [
                (LOCK_GLYPH, 0xF033E, "md-lock"),
                (UNLOCKED_GLYPH, 0xF033F, "md-lock_open"),
                (EYE_GLYPH, 0xF0208, "md-eye"),
                (EYE_OFF_GLYPH, 0xF0209, "md-eye_off"),
            ] {
                let ch = glyph.chars().next().unwrap();
                assert_eq!(ch as u32, cp, "{name}");
                let id = f
                    .glyph_index(ch)
                    .unwrap_or_else(|| panic!("{face} lacks {name}"));
                assert_eq!(f.glyph_name(id), Some(name), "{face}");
            }
        }
        // The dialog draws the eye pair itself.
        assert!(ui.contains(EYE_GLYPH) && ui.contains(EYE_OFF_GLYPH));
    }

    #[test]
    fn a_refused_key_reads_by_volume_type() {
        let e = DeviceError::WrongPassphrase;
        let t = |id_type: &str| unlock_error_text("SANDISK", &e, Crypto::of(id_type));
        assert_eq!(t("crypto_LUKS"), "Wrong passphrase. Try again.");
        assert_eq!(
            t("crypto_TCRYPT"),
            "Couldn't unlock SANDISK. This TrueCrypt volume may need options Robin doesn't support."
        );
        assert_eq!(
            t("BitLocker"),
            "Couldn't unlock SANDISK with this passphrase"
        );
        assert_eq!(
            t("crypto_unknown"),
            "Couldn't unlock SANDISK with this passphrase"
        );
        // Other failures read the same whatever the volume.
        let oom = DeviceError::Failed("Failed to activate device: Cannot allocate memory".into());
        for c in [Crypto::Luks, Crypto::TrueCrypt, Crypto::Other] {
            assert_eq!(
                unlock_error_text("SANDISK", &oom, c),
                "Not enough free memory to unlock SANDISK"
            );
        }
    }

    #[test]
    fn a_truecrypt_volume_lists_locked_with_its_own_type() {
        let root = tmp("tcrypt-list");
        let fake = FakeDevices::new();
        let mut d = luks(&root);
        if let Some(l) = d.luks.as_mut() {
            l.id_type = "crypto_TCRYPT".into();
        }
        fake.plug(d);
        let row = &fake.drives().unwrap()[0];
        assert!(row.locked);
        assert_eq!(row.fs_type, "crypto_TCRYPT");
        let e = fake.unlock("sdb1", &Passphrase::new(WRONG)).unwrap_err();
        assert_eq!(e, DeviceError::WrongPassphrase);
        assert_clean(
            "the error",
            &unlock_error_text("SANDISK", &e, Crypto::of(&row.fs_type)),
        );
        let _ = std::fs::remove_dir_all(root);
    }

    /// UDisks2's tree for one stick with a LUKS partition, locked or
    /// unlocked to a device-mapper filesystem.
    fn luks_tree(unlocked: bool) -> Managed {
        use zbus::zvariant::{ObjectPath, OwnedObjectPath, OwnedValue, Value};
        let v = |x: Value<'_>| OwnedValue::try_from(x).unwrap();
        let o = |p: &str| v(Value::from(ObjectPath::try_from(p).unwrap()));
        let bytes = |s: &str| {
            let mut b = s.as_bytes().to_vec();
            b.push(0);
            v(Value::from(b))
        };
        let props = |kv: Vec<(&str, OwnedValue)>| -> Props {
            kv.into_iter().map(|(k, v)| (k.to_string(), v)).collect()
        };
        let base = "/org/freedesktop/UDisks2";
        let drive = format!("{base}/drives/SanDisk_1");
        let part = format!("{base}/block_devices/sdb1");
        let dm = format!("{base}/block_devices/dm_2d0");
        let mut tree: Managed = HashMap::new();
        tree.insert(
            OwnedObjectPath::try_from(drive.as_str()).unwrap(),
            HashMap::from([(
                DRIVE.to_string(),
                props(vec![
                    ("ConnectionBus", v(Value::from("usb"))),
                    ("CanPowerOff", v(Value::from(true))),
                ]),
            )]),
        );
        tree.insert(
            OwnedObjectPath::try_from(part.as_str()).unwrap(),
            HashMap::from([
                (
                    BLOCK.to_string(),
                    props(vec![
                        ("Drive", o(&drive)),
                        ("Device", bytes("/dev/sdb1")),
                        ("IdType", v(Value::from("crypto_LUKS"))),
                        ("IdLabel", v(Value::from("SANDISK"))),
                        ("Size", v(Value::from(29_800_000_000u64))),
                        ("HintSystem", v(Value::from(false))),
                        ("CryptoBackingDevice", o("/")),
                    ]),
                ),
                (
                    ENCRYPTED.to_string(),
                    props(vec![(
                        "CleartextDevice",
                        o(if unlocked { dm.as_str() } else { "/" }),
                    )]),
                ),
            ]),
        );
        if unlocked {
            tree.insert(
                OwnedObjectPath::try_from(dm.as_str()).unwrap(),
                HashMap::from([
                    (
                        BLOCK.to_string(),
                        props(vec![
                            // Device-mapper: no drive, system by default.
                            ("Drive", o("/")),
                            ("Device", bytes("/dev/dm-0")),
                            ("IdType", v(Value::from("exfat"))),
                            ("IdLabel", v(Value::from("SANDISK"))),
                            ("HintSystem", v(Value::from(true))),
                            ("CryptoBackingDevice", o(&part)),
                        ]),
                    ),
                    (
                        FILESYSTEM.to_string(),
                        props(vec![(
                            "MountPoints",
                            v(Value::from(vec![b"/run/media/sky/SANDISK\0".to_vec()])),
                        )]),
                    ),
                ]),
            );
        }
        tree
    }

    #[test]
    fn udisks_lists_a_locked_luks_partition() {
        let all = drives_from_managed(&luks_tree(false));
        assert_eq!(all.len(), 1);
        let d = &all[0];
        assert!(d.locked && d.mounted().is_none());
        assert_eq!(d.id, "/org/freedesktop/UDisks2/block_devices/sdb1");
        assert_eq!(d.drive, "/org/freedesktop/UDisks2/drives/SanDisk_1");
        assert!(d.can_power_off);
    }

    #[test]
    fn udisks_lists_an_unlocked_one_as_its_filesystem_on_the_stick() {
        let all = drives_from_managed(&luks_tree(true));
        assert_eq!(all.len(), 1, "the encrypted block itself drops out");
        let d = &all[0];
        assert!(!d.locked);
        assert_eq!(d.id, "/org/freedesktop/UDisks2/block_devices/dm_2d0");
        assert_eq!(d.crypto, "/org/freedesktop/UDisks2/block_devices/sdb1");
        // The dm node's drive and hints come from the partition under it.
        assert_eq!(d.drive, "/org/freedesktop/UDisks2/drives/SanDisk_1");
        assert_eq!(d.mounted(), Some(Path::new("/run/media/sky/SANDISK")));
        assert_eq!(d.stable_id(), d.crypto);
    }
}
