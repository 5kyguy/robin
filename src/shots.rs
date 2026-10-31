//! Deterministic frames for `robin --render`.
//!
//! The clock is fixed in the strings (2026-10-30 18:58). Animations are not
//! running; each frame is the end state, except the danger button which stays
//! locked (opacity 0.42) with Cancel focused.

use std::path::Path;
use std::rc::Rc;

use slint::{ModelRc, VecModel};

use crate::error::{Error, Result};
use crate::theme::Theme;
use crate::{AppWindow, GridLine, Place, RefusalLine, Row};

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
            | "merge-partial"
            | "merge-partial-expanded"
            | "move-refused"
            | "move-refused-unreadable"
            | "move-refused-unreadable-two"
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
            | "cut"
            | "dnd-copy"
            | "dnd-move"
            | "dnd-refused"
            | "conflict-live"
            | "move-refused-many"
            | "progress"
            | "confirm-delete"
            | "undo"
            | "list-modified-sorted"
            | "list-name-natural"
            | "list-name-desc"
            | "watch-fallback"
            | "nav-pending"
            | "start-denied"
    )
}

pub fn paint(view: &str, theme: &Theme, size: (u32, u32), out: &Path) -> Result<()> {
    std::env::set_var("SLINT_SCALE_FACTOR", "1");
    let _ = slint::BackendSelector::new()
        .renderer_name("software".into())
        .select();
    let _ = slint::set_xdg_app_id("dev.skyguy.robin");
    let ui = AppWindow::new().map_err(|e| Error::Message(e.to_string()))?;
    crate::app::apply_theme(&ui, theme);
    if view == "harness" {
        ui.set_harness(true);
        ui.set_label("harness".into());
    } else {
        ui.set_harness(false);
        apply_scene(&ui, view);
        fileops_scene(&ui, view);
    }
    crate::render::snapshot(&ui, size, out)
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
    if view == "search-no-results" {
        ui.set_state_title("Nothing matches \"kotlin\"".into());
        ui.set_state_note("Searched 4 812 items in ~/Projects".into());
    }
    if view == "permission-denied" {
        ui.set_state_title("No permission to open ~/secret".into());
        ui.set_state_note("Owned by root (drwx------)".into());
    }
    if view == "loading-large-dir" {
        ui.set_state_note("18 240 items".into());
    }
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
    if view == "move-refused-unreadable-two" {
        blocked = vec![
            kept(
                "Projects/robin/.cache",
                crate::fsops::KeepReason::UnreadableFolder,
            ),
            kept(
                "Projects/robin/target/tmp",
                crate::fsops::KeepReason::UnreadableFolder,
            ),
        ];
    }
    let refused = crate::fsops::refusal("Projects", &blocked);
    ui.set_refusal_title(refused.title.as_str().into());
    ui.set_refusal_can_copy(refused.can_copy_instead);
    ui.set_dialog_focus(i32::from(refused.can_copy_instead));
    ui.set_refusal_dismiss(refused.dismiss_label.into());
    ui.set_refusal_notes(refused.note_count() as i32);
    ui.set_refusal_lines(ModelRc::new(Rc::new(VecModel::from(
        refused
            .lines
            .into_iter()
            .map(|l| RefusalLine {
                path: l.path.into(),
                note: l.note.unwrap_or_default().into(),
                head: false,
            })
            .collect::<Vec<_>>(),
    ))));
    let (tone, text, details, pinned) = toast(view);
    ui.set_toast_tone(tone.into());
    ui.set_toast_glyph(crate::app::toast_glyph(tone).into());
    ui.set_toast_text(text.into());
    crate::app::set_toast_rows(
        ui,
        &crate::app::detail_rows(&details, pinned, view == "merge-partial-expanded"),
    );
    ui.set_overlay(
        match view {
            "path-edit" => "path-edit",
            "path-invalid" => "path-invalid",
            "path-complete" => "path-complete",
            "search-results" | "search-no-results" => "search",
            "confirm-trash" => "trash",
            "confirm-delete" => "delete",
            "confirm-trash-no-trash" => "trash-no",
            "confirm-delete-permanent" => "delete",
            "conflict" => "conflict",
            "conflict-all" => "conflict-all",
            "conflict-live" => "conflict-live",
            "progress-1" => "progress-1",
            "progress-3" => "progress-3",
            "progress-error" => "progress-error",
            "skipped" => "skipped",
            "skipped-details" => "skipped-details",
            "move-partial" | "merge-partial" | "merge-partial-expanded" => "toast-details",
            "move-refused"
            | "move-refused-unreadable"
            | "move-refused-unreadable-two"
            | "move-refused-many" => "move-refused",
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
        crate::app::show_stats(ui, &mock_report(), "Scanned 16:10  ·  Rescan", false);
        if stats_mode == "drilled" {
            let weights: Vec<(String, u64)> = vec![
                ("raw-exports".into(), 180_000_000_000),
                ("tatooine-trip".into(), 140_000_000_000),
                ("speeder-run.mp4".into(), 52_000_000_000),
                ("cantina-band.heic".into(), 40_000_000_000),
            ];
            ui.set_map_cells(ModelRc::new(Rc::new(VecModel::from(
                crate::app::map_cells(&weights, 623.0, 300.0),
            ))));
            ui.set_stat_tree_title("LARGEST FOLDERS   ›   Videos".into());
        }
    }
}

/// The hand-filled overview the mock frames show.
fn mock_report() -> crate::stats::Report {
    const GB: u64 = 1_000_000_000;
    let disk = |name: &str, mount: &str, total: u64, free: u64| crate::stats::Disk {
        name: name.into(),
        mount: mount.into(),
        total,
        free,
    };
    let top = [
        ("Videos", 412),
        ("Projects", 188),
        ("Pictures", 141),
        ("Downloads", 96),
        ("Games", 61),
        ("Music", 58),
        ("Documents", 31),
        (".cache", 22),
        ("Mail", 9),
        ("notes", 14),
        ("misc", 10),
    ];
    crate::stats::Report {
        disks: vec![
            disk("r2-root", "/", 1_800 * GB, 812 * GB),
            disk("archive", "/mnt/archive", 4_000 * GB, 362 * GB),
        ],
        home: crate::stats::Scan {
            total_bytes: 1_042 * GB,
            top: top.iter().map(|(n, g)| (n.to_string(), g * GB)).collect(),
            kinds: [
                (1_840, 412 * GB),
                (48_210, 163 * GB),
                (312_064, 162 * GB),
                (612, 88 * GB),
                (6_120, 58 * GB),
                (9_870, 31 * GB),
                (21_400, 128 * GB),
            ],
            ..Default::default()
        },
        trash_bytes: 4_700_000_000,
        trash_items: 2_318,
        cache_bytes: 1_400_000_000,
        cache_parts: vec![
            ("thumbnails".into(), 212_000_000),
            ("mpv".into(), 38_000_000),
        ],
    }
}

/// Tone, line, and details for the transfer toasts.
fn toast(view: &str) -> (&'static str, String, Vec<String>, bool) {
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
                ..Default::default()
            };
            ("warning", report.summary(), report.details(), false)
        }
        "merge-partial" | "merge-partial-expanded" => {
            let mut missing = vec!["raw-exports/IMG_4432.CR3".to_string()];
            missing.extend((4433..4449).map(|n| format!("raw-exports/IMG_{n}.CR3")));
            let out = crate::ops::Outcome {
                report: crate::fsops::TransferReport {
                    moved: 31,
                    merges: vec![crate::fsops::PartialMerge {
                        into: "raw-exports".into(),
                        merged: 31,
                        total: 48,
                        error: "raw-exports/IMG_4432.CR3: No space left on device (os error 28)"
                            .into(),
                        missing,
                        ..Default::default()
                    }],
                    ..Default::default()
                },
                ..Default::default()
            };
            let t = out.toast(crate::ops::JobKind::Copy);
            (t.tone, t.text, t.details, t.pinned)
        }
        _ => {
            let report = skipped_report();
            ("", report.summary(), report.details(), false)
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
        ..Default::default()
    }
}

/// The folder a view shows as open.
fn open_folder(view: &str) -> &'static str {
    match view {
        "empty-folder" => "~/Pictures/empty",
        "start-denied" => "~/secret",
        _ => "~/Pictures",
    }
}

/// The pinned sidebar entry that is the open folder itself, if any. A
/// folder inside an entry (or outside all of them) highlights nothing.
pub fn sidebar_entry(open: &str, pinned: &[&'static str]) -> Option<&'static str> {
    pinned.iter().copied().find(|name| {
        let path = if *name == "Home" {
            "~".to_string()
        } else {
            format!("~/{name}")
        };
        path == open
    })
}

fn places(view: &str) -> Vec<Place> {
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
            selected: false,
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
    let pinned: Vec<&'static str> = [
        "Home",
        "Desktop",
        "Documents",
        "Downloads",
        "Pictures",
        "Videos",
    ]
    .to_vec();
    let pick = sidebar_entry(open_folder(view), &pinned);
    for r in rows.iter_mut().filter(|r| r.section == 0) {
        r.selected = pick == Some(r.name.as_str());
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
        cut: false,
        drop: false,
        drop_refused: false,
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
        .enumerate()
        .map(|(i, chunk)| {
            let mut line = empty_line();
            line.n = chunk.len() as i32;
            line.first = (i * cols) as i32;
            for (index, cell) in chunk.iter().enumerate() {
                set_cell(
                    &mut line, index, cell.0, cell.1, cell.2, cell.3, cell.4, cell.5,
                );
            }
            line
        })
        .collect()
}

pub(crate) fn empty_line() -> GridLine {
    let blank = slint::SharedString::default();
    GridLine {
        n: 0,
        first: 0,
        g0: blank.clone(),
        n0: blank.clone(),
        r0: 0,
        b0: blank.clone(),
        s0: false,
        h0: false,
        f0: false,
        g1: blank.clone(),
        n1: blank.clone(),
        r1: 0,
        b1: blank.clone(),
        s1: false,
        h1: false,
        f1: false,
        g2: blank.clone(),
        n2: blank.clone(),
        r2: 0,
        b2: blank.clone(),
        s2: false,
        h2: false,
        f2: false,
        g3: blank.clone(),
        n3: blank.clone(),
        r3: 0,
        b3: blank.clone(),
        s3: false,
        h3: false,
        f3: false,
        g4: blank.clone(),
        n4: blank.clone(),
        r4: 0,
        b4: blank.clone(),
        s4: false,
        h4: false,
        f4: false,
        g5: blank.clone(),
        n5: blank.clone(),
        r5: 0,
        b5: blank,
        s5: false,
        h5: false,
        f5: false,
    }
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn set_cell(
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

/// The views of the wired file operations: cut rows, a drag over a folder,
/// a running job, the keyboard-focused confirms, and the undo toast.
fn fileops_scene(ui: &AppWindow, view: &str) {
    if view.starts_with("list-")
        || view == "watch-fallback"
        || view == "nav-pending"
        || view == "start-denied"
    {
        return list_scene(ui, view);
    }
    let mut rows = files(false);
    let pick = |rows: &mut Vec<Row>, f: &dyn Fn(&mut Row)| {
        for r in rows
            .iter_mut()
            .filter(|r| r.name.starts_with("dune-sunset"))
        {
            f(r);
        }
    };
    match view {
        "cut" => {
            pick(&mut rows, &|r| {
                r.cut = true;
                r.selected = true;
                r.hovered = false;
            });
            ui.set_status_left("15 items · 2 selected · 2 items cut".into());
        }
        "dnd-copy" | "dnd-move" => {
            pick(&mut rows, &|r| {
                r.selected = true;
                r.hovered = false;
            });
            rows[0].drop = true;
            ui.set_status_left("15 items · 2 selected".into());
            let copy = view == "dnd-copy";
            ui.set_drag_label(
                if copy {
                    "Copy 2 items to raw-exports"
                } else {
                    "Move 2 items to raw-exports"
                }
                .into(),
            );
            ui.set_drag_glyph(if copy { "󰆏" } else { "󰪹" }.into());
        }
        "dnd-refused" => {
            // Dragging raw-exports back over itself: muted, no accent.
            rows[0].selected = true;
            rows[0].hovered = false;
            rows[0].drop_refused = true;
            ui.set_status_left("15 items · 1 selected".into());
            ui.set_drag_label("Can't drop into itself".into());
            ui.set_drag_glyph("󰜺".into());
        }
        "conflict-live" => {
            let ask = crate::app::ConflictAsk {
                prompt: crate::fsops::ConflictPrompt {
                    name: "raw-exports".into(),
                    kind: crate::fsops::ConflictKind::FolderFolder,
                    existing_size: 4096,
                    existing_mtime: 1_792_000_000,
                    incoming_size: 4096,
                    incoming_mtime: 1_793_400_000,
                    existing_items: Some(48),
                    incoming_items: Some(31),
                },
                dest: "SANDISK".into(),
                left: 4,
            };
            crate::app::conflict_props(ui, &ask, false);
            ui.set_dialog_focus(1);
        }
        "move-refused-many" => {
            use crate::fsops::{KeepReason, KeptEntry};
            let kept = |p: &str, reason| KeptEntry {
                path: p.into(),
                reason,
            };
            let mut fonts: Vec<KeptEntry> = (1..=9)
                .map(|i| {
                    kept(
                        &format!("Projects/vendor/fonts/Face-{i}.ttf"),
                        KeepReason::ReadOnlyFolder,
                    )
                })
                .collect();
            fonts.push(kept("Projects/.cache", KeepReason::StickyFolder));
            let mut refused: Vec<(std::ffi::OsString, Vec<KeptEntry>)> = vec![
                ("Projects".into(), fonts),
                (
                    "holo-backup".into(),
                    vec![kept("holo-backup/2026", KeepReason::ReadOnlyFilesystem)],
                ),
                (
                    "shared".into(),
                    vec![kept("shared/team.md", KeepReason::StickyFolder)],
                ),
            ];
            // Enough items that the list scrolls instead of growing.
            for name in ["tatooine-trip", "raw-exports", "archive-2025", "renders"] {
                refused.push((
                    name.into(),
                    vec![kept(&format!("{name}/locked"), KeepReason::ReadOnlyFolder)],
                ));
            }
            let view = crate::app::refusal_view(&refused);
            crate::app::set_refusal(ui, &view);
            ui.set_dialog_focus(i32::from(view.can_copy));
        }
        "progress" => {
            ui.set_job_on(true);
            ui.set_job_glyph("󰆏".into());
            ui.set_job_label("Copying 3 items to SANDISK".into());
            ui.set_job_detail("2 of 3 items · speeder-run.mp4".into());
            ui.set_job_amount(0.66);
        }
        "confirm-trash" => {
            ui.set_dialog_focus(1);
            ui.set_danger_armed(true);
        }
        "confirm-delete" => {
            // Eight items: five names, then "and 3 more".
            let names: Vec<std::ffi::OsString> = [
                "dune-sunset-0412.jpg",
                "dune-sunset-0413.jpg",
                "speeder-run.mp4",
                "cantina-band.heic",
                "droid-manual.pdf",
                "holo-backup.tar.zst",
                "palette.toml",
                "shot-list.md",
            ]
            .iter()
            .map(Into::into)
            .collect();
            ui.set_confirm_noun("8 items".into());
            ui.set_confirm_names(crate::app::names_text(&names).into());
            ui.set_confirm_names_count(6);
            ui.set_dialog_focus(0);
            ui.set_danger_armed(true);
        }
        "confirm-delete-permanent" => ui.set_dialog_focus(0),
        "undo" => {
            ui.set_toast_on(true);
            ui.set_toast_tone("".into());
            ui.set_toast_text("Moved 3 items to trash".into());
            ui.set_toast_action("Undo".into());
            crate::app::set_toast_rows(ui, &crate::app::detail_rows(&[], false, false));
        }
        _ => return,
    }
    if matches!(view, "cut" | "dnd-copy" | "dnd-move" | "dnd-refused") {
        ui.set_files(ModelRc::new(Rc::new(VecModel::from(rows))));
    }
}

/// The list as the real controller fills and sorts it, over made-up
/// entries, at a fixed time in +05:30.
fn list_scene(ui: &AppWindow, view: &str) {
    use crate::model::Entry;
    // 2026-10-31 16:10 in +05:30.
    let now = 1_793_443_200;
    let mut ctl =
        crate::app::Controller::new("/demo/Pictures".into(), crate::ops::Env::from_system());
    ctl.clock = Some((now, 19_800));
    let e = |name: &str, dir: bool, len: u64, ago: i64| Entry {
        name: name.into(),
        is_dir: dir,
        is_symlink: false,
        len,
        mtime: std::time::UNIX_EPOCH + std::time::Duration::from_secs((now - ago) as u64),
        mode: if dir { 0o040755 } else { 0o100644 },
    };
    ctl.listing.set_entries(vec![
        e("raw-exports", true, 0, 2 * 86_400),
        e("tatooine-trip", true, 0, 19 * 86_400),
        e("Desert-10", true, 0, 3_600),
        e("Desert-9", true, 0, 40 * 86_400),
        e("dune-sunset-0412.jpg", false, 6_500_000, 1_200),
        e("dune-sunset-0413.jpg", false, 6_300_000, 9_000),
        e("frame-2.png", false, 2_100_000, 86_400 + 600),
        e("frame-10.png", false, 2_400_000, 30_000),
        e("frame-1.png", false, 1_900_000, 400 * 86_400),
        e("speeder-run.mp4", false, 432_000_000, 3 * 86_400),
        e("palette.toml", false, 3_400, 52_000),
        e("shot-list.md", false, 1_200, 10 * 86_400),
        e("binary-sunset.flac", false, 39_000_000, 77 * 86_400),
    ]);
    for (name, n) in [
        ("raw-exports", 48),
        ("tatooine-trip", 312),
        ("Desert-10", 7),
        ("Desert-9", 0),
    ] {
        ctl.counts.insert(name.into(), n);
    }
    match view {
        "list-modified-sorted" => {
            ctl.sort_by(3);
            ctl.sort_by(3);
        }
        "list-name-desc" => {
            ctl.sort_by(0);
        }
        _ => {}
    }
    ui.set_files(ModelRc::new(Rc::new(VecModel::from(ctl.rows()))));
    let (column, desc) = ctl.sort_header();
    ui.set_sort_column(column);
    ui.set_sort_desc(desc);
    ui.set_status_left(crate::ops::count(ctl.listing.view().len()).into());
    if view == "start-denied" {
        // The app started in a folder it can't read: the denied state, which
        // replaces the error toast, as `landed` shows it.
        ctl.unreadable = Some(true);
        ctl.listing.set_entries(Vec::new());
        ui.set_files(ModelRc::new(Rc::new(VecModel::from(ctl.rows()))));
        ui.set_crumbs("~  ›  secret".into());
        ui.set_status_left(crate::ops::count(0).into());
        ui.set_state("denied".into());
        ui.set_state_title("No permission to open ~/secret".into());
        ui.set_state_note("".into());
    }
    if view == "nav-pending" {
        // A slow navigation into raw-exports: the crumb names the target,
        // the old listing stays, and the bar runs under the toolbar.
        let t0 = std::time::Instant::now();
        let gen = ctl
            .begin_navigate_at("/demo/Pictures/raw-exports".into(), t0)
            .gen();
        ctl.tick_nav(gen, t0 + crate::app::NAV_PENDING_AFTER);
        let crumbs: Vec<String> =
            crate::model::breadcrumbs(ctl.crumb_dir(), Some(Path::new("/demo")))
                .iter()
                .map(|c| c.label.clone())
                .collect();
        ui.set_crumbs(crumbs.join("  ›  ").into());
        ui.set_nav_busy(ctl.nav_pending());
        ui.set_nav_phase(0.45);
    }
    // The two neutral notices, with the same text the app shows.
    let notice = match view {
        "watch-fallback" => Some(crate::app::watch_fallback_notice(Path::new(
            "/mnt/archive/Pictures",
        ))),
        _ => None,
    };
    if let Some((text, details)) = notice {
        let tone = if view == "watch-fallback" { "info" } else { "" };
        ui.set_toast_tone(tone.into());
        ui.set_toast_glyph(crate::app::toast_glyph(tone).into());
        ui.set_toast_text(text.into());
        crate::app::set_toast_rows(ui, &crate::app::detail_rows(&details, false, false));
        ui.set_toast_on(true);
        ui.set_toast_open(true);
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn the_sidebar_highlights_only_the_open_entry() {
        let pinned = ["Home", "Pictures"];
        assert_eq!(
            super::sidebar_entry("~/Pictures", &pinned),
            Some("Pictures")
        );
        assert_eq!(super::sidebar_entry("~", &pinned), Some("Home"));
        assert_eq!(super::sidebar_entry("~/secret", &pinned), None);
        assert_eq!(super::sidebar_entry("~/Pictures/raw", &pinned), None);
        let picked = |view| super::places(view).iter().filter(|p| p.selected).count();
        assert_eq!(picked("start-denied"), 0, "nothing lit in ~/secret");
        assert_eq!(picked("browser-list"), 1);
    }

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
        let weights = crate::stats::treemap_weights(&mock_report().home.top, 9, false);
        let cells = crate::app::map_cells(&weights, 623.0, 300.0);
        let videos = cells.iter().find(|c| c.name == "Videos").unwrap();
        assert!((videos.shade - 1.0).abs() < 0.001, "{}", videos.shade);
        let full = cells.iter().filter(|c| c.shade > 0.999).count();
        assert_eq!(full, 1, "only the largest tile is full accent");
        assert!(cells.iter().all(|c| !c.outline));
        let lone = crate::app::map_cells(&[("Videos".into(), 9)], 623.0, 300.0);
        assert!(
            lone[0].outline && lone[0].shade < 0.2,
            "a lone tile is a tint"
        );
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
