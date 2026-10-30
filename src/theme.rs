//! Theme roles from R2-D2's `theme.json`.
//!
//! The file lives at `$XDG_CONFIG_HOME/r2-d2/theme.json` (default
//! `~/.config/r2-d2/theme.json`). Only the dark roles are read. Every tint
//! the UI uses is one of these roles with an alpha, computed here.

use std::fs;
use std::path::{Path, PathBuf};

use serde_json::Value;

use crate::error::{Error, Result};

/// One color. Alpha is 0–255.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Rgba {
    pub r: u8,
    pub g: u8,
    pub b: u8,
    pub a: u8,
}

impl Rgba {
    pub const fn rgb(r: u8, g: u8, b: u8) -> Self {
        Self { r, g, b, a: 255 }
    }

    /// `role@NN%` from the visual spec. `percent` is 0–100.
    pub fn at(self, percent: u8) -> Self {
        let percent = percent.min(100);
        Self {
            a: ((u16::from(percent) * 255) / 100) as u8,
            ..self
        }
    }

    pub fn to_slint(self) -> slint::Color {
        slint::Color::from_argb_u8(self.a, self.r, self.g, self.b)
    }

    pub fn parse(text: &str) -> Option<Self> {
        let hex = text.trim().trim_start_matches('#');
        let value = u32::from_str_radix(hex, 16).ok()?;
        match hex.len() {
            6 => Some(Self::rgb(
                ((value >> 16) & 0xff) as u8,
                ((value >> 8) & 0xff) as u8,
                (value & 0xff) as u8,
            )),
            8 => Some(Self {
                r: ((value >> 24) & 0xff) as u8,
                g: ((value >> 16) & 0xff) as u8,
                b: ((value >> 8) & 0xff) as u8,
                a: (value & 0xff) as u8,
            }),
            _ => None,
        }
    }
}

/// Dark roles. `deep` and `spark` are optional chart colors.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Theme {
    pub bg: Rgba,
    pub surface: Rgba,
    pub line: Rgba,
    pub accent: Rgba,
    pub on_accent: Rgba,
    pub text: Rgba,
    pub muted: Rgba,
    pub danger: Rgba,
    pub on_danger: Rgba,
    pub warning: Rgba,
    pub on_warning: Rgba,
    pub deep: Rgba,
    pub spark: Rgba,
}

impl Theme {
    /// Cyan reference values from the visual spec.
    pub fn cyan() -> Self {
        Self::from_hex([
            "#061216", "#0E1E26", "#1E6A78", "#00E5FF", "#042028", "#E7FBFF", "#7EC8D4", "#FF2A4A",
            "#FFF5F6", "#FFB000", "#1A1200", "#031014", "#7AF7FF",
        ])
    }

    /// Monochrome pass. Hue is gone so a frame still has to read from value alone.
    pub fn black() -> Self {
        Self::from_hex([
            "#0C0C0C", "#161616", "#3A3A3A", "#E6E6E6", "#101010", "#F4F4F4", "#8E8E8E", "#D0D0D0",
            "#101010", "#A6A6A6", "#101010", "#080808", "#FFFFFF",
        ])
    }

    fn from_hex(values: [&str; 13]) -> Self {
        let color = |index: usize| Rgba::parse(values[index]).expect("built-in theme color");
        Self {
            bg: color(0),
            surface: color(1),
            line: color(2),
            accent: color(3),
            on_accent: color(4),
            text: color(5),
            muted: color(6),
            danger: color(7),
            on_danger: color(8),
            warning: color(9),
            on_warning: color(10),
            deep: color(11),
            spark: color(12),
        }
    }

    pub fn load_str(text: &str) -> Result<Self> {
        let value: Value =
            serde_json::from_str(text).map_err(|e| Error::Message(format!("theme.json: {e}")))?;
        let roles = value
            .get("dark")
            .or_else(|| value.get("Dark"))
            .unwrap_or(&value);
        let mut theme = Self::cyan();
        assign(&mut theme.bg, roles, "bg");
        assign(&mut theme.surface, roles, "surface");
        assign(&mut theme.line, roles, "line");
        assign(&mut theme.accent, roles, "accent");
        assign(&mut theme.on_accent, roles, "on_accent");
        assign(&mut theme.text, roles, "text");
        assign(&mut theme.muted, roles, "muted");
        assign(&mut theme.danger, roles, "danger");
        assign(&mut theme.on_danger, roles, "on_danger");
        assign(&mut theme.warning, roles, "warning");
        assign(&mut theme.on_warning, roles, "on_warning");
        assign(&mut theme.deep, roles, "deep");
        assign(&mut theme.spark, roles, "spark");
        Ok(theme)
    }

    pub fn load_path(path: &Path) -> Result<Self> {
        let text = fs::read_to_string(path).map_err(|e| Error::io(path, e))?;
        Self::load_str(&text)
    }
}

fn assign(slot: &mut Rgba, roles: &Value, key: &str) {
    let Some(raw) = roles.get(key).and_then(Value::as_str) else {
        return;
    };
    if let Some(color) = Rgba::parse(raw) {
        *slot = color;
    }
}

/// `$XDG_CONFIG_HOME/r2-d2/theme.json`, else `~/.config/r2-d2/theme.json`.
pub fn user_theme_path() -> PathBuf {
    let base = std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".config")))
        .unwrap_or_else(|| PathBuf::from(".config"));
    base.join("r2-d2").join("theme.json")
}

pub fn load_user_or_default() -> Theme {
    let path = user_theme_path();
    Theme::load_path(&path).unwrap_or_else(|_| Theme::cyan())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cyan_matches_the_spec_hex() {
        let theme = Theme::cyan();
        assert_eq!(theme.bg, Rgba::rgb(0x06, 0x12, 0x16));
        assert_eq!(theme.accent, Rgba::rgb(0x00, 0xe5, 0xff));
        assert_eq!(theme.danger, Rgba::rgb(0xff, 0x2a, 0x4a));
        assert_eq!(theme.warning, Rgba::rgb(0xff, 0xb0, 0x00));
    }

    #[test]
    fn alpha_percent_is_role_at() {
        let text = Theme::cyan().text.at(12);
        assert_eq!(text.r, 0xe7);
        assert_eq!(text.a, ((12 * 255) / 100) as u8);
    }

    #[test]
    fn dark_object_is_read_and_light_is_ignored() {
        let theme = Theme::load_str(
            r##"{"light":{"bg":"#ffffff","text":"#000000"},"dark":{"bg":"#111111","accent":"#abcdef"}}"##,
        )
        .unwrap();
        assert_eq!(theme.bg, Rgba::rgb(0x11, 0x11, 0x11));
        assert_eq!(theme.accent, Rgba::rgb(0xab, 0xcd, 0xef));
        assert_eq!(theme.text, Theme::cyan().text);
    }

    #[test]
    fn flat_object_works() {
        let theme = Theme::load_str(r##"{"surface":"#222222"}"##).unwrap();
        assert_eq!(theme.surface, Rgba::rgb(0x22, 0x22, 0x22));
    }
}
