//! Which window is the live Media Manager (the undocked playlist window).
//!
//! `destroy()` asks the OS to tear the window down, but the window can take a
//! while to go away (WebView2 on Windows has been seen to take over 2 s). Main
//! waits up to 2 s for `sw://closed` and then reopens. Without this slot that
//! reopen would find the old window by label and focus it while it is dying.
//! With it, a window that was asked to close is never focused again: the
//! reopen builds a new window under a fresh label, and the old window can no
//! longer close the new one or tell main the new one closed.
//!
//! Labels are a fixed set (`playlist`, `playlist-2` ... `playlist-9`), listed
//! one by one in `capabilities/default.json`, so no other label gets IPC.
//!
//! Pure bookkeeping with no Tauri types, so it can be unit tested anywhere.

/// Every label a Media Manager window may have, in the order they are used.
pub const LABELS: [&str; 9] = [
    "playlist",
    "playlist-2",
    "playlist-3",
    "playlist-4",
    "playlist-5",
    "playlist-6",
    "playlist-7",
    "playlist-8",
    "playlist-9",
];

/// True only for one of the fixed Media Manager labels.
pub fn is_second_window_label(label: &str) -> bool {
    LABELS.contains(&label)
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OpenAction {
    /// The live window exists: show and focus it.
    Focus(String),
    /// Build a new window with this label.
    Build(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OpenError {
    /// All of `LABELS` belong to windows that are still closing.
    AllLabelsBusy,
}

impl OpenError {
    pub fn message(&self) -> &'static str {
        match self {
            OpenError::AllLabelsBusy => {
                "Earlier Media Manager windows are still closing. Try again in a few seconds."
            }
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct CloseOutcome {
    /// The window to destroy, if any.
    pub destroy: Option<String>,
    /// Tell main `sw://closed` now: the live window was already gone, so no
    /// `Destroyed` event will come.
    pub report_closed: bool,
}

#[derive(Debug, Default)]
pub struct SecondWindowSlot {
    /// The window main treats as its Media Manager. It is never in `dying`:
    /// `on_close_requested_by` takes it out before marking it.
    current: Option<String>,
    /// Windows that were asked to close and have not reported `Destroyed` yet.
    dying: Vec<String>,
}

impl SecondWindowSlot {
    pub fn new() -> Self {
        Self::default()
    }

    /// The live window, if any.
    pub fn current(&self) -> Option<&str> {
        self.current.as_deref()
    }

    pub fn is_dying(&self, label: &str) -> bool {
        self.dying.iter().any(|l| l == label)
    }

    /// Labels of windows that are still closing.
    pub fn dying(&self) -> &[String] {
        &self.dying
    }

    /// Open (or reopen). `exists(label)` says whether a window with that label
    /// is still registered. Labels of closing windows that are already gone are
    /// dropped first. A closing window is never focused: a reopen while it is
    /// still registered gets the next free label, or `AllLabelsBusy`.
    pub fn on_open(&mut self, exists: impl Fn(&str) -> bool) -> Result<OpenAction, OpenError> {
        self.dying.retain(|l| exists(l));
        if let Some(cur) = self.current.clone() {
            if exists(&cur) {
                return Ok(OpenAction::Focus(cur));
            }
        }
        let label = LABELS
            .iter()
            .find(|l| !exists(l) && !self.is_dying(l))
            .ok_or(OpenError::AllLabelsBusy)?;
        self.current = Some(label.to_string());
        Ok(OpenAction::Build(label.to_string()))
    }

    /// The window to focus for `focus_playlist_window`, or None when the live
    /// window is gone or closing.
    pub fn focus_target(&self, exists: impl Fn(&str) -> bool) -> Option<String> {
        self.current.as_deref().filter(|l| exists(l)).map(str::to_string)
    }

    /// A close request. `caller` is the Media Manager label that asked, or None
    /// for main. A Media Manager window that is not the live one (for example
    /// an old one whose Dock-back fallback fires late) only closes itself. Main
    /// or the live window closes the live window. A label with no window is not
    /// kept as closing, since `Destroyed` will never come for it.
    pub fn on_close_requested_by(
        &mut self,
        caller: Option<&str>,
        exists: impl Fn(&str) -> bool,
    ) -> CloseOutcome {
        if let Some(c) = caller {
            if self.current.as_deref() != Some(c) {
                if exists(c) && !self.is_dying(c) {
                    self.dying.push(c.to_string());
                }
                return CloseOutcome { destroy: exists(c).then(|| c.to_string()), report_closed: false };
            }
        }
        let Some(label) = self.current.take() else {
            return CloseOutcome::default();
        };
        if exists(&label) {
            if !self.is_dying(&label) {
                self.dying.push(label.clone());
            }
            CloseOutcome { destroy: Some(label), report_closed: false }
        } else {
            CloseOutcome { destroy: None, report_closed: true }
        }
    }

    /// The window with this label is gone. Returns true when main should be
    /// told (`sw://closed`): the live window went, or a closing one went and no
    /// newer window has replaced it. A stale window that was replaced returns false.
    pub fn on_destroyed(&mut self, label: &str) -> bool {
        let was_dying = self.is_dying(label);
        self.dying.retain(|l| l != label);
        match self.current.as_deref() {
            Some(cur) if cur == label => {
                self.current = None;
                true
            }
            Some(_) => false,
            None => was_dying,
        }
    }

    /// After `build()`: false when a dock raced the build and this window is no
    /// longer the live one, so the caller destroys it.
    pub fn keep_built(&self, label: &str) -> bool {
        self.current.as_deref() == Some(label)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;
    use std::collections::HashSet;

    /// A fake window registry.
    #[derive(Default)]
    struct Wins(RefCell<HashSet<String>>);
    impl Wins {
        fn add(&self, l: &str) { self.0.borrow_mut().insert(l.to_string()); }
        fn rm(&self, l: &str) { self.0.borrow_mut().remove(l); }
        fn has(&self) -> impl Fn(&str) -> bool + '_ { move |l| self.0.borrow().contains(l) }
    }

    fn invariant(s: &SecondWindowSlot) {
        if let Some(c) = s.current() {
            assert!(!s.is_dying(c), "the live window is never marked closing");
        }
    }

    fn build(s: &mut SecondWindowSlot, w: &Wins) -> String {
        match s.on_open(w.has()).expect("a free label") {
            OpenAction::Build(l) => { w.add(&l); invariant(s); l }
            other => panic!("expected Build, got {other:?}"),
        }
    }

    #[test]
    fn labels_are_the_fixed_set_only() {
        assert!(is_second_window_label("playlist"));
        for n in 2..=9 {
            assert!(is_second_window_label(&format!("playlist-{n}")));
        }
        for bad in ["main", "playlist-1", "playlist-10", "playlist-x", "playlist-", "playlists", "playlist-02"] {
            assert!(!is_second_window_label(bad), "{bad}");
        }
    }

    #[test]
    fn first_open_builds_then_focuses_the_live_window() {
        let (mut s, w) = (SecondWindowSlot::new(), Wins::default());
        assert_eq!(build(&mut s, &w), "playlist");
        assert_eq!(s.on_open(w.has()), Ok(OpenAction::Focus("playlist".into())));
        assert_eq!(s.focus_target(w.has()).as_deref(), Some("playlist"));
    }

    #[test]
    fn quick_close_then_reopen_reuses_the_label() {
        let (mut s, w) = (SecondWindowSlot::new(), Wins::default());
        build(&mut s, &w);
        let out = s.on_close_requested_by(None, w.has());
        assert_eq!(out, CloseOutcome { destroy: Some("playlist".into()), report_closed: false });
        w.rm("playlist");
        assert!(s.on_destroyed("playlist"), "main hears sw://closed");
        assert_eq!(build(&mut s, &w), "playlist");
    }

    #[test]
    fn destroy_slower_than_the_timeout_rebuilds_instead_of_focusing_the_dying_window() {
        let (mut s, w) = (SecondWindowSlot::new(), Wins::default());
        build(&mut s, &w);
        s.on_close_requested_by(None, w.has());
        invariant(&s);
        // 2 s later the old window is still registered: main reopens.
        assert!(s.focus_target(w.has()).is_none(), "a dying window is never a focus target");
        assert_eq!(build(&mut s, &w), "playlist-2", "rebuild, never Focus(\"playlist\")");
        // The old window finally goes: main must not re-dock the new one.
        w.rm("playlist");
        assert!(!s.on_destroyed("playlist"));
        assert_eq!(s.on_open(w.has()), Ok(OpenAction::Focus("playlist-2".into())));
        s.on_close_requested_by(None, w.has());
        w.rm("playlist-2");
        assert!(s.on_destroyed("playlist-2"));
        assert!(s.dying().is_empty());
    }

    #[test]
    fn a_late_close_from_the_closing_window_leaves_playlist_2_open() {
        let (mut s, w) = (SecondWindowSlot::new(), Wins::default());
        build(&mut s, &w);
        // Dock back from the old window: main closes it, but it stalls.
        s.on_close_requested_by(None, w.has());
        assert_eq!(build(&mut s, &w), "playlist-2");
        // The old window's 600 ms fallback lands now.
        let out = s.on_close_requested_by(Some("playlist"), w.has());
        assert_eq!(out, CloseOutcome { destroy: Some("playlist".into()), report_closed: false }, "only the caller");
        assert_eq!(s.current(), Some("playlist-2"), "the new window stays live");
        assert_eq!(s.focus_target(w.has()).as_deref(), Some("playlist-2"));
        invariant(&s);
        // A Media Manager that is the live one may still close itself (link dropped).
        let own = s.on_close_requested_by(Some("playlist-2"), w.has());
        assert_eq!(own.destroy.as_deref(), Some("playlist-2"));
        assert!(s.current().is_none());
    }

    #[test]
    fn labels_are_capped_at_playlist_9() {
        let (mut s, w) = (SecondWindowSlot::new(), Wins::default());
        for expect in LABELS {
            assert_eq!(build(&mut s, &w), expect);
            s.on_close_requested_by(None, w.has()); // every one stalls while closing
        }
        assert_eq!(s.dying().len(), 9);
        assert_eq!(s.on_open(w.has()), Err(OpenError::AllLabelsBusy));
        assert!(s.current().is_none(), "a refused open claims nothing");
        // One of them finally goes: its label is free again.
        w.rm("playlist-4");
        assert!(s.on_destroyed("playlist-4"));
        assert_eq!(build(&mut s, &w), "playlist-4");
    }

    #[test]
    fn failed_builds_leave_no_phantom_closing_labels() {
        let (mut s, w) = (SecondWindowSlot::new(), Wins::default());
        for _ in 0..5 {
            // build() fails: the label is claimed but no window appears. Then the user docks.
            assert_eq!(s.on_open(w.has()), Ok(OpenAction::Build("playlist".into())));
            let out = s.on_close_requested_by(None, w.has());
            assert_eq!(out, CloseOutcome { destroy: None, report_closed: true }, "nothing to destroy, tell main now");
            assert!(s.dying().is_empty(), "no phantom closing label");
        }
        // A closing label whose window vanished without Destroyed is dropped on the next open.
        build(&mut s, &w);
        s.on_close_requested_by(None, w.has());
        w.rm("playlist");
        assert_eq!(s.dying(), ["playlist".to_string()]);
        assert_eq!(build(&mut s, &w), "playlist", "reused, not playlist-2");
        assert!(s.dying().is_empty());
    }

    #[test]
    fn a_dock_that_races_the_build_drops_the_new_window() {
        let (mut s, w) = (SecondWindowSlot::new(), Wins::default());
        assert_eq!(s.on_open(w.has()), Ok(OpenAction::Build("playlist".into())));
        // Main docks before build() returns.
        assert!(s.on_close_requested_by(None, w.has()).report_closed);
        w.add("playlist");
        assert!(!s.keep_built("playlist"), "the builder destroys the stray window");
        w.rm("playlist");
        assert!(!s.on_destroyed("playlist"), "main already heard sw://closed");
        let mut t = SecondWindowSlot::new();
        assert_eq!(t.on_open(w.has()), Ok(OpenAction::Build("playlist".into())));
        assert!(t.keep_built("playlist"));
    }

    #[test]
    fn native_close_reports_and_strays_do_not() {
        let (mut s, w) = (SecondWindowSlot::new(), Wins::default());
        build(&mut s, &w);
        w.rm("playlist");
        assert!(s.on_destroyed("playlist"), "native close button");
        let mut empty = SecondWindowSlot::new();
        assert_eq!(empty.on_close_requested_by(None, w.has()), CloseOutcome::default());
        assert!(!empty.on_destroyed("playlist-9"));
    }
}
