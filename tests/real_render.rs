//! `--render` runs the real app in its own process (one Slint window per
//! process), so each scripted view is checked by spawning the binary.

use std::process::Command;

#[test]
fn scripted_views_render_through_the_real_app() {
    // test-bwrap.sh blanks these rather than unsetting them.
    let set = |k| std::env::var_os(k).is_some_and(|v| !v.is_empty());
    if !set("DISPLAY") && !set("WAYLAND_DISPLAY") {
        eprintln!("no display; skipping");
        return;
    }
    let dir = std::env::temp_dir().join(format!("robin-real-render-{}", std::process::id()));
    for view in [
        "browser-list",
        "nav-pending",
        "start-denied",
        "sidebar-device",
        "sidebar-trash",
        "trash-view",
        "trash-empty-confirm",
        "trash-empty",
        "trash-noinfo",
        "trash-restore-recreated",
    ] {
        let out = dir.join(format!("{view}.png"));
        let fixture = dir.join(format!("home-{view}"));
        let status = Command::new(env!("CARGO_BIN_EXE_robin"))
            .args(["--render", view, "--size", "800x500", "--fixture"])
            .arg(&fixture)
            .arg("--out")
            .arg(&out)
            .status()
            .unwrap();
        assert!(status.success(), "{view}: {status}");
        let img = image::open(&out).unwrap();
        assert_eq!((img.width(), img.height()), (800, 500), "{view}");
        robin::fixture::remove(&fixture).unwrap();
    }
    let _ = std::fs::remove_dir_all(&dir);
}
