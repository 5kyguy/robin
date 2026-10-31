//! Directory model. The window paints whatever [`Listing::view`] returns.
//!
//! Notify events are folded by [`Debounce`] before they touch the listing,
//! so a burst of writes becomes one incremental update.

use std::collections::BTreeSet;
use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant, SystemTime};

use crate::safety::{DirFd, Dirent};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SortKey {
    Name,
    Date,
    Size,
    Type,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SortDir {
    Asc,
    Desc,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Nav {
    Up,
    Down,
    Left,
    Right,
    Home,
    End,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Entry {
    pub name: OsString,
    pub is_dir: bool,
    pub is_symlink: bool,
    pub len: u64,
    pub mtime: SystemTime,
    pub mode: u32,
}

impl Entry {
    pub fn from_dirent(dirent: Dirent) -> Self {
        let mtime = SystemTime::UNIX_EPOCH + Duration::from_secs(dirent.mtime_secs.max(0) as u64);
        Self {
            name: dirent.name,
            is_dir: dirent.is_dir,
            is_symlink: dirent.is_symlink,
            len: dirent.len,
            mtime,
            mode: dirent.mode,
        }
    }

    pub fn is_hidden(&self) -> bool {
        self.name.to_string_lossy().starts_with('.')
    }

    pub fn extension(&self) -> String {
        Path::new(&self.name)
            .extension()
            .map(|ext| ext.to_string_lossy().to_lowercase())
            .unwrap_or_default()
    }
}

#[derive(Clone, Debug)]
pub struct Listing {
    entries: Vec<Entry>,
    sort_key: SortKey,
    sort_dir: SortDir,
    folders_first: bool,
    show_hidden: bool,
    query: String,
    selection: BTreeSet<OsString>,
    cursor: Option<OsString>,
    /// Where a Shift range starts: the last item picked without Shift.
    anchor: Option<OsString>,
    /// `view()` as indices into `entries`, filtered and sorted once and
    /// reused until something it depends on changes: a search redraws
    /// per keystroke and a 100k-entry sort is not free.
    order: std::sync::OnceLock<Vec<usize>>,
    /// Each entry's name as a search compares it (NFC, lowercased),
    /// aligned with `entries`. Kept across queries, so a keystroke in a
    /// 100k-entry folder doesn't re-normalise every name; dropped only
    /// when the entries change.
    folded: std::sync::OnceLock<Vec<Vec<char>>>,
}

impl Default for Listing {
    fn default() -> Self {
        Self {
            entries: Vec::new(),
            sort_key: SortKey::Name,
            sort_dir: SortDir::Asc,
            folders_first: true,
            show_hidden: false,
            query: String::new(),
            selection: BTreeSet::new(),
            cursor: None,
            anchor: None,
            order: std::sync::OnceLock::new(),
            folded: std::sync::OnceLock::new(),
        }
    }
}

impl Listing {
    pub fn from_dir(dir: &DirFd) -> crate::Result<Self> {
        let mut listing = Self::default();
        listing.replace_dir(dir)?;
        Ok(listing)
    }

    pub fn replace_dir(&mut self, dir: &DirFd) -> crate::Result<()> {
        let entries = dir.list()?.into_iter().map(Entry::from_dirent).collect();
        self.set_entries(entries);
        Ok(())
    }

    pub fn set_entries(&mut self, entries: Vec<Entry>) {
        let names: BTreeSet<_> = entries.iter().map(|e| e.name.clone()).collect();
        self.selection.retain(|name| names.contains(name));
        if self
            .cursor
            .as_ref()
            .is_some_and(|name| !names.contains(name))
        {
            self.cursor = None;
        }
        self.entries = entries;
        self.refold();
    }

    /// Forget the cached view order; the next `view()` rebuilds it.
    fn stale(&mut self) {
        self.order = std::sync::OnceLock::new();
    }

    /// The entries changed: forget their folded names and the order.
    fn refold(&mut self) {
        self.folded = std::sync::OnceLock::new();
        self.stale();
    }

    /// Whether the folded names are cached; tests check this.
    #[cfg(test)]
    pub fn folded_cached(&self) -> bool {
        self.folded.get().is_some()
    }

    /// Whether the view order is built and cached; tests check this.
    #[cfg(test)]
    pub fn order_cached(&self) -> bool {
        self.order.get().is_some()
    }

    pub fn set_sort(&mut self, key: SortKey, dir: SortDir) {
        self.sort_key = key;
        self.sort_dir = dir;
        self.stale();
    }

    pub fn sort(&self) -> (SortKey, SortDir) {
        (self.sort_key, self.sort_dir)
    }

    /// A header click: the sorted column flips direction, another column
    /// starts ascending, as Thunar does.
    pub fn toggle_sort(&mut self, key: SortKey) {
        self.sort_dir = if self.sort_key == key && self.sort_dir == SortDir::Asc {
            SortDir::Desc
        } else {
            SortDir::Asc
        };
        self.sort_key = key;
        self.stale();
    }

    pub fn set_show_hidden(&mut self, show: bool) {
        self.show_hidden = show;
        self.stale();
    }

    pub fn show_hidden(&self) -> bool {
        self.show_hidden
    }

    /// Filter by `query`. Selected names it hides stop being selected,
    /// so nothing acts on files the user can't see.
    pub fn set_query(&mut self, query: &str) {
        self.query = query.to_string();
        self.stale();
        self.drop_unseen();
    }

    /// The selection the view still shows, in view order.
    pub fn visible_selection(&self) -> Vec<OsString> {
        self.view()
            .iter()
            .filter(|e| self.selection.contains(&e.name))
            .map(|e| e.name.clone())
            .collect()
    }

    pub fn query(&self) -> &str {
        &self.query
    }

    /// Names in the order the window should paint them.
    pub fn view(&self) -> Vec<&Entry> {
        let order = self.order.get_or_init(|| {
            let query = fold(&self.query);
            // Folded only once something is searched for.
            let folded = (!query.is_empty()).then(|| {
                self.folded.get_or_init(|| {
                    self.entries
                        .iter()
                        .map(|e| fold(&e.name.to_string_lossy()))
                        .collect()
                })
            });
            let mut rows: Vec<usize> = (0..self.entries.len())
                .filter(|&i| self.show_hidden || !self.entries[i].is_hidden())
                .filter(|&i| folded.is_none_or(|f| folded_match(&f[i], &query)))
                .collect();
            rows.sort_by(|&a, &b| {
                cmp_entries(
                    &self.entries[a],
                    &self.entries[b],
                    self.sort_key,
                    self.sort_dir,
                    self.folders_first,
                )
            });
            rows
        });
        order.iter().map(|&i| &self.entries[i]).collect()
    }

    /// Drop selected names the view no longer shows.
    pub fn drop_unseen(&mut self) {
        let seen: BTreeSet<OsString> = self.view().iter().map(|e| e.name.clone()).collect();
        self.selection.retain(|n| seen.contains(n));
        if self.cursor.as_ref().is_some_and(|c| !seen.contains(c)) {
            self.cursor = None;
        }
    }

    /// How many entries a search looks through: hidden ones only when
    /// they are shown.
    pub fn searchable_count(&self) -> usize {
        self.entries
            .iter()
            .filter(|entry| self.show_hidden || !entry.is_hidden())
            .count()
    }

    pub fn hidden_count(&self) -> usize {
        self.entries
            .iter()
            .filter(|entry| entry.is_hidden())
            .count()
    }

    /// Everything read from the folder, hidden or filtered or not.
    pub fn entries(&self) -> &[Entry] {
        &self.entries
    }

    pub fn selection(&self) -> &BTreeSet<OsString> {
        &self.selection
    }

    pub fn cursor(&self) -> Option<&OsString> {
        self.cursor.as_ref()
    }

    pub fn select_only(&mut self, name: OsString) {
        self.selection.clear();
        self.selection.insert(name.clone());
        self.anchor = Some(name.clone());
        self.cursor = Some(name);
    }

    /// Ctrl+click: add or remove one name; it becomes the anchor.
    pub fn toggle(&mut self, name: OsString) {
        if !self.selection.remove(&name) {
            self.selection.insert(name.clone());
        }
        self.anchor = Some(name.clone());
        self.cursor = Some(name);
    }

    /// Shift+click: everything from the anchor to `name` in view order.
    /// With no anchor it picks just `name`.
    pub fn select_range_to(&mut self, name: OsString) {
        let names: Vec<OsString> = self.view().iter().map(|e| e.name.clone()).collect();
        let to = names.iter().position(|n| *n == name);
        let from = self
            .anchor
            .as_ref()
            .and_then(|a| names.iter().position(|n| n == a));
        match (from, to) {
            (Some(a), Some(b)) => {
                let (lo, hi) = (a.min(b), a.max(b));
                self.selection = names[lo..=hi].iter().cloned().collect();
                self.cursor = Some(name);
            }
            _ => self.select_only(name),
        }
    }

    pub fn clear_selection(&mut self) {
        self.selection.clear();
        self.cursor = None;
        self.anchor = None;
    }

    /// Move the keyboard cursor. `columns` is 1 in the list and the tile
    /// count of a grid row. `extend` keeps the previous selection (Shift).
    pub fn move_cursor(&mut self, nav: Nav, extend: bool, columns: usize) {
        let names: Vec<OsString> = self.view().iter().map(|e| e.name.clone()).collect();
        if names.is_empty() {
            self.clear_selection();
            return;
        }
        let columns = columns.max(1);
        let current = self
            .cursor
            .as_ref()
            .and_then(|name| names.iter().position(|n| n == name))
            .unwrap_or(0);
        let next = match nav {
            Nav::Home => 0,
            Nav::End => names.len() - 1,
            Nav::Left => current.saturating_sub(1),
            Nav::Right => (current + 1).min(names.len() - 1),
            Nav::Up => current.saturating_sub(columns),
            Nav::Down => (current + columns).min(names.len() - 1),
        };
        let name = names[next].clone();
        let anchor = self
            .anchor
            .as_ref()
            .and_then(|a| names.iter().position(|n| n == a));
        match anchor.filter(|_| extend) {
            // Shift: everything between the anchor and the new cursor.
            Some(from) => {
                let (lo, hi) = (from.min(next), from.max(next));
                self.selection = names[lo..=hi].iter().cloned().collect();
            }
            None => {
                if !extend {
                    self.selection.clear();
                }
                self.selection.insert(name.clone());
                self.anchor = Some(name.clone());
            }
        }
        self.cursor = Some(name);
    }

    /// One virtual grid row, `columns` tiles wide. The last row may be short.
    pub fn grid_rows(&self, columns: usize) -> Vec<Vec<&Entry>> {
        let columns = columns.max(1);
        self.view()
            .chunks(columns)
            .map(|chunk| chunk.to_vec())
            .collect()
    }

    pub fn upsert(&mut self, entry: Entry) {
        if let Some(slot) = self.entries.iter_mut().find(|e| e.name == entry.name) {
            *slot = entry;
        } else {
            self.entries.push(entry);
        }
        self.refold();
    }

    pub fn remove_name(&mut self, name: &OsString) {
        self.entries.retain(|entry| &entry.name != name);
        self.refold();
        self.selection.remove(name);
        if self.cursor.as_ref() == Some(name) {
            self.cursor = None;
        }
    }
}

fn cmp_entries(
    a: &Entry,
    b: &Entry,
    key: SortKey,
    dir: SortDir,
    folders_first: bool,
) -> std::cmp::Ordering {
    if folders_first && a.is_dir != b.is_dir {
        return if a.is_dir {
            std::cmp::Ordering::Less
        } else {
            std::cmp::Ordering::Greater
        };
    }
    let ordering = match key {
        SortKey::Name => cmp_name(a, b),
        SortKey::Date => a.mtime.cmp(&b.mtime).then_with(|| cmp_name(a, b)),
        SortKey::Size => a.len.cmp(&b.len).then_with(|| cmp_name(a, b)),
        SortKey::Type => a
            .extension()
            .cmp(&b.extension())
            .then_with(|| cmp_name(a, b)),
    };
    if dir == SortDir::Desc {
        ordering.reverse()
    } else {
        ordering
    }
}

fn cmp_name(a: &Entry, b: &Entry) -> std::cmp::Ordering {
    natural_cmp(&a.name.to_string_lossy(), &b.name.to_string_lossy())
        .then_with(|| a.name.cmp(&b.name))
}

/// Case-insensitive, with digit runs compared by value, so "img2"
/// sorts before "img10".
pub fn natural_cmp(a: &str, b: &str) -> std::cmp::Ordering {
    use std::cmp::Ordering;
    let (a, b) = (a.to_lowercase(), b.to_lowercase());
    let (mut x, mut y) = (a.chars().peekable(), b.chars().peekable());
    loop {
        match (x.peek().copied(), y.peek().copied()) {
            (None, None) => return Ordering::Equal,
            (None, Some(_)) => return Ordering::Less,
            (Some(_), None) => return Ordering::Greater,
            (Some(c), Some(d)) if c.is_ascii_digit() && d.is_ascii_digit() => {
                let take = |it: &mut std::iter::Peekable<std::str::Chars>| {
                    let mut run = String::new();
                    while let Some(c) = it.peek().copied().filter(char::is_ascii_digit) {
                        run.push(c);
                        it.next();
                    }
                    run
                };
                let (m, n) = (take(&mut x), take(&mut y));
                let (mt, nt) = (m.trim_start_matches('0'), n.trim_start_matches('0'));
                let by_value = mt.len().cmp(&nt.len()).then_with(|| mt.cmp(nt));
                // Equal values: fewer leading zeros first, so the order is total.
                let ord = by_value.then_with(|| m.len().cmp(&n.len()));
                if ord != Ordering::Equal {
                    return ord;
                }
            }
            (Some(c), Some(d)) => {
                if c != d {
                    return c.cmp(&d);
                }
                x.next();
                y.next();
            }
        }
    }
}

/// The Modified column, like Thunar: the time for today, the date for
/// anything older or in the future (even later today). Missing or
/// pre-1970 times (`mtime <= 0`) are blank. `offset` gives the local UTC
/// offset in seconds at a Unix time.
pub fn format_modified(mtime: i64, now: i64, offset: impl Fn(i64) -> i64) -> String {
    if mtime <= 0 {
        return String::new();
    }
    let local = mtime + offset(mtime);
    let day = local.div_euclid(86_400);
    if mtime <= now && day == (now + offset(now)).div_euclid(86_400) {
        let secs = local.rem_euclid(86_400);
        return format!("{:02}:{:02}", secs / 3600, secs % 3600 / 60);
    }
    let (y, m, d) = civil_from_days(day);
    format!("{y:04}-{m:02}-{d:02}")
}

/// Days since 1970-01-01 to (year, month, day), proleptic Gregorian.
fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    let y = yoe + era * 400 + i64::from(m <= 2);
    (y, m, d)
}

/// The local UTC offset at a Unix time, from the C library's zone data.
pub fn local_offset(t: i64) -> i64 {
    let t = t as libc::time_t;
    let mut tm: libc::tm = unsafe { std::mem::zeroed() };
    // SAFETY: localtime_r only writes the tm we pass.
    if unsafe { libc::localtime_r(&t, &mut tm) }.is_null() {
        return 0;
    }
    tm.tm_gmtoff
}

/// Text as a search compares it: NFC, so a precomposed "é" and
/// "e" + combining acute are the same, then lowercased.
pub fn fold(text: &str) -> Vec<char> {
    use unicode_normalization::UnicodeNormalization;
    text.nfc().flat_map(char::to_lowercase).collect()
}

/// Subsequence match, case-insensitive. An empty query matches everything.
pub fn fuzzy_match(name: &str, query: &str) -> bool {
    folded_match(&fold(name), &fold(query))
}

fn folded_match(name: &[char], query: &[char]) -> bool {
    let mut rest = name.iter();
    query.iter().all(|q| rest.any(|c| c == q))
}

/// `name` as runs of (text, matched) for a search for `query`: the
/// first contiguous hit if there is one, else only the letters the
/// subsequence match used ("dj" lights the d and the j of "dune.jpg",
/// not what lies between). Runs split on grapheme boundaries, so an
/// accent always takes its letter's colour. The text is the NFC name.
pub fn match_runs(name: &str, query: &str) -> Option<Vec<(String, bool)>> {
    use unicode_normalization::UnicodeNormalization;
    use unicode_segmentation::UnicodeSegmentation;
    let shown: String = name.nfc().collect();
    let chars: Vec<char> = shown.chars().collect();
    let query = fold(query);
    // Fold each char in full ("İ" lowercases to "i" + a combining dot)
    // and remember which shown char every folded one came from.
    let mut low: Vec<char> = Vec::new();
    let mut from: Vec<usize> = Vec::new();
    for (i, c) in chars.iter().enumerate() {
        for l in c.to_lowercase() {
            low.push(l);
            from.push(i);
        }
    }
    if query.is_empty() || query.len() > low.len() {
        return None;
    }
    let mut hit = vec![false; chars.len()];
    match low.windows(query.len()).position(|w| w == query.as_slice()) {
        Some(i) => from[i..i + query.len()].iter().for_each(|&c| hit[c] = true),
        None => {
            let mut at = 0;
            for q in &query {
                let i = at + low[at..].iter().position(|c| c == q)?;
                hit[from[i]] = true;
                at = i + 1;
            }
        }
    }
    let mut runs: Vec<(String, bool)> = Vec::new();
    let mut at = 0;
    for g in shown.graphemes(true) {
        let n = g.chars().count();
        let on = hit[at..at + n].iter().any(|h| *h);
        at += n;
        match runs.last_mut() {
            Some((text, was)) if *was == on => text.push_str(g),
            _ => runs.push((g.to_string(), on)),
        }
    }
    Some(runs)
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Crumb {
    pub label: String,
    pub path: PathBuf,
}

/// `~/Pictures` becomes `~`, `Pictures` when `path` is under `home`.
pub fn breadcrumbs(path: &Path, home: Option<&Path>) -> Vec<Crumb> {
    breadcrumbs_on(path, home, &[])
}

/// Breadcrumbs rooted where a person would start: the device a path is
/// on (`SANDISK`, `Photos`), else `~` under home, else `/`. A device
/// wins over home when its mount is deeper, so a stick mounted inside
/// home still reads as the stick. `devices` are (mount point, label).
pub fn breadcrumbs_on(
    path: &Path,
    home: Option<&Path>,
    devices: &[(PathBuf, String)],
) -> Vec<Crumb> {
    let mut root: (PathBuf, String) = (PathBuf::from("/"), "/".into());
    if let Some(home) = home.filter(|h| path.starts_with(h)) {
        root = (home.to_path_buf(), "~".into());
    }
    for (mount, label) in devices {
        let deeper = mount.components().count() > root.0.components().count();
        if deeper && path.starts_with(mount) {
            root = (mount.clone(), label.clone());
        }
    }
    let rest = path.strip_prefix(&root.0).unwrap_or(path);
    let mut acc = root.0.clone();
    let mut crumbs = vec![Crumb {
        label: root.1,
        path: root.0,
    }];
    for part in rest.components() {
        acc.push(part);
        crumbs.push(Crumb {
            label: part.as_os_str().to_string_lossy().into_owned(),
            path: acc.clone(),
        });
    }
    crumbs
}

/// Coalesce a burst of directory events into one apply.
#[derive(Debug)]
pub struct Debounce {
    delay: Duration,
    /// Fire anyway this long after the first push of a burst, so a folder
    /// that never goes quiet (a download, a build) still refreshes.
    max_wait: Option<Duration>,
    dirty: bool,
    first: Option<Instant>,
    last: Option<Instant>,
}

impl Debounce {
    pub fn new(delay: Duration) -> Self {
        Self {
            delay,
            max_wait: None,
            dirty: false,
            first: None,
            last: None,
        }
    }

    pub fn max_wait(mut self, max: Duration) -> Self {
        self.max_wait = Some(max);
        self
    }

    pub fn push(&mut self, now: Instant) {
        self.dirty = true;
        self.first.get_or_insert(now);
        self.last = Some(now);
    }

    /// True once `delay` has passed since the latest push.
    pub fn ready(&mut self, now: Instant) -> bool {
        let Some(last) = self.last else {
            return false;
        };
        let quiet = now.saturating_duration_since(last) >= self.delay;
        let overdue = match (self.first, self.max_wait) {
            (Some(first), Some(max)) => now.saturating_duration_since(first) >= max,
            _ => false,
        };
        if self.dirty && (quiet || overdue) {
            self.dirty = false;
            self.first = None;
            self.last = None;
            true
        } else {
            false
        }
    }
}

#[cfg(test)]
mod tests {

    #[test]
    fn match_runs_light_only_the_matched_letters() {
        let r = |v: &[(&str, bool)]| Some(v.iter().map(|(t, h)| (t.to_string(), *h)).collect());
        assert_eq!(
            match_runs("dune-sunset-0412.jpg", "SUN"),
            r(&[("dune-", false), ("sun", true), ("set-0412.jpg", false)])
        );
        // Not contiguous: only d and j, never "une.".
        assert_eq!(
            match_runs("dune.jpg", "dj"),
            r(&[("d", true), ("une.", false), ("j", true), ("pg", false)])
        );
        assert_eq!(match_runs("dune.jpg", "zz"), None);
        assert_eq!(match_runs("dune.jpg", ""), None);
        assert_eq!(
            match_runs("Ärger.txt", "är"),
            r(&[("Är", true), ("ger.txt", false)])
        );
    }

    #[test]
    fn an_accent_takes_its_letters_colour() {
        // Flag emoji and a combining mark that NFC can't fold away (q̃).
        let runs = match_runs("q\u{303}x", "q").unwrap();
        assert_eq!(runs[0], ("q\u{303}".to_string(), true));
        assert_eq!(runs[1], ("x".to_string(), false));
    }

    #[test]
    fn search_matches_ignore_case_and_unicode_form() {
        // Precomposed é (U+00E9) against e + combining acute (U+0301).
        assert!(fuzzy_match("Café.txt", "cafe\u{301}"));
        assert!(fuzzy_match("cafe\u{301}.txt", "CAFÉ"));
        assert!(fuzzy_match("ÄRGER", "ärger"));
        assert!(fuzzy_match("写真.jpg", "写"));
        assert!(!fuzzy_match("dune", "dunes"));
        assert!(fuzzy_match("anything", ""));
        let runs = match_runs("cafe\u{301}", "É").unwrap();
        assert_eq!(runs.last().unwrap(), &("é".to_string(), true));
    }

    use super::*;
    use std::fs;

    fn entry(name: &str, is_dir: bool, len: u64, secs: u64) -> Entry {
        Entry {
            name: OsString::from(name),
            is_dir,
            is_symlink: false,
            len,
            mtime: SystemTime::UNIX_EPOCH + Duration::from_secs(secs),
            mode: if is_dir { 0o040755 } else { 0o100644 },
        }
    }

    #[test]
    fn the_view_is_cached_until_something_it_depends_on_changes() {
        let mut listing = Listing::default();
        listing.set_entries(vec![entry("b", false, 1, 1), entry("a", false, 1, 1)]);
        assert!(!listing.order_cached());
        assert_eq!(names(&listing), ["a", "b"]);
        assert!(listing.order_cached());
        listing.set_query("b");
        assert_eq!(names(&listing), ["b"], "a query rebuilds the order");
        listing.set_sort(SortKey::Name, SortDir::Desc);
        assert!(!listing.order_cached(), "a sort drops the cache");
        listing.set_show_hidden(true);
        assert!(!listing.order_cached());
        assert_eq!(names(&listing), ["b"]);
    }

    #[test]
    fn folded_names_outlive_queries_and_go_with_the_entries() {
        let mut listing = Listing::default();
        listing.set_entries(vec![entry("Été.jpg", false, 1, 1), entry("b", false, 1, 1)]);
        assert_eq!(names(&listing).len(), 2);
        assert!(!listing.folded_cached(), "no search, no folding");
        listing.set_query("ét");
        assert_eq!(names(&listing), ["Été.jpg"]);
        assert!(listing.folded_cached());
        listing.set_query("b");
        listing.set_sort(SortKey::Name, SortDir::Desc);
        assert!(
            listing.folded_cached(),
            "queries and sorts keep the folded names"
        );
        assert_eq!(names(&listing), ["b"]);
        listing.upsert(entry("bb", false, 1, 1));
        assert!(!listing.folded_cached(), "new entries refold");
        assert_eq!(names(&listing), ["bb", "b"]);
        listing.remove_name(&OsString::from("bb"));
        assert!(!listing.folded_cached());
        assert_eq!(names(&listing), ["b"]);
    }

    #[test]
    fn letters_that_lowercase_to_two_still_light_up() {
        let on = |runs: Vec<(String, bool)>| -> Vec<String> {
            runs.into_iter().filter(|r| r.1).map(|r| r.0).collect()
        };
        // "İ" folds to "i" + U+0307; a plain "i" still lights the İ.
        assert_eq!(on(match_runs("İstanbul.jpg", "ist").unwrap()), ["İst"]);
        assert_eq!(on(match_runs("İstanbul.jpg", "İs").unwrap()), ["İs"]);
        assert_eq!(
            on(match_runs("xİy", "y").unwrap()),
            ["y"],
            "later letters stay aligned"
        );
        assert_eq!(on(match_runs("aİb", "ab").unwrap()), ["a", "b"]);
        assert!(fuzzy_match("İstanbul", "istan"));
    }

    fn names(listing: &Listing) -> Vec<String> {
        listing
            .view()
            .iter()
            .map(|e| e.name.to_string_lossy().into_owned())
            .collect()
    }

    #[test]
    fn sort_hides_dotfiles_and_puts_folders_first() {
        let mut listing = Listing::default();
        listing.set_entries(vec![
            entry("b.txt", false, 10, 2),
            entry("a-dir", true, 1, 1),
            entry(".hidden", false, 3, 3),
            entry("c.txt", false, 2, 4),
        ]);
        assert_eq!(names(&listing), ["a-dir", "b.txt", "c.txt"]);
        assert_eq!(listing.hidden_count(), 1);
        listing.set_show_hidden(true);
        assert_eq!(names(&listing), ["a-dir", ".hidden", "b.txt", "c.txt"]);
        listing.set_sort(SortKey::Size, SortDir::Desc);
        assert_eq!(names(&listing)[0], "a-dir");
        let files: Vec<_> = names(&listing).into_iter().skip(1).collect();
        assert_eq!(files, ["b.txt", ".hidden", "c.txt"]);
    }

    #[test]
    fn names_sort_naturally_and_headers_toggle() {
        let mut listing = Listing::default();
        listing.set_entries(vec![
            entry("img10.png", false, 1, 3),
            entry("IMG2.png", false, 5, 1),
            entry("img1.png", false, 9, 2),
            entry("zdir", true, 0, 0),
            entry("Adir", true, 0, 9),
        ]);
        assert_eq!(
            names(&listing),
            ["Adir", "zdir", "img1.png", "IMG2.png", "img10.png"]
        );
        // Clicking Name again reverses it; folders stay first.
        listing.toggle_sort(SortKey::Name);
        assert_eq!(listing.sort(), (SortKey::Name, SortDir::Desc));
        assert_eq!(
            names(&listing),
            ["zdir", "Adir", "img10.png", "IMG2.png", "img1.png"]
        );
        // Another column starts ascending.
        listing.toggle_sort(SortKey::Size);
        assert_eq!(listing.sort(), (SortKey::Size, SortDir::Asc));
        assert_eq!(names(&listing)[2..], ["img10.png", "IMG2.png", "img1.png"]);
        listing.toggle_sort(SortKey::Date);
        assert_eq!(
            names(&listing),
            ["zdir", "Adir", "IMG2.png", "img1.png", "img10.png"]
        );
        listing.toggle_sort(SortKey::Date);
        assert_eq!(names(&listing)[2..], ["img10.png", "img1.png", "IMG2.png"]);
        assert_eq!(natural_cmp("a01", "a1"), std::cmp::Ordering::Greater);
        assert_eq!(natural_cmp("a9", "a10"), std::cmp::Ordering::Less);
    }

    #[test]
    fn modified_shows_the_time_today_and_the_date_before() {
        // 2026-10-31 02:30 UTC; a +05:30 zone makes it 08:00 local.
        let now = 1_793_413_800;
        let ist = |_| 19_800;
        assert_eq!(format_modified(now, now, ist), "08:00");
        // 00:10 local today is still today.
        assert_eq!(format_modified(now - 7 * 3600 - 50 * 60, now, ist), "00:10");
        // 23:50 local yesterday shows its date.
        assert_eq!(
            format_modified(now - 8 * 3600 - 10 * 60, now, ist),
            "2026-10-30"
        );
        // The same instant in UTC is still the 31st.
        assert_eq!(format_modified(now - 2 * 3600, now, |_| 0), "00:30");
        assert_eq!(format_modified(1, now, |_| 0), "1970-01-01");
        assert_eq!(format_modified(951_782_400, now, |_| 0), "2000-02-29");
    }

    #[test]
    fn type_sort_groups_extensions() {
        let mut listing = Listing {
            folders_first: false,
            ..Listing::default()
        };
        listing.set_entries(vec![
            entry("b.rs", false, 1, 1),
            entry("a.txt", false, 1, 1),
            entry("c.rs", false, 1, 1),
        ]);
        listing.set_sort(SortKey::Type, SortDir::Asc);
        assert_eq!(names(&listing), ["b.rs", "c.rs", "a.txt"]);
    }

    #[test]
    fn fuzzy_query_and_keyboard() {
        assert!(fuzzy_match("dune-sunset-0412.jpg", "dsj"));
        assert!(!fuzzy_match("notes.txt", "xyz"));
        let mut listing = Listing::default();
        listing.set_entries(vec![
            entry("alpha", false, 1, 1),
            entry("beta", false, 1, 1),
            entry("gamma", false, 1, 1),
        ]);
        listing.set_query("lp");
        assert_eq!(names(&listing), ["alpha"]);
        listing.set_query("");
        listing.move_cursor(Nav::Home, false, 1);
        listing.move_cursor(Nav::Down, true, 1);
        assert_eq!(listing.selection().len(), 2);
        listing.move_cursor(Nav::End, false, 3);
        assert_eq!(listing.cursor().unwrap(), "gamma");
        let rows = listing.grid_rows(2);
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[1].len(), 1);
    }

    #[test]
    fn breadcrumbs_collapse_home() {
        let home = Path::new("/home/sky");
        let crumbs = breadcrumbs(Path::new("/home/sky/Pictures/2026"), Some(home));
        let labels: Vec<_> = crumbs.iter().map(|c| c.label.as_str()).collect();
        assert_eq!(labels, ["~", "Pictures", "2026"]);
        assert_eq!(crumbs[1].path, PathBuf::from("/home/sky/Pictures"));
    }

    #[test]
    fn breadcrumbs_root_at_the_device_or_slash() {
        let home = Some(Path::new("/home/sky"));
        let devices = [
            (
                PathBuf::from("/run/media/sky/SANDISK"),
                "SANDISK".to_string(),
            ),
            (PathBuf::from("/home/sky/media/STICK"), "STICK".to_string()),
        ];
        let labels = |p: &str| -> Vec<String> {
            breadcrumbs_on(Path::new(p), home, &devices)
                .into_iter()
                .map(|c| c.label)
                .collect()
        };
        assert_eq!(labels("/run/media/sky/SANDISK/DCIM"), ["SANDISK", "DCIM"]);
        assert_eq!(labels("/run/media/sky/SANDISK"), ["SANDISK"]);
        assert_eq!(
            labels("/home/sky/media/STICK/a"),
            ["STICK", "a"],
            "deeper than home"
        );
        assert_eq!(labels("/home/sky/Pictures"), ["~", "Pictures"]);
        assert_eq!(labels("/etc/robin"), ["/", "etc", "robin"]);
        assert_eq!(labels("/"), ["/"]);
        let c = breadcrumbs_on(Path::new("/run/media/sky/SANDISK/DCIM"), home, &devices);
        assert_eq!(c[1].path, PathBuf::from("/run/media/sky/SANDISK/DCIM"));
    }

    #[test]
    fn debounce_waits_for_quiet() {
        let mut debounce = Debounce::new(Duration::from_millis(50));
        let t0 = Instant::now();
        assert!(!debounce.ready(t0));
        debounce.push(t0);
        assert!(!debounce.ready(t0 + Duration::from_millis(49)));
        debounce.push(t0 + Duration::from_millis(40));
        assert!(!debounce.ready(t0 + Duration::from_millis(80)));
        assert!(debounce.ready(t0 + Duration::from_millis(90)));
        assert!(!debounce.ready(t0 + Duration::from_millis(200)));
    }

    #[test]
    fn incremental_upsert_and_remove() {
        let mut listing = Listing::default();
        listing.set_entries(vec![entry("a", false, 1, 1)]);
        listing.upsert(entry("b", false, 2, 2));
        listing.upsert(entry("a", false, 9, 1));
        assert_eq!(listing.view()[0].len, 9);
        listing.remove_name(&OsString::from("b"));
        assert_eq!(names(&listing), ["a"]);
    }

    #[test]
    fn reads_a_real_directory() {
        let root = std::env::temp_dir().join(format!(
            "robin-model-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir_all(root.join("sub")).unwrap();
        fs::write(root.join("note.txt"), b"hi").unwrap();
        fs::write(root.join(".secret"), b"no").unwrap();
        let dir = DirFd::open(&root).unwrap();
        let listing = Listing::from_dir(&dir).unwrap();
        assert_eq!(names(&listing), ["sub", "note.txt"]);
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn modified_is_blank_before_1970_and_a_date_when_later_today() {
        let now = 1_793_413_800; // 08:00 at +05:30
        let ist = |_| 19_800;
        assert_eq!(format_modified(0, now, ist), "");
        assert_eq!(format_modified(-86_400, now, ist), "");
        assert_eq!(format_modified(now + 3_600, now, ist), "2026-10-31");
        assert_eq!(format_modified(now - 60, now, ist), "07:59");
    }

    #[test]
    fn debounce_fires_after_max_wait_even_if_events_keep_coming() {
        let t0 = Instant::now();
        let ms = Duration::from_millis;
        let mut d = Debounce::new(ms(200)).max_wait(ms(1500));
        let mut fired = None;
        for i in 0..40u64 {
            let t = t0 + ms(i * 100);
            d.push(t);
            if d.ready(t) {
                fired = Some(i * 100);
                break;
            }
        }
        assert_eq!(fired, Some(1500));
        let mut quiet = Debounce::new(ms(200)).max_wait(ms(1500));
        quiet.push(t0);
        assert!(!quiet.ready(t0 + ms(100)));
        assert!(quiet.ready(t0 + ms(200)));
    }
}
