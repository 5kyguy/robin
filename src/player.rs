//! In-app playback as plain data: the transport's state, the commands
//! its buttons and keys send, and the saved resume positions. libmpv
//! itself lives in `mpv.rs`; nothing here needs it, so the controller
//! and its tests run without it.

use std::collections::VecDeque;
use std::path::{Path, PathBuf};

/// What a transport button or a viewer key asks the player to do.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Cmd {
    /// Space and the play button.
    Toggle,
    /// `j`/`k`: seconds back or on.
    SeekBy(f64),
    /// A click or drag on the scrub track, as a fraction of the duration.
    SeekTo(f64),
    /// `,`/`.`: one frame back or on (pauses, as mpv does).
    Frame(i8),
    /// The volume popover, 0..=100.
    Volume(f64),
    /// `m`.
    Mute,
    /// The speed chip: the next step of [`SPEEDS`].
    Speed,
    Loop,
    /// Cycle the subtitle track (off is one of the stops).
    Subs,
    /// The resume prompt's action: jump to the saved position.
    Resume(f64),
}

/// The speed chip's stops, in order; it wraps after the last.
pub const SPEEDS: [f64; 6] = [0.5, 0.75, 1.0, 1.25, 1.5, 2.0];

/// The next speed after `now`: the first stop above it, else the first.
pub fn next_speed(now: f64) -> f64 {
    SPEEDS
        .iter()
        .copied()
        .find(|s| *s > now + 0.001)
        .unwrap_or(SPEEDS[0])
}

/// What the transport shows, read back from the player each tick.
#[derive(Clone, Debug, PartialEq)]
pub struct Transport {
    /// Seconds played.
    pub pos: f64,
    /// Seconds in the file; 0 until it is known.
    pub dur: f64,
    pub paused: bool,
    /// 0..=100.
    pub volume: f64,
    pub muted: bool,
    pub speed: f64,
    pub looping: bool,
    /// A subtitle track is showing.
    pub subs: bool,
    /// The decoded picture's size; (0, 0) for audio without cover art,
    /// and until the first frame is decoded.
    pub video: (u32, u32),
    /// The file has a picture track (video, or audio with cover art).
    pub picture: bool,
    /// The file couldn't be played; the viewer says so and offers the
    /// default player.
    pub failed: bool,
}

impl Default for Transport {
    fn default() -> Self {
        Self {
            pos: 0.0,
            dur: 0.0,
            paused: false,
            volume: 100.0,
            muted: false,
            speed: 1.0,
            looping: false,
            subs: false,
            video: (0, 0),
            picture: false,
            failed: false,
        }
    }
}

impl Transport {
    /// Played fraction for the scrub track, 0..=1.
    pub fn progress(&self) -> f32 {
        if self.dur <= 0.0 {
            0.0
        } else {
            (self.pos / self.dur).clamp(0.0, 1.0) as f32
        }
    }

    /// "01:12 / 04:30".
    pub fn clock(&self) -> String {
        format!(
            "{} / {}",
            stamp(self.pos, self.dur),
            stamp(self.dur, self.dur)
        )
    }

    /// The speed chip, "1.0×" or "1.25×".
    pub fn speed_label(&self) -> String {
        speed_label(self.speed)
    }
}

/// "1.0×", "0.75×", "2.0×".
pub fn speed_label(speed: f64) -> String {
    let hundredths = (speed * 100.0).round() as i64;
    if hundredths % 10 == 0 {
        format!("{:.1}×", speed)
    } else {
        format!("{:.2}×", speed)
    }
}

/// `secs` as `mm:ss`, or `h:mm:ss` when `whole` (the duration) is an hour
/// or more, so both halves of the clock line up.
pub fn stamp(secs: f64, whole: f64) -> String {
    let t = if secs.is_finite() { secs.max(0.0) } else { 0.0 } as u64;
    let (h, m, s) = (t / 3600, t / 60 % 60, t % 60);
    if whole >= 3600.0 {
        format!("{h}:{m:02}:{s:02}")
    } else {
        format!("{:02}:{s:02}", t / 60)
    }
}

/// Where to offer to resume: a saved position at least 5s in and at
/// least 3s before the end (DESIGN §5). Anything else starts over.
pub fn resume_at(saved: f64, dur: f64) -> Option<f64> {
    (dur > 0.0 && saved >= 5.0 && saved <= dur - 3.0).then_some(saved)
}

/// The prompt's line, "Resume at 02:14?".
pub fn resume_line(at: f64, dur: f64) -> String {
    format!("Resume at {}?", stamp(at, dur))
}

/// Positions remembered past this many files drop the oldest.
pub const RESUME_KEEP: usize = 500;

/// Where each partly watched file was left, newest last. Stored as
/// `seconds<TAB>path` lines in Robin's cache, so it is never mpv's
/// watch-later and never in the user's config.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Positions {
    file: Option<PathBuf>,
    entries: VecDeque<(PathBuf, f64)>,
}

impl Positions {
    /// `$cache/robin/resume`, or nothing to remember with no cache dir.
    pub fn path(cache_dir: Option<PathBuf>) -> Option<PathBuf> {
        cache_dir.map(|d| d.join("robin").join("resume"))
    }

    pub fn load(file: Option<PathBuf>) -> Self {
        let text = file
            .as_ref()
            .and_then(|f| std::fs::read_to_string(f).ok())
            .unwrap_or_default();
        let entries = text
            .lines()
            .filter_map(|line| {
                let (secs, path) = line.split_once('\t')?;
                Some((PathBuf::from(path), secs.parse::<f64>().ok()?))
            })
            .filter(|(_, s)| s.is_finite())
            .collect();
        Self { file, entries }
    }

    pub fn get(&self, path: &Path) -> Option<f64> {
        self.entries
            .iter()
            .rev()
            .find(|(p, _)| p == path)
            .map(|e| e.1)
    }

    /// Remember where `path` was left: kept only when it would be offered
    /// again, otherwise forgotten (finished, or barely started).
    pub fn set(&mut self, path: &Path, pos: f64, dur: f64) {
        self.entries.retain(|(p, _)| p != path);
        let plain = path
            .to_str()
            .is_some_and(|s| !s.contains('\n') && !s.contains('\t'));
        if plain && resume_at(pos, dur).is_some() {
            self.entries.push_back((path.to_path_buf(), pos));
            while self.entries.len() > RESUME_KEEP {
                self.entries.pop_front();
            }
        }
        self.save();
    }

    fn save(&self) {
        let Some(file) = &self.file else { return };
        let mut text = String::new();
        for (p, s) in &self.entries {
            if let Some(p) = p.to_str() {
                text.push_str(&format!("{s:.1}\t{p}\n"));
            }
        }
        if let Some(dir) = file.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        // Written beside and renamed, so a crash never leaves half a file.
        let tmp = file.with_extension("tmp");
        if std::fs::write(&tmp, text).is_ok() {
            let _ = std::fs::rename(&tmp, file);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_clock_pads_minutes_and_grows_hours_only_for_long_files() {
        let t = Transport {
            pos: 72.4,
            dur: 270.0,
            ..Transport::default()
        };
        assert_eq!(t.clock(), "01:12 / 04:30");
        assert!((t.progress() - 72.4 / 270.0).abs() < 1e-6);
        assert_eq!(stamp(3725.0, 7200.0), "1:02:05");
        assert_eq!(stamp(f64::NAN, 10.0), "00:00");
        assert_eq!(Transport::default().progress(), 0.0, "unknown duration");
    }

    #[test]
    fn the_speed_chip_steps_through_its_stops_and_wraps() {
        assert_eq!(next_speed(1.0), 1.25);
        assert_eq!(next_speed(2.0), 0.5);
        assert_eq!(next_speed(1.1), 1.25, "an odd speed goes to the next stop");
        assert_eq!(speed_label(1.0), "1.0×");
        assert_eq!(speed_label(0.75), "0.75×");
        assert_eq!(speed_label(2.0), "2.0×");
    }

    #[test]
    fn resume_is_offered_between_five_seconds_in_and_three_before_the_end() {
        assert_eq!(resume_at(134.0, 270.0), Some(134.0));
        assert_eq!(resume_at(4.9, 270.0), None);
        assert_eq!(resume_at(267.5, 270.0), None);
        assert_eq!(resume_at(10.0, 0.0), None, "duration not known yet");
        assert_eq!(resume_line(134.0, 270.0), "Resume at 02:14?");
    }

    #[test]
    fn positions_survive_a_reload_and_forget_finished_files() {
        let dir = std::env::temp_dir().join(format!("robin-resume-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let file = Positions::path(Some(dir.clone()));
        let mut p = Positions::load(file.clone());
        let clip = Path::new("/v/clip.mp4");
        p.set(clip, 134.0, 270.0);
        p.set(Path::new("/v/odd\tname.mp4"), 50.0, 270.0);
        assert_eq!(Positions::load(file.clone()).get(clip), Some(134.0));
        assert_eq!(
            Positions::load(file.clone()).entries.len(),
            1,
            "tab names skipped"
        );
        p.set(clip, 269.0, 270.0);
        assert_eq!(Positions::load(file).get(clip), None, "finished: forgotten");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
