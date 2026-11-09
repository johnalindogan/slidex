//! Which window is the live Media Manager (the undocked playlist window).
//!
//! `destroy()` asks the OS to tear the window down, but the window can take a
//! while to go away (WebView2 on Windows has been seen to take over 2 s). Main
//! waits up to 2 s for `sw://closed` and then reopens. Without this slot that
//! reopen would find the old window by label and focus it while it is dying.
//! With it, a window that was asked to close is never focused again: the
//! reopen builds a new window under a fresh label, and the late `Destroyed`
//! of the old one does not tell main the new window closed.
//!
//! Pure bookkeeping with no Tauri types, so it can be unit tested anywhere.

/// Label of the first Media Manager window. Later ones are `playlist-2`, `playlist-3`, ...
pub const BASE_LABEL: &str = "playlist";

/// True for any Media Manager window label (`playlist` or `playlist-N`).
pub fn is_second_window_label(label: &str) -> bool {
    label == BASE_LABEL
        || label
            .strip_prefix("playlist-")
            .map_or(false, |n| !n.is_empty() && n.bytes().all(|b| b.is_ascii_digit()))
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OpenAction {
    /// The live window exists and was not asked to close: show and focus it.
    Focus(String),
    /// Build a new window with this label.
    Build(String),
}

#[derive(Debug, Default)]
pub struct SecondWindowSlot {
    /// The window main should treat as its Media Manager.
    current: Option<String>,
    /// Windows that were asked to close and have not reported `Destroyed` yet.
    dying: Vec<String>,
}

impl SecondWindowSlot {
    pub fn new() -> Self {
        Self::default()
    }

    /// The live window, if any. Never a window that was asked to close.
    #[allow(dead_code)]
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
    /// is still registered. A dying window is never focused: it gets a fresh label.
    pub fn on_open(&mut self, exists: impl Fn(&str) -> bool) -> OpenAction {
        if let Some(cur) = self.current.clone() {
            if exists(&cur) && !self.is_dying(&cur) {
                return OpenAction::Focus(cur);
            }
        }
        let taken = |l: &str| exists(l) || self.is_dying(l);
        let label = if !taken(BASE_LABEL) {
            BASE_LABEL.to_string()
        } else {
            (2u32..)
                .map(|n| format!("playlist-{n}"))
                .find(|l| !taken(l))
                .expect("a free playlist label")
        };
        self.current = Some(label.clone());
        OpenAction::Build(label)
    }

    /// The window to focus for `focus_playlist_window`, or None when the live
    /// window is gone or closing.
    pub fn focus_target(&self, exists: impl Fn(&str) -> bool) -> Option<String> {
        self.current
            .as_deref()
            .filter(|l| exists(l) && !self.is_dying(l))
            .map(str::to_string)
    }

    /// Dock back / close: returns the label to destroy and marks it dying, so a
    /// reopen cannot focus it even if `Destroyed` is late.
    pub fn on_close_requested(&mut self) -> Option<String> {
        let label = self.current.take()?;
        if !self.is_dying(&label) {
            self.dying.push(label.clone());
        }
        Some(label)
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
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;

    fn exists_in(set: &HashSet<String>) -> impl Fn(&str) -> bool + '_ {
        move |l| set.contains(l)
    }

    #[test]
    fn labels() {
        assert!(is_second_window_label("playlist"));
        assert!(is_second_window_label("playlist-2"));
        assert!(is_second_window_label("playlist-17"));
        assert!(!is_second_window_label("main"));
        assert!(!is_second_window_label("playlist-"));
        assert!(!is_second_window_label("playlist-x"));
        assert!(!is_second_window_label("playlists"));
    }

    #[test]
    fn first_open_builds_then_focuses_the_live_window() {
        let mut s = SecondWindowSlot::new();
        let mut win = HashSet::new();
        assert_eq!(s.on_open(exists_in(&win)), OpenAction::Build("playlist".into()));
        win.insert("playlist".to_string());
        assert_eq!(s.on_open(exists_in(&win)), OpenAction::Focus("playlist".into()));
        assert_eq!(s.focus_target(exists_in(&win)).as_deref(), Some("playlist"));
    }

    #[test]
    fn quick_close_then_reopen_reuses_the_label() {
        let mut s = SecondWindowSlot::new();
        let mut win = HashSet::new();
        s.on_open(exists_in(&win));
        win.insert("playlist".to_string());
        assert_eq!(s.on_close_requested().as_deref(), Some("playlist"));
        win.remove("playlist");
        assert!(s.on_destroyed("playlist"), "main hears sw://closed");
        assert_eq!(s.on_open(exists_in(&win)), OpenAction::Build("playlist".into()));
    }

    #[test]
    fn destroy_slower_than_the_timeout_rebuilds_instead_of_focusing_the_dying_window() {
        let mut s = SecondWindowSlot::new();
        let mut win = HashSet::new();
        s.on_open(exists_in(&win));
        win.insert("playlist".to_string());
        assert_eq!(s.on_close_requested().as_deref(), Some("playlist"));
        // 2 s later the old window is still registered: main reopens.
        assert!(s.focus_target(exists_in(&win)).is_none(), "a dying window is never a focus target");
        let action = s.on_open(exists_in(&win));
        assert_eq!(action, OpenAction::Build("playlist-2".into()), "rebuild, never Focus(\"playlist\")");
        win.insert("playlist-2".to_string());
        assert_eq!(s.current(), Some("playlist-2"));
        // The old window finally goes: main must not re-dock the new one.
        win.remove("playlist");
        assert!(!s.on_destroyed("playlist"));
        assert_eq!(s.current(), Some("playlist-2"));
        assert_eq!(s.on_open(exists_in(&win)), OpenAction::Focus("playlist-2".into()));
        // Closing the new one still reports.
        assert_eq!(s.on_close_requested().as_deref(), Some("playlist-2"));
        win.remove("playlist-2");
        assert!(s.on_destroyed("playlist-2"));
        assert!(s.dying().is_empty());
    }

    #[test]
    fn two_slow_destroys_in_a_row_get_distinct_labels() {
        let mut s = SecondWindowSlot::new();
        let mut win = HashSet::new();
        for expect in ["playlist", "playlist-2", "playlist-3"] {
            assert_eq!(s.on_open(exists_in(&win)), OpenAction::Build(expect.into()));
            win.insert(expect.to_string());
            s.on_close_requested();
        }
        assert_eq!(s.dying().len(), 3);
        assert!(s.current().is_none());
    }

    #[test]
    fn native_close_of_the_live_window_reports_and_a_failed_build_is_retried() {
        let mut s = SecondWindowSlot::new();
        let mut win = HashSet::new();
        s.on_open(exists_in(&win));
        win.insert("playlist".to_string());
        win.remove("playlist");
        assert!(s.on_destroyed("playlist"), "native close button");
        // Build failed: the label was claimed but no window exists; reopen builds again.
        assert_eq!(s.on_open(exists_in(&win)), OpenAction::Build("playlist".into()));
        assert_eq!(s.on_open(exists_in(&win)), OpenAction::Build("playlist".into()));
        // Close with nothing open is a no-op; a stray Destroyed for an unknown label does not report.
        let mut empty = SecondWindowSlot::new();
        assert!(empty.on_close_requested().is_none());
        assert!(!empty.on_destroyed("playlist-9"));
    }
}
