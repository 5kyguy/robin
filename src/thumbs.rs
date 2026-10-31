//! Freedesktop thumbnail cache.
//!
//! Images are scaled with the `image` crate. Video and PDF thumbnails are
//! separate programs with a fixed argv, never a shell. Robin records the
//! files it writes and drops the oldest until that set fits in 256 MB. Other
//! apps' thumbnails in the shared cache are left alone.

use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::SystemTime;

use md5::{Digest, Md5};

use crate::error::{Error, Result};

pub const NORMAL: u32 = 128;
pub const LARGE: u32 = 256;
pub const BUDGET: u64 = 256 * 1024 * 1024;
/// The freedesktop `Software` key Robin stamps on every thumbnail it
/// writes, so Clear thumbnail cache can tell its own entries apart.
pub const SOFTWARE: &str = "Robin";
const SIZE_DIRS: [&str; 4] = ["normal", "large", "x-large", "xx-large"];

/// Whether `png` is a thumbnail Robin wrote: its `Software` key says so.
pub fn is_robins(png: &Path) -> bool {
    fs::File::open(png).is_ok_and(stamped_by_robin)
}

fn stamped_by_robin(file: fs::File) -> bool {
    let Ok(reader) = png::Decoder::new(std::io::BufReader::new(file)).read_info() else {
        return false;
    };
    reader
        .info()
        .uncompressed_latin1_text
        .iter()
        .any(|t| t.keyword == "Software" && t.text == SOFTWARE)
}

/// A cache root Clear thumbnail cache may work in: an absolute, existing
/// folder that is not the settings folder or inside it. The thumbnails it
/// clears are reached only by fd-relative, no-follow opens below it, so
/// nothing outside the cache dir can be touched.
pub fn clearable(cache: &Path, home: Option<&Path>) -> bool {
    let Ok(real) = fs::canonicalize(cache) else {
        return false;
    };
    let config = home
        .map(|h| h.join(".config"))
        .map(|c| fs::canonicalize(&c).unwrap_or(c));
    cache.is_absolute() && real.is_dir() && config.is_none_or(|c| !real.starts_with(c))
}

/// Delete Robin's own thumbnails under `cache/thumbnails/<size>/`: plain
/// `.png` files stamped by Robin, never symlinks, folders or other apps'
/// entries. The stamp is read from the very file that gets unlinked, and
/// the unlink re-checks its inode first. Returns how many files and bytes
/// went.
pub fn clear_robins(cache: &Path, home: Option<&Path>) -> (usize, u64) {
    use crate::safety::DirFd;
    let mut gone = (0, 0);
    if !clearable(cache, home) {
        return gone;
    }
    let Ok(root) = DirFd::open(cache) else {
        return gone;
    };
    let Ok(thumbs) = root.open_dir_nofollow("thumbnails".as_ref()) else {
        return gone;
    };
    for size in SIZE_DIRS {
        let Ok(dir) = thumbs.open_dir_nofollow(size.as_ref()) else {
            continue;
        };
        let Ok(entries) = dir.list() else {
            continue;
        };
        for e in entries {
            if e.is_dir || e.is_symlink || Path::new(&e.name).extension().is_none_or(|x| x != "png")
            {
                continue;
            }
            let Ok((file, id)) = dir.open_file_nofollow(&e.name) else {
                continue;
            };
            if stamped_by_robin(file) && dir.unlink_if_same(&e.name, id).unwrap_or(false) {
                gone.0 += 1;
                gone.1 += e.len;
            }
        }
    }
    // The index only lists Robin's files; it would now point at nothing.
    if let Ok(robin) = root.open_dir_nofollow("robin".as_ref()) {
        let index = std::ffi::OsStr::new("thumb-index.txt");
        if let Ok((_, id)) = robin.open_file_nofollow(index) {
            let _ = robin.unlink_if_same(index, id);
        }
    }
    gone
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ThumbSize {
    Normal,
    Large,
}

impl ThumbSize {
    pub fn for_dpr(dpr: f32) -> Self {
        if dpr >= 1.5 {
            Self::Large
        } else {
            Self::Normal
        }
    }

    pub fn pixels(self) -> u32 {
        match self {
            Self::Normal => NORMAL,
            Self::Large => LARGE,
        }
    }

    pub fn dir_name(self) -> &'static str {
        match self {
            Self::Normal => "normal",
            Self::Large => "large",
        }
    }
}

/// `file://` URI percent-encoded the way the thumbnail spec hashes it.
pub fn file_uri(path: &Path) -> String {
    let mut out = String::from("file://");
    for byte in path.to_string_lossy().as_bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'/' | b'-' | b'_' | b'.' | b'~' => {
                out.push(*byte as char);
            }
            _ => out.push_str(&format!("%{byte:02X}")),
        }
    }
    out
}

pub fn cache_name(uri: &str) -> String {
    let digest = Md5::digest(uri.as_bytes());
    format!("{digest:x}.png")
}

#[derive(Clone, Debug)]
pub struct ThumbRequest<'a> {
    pub source: &'a Path,
    pub cache_home: &'a Path,
    pub size: ThumbSize,
    pub mtime: SystemTime,
}

pub struct Thumbnail {
    pub path: PathBuf,
    pub width: u32,
    pub height: u32,
}

/// Write a PNG thumbnail for an image already decoded as RGBA.
pub fn store_rgba(
    req: &ThumbRequest<'_>,
    rgba: &[u8],
    width: u32,
    height: u32,
) -> Result<Thumbnail> {
    let (tw, th) = fit(width, height, req.size.pixels());
    let scaled = scale_nearest(rgba, width, height, tw, th);
    let uri = file_uri(req.source);
    let name = cache_name(&uri);
    let dir = req.cache_home.join("thumbnails").join(req.size.dir_name());
    fs::create_dir_all(&dir).map_err(|e| Error::io(&dir, e))?;
    let path = dir.join(&name);
    write_png(&path, &scaled, tw, th, &uri, &mtime_string(req.mtime))?;
    touch_index(req.cache_home, &path)?;
    Ok(Thumbnail {
        path,
        width: tw,
        height: th,
    })
}

pub fn store_image_file(req: &ThumbRequest<'_>) -> Result<Thumbnail> {
    let image = image::open(req.source)
        .map_err(|e| Error::Message(format!("{}: {e}", req.source.display())))?;
    let rgba = image.to_rgba8();
    store_rgba(req, rgba.as_raw(), rgba.width(), rgba.height())
}

pub fn video_argv(source: &Path, out: &Path, pixels: u32) -> Vec<String> {
    vec![
        "ffmpegthumbnailer".into(),
        "-i".into(),
        source.display().to_string(),
        "-o".into(),
        out.display().to_string(),
        "-s".into(),
        pixels.to_string(),
        "-q".into(),
        "8".into(),
    ]
}

pub fn pdf_argv(source: &Path, prefix: &Path, pixels: u32) -> Vec<String> {
    vec![
        "pdftoppm".into(),
        "-png".into(),
        "-f".into(),
        "1".into(),
        "-singlefile".into(),
        "-scale-to".into(),
        pixels.to_string(),
        source.display().to_string(),
        prefix.display().to_string(),
    ]
}

/// Run a fixed argv. The caller passes the program and args; this never
/// invokes a shell.
pub fn run_argv(argv: &[String]) -> Result<()> {
    let Some((program, args)) = argv.split_first() else {
        return Err(Error::Message("empty argv".into()));
    };
    let status = Command::new(program)
        .args(args)
        .status()
        .map_err(|e| Error::Message(format!("{program}: {e}")))?;
    if status.success() {
        Ok(())
    } else {
        Err(Error::Message(format!("{program} exited {status}")))
    }
}

pub fn evict_until(index_paths: &mut Vec<(PathBuf, u64)>, budget: u64) -> Vec<PathBuf> {
    let mut total: u64 = index_paths.iter().map(|(_, n)| n).sum();
    let mut removed = Vec::new();
    while total > budget {
        let Some((path, size)) = index_paths.first().cloned() else {
            break;
        };
        index_paths.remove(0);
        total = total.saturating_sub(size);
        removed.push(path);
    }
    removed
}

fn fit(width: u32, height: u32, max: u32) -> (u32, u32) {
    if width == 0 || height == 0 {
        return (1, 1);
    }
    let scale = (max as f32 / width.max(height) as f32).min(1.0);
    (
        ((width as f32) * scale).round().max(1.0) as u32,
        ((height as f32) * scale).round().max(1.0) as u32,
    )
}

fn scale_nearest(src: &[u8], sw: u32, sh: u32, dw: u32, dh: u32) -> Vec<u8> {
    let mut out = vec![0u8; (dw as usize) * (dh as usize) * 4];
    for y in 0..dh {
        for x in 0..dw {
            let sx = (x as u64 * sw as u64 / dw as u64) as u32;
            let sy = (y as u64 * sh as u64 / dh as u64) as u32;
            let si = ((sy as usize) * (sw as usize) + sx as usize) * 4;
            let di = ((y as usize) * (dw as usize) + x as usize) * 4;
            if si + 4 <= src.len() {
                out[di..di + 4].copy_from_slice(&src[si..si + 4]);
            }
        }
    }
    out
}

fn write_png(
    path: &Path,
    rgba: &[u8],
    width: u32,
    height: u32,
    uri: &str,
    mtime: &str,
) -> Result<()> {
    let file = fs::File::create(path).map_err(|e| Error::io(path, e))?;
    let mut encoder = png::Encoder::new(file, width, height);
    encoder.set_color(png::ColorType::Rgba);
    encoder.set_depth(png::BitDepth::Eight);
    let mut writer = encoder
        .write_header()
        .map_err(|e| Error::Message(e.to_string()))?;
    writer
        .write_text_chunk(&png::text_metadata::TEXtChunk::new("Thumb::URI", uri))
        .map_err(|e| Error::Message(e.to_string()))?;
    writer
        .write_text_chunk(&png::text_metadata::TEXtChunk::new("Thumb::MTime", mtime))
        .map_err(|e| Error::Message(e.to_string()))?;
    writer
        .write_text_chunk(&png::text_metadata::TEXtChunk::new("Software", SOFTWARE))
        .map_err(|e| Error::Message(e.to_string()))?;
    writer
        .write_image_data(rgba)
        .map_err(|e| Error::Message(e.to_string()))?;
    Ok(())
}

fn mtime_string(time: SystemTime) -> String {
    time.duration_since(SystemTime::UNIX_EPOCH)
        .map(|d| d.as_secs().to_string())
        .unwrap_or_else(|_| "0".into())
}

/// Writes the JPEG photos, video frame, and PDF page used by the grid renders.
pub fn write_fixture_stills(dir: &Path) -> Result<()> {
    fs::create_dir_all(dir).map_err(|e| Error::io(dir, e))?;
    save_jpeg(&dir.join("dune-sunset-0412.jpg"), 0)?;
    save_jpeg(&dir.join("dune-sunset-0413.jpg"), 40)?;
    save_jpeg(&dir.join("lars-homestead.jpg"), 80)?;
    save_png(&dir.join("speeder-frame.png"), 0)?;
    save_png(&dir.join("droid-manual.png"), 1)?;
    save_png(&dir.join("cantina-band.png"), 2)?;
    Ok(())
}

fn save_jpeg(path: &Path, shift: i32) -> Result<()> {
    let image = sky(320, 180, shift);
    image
        .save(path)
        .map_err(|e| Error::Message(format!("write {}: {e}", path.display())))
}

fn save_png(path: &Path, kind: u8) -> Result<()> {
    let image = match kind {
        0 => night(320, 180),
        1 => document(320, 180),
        _ => sky(320, 180, 110),
    };
    image
        .save(path)
        .map_err(|e| Error::Message(format!("write {}: {e}", path.display())))
}

fn night(width: u32, height: u32) -> image::RgbImage {
    image::RgbImage::from_fn(width, height, |x, y| {
        let t = y as f32 / height as f32;
        let dx = x as f32 / width as f32 - 0.5;
        let glow = (1.0 - (dx * dx + (t - 0.55).powi(2)) * 8.0).clamp(0.0, 1.0);
        let r = (8.0 + glow * 30.0) as u8;
        let g = (22.0 + glow * 90.0) as u8;
        let b = (36.0 + glow * 110.0) as u8;
        image::Rgb([r, g, b])
    })
}

fn sky(width: u32, height: u32, shift: i32) -> image::RgbImage {
    image::RgbImage::from_fn(width, height, |x, y| {
        let t = y as f32 / height as f32;
        let sun = ((x as f32 / width as f32) - 0.5).powi(2) + ((t - 0.62) * 1.4).powi(2);
        let glow = (1.0 - sun * 6.0).clamp(0.0, 1.0);
        let r = (40.0 + shift as f32 + t * 180.0 + glow * 160.0).clamp(0.0, 255.0) as u8;
        let g = (16.0 + t * 70.0 + glow * 90.0).clamp(0.0, 255.0) as u8;
        let b = (70.0 - t * 50.0 + glow * 20.0).clamp(0.0, 255.0) as u8;
        image::Rgb([r, g, b])
    })
}

fn document(width: u32, height: u32) -> image::RgbImage {
    image::RgbImage::from_fn(width, height, |x, y| {
        if y < 28 {
            image::Rgb([28, 36, 48])
        } else if y % 18 < 3 && x > 24 && x + 24 < width && y > 40 {
            image::Rgb([180, 176, 168])
        } else {
            image::Rgb([244, 240, 230])
        }
    })
}

fn touch_index(cache_home: &Path, png: &Path) -> Result<()> {
    let index = cache_home.join("robin").join("thumb-index.txt");
    if let Some(parent) = index.parent() {
        fs::create_dir_all(parent).map_err(|e| Error::io(parent, e))?;
    }
    let mut file = fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&index)
        .map_err(|e| Error::io(&index, e))?;
    writeln!(file, "{}", png.display()).map_err(|e| Error::io(&index, e))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dpr_picks_large_from_one_and_a_half() {
        assert_eq!(ThumbSize::for_dpr(1.0), ThumbSize::Normal);
        assert_eq!(ThumbSize::for_dpr(1.5), ThumbSize::Large);
        assert_eq!(ThumbSize::Large.pixels(), 256);
    }

    #[test]
    fn uri_and_name_are_stable() {
        let uri = file_uri(Path::new("/home/sky/Pictures/a b.png"));
        assert!(uri.starts_with("file:///home/sky/Pictures/a%20b.png"));
        let name = cache_name(&uri);
        assert_eq!(name.len(), 36);
        assert!(name.ends_with(".png"));
        assert_eq!(name, cache_name(&uri));
    }

    #[test]
    fn clearing_removes_only_robins_own_thumbnails() {
        let root = std::env::temp_dir().join(format!("robin-clear-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        let home = root.join("home");
        let cache = home.join(".cache");
        let src = root.join("a.png");
        let req = ThumbRequest {
            source: &src,
            cache_home: &cache,
            size: ThumbSize::Large,
            mtime: SystemTime::UNIX_EPOCH,
        };
        let ours = store_rgba(&req, &[9; 16], 2, 2).unwrap().path;
        assert!(is_robins(&ours));
        // Another app's thumbnail, a stray file, a symlink to ours, and a
        // settings file that must all survive.
        let normal = cache.join("thumbnails").join("normal");
        fs::create_dir_all(&normal).unwrap();
        let theirs = normal.join("theirs.png");
        image::RgbaImage::new(2, 2).save(&theirs).unwrap();
        let note = normal.join("note.txt");
        fs::write(&note, "x").unwrap();
        let link = normal.join("link.png");
        std::os::unix::fs::symlink(&ours, &link).unwrap();
        let config = home.join(".config").join("robin");
        fs::create_dir_all(&config).unwrap();
        fs::write(config.join("settings.toml"), "x").unwrap();

        let (n, bytes) = clear_robins(&cache, Some(&home));
        assert_eq!(n, 1);
        assert!(bytes > 0);
        assert!(!ours.exists());
        assert!(theirs.exists() && note.exists());
        assert!(fs::symlink_metadata(&link).is_ok(), "symlinks are left");
        assert!(config.join("settings.toml").exists());
        assert!(!cache.join("robin").join("thumb-index.txt").exists());

        // Refused roots: settings, a symlink into settings, relative or
        // missing paths.
        assert!(!clearable(&home.join(".config"), Some(&home)));
        std::os::unix::fs::symlink(home.join(".config"), home.join("sneaky")).unwrap();
        assert!(!clearable(&home.join("sneaky"), Some(&home)));
        assert!(!clearable(Path::new("cache"), None));
        assert!(!clearable(&home.join("missing"), None));
        assert!(clearable(&cache, Some(&home)));
        // A size folder that is a symlink elsewhere is not followed.
        let elsewhere = root.join("elsewhere");
        fs::create_dir_all(&elsewhere).unwrap();
        let req = ThumbRequest {
            source: &src,
            cache_home: &root.join("other-cache"),
            size: ThumbSize::Normal,
            mtime: SystemTime::UNIX_EPOCH,
        };
        let outside = store_rgba(&req, &[9; 16], 2, 2).unwrap().path;
        fs::copy(&outside, elsewhere.join("x.png")).unwrap();
        fs::remove_dir_all(cache.join("thumbnails").join("large")).unwrap();
        std::os::unix::fs::symlink(&elsewhere, cache.join("thumbnails").join("large")).unwrap();
        assert_eq!(clear_robins(&cache, Some(&home)).0, 0);
        assert!(
            elsewhere.join("x.png").exists(),
            "symlinked size dir left alone"
        );
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn stores_an_image_with_metadata() {
        let root = std::env::temp_dir().join(format!("robin-thumb-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(&root).unwrap();
        let src = root.join("pixel.png");
        let req = ThumbRequest {
            source: &src,
            cache_home: &root,
            size: ThumbSize::Normal,
            mtime: SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(10),
        };
        let thumb = store_rgba(
            &req,
            &[
                255, 0, 0, 255, 0, 255, 0, 255, 0, 0, 255, 255, 255, 255, 255, 255,
            ],
            2,
            2,
        )
        .unwrap();
        assert!(thumb.path.exists());
        assert!(thumb.width <= 128 && thumb.height <= 128);
        let bytes = fs::read(&thumb.path).unwrap();
        let text = String::from_utf8_lossy(&bytes);
        assert!(text.contains("Thumb::URI") || bytes.windows(9).any(|w| w == b"Thumb::URI"));
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn video_argv_is_not_a_shell_line() {
        let argv = video_argv(Path::new("/tmp/clip.mp4"), Path::new("/tmp/out.png"), 128);
        assert_eq!(argv[0], "ffmpegthumbnailer");
        assert!(!argv.iter().any(|a| a.contains('|')));
        assert_eq!(
            pdf_argv(Path::new("/tmp/a.pdf"), Path::new("/tmp/p"), 128)[0],
            "pdftoppm"
        );
    }

    #[test]
    fn lru_drops_the_oldest_first() {
        let mut index = vec![
            (PathBuf::from("old"), 100),
            (PathBuf::from("mid"), 100),
            (PathBuf::from("new"), 50),
        ];
        let gone = evict_until(&mut index, 120);
        assert_eq!(gone, vec![PathBuf::from("old"), PathBuf::from("mid")]);
        assert_eq!(index.len(), 1);
    }

    #[test]
    fn fixture_stills_are_real_images() {
        let dir = std::env::temp_dir().join(format!("robin-stills-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        write_fixture_stills(&dir).unwrap();
        let jpeg = fs::read(dir.join("dune-sunset-0412.jpg")).unwrap();
        let page = fs::read(dir.join("droid-manual.png")).unwrap();
        let frame = fs::read(dir.join("speeder-frame.png")).unwrap();
        assert!(jpeg.starts_with(&[0xff, 0xd8, 0xff]), "jpeg magic");
        assert!(page.starts_with(b"\x89PNG"));
        assert!(frame.starts_with(b"\x89PNG"));
        assert!(jpeg.len() > 2000, "jpeg {}", jpeg.len());
        assert_ne!(&page[64..128], &frame[64..128]);
        if std::env::var("ROBIN_WRITE_STILLS").is_ok() {
            write_fixture_stills(Path::new("assets/thumbs")).unwrap();
        }
        let _ = fs::remove_dir_all(&dir);
    }
}
