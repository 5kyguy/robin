//! Toast stack: sticky (no-timer) toasts persist under newer ones;
//! ordinary timed toasts replace each other.
//!
//! A case-only rename that cannot put the original name back names the
//! hidden file on a sticky toast. A later toast must not take that name
//! with it: it sits on top, and when it closes the sticky toast is still
//! there. Only [Dismiss] removes a sticky toast. The stack keeps every
//! sticky toast, newest on top, so a second stranded rename or a hung
//! drop cannot wipe an earlier hidden name.
//!
//! At most [`FULL_STICKY_TOASTS`] sticky toasts are drawn in full (or
//! [`SHORT_STICKY_TOASTS`] when the window is shorter than
//! [`SHORT_WINDOW`]). Older ones collapse into a "+N more" line under
//! the stack; expanding that line shows the full list, with a "Show
//! fewer" line to collapse it again. The expanded stack is capped at
//! [`stack_room`] and the parked list scrolls under the current toast.

/// One toast's content, as bound to the window.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Toast {
    /// "" for neutral, "warning", or "danger".
    pub tone: String,
    pub text: String,
    pub details: Vec<String>,
    /// The first detail stays at the top of the list, above any "+N more".
    pub pinned: bool,
    pub action: String,
    pub action2: String,
    pub action2_danger: bool,
    /// The details panel is open (pinned line and list are showing).
    pub open: bool,
    /// "+N more" has been expanded.
    pub expanded: bool,
    /// No 4s/8s timer: only [Dismiss] (or this toast's own action) removes it.
    pub sticky: bool,
    /// The hung-drop "waiting to drop…" toast, cleared when the wait ends.
    pub drop_wait: bool,
}

/// How many sticky toasts are drawn in full in a tall window.
pub const FULL_STICKY_TOASTS: usize = 3;

/// How many sticky toasts are drawn in full when the window is shorter
/// than [`SHORT_WINDOW`].
pub const SHORT_STICKY_TOASTS: usize = 1;

/// Window height, in logical pixels, below which only one sticky toast
/// is drawn in full.
pub const SHORT_WINDOW: i32 = 600;

/// Gap kept below the toolbar and above the status bar or progress strip.
pub const STACK_CLEARANCE: i32 = 16;

/// The muted line under an expanded sticky list.
pub const SHOW_FEWER: &str = "Show fewer";

/// The toast showing now, and any number of sticky toasts parked under
/// it, oldest first. Closing the current toast brings back the newest
/// parked one.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ToastStack {
    current: Option<Toast>,
    parked: Vec<Toast>,
    gen: u64,
    /// The "+N more" line has been expanded to the full sticky list.
    expanded: bool,
}

impl ToastStack {
    pub fn current(&self) -> Option<&Toast> {
        self.current.as_ref()
    }

    /// Every parked sticky toast, oldest first, newest last (nearest
    /// the current toast).
    pub fn parked(&self) -> &[Toast] {
        &self.parked
    }

    /// The newest parked sticky toast, if any.
    pub fn under(&self) -> Option<&Toast> {
        self.parked.last()
    }

    pub fn gen(&self) -> u64 {
        self.gen
    }

    pub fn stack_expanded(&self) -> bool {
        self.expanded
    }

    /// Show or hide the full sticky list under the "+N more" line.
    pub fn set_stack_expanded(&mut self, expanded: bool) {
        self.expanded = expanded;
    }

    /// How many sticky toasts are drawn in full when the list is not
    /// expanded, at this window height.
    pub fn unexpanded_cap(&self, window_height: i32) -> usize {
        if window_height < SHORT_WINDOW {
            SHORT_STICKY_TOASTS
        } else {
            FULL_STICKY_TOASTS
        }
    }

    /// The muted line under the parked stack: "+N more", "Show fewer",
    /// or empty when every sticky toast is already shown in full.
    pub fn footer(&self, window_height: i32) -> String {
        if self.expanded {
            if self.sticky_count() > self.unexpanded_cap(window_height) {
                SHOW_FEWER.into()
            } else {
                String::new()
            }
        } else {
            match self.collapsed_count(window_height) {
                0 => String::new(),
                n => format!("+{n} more"),
            }
        }
    }

    /// Sticky toasts in the stack: the current one when it is sticky,
    /// plus every parked toast.
    pub fn sticky_count(&self) -> usize {
        self.parked.len() + usize::from(self.current.as_ref().is_some_and(|t| t.sticky))
    }

    /// How many sticky toasts may be drawn in full at this window height.
    pub fn sticky_cap(&self, window_height: i32) -> usize {
        if self.expanded {
            usize::MAX
        } else if window_height < SHORT_WINDOW {
            SHORT_STICKY_TOASTS
        } else {
            FULL_STICKY_TOASTS
        }
    }

    /// How many older sticky toasts are collapsed into "+N more".
    pub fn collapsed_count(&self, window_height: i32) -> usize {
        self.sticky_count()
            .saturating_sub(self.sticky_cap(window_height))
    }

    /// Parked toasts drawn in full, oldest of that slice first. A timed
    /// current toast is not counted in the cap; a sticky current toast
    /// takes one of the slots. Older ones stay in the stack.
    pub fn visible_parked(&self, window_height: i32) -> &[Toast] {
        let cap = self.sticky_cap(window_height);
        let parked_cap = if self.current.as_ref().is_some_and(|t| t.sticky) {
            cap.saturating_sub(1)
        } else {
            cap
        };
        let n = self.parked.len();
        let show = parked_cap.min(n);
        &self.parked[n - show..]
    }

    /// Show a new toast. A sticky toast that is showing is parked
    /// underneath; a timed one is replaced. Already-parked sticky toasts
    /// stay parked when one timed toast replaces another.
    pub fn push(&mut self, toast: Toast) -> u64 {
        if let Some(cur) = self.current.take() {
            if cur.sticky {
                self.parked.push(cur);
            }
        }
        self.gen = self.gen.saturating_add(1);
        self.current = Some(toast);
        self.gen
    }

    /// The current toast closed (its timer ran out, or its action hid it).
    /// The newest parked sticky toast comes back as the current one.
    pub fn expire(&mut self) -> u64 {
        self.current = self.parked.pop();
        self.gen = self.gen.saturating_add(1);
        self.gen
    }

    /// Hide the current toast if it is still this generation and is not
    /// sticky. Returns whether the stack changed.
    pub fn expire_if(&mut self, gen: u64) -> bool {
        if self.gen != gen {
            return false;
        }
        if self.current.as_ref().is_some_and(|t| t.sticky) {
            return false;
        }
        if self.current.as_ref().is_some_and(|t| t.open) {
            return false;
        }
        self.expire();
        true
    }

    /// [Dismiss] on the current sticky toast. The newest parked one
    /// comes back.
    pub fn dismiss(&mut self) -> u64 {
        self.current = self.parked.pop();
        self.gen = self.gen.saturating_add(1);
        self.gen
    }

    /// [Dismiss] on a visible parked toast, `index` being 0 for the
    /// newest one currently drawn (nearest the current toast). The
    /// current toast and its timer are left alone.
    pub fn dismiss_visible(&mut self, index: usize, window_height: i32) {
        let n_vis = self.visible_parked(window_height).len();
        if index < n_vis {
            let at = self.parked.len() - n_vis + (n_vis - 1 - index);
            self.parked.remove(at);
        }
    }

    /// Open or close the current toast's details panel.
    pub fn set_open(&mut self, open: bool) {
        if let Some(t) = &mut self.current {
            t.open = open;
        }
    }

    /// Expand the current toast's "+N more" list.
    pub fn set_expanded(&mut self, expanded: bool) {
        if let Some(t) = &mut self.current {
            t.expanded = expanded;
        }
    }

    /// Drop any toast matching `pred`. If that toast is current, the
    /// newest parked sticky one comes back. The current toast's
    /// generation is bumped only when the current toast is dropped, so a
    /// running timer still matches if only a parked toast was removed.
    pub fn drop_matching(&mut self, pred: impl Fn(&Toast) -> bool) {
        self.parked.retain(|t| !pred(t));
        if self.current.as_ref().is_some_and(pred) {
            self.current = self.parked.pop();
            self.gen = self.gen.saturating_add(1);
        }
    }
}

/// Max height of the whole toast stack (the current toast, the parked
/// list, and the footer), leaving [`STACK_CLEARANCE`] below the toolbar
/// and above the status bar or progress strip.
pub fn stack_room(window_height: i32, toolbar_h: i32, toast_floor: i32) -> i32 {
    (window_height - toolbar_h - toast_floor - STACK_CLEARANCE - STACK_CLEARANCE).max(0)
}

/// Max height of the parked list, including its footer, when a current
/// toast of `current_h` sits 8px above it.
pub fn parked_budget(room: i32, current_h: i32) -> i32 {
    if current_h > 0 {
        (room - current_h - 8).max(0)
    } else {
        room
    }
}

/// A danger sticky toast like the stranded-rename one, for tests and
/// render scenes.
pub fn sticky(text: &str, pin: &str, action: &str) -> Toast {
    Toast {
        tone: "danger".into(),
        text: text.into(),
        details: vec![pin.into()],
        pinned: true,
        action: action.into(),
        open: true,
        sticky: true,
        ..Toast::default()
    }
}

/// A short timed toast, such as "Copied 1 path".
pub fn timed(text: &str) -> Toast {
    Toast {
        text: text.into(),
        ..Toast::default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const HEAD: &str = "Couldn't finish renaming. It's now this hidden file:";
    const HIDDEN: &str = ".robin-rename-4127-0-dune-sunset-0412.jpg";
    const HIDDEN_B: &str = ".robin-rename-4127-1-dune-sunset-0413.jpg";
    const TALL: i32 = 800;
    const SHORT: i32 = 599;

    fn stranded() -> Toast {
        sticky(HEAD, HIDDEN, "Dismiss")
    }

    fn stranded_b() -> Toast {
        sticky(HEAD, HIDDEN_B, "Dismiss")
    }

    fn waiting() -> Toast {
        Toast {
            tone: "info".into(),
            text: "Waiting to drop into Pictures…".into(),
            action: "Cancel".into(),
            sticky: true,
            drop_wait: true,
            ..Toast::default()
        }
    }

    fn five_stranded(s: &mut ToastStack) {
        for i in 0..5 {
            s.push(sticky(HEAD, &format!("file-{i}.jpg"), "Dismiss"));
        }
    }

    #[test]
    fn a_newer_toast_sits_on_a_sticky_one() {
        let mut s = ToastStack::default();
        s.push(stranded());
        assert_eq!(s.current().unwrap().text, HEAD);
        assert_eq!(s.current().unwrap().details, [HIDDEN]);
        assert!(s.parked().is_empty());

        s.push(timed("Copied 1 path"));
        assert_eq!(s.current().unwrap().text, "Copied 1 path");
        assert!(!s.current().unwrap().sticky);
        let under = s.under().unwrap();
        assert_eq!(under.text, HEAD);
        assert_eq!(under.details, [HIDDEN]);
        assert!(under.open && under.sticky);
    }

    #[test]
    fn the_sticky_toast_comes_back_when_the_newer_one_closes() {
        let mut s = ToastStack::default();
        let g0 = s.push(stranded());
        let g1 = s.push(timed("Copied 1 path"));
        assert_ne!(g0, g1);
        assert!(s.expire_if(g1));
        assert_eq!(s.current().unwrap().text, HEAD);
        assert_eq!(s.current().unwrap().details, [HIDDEN]);
        assert!(s.current().unwrap().open && s.current().unwrap().sticky);
        assert!(s.parked().is_empty());
    }

    #[test]
    fn only_dismiss_removes_a_sticky_toast() {
        let mut s = ToastStack::default();
        let g = s.push(stranded());
        assert!(!s.expire_if(g), "a sticky toast has no timer");
        assert_eq!(s.current().unwrap().text, HEAD);

        s.push(timed("Copied 1 path"));
        s.expire();
        assert_eq!(s.current().unwrap().text, HEAD);
        s.dismiss();
        assert!(s.current().is_none());
        assert!(s.parked().is_empty());
    }

    #[test]
    fn a_timed_toast_replaces_another_and_leaves_the_sticky_underneath() {
        let mut s = ToastStack::default();
        s.push(stranded());
        let g1 = s.push(timed("Copied 1 path"));
        let g2 = s.push(timed("SANDISK connected · 29.8 GB"));
        assert_ne!(g1, g2);
        assert_eq!(s.current().unwrap().text, "SANDISK connected · 29.8 GB");
        assert_eq!(s.under().unwrap().text, HEAD);
        assert_eq!(s.under().unwrap().details, [HIDDEN]);
        // The first timed toast's timer must not hide the second, or the sticky one.
        assert!(!s.expire_if(g1));
        assert_eq!(s.current().unwrap().text, "SANDISK connected · 29.8 GB");
        s.expire();
        assert_eq!(s.current().unwrap().text, HEAD);
    }

    #[test]
    fn dismissing_the_under_toast_leaves_the_newer_one() {
        let mut s = ToastStack::default();
        s.push(stranded());
        let g = s.push(timed("Copied 1 path"));
        s.dismiss_visible(0, TALL);
        assert!(s.parked().is_empty());
        assert_eq!(s.current().unwrap().text, "Copied 1 path");
        assert_eq!(s.gen(), g, "the newer toast's timer still matches");
        s.expire();
        assert!(s.current().is_none());
    }

    #[test]
    fn an_open_timed_toast_is_not_hidden_by_its_timer() {
        let mut s = ToastStack::default();
        let mut t = timed("Copied 8 items, 2 failed");
        t.open = true;
        t.details = vec!["a".into(), "b".into()];
        let g = s.push(t);
        assert!(!s.expire_if(g));
        s.set_open(false);
        assert!(s.expire_if(g));
        assert!(s.current().is_none());
    }

    #[test]
    fn dropping_a_hung_drop_toast_does_not_take_a_newer_one_with_it() {
        let mut s = ToastStack::default();
        s.push(waiting());
        s.push(timed("Copied 1 path"));
        s.drop_matching(|t| t.drop_wait);
        assert!(s.parked().is_empty());
        assert_eq!(s.current().unwrap().text, "Copied 1 path");
    }

    #[test]
    fn two_stranded_toasts_are_both_kept_under_a_newer_one() {
        let mut s = ToastStack::default();
        s.push(stranded());
        s.push(stranded_b());
        assert_eq!(s.current().unwrap().details, [HIDDEN_B]);
        assert_eq!(s.parked().len(), 1);
        assert_eq!(s.parked()[0].details, [HIDDEN]);

        s.push(timed("Copied 1 path"));
        assert_eq!(s.current().unwrap().text, "Copied 1 path");
        assert_eq!(s.parked().len(), 2);
        assert_eq!(s.parked()[0].details, [HIDDEN]);
        assert_eq!(s.parked()[1].details, [HIDDEN_B]);

        s.expire();
        assert_eq!(s.current().unwrap().details, [HIDDEN_B]);
        s.dismiss();
        assert_eq!(s.current().unwrap().details, [HIDDEN]);
        s.dismiss();
        assert!(s.current().is_none());
        assert!(s.parked().is_empty());
    }

    #[test]
    fn dismissing_the_top_parked_toast_leaves_the_older_one() {
        let mut s = ToastStack::default();
        s.push(stranded());
        s.push(stranded_b());
        s.push(timed("Copied 1 path"));
        s.dismiss_visible(0, TALL);
        assert_eq!(s.current().unwrap().text, "Copied 1 path");
        assert_eq!(s.parked().len(), 1);
        assert_eq!(s.parked()[0].details, [HIDDEN]);
    }

    #[test]
    fn a_stranded_toast_under_a_waiting_toast_is_not_lost() {
        let mut s = ToastStack::default();
        s.push(stranded());
        s.push(waiting());
        assert_eq!(s.current().unwrap().text, "Waiting to drop into Pictures…");
        assert_eq!(s.parked()[0].details, [HIDDEN]);

        s.push(timed("Copied 1 path"));
        assert_eq!(s.current().unwrap().text, "Copied 1 path");
        assert_eq!(s.parked().len(), 2);
        assert_eq!(s.parked()[0].details, [HIDDEN]);
        assert!(s.parked()[1].drop_wait);

        s.drop_matching(|t| t.drop_wait);
        assert_eq!(s.current().unwrap().text, "Copied 1 path");
        assert_eq!(s.parked().len(), 1);
        assert_eq!(s.parked()[0].details, [HIDDEN]);
    }

    #[test]
    fn collapse_count_at_full_height_hides_older_stickies() {
        let mut s = ToastStack::default();
        five_stranded(&mut s);
        s.push(timed("Copied 1 path"));
        assert_eq!(s.parked().len(), 5);
        assert_eq!(s.visible_parked(TALL).len(), 3);
        assert_eq!(s.collapsed_count(TALL), 2);
        assert_eq!(s.visible_parked(TALL)[0].details, ["file-2.jpg"]);
        assert_eq!(s.visible_parked(TALL)[2].details, ["file-4.jpg"]);
        // A timed toast is not one of the three sticky slots.
        assert_eq!(s.collapsed_count(600), 2);
    }

    #[test]
    fn collapse_count_under_600_shows_one_sticky() {
        let mut s = ToastStack::default();
        five_stranded(&mut s);
        s.push(timed("Copied 1 path"));
        assert_eq!(s.visible_parked(SHORT).len(), 1);
        assert_eq!(s.collapsed_count(SHORT), 4);
        assert_eq!(s.visible_parked(SHORT)[0].details, ["file-4.jpg"]);
        // 600px is not under 600, so the tall cap still applies.
        assert_eq!(s.visible_parked(600).len(), 3);
        assert_eq!(s.collapsed_count(600), 2);
    }

    #[test]
    fn expanding_the_list_shows_every_sticky_toast() {
        let mut s = ToastStack::default();
        five_stranded(&mut s);
        s.push(timed("Copied 1 path"));
        s.set_stack_expanded(true);
        assert_eq!(s.visible_parked(TALL).len(), 5);
        assert_eq!(s.collapsed_count(TALL), 0);
        assert_eq!(s.visible_parked(SHORT).len(), 5);
        assert_eq!(s.collapsed_count(SHORT), 0);
        s.dismiss_visible(0, TALL);
        assert_eq!(s.current().unwrap().text, "Copied 1 path");
        assert_eq!(s.parked().len(), 4);
        assert_eq!(s.parked()[3].details, ["file-3.jpg"]);
    }

    #[test]
    fn collapsing_the_list_restores_the_plus_more_line() {
        let mut s = ToastStack::default();
        five_stranded(&mut s);
        s.push(timed("Copied 1 path"));
        assert_eq!(s.footer(TALL), "+2 more");
        s.set_stack_expanded(true);
        assert_eq!(s.footer(TALL), SHOW_FEWER);
        assert_eq!(s.visible_parked(TALL).len(), 5);
        s.set_stack_expanded(false);
        assert_eq!(s.footer(TALL), "+2 more");
        assert_eq!(s.visible_parked(TALL).len(), 3);
        assert_eq!(s.collapsed_count(TALL), 2);
        assert_eq!(s.parked().len(), 5);
    }

    #[test]
    fn the_expanded_stack_is_capped_under_the_toolbar() {
        // Comfortable density at 1280×800: 46px toolbar, 26px status bar.
        assert_eq!(stack_room(800, 46, 26), 696);
        assert_eq!(parked_budget(696, 52), 636);
        // A shorter window tightens the cap; resize re-applies it.
        assert_eq!(stack_room(700, 46, 26), 596);
        assert!(stack_room(700, 46, 26) < stack_room(800, 46, 26));
        // Eight open sticky toasts plus the footer do not fit in the
        // parked budget, so the list must scroll; they all stay in the
        // model.
        const CARD: i32 = 96;
        const FOOTER: i32 = 28;
        let cards = 8 * CARD + 7 * 8;
        assert!(cards + FOOTER > parked_budget(stack_room(800, 46, 26), 52));
        let mut s = ToastStack::default();
        for i in 0..8 {
            s.push(sticky(HEAD, &format!("file-{i}.jpg"), "Dismiss"));
        }
        s.push(timed("Copied 1 path"));
        s.set_stack_expanded(true);
        assert_eq!(s.visible_parked(TALL).len(), 8);
        assert_eq!(s.footer(TALL), SHOW_FEWER);
    }
}
