//! The home trash as a view: what each item was, read from
//! `info/<name>.trashinfo` (freedesktop.org Trash spec 1.0).

use std::collections::HashMap;
use std::ffi::{OsStr, OsString};
use std::os::unix::ffi::{OsStrExt, OsStringExt};
use std::path::{Path, PathBuf};

/// Where a trashed item came from and when it was deleted.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Info {
    pub original: PathBuf,
    /// `YYYY-MM-DDThh:mm:ss`, local time as the spec writes it.
    pub deleted: String,
}

/// Parse one `.trashinfo`. A relative `Path=` is relative to `top`, the
/// top of the drive a per-drive trash lives on.
pub fn parse(text: &str, top: Option<&Path>) -> Option<Info> {
    let mut lines = text.lines().map(str::trim);
    if lines.by_ref().find(|l| !l.is_empty())? != "[Trash Info]" {
        return None;
    }
    let (mut path, mut deleted) = (None, None);
    for line in lines {
        if line.starts_with('[') {
            break;
        }
        if let Some(v) = line.strip_prefix("Path=") {
            path = Some(PathBuf::from(OsString::from_vec(unescape(v)?)));
        } else if let Some(v) = line.strip_prefix("DeletionDate=") {
            deleted = Some(v.to_string());
        }
    }
    let path = path?;
    let original = match (path.is_absolute(), top) {
        (true, _) => path,
        (false, Some(top)) => top.join(path),
        (false, None) => return None,
    };
    Some(Info {
        original,
        deleted: deleted.unwrap_or_default(),
    })
}

/// Undo the spec's URL escaping (%XX).
fn unescape(s: &str) -> Option<Vec<u8>> {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'%' {
            let hex = std::str::from_utf8(b.get(i + 1..i + 3)?).ok()?;
            out.push(u8::from_str_radix(hex, 16).ok()?);
            i += 3;
        } else {
            out.push(b[i]);
            i += 1;
        }
    }
    Some(out)
}

/// The info for every item in `trash` (the folder holding `files/` and
/// `info/`), keyed by the stored name. Items without readable info are
/// left out; the view shows them with blank columns.
pub fn read_all(trash: &Path, top: Option<&Path>) -> HashMap<OsString, Info> {
    let mut out = HashMap::new();
    let Ok(dir) = std::fs::read_dir(trash.join("info")) else {
        return out;
    };
    for entry in dir.flatten() {
        let name = entry.file_name();
        let Some(stored) = name.as_bytes().strip_suffix(b".trashinfo") else {
            continue;
        };
        // Info files are tiny; anything big is not one.
        if entry.metadata().map_or(true, |m| m.len() > 64 * 1024) {
            continue;
        }
        let Ok(text) = std::fs::read_to_string(entry.path()) else {
            continue;
        };
        if let Some(info) = parse(&text, top) {
            out.insert(OsStr::from_bytes(stored).to_os_string(), info);
        }
    }
    out
}

/// The Deleted column: `2026-10-29 10:00`, or blank when malformed.
pub fn deleted_text(deleted: &str) -> String {
    let b = deleted.as_bytes();
    let ok = b.len() >= 16
        && b[4] == b'-'
        && b[7] == b'-'
        && b[10] == b'T'
        && b[13] == b':'
        && [0, 1, 2, 3, 5, 6, 8, 9, 11, 12, 14, 15]
            .iter()
            .all(|&i| b[i].is_ascii_digit());
    if ok {
        format!("{} {}", &deleted[..10], &deleted[11..16])
    } else {
        String::new()
    }
}

/// The Original location column: the folder it came from, `~` for home.
pub fn location_text(original: &Path, home: Option<&Path>) -> String {
    let parent = original.parent().unwrap_or(original);
    if let Some(rest) = home.and_then(|h| parent.strip_prefix(h).ok()) {
        if rest.as_os_str().is_empty() {
            return "~".into();
        }
        return format!("~/{}", rest.display());
    }
    parent.display().to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_absolute_relative_and_escaped_paths() {
        let i = parse(
            "[Trash Info]\nPath=/home/sky/My%20Docs/a%25b.txt\nDeletionDate=2026-10-29T10:00:00\n",
            None,
        )
        .unwrap();
        assert_eq!(i.original, Path::new("/home/sky/My Docs/a%b.txt"));
        assert_eq!(i.deleted, "2026-10-29T10:00:00");
        let r = parse(
            "[Trash Info]\nPath=photos/x.jpg\n",
            Some(Path::new("/media/s")),
        )
        .unwrap();
        assert_eq!(r.original, Path::new("/media/s/photos/x.jpg"));
        assert!(parse("[Trash Info]\nPath=rel\n", None).is_none());
        assert!(parse("[Other]\nPath=/x\n", None).is_none());
        assert!(parse("[Trash Info]\nPath=/bad%zz\n", None).is_none());
    }

    #[test]
    fn columns_read_like_people_write_them() {
        assert_eq!(deleted_text("2026-10-29T10:00:00"), "2026-10-29 10:00");
        assert_eq!(deleted_text("yesterday"), "");
        let home = Path::new("/home/sky");
        assert_eq!(
            location_text(Path::new("/home/sky/Documents/a"), Some(home)),
            "~/Documents"
        );
        assert_eq!(location_text(Path::new("/home/sky/a"), Some(home)), "~");
        assert_eq!(location_text(Path::new("/srv/a"), Some(home)), "/srv");
    }

    #[test]
    fn reads_every_info_and_skips_junk() {
        let root = std::env::temp_dir().join(format!("robin-trashinfo-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let info = root.join("info");
        std::fs::create_dir_all(&info).unwrap();
        std::fs::write(
            info.join("a.txt.trashinfo"),
            "[Trash Info]\nPath=/x/a.txt\nDeletionDate=2026-01-02T03:04:05\n",
        )
        .unwrap();
        std::fs::write(info.join("b.trashinfo"), "garbage").unwrap();
        std::fs::write(info.join("notes"), "[Trash Info]\nPath=/x/n\n").unwrap();
        let all = read_all(&root, None);
        let _ = std::fs::remove_dir_all(&root);
        assert_eq!(all.len(), 1);
        assert_eq!(
            all[&OsString::from("a.txt")].original,
            Path::new("/x/a.txt")
        );
    }
}
