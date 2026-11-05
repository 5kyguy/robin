//! Decoding pictures for the viewer, off the UI thread.
//!
//! Every open goes through [`open_regular`]: `O_NONBLOCK` so a FIFO or a
//! device can't block the open, and a fstat that refuses anything but a
//! regular file. The file is opened once per decode and read through one
//! buffered reader. [`Decodes`] tickets requests so a held arrow key
//! can't pile up work or show a stale picture.

use std::fs::File;
use std::io::{BufReader, Read, Seek, SeekFrom};
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};

use image::{ImageDecoder, RgbaImage};

/// Larger images are refused rather than decoded: 16k × 16k RGBA is 1 GiB.
pub const MAX_SIDE: u32 = 16_384;
pub const MAX_PIXELS: u64 = MAX_SIDE as u64 * MAX_SIDE as u64;
/// Stills with no shrinking decoder are only decoded for a tile up to
/// this size; larger ones show the tile's placeholder instead.
pub const TILE_SOURCE_PIXELS: u64 = 4_096 * 4_096;
/// Decodes running at once; later requests wait, newest first.
pub const MAX_IN_FLIGHT: usize = 2;

/// Why there is no picture. Shown as one muted line.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Why {
    TooLarge(u32, u32),
    Unreadable,
    Video,
}

impl Why {
    pub fn line(&self) -> String {
        match self {
            Why::TooLarge(w, h) => format!("Too large to preview ({w}×{h})"),
            Why::Unreadable => "Can't read this file".into(),
            Why::Video => "No built-in video player yet".into(),
        }
    }
}

/// Open `path` for reading without ever blocking on it, and only if it is
/// a regular file (no FIFOs, sockets or devices).
pub fn open_regular(path: &Path) -> Result<File, Why> {
    let file = std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NONBLOCK | libc::O_NOCTTY)
        .open(path)
        .map_err(|_| Why::Unreadable)?;
    let meta = file.metadata().map_err(|_| Why::Unreadable)?;
    if !meta.file_type().is_file() {
        return Err(Why::Unreadable);
    }
    Ok(file)
}

fn limits() -> image::Limits {
    let mut l = image::Limits::default();
    l.max_image_width = Some(MAX_SIDE);
    l.max_image_height = Some(MAX_SIDE);
    l.max_alloc = Some(MAX_PIXELS * 4);
    l
}

/// Decode the whole picture at `path`, within [`limits`].
pub fn full(path: &Path) -> Result<RgbaImage, Why> {
    if crate::viewer::media(path) == Some(crate::viewer::Media::Video) {
        return Err(Why::Video);
    }
    let file = open_regular(path)?;
    decode_reader(BufReader::new(file), None)
}

/// A tile-sized picture (longest side `side`), never decoded at full
/// size: a cached freedesktop thumbnail if there is one, JPEGs through a
/// scaling decoder, other stills only when small.
pub fn tile(path: &Path, side: u32, cache: Option<&Path>) -> Result<RgbaImage, Why> {
    if let Some(png) = cache.and_then(|c| cached_thumbnail(path, c)) {
        let file = open_regular(&png)?;
        return decode_reader(BufReader::new(file), Some(side));
    }
    if crate::viewer::media(path) == Some(crate::viewer::Media::Video) {
        return Err(Why::Video);
    }
    let mut file = BufReader::new(open_regular(path)?);
    let mut magic = [0u8; 3];
    let is_jpeg = file.read_exact(&mut magic).is_ok() && magic == [0xff, 0xd8, 0xff];
    file.seek(SeekFrom::Start(0)).map_err(|_| Why::Unreadable)?;
    if is_jpeg {
        return scaled_jpeg(file, side);
    }
    decode_reader(file, Some(side))
}

/// The freedesktop thumbnail for `path`, if one is already on disk
/// (written by Robin's grid or another file manager), and only while its
/// Thumb::MTime stamp still matches the file.
pub fn cached_thumbnail(path: &Path, cache: &Path) -> Option<PathBuf> {
    let name = crate::thumbs::cache_name(&crate::thumbs::file_uri(path));
    let mtime = std::fs::metadata(path)
        .ok()?
        .modified()
        .ok()?
        .duration_since(std::time::UNIX_EPOCH)
        .ok()?
        .as_secs();
    ["normal", "large", "x-large", "xx-large"]
        .iter()
        .map(|size| cache.join("thumbnails").join(size).join(&name))
        .find(|p| p.is_file() && thumb_mtime(p) == Some(mtime))
}

/// The source mtime a freedesktop thumbnail was made from (`Thumb::MTime`).
/// A thumbnail without it, or with another time, is stale and ignored.
fn thumb_mtime(png_path: &Path) -> Option<u64> {
    let file = std::fs::File::open(png_path).ok()?;
    let reader = png::Decoder::new(std::io::BufReader::new(file))
        .read_info()
        .ok()?;
    reader
        .info()
        .uncompressed_latin1_text
        .iter()
        .find(|t| t.keyword == "Thumb::MTime")
        .and_then(|t| t.text.trim().parse().ok())
}

fn decode_reader<R: std::io::BufRead + Seek>(r: R, tile: Option<u32>) -> Result<RgbaImage, Why> {
    let mut reader = image::ImageReader::new(r)
        .with_guessed_format()
        .map_err(|_| Why::Unreadable)?;
    // Limits wait until the size is known, so the refusal can say it.
    reader.no_limits();
    let mut decoder = reader.into_decoder().map_err(|e| match e {
        image::ImageError::Limits(_) => Why::TooLarge(0, 0),
        _ => Why::Unreadable,
    })?;
    let (w, h) = decoder.dimensions();
    let pixels = u64::from(w) * u64::from(h);
    if w > MAX_SIDE || h > MAX_SIDE || pixels > MAX_PIXELS {
        return Err(Why::TooLarge(w, h));
    }
    if tile.is_some() && pixels > TILE_SOURCE_PIXELS {
        return Err(Why::TooLarge(w, h));
    }
    decoder
        .set_limits(limits())
        .map_err(|_| Why::TooLarge(w, h))?;
    let img = image::DynamicImage::from_decoder(decoder).map_err(|e| match e {
        image::ImageError::Limits(_) => Why::TooLarge(w, h),
        _ => Why::Unreadable,
    })?;
    let img = match tile {
        Some(s) if w > s || h > s => img.thumbnail(s, s),
        _ => img,
    };
    Ok(img.into_rgba8())
}

/// A JPEG decoded straight to about `side` pixels by DCT scaling (down to
/// 1/8 size), then shrunk the rest of the way.
fn scaled_jpeg<R: std::io::Read>(r: R, side: u32) -> Result<RgbaImage, Why> {
    use jpeg_decoder::PixelFormat;
    let mut d = jpeg_decoder::Decoder::new(r);
    d.read_info().map_err(|_| Why::Unreadable)?;
    let info = d.info().ok_or(Why::Unreadable)?;
    let (w, h) = (u32::from(info.width), u32::from(info.height));
    if u64::from(w) * u64::from(h) > MAX_PIXELS {
        return Err(Why::TooLarge(w, h));
    }
    let (sw, sh) = d
        .scale(side as u16, side as u16)
        .map_err(|_| Why::Unreadable)?;
    let px = d.decode().map_err(|_| Why::Unreadable)?;
    let info = d.info().ok_or(Why::Unreadable)?;
    let (sw, sh) = (u32::from(sw), u32::from(sh));
    let rgba: Vec<u8> = match info.pixel_format {
        PixelFormat::RGB24 => px
            .as_chunks::<3>()
            .0
            .iter()
            .flat_map(|p| [p[0], p[1], p[2], 255])
            .collect(),
        PixelFormat::L8 => px.iter().flat_map(|&g| [g, g, g, 255]).collect(),
        // Print-ready photos: undo the ink, as the full decoder does.
        PixelFormat::CMYK32 => px
            .as_chunks::<4>()
            .0
            .iter()
            .flat_map(|p| {
                let k = 255 - u16::from(p[3]);
                let ink = |c: u8| ((255 - u16::from(c)) * k / 255) as u8;
                [ink(p[0]), ink(p[1]), ink(p[2]), 255]
            })
            .collect(),
        _ => return Err(Why::Unreadable),
    };
    let img = RgbaImage::from_raw(sw, sh, rgba).ok_or(Why::Unreadable)?;
    Ok(if sw > side || sh > side {
        image::DynamicImage::ImageRgba8(img)
            .thumbnail(side, side)
            .into_rgba8()
    } else {
        img
    })
}

/// Tickets for viewer decodes. Each request gets a new ticket; only the
/// newest ticket's result is shown. At most [`MAX_IN_FLIGHT`] run at
/// once, and while they do only the newest waiting request is kept.
#[derive(Debug, Default)]
pub struct Decodes<T> {
    latest: u64,
    running: usize,
    waiting: Option<(u64, T)>,
}

impl<T> Decodes<T> {
    /// Ask for `job`. Returns it with its ticket if it may start now;
    /// otherwise it waits, replacing any older waiting job.
    pub fn request(&mut self, job: T) -> Option<(u64, T)> {
        self.latest += 1;
        if self.running < MAX_IN_FLIGHT {
            self.running += 1;
            Some((self.latest, job))
        } else {
            self.waiting = Some((self.latest, job));
            None
        }
    }

    /// The job with `ticket` finished. Returns whether its result is
    /// still wanted, and the waiting job to start now, if any.
    pub fn finish(&mut self, ticket: u64) -> (bool, Option<(u64, T)>) {
        self.running = self.running.saturating_sub(1);
        let next = self.waiting.take();
        if next.is_some() {
            self.running += 1;
        }
        (ticket == self.latest, next)
    }

    /// Forget every request: whatever is running lands stale.
    pub fn cancel(&mut self) {
        self.latest += 1;
        self.waiting = None;
    }

    pub fn running(&self) -> usize {
        self.running
    }

    pub fn is_latest(&self, ticket: u64) -> bool {
        ticket == self.latest
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp(label: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("robin-pic-{label}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn holding_right_runs_two_decodes_and_shows_only_the_last() {
        let mut d = Decodes::default();
        let a = d.request("a").unwrap();
        let b = d.request("b").unwrap();
        for name in ["c", "d", "e", "f"] {
            assert!(d.request(name).is_none(), "{name} waits");
        }
        assert_eq!(d.running(), 2, "bounded");
        let (wanted, next) = d.finish(a.0);
        assert!(!wanted, "a is stale");
        let f = next.unwrap();
        assert_eq!(f.1, "f", "only the newest waiting job survives");
        let (wanted, next) = d.finish(b.0);
        assert!(!wanted && next.is_none());
        assert_eq!(d.finish(f.0), (true, None));
        assert_eq!(d.running(), 0);
        let g = d.request("g").unwrap();
        d.cancel();
        assert!(!d.finish(g.0).0, "cancelled");
    }

    #[test]
    fn oversized_pictures_say_too_large_with_their_size() {
        let dir = tmp("big");
        // A PNG header claiming 20000×10 pixels; the body never matters.
        let mut png = Vec::new();
        {
            let mut enc = png::Encoder::new(&mut png, 20_000, 10);
            enc.set_color(png::ColorType::Grayscale);
            let mut w = enc.write_header().unwrap();
            w.write_image_data(&vec![0u8; 200_000]).unwrap();
        }
        std::fs::write(dir.join("wide.png"), &png).unwrap();
        assert_eq!(
            full(&dir.join("wide.png")).unwrap_err(),
            Why::TooLarge(20_000, 10)
        );
        assert_eq!(
            Why::TooLarge(20_000, 10).line(),
            "Too large to preview (20000×10)"
        );
        std::fs::write(dir.join("junk.png"), b"\x89PNG\r\n\x1a\nnope").unwrap();
        assert_eq!(full(&dir.join("junk.png")).unwrap_err(), Why::Unreadable);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn fifos_and_folders_are_refused_without_blocking() {
        let dir = tmp("fifo");
        let fifo = dir.join("pipe.jpg");
        rustix::fs::mkfifoat(
            rustix::fs::CWD,
            &fifo,
            rustix::fs::Mode::from_raw_mode(0o600),
        )
        .unwrap();
        assert_eq!(full(&fifo).unwrap_err(), Why::Unreadable, "never blocks");
        std::fs::create_dir(dir.join("d.png")).unwrap();
        assert_eq!(full(&dir.join("d.png")).unwrap_err(), Why::Unreadable);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn cmyk_jpeg_tiles_match_the_full_picture() {
        let dir = tmp("cmyk");
        let path = dir.join("print.jpg");
        std::fs::write(&path, include_bytes!("testdata/cmyk.jpg")).unwrap();
        let full = full(&path).unwrap();
        let tile = tile(&path, 64, None).unwrap();
        assert_eq!(tile.dimensions(), (64, 48));
        // Left half blue, right half red, in both.
        for (x, fx) in [(8, 40), (56, 280)] {
            let a = tile.get_pixel(x, 24).0;
            let b = full.get_pixel(fx, 120).0;
            for i in 0..3 {
                assert!(a[i].abs_diff(b[i]) < 24, "{a:?} vs {b:?}");
            }
        }
        assert!(tile.get_pixel(56, 24).0[0] > 150, "red stays red");
    }

    #[test]
    fn tiles_shrink_jpegs_while_decoding_and_prefer_the_cache() {
        let dir = tmp("tile");
        crate::thumbs::write_fixture_stills(&dir).unwrap();
        let t = tile(&dir.join("dune-sunset-0412.jpg"), 72, None).unwrap();
        assert!(t.width() <= 72 && t.height() <= 72, "{:?}", t.dimensions());
        let clip = dir.join("clip.mp4");
        std::fs::write(&clip, b"v").unwrap();
        assert_eq!(tile(&clip, 72, Some(&dir)).unwrap_err(), Why::Video);
        // A frame stamped with another mtime is stale: still the video case.
        let frame = image::open(dir.join("speeder-frame.png"))
            .unwrap()
            .to_rgba8();
        let stamp = |mtime| {
            crate::thumbs::store_rgba(
                &crate::thumbs::ThumbRequest {
                    source: &clip,
                    cache_home: &dir,
                    size: crate::thumbs::ThumbSize::Normal,
                    mtime,
                },
                frame.as_raw(),
                frame.width(),
                frame.height(),
            )
            .unwrap()
        };
        let real = std::fs::metadata(&clip).unwrap().modified().unwrap();
        stamp(real - std::time::Duration::from_secs(60));
        assert_eq!(
            tile(&clip, 72, Some(&dir)).unwrap_err(),
            Why::Video,
            "stale"
        );
        stamp(real);
        assert_eq!(tile(&clip, 72, Some(&dir)).unwrap().width(), 72);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
