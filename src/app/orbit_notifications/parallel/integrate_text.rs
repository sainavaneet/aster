//! What the integrator is told, who the integrator is, and how its report
//! becomes the task's one report to Polaris.
use std::fmt::Write as _;

use super::integrate::Integrate;
use crate::api::schema::OrbitAgentReportParams;
use crate::app::orbit_notifications::assign::{assignment_instructions, report_command};
use crate::app::orbit_notifications::facts::check_path;
use crate::app::orbit_notifications::state::Task;
use crate::app::App;
use crate::parallel_checkout::MAX_LISTED_FILES;

const MAX_SUMMARY_BYTES: usize = 4000;
/// The role whose first member integrates when nothing is configured.
const REVIEWER_ROLE: &str = "reviewer";

impl App {
    /// The member who integrates the parallel steps of a task given to `role`:
    /// the role's configured `integrator`, else the first member of the
    /// `reviewer` role, else the first member of `role`. Returns its name and
    /// kind.
    pub(crate) fn orbit_integrator(&mut self, role: Option<&str>) -> Option<(String, String)> {
        self.ensure_orbit_team_loaded();
        let roster = &self.orbit_team.roster;
        let first = |role: &crate::app::orbit_team::roster::Role| {
            let member = role.members.iter().find(|member| member.supported())?;
            Some((member.name.clone(), member.kind.clone()))
        };
        let configured = role
            .and_then(|role| self.orbit_team.pinned_config(role))
            .and_then(|config| config.integrator.as_deref());
        if let Some((_, member)) = configured
            .and_then(|name| roster.member(name))
            .filter(|(_, member)| member.supported())
        {
            return Some((member.name.clone(), member.kind.clone()));
        }
        roster
            .role(REVIEWER_ROLE)
            .and_then(first)
            .or_else(|| role.and_then(|role| roster.role(role)).and_then(first))
    }

    /// The integrate step's prompt for `terminal`.
    pub(super) fn integrator_prompt(
        &mut self,
        index: usize,
        step: usize,
        terminal: &str,
    ) -> Option<String> {
        let task = &self.orbit_notifications.tasks[index];
        let integrate = task.steps[step].integrate.as_ref()?;
        let mut text = describe(task, integrate);
        let command = report_command(&task.task_id, terminal, Some(step)).ok()?;
        let task_id = task.task_id.clone();
        if let Some(line) = self.orbit_team_role_instruction(terminal) {
            text.push_str(&line);
        }
        text.push_str(&assignment_instructions(&task_id, &command, false));
        Some(text)
    }
}

fn describe(task: &Task, integrate: &Integrate) -> String {
    let mut text = format!(
        "Integrate task \"{}\": {} parallel steps left patches. Aster applies them to the main tree in step order with `git apply --3way`, which also stages them.\n\nPatches, in apply order:",
        task.title.as_deref().unwrap_or("untitled"),
        integrate.patches.len()
    );
    for patch in &integrate.patches {
        let _ = write!(
            text,
            "\n{}. step {} ({}): {}\n   files: {}\n   patch: {}",
            patch.step + 1,
            patch.step + 1,
            patch.member,
            patch.note,
            patch.files.join(", "),
            patch.path.display()
        );
    }
    for skipped in &integrate.skipped {
        let _ = write!(text, "\nNot included: {skipped}.");
    }
    text.push_str("\n\n");
    match &integrate.stopped {
        None if integrate.present.is_empty() && integrate.recovered.is_empty() => {
            text.push_str(&format!(
                "Aster applied all {} patches cleanly. Review the combined change, run the full test suite, fix any problem you find, then report with the checks you ran.",
                integrate.applied.len()
            ));
        }
        None => text.push_str(&format!(
            "Aster applied {} patches cleanly.{} Review the combined change, run the full test suite, fix any problem you find, then report with the checks you ran.",
            integrate.applied.len(),
            extras(integrate)
        )),
        Some(stopped) => {
            // Every patch after the stopped one, in order; the stopped patch
            // itself too unless resolving its conflicts finishes it.
            let remaining: Vec<String> = integrate
                .patches
                .iter()
                .filter(|p| !integrate.done(p.step))
                .filter(|p| p.step > stopped.step || (p.step == stopped.step && !stopped.conflict))
                .map(|p| format!("{} (step {})", p.path.display(), p.step + 1))
                .collect();
            let what = if stopped.unclear {
                format!(
                    "may be partly or wholly applied already ({}); the tree does not show whether it was applied, so check git status and git diff before anything else",
                    stopped.message
                )
            } else if stopped.conflict {
                format!(
                    "has conflicts in {}; the conflict markers are in the tree",
                    stopped.files.join(", ")
                )
            } else {
                format!("did not apply and changed nothing ({})", stopped.message)
            };
            text.push_str(&extras(integrate));
            let _ = write!(
                text,
                "Aster stopped at the patch of step {} ({}): it {what}. Patches applied before it: {}. Still to apply, in this order (`git apply --3way <patch>`): {}. Resolve what is left, apply the rest, run the full test suite, then report.",
                stopped.step + 1,
                stopped.path.display(),
                list_steps(&[integrate.applied.clone(), integrate.recovered.clone()].concat()),
                if remaining.is_empty() { "none".to_owned() } else { remaining.join("; ") },
            );
        }
    }
    text.push_str("\n\nThis step authorises you, whatever your role usually limits you to (for example never editing product code), to resolve the integration conflicts and to fix test failures that the integration caused. Make no other changes.");
    text.push_str("\nIf the integration has to be abandoned, undo it with `git apply -R --index <patch>` for each patch Aster applied, in reverse order (not the ones that were already present); that restores both the files and the index.");
    text
}

/// Sentences about patches Aster did not apply itself.
fn extras(integrate: &Integrate) -> String {
    let mut text = String::new();
    if !integrate.present.is_empty() {
        text.push_str(&format!(
            " Already in the tree, so Aster neither applied nor staged them (the same change had been made some other way), and an undo must not reverse them: {}.",
            list_steps(&integrate.present)
        ));
    }
    if !integrate.recovered.is_empty() {
        text.push_str(&format!(
            " After a restart without a journal, steps {} were found in the tree and are assumed to have been applied by Aster; verify with git diff --cached.",
            list_steps(&integrate.recovered)
        ));
    }
    text.push(' ');
    text
}

fn list_steps(steps: &[usize]) -> String {
    if steps.is_empty() {
        return "none".into();
    }
    steps
        .iter()
        .map(|s| (s + 1).to_string())
        .collect::<Vec<_>>()
        .join(", ")
}

/// The integrator's report as Polaris receives it: changed paths are every
/// step's patch paths plus the integrator's, and the summary names the patches
/// and how to undo them. The integrator's checks and outcome are unchanged.
/// Deterministic, so a repeated report merges to the same thing.
pub(in crate::app::orbit_notifications) fn merge_report(
    task: &Task,
    mut report: OrbitAgentReportParams,
) -> OrbitAgentReportParams {
    let Some(integrate) = task
        .integrate_step()
        .and_then(|step| task.steps[step].integrate.as_ref())
    else {
        return report;
    };
    let mut files: Vec<String> = integrate
        .patches
        .iter()
        .flat_map(|p| p.files.clone())
        .collect();
    files.extend(report.changed_files.take().unwrap_or_default());
    files.sort();
    files.dedup();
    files.retain(|path| check_path("changed_files", path).is_ok());
    files.truncate(MAX_LISTED_FILES);
    report.changed_files = (!files.is_empty()).then_some(files);
    let named = |present: bool| -> Vec<String> {
        integrate
            .patches
            .iter()
            .filter(|p| integrate.present.contains(&p.step) == present)
            .map(|p| p.path.display().to_string())
            .collect()
    };
    let (ours, already) = (named(false), named(true));
    let mut reversed = ours.clone();
    reversed.reverse();
    let mut addendum = format!(
        "\n\nIntegrated {} patches in order: {}. To undo, run git apply -R --index on each in reverse order: {}.",
        ours.len(),
        ours.join(", "),
        reversed.join(", ")
    );
    if !already.is_empty() {
        addendum.push_str(&format!(
            " Already present in the tree, not applied by Aster (do not reverse): {}.",
            already.join(", ")
        ));
    }
    let room = MAX_SUMMARY_BYTES.saturating_sub(addendum.len());
    let mut summary = report.summary;
    while summary.len() > room {
        summary.pop();
    }
    report.summary = summary + &addendum;
    while report.summary.len() > MAX_SUMMARY_BYTES {
        report.summary.pop();
    }
    report
}
