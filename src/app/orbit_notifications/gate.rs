//! When Orbit may type into an agent's composer. Reads only scalar terminal facts.
use std::collections::HashSet;
use std::time::Duration;

use super::approvals::redact_for_decisions;
use super::state::Delivery;
use super::App;
use crate::api::schema::{OrbitOutcome, ReadFormat, ReadSource};
use crate::app::orbit_decisions::interrupt::{self, InterruptInput, Pass};
use crate::app::orbit_decisions::{wall_ms, Outcome};
use crate::detect::AgentState;

/// Client keystrokes or paste on the target within this window defer delivery.
pub(super) const USER_INPUT_QUIET_MS: u64 = 2_000;
/// The quiet window while the client left text unsent in the composer (typed or
/// pasted without Enter, Ctrl-C, Ctrl-U, or Escape). Delivery stays live: a user
/// who pauses longer than this mid-draft can still have a report typed into it,
/// which is the accepted residual risk.
pub(super) const DRAFT_QUIET_MS: u64 = 60_000;
/// Continuous `Idle` that substitutes for a visible idle prompt, for detection
/// sources that never report one (hook authority, unknown manifests).
pub(super) const IDLE_FALLBACK_MS: u64 = 10_000;

impl App {
    /// Tracks continuous idleness for every terminal the ledger may prompt: the
    /// bound lead, leads of unresolved tasks, and workers awaiting a report.
    pub(super) fn observe_orbit_idle(&mut self, now: u64) {
        let notifications = &self.orbit_notifications;
        let mut watched: HashSet<&str> = notifications
            .tasks
            .iter()
            .filter(|task| !task.delivery.is_terminal())
            .flat_map(|task| {
                let worker =
                    (task.delivery == Delivery::AwaitingReport).then_some(task.worker_id.as_str());
                // Agents of parallel steps waiting for their prompt.
                let starting = task.steps.iter().filter_map(|step| {
                    let parallel = step.parallel.as_ref()?;
                    (parallel.phase == super::parallel::Phase::Starting)
                        .then_some(parallel.worker_id.as_deref()?)
                });
                // The integrator waiting to be prompted.
                let integrator = task.steps.iter().filter_map(|step| {
                    let integrate = step.integrate.as_ref()?;
                    (integrate.phase == super::parallel::IntegratePhase::Ready)
                        .then_some(integrate.worker_id.as_deref()?)
                });
                std::iter::once(task.lead_id.as_str())
                    .chain(worker)
                    .chain(starting)
                    .chain(integrator)
            })
            .collect();
        watched.extend(notifications.lead_terminal_id.as_deref());
        let observed: Vec<(String, Option<AgentState>)> = watched
            .into_iter()
            .map(|id| {
                let state = self.state.terminals.get(id).map(|terminal| terminal.state);
                (id.to_owned(), state)
            })
            .collect();
        let notifications = &mut self.orbit_notifications;
        let since = &mut notifications.idle_since;
        since.retain(|id, _| {
            observed
                .iter()
                .any(|(watched, state)| watched == id && *state == Some(AgentState::Idle))
        });
        let working = &mut notifications.working_at;
        working.retain(|id, _| observed.iter().any(|(watched, _)| watched == id));
        for (id, state) in observed {
            match state {
                Some(AgentState::Idle) => {
                    since.entry(id).or_insert(now);
                }
                Some(AgentState::Working) => {
                    working.insert(id, now);
                }
                _ => {}
            }
        }
    }

    /// Whether the terminal still exists where the ledger captured it.
    pub(super) fn orbit_terminal_present(&self, terminal_id: &str) -> bool {
        self.state.terminals.contains_key(terminal_id)
            && self
                .resolve_terminal_target(terminal_id)
                .is_ok_and(|target| target.terminal_id == terminal_id)
    }

    /// The target's public pane when it is the captured terminal, idle, hosting a
    /// known agent, not launching, free of recent client input (a longer window
    /// while a draft is unsent), and either visibly at its idle prompt or
    /// continuously idle for the fallback period. Anything else is a wait.
    pub(super) fn idle_orbit_target(&self, terminal_id: &str, now: u64) -> Option<String> {
        let target = self.resolve_terminal_target(terminal_id).ok()?;
        if target.terminal_id != terminal_id {
            return None;
        }
        let terminal = self.state.terminals.get(terminal_id)?;
        if terminal.state != AgentState::Idle
            || terminal.effective_known_agent().is_none()
            || terminal.managed_agent_launch_pending()
        {
            return None;
        }
        let notifications = &self.orbit_notifications;
        let instant = notifications.instant_at(now)?;
        let quiet_ms = if terminal.draft_pending() {
            DRAFT_QUIET_MS
        } else {
            USER_INPUT_QUIET_MS
        };
        if terminal.user_input_within(Duration::from_millis(quiet_ms), instant) {
            return None;
        }
        let settled = notifications
            .idle_since
            .get(terminal_id)
            .is_some_and(|since| now.saturating_sub(*since) >= IDLE_FALLBACK_MS);
        if !terminal.visible_idle() && !settled {
            return None;
        }
        self.public_pane_id(target.ws_idx, target.pane_id)
    }
}

/// Lead screen lines read for an interruption decision.
const INTERRUPT_SCREEN_LINES: u32 = 20;

impl App {
    /// The ledger clock now, for decisions applied outside a tick.
    pub(crate) fn orbit_ledger_now_ms(&mut self) -> u64 {
        self.orbit_notifications.now_ms(std::time::Instant::now())
    }

    /// After the gate above allowed a delivery to the idle lead: whether the
    /// interruption decision lets it go now. It can only defer (by
    /// `interrupt::DEFER_MS`, then it asks again), never deliver earlier, and a
    /// failed or slow decision lets the delivery proceed.
    pub(super) fn orbit_interruption_allows(&mut self, lead_id: &str, now: u64) -> bool {
        let decisions = &self.orbit_decisions;
        if !decisions.config().interruption.enabled || !decisions.has_provider() {
            return true;
        }
        let wait_ms = decisions.hold_ms();
        match self.orbit_decisions.interrupt.pass(now, wait_ms) {
            Pass::Proceed => true,
            Pass::Wait => false,
            Pass::Ask => {
                let input = self.orbit_interrupt_input(lead_id, now);
                let id = self.orbit_decisions.next_id();
                let spawned = self.spawn_orbit_decision(move |config, provider| {
                    let (verdict, entry) =
                        interrupt::decide(id, input, config, provider, wall_ms());
                    (Outcome::Interrupt(verdict), entry)
                });
                match spawned {
                    Ok(()) => {
                        self.orbit_decisions.interrupt.asked(id, now);
                        false
                    }
                    Err(error) => {
                        tracing::warn!(%error, "cannot start an Orbit interruption decision");
                        true
                    }
                }
            }
        }
    }

    fn orbit_interrupt_input(&self, lead_id: &str, now: u64) -> InterruptInput {
        let tasks = &self.orbit_notifications.tasks;
        let ready = tasks
            .iter()
            .filter(|task| task.delivery == Delivery::Queued && now >= task.live.retry_at)
            .filter_map(|task| task.report.as_ref());
        let mut outcomes = Vec::new();
        let mut blocked = false;
        for report in ready {
            blocked |= report.outcome == OrbitOutcome::Blocked;
            outcomes.push(
                serde_json::to_value(&report.outcome)
                    .ok()
                    .and_then(|value| value.as_str().map(str::to_owned))
                    .unwrap_or_default(),
            );
        }
        let tail = self
            .resolve_terminal_target(lead_id)
            .ok()
            .and_then(|target| self.lookup_runtime(target.ws_idx, target.pane_id))
            .map(|(runtime, _)| {
                crate::app::api_helpers::read_terminal_snapshot(
                    runtime,
                    ReadSource::RecentUnwrapped,
                    ReadFormat::Text,
                    Some(INTERRUPT_SCREEN_LINES),
                )
                .text
            })
            .unwrap_or_default();
        let tail = redact_for_decisions(tail.trim_end());
        let skip = tail
            .chars()
            .count()
            .saturating_sub(interrupt::SCREEN_TAIL_CHARS);
        InterruptInput {
            lead_state: "idle",
            screen_tail: tail.chars().skip(skip).collect(),
            outcomes,
            blocked,
        }
    }
}
