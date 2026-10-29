//! Explicit worker results and acknowledged, idle-only delivery to the bound lead.
//! The bounded ledger persists in the session data directory; acknowledgement
//! channels are runtime-only and are reconciled after a restart.
use std::collections::{HashMap, VecDeque};
use std::path::PathBuf;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use serde_json::{json, Value};

use super::orbit_files::OrbitFile;
use super::App;

mod approvals;
mod assign;
mod deadlines;
mod deliver;
mod facts;
mod gate;
mod lead;
mod ledger;
mod lifecycle;
mod notify;
mod pump;
mod redact;
mod report;
mod shell_view;
mod state;
mod store;
mod team;
#[cfg(test)]
mod tests;
mod token;

pub(crate) use approvals::{
    answer_keys, permission_prompt_rules, redact_for_decisions, Decision, Waiter, ANSWER_WAIT,
};
pub(crate) use assign::report_allow_args;
pub(crate) use facts::checks_summary;
pub(crate) use notify::HumanNotice;
pub(crate) use redact::redact_secrets;
pub(crate) use state::Task;
use state::Tombstone;
pub(crate) use token::{read_token_file, token_dir, TokenFile};

const CAPACITY: usize = 64;
const TOMBSTONE_CAPACITY: usize = 256;
/// After a failed save, the earliest retry; the ledger stays dirty until then.
const SAVE_RETRY_MS: u64 = 1_000;
/// Wall-clock lead over the monotonic clock treated as a sleep to catch up with.
const CLOCK_JUMP_MS: u64 = 1_000;

/// The single place Orbit ledger time comes from: unix milliseconds that never go
/// backwards. They advance with the given `Instant`, so tests can drive time, and
/// catch up with the wall clock when it runs ahead by more than `CLOCK_JUMP_MS`
/// (a monotonic clock that paused during system sleep). `skew_ms` records that
/// catch-up so `instant` stays the exact inverse and gate windows compare client
/// input on the same clock. Smaller drift is ignored so timestamps keep following
/// the `Instant` they were derived from.
#[derive(Debug, Clone, Copy)]
struct Clock {
    base: Instant,
    base_ms: u64,
    skew_ms: u64,
    last_ms: u64,
}

fn wall_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

impl Clock {
    fn new(base: Instant, base_ms: u64) -> Self {
        Self {
            base,
            base_ms,
            skew_ms: 0,
            last_ms: base_ms,
        }
    }

    /// `max(wall after a jump, base + elapsed + skew, last returned)`.
    fn now_ms(&mut self, now: Instant, wall_ms: u64) -> u64 {
        let elapsed = now.saturating_duration_since(self.base).as_millis() as u64;
        let mut monotonic = self.base_ms + elapsed + self.skew_ms;
        if wall_ms > monotonic + CLOCK_JUMP_MS {
            self.skew_ms += wall_ms - monotonic;
            monotonic = wall_ms;
        }
        self.last_ms = self.last_ms.max(monotonic);
        self.last_ms
    }

    /// The monotonic instant for a ledger millisecond timestamp.
    fn instant(self, ms: u64) -> Instant {
        self.base + Duration::from_millis(ms.saturating_sub(self.base_ms + self.skew_ms))
    }
}

#[derive(Default)]
pub(crate) struct Notifications {
    tasks: Vec<Task>,
    tombstones: VecDeque<Tombstone>,
    /// Persisted lead binding; resolved before the `polaris` name.
    lead_terminal_id: Option<String>,
    sequence: u64,
    clock: Option<Clock>,
    next_tick: u64,
    /// Per-lead earliest next delivery, keyed by lead terminal id.
    delivery_after: HashMap<String, u64>,
    /// When each terminal the ledger may prompt was first seen continuously idle.
    idle_since: HashMap<String, u64>,
    /// When each watched terminal was last observed `Working`; proof that an
    /// uncertain delivery to that lead was submitted.
    working_at: HashMap<String, u64>,
    /// In-flight delivery prompts, each with one PTY ACK shared by its members.
    batches: HashMap<u64, deliver::Batch>,
    next_batch: u64,
    /// Recent report calls per worker terminal for the sliding-window rate limit.
    report_calls: HashMap<String, VecDeque<u64>>,
    #[cfg(test)]
    last_issued_token: Option<String>,
    /// Per-task report token files; see `token`.
    token_files: token::TokenFiles,
    lifecycle_after: u64,
    /// Continuous `Blocked` episodes per agent terminal, for relays and activity.
    blocked: HashMap<String, lifecycle::BlockedEpisode>,
    blocked_next_tick: u64,
    notifier: notify::Notifier,
    approvals: approvals::Approvals,
    loaded: bool,
    store_path: Option<PathBuf>,
    load_error: Option<String>,
    save_error: Option<String>,
    save_retry_at: u64,
    dirty: bool,
    /// Server notices dropped or refused because the ledger was full.
    dropped_notices: u64,
    /// Entries skipped while loading because they could not be read.
    load_warning: Option<String>,
    /// `[polaris.delivery] lead_absent_timeout_ms`; see `deadlines`.
    lead_absent_fail_ms: Option<u64>,
    /// The Orbit sidebar projection published to client shells; see `shell_view`.
    shell_view: shell_view::ShellViewCache,
}

pub(super) fn response_error(id: String, message: impl ToString) -> String {
    json!({"id":id,"error":{"code":"orbit_notification_error","message":message.to_string()}})
        .to_string()
}

/// The ledger warning for entries skipped on load; they are kept in a side file.
fn skipped_warning(path: &std::path::Path, skipped: &store::Skipped, now_ms: u64) -> String {
    let count = skipped.len();
    match store::keep_skipped(path, skipped, now_ms) {
        Ok(kept) => format!(
            "Warning: {count} unreadable Orbit ledger entries were skipped on load (kept at {kept}); the rest loaded"
        ),
        Err(error) => {
            tracing::warn!(%error, "cannot keep skipped Orbit ledger entries");
            format!("Warning: {count} unreadable Orbit ledger entries were skipped on load; the rest loaded")
        }
    }
}

impl Notifications {
    /// Records a ledger write: a failure keeps the ledger dirty and retries
    /// after `SAVE_RETRY_MS`.
    fn note_ledger_saved(&mut self, path: &std::path::Path, result: Result<(), String>) {
        match result {
            Ok(()) => self.save_error = None,
            Err(error) => {
                tracing::warn!(%error, path = %path.display(), "cannot save Orbit task ledger");
                self.save_error = Some(format!("Cannot save Orbit task ledger: {error}"));
                self.dirty = true;
                let now = self.clock.map_or(0, |clock| clock.last_ms);
                self.save_retry_at = now + SAVE_RETRY_MS;
            }
        }
    }

    /// The ledger clock at `now` without advancing it: what `now_ms` would
    /// return, for read-only projections. The wall clock before the first task.
    fn peek_now_ms(&self, now: Instant) -> u64 {
        let wall = wall_ms();
        self.clock.map_or(wall, |clock| {
            let mut clock = clock;
            clock.now_ms(now, wall)
        })
    }

    fn now_ms(&mut self, now: Instant) -> u64 {
        let wall = wall_ms();
        self.clock
            .get_or_insert_with(|| Clock::new(now, wall))
            .now_ms(now, wall)
    }

    /// Nothing can change without a new call: every task is resolved, and no
    /// assignment or delivery acknowledgement is outstanding.
    fn is_quiescent(&self) -> bool {
        self.batches.is_empty()
            && self
                .tasks
                .iter()
                .all(|task| task.delivery.is_terminal() && task.live.submission_ack.is_none())
    }

    fn instant_at(&self, ms: u64) -> Option<Instant> {
        self.clock.map(|clock| clock.instant(ms))
    }

    /// The persisted lead binding, if any. Production code asks
    /// `App::orbit_lead_terminal`, which also checks the terminal exists.
    #[cfg(test)]
    pub(crate) fn lead_terminal_id(&self) -> Option<&str> {
        self.lead_terminal_id.as_deref()
    }

    /// Removes the token files of tasks the ledger can no longer authenticate a
    /// report for: neither a task nor a tombstone holds them any more. Resolved
    /// tasks keep theirs, so a late worker still learns the task was cancelled
    /// or resolved, and identical retries (also against a tombstone) still
    /// authenticate.
    fn retire_token_files(&mut self) {
        let (tasks, tombstones) = (&self.tasks, &self.tombstones);
        self.token_files.retire(|task_id| {
            tasks.iter().any(|task| task.task_id == task_id)
                || tombstones.iter().any(|entry| entry.task_id == task_id)
        });
    }

    /// Load and save failures plus warnings (entries skipped on load, notices
    /// dropped while full), joined; `None` when the ledger is healthy.
    fn ledger_error(&self) -> Option<String> {
        let dropped = (self.dropped_notices > 0).then(|| {
            format!(
                "Warning: {} Orbit server notices were dropped because the ledger was full ({CAPACITY})",
                self.dropped_notices
            )
        });
        let parts: Vec<&str> = [
            &self.load_error,
            &self.save_error,
            &self.load_warning,
            &dropped,
        ]
        .into_iter()
        .filter_map(|part| part.as_deref())
        .collect();
        (!parts.is_empty()).then(|| parts.join("; "))
    }
}

impl App {
    /// Applies `[polaris.notify]` and `[polaris.approvals]`; called at startup and on reload.
    pub(crate) fn configure_orbit_notifications(&mut self, config: &crate::config::PolarisConfig) {
        let notifications = &mut self.orbit_notifications;
        notifications.notifier.set_events(&config.notify.events);
        notifications.approvals.lead_may_approve = config.approvals.lead_may_approve;
        notifications.set_lead_absent_fail_ms(config.delivery.lead_absent_timeout_ms);
    }

    /// Loads the persisted ledger once and reconciles it with live terminals.
    fn ensure_orbit_ledger(&mut self, now_ms: u64) {
        let notifications = &mut self.orbit_notifications;
        if notifications.loaded {
            return;
        }
        notifications.loaded = true;
        notifications.lifecycle_after = now_ms + lifecycle::STARTUP_GRACE_MS;
        // Unit tests choose a token directory explicitly; see `tests`.
        #[cfg(not(test))]
        notifications
            .token_files
            .open(token::token_dir(&crate::session::data_dir()));
        if notifications.store_path.is_none() && self.policy.persist_session {
            notifications.store_path = Some(crate::session::data_dir().join(store::FILE_NAME));
        }
        let Some(path) = notifications.store_path.clone() else {
            return;
        };
        match store::load(&path, now_ms) {
            Ok(Some(loaded)) => {
                notifications.lead_terminal_id = loaded.lead_terminal_id;
                notifications.tasks = loaded.tasks;
                notifications.tombstones = loaded.tombstones;
                if loaded.skipped.len() > 0 {
                    notifications.load_warning =
                        Some(skipped_warning(&path, &loaded.skipped, now_ms));
                }
                self.reconcile_loaded_orbit_ledger(now_ms);
                self.orbit_notifications.retire_token_files();
            }
            Ok(None) => {}
            Err(store::LoadError::Reset(error)) => {
                tracing::warn!(%error, "Orbit task ledger reset");
                notifications.load_error = Some(error);
            }
            Err(store::LoadError::Unreadable(error)) => {
                // The original may still be valid: never overwrite it.
                tracing::warn!(%error, path = %path.display(), "Orbit task ledger saving disabled");
                notifications.store_path = None;
                notifications.load_error = Some(error);
            }
        }
    }

    fn persist_orbit_ledger(&mut self) {
        self.persist_orbit_ledger_now(false);
    }

    /// Serializes a dirty ledger and hands it to the Orbit writer; `force`
    /// ignores the retry delay after a failed save (shutdown and handoff).
    fn persist_orbit_ledger_now(&mut self, force: bool) {
        let notifications = &mut self.orbit_notifications;
        let now = notifications.clock.map_or(0, |clock| clock.last_ms);
        if !notifications.dirty || (!force && now < notifications.save_retry_at) {
            return;
        }
        notifications.dirty = false;
        notifications.retire_token_files();
        let Some(path) = notifications.store_path.clone() else {
            return;
        };
        let snapshot = store::Snapshot {
            lead_terminal_id: notifications.lead_terminal_id.as_deref(),
            tasks: &notifications.tasks,
            tombstones: &notifications.tombstones,
        };
        let result = match store::serialize(&snapshot) {
            Ok(bytes) => self
                .orbit_writer
                .replace(OrbitFile::Ledger, path.clone(), bytes),
            Err(error) => Some(Err(error)),
        };
        if let Some(result) = result {
            let result = result.map_err(|error| error.to_string());
            self.orbit_notifications.note_ledger_saved(&path, result);
        }
    }

    /// Saves pending Orbit state and waits for the writer: server shutdown and
    /// live handoff, after which another process may read the files.
    pub(crate) fn flush_orbit_files(&mut self) {
        self.persist_orbit_ledger_now(true);
        self.orbit_writer.flush();
    }

    /// Applies the result of a background Orbit state file write.
    pub(crate) fn finish_orbit_file_save(
        &mut self,
        saved: crate::app::orbit_files::OrbitFileSaved,
    ) {
        match saved.file {
            OrbitFile::Ledger => self
                .orbit_notifications
                .note_ledger_saved(&saved.path, saved.result),
            OrbitFile::Roster => self.note_orbit_team_saved(saved.result),
        }
    }

    /// The token of the most recent assignment, for tests that report as the worker.
    #[cfg(test)]
    pub(crate) fn orbit_last_issued_token(&self) -> Option<String> {
        self.orbit_notifications.last_issued_token.clone()
    }

    /// Assignment records, lead delivery status, and ledger health for `orbit.activity`.
    pub(crate) fn orbit_task_activity(&mut self) -> Value {
        let now = self.orbit_notifications.now_ms(Instant::now());
        self.ensure_orbit_ledger(now);
        let lead = self.orbit_lead_activity();
        let blocked_workers = self.orbit_blocked_workers();
        self.persist_orbit_ledger();
        let notifications = &self.orbit_notifications;
        json!({
            "tasks": notifications.tasks,
            "ledger_error": notifications.ledger_error(),
            "lead": lead,
            "lead_error": self.polaris_error,
            "blocked_workers": blocked_workers,
        })
    }
}
