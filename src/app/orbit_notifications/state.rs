//! Typed assignment and delivery states. Every change goes through `transition()`,
//! which enforces the legal-edge table and traces the edge with its reason.
use std::fmt;
use std::sync::mpsc::Receiver;

use serde::{Deserialize, Deserializer, Serialize};
use serde_json::{Map, Value};

pub(crate) use super::token::TokenHash;
use crate::api::schema::OrbitAgentReportParams;

pub(super) type Ack = Receiver<std::io::Result<()>>;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct IllegalTransition {
    pub(super) task_id: String,
    pub(super) from: &'static str,
    pub(super) to: &'static str,
}

impl fmt::Display for IllegalTransition {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "illegal Orbit task transition {} -> {} for {}",
            self.from, self.to, self.task_id
        )
    }
}

/// Whether the worker's PTY accepted the assignment text and Enter.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum Submission {
    Pending,
    Submitted,
    Uncertain,
    /// A matching worker report proves the assignment arrived.
    ConfirmedByReport,
}

impl Submission {
    #[cfg(test)]
    pub(super) const ALL: [Self; 4] = [
        Self::Pending,
        Self::Submitted,
        Self::Uncertain,
        Self::ConfirmedByReport,
    ];

    pub(super) fn as_str(self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::Submitted => "submitted",
            Self::Uncertain => "uncertain",
            Self::ConfirmedByReport => "confirmed_by_report",
        }
    }

    pub(super) fn can_transition(self, to: Self) -> bool {
        use Submission::*;
        matches!(
            (self, to),
            (Pending, Submitted | Uncertain | ConfirmedByReport) | (Uncertain, ConfirmedByReport)
        )
    }

    pub(super) fn transition(
        self,
        to: Self,
        task_id: &str,
        reason: &str,
    ) -> Result<Self, IllegalTransition> {
        checked_edge(
            self.can_transition(to),
            task_id,
            "submission",
            (self.as_str(), to.as_str()),
            reason,
        )
        .map(|()| to)
    }
}

/// Where the task's result stands relative to the lead.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum Delivery {
    AwaitingReport,
    Queued,
    Submitting,
    Submitted,
    Uncertain,
    Failed,
    Cancelled,
    WorkerLost,
    Unreported,
    TimedOut,
}

impl Delivery {
    #[cfg(test)]
    pub(super) const ALL: [Self; 10] = [
        Self::AwaitingReport,
        Self::Queued,
        Self::Submitting,
        Self::Submitted,
        Self::Uncertain,
        Self::Failed,
        Self::Cancelled,
        Self::WorkerLost,
        Self::Unreported,
        Self::TimedOut,
    ];

    pub(super) fn as_str(self) -> &'static str {
        match self {
            Self::AwaitingReport => "awaiting_report",
            Self::Queued => "queued",
            Self::Submitting => "submitting",
            Self::Submitted => "submitted",
            Self::Uncertain => "uncertain",
            Self::Failed => "failed",
            Self::Cancelled => "cancelled",
            Self::WorkerLost => "worker_lost",
            Self::Unreported => "unreported",
            Self::TimedOut => "timed_out",
        }
    }

    pub(super) fn is_terminal(self) -> bool {
        matches!(
            self,
            Self::Submitted
                | Self::Failed
                | Self::Cancelled
                | Self::WorkerLost
                | Self::Unreported
                | Self::TimedOut
        )
    }

    pub(super) fn can_transition(self, to: Self) -> bool {
        use Delivery::*;
        matches!(
            (self, to),
            (
                AwaitingReport,
                Queued | Cancelled | WorkerLost | Unreported | TimedOut
            ) | (Queued, Submitting | Failed | Cancelled)
                | (Submitting, Submitted | Uncertain | Cancelled)
                | (Uncertain, Submitted | Queued | Failed | Cancelled)
                // A late worker report for an assignment Aster resolved itself.
                | (WorkerLost | Unreported | TimedOut, Queued)
        )
    }

    /// Resolved by Aster on the worker's behalf; a valid late report is still
    /// accepted and delivered.
    pub(super) fn accepts_late_report(self) -> bool {
        matches!(self, Self::WorkerLost | Self::Unreported | Self::TimedOut)
    }

    pub(super) fn transition(
        self,
        to: Self,
        task_id: &str,
        reason: &str,
    ) -> Result<Self, IllegalTransition> {
        checked_edge(
            self.can_transition(to),
            task_id,
            "delivery",
            (self.as_str(), to.as_str()),
            reason,
        )
        .map(|()| to)
    }
}

fn checked_edge(
    legal: bool,
    task_id: &str,
    machine: &'static str,
    (from, to): (&'static str, &'static str),
    reason: &str,
) -> Result<(), IllegalTransition> {
    if !legal {
        return Err(IllegalTransition {
            task_id: task_id.into(),
            from,
            to,
        });
    }
    tracing::info!(task_id, machine, from, to, reason, "orbit task transition");
    Ok(())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum ReportOrigin {
    /// Submitted by the assigned worker through orbit.agent_report.
    Worker,
    /// Synthesised by Aster when the worker was lost, timed out, or never reported.
    Server,
}

/// Runtime-only bookkeeping. Acknowledgement receivers cannot survive a restart.
#[derive(Debug, Default)]
pub(super) struct LiveState {
    pub(super) submission_ack: Option<Ack>,
    /// Delivery batch whose shared PTY acknowledgement this task awaits.
    pub(super) batch: Option<u64>,
    pub(super) retry_at: u64,
    pub(super) idle_since: Option<u64>,
    pub(super) uncertain_since: Option<u64>,
    /// Since when this queued report has waited for an absent lead.
    pub(super) lead_absent_since: Option<u64>,
    /// The newest `[Pasted text #N` on the lead screen before this delivery
    /// was written (0 when none); unknown after a restart.
    pub(super) pasted_before: Option<u32>,
    /// Whether delivery waits for a decision (`[polaris.decisions]`).
    pub(super) decision: DecisionMark,
    /// The `Decision: …` line appended to the delivered entry.
    pub(super) decision_note: Option<String>,
    /// Report sanity answers, shown on the task in `orbit.activity`.
    pub(super) sanity: Option<serde_json::Value>,
}

/// Where a queued task stands with the decision layer.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub(super) enum DecisionMark {
    /// Not looked at yet.
    #[default]
    Unchecked,
    /// Delivery waits for a decision until this ledger time.
    Pending { until: u64 },
    /// Decided, or no decision applies.
    Done,
}

/// Persisted and exposed assignment record. Optional fields default so new fields
/// stay additive for existing ledger files.
#[derive(Debug, Serialize, Deserialize)]
pub(crate) struct Task {
    pub(super) task_id: String,
    pub(super) worker_id: String,
    pub(super) lead_id: String,
    pub(super) submission: Submission,
    pub(super) delivery: Delivery,
    #[serde(default, deserialize_with = "lenient_report")]
    pub(super) report: Option<OrbitAgentReportParams>,
    #[serde(default)]
    pub(super) origin: Option<ReportOrigin>,
    /// Set on server notices: the assignment this notice resolves.
    #[serde(default)]
    pub(super) notice_for: Option<String>,
    pub(super) error: Option<String>,
    #[serde(default)]
    pub(super) created_at: u64,
    #[serde(default)]
    pub(super) updated_at: u64,
    /// First line of the assignment, redacted and shortened; for display only.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(super) title: Option<String>,
    /// Lead delivery attempts: prompts queued to the lead PTY plus refusals that
    /// are not a wait for the lead to host its agent (`agent_not_ready`/`_found`).
    #[serde(default)]
    pub(super) attempts: u32,
    /// Uncertain deliveries already requeued after screen verification.
    #[serde(default)]
    pub(super) recoveries: u32,
    /// Requeued without proof the earlier delivery failed; the lead is told the
    /// report may be a duplicate.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub(super) redelivered: bool,
    #[serde(default)]
    pub(super) nudged_at: Option<u64>,
    /// Unix ms after which an unreported assignment times out.
    #[serde(default)]
    pub(super) deadline: Option<u64>,
    /// The report timeout the assignment was given, so each relayed step gets
    /// the full timeout from its own start.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(super) timeout_ms: Option<u64>,
    /// The team role the task was given to (or relayed within), which picks the
    /// integrator of its parallel steps.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(super) role: Option<String>,
    /// The worker reported after Aster had resolved the assignment itself.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub(super) late: bool,
    /// The state Aster had resolved a late-reported assignment as.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(super) late_from: Option<Delivery>,
    /// Ordered steps of a task relayed across the members of a role; empty for
    /// a task one worker handles.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub(super) steps: Vec<super::steps::Step>,
    /// Absent for legacy tasks and server notices, which accept tokenless reports.
    /// Written to the ledger file by `store`; never exposed through the API.
    #[serde(default, skip_serializing)]
    pub(super) token_hash: Option<TokenHash>,
    /// Fields from a newer build, kept so they survive this build's saves.
    #[serde(flatten)]
    pub(super) extra: Map<String, Value>,
    #[serde(skip)]
    pub(super) live: LiveState,
}

impl Task {
    /// An assignment (not a server notice) still waiting for the worker's report.
    pub(super) fn awaits_worker(&self) -> bool {
        self.delivery == Delivery::AwaitingReport && self.notice_for.is_none()
    }

    /// A server notice still waiting in the queue, not being delivered.
    pub(super) fn is_waiting_notice(&self) -> bool {
        self.origin == Some(ReportOrigin::Server)
            && self.delivery == Delivery::Queued
            && self.live.batch.is_none()
    }

    pub(super) fn set_delivery(&mut self, to: Delivery, now_ms: u64, reason: &str) -> bool {
        match self.delivery.transition(to, &self.task_id, reason) {
            Ok(next) => {
                self.delivery = next;
                self.updated_at = now_ms;
                true
            }
            Err(error) => {
                tracing::warn!(%error, reason, "rejected orbit task transition");
                false
            }
        }
    }

    pub(super) fn set_submission(&mut self, to: Submission, now_ms: u64, reason: &str) -> bool {
        match self.submission.transition(to, &self.task_id, reason) {
            Ok(next) => {
                self.submission = next;
                self.updated_at = now_ms;
                true
            }
            Err(error) => {
                tracing::warn!(%error, reason, "rejected orbit task transition");
                false
            }
        }
    }

    /// A late worker report for the evicted assignment `tombstone`, queued for
    /// delivery to `lead_id`.
    pub(super) fn late_report(
        tombstone: &Tombstone,
        lead_id: String,
        report: OrbitAgentReportParams,
        now_ms: u64,
    ) -> Self {
        Self {
            task_id: tombstone.task_id.clone(),
            worker_id: tombstone.worker_id.clone(),
            lead_id,
            submission: Submission::ConfirmedByReport,
            delivery: Delivery::Queued,
            report: Some(report),
            origin: Some(ReportOrigin::Worker),
            notice_for: None,
            error: None,
            created_at: now_ms,
            updated_at: now_ms,
            title: None,
            attempts: 0,
            recoveries: 0,
            redelivered: false,
            nudged_at: None,
            deadline: None,
            late: true,
            steps: Vec::new(),
            timeout_ms: None,
            role: None,
            late_from: Some(tombstone.final_delivery),
            token_hash: tombstone.token_hash.clone(),
            extra: Map::new(),
            live: LiveState::default(),
        }
    }

    /// Unique per delivery attempt, so an earlier attempt or the lead's own assignment
    /// output cannot be mistaken for this delivery on the lead's screen.
    pub(super) fn delivery_marker(&self) -> String {
        self.delivery_marker_for(self.attempts)
    }

    pub(super) fn delivery_marker_for(&self, attempt: u32) -> String {
        format!(
            "{} {} #{attempt}]",
            super::report::DELIVERY_MARKER_PREFIX,
            self.task_id
        )
    }
}

/// Report keys this build reads; others, from a newer build, are dropped.
const REPORT_KEYS: [&str; 13] = [
    "task_id",
    "worker_id",
    "token",
    "outcome",
    "summary",
    "tests_run",
    "checks",
    "changed_files",
    "branch",
    "job_ids",
    "blocked_on",
    "handoff",
    "step",
];
/// Structured facts dropped when only they keep a stored report from loading.
const REPORT_FACT_KEYS: [&str; 7] = [
    "tests_run",
    "checks",
    "changed_files",
    "branch",
    "job_ids",
    "blocked_on",
    "handoff",
];

/// A stored report, tolerating a newer build's additions: unknown keys are
/// dropped, then facts this build cannot read, so the outcome and summary
/// still load. Only an unreadable outcome or summary fails the record.
fn lenient_report<'de, D: Deserializer<'de>>(
    deserializer: D,
) -> Result<Option<OrbitAgentReportParams>, D::Error> {
    use serde::de::Error;
    let Some(value) = Option::<Value>::deserialize(deserializer)? else {
        return Ok(None);
    };
    if let Ok(report) = OrbitAgentReportParams::deserialize(&value) {
        return Ok(Some(report));
    }
    let Value::Object(mut fields) = value else {
        return Err(D::Error::custom("report is not an object"));
    };
    fields.retain(|key, _| REPORT_KEYS.contains(&key.as_str()));
    let known = Value::Object(fields.clone());
    if let Ok(report) = OrbitAgentReportParams::deserialize(&known) {
        return Ok(Some(report));
    }
    fields.retain(|key, _| !REPORT_FACT_KEYS.contains(&key.as_str()));
    OrbitAgentReportParams::deserialize(&Value::Object(fields))
        .map(Some)
        .map_err(D::Error::custom)
}

/// Evicted resolved assignment, kept so late identical retries stay idempotent.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub(crate) struct Tombstone {
    pub(super) task_id: String,
    pub(super) worker_id: String,
    #[serde(default, deserialize_with = "lenient_report")]
    pub(super) report: Option<OrbitAgentReportParams>,
    pub(super) final_delivery: Delivery,
    pub(super) resolved_at: u64,
    #[serde(default, skip_serializing)]
    pub(super) token_hash: Option<TokenHash>,
    /// Fields from a newer build, kept so they survive this build's saves.
    #[serde(flatten)]
    pub(super) extra: Map<String, Value>,
}

impl Tombstone {
    pub(super) fn from_task(task: &Task) -> Self {
        Self {
            task_id: task.task_id.clone(),
            worker_id: task.worker_id.clone(),
            report: task.report.clone(),
            final_delivery: task.delivery,
            resolved_at: task.updated_at,
            token_hash: task.token_hash.clone(),
            extra: Map::new(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use Delivery::*;

    const LEGAL_DELIVERY: &[(Delivery, Delivery)] = &[
        (AwaitingReport, Queued),
        (AwaitingReport, Cancelled),
        (AwaitingReport, WorkerLost),
        (AwaitingReport, Unreported),
        (AwaitingReport, TimedOut),
        (Queued, Submitting),
        (Queued, Failed),
        (Queued, Cancelled),
        (Submitting, Submitted),
        (Submitting, Uncertain),
        (Submitting, Cancelled),
        (Uncertain, Submitted),
        (Uncertain, Queued),
        (Uncertain, Failed),
        (Uncertain, Cancelled),
        (WorkerLost, Queued),
        (Unreported, Queued),
        (TimedOut, Queued),
    ];

    const LEGAL_SUBMISSION: &[(Submission, Submission)] = &[
        (Submission::Pending, Submission::Submitted),
        (Submission::Pending, Submission::Uncertain),
        (Submission::Pending, Submission::ConfirmedByReport),
        (Submission::Uncertain, Submission::ConfirmedByReport),
    ];

    #[test]
    fn delivery_table_accepts_exactly_the_listed_edges() {
        for from in Delivery::ALL {
            for to in Delivery::ALL {
                let expected = LEGAL_DELIVERY.contains(&(from, to));
                assert_eq!(
                    from.transition(to, "task", "test").is_ok(),
                    expected,
                    "{from:?} -> {to:?}"
                );
            }
        }
    }

    #[test]
    fn every_delivery_state_rejects_at_least_one_edge_and_terminals_reject_all() {
        for from in Delivery::ALL {
            let rejected = Delivery::ALL
                .into_iter()
                .filter(|to| from.transition(*to, "task", "test").is_err())
                .count();
            assert!(rejected > 0, "{from:?} accepts everything");
            if from.accepts_late_report() {
                // Only a late worker report reopens it, for delivery.
                assert_eq!(rejected, Delivery::ALL.len() - 1, "{from:?}");
                assert!(from.transition(Queued, "task", "test").is_ok());
            } else if from.is_terminal() {
                assert_eq!(rejected, Delivery::ALL.len(), "{from:?} is terminal");
            }
        }
        let error = Submitted.transition(Queued, "task-1", "test").unwrap_err();
        assert_eq!((error.from, error.to), ("submitted", "queued"));
    }

    #[test]
    fn submission_table_accepts_exactly_the_listed_edges() {
        for from in Submission::ALL {
            let mut rejected = 0;
            for to in Submission::ALL {
                let expected = LEGAL_SUBMISSION.contains(&(from, to));
                let actual = from.transition(to, "task", "test").is_ok();
                assert_eq!(actual, expected, "{from:?} -> {to:?}");
                rejected += usize::from(!actual);
            }
            assert!(rejected > 0, "{from:?} accepts everything");
        }
    }

    #[test]
    fn states_keep_existing_wire_strings() {
        for state in Delivery::ALL {
            assert_eq!(serde_json::to_value(state).unwrap(), state.as_str());
        }
        for state in Submission::ALL {
            assert_eq!(serde_json::to_value(state).unwrap(), state.as_str());
        }
    }
}
