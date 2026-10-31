//! Space usage. Walks do not follow symlinks and do not enter a pseudo filesystem.
//!
//! The treemap is squarified: rectangles keep names readable and area is
//! easier to compare than an arc.

use std::path::{Path, PathBuf};

use crate::error::Result;
use crate::safety::DirFd;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum UsageLevel {
    Accent,
    Warning,
    Danger,
}

pub fn usage_level(used_ratio: f32) -> UsageLevel {
    if used_ratio >= 0.95 {
        UsageLevel::Danger
    } else if used_ratio >= 0.85 {
        UsageLevel::Warning
    } else {
        UsageLevel::Accent
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct Rect {
    pub x: f32,
    pub y: f32,
    pub w: f32,
    pub h: f32,
    pub name: String,
    pub bytes: u64,
    /// Squarify recursion depth. Deeper rows sit inside the remainder of a
    /// parent row and are drawn darker than that parent.
    pub depth: u32,
}

/// Squarified treemap of `weights` into a `width` by `height` box.
pub fn squarify(weights: &[(String, u64)], width: f32, height: f32) -> Vec<Rect> {
    let total: u64 = weights.iter().map(|(_, n)| *n).sum();
    if total == 0 || width <= 0.0 || height <= 0.0 {
        return Vec::new();
    }
    let mut items: Vec<(String, f32)> = weights
        .iter()
        .filter(|(_, n)| *n > 0)
        .map(|(name, n)| (name.clone(), (*n as f32 / total as f32) * width * height))
        .collect();
    items.sort_by_key(|item| item.1.to_bits());
    items.reverse();
    let mut out = Vec::new();
    layout(&items, 0.0, 0.0, width, height, 0, &mut out);
    out
}

fn layout(
    items: &[(String, f32)],
    x: f32,
    y: f32,
    w: f32,
    h: f32,
    depth: u32,
    out: &mut Vec<Rect>,
) {
    if items.is_empty() || w <= 0.0 || h <= 0.0 {
        return;
    }
    if items.len() == 1 {
        out.push(Rect {
            x,
            y,
            w,
            h,
            name: items[0].0.clone(),
            bytes: items[0].1 as u64,
            depth,
        });
        return;
    }
    let vertical = w < h;
    let side = if vertical { w } else { h };
    let mut row = Vec::new();
    let mut rest_start = 0;
    let mut best = f32::MAX;
    for (index, item) in items.iter().enumerate() {
        row.push(item.clone());
        let worst = worst_ratio(&row, side);
        if worst <= best {
            best = worst;
            rest_start = index + 1;
        } else {
            row.pop();
            break;
        }
    }
    let row_area: f32 = row.iter().map(|(_, a)| *a).sum();
    let thickness = if side == 0.0 { 0.0 } else { row_area / side };
    let mut cursor = 0.0;
    for (name, area) in &row {
        let length = if thickness == 0.0 {
            0.0
        } else {
            area / thickness
        };
        if vertical {
            // Horizontal strip along the top. `thickness` is its height.
            out.push(Rect {
                x: x + cursor,
                y,
                w: length,
                h: thickness,
                name: name.clone(),
                bytes: *area as u64,
                depth,
            });
        } else {
            // Vertical strip along the left. `thickness` is its width.
            out.push(Rect {
                x,
                y: y + cursor,
                w: thickness,
                h: length,
                name: name.clone(),
                bytes: *area as u64,
                depth,
            });
        }
        cursor += length;
    }
    if vertical {
        layout(
            &items[rest_start..],
            x,
            y + thickness,
            w,
            h - thickness,
            depth + 1,
            out,
        );
    } else {
        layout(
            &items[rest_start..],
            x + thickness,
            y,
            w - thickness,
            h,
            depth + 1,
            out,
        );
    }
}

fn worst_ratio(row: &[(String, f32)], side: f32) -> f32 {
    let sum: f32 = row.iter().map(|(_, a)| *a).sum();
    if sum == 0.0 || side == 0.0 {
        return f32::MAX;
    }
    let mut worst = 0.0_f32;
    for (_, area) in row {
        let w = side;
        let h = area / (sum / side);
        let ratio = (w / h).max(h / w);
        worst = worst.max(ratio);
    }
    worst
}

/// File kinds the overview counts, in display order.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    Video,
    Images,
    Code,
    Archives,
    Audio,
    Docs,
    Other,
}

pub const KINDS: [Kind; 7] = [
    Kind::Video,
    Kind::Images,
    Kind::Code,
    Kind::Archives,
    Kind::Audio,
    Kind::Docs,
    Kind::Other,
];

impl Kind {
    pub fn name(self) -> &'static str {
        match self {
            Kind::Video => "Video",
            Kind::Images => "Images",
            Kind::Code => "Code",
            Kind::Archives => "Archives",
            Kind::Audio => "Audio",
            Kind::Docs => "Docs",
            Kind::Other => "Other",
        }
    }

    /// The kind of a file, by its extension.
    pub fn of(name: &std::ffi::OsStr) -> Kind {
        let ext = Path::new(name)
            .extension()
            .map(|e| e.to_string_lossy().to_ascii_lowercase())
            .unwrap_or_default();
        match ext.as_str() {
            "mp4" | "mkv" | "webm" | "mov" | "avi" | "m4v" | "wmv" | "flv" | "mpg" | "mpeg"
            | "ts" => Kind::Video,
            "jpg" | "jpeg" | "png" | "gif" | "webp" | "bmp" | "tif" | "tiff" | "heic" | "heif"
            | "avif" | "svg" | "raw" | "cr2" | "nef" | "dng" | "xcf" | "psd" => Kind::Images,
            "rs" | "c" | "h" | "cpp" | "hpp" | "py" | "js" | "tsx" | "jsx" | "go" | "java"
            | "kt" | "rb" | "sh" | "toml" | "json" | "yaml" | "yml" | "html" | "css" | "slint"
            | "lua" | "zig" | "swift" | "o" | "rlib" | "a" | "so" => Kind::Code,
            "zip" | "tar" | "gz" | "tgz" | "xz" | "zst" | "bz2" | "7z" | "rar" | "iso" | "deb"
            | "rpm" => Kind::Archives,
            "mp3" | "flac" | "ogg" | "opus" | "wav" | "m4a" | "aac" | "wma" => Kind::Audio,
            "pdf" | "txt" | "md" | "doc" | "docx" | "odt" | "ods" | "odp" | "xls" | "xlsx"
            | "ppt" | "pptx" | "epub" | "rtf" | "csv" => Kind::Docs,
            _ => Kind::Other,
        }
    }

    fn index(self) -> usize {
        KINDS.iter().position(|k| *k == self).unwrap_or(6)
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Scan {
    pub total_bytes: u64,
    pub file_count: u64,
    pub largest: Vec<(PathBuf, u64)>,
    /// Every folder under the root with its total, deepest first.
    pub children: Vec<(String, u64)>,
    /// The root's own entries (folders and files) with their totals.
    pub top: Vec<(String, u64)>,
    /// (files, bytes) per kind, in `KINDS` order.
    pub kinds: [(u64, u64); 7],
}

pub fn scan_dir(root: &Path, largest_limit: usize) -> Result<Scan> {
    let dir = DirFd::open(root)?;
    let mut scan = Scan::default();
    for entry in dir.list()? {
        if entry.is_symlink {
            continue;
        }
        let before = scan.total_bytes;
        if entry.is_dir {
            if let Ok(child) = dir.open_dir_nofollow(&entry.name) {
                walk(&child, &root.join(&entry.name), &mut scan, largest_limit)?;
                let used = scan.total_bytes - before;
                scan.children
                    .push((entry.name.to_string_lossy().into_owned(), used));
            }
        } else {
            file(&mut scan, root, &entry, largest_limit);
        }
        scan.top.push((
            entry.name.to_string_lossy().into_owned(),
            scan.total_bytes - before,
        ));
    }
    scan.top.sort_by_key(|item| std::cmp::Reverse(item.1));
    scan.children.sort_by_key(|item| std::cmp::Reverse(item.1));
    scan.largest.sort_by_key(|item| std::cmp::Reverse(item.1));
    scan.largest.truncate(largest_limit);
    Ok(scan)
}

fn walk(dir: &DirFd, path: &Path, scan: &mut Scan, limit: usize) -> Result<()> {
    for entry in dir.list()? {
        if entry.is_symlink {
            continue;
        }
        if entry.is_dir {
            if let Ok(child) = dir.open_dir_nofollow(&entry.name) {
                let child_path = path.join(&entry.name);
                let before = scan.total_bytes;
                walk(&child, &child_path, scan, limit)?;
                let used = scan.total_bytes - before;
                scan.children
                    .push((entry.name.to_string_lossy().into_owned(), used));
            }
        } else {
            file(scan, path, &entry, limit);
        }
    }
    Ok(())
}

fn file(scan: &mut Scan, path: &Path, entry: &crate::safety::Dirent, limit: usize) {
    scan.total_bytes += entry.len;
    scan.file_count += 1;
    let k = &mut scan.kinds[Kind::of(&entry.name).index()];
    k.0 += 1;
    k.1 += entry.len;
    if scan.largest.len() < limit || scan.largest.last().is_some_and(|(_, n)| entry.len > *n) {
        scan.largest.push((path.join(&entry.name), entry.len));
        scan.largest.sort_by_key(|item| std::cmp::Reverse(item.1));
        scan.largest.truncate(limit);
    }
}

/// `1234567` as `1 234 567`, the way the overview prints counts.
pub fn group_digits(n: u64) -> String {
    let digits = n.to_string();
    let mut out = String::new();
    for (i, ch) in digits.chars().enumerate() {
        if i > 0 && (digits.len() - i).is_multiple_of(3) {
            out.push(' ');
        }
        out.push(ch);
    }
    out
}

/// The `keep` largest entries, the rest folded into one "other" cell.
pub fn treemap_weights(top: &[(String, u64)], keep: usize) -> Vec<(String, u64)> {
    let mut sorted: Vec<(String, u64)> = top.iter().filter(|e| e.1 > 0).cloned().collect();
    sorted.sort_by_key(|e| std::cmp::Reverse(e.1));
    let rest: u64 = sorted.iter().skip(keep).map(|e| e.1).sum();
    sorted.truncate(keep);
    if rest > 0 {
        sorted.push(("other".into(), rest));
    }
    sorted
}

/// One mounted disk: its name, where it's mounted, and its space.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Disk {
    pub name: String,
    pub mount: String,
    pub total: u64,
    pub free: u64,
}

impl Disk {
    pub fn used_ratio(&self) -> f32 {
        if self.total == 0 {
            0.0
        } else {
            (self.total - self.free.min(self.total)) as f32 / self.total as f32
        }
    }
}

/// Everything the Statistics view shows, gathered in one pass off the UI
/// thread.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Report {
    pub disks: Vec<Disk>,
    pub home: Scan,
    pub trash_bytes: u64,
    pub trash_items: u64,
    pub cache_bytes: u64,
    /// Named parts of the cache worth calling out (thumbnails, mpv, robin).
    pub cache_parts: Vec<(String, u64)>,
}

/// What a stats scan reads. Sent to a worker thread.
pub struct StatsJob {
    pub home: PathBuf,
    pub trash: PathBuf,
    pub cache: PathBuf,
    pub mounts: std::sync::Arc<dyn crate::mounts::MountSource>,
}

impl StatsJob {
    pub fn perform(self) -> Report {
        let home = scan_dir(&self.home, 0).unwrap_or_default();
        let trash = scan_dir(&self.trash.join("files"), 0).unwrap_or_default();
        let cache = scan_dir(&self.cache, 0).unwrap_or_default();
        let cache_parts = ["thumbnails", "mpv", "robin"]
            .iter()
            .filter_map(|name| {
                cache
                    .top
                    .iter()
                    .find(|(n, _)| n == name)
                    .map(|(n, b)| (n.clone(), *b))
            })
            .collect();
        Report {
            disks: disks(self.mounts.as_ref()),
            trash_bytes: trash.total_bytes,
            trash_items: trash.top.len() as u64,
            cache_bytes: cache.total_bytes,
            cache_parts,
            home,
        }
    }
}

/// Block-device mounts, one per device, with their space. Pseudo
/// filesystems are already gone from the mount list.
pub fn disks(mounts: &dyn crate::mounts::MountSource) -> Vec<Disk> {
    let mut seen = std::collections::HashSet::new();
    let mut out = Vec::new();
    for v in mounts.volumes().unwrap_or_default() {
        if !v.source.starts_with("/dev/") || !seen.insert(v.source.clone()) {
            continue;
        }
        let Ok(space) = mounts.space(Path::new(&v.mount_point)) else {
            continue;
        };
        if space.total == 0 {
            continue;
        }
        let name = match Path::new(&v.mount_point).file_name() {
            Some(n) => n.to_string_lossy().into_owned(),
            None => "System".into(),
        };
        out.push(Disk {
            name,
            mount: v.mount_point,
            total: space.total,
            free: space.free,
        });
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    #[test]
    fn bars_turn_at_85_and_95() {
        assert_eq!(usage_level(0.84), UsageLevel::Accent);
        assert_eq!(usage_level(0.85), UsageLevel::Warning);
        assert_eq!(usage_level(0.95), UsageLevel::Danger);
    }

    #[test]
    fn treemap_covers_the_box() {
        let cells = squarify(
            &[
                ("videos".into(), 400),
                ("pictures".into(), 100),
                ("docs".into(), 100),
            ],
            100.0,
            60.0,
        );
        assert_eq!(cells.len(), 3);
        let area: f32 = cells.iter().map(|c| c.w * c.h).sum();
        assert!((area - 6000.0).abs() < 400.0, "{area}");
        assert_eq!(cells[0].name, "videos");
        assert!(cells.iter().all(|c| c.w > 0.0 && c.h > 0.0));
        for (i, a) in cells.iter().enumerate() {
            for b in cells.iter().skip(i + 1) {
                let ox = (a.x + a.w).min(b.x + b.w) - a.x.max(b.x);
                let oy = (a.y + a.h).min(b.y + b.h) - a.y.max(b.y);
                assert!(ox < 0.5 || oy < 0.5, "{} overlaps {}", a.name, b.name);
            }
        }
        assert!(cells.iter().any(|c| c.depth > cells[0].depth));
    }

    struct OneDisk;
    impl crate::mounts::MountSource for OneDisk {
        fn volumes(&self) -> Result<Vec<crate::mounts::Volume>> {
            let v = |mount: &str, source: &str| crate::mounts::Volume {
                mount_point: mount.into(),
                fs_type: "ext4".into(),
                source: source.into(),
                root: "/".into(),
            };
            Ok(vec![
                v("/", "/dev/nvme0n1p2"),
                v("/home", "/dev/nvme0n1p2"),
                v("/mnt/archive", "/dev/sda1"),
                v("/run/user/1000/doc", "portal"),
            ])
        }
        fn space(&self, mount: &Path) -> Result<crate::mounts::Space> {
            Ok(match mount.to_str() {
                Some("/") => crate::mounts::Space {
                    total: 1000,
                    free: 420,
                },
                _ => crate::mounts::Space {
                    total: 4000,
                    free: 360,
                },
            })
        }
        fn removable(&self, _: &crate::mounts::Volume) -> bool {
            false
        }
    }

    #[test]
    fn a_report_totals_home_trash_cache_and_disks() {
        let root = std::env::temp_dir().join(format!("robin-report-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        let w = |rel: &str, n: usize| {
            let p = root.join(rel);
            fs::create_dir_all(p.parent().unwrap()).unwrap();
            fs::write(p, vec![0u8; n]).unwrap();
        };
        w("home/Videos/a.mp4", 700);
        w("home/Videos/deep/b.MKV", 300);
        w("home/Pictures/c.jpg", 200);
        w("home/notes.md", 40);
        w("home/.cache/thumbnails/normal/t.png", 30);
        w("home/.cache/mpv/w", 20);
        w("home/.cache/other/z", 5);
        w("home/.local/share/Trash/files/old.txt", 11);
        w("home/.local/share/Trash/files/dir/x", 9);
        w("home/.local/share/Trash/info/old.txt.trashinfo", 1);
        let home = root.join("home");
        let r = StatsJob {
            trash: home.join(".local/share/Trash"),
            cache: home.join(".cache"),
            home: home.clone(),
            mounts: std::sync::Arc::new(OneDisk),
        }
        .perform();
        assert_eq!(r.home.top[0], ("Videos".into(), 1000));
        assert_eq!(r.home.kinds[0], (2, 1000), "mp4 and MKV are video");
        assert_eq!(
            r.home.kinds[1],
            (2, 230),
            "the cached thumbnail is an image too"
        );
        assert_eq!(r.trash_bytes, 20);
        assert_eq!(r.trash_items, 2, "top-level trashed items, not files");
        assert_eq!(r.cache_bytes, 55);
        assert_eq!(
            r.cache_parts,
            vec![("thumbnails".into(), 30), ("mpv".into(), 20)]
        );
        let names: Vec<_> = r.disks.iter().map(|d| d.name.as_str()).collect();
        assert_eq!(
            names,
            ["System", "archive"],
            "one row per device, no portal"
        );
        assert!((r.disks[0].used_ratio() - 0.58).abs() < 1e-6);
        assert_eq!(usage_level(r.disks[1].used_ratio()), UsageLevel::Warning);
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn counts_group_and_small_folders_fold_into_other() {
        assert_eq!(group_digits(312_064), "312 064");
        assert_eq!(group_digits(1_840), "1 840");
        assert_eq!(group_digits(612), "612");
        let top: Vec<(String, u64)> = (1..=5).map(|i| (format!("d{i}"), i * 10)).collect();
        let w = treemap_weights(&top, 3);
        assert_eq!(w.last().unwrap(), &("other".to_string(), 30));
        assert_eq!(w[0].0, "d5");
    }

    #[test]
    fn scan_skips_symlinks() {
        let root = std::env::temp_dir().join(format!("robin-stats-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(root.join("docs")).unwrap();
        fs::write(root.join("docs/a.txt"), vec![1u8; 50]).unwrap();
        fs::write(root.join("big.bin"), vec![2u8; 80]).unwrap();
        std::os::unix::fs::symlink(root.join("big.bin"), root.join("link")).unwrap();
        let scan = scan_dir(&root, 5).unwrap();
        assert_eq!(scan.file_count, 2);
        assert_eq!(scan.total_bytes, 130);
        assert_eq!(scan.largest[0].1, 80);
        let _ = fs::remove_dir_all(&root);
    }
}
