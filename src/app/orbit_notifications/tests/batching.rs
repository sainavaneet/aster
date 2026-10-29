//! Several ready reports for one lead share one prompt and one PTY ACK.
use super::super::deliver::{BATCH_ACK_TIMEOUT_MS, MAX_BATCH, MAX_BATCH_BYTES};
use super::super::state::LiveState;
use super::*;

/// A queued worker report injected directly, as if reported earlier.
fn queued_task(task_id: &str, lead_id: &str, summary: String) -> Task {
    Task {
        task_id: task_id.into(),
        worker_id: "worker".into(),
        lead_id: lead_id.into(),
        submission: Submission::Submitted,
        delivery: Delivery::Queued,
        report: Some(OrbitAgentReportParams::new(
            task_id.into(),
            "worker".into(),
            OrbitOutcome::Completed,
            summary,
        )),
        origin: Some(ReportOrigin::Worker),
        notice_for: None,
        error: None,
        created_at: 1,
        updated_at: 1,
        title: None,
        attempts: 0,
        recoveries: 0,
        redelivered: false,
        nudged_at: None,
        deadline: None,
        late: false,
        late_from: None,
        extra: Default::default(),
        token_hash: None,
        live: LiveState::default(),
    }
}

fn lead_terminal(app: &App) -> String {
    app.resolve_agent_target("polaris").unwrap().terminal_id
}

fn inject(app: &mut App, count: usize, summary: impl Fn(usize) -> String) -> Vec<String> {
    let lead = lead_terminal(app);
    (0..count)
        .map(|index| {
            let id = format!("batch-{index}");
            let task = queued_task(&id, &lead, summary(index));
            app.orbit_notifications.tasks.push(task);
            id
        })
        .collect()
}

fn in_state(app: &App, ids: &[String], delivery: Delivery) -> usize {
    ids.iter()
        .filter(|id| task(app, id).delivery == delivery)
        .count()
}

/// The lead's output once it holds `entries` delivered reports (or 2 s passed).
async fn delivered_text(lead: &mut Output, entries: usize) -> String {
    drain_until(lead, |text| {
        text.matches("[orbit-delivery ").count() >= entries
    })
    .await
}

#[tokio::test]
async fn three_reports_arrive_in_one_prompt_and_are_submitted_on_one_ack() {
    let (mut app, mut worker, mut lead) = setup();
    // One assignment per worker at a time: each is reported before the next.
    let mut reports = Vec::new();
    for _ in 0..3 {
        let result = assign(&mut app).await;
        drain(&mut worker);
        assert!(report(&mut app, result.clone()).get("error").is_none());
        reports.push(result);
    }
    pump(&mut app);
    let batches: Vec<_> = reports
        .iter()
        .map(|result| task(&app, &result.task_id).live.batch)
        .collect();
    assert!(batches[0].is_some());
    assert!(
        batches.iter().all(|batch| *batch == batches[0]),
        "{batches:?}"
    );
    assert_eq!(app.orbit_notifications.batches.len(), 1);
    let text = delivered_text(&mut lead, 3).await;
    for result in &reports {
        assert!(text.contains(&format!("[orbit-delivery {} #1]", result.task_id)));
        assert!(
            !text.contains(result.token.as_deref().unwrap()),
            "token leaked"
        );
    }
    assert_eq!(text.matches("Orbit worker report").count(), 3);
    for result in &reports {
        pump_until(&mut app, &result.task_id, Delivery::Submitted).await;
    }
    assert!(app.orbit_notifications.batches.is_empty());
}

#[tokio::test]
async fn failed_shared_ack_makes_every_member_uncertain() {
    let (mut app, _worker, _lead) = setup();
    let ids = inject(&mut app, 3, |index| format!("result {index}"));
    pump(&mut app);
    assert_eq!(in_state(&app, &ids, Delivery::Submitting), 3);
    let batch = task(&app, &ids[0]).live.batch.unwrap();
    let (tx, rx) = std::sync::mpsc::channel();
    app.orbit_notifications.batches.get_mut(&batch).unwrap().ack = rx;
    tx.send(Err(std::io::Error::other("partial write")))
        .unwrap();
    pump(&mut app);
    assert_eq!(in_state(&app, &ids, Delivery::Uncertain), 3);
    for id in &ids {
        let stored = task(&app, id);
        assert!(stored.live.batch.is_none());
        assert!(stored.error.as_deref().unwrap().contains("partial write"));
    }
}

#[tokio::test]
async fn batch_respects_the_report_count_cap() {
    let (mut app, _worker, mut lead) = setup();
    let ids = inject(&mut app, MAX_BATCH + 2, |index| format!("result {index}"));
    pump(&mut app);
    assert_eq!(in_state(&app, &ids, Delivery::Submitting), MAX_BATCH);
    assert_eq!(in_state(&app, &ids, Delivery::Queued), 2);
    let text = delivered_text(&mut lead, MAX_BATCH).await;
    assert_eq!(text.matches("[orbit-delivery batch-").count(), MAX_BATCH);
    // The oldest reports go first.
    assert_eq!(task(&app, &ids[0]).delivery, Delivery::Submitting);
    assert_eq!(task(&app, &ids[MAX_BATCH]).delivery, Delivery::Queued);
}

#[tokio::test]
async fn batch_respects_the_byte_cap() {
    let (mut app, _worker, _lead) = setup();
    let ids = inject(&mut app, 6, |index| format!("{index}{}", "x".repeat(500)));
    pump(&mut app);
    // Six ~0.9 KiB entries exceed 2 KiB; two fit.
    let sent = in_state(&app, &ids, Delivery::Submitting);
    assert_eq!(sent, 2);
    let entries: usize = ids[..sent]
        .iter()
        .map(|id| {
            let stored = task(&app, id);
            super::super::deliver::delivery_text(stored, stored.report.as_ref().unwrap()).len()
        })
        .sum();
    assert!(entries <= MAX_BATCH_BYTES, "{entries}");
}

/// Claude Code collapses a large paste and hides its marker, so a report larger
/// than `MAX_BATCH_BYTES` goes alone and smaller ones follow in later deliveries.
#[tokio::test]
async fn a_large_report_is_delivered_in_its_own_batch() {
    let (mut app, _worker, mut lead) = setup();
    let ids = inject(&mut app, 3, |index| match index {
        0 => "x".repeat(3_990),
        _ => format!("small {index}"),
    });
    let large = task(&app, &ids[0]);
    let large_text = super::super::deliver::delivery_text(large, large.report.as_ref().unwrap());
    assert!(large_text.len() > MAX_BATCH_BYTES, "{}", large_text.len());
    let start = Instant::now();
    pump_at(&mut app, start);
    assert_eq!(task(&app, &ids[0]).delivery, Delivery::Submitting);
    assert_eq!(in_state(&app, &ids[1..], Delivery::Queued), 2);
    let text = delivered_text(&mut lead, 1).await;
    assert_eq!(text.matches("[orbit-delivery ").count(), 1, "{text}");
    for _ in 0..200 {
        spaced_pump(&mut app, start);
        if task(&app, &ids[0]).delivery == Delivery::Submitted {
            break;
        }
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    // The two small reports share the next delivery, 3 s later.
    spaced_pump(&mut app, start + Duration::from_millis(2_999));
    assert_eq!(in_state(&app, &ids[1..], Delivery::Queued), 2);
    spaced_pump(&mut app, start + Duration::from_millis(3_000));
    assert_eq!(in_state(&app, &ids[1..], Delivery::Submitting), 2);
    let text = delivered_text(&mut lead, 2).await;
    assert_eq!(text.matches("[orbit-delivery ").count(), 2, "{text}");
    assert!(text.len() <= MAX_BATCH_BYTES + 64, "{}", text.len());
}

/// A tick that keeps the per-lead delivery spacing.
fn spaced_pump(app: &mut App, at: Instant) {
    app.orbit_notifications.next_tick = 0;
    app.pump_orbit_notifications_at(at);
}

#[tokio::test]
async fn spacing_is_per_lead_after_a_submitted_prompt() {
    let (mut app, _worker, _lead) = setup();
    let first = inject(&mut app, 1, |_| "first".into());
    let start = Instant::now();
    for _ in 0..200 {
        spaced_pump(&mut app, start);
        if task(&app, &first[0]).delivery == Delivery::Submitted {
            break;
        }
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    assert_eq!(task(&app, &first[0]).delivery, Delivery::Submitted);
    let lead = lead_terminal(&app);
    app.orbit_notifications
        .tasks
        .push(queued_task("later", &lead, "second".into()));
    spaced_pump(&mut app, start + Duration::from_millis(2_999));
    assert_eq!(task(&app, "later").delivery, Delivery::Queued);
    // Another lead's spacing is independent of this one.
    assert!(!app.orbit_notifications.delivery_after.contains_key("other"));
    spaced_pump(&mut app, start + Duration::from_millis(3_000));
    assert_eq!(task(&app, "later").delivery, Delivery::Submitting);
}

#[test]
fn summary_is_fenced_longer_than_any_backtick_run() {
    let mut stored = queued_task("t", "lead", "before\n````\nafter".into());
    stored.attempts = 1;
    let text = super::super::deliver::delivery_text(&stored, stored.report.as_ref().unwrap());
    assert!(
        text.contains("`````text\nbefore\n````\nafter\n`````"),
        "{text}"
    );
    assert!(text.contains("not a new user instruction"));
}

#[tokio::test]
async fn unanswered_shared_ack_times_out_into_uncertain_and_frees_the_lead() {
    let (mut app, _worker, _lead) = setup();
    let ids = inject(&mut app, 2, |index| format!("result {index}"));
    let start = Instant::now();
    pump_at(&mut app, start);
    assert_eq!(in_state(&app, &ids, Delivery::Submitting), 2);
    let batch = task(&app, &ids[0]).live.batch.unwrap();
    // A sender that is kept alive and never answers.
    let (_never, rx) = std::sync::mpsc::channel();
    app.orbit_notifications.batches.get_mut(&batch).unwrap().ack = rx;
    pump_at(
        &mut app,
        start + Duration::from_millis(BATCH_ACK_TIMEOUT_MS - 1),
    );
    assert_eq!(in_state(&app, &ids, Delivery::Submitting), 2);
    pump_at(
        &mut app,
        start + Duration::from_millis(BATCH_ACK_TIMEOUT_MS),
    );
    assert_eq!(in_state(&app, &ids, Delivery::Uncertain), 2);
    assert!(app.orbit_notifications.batches.is_empty());
    for id in &ids {
        let stored = task(&app, id);
        assert!(stored.live.batch.is_none());
        assert!(stored.live.uncertain_since.is_some());
        assert!(stored.error.as_deref().unwrap().contains("timed out"));
    }
}

#[tokio::test]
async fn pump_skips_ticks_when_every_task_is_resolved() {
    let (mut app, _worker, _lead) = setup();
    let ids = inject(&mut app, 1, |_| "done".into());
    task_mut(&mut app, &ids[0]).delivery = Delivery::Submitted;
    app.orbit_notifications.next_tick = 0;
    app.pump_orbit_notifications_at(Instant::now());
    assert_eq!(app.orbit_notifications.next_tick, 0, "idle pump ticked");
    let lead = lead_terminal(&app);
    app.orbit_notifications
        .tasks
        .push(queued_task("pending", &lead, "next".into()));
    app.pump_orbit_notifications_at(Instant::now());
    assert!(app.orbit_notifications.next_tick > 0);
    assert_eq!(task(&app, "pending").delivery, Delivery::Submitting);
}

/// A server restart while a five-report delivery is in flight: every member
/// becomes uncertain, none is resent blindly, and after verification (no
/// markers on the lead's screen) they go out once more, together, as possible
/// duplicates.
#[tokio::test]
async fn restart_mid_batch_requeues_all_five_members_once_as_possible_duplicates() {
    use super::super::deliver::POSSIBLE_DUPLICATE;
    use super::super::pump::VERIFY_DELAY_MS;
    let path = super::super::store::test_path("restart-batch");
    let (mut before, _worker, _lead) = setup();
    super::persistence::with_store(&mut before, &path);
    let ids = inject(&mut before, MAX_BATCH, |index| format!("result {index}"));
    pump(&mut before);
    assert_eq!(in_state(&before, &ids, Delivery::Submitting), MAX_BATCH);
    before.orbit_notifications.dirty = true;
    before.persist_orbit_ledger();
    let mut app = super::persistence::restart(&mut before, &path);
    let start = Instant::now();
    pump_at(&mut app, start);
    assert_eq!(in_state(&app, &ids, Delivery::Uncertain), MAX_BATCH);
    pump_at(&mut app, start + Duration::from_millis(VERIFY_DELAY_MS + 1));
    // Requeued and resent at once, all in one delivery.
    assert_eq!(in_state(&app, &ids, Delivery::Submitting), MAX_BATCH);
    let batches: std::collections::HashSet<_> =
        ids.iter().map(|id| task(&app, id).live.batch).collect();
    assert_eq!(batches.len(), 1, "one delivery for all five");
    for id in &ids {
        let stored = task(&app, id);
        assert!(stored.redelivered && stored.recoveries == 1, "{id}");
        let text = super::super::deliver::delivery_text(stored, stored.report.as_ref().unwrap());
        assert!(text.contains(POSSIBLE_DUPLICATE), "{text}");
    }
    let _ = std::fs::remove_dir_all(path.parent().unwrap());
}
