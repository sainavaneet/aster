//! Delivery timing. Only after the existing gate already allows typing into an
//! idle lead, one `deliver_now` question may defer the batch by `DEFER_MS`, then
//! it is asked again, at most `INTERRUPT_MAX_DEFERRALS` times in a row. It can
//! never deliver earlier or bypass the gate; a failed or slow call, or the
//! deferral cap, lets the delivery proceed.
use std::collections::BTreeMap;

use serde_json::json;

use super::journal::{bounded, Entry, MAX_SUMMARY_BYTES};
use super::provider::{DecisionProvider, DecisionRequest, Question};
use crate::config::PolarisDecisionsConfig;

pub(crate) const DEFER_MS: u64 = 30_000;
/// Deferrals in a row before delivery proceeds through the normal gate without
/// asking (10 × `DEFER_MS` = 5 minutes).
pub(crate) const INTERRUPT_MAX_DEFERRALS: u32 = 10;
/// How long a "deliver now" answer stays valid for the gate's next pass.
const CLEARED_MS: u64 = 5_000;
/// Lead screen characters sent, after redaction.
pub(crate) const SCREEN_TAIL_CHARS: usize = 500;

#[derive(Debug, Clone)]
pub(crate) struct InterruptInput {
    pub(crate) lead_state: &'static str,
    pub(crate) screen_tail: String,
    pub(crate) outcomes: Vec<String>,
    pub(crate) blocked: bool,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) struct InterruptVerdict {
    pub(crate) id: u64,
    pub(crate) deliver: bool,
}

/// Whether the gate lets this pass deliver.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Pass {
    Proceed,
    Wait,
    /// Ask now; the delivery waits for the answer.
    Ask,
}

/// Runtime state of the interruption check for the lead.
#[derive(Debug, Default)]
pub(crate) struct Gate {
    pending: Option<(u64, u64)>,
    defer_until: u64,
    cleared_until: u64,
    /// "Not now" answers since the last delivery.
    deferrals: u32,
}

impl Gate {
    /// `wait_ms` bounds how long a pending call may hold the delivery.
    pub(crate) fn pass(&mut self, now: u64, wait_ms: u64) -> Pass {
        if now < self.defer_until {
            return Pass::Wait;
        }
        if now < self.cleared_until || self.deferrals >= INTERRUPT_MAX_DEFERRALS {
            return Pass::Proceed;
        }
        match self.pending {
            Some((_, since)) if now < since + wait_ms => Pass::Wait,
            Some(_) => {
                self.pending = None;
                Pass::Proceed
            }
            None => Pass::Ask,
        }
    }

    pub(crate) fn asked(&mut self, id: u64, now: u64) {
        self.pending = Some((id, now));
    }

    /// Applies an answer to the pending call; stale answers are ignored.
    pub(crate) fn answer(&mut self, verdict: InterruptVerdict, now: u64) {
        if self.pending.map(|(id, _)| id) != Some(verdict.id) {
            return;
        }
        self.pending = None;
        if verdict.deliver {
            self.cleared_until = now + CLEARED_MS;
        } else {
            self.deferrals += 1;
            self.defer_until = now + DEFER_MS;
        }
    }

    /// A delivery prompt went out: the next one may be deferred again.
    pub(crate) fn delivered(&mut self) {
        self.deferrals = 0;
    }

    pub(crate) fn reset(&mut self) {
        *self = Self::default();
    }
}

pub(crate) fn request(input: &InterruptInput) -> DecisionRequest {
    let tail: String = input.screen_tail.chars().take(SCREEN_TAIL_CHARS).collect();
    DecisionRequest {
        state: json!({
            "lead_state": input.lead_state,
            "lead_recent_screen_tail": tail,
            "report_outcomes": input.outcomes,
            "report_is_blocking_anything": input.blocked,
        }),
        questions: BTreeMap::from([(
            "deliver_now".to_owned(),
            Question::Noul {
                instructions: "Should the lead be interrupted with this report right now, rather than batching it for later delivery?".into(),
                criteria: Some(BTreeMap::from([
                    ("true".to_owned(), "Deliver immediately; the lead is idle or the report blocks work".to_owned()),
                    ("false".to_owned(), "Batch for later; the lead is busy mid-thought and the report blocks nothing".to_owned()),
                ])),
            },
        )]),
    }
}

pub(crate) fn decide(
    id: u64,
    input: InterruptInput,
    config: &PolarisDecisionsConfig,
    provider: &dyn DecisionProvider,
    now_ms: u64,
) -> (InterruptVerdict, Entry) {
    let started = std::time::Instant::now();
    let deliver_min = config.interruption.deliver_min;
    let mut entry = Entry {
        ts: now_ms,
        kind: "interruption".into(),
        provider: provider.name().into(),
        subject: None,
        inputs_summary: bounded(
            &format!(
                "lead_state: {}; outcomes: {:?}; blocked: {}; screen: {}",
                input.lead_state, input.outcomes, input.blocked, input.screen_tail
            ),
            MAX_SUMMARY_BYTES,
        ),
        answers: None,
        thresholds: json!({"deliver_min": deliver_min}),
        action: "deliver".into(),
        latency_ms: 0,
        error: None,
    };
    let deliver = match provider.evaluate(request(&input)) {
        Ok(answers) => {
            entry.answers = serde_json::to_value(&answers).ok();
            answers.noul("deliver_now").unwrap_or(1.0) >= deliver_min
        }
        Err(error) => {
            entry.error = Some(error.to_string());
            true
        }
    };
    if !deliver {
        entry.action = format!("defer {}s", DEFER_MS / 1000);
    }
    entry.latency_ms = started.elapsed().as_millis() as u64;
    (InterruptVerdict { id, deliver }, entry)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::orbit_decisions::provider::{DecisionError, Scripted};

    fn fixture(name: &str) -> String {
        std::fs::read_to_string(format!(
            "{}/tests/fixtures/decisions/{name}.json",
            env!("CARGO_MANIFEST_DIR")
        ))
        .unwrap()
    }

    fn input() -> InterruptInput {
        InterruptInput {
            lead_state: "idle",
            screen_tail: "Plan: 1) inspect the config loader, 2) then I will".into(),
            outcomes: vec!["completed".into()],
            blocked: false,
        }
    }

    fn verdict(name: &str) -> bool {
        let config = PolarisDecisionsConfig::default();
        decide(1, input(), &config, &Scripted::fixture(&fixture(name)), 0)
            .0
            .deliver
    }

    #[test]
    fn deliver_now_below_the_threshold_defers() {
        assert!(verdict("interrupt_idle"));
        assert!(verdict("interrupt_blocked"));
        assert!(!verdict("interrupt_busy"));
        let failing = Scripted::new(|_| Err(DecisionError::Timeout));
        let config = PolarisDecisionsConfig::default();
        assert!(
            decide(1, input(), &config, &failing, 0).0.deliver,
            "errors never hold delivery"
        );
    }

    #[test]
    fn gate_only_waits_and_asks_again_after_the_deferral() {
        let mut gate = Gate::default();
        assert_eq!(gate.pass(0, 5_000), Pass::Ask);
        gate.asked(1, 0);
        assert_eq!(gate.pass(100, 5_000), Pass::Wait);
        gate.answer(
            InterruptVerdict {
                id: 1,
                deliver: false,
            },
            200,
        );
        assert_eq!(gate.pass(200 + DEFER_MS - 1, 5_000), Pass::Wait);
        assert_eq!(gate.pass(200 + DEFER_MS, 5_000), Pass::Ask);
        gate.asked(2, 200 + DEFER_MS);
        // A stale answer changes nothing.
        gate.answer(
            InterruptVerdict {
                id: 1,
                deliver: true,
            },
            200 + DEFER_MS,
        );
        assert_eq!(gate.pass(201 + DEFER_MS, 5_000), Pass::Wait);
        gate.answer(
            InterruptVerdict {
                id: 2,
                deliver: true,
            },
            300 + DEFER_MS,
        );
        assert_eq!(gate.pass(301 + DEFER_MS, 5_000), Pass::Proceed);
        // A call that never answers stops holding delivery after the wait.
        let mut gate = Gate::default();
        gate.asked(3, 0);
        assert_eq!(gate.pass(4_999, 5_000), Pass::Wait);
        assert_eq!(gate.pass(5_000, 5_000), Pass::Proceed);
    }

    #[test]
    fn repeated_not_now_answers_stop_deferring_after_the_cap() {
        let mut gate = Gate::default();
        let mut now = 0;
        for id in 0..u64::from(INTERRUPT_MAX_DEFERRALS) {
            assert_eq!(gate.pass(now, 5_000), Pass::Ask, "deferral {id}");
            gate.asked(id, now);
            gate.answer(InterruptVerdict { id, deliver: false }, now);
            assert_eq!(gate.pass(now + DEFER_MS - 1, 5_000), Pass::Wait);
            now += DEFER_MS;
        }
        // Five minutes of "not now": the delivery goes through the normal gate.
        assert_eq!(now, 5 * 60 * 1000);
        assert_eq!(gate.pass(now, 5_000), Pass::Proceed);
        assert_eq!(gate.pass(now + 60_000, 5_000), Pass::Proceed);
        // After a delivery the next report may be deferred again.
        gate.delivered();
        assert_eq!(gate.pass(now + 60_000, 5_000), Pass::Ask);
    }
}
