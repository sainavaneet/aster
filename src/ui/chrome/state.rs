//! The modern style's state vocabulary, resolved once from wire strings.

/// One agent or task state as the chrome shows it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) enum StateKind {
    Working,
    Idle,
    Blocked,
    Approval,
    Done,
    Failed,
    Queued,
    Exited,
    Unknown,
}

impl StateKind {
    /// Importance for roll-ups: the highest wins.
    pub(crate) const fn priority(self) -> u8 {
        match self {
            Self::Approval => 8,
            Self::Blocked => 7,
            Self::Failed => 6,
            Self::Working => 5,
            Self::Queued => 4,
            Self::Done => 3,
            Self::Idle => 2,
            Self::Exited => 1,
            Self::Unknown => 0,
        }
    }

    /// A member's `OrbitShellMember.state`: an agent state, or a supervision
    /// phase for a member without a live agent. Unknown strings stay opaque.
    pub(crate) fn from_member_state(state: &str) -> Self {
        match state {
            "idle" => Self::Idle,
            "working" => Self::Working,
            "blocked" => Self::Blocked,
            "pending" | "starting" | "relaunching" => Self::Queued,
            "exited" | "gave_up" | "unsupervised" | "unsupported" => Self::Exited,
            _ => Self::Unknown,
        }
    }
}

/// The work-board lane an active task belongs to.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) enum Lane {
    NeedsYou,
    Working,
    Queued,
    Done,
}

impl Lane {
    pub(crate) const ALL: [Lane; 4] = [Self::NeedsYou, Self::Working, Self::Queued, Self::Done];

    /// Lane of an active task; a pure function of the wire task, so lane
    /// membership never depends on the clock.
    pub(crate) fn of_active(blocked: bool, awaiting_approval: bool, state: &str) -> Self {
        if blocked || awaiting_approval {
            Self::NeedsYou
        } else if state == "assigning" {
            Self::Queued
        } else {
            Self::Working
        }
    }

    /// The state glyph a lane header uses.
    pub(crate) const fn state(self) -> StateKind {
        match self {
            Self::NeedsYou => StateKind::Blocked,
            Self::Working => StateKind::Working,
            Self::Queued => StateKind::Queued,
            Self::Done => StateKind::Done,
        }
    }

    pub(crate) const fn label(self) -> &'static str {
        match self {
            Self::NeedsYou => "needs you",
            Self::Working => "working",
            Self::Queued => "queued",
            Self::Done => "done",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn member_states_map_agent_states_and_supervision_phases() {
        assert_eq!(StateKind::from_member_state("working"), StateKind::Working);
        assert_eq!(
            StateKind::from_member_state("relaunching"),
            StateKind::Queued
        );
        assert_eq!(StateKind::from_member_state("gave_up"), StateKind::Exited);
        assert_eq!(StateKind::from_member_state("future"), StateKind::Unknown);
    }

    #[test]
    fn lanes_follow_blocked_approval_and_assigning() {
        assert_eq!(Lane::of_active(true, false, "working"), Lane::NeedsYou);
        assert_eq!(Lane::of_active(false, true, "working"), Lane::NeedsYou);
        assert_eq!(Lane::of_active(false, false, "assigning"), Lane::Queued);
        assert_eq!(Lane::of_active(false, false, "idle"), Lane::Working);
    }

    #[test]
    fn needs_you_outranks_everything() {
        let top = [
            StateKind::Working,
            StateKind::Blocked,
            StateKind::Done,
            StateKind::Approval,
        ]
        .into_iter()
        .max_by_key(|state| state.priority());
        assert_eq!(top, Some(StateKind::Approval));
    }
}
