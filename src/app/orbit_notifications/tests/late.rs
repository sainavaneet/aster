//! Late worker reports: a valid report for an assignment Aster already resolved
//! as timed out, unreported, or lost is delivered once with a late line, also
//! after eviction to a tombstone. Cancelled assignments still refuse reports.
use super::super::deliver::late_line;
use super::super::lifecycle::STARTUP_GRACE_MS;
use super::super::state::{LiveState, Tombstone};
use super::*;

const AFTER_GRACE: Duration = Duration::from_millis(STARTUP_GRACE_MS + 1_000);
const CANCELLED: &str =
    "Assignment was cancelled by Polaris; stop this task and do not retry the report";

/// A task that timed out, its server notice already delivered and the lead drained.
async fn timed_out(app: &mut App, lead: &mut Output) -> OrbitAgentReportParams {
    let result = assign_with(app, Some(1_000)).await;
    set_state(app, &result.worker_id, AgentState::Working);
    pump_at(app, Instant::now() + AFTER_GRACE);
    assert_eq!(task(app, &result.task_id).delivery, Delivery::TimedOut);
    let notice = format!("{}/server-notice", result.task_id);
    pump_until(app, &notice, Delivery::Submitted).await;
    drain(lead);
    result
}

/// Moves the resolved task to the tombstone ring, as eviction does.
fn evict(app: &mut App, task_id: &str) {
    let notifications = &mut app.orbit_notifications;
    let index = notifications.find(task_id).unwrap();
    let evicted = notifications.tasks.remove(index);
    notifications
        .tombstones
        .push_back(Tombstone::from_task(&evicted));
}

fn filler(index: usize, delivery: Delivery, origin: Option<ReportOrigin>) -> Task {
    Task {
        task_id: format!("filler-{index}"),
        worker_id: "old-worker".into(),
        lead_id: "old-lead".into(),
        submission: Submission::Submitted,
        delivery,
        report: origin.map(|_| {
            OrbitAgentReportParams::new(
                format!("filler-{index}"),
                "old-worker".into(),
                OrbitOutcome::Blocked,
                "filler".into(),
            )
        }),
        origin,
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
        steps: Vec::new(),
        timeout_ms: None,
        role: None,
        late_from: None,
        extra: Default::default(),
        token_hash: None,
        live: LiveState::default(),
    }
}

async fn delivered_once(app: &mut App, lead: &mut Output, task_id: &str, from: Delivery) {
    pump_until(app, task_id, Delivery::Submitted).await;
    let text = drain(lead);
    let marker = format!("[orbit-delivery {task_id} #1]\n{}\n", late_line(from));
    assert_eq!(text.matches(&marker).count(), 1, "{text}");
    assert_eq!(text.matches("Orbit worker report").count(), 1, "{text}");
    assert!(text.contains("Review complete"), "{text}");
}

#[tokio::test]
async fn late_report_after_timeout_is_delivered_once_with_the_late_line() {
    let (mut app, _worker, mut lead) = setup();
    let result = timed_out(&mut app, &mut lead).await;
    let response = report(&mut app, result.clone());
    assert!(response.get("error").is_none(), "{response}");
    assert_eq!(response["result"]["late"], true);
    assert_eq!(response["result"]["late_from"], "timed_out");
    assert_eq!(response["result"]["delivery"], "queued");
    delivered_once(&mut app, &mut lead, &result.task_id, Delivery::TimedOut).await;
    // An identical retry is recognised and never delivered again.
    let retry = report(&mut app, result.clone());
    assert!(retry.get("error").is_none(), "{retry}");
    pump(&mut app);
    tokio::time::sleep(Duration::from_millis(20)).await;
    assert!(drain(&mut lead).is_empty());
    let mut conflict = result;
    conflict.summary = "something else".into();
    assert!(report(&mut app, conflict).get("error").is_some());
}

#[tokio::test]
async fn late_report_after_unreported_is_accepted() {
    let (mut app, _worker, mut lead) = setup();
    let result = assign(&mut app).await;
    let now = app.orbit_notifications.now_ms(Instant::now());
    assert!(task_mut(&mut app, &result.task_id).set_delivery(Delivery::Unreported, now, "test"));
    let response = report(&mut app, result.clone());
    assert_eq!(response["result"]["late_from"], "unreported", "{response}");
    delivered_once(&mut app, &mut lead, &result.task_id, Delivery::Unreported).await;
}

#[tokio::test]
async fn late_report_against_a_tombstone_restores_the_task_and_delivers_once() {
    let (mut app, _worker, mut lead) = setup();
    let result = timed_out(&mut app, &mut lead).await;
    evict(&mut app, &result.task_id);
    let response = report(&mut app, result.clone());
    assert!(response.get("error").is_none(), "{response}");
    assert_eq!(response["result"]["late"], true);
    let notifications = &app.orbit_notifications;
    assert!(notifications
        .tombstones
        .iter()
        .all(|entry| entry.task_id != result.task_id));
    delivered_once(&mut app, &mut lead, &result.task_id, Delivery::TimedOut).await;
    assert!(report(&mut app, result).get("error").is_none());
    pump(&mut app);
    tokio::time::sleep(Duration::from_millis(20)).await;
    assert!(drain(&mut lead).is_empty(), "retry delivered again");
}

#[tokio::test]
async fn late_report_against_a_tombstone_in_a_full_ledger_uses_a_notice_or_asks_to_retry() {
    let (mut app, _worker, mut lead) = setup();
    let result = timed_out(&mut app, &mut lead).await;
    evict(&mut app, &result.task_id);
    app.orbit_notifications.tasks.clear();
    for index in 0..CAPACITY {
        let filler = filler(index, Delivery::AwaitingReport, None);
        app.orbit_notifications.tasks.push(filler);
    }
    // Full of active assignments: refused visibly, nothing recorded.
    let response = report(&mut app, result.clone());
    let message = response["error"]["message"].as_str().unwrap_or_default();
    assert!(message.contains("ledger is full"), "{response}");
    assert!(app
        .orbit_notifications
        .tombstones
        .back()
        .unwrap()
        .report
        .is_none());
    // One slot holds a queued notice: it gives way to the late report.
    app.orbit_notifications.tasks[0] = filler(0, Delivery::Queued, Some(ReportOrigin::Server));
    app.orbit_notifications.tasks[0].live.retry_at = u64::MAX;
    let response = report(&mut app, result.clone());
    assert!(response.get("error").is_none(), "{response}");
    let notice = format!("{}/late-report", result.task_id);
    assert_eq!(app.orbit_notifications.tasks.len(), CAPACITY);
    assert!(app.orbit_notifications.find("filler-0").is_none());
    assert_eq!(task(&app, &notice).origin, Some(ReportOrigin::Server));
    pump_until(&mut app, &notice, Delivery::Submitted).await;
    let text = drain(&mut lead);
    assert!(
        text.starts_with(&format!(
            "[orbit-delivery {notice} #1]\n{}\n",
            late_line(Delivery::TimedOut)
        )),
        "{text}"
    );
    assert!(text.contains("worker-provided data"), "{text}");
    // The tombstone keeps the report, so a retry is not delivered again.
    assert!(report(&mut app, result).get("error").is_none());
    assert_eq!(
        app.orbit_notifications
            .tasks
            .iter()
            .filter(|task| task.task_id.ends_with("/late-report"))
            .count(),
        1
    );
    let activity = api(&mut app, "orbit.activity", json!({}));
    let warning = activity["result"]["ledger_error"].as_str().unwrap();
    assert!(warning.contains("server notices were dropped"), "{warning}");
}

#[tokio::test]
async fn cancelled_assignment_still_rejects_reports_live_and_as_tombstone() {
    let (mut app, _worker, _lead) = setup();
    let result = assign(&mut app).await;
    let cancel = api(
        &mut app,
        "orbit.task_cancel",
        json!({"task_id": result.task_id}),
    );
    assert!(cancel.get("error").is_none(), "{cancel}");
    let response = report(&mut app, result.clone());
    assert_eq!(response["error"]["message"], CANCELLED, "{response}");
    evict(&mut app, &result.task_id);
    let response = report(&mut app, result);
    assert_eq!(response["error"]["message"], CANCELLED, "{response}");
}
