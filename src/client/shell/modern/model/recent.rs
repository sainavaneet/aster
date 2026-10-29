//! "Recently changed" for the modern chrome, independent of animations.
//!
//! The animation clock's flash and recency highlight are motion: with
//! `ui.animations = "off"` they do not exist, so a pane that just finished or
//! just started waiting on you would look like one idle for an hour. This keeps
//! a plain record instead: which panes changed to a state worth noticing, and
//! until when. It is updated when a snapshot arrives and expired by the existing
//! coarse client tick; drawing only compares an `Instant` against it.

use std::collections::HashMap;
use std::time::{Duration, Instant};

use crate::ui::chrome::state::StateKind;

/// How long a change stays marked.
pub(in crate::client::shell) const RECENT_WINDOW: Duration = Duration::from_secs(60);

/// Whether going from `previous` to `current` is worth a marker: the pane now
/// needs you (blocked or an approval), or it stopped working.
pub(in crate::client::shell) fn is_notable(previous: StateKind, current: StateKind) -> bool {
    previous != current
        && (matches!(current, StateKind::Blocked | StateKind::Approval)
            || (previous == StateKind::Working
                && matches!(current, StateKind::Idle | StateKind::Done)))
}

#[derive(Debug, Default)]
pub(in crate::client::shell) struct RecentMarks {
    /// Per pane: when the marker ends.
    until: HashMap<String, Instant>,
    /// The earliest end among the marks, for the cheap expiry check.
    deadline: Option<Instant>,
}

impl RecentMarks {
    /// Marks every pane whose state changed notably between the two maps.
    /// Panes missing from `previous` are new and are not marked.
    pub(in crate::client::shell) fn record(
        &mut self,
        previous: &HashMap<String, StateKind>,
        current: &HashMap<String, StateKind>,
        now: Instant,
    ) {
        for (pane_id, state) in current {
            if previous
                .get(pane_id)
                .is_some_and(|before| is_notable(*before, *state))
            {
                self.until.insert(pane_id.clone(), now + RECENT_WINDOW);
            }
        }
        // A pane that left the snapshot has nothing to mark.
        self.until
            .retain(|pane_id, _| current.contains_key(pane_id));
        self.deadline = self.until.values().min().copied();
    }

    /// The end of `pane_id`'s marker, if it has one.
    pub(in crate::client::shell) fn until(&self, pane_id: &str) -> Option<Instant> {
        self.until.get(pane_id).copied()
    }

    pub(in crate::client::shell) fn is_recent(&self, pane_id: &str, now: Instant) -> bool {
        self.until(pane_id).is_some_and(|until| now < until)
    }

    /// Drops ended markers; true when any ended, so one repaint can remove them.
    pub(in crate::client::shell) fn expire(&mut self, now: Instant) -> bool {
        if self.deadline.is_none_or(|deadline| now < deadline) {
            return false;
        }
        self.until.retain(|_, until| now < *until);
        self.deadline = self.until.values().min().copied();
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn states(entries: &[(&str, StateKind)]) -> HashMap<String, StateKind> {
        entries
            .iter()
            .map(|(pane, state)| ((*pane).to_owned(), *state))
            .collect()
    }

    #[test]
    fn finishing_and_needing_you_are_notable_but_starting_and_idling_are_not() {
        use StateKind::{Approval, Blocked, Done, Idle, Working};
        assert!(is_notable(Working, Idle) && is_notable(Working, Done));
        assert!(is_notable(Idle, Blocked) && is_notable(Working, Approval));
        assert!(!is_notable(Idle, Working) && !is_notable(Idle, Done));
        assert!(!is_notable(Blocked, Blocked));
    }

    #[test]
    fn marks_start_at_the_change_and_expire_after_the_window() {
        let now = Instant::now();
        let mut marks = RecentMarks::default();
        let before = states(&[("a", StateKind::Working), ("b", StateKind::Idle)]);
        let after = states(&[
            ("a", StateKind::Idle),
            ("b", StateKind::Idle),
            ("c", StateKind::Blocked),
        ]);
        marks.record(&before, &after, now);
        assert!(marks.is_recent("a", now));
        assert!(!marks.is_recent("b", now), "unchanged");
        assert!(!marks.is_recent("c", now), "a new pane is not a change");
        assert!(!marks.expire(now + RECENT_WINDOW - Duration::from_secs(1)));
        assert!(marks.is_recent("a", now + RECENT_WINDOW - Duration::from_secs(1)));
        assert!(marks.expire(now + RECENT_WINDOW));
        assert!(!marks.is_recent("a", now + RECENT_WINDOW));
        assert!(
            !marks.expire(now + RECENT_WINDOW * 2),
            "nothing left to expire"
        );
    }
}
