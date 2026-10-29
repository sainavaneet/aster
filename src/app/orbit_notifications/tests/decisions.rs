//! The decision layer on the ledger: a blocked worker's permission prompt is
//! decided (rules first, then a scripted provider) before its notice reaches
//! the lead; report checks add a line; interruption only defers.
use std::sync::Arc;

use super::super::lifecycle::{BLOCKED_NOTICE_MS, STARTUP_GRACE_MS};
use super::super::state::DecisionMark;
use super::*;
use crate::app::orbit_decisions::provider::{DecisionProvider, Scripted};
use crate::config::PolarisDecisionsConfig;

const AFTER_GRACE: Duration = Duration::from_millis(STARTUP_GRACE_MS + 1_000);
const BLOCKED: Duration = Duration::from_millis(BLOCKED_NOTICE_MS);

fn fixture(name: &str) -> String {
    std::fs::read_to_string(format!(
        "{}/tests/fixtures/decisions/{name}.json",
        env!("CARGO_MANIFEST_DIR")
    ))
    .unwrap()
}

/// Claude's Bash permission dialog for `command`.
fn permission_prompt(command: &str) -> String {
    format!("\r\n Bash command\r\n\r\n   {command}\r\n\r\n Do you want to proceed?\r\n \u{276f} 1. Yes\r\n   2. Yes, and don't ask again for this command\r\n   3. No, and tell Claude what to do differently (esc)\r\n\r\n Esc to cancel \u{b7} Tab to amend\r\n")
}

fn show(app: &mut App, terminal_id: &str, screen: &str) {
    let key = app
        .state
        .terminals
        .get_key_value(terminal_id)
        .map(|(key, _)| key.clone())
        .unwrap();
    let runtime = app.terminal_runtimes.get(&key).unwrap();
    runtime.test_process_pty_bytes(format!("\x1b[2J\x1b[H{screen}").as_bytes());
}

/// `lead_may_approve`, the given decisions config, and a scripted provider.
fn configure(app: &mut App, decisions: PolarisDecisionsConfig, provider: Option<Scripted>) {
    let mut config = crate::config::PolarisConfig::default();
    config.approvals.lead_may_approve = true;
    *config.decisions = decisions.clone();
    app.configure_orbit_notifications(&config);
    app.configure_orbit_decisions(&decisions);
    let provider = provider.map(|provider| Arc::new(provider) as Arc<dyn DecisionProvider>);
    app.orbit_decisions.set_test_provider(provider);
}

fn approvals_enabled() -> PolarisDecisionsConfig {
    let mut config = PolarisDecisionsConfig::default();
    config.approvals.enabled = true;
    config
}

/// Pumps until every queued task is decided, applying decision events.
async fn settle_decisions(app: &mut App, at: Instant) {
    for _ in 0..400 {
        app.drain_all_internal_events();
        pump_at(app, at);
        let pending = app
            .orbit_notifications
            .tasks
            .iter()
            .any(|task| matches!(task.live.decision, DecisionMark::Pending { .. }));
        if !pending {
            return;
        }
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    panic!("decision never arrived");
}

fn blocked_notice(app: &App) -> String {
    app.orbit_notifications
        .tasks
        .iter()
        .find(|task| task.task_id.contains("/blocked-"))
        .map(|task| task.task_id.clone())
        .expect("blocked notice")
}

/// Assigns, then keeps the worker blocked on `command` until it is relayed
/// while the lead is busy. Returns the notice id and the time reached.
async fn block_on(app: &mut App, worker: &mut Output, command: &str) -> (String, Instant) {
    let result = assign(app).await;
    drain(worker);
    let lead = lead_id(app);
    set_state(app, &lead, AgentState::Working);
    show(app, &result.worker_id, &permission_prompt(command));
    set_state(app, &result.worker_id, AgentState::Blocked);
    let start = Instant::now() + AFTER_GRACE;
    pump_at(app, start);
    pump_at(app, start + BLOCKED);
    (blocked_notice(app), start + BLOCKED)
}

/// Delivers the queued notice to the now idle lead and returns the text.
async fn deliver(app: &mut App, lead: &mut Output, notice: &str) -> String {
    let lead_terminal = lead_id(app);
    set_state(app, &lead_terminal, AgentState::Idle);
    pump_until(app, notice, Delivery::Submitted).await;
    drain(lead)
}

#[tokio::test]
async fn scripted_approval_sends_the_keys_and_the_notice_says_so() {
    let (mut app, mut worker, mut lead) = setup();
    let provider = Scripted::fixture(&fixture("approve_date"));
    let calls = provider.calls.clone();
    configure(&mut app, approvals_enabled(), Some(provider));
    let (notice, at) = block_on(&mut app, &mut worker, "date").await;
    assert!(matches!(
        task(&app, &notice).live.decision,
        DecisionMark::Pending { .. }
    ));
    settle_decisions(&mut app, at).await;
    let keys = drain_until(&mut worker, |text| !text.is_empty()).await;
    assert_eq!(keys, "1", "approval keys sent");
    assert_eq!(calls.load(std::sync::atomic::Ordering::SeqCst), 1);
    let text = deliver(&mut app, &mut lead, &notice).await;
    assert!(
        text.contains("Decision: approve (scripted, p=0.99, risk 0.0)"),
        "{text}"
    );
    let activity = app.orbit_decision_activity();
    let entries = activity.as_array().unwrap();
    assert_eq!(entries.len(), 1, "{activity}");
    assert_eq!(entries[0]["kind"], "approval");
    assert_eq!(entries[0]["action"], "approve");
    assert_eq!(entries[0]["provider"], "scripted");
}

#[tokio::test]
async fn scripted_ask_sends_no_keys_and_the_notice_says_why() {
    let (mut app, mut worker, mut lead) = setup();
    configure(
        &mut app,
        approvals_enabled(),
        Some(Scripted::fixture(&fixture("approve_cargo"))),
    );
    let (notice, at) = block_on(&mut app, &mut worker, "cargo test").await;
    settle_decisions(&mut app, at).await;
    tokio::time::sleep(Duration::from_millis(20)).await;
    assert_eq!(drain(&mut worker), "", "no keys");
    let text = deliver(&mut app, &mut lead, &notice).await;
    assert!(
        text.contains("Decision: ask the user (scripted, p=0.53, risk 1.0)"),
        "{text}"
    );
}

#[tokio::test]
async fn deny_patterns_ask_the_user_without_calling_the_provider() {
    let (mut app, mut worker, mut lead) = setup();
    let provider = Scripted::fixture(&fixture("approve_date"));
    let calls = provider.calls.clone();
    configure(&mut app, approvals_enabled(), Some(provider));
    let (notice, at) = block_on(&mut app, &mut worker, "rm -rf build").await;
    settle_decisions(&mut app, at).await;
    tokio::time::sleep(Duration::from_millis(20)).await;
    assert_eq!(drain(&mut worker), "");
    assert_eq!(calls.load(std::sync::atomic::Ordering::SeqCst), 0);
    let text = deliver(&mut app, &mut lead, &notice).await;
    assert!(
        text.contains("Decision: ask the user (rules: deny pattern 'rm -rf')"),
        "{text}"
    );
}

#[tokio::test]
async fn without_lead_may_approve_no_decision_is_made() {
    let (mut app, mut worker, mut lead) = setup();
    configure(
        &mut app,
        approvals_enabled(),
        Some(Scripted::fixture(&fixture("approve_date"))),
    );
    let mut config = crate::config::PolarisConfig::default();
    config.approvals.lead_may_approve = false;
    app.configure_orbit_notifications(&config);
    let (notice, _) = block_on(&mut app, &mut worker, "date").await;
    assert_eq!(task(&app, &notice).live.decision, DecisionMark::Done);
    let text = deliver(&mut app, &mut lead, &notice).await;
    assert!(!text.contains("Decision:"), "{text}");
    assert_eq!(drain(&mut worker), "");
    assert!(app.orbit_decision_activity().as_array().unwrap().is_empty());
}

#[tokio::test]
async fn incomplete_report_carries_a_decision_line_and_sanity_fields() {
    let (mut app, _worker, mut lead) = setup();
    let mut decisions = PolarisDecisionsConfig::default();
    decisions.report_sanity.enabled = true;
    configure(
        &mut app,
        decisions,
        Some(Scripted::fixture(&fixture("report_mid"))),
    );
    let result = assign(&mut app).await;
    let response = serde_json::json!({"result": {"task_id": result.task_id}}).to_string();
    app.note_orbit_assignment_prompt("Review the commit and run the tests", &response);
    let lead_terminal = lead_id(&app);
    set_state(&mut app, &lead_terminal, AgentState::Working);
    assert!(report(&mut app, result.clone()).get("error").is_none());
    settle_decisions(&mut app, Instant::now()).await;
    let sanity = app.orbit_task_sanity();
    assert_eq!(sanity.len(), 1);
    assert_eq!(sanity[0].1["flagged"], true);
    let text = deliver(&mut app, &mut lead, &result.task_id).await;
    assert!(
        text.contains("Decision: report may be incomplete (score 2.1, follow-up 0.89)"),
        "{text}"
    );
}

#[tokio::test]
async fn thorough_report_is_delivered_without_a_line() {
    let (mut app, _worker, mut lead) = setup();
    let mut decisions = PolarisDecisionsConfig::default();
    decisions.report_sanity.enabled = true;
    configure(
        &mut app,
        decisions,
        Some(Scripted::fixture(&fixture("report_good"))),
    );
    let result = assign(&mut app).await;
    let response = serde_json::json!({"result": {"task_id": result.task_id}}).to_string();
    app.note_orbit_assignment_prompt("Review the commit", &response);
    let lead_terminal = lead_id(&app);
    set_state(&mut app, &lead_terminal, AgentState::Working);
    assert!(report(&mut app, result.clone()).get("error").is_none());
    settle_decisions(&mut app, Instant::now()).await;
    let text = deliver(&mut app, &mut lead, &result.task_id).await;
    assert!(!text.contains("Decision:"), "{text}");
}

#[tokio::test]
async fn interruption_defers_delivery_and_never_bypasses_the_gate() {
    let (mut app, _worker, mut lead) = setup();
    let mut decisions = PolarisDecisionsConfig::default();
    decisions.interruption.enabled = true;
    configure(
        &mut app,
        decisions,
        Some(Scripted::fixture(&fixture("interrupt_busy"))),
    );
    let result = assign(&mut app).await;
    let lead_terminal = lead_id(&app);
    // A busy lead is never asked: the gate decides first.
    set_state(&mut app, &lead_terminal, AgentState::Working);
    assert!(report(&mut app, result.clone()).get("error").is_none());
    let start = Instant::now();
    pump_at(&mut app, start);
    tokio::time::sleep(Duration::from_millis(30)).await;
    app.drain_all_internal_events();
    assert!(app.orbit_decision_activity().as_array().unwrap().is_empty());
    // Idle lead: asked, told to wait, so nothing is typed for 30 seconds.
    set_state(&mut app, &lead_terminal, AgentState::Idle);
    for _ in 0..100 {
        pump_at(&mut app, start);
        app.drain_all_internal_events();
        if !app.orbit_decision_activity().as_array().unwrap().is_empty() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    pump_at(&mut app, start + Duration::from_secs(29));
    assert!(lead.try_recv().is_err());
    assert_eq!(task(&app, &result.task_id).delivery, Delivery::Queued);
    // After the deferral it asks again; an answer to deliver lets it through.
    app.orbit_decisions
        .set_test_provider(Some(Arc::new(Scripted::fixture(&fixture(
            "interrupt_idle",
        )))));
    let later = start + Duration::from_secs(31);
    for _ in 0..200 {
        pump_at(&mut app, later);
        app.drain_all_internal_events();
        if task(&app, &result.task_id).delivery != Delivery::Queued {
            break;
        }
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    assert_ne!(task(&app, &result.task_id).delivery, Delivery::Queued);
}

/// A provider that never answers within the hold: the notice is released by
/// the ledger tick as "ask the user" after `timeout_ms` + 2 s, and no keys are sent.
#[tokio::test]
async fn a_decision_that_never_answers_is_released_as_ask_user_on_the_tick() {
    let (mut app, mut worker, mut lead) = setup();
    let (release, parked) = std::sync::mpsc::channel::<()>();
    let parked = std::sync::Mutex::new(parked);
    let provider = Scripted::new(move |_| {
        let _ = parked.lock().unwrap().recv();
        Err(crate::app::orbit_decisions::provider::DecisionError::Timeout)
    });
    configure(&mut app, approvals_enabled(), Some(provider));
    let (notice, at) = block_on(&mut app, &mut worker, "date").await;
    let DecisionMark::Pending { until } = task(&app, &notice).live.decision else {
        panic!("decision not held");
    };
    let hold = app.orbit_decisions.hold_ms();
    assert_eq!(hold, 3_000 + 2_000, "timeout_ms plus the 2 s grace");
    let held_at = app.orbit_notifications.now_ms(at);
    assert_eq!(until, held_at + hold);
    pump_at(&mut app, at + Duration::from_millis(hold - 1));
    assert!(matches!(
        task(&app, &notice).live.decision,
        DecisionMark::Pending { .. }
    ));
    pump_at(&mut app, at + Duration::from_millis(hold));
    assert_eq!(task(&app, &notice).live.decision, DecisionMark::Done);
    let text = deliver(&mut app, &mut lead, &notice).await;
    assert!(
        text.contains("Decision: ask the user (decision timed out)"),
        "{text}"
    );
    // The late answer changes nothing and no approval keys were sent.
    drop(release);
    tokio::time::sleep(Duration::from_millis(50)).await;
    app.drain_all_internal_events();
    assert_eq!(drain(&mut worker), "");
}

/// Codex's approval overlay shows no Claude-style "Bash command" block, so the
/// command cannot be read: even an approving provider is never asked and no
/// keys are sent (review finding 1, end to end on the ledger).
#[tokio::test]
async fn a_codex_overlay_with_an_approving_provider_sends_no_keys() {
    let (mut app, mut worker, mut lead) = setup();
    let provider = Scripted::fixture(&fixture("approve_date"));
    let calls = provider.calls.clone();
    configure(&mut app, approvals_enabled(), Some(provider));
    let result = assign(&mut app).await;
    drain(&mut worker);
    let lead_terminal = lead_id(&app);
    set_state(&mut app, &lead_terminal, AgentState::Working);
    let overlay = "\r\n Would you like to run the following command?\r\n\r\n   $ date\r\n\r\n \u{203a} 1. Yes, proceed (y)\r\n   2. Yes, and don't ask again for this command (a)\r\n   3. No, and tell Codex what to do differently (esc)\r\n\r\n Press enter to confirm or esc to cancel\r\n";
    show(&mut app, &result.worker_id, overlay);
    app.state
        .terminals
        .get_mut(result.worker_id.as_str())
        .unwrap()
        .set_detected_state_with_screen_signals_at(
            Some(Agent::Codex),
            AgentState::Blocked,
            true,
            false,
            false,
            false,
            Instant::now(),
        );
    assert_eq!(
        app.orbit_permission_prompt_agent(&result.worker_id),
        Some(Agent::Codex),
        "the overlay is a permission prompt Orbit could answer"
    );
    let start = Instant::now() + AFTER_GRACE;
    pump_at(&mut app, start);
    pump_at(&mut app, start + BLOCKED);
    let notice = blocked_notice(&app);
    settle_decisions(&mut app, start + BLOCKED).await;
    assert_eq!(
        calls.load(std::sync::atomic::Ordering::SeqCst),
        0,
        "provider asked"
    );
    let text = deliver(&mut app, &mut lead, &notice).await;
    assert!(
        text.contains("Decision: ask the user (rules: command not readable from the prompt)"),
        "{text}"
    );
    tokio::time::sleep(Duration::from_millis(20)).await;
    assert_eq!(drain(&mut worker), "", "no approval keys");
}

/// Decisions are runtime-only: a notice held for a decision when the server
/// restarts is queued again, decided again (here: no provider, so plainly),
/// and delivered exactly once.
#[tokio::test]
async fn a_notice_held_for_a_decision_at_restart_is_delivered_once_afterwards() {
    let path = super::super::store::test_path("restart-decision");
    let (mut before, mut worker, mut lead) = setup();
    super::persistence::with_store(&mut before, &path);
    let (release, parked) = std::sync::mpsc::channel::<()>();
    let parked = std::sync::Mutex::new(parked);
    let provider = Scripted::new(move |_| {
        let _ = parked.lock().unwrap().recv();
        Err(crate::app::orbit_decisions::provider::DecisionError::Timeout)
    });
    configure(&mut before, approvals_enabled(), Some(provider));
    let (notice, _) = block_on(&mut before, &mut worker, "date").await;
    assert!(matches!(
        task(&before, &notice).live.decision,
        DecisionMark::Pending { .. }
    ));
    before.persist_orbit_ledger();
    let mut app = super::persistence::restart(&mut before, &path);
    pump(&mut app);
    let stored = task(&app, &notice);
    assert_eq!(
        stored.delivery,
        Delivery::Queued,
        "notice survived the restart"
    );
    assert_eq!(
        stored.live.decision,
        DecisionMark::Done,
        "decided again at once"
    );
    let text = deliver(&mut app, &mut lead, &notice).await;
    assert_eq!(
        text.matches(&format!("[orbit-delivery {notice} #1]"))
            .count(),
        1,
        "{text}"
    );
    drop(release);
    pump(&mut app);
    tokio::time::sleep(Duration::from_millis(20)).await;
    assert!(drain(&mut lead).is_empty(), "delivered twice");
    let _ = std::fs::remove_dir_all(path.parent().unwrap());
}
