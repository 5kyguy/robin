//! The interactive window: listing, keyboard, drag and drop, and file jobs.
//!
//! [`Controller`] holds everything that decides what a key or a drop does,
//! so it is tested without a window. [`run`] binds it to Slint and runs jobs
//! on a worker thread that reports back through the event loop.

use std::cell::RefCell;
use std::collections::VecDeque;
use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::Arc;
use std::time::Duration;

use slint::{ComponentHandle, Model, ModelRc, SharedString, VecModel};

use crate::fsops::{self, Cancel, KeptEntry, TrashChoice};
use crate::model::{Listing, Nav};
use crate::ops::{self, Clipboard, Env, Job, JobKind, Mods, Op, Outcome, Progress, Trashed};
use crate::safety::DirFd;
use crate::{AppWindow, Error, Result, Row};

/// What the trash/delete dialog is asking about.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Dialog {
    Trash(Vec<OsString>),
    /// The drive has no trash: Delete permanently is the only way.
    TrashNo(Vec<OsString>),
    Delete(Vec<OsString>),
    /// A move left `name` where it was. Button 1 is Copy instead when allowed.
    Refused {
        job: Job,
        name: OsString,
        can_copy: bool,
    },
}

impl Dialog {
    pub fn overlay(&self) -> &'static str {
        match self {
            Dialog::Trash(_) => "trash",
            Dialog::TrashNo(_) => "trash-no",
            Dialog::Delete(_) => "delete",
            Dialog::Refused { .. } => "move-refused",
        }
    }

    fn buttons(&self) -> i32 {
        match self {
            Dialog::Refused {
                can_copy: false, ..
            } => 1,
            _ => 2,
        }
    }

    /// The button Enter presses when the dialog opens. Delete starts on
    /// Cancel so a held Enter can't destroy anything.
    fn default_focus(&self) -> i32 {
        match self {
            Dialog::Delete(_) | Dialog::TrashNo(_) => 0,
            Dialog::Trash(_) => 1,
            Dialog::Refused { can_copy, .. } => i32::from(*can_copy),
        }
    }
}

/// What the window should do after the controller handled an input.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Action {
    None,
    /// Repaint rows and overlays.
    Redraw,
    Run(Job),
    Open(Dialog),
    Navigate(PathBuf),
    CancelJob,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Drag {
    pub names: Vec<OsString>,
    pub target: Option<usize>,
    pub op: Option<Op>,
}

pub struct Controller {
    pub dir: PathBuf,
    pub listing: Listing,
    pub clip: Clipboard,
    pub env: Env,
    pub dialog: Option<Dialog>,
    pub focus: i32,
    pub armed: bool,
    pub drag: Option<Drag>,
    pressed: Option<usize>,
    pub job_running: bool,
}

pub mod keys {
    pub const DELETE: &str = "\u{7f}";
    pub const RETURN: &str = "\n";
    pub const BACKSPACE: &str = "\u{8}";
    pub const ESCAPE: &str = "\u{1b}";
    pub const TAB: &str = "\t";
    pub const BACKTAB: &str = "\u{19}";
    pub const UP: &str = "\u{F700}";
    pub const DOWN: &str = "\u{F701}";
    pub const LEFT: &str = "\u{F702}";
    pub const RIGHT: &str = "\u{F703}";
    pub const HOME: &str = "\u{F729}";
    pub const END: &str = "\u{F72B}";
}

/// Ctrl+letter arrives either as the letter or as its control character.
fn ctrl_letter(text: &str, letter: char) -> bool {
    let ctl = char::from(letter.to_ascii_lowercase() as u8 - b'a' + 1);
    text.eq_ignore_ascii_case(&letter.to_string()) || text == ctl.to_string()
}

impl Controller {
    pub fn new(dir: PathBuf, env: Env) -> Self {
        Self {
            dir,
            listing: Listing::default(),
            clip: Clipboard::default(),
            env,
            dialog: None,
            focus: 0,
            armed: false,
            drag: None,
            pressed: None,
            job_running: false,
        }
    }

    pub fn reload(&mut self) -> Result<()> {
        let fd = DirFd::open(&self.dir)?;
        self.listing.replace_dir(&fd)
    }

    fn names(&self) -> Vec<OsString> {
        self.listing.selection().iter().cloned().collect()
    }

    fn row_name(&self, index: usize) -> Option<(OsString, bool)> {
        self.listing
            .view()
            .get(index)
            .map(|e| (e.name.clone(), e.is_dir))
    }

    pub fn open_dialog(&mut self, dialog: Dialog) {
        self.focus = dialog.default_focus();
        self.armed = !matches!(dialog, Dialog::Delete(_));
        self.dialog = Some(dialog);
    }

    /// Del or Shift+Del on the selection.
    pub fn remove(&self, permanent: bool) -> Action {
        let names = self.names();
        if names.is_empty() {
            return Action::None;
        }
        if permanent {
            return Action::Open(Dialog::Delete(names));
        }
        match ops::trash_choice(&self.dir, &self.env) {
            TrashChoice::Ask => Action::Open(Dialog::TrashNo(names)),
            _ => Action::Open(Dialog::Trash(names)),
        }
    }

    pub fn key(&mut self, text: &str, ctrl: bool, shift: bool) -> Action {
        if self.dialog.is_some() {
            return self.dialog_key(text, shift);
        }
        if ctrl {
            if ctrl_letter(text, 'x') {
                self.clip.cut(&self.dir, self.names());
                return Action::Redraw;
            }
            if ctrl_letter(text, 'c') {
                self.clip.copy(&self.dir, self.names());
                return Action::Redraw;
            }
            if ctrl_letter(text, 'v') {
                return self.clip.paste(&self.dir).map_or(Action::None, Action::Run);
            }
        }
        let nav = match text {
            keys::UP => Some(Nav::Up),
            keys::DOWN => Some(Nav::Down),
            keys::HOME => Some(Nav::Home),
            keys::END => Some(Nav::End),
            _ => None,
        };
        if let Some(nav) = nav {
            self.listing.move_cursor(nav, shift, 1);
            return Action::Redraw;
        }
        match text {
            keys::DELETE => self.remove(shift),
            keys::ESCAPE if self.clip.clip().is_some() => {
                self.clip.clear();
                Action::Redraw
            }
            keys::ESCAPE if self.job_running => Action::CancelJob,
            keys::RETURN => match self.listing.cursor().cloned() {
                Some(name) if self.dir.join(&name).is_dir() => {
                    Action::Navigate(self.dir.join(name))
                }
                _ => Action::None,
            },
            keys::BACKSPACE => match self.dir.parent() {
                Some(p) => Action::Navigate(p.to_path_buf()),
                None => Action::None,
            },
            _ => Action::None,
        }
    }

    fn dialog_key(&mut self, text: &str, shift: bool) -> Action {
        let n = self.dialog.as_ref().map_or(1, Dialog::buttons);
        match text {
            keys::TAB | keys::RIGHT if !shift => {
                self.focus = (self.focus + 1) % n;
                Action::Redraw
            }
            keys::TAB | keys::BACKTAB | keys::LEFT => {
                self.focus = (self.focus + n - 1) % n;
                Action::Redraw
            }
            keys::RETURN | " " => self.button(self.focus),
            keys::ESCAPE => self.button(0),
            _ => Action::None,
        }
    }

    /// A dialog button: 0 is always Cancel/Close, 1 confirms.
    pub fn button(&mut self, index: i32) -> Action {
        let Some(dialog) = self.dialog.clone() else {
            return Action::None;
        };
        if index == 0 {
            self.dialog = None;
            return Action::Redraw;
        }
        if !self.armed {
            return Action::None;
        }
        self.dialog = None;
        match dialog {
            Dialog::Trash(names) => {
                Action::Run(Job::new(JobKind::Trash, &self.dir, names, &self.dir))
            }
            Dialog::TrashNo(names) | Dialog::Delete(names) => {
                Action::Run(Job::new(JobKind::Delete, &self.dir, names, &self.dir))
            }
            Dialog::Refused {
                job,
                name,
                can_copy,
            } if can_copy => Action::Run(Job::new(JobKind::Copy, &job.src, vec![name], &job.dest)),
            Dialog::Refused { .. } => Action::Redraw,
        }
    }

    /// Pointer on row `index`: `kind` 0 down, 1 move, 2 up. `dy` is how many
    /// rows the pointer is from the pressed row.
    pub fn pointer(&mut self, index: usize, kind: i32, dy: i32, far: bool, mods: Mods) -> Action {
        match kind {
            0 => {
                self.pressed = Some(index);
                if let Some((name, _)) = self.row_name(index) {
                    if !self.listing.selection().contains(&name) {
                        self.listing.select_only(name);
                    }
                }
                Action::Redraw
            }
            1 => {
                let Some(start) = self.pressed else {
                    return Action::None;
                };
                if self.drag.is_none() && !far {
                    return Action::None;
                }
                let names = match &self.drag {
                    Some(d) => d.names.clone(),
                    None => self.names(),
                };
                let at = start as i64 + i64::from(dy);
                let target = usize::try_from(at).ok().and_then(|t| {
                    let (name, is_dir) = self.row_name(t)?;
                    (is_dir && !names.contains(&name)).then_some(t)
                });
                let op = target.and_then(|t| {
                    let dest = self.dir.join(self.row_name(t)?.0);
                    let op = ops::drop_op(mods, ops::same_filesystem(&self.dir, &dest))?;
                    ops::drop_allowed(op, &self.dir, &names, &dest).then_some(op)
                });
                let next = Drag {
                    names,
                    target: op.and(target),
                    op,
                };
                let changed = self.drag.as_ref() != Some(&next);
                self.drag = Some(next);
                if changed {
                    Action::Redraw
                } else {
                    Action::None
                }
            }
            _ => {
                self.pressed = None;
                let Some(drag) = self.drag.take() else {
                    if !far {
                        if let Some((name, _)) = self.row_name(index) {
                            self.listing.select_only(name);
                        }
                    }
                    return Action::Redraw;
                };
                match (drag.target, drag.op) {
                    (Some(t), Some(op)) => {
                        let dest = self
                            .dir
                            .join(self.row_name(t).map(|r| r.0).unwrap_or_default());
                        Action::Run(Job::transfer(op, &self.dir, drag.names, &dest))
                    }
                    _ => Action::Redraw,
                }
            }
        }
    }

    pub fn drag_label(&self) -> Option<(String, &'static str)> {
        let drag = self.drag.as_ref()?;
        let (Some(t), Some(op)) = (drag.target, drag.op) else {
            return Some(("Drop on a folder".into(), "󰆏"));
        };
        let to = self.row_name(t)?.0.to_string_lossy().into_owned();
        let n = ops::count(drag.names.len());
        Some(match op {
            Op::Copy => (format!("Copy {n} to {to}"), "󰆏"),
            Op::Move => (format!("Move {n} to {to}"), "󰪹"),
        })
    }

    /// A finished job: the refused moves come back as a dialog.
    pub fn finish(&mut self, job: &Job, outcome: &Outcome) -> Option<Dialog> {
        let (name, kept) = outcome.refused.first()?;
        let refusal = fsops::refusal(&name.to_string_lossy(), kept);
        Some(Dialog::Refused {
            job: job.clone(),
            name: name.clone(),
            can_copy: refusal.can_copy_instead,
        })
    }

    pub fn rows(&self) -> Vec<Row> {
        let target = self.drag.as_ref().and_then(|d| d.target);
        self.listing
            .view()
            .iter()
            .enumerate()
            .map(|(i, e)| Row {
                glyph: if e.is_dir { "󰉋" } else { "󰈔" }.into(),
                name: e.name.to_string_lossy().as_ref().into(),
                size: if e.is_dir {
                    String::new()
                } else {
                    human(e.len)
                }
                .into(),
                kind: if e.is_dir {
                    "Folder".into()
                } else {
                    e.extension().to_uppercase()
                }
                .into(),
                modified: SharedString::new(),
                selected: self.listing.selection().contains(&e.name),
                hovered: false,
                hidden: e.is_hidden(),
                role: i32::from(e.is_dir),
                badge: SharedString::new(),
                cut: self.clip.is_cut(&self.dir, &e.name),
                drop: target == Some(i),
            })
            .collect()
    }
}

fn human(n: u64) -> String {
    let units = ["B", "KB", "MB", "GB", "TB"];
    let mut v = n as f64;
    let mut u = 0;
    while v >= 1000.0 && u < units.len() - 1 {
        v /= 1000.0;
        u += 1;
    }
    if u == 0 {
        format!("{n} B")
    } else {
        format!("{v:.1} {}", units[u])
    }
}

/// The names line under a confirm title: up to five, then "and N more".
pub fn names_text(names: &[OsString]) -> String {
    let mut lines: Vec<String> = names
        .iter()
        .take(5)
        .map(|n| n.to_string_lossy().into_owned())
        .collect();
    if names.len() > 5 {
        lines.push(format!("and {} more", names.len() - 5));
    }
    lines.join("\n")
}

fn noun(names: &[OsString]) -> String {
    match names {
        [one] => format!("“{}”", one.to_string_lossy()),
        _ => ops::count(names.len()),
    }
}

// ---- window binding -------------------------------------------------------

struct State {
    ctl: Controller,
    queue: VecDeque<Job>,
    cancel: Option<Arc<Cancel>>,
    undo: Vec<Trashed>,
    toast_open: bool,
    toast_gen: u64,
    arm_timer: slint::Timer,
}

type Shared = Rc<RefCell<State>>;

thread_local! {
    static STATE: RefCell<Option<(Shared, slint::Weak<AppWindow>)>> = const { RefCell::new(None) };
}

pub fn run(dir: PathBuf) -> Result<i32> {
    let ui = AppWindow::new().map_err(|e| Error::Message(e.to_string()))?;
    crate::shots::apply_theme(&ui, &crate::theme::Theme::cyan());
    ui.set_harness(false);
    let mut ctl = Controller::new(dir, Env::from_system());
    ctl.reload()?;
    let state: Shared = Rc::new(RefCell::new(State {
        ctl,
        queue: VecDeque::new(),
        cancel: None,
        undo: Vec::new(),
        toast_open: false,
        toast_gen: 0,
        arm_timer: slint::Timer::default(),
    }));
    STATE.with(|s| *s.borrow_mut() = Some((state.clone(), ui.as_weak())));
    redraw(&ui, &state.borrow());

    let w = ui.as_weak();
    let st = state.clone();
    ui.on_key(move |text, ctrl, shift, _alt| {
        let action = st.borrow_mut().ctl.key(&text, ctrl, shift);
        let handled = action != Action::None;
        if let Some(ui) = w.upgrade() {
            apply(&ui, &st, action);
        }
        handled
    });
    let w = ui.as_weak();
    let st = state.clone();
    ui.on_row_pointer(move |index, kind, dy, far, ctrl, shift| {
        let action =
            st.borrow_mut()
                .ctl
                .pointer(index.max(0) as usize, kind, dy, far, Mods { ctrl, shift });
        if let Some(ui) = w.upgrade() {
            apply(&ui, &st, action);
        }
    });
    let w = ui.as_weak();
    let st = state.clone();
    ui.on_row_open(move |index| {
        let target = {
            let s = st.borrow();
            s.ctl
                .row_name(index.max(0) as usize)
                .filter(|r| r.1)
                .map(|r| s.ctl.dir.join(r.0))
        };
        if let (Some(ui), Some(t)) = (w.upgrade(), target) {
            apply(&ui, &st, Action::Navigate(t));
        }
    });
    let w = ui.as_weak();
    let st = state.clone();
    ui.on_dialog_button(move |i| {
        let action = st.borrow_mut().ctl.button(i);
        if let Some(ui) = w.upgrade() {
            apply(&ui, &st, action);
        }
    });
    let w = ui.as_weak();
    let st = state.clone();
    ui.on_job_cancel(move || {
        if let Some(ui) = w.upgrade() {
            apply(&ui, &st, Action::CancelJob);
        }
    });
    let w = ui.as_weak();
    let st = state.clone();
    ui.on_toast_link(move || {
        let Some(ui) = w.upgrade() else { return };
        if ui.get_toast_details().row_count() > 0 {
            let open = {
                let mut s = st.borrow_mut();
                s.toast_open = !s.toast_open;
                s.toast_open
            };
            ui.set_toast_open(open);
            return;
        }
        let items = std::mem::take(&mut st.borrow_mut().undo);
        if items.is_empty() {
            return;
        }
        let (restored, failed) = ops::undo(&items);
        let _ = st.borrow_mut().ctl.reload();
        redraw(&ui, &st.borrow());
        let text = if failed.is_empty() {
            format!("Restored {}", ops::count(restored))
        } else {
            format!(
                "Restored {}, {} stayed in the trash",
                ops::count(restored),
                failed.len()
            )
        };
        let tone = if failed.is_empty() { "" } else { "warning" };
        show_toast(&ui, &st, tone, text, failed, "");
    });

    ui.show().map_err(|e| Error::Message(e.to_string()))?;
    slint::run_event_loop().map_err(|e| Error::Message(e.to_string()))?;
    Ok(0)
}

fn redraw(ui: &AppWindow, s: &State) {
    let ctl = &s.ctl;
    ui.set_files(ModelRc::new(Rc::new(VecModel::from(ctl.rows()))));
    let home = std::env::var_os("HOME").map(PathBuf::from);
    let crumbs: Vec<String> = crate::model::breadcrumbs(&ctl.dir, home.as_deref())
        .iter()
        .map(|c| c.label.clone())
        .collect();
    ui.set_crumbs(crumbs.join("  ›  ").into());
    ui.set_path_text(ctl.dir.display().to_string().into());
    let n = ctl.listing.view().len();
    let sel = ctl.listing.selection().len();
    let mut left = ops::count(n);
    if sel > 0 {
        left = format!("{left} · {sel} selected");
    }
    if let Some(clip) = ctl.clip.clip() {
        let verb = if clip.mode == ops::ClipMode::Cut {
            "cut"
        } else {
            "copied"
        };
        left = format!("{left} · {} {verb}", ops::count(clip.names.len()));
    }
    ui.set_status_left(left.into());
    ui.set_state(if n == 0 { "empty" } else { "files" }.into());
    match ctl.drag_label() {
        Some((label, glyph)) => {
            ui.set_drag_label(label.into());
            ui.set_drag_glyph(glyph.into());
        }
        None => ui.set_drag_label(SharedString::new()),
    }
    ui.set_dialog_focus(ctl.focus);
    ui.set_danger_armed(ctl.armed);
    match &ctl.dialog {
        Some(d) => {
            ui.set_overlay(d.overlay().into());
            match d {
                Dialog::Trash(names) | Dialog::TrashNo(names) | Dialog::Delete(names) => {
                    ui.set_confirm_noun(noun(names).into());
                    ui.set_confirm_names(names_text(names).into());
                    ui.set_confirm_names_count(names.len().min(6) as i32);
                }
                Dialog::Refused { .. } => {}
            }
        }
        None => ui.set_overlay(SharedString::new()),
    }
}

fn apply(ui: &AppWindow, st: &Shared, action: Action) {
    match action {
        Action::None => {}
        Action::Redraw => redraw(ui, &st.borrow()),
        Action::Open(d) => {
            let delete = matches!(d, Dialog::Delete(_));
            st.borrow_mut().ctl.open_dialog(d);
            redraw(ui, &st.borrow());
            if delete {
                let w = ui.as_weak();
                let st2 = st.clone();
                st.borrow().arm_timer.start(
                    slint::TimerMode::SingleShot,
                    Duration::from_millis(600),
                    move || {
                        st2.borrow_mut().ctl.armed = true;
                        if let Some(ui) = w.upgrade() {
                            ui.set_danger_armed(true);
                        }
                    },
                );
            }
        }
        Action::Navigate(dir) => {
            let mut s = st.borrow_mut();
            let old = std::mem::replace(&mut s.ctl.dir, dir);
            s.ctl.listing = Listing::default();
            if let Err(e) = s.ctl.reload() {
                s.ctl.dir = old;
                let _ = s.ctl.reload();
                drop(s);
                show_toast(ui, st, "danger", e.to_string(), Vec::new(), "");
                return;
            }
            redraw(ui, &s);
        }
        Action::CancelJob => {
            if let Some(c) = &st.borrow().cancel {
                c.cancel();
            }
            st.borrow_mut().queue.clear();
            ui.set_job_detail("Cancelling after this item…".into());
        }
        Action::Run(job) => {
            st.borrow_mut().queue.push_back(job);
            redraw(ui, &st.borrow());
            pump(ui, st);
        }
    }
}

/// Start the next queued job if none is running.
fn pump(ui: &AppWindow, st: &Shared) {
    let (job, env, cancel) = {
        let mut s = st.borrow_mut();
        if s.ctl.job_running {
            return;
        }
        let Some(job) = s.queue.pop_front() else {
            return;
        };
        s.ctl.job_running = true;
        let cancel = Arc::new(Cancel::new());
        s.cancel = Some(cancel.clone());
        (job, s.ctl.env.clone(), cancel)
    };
    ui.set_job_on(true);
    ui.set_job_glyph(job.glyph().into());
    ui.set_job_label(job.label().into());
    ui.set_job_detail(format!("0 of {}", ops::count(job.names.len())).into());
    ui.set_job_amount(0.0);
    std::thread::spawn(move || {
        let outcome = ops::run(&job, &env, &cancel, &mut |p: &Progress| {
            let (detail, amount) = (p.detail(), p.amount());
            let _ = slint::invoke_from_event_loop(move || {
                with_state(|ui, _| {
                    if !ui.get_job_detail().starts_with("Cancelling") {
                        ui.set_job_detail(detail.into());
                    }
                    ui.set_job_amount(amount);
                });
            });
        });
        let _ = slint::invoke_from_event_loop(move || {
            with_state(|ui, st| done(&ui, st, job, outcome));
        });
    });
}

fn with_state(f: impl FnOnce(AppWindow, &Shared)) {
    STATE.with(|s| {
        if let Some((st, w)) = s.borrow().as_ref() {
            if let Some(ui) = w.upgrade() {
                f(ui, st);
            }
        }
    });
}

fn done(ui: &AppWindow, st: &Shared, job: Job, outcome: Outcome) {
    ui.set_job_on(false);
    let dialog = {
        let mut s = st.borrow_mut();
        s.ctl.job_running = false;
        s.cancel = None;
        let _ = s.ctl.reload();
        s.ctl.finish(&job, &outcome)
    };
    redraw(ui, &st.borrow());
    let toast = outcome.toast(job.kind);
    if outcome.done > 0 || !toast.details.is_empty() || outcome.cancelled {
        if toast.undo {
            st.borrow_mut().undo = outcome.trashed.clone();
        }
        show_toast(
            ui,
            st,
            toast.tone,
            toast.text,
            toast.details,
            if toast.undo { "Undo" } else { "" },
        );
    }
    if let Some(d) = dialog {
        if let Dialog::Refused { name, .. } = &d {
            let kept: &Vec<KeptEntry> = &outcome.refused[0].1;
            let r = fsops::refusal(&name.to_string_lossy(), kept);
            ui.set_refusal_title(r.title.as_str().into());
            ui.set_refusal_can_copy(r.can_copy_instead);
            ui.set_refusal_dismiss(r.dismiss_label.into());
            ui.set_refusal_notes(r.note_count() as i32);
            ui.set_refusal_lines(ModelRc::new(Rc::new(VecModel::from(
                r.lines
                    .into_iter()
                    .map(|l| crate::RefusalLine {
                        path: l.path.into(),
                        note: l.note.unwrap_or_default().into(),
                    })
                    .collect::<Vec<_>>(),
            ))));
        }
        apply(ui, st, Action::Open(d));
    }
    pump(ui, st);
}

fn show_toast(
    ui: &AppWindow,
    st: &Shared,
    tone: &str,
    text: String,
    details: Vec<String>,
    action: &str,
) {
    let gen = {
        let mut s = st.borrow_mut();
        s.toast_gen += 1;
        s.toast_open = false;
        s.toast_gen
    };
    ui.set_toast_tone(tone.into());
    ui.set_toast_text(text.into());
    ui.set_toast_details(ModelRc::new(Rc::new(VecModel::from(
        details
            .into_iter()
            .map(SharedString::from)
            .collect::<Vec<_>>(),
    ))));
    ui.set_toast_action(action.into());
    ui.set_toast_open(false);
    ui.set_toast_on(true);
    let secs = if action.is_empty() && ui.get_toast_details().row_count() == 0 {
        4
    } else {
        8
    };
    let w = ui.as_weak();
    let st = st.clone();
    slint::Timer::single_shot(Duration::from_secs(secs), move || {
        let mut s = st.borrow_mut();
        if s.toast_gen == gen && !s.toast_open {
            s.undo.clear();
            if let Some(ui) = w.upgrade() {
                ui.set_toast_on(false);
            }
        }
    });
}

pub fn start_dir(arg: Option<&Path>) -> PathBuf {
    match arg {
        Some(p) => p.to_path_buf(),
        None => std::env::var_os("HOME")
            .map(PathBuf::from)
            .unwrap_or_else(|| "/".into()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn setup(label: &str) -> Controller {
        let root = std::env::temp_dir().join(format!("robin-app-{label}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        let dir = root.join("Pictures");
        fs::create_dir_all(dir.join("raw-exports")).unwrap();
        fs::write(dir.join("a.jpg"), b"a").unwrap();
        fs::write(dir.join("b.jpg"), b"b").unwrap();
        let env = Env {
            home: root.clone(),
            home_trash: root.join("trash"),
        };
        let mut c = Controller::new(dir, env);
        c.reload().unwrap();
        c
    }

    fn select(c: &mut Controller, name: &str) {
        c.listing.select_only(name.into());
    }

    #[test]
    fn ctrl_x_dims_rows_and_ctrl_v_moves_once() {
        let mut c = setup("cut");
        select(&mut c, "a.jpg");
        assert_eq!(c.key("x", true, false), Action::Redraw);
        let rows = c.rows();
        assert!(rows.iter().find(|r| r.name == "a.jpg").unwrap().cut);
        assert!(!rows.iter().find(|r| r.name == "b.jpg").unwrap().cut);
        c.dir = c.dir.join("raw-exports");
        let Action::Run(job) = c.key("\u{16}", true, false) else {
            panic!("paste should run a job")
        };
        assert_eq!(job.kind, JobKind::Move);
        assert_eq!(c.key("v", true, false), Action::None, "a cut is used up");
        // Pasting a cut back into its own folder does nothing.
        c.dir = c.dir.parent().unwrap().to_path_buf();
        c.key("x", true, false);
        assert_eq!(c.key("v", true, false), Action::None);
    }

    #[test]
    fn ctrl_c_pastes_again_and_esc_clears_a_cut() {
        let mut c = setup("copy");
        select(&mut c, "b.jpg");
        c.key("c", true, false);
        c.dir = c.dir.join("raw-exports");
        assert!(matches!(
            c.key("v", true, false),
            Action::Run(Job {
                kind: JobKind::Copy,
                ..
            })
        ));
        assert!(matches!(c.key("v", true, false), Action::Run(_)));
        c.key("x", true, false);
        c.key(keys::ESCAPE, false, false);
        assert!(c.clip.clip().is_none());
    }

    #[test]
    fn drag_onto_a_folder_follows_modifiers() {
        let mut c = setup("dnd");
        // Rows: raw-exports, a.jpg, b.jpg (folders first).
        let none = Mods::default();
        c.pointer(1, 0, 0, false, none);
        assert_eq!(
            c.pointer(1, 1, 0, false, none),
            Action::None,
            "below threshold"
        );
        c.pointer(1, 1, -1, true, none);
        assert!(c.rows()[0].drop);
        assert_eq!(c.drag_label().unwrap().0, "Move 1 item to raw-exports");
        let ctrl = Mods {
            ctrl: true,
            shift: false,
        };
        c.pointer(1, 1, -1, true, ctrl);
        assert_eq!(c.drag_label().unwrap().0, "Copy 1 item to raw-exports");
        let Action::Run(job) = c.pointer(1, 2, -1, true, ctrl) else {
            panic!("drop should run")
        };
        assert_eq!(job.kind, JobKind::Copy);
        assert!(job.dest.ends_with("raw-exports"));
        assert!(c.drag.is_none());
    }

    #[test]
    fn dropping_on_a_file_does_nothing() {
        let mut c = setup("dnd-file");
        c.pointer(1, 0, 0, false, Mods::default());
        c.pointer(1, 1, 1, true, Mods::default());
        assert!(c.rows().iter().all(|r| !r.drop));
        assert_eq!(c.pointer(1, 2, 1, true, Mods::default()), Action::Redraw);
    }

    #[test]
    fn del_confirms_from_the_keyboard() {
        let mut c = setup("del");
        select(&mut c, "a.jpg");
        let Action::Open(d) = c.key(keys::DELETE, false, false) else {
            panic!()
        };
        assert_eq!(d, Dialog::Trash(vec!["a.jpg".into()]));
        c.open_dialog(d);
        assert_eq!(c.focus, 1, "Trash is the default");
        assert!(matches!(
            c.key(keys::RETURN, false, false),
            Action::Run(Job {
                kind: JobKind::Trash,
                ..
            })
        ));
        assert!(c.dialog.is_none());
    }

    #[test]
    fn shift_del_needs_tab_and_the_arm_delay() {
        let mut c = setup("shiftdel");
        select(&mut c, "a.jpg");
        let Action::Open(d) = c.key(keys::DELETE, false, true) else {
            panic!()
        };
        c.open_dialog(d);
        assert_eq!(c.focus, 0, "Cancel is the default");
        c.key(keys::TAB, false, false);
        assert_eq!(c.focus, 1);
        assert_eq!(
            c.key(keys::RETURN, false, false),
            Action::None,
            "not armed yet"
        );
        c.armed = true;
        assert!(matches!(
            c.key(keys::RETURN, false, false),
            Action::Run(Job {
                kind: JobKind::Delete,
                ..
            })
        ));
    }

    #[test]
    fn escape_cancels_a_dialog() {
        let mut c = setup("esc");
        select(&mut c, "a.jpg");
        c.open_dialog(Dialog::Trash(vec!["a.jpg".into()]));
        assert_eq!(c.key(keys::ESCAPE, false, false), Action::Redraw);
        assert!(c.dialog.is_none());
    }

    #[test]
    fn trash_then_undo_puts_the_file_back() {
        let mut c = setup("undo");
        let job = Job::new(JobKind::Trash, &c.dir, vec!["a.jpg".into()], &c.dir);
        let out = ops::run(&job, &c.env, &Cancel::new(), &mut |_| {});
        assert!(out.toast(JobKind::Trash).undo);
        assert!(!c.dir.join("a.jpg").exists());
        let (n, failed) = ops::undo(&out.trashed);
        assert_eq!((n, failed.len()), (1, 0));
        assert_eq!(fs::read(c.dir.join("a.jpg")).unwrap(), b"a");
        c.reload().unwrap();
    }

    #[test]
    fn cancel_before_start_runs_nothing() {
        let c = setup("cancel");
        let job = Job::new(
            JobKind::Delete,
            &c.dir,
            vec!["a.jpg".into(), "b.jpg".into()],
            &c.dir,
        );
        let cancel = Cancel::new();
        cancel.cancel();
        let out = ops::run(&job, &c.env, &cancel, &mut |_| {});
        assert!(out.cancelled);
        assert!(c.dir.join("a.jpg").exists() && c.dir.join("b.jpg").exists());
    }

    #[test]
    fn copy_instead_from_a_refusal() {
        let mut c = setup("refused");
        let job = Job::new(
            JobKind::Move,
            &c.dir,
            vec!["a.jpg".into()],
            &c.dir.join("raw-exports"),
        );
        c.open_dialog(Dialog::Refused {
            job,
            name: "a.jpg".into(),
            can_copy: true,
        });
        assert_eq!(c.focus, 1);
        assert!(matches!(
            c.button(1),
            Action::Run(Job {
                kind: JobKind::Copy,
                ..
            })
        ));
        c.open_dialog(Dialog::Refused {
            job: Job::new(JobKind::Move, &c.dir, vec![], &c.dir),
            name: "a.jpg".into(),
            can_copy: false,
        });
        assert_eq!(c.focus, 0);
        c.key(keys::TAB, false, false);
        assert_eq!(c.focus, 0, "Close is the only button");
    }

    #[test]
    fn names_list_is_capped() {
        let many: Vec<OsString> = (0..7).map(|i| format!("f{i}").into()).collect();
        assert_eq!(names_text(&many), "f0\nf1\nf2\nf3\nf4\nand 2 more");
        assert_eq!(
            names_text(&many[..5]),
            "f0\nf1\nf2\nf3\nf4",
            "five fit without a tail"
        );
        assert_eq!(names_text(&many[..6]).lines().count(), 6);
        assert_eq!(noun(&many[..1]), "“f0”");
    }
}
