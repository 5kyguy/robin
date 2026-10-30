//! Command line. The window is not interactive yet beyond `--render` frames
//! that the later milestones fill in. `--demo` writes the fixture tree.

use std::path::PathBuf;

use crate::error::{Error, Result};
use crate::fixture;
use crate::theme::Theme;

pub fn run<I, S>(args: I) -> Result<i32>
where
    I: IntoIterator<Item = S>,
    S: Into<std::ffi::OsString>,
{
    let args: Vec<std::ffi::OsString> = args.into_iter().map(Into::into).collect();
    let mut iter = args.iter();
    match iter.next().and_then(|s| s.to_str()) {
        None | Some("--help") | Some("-h") => {
            print_help();
            Ok(0)
        }
        Some("--demo") => demo(iter),
        Some("--render") => render(iter),
        Some(other) => Err(Error::Message(format!(
            "unknown argument {other}. Try `robin --help`."
        ))),
    }
}

fn print_help() {
    println!(
        "\
robin — file manager for R2-D2

Usage:
  robin --demo [--out DIR]
  robin --render <view> [--fixture DIR] [--size WxH] [--density comfortable|compact]
                        [--theme cyan|black] [--out FILE]
  robin --help

The interface follows docs/DESIGN.md. Views that are not built yet fail
with a message instead of drawing a stand-in."
    );
}

fn demo<'a>(mut iter: impl Iterator<Item = &'a std::ffi::OsString>) -> Result<i32> {
    let mut out = default_demo_dir();
    while let Some(flag) = iter.next() {
        match flag.to_str() {
            Some("--out") => {
                let value = iter
                    .next()
                    .ok_or_else(|| Error::Message("--out needs a directory".into()))?;
                out = PathBuf::from(value);
            }
            Some(other) => return Err(Error::Message(format!("unknown flag {other}"))),
            None => return Err(Error::Message("bad flag".into())),
        }
    }
    fixture::materialize(&out)?;
    println!("{}", out.display());
    Ok(0)
}

fn default_demo_dir() -> PathBuf {
    std::env::temp_dir().join("robin-demo")
}

fn render<'a>(mut iter: impl Iterator<Item = &'a std::ffi::OsString>) -> Result<i32> {
    let view = iter
        .next()
        .and_then(|s| s.to_str().map(str::to_string))
        .ok_or_else(|| Error::Message("--render needs a view name".into()))?;
    let mut size = (1280, 800);
    let mut theme_name = "cyan".to_string();
    let mut out: Option<PathBuf> = None;
    let mut _fixture: Option<PathBuf> = None;
    let mut _density = "comfortable".to_string();
    while let Some(flag) = iter.next() {
        let flag = flag
            .to_str()
            .ok_or_else(|| Error::Message("bad flag".into()))?;
        let value = |iter: &mut dyn Iterator<Item = &std::ffi::OsString>| {
            iter.next()
                .and_then(|s| s.to_str().map(str::to_string))
                .ok_or_else(|| Error::Message(format!("{flag} needs a value")))
        };
        match flag {
            "--fixture" => _fixture = Some(PathBuf::from(value(&mut iter)?)),
            "--size" => size = parse_size(&value(&mut iter)?)?,
            "--density" => _density = value(&mut iter)?,
            "--theme" => theme_name = value(&mut iter)?,
            "--out" => out = Some(PathBuf::from(value(&mut iter)?)),
            other => return Err(Error::Message(format!("unknown flag {other}"))),
        }
    }
    let theme = match theme_name.as_str() {
        "cyan" => Theme::cyan(),
        "black" => Theme::black(),
        other => {
            return Err(Error::Message(format!(
                "unknown theme {other}. Use cyan or black."
            )))
        }
    };
    let out = out.unwrap_or_else(|| PathBuf::from(format!("{view}.png")));
    if !crate::shots::known(&view) {
        return Err(Error::Message(format!("view `{view}` is not built yet")));
    }
    crate::shots::paint(&view, &theme, size, &out)?;
    println!("{}", out.display());
    Ok(0)
}

fn parse_size(text: &str) -> Result<(u32, u32)> {
    let (w, h) = text
        .split_once('x')
        .ok_or_else(|| Error::Message(format!("size {text} is not WxH")))?;
    let w: u32 = w
        .parse()
        .map_err(|_| Error::Message(format!("size {text} is not WxH")))?;
    let h: u32 = h
        .parse()
        .map_err(|_| Error::Message(format!("size {text} is not WxH")))?;
    if w == 0 || h == 0 {
        return Err(Error::Message("size must be non-zero".into()));
    }
    Ok((w, h))
}

/// True when a display server is reachable. Render tests skip otherwise.
pub fn have_display() -> bool {
    nonempty("DISPLAY") || nonempty("WAYLAND_DISPLAY")
}

fn nonempty(key: &str) -> bool {
    std::env::var(key).is_ok_and(|v| !v.is_empty())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn help_exits_clean() {
        assert_eq!(run(["--help"]).unwrap(), 0);
    }

    #[test]
    fn demo_prints_a_tree() {
        let dir = std::env::temp_dir().join(format!("robin-cli-demo-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        assert_eq!(
            run([
                "--demo".into(),
                "--out".into(),
                dir.clone().into_os_string()
            ])
            .unwrap(),
            0
        );
        assert!(dir.join("Pictures/palette.toml").is_file());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn unknown_view_is_an_error() {
        let err = run(["--render", "not-a-view"]).unwrap_err();
        let message = err.to_string();
        assert!(message.contains("not built"), "{message}");
    }

    #[test]
    fn harness_png_matches_the_theme() {
        if !have_display() {
            return;
        }
        let out = std::env::temp_dir().join(format!("robin-harness-{}.png", std::process::id()));
        let _ = std::fs::remove_file(&out);
        run([
            "--render".into(),
            "harness".into(),
            "--size".into(),
            "320x200".into(),
            "--theme".into(),
            "cyan".into(),
            "--out".into(),
            out.clone().into_os_string(),
        ])
        .unwrap();
        let image = image::open(&out).unwrap().to_rgba8();
        assert_eq!(image.width(), 320);
        assert_eq!(image.height(), 200);
        let pixel = image.get_pixel(1, 1).0;
        let bg = Theme::cyan().bg;
        assert_eq!(pixel, [bg.r, bg.g, bg.b, 255], "corner pixel {pixel:?}");
        let _ = std::fs::remove_file(&out);
    }
}
