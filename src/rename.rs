//! Renaming one item in place (DESIGN §4, Inline rename).
//!
//! A rename never replaces anything. The first try is `renameat2` with
//! `RENAME_NOREPLACE`, which the kernel refuses with EEXIST if the name is
//! taken, however late the other file appeared. Some filesystems don't
//! take the flag (gvfs's FUSE mount, some FAT setups, old kernels): they
//! answer EINVAL, ENOSYS or EOPNOTSUPP. Robin never quietly drops to a
//! plain `rename()` there. A file is hard-linked under the new name
//! (`linkat` also fails with EEXIST on a taken name) and only then
//! unlinked under the old one. Where links don't work either (FAT, FUSE),
//! or for a folder, the new name is checked immediately before a plain
//! `renameat`. That leaves one race: a file created under the new name
//! between that check and the rename call can be replaced. It is the
//! window of one syscall, on filesystems that offer nothing better.
//!
//! Case-only renames (`photo.jpg` to `Photo.JPG`) on case-insensitive
//! filesystems such as vfat or exfat see their own file under the new
//! name, so NOREPLACE says EEXIST. When the "other" file is the same
//! inode on the same device, the rename goes in two steps through a
//! hidden name in the same folder, and the second step failing puts the
//! original name back.

use std::ffi::{CStr, CString, OsStr, OsString};
use std::os::unix::ffi::OsStrExt;
use std::path::Path;

use rustix::fs::{AtFlags, RenameFlags};
use rustix::io::Errno;

use crate::safety::DirFd;

/// Why a typed name can't be used. Shown inline under the field.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Problem {
    Empty,
    /// `.` or `..`.
    Dots,
    Slash,
    Nul,
    /// Longer than the 255 bytes a Linux filesystem allows.
    TooLong,
    /// Another item in the folder has this name.
    Taken {
        name: String,
        dir: bool,
    },
}

impl Problem {
    pub fn text(&self) -> String {
        match self {
            Problem::Empty => "Enter a name".into(),
            Problem::Dots => "\".\" and \"..\" can't be used as names".into(),
            Problem::Slash => "Names can't contain \"/\"".into(),
            Problem::Nul => "Names can't contain a null character".into(),
            Problem::TooLong => "Names can be at most 255 bytes long".into(),
            Problem::Taken { name, dir } => taken_text(name, *dir),
        }
    }
}

/// "A file named x already exists", or "A folder named …".
pub fn taken_text(name: &str, dir: bool) -> String {
    format!(
        "A {} named {name} already exists",
        if dir { "folder" } else { "file" }
    )
}

/// Check a typed name against the rules and the folder's other items.
/// `taken` answers whether another item has exactly that name (and
/// whether it is a folder). The item's own name is never a clash, and
/// a case-only change is left to the filesystem to settle.
pub fn check(
    typed: &str,
    old: &OsStr,
    taken: impl Fn(&OsStr) -> Option<bool>,
) -> Result<(), Problem> {
    if typed.is_empty() {
        return Err(Problem::Empty);
    }
    if typed == "." || typed == ".." {
        return Err(Problem::Dots);
    }
    if typed.contains('/') {
        return Err(Problem::Slash);
    }
    if typed.contains('\0') {
        return Err(Problem::Nul);
    }
    if typed.len() > 255 {
        return Err(Problem::TooLong);
    }
    let name = OsStr::new(typed);
    if name == old {
        return Ok(());
    }
    match taken(name) {
        Some(dir) => Err(Problem::Taken {
            name: typed.into(),
            dir,
        }),
        None => Ok(()),
    }
}

/// How many bytes of `name` the field pre-selects: the name without its
/// extension. Folders, dotfiles with no further dot (`.bashrc`) and names
/// ending in a dot select everything; `.tar.gz` and its kin count as one
/// extension.
pub fn stem_len(name: &str, dir: bool) -> usize {
    if dir {
        return name.len();
    }
    let lower = name.to_ascii_lowercase();
    for double in [".tar.gz", ".tar.xz", ".tar.zst", ".tar.bz2", ".tar.lz"] {
        if lower.ends_with(double) && lower.len() > double.len() {
            return name.len() - double.len();
        }
    }
    match name.rfind('.') {
        Some(0) | None => name.len(),
        Some(i) if i + 1 == name.len() => name.len(),
        Some(i) => i,
    }
}

/// How a rename went through.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum How {
    /// `renameat2` with `RENAME_NOREPLACE`.
    Atomic,
    /// The flag isn't supported: hard link, then unlink.
    Linked,
    /// Neither the flag nor links: checked, then a plain rename.
    Checked,
    /// A case-only rename on a case-insensitive filesystem, through a
    /// hidden name.
    TwoStep,
    /// The name didn't change.
    Same,
}

/// Why a rename didn't happen. The original is untouched in every case.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Failure {
    /// The name is taken (`dir`: by a folder).
    Taken {
        dir: bool,
    },
    /// The filesystem won't rename here (a phone folder, a read-only
    /// drive, no permission).
    Refused(Errno),
    /// The item is no longer there.
    Gone,
    Io(Errno),
    /// The name can't be a file name.
    Invalid,
}

/// The few calls a rename makes, inside one folder. [`DirOps`] makes them
/// on a real folder; tests swap in a case-insensitive fake.
pub trait Ops {
    fn rename_noreplace(&self, from: &CStr, to: &CStr) -> Result<(), Errno>;
    /// A plain rename, which would replace `to`. Only reached after a
    /// last-moment check that `to` doesn't exist.
    fn rename_plain(&self, from: &CStr, to: &CStr) -> Result<(), Errno>;
    fn link(&self, from: &CStr, to: &CStr) -> Result<(), Errno>;
    fn unlink(&self, name: &CStr) -> Result<(), Errno>;
    /// (device, inode, is a folder), not following a final symlink.
    fn ident(&self, name: &CStr) -> Result<(u64, u64, bool), Errno>;
}

/// [`Ops`] on a real folder, through its file descriptor.
pub struct DirOps<'a>(pub &'a DirFd);

impl Ops for DirOps<'_> {
    fn rename_noreplace(&self, from: &CStr, to: &CStr) -> Result<(), Errno> {
        rustix::fs::renameat_with(
            self.0.as_fd(),
            from,
            self.0.as_fd(),
            to,
            RenameFlags::NOREPLACE,
        )
    }
    fn rename_plain(&self, from: &CStr, to: &CStr) -> Result<(), Errno> {
        rustix::fs::renameat(self.0.as_fd(), from, self.0.as_fd(), to)
    }
    fn link(&self, from: &CStr, to: &CStr) -> Result<(), Errno> {
        rustix::fs::linkat(self.0.as_fd(), from, self.0.as_fd(), to, AtFlags::empty())
    }
    fn unlink(&self, name: &CStr) -> Result<(), Errno> {
        rustix::fs::unlinkat(self.0.as_fd(), name, AtFlags::empty())
    }
    fn ident(&self, name: &CStr) -> Result<(u64, u64, bool), Errno> {
        let st = rustix::fs::statat(self.0.as_fd(), name, AtFlags::SYMLINK_NOFOLLOW)?;
        Ok((st.st_dev, st.st_ino, st.st_mode & 0o170_000 == 0o040_000))
    }
}

/// Rename `from` to `to` inside `dir`, never replacing anything.
/// Blocking: on a phone each call crosses USB, so the window runs it on
/// a worker.
pub fn rename(dir: &Path, from: &OsStr, to: &OsStr) -> Result<How, Failure> {
    let fd = DirFd::open(dir).map_err(|e| match &e {
        crate::Error::Io { source, .. } => match source.raw_os_error() {
            Some(n) if n == libc::ENOENT || n == libc::ENOTDIR => Failure::Gone,
            Some(n) => Failure::Io(Errno::from_raw_os_error(n)),
            None => Failure::Io(Errno::IO),
        },
        _ => Failure::Refused(Errno::PERM),
    })?;
    rename_with(&DirOps(&fd), from, to, &mut || {})
}

/// [`rename`] over any [`Ops`]. `before` runs right before each call
/// that could create the new name, so a test can make the name appear
/// at the last moment.
pub fn rename_with(
    ops: &dyn Ops,
    from: &OsStr,
    to: &OsStr,
    before: &mut dyn FnMut(),
) -> Result<How, Failure> {
    let from_c = c_name(from)?;
    let to_c = c_name(to)?;
    if from == to {
        return Ok(How::Same);
    }
    match place(ops, &from_c, &to_c, before) {
        Err(Failure::Taken { dir }) => {
            if same_object(ops, &from_c, &to_c) {
                two_step(ops, from, &from_c, &to_c, before)
            } else {
                Err(Failure::Taken { dir })
            }
        }
        other => other,
    }
}

fn c_name(name: &OsStr) -> Result<CString, Failure> {
    let b = name.as_bytes();
    if b.is_empty() || b.contains(&b'/') || b == b"." || b == b".." {
        return Err(Failure::Invalid);
    }
    CString::new(b).map_err(|_| Failure::Invalid)
}

/// The flag isn't understood: EINVAL from FUSE and older kernels,
/// ENOSYS without renameat2, EOPNOTSUPP from filesystems that refuse it.
fn flag_unsupported(e: Errno) -> bool {
    e == Errno::INVAL || e == Errno::NOSYS || e == Errno::OPNOTSUPP
}

fn failure(e: Errno) -> Failure {
    if e == Errno::NOENT {
        Failure::Gone
    } else if e == Errno::PERM
        || e == Errno::ACCESS
        || e == Errno::ROFS
        || e == Errno::OPNOTSUPP
        || e == Errno::NOSYS
    {
        Failure::Refused(e)
    } else {
        Failure::Io(e)
    }
}

fn taken(ops: &dyn Ops, to: &CStr) -> Failure {
    Failure::Taken {
        dir: ops.ident(to).is_ok_and(|i| i.2),
    }
}

fn same_object(ops: &dyn Ops, a: &CStr, b: &CStr) -> bool {
    match (ops.ident(a), ops.ident(b)) {
        (Ok(x), Ok(y)) => x.0 == y.0 && x.1 == y.1,
        _ => false,
    }
}

/// Put `from` at `to` unless `to` exists. `Taken` covers a name taken
/// by the same file under another case, which the caller sorts out.
fn place(ops: &dyn Ops, from: &CStr, to: &CStr, before: &mut dyn FnMut()) -> Result<How, Failure> {
    before();
    match ops.rename_noreplace(from, to) {
        Ok(()) => return Ok(How::Atomic),
        Err(e) if e == Errno::EXIST => return Err(taken(ops, to)),
        Err(e) if flag_unsupported(e) => {}
        Err(e) => return Err(failure(e)),
    }
    let (_, _, dir) = ops.ident(from).map_err(failure)?;
    if !dir {
        before();
        match ops.link(from, to) {
            Ok(()) => {
                return match ops.unlink(from) {
                    Ok(()) => Ok(How::Linked),
                    Err(e) => {
                        // Two names for one file: drop the new one.
                        let _ = ops.unlink(to);
                        Err(failure(e))
                    }
                };
            }
            Err(e) if e == Errno::EXIST => return Err(taken(ops, to)),
            // No hard links here (FAT, FUSE): the checked rename.
            Err(_) => {}
        }
    }
    before();
    match ops.ident(to) {
        Ok(_) => return Err(taken(ops, to)),
        Err(e) if e == Errno::NOENT => {}
        Err(e) => return Err(failure(e)),
    }
    // The one window left: something created at `to` between the check
    // above and this call is replaced.
    ops.rename_plain(from, to).map_err(failure)?;
    Ok(How::Checked)
}

/// A case-only rename where the filesystem sees one file under both
/// names: to a hidden name, then to the new one. If the second step
/// fails, the original name comes back.
fn two_step(
    ops: &dyn Ops,
    from: &OsStr,
    from_c: &CStr,
    to_c: &CStr,
    before: &mut dyn FnMut(),
) -> Result<How, Failure> {
    let tmp = (0..100)
        .map(|n| {
            let mut s = OsString::from(format!(".robin-rename-{}-{n}-", std::process::id()));
            s.push(from);
            s
        })
        .filter_map(|s| CString::new(s.as_bytes()).ok())
        .find(|c| c.as_bytes().len() <= 255 && ops.ident(c) == Err(Errno::NOENT))
        .ok_or(Failure::Io(Errno::EXIST))?;
    place(ops, from_c, &tmp, before)?;
    match place(ops, &tmp, to_c, before) {
        Ok(_) => Ok(How::TwoStep),
        Err(e) => {
            let _ = place(ops, &tmp, from_c, &mut || {});
            Err(e)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::{Cell, RefCell};

    fn scratch(label: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "robin-rename-{label}-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn names_are_checked_before_anything_is_touched() {
        let none = |_: &OsStr| None;
        let old = OsStr::new("a.txt");
        assert_eq!(check("", old, none), Err(Problem::Empty));
        assert_eq!(check(".", old, none), Err(Problem::Dots));
        assert_eq!(check("..", old, none), Err(Problem::Dots));
        assert_eq!(check("a/b", old, none), Err(Problem::Slash));
        assert_eq!(check("a\0b", old, none), Err(Problem::Nul));
        assert_eq!(check(&"x".repeat(256), old, none), Err(Problem::TooLong));
        assert_eq!(check("a.txt", old, |_| Some(false)), Ok(()), "its own name");
        assert_eq!(check("...", old, none), Ok(()));
        let clash = check("b.txt", old, |n| (n == "b.txt").then_some(false));
        assert_eq!(
            clash.unwrap_err().text(),
            "A file named b.txt already exists"
        );
        let folder = check("raw", old, |n| (n == "raw").then_some(true));
        assert_eq!(
            folder.unwrap_err().text(),
            "A folder named raw already exists"
        );
        assert_eq!(
            check("A.TXT", old, none),
            Ok(()),
            "case-only is the filesystem's call"
        );
    }

    #[test]
    fn the_stem_leaves_the_extension_out() {
        assert_eq!(stem_len("dune-sunset-0412.jpg", false), 16);
        assert_eq!(stem_len("holo-backup.tar.zst", false), 11);
        assert_eq!(stem_len(".bashrc", false), 7);
        assert_eq!(stem_len(".config.json", false), 7);
        assert_eq!(stem_len("README", false), 6);
        assert_eq!(stem_len("odd.", false), 4);
        assert_eq!(stem_len("raw-exports.2026", true), 16, "folders select all");
        assert_eq!(stem_len("été.png", false), "été".len(), "bytes, not chars");
    }

    #[test]
    fn a_rename_onto_a_taken_name_leaves_both_alone() {
        let dir = scratch("clash");
        std::fs::write(dir.join("a.txt"), "a").unwrap();
        std::fs::write(dir.join("b.txt"), "b").unwrap();
        let got = rename(&dir, OsStr::new("a.txt"), OsStr::new("b.txt"));
        assert_eq!(got, Err(Failure::Taken { dir: false }));
        assert_eq!(std::fs::read_to_string(dir.join("a.txt")).unwrap(), "a");
        assert_eq!(std::fs::read_to_string(dir.join("b.txt")).unwrap(), "b");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_name_taken_after_the_check_is_still_never_replaced() {
        let dir = scratch("race");
        std::fs::write(dir.join("a.txt"), "a").unwrap();
        // The listing said "b.txt" was free; it appears just before the
        // rename call.
        assert_eq!(check("b.txt", OsStr::new("a.txt"), |_| None), Ok(()));
        let fd = DirFd::open(&dir).unwrap();
        let late = dir.join("b.txt");
        let mut before = || {
            if !late.exists() {
                std::fs::write(&late, "late").unwrap();
            }
        };
        let got = rename_with(
            &DirOps(&fd),
            OsStr::new("a.txt"),
            OsStr::new("b.txt"),
            &mut before,
        );
        assert_eq!(got, Err(Failure::Taken { dir: false }));
        assert_eq!(std::fs::read_to_string(dir.join("a.txt")).unwrap(), "a");
        assert_eq!(std::fs::read_to_string(&late).unwrap(), "late");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_case_only_rename_on_a_case_sensitive_disk_is_one_step() {
        let dir = scratch("case");
        std::fs::write(dir.join("photo.jpg"), "p").unwrap();
        let got = rename(&dir, OsStr::new("photo.jpg"), OsStr::new("Photo.JPG"));
        assert_eq!(got, Ok(How::Atomic));
        let names: Vec<_> = std::fs::read_dir(&dir)
            .unwrap()
            .map(|e| e.unwrap().file_name())
            .collect();
        assert_eq!(names, ["Photo.JPG"]);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_hard_link_under_the_new_name_is_not_mistaken_for_a_case_change() {
        // Same inode, but a real second name: the two-step must not
        // replace it, and the original name comes back.
        let dir = scratch("hardlink");
        std::fs::write(dir.join("a"), "x").unwrap();
        std::fs::hard_link(dir.join("a"), dir.join("b")).unwrap();
        let got = rename(&dir, OsStr::new("a"), OsStr::new("b"));
        assert_eq!(got, Err(Failure::Taken { dir: false }));
        assert!(dir.join("a").exists() && dir.join("b").exists());
        assert_eq!(
            std::fs::read_dir(&dir).unwrap().count(),
            2,
            "no hidden name left"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A folder held in memory: case-insensitive like vfat when `fold`,
    /// with or without NOREPLACE and hard links, or refusing every
    /// rename like some phone folders.
    #[derive(Default)]
    struct Fake {
        /// (name, inode).
        items: RefCell<Vec<(String, u64)>>,
        fold: bool,
        no_flag: bool,
        no_links: bool,
        refuse: bool,
        /// Fail the n-th successful-looking rename (1-based) with EIO.
        fail_nth: Cell<u32>,
        plain_calls: Cell<u32>,
    }

    impl Fake {
        fn with(names: &[&str]) -> Self {
            let f = Fake::default();
            for (i, n) in names.iter().enumerate() {
                f.items.borrow_mut().push((n.to_string(), i as u64 + 1));
            }
            f
        }
        fn find(&self, name: &CStr) -> Option<usize> {
            let n = name.to_str().unwrap();
            self.items.borrow().iter().position(|(m, _)| {
                if self.fold {
                    m.eq_ignore_ascii_case(n)
                } else {
                    m == n
                }
            })
        }
        fn names(&self) -> Vec<String> {
            self.items.borrow().iter().map(|i| i.0.clone()).collect()
        }
        fn tick(&self) -> Result<(), Errno> {
            let n = self.fail_nth.get();
            if n == 1 {
                self.fail_nth.set(0);
                return Err(Errno::IO);
            }
            if n > 1 {
                self.fail_nth.set(n - 1);
            }
            Ok(())
        }
        fn set_name(&self, from: &CStr, to: &CStr) -> Result<(), Errno> {
            let i = self.find(from).ok_or(Errno::NOENT)?;
            self.items.borrow_mut()[i].0 = to.to_str().unwrap().into();
            Ok(())
        }
    }

    impl Ops for Fake {
        fn rename_noreplace(&self, from: &CStr, to: &CStr) -> Result<(), Errno> {
            if self.refuse {
                return Err(Errno::OPNOTSUPP);
            }
            if self.no_flag {
                return Err(Errno::INVAL);
            }
            if self.find(to).is_some() {
                return Err(Errno::EXIST);
            }
            self.tick()?;
            self.set_name(from, to)
        }
        fn rename_plain(&self, from: &CStr, to: &CStr) -> Result<(), Errno> {
            self.plain_calls.set(self.plain_calls.get() + 1);
            if self.refuse {
                return Err(Errno::OPNOTSUPP);
            }
            self.tick()?;
            let src = self.find(from).ok_or(Errno::NOENT)?;
            if let Some(dst) = self.find(to) {
                if dst != src {
                    // A plain rename replaces: the fallback must never
                    // get here with the name taken.
                    self.items.borrow_mut().remove(dst);
                }
            }
            self.set_name(from, to)
        }
        fn link(&self, from: &CStr, to: &CStr) -> Result<(), Errno> {
            if self.no_links || self.refuse {
                return Err(Errno::PERM);
            }
            if self.find(to).is_some() {
                return Err(Errno::EXIST);
            }
            let ino = self.items.borrow()[self.find(from).ok_or(Errno::NOENT)?].1;
            self.items
                .borrow_mut()
                .push((to.to_str().unwrap().into(), ino));
            Ok(())
        }
        fn unlink(&self, name: &CStr) -> Result<(), Errno> {
            let i = self.find(name).ok_or(Errno::NOENT)?;
            self.items.borrow_mut().remove(i);
            Ok(())
        }
        fn ident(&self, name: &CStr) -> Result<(u64, u64, bool), Errno> {
            let i = self.find(name).ok_or(Errno::NOENT)?;
            Ok((1, self.items.borrow()[i].1, false))
        }
    }

    fn go(f: &Fake, from: &str, to: &str) -> Result<How, Failure> {
        rename_with(f, OsStr::new(from), OsStr::new(to), &mut || {})
    }

    #[test]
    fn a_case_only_rename_on_vfat_goes_through_a_hidden_name() {
        let f = Fake {
            fold: true,
            ..Fake::with(&["photo.jpg", "other.jpg"])
        };
        assert_eq!(go(&f, "photo.jpg", "Photo.JPG"), Ok(How::TwoStep));
        assert_eq!(f.names(), ["Photo.JPG", "other.jpg"]);
        // A real clash is still a clash on a folding disk.
        assert_eq!(
            go(&f, "Photo.JPG", "OTHER.jpg"),
            Err(Failure::Taken { dir: false })
        );
        assert_eq!(f.names(), ["Photo.JPG", "other.jpg"]);
    }

    #[test]
    fn a_failed_second_step_puts_the_original_name_back() {
        let f = Fake {
            fold: true,
            ..Fake::with(&["photo.jpg"])
        };
        // First rename (to the hidden name) works, the second fails.
        f.fail_nth.set(2);
        assert_eq!(
            go(&f, "photo.jpg", "Photo.JPG"),
            Err(Failure::Io(Errno::IO))
        );
        assert_eq!(f.names(), ["photo.jpg"]);
    }

    #[test]
    fn without_the_flag_a_file_is_linked_then_unlinked() {
        let f = Fake {
            no_flag: true,
            ..Fake::with(&["a.txt", "b.txt"])
        };
        assert_eq!(go(&f, "a.txt", "c.txt"), Ok(How::Linked));
        assert_eq!(f.names(), ["b.txt", "c.txt"]);
        assert_eq!(go(&f, "c.txt", "b.txt"), Err(Failure::Taken { dir: false }));
        assert_eq!(f.names(), ["b.txt", "c.txt"]);
        assert_eq!(f.plain_calls.get(), 0, "never a plain rename");
    }

    #[test]
    fn without_the_flag_or_links_the_name_is_checked_at_the_last_moment() {
        let f = Fake {
            no_flag: true,
            no_links: true,
            ..Fake::with(&["a.txt"])
        };
        // The name appears after validation, before the rename call.
        let appeared = Cell::new(false);
        let mut before = || {
            if !appeared.replace(true) {
                f.items.borrow_mut().push(("b.txt".into(), 99));
            }
        };
        let got = rename_with(&f, OsStr::new("a.txt"), OsStr::new("b.txt"), &mut before);
        assert_eq!(got, Err(Failure::Taken { dir: false }));
        assert_eq!(f.names(), ["a.txt", "b.txt"], "nothing replaced");
        assert_eq!(f.plain_calls.get(), 0);
        assert_eq!(go(&f, "a.txt", "c.txt"), Ok(How::Checked));
        assert_eq!(f.names(), ["c.txt", "b.txt"]);
    }

    #[test]
    fn a_case_only_rename_without_the_flag_still_takes_two_steps() {
        let f = Fake {
            fold: true,
            no_flag: true,
            no_links: true,
            ..Fake::with(&["photo.jpg"])
        };
        assert_eq!(go(&f, "photo.jpg", "Photo.JPG"), Ok(How::TwoStep));
        assert_eq!(f.names(), ["Photo.JPG"]);
    }

    #[test]
    fn a_phone_folder_that_refuses_renames_keeps_the_original() {
        let f = Fake {
            refuse: true,
            ..Fake::with(&["PXL_1.jpg"])
        };
        assert_eq!(
            go(&f, "PXL_1.jpg", "beach.jpg"),
            Err(Failure::Refused(Errno::OPNOTSUPP))
        );
        assert_eq!(f.names(), ["PXL_1.jpg"]);
    }

    #[test]
    fn invalid_names_never_reach_the_filesystem() {
        let f = Fake::with(&["a"]);
        for bad in ["", ".", "..", "x/y"] {
            assert_eq!(go(&f, "a", bad), Err(Failure::Invalid));
        }
        assert_eq!(go(&f, "a", "a"), Ok(How::Same));
        assert_eq!(f.names(), ["a"]);
    }
}
