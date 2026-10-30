//! Role cards for the modern team tree, parallel to `TeamSidebar::groups`:
//! `ModernTeam::roles[g].members[m]` extends `groups[g].members[m]`.

use std::collections::HashMap;

use super::lanes::Lanes;
use crate::protocol::{OrbitShellMember, OrbitShellView};
use crate::ui::chrome::state::{StateCounts, StateKind};

/// What the modern member row shows beyond the shared `TeamRow`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(in crate::client::shell) struct ModernMember {
    pub(in crate::client::shell) state: StateKind,
    pub(in crate::client::shell) kind: String,
    /// The member's model; the draw path joins it to the kind with the active
    /// glyph set's separator, so no glyph is baked into this model.
    pub(in crate::client::shell) model: Option<String>,
    /// Why a member without a live agent is not running (`relaunching`).
    pub(in crate::client::shell) phase: String,
    /// The open task's title while the member has one.
    pub(in crate::client::shell) task_title: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(in crate::client::shell) struct ModernRole {
    pub(in crate::client::shell) counts: StateCounts,
    /// The title of the role's oldest open task, an at-a-glance area hint.
    pub(in crate::client::shell) hint: String,
    pub(in crate::client::shell) members: Vec<ModernMember>,
}

#[derive(Debug, PartialEq, Eq)]
pub(in crate::client::shell) struct ModernTeam {
    pub(in crate::client::shell) attention: PaneAttention,
    pub(in crate::client::shell) lead: ModernMember,
    /// One per `TeamSidebar::groups` entry: the roles, then `scratch`.
    pub(in crate::client::shell) roles: Vec<ModernRole>,
    pub(in crate::client::shell) lanes: Lanes,
    /// Worker pane headers by pane id; non-worker panes have none.
    pub(in crate::client::shell) headers: HashMap<String, super::pane_header::PaneHeader>,
}

/// The state an open task puts its worker in: waiting on you, blocked, or just
/// working.
fn task_state(task: &crate::protocol::OrbitShellActiveTask) -> StateKind {
    if task.awaiting_approval {
        StateKind::Approval
    } else if task.blocked {
        StateKind::Blocked
    } else {
        StateKind::Working
    }
}

/// Client-only attention facts, built once per snapshot/projection change.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(in crate::client::shell) struct PaneAttention(HashMap<String, StateKind>);

impl PaneAttention {
    /// The approval-aware state of `pane_id`, when the Orbit projection has one.
    pub(in crate::client::shell) fn state(&self, pane_id: &str) -> Option<StateKind> {
        self.0.get(pane_id).copied()
    }

    pub(in crate::client::shell) fn build(orbit: Option<&OrbitShellView>) -> Self {
        let Some(orbit) = orbit else {
            return Self::default();
        };
        let terminals: HashMap<&str, &str> = std::iter::once((
            orbit.lead.terminal_id.as_str(),
            orbit.lead.pane_id.as_deref(),
        ))
        .chain(
            orbit
                .roles
                .iter()
                .flat_map(|role| &role.members)
                .chain(&orbit.scratch)
                .filter_map(|member| {
                    Some((member.terminal_id.as_deref()?, member.pane_id.as_deref()))
                }),
        )
        .filter_map(|(terminal, pane)| Some((terminal, pane?)))
        .collect();
        let mut states = HashMap::new();
        for task in &orbit.active_tasks {
            let state = task_state(task);
            if !matches!(state, StateKind::Approval | StateKind::Blocked) {
                continue;
            }
            if let Some(pane) = terminals.get(task.worker_terminal_id.as_str()) {
                let current = states.entry((*pane).to_owned()).or_insert(state);
                if state.priority() > current.priority() {
                    *current = state;
                }
            }
        }
        Self(states)
    }
}

/// One borrowed lookup; no roster/task walks in pane-scaled rendering.
pub(in crate::client::shell) fn pane_state(
    attention: Option<&PaneAttention>,
    pane_id: &str,
    status: crate::api::schema::AgentStatus,
) -> StateKind {
    attention
        .and_then(|attention| attention.0.get(pane_id))
        .copied()
        .unwrap_or(StateKind::from_status(status))
}

/// An open task of a worker terminal, as far as its member row needs.
pub(super) struct Open<'a> {
    pub(super) task: &'a crate::protocol::OrbitShellActiveTask,
    title: &'a str,
    state: StateKind,
    created_at_ms: u64,
}

fn member(member: &OrbitShellMember, open: Option<&Open<'_>>) -> ModernMember {
    // An agent state, as opposed to a supervision phase of a member without one.
    let live = matches!(
        member.state.as_str(),
        "idle" | "working" | "blocked" | "unknown"
    );
    let base = StateKind::from_member_state(&member.state);
    // A task waiting on you outranks what the agent screen says.
    let state = match open.map(|open| open.state) {
        Some(state @ (StateKind::Approval | StateKind::Blocked)) => state,
        _ => base,
    };
    ModernMember {
        state,
        kind: member.kind.clone(),
        model: member.model.clone().filter(|model| !model.is_empty()),
        phase: if live {
            String::new()
        } else {
            member.state.replace('_', " ")
        },
        task_title: open
            .filter(|_| state == StateKind::Working)
            .map(|open| open.title.to_owned()),
    }
}

fn role(entries: &[OrbitShellMember], open: &HashMap<&str, Open<'_>>) -> ModernRole {
    let opens = entries
        .iter()
        .map(|entry| entry.terminal_id.as_deref().and_then(|id| open.get(id)))
        .collect::<Vec<_>>();
    let members = entries
        .iter()
        .zip(&opens)
        .map(|(entry, open)| member(entry, *open))
        .collect::<Vec<_>>();
    let mut counts = StateCounts::default();
    for member in &members {
        counts.add(member.state);
    }
    let hint = opens
        .iter()
        .flatten()
        .min_by_key(|open| open.created_at_ms)
        .map(|open| open.title.to_owned())
        .unwrap_or_default();
    ModernRole {
        counts,
        hint,
        members,
    }
}

impl ModernTeam {
    pub(in crate::client::shell) fn build(orbit: &OrbitShellView) -> Self {
        let mut open = HashMap::<&str, Open<'_>>::new();
        for task in &orbit.active_tasks {
            let state = task_state(task);
            let candidate = Open {
                task,
                title: task.title.as_str(),
                state,
                created_at_ms: task.created_at_ms,
            };
            let keep = open
                .get(task.worker_terminal_id.as_str())
                .is_none_or(|current| {
                    (
                        candidate.state.priority(),
                        std::cmp::Reverse(candidate.created_at_ms),
                    ) > (
                        current.state.priority(),
                        std::cmp::Reverse(current.created_at_ms),
                    )
                });
            if keep {
                open.insert(task.worker_terminal_id.as_str(), candidate);
            }
        }
        let lead = ModernMember {
            state: StateKind::from_member_state(&orbit.lead.state),
            kind: "lead".to_owned(),
            model: None,
            phase: String::new(),
            task_title: None,
        };
        let mut roles = orbit
            .roles
            .iter()
            .map(|entry| role(&entry.members, &open))
            .collect::<Vec<_>>();
        if !orbit.scratch.is_empty() {
            roles.push(role(&orbit.scratch, &open));
        }
        let headers = super::pane_header::build(orbit, &lead, &roles, &open);
        Self {
            attention: PaneAttention::build(Some(orbit)),
            lead,
            roles,
            lanes: Lanes::build(orbit),
            headers,
        }
    }
}
