//! Report-as-proof, cancellation, status, eviction, and tombstones.
use super::*;
use crate::app::orbit_notifications::state::LiveState;

fn resolved_task(task_id: &str, delivery: Delivery) -> Task {
    Task {
        task_id: task_id.into(),
        worker_id: "old-worker".into(),
        lead_id: "old-lead".into(),
        submission: Submission::Submitted,
        delivery,
        report: Some(OrbitAgentReportParams::new(
            task_id.into(),
            "old-worker".into(),
            OrbitOutcome::Completed,
            format!("done {task_id}"),
        )),
        origin: Some(ReportOrigin::Worker),
        notice_for: None,
        error: None,
        created_at: 1,
        updated_at: 2,
        title: None,
        attempts: 1,
        recoveries: 0,
        redelivered: false,
        nudged_at: None,
        deadline: None,
        late: false,
        steps: Vec::new(),
        timeout_ms: None,
        role: None,
        late_from: None,
        extra: Default::default(),
        token_hash: None,
        live: LiveState::default(),
    }
}

fn fill_ledger(app: &mut App, delivery_of: impl Fn(usize) -> Delivery) {
    for index in 0..CAPACITY {
        let task = resolved_task(&format!("filler-{index}"), delivery_of(index));
        app.orbit_notifications.tasks.push(task);
    }
}

#[tokio::test]
async fn report_after_assignment_ack_error_is_accepted_as_proof() {
    let (mut app, _worker, _lead) = setup();
    let result = assign(&mut app).await;
    let (tx, rx) = std::sync::mpsc::channel();
    let stored = task_mut(&mut app, &result.task_id);
    stored.submission = Submission::Pending;
    stored.live.submission_ack = Some(rx);
    tx.send(Err(std::io::Error::other("partial write")))
        .unwrap();
    pump(&mut app);
    assert_eq!(
        task(&app, &result.task_id).submission,
        Submission::Uncertain
    );
    let response = report(&mut app, result.clone());
    assert!(response.get("error").is_none(), "{response}");
    assert_eq!(response["result"]["submission"], "confirmed_by_report");
    let stored = task(&app, &result.task_id);
    assert_eq!(stored.submission, Submission::ConfirmedByReport);
    assert_eq!(stored.delivery, Delivery::Queued);
    assert_eq!(stored.origin, Some(ReportOrigin::Worker));
    // Identical retries remain idempotent after confirmation.
    assert!(report(&mut app, result).get("error").is_none());
}

#[tokio::test]
async fn cancel_rejects_later_report_and_is_idempotent() {
    let (mut app, _worker, _lead) = setup();
    let result = assign(&mut app).await;
    let cancelled = api(
        &mut app,
        "orbit.task_cancel",
        json!({"task_id":result.task_id}),
    );
    assert_eq!(cancelled["result"]["delivery"], "cancelled", "{cancelled}");
    let again = api(
        &mut app,
        "orbit.task_cancel",
        json!({"task_id":result.task_id}),
    );
    assert!(again.get("error").is_none(), "{again}");
    let response = report(&mut app, result.clone());
    assert!(response["error"]["message"]
        .as_str()
        .unwrap()
        .contains("cancelled"));
    let unknown = api(&mut app, "orbit.task_cancel", json!({"task_id":"nope"}));
    assert!(unknown.get("error").is_some());
}

#[tokio::test]
async fn cancel_open_tasks_for_worker_cancels_only_that_workers_open_tasks() {
    let (mut app, _worker, _lead) = setup();
    let open = assign(&mut app).await;
    app.orbit_notifications
        .tasks
        .push(resolved_task("done", Delivery::Submitted));
    let now = app.orbit_notifications.now_ms(Instant::now());
    let notifications = &mut app.orbit_notifications;
    assert!(notifications
        .cancel_open_tasks_for_worker("someone-else", now)
        .is_empty());
    assert_eq!(
        notifications.cancel_open_tasks_for_worker(&open.worker_id, now),
        vec![open.task_id.clone()]
    );
    assert_eq!(task(&app, &open.task_id).delivery, Delivery::Cancelled);
    assert_eq!(task(&app, "done").delivery, Delivery::Submitted);
    assert!(app
        .orbit_notifications
        .cancel_open_tasks_for_worker(&open.worker_id, now)
        .is_empty());
    let response = report(&mut app, open);
    assert!(response["error"]["message"]
        .as_str()
        .unwrap()
        .contains("cancelled"));
}

#[tokio::test]
async fn cancel_refuses_resolved_tasks() {
    let (mut app, _worker, _lead) = setup();
    app.orbit_notifications
        .tasks
        .push(resolved_task("done", Delivery::Submitted));
    let response = api(&mut app, "orbit.task_cancel", json!({"task_id":"done"}));
    assert!(response["error"]["message"]
        .as_str()
        .unwrap()
        .contains("already resolved as submitted"));
}

#[tokio::test]
async fn task_status_reports_timestamps_attempts_and_tombstones() {
    let (mut app, _worker, _lead) = setup();
    let result = assign_with(&mut app, Some(60_000)).await;
    let status = api(
        &mut app,
        "orbit.task_status",
        json!({"task_id":result.task_id}),
    );
    let task = &status["result"]["task"];
    assert_eq!(task["delivery"], "awaiting_report", "{status}");
    assert_eq!(task["attempts"], 0);
    let created = task["created_at"].as_u64().unwrap();
    assert!(created > 0);
    assert_eq!(task["deadline"].as_u64().unwrap(), created + 60_000);
    assert!(status["result"]["tombstone"].is_null());
    assert!(status["result"]["ledger_error"].is_null());
    let unknown = api(&mut app, "orbit.task_status", json!({"task_id":"nope"}));
    assert!(unknown.get("error").is_some());
}

#[tokio::test]
async fn eviction_takes_oldest_resolved_task_after_admission_and_keeps_tombstone() {
    let (mut app, mut worker, _lead) = setup();
    // filler-0 is active; filler-1 is the oldest resolved entry.
    fill_ledger(&mut app, |index| match index {
        0 => Delivery::AwaitingReport,
        _ => Delivery::Submitted,
    });
    // Legacy (tokenless) entries owned by the real, in-scope worker terminal.
    // filler-0 stays with another worker, so the reviewer is free to assign.
    let worker_id = app.resolve_agent_target("reviewer").unwrap().terminal_id;
    for task in app.orbit_notifications.tasks.iter_mut().skip(1) {
        task.worker_id = worker_id.clone();
        task.report.as_mut().unwrap().worker_id = worker_id.clone();
    }
    let rejected = try_assign(&mut app, Some(0));
    assert!(rejected.get("error").is_some());
    assert_eq!(
        app.orbit_notifications.tasks.len(),
        CAPACITY,
        "invalid assignment evicted"
    );
    let result = assign(&mut app).await;
    assert!(drain(&mut worker).contains(&result.task_id));
    let notifications = &app.orbit_notifications;
    assert_eq!(notifications.tasks.len(), CAPACITY);
    assert!(notifications.find("filler-0").is_some());
    assert!(notifications.find("filler-1").is_none());
    assert_eq!(notifications.tombstones.back().unwrap().task_id, "filler-1");
    let mut original = resolved_task("filler-1", Delivery::Submitted)
        .report
        .unwrap();
    original.worker_id = worker_id;
    assert!(report(&mut app, original.clone()).get("error").is_none());
    original.summary = "changed".into();
    let conflict = report(&mut app, original);
    assert!(conflict["error"]["message"]
        .as_str()
        .unwrap()
        .contains("already resolved"));
    let status = api(&mut app, "orbit.task_status", json!({"task_id":"filler-1"}));
    assert_eq!(status["result"]["tombstone"]["final_delivery"], "submitted");
}

#[tokio::test]
async fn full_ledger_of_active_tasks_rejects_without_prompting_worker() {
    let (mut app, mut worker, _lead) = setup();
    fill_ledger(&mut app, |_| Delivery::AwaitingReport);
    let response = try_assign(&mut app, None);
    let message = response["error"]["message"].as_str().unwrap();
    assert!(message.contains("ledger is full"), "{message}");
    assert!(
        message.contains("64 assignments are still active"),
        "{message}"
    );
    assert!(
        worker.try_recv().is_err(),
        "a rejected assignment reached the worker"
    );
}

#[test]
fn tombstone_ring_is_bounded() {
    let mut notifications = Notifications::default();
    for index in 0..(TOMBSTONE_CAPACITY + CAPACITY + 5) {
        notifications.insert(resolved_task(&format!("t{index}"), Delivery::Failed));
    }
    assert_eq!(notifications.tasks.len(), CAPACITY);
    assert_eq!(notifications.tombstones.len(), TOMBSTONE_CAPACITY);
    assert_eq!(notifications.tombstones.front().unwrap().task_id, "t5");
}

#[tokio::test]
async fn cancel_before_the_assignment_ack_still_reaps_the_submission() {
    let (mut app, _worker, _lead) = setup();
    let response = try_assign(&mut app, None);
    assert!(response.get("error").is_none(), "{response}");
    let task_id = response["result"]["task_id"].as_str().unwrap().to_owned();
    let cancelled = api(&mut app, "orbit.task_cancel", json!({"task_id":task_id}));
    assert_eq!(cancelled["result"]["delivery"], "cancelled", "{cancelled}");
    for _ in 0..100 {
        let now = app.orbit_notifications.now_ms(Instant::now());
        app.orbit_notifications.reap_acks(now);
        if task(&app, &task_id).submission != Submission::Pending {
            break;
        }
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    let stored = task(&app, &task_id);
    assert_eq!(stored.submission, Submission::Submitted);
    assert_eq!(stored.delivery, Delivery::Cancelled);
}

#[tokio::test]
async fn a_worker_with_an_assignment_awaiting_its_report_is_not_assigned_again() {
    let (mut app, mut worker, _lead) = setup();
    let first = assign(&mut app).await;
    drain(&mut worker);
    let refused = try_assign(&mut app, None);
    assert_eq!(
        refused["error"]["code"], "orbit_worker_assigned",
        "{refused}"
    );
    assert!(refused["error"]["message"]
        .as_str()
        .unwrap()
        .contains(&first.task_id));
    tokio::time::sleep(Duration::from_millis(20)).await;
    assert_eq!(
        drain(&mut worker),
        "",
        "a refused assignment prompted the worker"
    );
    assert_eq!(app.orbit_notifications.tasks.len(), 1);
    // Once the worker reports, or the lead cancels, it can be assigned again.
    assert!(report(&mut app, first).get("error").is_none());
    let second = assign(&mut app).await;
    let cancelled = api(
        &mut app,
        "orbit.task_cancel",
        json!({"task_id": second.task_id}),
    );
    assert!(cancelled.get("error").is_none(), "{cancelled}");
    assert!(try_assign(&mut app, None).get("error").is_none());
}

/// Cancel after the worker's report was accepted (finding 15): the cancel is
/// accepted today and the report is not delivered; an identical retry gets the
/// stored task back, now `cancelled`. This documents the current behaviour.
#[tokio::test]
async fn cancelling_a_reported_task_drops_its_queued_or_in_flight_delivery() {
    let (mut app, mut worker, mut lead) = setup();
    let queued = assign(&mut app).await;
    drain(&mut worker);
    let lead_terminal = lead_id(&app);
    set_state(&mut app, &lead_terminal, AgentState::Working);
    assert!(report(&mut app, queued.clone()).get("error").is_none());
    assert_eq!(task(&app, &queued.task_id).delivery, Delivery::Queued);
    let cancelled = api(
        &mut app,
        "orbit.task_cancel",
        json!({"task_id": queued.task_id}),
    );
    assert_eq!(cancelled["result"]["delivery"], "cancelled", "{cancelled}");
    let retry = report(&mut app, queued.clone());
    assert!(
        retry.get("error").is_none(),
        "identical retry is the stored report"
    );
    // In flight: the shared ACK still settles, but the member stays cancelled.
    let submitting = assign(&mut app).await;
    assert!(report(&mut app, submitting.clone()).get("error").is_none());
    set_state(&mut app, &lead_terminal, AgentState::Idle);
    pump(&mut app);
    assert_eq!(
        task(&app, &submitting.task_id).delivery,
        Delivery::Submitting
    );
    let cancelled = api(
        &mut app,
        "orbit.task_cancel",
        json!({"task_id": submitting.task_id}),
    );
    assert_eq!(cancelled["result"]["delivery"], "cancelled", "{cancelled}");
    for _ in 0..100 {
        pump(&mut app);
        if app.orbit_notifications.batches.is_empty() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    assert_eq!(
        task(&app, &submitting.task_id).delivery,
        Delivery::Cancelled
    );
    assert_eq!(task(&app, &queued.task_id).delivery, Delivery::Cancelled);
    // The bytes already written for the in-flight one reached the lead once;
    // the queued one never did.
    let text = drain(&mut lead);
    assert!(!text.contains(&queued.task_id), "{text}");
}
