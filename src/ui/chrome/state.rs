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
    pub(crate) const COUNT: usize = 9;

    /// Position in a `StateCounts`.
    pub(crate) const fn index(self) -> usize {
        match self {
            Self::Approval => 0,
            Self::Blocked => 1,
            Self::Failed => 2,
            Self::Working => 3,
            Self::Queued => 4,
            Self::Done => 5,
            Self::Idle => 6,
            Self::Exited => 7,
            Self::Unknown => 8,
        }
    }

    /// States in roll-up order (most urgent first), the inverse of `index`.
    pub(crate) const ORDER: [StateKind; Self::COUNT] = [
        Self::Approval,
        Self::Blocked,
        Self::Failed,
        Self::Working,
        Self::Queued,
        Self::Done,
        Self::Idle,
        Self::Exited,
        Self::Unknown,
    ];

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

    /// The state an agent status shows when nothing more is known about it.
    pub(crate) const fn from_status(status: crate::api::schema::AgentStatus) -> Self {
        use crate::api::schema::AgentStatus;
        match status {
            AgentStatus::Working => Self::Working,
            AgentStatus::Blocked => Self::Blocked,
            AgentStatus::Done => Self::Done,
            AgentStatus::Idle => Self::Idle,
            AgentStatus::Unknown => Self::Unknown,
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

/// How many members or tasks are in each state.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct StateCounts([u16; StateKind::COUNT]);

impl StateCounts {
    pub(crate) fn add(&mut self, state: StateKind) {
        let slot = &mut self.0[state.index()];
        *slot = slot.saturating_add(1);
    }

    pub(crate) fn get(&self, state: StateKind) -> u16 {
        self.0[state.index()]
    }
}

/// A set of work-board lanes, used for the collapsed ones.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct LaneSet(u8);

impl LaneSet {
    pub(crate) const EMPTY: Self = Self(0);

    const fn bit(lane: Lane) -> u8 {
        match lane {
            Lane::NeedsYou => 1,
            Lane::Working => 2,
            Lane::Queued => 4,
            Lane::Done => 8,
        }
    }

    pub(crate) fn contains(self, lane: Lane) -> bool {
        self.0 & Self::bit(lane) != 0
    }

    /// Collapses an expanded lane and expands a collapsed one.
    pub(crate) fn toggle(&mut self, lane: Lane) {
        self.0 ^= Self::bit(lane);
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
    fn state_indices_round_trip_and_counts_saturate() {
        for (position, state) in StateKind::ORDER.into_iter().enumerate() {
            assert_eq!(state.index(), position);
        }
        let mut counts = StateCounts::default();
        counts.add(StateKind::Working);
        counts.add(StateKind::Working);
        assert_eq!(counts.get(StateKind::Working), 2);
        assert_eq!(counts.get(StateKind::Idle), 0);
    }

    #[test]
    fn lane_sets_toggle_independently() {
        let mut set = LaneSet::EMPTY;
        set.toggle(Lane::Queued);
        assert!(set.contains(Lane::Queued) && !set.contains(Lane::Done));
        set.toggle(Lane::Queued);
        assert_eq!(set, LaneSet::EMPTY);
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
