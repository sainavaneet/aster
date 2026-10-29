//! Turning a parallel request (on `orbit.agent_assign` or a handoff) into steps.
use serde_json::{json, Value};

use super::{shared_path, Parallel, MAX_PARALLEL};
use crate::api::schema::{OrbitAgentAssignParams, OrbitHandoff, OrbitParallelStep};
use crate::app::orbit_notifications::assign::assignment_timeout;
use crate::app::orbit_notifications::facts::{check_note, check_path, check_text};
use crate::app::orbit_notifications::handoff::handoff_text;
use crate::app::orbit_notifications::state::{Delivery, LiveState, Submission, Task};
use crate::app::orbit_notifications::steps::Step;
use crate::app::orbit_notifications::{response_error, shell_view, token};
use crate::app::App;
use crate::detect::{agent_label, parse_agent_label, Agent};

const MAX_STEP_PROMPT_CHARS: usize = 8000;
const MAX_STEP_FILES: usize = 50;
const MAX_NAME_BYTES: usize = 64;

/// The shape of a parallel request, without resolving who runs it.
pub(in crate::app::orbit_notifications) fn check_requests(
    requests: &[OrbitParallelStep],
) -> Result<(), String> {
    if requests.is_empty() || requests.len() > MAX_PARALLEL {
        return Err(format!("parallel must list 1 to {MAX_PARALLEL} steps"));
    }
    for (index, request) in requests.iter().enumerate() {
        let field = |name: &str| format!("parallel[{index}].{name}");
        check_text(
            &field("member_or_kind"),
            &request.member_or_kind,
            MAX_NAME_BYTES,
        )?;
        check_note(&field("prompt"), &request.prompt, MAX_STEP_PROMPT_CHARS)?;
        if let Some(model) = &request.model {
            check_text(&field("model"), model, MAX_NAME_BYTES)?;
        }
        if request.files.is_empty() || request.files.len() > MAX_STEP_FILES {
            return Err(format!(
                "{} must declare 1 to {MAX_STEP_FILES} paths the step will change, so overlapping steps can run one after another",
                field("files")
            ));
        }
        for (at, path) in request.files.iter().enumerate() {
            check_path(&format!("parallel[{index}].files[{at}]"), path)?;
        }
    }
    Ok(())
}

/// A request resolved to the agent that will run it.
pub(in crate::app::orbit_notifications) struct Prepared {
    member: String,
    kind: String,
    model: Option<String>,
    permission_mode: Option<String>,
    prompt: String,
    files: Vec<String>,
}

/// The note on a step that waits for an earlier step of its own task.
pub(super) fn earlier_step_note(number: usize, path: &str) -> String {
    format!("overlaps step {number} on {path}; runs after it")
}

impl App {
    /// Resolves each request to a kind, model, and permission mode: a team
    /// member's own, or the kind named, checked against `[polaris.workers]`.
    pub(in crate::app::orbit_notifications) fn prepare_parallel(
        &mut self,
        requests: &[OrbitParallelStep],
    ) -> Result<Vec<Prepared>, String> {
        check_requests(requests)?;
        self.ensure_orbit_team_loaded();
        requests
            .iter()
            .enumerate()
            .map(|(index, request)| {
                let named = self.orbit_team.roster.member(&request.member_or_kind);
                let (kind, model, permission_mode) = match named {
                    Some((_, member)) => (
                        member.kind.clone(),
                        member.model.clone(),
                        member.permission_mode.clone(),
                    ),
                    None => match parse_agent_label(&request.member_or_kind) {
                        Some(agent @ (Agent::Claude | Agent::Codex)) => {
                            (agent_label(agent).to_owned(), request.model.clone(), None)
                        }
                        _ => {
                            return Err(format!(
                                "parallel[{index}].member_or_kind {:?} is neither a team member nor a kind (claude or codex)",
                                request.member_or_kind
                            ))
                        }
                    },
                };
                self.orbit_worker_policy
                    .admit_launch(&kind, model.clone(), permission_mode.as_deref())
                    .map_err(|(_, message)| format!("parallel[{index}]: {message}"))?;
                Ok(Prepared {
                    member: request.member_or_kind.clone(),
                    kind,
                    model,
                    permission_mode,
                    prompt: request.prompt.clone(),
                    files: request.files.clone(),
                })
            })
            .collect()
    }
}

/// Steps numbered from `first`, each waiting to launch. `brief` leads every
/// step's prompt. A step that shares paths with an earlier step of the batch
/// says so in its log and runs after it.
pub(in crate::app::orbit_notifications) fn parallel_steps(
    prepared: Vec<Prepared>,
    first: usize,
    brief: &str,
    now: u64,
) -> Vec<Step> {
    let mut steps: Vec<Step> = Vec::with_capacity(prepared.len());
    for (offset, item) in prepared.into_iter().enumerate() {
        let mut parallel = Parallel::new(
            format!("{brief}\n\nYour part:\n{}", item.prompt),
            item.files,
        );
        parallel.model = item.model;
        parallel.permission_mode = item.permission_mode;
        for (earlier, step) in steps.iter().enumerate() {
            let files = step.parallel.as_ref().map(|p| p.files.as_slice());
            if let Some(path) = files.and_then(|files| shared_path(&parallel.files, files)) {
                parallel.note(earlier_step_note(first + earlier + 1, &path));
            }
        }
        let mut step = Step::pending(first + offset, &item.member, &item.kind, now);
        step.parallel = Some(parallel);
        steps.push(step);
    }
    steps
}

impl App {
    /// `orbit.agent_assign {parallel}`: one task whose steps run in new agents,
    /// each in its own checkout. The steps start on the ledger tick.
    pub(in crate::app::orbit_notifications) fn assign_parallel_task(
        &mut self,
        id: &str,
        params: OrbitAgentAssignParams,
        requests: Vec<OrbitParallelStep>,
        now: u64,
    ) -> Result<Value, String> {
        let fail = |message: String| response_error(id.into(), message);
        if !params.name.trim().is_empty() || params.kind.is_some() {
            return Err(fail(
                "parallel replaces name and kind: its steps start their own agents; role only names the team role whose integrator integrates their patches".into(),
            ));
        }
        let timeout = assignment_timeout(params.timeout_ms).map_err(fail)?;
        self.orbit_notifications.check_room().map_err(fail)?;
        let prepared = self.prepare_parallel(&requests).map_err(fail)?;
        let params_role = params.role.clone();
        if let Some(role) = &params_role {
            if self.orbit_team.roster.role(role).is_none() {
                return Err(fail(format!(
                    "No team role named {role}; see orbit_team_list"
                )));
            }
        }
        let lead_id = self
            .resolve_orbit_lead()
            .map(|lead| lead.terminal_id)
            .ok_or_else(|| {
                fail("A unique Polaris lead is required in the Orbit workspace".into())
            })?;
        let task_id = self.orbit_notifications.next_task_id();
        let secret = token::generate().map_err(fail)?;
        let socket = crate::api::socket_path();
        let files = &mut self.orbit_notifications.token_files;
        files.issue(&task_id, &secret, &socket).map_err(fail)?;
        tracing::info!(%task_id, steps = prepared.len(), "orbit parallel task assigned");
        #[cfg(test)]
        {
            self.orbit_notifications.last_issued_token = Some(secret.clone());
        }
        let task = Task {
            task_id,
            worker_id: String::new(),
            lead_id,
            // Nothing is written to a worker for the task as a whole.
            submission: Submission::Submitted,
            delivery: Delivery::AwaitingReport,
            report: None,
            origin: None,
            notice_for: None,
            error: None,
            created_at: now,
            updated_at: now,
            title: shell_view::assignment_title(&params.prompt),
            attempts: 0,
            recoveries: 0,
            redelivered: false,
            nudged_at: None,
            deadline: Some(now + timeout),
            late: false,
            steps: parallel_steps(prepared, 0, &params.prompt, now),
            timeout_ms: Some(timeout),
            role: params_role,
            late_from: None,
            extra: Default::default(),
            token_hash: Some(token::hash(&secret)),
            live: LiveState::default(),
        };
        let role = task.role.clone();
        let task = self.orbit_notifications.insert(task);
        if let Some(role) = role {
            self.note_orbit_role_used(&role);
        }
        Ok(task)
    }

    /// A handoff that asks for parallel steps instead of one next member: the
    /// reporter's step ends handed off and the new steps wait to launch.
    pub(in crate::app::orbit_notifications) fn fan_out_handoff(
        &mut self,
        index: usize,
        (role, member, kind): (&str, &str, &str),
        handoff: &OrbitHandoff,
        requests: &[OrbitParallelStep],
        now: u64,
    ) -> Result<Value, String> {
        let prepared = self.prepare_parallel(requests)?;
        let task = &self.orbit_notifications.tasks[index];
        let first = task.steps.len().max(1);
        let brief = handoff_text(task.title.as_deref(), member, first + 1, handoff);
        let steps = parallel_steps(prepared, first, &brief, now);
        let task = self.orbit_notifications.task_mut(index);
        task.begin_steps(member, kind);
        task.end_step_with_handoff(handoff.clone(), now);
        task.steps.extend(steps);
        task.role.get_or_insert_with(|| role.to_owned());
        task.worker_id = String::new();
        task.nudged_at = None;
        task.error = None;
        task.deadline = Some(
            now + task
                .timeout_ms
                .unwrap_or(crate::app::orbit_notifications::assign::DEFAULT_TIMEOUT_MS),
        );
        task.updated_at = now;
        task.live = LiveState::default();
        Ok(json!(&self.orbit_notifications.tasks[index]))
    }
}
