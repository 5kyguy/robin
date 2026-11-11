//! libmpv for the viewer's video and audio, loaded at run time.
//!
//! Robin is built and tested without libmpv: the library is opened with
//! `dlopen` the first time a video or audio file is played. Without it,
//! the viewer says so and offers the default player instead. Frames come
//! from the software render API (`MPV_RENDER_API_TYPE_SW`, format `rgb0`,
//! docs/STATUS.md), forced opaque, and handed to Slint as RGBA.
//!
//! The player never reads or writes mpv's own config, scripts or
//! watch-later files; resume positions are Robin's (`player.rs`).

use std::ffi::{c_char, c_int, c_void, CStr, CString};
use std::os::unix::ffi::OsStrExt;
use std::path::Path;
use std::sync::OnceLock;

use crate::player::{next_speed, Cmd, Transport};

#[repr(C)]
struct RenderParam {
    kind: c_int,
    data: *mut c_void,
}

#[repr(C)]
struct Event {
    event_id: c_int,
    error: c_int,
    reply_userdata: u64,
    data: *mut c_void,
}

#[repr(C)]
struct EndFile {
    reason: c_int,
    error: c_int,
}

// client.h and render.h.
const FORMAT_FLAG: c_int = 3;
const FORMAT_INT64: c_int = 4;
const FORMAT_DOUBLE: c_int = 5;
const EVENT_NONE: c_int = 0;
const EVENT_SHUTDOWN: c_int = 1;
const EVENT_END_FILE: c_int = 7;
const EVENT_FILE_LOADED: c_int = 8;
const END_FILE_ERROR: c_int = 4;
const PARAM_INVALID: c_int = 0;
const PARAM_API_TYPE: c_int = 1;
const PARAM_BLOCK_FOR_TARGET_TIME: c_int = 12;
const PARAM_SW_SIZE: c_int = 17;
const PARAM_SW_FORMAT: c_int = 18;
const PARAM_SW_STRIDE: c_int = 19;
const PARAM_SW_POINTER: c_int = 20;
const UPDATE_FRAME: u64 = 1;

type Handle = c_void;

type CreateFn = unsafe extern "C" fn() -> *mut Handle;
type InitFn = unsafe extern "C" fn(*mut Handle) -> c_int;
type SetStrFn = unsafe extern "C" fn(*mut Handle, *const c_char, *const c_char) -> c_int;
type CommandFn = unsafe extern "C" fn(*mut Handle, *mut *const c_char) -> c_int;
type GetPropFn = unsafe extern "C" fn(*mut Handle, *const c_char, c_int, *mut c_void) -> c_int;
type GetStrFn = unsafe extern "C" fn(*mut Handle, *const c_char) -> *mut c_char;
type FreeFn = unsafe extern "C" fn(*mut c_void);
type WaitFn = unsafe extern "C" fn(*mut Handle, f64) -> *mut Event;
type DestroyFn = unsafe extern "C" fn(*mut Handle);
type RenderCreateFn =
    unsafe extern "C" fn(*mut *mut c_void, *mut Handle, *mut RenderParam) -> c_int;
type RenderUpdateFn = unsafe extern "C" fn(*mut c_void) -> u64;
type RenderFn = unsafe extern "C" fn(*mut c_void, *mut RenderParam) -> c_int;
type RenderFreeFn = unsafe extern "C" fn(*mut c_void);

struct Api {
    create: CreateFn,
    initialize: InitFn,
    set_option_string: SetStrFn,
    command: CommandFn,
    get_property: GetPropFn,
    set_property_string: SetStrFn,
    get_property_string: GetStrFn,
    free: FreeFn,
    wait_event: WaitFn,
    terminate_destroy: DestroyFn,
    render_create: RenderCreateFn,
    render_update: RenderUpdateFn,
    render_render: RenderFn,
    render_free: RenderFreeFn,
}

/// Sonames tried in order: libmpv 0.35+ is `.so.2`.
const SONAMES: [&CStr; 3] = [c"libmpv.so.2", c"libmpv.so.1", c"libmpv.so"];

fn api() -> Option<&'static Api> {
    static API: OnceLock<Option<Api>> = OnceLock::new();
    API.get_or_init(load).as_ref()
}

fn load() -> Option<Api> {
    // Lets tests and a bug report check the no-libmpv path on a machine
    // that has it.
    if std::env::var_os("ROBIN_NO_MPV").is_some_and(|v| !v.is_empty()) {
        return None;
    }
    // SAFETY: dlopen with a constant soname; the handle is never closed,
    // so every symbol taken from it stays valid for the process.
    let lib = SONAMES.iter().find_map(|name| {
        let h = unsafe { libc::dlopen(name.as_ptr(), libc::RTLD_NOW | libc::RTLD_LOCAL) };
        (!h.is_null()).then_some(h)
    })?;
    macro_rules! sym {
        ($name:literal, $ty:ty) => {{
            // SAFETY: the symbol is libmpv's, with the C signature its
            // header declares, transmuted to that same signature.
            let p = unsafe { libc::dlsym(lib, $name.as_ptr()) };
            if p.is_null() {
                return None;
            }
            unsafe { std::mem::transmute::<*mut c_void, $ty>(p) }
        }};
    }
    Some(Api {
        create: sym!(c"mpv_create", CreateFn),
        initialize: sym!(c"mpv_initialize", InitFn),
        set_option_string: sym!(c"mpv_set_option_string", SetStrFn),
        command: sym!(c"mpv_command", CommandFn),
        get_property: sym!(c"mpv_get_property", GetPropFn),
        set_property_string: sym!(c"mpv_set_property_string", SetStrFn),
        get_property_string: sym!(c"mpv_get_property_string", GetStrFn),
        free: sym!(c"mpv_free", FreeFn),
        wait_event: sym!(c"mpv_wait_event", WaitFn),
        terminate_destroy: sym!(c"mpv_terminate_destroy", DestroyFn),
        render_create: sym!(c"mpv_render_context_create", RenderCreateFn),
        render_update: sym!(c"mpv_render_context_update", RenderUpdateFn),
        render_render: sym!(c"mpv_render_context_render", RenderFn),
        render_free: sym!(c"mpv_render_context_free", RenderFreeFn),
    })
}

/// libmpv could be loaded.
pub fn available() -> bool {
    api().is_some()
}

/// Why a file isn't playing in Robin.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Why {
    /// libmpv isn't installed.
    NoLibrary,
    /// libmpv refused to start, or the file couldn't be opened or decoded.
    Unplayable,
}

impl Why {
    pub fn line(self) -> &'static str {
        match self {
            Why::NoLibrary => "Playing in Robin needs libmpv",
            Why::Unplayable => "Can't play this file",
        }
    }
}

/// One file playing.
pub struct Mpv {
    api: &'static Api,
    h: *mut Handle,
    render: *mut c_void,
    loaded: bool,
    failed: bool,
}

impl Mpv {
    /// Start playing `path`. `silent` sends audio nowhere (renders and
    /// tests). Opening is asynchronous: a file that turns out not to be
    /// media fails on a later [`Mpv::poll`].
    pub fn open(path: &Path, silent: bool) -> Result<Self, Why> {
        let api = api().ok_or(Why::NoLibrary)?;
        let file = CString::new(path.as_os_str().as_bytes()).map_err(|_| Why::Unplayable)?;
        // SAFETY: a fresh handle, used only through this struct, which
        // destroys it exactly once.
        let h = unsafe { (api.create)() };
        if h.is_null() {
            return Err(Why::Unplayable);
        }
        let mut me = Mpv {
            api,
            h,
            render: std::ptr::null_mut(),
            loaded: false,
            failed: false,
        };
        let mut opts = vec![
            // Nothing from ~/.config/mpv, no scripts, no watch-later.
            ("config", "no"),
            ("load-scripts", "no"),
            ("ytdl", "no"),
            ("resume-playback", "no"),
            ("save-position-on-quit", "no"),
            ("input-default-bindings", "no"),
            ("input-vo-keyboard", "no"),
            ("osc", "no"),
            ("terminal", "no"),
            ("msg-level", "all=no"),
            ("vo", "libmpv"),
            ("hwdec", "no"),
            // The last frame stays up at the end instead of the file closing.
            ("keep-open", "yes"),
            ("idle", "yes"),
            ("sub-auto", "fuzzy"),
        ];
        if silent {
            opts.push(("ao", "null"));
        }
        for (k, v) in opts {
            me.option(k, v);
        }
        // SAFETY: h is live; initialize is called once, before any render
        // context or command.
        if unsafe { (api.initialize)(h) } < 0 {
            return Err(Why::Unplayable);
        }
        let mut params = [
            RenderParam {
                kind: PARAM_API_TYPE,
                data: c"sw".as_ptr() as *mut c_void,
            },
            RenderParam {
                kind: PARAM_INVALID,
                data: std::ptr::null_mut(),
            },
        ];
        // SAFETY: params is terminated by PARAM_INVALID and outlives the call.
        if unsafe { (api.render_create)(&mut me.render, h, params.as_mut_ptr()) } < 0 {
            me.render = std::ptr::null_mut();
            return Err(Why::Unplayable);
        }
        me.command(&[c"loadfile", &file]);
        Ok(me)
    }

    fn option(&self, k: &str, v: &str) {
        let (k, v) = (CString::new(k).unwrap(), CString::new(v).unwrap());
        // SAFETY: h is live; both strings are NUL-terminated.
        unsafe { (self.api.set_option_string)(self.h, k.as_ptr(), v.as_ptr()) };
    }

    fn command(&self, args: &[&CStr]) {
        let mut argv: Vec<*const c_char> = args.iter().map(|a| a.as_ptr()).collect();
        argv.push(std::ptr::null());
        // SAFETY: a NULL-terminated array of live C strings.
        unsafe { (self.api.command)(self.h, argv.as_mut_ptr()) };
    }

    fn set(&self, name: &CStr, value: &str) {
        let v = CString::new(value).unwrap();
        // SAFETY: h is live; both strings are NUL-terminated.
        unsafe { (self.api.set_property_string)(self.h, name.as_ptr(), v.as_ptr()) };
    }

    fn double(&self, name: &CStr) -> Option<f64> {
        let mut out = 0f64;
        // SAFETY: FORMAT_DOUBLE writes one f64 into `out`.
        let rc = unsafe {
            (self.api.get_property)(
                self.h,
                name.as_ptr(),
                FORMAT_DOUBLE,
                &mut out as *mut f64 as *mut c_void,
            )
        };
        (rc >= 0).then_some(out)
    }

    fn int(&self, name: &CStr) -> Option<i64> {
        let mut out = 0i64;
        // SAFETY: FORMAT_INT64 writes one i64 into `out`.
        let rc = unsafe {
            (self.api.get_property)(
                self.h,
                name.as_ptr(),
                FORMAT_INT64,
                &mut out as *mut i64 as *mut c_void,
            )
        };
        (rc >= 0).then_some(out)
    }

    fn flag(&self, name: &CStr) -> Option<bool> {
        let mut out: c_int = 0;
        // SAFETY: FORMAT_FLAG writes one int into `out`.
        let rc = unsafe {
            (self.api.get_property)(
                self.h,
                name.as_ptr(),
                FORMAT_FLAG,
                &mut out as *mut c_int as *mut c_void,
            )
        };
        (rc >= 0).then_some(out != 0)
    }

    fn string(&self, name: &CStr) -> Option<String> {
        // SAFETY: the returned string is mpv's, freed with mpv_free.
        unsafe {
            let p = (self.api.get_property_string)(self.h, name.as_ptr());
            if p.is_null() {
                return None;
            }
            let s = CStr::from_ptr(p).to_string_lossy().into_owned();
            (self.api.free)(p as *mut c_void);
            Some(s)
        }
    }

    /// Drain mpv's events: notes when the file loaded or failed to.
    pub fn poll(&mut self) {
        loop {
            // SAFETY: a 0 timeout never blocks; the event is mpv's and
            // valid until the next wait_event.
            let ev = unsafe { &*(self.api.wait_event)(self.h, 0.0) };
            match ev.event_id {
                EVENT_NONE => break,
                EVENT_FILE_LOADED => self.loaded = true,
                EVENT_SHUTDOWN => self.failed = true,
                EVENT_END_FILE if !ev.data.is_null() => {
                    // SAFETY: END_FILE carries an mpv_event_end_file.
                    let end = unsafe { &*(ev.data as *const EndFile) };
                    if end.reason == END_FILE_ERROR && !self.loaded {
                        self.failed = true;
                    }
                }
                _ => {}
            }
        }
    }

    /// The file has opened and has a duration.
    pub fn loaded(&self) -> bool {
        self.loaded
    }

    /// The transport as mpv reports it now.
    pub fn state(&self) -> Transport {
        let d = Transport::default();
        let size = |n| self.int(n).map_or(0, |v| v.max(0) as u32);
        let video = (size(c"dwidth"), size(c"dheight"));
        Transport {
            pos: self.double(c"time-pos").unwrap_or(0.0),
            dur: self.double(c"duration").unwrap_or(0.0),
            paused: self.flag(c"pause").unwrap_or(false),
            volume: self.double(c"volume").unwrap_or(d.volume),
            muted: self.flag(c"mute").unwrap_or(false),
            speed: self.double(c"speed").unwrap_or(d.speed),
            looping: self.string(c"loop-file").is_some_and(|s| s != "no"),
            subs: self
                .string(c"sid")
                .is_some_and(|s| s != "no" && s != "auto"),
            video: if video.0 > 0 && video.1 > 0 {
                video
            } else {
                (0, 0)
            },
            picture: self.string(c"video-codec").is_some(),
            failed: self.failed,
        }
    }

    pub fn send(&self, cmd: Cmd) {
        match cmd {
            Cmd::Toggle => {
                // At the end, play starts over rather than doing nothing.
                if self.flag(c"eof-reached") == Some(true) {
                    self.command(&[c"seek", c"0", c"absolute"]);
                    self.set(c"pause", "no");
                } else {
                    self.command(&[c"cycle", c"pause"]);
                }
            }
            Cmd::SeekBy(s) => {
                let s = CString::new(format!("{s}")).unwrap();
                self.command(&[c"seek", &s, c"relative+exact"]);
            }
            Cmd::SeekTo(f) => {
                let p = CString::new(format!("{}", f.clamp(0.0, 1.0) * 100.0)).unwrap();
                self.command(&[c"seek", &p, c"absolute-percent+exact"]);
            }
            Cmd::Resume(t) => {
                let t = CString::new(format!("{t}")).unwrap();
                self.command(&[c"seek", &t, c"absolute+exact"]);
            }
            Cmd::Frame(d) if d < 0 => self.command(&[c"frame-back-step"]),
            Cmd::Frame(_) => self.command(&[c"frame-step"]),
            Cmd::Volume(v) => self.set(c"volume", &format!("{}", v.clamp(0.0, 100.0))),
            Cmd::Mute => self.command(&[c"cycle", c"mute"]),
            Cmd::Speed => {
                let now = self.double(c"speed").unwrap_or(1.0);
                self.set(c"speed", &format!("{}", next_speed(now)));
            }
            Cmd::Loop => {
                let on = self.string(c"loop-file").is_some_and(|s| s != "no");
                self.set(c"loop-file", if on { "no" } else { "inf" });
            }
            Cmd::Subs => self.command(&[c"cycle", c"sub"]),
        }
    }

    /// Pause or play outright (the window lost the viewer, a render).
    pub fn set_paused(&self, paused: bool) {
        self.set(c"pause", if paused { "yes" } else { "no" });
    }

    /// A new frame at `w`×`h` if mpv has one, or always when `force` (the
    /// size changed while paused). RGBA, opaque.
    pub fn frame(&mut self, w: u32, h: u32, force: bool) -> Option<Vec<u8>> {
        if self.render.is_null() || w == 0 || h == 0 {
            return None;
        }
        // SAFETY: render is live; update only reads mpv's state.
        let flags = unsafe { (self.api.render_update)(self.render) };
        if flags & UPDATE_FRAME == 0 && !force {
            return None;
        }
        let mut buf = vec![0u8; w as usize * h as usize * 4];
        let mut size: [c_int; 2] = [w as c_int, h as c_int];
        let mut stride: usize = w as usize * 4;
        let mut block: c_int = 0;
        let mut params = [
            RenderParam {
                kind: PARAM_SW_SIZE,
                data: size.as_mut_ptr() as *mut c_void,
            },
            RenderParam {
                kind: PARAM_SW_FORMAT,
                data: c"rgb0".as_ptr() as *mut c_void,
            },
            RenderParam {
                kind: PARAM_SW_STRIDE,
                data: &mut stride as *mut usize as *mut c_void,
            },
            RenderParam {
                kind: PARAM_SW_POINTER,
                data: buf.as_mut_ptr() as *mut c_void,
            },
            RenderParam {
                kind: PARAM_BLOCK_FOR_TARGET_TIME,
                data: &mut block as *mut c_int as *mut c_void,
            },
            RenderParam {
                kind: PARAM_INVALID,
                data: std::ptr::null_mut(),
            },
        ];
        // SAFETY: buf holds h rows of `stride` bytes and outlives the call.
        let rc = unsafe { (self.api.render_render)(self.render, params.as_mut_ptr()) };
        if rc < 0 {
            return None;
        }
        crate::video::rgb0_to_rgba(&mut buf);
        Some(buf)
    }
}

impl Drop for Mpv {
    fn drop(&mut self) {
        // SAFETY: the render context goes before its handle, each once.
        unsafe {
            if !self.render.is_null() {
                (self.api.render_free)(self.render);
            }
            (self.api.terminate_destroy)(self.h);
        }
    }
}

/// The size to render a `video` frame at so it fits `view` (device
/// pixels): aspect kept, never larger than the video, even sides.
pub fn render_size(video: (u32, u32), view: (u32, u32)) -> (u32, u32) {
    let (vw, vh) = (video.0.max(1) as f64, video.1.max(1) as f64);
    let s = (view.0 as f64 / vw).min(view.1 as f64 / vh).min(1.0);
    let even = |x: f64| ((x.round() as u32) & !1).max(2);
    (even(vw * s), even(vh * s))
}

/// A YUV4MPEG2 clip of `frames` RGBA pictures (all `w`×`h`, even sides)
/// at `fps_num`/`fps_den`: a real video without an encoder, for the
/// render harness and the tests.
pub fn write_y4m(
    path: &Path,
    w: u32,
    h: u32,
    frames: &[&[u8]],
    fps: (u32, u32),
) -> std::io::Result<()> {
    use std::io::Write;
    let mut out = std::io::BufWriter::new(std::fs::File::create(path)?);
    writeln!(
        out,
        "YUV4MPEG2 W{w} H{h} F{}:{} Ip A1:1 C420jpeg",
        fps.0, fps.1
    )?;
    let (w, h) = (w as usize, h as usize);
    for rgba in frames {
        let px = |x: usize, y: usize| {
            let i = (y * w + x) * 4;
            (rgba[i] as f32, rgba[i + 1] as f32, rgba[i + 2] as f32)
        };
        let mut y_plane = Vec::with_capacity(w * h);
        for y in 0..h {
            for x in 0..w {
                let (r, g, b) = px(x, y);
                y_plane.push(
                    (0.299 * r + 0.587 * g + 0.114 * b)
                        .round()
                        .clamp(0.0, 255.0) as u8,
                );
            }
        }
        let (mut u, mut v) = (Vec::new(), Vec::new());
        for y in (0..h).step_by(2) {
            for x in (0..w).step_by(2) {
                let (r, g, b) = px(x, y);
                u.push((128.0 - 0.168_736 * r - 0.331_264 * g + 0.5 * b).clamp(0.0, 255.0) as u8);
                v.push((128.0 + 0.5 * r - 0.418_688 * g - 0.081_312 * b).clamp(0.0, 255.0) as u8);
            }
        }
        out.write_all(b"FRAME\n")?;
        out.write_all(&y_plane)?;
        out.write_all(&u)?;
        out.write_all(&v)?;
    }
    out.flush()
}

/// A mono 16-bit WAV of a `hz` tone, `secs` long: real audio for tests.
pub fn write_wav(path: &Path, secs: u32, hz: f32) -> std::io::Result<()> {
    let rate = 8_000u32;
    let n = rate * secs;
    let mut b = Vec::with_capacity(44 + n as usize * 2);
    b.extend_from_slice(b"RIFF");
    b.extend_from_slice(&(36 + n * 2).to_le_bytes());
    b.extend_from_slice(b"WAVEfmt ");
    b.extend_from_slice(&16u32.to_le_bytes());
    b.extend_from_slice(&1u16.to_le_bytes());
    b.extend_from_slice(&1u16.to_le_bytes());
    b.extend_from_slice(&rate.to_le_bytes());
    b.extend_from_slice(&(rate * 2).to_le_bytes());
    b.extend_from_slice(&2u16.to_le_bytes());
    b.extend_from_slice(&16u16.to_le_bytes());
    b.extend_from_slice(b"data");
    b.extend_from_slice(&(n * 2).to_le_bytes());
    for i in 0..n {
        let t = i as f32 / rate as f32;
        let s = ((t * hz * std::f32::consts::TAU).sin() * 8_000.0) as i16;
        b.extend_from_slice(&s.to_le_bytes());
    }
    std::fs::write(path, b)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{Duration, Instant};

    #[test]
    fn frames_fit_the_view_keep_the_aspect_and_never_upscale() {
        assert_eq!(render_size((1920, 1080), (960, 1000)), (960, 540));
        assert_eq!(render_size((320, 180), (1280, 800)), (320, 180));
        assert_eq!(render_size((641, 361), (5000, 5000)), (640, 360), "even");
        assert_eq!(render_size((0, 0), (100, 100)), (2, 2));
    }

    fn wait(m: &mut Mpv, mut until: impl FnMut(&mut Mpv) -> bool) -> bool {
        let t0 = Instant::now();
        while t0.elapsed() < Duration::from_secs(10) {
            m.poll();
            if until(m) {
                return true;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        false
    }

    fn temp(name: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("robin-mpv-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// Plays a real clip through libmpv's software renderer when libmpv
    /// is installed; skipped (not failed) without it.
    #[test]
    fn a_clip_plays_and_renders_opaque_frames() {
        if !available() {
            eprintln!("libmpv not installed; skipping");
            return;
        }
        let dir = temp("clip");
        let clip = dir.join("clip.mp4");
        let red: Vec<u8> = [220u8, 30, 30, 255].repeat(64 * 36);
        let frames: Vec<&[u8]> = (0..20).map(|_| red.as_slice()).collect();
        write_y4m(&clip, 64, 36, &frames, (10, 1)).unwrap();
        let mut m = Mpv::open(&clip, true).unwrap();
        assert!(wait(&mut m, |m| m.loaded() && m.state().video == (64, 36)));
        let st = m.state();
        assert!((st.dur - 2.0).abs() < 0.2, "{st:?}");
        assert!(!st.failed && st.picture);
        let mut px = None;
        assert!(wait(&mut m, |m| {
            px = m.frame(64, 36, false);
            px.is_some()
        }));
        let px = px.unwrap();
        assert_eq!(px.len(), 64 * 36 * 4);
        assert!(px.chunks(4).all(|p| p[3] == 255), "opaque");
        let mid = &px[(18 * 64 + 32) * 4..][..3];
        assert!(mid[0] > 150 && mid[1] < 90 && mid[2] < 90, "red: {mid:?}");
        m.send(Cmd::Toggle);
        assert!(wait(&mut m, |m| m.state().paused));
        m.send(Cmd::Speed);
        m.send(Cmd::Loop);
        assert!(wait(&mut m, |m| {
            let s = m.state();
            s.speed == 1.25 && s.looping
        }));
        drop(m);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn audio_plays_with_no_picture_and_junk_fails() {
        if !available() {
            eprintln!("libmpv not installed; skipping");
            return;
        }
        let dir = temp("audio");
        let wav = dir.join("tone.wav");
        write_wav(&wav, 3, 440.0).unwrap();
        let mut m = Mpv::open(&wav, true).unwrap();
        assert!(wait(&mut m, |m| m.loaded() && m.state().dur > 2.5));
        assert_eq!(m.state().video, (0, 0));
        assert!(!m.state().picture, "no picture track");
        let junk = dir.join("junk.mp4");
        std::fs::write(&junk, b"not a real video\n").unwrap();
        let mut j = Mpv::open(&junk, true).unwrap();
        assert!(wait(&mut j, |j| j.state().failed));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn without_libmpv_the_reason_is_said_plainly() {
        assert_eq!(Why::NoLibrary.line(), "Playing in Robin needs libmpv");
        assert_eq!(Why::Unplayable.line(), "Can't play this file");
    }
}
