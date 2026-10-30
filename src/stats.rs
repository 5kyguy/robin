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
    layout(&items, 0.0, 0.0, width, height, &mut out);
    out
}

fn layout(items: &[(String, f32)], x: f32, y: f32, w: f32, h: f32, out: &mut Vec<Rect>) {
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
            out.push(Rect {
                x,
                y: y + cursor,
                w,
                h: length,
                name: name.clone(),
                bytes: *area as u64,
            });
        } else {
            out.push(Rect {
                x: x + cursor,
                y,
                w: length,
                h,
                name: name.clone(),
                bytes: *area as u64,
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
            out,
        );
    } else {
        layout(
            &items[rest_start..],
            x + thickness,
            y,
            w - thickness,
            h,
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

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Scan {
    pub total_bytes: u64,
    pub file_count: u64,
    pub largest: Vec<(PathBuf, u64)>,
    pub children: Vec<(String, u64)>,
}

pub fn scan_dir(root: &Path, largest_limit: usize) -> Result<Scan> {
    let dir = DirFd::open(root)?;
    let mut scan = Scan::default();
    walk(&dir, root, &mut scan, largest_limit)?;
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
            scan.total_bytes += entry.len;
            scan.file_count += 1;
            if scan.largest.len() < limit
                || scan.largest.last().is_some_and(|(_, n)| entry.len > *n)
            {
                scan.largest.push((path.join(&entry.name), entry.len));
                scan.largest.sort_by_key(|item| std::cmp::Reverse(item.1));
                scan.largest.truncate(limit);
            }
        }
    }
    Ok(())
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
