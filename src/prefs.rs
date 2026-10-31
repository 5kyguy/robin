//! The view settings robin remembers between launches. They are global,
//! not per folder: the last view mode, tile size, density and whether
//! hidden files show.

use std::path::{Path, PathBuf};

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

    pub fn load(path: &Path) -> Self {
        std::fs::read_to_string(path)
            .map(|t| Self::parse(&t))
            .unwrap_or_else(|_| Self::fresh())
    }

    /// Write through a temp and rename, so a crash leaves the old
    /// settings or the new ones.
    pub fn save(&self, path: &Path) -> std::io::Result<()> {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        let tmp = path.with_extension(format!("tmp-{}", std::process::id()));
        std::fs::write(&tmp, self.render())?;
        std::fs::rename(&tmp, path)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
