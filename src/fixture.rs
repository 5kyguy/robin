//! Fictional tree used by `--demo` and by tests.
//!
//! Names are made up. Nothing here is a real user's files.

use std::fs;
use std::io::Write;
use std::os::unix::fs::symlink;
use std::path::Path;

use crate::error::{Error, Result};

/// 1×1 PNG. The browser draws its own thumbs; this only has to be a real file.
const PIXEL_PNG: &[u8] = &[
    0x89, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a, 0x00, 0x00, 0x00, 0x0d, 0x49, 0x48, 0x44, 0x52,
    0x00, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0x01, 0x08, 0x02, 0x00, 0x00, 0x00, 0x90, 0x77, 0x53,
    0xde, 0x00, 0x00, 0x00, 0x0c, 0x49, 0x44, 0x41, 0x54, 0x08, 0xd7, 0x63, 0xf8, 0xcf, 0xc0, 0x00,
    0x00, 0x00, 0x03, 0x00, 0x01, 0x00, 0x05, 0xfe, 0xd4, 0xef, 0x00, 0x00, 0x00, 0x00, 0x49, 0x45,
    0x4e, 0x44, 0xae, 0x42, 0x60, 0x82,
];

/// Write the demo tree into `root`, which is created.
pub fn materialize(root: &Path) -> Result<()> {
    fs::create_dir_all(root).map_err(|e| Error::io(root, e))?;
    if !is_marked(root) {
        // Only ever claim an empty folder: the tree below is rewritten,
        // so a typo'd --out must not land in someone's real directory.
        let used = fs::read_dir(root)
            .map_err(|e| Error::io(root, e))?
            .next()
            .is_some();
        if used {
            return Err(Error::Message(format!(
                "{} is not empty and not a robin fixture",
                root.display()
            )));
        }
        let marker = root.join(MARKER);
        if marker.symlink_metadata().is_ok() {
            return Err(Error::Message(format!(
                "{} exists and is not a fixture marker",
                marker.display()
            )));
        }
        write(root, MARKER, MARKER_TEXT)?;
    }
    // The user's home sits below the fixture root, so mounts and the
    // marker live outside HOME the way they do on a real system.
    let top = root;
    let home = home(top);
    let root = home.as_path();
    fs::create_dir_all(root).map_err(|e| Error::io(root, e))?;
    let dirs = [
        "Desktop",
        "Documents/empty",
        "Downloads",
        "Pictures/raw-exports",
        "Pictures/tatooine-trip",
        "Videos",
        "Music",
        "Projects",
        "Templates",
    ];
    for dir in dirs {
        let path = root.join(dir);
        fs::create_dir_all(&path).map_err(|e| Error::io(&path, e))?;
    }
    write(
        root,
        "Documents/notes.txt",
        b"field notes from the dune sea\n",
    )?;
    write(root, "Downloads/readme.txt", b"nothing in here is real\n")?;
    write(root, "Projects/shot-list.md", b"# shot list\n")?;
    write(root, "Pictures/palette.toml", b"accent = \"cyan\"\n")?;
    write(root, "Pictures/dune-sunset-0412.jpg", PIXEL_PNG)?;
    write(root, "Pictures/dune-sunset-0413.jpg", PIXEL_PNG)?;
    write(root, "Pictures/r2-portrait-001.png", PIXEL_PNG)?;
    write(root, "Pictures/raw-exports/dune-sunset-0412.dng", PIXEL_PNG)?;
    write(root, "Pictures/raw-exports/dune-sunset-0413.dng", PIXEL_PNG)?;
    write(root, "Pictures/raw-exports/dune-sunset-0414.dng", PIXEL_PNG)?;
    write(root, "Pictures/tatooine-trip/mos-espa-01.jpg", PIXEL_PNG)?;
    write(root, "Pictures/tatooine-trip/mos-espa-02.jpg", PIXEL_PNG)?;
    write(root, "Videos/speeder-run.mp4", b"not a real video\n")?;
    seed_home(top, root)?;
    let link = root.join("Pictures/latest");
    if link.symlink_metadata().is_err() {
        symlink("dune-sunset-0412.jpg", &link).map_err(|e| Error::io(&link, e))?;
    }
    Ok(())
}

/// The marker file every fixture carries, and its exact contents.
pub const MARKER: &str = ".robin-fixture";
pub const MARKER_TEXT: &[u8] = b"robin render fixture v1\n";

/// The fixture user's home: `{root}/home/sky`.
pub fn home(root: &Path) -> std::path::PathBuf {
    root.join("home/sky")
}

/// Where the fake mount table lives inside a fixture.
pub fn mounts_dir(root: &Path) -> std::path::PathBuf {
    root.join(".robin-mounts")
}

/// True only when `root` holds a regular marker file with the known text.
/// A folder or a symlink by that name does not count.
pub fn is_marked(root: &Path) -> bool {
    let path = root.join(MARKER);
    path.symlink_metadata()
        .is_ok_and(|m| m.file_type().is_file())
        && fs::read(&path).is_ok_and(|b| b == MARKER_TEXT)
}

/// The XDG dirs, one trashed file, a folder nobody may read, and a mount
/// table that puts the fixture on a home disk and a fake SANDISK stick.
fn seed_home(top: &Path, root: &Path) -> Result<()> {
    for dir in [".config", ".cache", ".local/share/Trash/files"] {
        let path = root.join(dir);
        fs::create_dir_all(&path).map_err(|e| Error::io(&path, e))?;
    }
    let stick = top.join("media/sky/SANDISK");
    fs::create_dir_all(&stick).map_err(|e| Error::io(&stick, e))?;
    write(root, ".local/share/Trash/files/old-draft.txt", b"draft\n")?;
    write(
        root,
        ".config/user-dirs.dirs",
        b"XDG_DESKTOP_DIR=\"$HOME/Desktop\"\nXDG_DOCUMENTS_DIR=\"$HOME/Documents\"\nXDG_DOWNLOAD_DIR=\"$HOME/Downloads\"\nXDG_MUSIC_DIR=\"$HOME/Music\"\nXDG_PICTURES_DIR=\"$HOME/Pictures\"\nXDG_VIDEOS_DIR=\"$HOME/Videos\"\nXDG_TEMPLATES_DIR=\"$HOME/Templates\"\n",
    )?;
    let info = format!(
        "[Trash Info]\nPath={}/Documents/old-draft.txt\nDeletionDate=2026-10-29T10:00:00\n",
        root.display()
    );
    write(
        root,
        ".local/share/Trash/info/old-draft.txt.trashinfo",
        info.as_bytes(),
    )?;
    // Two more trashed items, one a folder, so the Trash view has columns to show.
    write(root, ".local/share/Trash/files/q3-budget.ods", b"budget\n")?;
    write(
        root,
        ".local/share/Trash/files/old-screenshots/shot-1.png",
        b"png\n",
    )?;
    for (name, path, when) in [
        (
            "q3-budget.ods",
            "Documents/Finance/q3-budget.ods",
            "2026-10-30T18:45:00",
        ),
        (
            "old-screenshots",
            "Pictures/old-screenshots",
            "2026-10-27T09:12:00",
        ),
    ] {
        let info = format!(
            "[Trash Info]\nPath={}/{path}\nDeletionDate={when}\n",
            root.display()
        );
        write(
            root,
            &format!(".local/share/Trash/info/{name}.trashinfo"),
            info.as_bytes(),
        )?;
    }
    // An empty trash for the Trash view's empty state.
    let empty = top.join("empty-trash/files");
    fs::create_dir_all(&empty).map_err(|e| Error::io(&empty, e))?;
    let t = top.display();
    let mountinfo = format!(
        "22 1 259:2 / / rw - ext4 /dev/nvme0n1p2 rw\n\
         40 22 259:3 / {t}/home rw - ext4 /dev/nvme0n1p3 rw\n\
         41 22 8:1 / {t}/media/sky/SANDISK rw - exfat /dev/sda1 rw\n"
    );
    let space = format!(
        "/ 512000000000 128000000000\n\
         {t}/home 1800000000000 812000000000\n\
         {t}/media/sky/SANDISK 64000000000 6000000000\n"
    );
    let m = mounts_dir(top);
    fs::create_dir_all(&m).map_err(|e| Error::io(&m, e))?;
    write(&m, "mountinfo", mountinfo.as_bytes())?;
    write(&m, "space", space.as_bytes())?;
    write(&m, "removable", b"/dev/sda1\n")?;
    let secret = root.join("secret");
    fs::create_dir_all(&secret).map_err(|e| Error::io(&secret, e))?;
    // chmod follows links: lock only a real folder, never a link's target.
    if !secret.symlink_metadata().is_ok_and(|m| m.is_dir()) {
        return Err(Error::Message(format!(
            "{} is not a real folder",
            secret.display()
        )));
    }
    set_mode(&secret, 0o000)
}

fn set_mode(path: &Path, mode: u32) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(path, fs::Permissions::from_mode(mode)).map_err(|e| Error::io(path, e))
}

/// Stamp every file and folder (except the unreadable one) with the same
/// mtime, so a frozen clock renders the same "Modified" column every run.
pub fn freeze_times(root: &Path, secs: i64) -> Result<()> {
    let when = std::time::UNIX_EPOCH + std::time::Duration::from_secs(secs as u64);
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let Ok(read) = fs::read_dir(&dir) else {
            continue;
        };
        for entry in read.flatten() {
            let path = entry.path();
            let Ok(kind) = entry.file_type() else {
                continue;
            };
            if kind.is_symlink() {
                // The link's own time, not its target's, is what a listing shows.
                use rustix::fs::{utimensat, AtFlags, Timespec, Timestamps, CWD};
                let t = Timespec {
                    tv_sec: secs,
                    tv_nsec: 0,
                };
                let times = Timestamps {
                    last_access: t,
                    last_modification: t,
                };
                utimensat(CWD, &path, &times, AtFlags::SYMLINK_NOFOLLOW)
                    .map_err(|e| Error::io(&path, e))?;
                continue;
            }
            if kind.is_dir() {
                stack.push(path.clone());
            }
            if let Ok(f) = fs::File::open(&path) {
                f.set_modified(when).map_err(|e| Error::io(&path, e))?;
            }
        }
    }
    Ok(())
}

/// Delete a fixture, reopening its unreadable folder first.
pub fn remove(root: &Path) -> Result<()> {
    let secret = home(root).join("secret");
    // Only a real folder: chmod follows links, and a link could point out.
    if secret.symlink_metadata().is_ok_and(|m| m.is_dir()) {
        set_mode(&secret, 0o755)?;
    }
    match fs::remove_dir_all(root) {
        Err(e) if e.kind() != std::io::ErrorKind::NotFound => Err(Error::io(root, e)),
        _ => Ok(()),
    }
}

fn write(root: &Path, rel: &str, bytes: &[u8]) -> Result<()> {
    let path = root.join(rel);
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).map_err(|e| Error::io(parent, e))?;
    }
    let mut file = fs::File::create(&path).map_err(|e| Error::io(&path, e))?;
    file.write_all(bytes).map_err(|e| Error::io(&path, e))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn demo_tree_has_the_fictional_pictures() {
        let root = std::env::temp_dir().join(format!("robin-fixture-{}", std::process::id()));
        let _ = remove(&root);
        materialize(&root).unwrap();
        let top = root.clone();
        let root = home(&top);
        assert!(root
            .join(".local/share/Trash/info/old-draft.txt.trashinfo")
            .is_file());
        assert!(
            std::fs::read_dir(root.join("secret")).is_err(),
            "seeded unreadable"
        );
        assert!(root.join("Pictures/palette.toml").is_file());
        assert!(root.join("Pictures/dune-sunset-0412.jpg").is_file());
        assert!(root.join("Documents/empty").is_dir());
        assert!(root
            .join("Pictures/latest")
            .symlink_metadata()
            .unwrap()
            .file_type()
            .is_symlink());
        let n = |d: &str| std::fs::read_dir(root.join(d)).unwrap().count();
        assert_eq!(
            (n("Pictures/raw-exports"), n("Pictures/tatooine-trip")),
            (3, 2)
        );
        freeze_times(&top, 1_000_000).unwrap();
        let m = std::fs::metadata(root.join("Pictures/palette.toml")).unwrap();
        let secs = m
            .modified()
            .unwrap()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap();
        assert_eq!(secs.as_secs(), 1_000_000);
        remove(&top).unwrap();
        assert!(!top.exists());
    }

    #[test]
    fn a_folder_or_link_named_like_the_marker_is_not_a_marker() {
        let root = std::env::temp_dir().join(format!("robin-marker-{}", std::process::id()));
        let _ = remove(&root);
        fs::create_dir_all(root.join(MARKER)).unwrap();
        assert!(!is_marked(&root), "folder");
        fs::remove_dir(root.join(MARKER)).unwrap();
        fs::write(root.join("real"), MARKER_TEXT).unwrap();
        symlink("real", root.join(MARKER)).unwrap();
        assert!(!is_marked(&root), "symlink");
        fs::remove_file(root.join(MARKER)).unwrap();
        fs::write(root.join(MARKER), b"something else").unwrap();
        assert!(!is_marked(&root), "wrong text");
        fs::write(root.join(MARKER), MARKER_TEXT).unwrap();
        assert!(is_marked(&root));
        remove(&root).unwrap();
    }

    #[test]
    fn a_used_folder_is_never_claimed() {
        let root = std::env::temp_dir().join(format!("robin-used-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(&root).unwrap();
        fs::write(root.join("notes.txt"), b"mine").unwrap();
        assert!(materialize(&root).is_err());
        assert!(!root.join(MARKER).exists(), "no marker written");
        assert!(!home(&root).exists(), "no tree written");
        fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn a_secret_link_is_never_chmodded() {
        let root = std::env::temp_dir().join(format!("robin-secretlink-{}", std::process::id()));
        let outside = std::env::temp_dir().join(format!("robin-outside-{}", std::process::id()));
        let _ = remove(&root);
        fs::create_dir_all(&outside).unwrap();
        fs::create_dir_all(home(&root)).unwrap();
        symlink(&outside, home(&root).join("secret")).unwrap();
        fs::write(root.join(MARKER), MARKER_TEXT).unwrap();
        assert!(materialize(&root).is_err());
        assert!(fs::read_dir(&outside).is_ok(), "target untouched");
        fs::remove_file(home(&root).join("secret")).unwrap();
        remove(&root).unwrap();
        fs::remove_dir_all(&outside).unwrap();
    }
}
