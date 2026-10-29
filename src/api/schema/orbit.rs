//! Orbit operations select a server-owned tab, never the caller's current pane.
use super::SplitDirection;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct OrbitRunParams {
    #[serde(default)]
    pub pane_id: Option<String>,
    pub command: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, schemars::JsonSchema, Default)]
#[serde(deny_unknown_fields)]
pub struct OrbitReadParams {
    #[serde(default)]
    pub pane_id: Option<String>,
    #[serde(default)]
    pub lines: Option<u32>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, schemars::JsonSchema, Default)]
#[serde(deny_unknown_fields)]
pub struct OrbitSplitParams {
    #[serde(default)]
    pub pane_id: Option<String>,
    #[serde(default)]
    pub direction: Option<SplitDirection>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct OrbitExecParams {
    pub command: String,
    #[serde(default)]
    pub timeout_ms: Option<u64>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct OrbitJobStatusParams {
    pub job_id: String,
    #[serde(default)]
    pub offset: Option<u64>,
    #[serde(default)]
    pub limit: Option<usize>,
}
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct OrbitJobCancelParams {
    pub job_id: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct OrbitAgentStartParams {
    /// Worker identity and visible pane title, applied automatically after launch admission.
    pub name: String,
    pub kind: String,
    #[serde(default)]
    pub pane_id: Option<String>,
    #[serde(default)]
    pub model: Option<String>,
    /// Return an idle Orbit worker of the same kind (and model, when requested)
    /// instead of starting another. Defaults to true.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reuse: Option<bool>,
    /// Start the worker in a new pane on a new Git worktree of the Orbit project:
    /// `true` generates a branch name, a string names the branch.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub worktree: Option<OrbitWorktreeRequest>,
    /// Claude `--permission-mode`: manual, acceptEdits, auto, plan, or dontAsk.
    /// bypassPermissions is always refused, and other kinds refuse the field.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub permission_mode: Option<String>,
}

/// `true` (generated branch), `false` (no worktree), or a branch name.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(untagged)]
pub enum OrbitWorktreeRequest {
    Enabled(bool),
    Branch(String),
}

impl OrbitAgentStartParams {
    /// The requested worktree branch: `Some(None)` generates one.
    pub fn worktree_branch(&self) -> Option<Option<&str>> {
        match self.worktree.as_ref()? {
            OrbitWorktreeRequest::Enabled(true) => Some(None),
            OrbitWorktreeRequest::Enabled(false) => None,
            // Tool callers often send the flag as text; "true" is never a branch name.
            OrbitWorktreeRequest::Branch(branch) => {
                match branch.trim().to_ascii_lowercase().as_str() {
                    "true" | "yes" | "on" => Some(None),
                    "" | "false" | "no" | "off" | "null" | "none" => None,
                    _ => Some(Some(branch.as_str())),
                }
            }
        }
    }
}

/// Stops an Orbit worker; the lead and panes outside Orbit are refused.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct OrbitAgentStopParams {
    pub name: String,
    /// Close the worker pane (default). Otherwise end the agent and keep the shell.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub close_pane: Option<bool>,
    /// Cancel the worker's assignments still awaiting its report instead of
    /// refusing. Reports the worker already made keep delivering.
    #[serde(default, skip_serializing_if = "super::is_false")]
    pub force: bool,
    /// Remove the worker's Orbit worktree after closing its pane.
    #[serde(default, skip_serializing_if = "super::is_false")]
    pub remove_worktree: bool,
    /// With `remove_worktree`, also remove a worktree with uncommitted or
    /// untracked changes, discarding them. `force` never does.
    #[serde(default, skip_serializing_if = "super::is_false")]
    pub discard_changes: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct OrbitPaneResizeParams {
    pub pane_id: String,
    pub direction: super::PaneDirection,
    /// Terminal cells (columns for left/right, rows for up/down), 1 to 200; default 10.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub amount: Option<u16>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct OrbitPaneZoomParams {
    pub pane_id: String,
    pub zoomed: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct OrbitPaneCloseParams {
    pub pane_id: String,
    /// Also close a pane hosting a live worker, cancelling its assignments still
    /// awaiting a report. Reports the worker already made keep delivering.
    #[serde(default, skip_serializing_if = "super::is_false")]
    pub force: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct OrbitAgentPromptParams {
    /// Worker terminal_id, name, or pane ID; empty when `role` picks the worker.
    #[serde(default)]
    pub name: String,
    pub prompt: String,
    /// Team role whose idle member receives the prompt, instead of `name`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub role: Option<String>,
    /// With `role`, prefer an idle member of this kind.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub kind: Option<String>,
}
/// Tracked assignment: the worker must report back with orbit.agent_report.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct OrbitAgentAssignParams {
    /// Worker terminal_id, name, or pane ID; empty when `role` picks the worker.
    #[serde(default)]
    pub name: String,
    pub prompt: String,
    /// Report deadline in milliseconds; defaults to 2 hours, at most 24 hours.
    #[serde(default)]
    pub timeout_ms: Option<u64>,
    /// Team role whose idle, unassigned member takes the task, instead of `name`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub role: Option<String>,
    /// With `role`, prefer an idle member of this kind.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub kind: Option<String>,
    /// Run the task as parallel steps, each by a new agent in its own checkout
    /// of the project (instead of `name` or `role`); Polaris gets one report
    /// when all have finished.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parallel: Option<Vec<OrbitParallelStep>>,
}
/// Identifies an assignment issued by orbit.agent_assign.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct OrbitTaskParams {
    pub task_id: String,
}
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct OrbitAgentStatusParams {
    /// Worker terminal_id, name, or pane ID; empty when `role` is given.
    #[serde(default)]
    pub name: String,
    /// Team role: the status of every member of the role.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub role: Option<String>,
}
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, schemars::JsonSchema, Default)]
#[serde(deny_unknown_fields)]
pub struct OrbitDiffParams {
    #[serde(default)]
    pub path: Option<String>,
}

/// Explicit result for an assignment issued by orbit.agent_assign.
#[derive(Clone, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct OrbitAgentReportParams {
    pub task_id: String,
    /// Stable worker terminal identity supplied in the assignment.
    pub worker_id: String,
    /// Per-assignment secret supplied in the assignment. Optional on the wire for
    /// compatibility, but required for every assignment issued with a token.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub token: Option<String>,
    pub outcome: OrbitOutcome,
    pub summary: String,
    /// Whether the worker ran the project's tests for this task.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tests_run: Option<bool>,
    /// Checks the worker ran (tests, lints, builds), at most 20.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub checks: Option<Vec<OrbitReportCheck>>,
    /// Relative paths of files the worker changed, at most 200.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub changed_files: Option<Vec<String>>,
    /// Git branch holding the worker's changes.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub branch: Option<String>,
    /// Orbit job IDs the worker started, at most 20.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub job_ids: Option<Vec<String>>,
    /// What the worker needs to continue; expected with outcome `blocked`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub blocked_on: Option<String>,
    /// Relays the task to another member of the worker's role instead of
    /// finishing it: this report ends the worker's step, not the task.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub handoff: Option<OrbitHandoff>,
    /// The step of a relayed task this report belongs to, as given in the
    /// assignment; a report for a step already handed off is stale.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub step: Option<usize>,
}

/// The note one step of a relayed task leaves for the next member.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct OrbitHandoff {
    /// What this step finished, at most 1000 characters.
    pub done: String,
    /// What the next member should do, at most 1000 characters.
    pub next: String,
    /// Relative paths changed so far, at most 200.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub changed_files: Vec<String>,
    /// Open problems the next member should know, at most 500 characters.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub known_issues: Option<String>,
    /// Name of an idle member of the same role to take over; otherwise the
    /// role's pipeline, or a member of another kind, is chosen.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub to: Option<String>,
    /// Instead of one next member, run these steps in parallel, each in its own
    /// checkout; the task finishes when all of them have reported.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parallel: Option<Vec<OrbitParallelStep>>,
}

/// One step of a parallel fan-out, run by a new agent in its own checkout of the
/// Orbit project.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct OrbitParallelStep {
    /// A team member's name (its kind, model, and permission mode are used) or
    /// an agent kind such as `claude` or `codex`.
    pub member_or_kind: String,
    /// Model for the step's agent, when `member_or_kind` is a kind (a member's
    /// own model is used otherwise).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    /// What this step should do.
    pub prompt: String,
    /// Paths (files or directories, relative to the project) this step will
    /// change. Steps whose paths overlap run one after another.
    pub files: Vec<String>,
}

impl OrbitAgentReportParams {
    /// A report without a token or structured fields.
    pub fn new(task_id: String, worker_id: String, outcome: OrbitOutcome, summary: String) -> Self {
        Self {
            task_id,
            worker_id,
            token: None,
            outcome,
            summary,
            tests_run: None,
            checks: None,
            changed_files: None,
            branch: None,
            job_ids: None,
            blocked_on: None,
            handoff: None,
            step: None,
        }
    }

    /// Whether any optional structured field is present.
    pub fn has_facts(&self) -> bool {
        self.tests_run.is_some()
            || self.checks.is_some()
            || self.changed_files.is_some()
            || self.branch.is_some()
            || self.job_ids.is_some()
            || self.blocked_on.is_some()
    }
}

/// One check a worker ran, as reported in `checks`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct OrbitReportCheck {
    pub name: String,
    pub status: OrbitCheckStatus,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum OrbitCheckStatus {
    Passed,
    Failed,
    Skipped,
}

impl OrbitCheckStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Passed => "passed",
            Self::Failed => "failed",
            Self::Skipped => "skipped",
        }
    }
}

/// Redacts the token so request or report debugging never prints it.
impl std::fmt::Debug for OrbitAgentReportParams {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("OrbitAgentReportParams")
            .field("task_id", &self.task_id)
            .field("worker_id", &self.worker_id)
            .field("token", &self.token.as_ref().map(|_| "<redacted>"))
            .field("outcome", &self.outcome)
            .field("summary", &self.summary)
            .field("tests_run", &self.tests_run)
            .field("checks", &self.checks)
            .field("changed_files", &self.changed_files)
            .field("branch", &self.branch)
            .field("job_ids", &self.job_ids)
            .field("blocked_on", &self.blocked_on)
            .field("handoff", &self.handoff)
            .field("step", &self.step)
            .finish()
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum OrbitOutcome {
    Completed,
    Failed,
    Blocked,
}

/// Answers a blocked Orbit worker's approval prompt for Polaris. Allowed only
/// with `[polaris.approvals] lead_may_approve = true`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct OrbitAgentApprovalParams {
    /// Worker terminal_id, name, or pane ID inside the bound Orbit tab.
    pub name: String,
}
