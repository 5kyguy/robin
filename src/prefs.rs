//! The view settings robin remembers between launches. They are global,
//! not per folder: the last view mode, tile size, density and whether
//! hidden files show.

use std::path::{Path, PathBuf};

/// The settings file is a few short lines; anything bigger is not ours.
pub const MAX_PREFS_BYTES: usize = 4096;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ViewPrefs {
    pub grid: bool,
    /// 0, 1 or 2: small, medium, large tiles.
    pub tile_step: usize,
    pub compact: bool,
    pub show_hidden: bool,
}

impl ViewPrefs {
    /// The defaults a fresh install starts with.
    pub fn fresh() -> Self {
        Self {
            tile_step: 1,
            ..Self::default()
        }
    }

    /// `$XDG_CONFIG_HOME/robin/view`, or `~/.config/robin/view`.
    pub fn path(config_dir: Option<PathBuf>) -> Option<PathBuf> {
        config_dir.map(|d| d.join("robin").join("view"))
    }

    /// Read `key = value` lines; anything missing or unreadable keeps
    /// its default, so a damaged file never stops the app opening.
    pub fn parse(text: &str) -> Self {
        let mut p = Self::fresh();
        for line in text.lines() {
            let Some((key, value)) = line.split_once('=') else {
                continue;
            };
            let (key, value) = (key.trim(), value.trim());
            match key {
                "view" => p.grid = value == "grid",
                "tile" => {
                    p.tile_step = match value {
                        "small" => 0,
                        "large" => 2,
                        _ => 1,
                    }
                }
                "density" => p.compact = value == "compact",
                "hidden" => p.show_hidden = value == "shown",
                _ => {}
            }
        }
        p
    }

    pub fn render(&self) -> String {
        format!(
            "view = {}\ntile = {}\ndensity = {}\nhidden = {}\n",
            if self.grid { "grid" } else { "list" },
            ["small", "medium", "large"][self.tile_step.min(2)],
            if self.compact {
                "compact"
            } else {
                "comfortable"
            },
            if self.show_hidden { "shown" } else { "hidden" },
        )
    }

    /// Read the file, or the defaults if it is missing, unreadable or
    /// bigger than [`MAX_PREFS_BYTES`] (a few short lines, never more).
    pub fn load(path: &Path) -> Self {
        use std::io::Read;
        use std::os::unix::fs::OpenOptionsExt;
        // No symlinks, and O_NONBLOCK so opening a FIFO can't wait for a
        // writer; then anything but a regular file is ignored.
        let Ok(file) = std::fs::OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
            .open(path)
        else {
            return Self::fresh();
        };
        if !file.metadata().is_ok_and(|m| m.is_file()) {
            return Self::fresh();
        }
        let mut bytes = Vec::new();
        let cap = MAX_PREFS_BYTES as u64;
        match file.take(cap + 1).read_to_end(&mut bytes) {
            Ok(n) if n as u64 <= cap => Self::parse(&String::from_utf8_lossy(&bytes)),
            _ => Self::fresh(),
        }
    }

    /// Write through `fsops::replace_file`, so a crash leaves the old
    /// settings or the new ones and a symlinked temp is never followed.
    pub fn save(&self, path: &Path) -> crate::Result<()> {
        let (Some(dir), Some(name)) = (path.parent(), path.file_name()) else {
            return Err(crate::Error::Message(format!(
                "no folder for {}",
                path.display()
            )));
        };
        std::fs::create_dir_all(dir).map_err(|e| crate::Error::io(dir, e))?;
        let fd = crate::safety::DirFd::open(dir)?;
        crate::fsops::replace_file(&fd, name, self.render().as_bytes())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn load_ignores_symlinks_fifos_and_folders() {
        let dir = std::env::temp_dir().join(format!("robin-prefs-odd-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let mut grid = ViewPrefs::fresh();
        grid.grid = true;
        let real = dir.join("real");
        grid.save(&real).unwrap();
        assert_eq!(ViewPrefs::load(&real), grid, "a regular file loads");
        let link = dir.join("link");
        std::os::unix::fs::symlink(&real, &link).unwrap();
        assert_eq!(ViewPrefs::load(&link), ViewPrefs::fresh(), "symlink");
        let fifo = dir.join("fifo");
        let c = std::ffi::CString::new(fifo.as_os_str().as_encoded_bytes()).unwrap();
        assert_eq!(unsafe { libc::mkfifo(c.as_ptr(), 0o600) }, 0);
        // Would block forever without O_NONBLOCK: nobody writes the FIFO.
        assert_eq!(ViewPrefs::load(&fifo), ViewPrefs::fresh(), "fifo");
        assert_eq!(ViewPrefs::load(&dir), ViewPrefs::fresh(), "folder");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn settings_round_trip_and_damage_falls_back_to_defaults() {
        let p = ViewPrefs {
            grid: true,
            tile_step: 2,
            compact: true,
            show_hidden: true,
        };
        assert_eq!(ViewPrefs::parse(&p.render()), p);
        assert_eq!(ViewPrefs::parse("garbage\nview = ?"), ViewPrefs::fresh());
    }

    #[test]
    fn an_oversized_file_loads_the_defaults() {
        let dir = std::env::temp_dir().join(format!("robin-prefs-big-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("view");
        let mut text = "view = grid\n".to_string();
        text.push_str(&"#".repeat(MAX_PREFS_BYTES));
        std::fs::write(&path, text).unwrap();
        assert_eq!(ViewPrefs::load(&path), ViewPrefs::fresh());
        std::fs::write(&path, "view = grid\n").unwrap();
        assert!(ViewPrefs::load(&path).grid);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn saving_creates_the_folder_and_loading_reads_it_back() {
        let dir = std::env::temp_dir().join(format!("robin-prefs-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let path = ViewPrefs::path(Some(dir.clone())).unwrap();
        assert_eq!(ViewPrefs::load(&path), ViewPrefs::fresh());
        let p = ViewPrefs {
            grid: true,
            ..ViewPrefs::fresh()
        };
        p.save(&path).unwrap();
        assert_eq!(ViewPrefs::load(&path), p);
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
