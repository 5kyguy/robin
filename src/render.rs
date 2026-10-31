//! `--render`: the real app against a seeded fixture home, driven by a
//! short input script with a frozen clock, then captured to a PNG.
//!
//! Only what the live app sets can show up; anything the live app leaves
//! unset renders blank. `--render-mock` keeps the hand-filled frames in
//! `shots.rs` for views that have no script yet.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use slint::ComponentHandle;

use crate::app::{self, Launch};
use crate::error::{Error, Result};
use crate::theme::Theme;
use crate::AppWindow;

/// 2026-10-31 16:10 in +05:30, the same instant as the mock frames.
pub const FROZEN_NOW: i64 = 1_793_443_200;
pub const FROZEN_OFFSET: i64 = 19_800;
/// Fixture files are stamped 40 minutes before the frozen now.
pub const FIXTURE_MTIME: i64 = FROZEN_NOW - 2_400;

/// Views with an input script.
pub fn scripted(view: &str) -> bool {
    matches!(view, "browser-list" | "nav-pending" | "start-denied")
}

pub struct Options<'a> {
    pub view: &'a str,
    pub theme: Theme,
    pub size: (u32, u32),
    pub compact: bool,
    /// The fixture home; a fixed temp folder when not given.
    pub fixture: Option<PathBuf>,
    pub out: &'a Path,
}

/// The fixture home to use: seeded (again) and with frozen times.
pub fn seed(fixture: Option<PathBuf>) -> Result<PathBuf> {
    let root = fixture.unwrap_or_else(|| std::env::temp_dir().join("robin-render"));
    crate::fixture::materialize(&root)?;
    crate::fixture::freeze_times(&root, FIXTURE_MTIME)?;
    Ok(root)
}

pub fn paint(opts: Options<'_>) -> Result<()> {
    std::env::set_var("SLINT_SCALE_FACTOR", "1");
    let _ = slint::BackendSelector::new()
        .renderer_name("software".into())
        .select();
    let _ = slint::set_xdg_app_id("dev.skyguy.robin");
    let home = seed(opts.fixture)?;
    let start = match opts.view {
        "start-denied" => home.join("secret"),
        _ => home.join("Pictures"),
    };
    let mounts = crate::mounts::FixtureMounts::load(&crate::fixture::mounts_dir(&home))?;
    let ui = app::build_scripted(Launch {
        dir: start,
        env: crate::ops::Env::for_home(&home),
        theme: opts.theme,
        compact: opts.compact,
        clock: Some((FROZEN_NOW, FROZEN_OFFSET)),
        mounts: Arc::new(mounts),
    })?;
    app::land_reads();
    if opts.view == "nav-pending" {
        // A slow navigation into raw-exports whose read hasn't landed.
        let t0 = std::time::Instant::now();
        app::navigate_pending(home.join("Pictures/raw-exports"), t0, 0.45);
    }
    snapshot(&ui, opts.size, opts.out)
}

/// Size, show and capture a window to `out`.
pub fn snapshot(ui: &AppWindow, size: (u32, u32), out: &Path) -> Result<()> {
    ui.window()
        .set_size(slint::PhysicalSize::new(size.0, size.1));
    ui.show().map_err(|e| Error::Message(e.to_string()))?;
    let buffer = ui
        .window()
        .take_snapshot()
        .map_err(|e| Error::Message(e.to_string()))?;
    if let Some(parent) = out.parent() {
        if !parent.as_os_str().is_empty() {
            std::fs::create_dir_all(parent).map_err(|e| Error::io(parent, e))?;
        }
    }
    image::save_buffer(
        out,
        buffer.as_bytes(),
        buffer.width(),
        buffer.height(),
        image::ExtendedColorType::Rgba8,
    )
    .map_err(|e| Error::Message(format!("write {}: {e}", out.display())))?;
    Ok(())
}
