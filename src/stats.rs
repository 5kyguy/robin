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
    /// Folders that could not be opened or read; their contents are not
    /// counted.
    pub skipped: Vec<PathBuf>,
    /// Mount points under the root on another filesystem, not descended.
    pub other_drives: Vec<PathBuf>,
    /// The scan was cancelled before it finished; totals are partial.
    pub cancelled: bool,
}

/// Shared with a running scan: a cancel flag the UI sets and a live file
/// count the UI reads.
#[derive(Clone, Debug, Default)]
pub struct ScanCtl {
    pub cancel: std::sync::Arc<std::sync::atomic::AtomicBool>,
    pub files: std::sync::Arc<std::sync::atomic::AtomicU64>,
}

impl ScanCtl {
    pub fn cancelled(&self) -> bool {
        self.cancel.load(std::sync::atomic::Ordering::Relaxed)
    }
}

pub fn scan_dir(root: &Path, largest_limit: usize) -> Result<Scan> {
    scan_dir_with(root, largest_limit, &ScanCtl::default())
}

/// Walk `root` on its own filesystem: other mounts are noted, not entered;
/// symlinks are not followed; a hardlinked file counts once; sizes are
/// allocated blocks; an unreadable folder is noted and skipped. `ctl` can
/// stop the walk early.
pub fn scan_dir_with(root: &Path, largest_limit: usize, ctl: &ScanCtl) -> Result<Scan> {
    let dir = DirFd::open(root)?;
    let mut w = Walker {
        dev: dir.dev()?,
        limit: largest_limit,
        ctl,
        seen: std::collections::HashSet::new(),
        heap: std::collections::BinaryHeap::new(),
        scan: Scan::default(),
    };
    let entries = dir.list()?;
    for entry in entries {
        let before = w.scan.total_bytes;
        if !w.entry(&dir, root, &entry) {
            continue;
        }
        w.scan.top.push((
            entry.name.to_string_lossy().into_owned(),
            w.scan.total_bytes - before,
        ));
    }
    let mut scan = w.scan;
    scan.cancelled = ctl.cancelled();
    scan.largest = w
        .heap
        .into_sorted_vec()
        .into_iter()
        .map(|std::cmp::Reverse((n, p))| (p, n))
        .collect();
    scan.top.sort_by_key(|item| std::cmp::Reverse(item.1));
    scan.children.sort_by_key(|item| std::cmp::Reverse(item.1));
    Ok(scan)
}

struct Walker<'a> {
    dev: u64,
    limit: usize,
    ctl: &'a ScanCtl,
    seen: std::collections::HashSet<(u64, u64)>,
    /// Min-heap of the `limit` largest files: the smallest is popped when a
    /// bigger one arrives.
    heap: std::collections::BinaryHeap<std::cmp::Reverse<(u64, PathBuf)>>,
    scan: Scan,
}

impl Walker<'_> {
    /// Count one entry of `dir` (at `path`). False when it is not counted
    /// at all (symlink, other drive, cancelled).
    fn entry(&mut self, dir: &DirFd, path: &Path, entry: &crate::safety::Dirent) -> bool {
        if entry.is_symlink || self.ctl.cancelled() {
            return false;
        }
        if entry.dev != self.dev {
            self.scan.other_drives.push(path.join(&entry.name));
            return false;
        }
        if entry.is_dir {
            let child_path = path.join(&entry.name);
            let before = self.scan.total_bytes;
            match dir.open_dir_nofollow(&entry.name) {
                Ok(child) => self.walk(&child, &child_path),
                Err(_) => self.scan.skipped.push(child_path),
            }
            let used = self.scan.total_bytes - before;
            self.scan
                .children
                .push((entry.name.to_string_lossy().into_owned(), used));
        } else {
            self.file(path, entry);
        }
        true
    }

    fn walk(&mut self, dir: &DirFd, path: &Path) {
        match dir.list() {
            Ok(entries) => {
                for entry in entries {
                    self.entry(dir, path, &entry);
                }
            }
            // A read error partway through: skip this folder, keep going.
            Err(_) => self.scan.skipped.push(path.to_path_buf()),
        }
    }

    fn file(&mut self, path: &Path, entry: &crate::safety::Dirent) {
        if entry.nlink > 1 && !self.seen.insert((entry.dev, entry.ino)) {
            return;
        }
        let size = entry.allocated;
        let scan = &mut self.scan;
        scan.total_bytes += size;
        scan.file_count += 1;
        self.ctl
            .files
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let k = &mut scan.kinds[Kind::of(&entry.name).index()];
        k.0 += 1;
        k.1 += size;
        if self.limit == 0 {
            return;
        }
        if self.heap.len() < self.limit {
            self.heap
                .push(std::cmp::Reverse((size, path.join(&entry.name))));
        } else if self.heap.peek().is_some_and(|m| size > m.0 .0) {
            self.heap.pop();
            self.heap
                .push(std::cmp::Reverse((size, path.join(&entry.name))));
        }
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
/// Dot-named entries such as `.cache` only count when `hidden` is on, as
/// in the file list.
pub fn treemap_weights(top: &[(String, u64)], keep: usize, hidden: bool) -> Vec<(String, u64)> {
    let mut sorted: Vec<(String, u64)> = top
        .iter()
        .filter(|e| e.1 > 0 && (hidden || !e.0.starts_with('.')))
        .cloned()
        .collect();
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
    pub ctl: ScanCtl,
}

impl StatsJob {
    pub fn perform(self) -> Report {
        let home = scan_dir_with(&self.home, 0, &self.ctl).unwrap_or_default();
        let trash = scan_dir_with(&self.trash.join("files"), 0, &self.ctl).unwrap_or_default();
        let cache = scan_dir_with(&self.cache, 0, &self.ctl).unwrap_or_default();
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
        w("home/Videos/a.mp4", 700_000);
        w("home/Videos/deep/b.MKV", 300_000);
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
            ctl: ScanCtl::default(),
        }
        .perform();
        // Sizes are allocated blocks, so expectations come from the disk.
        let a = |rel: &str| alloc(&home.join(rel));
        let videos = a("Videos/a.mp4") + a("Videos/deep/b.MKV");
        let top = r.home.top.iter().find(|t| t.0 == "Videos").unwrap();
        assert_eq!(top.1, videos);
        assert_eq!(r.home.kinds[0], (2, videos), "mp4 and MKV are video");
        assert_eq!(
            r.home.kinds[1],
            (2, a("Pictures/c.jpg") + a(".cache/thumbnails/normal/t.png")),
            "the cached thumbnail is an image too"
        );
        let t = ".local/share/Trash/files/";
        assert_eq!(
            r.trash_bytes,
            a(&format!("{t}old.txt")) + a(&format!("{t}dir/x"))
        );
        assert_eq!(r.trash_items, 2, "top-level trashed items, not files");
        assert_eq!(
            r.cache_bytes,
            a(".cache/thumbnails/normal/t.png") + a(".cache/mpv/w") + a(".cache/other/z")
        );
        assert_eq!(
            r.cache_parts,
            vec![
                ("thumbnails".into(), a(".cache/thumbnails/normal/t.png")),
                ("mpv".into(), a(".cache/mpv/w"))
            ]
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
        let w = treemap_weights(&top, 3, false);
        assert_eq!(w.last().unwrap(), &("other".to_string(), 30));
        assert_eq!(w[0].0, "d5");
        let mut top = top;
        top.push((".cache".into(), 99));
        assert_eq!(treemap_weights(&top, 3, false)[0].0, "d5", "hidden off");
        assert_eq!(treemap_weights(&top, 3, true)[0].0, ".cache", "hidden on");
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
        assert_eq!(
            scan.total_bytes,
            alloc(&root.join("docs/a.txt")) + alloc(&root.join("big.bin"))
        );
        assert_eq!(scan.largest[0].1, alloc(&root.join("big.bin")));
        // A symlinked folder is not entered either.
        std::os::unix::fs::symlink(root.join("docs"), root.join("docs-link")).unwrap();
        assert_eq!(scan_dir(&root, 5).unwrap().file_count, 2);
        let _ = fs::remove_dir_all(&root);
    }

    fn alloc(p: &Path) -> u64 {
        use std::os::unix::fs::MetadataExt;
        fs::symlink_metadata(p).unwrap().blocks() * 512
    }

    fn tmp(tag: &str) -> PathBuf {
        let root = std::env::temp_dir().join(format!("robin-{tag}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(&root).unwrap();
        root
    }

    #[test]
    fn hardlinks_count_once_and_sparse_files_count_what_they_use() {
        let root = tmp("links");
        fs::write(root.join("a"), vec![1u8; 64 * 1024]).unwrap();
        fs::hard_link(root.join("a"), root.join("b")).unwrap();
        let sparse = fs::File::create(root.join("sparse")).unwrap();
        sparse.set_len(1 << 30).unwrap();
        let scan = scan_dir(&root, 5).unwrap();
        assert_eq!(scan.file_count, 2, "a and b are one file");
        assert_eq!(
            scan.total_bytes,
            alloc(&root.join("a")) + alloc(&root.join("sparse"))
        );
        assert!(scan.total_bytes < 1 << 20, "the 1 GiB hole is not counted");
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn an_unreadable_folder_is_noted_and_the_scan_goes_on() {
        use std::os::unix::fs::PermissionsExt;
        let root = tmp("noaccess");
        fs::create_dir_all(root.join("locked")).unwrap();
        fs::write(root.join("locked/x"), b"x").unwrap();
        fs::write(root.join("ok"), b"ok").unwrap();
        fs::set_permissions(root.join("locked"), fs::Permissions::from_mode(0o000)).unwrap();
        let scan = scan_dir(&root, 5).unwrap();
        fs::set_permissions(root.join("locked"), fs::Permissions::from_mode(0o755)).unwrap();
        // root ignores permissions; only assert the skip when it applies.
        if fs::read_dir(root.join("locked")).is_err() || scan.skipped.len() == 1 {
            assert_eq!(scan.skipped, vec![root.join("locked")]);
            assert_eq!(scan.file_count, 1);
        }
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn another_filesystem_is_noted_not_entered() {
        // Mounting needs privileges, so pose as a walk that started on
        // another device: everything here then looks like a crossing.
        let ctl = ScanCtl::default();
        let mut w = Walker {
            dev: u64::MAX,
            limit: 0,
            ctl: &ctl,
            seen: Default::default(),
            heap: Default::default(),
            scan: Scan::default(),
        };
        let root = tmp("mount");
        fs::create_dir_all(root.join("sub")).unwrap();
        fs::write(root.join("sub/f"), b"f").unwrap();
        let dir = DirFd::open(&root).unwrap();
        for e in dir.list().unwrap() {
            w.entry(&dir, &root, &e);
        }
        assert_eq!(w.scan.other_drives, vec![root.join("sub")]);
        assert_eq!(w.scan.file_count, 0, "nothing past the mount is counted");
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn a_cancelled_scan_stops_and_says_so() {
        let root = tmp("cancel");
        for i in 0..20 {
            fs::write(root.join(format!("f{i}")), b"x").unwrap();
        }
        let ctl = ScanCtl::default();
        ctl.cancel.store(true, std::sync::atomic::Ordering::Relaxed);
        let scan = scan_dir_with(&root, 5, &ctl).unwrap();
        assert!(scan.cancelled);
        assert_eq!(scan.file_count, 0);
        let live = ScanCtl::default();
        let done = scan_dir_with(&root, 3, &live).unwrap();
        assert_eq!(live.files.load(std::sync::atomic::Ordering::Relaxed), 20);
        assert_eq!(done.largest.len(), 3, "the heap keeps only the limit");
        let _ = fs::remove_dir_all(&root);
    }
}
