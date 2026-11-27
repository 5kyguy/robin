//! `--render`: the real app against a seeded fixture home, driven by a
//! short input script with a frozen clock, then captured to a PNG.
//!
//! Only what the live app sets can show up; anything the live app leaves
//! unset renders blank. `--render-mock` keeps the hand-filled frames in
//! `shots.rs` for views that have no script yet.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use slint::ComponentHandle;

use crate::app::{self, Launch};
use crate::error::{Error, Result};
use crate::theme::Theme;
use crate::AppWindow;

/// 2026-10-31 16:10 in +05:30, the same instant as the mock frames.
pub const FROZEN_NOW: i64 = 1_793_443_200;
pub const FROZEN_OFFSET: i64 = 19_800;
/// Fixture files are stamped 40 minutes before the frozen now.
pub const FIXTURE_MTIME: i64 = FROZEN_NOW - 2_400;

/// Views with an input script.
pub fn scripted(view: &str) -> bool {
    matches!(
        view,
        "browser-list"
            | "nav-pending"
            | "nav-history"
            | "start-denied"
            | "sidebar-device"
            | "sidebar-trash"
            | "trash-view"
            | "trash-empty-confirm"
            | "trash-empty"
            | "trash-noinfo"
            | "trash-restore-recreated"
            | "grid-view"
            | "grid-view-s"
            | "grid-view-l"
            | "grid-hidden"
            | "prefs-comfortable"
            | "search-results"
            | "search-letters"
            | "search-nosearch"
            | "view-image"
            | "view-zoom"
            | "view-info"
            | "view-turn"
            | "view-video"
            | "view-scrub"
            | "view-resume"
            | "view-audio"
            | "view-loading"
            | "view-pan"
            | "view-wake"
            | "view-strip-video"
            | "stats-real"
            | "stats-scanning"
            | "stats-scanning-first"
            | "devices-plugged"
            | "phones-plugged"
            | "phones-row"
            | "phones-waiting"
            | "phones-still-waiting"
            | "phones-allowed"
            | "phones-storage"
            | "phones-loading"
            | "phones-top-refused"
            | "phones-no-write"
            | "phones-no-trash"
            | "phones-copying"
            | "phones-copied"
            | "phones-busy-copy"
            | "phones-safe-unplug"
            | "phones-removed"
            | "phones-removed-elsewhere"
            | "phones-menu"
            | "phones-camera-waiting"
            | "phones-camera-still-waiting"
            | "phones-new-folder"
            | "devices-mounted"
            | "devices-read-only"
            | "devices-busy-copy"
            | "devices-busy-other"
            | "devices-safe-unplug"
            | "devices-removed"
            | "devices-removed-elsewhere"
            | "devices-power-off"
            | "devices-partition-busy"
            | "devices-partition-unplug"
            | "devices-partitions-removed-elsewhere"
            | "devices-unmounted"
            | "devices-no-space"
            | "devices-fat-too-big"
            | "devices-stats"
            | "devices-menu"
            | "phones-rename-refused"
            | "phones-rename-refused-reached"
            | "context-menu"
            | "context-menu-edge"
            | "context-menu-multi"
            | "devices-menu-read-only"
            | "rename-editing"
            | "rename-clash"
            | "rename-invalid"
            | "rename-grid"
            | "rename-done"
            | "properties"
            | "properties-multi"
            | "copy-path"
            | "copy-path-xclip"
            | "copy-path-no-tool"
            | "rename-stranded"
            | "rename-stranded-dismissed"
            | "toast-stack"
            | "toast-stack-restored"
            | "toast-stack-two"
            | "toast-stack-more"
            | "toast-stack-expanded"
            | "toast-stack-short"
    )
}

/// The fake stick the `devices-*` scenes plug in: the fixture's SANDISK,
/// unmounted until the scene mounts it.
fn sandisk(root: &Path, view: &str) -> crate::devices::FakeDrive {
    let mut d = crate::devices::FakeDrive::stick(
        "sda1",
        "SANDISK",
        29_800_000_000,
        &root.join("media/sky/SANDISK"),
    );
    d.read_only = matches!(view, "devices-read-only" | "devices-menu-read-only");
    if view == "devices-fat-too-big" {
        d.drive.fs_type = "vfat".into();
    }
    d.busy = view == "devices-busy-other";
    // The statistics scene keeps the fixture's mount table, where SANDISK
    // is already mounted, so its DISKS card lists the drive.
    if view == "devices-stats" {
        d.drive.mount_points = vec![root.join("media/sky/SANDISK")];
    }
    d
}

/// Where the `phones-*` scenes' Pixel 8 shows up, as gvfsd-fuse would put it.
fn pixel_mount(root: &Path) -> PathBuf {
    root.join("run/user/1000/gvfs/mtp:host=Google_Pixel_8_37281FDJH00ABC")
}

/// The `phones-*` scenes' Pixel 8: locked in the waiting scenes, slow in
/// the loading one, with a scripted no-write folder, and (once it allows
/// access) a DCIM with photos on its internal storage.
fn pixel(root: &Path, view: &str) -> crate::devices::FakeDrive {
    let mut p = crate::devices::FakeDrive::phone("pixel", "Pixel 8", &pixel_mount(root));
    p.storages = vec!["Internal shared storage".into(), "SD card".into()];
    p.locked = matches!(
        view,
        "phones-waiting" | "phones-still-waiting" | "phones-allowed"
    );
    if view == "phones-loading" {
        p.list_delay = Some(std::time::Duration::from_millis(50));
    }
    let data = pixel_mount(root).join("Internal shared storage/Android/data");
    p.drive.no_write = vec![(data, "Pixel 8 doesn't allow changes in Android/data".into())];
    if view == "phones-rename-refused" {
        let dcim = pixel_mount(root).join("Internal shared storage/DCIM");
        p.drive.no_rename = vec![(dcim, "Pixel 8 doesn't allow renaming in DCIM".into())];
    }
    p
}

/// Photos and folders on the Pixel's internal storage, written once it
/// is mounted and allowed (a locked phone lists nothing).
fn seed_pixel(root: &Path, home: &Path) -> Result<PathBuf> {
    let storage = pixel_mount(root).join("Internal shared storage");
    let dcim = storage.join("DCIM");
    for d in [
        dcim.join("Camera"),
        dcim.join("Screenshots"),
        storage.join("Android/data"),
        storage.join("Download"),
    ] {
        std::fs::create_dir_all(&d).map_err(|e| Error::io(&d, e))?;
    }
    for n in [
        "PXL_20261118_101204.jpg",
        "PXL_20261118_101233.jpg",
        "PXL_20261119_174502.mp4",
    ] {
        let from = home.join("Pictures/dune-sunset-0412.jpg");
        let to = dcim.join(n);
        std::fs::copy(&from, &to).map_err(|e| Error::io(&to, e))?;
    }
    crate::fixture::freeze_times(&pixel_mount(root), FIXTURE_MTIME)?;
    Ok(dcim)
}

/// Play a `phones-*` scene against the fake backend.
fn phone_scene(
    ui: &AppWindow,
    view: &str,
    root: &Path,
    home: &Path,
    fake: &crate::devices::FakeDevices,
) -> Result<()> {
    use crate::ops::{Job, JobKind};
    let check = |what: &str, got: String, want: &str| {
        if got == want {
            Ok(())
        } else {
            Err(Error::Message(format!("{what}: {got:?}, want {want:?}")))
        }
    };
    let names = |n: &[&str]| n.iter().map(std::ffi::OsString::from).collect::<Vec<_>>();
    match view {
        "phones-plugged" => {
            fake.plug(pixel(root, view));
            app::devices_poll();
            app::land_reads();
            return check(
                "arrival toast",
                ui.get_toast_text().to_string(),
                "Pixel 8 connected",
            );
        }
        // Unmounted rows: the phone's and a camera's glyph, both muted.
        "phones-row" => return Ok(()),
        "phones-menu" => {
            ui.invoke_place_menu(device_row(ui, "Pixel 8")?, 150.0, 380.0);
            return Ok(());
        }
        "phones-camera-waiting" | "phones-camera-still-waiting" => {
            ui.invoke_place_open(1, device_row(ui, "Canon EOS R6")?);
            app::land_reads();
            check("camera state", ui.get_state().to_string(), "phone-waiting")?;
            check(
                "camera note",
                ui.get_state_note().to_string(),
                "Turn on Canon EOS R6 and set its USB mode to PTP",
            )?;
            if view == "phones-camera-still-waiting" {
                app::press_state("retry");
                app::land_reads();
                check(
                    "camera retry toast",
                    ui.get_toast_text().to_string(),
                    "Canon EOS R6 still isn't ready",
                )?;
            }
            return Ok(());
        }
        _ => {}
    }
    // Every other scene opens the phone with a click on its row.
    ui.invoke_place_open(1, device_row(ui, "Pixel 8")?);
    app::land_reads();
    let mount = pixel_mount(root);
    if matches!(
        view,
        "phones-waiting" | "phones-still-waiting" | "phones-allowed"
    ) {
        check("waiting state", ui.get_state().to_string(), "phone-waiting")?;
        check(
            "waiting note",
            ui.get_state_note().to_string(),
            app::PHONE_WAITING,
        )?;
        match view {
            "phones-still-waiting" => {
                app::press_state("retry");
                app::land_reads();
                check(
                    "retry toast",
                    ui.get_toast_text().to_string(),
                    &app::still_waiting_text("Pixel 8", crate::devices::Kind::Phone),
                )?;
            }
            // Allow tapped on the phone, then [Retry]: its storages list.
            "phones-allowed" => {
                fake.allow("pixel");
                crate::fixture::freeze_times(&mount, FIXTURE_MTIME)?;
                app::press_state("retry");
                app::land_reads();
                check("allowed", ui.get_state().to_string(), "files")?;
            }
            _ => {}
        }
        return Ok(());
    }
    let dcim = seed_pixel(root, home)?;
    let pictures = home.join("Pictures");
    let back_home = |ui: &AppWindow| {
        app::go(pictures.clone());
        app::land_reads();
        let _ = ui;
    };
    match view {
        "phones-storage" => {
            app::go(dcim.clone());
            app::land_reads();
            check(
                "storage free space",
                ui.get_status_right().to_string(),
                "12 GB free of 128 GB",
            )?;
        }
        // Made on a worker, then the reread lists it selected.
        // From an empty folder's [New folder]: made on a worker, then the
        // reread lists it selected.
        "phones-new-folder" => {
            let camera = dcim.join("Camera");
            app::go(camera.clone());
            app::land_reads();
            check("empty first", ui.get_state().to_string(), "empty")?;
            app::press_state("new-folder");
            app::land_reads();
            if !camera.join("New folder").is_dir() {
                return Err(Error::Message("New folder wasn't made on the phone".into()));
            }
            check("listed", ui.get_state().to_string(), "files")?;
            crate::fixture::freeze_times(&camera, FIXTURE_MTIME)?;
            app::go(camera.clone());
            app::land_reads();
            select_row(ui, "New folder");
        }
        // A slow listing: the pane shows it is reading while the UI keeps
        // running (the read is on its own thread in the app).
        "phones-loading" => {
            app::navigate_pending(dcim.clone(), std::time::Instant::now(), 0.45);
        }
        "phones-top-refused" => {
            app::run_job(Job::new(
                JobKind::Copy,
                &pictures,
                names(&["dune-sunset-0412.jpg"]),
                &mount,
            ));
            check(
                "refused toast",
                ui.get_toast_text().to_string(),
                "Files can only go inside a storage on Pixel 8",
            )?;
        }
        "phones-no-write" => {
            app::go(mount.join("Internal shared storage/Android/data"));
            app::land_reads();
            check(
                "no-write title",
                ui.get_state_title().to_string(),
                "Nothing can be added here",
            )?;
        }
        // A folder that refuses renames: the menu's Rename is greyed with
        // the reason in its tooltip.
        "phones-rename-refused" => {
            app::go(dcim.clone());
            app::land_reads();
            let i = row_index(ui, "PXL_20261118_101204.jpg")?;
            ui.invoke_row_menu(i, 330.0, 170.0);
            let line = menu_line(ui, "Rename")?;
            let item = slint::Model::row_data(&ui.get_file_menu_items(), line as usize)
                .ok_or_else(|| Error::Message("no Rename line".into()))?;
            check("rename greyed", item.off.to_string(), "true")?;
            check(
                "rename tooltip",
                item.tip.to_string(),
                "Pixel 8 doesn't allow renaming in DCIM",
            )?;
            ui.set_menu_tip_line(line);
        }
        // A folder that refuses only when asked: the phone's answer shows
        // under the field, and the photo keeps its name.
        "phones-rename-refused-reached" => {
            app::go(dcim.clone());
            app::land_reads();
            select_row(ui, "PXL_20261118_101204.jpg");
            ui.invoke_key(app::keys::F2.into(), false, false, false);
            ui.show().map_err(|e| Error::Message(e.to_string()))?;
            app::refuse_renames(true);
            type_field(ui, "beach");
            type_field(ui, "\n");
            app::refuse_renames(false);
            check(
                "refused inline",
                ui.get_rename_error().to_string(),
                "Pixel 8 doesn't allow renaming here",
            )?;
            if !dcim.join("PXL_20261118_101204.jpg").exists() {
                return Err(Error::Message("the photo lost its name".into()));
            }
        }
        "phones-no-trash" => {
            app::go(dcim.clone());
            app::land_reads();
            select_row(ui, "PXL_20261118_101204.jpg");
            ui.invoke_key(app::keys::DELETE.into(), false, false, false);
            check(
                "trash title",
                ui.get_trash_no_title().to_string(),
                "This phone has no trash",
            )?;
            check(
                "trash note",
                ui.get_trash_no_note().to_string(),
                "Files deleted on Pixel 8 can't be recovered",
            )?;
        }
        "phones-copying" => {
            app::go(dcim.clone());
            app::land_reads();
            app::hold_job(Job::new(
                JobKind::Copy,
                &pictures,
                names(&[
                    "dune-sunset-0412.jpg",
                    "dune-sunset-0413.jpg",
                    "speeder-run.mp4",
                ]),
                &dcim,
            ));
        }
        // Through the op queue for real: the files land under their own
        // names, with no partial file left on the phone.
        "phones-copied" => {
            app::go(dcim.clone());
            app::land_reads();
            app::run_job(Job::new(
                JobKind::Copy,
                &pictures,
                names(&["dune-sunset-0412.jpg", "dune-sunset-0413.jpg"]),
                &dcim,
            ));
            app::land_reads();
            for n in ["dune-sunset-0412.jpg", "dune-sunset-0413.jpg"] {
                if !dcim.join(n).is_file() {
                    return Err(Error::Message(format!("{n} didn't land on the phone")));
                }
            }
            let partial = std::fs::read_dir(&dcim)
                .map_err(|e| Error::io(&dcim, e))?
                .flatten()
                .any(|e| {
                    e.file_name()
                        .to_string_lossy()
                        .starts_with(".robin-partial")
                });
            if partial {
                return Err(Error::Message(
                    "a partial file was left on the phone".into(),
                ));
            }
            // A phone keeps no source times, so pin them for a stable frame.
            crate::fixture::freeze_times(&dcim, FIXTURE_MTIME)?;
            app::go(dcim.clone());
            app::land_reads();
        }
        "phones-busy-copy" => {
            back_home(ui);
            app::hold_job(Job::new(
                JobKind::Copy,
                &pictures,
                names(&[
                    "dune-sunset-0412.jpg",
                    "dune-sunset-0413.jpg",
                    "speeder-run.mp4",
                ]),
                &dcim,
            ));
            ui.invoke_place_eject(device_row(ui, "Pixel 8")?);
            check(
                "busy toast",
                ui.get_toast_text().to_string(),
                "Copying 3 items to Pixel 8",
            )?;
        }
        "phones-safe-unplug" => {
            ui.invoke_place_eject(device_row(ui, "Pixel 8")?);
            app::land_reads();
            check(
                "unplug toast",
                ui.get_toast_text().to_string(),
                "Pixel 8 can be unplugged",
            )?;
            if device_names(ui).iter().any(|n| n == "Pixel 8") {
                return Err(Error::Message("the phone's row stayed".into()));
            }
            if fake
                .log
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .iter()
                .any(|l| l.starts_with("power-off"))
            {
                return Err(Error::Message("a phone was powered off".into()));
            }
        }
        "phones-removed" => {
            app::go(dcim.clone());
            app::land_reads();
            fake.unplug("pixel");
            app::devices_poll();
            app::land_reads();
            check(
                "removed title",
                ui.get_state_title().to_string(),
                "Pixel 8 was removed",
            )?;
        }
        "phones-removed-elsewhere" => {
            back_home(ui);
            fake.unplug("pixel");
            app::devices_poll();
            app::land_reads();
            check(
                "removed toast",
                ui.get_toast_text().to_string(),
                "Pixel 8 was removed without being ejected",
            )?;
        }
        _ => {}
    }
    Ok(())
}

/// The DEVICES row named `name`, as a click would find it.
fn device_row(ui: &AppWindow, name: &str) -> Result<i32> {
    use slint::Model;
    let rows = ui.get_device_places();
    (0..rows.row_count())
        .find(|&i| rows.row_data(i).is_some_and(|p| p.name == name))
        .map(|i| i as i32)
        .ok_or_else(|| Error::Message(format!("no DEVICES row named {name}")))
}

/// The DEVICES rows' names, top to bottom.
fn device_names(ui: &AppWindow) -> Vec<String> {
    use slint::Model;
    ui.get_device_places()
        .iter()
        .map(|p| p.name.to_string())
        .collect()
}

/// Plug a second partition, BACKUP, onto the SANDISK stick and open it.
fn plug_backup(ui: &AppWindow, root: &Path, fake: &crate::devices::FakeDevices) -> Result<()> {
    let mut b = crate::devices::FakeDrive::stick(
        "sda2",
        "BACKUP",
        32_000_000_000,
        &root.join("media/sky/BACKUP"),
    );
    b.drive.drive = "sda1-drive".into();
    fake.plug(b);
    app::devices_poll();
    app::land_reads();
    ui.invoke_place_open(1, device_row(ui, "BACKUP")?);
    app::land_reads();
    Ok(())
}

/// Play a `devices-*` scene against the fake backend.
fn device_scene(
    ui: &AppWindow,
    view: &str,
    root: &Path,
    home: &Path,
    fake: &crate::devices::FakeDevices,
) -> Result<()> {
    use crate::ops::{Job, JobKind};
    let stick = root.join("media/sky/SANDISK");
    // Every scene but the arrival starts with the stick already in, so
    // it gets no toast (it was there at startup), then mounts it with a
    // click on its row.
    if view == "devices-plugged" {
        fake.plug(sandisk(root, view));
        app::devices_poll();
        app::land_reads();
        return Ok(());
    }
    // The pointer on the unmounted row's eject button: "Power off".
    if view == "devices-power-off" {
        hover_at(ui, 209.0, 366.0);
        return Ok(());
    }
    let mount = |ui: &AppWindow| -> Result<()> {
        ui.invoke_place_open(1, device_row(ui, "SANDISK")?);
        app::land_reads();
        Ok(())
    };
    if view == "devices-menu-read-only" {
        std::fs::write(stick.join("trip-notes.txt"), b"notes\n")
            .map_err(|e| Error::Message(e.to_string()))?;
    }
    mount(ui)?;
    let back_home = |ui: &AppWindow| {
        use slint::Model;
        let pinned = ui.get_pinned_places();
        let at = (0..pinned.row_count())
            .find(|&i| pinned.row_data(i).is_some_and(|p| p.name == "Pictures"))
            .unwrap_or(0);
        ui.invoke_place_open(0, at as i32);
        app::land_reads();
    };
    let names = |n: &[&str]| n.iter().map(std::ffi::OsString::from).collect::<Vec<_>>();
    match view {
        "devices-busy-copy" => {
            back_home(ui);
            app::hold_job(Job::new(
                JobKind::Copy,
                &home.join("Pictures"),
                names(&[
                    "dune-sunset-0412.jpg",
                    "dune-sunset-0413.jpg",
                    "speeder-run.mp4",
                ]),
                &stick,
            ));
            ui.invoke_place_eject(device_row(ui, "SANDISK")?);
        }
        // A second partition on the same stick: a copy onto it blocks
        // ejecting SANDISK, and the toast names BACKUP.
        "devices-partition-busy" => {
            plug_backup(ui, root, fake)?;
            back_home(ui);
            app::hold_job(Job::new(
                JobKind::Copy,
                &home.join("Pictures"),
                names(&[
                    "dune-sunset-0412.jpg",
                    "dune-sunset-0413.jpg",
                    "speeder-run.mp4",
                ]),
                &root.join("media/sky/BACKUP"),
            ));
            ui.invoke_place_eject(device_row(ui, "SANDISK")?);
        }
        // Ejecting SANDISK's row takes the whole stick: both partitions
        // unmount, it powers off, and both rows go in the same redraw.
        // The scene checks that itself, so the render test fails if not.
        // The stick pulled out while Pictures is open: both partitions go
        // and one toast names them. Checked here, so the render test fails
        // on two toasts (the second would replace the first's text).
        "devices-partitions-removed-elsewhere" => {
            plug_backup(ui, root, fake)?;
            back_home(ui);
            fake.unplug("sda1");
            fake.unplug("sda2");
            app::devices_poll();
            app::land_reads();
            let toast = ui.get_toast_text().to_string();
            if toast != "SANDISK and BACKUP were removed without being ejected" {
                return Err(Error::Message(format!("removed toast: {toast:?}")));
            }
            let left = device_names(ui);
            if left.iter().any(|n| n == "SANDISK" || n == "BACKUP") {
                return Err(Error::Message(format!("rows left behind: {left:?}")));
            }
        }
        "devices-partition-unplug" => {
            plug_backup(ui, root, fake)?;
            back_home(ui);
            ui.invoke_place_eject(device_row(ui, "SANDISK")?);
            // Before the side job lands both rows still show; never one.
            let ours = |ui: &AppWindow| {
                device_names(ui)
                    .iter()
                    .filter(|n| *n == "SANDISK" || *n == "BACKUP")
                    .count()
            };
            if ours(ui) == 1 {
                return Err(Error::Message("one partition's row went alone".into()));
            }
            app::land_reads();
            let toast = ui.get_toast_text().to_string();
            if toast != "SANDISK and BACKUP can be unplugged" {
                return Err(Error::Message(format!("unplug toast: {toast:?}")));
            }
            let left = device_names(ui);
            if left.iter().any(|n| n == "SANDISK" || n == "BACKUP") {
                return Err(Error::Message(format!("rows left behind: {left:?}")));
            }
            if !crate::devices::DeviceBackend::drives(fake)
                .unwrap_or_default()
                .is_empty()
            {
                return Err(Error::Message("the stick is still powered".into()));
            }
        }
        "devices-busy-other" => {
            back_home(ui);
            *fake.holder.lock().unwrap_or_else(|e| e.into_inner()) = Some("vlc".into());
            ui.invoke_place_eject(device_row(ui, "SANDISK")?);
        }
        "devices-safe-unplug" => {
            ui.invoke_place_eject(device_row(ui, "SANDISK")?);
            app::land_reads();
        }
        "devices-removed-elsewhere" => {
            back_home(ui);
            fake.unplug("sda1");
            app::devices_poll();
            app::land_reads();
        }
        "devices-removed" => {
            fake.unplug("sda1");
            app::devices_poll();
            app::land_reads();
        }
        "devices-unmounted" => {
            fake.unmount_elsewhere("sda1");
            app::devices_poll();
            app::land_reads();
        }
        // A read-only stick: everything that would change it is greyed.
        "devices-menu-read-only" => {
            let i = row_index(ui, "trip-notes.txt")?;
            ui.invoke_row_menu(i, 330.0, 150.0);
            ui.set_menu_tip_line(menu_line(ui, "Rename")?);
        }
        "devices-menu" => {
            back_home(ui);
            // Just under the row, where a right-click on it opens the menu.
            ui.invoke_place_menu(device_row(ui, "SANDISK")?, 150.0, 380.0);
        }
        "devices-stats" => {
            ui.invoke_stats_open();
            app::land_reads();
        }
        "devices-no-space" | "devices-fat-too-big" => {
            back_home(ui);
            // Sparse: GBs on paper, nothing on disk. The fixture leaves
            // SANDISK 6 GB free. FAT32 refuses two of these for size, so
            // its details name both and say why.
            let src = home.join("Downloads");
            let files: &[(&str, u64)] = if view == "devices-fat-too-big" {
                &[
                    ("holo-archive.img", 7_000_000_000),
                    ("speeder-run-4k.mkv", 4_600_000_000),
                    ("notes.txt", 2_000),
                ]
            } else {
                &[("holo-archive.img", 7_000_000_000)]
            };
            for (n, len) in files {
                let p = src.join(n);
                let f = std::fs::File::create(&p).map_err(|e| Error::io(&p, e))?;
                f.set_len(*len).map_err(|e| Error::io(&p, e))?;
            }
            let picked: Vec<&str> = files.iter().map(|(n, _)| *n).collect();
            app::run_job(Job::new(JobKind::Copy, &src, names(&picked), &stick));
            app::land_reads();
            // Open the details, as a click on the link would.
            ui.invoke_toast_link();
            for (n, _) in files {
                let _ = std::fs::remove_file(src.join(n));
            }
        }
        _ => {}
    }
    Ok(())
}

pub struct Options<'a> {
    pub view: &'a str,
    pub theme: Theme,
    pub size: (u32, u32),
    pub compact: bool,
    /// The fixture home; a fixed temp folder when not given.
    pub fixture: Option<PathBuf>,
    pub out: &'a Path,
}

/// Where a render seeds its home when `--fixture` isn't given: private to
/// this user, under `$XDG_RUNTIME_DIR` or `/tmp/robin-render-$UID`.
pub fn default_home() -> Result<PathBuf> {
    match std::env::var_os("XDG_RUNTIME_DIR").filter(|v| !v.is_empty()) {
        Some(run) => Ok(PathBuf::from(run).join("robin-render")),
        None => {
            let uid = rustix::process::getuid().as_raw();
            let dir = std::env::temp_dir().join(format!("robin-render-{uid}"));
            check_owned(&dir, uid)?;
            Ok(dir)
        }
    }
}

/// /tmp is shared: a name there could be another user's folder or a link
/// planted to redirect the render. Accept only a missing path or a real
/// folder this user owns.
pub fn check_owned(dir: &Path, uid: u32) -> Result<()> {
    use std::os::unix::fs::MetadataExt;
    match dir.symlink_metadata() {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(Error::io(dir, e)),
        Ok(m) if m.file_type().is_symlink() => Err(Error::Message(format!(
            "{} is a symlink; refusing to render there",
            dir.display()
        ))),
        Ok(m) if !m.is_dir() || m.uid() != uid => Err(Error::Message(format!(
            "{} is not a folder owned by this user; refusing to render there",
            dir.display()
        ))),
        Ok(_) => Ok(()),
    }
}

/// Refuse to seed a folder that has things in it and isn't a fixture, so
/// `--fixture ~/Pictures` can never write into a real folder.
pub fn check_fixture(root: &Path) -> Result<()> {
    let marked = crate::fixture::is_marked(root);
    let empty = match std::fs::read_dir(root) {
        Ok(mut entries) => entries.next().is_none(),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => true,
        Err(e) => return Err(Error::io(root, e)),
    };
    if marked || empty {
        Ok(())
    } else {
        Err(Error::Message(format!(
            "{} has files and no .robin-fixture marker; refusing to seed a real folder",
            root.display()
        )))
    }
}

/// The fixture home to use: seeded (again) and with frozen times.
pub fn seed(root: &Path) -> Result<()> {
    check_fixture(root)?;
    crate::fixture::materialize(root)?;
    crate::fixture::freeze_times(root, FIXTURE_MTIME)
}

pub fn paint(opts: Options<'_>) -> Result<()> {
    if let Some(dir) = &opts.fixture {
        check_fixture(dir)?;
    }
    std::env::set_var("SLINT_SCALE_FACTOR", "1");
    let _ = slint::BackendSelector::new()
        .renderer_name("software".into())
        .select();
    let _ = slint::set_xdg_app_id("dev.skyguy.robin");
    // A home this render made is removed after; a given --fixture is kept.
    let made = opts.fixture.is_none();
    let home = match opts.fixture {
        Some(dir) => dir,
        None => default_home()?,
    };
    let result = seed(&home).and_then(|()| {
        paint_in(
            opts.view,
            opts.theme,
            opts.size,
            opts.compact,
            &home,
            opts.out,
        )
    });
    if made {
        let _ = crate::fixture::remove(&home);
    }
    result
}

fn paint_in(
    view: &str,
    theme: Theme,
    size: (u32, u32),
    compact: bool,
    home: &Path,
    out: &Path,
) -> Result<()> {
    let root = home;
    let home = &crate::fixture::home(root);
    let start = match view {
        "start-denied" => home.join("secret"),
        "view-video" | "view-scrub" | "view-resume" => home.join("Videos"),
        "view-audio" => home.join("Music"),
        _ => home.join("Pictures"),
    };
    if view.starts_with("view-") {
        // The fixture's stills are 1px placeholders; the viewer needs
        // real pictures to show.
        crate::thumbs::write_fixture_stills(&home.join("Pictures"))?;
        if view == "view-strip-video" {
            seed_strip_videos(home)?;
        }
        seed_media(view, home)?;
        crate::fixture::freeze_times(root, FIXTURE_MTIME)?;
    }
    let mut mounts = crate::mounts::FixtureMounts::load(&crate::fixture::mounts_dir(root))?;
    // What gvfs says of the Pixel's internal storage (statfs per storage).
    if view.starts_with("phones-") {
        mounts = mounts.with_space(
            &pixel_mount(root).join("Internal shared storage"),
            128_000_000_000,
            12_000_000_000,
        );
    }
    // The device scenes plug SANDISK in through the fake backend instead.
    let fake = crate::devices::FakeDevices::new();
    let devices = view.starts_with("devices-").then(|| {
        if view != "devices-plugged" {
            fake.plug(sandisk(root, view));
        }
        fake.clone() as Arc<dyn crate::devices::DeviceBackend>
    });
    // The phone scenes plug the Pixel 8 (and, for the rows, a camera).
    let devices = devices.or_else(|| {
        view.starts_with("phones-").then(|| {
            if view != "phones-plugged" {
                fake.plug(pixel(root, view));
            }
            if matches!(
                view,
                "phones-row" | "phones-camera-waiting" | "phones-camera-still-waiting"
            ) {
                let mut cam = crate::devices::FakeDrive::camera(
                    "eos",
                    "Canon EOS R6",
                    &root.join("run/user/1000/gvfs/gphoto2:host=Canon_Inc._Canon_Digital_Camera"),
                );
                // Off, or in mass-storage mode: it lists nothing yet.
                cam.locked = view != "phones-row";
                fake.plug(cam);
            }
            fake.clone() as Arc<dyn crate::devices::DeviceBackend>
        })
    });
    if devices.is_some() && view != "devices-stats" {
        mounts = mounts.without("/dev/sda1");
    }
    if view.starts_with("stats-") {
        seed_cache(home)?;
    }
    let mut env = crate::ops::Env::for_home(home);
    if view == "trash-empty" {
        env.home_trash = crate::fixture::empty_trash(root);
    }
    let prefs = if view == "prefs-comfortable" {
        let path = root.join("config/robin/view");
        std::fs::create_dir_all(path.parent().unwrap())
            .map_err(|e| crate::Error::io(path.clone(), e))?;
        std::fs::write(&path, "view = list\ntile = medium\ndensity = compact\n")
            .map_err(|e| crate::Error::io(path.clone(), e))?;
        Some(path)
    } else {
        None
    };
    let ui = app::build_scripted(Launch {
        dir: start,
        env,
        theme,
        compact,
        clock: Some((FROZEN_NOW, FROZEN_OFFSET)),
        mounts: Arc::new(mounts),
        prefs,
        devices,
    })?;
    // A view file left by a build that still had the density toggle:
    // the app stays comfortable whatever it says.
    if view == "prefs-comfortable" && ui.get_compact() {
        return Err(crate::Error::Message(
            "prefs-comfortable: a saved density made the app compact".into(),
        ));
    }
    // Size the window before any keys: the viewer fits and zooms to it.
    ui.window()
        .set_size(slint::PhysicalSize::new(size.0, size.1));
    app::land_reads();
    if view == "nav-pending" {
        // A slow navigation into raw-exports whose read hasn't landed.
        let t0 = std::time::Instant::now();
        app::navigate_pending(home.join("Pictures/raw-exports"), t0, 0.45);
    }
    // Click a sidebar row as a person would, then let its reads land.
    let click = match view {
        "sidebar-device" => Some((1, 2)), // SANDISK
        "sidebar-trash"
        | "trash-view"
        | "trash-empty-confirm"
        | "trash-empty"
        | "trash-noinfo"
        | "trash-restore-recreated" => Some((2, 0)),
        _ => None,
    };
    if let Some((section, index)) = click {
        ui.invoke_place_open(section, index);
        app::land_reads();
    }
    match view {
        // Pick a restorable item, as a click would.
        "trash-view" => select_row(&ui, "q3-budget.ods"),
        // The toolbar's up, back and forward buttons, clicked as a person
        // would: up to home, back to Pictures, forward to home, back
        // again. Ends in Pictures with back dimmed and forward lit.
        "nav-history" => {
            let steps = [
                ("", "~  ›  Pictures", false, false),
                ("up", "~", true, false),
                ("back", "~  ›  Pictures", false, true),
                ("forward", "~", true, false),
                ("back", "~  ›  Pictures", false, true),
            ];
            for (click, crumbs, back, forward) in steps {
                match click {
                    "up" => ui.invoke_nav_up(),
                    "back" => ui.invoke_nav_back(),
                    "forward" => ui.invoke_nav_forward(),
                    _ => {}
                }
                app::land_reads();
                let got = (
                    ui.get_crumbs().to_string(),
                    ui.get_can_back(),
                    ui.get_can_forward(),
                );
                if got != (crumbs.to_string(), back, forward) {
                    return Err(crate::Error::Message(format!(
                        "nav-history: wanted {crumbs:?} back={back} forward={forward}, got {got:?}"
                    )));
                }
            }
        }
        // The grid button, then a tile click.
        "grid-view" => {
            ui.invoke_set_grid(true);
            select_row(&ui, "dune-sunset-0413.jpg");
            // The grid must not keep the list's model alive (100k-entry
            // folders): fail the render, and the real_render test, if it does.
            use slint::Model;
            let (files, lines) = (ui.get_files().row_count(), ui.get_grid_rows().row_count());
            if files != 0 || lines == 0 {
                return Err(crate::Error::Message(format!(
                    "grid mode: {files} list rows, {lines} grid lines"
                )));
            }
        }
        // Small and large tiles, stepped from medium with Ctrl+- and
        // Ctrl+= as a person would.
        "grid-view-s" | "grid-view-l" => {
            ui.invoke_set_grid(true);
            let step = if view == "grid-view-s" { "-" } else { "=" };
            ui.invoke_key(step.into(), true, false, false);
            select_row(&ui, "dune-sunset-0413.jpg");
        }
        // Grid with hidden files shown, from the status bar toggle.
        "grid-hidden" => {
            ui.invoke_set_grid(true);
            ui.invoke_toggle_hidden();
        }
        // Slash opens the search; typed keys narrow the listing.
        "search-results" => type_keys(&ui, "/dune"),
        "search-nosearch" => type_keys(&ui, "/kotlin"),
        // Not contiguous anywhere: only the d and the j light up.
        "search-letters" => type_keys(&ui, "/dj"),
        // Return on a photo opens it; the scene's keys then act in the viewer.
        // The decode is held: the stand-in box shows.
        "view-loading" => {
            select_row(&ui, "dune-sunset-0412.jpg");
            type_keys(&ui, "\n");
        }
        "view-image" | "view-zoom" | "view-info" | "view-turn" | "view-pan" | "view-wake"
        | "view-strip-video" => {
            select_row(&ui, "dune-sunset-0412.jpg");
            type_keys(&ui, "\n");
            // Land the decode (and tiles) before zooming: zoom steps
            // need the picture's size.
            app::land_reads();
            match view {
                "view-zoom" => {
                    type_keys(&ui, "l");
                    app::land_reads();
                    type_keys(&ui, "++++");
                }
                // Zoomed past fit, the arrows pan: down twice, right once.
                "view-pan" => {
                    type_keys(&ui, "++++++");
                    for k in [app::keys::DOWN, app::keys::DOWN, app::keys::RIGHT] {
                        type_keys(&ui, k);
                    }
                }
                // Zoomed (chrome hidden), then the mouse moves.
                "view-wake" => {
                    type_keys(&ui, "++++++");
                    ui.invoke_view_wake();
                }
                "view-info" => type_keys(&ui, "i"),
                "view-turn" => type_keys(&ui, "r"),
                _ => {}
            }
            // Turns are made on a worker too.
            app::land_reads();
        }
        // Return on the clip plays it in the viewer; the script pauses it
        // at 01:12 of 04:30 so the frame and the clock are fixed.
        "view-video" | "view-scrub" | "view-resume" => {
            select_row(&ui, "speeder-run.mp4");
            type_keys(&ui, "\n");
            app::land_reads();
            app::land_playback(|t| t.dur > 0.0);
            if view != "view-resume" {
                app::script_transport("toggle", 0.0);
                app::script_transport("seek", (72.0 / CLIP_SECS) as f32);
                app::land_playback(|t| t.paused && (t.pos - 72.0).abs() < 1.0);
            }
            if view == "view-scrub" {
                hover_scrub(&ui, size, 0.5);
            }
        }
        // An audio file plays with a music glyph and the same transport.
        "view-audio" => {
            select_row(&ui, "cantina-band.wav");
            type_keys(&ui, "\n");
            app::land_reads();
            app::land_playback(|t| t.dur > 0.0);
            app::script_transport("toggle", 0.0);
            app::script_transport("seek", (24.5 / 96.0) as f32);
            app::land_playback(|t| t.paused && t.pos > 24.0);
        }
        "trash-empty-confirm" => ui.invoke_trash_empty(),
        // The sidebar's Statistics row; the scan lands like any read.
        "stats-real" => {
            ui.invoke_stats_open();
            app::land_reads();
        }
        // A rescan in flight over the last results, dimmed; a second open
        // reuses it.
        "stats-scanning" => {
            ui.invoke_stats_open();
            app::land_reads();
            ui.invoke_stats_rescan();
            ui.invoke_stats_open();
            ui.set_stat_scan_count("12 480".into());
        }
        // The first scan, nothing to show yet.
        "stats-scanning-first" => {
            ui.invoke_stats_open();
            ui.set_stat_scan_count("3 211".into());
        }
        // Select the item with no trashinfo; the banner explains it.
        "trash-noinfo" => select_row(&ui, "mystery.bin"),
        // Restore into ~/Documents/Finance, which no longer exists.
        "trash-restore-recreated" => {
            select_row(&ui, "q3-budget.ods");
            ui.invoke_trash_restore();
            app::land_reads();
        }
        v if v.starts_with("rename-stranded") || v.starts_with("toast-stack") => {
            stranded_scene(&ui, v)?
        }
        v if v.starts_with("context-menu")
            || v.starts_with("rename-")
            || v.starts_with("properties")
            || v.starts_with("copy-path") =>
        {
            file_scene(&ui, v, size)?
        }
        v if v.starts_with("devices-") => device_scene(&ui, v, root, home, &fake)?,
        v if v.starts_with("phones-") => phone_scene(&ui, v, root, home, &fake)?,
        _ => {}
    }
    snapshot(&ui, size, out)
}

/// Length of the fixture's speeder-run clip, as in the mock frames.
const CLIP_SECS: f64 = 270.0;

/// Real media for the player scenes: a cantina-band tone, and with
/// libmpv installed a 04:30 speeder-run clip (YUV4MPEG2, one frame per
/// 3s) in place of the fixture's stand-in bytes, plus for view-resume a
/// saved position at 02:14. Without libmpv the viewer explains.
fn seed_media(view: &str, home: &Path) -> Result<()> {
    if !matches!(
        view,
        "view-video" | "view-scrub" | "view-resume" | "view-audio"
    ) {
        return Ok(());
    }
    let io = |p: &Path, e| crate::Error::io(p, e);
    let tone = home.join("Music/cantina-band.wav");
    if let Some(dir) = tone.parent() {
        std::fs::create_dir_all(dir).map_err(|e| io(dir, e))?;
    }
    crate::mpv::write_wav(&tone, 96, 330.0).map_err(|e| io(&tone, e))?;
    // The clip is 11 MB: only worth writing when it can be played.
    if !crate::mpv::available() {
        return Ok(());
    }
    let clip = home.join("Videos/speeder-run.mp4");
    let frames = crate::thumbs::fixture_clip_frames((CLIP_SECS / 3.0) as usize, 384, 216);
    let frames: Vec<&[u8]> = frames.iter().map(Vec::as_slice).collect();
    crate::mpv::write_y4m(&clip, 384, 216, &frames, (1, 3)).map_err(|e| io(&clip, e))?;
    if view == "view-resume" {
        let env = crate::ops::Env::for_home(home);
        let mut saved =
            crate::player::Positions::load(crate::player::Positions::path(env.cache_dir()));
        saved.set(&clip, 134.0, CLIP_SECS);
    }
    Ok(())
}

/// Move the pointer to `(x, y)` in the window, for hover states.
pub fn hover_at(ui: &AppWindow, x: f32, y: f32) {
    ui.window()
        .dispatch_event(slint::platform::WindowEvent::PointerMoved {
            position: slint::LogicalPosition::new(x, y),
        });
}

/// Move the pointer over the transport's scrub track at `at` (0..1), so
/// the track lifts and the hover time shows.
fn hover_scrub(ui: &AppWindow, size: (u32, u32), at: f32) {
    // The transport is window − 120 wide and centred; the track starts
    // 156 in and leaves 196 for the buttons on the right.
    let (w, h) = (size.0 as f32, size.1 as f32);
    let bar = (w - 120.0).max(320.0);
    let left = (w - bar) / 2.0;
    let track_w = (bar - 156.0 - 196.0).max(40.0);
    let x = left + 156.0 + track_w * at;
    let y = h - 16.0 - 23.0;
    ui.window()
        .dispatch_event(slint::platform::WindowEvent::PointerMoved {
            position: slint::LogicalPosition::new(x, y),
        });
}

/// Two videos beside the dune photos: one with a frame already in the
/// freedesktop thumbnail cache, one without (the filmstrip's icon case).
fn seed_strip_videos(home: &Path) -> Result<()> {
    let pics = home.join("Pictures");
    let io = |p: &Path, e| crate::Error::io(p, e);
    let cached = pics.join("dune-flyover.mp4");
    std::fs::write(&cached, b"video").map_err(|e| io(&cached, e))?;
    let bare = pics.join("dune-timelapse.mp4");
    std::fs::write(&bare, b"video").map_err(|e| io(&bare, e))?;
    // Stamped with the time freeze_times gives the video, so it's fresh.
    let src = pics.join("speeder-frame.png");
    let frame = image::open(&src)
        .map_err(|e| crate::Error::Message(format!("{}: {e}", src.display())))?
        .to_rgba8();
    crate::thumbs::store_rgba(
        &crate::thumbs::ThumbRequest {
            source: &cached,
            cache_home: &home.join(".cache"),
            size: crate::thumbs::ThumbSize::Normal,
            mtime: std::time::UNIX_EPOCH + std::time::Duration::from_secs(FIXTURE_MTIME as u64),
        },
        frame.as_raw(),
        frame.width(),
        frame.height(),
    )?;
    Ok(())
}

/// A thumbnail cache and an app cache, so the Cache card has parts to name.
fn seed_cache(home: &Path) -> Result<()> {
    for (rel, len) in [
        (".cache/thumbnails/normal/0a1b.png", 48_000),
        (".cache/thumbnails/large/0a1b.png", 160_000),
        (".cache/robin/state.bin", 12_000),
        (".cache/fontconfig/cache-9", 90_000),
    ] {
        let path = home.join(rel);
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir).map_err(|e| Error::io(dir, e))?;
        }
        std::fs::write(&path, vec![0u8; len]).map_err(|e| Error::io(&path, e))?;
    }
    Ok(())
}

/// Click the row named `name`, as a person would.
fn select_row(ui: &AppWindow, name: &str) {
    use slint::Model;
    // The grid leaves the list model empty, so look in its lines too.
    let rows = ui.get_files();
    let in_list = (0..rows.row_count()).find(|&i| rows.row_data(i).is_some_and(|r| r.name == name));
    let lines = ui.get_grid_rows();
    let in_grid = || {
        lines.iter().find_map(|l| {
            [&l.n0, &l.n1, &l.n2, &l.n3, &l.n4, &l.n5]
                .iter()
                .position(|n| n.as_str() == name)
                .map(|k| l.first as usize + k)
        })
    };
    let i = in_list
        .or_else(in_grid)
        .unwrap_or_else(|| panic!("no row {name}")) as i32;
    ui.invoke_row_pointer(i, 0, 0, false, false, false);
    ui.invoke_row_pointer(i, 2, 0, false, false, false);
}

/// Size, show and capture a window to `out`.
pub fn snapshot(ui: &AppWindow, size: (u32, u32), out: &Path) -> Result<()> {
    ui.window()
        .set_size(slint::PhysicalSize::new(size.0, size.1));
    ui.show().map_err(|e| Error::Message(e.to_string()))?;
    let buffer = ui
        .window()
        .take_snapshot()
        .map_err(|e| Error::Message(e.to_string()))?;
    if let Some(parent) = out.parent() {
        if !parent.as_os_str().is_empty() {
            std::fs::create_dir_all(parent).map_err(|e| Error::io(parent, e))?;
        }
    }
    image::save_buffer(
        out,
        buffer.as_bytes(),
        buffer.width(),
        buffer.height(),
        image::ExtendedColorType::Rgba8,
    )
    .map_err(|e| Error::Message(format!("write {}: {e}", out.display())))?;
    Ok(())
}

/// The file menu, inline rename and Properties in Pictures.
fn file_scene(ui: &AppWindow, view: &str, size: (u32, u32)) -> Result<()> {
    let check = |what: &str, got: String, want: &str| {
        if got == want {
            Ok(())
        } else {
            Err(Error::Message(format!("{what}: {got:?}, want {want:?}")))
        }
    };
    let photo = "dune-sunset-0412.jpg";
    match view {
        "context-menu" => {
            let i = row_index(ui, photo)?;
            ui.invoke_row_menu(i, 330.0, 150.0);
            check("overlay", ui.get_overlay().to_string(), "menu-file")?;
        }
        // Near the bottom-right corner the menu opens up and to the left
        // of the pointer instead of running off the window.
        "context-menu-edge" => {
            let i = row_index(ui, "r2-portrait-001.png")?;
            let (w, h) = (size.0 as f32, size.1 as f32);
            ui.invoke_row_menu(i, w - 24.0, h - 40.0);
        }
        // Two items: Open with… and Rename are greyed, Rename's tooltip
        // pinned as a hover would show it.
        "context-menu-multi" => {
            select_row(ui, photo);
            let j = row_index(ui, "dune-sunset-0413.jpg")?;
            ui.invoke_row_pointer(j, 0, 0, false, true, false);
            ui.invoke_row_pointer(j, 2, 0, false, true, false);
            ui.invoke_row_menu(j, 330.0, 150.0);
            let line = menu_line(ui, "Rename")?;
            ui.set_menu_tip_line(line);
        }
        "rename-editing" | "rename-clash" | "rename-invalid" | "rename-done" => {
            select_row(ui, photo);
            ui.invoke_key(app::keys::F2.into(), false, false, false);
            check("field", ui.get_rename_text().to_string(), photo)?;
            check("stem", ui.get_rename_stem().to_string(), "16")?;
            // The field has the keyboard: type over the selected stem.
            ui.show().map_err(|e| Error::Message(e.to_string()))?;
            match view {
                "rename-clash" => {
                    type_field(ui, "dune-sunset-0413");
                    check(
                        "clash",
                        ui.get_rename_error().to_string(),
                        "A file named dune-sunset-0413.jpg already exists",
                    )?;
                    type_field(ui, "\n");
                    check(
                        "still editing",
                        (ui.get_rename_at() >= 0).to_string(),
                        "true",
                    )?;
                }
                "rename-invalid" => {
                    type_field(ui, "dune/sunset");
                    check(
                        "invalid",
                        ui.get_rename_error().to_string(),
                        "Names can't contain \"/\"",
                    )?;
                }
                "rename-done" => {
                    type_field(ui, "anchorhead-dusk");
                    type_field(ui, "\n");
                    app::land_reads();
                    check("closed", ui.get_rename_at().to_string(), "-1")?;
                    use slint::Model;
                    let picked: Vec<String> = ui
                        .get_files()
                        .iter()
                        .filter(|r| r.selected)
                        .map(|r| r.name.to_string())
                        .collect();
                    check("selected", picked.join(","), "anchorhead-dusk.jpg")?;
                }
                _ => {}
            }
        }
        "rename-grid" => {
            ui.invoke_set_grid(true);
            select_row(ui, photo);
            ui.invoke_key(app::keys::F2.into(), false, false, false);
            ui.show().map_err(|e| Error::Message(e.to_string()))?;
            type_field(ui, "dune-sunset-0413");
        }
        // Copy path against a stand-in clipboard: wl-copy on Wayland,
        // xclip on X11 without wl-copy, and nothing installed at all.
        "copy-path" | "copy-path-xclip" | "copy-path-no-tool" => {
            use crate::clipboard::Session;
            let (tools, session, want): (&[&'static str], _, _) = match view {
                "copy-path" => (
                    &["wl-copy"],
                    Session {
                        wayland: true,
                        x11: false,
                    },
                    "Copied 2 paths",
                ),
                "copy-path-xclip" => (
                    &["xclip"],
                    Session {
                        wayland: false,
                        x11: true,
                    },
                    "Copied 2 paths",
                ),
                _ => (
                    &[],
                    Session {
                        wayland: true,
                        x11: true,
                    },
                    "Couldn't copy: install wl-clipboard or xclip",
                ),
            };
            app::fake_clipboard(tools, session);
            select_row(ui, photo);
            let j = row_index(ui, "dune-sunset-0413.jpg")?;
            ui.invoke_row_pointer(j, 0, 0, false, true, false);
            ui.invoke_row_pointer(j, 2, 0, false, true, false);
            ui.invoke_row_menu(j, 330.0, 150.0);
            ui.invoke_file_menu_pick(menu_line(ui, "Copy path")?);
            check("toast", ui.get_toast_text().to_string(), want)?;
        }
        "properties" | "properties-multi" => {
            select_row(ui, photo);
            let mut i = row_index(ui, photo)?;
            if view == "properties-multi" {
                i = row_index(ui, "raw-exports")?;
                ui.invoke_row_pointer(i, 0, 0, false, true, false);
                ui.invoke_row_pointer(i, 2, 0, false, true, false);
            }
            ui.invoke_row_menu(i, 330.0, 150.0);
            ui.invoke_file_menu_pick(menu_line(ui, "Properties")?);
            check("overlay", ui.get_overlay().to_string(), "properties")?;
        }
        _ => {}
    }
    Ok(())
}

/// A case-only rename that ended under a hidden name: the toast names
/// it, has no timer (so a long wait can't hide it), and only [Dismiss]
/// (`-dismissed`) closes it. `toast-stack` then copies a path so a newer
/// toast sits on top of it; `toast-stack-restored` closes that newer
/// toast and checks the sticky one is back with its filename.
/// `toast-stack-two` strands a second file first, so two sticky toasts
/// sit under the newer one. `toast-stack-more`, `-expanded` and
/// `-short` seed five sticky toasts under a timed one to check the
/// "+N more" cap, expanding the list, and the under-600 window.
fn stranded_scene(ui: &AppWindow, view: &str) -> Result<()> {
    use slint::Model;
    let check = |what: &str, got: String, want: &str| {
        if got == want {
            Ok(())
        } else {
            Err(Error::Message(format!("{what}: {got:?}, want {want:?}")))
        }
    };
    if matches!(
        view,
        "toast-stack-more" | "toast-stack-expanded" | "toast-stack-short"
    ) {
        return many_sticky_scene(ui, view, check);
    }
    select_row(ui, "dune-sunset-0412.jpg");
    ui.invoke_key(app::keys::F2.into(), false, false, false);
    ui.show().map_err(|e| Error::Message(e.to_string()))?;
    app::strand_renames(true);
    type_field(ui, "Dune-sunset-0412");
    type_field(ui, "\n");
    app::strand_renames(false);
    app::land_reads();
    check("toast", ui.get_toast_text().to_string(), app::STRANDED_HEAD)?;
    check(
        "hidden name",
        ui.get_toast_pin().to_string(),
        ".robin-rename-4127-0-dune-sunset-0412.jpg",
    )?;
    check(
        "action",
        ui.get_toast_action().to_string(),
        app::TOAST_DISMISS,
    )?;
    check("shown", ui.get_toast_on().to_string(), "true")?;
    if view == "rename-stranded-dismissed" {
        ui.invoke_toast_link();
        check("dismissed", ui.get_toast_on().to_string(), "false")?;
        return Ok(());
    }
    if view == "toast-stack-two" {
        // A second case-only rename also gets stuck, so both hidden
        // names sit in the stack before a newer toast arrives.
        select_row(ui, "dune-sunset-0413.jpg");
        ui.invoke_key(app::keys::F2.into(), false, false, false);
        app::strand_renames(true);
        type_field(ui, "Dune-sunset-0413");
        type_field(ui, "\n");
        app::strand_renames(false);
        app::land_reads();
        check(
            "second",
            ui.get_toast_text().to_string(),
            app::STRANDED_HEAD,
        )?;
        check(
            "second name",
            ui.get_toast_pin().to_string(),
            ".robin-rename-4127-0-dune-sunset-0413.jpg",
        )?;
        check(
            "first parked",
            parked_pin(ui, 0)?,
            ".robin-rename-4127-0-dune-sunset-0412.jpg",
        )?;
    }
    if view == "toast-stack" || view == "toast-stack-restored" || view == "toast-stack-two" {
        use crate::clipboard::Session;
        app::fake_clipboard(
            &["wl-copy"],
            Session {
                wayland: true,
                x11: false,
            },
        );
        // The original photo is now the hidden file; copy another path
        // so a newer toast arrives on top of the sticky one(s).
        let copy = if view == "toast-stack-two" {
            "palette.toml"
        } else {
            "dune-sunset-0413.jpg"
        };
        let j = row_index(ui, copy)?;
        ui.invoke_row_pointer(j, 0, 0, false, false, false);
        ui.invoke_row_pointer(j, 2, 0, false, false, false);
        ui.invoke_row_menu(j, 330.0, 150.0);
        ui.invoke_file_menu_pick(menu_line(ui, "Copy path")?);
        check("front", ui.get_toast_text().to_string(), "Copied 1 path")?;
        let n = if view == "toast-stack-two" { 2 } else { 1 };
        check(
            "parked",
            ui.get_parked_toasts().row_count().to_string(),
            &n.to_string(),
        )?;
        check("under", parked_text(ui, 0)?, app::STRANDED_HEAD)?;
        check("under action", parked_action(ui, 0)?, app::TOAST_DISMISS)?;
        if view == "toast-stack-two" {
            // Newest parked toast sits just under the current one.
            check(
                "under newer",
                parked_pin(ui, 0)?,
                ".robin-rename-4127-0-dune-sunset-0413.jpg",
            )?;
            check(
                "under older",
                parked_pin(ui, 1)?,
                ".robin-rename-4127-0-dune-sunset-0412.jpg",
            )?;
        } else {
            check(
                "under name",
                parked_pin(ui, 0)?,
                ".robin-rename-4127-0-dune-sunset-0412.jpg",
            )?;
        }
        if view == "toast-stack-restored" {
            app::expire_toast();
            check(
                "restored",
                ui.get_toast_text().to_string(),
                app::STRANDED_HEAD,
            )?;
            check(
                "restored name",
                ui.get_toast_pin().to_string(),
                ".robin-rename-4127-0-dune-sunset-0412.jpg",
            )?;
            check(
                "restored action",
                ui.get_toast_action().to_string(),
                app::TOAST_DISMISS,
            )?;
            check("still shown", ui.get_toast_on().to_string(), "true")?;
            check(
                "under gone",
                ui.get_parked_toasts().row_count().to_string(),
                "0",
            )?;
        }
    }
    Ok(())
}

fn parked_text(ui: &AppWindow, i: usize) -> Result<String> {
    use slint::Model;
    ui.get_parked_toasts()
        .row_data(i)
        .map(|c| c.text.to_string())
        .ok_or_else(|| Error::Message(format!("no parked toast {i}")))
}

fn parked_pin(ui: &AppWindow, i: usize) -> Result<String> {
    use slint::Model;
    ui.get_parked_toasts()
        .row_data(i)
        .map(|c| c.pin.to_string())
        .ok_or_else(|| Error::Message(format!("no parked toast {i}")))
}

fn parked_action(ui: &AppWindow, i: usize) -> Result<String> {
    use slint::Model;
    ui.get_parked_toasts()
        .row_data(i)
        .map(|c| c.action.to_string())
        .ok_or_else(|| Error::Message(format!("no parked toast {i}")))
}

fn hidden_pin(n: u32) -> String {
    format!(".robin-rename-4127-0-dune-sunset-041{n}.jpg")
}

/// Five sticky hidden-file toasts under a timed one: three shown and
/// "+2 more" at full height, the full list after expand, or one shown
/// and "+4 more" when the window is under 600px tall.
fn many_sticky_scene(
    ui: &AppWindow,
    view: &str,
    check: impl Fn(&str, String, &str) -> Result<()>,
) -> Result<()> {
    use slint::Model;
    let pins: Vec<String> = (2..=6).map(hidden_pin).collect();
    let pin_refs: Vec<&str> = pins.iter().map(String::as_str).collect();
    app::seed_parked_stickies(&pin_refs, "Copied 1 path", false);
    check("front", ui.get_toast_text().to_string(), "Copied 1 path")?;
    if view == "toast-stack-short" {
        check(
            "parked",
            ui.get_parked_toasts().row_count().to_string(),
            "1",
        )?;
        check("more", ui.get_parked_more().to_string(), "4")?;
        check("under newer", parked_pin(ui, 0)?, &hidden_pin(6))?;
        check("under action", parked_action(ui, 0)?, app::TOAST_DISMISS)?;
        return Ok(());
    }
    check(
        "parked",
        ui.get_parked_toasts().row_count().to_string(),
        "3",
    )?;
    check("more", ui.get_parked_more().to_string(), "2")?;
    check("under newer", parked_pin(ui, 0)?, &hidden_pin(6))?;
    check("under mid", parked_pin(ui, 1)?, &hidden_pin(5))?;
    check("under older", parked_pin(ui, 2)?, &hidden_pin(4))?;
    check("under action", parked_action(ui, 0)?, app::TOAST_DISMISS)?;
    if view == "toast-stack-expanded" {
        ui.invoke_parked_more_expand();
        check(
            "expanded",
            ui.get_parked_toasts().row_count().to_string(),
            "5",
        )?;
        check("more gone", ui.get_parked_more().to_string(), "0")?;
        check("top", parked_pin(ui, 0)?, &hidden_pin(6))?;
        check("bottom", parked_pin(ui, 4)?, &hidden_pin(2))?;
        check(
            "still front",
            ui.get_toast_text().to_string(),
            "Copied 1 path",
        )?;
    }
    Ok(())
}

/// A list row's index by name.
fn row_index(ui: &AppWindow, name: &str) -> Result<i32> {
    use slint::Model;
    let rows = ui.get_files();
    (0..rows.row_count())
        .find(|&i| rows.row_data(i).is_some_and(|r| r.name == name))
        .map(|i| i as i32)
        .ok_or_else(|| Error::Message(format!("no row {name}")))
}

/// The open file menu's line with this label.
fn menu_line(ui: &AppWindow, label: &str) -> Result<i32> {
    use slint::Model;
    let items = ui.get_file_menu_items();
    (0..items.row_count())
        .find(|&i| items.row_data(i).is_some_and(|m| m.label == label))
        .map(|i| i as i32)
        .ok_or_else(|| Error::Message(format!("no menu line {label}")))
}

/// Type into whatever has the keyboard (the rename field), as key
/// presses through the window.
fn type_field(ui: &AppWindow, text: &str) {
    use slint::platform::WindowEvent;
    for ch in text.chars() {
        let text: slint::SharedString = ch.to_string().into();
        ui.window()
            .dispatch_event(WindowEvent::KeyPressed { text: text.clone() });
        ui.window()
            .dispatch_event(WindowEvent::KeyReleased { text });
    }
}

/// Sends each character as a key press, as the window would.
fn type_keys(ui: &AppWindow, text: &str) {
    for ch in text.chars() {
        ui.invoke_key(ch.to_string().into(), false, false, false);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_real_folder_is_never_seeded() {
        let dir = std::env::temp_dir().join(format!("robin-unmarked-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("taxes.txt"), b"real").unwrap();
        let before = listing(&dir);
        let err = seed(&dir).unwrap_err().to_string();
        assert!(err.contains("refusing"), "{err}");
        assert_eq!(listing(&dir), before, "nothing written");
        assert_eq!(std::fs::read(dir.join("taxes.txt")).unwrap(), b"real");
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn an_empty_or_marked_folder_is_seeded() {
        let dir = std::env::temp_dir().join(format!("robin-marked-{}", std::process::id()));
        let _ = crate::fixture::remove(&dir);
        seed(&dir).unwrap();
        assert!(crate::fixture::is_marked(&dir), "seeding writes the marker");
        seed(&dir).unwrap();
        crate::fixture::remove(&dir).unwrap();
    }

    #[test]
    fn the_tmp_fallback_must_be_a_real_folder_we_own() {
        let uid = rustix::process::getuid().as_raw();
        let base = std::env::temp_dir().join(format!("robin-owned-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        std::fs::create_dir_all(&base).unwrap();
        let missing = base.join("missing");
        assert!(check_owned(&missing, uid).is_ok());
        let link = base.join("link");
        std::os::unix::fs::symlink(&base, &link).unwrap();
        assert!(check_owned(&link, uid).is_err(), "symlink");
        assert!(check_owned(&base, uid).is_ok());
        assert!(
            check_owned(&base, uid.wrapping_add(1)).is_err(),
            "other owner"
        );
        std::fs::remove_dir_all(&base).unwrap();
    }

    #[test]
    fn a_marker_folder_does_not_unlock_seeding() {
        let dir = std::env::temp_dir().join(format!("robin-fakemark-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join(crate::fixture::MARKER)).unwrap();
        std::fs::write(dir.join("taxes.txt"), b"real").unwrap();
        assert!(seed(&dir).is_err());
        std::fs::remove_dir_all(&dir).unwrap();
    }

    fn listing(dir: &Path) -> Vec<std::ffi::OsString> {
        let mut v: Vec<_> = std::fs::read_dir(dir)
            .unwrap()
            .map(|e| e.unwrap().file_name())
            .collect();
        v.sort();
        v
    }
}
