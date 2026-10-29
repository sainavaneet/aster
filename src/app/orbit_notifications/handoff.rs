//! Relaying one task across the members of a role. A report carrying a
//! `handoff` ends the reporter's step instead of the task: Aster picks the
//! next member, prompts it with the task title and the handoff note, and keeps
//! the task awaiting a report. Polaris sees one task and one final report.
use std::fmt::Write as _;

use serde_json::{json, Value};

use super::assign::{assignment_instructions, report_command};
use super::lifecycle::server_notice;
use super::notify::bounded_text;
use super::redact::redact_secrets;
use super::state::{Ack, ReportOrigin, Task};
use super::steps::{Relay, Step, StepState};
use super::{token, App};
use crate::api::schema::{
    OrbitAgentPromptParams, OrbitAgentReportParams, OrbitHandoff, OrbitOutcome,
};
use crate::detect::{parse_agent_label, Agent, AgentState};

/// `blocked_on` of a task whose reporter asked for a handoff nobody can take.
pub(super) const NO_MEMBER: &str = "no member available for handoff";
const NOT_IN_ROLE: &str =
    "A handoff needs a member of a team role; report the task again without handoff";
const STALE_STEP: &str = "This report belongs to an earlier step of the task, which was already handed off; report for your current step";
const STEP_REQUIRED: &str = "This task is relayed across several steps; report with the step listed in your assignment's json_argument";
const INTEGRATE_DIRECT: &str =
    "The integration step reports its result directly; report again without handoff";
const NOT_CURRENT: &str = "Handoff refused: you are not the member of the task's current step";
/// Changed paths listed in a follow-up prompt.
const SHOWN_FILES: usize = 20;
const NOTICE_NEXT_CHARS: usize = 300;

enum Gate {
    Pass,
    Report,
    Retry(Value),
}

/// An idle member that could take the task over.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Candidate {
    member: String,
    kind: Option<Agent>,
    terminal_id: String,
}

/// The candidates in the order a handoff tries them: the member the reporter
/// named, then the role pipeline's next kind, then any other kind than the
/// reporter's.
fn rank(
    candidates: &[Candidate],
    to: Option<&str>,
    pipeline_kind: Option<Agent>,
    current: Option<Agent>,
) -> Vec<Candidate> {
    let tiers: [&dyn Fn(&Candidate) -> bool; 3] = [
        &|candidate| to == Some(candidate.member.as_str()),
        &|candidate| pipeline_kind.is_some() && candidate.kind == pipeline_kind,
        &|candidate| candidate.kind != current,
    ];
    let mut ordered: Vec<Candidate> = Vec::new();
    for tier in tiers {
        for candidate in candidates {
            if tier(candidate) && !ordered.iter().any(|o| o.member == candidate.member) {
                ordered.push(candidate.clone());
            }
        }
    }
    ordered
}

/// The follow-up the next member receives, ahead of the report instructions.
pub(super) fn handoff_text(
    title: Option<&str>,
    from: &str,
    number: usize,
    handoff: &OrbitHandoff,
) -> String {
    let mut text = format!(
        "Continue task \"{}\", relayed from {from} (step {number}).\n\nWhat {from} finished:\n{}\n\nWhat you should do next:\n{}",
        title.unwrap_or("untitled"),
        handoff.done,
        handoff.next
    );
    if !handoff.changed_files.is_empty() {
        let _ = write!(text, "\n\nFiles changed so far:");
        for path in handoff.changed_files.iter().take(SHOWN_FILES) {
            let _ = write!(text, "\n- {path}");
        }
        let more = handoff.changed_files.len().saturating_sub(SHOWN_FILES);
        if more > 0 {
            let _ = write!(text, "\n… {more} more");
        }
    }
    if let Some(issues) = &handoff.known_issues {
        let _ = write!(text, "\n\nKnown issues:\n{issues}");
    }
    redact_secrets(&text)
}

impl App {
    /// The kinds a task relayed within `role` passes through, from the role's
    /// `[[polaris.team]]` entry; empty for other roles.
    pub(crate) fn orbit_role_pipeline(&mut self, role: &str) -> Vec<String> {
        self.ensure_orbit_team_loaded();
        self.orbit_team
            .pinned_config(role)
            .map(|config| config.pipeline.clone())
            .unwrap_or_default()
    }

    /// Step 1 of a task given to a member of a role with other members; empty
    /// for any other worker, whose task cannot be relayed.
    pub(super) fn initial_orbit_steps(&mut self, worker_id: &str, now: u64) -> Vec<Step> {
        let Some((_, member)) = self.orbit_team_member_of_terminal(worker_id) else {
            return Vec::new();
        };
        match self.orbit_team.roster.member(&member) {
            Some((role, member)) if role.members.len() > 1 => {
                vec![Step::active(0, &member.name, &member.kind, now)]
            }
            _ => Vec::new(),
        }
    }

    /// `step 2/3 reviewer-codex` for a relayed task, else `None`. The total is
    /// the role's pipeline length, or the steps so far.
    pub(super) fn orbit_step_line(&self, task: &Task) -> Option<String> {
        if let Some(line) = task.parallel_line() {
            return Some(line);
        }
        let member = task.steps.last()?.member.as_str();
        let total = self
            .orbit_team
            .roster
            .member(member)
            .map_or(0, |(role, _)| {
                self.orbit_team
                    .pinned_config(&role.name)
                    .map_or(0, |config| config.pipeline.len())
            });
        if task.steps.len() < 2 && total < 2 {
            return None;
        }
        task.step_line(total)
    }

    /// Relays the task when `params` carries a handoff from the member of its
    /// current step and returns the answer to the reporter. `None` means the
    /// report is admitted as an ordinary one (a refused one is rejected there),
    /// possibly turned into a blocked report when nobody can take the handoff.
    pub(super) fn relay_orbit_handoff(
        &mut self,
        id: &str,
        params: &mut OrbitAgentReportParams,
        now: u64,
    ) -> Option<Result<Value, String>> {
        if let Some(answer) = self.stale_step_answer(params) {
            return Some(answer);
        }
        let handoff = params.handoff.clone()?;
        let index = self.orbit_notifications.find(&params.task_id)?;
        match self.handoff_gate(index, params) {
            Gate::Pass => {}
            Gate::Report => return None,
            Gate::Retry(task) => return Some(Ok(task)),
        }
        let Some((role, member)) = self.orbit_team_member_of_terminal(&params.worker_id) else {
            return Some(Err(NOT_IN_ROLE.into()));
        };
        let task = &self.orbit_notifications.tasks[index];
        let current = task
            .current_step()
            .map(|step| (step.index, step.member.clone()));
        if !task.steps.is_empty() && current.as_ref().is_none_or(|(_, name)| *name != member) {
            return Some(Err(NOT_CURRENT.into()));
        }
        if task
            .current_step()
            .is_some_and(|step| step.integrate.is_some())
        {
            return Some(Err(INTEGRATE_DIRECT.into()));
        }
        let step_index = current.map_or(0, |(index, _)| index);
        let kind = self
            .orbit_team
            .roster
            .member(&member)
            .map(|(_, member)| (member.kind.clone(), member.agent()));
        let (kind_label, current_kind) = kind.unwrap_or_default();
        let pipeline = self.orbit_role_pipeline(&role);
        let pipeline_kind = pipeline
            .get(step_index + 1)
            .and_then(|kind| parse_agent_label(kind));
        if let Some(requests) = handoff.parallel.clone() {
            return Some(self.fan_out_handoff(
                index,
                (&role, &member, &kind_label),
                &handoff,
                &requests,
                now,
            ));
        }
        let candidates = self.handoff_candidates(&role, &member);
        let ranked = rank(
            &candidates,
            handoff.to.as_deref(),
            pipeline_kind,
            current_kind,
        );
        for candidate in ranked {
            let ack = match self.queue_handoff(id, index, &candidate, &member, &handoff) {
                Ok(ack) => ack,
                Err(error) => {
                    tracing::warn!(%error, member = %candidate.member, "orbit handoff prompt not queued");
                    continue;
                }
            };
            let next = Relay {
                worker_id: candidate.terminal_id,
                member: candidate.member,
                kind: candidate.kind.map_or_else(String::new, |kind| {
                    crate::detect::agent_label(kind).to_owned()
                }),
            };
            let task = self.orbit_notifications.task_mut(index);
            task.begin_steps(&member, &kind_label);
            task.relay_to(handoff, next, ack, now);
            return Some(Ok(json!(&self.orbit_notifications.tasks[index])));
        }
        self.block_unrelayed_task(index, &role, &member, &kind_label, &handoff, now);
        params.outcome = OrbitOutcome::Blocked;
        params.blocked_on = Some(NO_MEMBER.into());
        None
    }

    /// Whether a handoff report is a worker's own report, a retry of one
    /// already relayed, or must be admitted (and rejected) as an ordinary report.
    fn handoff_gate(&self, index: usize, params: &OrbitAgentReportParams) -> Gate {
        let task = &self.orbit_notifications.tasks[index];
        let valid_token = token::accepts(task.token_hash.as_ref(), params.token.as_deref());
        if task.origin == Some(ReportOrigin::Server) || !valid_token {
            return Gate::Report;
        }
        if task.worker_id != params.worker_id {
            return match self.is_relayed_retry(task, params) {
                true => Gate::Retry(json!(task)),
                false => Gate::Report,
            };
        }
        if !task.awaits_worker() || task.report.is_some() {
            return Gate::Report;
        }
        Gate::Pass
    }

    /// A report bound to a step other than the task's current one (a delayed
    /// report, for example from a member that has since been handed the task
    /// again) never acts on the current step: the identical handoff it made is
    /// acknowledged, anything else is refused. `None` for reports that name no
    /// step or the current one, and for tokens that do not authenticate.
    fn stale_step_answer(&self, params: &OrbitAgentReportParams) -> Option<Result<Value, String>> {
        let notifications = &self.orbit_notifications;
        let task = &notifications.tasks[notifications.find(&params.task_id)?];
        let valid_token = token::accepts(task.token_hash.as_ref(), params.token.as_deref());
        if task.steps.is_empty() || task.origin == Some(ReportOrigin::Server) || !valid_token {
            return None;
        }
        let live = task.current_step().or(task.steps.last())?;
        let Some(step) = params.step else {
            return self.stepless_answer(task, live, params);
        };
        let live = live.index;
        if step == live {
            return None;
        }
        let member = self
            .loaded_orbit_team_member_of_terminal(&params.worker_id)
            .map(|(_, member)| member);
        let repeated = task.steps.get(step).is_some_and(|earlier| {
            earlier.state == StepState::HandedOff
                && params.handoff.is_some()
                && earlier.handoff == params.handoff
                && member.as_deref() == Some(earlier.member.as_str())
        });
        Some(if repeated {
            Ok(json!(task))
        } else {
            Err(STALE_STEP.into())
        })
    }

    /// A report without a step on a relayed task acts only when it is
    /// unambiguous: the reporter owns the task, appears in exactly one step,
    /// and that step is the current one. Otherwise it could be a delayed report
    /// of an earlier step, so it is refused. Reports from other workers are
    /// left to the ordinary identity checks.
    fn stepless_answer(
        &self,
        task: &Task,
        live: &Step,
        params: &OrbitAgentReportParams,
    ) -> Option<Result<Value, String>> {
        if task.worker_id != params.worker_id {
            return None;
        }
        let member = self
            .loaded_orbit_team_member_of_terminal(&params.worker_id)
            .map(|(_, member)| member);
        let own_steps = task
            .steps
            .iter()
            .filter(|step| member.as_deref() == Some(step.member.as_str()))
            .count();
        let unambiguous = own_steps == 1 && member.as_deref() == Some(live.member.as_str());
        (!unambiguous).then(|| Err(STEP_REQUIRED.into()))
    }

    /// The identical handoff, repeated by the member that already made it.
    fn is_relayed_retry(&self, task: &Task, params: &OrbitAgentReportParams) -> bool {
        let Some((_, member)) = self.loaded_orbit_team_member_of_terminal(&params.worker_id) else {
            return false;
        };
        task.steps.iter().any(|step| {
            step.state == StepState::HandedOff
                && step.member == member
                && step.handoff == params.handoff
        })
    }

    /// Idle members of `role` other than `current` with no open assignment.
    fn handoff_candidates(&mut self, role: &str, current: &str) -> Vec<Candidate> {
        let Some(role) = self.orbit_team.roster.role(role).cloned() else {
            return Vec::new();
        };
        let Ok((ws, tab)) = self.resolve_role_tab(&role) else {
            return Vec::new();
        };
        let mut candidates = Vec::new();
        for member in &role.members {
            if member.name == current || !member.supported() {
                continue;
            }
            let Some((_, terminal_id)) = self.orbit_member_pane(ws, tab, &member.name) else {
                continue;
            };
            let idle = self
                .state
                .terminals
                .get(terminal_id.as_str())
                .is_some_and(|terminal| terminal.state == AgentState::Idle);
            if idle && !self.orbit_worker_has_open_task(&terminal_id) {
                candidates.push(Candidate {
                    member: member.name.clone(),
                    kind: member.agent(),
                    terminal_id,
                });
            }
        }
        candidates
    }

    /// Prompts the candidate with the follow-up and the report instructions
    /// for the same task; its report uses the task's existing token.
    fn queue_handoff(
        &mut self,
        id: &str,
        index: usize,
        candidate: &Candidate,
        from: &str,
        handoff: &OrbitHandoff,
    ) -> Result<Ack, String> {
        let task = &self.orbit_notifications.tasks[index];
        let task_id = task.task_id.clone();
        let number = task.steps.len().max(1) + 1;
        let text = handoff_text(task.title.as_deref(), from, number, handoff);
        let command = report_command(&task_id, &candidate.terminal_id, Some(number - 1))?;
        let mut prompt = self.orbit_agent_prompt_params(
            id,
            OrbitAgentPromptParams {
                name: candidate.terminal_id.clone(),
                prompt: text,
                role: None,
                kind: None,
            },
            true,
        )?;
        if let Some(line) = self.orbit_worktree_instruction(&candidate.terminal_id) {
            prompt.text.push_str(&line);
        }
        if let Some(line) = self.orbit_team_role_instruction(&candidate.terminal_id) {
            prompt.text.push_str(&line);
        }
        prompt
            .text
            .push_str(&assignment_instructions(&task_id, &command, true));
        self.queue_orbit_prompt(id.into(), &prompt)
            .map_err(|error| error.to_string())
    }

    /// Nobody can take the handoff: the reporter's step ends as handed off
    /// (its note kept), and Polaris gets a server notice next to the blocked report.
    fn block_unrelayed_task(
        &mut self,
        index: usize,
        role: &str,
        member: &str,
        kind: &str,
        handoff: &OrbitHandoff,
        now: u64,
    ) {
        let task = self.orbit_notifications.task_mut(index);
        task.begin_steps(member, kind);
        task.end_step_with_handoff(handoff.clone(), now);
        let next = redact_secrets(&bounded_text(&handoff.next, NOTICE_NEXT_CHARS));
        let summary = format!(
            "Task {} is blocked: {member} handed it off but {NO_MEMBER} in role {role}. Next step it asked for: {next}",
            task.task_id
        );
        let notice = server_notice(
            task,
            format!("{}/handoff-blocked", task.task_id),
            OrbitOutcome::Blocked,
            summary,
            now,
        );
        self.orbit_notifications.insert_notice(notice);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn candidate(member: &str, kind: Agent) -> Candidate {
        Candidate {
            member: member.into(),
            kind: Some(kind),
            terminal_id: format!("term-{member}"),
        }
    }

    fn names(ranked: &[Candidate]) -> Vec<&str> {
        ranked.iter().map(|c| c.member.as_str()).collect()
    }

    #[test]
    fn rank_prefers_the_named_member_then_the_pipeline_kind_then_another_kind() {
        let all = [
            candidate("a-claude", Agent::Claude),
            candidate("b-codex", Agent::Codex),
            candidate("c-codex", Agent::Codex),
        ];
        let named = rank(&all, Some("c-codex"), None, Some(Agent::Claude));
        assert_eq!(names(&named), ["c-codex", "b-codex"]);
        let piped = rank(&all, None, Some(Agent::Codex), Some(Agent::Codex));
        assert_eq!(names(&piped), ["b-codex", "c-codex", "a-claude"]);
        let other = rank(&all, Some("missing"), None, Some(Agent::Claude));
        assert_eq!(names(&other), ["b-codex", "c-codex"]);
        assert!(rank(&all[..1], None, None, Some(Agent::Claude)).is_empty());
    }

    #[test]
    fn only_relayable_assignments_explain_the_handoff_object() {
        let command = json!({"json_argument": {}});
        assert!(!assignment_instructions("t", &command, false).contains("handoff"));
        let relayed = assignment_instructions("t", &command, true);
        assert!(relayed.contains("handoff object") && relayed.contains("at most 1000"));
    }

    #[test]
    fn follow_up_names_the_task_the_note_and_redacts_secrets() {
        let handoff = OrbitHandoff {
            done: "Wrote the parser".into(),
            next: "Review it".into(),
            changed_files: (0..25).map(|n| format!("src/f{n}.rs")).collect(),
            known_issues: Some("token=ghp_0123456789abcdef0123456789abcdef0123".into()),
            to: None,
            parallel: None,
        };
        let text = handoff_text(Some("Parser work"), "rc", 2, &handoff);
        assert!(
            text.contains("\"Parser work\", relayed from rc (step 2)"),
            "{text}"
        );
        assert!(text.contains("Wrote the parser") && text.contains("Review it"));
        assert!(text.contains("- src/f19.rs") && !text.contains("src/f20.rs"));
        assert!(text.contains("… 5 more"), "{text}");
        assert!(!text.contains("ghp_0123"), "{text}");
    }
}
