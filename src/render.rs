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
    matches!(
        view,
        "browser-list"
            | "nav-pending"
            | "start-denied"
            | "sidebar-device"
            | "sidebar-trash"
            | "trash-view"
            | "trash-empty-confirm"
            | "trash-empty"
            | "trash-noinfo"
            | "trash-restore-recreated"
            | "grid-view"
            | "grid-hidden"
            | "list-density"
            | "search-results"
            | "search-nosearch"
    )
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

/// Where a render seeds its home when `--fixture` isn't given: private to
/// this user, under `$XDG_RUNTIME_DIR` or `/tmp/robin-render-$UID`.
pub fn default_home() -> Result<PathBuf> {
    match std::env::var_os("XDG_RUNTIME_DIR").filter(|v| !v.is_empty()) {
        Some(run) => Ok(PathBuf::from(run).join("robin-render")),
        None => {
            let uid = rustix::process::getuid().as_raw();
            let dir = std::env::temp_dir().join(format!("robin-render-{uid}"));
            check_owned(&dir, uid)?;
            Ok(dir)
        }
    }
}

/// /tmp is shared: a name there could be another user's folder or a link
/// planted to redirect the render. Accept only a missing path or a real
/// folder this user owns.
pub fn check_owned(dir: &Path, uid: u32) -> Result<()> {
    use std::os::unix::fs::MetadataExt;
    match dir.symlink_metadata() {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(Error::io(dir, e)),
        Ok(m) if m.file_type().is_symlink() => Err(Error::Message(format!(
            "{} is a symlink; refusing to render there",
            dir.display()
        ))),
        Ok(m) if !m.is_dir() || m.uid() != uid => Err(Error::Message(format!(
            "{} is not a folder owned by this user; refusing to render there",
            dir.display()
        ))),
        Ok(_) => Ok(()),
    }
}

/// Refuse to seed a folder that has things in it and isn't a fixture, so
/// `--fixture ~/Pictures` can never write into a real folder.
pub fn check_fixture(root: &Path) -> Result<()> {
    let marked = crate::fixture::is_marked(root);
    let empty = match std::fs::read_dir(root) {
        Ok(mut entries) => entries.next().is_none(),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => true,
        Err(e) => return Err(Error::io(root, e)),
    };
    if marked || empty {
        Ok(())
    } else {
        Err(Error::Message(format!(
            "{} has files and no .robin-fixture marker; refusing to seed a real folder",
            root.display()
        )))
    }
}

/// The fixture home to use: seeded (again) and with frozen times.
pub fn seed(root: &Path) -> Result<()> {
    check_fixture(root)?;
    crate::fixture::materialize(root)?;
    crate::fixture::freeze_times(root, FIXTURE_MTIME)
}

pub fn paint(opts: Options<'_>) -> Result<()> {
    if let Some(dir) = &opts.fixture {
        check_fixture(dir)?;
    }
    std::env::set_var("SLINT_SCALE_FACTOR", "1");
    let _ = slint::BackendSelector::new()
        .renderer_name("software".into())
        .select();
    let _ = slint::set_xdg_app_id("dev.skyguy.robin");
    // A home this render made is removed after; a given --fixture is kept.
    let made = opts.fixture.is_none();
    let home = match opts.fixture {
        Some(dir) => dir,
        None => default_home()?,
    };
    let result = seed(&home).and_then(|()| {
        paint_in(
            opts.view,
            opts.theme,
            opts.size,
            opts.compact,
            &home,
            opts.out,
        )
    });
    if made {
        let _ = crate::fixture::remove(&home);
    }
    result
}

fn paint_in(
    view: &str,
    theme: Theme,
    size: (u32, u32),
    compact: bool,
    home: &Path,
    out: &Path,
) -> Result<()> {
    let root = home;
    let home = &crate::fixture::home(root);
    let start = match view {
        "start-denied" => home.join("secret"),
        _ => home.join("Pictures"),
    };
    let mounts = crate::mounts::FixtureMounts::load(&crate::fixture::mounts_dir(root))?;
    let mut env = crate::ops::Env::for_home(home);
    if view == "trash-empty" {
        env.home_trash = crate::fixture::empty_trash(root);
    }
    let ui = app::build_scripted(Launch {
        dir: start,
        env,
        theme,
        compact,
        clock: Some((FROZEN_NOW, FROZEN_OFFSET)),
        mounts: Arc::new(mounts),
        prefs: None,
    })?;
    app::land_reads();
    if view == "nav-pending" {
        // A slow navigation into raw-exports whose read hasn't landed.
        let t0 = std::time::Instant::now();
        app::navigate_pending(home.join("Pictures/raw-exports"), t0, 0.45);
    }
    // Click a sidebar row as a person would, then let its reads land.
    let click = match view {
        "sidebar-device" => Some((1, 2)), // SANDISK
        "sidebar-trash"
        | "trash-view"
        | "trash-empty-confirm"
        | "trash-empty"
        | "trash-noinfo"
        | "trash-restore-recreated" => Some((2, 0)),
        _ => None,
    };
    if let Some((section, index)) = click {
        ui.invoke_place_open(section, index);
        app::land_reads();
    }
    match view {
        // Pick a restorable item, as a click would.
        "trash-view" => select_row(&ui, "q3-budget.ods"),
        // The grid button, then a tile click.
        "grid-view" => {
            ui.invoke_set_grid(true);
            select_row(&ui, "dune-sunset-0413.jpg");
            // The grid must not keep the list's model alive (100k-entry
            // folders): fail the render, and the real_render test, if it does.
            use slint::Model;
            let (files, lines) = (ui.get_files().row_count(), ui.get_grid_rows().row_count());
            if files != 0 || lines == 0 {
                return Err(crate::Error::Message(format!(
                    "grid mode: {files} list rows, {lines} grid lines"
                )));
            }
        }
        // Grid with hidden files shown, from the status bar toggle.
        "grid-hidden" => {
            ui.invoke_set_grid(true);
            ui.invoke_toggle_hidden();
        }
        // The density label flips Comfortable to Compact.
        "list-density" => ui.invoke_toggle_density(),
        // Slash opens the search; typed keys narrow the listing.
        "search-results" => type_keys(&ui, "/dune"),
        "search-nosearch" => type_keys(&ui, "/kotlin"),
        "trash-empty-confirm" => ui.invoke_trash_empty(),
        // Select the item with no trashinfo; the banner explains it.
        "trash-noinfo" => select_row(&ui, "mystery.bin"),
        // Restore into ~/Documents/Finance, which no longer exists.
        "trash-restore-recreated" => {
            select_row(&ui, "q3-budget.ods");
            ui.invoke_trash_restore();
            app::land_reads();
        }
        _ => {}
    }
    snapshot(&ui, size, out)
}

/// Click the row named `name`, as a person would.
fn select_row(ui: &AppWindow, name: &str) {
    use slint::Model;
    // The grid leaves the list model empty, so look in its lines too.
    let rows = ui.get_files();
    let in_list = (0..rows.row_count()).find(|&i| rows.row_data(i).is_some_and(|r| r.name == name));
    let lines = ui.get_grid_rows();
    let in_grid = || {
        lines.iter().find_map(|l| {
            [&l.n0, &l.n1, &l.n2, &l.n3, &l.n4, &l.n5]
                .iter()
                .position(|n| n.as_str() == name)
                .map(|k| l.first as usize + k)
        })
    };
    let i = in_list
        .or_else(in_grid)
        .unwrap_or_else(|| panic!("no row {name}")) as i32;
    ui.invoke_row_pointer(i, 0, 0, false, false, false);
    ui.invoke_row_pointer(i, 2, 0, false, false, false);
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

/// Sends each character as a key press, as the window would.
fn type_keys(ui: &AppWindow, text: &str) {
    for ch in text.chars() {
        ui.invoke_key(ch.to_string().into(), false, false, false);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_real_folder_is_never_seeded() {
        let dir = std::env::temp_dir().join(format!("robin-unmarked-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("taxes.txt"), b"real").unwrap();
        let before = listing(&dir);
        let err = seed(&dir).unwrap_err().to_string();
        assert!(err.contains("refusing"), "{err}");
        assert_eq!(listing(&dir), before, "nothing written");
        assert_eq!(std::fs::read(dir.join("taxes.txt")).unwrap(), b"real");
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn an_empty_or_marked_folder_is_seeded() {
        let dir = std::env::temp_dir().join(format!("robin-marked-{}", std::process::id()));
        let _ = crate::fixture::remove(&dir);
        seed(&dir).unwrap();
        assert!(crate::fixture::is_marked(&dir), "seeding writes the marker");
        seed(&dir).unwrap();
        crate::fixture::remove(&dir).unwrap();
    }

    #[test]
    fn the_tmp_fallback_must_be_a_real_folder_we_own() {
        let uid = rustix::process::getuid().as_raw();
        let base = std::env::temp_dir().join(format!("robin-owned-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        std::fs::create_dir_all(&base).unwrap();
        let missing = base.join("missing");
        assert!(check_owned(&missing, uid).is_ok());
        let link = base.join("link");
        std::os::unix::fs::symlink(&base, &link).unwrap();
        assert!(check_owned(&link, uid).is_err(), "symlink");
        assert!(check_owned(&base, uid).is_ok());
        assert!(
            check_owned(&base, uid.wrapping_add(1)).is_err(),
            "other owner"
        );
        std::fs::remove_dir_all(&base).unwrap();
    }

    #[test]
    fn a_marker_folder_does_not_unlock_seeding() {
        let dir = std::env::temp_dir().join(format!("robin-fakemark-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join(crate::fixture::MARKER)).unwrap();
        std::fs::write(dir.join("taxes.txt"), b"real").unwrap();
        assert!(seed(&dir).is_err());
        std::fs::remove_dir_all(&dir).unwrap();
    }

    fn listing(dir: &Path) -> Vec<std::ffi::OsString> {
        let mut v: Vec<_> = std::fs::read_dir(dir)
            .unwrap()
            .map(|e| e.unwrap().file_name())
            .collect();
        v.sort();
        v
    }
}
