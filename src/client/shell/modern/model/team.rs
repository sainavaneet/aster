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
    /// `claude·opus`, or just the kind without a model.
    pub(in crate::client::shell) kind_model: String,
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
    pub(in crate::client::shell) lead: ModernMember,
    /// One per `TeamSidebar::groups` entry: the roles, then `scratch`.
    pub(in crate::client::shell) roles: Vec<ModernRole>,
    pub(in crate::client::shell) lanes: Lanes,
}

/// An open task of a worker terminal, as far as its member row needs.
struct Open<'a> {
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
    let kind_model = match member.model.as_deref().filter(|model| !model.is_empty()) {
        Some(model) => format!("{}·{model}", member.kind),
        None => member.kind.clone(),
    };
    ModernMember {
        state,
        kind: member.kind.clone(),
        kind_model,
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
            let state = if task.awaiting_approval {
                StateKind::Approval
            } else if task.blocked {
                StateKind::Blocked
            } else {
                StateKind::Working
            };
            let candidate = Open {
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
            kind_model: "lead".to_owned(),
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
        Self {
            lead,
            roles,
            lanes: Lanes::build(orbit),
        }
    }
}
