//! Retry policy and uncertain-delivery recovery with controlled time.
use super::super::deliver::POSSIBLE_DUPLICATE;
use super::super::pump::{backoff_ms, MAX_ATTEMPTS, VERIFY_DELAY_MS};
use super::*;

async fn reported(app: &mut App) -> (String, String) {
    let result = assign(app).await;
    assert!(report(app, result.clone()).get("error").is_none());
    let lead_id = task(app, &result.task_id).lead_id.clone();
    (result.task_id, lead_id)
}

fn lead_runtime<'a>(app: &'a App, lead_id: &str) -> &'a crate::terminal::TerminalRuntime {
    let key = app.state.terminals.get_key_value(lead_id).unwrap().0;
    app.terminal_runtimes.get(key).unwrap()
}

/// A Copilot lead whose PTY input queue is full: each submission is refused
/// before any write with a retryable `agent_prompt_failed`.
fn saturate_copilot_lead(app: &mut App, lead_id: &str) {
    let terminal = app.state.terminals.get_mut(lead_id).unwrap();
    terminal.set_detected_state_with_screen_signals_at(
        Some(Agent::GithubCopilot),
        AgentState::Idle,
        false,
        true,
        false,
        false,
        Instant::now(),
    );
    let runtime = lead_runtime(app, lead_id);
    while runtime
        .try_send_bytes(bytes::Bytes::from_static(b"x"))
        .is_ok()
    {}
}

#[tokio::test]
async fn retryable_submit_errors_back_off_then_fail_after_max_attempts() {
    let (mut app, _worker, _lead) = setup();
    let (task_id, lead_id) = reported(&mut app).await;
    saturate_copilot_lead(&mut app, &lead_id);
    let mut at = Instant::now();
    for attempt in 1..MAX_ATTEMPTS {
        pump_at(&mut app, at);
        let stored = task(&app, &task_id);
        assert_eq!(stored.attempts, attempt);
        assert_eq!(stored.delivery, Delivery::Queued);
        assert!(stored
            .error
            .as_deref()
            .unwrap()
            .contains("agent_prompt_failed"));
        let delay = Duration::from_millis(backoff_ms(attempt));
        // Before the backoff elapses nothing is attempted.
        pump_at(&mut app, at + delay - Duration::from_millis(1));
        assert_eq!(task(&app, &task_id).attempts, attempt);
        at += delay;
    }
    pump_at(&mut app, at);
    let stored = task(&app, &task_id);
    assert_eq!(stored.attempts, MAX_ATTEMPTS);
    assert_eq!(stored.delivery, Delivery::Failed);
}

#[tokio::test]
async fn lead_at_a_shell_keeps_the_report_queued_without_spending_attempts() {
    let (mut app, _worker, mut lead) = setup();
    let (task_id, lead_id) = reported(&mut app).await;
    // Idle, but the pane runs a shell: no known agent hosts the lead composer.
    app.state
        .terminals
        .get_mut(lead_id.as_str())
        .unwrap()
        .set_detected_state(None, AgentState::Idle);
    let start = Instant::now();
    for second in (0..=60).step_by(5) {
        pump_at(&mut app, start + Duration::from_secs(second));
        let stored = task(&app, &task_id);
        assert_eq!(stored.delivery, Delivery::Queued);
        assert_eq!(stored.attempts, 0, "a shell lead is a wait");
    }
    assert!(lead.try_recv().is_err());
    set_state(&mut app, &lead_id, AgentState::Idle);
    pump_at(&mut app, start + Duration::from_secs(61));
    let stored = task(&app, &task_id);
    assert_eq!(stored.delivery, Delivery::Submitting);
    assert_eq!(stored.attempts, 1);
    pump_until(&mut app, &task_id, Delivery::Submitted).await;
    assert!(drain(&mut lead).contains(&format!("[orbit-delivery {task_id} #1]")));
}

#[tokio::test]
async fn lead_runtime_refusal_is_retried_without_spending_attempts() {
    let (mut app, _worker, _lead) = setup();
    let (task_id, lead_id) = reported(&mut app).await;
    let key = app
        .state
        .terminals
        .get_key_value(lead_id.as_str())
        .unwrap()
        .0
        .clone();
    let runtime = app.terminal_runtimes.remove(&key).unwrap();
    let start = Instant::now();
    for second in (0..=60).step_by(4) {
        pump_at(&mut app, start + Duration::from_secs(second));
    }
    let stored = task(&app, &task_id);
    assert_eq!(stored.delivery, Delivery::Queued);
    assert_eq!(stored.attempts, 0);
    assert!(stored.error.as_deref().unwrap().contains("agent_not_found"));
    app.terminal_runtimes.insert(key, runtime);
    pump_at(&mut app, start + Duration::from_secs(64));
    let stored = task(&app, &task_id);
    assert_eq!(stored.delivery, Delivery::Submitting);
    assert_eq!(stored.attempts, 1);
}

#[tokio::test]
async fn uncertain_delivery_with_visible_marker_becomes_submitted_without_resend() {
    let (mut app, _worker, mut lead) = setup();
    let (task_id, lead_id) = reported(&mut app).await;
    fail_delivery_ack(&mut app, &task_id, 1);
    let start = Instant::now();
    pump_at(&mut app, start);
    assert_eq!(task(&app, &task_id).delivery, Delivery::Uncertain);
    let marker = task(&app, &task_id).delivery_marker();
    lead_runtime(&app, &lead_id).test_process_pty_bytes(format!("{marker}\r\n").as_bytes());
    pump_at(&mut app, start + Duration::from_millis(VERIFY_DELAY_MS / 2));
    assert_eq!(
        task(&app, &task_id).delivery,
        Delivery::Uncertain,
        "verified too early"
    );
    pump_at(&mut app, start + Duration::from_millis(VERIFY_DELAY_MS + 1));
    assert_eq!(task(&app, &task_id).delivery, Delivery::Submitted);
    assert!(lead.try_recv().is_err(), "verified delivery was resent");
}

#[tokio::test]
async fn uncertain_delivery_without_marker_requeues_once_then_fails() {
    let (mut app, _worker, mut lead) = setup();
    let (task_id, lead_id) = reported(&mut app).await;
    // An earlier attempt's marker must not count for this attempt.
    let stale = format!("[orbit-delivery {task_id} #1]\r\n");
    lead_runtime(&app, &lead_id).test_process_pty_bytes(stale.as_bytes());
    fail_delivery_ack(&mut app, &task_id, 2);
    let start = Instant::now();
    pump_at(&mut app, start);
    let later = start + Duration::from_millis(VERIFY_DELAY_MS + 1);
    pump_at(&mut app, later);
    // Requeued once and resubmitted on the same tick as a real, counted attempt.
    let stored = task(&app, &task_id);
    assert_eq!(stored.delivery, Delivery::Submitting);
    assert_eq!(stored.recoveries, 1);
    pump_until(&mut app, &task_id, Delivery::Submitted).await;
    assert_eq!(task(&app, &task_id).attempts, 3);
    assert_eq!(drain(&mut lead).matches("Orbit worker report").count(), 1);
    // A second uncertain write is never resent. Ledger time never runs backwards,
    // so the next phase starts after the last instant used above.
    fail_delivery_ack(&mut app, &task_id, 4);
    let start = later.max(Instant::now()) + Duration::from_secs(1);
    pump_at(&mut app, start);
    pump_at(&mut app, start + Duration::from_millis(VERIFY_DELAY_MS + 1));
    let stored = task(&app, &task_id);
    assert_eq!(stored.delivery, Delivery::Failed);
    assert!(stored.error.as_deref().unwrap().contains("uncertain twice"));
    pump(&mut app);
    assert!(lead.try_recv().is_err());
}

#[tokio::test]
async fn uncertain_delivery_to_a_vanished_lead_goes_to_the_new_lead_once() {
    let (mut app, mut worker, _lead) = setup();
    let result = assign(&mut app).await;
    drain(&mut worker);
    assert!(report(&mut app, result.clone()).get("error").is_none());
    let task_id = result.task_id;
    let old_lead = task(&app, &task_id).lead_id.clone();
    fail_delivery_ack(&mut app, &task_id, 1);
    pump(&mut app);
    assert_eq!(task(&app, &task_id).delivery, Delivery::Uncertain);
    app.state.terminals.remove(old_lead.as_str()).unwrap();
    // The worker pane now serves as the lead.
    let new_lead = result.worker_id;
    app.bind_orbit_lead(new_lead.clone());
    pump(&mut app);
    let stored = task(&app, &task_id);
    assert_eq!(stored.delivery, Delivery::Submitting);
    assert_eq!(stored.lead_id, new_lead);
    assert!(stored.redelivered);
    assert_eq!(stored.recoveries, 1);
    pump_until(&mut app, &task_id, Delivery::Submitted).await;
    let text = drain(&mut worker);
    assert!(text.contains(POSSIBLE_DUPLICATE), "{text}");
    assert!(text.contains(&format!("[orbit-delivery {task_id} #2]")));
    // A second unverifiable delivery is never resent.
    fail_delivery_ack(&mut app, &task_id, 2);
    pump(&mut app);
    assert_eq!(task(&app, &task_id).delivery, Delivery::Uncertain);
    app.state.terminals.remove(new_lead.as_str()).unwrap();
    pump(&mut app);
    let stored = task(&app, &task_id);
    assert_eq!(stored.delivery, Delivery::Failed);
    assert!(stored
        .error
        .as_deref()
        .unwrap()
        .contains("lead terminal gone"));
}

#[tokio::test]
async fn lead_working_after_an_uncertain_write_proves_submission() {
    let (mut app, _worker, mut lead) = setup();
    let (task_id, lead_id) = reported(&mut app).await;
    fail_delivery_ack(&mut app, &task_id, 1);
    let start = Instant::now();
    pump_at(&mut app, start);
    assert_eq!(task(&app, &task_id).delivery, Delivery::Uncertain);
    set_state(&mut app, &lead_id, AgentState::Working);
    // Proof needs no verify delay and no screen read.
    pump_at(&mut app, start + Duration::from_millis(1));
    let stored = task(&app, &task_id);
    assert_eq!(stored.delivery, Delivery::Submitted);
    assert_eq!(stored.recoveries, 0);
    assert!(lead.try_recv().is_err(), "proven delivery was resent");
}

#[tokio::test]
async fn an_unverified_delivery_holds_later_reports_for_that_lead() {
    let (mut app, mut worker, mut lead) = setup();
    let first = assign(&mut app).await;
    assert!(report(&mut app, first.clone()).get("error").is_none());
    let second = assign(&mut app).await;
    drain(&mut worker);
    assert!(report(&mut app, second.clone()).get("error").is_none());
    // `first` is uncertain; `second` must wait so it cannot fake proof for `first`.
    fail_delivery_ack(&mut app, &first.task_id, 1);
    task_mut(&mut app, &second.task_id).delivery = Delivery::Queued;
    let start = Instant::now();
    pump_at(&mut app, start);
    assert_eq!(task(&app, &first.task_id).delivery, Delivery::Uncertain);
    assert_eq!(task(&app, &second.task_id).delivery, Delivery::Queued);
    assert!(lead.try_recv().is_err());
    // After verification requeues `first`, both go out together.
    pump_at(&mut app, start + Duration::from_millis(VERIFY_DELAY_MS + 1));
    assert_eq!(task(&app, &first.task_id).delivery, Delivery::Submitting);
    assert_eq!(task(&app, &second.task_id).delivery, Delivery::Submitting);
}

/// A real delivery whose shared ACK then fails: `Uncertain`, with the paste
/// number recorded before the write.
async fn uncertain_after_real_write(app: &mut App, task_id: &str, start: Instant) {
    pump_at(app, start);
    assert_eq!(task(app, task_id).delivery, Delivery::Submitting);
    let batch = task(app, task_id).live.batch.unwrap();
    let (tx, rx) = std::sync::mpsc::channel();
    app.orbit_notifications.batches.get_mut(&batch).unwrap().ack = rx;
    tx.send(Err(std::io::Error::other("partial write")))
        .unwrap();
    pump_at(app, start + Duration::from_millis(1));
    assert_eq!(task(app, task_id).delivery, Delivery::Uncertain);
}

#[tokio::test]
async fn a_newer_collapsed_paste_on_the_lead_screen_proves_an_uncertain_delivery() {
    let (mut app, _worker, mut lead) = setup();
    let (task_id, lead_id) = reported(&mut app).await;
    // An earlier paste is already on screen before the delivery is written.
    lead_runtime(&app, &lead_id).test_process_pty_bytes(b"> [Pasted text #1 +12 lines]\r\n");
    let start = Instant::now();
    uncertain_after_real_write(&mut app, &task_id, start).await;
    assert_eq!(task(&app, &task_id).live.pasted_before, Some(1));
    drain(&mut lead);
    // Claude collapsed this delivery: no marker, but a newer placeholder.
    lead_runtime(&app, &lead_id).test_process_pty_bytes(b"> [Pasted text #2 +40 lines]\r\n");
    pump_at(&mut app, start + Duration::from_millis(VERIFY_DELAY_MS + 2));
    let stored = task(&app, &task_id);
    assert_eq!(stored.delivery, Delivery::Submitted);
    assert_eq!(stored.recoveries, 0);
    // The first write may still be finishing (its Enter goes separately), but
    // no second delivery follows.
    tokio::time::sleep(Duration::from_millis(20)).await;
    pump_at(&mut app, start + Duration::from_millis(VERIFY_DELAY_MS + 3));
    let later = drain(&mut lead);
    assert!(
        !later.contains("[orbit-delivery"),
        "proven delivery was resent: {later}"
    );
}

#[tokio::test]
async fn an_old_collapsed_paste_is_no_proof_and_the_report_is_requeued_once() {
    let (mut app, _worker, mut lead) = setup();
    let (task_id, lead_id) = reported(&mut app).await;
    lead_runtime(&app, &lead_id).test_process_pty_bytes(b"> [Pasted text #3 +12 lines]\r\n");
    let start = Instant::now();
    uncertain_after_real_write(&mut app, &task_id, start).await;
    drain(&mut lead);
    pump_at(&mut app, start + Duration::from_millis(VERIFY_DELAY_MS + 2));
    let stored = task(&app, &task_id);
    assert_eq!(stored.recoveries, 1, "requeued once");
    assert!(stored.redelivered);
}

/// Verification reads the last 200 lines: a marker scrolled further up is not
/// seen, so the report is requeued once and labelled a possible duplicate.
#[tokio::test]
async fn a_marker_scrolled_past_the_verified_lines_requeues_once_as_a_possible_duplicate() {
    let (mut app, _worker, mut lead) = setup();
    let (task_id, lead_id) = reported(&mut app).await;
    let start = Instant::now();
    uncertain_after_real_write(&mut app, &task_id, start).await;
    drain(&mut lead);
    let marker = task(&app, &task_id).delivery_marker();
    let mut screen = format!("{marker}\r\n");
    for line in 0..250 {
        screen.push_str(&format!("lead output line {line}\r\n"));
    }
    lead_runtime(&app, &lead_id).test_process_pty_bytes(screen.as_bytes());
    pump_at(&mut app, start + Duration::from_millis(VERIFY_DELAY_MS + 2));
    let stored = task(&app, &task_id);
    assert_eq!(stored.recoveries, 1);
    assert!(stored.redelivered);
    let text = drain_until(&mut lead, |text| text.contains(POSSIBLE_DUPLICATE)).await;
    assert!(text.contains(POSSIBLE_DUPLICATE), "{text}");
}
