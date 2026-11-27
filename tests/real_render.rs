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
        // Removable drives against the fake backend.
        ("devices-plugged", 800),
        ("devices-mounted", 800),
        ("devices-read-only", 800),
        ("devices-busy-copy", 800),
        ("devices-busy-other", 800),
        ("devices-safe-unplug", 800),
        ("devices-removed", 800),
        ("devices-removed-elsewhere", 800),
        ("devices-power-off", 800),
        ("devices-partition-busy", 800),
        ("devices-partition-unplug", 800),
        ("devices-partitions-removed-elsewhere", 800),
        ("devices-unmounted", 800),
        ("devices-no-space", 800),
        ("devices-fat-too-big", 800),
        ("devices-stats", 1100),
        ("devices-menu", 800),
        // Encrypted drives: each scene checks itself and that the
        // passphrase is in nothing it can read back.
        ("devices-luks-locked", 800),
        ("devices-luks-menu", 800),
        ("devices-luks-unlock", 800),
        ("devices-luks-cancel", 800),
        ("devices-luks-unlocking", 800),
        ("devices-luks-wrong", 800),
        ("devices-luks-error", 800),
        ("devices-luks-retry", 800),
        ("devices-luks-unlocked", 800),
        ("devices-luks-eject", 800),
        ("devices-luks-eject-unmounted", 800),
        ("devices-luks-lock-failed", 800),
        ("devices-luks-wrong", 480),
        ("devices-luks-masked", 800),
        ("devices-luks-shown", 800),
        ("devices-luks-shown", 480),
        ("devices-luks-unlocked-row", 800),
        ("devices-luks-truecrypt-wrong", 800),
        ("devices-luks-truecrypt-wrong", 480),
        ("devices-luks-bitlocker-wrong", 800),
        ("phones-plugged", 800),
        ("phones-row", 800),
        ("phones-waiting", 800),
        ("phones-still-waiting", 800),
        ("phones-allowed", 800),
        ("phones-storage", 800),
        ("phones-loading", 800),
        ("phones-top-refused", 800),
        ("phones-no-write", 800),
        ("phones-no-trash", 800),
        ("phones-copying", 800),
        ("phones-copied", 800),
        ("phones-busy-copy", 800),
        ("phones-safe-unplug", 800),
        ("phones-removed", 800),
        ("phones-removed-elsewhere", 800),
        ("phones-menu", 800),
        ("phones-camera-waiting", 800),
        ("phones-camera-still-waiting", 800),
        ("phones-new-folder", 800),
        ("phones-rename-refused", 800),
        ("phones-rename-refused-reached", 800),
        // The file menu, inline rename and Properties.
        ("context-menu", 800),
        ("context-menu-edge", 800),
        ("context-menu-multi", 800),
        ("devices-menu-read-only", 800),
        ("rename-editing", 800),
        ("rename-clash", 800),
        ("rename-invalid", 800),
        ("rename-grid", 800),
        ("rename-done", 800),
        ("properties", 800),
        ("properties-multi", 800),
        ("properties", 480),
        ("copy-path", 800),
        ("copy-path-xclip", 800),
        ("copy-path-no-tool", 800),
        ("rename-stranded", 800),
        ("rename-stranded-dismissed", 800),
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
