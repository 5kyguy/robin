//! Fixed argv for the programs Robin does not replace.
//!
//! No shell. The first element is the executable and the rest are arguments.

use std::ffi::OsString;
use std::path::Path;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Handoff {
    Pdf,
    Archive,
    Annotate,
    Wallpaper,
    Share,
    Terminal,
    Notify,
    CopyPath,
}

pub fn argv(kind: Handoff, target: &Path) -> Vec<OsString> {
    let program = match kind {
        Handoff::Pdf => "zathura",
        Handoff::Archive => "file-roller",
        Handoff::Annotate => "tensaku",
        Handoff::Wallpaper => "r2-d2-theme-bg-set",
        Handoff::Share => "r2-d2-cmd-share",
        Handoff::Terminal => "xdg-terminal-exec",
        Handoff::Notify => "r2-d2-notification-send",
        Handoff::CopyPath => "wl-copy",
    };
    let mut out = vec![OsString::from(program)];
    match kind {
        Handoff::Terminal => {}
        Handoff::Notify => {
            out.push(OsString::from("--"));
            out.push(target.as_os_str().to_os_string());
        }
        _ => out.push(target.as_os_str().to_os_string()),
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_handoff_is_a_fixed_program() {
        let path = Path::new("/tmp/notes.pdf");
        for kind in [
            Handoff::Pdf,
            Handoff::Archive,
            Handoff::Annotate,
            Handoff::Wallpaper,
            Handoff::Share,
            Handoff::Terminal,
            Handoff::Notify,
            Handoff::CopyPath,
        ] {
            let argv = argv(kind, path);
            assert!(!argv[0].to_string_lossy().contains(' '));
            assert!(!argv.iter().any(|arg| arg.to_string_lossy().contains('|')));
        }
        assert_eq!(argv(Handoff::Pdf, path)[0], "zathura");
        assert_eq!(argv(Handoff::Terminal, path).len(), 1);
        assert_eq!(argv(Handoff::CopyPath, path)[1], "/tmp/notes.pdf");
    }
}
