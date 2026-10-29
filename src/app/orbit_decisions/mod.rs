//! The typed decision layer behind Orbit: rules first, then an optional
//! provider (`[polaris.decisions]`), for worker approvals, report sanity,
//! routing suggestions, and delivery timing. Decisions run on their own threads
//! and come back as `AppEvent::OrbitDecisionReady`; the event loop never waits
//! on one. Every decision is journaled to `orbit-decisions.jsonl`.
use std::collections::VecDeque;
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde_json::{json, Value};

use super::App;
use crate::config::{DecisionProviderKind, PolarisDecisionsConfig};
use crate::events::AppEvent;

mod api;
pub(crate) mod approval;
pub(crate) mod interrupt;
mod jev;
pub(crate) mod journal;
pub(crate) mod provider;
pub(crate) mod route;
pub(crate) mod rules;
pub(crate) mod sanity;

use journal::{Entry, Journal};
use provider::{DecisionProvider, RulesOnly};

/// Assignment prompts remembered for approval and report-sanity context.
const MAX_PROMPTS: usize = 64;
/// Time a held delivery waits past the provider timeout for a decision.
pub(crate) const HOLD_GRACE_MS: u64 = 2_000;

/// A decision's result, applied on the event loop.
#[derive(Debug)]
pub(crate) enum Outcome {
    Approval(approval::ApprovalVerdict),
    Sanity(sanity::SanityVerdict),
    Interrupt(interrupt::InterruptVerdict),
}

/// Runtime state of the decision layer; nothing here is persisted except the
/// journal file, and nothing holds key material.
pub(crate) struct Decisions {
    config: Arc<PolarisDecisionsConfig>,
    provider: Arc<dyn DecisionProvider>,
    jev: Option<Arc<jev::Jev>>,
    journal: Arc<Journal>,
    next_id: u64,
    prompts: VecDeque<(String, String)>,
    pub(crate) interrupt: interrupt::Gate,
    #[cfg(test)]
    test_provider: Option<Arc<dyn DecisionProvider>>,
}

impl Default for Decisions {
    fn default() -> Self {
        Self {
            config: Arc::default(),
            provider: Arc::new(RulesOnly),
            jev: None,
            journal: Arc::default(),
            next_id: 0,
            prompts: VecDeque::new(),
            interrupt: interrupt::Gate::default(),
            #[cfg(test)]
            test_provider: None,
        }
    }
}

pub(crate) fn wall_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

fn jev_settings(config: &PolarisDecisionsConfig) -> jev::JevSettings {
    jev::JevSettings {
        endpoint: config.endpoint.clone(),
        model: config.model.clone(),
        api_key_env: config.api_key_env.clone(),
        api_key_file: config
            .api_key_file
            .as_deref()
            .map(crate::worktree::expand_tilde_path),
        timeout: Duration::from_millis(config.timeout_ms),
    }
}

impl Decisions {
    pub(crate) fn config(&self) -> &PolarisDecisionsConfig {
        &self.config
    }

    pub(crate) fn provider_name(&self) -> &'static str {
        self.provider.name()
    }

    /// Whether a provider beyond the rules answers questions.
    pub(crate) fn has_provider(&self) -> bool {
        self.provider.name() != "rules"
    }

    /// How long a delivery may wait for a decision before going ahead.
    pub(crate) fn hold_ms(&self) -> u64 {
        self.config.timeout_ms + HOLD_GRACE_MS
    }

    pub(crate) fn next_id(&mut self) -> u64 {
        self.next_id += 1;
        self.next_id
    }

    /// The prompt of assignment `task_id`, when this server run issued it.
    pub(crate) fn assignment_prompt(&self, task_id: &str) -> Option<&str> {
        self.prompts
            .iter()
            .rev()
            .find(|(id, _)| id == task_id)
            .map(|(_, prompt)| prompt.as_str())
    }

    #[cfg(test)]
    pub(crate) fn set_test_provider(&mut self, provider: Option<Arc<dyn DecisionProvider>>) {
        self.test_provider = provider;
        let config = Arc::clone(&self.config);
        self.apply(&config, None);
    }

    fn apply(&mut self, config: &PolarisDecisionsConfig, journal_path: Option<std::path::PathBuf>) {
        self.config = Arc::new(config.clone());
        self.jev = Some(Arc::new(jev::Jev {
            settings: jev_settings(config),
        }));
        self.provider = match config.provider {
            DecisionProviderKind::Rules => Arc::new(RulesOnly),
            DecisionProviderKind::Jev => self.jev.clone().expect("set above"),
        };
        #[cfg(test)]
        if let Some(provider) = &self.test_provider {
            self.provider = Arc::clone(provider);
        }
        if journal_path.is_some() && self.journal.path() != journal_path.as_deref() {
            self.journal = Arc::new(Journal::new(journal_path));
        }
        self.interrupt.reset();
    }
}

impl App {
    /// Applies `[polaris.decisions]`; called at startup and on config reload.
    pub(crate) fn configure_orbit_decisions(&mut self, config: &PolarisDecisionsConfig) {
        let path = self
            .policy
            .persist_session
            .then(|| crate::session::data_dir().join(journal::FILE_NAME));
        let fresh = path.is_some() && self.orbit_decisions.journal.path() != path.as_deref();
        self.orbit_decisions.apply(config, path);
        if fresh {
            let journal = Arc::clone(&self.orbit_decisions.journal);
            if let Err(error) =
                crate::thread_spawn::spawn_named("aster-orbit-decision-journal", move || {
                    journal.load_recent()
                })
            {
                tracing::warn!(%error, "cannot load the Orbit decision journal");
            }
        }
    }

    /// Remembers the prompt of an assignment the response accepted.
    pub(crate) fn note_orbit_assignment_prompt(&mut self, prompt: &str, response: &str) {
        let Ok(value) = serde_json::from_str::<Value>(response) else {
            return;
        };
        let Some(task_id) = value["result"]["task_id"].as_str() else {
            return;
        };
        let prompt: String = prompt.chars().take(approval::TASK_PROMPT_CHARS).collect();
        let prompts = &mut self.orbit_decisions.prompts;
        prompts.push_back((task_id.to_owned(), prompt));
        while prompts.len() > MAX_PROMPTS {
            prompts.pop_front();
        }
    }

    /// Runs `job` on a decision thread, journals its entry, and sends the
    /// outcome back to the event loop. `Err` returns `job` unrun-able: the
    /// caller applies its fallback.
    pub(crate) fn spawn_orbit_decision<F>(&mut self, job: F) -> std::io::Result<()>
    where
        F: FnOnce(&PolarisDecisionsConfig, &dyn DecisionProvider) -> (Outcome, Entry)
            + Send
            + 'static,
    {
        let config = Arc::clone(&self.orbit_decisions.config);
        let provider = Arc::clone(&self.orbit_decisions.provider);
        let journal = Arc::clone(&self.orbit_decisions.journal);
        let event_tx = self.event_tx.clone();
        crate::thread_spawn::spawn_named("aster-orbit-decision", move || {
            let (outcome, entry) = job(&config, provider.as_ref());
            tracing::info!(
                kind = %entry.kind,
                provider = %entry.provider,
                action = %entry.action,
                latency_ms = entry.latency_ms,
                failed = entry.error.is_some(),
                "orbit decision"
            );
            journal.append(entry);
            let _ = event_tx.blocking_send(AppEvent::OrbitDecisionReady(Box::new(outcome)));
        })
        .map(|_| ())
    }

    /// Applies a finished decision.
    pub(crate) fn finish_orbit_decision(&mut self, outcome: Outcome) {
        match outcome {
            Outcome::Approval(verdict) => self.apply_orbit_approval_decision(verdict),
            Outcome::Sanity(verdict) => self.apply_orbit_sanity_decision(verdict),
            Outcome::Interrupt(verdict) => {
                let now = self.orbit_ledger_now_ms();
                self.orbit_decisions.interrupt.answer(verdict, now);
            }
        }
    }

    /// The newest journal entries, oldest first, for `orbit.activity`.
    pub(crate) fn orbit_decision_activity(&self) -> Value {
        json!(self.orbit_decisions.journal.recent())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn configuration_selects_the_provider_and_keeps_no_key() {
        let mut decisions = Decisions::default();
        assert_eq!(decisions.provider_name(), "rules");
        let mut config = PolarisDecisionsConfig {
            provider: DecisionProviderKind::Jev,
            api_key_file: Some("~/typesafe.key".into()),
            ..PolarisDecisionsConfig::default()
        };
        decisions.apply(&config, None);
        assert_eq!(decisions.provider_name(), "jev");
        assert!(decisions.has_provider());
        let settings = &decisions.jev.as_ref().unwrap().settings;
        assert!(!settings
            .api_key_file
            .as_ref()
            .unwrap()
            .to_string_lossy()
            .starts_with('~'));
        assert_eq!(decisions.hold_ms(), 3_000 + HOLD_GRACE_MS);
        // Reload back to rules.
        config.provider = DecisionProviderKind::Rules;
        decisions.apply(&config, None);
        assert_eq!(decisions.provider_name(), "rules");
        assert!(!decisions.has_provider());
    }

    #[test]
    fn assignment_prompts_are_bounded_and_found_by_task() {
        let mut decisions = Decisions::default();
        for index in 0..(MAX_PROMPTS + 3) {
            decisions
                .prompts
                .push_back((format!("task-{index}"), "p".into()));
            while decisions.prompts.len() > MAX_PROMPTS {
                decisions.prompts.pop_front();
            }
        }
        assert_eq!(decisions.assignment_prompt("task-0"), None);
        assert_eq!(decisions.assignment_prompt("task-66"), Some("p"));
    }
}
