//! Live refresh: an inotify watch on the open folder.
//!
//! Events only set a dirty flag; the UI thread polls [`Watch::pending`],
//! folds bursts through [`crate::model::Debounce`] and has the folder
//! reread off the UI thread. When the folder itself is deleted, renamed
//! or replaced, inotify drops the watch (IN_DELETE_SELF / IN_MOVE_SELF,
//! then IN_IGNORED); [`Watch::stale`] reports that so the caller can
//! re-arm on whatever now sits at the path.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use notify::{EventKind, RecommendedWatcher, RecursiveMode, Watcher};

use crate::error::{Error, Result};

pub struct Watch {
    dir: PathBuf,
    dirty: Arc<AtomicBool>,
    stale: Arc<AtomicBool>,
    _watcher: RecommendedWatcher,
}

impl Watch {
    /// Watch `dir` itself, not its subfolders.
    pub fn new(dir: &Path) -> Result<Self> {
        let dirty = Arc::new(AtomicBool::new(false));
        let stale = Arc::new(AtomicBool::new(false));
        let (d, s, me) = (dirty.clone(), stale.clone(), dir.to_path_buf());
        let mut watcher = notify::recommended_watcher(move |res: notify::Result<notify::Event>| {
            let Ok(event) = res else {
                // Queue overflow or a broken watch: reread, and re-arm.
                d.store(true, Ordering::Relaxed);
                s.store(true, Ordering::Relaxed);
                return;
            };
            if is_self_gone(&event, &me) {
                s.store(true, Ordering::Relaxed);
            }
            if changes_listing(&event.kind) {
                d.store(true, Ordering::Relaxed);
            }
        })
        .map_err(|e| Error::Message(format!("watch {}: {e}", dir.display())))?;
        watcher
            .watch(dir, RecursiveMode::NonRecursive)
            .map_err(|e| Error::Message(format!("watch {}: {e}", dir.display())))?;
        Ok(Self {
            dir: dir.to_path_buf(),
            dirty,
            stale,
            _watcher: watcher,
        })
    }

    pub fn dir(&self) -> &Path {
        &self.dir
    }

    /// True when anything changed since the last call.
    pub fn pending(&self) -> bool {
        self.dirty.swap(false, Ordering::Relaxed)
    }

    /// The watched folder itself was deleted, renamed or replaced, so this
    /// watch no longer sees the path; build a new one.
    pub fn stale(&self) -> bool {
        self.stale.load(Ordering::Relaxed)
    }
}

/// The event is about the watched folder itself going away: notify's
/// inotify backend reports IN_DELETE_SELF as a Remove and IN_MOVE_SELF as a
/// rename, both with the watched path.
fn is_self_gone(event: &notify::Event, dir: &Path) -> bool {
    use notify::event::ModifyKind;
    matches!(
        event.kind,
        EventKind::Remove(_) | EventKind::Modify(ModifyKind::Name(_))
    ) && event.paths.iter().any(|p| p == dir)
}

/// Reads and opens don't change what the list shows.
fn changes_listing(kind: &EventKind) -> bool {
    matches!(
        kind,
        EventKind::Create(_) | EventKind::Remove(_) | EventKind::Modify(_) | EventKind::Any
    )
}

/// What to do about a folder whose watch can't be set up (inotify limits,
/// a filesystem without inotify, permissions): retry with backoff instead
/// of every tick, poll slowly meanwhile, and warn once per folder.
#[derive(Debug)]
pub struct Fallback {
    dir: PathBuf,
    backoff: Duration,
    next_retry: Instant,
    next_poll: Instant,
    warned: bool,
}

/// How often a folder without a watch is reread.
pub const SLOW_POLL: Duration = Duration::from_secs(2);
const FIRST_BACKOFF: Duration = Duration::from_secs(2);
const MAX_BACKOFF: Duration = Duration::from_secs(60);

impl Fallback {
    pub fn new(dir: &Path, now: Instant) -> Self {
        Self {
            dir: dir.to_path_buf(),
            backoff: FIRST_BACKOFF,
            next_retry: now + FIRST_BACKOFF,
            next_poll: now + SLOW_POLL,
            warned: false,
        }
    }

    pub fn dir(&self) -> &Path {
        &self.dir
    }

    /// Another failed attempt: double the wait, up to a minute.
    pub fn failed_again(&mut self, now: Instant) {
        self.backoff = (self.backoff * 2).min(MAX_BACKOFF);
        self.next_retry = now + self.backoff;
    }

    pub fn may_retry(&self, now: Instant) -> bool {
        now >= self.next_retry
    }

    /// True every [`SLOW_POLL`]: time to reread and compare.
    pub fn poll_due(&mut self, now: Instant) -> bool {
        if now < self.next_poll {
            return false;
        }
        self.next_poll = now + SLOW_POLL;
        true
    }

    /// True the first time only.
    pub fn take_warning(&mut self) -> bool {
        !std::mem::replace(&mut self.warned, true)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_failed_watch_backs_off_polls_slowly_and_warns_once() {
        let t0 = Instant::now();
        let s = Duration::from_secs;
        let mut f = Fallback::new(Path::new("/x"), t0);
        assert!(f.take_warning());
        assert!(!f.take_warning(), "warned once per folder");
        let ticks: Vec<_> = (1..=19)
            .map(|i| t0 + Duration::from_millis(100 * i))
            .collect();
        assert!(
            ticks.iter().all(|&t| !f.may_retry(t)),
            "no retry every tick"
        );
        assert!(f.may_retry(t0 + s(2)));
        f.failed_again(t0 + s(2));
        assert!(!f.may_retry(t0 + s(5)));
        assert!(f.may_retry(t0 + s(6)));
        for i in 0..10 {
            f.failed_again(t0 + s(i));
        }
        assert!(f.may_retry(t0 + s(9 + 60)), "capped at a minute");
        let polls = (1..=60)
            .filter(|i| f.poll_due(t0 + Duration::from_millis(100 * i)))
            .count();
        assert_eq!(polls, 3, "a reread every 2s over 6s");
    }

    fn wait(w: &Watch) -> bool {
        let end = Instant::now() + Duration::from_secs(5);
        while Instant::now() < end {
            if w.pending() {
                return true;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        false
    }

    #[test]
    fn sees_creates_renames_and_deletes_but_not_subfolder_contents() {
        let root = std::env::temp_dir().join(format!("robin-watch-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join("sub")).unwrap();
        let w = Watch::new(&root).unwrap();
        assert!(!w.pending());
        std::fs::write(root.join("new.txt"), b"x").unwrap();
        assert!(wait(&w), "create");
        std::fs::rename(root.join("new.txt"), root.join("moved.txt")).unwrap();
        assert!(wait(&w), "rename");
        std::fs::remove_file(root.join("moved.txt")).unwrap();
        assert!(wait(&w), "delete");
        std::thread::sleep(Duration::from_millis(100));
        let _ = w.pending();
        std::fs::write(root.join("sub/deep.txt"), b"x").unwrap();
        std::thread::sleep(Duration::from_millis(300));
        assert!(!w.pending(), "subfolder contents are not watched");
        std::fs::remove_dir_all(&root).unwrap();
    }

    fn wait_stale(w: &Watch) -> bool {
        let end = Instant::now() + Duration::from_secs(5);
        while Instant::now() < end {
            if w.stale() {
                return true;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        false
    }

    #[test]
    fn goes_stale_when_the_folder_is_replaced_or_renamed() {
        let root = std::env::temp_dir().join(format!("robin-watch-stale-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let dir = root.join("open");
        std::fs::create_dir_all(&dir).unwrap();
        let w = Watch::new(&dir).unwrap();
        assert!(!w.stale());
        std::fs::remove_dir(&dir).unwrap();
        std::fs::create_dir(&dir).unwrap();
        assert!(wait_stale(&w), "delete and recreate");
        let w = Watch::new(&dir).unwrap();
        std::fs::write(dir.join("a"), b"x").unwrap();
        assert!(wait(&w), "the new watch sees the new folder");
        std::fs::create_dir(root.join("other")).unwrap();
        std::fs::rename(&dir, root.join("gone")).unwrap();
        std::fs::rename(root.join("other"), &dir).unwrap();
        assert!(wait_stale(&w), "renamed over");
        std::fs::remove_dir_all(&root).unwrap();
    }
}
