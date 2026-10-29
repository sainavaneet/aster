//! Bounded waits for deliveries, so every report ends delivered or visibly
//! failed: a queued report waits at most `lead_absent_fail_ms` for an absent
//! lead (one whose pane does not run its agent), and an uncertain delivery is
//! verified within `UNCERTAIN_RESOLVE_MS`. Both clocks are runtime-only and
//! restart with the server.
use super::state::Delivery;
use super::{App, Notifications};

/// Default bound on a queued report while the lead is absent or not hosting
/// its agent (`[polaris.delivery] lead_absent_timeout_ms`).
pub(crate) const LEAD_ABSENT_FAIL_MS: u64 = 30 * 60 * 1000;
/// Smallest configurable `lead_absent_timeout_ms`.
const MIN_LEAD_ABSENT_FAIL_MS: u64 = 1_000;
/// An uncertain delivery whose lead never became idle (so its screen could not
/// be checked) is treated as unverifiable after this long.
pub(crate) const UNCERTAIN_RESOLVE_MS: u64 = 10 * 60 * 1000;

impl Notifications {
    pub(super) fn lead_absent_fail_ms(&self) -> u64 {
        self.lead_absent_fail_ms.unwrap_or(LEAD_ABSENT_FAIL_MS)
    }

    pub(super) fn set_lead_absent_fail_ms(&mut self, ms: u64) {
        self.lead_absent_fail_ms = Some(ms.max(MIN_LEAD_ABSENT_FAIL_MS));
    }

    /// Starts or clears each queued report's absence clock and fails those
    /// that waited `lead_absent_fail_ms` for an absent lead.
    fn expire_absent_lead_deliveries(&mut self, lead_present: bool, now: u64) {
        let limit = self.lead_absent_fail_ms();
        for index in 0..self.tasks.len() {
            let task = &mut self.tasks[index];
            if task.delivery != Delivery::Queued {
                continue;
            }
            if lead_present {
                task.live.lead_absent_since = None;
                continue;
            }
            let since = *task.live.lead_absent_since.get_or_insert(now);
            if now.saturating_sub(since) < limit {
                continue;
            }
            let task = self.task_mut(index);
            task.error = Some(format!(
                "Polaris was absent or not running its agent for {}; the report was not delivered",
                duration_text(limit)
            ));
            task.set_delivery(
                Delivery::Failed,
                now,
                "lead absent past the delivery deadline",
            );
            self.note_delivery(index);
        }
    }
}

/// `30 minutes`, `90 seconds`.
fn duration_text(ms: u64) -> String {
    if ms >= 60_000 && ms.is_multiple_of(60_000) {
        format!("{} minutes", ms / 60_000)
    } else {
        format!("{} seconds", ms.div_ceil(1_000))
    }
}

impl App {
    /// Whether `lead_id` exists where the ledger captured it, has a runtime,
    /// and runs (or is launching) a known agent.
    fn orbit_lead_hosts_agent(&self, lead_id: &str) -> bool {
        if !self.orbit_terminal_present(lead_id) {
            return false;
        }
        let Some((key, terminal)) = self.state.terminals.get_key_value(lead_id) else {
            return false;
        };
        self.terminal_runtimes.get(key).is_some()
            && (terminal.effective_known_agent().is_some()
                || terminal.managed_agent_launch_pending())
    }

    /// Runs each delivery tick before the lead gate.
    pub(super) fn expire_orbit_deliveries(&mut self, lead_id: Option<&str>, now: u64) {
        let present = lead_id.is_some_and(|id| self.orbit_lead_hosts_agent(id));
        self.orbit_notifications
            .expire_absent_lead_deliveries(present, now);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn durations_read_naturally() {
        assert_eq!(duration_text(LEAD_ABSENT_FAIL_MS), "30 minutes");
        assert_eq!(duration_text(90_000), "90 seconds");
        assert_eq!(duration_text(1_500), "2 seconds");
    }
}
