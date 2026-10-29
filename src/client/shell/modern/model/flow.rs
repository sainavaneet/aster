//! The Orbit flow view's layout: which rows a task's steps make, in which order
//! and which are focusable. Built when the overlay opens, when the snapshot, the
//! selected task or the popup geometry changes, never per frame and never while
//! the view is closed. It stores indices into the snapshot's steps, not copies
//! of their text, so drawing borrows everything from the snapshot.

use ratatui::layout::Rect;

use super::lanes::strip;
use crate::protocol::{OrbitShellActiveTask, OrbitShellView};

/// Inner width from which parallel steps sit side by side.
pub(in crate::client::shell) const SIDE_BY_SIDE_FROM: u16 = 100;
/// Most parallel steps drawn as columns; more fall back to a list.
pub(in crate::client::shell) const MAX_COLUMNS: usize = 4;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(in crate::client::shell) enum FlowRow {
    /// `lead ━▶ role` and the task's progress.
    Header,
    /// Earlier steps beyond the projected list.
    OmittedBefore(u16),
    /// A relay step: an index into `FlowLayout::slots`.
    Step(usize),
    /// The handoff from the step in this slot to the next one.
    Arrow(usize),
    /// `┬ parallel 3/4 done`.
    ParallelHead,
    /// One parallel step in a list.
    Branch { slot: usize, last: bool },
    /// Parallel steps side by side: slots `first..first + count`.
    Columns { first: usize, count: usize },
    /// The integrating step.
    Integrate(usize),
    /// Later steps beyond the projected list.
    OmittedAfter(u16),
    /// A task with no steps: the worker alone.
    Lone,
}

impl FlowRow {
    /// Terminal rows this row takes.
    pub(in crate::client::shell) const fn height(self) -> u16 {
        match self {
            Self::Columns { .. } => 2,
            _ => 1,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(in crate::client::shell) struct StepSlot {
    /// Index into the task's `steps`.
    pub(in crate::client::shell) step: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(in crate::client::shell) struct FlowKey {
    pub(in crate::client::shell) serial: u64,
    pub(in crate::client::shell) task: usize,
    pub(in crate::client::shell) width: u16,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(in crate::client::shell) struct FlowLayout {
    pub(in crate::client::shell) key: FlowKey,
    pub(in crate::client::shell) task_id: String,
    pub(in crate::client::shell) step_indices: Vec<u16>,
    pub(in crate::client::shell) side_by_side: bool,
    pub(in crate::client::shell) rows: Vec<FlowRow>,
    /// Focusable steps in display order; the overlay's selection indexes this.
    pub(in crate::client::shell) slots: Vec<StepSlot>,
    /// `step 2/3`, `parallel 3/4 done`, or empty.
    pub(in crate::client::shell) summary: String,
    /// For a task without steps: the pane of its worker, if it sits in one.
    pub(in crate::client::shell) lone_pane: Option<String>,
}

impl FlowLayout {
    pub(in crate::client::shell) fn build(
        orbit: &OrbitShellView,
        task_index: usize,
        key: FlowKey,
    ) -> Self {
        let mut layout = Self {
            key,
            task_id: String::new(),
            step_indices: Vec::new(),
            side_by_side: key.width >= SIDE_BY_SIDE_FROM,
            rows: vec![FlowRow::Header],
            slots: Vec::new(),
            summary: String::new(),
            lone_pane: None,
        };
        let Some(task) = orbit.active_tasks.get(task_index) else {
            return layout;
        };
        layout.task_id.clone_from(&task.task_id);
        layout.step_indices = task.steps.iter().map(|step| step.index).collect();
        layout.summary = summary(task);
        if task.steps.is_empty() {
            layout.rows.push(FlowRow::Lone);
            layout.lone_pane = orbit
                .roles
                .iter()
                .flat_map(|role| &role.members)
                .chain(&orbit.scratch)
                .find(|member| member.terminal_id.as_deref() == Some(&task.worker_terminal_id))
                .and_then(|member| member.pane_id.clone());
            return layout;
        }
        if let Some(omitted) = task.steps_omitted.filter(|omitted| omitted.before > 0) {
            layout.rows.push(FlowRow::OmittedBefore(omitted.before));
        }
        let parallel: Vec<usize> = task
            .steps
            .iter()
            .enumerate()
            .filter(|(_, step)| step.parallel.is_some())
            .map(|(index, _)| index)
            .collect();
        let mut group_done = false;
        let mut previous_relay = None::<usize>;
        for (index, step) in task.steps.iter().enumerate() {
            if step.parallel.is_some() {
                if group_done {
                    continue;
                }
                group_done = true;
                if let Some(before) = previous_relay.take() {
                    layout.rows.push(FlowRow::Arrow(before));
                }
                layout.push_group(&parallel);
            } else if step.integrate.is_some() {
                let slot = layout.slot(index);
                if let Some(before) = previous_relay.take() {
                    layout.rows.push(FlowRow::Arrow(before));
                }
                layout.rows.push(FlowRow::Integrate(slot));
            } else {
                let slot = layout.slot(index);
                if let Some(before) = previous_relay {
                    layout.rows.push(FlowRow::Arrow(before));
                }
                layout.rows.push(FlowRow::Step(slot));
                previous_relay = Some(slot);
            }
        }
        if let Some(omitted) = task.steps_omitted.filter(|omitted| omitted.after > 0) {
            layout.rows.push(FlowRow::OmittedAfter(omitted.after));
        }
        layout
    }

    fn slot(&mut self, step: usize) -> usize {
        self.slots.push(StepSlot { step });
        self.slots.len() - 1
    }

    fn push_group(&mut self, parallel: &[usize]) {
        self.rows.push(FlowRow::ParallelHead);
        let columns = self.side_by_side && (2..=MAX_COLUMNS).contains(&parallel.len());
        let first = self.slots.len();
        for step in parallel {
            self.slot(*step);
        }
        if columns {
            self.rows.push(FlowRow::Columns {
                first,
                count: parallel.len(),
            });
        } else {
            for (offset, _) in parallel.iter().enumerate() {
                self.rows.push(FlowRow::Branch {
                    slot: first + offset,
                    last: offset + 1 == parallel.len(),
                });
            }
        }
    }

    /// Total terminal rows of the flow body.
    pub(in crate::client::shell) fn height(&self) -> u16 {
        self.rows.iter().map(|row| row.height()).sum()
    }

    /// The first row index and top offset (in body rows) of the row showing `slot`.
    #[cfg(test)]
    pub(in crate::client::shell) fn row_of_slot(&self, slot: usize) -> Option<u16> {
        self.slot_extent(slot).map(|(y, _)| y)
    }

    pub(in crate::client::shell) fn slot_extent(&self, slot: usize) -> Option<(u16, u16)> {
        let mut y = 0;
        for row in &self.rows {
            let shows = match *row {
                FlowRow::Step(s) | FlowRow::Integrate(s) | FlowRow::Branch { slot: s, .. } => {
                    s == slot
                }
                FlowRow::Columns { first, count } => (first..first + count).contains(&slot),
                _ => false,
            };
            if shows {
                return Some((y, row.height()));
            }
            y += row.height();
        }
        None
    }
}

/// `parallel 3/4 done` for a fan-out, else `step 2/3`.
fn summary(task: &OrbitShellActiveTask) -> String {
    match task.parallel {
        Some(parallel) => format!("parallel {}/{} done", parallel.done, parallel.total),
        None => strip(task).map(|strip| strip.line).unwrap_or_default(),
    }
}

/// The popup the flow view uses inside `area`, and its inner rectangle.
pub(in crate::client::shell) fn inner_of(popup: Rect) -> Rect {
    Rect::new(
        popup.x + 1,
        popup.y + 1,
        popup.width.saturating_sub(2),
        popup.height.saturating_sub(2),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::{
        OrbitShellLead, OrbitShellMember, OrbitShellOmitted, OrbitShellParallel, OrbitShellRole,
        OrbitShellStep,
    };

    fn step(index: u16, state: &str) -> OrbitShellStep {
        OrbitShellStep {
            index,
            member: format!("m{index}"),
            kind: "codex".into(),
            state: state.into(),
            note: None,
            next: None,
            parallel: None,
            integrate: None,
            started_at_ms: None,
            finished_at_ms: None,
            worker_terminal_id: Some(format!("term_{index}")),
            pane_id: Some(format!("pane_{index}")),
        }
    }

    fn orbit(steps: Vec<OrbitShellStep>) -> OrbitShellView {
        let task = OrbitShellActiveTask {
            task_id: "t".into(),
            worker_terminal_id: "term_w".into(),
            role: Some("builders".into()),
            title: "title".into(),
            state: "working".into(),
            created_at_ms: 1,
            blocked: false,
            awaiting_approval: false,
            parallel: steps
                .iter()
                .any(|step| step.parallel.is_some())
                .then_some(OrbitShellParallel { done: 1, total: 2 }),
            step_total: Some(steps.len() as u16),
            steps,
            steps_omitted: None,
        };
        OrbitShellView {
            lead: OrbitShellLead {
                name: "Polaris".into(),
                terminal_id: "term_lead".into(),
                pane_id: None,
                state: "idle".into(),
            },
            roles: vec![OrbitShellRole {
                name: "builders".into(),
                pinned: false,
                members: vec![OrbitShellMember {
                    name: "w".into(),
                    kind: "codex".into(),
                    terminal_id: Some("term_w".into()),
                    pane_id: Some("pane_w".into()),
                    state: "working".into(),
                    model: None,
                }],
            }],
            scratch: Vec::new(),
            active_tasks: vec![task],
            recent_tasks: Vec::new(),
            server_now_ms: None,
        }
    }

    fn key(width: u16) -> FlowKey {
        FlowKey {
            serial: 1,
            task: 0,
            width,
        }
    }

    #[test]
    fn a_relay_is_steps_joined_by_arrows() {
        let layout = FlowLayout::build(
            &orbit(vec![
                step(0, "handed_off"),
                step(1, "active"),
                step(2, "pending"),
            ]),
            0,
            key(80),
        );
        assert_eq!(
            layout.rows,
            [
                FlowRow::Header,
                FlowRow::Step(0),
                FlowRow::Arrow(0),
                FlowRow::Step(1),
                FlowRow::Arrow(1),
                FlowRow::Step(2),
            ]
        );
        assert_eq!(layout.slots.len(), 3);
        assert_eq!(layout.summary, "step 2/3");
    }

    #[test]
    fn parallel_steps_are_columns_when_wide_and_a_list_when_narrow() {
        let mut steps = vec![step(0, "handed_off")];
        for index in 1..=2 {
            let mut branch = step(index, "active");
            branch.parallel = Some("running".into());
            steps.push(branch);
        }
        let mut integrator = step(3, "pending");
        integrator.integrate = Some("waiting".into());
        steps.push(integrator);
        let wide = FlowLayout::build(&orbit(steps.clone()), 0, key(120));
        assert!(wide.rows.contains(&FlowRow::Columns { first: 1, count: 2 }));
        assert_eq!(wide.rows.last(), Some(&FlowRow::Integrate(3)));
        assert_eq!(wide.summary, "parallel 1/2 done");
        let narrow = FlowLayout::build(&orbit(steps), 0, key(60));
        assert!(narrow.rows.contains(&FlowRow::Branch {
            slot: 1,
            last: false
        }));
        assert!(narrow.rows.contains(&FlowRow::Branch {
            slot: 2,
            last: true
        }));
        assert_eq!(narrow.slots.len(), 4);
        assert_eq!(
            wide.height(),
            wide.rows.len() as u16 + 1,
            "columns take two rows"
        );
    }

    #[test]
    fn five_parallel_steps_fall_back_to_a_list_and_omitted_steps_are_marked() {
        let mut steps = Vec::new();
        for index in 0..5 {
            let mut branch = step(index, "active");
            branch.parallel = Some("running".into());
            steps.push(branch);
        }
        let mut value = orbit(steps);
        value.active_tasks[0].steps_omitted = Some(OrbitShellOmitted {
            before: 3,
            after: 2,
        });
        let layout = FlowLayout::build(&value, 0, key(200));
        assert!(!layout
            .rows
            .iter()
            .any(|row| matches!(row, FlowRow::Columns { .. })));
        assert_eq!(layout.rows[1], FlowRow::OmittedBefore(3));
        assert_eq!(layout.rows.last(), Some(&FlowRow::OmittedAfter(2)));
    }

    #[test]
    fn a_task_without_steps_is_the_worker_alone_and_a_missing_task_is_empty() {
        let layout = FlowLayout::build(&orbit(Vec::new()), 0, key(80));
        assert_eq!(layout.rows, [FlowRow::Header, FlowRow::Lone]);
        assert_eq!(layout.lone_pane.as_deref(), Some("pane_w"));
        assert!(layout.slots.is_empty());
        let none = FlowLayout::build(&orbit(Vec::new()), 5, key(80));
        assert_eq!(none.rows, [FlowRow::Header]);
    }

    #[test]
    fn rows_know_which_slot_they_show() {
        let layout = FlowLayout::build(
            &orbit(vec![step(0, "handed_off"), step(1, "active")]),
            0,
            key(80),
        );
        assert_eq!(layout.row_of_slot(0), Some(1));
        assert_eq!(layout.row_of_slot(1), Some(3));
        assert_eq!(layout.row_of_slot(9), None);
    }
}

/// Bounded by the 32 projected steps/tasks; drawing never grows a heap buffer.
#[derive(Debug, Clone, Default)]
pub(crate) struct FlowHits {
    entries: [(Rect, usize); 32],
    len: usize,
}

impl FlowHits {
    pub(crate) fn push(&mut self, hit: (Rect, usize)) {
        if let Some(entry) = self.entries.get_mut(self.len) {
            *entry = hit;
            self.len += 1;
        }
    }

    pub(crate) fn iter(&self) -> std::slice::Iter<'_, (Rect, usize)> {
        self.entries[..self.len].iter()
    }

    #[cfg(test)]
    pub(crate) fn is_empty(&self) -> bool {
        self.len == 0
    }
}
