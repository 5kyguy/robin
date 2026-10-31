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
        None if have_display() => crate::app::run(crate::app::start_dir(None)),
        None | Some("--help") | Some("-h") => {
            print_help();
            Ok(0)
        }
        Some("--demo") => demo(iter),
        Some("--select") => select_file(iter),
        Some("--render") => render(iter, false),
        Some("--render-mock") => render(iter, true),
        Some(other) if !other.starts_with('-') => {
            crate::app::run(crate::app::start_dir(Some(std::path::Path::new(other))))
        }
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
  robin [DIR]                 open DIR (default: home) in a window
  robin --demo [--out DIR]
  robin --render <view> [--fixture DIR] [--size WxH] [--density comfortable|compact]
                        [--theme cyan|black] [--out FILE]
  robin --render-mock <view> [--size WxH] [--theme cyan|black] [--out FILE]

--render starts the real app on a seeded fixture home (DIR, or a temp
folder) with a frozen clock and plays the view's input script. Views
without a script yet are only in --render-mock, hand-filled frames.
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

fn select_file<'a>(mut iter: impl Iterator<Item = &'a std::ffi::OsString>) -> Result<i32> {
    let file = iter
        .next()
        .ok_or_else(|| Error::Message("--select needs a file".into()))?;
    let sock = crate::instance::runtime_socket();
    crate::instance::send_select(&sock, std::path::Path::new(file))?;
    Ok(0)
}

fn default_demo_dir() -> PathBuf {
    std::env::temp_dir().join("robin-demo")
}

fn render<'a>(mut iter: impl Iterator<Item = &'a std::ffi::OsString>, mock: bool) -> Result<i32> {
    let view = iter
        .next()
        .and_then(|s| s.to_str().map(str::to_string))
        .ok_or_else(|| Error::Message("--render needs a view name".into()))?;
    let mut size = (1280, 800);
    let mut theme_name = "cyan".to_string();
    let mut out: Option<PathBuf> = None;
    let mut fixture: Option<PathBuf> = None;
    let mut density = "comfortable".to_string();
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
            "--fixture" => fixture = Some(PathBuf::from(value(&mut iter)?)),
            "--size" => size = parse_size(&value(&mut iter)?)?,
            "--density" => density = value(&mut iter)?,
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
    let compact = match density.as_str() {
        "comfortable" => false,
        "compact" => true,
        other => {
            return Err(Error::Message(format!(
                "unknown density {other}. Use comfortable or compact."
            )))
        }
    };
    let out = out.unwrap_or_else(|| PathBuf::from(format!("{view}.png")));
    // The harness frame is a theme calibration, not a scene: it stays mock.
    if mock || view == "harness" {
        if !crate::shots::known(&view) {
            return Err(Error::Message(format!("view `{view}` is not built yet")));
        }
        crate::shots::paint(&view, &theme, size, &out)?;
    } else if crate::render::scripted(&view) {
        crate::render::paint(crate::render::Options {
            view: &view,
            theme,
            size,
            compact,
            fixture,
            out: &out,
        })?;
    } else if crate::shots::known(&view) {
        return Err(Error::Message(format!(
            "view `{view}` has no real-app script yet. Try --render-mock {view}."
        )));
    } else {
        return Err(Error::Message(format!("view `{view}` is not built yet")));
    }
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
        let _ = crate::fixture::remove(&dir);
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
        let _ = crate::fixture::remove(&dir);
    }

    #[test]
    fn unknown_view_is_an_error() {
        let err = run(["--render", "not-a-view"]).unwrap_err();
        let message = err.to_string();
        assert!(message.contains("not built"), "{message}");
    }

    #[test]
    fn a_mock_only_view_points_at_render_mock() {
        let err = run(["--render", "browser-grid-m"]).unwrap_err().to_string();
        assert!(err.contains("--render-mock"), "{err}");
        let err = run(["--render", "browser-list", "--density", "airy"])
            .unwrap_err()
            .to_string();
        assert!(err.contains("density"), "{err}");
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
