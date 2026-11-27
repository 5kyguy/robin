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
        ..MenuItem::default()
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

/// What a file-menu line does when picked.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FileAct {
    Open,
    OpenWith,
    Cut,
    Copy,
    Paste,
    Rename,
    /// Move to Trash, or Delete permanently where there is no trash.
    Remove,
    CopyPath,
    Properties,
}

/// What the file menu needs to know about the selection and the folder
/// to grey out what can't be done there, and say why.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct FileCtx {
    /// Selected items (the right-clicked one is always among them).
    pub count: usize,
    /// The right-clicked item is a folder.
    pub dir: bool,
    /// Something is cut or copied.
    pub can_paste: bool,
    /// No trash here (a phone, a drive without one, the Trash itself):
    /// the remove line is "Delete permanently".
    pub no_trash: bool,
    /// The open folder is the Trash.
    pub in_trash: bool,
    /// Why nothing can be added here (a phone's top folder, a scripted
    /// no-write folder).
    pub write: Option<String>,
    /// Why what is here can't be deleted (a phone's storages).
    pub remove: Option<String>,
    /// Why what is here can't be renamed.
    pub rename: Option<String>,
    /// Why nothing here can change: a read-only drive, or a folder the
    /// user has no write permission on.
    pub read_only: Option<String>,
}

/// A greyed-out line keeps its place in the menu; this is the reason
/// its tooltip gives.
const ONE_TO_RENAME: &str = "Select one item to rename";
const ONE_TO_OPEN_WITH: &str = "Select one item to open with another app";
const FOLDERS_OPEN_HERE: &str = "Folders open in Robin";
const NOTHING_TO_PASTE: &str = "Nothing to paste: cut or copy something first";
const RESTORE_FIRST: &str = "Restore it from the Trash first";

/// The file menu (DESIGN §7), top to bottom: Open · Open with… · ─ ·
/// Cut · Copy · Paste · Rename · ─ · Move to Trash (or Delete
/// permanently) · ─ · Copy path · Properties. Each line comes with what
/// it does; lines the location can't do are greyed with a reason.
pub fn file_menu_for(ctx: &FileCtx, compact: bool) -> Vec<(MenuItem, Option<FileAct>)> {
    let many = ctx.count > 1;
    let blocked = |why: Option<&String>| why.cloned();
    let open_with = if many {
        Some(ONE_TO_OPEN_WITH.to_string())
    } else if ctx.dir {
        Some(FOLDERS_OPEN_HERE.to_string())
    } else {
        None
    };
    let cut = if ctx.in_trash {
        Some(RESTORE_FIRST.to_string())
    } else {
        blocked(ctx.read_only.as_ref().or(ctx.remove.as_ref()))
    };
    let paste = if ctx.in_trash {
        Some("Nothing can be pasted into the Trash".to_string())
    } else if let Some(why) = ctx.read_only.as_ref().or(ctx.write.as_ref()) {
        Some(why.clone())
    } else if !ctx.can_paste {
        Some(NOTHING_TO_PASTE.to_string())
    } else {
        None
    };
    let rename = if many {
        Some(ONE_TO_RENAME.to_string())
    } else if ctx.in_trash {
        Some(RESTORE_FIRST.to_string())
    } else {
        blocked(ctx.rename.as_ref().or(ctx.read_only.as_ref()))
    };
    let remove = blocked(ctx.remove.as_ref().or(ctx.read_only.as_ref()));
    let remove_item = if ctx.no_trash || ctx.in_trash {
        danger("󰀦", "Delete permanently", "Del")
    } else {
        item("󰩺", "Move to Trash", "Del")
    };
    let lines: Vec<(MenuItem, Option<FileAct>, Option<String>)> = vec![
        (item("󰏌", "Open", "Enter"), Some(FileAct::Open), None),
        (
            item("󰘔", "Open with…", ""),
            Some(FileAct::OpenWith),
            open_with,
        ),
        (sep(), None, None),
        (item("󰆐", "Cut", "Ctrl+X"), Some(FileAct::Cut), cut),
        (item("󰆏", "Copy", "Ctrl+C"), Some(FileAct::Copy), None),
        (item("󰆒", "Paste", "Ctrl+V"), Some(FileAct::Paste), paste),
        (item("󰏫", "Rename", "F2"), Some(FileAct::Rename), rename),
        (sep(), None, None),
        (remove_item, Some(FileAct::Remove), remove),
        (sep(), None, None),
        (item("󰌷", "Copy path", ""), Some(FileAct::CopyPath), None),
        (item("󰋽", "Properties", ""), Some(FileAct::Properties), None),
    ];
    let line_h = if compact { 24.0 } else { 28.0 };
    let mut top = 0.0;
    lines
        .into_iter()
        .map(|(mut it, act, why)| {
            it.top = top;
            top += if it.sep { 9.0 } else { line_h };
            if let Some(why) = why {
                it.off = true;
                it.tip = why.into();
            }
            (it, act)
        })
        .collect()
}

/// The file menu for one file in a folder that can do everything: the
/// `menu-file` mock frame.
pub fn file_menu() -> Vec<MenuItem> {
    let ctx = FileCtx {
        count: 1,
        can_paste: true,
        ..FileCtx::default()
    };
    file_menu_for(&ctx, false)
        .into_iter()
        .map(|l| l.0)
        .collect()
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
                    (vec!["Alt", "←", "→"], "back / forward"),
                    (vec!["M4", "M5"], "mouse back / forward"),
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
    fn the_file_menu_follows_the_spec() {
        let menu = file_menu();
        assert_eq!(
            labels(&menu).join(" · "),
            "Open · Open with… · ─ · Cut · Copy · Paste · Rename · ─ · \
             Move to Trash · ─ · Copy path · Properties"
        );
        assert!(menu.iter().all(|i| !i.off), "a plain file can do it all");
        let rename = menu.iter().find(|i| i.label == "Rename").unwrap();
        assert_eq!(rename.shortcut, "F2");
        // Lines are 28 tall, separators 9: Rename sits under two lines,
        // a separator and three more.
        assert_eq!(rename.top, 28.0 * 5.0 + 9.0);
    }

    fn ctx(f: impl FnOnce(&mut FileCtx)) -> Vec<(MenuItem, Option<FileAct>)> {
        let mut c = FileCtx {
            count: 1,
            can_paste: true,
            ..FileCtx::default()
        };
        f(&mut c);
        file_menu_for(&c, false)
    }

    fn line(menu: &[(MenuItem, Option<FileAct>)], act: FileAct) -> MenuItem {
        menu.iter().find(|l| l.1 == Some(act)).unwrap().0.clone()
    }

    #[test]
    fn without_a_trash_the_remove_line_deletes_permanently_in_danger() {
        let menu = ctx(|c| c.no_trash = true);
        let remove = line(&menu, FileAct::Remove);
        assert_eq!(remove.label, "Delete permanently");
        assert!(remove.danger && !remove.off);
        assert_eq!(menu.iter().filter(|l| l.0.danger).count(), 1);
    }

    #[test]
    fn a_phone_folder_that_refuses_renames_greys_rename_with_its_reason() {
        let why = "Pixel 8 doesn't allow renaming in Android/data";
        let menu = ctx(|c| {
            c.no_trash = true;
            c.rename = Some(why.into());
        });
        let rename = line(&menu, FileAct::Rename);
        assert!(rename.off);
        assert_eq!(rename.tip, why);
        assert!(!line(&menu, FileAct::Cut).off && !line(&menu, FileAct::Remove).off);
    }

    #[test]
    fn a_read_only_drive_greys_every_change_but_not_copy() {
        let menu = ctx(|c| c.read_only = Some("SANDISK is read-only".into()));
        for act in [
            FileAct::Cut,
            FileAct::Paste,
            FileAct::Rename,
            FileAct::Remove,
        ] {
            let l = line(&menu, act);
            assert!(l.off, "{act:?}");
            assert_eq!(l.tip, "SANDISK is read-only");
        }
        for act in [
            FileAct::Open,
            FileAct::OpenWith,
            FileAct::Copy,
            FileAct::CopyPath,
            FileAct::Properties,
        ] {
            assert!(!line(&menu, act).off, "{act:?}");
        }
    }

    #[test]
    fn several_items_grey_rename_and_open_with_only() {
        let menu = ctx(|c| c.count = 3);
        assert_eq!(line(&menu, FileAct::Rename).tip, ONE_TO_RENAME);
        assert_eq!(line(&menu, FileAct::OpenWith).tip, ONE_TO_OPEN_WITH);
        let off: Vec<_> = menu.iter().filter(|l| l.0.off).map(|l| l.1).collect();
        assert_eq!(off, [Some(FileAct::OpenWith), Some(FileAct::Rename)]);
    }

    #[test]
    fn paste_needs_something_cut_or_copied_and_folders_have_no_open_with() {
        let menu = ctx(|c| {
            c.can_paste = false;
            c.dir = true;
        });
        assert_eq!(line(&menu, FileAct::Paste).tip, NOTHING_TO_PASTE);
        assert_eq!(line(&menu, FileAct::OpenWith).tip, FOLDERS_OPEN_HERE);
        let top = ctx(|c| c.write = Some("Files can only go inside a storage on Pixel 8".into()));
        assert!(line(&top, FileAct::Paste).off);
    }

    #[test]
    fn the_trash_offers_delete_permanently_and_no_rename() {
        let menu = ctx(|c| c.in_trash = true);
        assert_eq!(line(&menu, FileAct::Remove).label, "Delete permanently");
        assert_eq!(line(&menu, FileAct::Rename).tip, RESTORE_FIRST);
        assert!(line(&menu, FileAct::Paste).off && line(&menu, FileAct::Cut).off);
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
