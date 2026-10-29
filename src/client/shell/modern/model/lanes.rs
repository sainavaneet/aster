//! Work-board lanes for the modern sidebar: needs you, working, queued, done.
//! Built with the team model when the `orbit` projection changes; lane
//! membership and order depend only on the projected facts, never on the clock.

use std::collections::HashMap;

use crate::protocol::{OrbitShellActiveTask, OrbitShellView};
use crate::ui::chrome::state::{Lane, StateKind};

/// Progress nodes shown on a relay or parallel strip.
const MAX_NODES: usize = 8;
const SHORT_ID_CHARS: usize = 8;
const MAX_RECENT: usize = 5;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(in crate::client::shell) enum StepNode {
    Done,
    Active,
    Pending,
    Failed,
    Omitted,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(in crate::client::shell) enum StripKind {
    Relay,
    Parallel,
    Integrating,
}

/// The relay or parallel progress row of a task card.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(in crate::client::shell) struct StepStrip {
    pub(in crate::client::shell) kind: StripKind,
    pub(in crate::client::shell) nodes: Vec<StepNode>,
    /// `step 2/3`, `parallel 1/2`, or `integrating`.
    pub(in crate::client::shell) line: String,
    /// The kind of the member the relay goes to next, when known.
    pub(in crate::client::shell) next: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(in crate::client::shell) struct TaskCard {
    pub(in crate::client::shell) pane_id: Option<String>,
    /// The role the worker acts for, else its name.
    pub(in crate::client::shell) who: String,
    pub(in crate::client::shell) title: String,
    pub(in crate::client::shell) short_id: String,
    pub(in crate::client::shell) state: StateKind,
    pub(in crate::client::shell) approval: bool,
    pub(in crate::client::shell) created_at_ms: u64,
    pub(in crate::client::shell) strip: Option<StepStrip>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(in crate::client::shell) struct DoneCard {
    /// `Done`, `Failed`, or `Unknown` for cancelled and timed out.
    pub(in crate::client::shell) state: StateKind,
    pub(in crate::client::shell) summary: String,
    pub(in crate::client::shell) finished_at_ms: u64,
}

#[derive(Debug, Default, PartialEq, Eq)]
pub(in crate::client::shell) struct Lanes {
    needs_you: Vec<TaskCard>,
    working: Vec<TaskCard>,
    queued: Vec<TaskCard>,
    pub(in crate::client::shell) done: Vec<DoneCard>,
}

impl Lanes {
    pub(in crate::client::shell) fn build(orbit: &OrbitShellView) -> Self {
        let mut names = HashMap::<&str, &str>::new();
        let mut panes = HashMap::<&str, &str>::new();
        names.insert(orbit.lead.terminal_id.as_str(), orbit.lead.name.as_str());
        if let Some(pane_id) = orbit.lead.pane_id.as_deref() {
            panes.insert(orbit.lead.terminal_id.as_str(), pane_id);
        }
        for member in orbit
            .roles
            .iter()
            .flat_map(|role| &role.members)
            .chain(&orbit.scratch)
        {
            let Some(terminal_id) = member.terminal_id.as_deref() else {
                continue;
            };
            names.insert(terminal_id, member.name.as_str());
            if let Some(pane_id) = member.pane_id.as_deref() {
                panes.insert(terminal_id, pane_id);
            }
        }
        let mut lanes = Self::default();
        for task in &orbit.active_tasks {
            let worker = task.worker_terminal_id.as_str();
            let lane = Lane::of_active(task.blocked, task.awaiting_approval, &task.state);
            let card = TaskCard {
                pane_id: panes.get(worker).map(|pane_id| (*pane_id).to_owned()),
                who: task
                    .role
                    .clone()
                    .or_else(|| names.get(worker).map(|name| (*name).to_owned()))
                    .unwrap_or_else(|| worker.to_owned()),
                title: task.title.clone(),
                short_id: short_id(&task.task_id),
                state: card_state(task),
                approval: task.awaiting_approval,
                created_at_ms: task.created_at_ms,
                strip: strip(task),
            };
            match lane {
                Lane::NeedsYou => lanes.needs_you.push(card),
                Lane::Queued => lanes.queued.push(card),
                Lane::Working | Lane::Done => lanes.working.push(card),
            }
        }
        // Oldest first: the longest wait is the most urgent.
        lanes.needs_you.sort_by_key(|card| card.created_at_ms);
        lanes.working.sort_by_key(|card| card.created_at_ms);
        lanes.done = orbit
            .recent_tasks
            .iter()
            .take(MAX_RECENT)
            .map(|task| DoneCard {
                state: match task.outcome.as_str() {
                    "completed" => StateKind::Done,
                    "failed" | "blocked" => StateKind::Failed,
                    _ => StateKind::Unknown,
                },
                summary: task.summary.clone(),
                finished_at_ms: task.finished_at_ms,
            })
            .collect();
        lanes
    }

    pub(in crate::client::shell) fn tasks(&self, lane: Lane) -> &[TaskCard] {
        match lane {
            Lane::NeedsYou => &self.needs_you,
            Lane::Working => &self.working,
            Lane::Queued => &self.queued,
            Lane::Done => &[],
        }
    }

    /// Cards in `lane`; for `Done`, the resolved tasks.
    pub(in crate::client::shell) fn len(&self, lane: Lane) -> usize {
        match lane {
            Lane::Done => self.done.len(),
            lane => self.tasks(lane).len(),
        }
    }

    pub(in crate::client::shell) fn has_active(&self) -> bool {
        !(self.needs_you.is_empty() && self.working.is_empty() && self.queued.is_empty())
    }
}

fn short_id(task_id: &str) -> String {
    let skip = task_id.chars().count().saturating_sub(SHORT_ID_CHARS);
    task_id.chars().skip(skip).collect()
}

fn card_state(task: &OrbitShellActiveTask) -> StateKind {
    if task.awaiting_approval {
        StateKind::Approval
    } else if task.blocked {
        StateKind::Blocked
    } else if task.state == "assigning" {
        StateKind::Queued
    } else {
        StateKind::from_member_state(&task.state)
    }
}

fn node(state: &str) -> StepNode {
    match state {
        "done" | "handed_off" => StepNode::Done,
        "active" => StepNode::Active,
        "failed" => StepNode::Failed,
        _ => StepNode::Pending,
    }
}

/// Keeps at most `MAX_NODES` nodes, always including the active one.
fn window(mut nodes: Vec<StepNode>) -> Vec<StepNode> {
    if nodes.len() > MAX_NODES {
        let active = nodes
            .iter()
            .position(|node| *node == StepNode::Active)
            .unwrap_or(0);
        let start = active
            .saturating_sub(MAX_NODES / 2)
            .min(nodes.len() - MAX_NODES);
        nodes = nodes[start..start + MAX_NODES].to_vec();
    }
    nodes
}

pub(in crate::client::shell) fn strip(task: &OrbitShellActiveTask) -> Option<StepStrip> {
    if task.steps.is_empty() && task.parallel.is_none() {
        return None;
    }
    let integrating = task.steps.iter().any(|step| {
        step.integrate.is_some() && matches!(step.state.as_str(), "active" | "pending")
    });
    let branches = || {
        task.steps
            .iter()
            .filter(|step| step.parallel.is_some())
            .map(|step| node(&step.state))
            .collect::<Vec<_>>()
    };
    if integrating {
        return Some(StepStrip {
            kind: StripKind::Integrating,
            nodes: window(branches()),
            line: "integrating".to_owned(),
            next: None,
        });
    }
    if let Some(parallel) = task.parallel {
        return Some(StepStrip {
            kind: StripKind::Parallel,
            nodes: window(branches()),
            line: format!("parallel {}/{}", parallel.done, parallel.total),
            next: None,
        });
    }
    let total = usize::from(task.step_total.unwrap_or(0)).max(task.steps.len());
    let current = task
        .steps
        .iter()
        .rev()
        .find(|step| step.state == "active")
        .or_else(|| task.steps.iter().rev().find(|step| step.state != "pending"))?;
    let total = total.max(usize::from(current.index) + 1);
    let start = usize::from(current.index)
        .saturating_sub(MAX_NODES / 2)
        .min(total.saturating_sub(MAX_NODES));
    let first = task.steps.first().map_or(0, |step| step.index);
    let last = task.steps.last().map_or(0, |step| step.index);
    let nodes = (start..total.min(start + MAX_NODES))
        .map(|index| {
            task.steps
                .iter()
                .find(|step| usize::from(step.index) == index)
                .map(|step| node(&step.state))
                .unwrap_or_else(|| {
                    if index < usize::from(first)
                        || (index > usize::from(last)
                            && task.steps_omitted.is_some_and(|omitted| omitted.after > 0))
                    {
                        StepNode::Omitted
                    } else {
                        StepNode::Pending
                    }
                })
        })
        .collect();
    let next = task
        .steps
        .iter()
        .find(|step| step.index == current.index.saturating_add(1))
        .map(|step| step.kind.clone());
    Some(StepStrip {
        kind: StripKind::Relay,
        nodes,
        line: format!("step {}/{total}", u32::from(current.index) + 1),
        next,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::{OrbitShellOmitted, OrbitShellParallel, OrbitShellStep};

    fn task(id: &str, state: &str, blocked: bool, approval: bool, at: u64) -> OrbitShellActiveTask {
        OrbitShellActiveTask {
            task_id: id.into(),
            worker_terminal_id: "term_w".into(),
            role: Some("builders".into()),
            title: format!("title {id}"),
            state: state.into(),
            created_at_ms: at,
            blocked,
            awaiting_approval: approval,
            steps: Vec::new(),
            step_total: None,
            parallel: None,
            steps_omitted: None,
        }
    }

    fn step(index: u16, state: &str) -> OrbitShellStep {
        OrbitShellStep {
            index,
            member: format!("m{index}"),
            kind: format!("kind{index}"),
            state: state.into(),
            note: None,
            next: None,
            parallel: None,
            integrate: None,
            started_at_ms: None,
            finished_at_ms: None,
            worker_terminal_id: None,
            pane_id: None,
        }
    }

    fn view(tasks: Vec<OrbitShellActiveTask>) -> OrbitShellView {
        OrbitShellView {
            lead: crate::protocol::OrbitShellLead {
                name: "Polaris".into(),
                terminal_id: "term_lead".into(),
                pane_id: None,
                state: "idle".into(),
            },
            roles: Vec::new(),
            scratch: Vec::new(),
            active_tasks: tasks,
            recent_tasks: Vec::new(),
            server_now_ms: None,
        }
    }

    #[test]
    fn tasks_land_in_lanes_oldest_first() {
        let lanes = Lanes::build(&view(vec![
            task("b", "working", false, false, 20),
            task("a", "working", false, false, 10),
            task("n", "idle", true, false, 40),
            task("m", "working", false, true, 30),
            task("q", "assigning", false, false, 50),
        ]));
        assert_eq!(lanes.len(Lane::NeedsYou), 2);
        assert_eq!(lanes.tasks(Lane::NeedsYou)[0].title, "title m");
        assert_eq!(lanes.tasks(Lane::NeedsYou)[0].state, StateKind::Approval);
        assert_eq!(lanes.tasks(Lane::NeedsYou)[1].state, StateKind::Blocked);
        assert_eq!(lanes.tasks(Lane::Working)[0].title, "title a");
        assert_eq!(lanes.tasks(Lane::Queued)[0].state, StateKind::Queued);
        assert!(lanes.has_active());
    }

    #[test]
    fn a_relay_strip_counts_steps_and_names_the_next_kind() {
        let mut relayed = task("r", "working", false, false, 1);
        relayed.steps = vec![step(0, "handed_off"), step(1, "active"), step(2, "pending")];
        relayed.step_total = Some(3);
        let lanes = Lanes::build(&view(vec![relayed]));
        let strip = lanes.tasks(Lane::Working)[0].strip.as_ref().unwrap();
        assert_eq!(strip.kind, StripKind::Relay);
        assert_eq!(strip.line, "step 2/3");
        assert_eq!(
            strip.nodes,
            [StepNode::Done, StepNode::Active, StepNode::Pending]
        );
        assert_eq!(strip.next.as_deref(), Some("kind2"));
    }

    #[test]
    fn a_pipeline_longer_than_the_steps_so_far_pads_with_pending_nodes() {
        let mut relayed = task("r", "working", false, false, 1);
        relayed.steps = vec![step(0, "active")];
        relayed.step_total = Some(3);
        let strip = Lanes::build(&view(vec![relayed])).tasks(Lane::Working)[0]
            .strip
            .clone()
            .unwrap();
        assert_eq!(strip.line, "step 1/3");
        assert_eq!(strip.nodes.len(), 3);
    }

    #[test]
    fn parallel_and_integrating_strips_use_task_totals_not_the_window() {
        let mut fanned = task("p", "working", false, false, 1);
        let branch = |index, state: &str| {
            let mut branch = step(index, state);
            branch.parallel = Some("running".into());
            branch
        };
        fanned.steps = vec![
            step(0, "handed_off"),
            branch(1, "done"),
            branch(2, "active"),
        ];
        fanned.parallel = Some(OrbitShellParallel { done: 3, total: 4 });
        fanned.steps_omitted = Some(OrbitShellOmitted {
            before: 0,
            after: 2,
        });
        let strip = Lanes::build(&view(vec![fanned.clone()])).tasks(Lane::Working)[0]
            .strip
            .clone()
            .unwrap();
        assert_eq!(
            (strip.kind, strip.line.as_str()),
            (StripKind::Parallel, "parallel 3/4")
        );
        assert_eq!(strip.nodes, [StepNode::Done, StepNode::Active]);

        let mut integrator = step(3, "active");
        integrator.integrate = Some("applying".into());
        fanned.steps.push(integrator);
        let strip = Lanes::build(&view(vec![fanned])).tasks(Lane::Working)[0]
            .strip
            .clone()
            .unwrap();
        assert_eq!(strip.kind, StripKind::Integrating);
        assert_eq!(strip.line, "integrating");
    }

    #[test]
    fn a_task_without_steps_has_no_strip_and_long_relays_window_to_eight_nodes() {
        let plain = Lanes::build(&view(vec![task("x", "working", false, false, 1)]));
        assert!(plain.tasks(Lane::Working)[0].strip.is_none());
        let mut long = task("l", "working", false, false, 1);
        long.steps = (0..20)
            .map(|index| {
                step(
                    index,
                    if index == 15 {
                        "active"
                    } else if index < 15 {
                        "done"
                    } else {
                        "pending"
                    },
                )
            })
            .collect();
        long.step_total = Some(20);
        let strip = Lanes::build(&view(vec![long])).tasks(Lane::Working)[0]
            .strip
            .clone()
            .unwrap();
        assert_eq!(strip.nodes.len(), MAX_NODES);
        assert!(strip.nodes.contains(&StepNode::Active));
        assert_eq!(strip.line, "step 16/20");
    }
    #[test]
    fn omitted_original_indices_do_not_expand_a_strip_to_the_pipeline_length() {
        let mut relay = task("window", "working", false, false, 1);
        relay.steps = vec![step(40, "done"), step(41, "active"), step(42, "pending")];
        relay.step_total = Some(u16::MAX);
        relay.steps_omitted = Some(OrbitShellOmitted {
            before: 40,
            after: 10,
        });
        let strip = strip(&relay).unwrap();
        assert_eq!(strip.line, "step 42/65535");
        assert_eq!(strip.nodes.len(), MAX_NODES);
        assert!(strip.nodes.contains(&StepNode::Active));
        assert!(strip.nodes.contains(&StepNode::Omitted));
        assert_eq!(strip.next.as_deref(), Some("kind42"));
    }

    #[test]
    fn idle_and_unknown_tasks_stay_working_unless_attention_overrides_queue() {
        let lanes = Lanes::build(&view(vec![
            task("idle", "idle", false, false, 1),
            task("future", "future-state", false, false, 2),
            task("queue-approval", "assigning", false, true, 3),
            task("queue-blocked", "assigning", true, false, 4),
        ]));
        assert_eq!(lanes.len(Lane::Working), 2);
        assert_eq!(lanes.tasks(Lane::Working)[0].state, StateKind::Idle);
        assert_eq!(lanes.tasks(Lane::Working)[1].state, StateKind::Unknown);
        assert_eq!(lanes.len(Lane::NeedsYou), 2);
        assert_eq!(lanes.len(Lane::Queued), 0);
    }
}
