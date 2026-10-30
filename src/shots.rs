//! Deterministic frames for `robin --render`.
//!
//! The clock is fixed in the strings (2026-10-30 18:58). Animations are not
//! running; each frame is the end state, except the danger button which stays
//! locked (opacity 0.42) with Cancel focused.

use std::path::Path;
use std::rc::Rc;

use slint::{ComponentHandle, ModelRc, VecModel};

use crate::error::{Error, Result};
use crate::theme::Theme;
use crate::{AppWindow, GridLine, MapCell, Place, RefusalLine, Row};

pub fn known(view: &str) -> bool {
    matches!(
        view,
        "harness"
            | "browser-list"
            | "browser-list-compact"
            | "browser-grid-m"
            | "browser-grid-s"
            | "browser-grid-l"
            | "sidebar-collapsed"
            | "breadcrumb-overflow"
            | "path-edit"
            | "path-invalid"
            | "path-complete"
            | "search-results"
            | "search-no-results"
            | "empty-folder"
            | "permission-denied"
            | "device-unmounted"
            | "device-ejected"
            | "loading-large-dir"
            | "hidden-files-shown"
            | "confirm-trash"
            | "confirm-trash-no-trash"
            | "confirm-delete-permanent"
            | "conflict"
            | "conflict-all"
            | "progress-1"
            | "progress-3"
            | "progress-error"
            | "skipped"
            | "skipped-details"
            | "move-partial"
            | "move-refused"
            | "move-refused-unreadable"
            | "menu-file"
            | "shortcuts-browser"
            | "reduce-motion"
            | "theme-black-list"
            | "theme-black-grid"
            | "viewer-image"
            | "viewer-hidden"
            | "viewer-zoom-400"
            | "viewer-video"
            | "viewer-scrub"
            | "viewer-resume"
            | "viewer-info"
            | "viewer-slideshow"
            | "shortcuts-viewer"
            | "theme-black-viewer"
            | "stats-overview"
            | "stats-treemap-drilled"
            | "stats-empty-trash"
            | "theme-black-stats"
    )
}

pub fn paint(view: &str, theme: &Theme, size: (u32, u32), out: &Path) -> Result<()> {
    std::env::set_var("SLINT_SCALE_FACTOR", "1");
    let _ = slint::BackendSelector::new()
        .renderer_name("software".into())
        .select();
    let _ = slint::set_xdg_app_id("dev.skyguy.robin");
    let ui = AppWindow::new().map_err(|e| Error::Message(e.to_string()))?;
    apply_theme(&ui, theme);
    if view == "harness" {
        ui.set_harness(true);
        ui.set_label("harness".into());
    } else {
        ui.set_harness(false);
        apply_scene(&ui, view);
    }
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

fn apply_theme(ui: &AppWindow, theme: &Theme) {
    ui.set_bg(theme.bg.to_slint());
    ui.set_surface(theme.surface.to_slint());
    ui.set_text(theme.text.to_slint());
    ui.set_muted(theme.muted.to_slint());
    ui.set_accent(theme.accent.to_slint());
    ui.set_on_accent(theme.on_accent.to_slint());
    ui.set_line(theme.line.to_slint());
    ui.set_danger(theme.danger.to_slint());
    ui.set_on_danger(theme.on_danger.to_slint());
    ui.set_warning(theme.warning.to_slint());
    ui.set_on_warning(theme.on_warning.to_slint());
}

fn apply_scene(ui: &AppWindow, view: &str) {
    let compact = view == "browser-list-compact";
    let grid = matches!(
        view,
        "browser-grid-m" | "browser-grid-s" | "browser-grid-l" | "theme-black-grid"
    );
    ui.set_compact(compact);
    ui.set_grid(grid);
    ui.set_collapsed(view == "sidebar-collapsed");
    let tile: f32 = match view {
        "browser-grid-s" => 112.0,
        "browser-grid-l" => 208.0,
        _ => 152.0,
    };
    ui.set_tile(tile);
    let all_places = places(view);
    ui.set_places(ModelRc::new(Rc::new(VecModel::from(all_places.clone()))));
    ui.set_pinned_places(ModelRc::new(Rc::new(VecModel::from(
        all_places
            .iter()
            .filter(|p| p.section == 0)
            .cloned()
            .collect::<Vec<_>>(),
    ))));
    ui.set_device_places(ModelRc::new(Rc::new(VecModel::from(
        all_places
            .iter()
            .filter(|p| p.section == 1)
            .cloned()
            .collect::<Vec<_>>(),
    ))));
    ui.set_trash_places(ModelRc::new(Rc::new(VecModel::from(
        all_places
            .iter()
            .filter(|p| p.section == 2)
            .cloned()
            .collect::<Vec<_>>(),
    ))));
    let show_hidden = view == "hidden-files-shown";
    ui.set_hidden_on(show_hidden);
    ui.set_files(ModelRc::new(Rc::new(VecModel::from(files(show_hidden)))));
    ui.set_grid_rows(ModelRc::new(Rc::new(VecModel::from(grid_lines(view)))));
    ui.set_menu_labels(ModelRc::new(Rc::new(VecModel::from(menu()))));
    ui.set_status_left(if show_hidden {
        "16 items · 1 hidden · 1 selected (6.2 MB)".into()
    } else {
        "15 items · 1 hidden · 1 selected (6.2 MB)".into()
    });
    ui.set_status_right("812 GB free of 1.8 TB".into());
    ui.set_crumbs(if view == "breadcrumb-overflow" {
        "…  ›  Pictures  ›  tatooine-trip  ›  2026".into()
    } else {
        "~  ›  Pictures".into()
    });
    ui.set_path_text("/home/sky/Pictures".into());
    ui.set_search_query(if view == "search-no-results" {
        "kotlin".into()
    } else {
        "dune".into()
    });
    ui.set_search_count(if view == "search-results" {
        "2".into()
    } else {
        "0".into()
    });
    ui.set_state(
        match view {
            "empty-folder" => "empty",
            "permission-denied" => "denied",
            "device-unmounted" => "unmounted",
            "device-ejected" => "ejected",
            "loading-large-dir" => "loading",
            "search-no-results" => "nosearch",
            _ => "files",
        }
        .into(),
    );
    let kept = |path: &str, reason| crate::fsops::KeptEntry {
        path: path.into(),
        reason,
    };
    let mut blocked = vec![
        kept(
            "Projects/robin/vendor/fonts/Inter.ttf",
            crate::fsops::KeepReason::ReadOnlyFolder,
        ),
        kept(
            "Projects/robin/vendor/fonts/Mono.ttf",
            crate::fsops::KeepReason::ReadOnlyFolder,
        ),
    ];
    if view == "move-refused-unreadable" {
        blocked = vec![kept(
            "Projects/robin/.cache",
            crate::fsops::KeepReason::UnreadableFolder,
        )];
    }
    let refused = crate::fsops::refusal("Projects", &blocked);
    ui.set_refusal_title(refused.title.into());
    ui.set_refusal_can_copy(refused.can_copy_instead);
    ui.set_refusal_lines(ModelRc::new(Rc::new(VecModel::from(
        refused
            .lines
            .into_iter()
            .map(|l| RefusalLine {
                path: l.path.into(),
                note: l.note.unwrap_or_default().into(),
            })
            .collect::<Vec<_>>(),
    ))));
    let (tone, text, details) = toast(view);
    ui.set_toast_tone(tone.into());
    ui.set_toast_text(text.into());
    ui.set_toast_details(ModelRc::new(Rc::new(VecModel::from(
        details
            .into_iter()
            .map(slint::SharedString::from)
            .collect::<Vec<_>>(),
    ))));
    ui.set_overlay(
        match view {
            "path-edit" => "path-edit",
            "path-invalid" => "path-invalid",
            "path-complete" => "path-complete",
            "search-results" | "search-no-results" => "search",
            "confirm-trash" => "trash",
            "confirm-trash-no-trash" => "trash-no",
            "confirm-delete-permanent" => "delete",
            "conflict" => "conflict",
            "conflict-all" => "conflict-all",
            "progress-1" => "progress-1",
            "progress-3" => "progress-3",
            "progress-error" => "progress-error",
            "skipped" => "skipped",
            "skipped-details" => "skipped-details",
            "move-partial" => "toast-details",
            "move-refused" | "move-refused-unreadable" => "move-refused",
            "menu-file" => "menu-file",
            "shortcuts-browser" | "shortcuts-viewer" => "shortcuts",
            _ => "",
        }
        .into(),
    );
    ui.set_viewer(
        match view {
            "viewer-image" | "theme-black-viewer" => "image",
            "viewer-hidden" => "hidden",
            "viewer-zoom-400" => "zoom",
            "viewer-video" => "video",
            "viewer-scrub" => "scrub",
            "viewer-resume" => "resume",
            "viewer-info" => "info",
            "viewer-slideshow" => "slideshow",
            _ => "",
        }
        .into(),
    );
    let stats_mode = match view {
        "stats-overview" | "theme-black-stats" => "overview",
        "stats-treemap-drilled" => "drilled",
        "stats-empty-trash" => "empty-trash",
        _ => "",
    };
    ui.set_stats_mode(stats_mode.into());
    if !stats_mode.is_empty() {
        ui.set_map_cells(ModelRc::new(Rc::new(VecModel::from(stats_cells(
            stats_mode == "drilled",
        )))));
    }
}

fn stats_cells(drilled: bool) -> Vec<MapCell> {
    let weights: Vec<(String, u64)> = if drilled {
        vec![
            ("raw-exports".into(), 180),
            ("tatooine-trip".into(), 140),
            ("speeder-run.mp4".into(), 52),
            ("cantina-band.heic".into(), 40),
        ]
    } else {
        vec![
            ("Videos".into(), 412),
            ("Projects".into(), 188),
            ("Pictures".into(), 141),
            ("Downloads".into(), 96),
            ("Games".into(), 61),
            ("Music".into(), 58),
            (".cache".into(), 22),
            ("Documents".into(), 31),
            ("Mail".into(), 9),
            ("other".into(), 24),
        ]
    };
    let mut ranked = weights.clone();
    ranked.sort_by_key(|(_, bytes)| std::cmp::Reverse(*bytes));
    let rank_of: std::collections::HashMap<String, usize> = ranked
        .iter()
        .enumerate()
        .map(|(index, (name, _))| (name.clone(), index))
        .collect();
    let labels: std::collections::HashMap<_, _> = weights.iter().cloned().collect();
    // 1px off each shared edge is a 2px surface gutter.
    const GUTTER: f32 = 1.0;
    crate::stats::squarify(&weights, 623.0, 300.0)
        .into_iter()
        .map(|rect| {
            let rank = rank_of[&rect.name] as f32;
            // Largest cell is full accent. Each lower rank steps down, and a
            // deeper squarify row is clearly darker than the row above it.
            let shade = (1.0 - rank * 0.08 - rect.depth as f32 * 0.15).clamp(0.18, 1.0);
            MapCell {
                x: rect.x + GUTTER,
                y: rect.y + GUTTER,
                w: (rect.w - GUTTER * 2.0).max(1.0),
                h: (rect.h - GUTTER * 2.0).max(1.0),
                name: rect.name.clone().into(),
                detail: format!("{} GB", labels[&rect.name]).into(),
                shade,
            }
        })
        .collect()
}

/// Tone, line, and details for the transfer toasts.
fn toast(view: &str) -> (&'static str, String, Vec<String>) {
    use crate::fsops::{KeepReason, KeptEntry};
    let kept = |path: &str, reason| KeptEntry {
        path: path.into(),
        reason,
    };
    match view {
        "move-partial" => {
            let report = crate::fsops::TransferReport {
                moved: 212,
                skipped: Vec::new(),
                left_behind: vec![
                    kept("Projects/robin/vendor/fonts", KeepReason::ReadOnlyFolder),
                    kept("Projects/robin/vendor/icons", KeepReason::ReadOnlyFolder),
                ],
            };
            ("warning", report.summary(), report.details())
        }
        _ => {
            let report = skipped_report();
            ("", report.summary(), report.details())
        }
    }
}

/// The report a cross-device move of ~/Projects to SANDISK would produce.
fn skipped_report() -> crate::fsops::TransferReport {
    use crate::fsops::{SkipReason, SkippedEntry, TransferReport};
    let skip = |path: &str, reason| SkippedEntry {
        path: path.into(),
        reason,
    };
    TransferReport {
        moved: 212,
        skipped: vec![
            skip("Projects/robin/target/mnt", SkipReason::MountPoint),
            skip("Projects/robin/.cache/build.fifo", SkipReason::Fifo),
            skip("Projects/r2-d2/run/agent.sock", SkipReason::Socket),
        ],
        left_behind: Vec::new(),
    }
}

fn places(view: &str) -> Vec<Place> {
    let pictures = view != "empty-folder";
    let mut rows = vec![
        place(PlaceSpec {
            glyph: "󰋜",
            name: "Home",
            count: "",
            section: 0,
            selected: false,
            usage: -1.0,
            level: 0,
        }),
        place(PlaceSpec {
            glyph: "󰇄",
            name: "Desktop",
            count: "4",
            section: 0,
            selected: false,
            usage: -1.0,
            level: 0,
        }),
        place(PlaceSpec {
            glyph: "󰈙",
            name: "Documents",
            count: "212",
            section: 0,
            selected: false,
            usage: -1.0,
            level: 0,
        }),
        place(PlaceSpec {
            glyph: "󰇚",
            name: "Downloads",
            count: "38",
            section: 0,
            selected: false,
            usage: -1.0,
            level: 0,
        }),
        place(PlaceSpec {
            glyph: "󰉏",
            name: "Pictures",
            count: "1 204",
            section: 0,
            selected: pictures,
            usage: -1.0,
            level: 0,
        }),
        place(PlaceSpec {
            glyph: "󰕧",
            name: "Videos",
            count: "96",
            section: 0,
            selected: false,
            usage: -1.0,
            level: 0,
        }),
        place(PlaceSpec {
            glyph: "󰋊",
            name: "r2-root",
            count: "58%",
            section: 1,
            selected: false,
            usage: 0.58,
            level: 0,
        }),
        place(PlaceSpec {
            glyph: "󰋊",
            name: "archive",
            count: "91%",
            section: 1,
            selected: false,
            usage: 0.91,
            level: 1,
        }),
        place(PlaceSpec {
            glyph: "󰕓",
            name: "SANDISK",
            count: "34%",
            section: 1,
            selected: false,
            usage: 0.34,
            level: 0,
        }),
        place(PlaceSpec {
            glyph: "󰩺",
            name: "Trash",
            count: "2 318",
            section: 2,
            selected: false,
            usage: -1.0,
            level: 0,
        }),
    ];
    if view == "device-unmounted" || view == "device-ejected" {
        if let Some(disk) = rows.iter_mut().find(|row| row.name == "SANDISK") {
            disk.count = "—".into();
            disk.usage = 0.0;
        }
    }
    rows
}

struct PlaceSpec<'a> {
    glyph: &'a str,
    name: &'a str,
    count: &'a str,
    section: i32,
    selected: bool,
    usage: f32,
    level: i32,
}

fn place(spec: PlaceSpec<'_>) -> Place {
    Place {
        glyph: spec.glyph.into(),
        name: spec.name.into(),
        count: spec.count.into(),
        selected: spec.selected,
        usage: spec.usage,
        level: spec.level,
        section: spec.section,
    }
}

fn files(show_hidden: bool) -> Vec<Row> {
    let mut rows = vec![
        row(
            "󰉋",
            "raw-exports",
            "48 items",
            "Folder",
            "2026-10-28 09:12",
            false,
            false,
            false,
            1,
            "",
        ),
        row(
            "󰉋",
            "tatooine-trip",
            "312 items",
            "Folder",
            "2026-10-12 21:40",
            false,
            false,
            false,
            1,
            "",
        ),
        row(
            "󰈟",
            "dune-sunset-0412.jpg",
            "6.2 MB",
            "JPEG image",
            "3h ago",
            true,
            false,
            false,
            0,
            "",
        ),
        row(
            "󰈟",
            "dune-sunset-0413.jpg",
            "6.0 MB",
            "JPEG image",
            "3h ago",
            false,
            true,
            false,
            0,
            "",
        ),
        row(
            "󰕧",
            "speeder-run.mp4",
            "412 MB",
            "MP4 video",
            "2026-10-29 17:05",
            false,
            false,
            false,
            1,
            "",
        ),
        row(
            "󰈟",
            "cantina-band.heic",
            "3.1 MB",
            "HEIC image",
            "2026-10-27 11:30",
            false,
            false,
            false,
            0,
            "HEIC",
        ),
        row(
            "󰈦",
            "droid-manual.pdf",
            "18.4 MB",
            "PDF",
            "2026-10-02 08:44",
            false,
            false,
            false,
            2,
            "",
        ),
        row(
            "󰗄",
            "holo-backup.tar.zst",
            "2.3 GB",
            "Archive",
            "2026-09-30 23:01",
            false,
            false,
            false,
            3,
            "",
        ),
        row(
            "\u{e6b2}",
            "palette.toml",
            "3.4 KB",
            "TOML",
            "2026-10-30 18:20",
            false,
            false,
            false,
            1,
            "",
        ),
        row(
            "\u{f0354}",
            "shot-list.md",
            "1.2 KB",
            "Markdown",
            "2026-10-21 14:02",
            false,
            false,
            false,
            0,
            "",
        ),
        row(
            "󰎆",
            "binary-sunset.flac",
            "38 MB",
            "FLAC audio",
            "2026-08-14 19:55",
            false,
            false,
            false,
            1,
            "",
        ),
        row(
            "󰈔",
            "mos-eisley.xcf",
            "88 MB",
            "Unknown",
            "2026-07-03 10:10",
            false,
            false,
            false,
            4,
            "",
        ),
        row(
            "󰈟",
            "r2-portrait-001.png",
            "9.8 MB",
            "PNG image",
            "2026-06-19 16:48",
            false,
            false,
            false,
            0,
            "",
        ),
    ];
    if show_hidden {
        rows.push(row(
            "󰉋",
            ".thumbs-old",
            "9 items",
            "Folder",
            "2026-05-02 12:00",
            false,
            false,
            true,
            1,
            "",
        ));
    }
    rows
}

#[allow(clippy::too_many_arguments)]
fn row(
    glyph: &str,
    name: &str,
    size: &str,
    kind: &str,
    modified: &str,
    selected: bool,
    hovered: bool,
    hidden: bool,
    role: i32,
    badge: &str,
) -> Row {
    Row {
        glyph: glyph.into(),
        name: name.into(),
        size: size.into(),
        kind: kind.into(),
        modified: modified.into(),
        selected,
        hovered,
        hidden,
        role,
        badge: badge.into(),
    }
}

fn grid_lines(view: &str) -> Vec<GridLine> {
    let cols: usize = match view {
        "browser-grid-s" => 6,
        "browser-grid-l" => 4,
        _ => 5,
    };
    let cells = [
        ("󰉋", "raw-exports", 1, "", false, false),
        ("󰉋", "tatooine-trip", 1, "", false, false),
        ("󰈟", "dune-sunset-0412.jpg", 0, "", true, false),
        ("󰈟", "dune-sunset-0413.jpg", 0, "", false, true),
        ("󰕧", "speeder-run.mp4", 1, "04:30", false, false),
        ("󰈟", "cantina-band.heic", 0, "HEIC", false, false),
        ("󰈦", "droid-manual.pdf", 2, "PDF", false, false),
        ("󰗄", "holo-backup.tar.zst", 3, "ZST", false, false),
        ("\u{e6b2}", "palette.toml", 1, "TOML", false, false),
        ("\u{f0354}", "shot-list.md", 0, "MD", false, false),
        ("󰎆", "binary-sunset.flac", 1, "FLAC", false, false),
        ("󰈔", "mos-eisley.xcf", 4, "XCF", false, false),
        (
            "󰈟",
            "lars homestead family portrait final.jpg",
            0,
            "",
            false,
            false,
        ),
    ];
    cells
        .chunks(cols)
        .map(|chunk| {
            let mut line = empty_line();
            line.n = chunk.len() as i32;
            for (index, cell) in chunk.iter().enumerate() {
                set_cell(
                    &mut line, index, cell.0, cell.1, cell.2, cell.3, cell.4, cell.5,
                );
            }
            line
        })
        .collect()
}

fn empty_line() -> GridLine {
    let blank = slint::SharedString::default();
    GridLine {
        n: 0,
        g0: blank.clone(),
        n0: blank.clone(),
        r0: 0,
        b0: blank.clone(),
        s0: false,
        h0: false,
        g1: blank.clone(),
        n1: blank.clone(),
        r1: 0,
        b1: blank.clone(),
        s1: false,
        h1: false,
        g2: blank.clone(),
        n2: blank.clone(),
        r2: 0,
        b2: blank.clone(),
        s2: false,
        h2: false,
        g3: blank.clone(),
        n3: blank.clone(),
        r3: 0,
        b3: blank.clone(),
        s3: false,
        h3: false,
        g4: blank.clone(),
        n4: blank.clone(),
        r4: 0,
        b4: blank.clone(),
        s4: false,
        h4: false,
        g5: blank.clone(),
        n5: blank.clone(),
        r5: 0,
        b5: blank,
        s5: false,
        h5: false,
    }
}

#[allow(clippy::too_many_arguments)]
fn set_cell(
    line: &mut GridLine,
    index: usize,
    glyph: &str,
    name: &str,
    role: i32,
    badge: &str,
    selected: bool,
    hovered: bool,
) {
    let g = slint::SharedString::from(glyph);
    let n = slint::SharedString::from(name);
    let b = slint::SharedString::from(badge);
    match index {
        0 => {
            line.g0 = g;
            line.n0 = n;
            line.r0 = role;
            line.b0 = b;
            line.s0 = selected;
            line.h0 = hovered;
        }
        1 => {
            line.g1 = g;
            line.n1 = n;
            line.r1 = role;
            line.b1 = b;
            line.s1 = selected;
            line.h1 = hovered;
        }
        2 => {
            line.g2 = g;
            line.n2 = n;
            line.r2 = role;
            line.b2 = b;
            line.s2 = selected;
            line.h2 = hovered;
        }
        3 => {
            line.g3 = g;
            line.n3 = n;
            line.r3 = role;
            line.b3 = b;
            line.s3 = selected;
            line.h3 = hovered;
        }
        4 => {
            line.g4 = g;
            line.n4 = n;
            line.r4 = role;
            line.b4 = b;
            line.s4 = selected;
            line.h4 = hovered;
        }
        _ => {
            line.g5 = g;
            line.n5 = n;
            line.r5 = role;
            line.b5 = b;
            line.s5 = selected;
            line.h5 = hovered;
        }
    }
}

fn menu() -> Vec<slint::SharedString> {
    [
        "Open",
        "Rename",
        "Copy path",
        "Move to trash",
        "Delete permanently",
    ]
    .into_iter()
    .map(slint::SharedString::from)
    .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn checklist_names_are_known() {
        for name in [
            "browser-list",
            "browser-list-compact",
            "browser-grid-m",
            "sidebar-collapsed",
            "empty-folder",
            "confirm-delete-permanent",
            "theme-black-list",
            "theme-black-grid",
        ] {
            assert!(known(name), "{name}");
        }
    }

    #[test]
    fn treemap_tiles_keep_a_surface_gutter() {
        let cells = stats_cells(false);
        let videos = cells.iter().find(|c| c.name == "Videos").unwrap();
        assert!((videos.shade - 1.0).abs() < 0.001, "{}", videos.shade);
        let mut gaps = Vec::new();
        for a in &cells {
            for b in &cells {
                if a.name == b.name {
                    continue;
                }
                let ax2 = a.x + a.w;
                let ay2 = a.y + a.h;
                let bx2 = b.x + b.w;
                let by2 = b.y + b.h;
                let overlap_y = a.y < by2 - 1.0 && b.y < ay2 - 1.0;
                let overlap_x = a.x < bx2 - 1.0 && b.x < ax2 - 1.0;
                if overlap_y && b.x >= ax2 - 0.5 {
                    gaps.push(b.x - ax2);
                }
                if overlap_x && b.y >= ay2 - 0.5 {
                    gaps.push(b.y - ay2);
                }
            }
        }
        let min = gaps.iter().copied().fold(f32::MAX, f32::min);
        assert!((min - 2.0).abs() < 0.2, "gutter should be 2px, min {min}");
        assert!(cells.iter().any(|c| c.shade < videos.shade - 0.1));
    }
}
