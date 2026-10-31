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
    write(root, "Videos/speeder-run.mp4", b"not a real video\n")?;
    seed_home(root)?;
    let link = root.join("Pictures/latest");
    if link.symlink_metadata().is_err() {
        symlink("dune-sunset-0412.jpg", &link).map_err(|e| Error::io(&link, e))?;
    }
    Ok(())
}

/// Where the fake mount table lives inside a fixture.
pub fn mounts_dir(root: &Path) -> std::path::PathBuf {
    root.join(".robin-fixture")
}

/// The XDG dirs, one trashed file, a folder nobody may read, and a mount
/// table that puts the fixture on a home disk and a fake SANDISK stick.
fn seed_home(root: &Path) -> Result<()> {
    for dir in [
        ".config",
        ".cache",
        ".local/share/Trash/files",
        "media/SANDISK",
    ] {
        let path = root.join(dir);
        fs::create_dir_all(&path).map_err(|e| Error::io(&path, e))?;
    }
    write(root, ".local/share/Trash/files/old-draft.txt", b"draft\n")?;
    write(
        root,
        ".local/share/Trash/info/old-draft.txt.trashinfo",
        b"[Trash Info]\nPath=/demo/Documents/old-draft.txt\nDeletionDate=2026-10-29T10:00:00\n",
    )?;
    let r = root.display();
    let mountinfo = format!(
        "22 1 259:2 / / rw - ext4 /dev/nvme0n1p2 rw\n\
         40 22 259:3 / {r} rw - ext4 /dev/nvme0n1p3 rw\n\
         41 40 8:1 / {r}/media/SANDISK rw - exfat /dev/sda1 rw\n"
    );
    let space = format!(
        "/ 512000000000 128000000000\n\
         {r} 1800000000000 812000000000\n\
         {r}/media/SANDISK 64000000000 6000000000\n"
    );
    let m = mounts_dir(root);
    fs::create_dir_all(&m).map_err(|e| Error::io(&m, e))?;
    write(&m, "mountinfo", mountinfo.as_bytes())?;
    write(&m, "space", space.as_bytes())?;
    let secret = root.join("secret");
    fs::create_dir_all(&secret).map_err(|e| Error::io(&secret, e))?;
    set_mode(&secret, 0o000)
}

fn set_mode(path: &Path, mode: u32) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(path, fs::Permissions::from_mode(mode)).map_err(|e| Error::io(path, e))
}

/// Delete a fixture, reopening its unreadable folder first.
pub fn remove(root: &Path) -> Result<()> {
    let secret = root.join("secret");
    if secret.is_dir() || secret.symlink_metadata().is_ok() {
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
        remove(&root).unwrap();
        assert!(!root.exists());
    }
}
