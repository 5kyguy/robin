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
use std::sync::{mpsc, Arc};
use std::time::Duration;

use slint::{ComponentHandle, Model, ModelRc, SharedString, VecModel};

use crate::fsops::{
    self, Cancel, Conflict, ConflictChoice, ConflictPrompt, KeptEntry, TrashChoice,
};
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
    /// A move left `names` where they were. Button 1 is Copy instead when
    /// every one of them can be copied.
    Refused {
        job: Job,
        names: Vec<OsString>,
        can_copy: bool,
    },
    /// A paste, drop or move found the name taken. Buttons: 0 Cancel,
    /// 1 Skip, 2 Keep both, 3 Replace, 4 the apply-to-all toggle.
    Conflict {
        ask: ConflictAsk,
        apply_all: bool,
    },
}

/// One name clash the job thread is waiting on.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ConflictAsk {
    pub prompt: ConflictPrompt,
    /// The destination folder's name for the title.
    pub dest: String,
    /// Items left in the job, including this one; the toggle offers to
    /// answer them all.
    pub left: usize,
}

impl Dialog {
    pub fn overlay(&self) -> &'static str {
        match self {
            Dialog::Trash(_) => "trash",
            Dialog::TrashNo(_) => "trash-no",
            Dialog::Delete(_) => "delete",
            Dialog::Refused { .. } => "move-refused",
            Dialog::Conflict { .. } => "conflict-live",
        }
    }

    fn buttons(&self) -> i32 {
        match self {
            Dialog::Refused {
                can_copy: false, ..
            } => 1,
            Dialog::Conflict { .. } => 5,
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
            Dialog::Conflict { .. } => 1,
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
    /// Reply to the job thread's conflict question; `None` cancels the job.
    Answer(Option<ConflictChoice>),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Drag {
    pub names: Vec<OsString>,
    pub target: Option<usize>,
    pub op: Option<Op>,
    /// A folder under the pointer that is one of the dragged items or
    /// inside one: shown muted, never dropped on.
    pub refused: Option<usize>,
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
            _ if matches!(self.dialog, Some(Dialog::Conflict { .. })) => {
                match text.to_ascii_lowercase().as_str() {
                    "s" => self.button(1),
                    "k" => self.button(2),
                    "r" => self.button(3),
                    "a" => self.button(4),
                    _ => Action::None,
                }
            }
            _ => Action::None,
        }
    }

    /// A dialog button: 0 is always Cancel/Close, 1 confirms.
    pub fn button(&mut self, index: i32) -> Action {
        let Some(dialog) = self.dialog.clone() else {
            return Action::None;
        };
        if let Dialog::Conflict { ask, apply_all } = dialog {
            let action = match index {
                0 => None,
                1 => Some(Conflict::Skip),
                2 => Some(Conflict::Rename),
                3 => Some(Conflict::Replace),
                4 => {
                    self.focus = 4;
                    self.dialog = Some(Dialog::Conflict {
                        ask,
                        apply_all: !apply_all,
                    });
                    return Action::Redraw;
                }
                _ => return Action::None,
            };
            self.dialog = None;
            return Action::Answer(action.map(|action| ConflictChoice { action, apply_all }));
        }
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
                names,
                can_copy,
            } if can_copy => Action::Run(Job::new(JobKind::Copy, &job.src, names, &job.dest)),
            Dialog::Refused { .. } | Dialog::Conflict { .. } => Action::Redraw,
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
                let folder = usize::try_from(at).ok().and_then(|t| {
                    let (name, is_dir) = self.row_name(t)?;
                    is_dir.then_some((t, name))
                });
                let mut refused = None;
                let op = folder.as_ref().and_then(|(t, name)| {
                    if names.contains(name) {
                        refused = Some(*t);
                        return None;
                    }
                    let dest = self.dir.join(name);
                    let op = ops::drop_op(mods, ops::same_filesystem(&self.dir, &dest))?;
                    match ops::drop_check(op, &self.dir, &names, &dest) {
                        ops::DropCheck::Ok => Some(op),
                        ops::DropCheck::IntoItself => {
                            refused = Some(*t);
                            None
                        }
                        ops::DropCheck::Nothing => None,
                    }
                });
                let next = Drag {
                    names,
                    target: op.and(folder.map(|f| f.0)),
                    op,
                    refused,
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
        if drag.refused.is_some() {
            return Some(("Can't drop into itself".into(), "󰜺"));
        }
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
        // A cut lives until its move fully succeeds; a refused, failed,
        // cancelled or partial move keeps it so the user can try again.
        if job.kind == JobKind::Move
            && !outcome.cancelled
            && outcome.failed.is_empty()
            && outcome.refused.is_empty()
            && outcome.done == job.names.len()
        {
            self.clip.cut_moved(job);
        }
        if outcome.refused.is_empty() {
            return None;
        }
        Some(Dialog::Refused {
            job: job.clone(),
            names: outcome.refused.iter().map(|r| r.0.clone()).collect(),
            can_copy: refusal_view(&outcome.refused).can_copy,
        })
    }

    pub fn rows(&self) -> Vec<Row> {
        let target = self.drag.as_ref().and_then(|d| d.target);
        let refused = self.drag.as_ref().and_then(|d| d.refused);
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
                drop_refused: refused == Some(i),
            })
            .collect()
    }
}

/// The move-refused dialog's contents for every refused item. One item
/// reads as before; several get a "Can't move N items" title with each
/// item's reason as a heading over its blocking paths.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RefusalView {
    pub title: String,
    /// (path, note, heading) rows.
    pub lines: Vec<(String, String, bool)>,
    pub can_copy: bool,
    pub dismiss: &'static str,
    pub notes: usize,
    pub items: usize,
}

pub fn refusal_view(refused: &[(OsString, Vec<KeptEntry>)]) -> RefusalView {
    let parts: Vec<fsops::Refusal> = refused
        .iter()
        .map(|(n, k)| fsops::refusal(&n.to_string_lossy(), k))
        .collect();
    let can_copy = !parts.is_empty() && parts.iter().all(|r| r.can_copy_instead);
    let rows = |r: &fsops::Refusal| {
        r.lines
            .iter()
            .map(|l| (l.path.clone(), l.note.clone().unwrap_or_default(), false))
            .collect::<Vec<_>>()
    };
    if let [one] = parts.as_slice() {
        return RefusalView {
            title: one.title.clone(),
            lines: rows(one),
            can_copy,
            dismiss: one.dismiss_label,
            notes: one.note_count(),
            items: 1,
        };
    }
    let mut lines = Vec::new();
    let mut notes = 0;
    for r in &parts {
        let head = r.title.strip_prefix("Can't move ").unwrap_or(&r.title);
        lines.push((head.to_string(), String::new(), true));
        lines.extend(rows(r));
        notes += r.note_count();
    }
    RefusalView {
        title: format!("Can't move {}", ops::count(parts.len())),
        lines,
        can_copy,
        dismiss: if can_copy { "Cancel" } else { "Close" },
        notes,
        items: parts.len(),
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
    /// Where the waiting job thread's conflict answer goes.
    reply: Option<mpsc::Sender<Option<ConflictChoice>>>,
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
        reply: None,
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
        // Restoring renames on the job thread like any other job, so a
        // slow drive never stalls the window.
        ui.set_toast_on(false);
        apply(&ui, &st, Action::Run(Job::restore(items)));
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
                Dialog::Conflict { ask, apply_all } => conflict_props(ui, ask, *apply_all),
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
        Action::Answer(choice) => {
            if let Some(tx) = st.borrow_mut().reply.take() {
                let _ = tx.send(choice);
            }
            redraw(ui, &st.borrow());
        }
        Action::CancelJob => {
            if let Some(c) = &st.borrow().cancel {
                c.cancel();
            }
            let pending = {
                let mut s = st.borrow_mut();
                if matches!(s.ctl.dialog, Some(Dialog::Conflict { .. })) {
                    s.ctl.dialog = None;
                }
                s.reply.take()
            };
            if let Some(tx) = pending {
                let _ = tx.send(None);
                redraw(ui, &st.borrow());
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
    let dest = job.dest.file_name().map_or_else(
        || job.dest.display().to_string(),
        |n| n.to_string_lossy().into_owned(),
    );
    std::thread::spawn(move || {
        let total = job.names.len();
        let mut seen = 0usize;
        let mut ask = |prompt: &ConflictPrompt| -> Result<ConflictChoice> {
            seen += 1;
            let (tx, rx) = mpsc::channel();
            let ask = ConflictAsk {
                prompt: prompt.clone(),
                dest: dest.clone(),
                left: total.saturating_sub(seen - 1).max(1),
            };
            let _ = slint::invoke_from_event_loop(move || {
                with_state(|ui, st| ask_conflict(&ui, st, ask, tx));
            });
            // The window answers on the event loop; this thread waits so
            // the transfer resumes exactly where it stopped.
            match rx.recv() {
                Ok(Some(choice)) => Ok(choice),
                _ => Err(Error::Cancelled),
            }
        };
        let outcome = ops::run_asking(
            &job,
            &env,
            &cancel,
            &mut |p: &Progress| {
                let (detail, amount) = (p.detail(), p.amount());
                let _ = slint::invoke_from_event_loop(move || {
                    with_state(|ui, _| {
                        if !ui.get_job_detail().starts_with("Cancelling") {
                            ui.set_job_detail(detail.into());
                        }
                        ui.set_job_amount(amount);
                    });
                });
            },
            &mut ask,
        );
        let _ = slint::invoke_from_event_loop(move || {
            with_state(|ui, st| done(&ui, st, job, outcome));
        });
    });
}

/// Show the job thread's conflict question. Another dialog open means the
/// user is mid-answer elsewhere, so the question waits for it to close.
fn ask_conflict(
    ui: &AppWindow,
    st: &Shared,
    ask: ConflictAsk,
    tx: mpsc::Sender<Option<ConflictChoice>>,
) {
    if st.borrow().ctl.dialog.is_some() {
        let w = ui.as_weak();
        let st = st.clone();
        slint::Timer::single_shot(Duration::from_millis(200), move || {
            if let Some(ui) = w.upgrade() {
                ask_conflict(&ui, &st, ask, tx);
            }
        });
        return;
    }
    st.borrow_mut().reply = Some(tx);
    apply(
        ui,
        st,
        Action::Open(Dialog::Conflict {
            ask,
            apply_all: false,
        }),
    );
}

pub fn conflict_props(ui: &AppWindow, ask: &ConflictAsk, apply_all: bool) {
    let p = &ask.prompt;
    ui.set_conflict_title(format!("{} already has “{}”", ask.dest, p.name).into());
    ui.set_conflict_name(p.name.as_str().into());
    let ext = Path::new(&p.name)
        .extension()
        .map(|e| format!("{} file", e.to_string_lossy().to_uppercase()))
        .unwrap_or_else(|| "File".into());
    ui.set_conflict_kind(ext.into());
    ui.set_conflict_existing_size(human(p.existing_size).into());
    ui.set_conflict_incoming_size(human(p.incoming_size).into());
    ui.set_conflict_existing_date(fsops::short_date(p.existing_mtime).into());
    ui.set_conflict_incoming_date(fsops::short_date(p.incoming_mtime).into());
    let (existing, incoming) = side_badges(p);
    let at = |v: &[&'static str], i: usize| SharedString::from(v.get(i).copied().unwrap_or(""));
    ui.set_conflict_existing_badge(at(&existing, 0));
    ui.set_conflict_existing_badge2(at(&existing, 1));
    ui.set_conflict_incoming_badge(at(&incoming, 0));
    ui.set_conflict_incoming_badge2(at(&incoming, 1));
    ui.set_conflict_hint(format!("Keep both saves it as {}", keep_both_name(&p.name)).into());
    ui.set_conflict_apply_label(
        if ask.left > 1 {
            format!("Apply to the rest ({} left)", ask.left - 1)
        } else {
            "Apply to the rest of this job".into()
        }
        .into(),
    );
    ui.set_conflict_apply(apply_all);
}

/// ("Larger", "Newer") badges for the (existing, incoming) side.
pub fn conflict_badges(
    p: &ConflictPrompt,
) -> ((&'static str, &'static str), (&'static str, &'static str)) {
    let larger = match p.existing_size.cmp(&p.incoming_size) {
        std::cmp::Ordering::Greater => ("Larger", ""),
        std::cmp::Ordering::Less => ("", "Larger"),
        std::cmp::Ordering::Equal => ("", ""),
    };
    let newer = match p.existing_mtime.cmp(&p.incoming_mtime) {
        std::cmp::Ordering::Greater => ("Newer", ""),
        std::cmp::Ordering::Less => ("", "Newer"),
        std::cmp::Ordering::Equal => ("", ""),
    };
    (larger, newer)
}

/// Every badge each side earns, in reading order: a file both newer and
/// larger shows "Newer" and "Larger" side by side.
pub fn side_badges(p: &ConflictPrompt) -> (Vec<&'static str>, Vec<&'static str>) {
    let (larger, newer) = conflict_badges(p);
    let keep = |v: [&'static str; 2]| v.into_iter().filter(|b| !b.is_empty()).collect();
    (keep([newer.0, larger.0]), keep([newer.1, larger.1]))
}

/// The first name Keep both tries: "photo (2).jpg".
pub fn keep_both_name(name: &str) -> String {
    match name.rfind('.') {
        Some(i) if i > 0 => format!("{} (2){}", &name[..i], &name[i..]),
        _ => format!("{name} (2)"),
    }
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
        if matches!(d, Dialog::Refused { .. }) {
            set_refusal(ui, &refusal_view(&outcome.refused));
        }
        apply(ui, st, Action::Open(d));
    }
    pump(ui, st);
}

pub fn set_refusal(ui: &AppWindow, r: &RefusalView) {
    ui.set_refusal_title(r.title.as_str().into());
    ui.set_refusal_can_copy(r.can_copy);
    ui.set_refusal_dismiss(r.dismiss.into());
    ui.set_refusal_notes(r.notes as i32);
    ui.set_refusal_lines_items(r.items as i32);
    ui.set_refusal_lines(ModelRc::new(Rc::new(VecModel::from(
        r.lines
            .iter()
            .map(|(path, note, head)| crate::RefusalLine {
                path: path.as_str().into(),
                note: note.as_str().into(),
                head: *head,
            })
            .collect::<Vec<_>>(),
    ))));
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
        assert!(c.clip.clip().is_some(), "paste start keeps the cut");
        c.finish(
            &job,
            &Outcome {
                done: 1,
                ..Outcome::default()
            },
        );
        assert!(c.clip.clip().is_none(), "a successful move uses the cut up");
        assert_eq!(c.key("v", true, false), Action::None);
    }

    #[test]
    fn a_cut_survives_refused_failed_and_same_folder_pastes() {
        let mut c = setup("cut-kept");
        select(&mut c, "a.jpg");
        c.key("x", true, false);
        // Same folder: nothing runs, the cut stays.
        assert_eq!(c.key("v", true, false), Action::None);
        assert!(c.clip.clip().is_some());
        c.dir = c.dir.join("raw-exports");
        let Action::Run(job) = c.key("v", true, false) else {
            panic!("paste should run a job")
        };
        let failed = Outcome {
            failed: vec!["a.jpg: disk full".into()],
            ..Outcome::default()
        };
        c.finish(&job, &failed);
        assert!(c.clip.clip().is_some(), "a failed move keeps the cut");
        let refused = Outcome {
            refused: vec![("a.jpg".into(), Vec::new())],
            ..Outcome::default()
        };
        c.finish(&job, &refused);
        assert!(c.clip.clip().is_some(), "a refused move keeps the cut");
        let cancelled = Outcome {
            cancelled: true,
            ..Outcome::default()
        };
        c.finish(&job, &cancelled);
        assert!(c.clip.clip().is_some(), "a cancelled move keeps the cut");
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
        let back = ops::run(
            &Job::restore(out.trashed.clone()),
            &c.env,
            &Cancel::new(),
            &mut |_| {},
        );
        assert_eq!((back.done, back.failed.len()), (1, 0));
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
            names: vec!["a.jpg".into()],
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
            names: vec!["a.jpg".into()],
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

    fn prompt() -> ConflictPrompt {
        ConflictPrompt {
            name: "dune.jpg".into(),
            existing_size: 6_200_000,
            existing_mtime: 100,
            incoming_size: 4_100_000,
            incoming_mtime: 200,
        }
    }

    fn conflict(c: &mut Controller) {
        c.open_dialog(Dialog::Conflict {
            ask: ConflictAsk {
                prompt: prompt(),
                dest: "SANDISK".into(),
                left: 3,
            },
            apply_all: false,
        });
    }

    #[test]
    fn conflict_dialog_is_keyboard_operable() {
        let mut c = setup("conflict-keys");
        conflict(&mut c);
        assert_eq!(c.focus, 1, "Skip is the default");
        assert_eq!(
            c.key(keys::RETURN, false, false),
            Action::Answer(Some(ConflictChoice {
                action: Conflict::Skip,
                apply_all: false
            }))
        );
        conflict(&mut c);
        c.key(keys::TAB, false, false);
        c.key(keys::TAB, false, false);
        c.key(keys::TAB, false, false);
        assert_eq!(c.focus, 4);
        c.key(" ", false, false);
        assert!(matches!(
            c.dialog,
            Some(Dialog::Conflict {
                apply_all: true,
                ..
            })
        ));
        c.key(keys::LEFT, false, false);
        assert_eq!(
            c.key(keys::RETURN, false, false),
            Action::Answer(Some(ConflictChoice {
                action: Conflict::Replace,
                apply_all: true
            }))
        );
        conflict(&mut c);
        assert_eq!(
            c.key("k", false, false),
            Action::Answer(Some(ConflictChoice {
                action: Conflict::Rename,
                apply_all: false
            }))
        );
        conflict(&mut c);
        assert_eq!(c.key(keys::ESCAPE, false, false), Action::Answer(None));
        assert!(c.dialog.is_none());
    }

    #[test]
    fn conflict_badges_and_names() {
        assert_eq!(conflict_badges(&prompt()), (("Larger", ""), ("", "Newer")));
        assert_eq!(side_badges(&prompt()), (vec!["Larger"], vec!["Newer"]));
        let both = ConflictPrompt {
            incoming_size: 9_000_000,
            ..prompt()
        };
        assert_eq!(side_badges(&both), (vec![], vec!["Newer", "Larger"]));
        let same = ConflictPrompt {
            incoming_size: 6_200_000,
            incoming_mtime: 100,
            ..prompt()
        };
        assert_eq!(side_badges(&same), (vec![], vec![]));
        assert_eq!(keep_both_name("dune.jpg"), "dune (2).jpg");
        assert_eq!(keep_both_name(".bashrc"), ".bashrc (2)");
    }

    #[test]
    fn every_refused_item_is_listed_and_copied() {
        let mut c = setup("refused-many");
        let job = Job::new(
            JobKind::Move,
            &c.dir,
            vec!["a.jpg".into(), "b.jpg".into()],
            &c.dir.join("raw-exports"),
        );
        let kept = |p: &str| {
            vec![KeptEntry {
                path: p.into(),
                reason: fsops::KeepReason::ReadOnlyFolder,
            }]
        };
        let outcome = Outcome {
            refused: vec![
                ("a.jpg".into(), kept("a.jpg")),
                ("b.jpg".into(), kept("b.jpg")),
            ],
            ..Outcome::default()
        };
        let view = refusal_view(&outcome.refused);
        assert_eq!(view.items, 2);
        assert!(view.title.contains("2 items"), "{}", view.title);
        assert_eq!(view.lines.iter().filter(|l| l.2).count(), 2);
        let d = c.finish(&job, &outcome).unwrap();
        let Dialog::Refused { names, .. } = &d else {
            panic!()
        };
        assert_eq!(names.len(), 2);
        c.open_dialog(d.clone());
        if let Dialog::Refused { can_copy: true, .. } = d {
            let Action::Run(copy) = c.button(1) else {
                panic!()
            };
            assert_eq!(copy.names.len(), 2);
        }
    }

    #[test]
    fn dragging_onto_itself_is_refused_and_labelled() {
        let mut c = setup("drag-self");
        let i = c
            .listing
            .view()
            .iter()
            .position(|e| e.name == "raw-exports")
            .unwrap();
        c.listing.select_only("raw-exports".into());
        c.pointer(i, 0, 0, false, Mods::default());
        c.pointer(i, 1, 0, true, Mods::default());
        let d = c.drag.clone().unwrap();
        assert_eq!((d.target, d.refused), (None, Some(i)));
        assert_eq!(c.drag_label().unwrap().0, "Can't drop into itself");
        let row = &c.rows()[i];
        assert!(row.drop_refused && !row.drop);
    }
}
