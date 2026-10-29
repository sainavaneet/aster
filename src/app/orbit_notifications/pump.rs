//! Tick-driven, non-blocking delivery of reports to the lead.
use std::sync::mpsc::TryRecvError;
use std::time::Instant;

use super::deadlines::UNCERTAIN_RESOLVE_MS;
use super::state::{Ack, Delivery, Submission, Task};
use super::{App, Notifications};
use crate::api::schema::{ReadFormat, ReadSource};

const TICK_MS: u64 = 250;
pub(super) const MAX_ATTEMPTS: u32 = 5;
const BACKOFF_BASE_MS: u64 = 3_000;
const BACKOFF_CAP_MS: u64 = 60_000;
/// Let an uncertain write finish rendering before inspecting the lead's screen.
pub(super) const VERIFY_DELAY_MS: u64 = 2_000;
const VERIFY_LINES: u32 = 200;

/// 3s, 6s, 12s, … capped at 60s for the given 1-based attempt.
pub(super) fn backoff_ms(attempt: u32) -> u64 {
    let doublings = attempt.saturating_sub(1).min(16);
    (BACKOFF_BASE_MS << doublings).min(BACKOFF_CAP_MS)
}

/// A finished ACK; a dropped sender counts as a failed write.
pub(super) fn try_ack(ack: &Ack) -> Option<std::io::Result<()>> {
    match ack.try_recv() {
        Ok(result) => Some(result),
        Err(TryRecvError::Empty) => None,
        Err(TryRecvError::Disconnected) => Some(Err(std::io::Error::other(
            "PTY acknowledgement channel closed",
        ))),
    }
}

fn poll(ack: &mut Option<Ack>) -> Option<std::io::Result<()>> {
    let result = try_ack(ack.as_ref()?)?;
    *ack = None;
    Some(result)
}

fn reap_submission(task: &mut Task, now: u64) -> bool {
    let Some(result) = poll(&mut task.live.submission_ack) else {
        return false;
    };
    // A report may already have confirmed the assignment; the ACK is then moot.
    if task.submission != Submission::Pending {
        return true;
    }
    match result {
        Ok(()) => task.set_submission(
            Submission::Submitted,
            now,
            "assignment PTY write acknowledged",
        ),
        Err(error) => {
            task.error = Some(error.to_string());
            task.set_submission(Submission::Uncertain, now, "assignment PTY write failed")
        }
    };
    true
}

impl Notifications {
    pub(super) fn reap_acks(&mut self, now: u64) {
        let mut changed = false;
        for task in &mut self.tasks {
            changed |= reap_submission(task, now);
        }
        self.dirty |= changed;
        self.reap_batches(now);
    }
}

impl App {
    pub(crate) fn pump_orbit_notifications_at(&mut self, instant: Instant) {
        self.poll_orbit_answers(instant);
        let now = self.orbit_notifications.now_ms(instant);
        self.ensure_orbit_ledger(now);
        self.observe_orbit_blocked(now);
        let notifications = &mut self.orbit_notifications;
        if notifications.is_quiescent() || now < notifications.next_tick {
            self.flush_orbit_notices();
            self.persist_orbit_ledger();
            return;
        }
        notifications.next_tick = now + TICK_MS;
        notifications.reap_acks(now);
        self.observe_orbit_idle(now);
        self.check_orbit_workers(now);
        self.recover_uncertain_deliveries(now);
        self.deliver_orbit_reports(now);
        self.flush_orbit_notices();
        self.persist_orbit_ledger();
    }

    fn lead_screen_text(&self, lead_id: &str) -> Option<String> {
        let target = self.resolve_terminal_target(lead_id).ok()?;
        let (runtime, _) = self.lookup_runtime(target.ws_idx, target.pane_id)?;
        let snapshot = crate::app::api_helpers::read_terminal_snapshot(
            runtime,
            ReadSource::RecentUnwrapped,
            ReadFormat::Text,
            Some(VERIFY_LINES),
        );
        Some(snapshot.text)
    }

    /// The newest collapsed-paste number on the lead screen; 0 when none.
    pub(super) fn lead_pasted_number(&self, lead_id: &str) -> Option<u32> {
        Some(newest_pasted_number(&self.lead_screen_text(lead_id)?))
    }

    /// Whether the lead screen shows `marker`, or a collapsed paste newer than
    /// the one recorded before the delivery was written.
    fn lead_screen_shows_delivery(
        &self,
        lead_id: &str,
        marker: &str,
        pasted_before: Option<u32>,
    ) -> Option<Verdict> {
        let text = self.lead_screen_text(lead_id)?;
        Some(if text.contains(marker) {
            Verdict::Delivered("delivery marker visible on lead screen")
        } else if pasted_before.is_some_and(|before| newest_pasted_number(&text) > before) {
            Verdict::Delivered("collapsed paste of the delivery visible on lead screen")
        } else {
            Verdict::Missing("Delivery stayed uncertain twice; not resent")
        })
    }

    /// Never blindly resend. The lead starting work after the uncertain write
    /// proves submission. Otherwise only a marker missing from an idle lead's
    /// screen, a lead terminal that is gone, or a lead that stayed unverifiable
    /// (never idle) for `UNCERTAIN_RESOLVE_MS` requeues the report, only once,
    /// and the requeued report is marked as a possible duplicate.
    fn recover_uncertain_deliveries(&mut self, now: u64) {
        for index in 0..self.orbit_notifications.tasks.len() {
            let task = &self.orbit_notifications.tasks[index];
            if task.delivery != Delivery::Uncertain || task.live.batch.is_some() {
                continue;
            }
            let since = task.live.uncertain_since.unwrap_or(now);
            let worked = self
                .orbit_notifications
                .working_at
                .get(&task.lead_id)
                .is_some_and(|at| *at >= since);
            let verdict = if worked {
                Verdict::Delivered("lead started working after the uncertain write")
            } else if !self.orbit_terminal_present(&task.lead_id) {
                Verdict::Missing("lead terminal gone; delivery unverifiable")
            } else if now >= since + UNCERTAIN_RESOLVE_MS {
                Verdict::Missing(
                    "Delivery could not be verified: Polaris was not idle for 10 minutes; not resent",
                )
            } else if now < since + VERIFY_DELAY_MS
                || self.idle_orbit_target(&task.lead_id, now).is_none()
            {
                self.orbit_notifications.tasks[index].live.uncertain_since = Some(since);
                continue;
            } else {
                let shown = self.lead_screen_shows_delivery(
                    &task.lead_id,
                    &task.delivery_marker(),
                    task.live.pasted_before,
                );
                match shown {
                    None => continue,
                    Some(verdict) => verdict,
                }
            };
            apply_verdict(self.orbit_notifications.task_mut(index), verdict, now);
            self.orbit_notifications.note_delivery(index);
        }
    }
}

/// Claude Code's placeholder for a collapsed paste: `[Pasted text #3 +40 lines]`.
const PASTED_TEXT: &str = "[Pasted text #";

/// The highest `N` of any `[Pasted text #N` in `text`; 0 when none.
fn newest_pasted_number(text: &str) -> u32 {
    text.match_indices(PASTED_TEXT)
        .filter_map(|(at, _)| {
            let digits: String = text[at + PASTED_TEXT.len()..]
                .chars()
                .take_while(char::is_ascii_digit)
                .collect();
            digits.parse().ok()
        })
        .max()
        .unwrap_or(0)
}

enum Verdict {
    Delivered(&'static str),
    /// Not proven delivered; the reason is the error if it cannot be requeued.
    Missing(&'static str),
}

fn apply_verdict(task: &mut Task, verdict: Verdict, now: u64) {
    match verdict {
        Verdict::Delivered(reason) => {
            task.set_delivery(Delivery::Submitted, now, reason);
        }
        Verdict::Missing(_) if task.recoveries == 0 => {
            task.recoveries += 1;
            task.redelivered = true;
            task.live.uncertain_since = None;
            task.set_delivery(
                Delivery::Queued,
                now,
                "delivery not verified; requeued once",
            );
        }
        Verdict::Missing(reason) => {
            task.error = Some(reason.into());
            task.set_delivery(Delivery::Failed, now, "second unverified delivery");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pasted_text_numbers_are_read_from_the_screen() {
        assert_eq!(newest_pasted_number("nothing here"), 0);
        assert_eq!(
            newest_pasted_number("> [Pasted text #2 +40 lines]\n> [Pasted text #11 +3 lines]"),
            11
        );
        assert_eq!(newest_pasted_number("[Pasted text #x]"), 0);
    }

    #[test]
    fn backoff_doubles_from_three_seconds_and_caps_at_one_minute() {
        let delays: Vec<_> = (1..=7).map(backoff_ms).collect();
        assert_eq!(
            delays,
            [3_000, 6_000, 12_000, 24_000, 48_000, 60_000, 60_000]
        );
    }
}
