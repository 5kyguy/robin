//! Copying text to the desktop clipboard through a helper program
//! (`wl-copy` on Wayland, `xclip` or `xsel` on X11).
//!
//! Slint's own clipboard (`set_clipboard_text` via winit/arboard) keeps
//! the text only while Robin owns the selection: on Wayland it is lost
//! when the window closes, and its errors aren't reported back. The
//! helpers fork a small daemon that keeps serving the text, and they
//! fail with an exit status Robin can show.

use std::io::Write;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

/// Which display servers this session can reach.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Session {
    pub wayland: bool,
    pub x11: bool,
}

impl Session {
    /// From `WAYLAND_DISPLAY` and `DISPLAY` (set and not empty).
    pub fn from_env() -> Self {
        let set = |k| std::env::var_os(k).is_some_and(|v| !v.is_empty());
        Session {
            wayland: set("WAYLAND_DISPLAY"),
            x11: set("DISPLAY"),
        }
    }
}

const WL_COPY: &[&str] = &["wl-copy"];
const XCLIP: &[&str] = &["xclip", "-selection", "clipboard"];
const XSEL: &[&str] = &["xsel", "--clipboard", "--input"];

/// The helpers to try, in order: `wl-copy` on Wayland, then the X11
/// tools wherever `DISPLAY` is set (XWayland included).
pub fn candidates(session: Session) -> Vec<&'static [&'static str]> {
    let mut out = Vec::new();
    if session.wayland {
        out.push(WL_COPY);
    }
    if session.x11 {
        out.push(XCLIP);
        out.push(XSEL);
    }
    out
}

/// Why one helper didn't take the text.
#[derive(Debug, PartialEq, Eq)]
pub enum RunError {
    /// Not installed: try the next one.
    NotFound,
    /// Installed but failed: stop and say why.
    Failed(String),
}

/// Runs one helper with `data` on its stdin.
pub trait Runner {
    fn run(&self, argv: &[&str], data: &[u8]) -> Result<(), RunError>;
}

/// How a copy went.
#[derive(Debug, PartialEq, Eq)]
pub enum Outcome {
    /// The helper took the text.
    Copied,
    /// No helper is installed for this session.
    NoTool,
    /// A helper (or the session) failed; the reason for the toast.
    Failed(String),
}

/// Copy `data` with the first helper that is installed.
pub fn copy_with(runner: &dyn Runner, session: Session, data: &[u8]) -> Outcome {
    let tools = candidates(session);
    if tools.is_empty() {
        return Outcome::Failed("no Wayland or X11 display".into());
    }
    for argv in tools {
        match runner.run(argv, data) {
            Ok(()) => return Outcome::Copied,
            Err(RunError::NotFound) => continue,
            Err(RunError::Failed(why)) => return Outcome::Failed(why),
        }
    }
    Outcome::NoTool
}

/// Copy with the real helpers. Blocks for up to ~300ms: call it off the
/// UI thread.
pub fn copy(data: &[u8]) -> Outcome {
    copy_with(&System, Session::from_env(), data)
}

/// The toast text and tone for an outcome of copying `n` paths.
pub fn paths_toast(outcome: &Outcome, n: usize) -> (&'static str, String) {
    match outcome {
        Outcome::Copied if n == 1 => ("", "Copied 1 path".into()),
        Outcome::Copied => ("", format!("Copied {n} paths")),
        Outcome::NoTool => (
            "warning",
            "Couldn't copy: install wl-clipboard or xclip".into(),
        ),
        Outcome::Failed(why) => ("danger", format!("Couldn't copy the path: {why}")),
    }
}

/// Spawns the helper in its own session (so it outlives Robin and its
/// terminal), writes the text and closes stdin. The helpers fork a
/// daemon and exit, or keep running to serve the selection: either a
/// zero exit or still running after the grace period counts as taken.
struct System;

/// How long a helper gets to fail before it counts as having started.
const GRACE: Duration = Duration::from_millis(300);

impl Runner for System {
    fn run(&self, argv: &[&str], data: &[u8]) -> Result<(), RunError> {
        let (program, args) = argv.split_first().ok_or(RunError::NotFound)?;
        let mut cmd = Command::new(program);
        cmd.args(args)
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        {
            use std::os::unix::process::CommandExt;
            // SAFETY: setsid is async-signal-safe and touches no Rust state.
            unsafe {
                cmd.pre_exec(|| {
                    if libc::setsid() == -1 {
                        return Err(std::io::Error::last_os_error());
                    }
                    Ok(())
                });
            }
        }
        let mut child = match cmd.spawn() {
            Ok(c) => c,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Err(RunError::NotFound),
            Err(e) => return Err(RunError::Failed(format!("couldn't start {program}: {e}"))),
        };
        let wrote = child
            .stdin
            .take()
            .map(|mut s| s.write_all(data))
            .unwrap_or(Ok(()));
        let start = Instant::now();
        loop {
            match child.try_wait() {
                Ok(Some(status)) if status.success() => return Ok(()),
                Ok(Some(status)) => {
                    return Err(RunError::Failed(format!("{program} failed ({status})")))
                }
                Ok(None) if start.elapsed() >= GRACE => {
                    std::thread::spawn(move || child.wait());
                    return match wrote {
                        Ok(()) => Ok(()),
                        Err(e) => Err(RunError::Failed(format!("{program}: {e}"))),
                    };
                }
                Ok(None) => std::thread::sleep(Duration::from_millis(20)),
                Err(e) => return Err(RunError::Failed(format!("{program}: {e}"))),
            }
        }
    }
}

/// A stand-in for tests and renders: `installed` helpers take the text
/// (or fail with `fail`), the rest are missing. Records every call.
#[derive(Default)]
pub struct Fake {
    pub installed: Vec<&'static str>,
    pub fail: Option<String>,
    pub calls: std::cell::RefCell<Vec<(Vec<String>, Vec<u8>)>>,
}

impl Runner for Fake {
    fn run(&self, argv: &[&str], data: &[u8]) -> Result<(), RunError> {
        self.calls
            .borrow_mut()
            .push((argv.iter().map(|s| s.to_string()).collect(), data.to_vec()));
        if !self.installed.contains(&argv[0]) {
            return Err(RunError::NotFound);
        }
        match &self.fail {
            Some(why) => Err(RunError::Failed(why.clone())),
            None => Ok(()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const WAYLAND: Session = Session {
        wayland: true,
        x11: false,
    };
    const X11: Session = Session {
        wayland: false,
        x11: true,
    };
    const BOTH: Session = Session {
        wayland: true,
        x11: true,
    };

    fn fake(installed: &[&'static str]) -> Fake {
        Fake {
            installed: installed.to_vec(),
            ..Fake::default()
        }
    }

    fn tried(f: &Fake) -> Vec<String> {
        f.calls.borrow().iter().map(|c| c.0.join(" ")).collect()
    }

    #[test]
    fn wayland_copies_with_wl_copy() {
        let f = fake(&["wl-copy", "xclip"]);
        assert_eq!(copy_with(&f, BOTH, b"/home/sky/a.jpg"), Outcome::Copied);
        assert_eq!(tried(&f), ["wl-copy"]);
        assert_eq!(f.calls.borrow()[0].1, b"/home/sky/a.jpg");
        assert_eq!(
            paths_toast(&Outcome::Copied, 1),
            ("", "Copied 1 path".into())
        );
        assert_eq!(
            paths_toast(&Outcome::Copied, 3),
            ("", "Copied 3 paths".into())
        );
    }

    #[test]
    fn x11_uses_xclip_then_xsel() {
        let f = fake(&["xclip", "xsel"]);
        assert_eq!(copy_with(&f, X11, b"p"), Outcome::Copied);
        assert_eq!(tried(&f), ["xclip -selection clipboard"]);
        let f = fake(&["xsel"]);
        assert_eq!(copy_with(&f, X11, b"p"), Outcome::Copied);
        assert_eq!(
            tried(&f),
            ["xclip -selection clipboard", "xsel --clipboard --input"]
        );
        // wl-copy is never tried without a Wayland display.
        let f = fake(&["wl-copy"]);
        assert_eq!(copy_with(&f, X11, b"p"), Outcome::NoTool);
        assert!(!tried(&f).contains(&"wl-copy".to_string()));
    }

    #[test]
    fn missing_wl_copy_falls_back_to_x11_tools_under_xwayland() {
        let f = fake(&["xclip"]);
        assert_eq!(copy_with(&f, BOTH, b"p"), Outcome::Copied);
        assert_eq!(tried(&f), ["wl-copy", "xclip -selection clipboard"]);
        let f = fake(&["xclip"]);
        assert_eq!(copy_with(&f, WAYLAND, b"p"), Outcome::NoTool, "no DISPLAY");
    }

    #[test]
    fn no_tool_found_says_what_to_install() {
        let f = fake(&[]);
        let out = copy_with(&f, BOTH, b"p");
        assert_eq!(out, Outcome::NoTool);
        assert_eq!(tried(&f).len(), 3);
        assert_eq!(
            paths_toast(&out, 1),
            (
                "warning",
                "Couldn't copy: install wl-clipboard or xclip".into()
            )
        );
    }

    #[test]
    fn a_failing_tool_is_reported_not_skipped() {
        let f = Fake {
            installed: vec!["wl-copy", "xclip"],
            fail: Some("wl-copy failed (exit status: 1)".into()),
            ..Fake::default()
        };
        let out = copy_with(&f, BOTH, b"p");
        assert_eq!(tried(&f), ["wl-copy"]);
        assert_eq!(
            paths_toast(&out, 2),
            (
                "danger",
                "Couldn't copy the path: wl-copy failed (exit status: 1)".into()
            )
        );
        let out = copy_with(&fake(&[]), Session::default(), b"p");
        assert!(matches!(out, Outcome::Failed(_)), "no display at all");
    }

    #[test]
    fn the_real_runner_tells_missing_from_failing() {
        assert_eq!(
            System.run(&["robin-no-such-clipboard-tool"], b"p"),
            Err(RunError::NotFound)
        );
        // Exits non-zero at once: a failure, not a copy.
        assert!(matches!(
            System.run(&["sh", "-c", "cat >/dev/null; exit 3"], b"p"),
            Err(RunError::Failed(_))
        ));
        // Reads the text and exits zero, like wl-copy's parent.
        assert_eq!(System.run(&["sh", "-c", "cat >/dev/null"], b"p"), Ok(()));
        // Still running after the grace period, like a serving xclip.
        assert_eq!(
            System.run(&["sh", "-c", "cat >/dev/null; sleep 1"], b"p"),
            Ok(())
        );
    }
}
