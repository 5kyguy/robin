//! Toast stack: sticky (no-timer) toasts persist under newer ones;
//! ordinary timed toasts replace each other.
//!
//! A case-only rename that cannot put the original name back names the
//! hidden file on a sticky toast. A later toast must not take that name
//! with it: it sits on top, and when it closes the sticky toast is still
//! there. Only [Dismiss] removes a sticky toast.

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

/// The toast showing now, and at most one sticky toast parked under it.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ToastStack {
    current: Option<Toast>,
    under: Option<Toast>,
    gen: u64,
}

impl ToastStack {
    pub fn current(&self) -> Option<&Toast> {
        self.current.as_ref()
    }

    pub fn under(&self) -> Option<&Toast> {
        self.under.as_ref()
    }

    pub fn gen(&self) -> u64 {
        self.gen
    }

    /// Show a new toast. A sticky toast that is showing is parked
    /// underneath; a timed one is replaced. An already-parked sticky
    /// toast stays parked when one timed toast replaces another.
    pub fn push(&mut self, toast: Toast) -> u64 {
        if let Some(cur) = self.current.take() {
            if cur.sticky {
                self.under = Some(cur);
            }
        }
        self.gen = self.gen.saturating_add(1);
        self.current = Some(toast);
        self.gen
    }

    /// The current toast closed (its timer ran out, or its action hid it).
    /// A parked sticky toast comes back as the current one.
    pub fn expire(&mut self) -> u64 {
        self.current = self.under.take();
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

    /// [Dismiss] on the current sticky toast. A parked one comes back.
    pub fn dismiss(&mut self) -> u64 {
        self.current = self.under.take();
        self.gen = self.gen.saturating_add(1);
        self.gen
    }

    /// [Dismiss] on the parked sticky toast, while a newer one is showing.
    /// The current toast and its timer are left alone.
    pub fn dismiss_under(&mut self) {
        self.under = None;
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

    /// Drop any toast matching `pred`. If that toast is current, a parked
    /// sticky one comes back. The current toast's generation is bumped
    /// only when the current toast is dropped, so a running timer still
    /// matches if only the parked toast was removed.
    pub fn drop_matching(&mut self, pred: impl Fn(&Toast) -> bool) {
        if self.under.as_ref().is_some_and(&pred) {
            self.under = None;
        }
        if self.current.as_ref().is_some_and(pred) {
            self.current = self.under.take();
            self.gen = self.gen.saturating_add(1);
        }
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

    fn stranded() -> Toast {
        sticky(HEAD, HIDDEN, "Dismiss")
    }

    #[test]
    fn a_newer_toast_sits_on_a_sticky_one() {
        let mut s = ToastStack::default();
        s.push(stranded());
        assert_eq!(s.current().unwrap().text, HEAD);
        assert_eq!(s.current().unwrap().details, [HIDDEN]);
        assert!(s.under().is_none());

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
        assert!(s.under().is_none());
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
        assert!(s.under().is_none());
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
        s.dismiss_under();
        assert!(s.under().is_none());
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
        let mut wait = timed("Waiting to drop into Pictures…");
        wait.sticky = true;
        wait.drop_wait = true;
        wait.action = "Cancel".into();
        s.push(wait);
        s.push(timed("Copied 1 path"));
        s.drop_matching(|t| t.drop_wait);
        assert!(s.under().is_none());
        assert_eq!(s.current().unwrap().text, "Copied 1 path");
    }
}
