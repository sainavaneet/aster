//! The relay and parallel part of the Orbit shell projection: identity of every
//! step (bounded), handoff notes for a bounded window, and totals that do not
//! depend on the window. Built with the rest of the view on the server tick.
use std::collections::BTreeSet;

use super::parallel::{IntegratePhase, Phase};
use super::shell_view::display_first_line;
use super::state::Task;
use super::steps::{Step, StepState};
use crate::protocol::{OrbitShellOmitted, OrbitShellParallel, OrbitShellStep};

/// Steps whose identity is projected.
const MAX_STEPS: usize = 32;
/// Steps that also carry handoff notes.
const NOTE_WINDOW: usize = 8;
const NOTE_MAX_CHARS: usize = 80;

#[derive(Default)]
pub(super) struct ShellSteps {
    pub(super) steps: Vec<OrbitShellStep>,
    pub(super) step_total: Option<u16>,
    pub(super) parallel: Option<OrbitShellParallel>,
    pub(super) omitted: Option<OrbitShellOmitted>,
}

fn saturating_u16(value: usize) -> u16 {
    u16::try_from(value).unwrap_or(u16::MAX)
}

fn state_name(state: StepState) -> &'static str {
    match state {
        StepState::Pending => "pending",
        StepState::Active => "active",
        StepState::Done => "done",
        StepState::HandedOff => "handed_off",
        StepState::Failed => "failed",
    }
}

fn parallel_phase(phase: Phase) -> &'static str {
    match phase {
        Phase::Waiting => "waiting",
        Phase::Launching => "launching",
        Phase::Starting => "starting",
        Phase::Running => "running",
        Phase::Capturing => "capturing",
    }
}

fn integrate_phase(phase: IntegratePhase) -> &'static str {
    match phase {
        IntegratePhase::Waiting => "waiting",
        IntegratePhase::Applying => "applying",
        IntegratePhase::Ready => "ready",
        IntegratePhase::Running => "running",
    }
}

/// The indices (into the identity list) that carry notes: every active step,
/// every parallel and integrate step, then the nearest to the active one.
fn note_window(steps: &[Step]) -> BTreeSet<usize> {
    let mut window = BTreeSet::new();
    let centre = steps
        .iter()
        .rposition(|step| step.state == StepState::Active)
        .unwrap_or(steps.len().saturating_sub(1));
    let mut by_priority: Vec<usize> = (0..steps.len()).collect();
    by_priority.sort_by_key(|position| {
        let step = &steps[*position];
        let priority = if step.state == StepState::Active {
            0
        } else if step.parallel.is_some() || step.integrate.is_some() {
            1
        } else {
            2
        };
        (priority, position.abs_diff(centre))
    });
    for position in by_priority.into_iter().take(NOTE_WINDOW) {
        window.insert(position);
    }
    window
}

impl Task {
    /// The projected steps of this task; `total` is the role's pipeline length.
    /// Empty for a task that is neither relayed nor fanned out.
    pub(super) fn shell_steps(&self, total: usize) -> ShellSteps {
        let fanned = self.steps.iter().any(|step| step.parallel.is_some());
        if self.steps.is_empty() || (!fanned && self.steps.len() < 2 && total < 2) {
            return ShellSteps::default();
        }
        let start = if self.steps.len() > MAX_STEPS {
            let active = self
                .steps
                .iter()
                .rposition(|step| step.state == StepState::Active)
                .unwrap_or(self.steps.len() - 1);
            active
                .saturating_sub(MAX_STEPS / 2)
                .min(self.steps.len() - MAX_STEPS)
        } else {
            0
        };
        let listed = &self.steps[start..self.steps.len().min(start + MAX_STEPS)];
        let window = note_window(listed);
        let steps = listed
            .iter()
            .enumerate()
            .filter_map(|(position, step)| {
                let index = u16::try_from(step.index).ok()?;
                let noted = window.contains(&position);
                let note = |text: &str| noted.then(|| display_first_line(text, NOTE_MAX_CHARS));
                let handoff = step.handoff.as_ref();
                Some(OrbitShellStep {
                    index,
                    member: step.member.clone(),
                    kind: step.kind.clone(),
                    state: state_name(step.state).to_owned(),
                    note: handoff.and_then(|handoff| note(&handoff.done)),
                    next: handoff.and_then(|handoff| note(&handoff.next)),
                    parallel: step
                        .parallel
                        .as_ref()
                        .map(|parallel| parallel_phase(parallel.phase).to_owned()),
                    integrate: step
                        .integrate
                        .as_ref()
                        .map(|integrate| integrate_phase(integrate.phase).to_owned()),
                    started_at_ms: noted.then_some(step.started_at).flatten(),
                    finished_at_ms: noted.then_some(step.finished_at).flatten(),
                    // Filled by the view, which knows terminals and panes.
                    worker_terminal_id: None,
                    pane_id: None,
                    model: None,
                })
            })
            .collect();
        let parallel_steps: Vec<&Step> = self
            .steps
            .iter()
            .filter(|step| step.parallel.is_some())
            .collect();
        let parallel = (!parallel_steps.is_empty()).then(|| OrbitShellParallel {
            done: saturating_u16(
                parallel_steps
                    .iter()
                    .filter(|step| matches!(step.state, StepState::Done | StepState::HandedOff))
                    .count(),
            ),
            total: saturating_u16(parallel_steps.len()),
        });
        let unrepresentable = listed
            .iter()
            .filter(|step| u16::try_from(step.index).is_err())
            .count();
        let after = self.steps.len() - (start + listed.len()) + unrepresentable;
        let omitted = (start > 0 || after > 0).then(|| OrbitShellOmitted {
            before: saturating_u16(start),
            after: saturating_u16(after),
        });
        ShellSteps {
            steps,
            step_total: Some(saturating_u16(total.max(self.steps.len()))),
            parallel,
            omitted,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn task(steps: serde_json::Value) -> Task {
        serde_json::from_value(serde_json::json!({
            "task_id": "t", "worker_id": "w0", "lead_id": "l",
            "submission": "submitted", "delivery": "awaiting_report", "error": null,
            "created_at": 1_000, "steps": steps
        }))
        .unwrap()
    }

    fn step(index: usize, state: &str) -> serde_json::Value {
        serde_json::json!({
            "index": index, "member": format!("m{index}"), "kind": "claude", "state": state,
            "handoff": {"done": format!("done {index}"), "next": "n", "changed_files": []}
        })
    }

    #[test]
    fn an_unrelayed_task_projects_nothing() {
        assert!(task(serde_json::json!([])).shell_steps(0).steps.is_empty());
        let single = task(serde_json::json!([step(0, "active")]));
        assert!(single.shell_steps(1).steps.is_empty());
        assert!(single.shell_steps(0).step_total.is_none());
    }

    #[test]
    fn a_relay_projects_identity_total_and_notes() {
        let relay = task(serde_json::json!([
            step(0, "handed_off"),
            step(1, "active")
        ]));
        let shell = relay.shell_steps(3);
        assert_eq!(shell.step_total, Some(3));
        assert_eq!(shell.steps.len(), 2);
        assert_eq!(shell.steps[0].state, "handed_off");
        assert_eq!(shell.steps[0].note.as_deref(), Some("done 0"));
        assert!(shell.omitted.is_none() && shell.parallel.is_none());
    }

    #[test]
    fn a_long_relay_keeps_identity_for_32_steps_and_notes_for_8() {
        let steps: Vec<_> = (0..40)
            .map(|index| step(index, if index == 39 { "active" } else { "handed_off" }))
            .collect();
        let shell = task(serde_json::Value::Array(steps)).shell_steps(40);
        assert_eq!(shell.steps.len(), MAX_STEPS);
        assert_eq!(shell.step_total, Some(40));
        let omitted = shell.omitted.expect("steps beyond the list are counted");
        assert_eq!((omitted.before, omitted.after), (8, 0));
        assert_eq!(shell.steps.last().map(|step| step.index), Some(39));
        assert_eq!(
            shell
                .steps
                .iter()
                .filter(|step| step.note.is_some())
                .count(),
            NOTE_WINDOW
        );
        assert!(
            shell.steps.last().unwrap().note.is_some(),
            "active step is in the window"
        );
        assert!(shell.steps.iter().all(|step| !step.member.is_empty()));
    }

    #[test]
    fn parallel_totals_cover_steps_outside_the_note_window() {
        let mut steps = vec![step(0, "handed_off")];
        for index in 1..=4 {
            let mut parallel = step(index, if index <= 3 { "done" } else { "active" });
            parallel["parallel"] = serde_json::json!({
                "prompt": "p", "files": [], "phase": "running"
            });
            steps.push(parallel);
        }
        let shell = task(serde_json::Value::Array(steps)).shell_steps(2);
        let parallel = shell.parallel.expect("a fan-out reports totals");
        assert_eq!((parallel.done, parallel.total), (3, 4));
        assert_eq!(shell.steps[4].parallel.as_deref(), Some("running"));
    }
    #[test]
    fn active_notes_outrank_old_parallel_steps_and_detail_is_bounded() {
        let mut steps: Vec<_> = (0..12)
            .map(|index| {
                let mut value = step(index, if index == 11 { "active" } else { "done" });
                value["started_at"] = serde_json::json!(1000);
                if index < 10 {
                    value["parallel"] =
                        serde_json::json!({"prompt":"p", "files":[], "phase":"running"});
                }
                value
            })
            .collect();
        steps[11]["handoff"]["done"] =
            serde_json::json!("0123456789abcdef\n0123456789abcdef\nprivate second line");
        let shell = task(serde_json::Value::Array(steps)).shell_steps(12);
        let active = shell.steps.last().unwrap();
        assert!(active.note.as_ref().unwrap().contains("<redacted>"));
        assert!(!active.note.as_ref().unwrap().contains("private"));
        assert_eq!(
            shell
                .steps
                .iter()
                .filter(|step| step.note.is_some())
                .count(),
            NOTE_WINDOW
        );
        assert!(shell
            .steps
            .iter()
            .filter(|step| step.note.is_none())
            .all(|step| step.started_at_ms.is_none()));
    }
    #[test]
    fn indices_beyond_the_wire_range_are_omitted_instead_of_aliased() {
        let shell = task(serde_json::json!([
            step(65_534, "done"),
            step(65_535, "done"),
            step(65_536, "done"),
            step(65_537, "active")
        ]))
        .shell_steps(65_538);
        assert_eq!(
            shell
                .steps
                .iter()
                .map(|step| step.index)
                .collect::<Vec<_>>(),
            [65_534, 65_535]
        );
        assert_eq!(shell.omitted.unwrap().after, 2);
        assert_eq!(shell.step_total, Some(u16::MAX));
    }
}
