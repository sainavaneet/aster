//! API handlers: assign, report, status, cancel.
use std::time::Instant;

use serde_json::{json, Value};

use super::state::{Delivery, LiveState, Submission, Task};
use super::{response_error, token, App};
use crate::api::schema::{OrbitAgentAssignParams, OrbitAgentPromptParams, OrbitTaskParams};

pub(super) const DEFAULT_TIMEOUT_MS: u64 = 2 * 60 * 60 * 1000;
pub(super) const MAX_TIMEOUT_MS: u64 = 24 * 60 * 60 * 1000;

fn reply(id: String, result: Result<Value, String>) -> String {
    match result {
        Ok(result) => json!({"id":id,"result":result}).to_string(),
        Err(error) => response_error(id, error),
    }
}

pub(super) fn assignment_timeout(timeout_ms: Option<u64>) -> Result<u64, String> {
    match timeout_ms {
        None => Ok(DEFAULT_TIMEOUT_MS),
        Some(ms) if (1..=MAX_TIMEOUT_MS).contains(&ms) => Ok(ms),
        Some(_) => Err(format!(
            "timeout_ms must be between 1 and {MAX_TIMEOUT_MS} (24 hours)"
        )),
    }
}

/// The executable workers report with: this server's own binary.
fn report_executable() -> Result<std::path::PathBuf, String> {
    std::env::current_exe().map_err(|error| error.to_string())
}

/// JSON data, not a shell fragment: worker tools construct argv in their native
/// shell. The command starts with the executable and carries no credentials:
/// `aster orbit agent-report` reads the task's token and session socket from its
/// token file, so a permission rule can match the command by prefix.
/// `step` is the step of a relayed task the report is for; it binds the report
/// to that step so a delayed one cannot act on a later step.
pub(super) fn report_command(
    task_id: &str,
    worker_id: &str,
    step: Option<usize>,
) -> Result<Value, String> {
    let mut command = json!({
        "executable": report_executable()?,
        "arguments": ["orbit", "agent-report"],
        "json_argument": {"task_id":task_id,"worker_id":worker_id,"outcome":"completed","summary":"REPLACE with your result summary","tests_run":false}
    });
    if let Some(step) = step {
        command["json_argument"]["step"] = json!(step);
    }
    Ok(command)
}

/// The Claude permission rule that pre-allows exactly the report command.
fn report_allow_rule(executable: &std::path::Path) -> String {
    format!("Bash({} orbit agent-report:*)", executable.display())
}

/// Launch arguments that let a Claude worker run its report command without a
/// permission prompt, in any permission mode; empty for other kinds. Never used
/// for the lead.
pub(crate) fn report_allow_args(agent: crate::detect::Agent) -> Vec<String> {
    if agent != crate::detect::Agent::Claude {
        return Vec::new();
    }
    match report_executable() {
        Ok(executable) => vec!["--allowedTools".into(), report_allow_rule(&executable)],
        Err(error) => {
            tracing::warn!(%error, "cannot pre-allow the orbit report command");
            Vec::new()
        }
    }
}

/// How to run the report command, shared by the assignment and the reminder.
pub(super) const REPORT_HOW: &str = "Run the executable with the listed arguments, followed by json_argument serialized as ONE argument (quote it for your shell), as one command exactly in that form: the executable path first, with no environment variable assignments, cd, pipes, or other commands around it, so it runs without a permission prompt. Aster supplies this task's report credentials itself; do not add a token.";

/// The optional structured report keys, shared by the assignment and the reminder.
pub(super) const REPORT_FIELDS_HINT: &str = "Set tests_run to true only if you ran the project's tests for this task, otherwise false. Optional keys: checks (list of {\"name\", \"status\": passed|failed|skipped, \"detail\"} for tests, lints, builds you ran), changed_files (relative paths you changed), branch (Git branch holding your changes), job_ids (Orbit job IDs you started), blocked_on (what you need to continue; use with outcome blocked).";

/// Explains the report `handoff` object to members of a role with other members.
pub(super) const HANDOFF_HINT: &str = "Your role has other members, so this task can be relayed. When your part is done but more work remains for a different member (for example a review of your change by another agent), add a handoff object to your report instead of finishing the task: done (what you finished, at most 1000 characters), next (what the next member should do, at most 1000), optional changed_files (paths so far, at most 200), known_issues (open problems, at most 500), and to (the name of an idle member of your role; otherwise Aster picks one). To split the remaining work across parallel agents instead, give parallel: a list of {member_or_kind, prompt, files} (files are the paths each part will change; parts that overlap run one after another), and each part runs in its own checkout. Aster then gives the task to that member, and Polaris receives one report when the last member reports without a handoff. Use a handoff only when more work remains; otherwise report normally.";

pub(super) fn assignment_instructions(task_id: &str, command: &Value, relay: bool) -> String {
    let handoff = if relay {
        format!(" {HANDOFF_HINT}")
    } else {
        String::new()
    };
    format!("\n\n[Aster assignment {task_id}]\nBefore ending this task, explicitly report back to Polaris using your shell tool. {REPORT_HOW} Replace summary with a concise result and outcome with completed, failed, or blocked. Pass task_id, worker_id and step (when listed) exactly as given. This reporting command is part of your task, including for read-only reviews. Do not merely print the command. Check that it succeeds; retry the identical report if needed. Do not guess another task or worker ID. {REPORT_FIELDS_HINT}{handoff}\n{command}\n")
}

impl App {
    pub(crate) fn handle_orbit_agent_assign(
        &mut self,
        id: String,
        params: OrbitAgentAssignParams,
    ) -> String {
        let now = self.orbit_notifications.now_ms(Instant::now());
        self.ensure_orbit_ledger(now);
        let response = match self.assign_orbit_task(&id, params, now) {
            Ok(task) => json!({"id":id,"result":task}).to_string(),
            Err(response) => response,
        };
        self.persist_orbit_ledger();
        response
    }

    fn assign_orbit_task(
        &mut self,
        id: &str,
        params: OrbitAgentAssignParams,
        now: u64,
    ) -> Result<Value, String> {
        let fail = |message: String| response_error(id.into(), message);
        let title = super::shell_view::assignment_title(&params.prompt);
        if params.prompt.trim().is_empty() {
            return Err(fail("Task prompt must not be empty".into()));
        }
        if let Some(requests) = params.parallel.clone() {
            return self.assign_parallel_task(id, params, requests, now);
        }
        let timeout = assignment_timeout(params.timeout_ms).map_err(fail)?;
        self.orbit_notifications.check_room().map_err(fail)?;
        // A role pipeline names the kind that starts a task.
        let kind = params.kind.or_else(|| {
            let role = params.role.as_deref()?;
            self.orbit_role_pipeline(role).into_iter().next()
        });
        let task_role = params.role.clone();
        let mut prompt = self.orbit_agent_prompt_params(
            id,
            OrbitAgentPromptParams {
                name: params.name,
                prompt: params.prompt,
                role: params.role,
                kind,
            },
            true,
        )?;
        let (worker_id, lead_id) = self.assignment_parties(&prompt.target).map_err(fail)?;
        self.refuse_double_booking(id, &worker_id)?;
        if let Some(line) = self.orbit_worktree_instruction(&worker_id) {
            prompt.text.push_str(&line);
        }
        if let Some(line) = self.orbit_team_role_instruction(&worker_id) {
            prompt.text.push_str(&line);
        }
        let task_id = self.orbit_notifications.next_task_id();
        let secret = token::generate().map_err(fail)?;
        let steps = self.initial_orbit_steps(&worker_id, now);
        let first_step = (!steps.is_empty()).then_some(0);
        let command = report_command(&task_id, &worker_id, first_step).map_err(fail)?;
        prompt.text.push_str(&assignment_instructions(
            &task_id,
            &command,
            !steps.is_empty(),
        ));
        let socket = crate::api::socket_path();
        let token_files = &mut self.orbit_notifications.token_files;
        token_files
            .issue(&task_id, &secret, &socket)
            .map_err(fail)?;
        let ack = match self.queue_orbit_prompt(id.into(), &prompt) {
            Ok(ack) => ack,
            Err(error) => {
                // The task is never recorded, so its token file goes too.
                let task = task_id.as_str();
                self.orbit_notifications
                    .token_files
                    .retire(|other| other != task);
                return Err(error.to_response(id.into()));
            }
        };
        tracing::info!(%task_id, %worker_id, "orbit task assigned");
        let token_hash = Some(token::hash(&secret));
        #[cfg(test)]
        {
            self.orbit_notifications.last_issued_token = Some(secret);
        }
        Ok(self.orbit_notifications.insert(Task {
            task_id,
            worker_id,
            lead_id,
            submission: Submission::Pending,
            delivery: Delivery::AwaitingReport,
            report: None,
            origin: None,
            notice_for: None,
            error: None,
            created_at: now,
            updated_at: now,
            title,
            attempts: 0,
            recoveries: 0,
            redelivered: false,
            nudged_at: None,
            deadline: Some(now + timeout),
            late: false,
            steps,
            timeout_ms: Some(timeout),
            role: task_role,
            late_from: None,
            extra: Default::default(),
            token_hash,
            live: LiveState {
                submission_ack: Some(ack),
                ..LiveState::default()
            },
        }))
    }

    /// One assignment per worker at a time, so two quick assignments (for example
    /// after two reuse starts returned the same worker) never prompt it twice.
    fn refuse_double_booking(&self, id: &str, worker_id: &str) -> Result<(), String> {
        let open = self.orbit_notifications.open_tasks_for_worker(worker_id);
        if open.is_empty() {
            return Ok(());
        }
        Err(json!({"id": id, "error": {
            "code": "orbit_worker_assigned",
            "message": format!(
                "Worker already has an assignment awaiting its report: {}; wait for the report, cancel it with orbit_task_cancel, or start another worker",
                open.join(", ")
            ),
        }})
        .to_string())
    }

    /// Captures the worker terminal; the lead resolves through its persisted binding
    /// first, so renaming Polaris never breaks assignment.
    fn assignment_parties(&mut self, target: &str) -> Result<(String, String), String> {
        let worker = self
            .resolve_agent_target(target)
            .map_err(|error| format!("Worker unavailable: {error:?}"))?;
        match self.resolve_orbit_lead() {
            Some(lead)
                if lead.ws_idx == worker.ws_idx && lead.terminal_id != worker.terminal_id =>
            {
                Ok((worker.terminal_id, lead.terminal_id))
            }
            _ => Err("A unique Polaris lead is required in the Orbit workspace".into()),
        }
    }

    pub(crate) fn handle_orbit_task_status(
        &mut self,
        id: String,
        params: OrbitTaskParams,
    ) -> String {
        let now = self.orbit_notifications.now_ms(Instant::now());
        self.ensure_orbit_ledger(now);
        self.orbit_notifications.reap_acks(now);
        self.persist_orbit_ledger();
        let notifications = &self.orbit_notifications;
        let step_line = notifications
            .find(&params.task_id)
            .and_then(|index| self.orbit_step_line(&notifications.tasks[index]));
        let result = notifications.status(&params.task_id).map(|mut value| {
            value["ledger_error"] = json!(notifications.ledger_error());
            if let Some(line) = step_line {
                value["task"]["step_line"] = json!(line);
            }
            value
        });
        reply(id, result)
    }

    pub(crate) fn handle_orbit_task_cancel(
        &mut self,
        id: String,
        params: OrbitTaskParams,
    ) -> String {
        let now = self.orbit_notifications.now_ms(Instant::now());
        self.ensure_orbit_ledger(now);
        let result = self.orbit_notifications.cancel(&params.task_id, now);
        self.persist_orbit_ledger();
        reply(id, result)
    }
}
