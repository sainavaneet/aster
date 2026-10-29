//! A worker blocked on a prompt: one relay to Polaris and one human notification
//! per blocked episode after `BLOCKED_NOTICE_MS`, and activity rows.
use super::super::lifecycle::{prompt_excerpt, BLOCKED_NOTICE_MS, STARTUP_GRACE_MS};
use super::super::redact::redact_secrets;
use super::*;
use crate::api::schema::OrbitOutcome;
use crate::config::{OrbitNotifyEvent, ToastDelivery};

const AFTER_GRACE: Duration = Duration::from_millis(STARTUP_GRACE_MS + 1_000);
const BLOCKED: Duration = Duration::from_millis(BLOCKED_NOTICE_MS);

fn blocked_notices(app: &App, task_id: &str) -> Vec<String> {
    let prefix = format!("{task_id}/blocked-");
    app.orbit_notifications
        .tasks
        .iter()
        .filter(|task| task.task_id.starts_with(&prefix))
        .map(|task| task.task_id.clone())
        .collect()
}

#[tokio::test]
async fn blocked_worker_is_relayed_and_notified_once_per_episode() {
    let (mut app, _worker, mut lead) = setup();
    app.state.toast_config.delivery = ToastDelivery::Aster;
    let result = assign(&mut app).await;
    // A busy lead keeps notices queued while time jumps ahead.
    let lead_terminal = lead_id(&app);
    set_state(&mut app, &lead_terminal, AgentState::Working);
    let start = Instant::now() + AFTER_GRACE;
    set_state(&mut app, &result.worker_id, AgentState::Blocked);
    pump_at(&mut app, start);
    pump_at(&mut app, start + BLOCKED - Duration::from_millis(300));
    assert!(blocked_notices(&app, &result.task_id).is_empty());
    let activity = app.orbit_task_activity();
    let rows = activity["blocked_workers"].as_array().unwrap();
    assert_eq!(rows.len(), 1, "{activity}");
    assert_eq!(rows[0]["name"], "reviewer");
    assert_eq!(rows[0]["task_id"], result.task_id.as_str());
    assert!(rows[0]["pane_id"].is_string());
    assert!(rows[0]["since_ms"].as_u64().unwrap() > 0);

    pump_at(&mut app, start + BLOCKED);
    let notices = blocked_notices(&app, &result.task_id);
    assert_eq!(notices.len(), 1);
    let stored = task(&app, &notices[0]);
    assert_eq!(stored.origin, Some(ReportOrigin::Server));
    assert_eq!(stored.notice_for.as_deref(), Some(result.task_id.as_str()));
    let report = stored.report.as_ref().unwrap();
    assert_eq!(report.outcome, OrbitOutcome::Blocked);
    let pane = rows[0]["pane_id"].as_str().unwrap();
    assert!(report.summary.starts_with(&format!(
        "Worker reviewer ({pane}) is blocked on an approval prompt for task {}.",
        result.task_id
    )));
    assert!(report.summary.contains("orbit_agent_approve/deny"));
    // The assignment stays open; the worker can still report.
    assert_eq!(
        task(&app, &result.task_id).delivery,
        Delivery::AwaitingReport
    );
    let human = app.take_orbit_human_notices();
    assert_eq!(human.len(), 1, "{human:?}");
    assert_eq!(human[0].event, OrbitNotifyEvent::Blocked);
    assert_eq!(human[0].title, "Orbit worker reviewer is blocked");
    assert!(human[0].body.contains(&result.task_id));

    // Still the same episode: nothing more.
    pump_at(&mut app, start + BLOCKED * 3);
    assert_eq!(blocked_notices(&app, &result.task_id).len(), 1);
    assert!(app.take_orbit_human_notices().is_empty());
    // A new episode is relayed again, and the user is notified again for it.
    let resumed = start + BLOCKED * 3 + Duration::from_secs(1);
    set_state(&mut app, &result.worker_id, AgentState::Working);
    pump_at(&mut app, resumed);
    set_state(&mut app, &result.worker_id, AgentState::Blocked);
    pump_at(&mut app, resumed + Duration::from_secs(1));
    pump_at(&mut app, resumed + Duration::from_secs(1) + BLOCKED);
    assert_eq!(blocked_notices(&app, &result.task_id).len(), 2);
    let human = app.take_orbit_human_notices();
    assert_eq!(human.len(), 1, "{human:?}");
    assert_eq!(human[0].event, OrbitNotifyEvent::Blocked);
    pump_at(&mut app, resumed + Duration::from_secs(1) + BLOCKED * 2);
    assert!(app.take_orbit_human_notices().is_empty());

    set_state(&mut app, &lead_terminal, AgentState::Idle);
    let first = notices[0].clone();
    pump_until(&mut app, &first, Delivery::Submitted).await;
    let text = drain(&mut lead);
    assert!(text.contains("origin: server"), "{text}");
    assert!(text.contains("waiting on a prompt"), "{text}");
    assert!(text.contains("still open"), "{text}");
}

#[tokio::test]
async fn blocked_worker_without_an_open_assignment_is_listed_but_not_relayed() {
    let (mut app, _worker, _lead) = setup();
    let worker_id = {
        let ws = &app.state.workspaces[0];
        ws.terminal_id(ws.tabs[1].root_pane).unwrap().to_string()
    };
    set_state(&mut app, &worker_id, AgentState::Blocked);
    let start = Instant::now() + AFTER_GRACE;
    pump_at(&mut app, start);
    pump_at(&mut app, start + BLOCKED * 2);
    assert!(app.orbit_notifications.tasks.is_empty());
    let activity = app.orbit_task_activity();
    let rows = activity["blocked_workers"].as_array().unwrap();
    assert_eq!(rows.len(), 1, "{activity}");
    assert!(rows[0]["task_id"].is_null());
    // Without an assignment the tick tracks no episode.
    assert!(rows[0]["since_ms"].is_null(), "{activity}");
    assert!(app.orbit_notifications.blocked.is_empty());
    // The lead, outside the Orbit tab, is never listed as a blocked worker.
    let lead = {
        let ws = &app.state.workspaces[0];
        ws.terminal_id(ws.tabs[0].root_pane).unwrap().to_string()
    };
    set_state(&mut app, &lead, AgentState::Blocked);
    pump_at(&mut app, start + BLOCKED * 3);
    let activity = app.orbit_task_activity();
    assert_eq!(activity["blocked_workers"].as_array().unwrap().len(), 1);
}

#[test]
fn prompt_excerpt_keeps_the_question_and_options_bounded_and_clean() {
    let screen = "older output?\n\
        ╭────────────────────────────╮\n\
        │ Bash command               │\n\
        │   rm -rf build\u{1b}[31m          │\n\
        │ Do you want to proceed?    │\n\
        │ ❯ 1. Yes                   │\n\
        │   2. Yes, and don't ask again │\n\
        │   3. No, and tell Claude what to do differently (esc) │\n\
        ╰────────────────────────────╯\n\
        Esc to cancel\n";
    let excerpt = prompt_excerpt(screen).unwrap();
    assert_eq!(
        excerpt,
        "Do you want to proceed? / ❯ 1. Yes / 2. Yes, and don't ask again / 3. No, and tell Claude what to do differently (esc)"
    );
    assert!(prompt_excerpt("no question here\n").is_none());
    let token = "0123456789abcdef0123456789abcdef";
    let quoted = format!("Run ASTER_ORBIT_TOKEN={token} aster orbit agent-report?\n");
    let redacted = prompt_excerpt(&quoted).unwrap();
    assert!(!redacted.contains(token), "{redacted}");
    assert_eq!(
        redacted,
        "Run ASTER_ORBIT_TOKEN=<redacted> aster orbit agent-report?"
    );
    let long = format!("{}?\n", "x".repeat(400));
    let bounded = prompt_excerpt(&long).unwrap();
    assert_eq!(bounded.chars().count(), 200);
    assert!(bounded.chars().all(|ch| !ch.is_control()));
}

#[test]
fn prompt_excerpt_drops_codex_command_bodies_and_redacts_secrets() {
    let secret = "sk-abcDEF0123456789xyzSECRET";
    let screen = format!(
        "Would you like to run the following command?\n\
         \n\
         $ curl -H 'Authorization: Bearer ghp_TOKENVALUE123' -d key={secret} https://api.example\n\
         \n\
         › 1. Yes, proceed (y)\n\
           2. Yes, and don't ask again for curl {secret} (a)\n\
           3. No, and tell Codex what to do differently (esc)\n\
         \n\
         Press enter to confirm or esc to cancel\n"
    );
    let excerpt = prompt_excerpt(&screen).unwrap();
    assert!(!excerpt.contains("curl -H"), "{excerpt}");
    assert!(!excerpt.contains("abcDEF"), "{excerpt}");
    assert!(!excerpt.contains("TOKENVALUE"), "{excerpt}");
    assert_eq!(
        excerpt,
        "Would you like to run the following command? / › 1. Yes, proceed (y) / 2. Yes, and don't ask again for curl <redacted> (a) / 3. No, and tell Codex what to do differently (esc)"
    );
    assert!(excerpt.chars().count() <= 200);
    for (text, expected) in [
        ("Use Bearer abc.def.ghi now", "Use Bearer <redacted> now"),
        (
            "key -----BEGIN RSA PRIVATE KEY----- MIIE\nnext",
            "key <redacted>\nnext",
        ),
        ("Allow AKIAIOSFODNN7EXAMPLE?", "Allow <redacted>"),
        ("Post to xoxb-1234-abcd now", "Post to <redacted> now"),
        ("Clone gho_16C7e42F292c6912E7 now", "Clone <redacted> now"),
    ] {
        assert_eq!(redact_secrets(text), expected, "{text}");
    }
    // Words that merely contain a prefix are kept.
    assert_eq!(
        prompt_excerpt("Run the task-runner for ask-me?\n").unwrap(),
        "Run the task-runner for ask-me?"
    );
}

#[test]
fn prompt_excerpt_redacts_a_token_wrapped_across_two_lines() {
    let token = "0123456789abcdef0123456789abcdef";
    let (head, tail) = token.split_at(20);
    let screen = format!(
        "│ Allow ASTER_ORBIT_TOKEN={head} │\n│   {tail} for this task? │\n│ ❯ 1. Yes │\n│   2. No │\n"
    );
    let excerpt = prompt_excerpt(&screen).unwrap();
    assert!(!excerpt.contains(head), "{excerpt}");
    assert!(!excerpt.contains(tail), "{excerpt}");
    assert_eq!(excerpt, "<redacted> for this task? / ❯ 1. Yes / 2. No");
    // A short hex word next to a line break is left alone.
    assert_eq!(prompt_excerpt("Add a bad cafe\nface?\n").unwrap(), "face?");
}

fn lead_pane(app: &App) -> String {
    let ws = &app.state.workspaces[0];
    ws.terminal_id(ws.tabs[0].root_pane).unwrap().to_string()
}

/// The Orbit worker pane, recorded as a worker Orbit started.
fn record_started_worker(app: &mut App) -> String {
    let worker_id = {
        let ws = &app.state.workspaces[0];
        ws.terminal_id(ws.tabs[1].root_pane).unwrap().to_string()
    };
    app.orbit_workers.insert(
        worker_id.clone(),
        crate::app::api::orbit_lifecycle::WorkerRecord {
            name: "reviewer".into(),
            kind: "claude".into(),
            model: None,
            permission_mode: Some("manual"),
            worktree: None,
            exited: false,
        },
    );
    worker_id
}

fn worker_notices(app: &App, worker_id: &str) -> Vec<String> {
    let prefix = format!("worker-{worker_id}-");
    app.orbit_notifications
        .tasks
        .iter()
        .filter(|task| task.task_id.starts_with(&prefix))
        .map(|task| task.task_id.clone())
        .collect()
}

#[tokio::test]
async fn started_worker_blocked_without_an_assignment_is_relayed_once_per_episode() {
    let (mut app, _worker, mut lead) = setup();
    app.state.toast_config.delivery = ToastDelivery::Aster;
    let worker_id = record_started_worker(&mut app);
    let lead_terminal = lead_pane(&app);
    set_state(&mut app, &lead_terminal, AgentState::Working);
    let start = Instant::now() + AFTER_GRACE;
    set_state(&mut app, &worker_id, AgentState::Blocked);
    pump_at(&mut app, start);
    pump_at(&mut app, start + BLOCKED - Duration::from_millis(300));
    assert!(worker_notices(&app, &worker_id).is_empty());
    let activity = app.orbit_task_activity();
    let rows = activity["blocked_workers"].as_array().unwrap();
    assert_eq!(rows.len(), 1, "{activity}");
    assert!(rows[0]["task_id"].is_null());
    let since = rows[0]["since_ms"].as_u64().expect("episode tracked");
    let pane = rows[0]["pane_id"].as_str().unwrap().to_owned();

    pump_at(&mut app, start + BLOCKED);
    let notices = worker_notices(&app, &worker_id);
    let notice_id = format!("worker-{worker_id}-{since}/server-notice");
    assert_eq!(notices.len(), 1);
    assert_eq!(notices[0], notice_id);
    let stored = task(&app, &notice_id);
    assert_eq!(stored.origin, Some(ReportOrigin::Server));
    assert_eq!(stored.notice_for, None);
    assert_eq!(stored.worker_id, worker_id);
    assert_eq!(stored.delivery, Delivery::Queued);
    assert!(!stored.awaits_worker());
    let report = stored.report.as_ref().unwrap();
    assert_eq!(report.task_id, notice_id);
    assert_eq!(report.outcome, OrbitOutcome::Blocked);
    assert!(
        report.summary.starts_with(&format!(
            "Worker reviewer ({pane}) is blocked on a dialog and has no assignment: "
        )),
        "{}",
        report.summary
    );
    assert!(report.summary.ends_with(
        "use orbit_agent_approve/deny only if the lead is allowed to and it is a permission prompt."
    ));
    let human = app.take_orbit_human_notices();
    assert_eq!(human.len(), 1, "{human:?}");
    assert_eq!(human[0].event, OrbitNotifyEvent::Blocked);
    assert_eq!(human[0].title, "Orbit worker reviewer is blocked");
    assert!(human[0].body.contains("no assignment"), "{}", human[0].body);
    // The notice is inspectable like any task.
    let status = api(
        &mut app,
        "orbit.task_status",
        serde_json::json!({"task_id": notice_id}),
    );
    assert_eq!(
        status["result"]["task"]["notice_for"],
        serde_json::Value::Null
    );

    // Still the same episode: nothing more.
    pump_at(&mut app, start + BLOCKED * 3);
    assert_eq!(worker_notices(&app, &worker_id).len(), 1);
    assert!(app.take_orbit_human_notices().is_empty());
    // Leaving Blocked ends the episode; the next one is relayed and notified again.
    let resumed = start + BLOCKED * 3 + Duration::from_secs(1);
    set_state(&mut app, &worker_id, AgentState::Working);
    pump_at(&mut app, resumed);
    set_state(&mut app, &worker_id, AgentState::Blocked);
    pump_at(&mut app, resumed + Duration::from_secs(1));
    pump_at(&mut app, resumed + Duration::from_secs(1) + BLOCKED);
    assert_eq!(worker_notices(&app, &worker_id).len(), 2);
    assert_eq!(app.take_orbit_human_notices().len(), 1);

    set_state(&mut app, &lead_terminal, AgentState::Idle);
    pump_until(&mut app, &notice_id, Delivery::Submitted).await;
    let text = drain(&mut lead);
    assert!(text.contains("origin: server"), "{text}");
    assert!(text.contains("no assignment"), "{text}");
    assert!(!text.contains("still open"), "{text}");
}

#[tokio::test]
async fn exited_or_unbound_started_workers_are_not_relayed() {
    let (mut app, _worker, _lead) = setup();
    let worker_id = record_started_worker(&mut app);
    app.orbit_workers.get_mut(&worker_id).unwrap().exited = true;
    set_state(&mut app, &worker_id, AgentState::Blocked);
    let start = Instant::now() + AFTER_GRACE;
    pump_at(&mut app, start);
    pump_at(&mut app, start + BLOCKED * 2);
    assert!(app.orbit_notifications.blocked.is_empty());
    assert!(worker_notices(&app, &worker_id).is_empty());
}

#[tokio::test]
async fn worker_notice_without_notice_for_survives_a_restart() {
    use super::persistence::{restart, saved, with_store};
    let path = super::super::store::test_path("worker-notice");
    let (mut app, _worker, _lead) = setup();
    with_store(&mut app, &path);
    let worker_id = record_started_worker(&mut app);
    let lead_terminal = lead_pane(&app);
    set_state(&mut app, &lead_terminal, AgentState::Working);
    set_state(&mut app, &worker_id, AgentState::Blocked);
    let start = Instant::now() + AFTER_GRACE;
    pump_at(&mut app, start);
    pump_at(&mut app, start + BLOCKED);
    let notice_id = worker_notices(&app, &worker_id)
        .pop()
        .expect("notice queued");
    let file = saved(&path);
    let entry = &file["tasks"][0];
    assert_eq!(entry["task_id"], notice_id.as_str());
    assert!(entry["notice_for"].is_null(), "{entry}");
    let mut restarted = restart(&mut app, &path);
    pump(&mut restarted);
    let loaded = task(&restarted, &notice_id);
    assert_eq!(loaded.notice_for, None);
    assert_eq!(loaded.origin, Some(ReportOrigin::Server));
    assert_eq!(loaded.delivery, Delivery::Queued);
    assert_eq!(loaded.report.as_ref().unwrap().task_id, notice_id);
    let _ = std::fs::remove_dir_all(path.parent().unwrap());
}

/// 100 blocked episodes while the lead never takes a delivery: server notices
/// count toward the ledger's capacity, the oldest queued notice gives way, the
/// open assignment is never evicted, and the drops are reported.
#[tokio::test]
async fn a_hundred_blocked_episodes_never_grow_the_ledger_past_capacity() {
    let (mut app, _worker, _lead) = setup();
    let result = assign(&mut app).await;
    let lead_terminal = lead_id(&app);
    set_state(&mut app, &lead_terminal, AgentState::Working);
    let mut at = Instant::now() + AFTER_GRACE;
    for _ in 0..100 {
        set_state(&mut app, &result.worker_id, AgentState::Blocked);
        pump_at(&mut app, at);
        at += BLOCKED;
        pump_at(&mut app, at);
        assert!(app.orbit_notifications.tasks.len() <= CAPACITY);
        set_state(&mut app, &result.worker_id, AgentState::Working);
        at += Duration::from_secs(1);
        pump_at(&mut app, at);
        at += Duration::from_secs(1);
    }
    let notifications = &app.orbit_notifications;
    assert_eq!(notifications.tasks.len(), CAPACITY);
    assert_eq!(
        task(&app, &result.task_id).delivery,
        Delivery::AwaitingReport,
        "the assignment itself is never dropped"
    );
    let notices = blocked_notices(&app, &result.task_id);
    assert_eq!(notices.len(), CAPACITY - 1);
    assert_eq!(
        app.orbit_notifications.dropped_notices,
        100 - (CAPACITY as u64 - 1)
    );
    let activity = app.orbit_task_activity();
    let warning = activity["ledger_error"].as_str().unwrap();
    assert!(warning.contains("server notices were dropped"), "{warning}");
}
