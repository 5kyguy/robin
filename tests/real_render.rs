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
    // where full drives show their amber/red dots; 901 keeps it whole;
    // 480 is the smallest window the app is laid out for.
    let scenes = [
        ("browser-list", 800),
        ("nav-pending", 800),
        ("nav-history", 800),
        ("start-denied", 800),
        ("sidebar-device", 800),
        ("sidebar-trash", 800),
        ("trash-view", 800),
        ("trash-empty-confirm", 800),
        ("trash-empty", 800),
        ("trash-noinfo", 800),
        ("trash-restore-recreated", 800),
        ("grid-view", 800),
        ("grid-view-s", 800),
        ("grid-view-l", 800),
        ("grid-hidden", 800),
        ("prefs-comfortable", 800),
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
        ("view-scrub", 900),
        ("view-resume", 900),
        ("view-audio", 900),
        ("view-loading", 900),
        ("view-pan", 900),
        ("view-wake", 900),
        ("view-strip-video", 900),
        ("stats-real", 1100),
        ("stats-scanning", 1100),
        ("stats-scanning-first", 1100),
        // The 480px floor: the info panel floats, the filmstrip is cut
        // to the window, and the statistics cards stack.
        ("view-info", 480),
        ("view-image", 480),
        ("stats-real", 480),
        ("trash-empty-confirm", 480),
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
        if (view, width) == ("view-info", 480) {
            // The floating panel stops above the filmstrip: the current
            // tile's accent ring (strip at y 366..426) is still drawn.
            let rgb = img.to_rgb8();
            let ring = (366..426).any(|y| {
                (0..width).any(|x| {
                    let [r, g, b] = rgb.get_pixel(x, y).0;
                    r < 40 && g > 200 && b > 220
                })
            });
            assert!(ring, "the info panel hides the filmstrip's current tile");
        }
        robin::fixture::remove(&fixture).unwrap();
    }
    let _ = std::fs::remove_dir_all(&dir);
}

/// Return on a video plays it inside Robin: with libmpv installed the
/// frame is the clip's picture; without it (or with ROBIN_NO_MPV set) the
/// viewer says why and offers the default player instead.
#[test]
fn return_on_a_video_shows_its_frames_in_the_viewer() {
    let set = |k| std::env::var_os(k).is_some_and(|v| !v.is_empty());
    if !set("DISPLAY") && !set("WAYLAND_DISPLAY") {
        eprintln!("no display; skipping");
        return;
    }
    let dir = std::env::temp_dir().join(format!("robin-play-render-{}", std::process::id()));
    // Blue in the clip's glow, left of centre; the plain canvas otherwise.
    let blue_at = |no_mpv: bool| {
        let tag = if no_mpv { "off" } else { "on" };
        let out = dir.join(format!("view-video-{tag}.png"));
        let fixture = dir.join(format!("home-{tag}"));
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_robin"));
        cmd.args(["--render", "view-video", "--size", "900x500", "--fixture"])
            .arg(&fixture)
            .arg("--out")
            .arg(&out);
        if no_mpv {
            cmd.env("ROBIN_NO_MPV", "1");
        }
        assert!(cmd.status().unwrap().success(), "{tag}");
        let img = image::open(&out).unwrap().to_rgb8();
        robin::fixture::remove(&fixture).unwrap();
        img.get_pixel(360, 225)[2]
    };
    let off = blue_at(true);
    assert!(off < 60, "no libmpv: plain canvas, got {off}");
    if robin::mpv::available() {
        let on = blue_at(false);
        assert!(on > 90, "libmpv: the clip's frame, got {on}");
    } else {
        eprintln!("libmpv not installed; only the fallback was checked");
    }
    let _ = std::fs::remove_dir_all(&dir);
}
