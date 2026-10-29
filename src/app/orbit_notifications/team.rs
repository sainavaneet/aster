//! Notices for the Orbit team supervisor: a human notification through the
//! usual `orbit.notice` path, and a server notice delivered to Polaris like the
//! notices for lost or blocked workers.
use std::time::Instant;

use super::lifecycle::worker_notice;
use super::App;
use crate::config::OrbitNotifyEvent;

/// Prefix of the notice keys the team records; the notice text is the detail.
pub(super) const TEAM_KEY_PREFIX: &str = "team:";

impl App {
    /// One human notification per `key`: `subject` is a role or member name.
    pub(crate) fn record_orbit_team_notice(
        &mut self,
        key: &str,
        subject: &str,
        detail: &'static str,
    ) {
        self.orbit_notifications.notifier.record(
            OrbitNotifyEvent::Notice,
            &format!("{TEAM_KEY_PREFIX}{key}"),
            subject,
            detail,
        );
    }

    /// A standalone server notice to the bound lead about `worker_id`.
    pub(crate) fn send_orbit_team_lead_notice(
        &mut self,
        notice_id: String,
        worker_id: &str,
        summary: String,
    ) {
        let now = self.orbit_notifications.now_ms(Instant::now());
        self.ensure_orbit_ledger(now);
        let Some(lead_id) = self.orbit_notice_lead_id() else {
            tracing::warn!(notice_id, "orbit team notice has no lead to tell");
            return;
        };
        let notice_id = format!("{notice_id}-{now}/server-notice");
        self.orbit_notifications
            .insert_notice(worker_notice(notice_id, worker_id, lead_id, summary, now));
        self.persist_orbit_ledger();
    }
}
