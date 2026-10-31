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
    // (scene, window width). 680 and 700 fold the sidebar to the rail,
    // where full drives show their amber/red dots; 901 keeps it whole.
    let scenes = [
        ("browser-list", 800),
        ("nav-pending", 800),
        ("start-denied", 800),
        ("sidebar-device", 800),
        ("sidebar-trash", 800),
        ("trash-view", 800),
        ("trash-empty-confirm", 800),
        ("trash-empty", 800),
        ("trash-noinfo", 800),
        ("trash-restore-recreated", 800),
        ("grid-view", 800),
        ("grid-hidden", 800),
        ("list-density", 800),
        ("search-results", 800),
        ("search-nosearch", 800),
        ("search-letters", 800),
        ("browser-list", 680),
        ("browser-list", 700),
        ("browser-list", 901),
        ("view-image", 900),
        ("view-zoom", 900),
        ("view-info", 900),
        ("view-turn", 900),
        ("view-video", 900),
        ("view-loading", 900),
        ("view-pan", 900),
        ("view-wake", 900),
        ("view-strip-video", 900),
        ("stats-real", 1100),
    ];
    for (view, width) in scenes {
        let out = dir.join(format!("{view}-{width}.png"));
        let fixture = dir.join(format!("home-{view}-{width}"));
        let status = Command::new(env!("CARGO_BIN_EXE_robin"))
            .args([
                "--render",
                view,
                "--size",
                &format!("{width}x500"),
                "--fixture",
            ])
            .arg(&fixture)
            .arg("--out")
            .arg(&out)
            .status()
            .unwrap();
        assert!(status.success(), "{view}: {status}");
        let img = image::open(&out).unwrap();
        assert_eq!((img.width(), img.height()), (width, 500), "{view}");
        robin::fixture::remove(&fixture).unwrap();
    }
    let _ = std::fs::remove_dir_all(&dir);
}
