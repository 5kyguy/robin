//! The media viewer's state: which item of the folder is open, how it is
//! zoomed and turned, and which chrome shows. Pixels and Slint stay in
//! `app.rs`; everything here is plain data so it can be tested.

use std::ffi::OsString;
use std::path::Path;

/// Zoom steps are ×1.25, from fit up to 16× (DESIGN §5).
pub const ZOOM_STEP: f32 = 1.25;
pub const ZOOM_MAX: f32 = 16.0;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Media {
    Image,
    Video,
}

/// What the viewer can show, by extension. Stills the decoder can't read
/// (HEIC, RAW) still open and say so, rather than being skipped.
pub fn media(name: &Path) -> Option<Media> {
    let ext = name.extension()?.to_str()?.to_ascii_lowercase();
    match ext.as_str() {
        "jpg" | "jpeg" | "png" | "gif" | "webp" | "bmp" | "heic" | "heif" | "tif" | "tiff" => {
            Some(Media::Image)
        }
        "mp4" | "m4v" | "mkv" | "webm" | "mov" | "avi" => Some(Media::Video),
        _ => None,
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Zoom {
    Fit,
    /// Image pixels per screen pixel, 1.0 being 100%.
    Scale(f32),
}

#[derive(Clone, Debug, PartialEq)]
pub struct Viewer {
    /// The folder's media in the browser's view order.
    pub items: Vec<OsString>,
    pub index: usize,
    pub zoom: Zoom,
    /// Quarter turns clockwise, 0..4.
    pub turns: u8,
    pub info: bool,
    pub strip: bool,
}

/// What a key did in the viewer.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Step {
    /// Not a viewer key.
    Ignored,
    /// Same item, new zoom/turn/chrome.
    Changed,
    /// Another item is current: decode it.
    Moved,
    Close,
}

impl Viewer {
    /// Open on `start`, browsing the media among `names` (view order).
    /// None when `start` is not media.
    pub fn open(names: &[OsString], start: &OsString) -> Option<Self> {
        media(Path::new(start))?;
        let items: Vec<OsString> = names
            .iter()
            .filter(|n| media(Path::new(n)).is_some())
            .cloned()
            .collect();
        let index = items.iter().position(|n| n == start)?;
        Some(Self {
            items,
            index,
            zoom: Zoom::Fit,
            turns: 0,
            info: false,
            strip: true,
        })
    }

    /// Keep only the items `keep` accepts (after a reread drops deleted
    /// files), staying on the current item or the one that took its
    /// place. False when nothing is left.
    pub fn retain(&mut self, keep: impl Fn(&OsString) -> bool) -> bool {
        let before = self.items.len();
        let current = self.current().clone();
        let gone_before = self.items[..self.index].iter().filter(|n| !keep(n)).count();
        self.items.retain(|n| keep(n));
        if self.items.is_empty() {
            return false;
        }
        if self.items.len() != before {
            match self.items.iter().position(|n| *n == current) {
                Some(i) => self.index = i,
                None => {
                    self.index = (self.index - gone_before).min(self.items.len() - 1);
                    self.zoom = Zoom::Fit;
                    self.turns = 0;
                }
            }
        }
        true
    }

    pub fn current(&self) -> &OsString {
        &self.items[self.index]
    }

    /// "6 / 248".
    pub fn counter(&self) -> String {
        format!("{} / {}", self.index + 1, self.items.len())
    }

    /// Jump to `index`, resetting zoom and turn as a new item does.
    pub fn go(&mut self, index: usize) -> Step {
        let index = index.min(self.items.len() - 1);
        if index == self.index {
            return Step::Changed;
        }
        self.index = index;
        self.zoom = Zoom::Fit;
        self.turns = 0;
        Step::Moved
    }

    /// The scale that fits an `iw`×`ih` image (after turning) into a
    /// `vw`×`vh` viewport. Never above 1: small images show at 100%.
    pub fn fit_scale(&self, (iw, ih): (f32, f32), (vw, vh): (f32, f32)) -> f32 {
        let (iw, ih) = if self.turns % 2 == 1 {
            (ih, iw)
        } else {
            (iw, ih)
        };
        if iw <= 0.0 || ih <= 0.0 || vw <= 0.0 || vh <= 0.0 {
            return 1.0;
        }
        (vw / iw).min(vh / ih).min(1.0)
    }

    /// The scale to draw at now.
    pub fn scale(&self, image: (f32, f32), view: (f32, f32)) -> f32 {
        match self.zoom {
            Zoom::Fit => self.fit_scale(image, view),
            Zoom::Scale(s) => s,
        }
    }

    /// One ×1.25 step in (`dir` > 0) or out, between fit and 16×. Zooming
    /// out to or below the fit scale lands back on Fit.
    pub fn step_zoom(&mut self, dir: i32, image: (f32, f32), view: (f32, f32)) {
        let fit = self.fit_scale(image, view);
        let now = self.scale(image, view);
        let next = if dir > 0 {
            (now * ZOOM_STEP).min(ZOOM_MAX)
        } else {
            now / ZOOM_STEP
        };
        self.zoom = if next <= fit + f32::EPSILON {
            Zoom::Fit
        } else {
            Zoom::Scale(next)
        };
    }

    /// A viewer key. `image` and `view` are the current image's and the
    /// viewport's sizes, for zoom steps.
    pub fn key(&mut self, text: &str, image: (f32, f32), view: (f32, f32)) -> Step {
        use crate::app::keys;
        let last = self.items.len() - 1;
        match text {
            keys::LEFT | "h" => self.go(self.index.saturating_sub(1)),
            keys::RIGHT | "l" | " " => self.go((self.index + 1).min(last)),
            keys::HOME => self.go(0),
            keys::END => self.go(last),
            "+" | "=" => {
                self.step_zoom(1, image, view);
                Step::Changed
            }
            "-" => {
                self.step_zoom(-1, image, view);
                Step::Changed
            }
            "0" => {
                self.zoom = Zoom::Fit;
                Step::Changed
            }
            "1" => {
                self.zoom = Zoom::Scale(1.0);
                Step::Changed
            }
            "r" => {
                self.turns = (self.turns + 1) % 4;
                self.zoom = Zoom::Fit;
                Step::Changed
            }
            "R" => {
                self.turns = (self.turns + 3) % 4;
                self.zoom = Zoom::Fit;
                Step::Changed
            }
            "i" => {
                self.info = !self.info;
                Step::Changed
            }
            "f" => {
                self.strip = !self.strip;
                Step::Changed
            }
            keys::ESCAPE | "q" => Step::Close,
            _ => Step::Ignored,
        }
    }

    /// Up to `n` filmstrip indices centred on the current item, kept
    /// inside the list (ViewerFilmstrip's preferred highlight).
    pub fn strip_window(&self, n: usize) -> std::ops::Range<usize> {
        let len = self.items.len();
        let n = n.min(len);
        let start = self.index.saturating_sub(n / 2).min(len - n);
        start..start + n
    }
}

/// "200%": the zoom HUD's label.
pub fn percent(scale: f32) -> String {
    format!("{}%", (scale * 100.0).round() as u32)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn names(v: &[&str]) -> Vec<OsString> {
        v.iter().map(OsString::from).collect()
    }

    fn open(start: &str) -> Viewer {
        let all = names(&["raw", "a.jpg", "notes.txt", "b.PNG", "c.mp4", "d.heic"]);
        Viewer::open(&all, &OsString::from(start)).unwrap()
    }

    #[test]
    fn only_media_is_browsed_in_view_order() {
        let v = open("b.PNG");
        assert_eq!(v.items, names(&["a.jpg", "b.PNG", "c.mp4", "d.heic"]));
        assert_eq!(v.counter(), "2 / 4");
        assert_eq!(
            Viewer::open(&names(&["notes.txt"]), &"notes.txt".into()),
            None
        );
        assert_eq!(media(Path::new("x.MKV")), Some(Media::Video));
        assert_eq!(media(Path::new("README")), None);
    }

    #[test]
    fn arrows_and_vim_keys_walk_and_stop_at_the_ends() {
        let mut v = open("a.jpg");
        let k = |v: &mut Viewer, t: &str| v.key(t, (10.0, 10.0), (100.0, 100.0));
        assert_eq!(
            k(&mut v, crate::app::keys::LEFT),
            Step::Changed,
            "already first"
        );
        assert_eq!(k(&mut v, "l"), Step::Moved);
        assert_eq!(v.current(), "b.PNG");
        assert_eq!(k(&mut v, crate::app::keys::END), Step::Moved);
        assert_eq!(
            k(&mut v, crate::app::keys::RIGHT),
            Step::Changed,
            "already last"
        );
        assert_eq!(k(&mut v, "h"), Step::Moved);
        assert_eq!(v.current(), "c.mp4");
        assert_eq!(k(&mut v, crate::app::keys::HOME), Step::Moved);
        assert_eq!(k(&mut v, "q"), Step::Close);
        assert_eq!(k(&mut v, "z"), Step::Ignored);
    }

    #[test]
    fn zoom_steps_by_a_quarter_between_fit_and_sixteen_times() {
        let mut v = open("a.jpg");
        let (img, view) = ((4000.0, 2000.0), (1000.0, 1000.0));
        assert_eq!(v.scale(img, view), 0.25, "fit");
        v.key("+", img, view);
        assert_eq!(v.zoom, Zoom::Scale(0.3125));
        v.key("-", img, view);
        assert_eq!(v.zoom, Zoom::Fit, "back down to fit snaps to Fit");
        v.key("-", img, view);
        assert_eq!(v.zoom, Zoom::Fit, "never below fit");
        v.key("1", img, view);
        assert_eq!(v.scale(img, view), 1.0);
        for _ in 0..40 {
            v.key("+", img, view);
        }
        assert_eq!(v.zoom, Zoom::Scale(ZOOM_MAX));
        v.key("0", img, view);
        assert_eq!(v.zoom, Zoom::Fit);
        assert_eq!(percent(2.0), "200%");
    }

    #[test]
    fn small_images_fit_at_100_percent_and_turns_swap_the_sides() {
        let mut v = open("a.jpg");
        assert_eq!(v.fit_scale((320.0, 180.0), (1000.0, 800.0)), 1.0);
        assert_eq!(v.fit_scale((2000.0, 1000.0), (1000.0, 1000.0)), 0.5);
        v.key("r", (0.0, 0.0), (0.0, 0.0));
        assert_eq!(v.turns, 1);
        assert_eq!(v.fit_scale((2000.0, 1000.0), (1000.0, 1000.0)), 0.5);
        assert_eq!(v.fit_scale((2000.0, 1000.0), (2000.0, 500.0)), 0.25);
        v.key("R", (0.0, 0.0), (0.0, 0.0));
        v.key("R", (0.0, 0.0), (0.0, 0.0));
        assert_eq!(v.turns, 3);
    }

    #[test]
    fn a_new_item_resets_zoom_and_turn_and_chrome_toggles_stick() {
        let mut v = open("a.jpg");
        v.key("1", (1.0, 1.0), (1.0, 1.0));
        v.key("r", (1.0, 1.0), (1.0, 1.0));
        v.key("1", (1.0, 1.0), (1.0, 1.0));
        v.key("i", (1.0, 1.0), (1.0, 1.0));
        v.key("f", (1.0, 1.0), (1.0, 1.0));
        v.key("l", (1.0, 1.0), (1.0, 1.0));
        assert_eq!((v.zoom, v.turns), (Zoom::Fit, 0));
        assert!(v.info && !v.strip);
    }

    #[test]
    fn the_filmstrip_centres_on_the_current_item_inside_the_list() {
        let all: Vec<OsString> = (0..20).map(|i| format!("{i:02}.jpg").into()).collect();
        let mut v = Viewer::open(&all, &all[0]).unwrap();
        assert_eq!(v.strip_window(11), 0..11);
        v.go(10);
        assert_eq!(v.strip_window(11), 5..16);
        v.go(19);
        assert_eq!(v.strip_window(11), 9..20);
        let few = open("a.jpg");
        assert_eq!(few.strip_window(11), 0..4);
    }
}
