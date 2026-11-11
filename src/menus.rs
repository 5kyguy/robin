//! The context menus and the shortcut overlay (DESIGN §7), as data the
//! window draws. Kept apart from the window so the lists are tested.

use slint::{ModelRc, SharedString, VecModel};

use crate::{KeyRow, KeySection, MenuItem};

fn item(glyph: &str, label: &str, shortcut: &str) -> MenuItem {
    MenuItem {
        glyph: glyph.into(),
        label: label.into(),
        shortcut: shortcut.into(),
        danger: false,
        sub: false,
        sep: false,
    }
}

fn sub(glyph: &str, label: &str, shortcut: &str) -> MenuItem {
    MenuItem {
        sub: true,
        ..item(glyph, label, shortcut)
    }
}

fn danger(glyph: &str, label: &str, shortcut: &str) -> MenuItem {
    MenuItem {
        danger: true,
        ..item(glyph, label, shortcut)
    }
}

fn sep() -> MenuItem {
    MenuItem {
        sep: true,
        ..item("", "", "")
    }
}

/// The file menu, in the spec's order: Open · Open with › · Open in
/// viewer · ─ · Cut · Copy · Copy path · Paste · ─ · Rename · Duplicate ·
/// Compress · Pin to sidebar · ─ · Properties · ─ · Move to trash ·
/// Delete permanently. Shortcuts are only the ones the spec gives.
pub fn file_menu() -> Vec<MenuItem> {
    vec![
        item("󰏌", "Open", "Enter"),
        sub("󰘔", "Open with", ""),
        item("󰈟", "Open in viewer", ""),
        sep(),
        item("󰆐", "Cut", "Ctrl+X"),
        item("󰆏", "Copy", "Ctrl+C"),
        item("󰌷", "Copy path", ""),
        item("󰆒", "Paste", "Ctrl+V"),
        sep(),
        item("󰏫", "Rename", "F2"),
        item("󰆑", "Duplicate", ""),
        item("󰗄", "Compress", ""),
        item("󰐃", "Pin to sidebar", ""),
        sep(),
        item("󰋽", "Properties", ""),
        sep(),
        item("󰩺", "Move to trash", "Del"),
        danger("󰀦", "Delete permanently", "Shift+Del"),
    ]
}

/// The background menu: New folder · New file › · Paste · Select all ·
/// View › · Sort by › · Show hidden · Open terminal here · Properties.
pub fn background_menu() -> Vec<MenuItem> {
    vec![
        item("󰉗", "New folder", "Ctrl+Shift+N"),
        sub("󰝒", "New file", "Ctrl+Alt+N"),
        item("󰆒", "Paste", "Ctrl+V"),
        item("󰒆", "Select all", "Ctrl+A"),
        sub("󰕰", "View", ""),
        sub("󰒺", "Sort by", ""),
        item("󰈈", "Show hidden", "Ctrl+H"),
        item("󰆍", "Open terminal here", ""),
        item("󰋽", "Properties", ""),
    ]
}

/// One overlay column: a header and (keys, what they do) lines.
pub type Section = (&'static str, Vec<(Vec<&'static str>, &'static str)>);

/// The overlay's three columns, NAVIGATE / FILES / VIEW. Opened from the
/// viewer they hold the viewer's keys (DESIGN §5 keyboard map).
pub fn shortcut_sections(viewer: bool) -> Vec<Section> {
    if viewer {
        vec![
            (
                "NAVIGATE",
                vec![
                    (vec!["←", "→"], "previous / next"),
                    (vec!["h", "l"], "previous / next"),
                    (vec!["Home", "End"], "first / last"),
                    (vec!["Space"], "play, or next image"),
                    (vec!["Enter"], "fullscreen"),
                    (vec!["F11"], "fullscreen"),
                    (vec!["Esc", "q"], "back to the browser"),
                ],
            ),
            (
                "FILES",
                vec![
                    (vec!["Del"], "move to trash"),
                    (vec!["Shift", "Del"], "delete permanently"),
                    (vec!["Ctrl", "C"], "copy image"),
                ],
            ),
            (
                "VIEW",
                vec![
                    (vec!["+", "−"], "zoom in / out"),
                    (vec!["0"], "fit"),
                    (vec!["1"], "100%"),
                    (vec!["r", "R"], "rotate cw / ccw"),
                    (vec!["f"], "filmstrip"),
                    (vec!["i"], "info"),
                    (vec!["s"], "slideshow"),
                    (vec![",", "."], "frame step"),
                    (vec!["j", "k"], "seek ∓5s"),
                    (vec!["m"], "mute"),
                    (vec!["?"], "this list"),
                ],
            ),
        ]
    } else {
        vec![
            (
                "NAVIGATE",
                vec![
                    (vec!["↑", "↓"], "move"),
                    (vec!["Home", "End"], "first / last"),
                    (vec!["Enter"], "open"),
                    (vec!["Backspace"], "parent folder"),
                    (vec!["Ctrl", "L"], "edit path"),
                    (vec!["Ctrl", "F"], "search"),
                    (vec!["/"], "search"),
                    (vec!["Ctrl", "B"], "collapse sidebar"),
                    (vec!["Esc"], "cancel"),
                ],
            ),
            (
                "FILES",
                vec![
                    (vec!["Ctrl", "X"], "cut"),
                    (vec!["Ctrl", "C"], "copy"),
                    (vec!["Ctrl", "V"], "paste"),
                    (vec!["F2"], "rename"),
                    (vec!["Ctrl", "Shift", "N"], "new folder"),
                    (vec!["Ctrl", "Alt", "N"], "new file"),
                    (vec!["Del"], "move to trash"),
                    (vec!["Shift", "Del"], "delete permanently"),
                ],
            ),
            (
                "VIEW",
                vec![
                    (vec!["Ctrl", "1"], "list"),
                    (vec!["Ctrl", "2"], "grid"),
                    (vec!["Ctrl", "="], "larger tiles"),
                    (vec!["Ctrl", "-"], "smaller tiles"),
                    (vec!["Ctrl", "H"], "hidden files"),
                    (vec!["Ctrl", "I"], "statistics"),
                    (vec!["?"], "this list"),
                ],
            ),
        ]
    }
}

/// The lines whose keys or description contain `filter` (any case); a
/// column left empty keeps its header so the layout doesn't jump.
pub fn filter_sections(sections: Vec<Section>, filter: &str) -> Vec<Section> {
    let needle = filter.trim().to_lowercase();
    if needle.is_empty() {
        return sections;
    }
    sections
        .into_iter()
        .map(|(title, rows)| {
            let rows = rows
                .into_iter()
                .filter(|(keys, label)| {
                    label.to_lowercase().contains(&needle)
                        || keys.iter().any(|k| k.to_lowercase() == needle)
                })
                .collect();
            (title, rows)
        })
        .collect()
}

/// The overlay's columns as the window's model.
pub fn key_sections(viewer: bool, filter: &str) -> Vec<KeySection> {
    filter_sections(shortcut_sections(viewer), filter)
        .into_iter()
        .map(|(title, rows)| KeySection {
            title: title.into(),
            rows: ModelRc::new(VecModel::from(
                rows.into_iter()
                    .map(|(keys, label)| KeyRow {
                        keys: ModelRc::new(VecModel::from(
                            keys.into_iter().map(SharedString::from).collect::<Vec<_>>(),
                        )),
                        label: label.into(),
                    })
                    .collect::<Vec<_>>(),
            )),
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn labels(items: &[MenuItem]) -> Vec<String> {
        items
            .iter()
            .map(|i| {
                if i.sep {
                    "─".to_string()
                } else {
                    format!("{}{}", i.label, if i.sub { " ›" } else { "" })
                }
            })
            .collect()
    }

    #[test]
    fn the_file_menu_follows_the_spec_with_delete_last_in_danger() {
        let menu = file_menu();
        assert_eq!(
            labels(&menu).join(" · "),
            "Open · Open with › · Open in viewer · ─ · Cut · Copy · Copy path · Paste · ─ · \
             Rename · Duplicate · Compress · Pin to sidebar · ─ · Properties · ─ · \
             Move to trash · Delete permanently"
        );
        let last = menu.last().unwrap();
        assert!(last.danger && last.label == "Delete permanently");
        assert_eq!(menu.iter().filter(|i| i.danger).count(), 1);
    }

    #[test]
    fn the_background_menu_follows_the_spec() {
        assert_eq!(
            labels(&background_menu()).join(" · "),
            "New folder · New file › · Paste · Select all · View › · Sort by › · \
             Show hidden · Open terminal here · Properties"
        );
    }

    #[test]
    fn the_overlay_has_three_columns_and_the_viewer_its_own_keys() {
        let browser = shortcut_sections(false);
        let viewer = shortcut_sections(true);
        let titles = |s: &[Section]| s.iter().map(|c| c.0).collect::<Vec<_>>();
        assert_eq!(titles(&browser), ["NAVIGATE", "FILES", "VIEW"]);
        assert_eq!(titles(&viewer), ["NAVIGATE", "FILES", "VIEW"]);
        assert_ne!(browser, viewer, "the viewer frame is not the browser's");
        let has = |s: &[Section], label| s.iter().any(|c| c.1.iter().any(|r| r.1 == label));
        assert!(has(&viewer, "slideshow") && !has(&browser, "slideshow"));
        assert!(has(&browser, "new folder") && !has(&viewer, "new folder"));
    }

    #[test]
    fn the_filter_keeps_matching_lines_and_every_header() {
        let found = filter_sections(shortcut_sections(false), "TRASH");
        assert_eq!(found.len(), 3);
        let lines: Vec<&str> = found.iter().flat_map(|c| c.1.iter().map(|r| r.1)).collect();
        assert_eq!(lines, ["move to trash"]);
        let by_key = filter_sections(shortcut_sections(false), "f2");
        assert_eq!(by_key[1].1.len(), 1);
        assert_eq!(filter_sections(shortcut_sections(true), " ").len(), 3);
        assert_eq!(key_sections(false, "")[0].title, "NAVIGATE");
    }
}
