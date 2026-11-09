//! Worker loss, timeout, and nonterminal idle reporting warnings.
use super::super::lifecycle::{NUDGE_AFTER, STARTUP_GRACE_MS};
use super::*;

const AFTER_GRACE: Duration = Duration::from_millis(STARTUP_GRACE_MS + 1_000);

fn notice_id(task_id: &str) -> String {
    format!("{task_id}/server-notice")
}

async fn assert_notice_delivered(app: &mut App, lead: &mut Output, task_id: &str, summary: &str) {
    let notice = notice_id(task_id);
    let stored = task(app, &notice);
    assert_eq!(stored.origin, Some(ReportOrigin::Server));
    assert_eq!(stored.notice_for.as_deref(), Some(task_id));
    assert!(stored.report.as_ref().unwrap().summary.contains(summary));
    pump_until(app, &notice, Delivery::Submitted).await;
    let text = drain(lead);
    assert!(text.contains("origin: server"), "{text}");
    assert!(text.contains(summary), "{text}");
}

#[tokio::test]
async fn worker_runtime_loss_resolves_task_and_notifies_lead() {
    let (mut app, _worker, mut lead) = setup();
    let result = assign(&mut app).await;
    let key = app
        .state
        .terminals
        .get_key_value(result.worker_id.as_str())
        .unwrap()
        .0
        .clone();
    app.terminal_runtimes.remove(&key);
    let start = Instant::now();
    pump_at(&mut app, start);
    assert_eq!(
        task(&app, &result.task_id).delivery,
        Delivery::AwaitingReport,
        "grace"
    );
    pump_at(&mut app, start + AFTER_GRACE);
    assert_eq!(task(&app, &result.task_id).delivery, Delivery::WorkerLost);
    let summary = format!("Worker {} was lost before reporting", result.worker_id);
    assert_notice_delivered(&mut app, &mut lead, &result.task_id, &summary).await;
    // The worker's report still arrives: it is delivered once, marked late.
    let late = report(&mut app, result.clone());
    assert!(late.get("error").is_none(), "{late}");
    assert_eq!(late["result"]["late"], true);
    assert_eq!(late["result"]["late_from"], "worker_lost");
    pump_until(&mut app, &result.task_id, Delivery::Submitted).await;
    let text = drain(&mut lead);
    let expected = super::super::deliver::late_line(Delivery::WorkerLost);
    assert_eq!(text.matches(&expected).count(), 1, "{text}");
    assert_eq!(text.matches("Orbit worker report").count(), 1, "{text}");
}

#[tokio::test]
async fn worker_that_stops_hosting_an_agent_is_lost() {
    let (mut app, _worker, _lead) = setup();
    let result = assign(&mut app).await;
    let terminal = app
        .state
        .terminals
        .get_mut(result.worker_id.as_str())
        .unwrap();
    terminal.clear_agent_name();
    terminal.set_detected_state(None, AgentState::Unknown);
    pump_at(&mut app, Instant::now() + AFTER_GRACE);
    assert_eq!(task(&app, &result.task_id).delivery, Delivery::WorkerLost);
}

#[tokio::test]
async fn idle_worker_warns_once_keeps_assignment_open_and_accepts_report() {
    let (mut app, mut worker, mut lead) = setup();
    let result = assign(&mut app).await;
    drain(&mut worker);
    let start = Instant::now() + AFTER_GRACE;
    pump_at(&mut app, start);
    pump_at(&mut app, start + NUDGE_AFTER - Duration::from_secs(1));
    assert!(task(&app, &result.task_id).nudged_at.is_none());
    let nudge_at = start + NUDGE_AFTER;
    pump_at(&mut app, nudge_at);
    assert!(task(&app, &result.task_id).nudged_at.is_some());
    tokio::time::sleep(Duration::from_millis(50)).await;
    let reminder = drain(&mut worker);
    assert!(reminder.contains("reminder"), "{reminder}");
    assert!(reminder.contains("agent-report"));
    assert!(reminder.contains(&result.task_id));
    assert!(reminder.contains("tests_run"), "{reminder}");
    assert!(reminder.contains("blocked_on"), "{reminder}");
    // Idleness restarts after the reminder; no second reminder is sent.
    let resumed = nudge_at + Duration::from_secs(1);
    pump_at(&mut app, resumed);
    pump_at(&mut app, resumed + NUDGE_AFTER - Duration::from_secs(1));
    assert_eq!(
        task(&app, &result.task_id).delivery,
        Delivery::AwaitingReport
    );
    pump_at(&mut app, resumed + NUDGE_AFTER);
    assert_eq!(
        task(&app, &result.task_id).delivery,
        Delivery::AwaitingReport
    );
    tokio::time::sleep(Duration::from_millis(50)).await;
    assert!(!drain(&mut worker).contains("reminder"));
    let notice = format!("{}/idle-notice", result.task_id);
    assert_eq!(task(&app, &notice).origin, Some(ReportOrigin::Server));
    pump_until(&mut app, &notice, Delivery::Submitted).await;
    let text = drain(&mut lead);
    assert!(
        text.contains("Background work may still be running"),
        "{text}"
    );
    assert!(text.contains("not evidence that the job failed"), "{text}");
    assert!(!text.contains("worker is waiting on a prompt"), "{text}");
    let count = app.orbit_notifications.tasks.len();
    pump_at(&mut app, resumed + NUDGE_AFTER * 3);
    assert_eq!(app.orbit_notifications.tasks.len(), count);
    assert_eq!(
        task(&app, &result.task_id).delivery,
        Delivery::AwaitingReport
    );
    assert!(task(&app, &result.task_id).error.is_none());
    // A restored runtime still deduplicates against the persisted notice.
    let index = app.orbit_notifications.find(&result.task_id).unwrap();
    app.orbit_notifications.tasks[index].live.idle_notice_sent = false;
    pump_at(&mut app, resumed + NUDGE_AFTER * 4);
    assert_eq!(app.orbit_notifications.tasks.len(), count);
    assert!(app.orbit_notifications.tasks[index].live.idle_notice_sent);
    let response = report(&mut app, result.clone());
    assert!(response.get("error").is_none(), "{response}");
    assert_ne!(response["result"]["late"], true);
    pump_until(&mut app, &result.task_id, Delivery::Submitted).await;
}

#[tokio::test]
async fn busy_worker_is_never_reminded() {
    let (mut app, mut worker, _lead) = setup();
    let result = assign(&mut app).await;
    drain(&mut worker);
    set_state(&mut app, &result.worker_id, AgentState::Working);
    let start = Instant::now() + AFTER_GRACE;
    pump_at(&mut app, start);
    pump_at(&mut app, start + NUDGE_AFTER * 3);
    assert!(task(&app, &result.task_id).nudged_at.is_none());
    assert_eq!(
        task(&app, &result.task_id).delivery,
        Delivery::AwaitingReport
    );
}

#[tokio::test]
async fn assignment_times_out_and_notifies_lead() {
    let (mut app, _worker, mut lead) = setup();
    let result = assign_with(&mut app, Some(1_000)).await;
    set_state(&mut app, &result.worker_id, AgentState::Working);
    pump_at(&mut app, Instant::now() + AFTER_GRACE);
    assert_eq!(task(&app, &result.task_id).delivery, Delivery::TimedOut);
    let summary = "did not report before the assignment timeout";
    assert_notice_delivered(&mut app, &mut lead, &result.task_id, summary).await;
}

#[tokio::test]
async fn idle_warning_does_not_disable_assignment_deadline() {
    let (mut app, _worker, mut lead) = setup();
    let result = assign_with(&mut app, Some(20 * 60 * 1_000)).await;
    let start = Instant::now() + AFTER_GRACE;
    pump_at(&mut app, start);
    pump_at(&mut app, start + NUDGE_AFTER);
    pump_at(&mut app, start + NUDGE_AFTER + Duration::from_secs(1));
    pump_at(&mut app, start + NUDGE_AFTER * 2 + Duration::from_secs(1));
    assert_eq!(
        task(&app, &result.task_id).delivery,
        Delivery::AwaitingReport
    );
    assert!(app
        .orbit_notifications
        .find(&format!("{}/idle-notice", result.task_id))
        .is_some());
    let warning = format!("{}/idle-notice", result.task_id);
    pump_until(&mut app, &warning, Delivery::Submitted).await;
    drain(&mut lead);
    pump_at(&mut app, start + Duration::from_secs(21 * 60));
    assert_eq!(task(&app, &result.task_id).delivery, Delivery::TimedOut);
    assert_notice_delivered(
        &mut app,
        &mut lead,
        &result.task_id,
        "did not report before the assignment timeout",
    )
    .await;
}

#[tokio::test]
async fn assignment_timeout_is_bounded() {
    let (mut app, mut worker, _lead) = setup();
    for timeout in [0, super::super::assign::MAX_TIMEOUT_MS + 1] {
        let response = try_assign(&mut app, Some(timeout));
        assert!(response["error"]["message"]
            .as_str()
            .unwrap()
            .contains("timeout_ms"));
    }
    assert!(worker.try_recv().is_err());
    let result = assign(&mut app).await;
    let stored = task(&app, &result.task_id);
    assert_eq!(
        stored.deadline.unwrap() - stored.created_at,
        super::super::assign::DEFAULT_TIMEOUT_MS
    );
}
