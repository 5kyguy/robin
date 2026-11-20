//! Phones and cameras through gvfs, the way GNOME's file manager sees
//! them.
//!
//! **Finding them** is D-Bus: gvfs runs one "remote volume monitor" per
//! kind of device on the session bus, `org.gtk.vfs.MTPVolumeMonitor` for
//! Android phones and `org.gtk.vfs.GPhoto2VolumeMonitor` for cameras, each
//! serving `org.gtk.Private.RemoteVolumeMonitor` at
//! `/org/gtk/Private/RemoteVolumeMonitor`. `List` returns their volumes
//! (name, `mtp://…/` activation URI), and `VolumeAdded`/`VolumeRemoved`
//! (plus the mount tracker's `Mounted`/`Unmounted`) say when to list again.
//!
//! **Mounting and unmounting** run `gio mount` / `gio mount -u`. The
//! monitor's own `VolumeMount` wants a GMountOperation object served back
//! by the caller, and a phone's mount belongs to gvfsd's mount tracker, not
//! to the monitor, so there is no D-Bus unmount next to it; `gio` speaks
//! both protocols and is part of GLib, which gvfs needs anyway.
//!
//! **Browsing** needs nothing gvfs-specific: gvfsd-fuse shows each mount as
//! a folder under `$XDG_RUNTIME_DIR/gvfs/` (`mtp:host=Google_Pixel_8_…`),
//! so Robin's reads, copies and viewer use it like any other folder.
//!
//! **Without gvfs**: no session bus means no [`Gvfs`] at all; a monitor
//! that isn't installed (`ServiceUnknown`) is skipped from then on; with
//! neither monitor DEVICES is exactly the drives-only phase 1 list.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{mpsc, Mutex};
use std::time::{Duration, Instant};

use crate::devices::{DeviceBackend, DeviceError, Drive, Kind};

const MONITORS: [(&str, Kind); 2] = [
    ("org.gtk.vfs.MTPVolumeMonitor", Kind::Phone),
    ("org.gtk.vfs.GPhoto2VolumeMonitor", Kind::Camera),
];
const RVM: &str = "org.gtk.Private.RemoteVolumeMonitor";
const RVM_PATH: &str = "/org/gtk/Private/RemoteVolumeMonitor";
const TRACKER: &str = "org.gtk.vfs.MountTracker";
/// Ids this backend hands out: `gvfs:` and the volume's activation URI.
const ID_PREFIX: &str = "gvfs:";
/// A phone can take a while to answer its first MTP session.
const GIO_TIMEOUT: Duration = Duration::from_secs(60);
/// How long a fresh mount may take to show up under the FUSE folder.
const FUSE_WAIT: Duration = Duration::from_secs(3);

/// One volume from a monitor's `List`: the parts Robin uses.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Volume {
    pub name: String,
    /// `mtp://Google_Pixel_8_37281FDJH00ABC/`.
    pub uri: String,
    pub can_mount: bool,
}

/// Pull the volumes out of a `List` reply,
/// `(a(ssssbbbbbbbbuasa{ss}sa{sv}) drives, a(ssssssbbssa{ss}sa{sv})
/// volumes, a(ssssssbsassa{sv}) mounts)`. A volume is `(id, name, gicon,
/// symbolic_gicon, uuid, activation_uri, can_mount, should_automount,
/// drive_id, mount_id, identifiers, sort_key, expansion)`.
pub fn volumes_from_list(reply: &zbus::zvariant::Structure<'_>) -> Vec<Volume> {
    use zbus::zvariant::Value;
    let Some(Value::Array(vols)) = reply.fields().get(1) else {
        return Vec::new();
    };
    vols.iter()
        .filter_map(|v| {
            let v = match v {
                Value::Value(inner) => inner.as_ref(),
                v => v,
            };
            let Value::Structure(s) = v else { return None };
            let f = s.fields();
            let text = |i: usize| match f.get(i) {
                Some(Value::Str(s)) => Some(s.as_str().to_string()),
                _ => None,
            };
            let can_mount = matches!(f.get(6), Some(Value::Bool(true)));
            Some(Volume {
                name: text(1)?,
                uri: text(5)?,
                can_mount,
            })
        })
        .collect()
}

/// `mtp://Google_Pixel_8_X/` → (`mtp`, `Google_Pixel_8_X`), host decoded.
pub fn uri_host(uri: &str) -> Option<(&str, String)> {
    let (scheme, rest) = uri.split_once("://")?;
    let host = rest.split('/').next().unwrap_or("");
    (!host.is_empty()).then(|| (scheme, percent_decode(host)))
}

/// The host in a gvfs FUSE folder name: `mtp:host=Google_Pixel_8_X`
/// (other keys may follow after a comma), decoded.
pub fn fuse_name_host(name: &str) -> String {
    let Some((_, spec)) = name.split_once(':') else {
        return String::new();
    };
    spec.split(',')
        .find_map(|kv| kv.strip_prefix("host="))
        .map(percent_decode)
        .unwrap_or_default()
}

fn percent_decode(s: &str) -> String {
    let b = s.as_bytes();
    let hex = |c: u8| (c as char).to_digit(16);
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'%' && i + 2 < b.len() {
            if let (Some(h), Some(l)) = (hex(b[i + 1]), hex(b[i + 2])) {
                out.push((h * 16 + l) as u8);
                i += 3;
                continue;
            }
        }
        out.push(b[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// The FUSE folder for `uri` among `names` (the entries of the gvfs FUSE
/// root): same scheme, same decoded host.
pub fn fuse_dir_for<'a>(uri: &str, names: &'a [String]) -> Option<&'a String> {
    let (scheme, host) = uri_host(uri)?;
    names.iter().find(|n| {
        n.split_once(':')
            .is_some_and(|(s, _)| s == scheme && fuse_name_host(n) == host)
    })
}

/// What `gio mount` printed on failure, as the toast should say it:
/// "gio: mtp://X/: Unable to open MTP device" → the part after the URI.
pub fn gio_error(stderr: &str) -> DeviceError {
    let line = stderr
        .lines()
        .map(str::trim)
        .find(|l| !l.is_empty())
        .unwrap_or("");
    let msg = line.strip_prefix("gio:").unwrap_or(line).trim();
    // Drop the location the message starts with.
    let msg = match msg.find("://") {
        Some(_) => msg.split_once("/: ").map_or(msg, |(_, rest)| rest),
        None => msg,
    }
    .trim()
    .to_string();
    let low = msg.to_lowercase();
    if low.contains("unable to open mtp device") || low.contains("unable to connect to mtp device")
    {
        DeviceError::Locked
    } else if low.contains("busy") {
        DeviceError::Busy
    } else if low.contains("not authorized") || low.contains("permission denied") {
        DeviceError::NotAuthorized(msg)
    } else if low.contains("not supported") {
        DeviceError::Unsupported
    } else if msg.is_empty() {
        DeviceError::Failed("gio mount failed".into())
    } else {
        DeviceError::Failed(msg)
    }
}

/// Run `gio` with a deadline; `Err` carries what it printed.
fn run_gio(args: &[&str]) -> Result<(), DeviceError> {
    use std::io::Read;
    let mut child = std::process::Command::new("gio")
        .args(args)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .map_err(|e| match e.kind() {
            std::io::ErrorKind::NotFound => {
                DeviceError::Failed("gio isn't installed (it comes with GLib)".into())
            }
            _ => DeviceError::Failed(format!("couldn't run gio: {e}")),
        })?;
    let deadline = Instant::now() + GIO_TIMEOUT;
    let status = loop {
        match child.try_wait() {
            Ok(Some(st)) => break st,
            Ok(None) if Instant::now() < deadline => std::thread::sleep(Duration::from_millis(50)),
            Ok(None) => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(DeviceError::Failed("the phone didn't answer".into()));
            }
            Err(e) => return Err(DeviceError::Failed(e.to_string())),
        }
    };
    if status.success() {
        return Ok(());
    }
    let mut err = String::new();
    if let Some(mut s) = child.stderr.take() {
        let _ = s.read_to_string(&mut err);
    }
    Err(gio_error(&err))
}

/// gvfs's monitors on the session bus.
pub struct Gvfs {
    conn: zbus::blocking::Connection,
    /// `$XDG_RUNTIME_DIR/gvfs`, where gvfsd-fuse shows mounts.
    fuse_root: PathBuf,
    /// Monitors that aren't installed, so aren't asked again.
    absent: Mutex<Vec<&'static str>>,
    /// Each monitor's last good listing, kept through a failed call.
    last: Mutex<HashMap<&'static str, Vec<Drive>>>,
}

impl Gvfs {
    /// Connect to the session bus. Nothing is asked yet: the first
    /// [`DeviceBackend::drives`] (on a worker) finds which monitors exist.
    pub fn session() -> Option<Self> {
        let conn = zbus::blocking::Connection::session().ok()?;
        let fuse_root = std::env::var_os("XDG_RUNTIME_DIR")
            .map(PathBuf::from)
            .filter(|p| p.is_absolute())
            .unwrap_or_else(|| {
                PathBuf::from(format!("/run/user/{}", rustix::process::getuid().as_raw()))
            })
            .join("gvfs");
        Some(Self {
            conn,
            fuse_root,
            absent: Mutex::new(Vec::new()),
            last: Mutex::new(HashMap::new()),
        })
    }

    fn fuse_names(&self) -> Vec<String> {
        std::fs::read_dir(&self.fuse_root)
            .map(|rd| {
                rd.flatten()
                    .map(|e| e.file_name().to_string_lossy().into_owned())
                    .collect()
            })
            .unwrap_or_default()
    }

    fn list(&self, bus: &str) -> zbus::Result<Vec<Volume>> {
        let reply = self
            .conn
            .call_method(Some(bus), RVM_PATH, Some(RVM), "List", &())?;
        let body = reply.body();
        let s: zbus::zvariant::Structure<'_> = body.deserialize()?;
        Ok(volumes_from_list(&s))
    }

    fn uri(id: &str) -> Result<&str, DeviceError> {
        id.strip_prefix(ID_PREFIX)
            .ok_or_else(|| DeviceError::Failed("No such device".into()))
    }

    fn mounted_at(&self, uri: &str) -> Option<PathBuf> {
        fuse_dir_for(uri, &self.fuse_names()).map(|n| self.fuse_root.join(n))
    }

    /// Mount, then wait for gvfsd-fuse to show the folder.
    fn mount_uri(&self, uri: &str) -> Result<PathBuf, DeviceError> {
        if let Some(at) = self.mounted_at(uri) {
            return Ok(at);
        }
        match run_gio(&["mount", uri]) {
            Ok(()) => {}
            Err(DeviceError::Failed(m)) if m.to_lowercase().contains("already mounted") => {}
            Err(e) => return Err(e),
        }
        let until = Instant::now() + FUSE_WAIT;
        loop {
            if let Some(at) = self.mounted_at(uri) {
                return Ok(at);
            }
            if Instant::now() >= until {
                return Err(DeviceError::Failed(format!(
                    "it mounted, but {} has no folder for it (is gvfs-fuse installed?)",
                    self.fuse_root.display()
                )));
            }
            std::thread::sleep(Duration::from_millis(100));
        }
    }
}

/// A phone's top folder lists its storages; none means it hasn't given
/// access yet.
fn lists_nothing(dir: &Path) -> bool {
    std::fs::read_dir(dir).map_or(true, |mut rd| rd.next().is_none())
}

/// The device list from monitor volumes and the FUSE folders present.
pub fn drives_from_volumes(
    kind: Kind,
    vols: &[Volume],
    fuse_root: &Path,
    fuse_names: &[String],
    empty: impl Fn(&Path) -> bool,
) -> Vec<Drive> {
    vols.iter()
        .filter(|v| uri_host(&v.uri).is_some())
        .map(|v| {
            let at = fuse_dir_for(&v.uri, fuse_names).map(|n| fuse_root.join(n));
            let waiting = kind == Kind::Phone && at.as_deref().is_some_and(&empty);
            Drive {
                id: format!("{ID_PREFIX}{}", v.uri),
                drive: String::new(),
                label: v.name.clone(),
                device: v.uri.clone(),
                size: 0,
                fs_type: v.uri.split("://").next().unwrap_or("").to_string(),
                mount_points: at.into_iter().collect(),
                read_only: false,
                can_power_off: false,
                kind,
                waiting,
                no_write: Vec::new(),
            }
        })
        .collect()
}

impl DeviceBackend for Gvfs {
    fn drives(&self) -> Result<Vec<Drive>, DeviceError> {
        let names = self.fuse_names();
        let mut out = Vec::new();
        for (bus, kind) in MONITORS {
            if self
                .absent
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .contains(&bus)
            {
                continue;
            }
            let got = match self.list(bus) {
                Ok(vols) => {
                    let d =
                        drives_from_volumes(kind, &vols, &self.fuse_root, &names, lists_nothing);
                    self.last
                        .lock()
                        .unwrap_or_else(|e| e.into_inner())
                        .insert(bus, d.clone());
                    d
                }
                Err(zbus::Error::MethodError(name, _, _))
                    if name.as_str() == "org.freedesktop.DBus.Error.ServiceUnknown" =>
                {
                    // Not installed (no gvfs-mtp, or no gvfs): never ask again.
                    self.absent
                        .lock()
                        .unwrap_or_else(|e| e.into_inner())
                        .push(bus);
                    Vec::new()
                }
                // A monitor restarting: keep what it last said.
                Err(_) => self
                    .last
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .get(bus)
                    .cloned()
                    .unwrap_or_default(),
            };
            out.extend(got);
        }
        Ok(out)
    }

    fn mount(&self, id: &str) -> Result<PathBuf, DeviceError> {
        self.mount_uri(Self::uri(id)?)
    }

    fn unmount(&self, id: &str, force: bool) -> Result<(), DeviceError> {
        let uri = Self::uri(id)?;
        if self.mounted_at(uri).is_none() {
            return Ok(());
        }
        let mut args = vec!["mount", "-u"];
        if force {
            args.push("-f");
        }
        args.push(uri);
        run_gio(&args)
    }

    /// A phone can't be powered off; unmounted, it is safe to pull.
    fn power_off(&self, _id: &str) -> Result<(), DeviceError> {
        Err(DeviceError::Unsupported)
    }

    fn owns(&self, id: &str) -> bool {
        id.starts_with(ID_PREFIX)
    }

    /// Look again; a phone that still lists nothing is remounted, since
    /// some only offer their storages to a fresh MTP session.
    fn retry(&self, id: &str) -> Result<PathBuf, DeviceError> {
        let uri = Self::uri(id)?;
        if let Some(at) = self.mounted_at(uri) {
            if !lists_nothing(&at) {
                return Ok(at);
            }
            run_gio(&["mount", "-u", uri])?;
        }
        self.mount_uri(uri)
    }

    fn watch(&self) -> Option<mpsc::Receiver<()>> {
        let mut rules = Vec::new();
        for (bus, _) in MONITORS {
            rules.push(
                zbus::MatchRule::builder()
                    .msg_type(zbus::message::Type::Signal)
                    .interface(RVM)
                    .ok()?
                    // Every RVM signal starts with the monitor's bus name.
                    // Matched on that rather than the sender: a monitor
                    // started (or restarted) later gets a new unique name.
                    .arg(0, bus)
                    .ok()?
                    .build(),
            );
        }
        rules.push(
            zbus::MatchRule::builder()
                .msg_type(zbus::message::Type::Signal)
                .interface(TRACKER)
                .ok()?
                .build(),
        );
        let (tx, rx) = mpsc::channel();
        let mut any = false;
        for rule in rules {
            let Ok(iter) =
                zbus::blocking::MessageIterator::for_match_rule(rule, &self.conn, Some(64))
            else {
                continue;
            };
            any = true;
            let tx = tx.clone();
            std::thread::spawn(move || {
                for msg in iter {
                    if msg.is_ok() && tx.send(()).is_err() {
                        break;
                    }
                }
            });
        }
        any.then_some(rx)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use zbus::zvariant::{Structure, Value};

    fn volume(name: &str, uri: &str) -> Value<'static> {
        let ids: HashMap<String, String> = HashMap::new();
        let exp: HashMap<String, Value<'static>> = HashMap::new();
        Value::from((
            "0x55d1".to_string(),
            name.to_string(),
            ". GThemedIcon phone".to_string(),
            "".to_string(),
            "".to_string(),
            uri.to_string(),
            true,
            false,
            "".to_string(),
            "".to_string(),
            ids,
            "gvfs.time_detected_usec.1".to_string(),
            exp,
        ))
    }

    #[test]
    fn a_list_reply_gives_each_volume_name_and_uri() {
        let vols = vec![
            volume("Pixel 8", "mtp://Google_Pixel_8_37281FDJH00ABC/"),
            volume("Galaxy A54", "mtp://SAMSUNG_SAMSUNG_Android_R5CW/"),
        ];
        let vols = zbus::zvariant::Array::from(vols);
        let empty = || zbus::zvariant::Array::from(Vec::<i32>::new());
        let reply = Structure::from((empty(), vols, empty()));
        let got = volumes_from_list(&reply);
        assert_eq!(
            got,
            [
                Volume {
                    name: "Pixel 8".into(),
                    uri: "mtp://Google_Pixel_8_37281FDJH00ABC/".into(),
                    can_mount: true,
                },
                Volume {
                    name: "Galaxy A54".into(),
                    uri: "mtp://SAMSUNG_SAMSUNG_Android_R5CW/".into(),
                    can_mount: true,
                },
            ]
        );
    }

    #[test]
    fn a_volume_finds_its_fuse_folder_by_scheme_and_host() {
        let names = vec![
            "sftp:host=nas.local".to_string(),
            "mtp:host=Google_Pixel_8_37281FDJH00ABC".to_string(),
            "gphoto2:host=%5Busb%3A001%2C007%5D".to_string(),
        ];
        assert_eq!(
            fuse_dir_for("mtp://Google_Pixel_8_37281FDJH00ABC/", &names).map(String::as_str),
            Some("mtp:host=Google_Pixel_8_37281FDJH00ABC")
        );
        assert_eq!(
            fuse_dir_for("gphoto2://%5Busb%3A001%2C007%5D/", &names).map(String::as_str),
            Some("gphoto2:host=%5Busb%3A001%2C007%5D")
        );
        assert_eq!(fuse_dir_for("mtp://Other/", &names), None);
        assert_eq!(
            fuse_name_host("gphoto2:host=%5Busb%3A001%2C007%5D"),
            "[usb:001,007]"
        );
    }

    #[test]
    fn volumes_become_phone_rows_mounted_or_not_and_waiting_when_empty() {
        let root = Path::new("/run/user/1000/gvfs");
        let vols = vec![
            Volume {
                name: "Pixel 8".into(),
                uri: "mtp://Google_Pixel_8_X/".into(),
                can_mount: true,
            },
            Volume {
                name: "Galaxy".into(),
                uri: "mtp://Galaxy_Y/".into(),
                can_mount: true,
            },
        ];
        let names = vec!["mtp:host=Google_Pixel_8_X".to_string()];
        let d = drives_from_volumes(Kind::Phone, &vols, root, &names, |_| true);
        assert_eq!(d.len(), 2);
        assert_eq!(d[0].id, "gvfs:mtp://Google_Pixel_8_X/");
        assert_eq!(d[0].name(), "Pixel 8");
        assert_eq!(
            d[0].mounted(),
            Some(root.join("mtp:host=Google_Pixel_8_X").as_path())
        );
        assert!(d[0].waiting, "mounted and its top folder lists nothing");
        assert!(d[1].mounted().is_none() && !d[1].waiting);
        assert!(!d[0].can_power_off && d[0].drive.is_empty());
        let cams = drives_from_volumes(Kind::Camera, &vols, root, &names, |_| true);
        assert!(!cams[0].waiting, "only phones wait for access");
    }

    #[test]
    fn gio_errors_read_like_the_toast_says_them() {
        assert_eq!(
            gio_error("gio: mtp://Google_Pixel_8_X/: Unable to open MTP device “001,012”\n"),
            DeviceError::Locked
        );
        assert_eq!(
            gio_error("gio: mtp://X/: Device or resource busy"),
            DeviceError::Busy
        );
        assert_eq!(
            gio_error("gio: mtp://X/: Operation not supported"),
            DeviceError::Unsupported
        );
        assert_eq!(
            gio_error("gio: mtp://X/: Device not found"),
            DeviceError::Failed("Device not found".into())
        );
        assert_eq!(
            gio_error(""),
            DeviceError::Failed("gio mount failed".into())
        );
    }

    #[test]
    fn ids_route_to_gvfs_only_with_its_prefix() {
        assert_eq!(Gvfs::uri("gvfs:mtp://X/"), Ok("mtp://X/"));
        assert!(Gvfs::uri("/org/freedesktop/UDisks2/block_devices/sdb1").is_err());
    }

    /// Against scripts/fake-mtp-monitor.py on a private session bus (see
    /// its header): the real zbus List call, parsing and signal match.
    #[test]
    #[ignore = "needs scripts/fake-mtp-monitor.py on a private session bus"]
    fn live_monitor_lists_the_phone_and_pings_the_watcher() {
        if std::env::var_os("ROBIN_GVFS_LIVE").is_none() {
            return;
        }
        let run = std::env::temp_dir().join(format!("robin-gvfs-live-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&run);
        std::fs::create_dir_all(run.join("gvfs/mtp:host=Google_Pixel_8_X/Internal shared storage"))
            .unwrap();
        std::env::set_var("XDG_RUNTIME_DIR", &run);
        let g = Gvfs::session().expect("a session bus");
        let drives = g.drives().unwrap();
        assert_eq!(drives.len(), 1, "{drives:?}");
        let d = &drives[0];
        assert_eq!((d.name(), d.kind), ("Pixel 8".to_string(), Kind::Phone));
        assert_eq!(d.id, "gvfs:mtp://Google_Pixel_8_X/");
        assert_eq!(
            d.mounted(),
            Some(run.join("gvfs/mtp:host=Google_Pixel_8_X").as_path())
        );
        assert!(!d.waiting, "a storage shows, so access was allowed");
        assert!(g.owns(&d.id) && !g.owns("/org/freedesktop/UDisks2/block_devices/sdb1"));
        let rx = g.watch().expect("match rules added");
        rx.recv_timeout(Duration::from_secs(5))
            .expect("VolumeAdded reaches the watcher");
        let _ = std::fs::remove_dir_all(run);
    }
}
