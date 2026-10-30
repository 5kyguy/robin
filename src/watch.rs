//! Live refresh: an inotify watch on the open folder.
//!
//! Events only mark the folder dirty; the UI thread polls [`Watch::pending`],
//! folds bursts through [`crate::model::Debounce`] and rereads the folder
//! once things go quiet.

use std::path::{Path, PathBuf};
use std::sync::mpsc::{channel, Receiver};

use notify::{EventKind, RecommendedWatcher, RecursiveMode, Watcher};

use crate::error::{Error, Result};

pub struct Watch {
    dir: PathBuf,
    rx: Receiver<()>,
    _watcher: RecommendedWatcher,
}

impl Watch {
    /// Watch `dir` itself, not its subfolders.
    pub fn new(dir: &Path) -> Result<Self> {
        let (tx, rx) = channel();
        let mut watcher = notify::recommended_watcher(move |res: notify::Result<notify::Event>| {
            if let Ok(event) = res {
                if changes_listing(&event.kind) {
                    let _ = tx.send(());
                }
            }
        })
        .map_err(|e| Error::Message(format!("watch {}: {e}", dir.display())))?;
        watcher
            .watch(dir, RecursiveMode::NonRecursive)
            .map_err(|e| Error::Message(format!("watch {}: {e}", dir.display())))?;
        Ok(Self {
            dir: dir.to_path_buf(),
            rx,
            _watcher: watcher,
        })
    }

    pub fn dir(&self) -> &Path {
        &self.dir
    }

    /// True when anything changed since the last call.
    pub fn pending(&self) -> bool {
        let mut any = false;
        while self.rx.try_recv().is_ok() {
            any = true;
        }
        any
    }
}

/// Reads and opens don't change what the list shows.
fn changes_listing(kind: &EventKind) -> bool {
    matches!(
        kind,
        EventKind::Create(_) | EventKind::Remove(_) | EventKind::Modify(_) | EventKind::Any
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{Duration, Instant};

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
}
