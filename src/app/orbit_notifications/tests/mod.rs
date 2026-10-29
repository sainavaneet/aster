//! App-level ledger tests on the in-process harness: two Claude panes in the
//! Polaris workspace, the lead in tab 0 and a worker named "reviewer" in tab 1.
use std::time::{Duration, Instant};

use super::state::{Delivery, ReportOrigin, Submission, Task};
use super::*;
use crate::api::schema::{OrbitAgentAssignParams, OrbitAgentReportParams, OrbitOutcome, Request};
use crate::detect::{Agent, AgentState};
use crate::workspace::Workspace;

mod approvals;
mod batching;
mod blocked;
mod deadlines;
mod decisions;
mod delivery;
mod facts;
mod gate;
mod human;
mod late;
mod lead;
mod lifecycle;
mod parallel;
mod persistence;
mod reports;
mod security;
mod tolerance;

type Output = tokio::sync::mpsc::Receiver<bytes::Bytes>;

fn setup() -> (App, Output, Output) {
    let (_, rx) = tokio::sync::mpsc::unbounded_channel();
    let mut app = App::new(
        &crate::config::Config::default(),
        crate::app::AppPolicy::TEST,
        None,
        rx,
        crate::api::EventHub::default(),
    );
    let mut workspace = Workspace::test_new("notifications");
    workspace.custom_name = Some("Polaris".into());
    workspace.tabs[0].set_custom_name("Polaris".into());
    app.state.workspaces.push(workspace);
    app.state.switch_workspace(0);
    app.prepare_orbit_startup();
    assert!(
        app.orbit_binding_error.is_none(),
        "{:?}",
        app.orbit_binding_error
    );
    app.state.ensure_test_terminals();
    let mut receivers = Vec::new();
    for (tab, name) in [(0, "polaris"), (1, "reviewer")] {
        let ws = &app.state.workspaces[0];
        let terminal_id = ws.terminal_id(ws.tabs[tab].root_pane).unwrap().clone();
        set_screen(&mut app, terminal_id.as_str(), AgentState::Idle, true);
        let terminal = app.state.terminals.get_mut(&terminal_id).unwrap();
        terminal.set_agent_name(name.into());
        let (runtime, receiver) =
            crate::terminal::TerminalRuntime::test_with_channel_capacity(80, 24, 8);
        app.terminal_runtimes.insert(terminal_id, runtime);
        receivers.push(receiver);
    }
    let worker = receivers.pop().unwrap();
    let lead = receivers.pop().unwrap();
    (app, worker, lead)
}

fn assign_params(timeout_ms: Option<u64>) -> OrbitAgentAssignParams {
    OrbitAgentAssignParams {
        name: "reviewer".into(),
        prompt: "Review only".into(),
        timeout_ms,
        role: None,
        kind: None,
        parallel: None,
    }
}

fn try_assign(app: &mut App, timeout_ms: Option<u64>) -> serde_json::Value {
    serde_json::from_str(&app.handle_orbit_agent_assign("assign".into(), assign_params(timeout_ms)))
        .unwrap()
}

async fn assign_with(app: &mut App, timeout_ms: Option<u64>) -> OrbitAgentReportParams {
    let response = try_assign(app, timeout_ms);
    assert!(response.get("error").is_none(), "{response}");
    let task_id = response["result"]["task_id"].as_str().unwrap().to_owned();
    for _ in 0..100 {
        let now = app.orbit_notifications.now_ms(Instant::now());
        app.orbit_notifications.reap_acks(now);
        if task(app, &task_id).submission != Submission::Pending {
            break;
        }
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    assert_eq!(task(app, &task_id).submission, Submission::Submitted);
    OrbitAgentReportParams {
        token: app.orbit_notifications.last_issued_token.clone(),
        ..OrbitAgentReportParams::new(
            task_id,
            response["result"]["worker_id"].as_str().unwrap().into(),
            OrbitOutcome::Completed,
            "Review complete; no issues".into(),
        )
    }
}

async fn assign(app: &mut App) -> OrbitAgentReportParams {
    assign_with(app, None).await
}

fn report(app: &mut App, report: OrbitAgentReportParams) -> serde_json::Value {
    serde_json::from_str(&app.handle_orbit_agent_report("report".into(), report)).unwrap()
}

fn api(app: &mut App, method: &str, params: serde_json::Value) -> serde_json::Value {
    let request: Request =
        serde_json::from_value(serde_json::json!({"id":"api","method":method,"params":params}))
            .unwrap();
    serde_json::from_str(&app.handle_api_request(request)).unwrap()
}

fn task<'a>(app: &'a App, task_id: &str) -> &'a Task {
    let notifications = &app.orbit_notifications;
    &notifications.tasks[notifications.find(task_id).expect("task present")]
}

fn task_mut<'a>(app: &'a mut App, task_id: &str) -> &'a mut Task {
    let index = app.orbit_notifications.find(task_id).expect("task present");
    app.orbit_notifications.task_mut(index)
}

/// One tick at `now`, ignoring tick and per-lead delivery spacing.
fn pump_at(app: &mut App, now: Instant) {
    app.orbit_notifications.next_tick = 0;
    app.orbit_notifications.delivery_after.clear();
    app.pump_orbit_notifications_at(now);
}

fn pump(app: &mut App) {
    pump_at(app, Instant::now());
}

/// Detected state as a Claude manifest would report it: idle shows the prompt.
fn set_state(app: &mut App, terminal_id: &str, state: AgentState) {
    set_screen(app, terminal_id, state, state == AgentState::Idle);
}

/// Detected state with an explicit visible-idle screen signal.
fn set_screen(app: &mut App, terminal_id: &str, state: AgentState, visible_idle: bool) {
    let terminal = app.state.terminals.get_mut(terminal_id).unwrap();
    terminal.set_detected_state_with_screen_signals_at(
        Some(Agent::Claude),
        state,
        false,
        visible_idle,
        false,
        false,
        Instant::now(),
    );
    assert_eq!(terminal.visible_idle(), visible_idle);
}

/// Client keystrokes on the terminal at `at`, as `note_interactive_input` records them.
fn type_into(app: &mut App, terminal_id: &str, at: Instant) {
    app.state
        .terminals
        .get_mut(terminal_id)
        .unwrap()
        .record_user_input_at(at);
}

/// Client input bytes at `at`, as the headless input paths record them.
fn type_bytes(app: &mut App, terminal_id: &str, bytes: &[u8], at: Instant) {
    let terminal = app.state.terminals.get_mut(terminal_id).unwrap();
    terminal.record_user_input_at(at);
    terminal.note_client_input_bytes(bytes);
}

fn lead_id(app: &App) -> String {
    app.orbit_notifications
        .lead_terminal_id
        .clone()
        .expect("lead bound")
}

fn drain(output: &mut Output) -> String {
    let mut bytes = Vec::new();
    while let Ok(chunk) = output.try_recv() {
        bytes.extend_from_slice(&chunk);
    }
    String::from_utf8_lossy(&bytes).into_owned()
}

/// PTY output polled every 5 ms for up to 2 s until `done` holds for all of
/// it so far; returns what arrived. Replaces fixed sleeps before reading output.
async fn drain_until(output: &mut Output, done: impl Fn(&str) -> bool) -> String {
    let mut text = String::new();
    for _ in 0..400 {
        text.push_str(&drain(output));
        if done(&text) {
            return text;
        }
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    text
}

/// Pumps until the task reaches `expected`, letting test PTY ACK threads finish.
async fn pump_until(app: &mut App, task_id: &str, expected: Delivery) {
    for _ in 0..200 {
        pump(app);
        if task(app, task_id).delivery == expected {
            return;
        }
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    assert_eq!(task(app, task_id).delivery, expected);
}

/// A failed PTY ACK as if the delivery write was partial.
fn fail_delivery_ack(app: &mut App, task_id: &str, attempts: u32) {
    let (tx, rx) = std::sync::mpsc::channel();
    let lead = task(app, task_id).lead_id.clone();
    let now = app.orbit_notifications.now_ms(Instant::now());
    let batch = app.orbit_notifications.start_batch(lead, rx, now);
    let task = task_mut(app, task_id);
    task.delivery = Delivery::Submitting;
    task.attempts = attempts;
    task.live.batch = Some(batch);
    tx.send(Err(std::io::Error::other("partial write")))
        .unwrap();
}

#[tokio::test]
async fn orbit_notification_defers_busy_and_blocked_then_submits_once_without_focus_change() {
    let (mut app, mut worker, mut lead) = setup();
    let focused = app.state.workspaces[0].focused_pane_id();
    let result = assign(&mut app).await;
    let assignment = worker.try_recv().unwrap();
    let text = String::from_utf8_lossy(&assignment);
    assert!(text.contains("agent-report"));
    assert!(text.contains("\"tests_run\":false"), "{text}");
    assert!(
        text.contains("Set tests_run to true only if you ran"),
        "{text}"
    );
    assert!(text.contains(&result.task_id));
    assert!(!text.contains("ASTER_SOCKET_PATH"), "{text}");
    assert!(report(&mut app, result.clone()).get("error").is_none());
    let lead_id = task(&app, &result.task_id).lead_id.clone();
    for state in [AgentState::Working, AgentState::Blocked] {
        set_state(&mut app, &lead_id, state);
        pump(&mut app);
        assert!(lead.try_recv().is_err());
        assert_eq!(task(&app, &result.task_id).delivery, Delivery::Queued);
        // A busy lead is a wait, not a delivery attempt.
        assert_eq!(task(&app, &result.task_id).attempts, 0);
    }
    set_state(&mut app, &lead_id, AgentState::Idle);
    // Rename does not redirect the captured lead identity.
    app.state
        .terminals
        .get_mut(lead_id.as_str())
        .unwrap()
        .set_agent_name("renamed-lead".into());
    pump(&mut app);
    let message = tokio::time::timeout(Duration::from_secs(1), lead.recv())
        .await
        .unwrap()
        .unwrap();
    assert!(String::from_utf8_lossy(&message).contains("Review complete"));
    pump_until(&mut app, &result.task_id, Delivery::Submitted).await;
    // The validated submission sends pasted text and Enter separately.
    let delivered = String::from_utf8_lossy(&message).into_owned() + &drain(&mut lead);
    assert_eq!(delivered.matches("Orbit worker report").count(), 1);
    assert!(delivered.contains(r#""origin":"worker""#));
    assert!(report(&mut app, result).get("error").is_none());
    pump(&mut app);
    assert!(lead.try_recv().is_err());
    assert_eq!(app.state.workspaces[0].focused_pane_id(), focused);
}

#[tokio::test]
async fn orbit_notification_rejects_wrong_identity_conflicts_and_unknown_assignments() {
    let (mut app, _worker, _lead) = setup();
    let result = assign(&mut app).await;
    let mut wrong = result.clone();
    wrong.worker_id = "another-worker".into();
    assert!(report(&mut app, wrong).get("error").is_some());
    assert!(report(&mut app, result.clone()).get("error").is_none());
    let mut conflict = result.clone();
    conflict.summary = "different".into();
    assert!(report(&mut app, conflict).get("error").is_some());
    let mut unknown = result;
    unknown.task_id = "unknown".into();
    let response = report(&mut app, unknown);
    assert_eq!(response["error"]["message"], super::ledger::REPORT_REJECTED);
}

#[tokio::test]
async fn orbit_notification_missing_lead_retains_result_and_failed_ack_is_not_retried() {
    let (mut app, _worker, mut lead) = setup();
    let result = assign(&mut app).await;
    assert!(report(&mut app, result.clone()).get("error").is_none());
    let lead_id = task(&app, &result.task_id).lead_id.clone();
    let runtime_key = app
        .state
        .terminals
        .get_key_value(lead_id.as_str())
        .unwrap()
        .0
        .clone();
    let runtime = app.terminal_runtimes.remove(&runtime_key).unwrap();
    pump(&mut app);
    assert_eq!(task(&app, &result.task_id).delivery, Delivery::Queued);
    app.terminal_runtimes.insert(runtime_key, runtime);
    let (terminal_key, terminal) = app.state.terminals.remove_entry(lead_id.as_str()).unwrap();
    pump(&mut app);
    assert_eq!(task(&app, &result.task_id).delivery, Delivery::Queued);
    app.state.terminals.insert(terminal_key, terminal);
    fail_delivery_ack(&mut app, &result.task_id, 1);
    pump(&mut app);
    assert_eq!(task(&app, &result.task_id).delivery, Delivery::Uncertain);
    pump(&mut app);
    assert!(lead.try_recv().is_err());
}

#[test]
fn server_origin_marker_is_explicit_in_delivered_text() {
    let report = OrbitAgentReportParams::new(
        "task-1".into(),
        "worker".into(),
        OrbitOutcome::Failed,
        "Worker worker was lost before reporting".into(),
    );
    let mut notice = Task {
        task_id: "task-1/server-notice".into(),
        worker_id: "worker".into(),
        lead_id: "lead".into(),
        submission: Submission::Submitted,
        delivery: Delivery::Queued,
        report: Some(report.clone()),
        origin: Some(ReportOrigin::Server),
        notice_for: Some("task-1".into()),
        error: None,
        created_at: 0,
        updated_at: 0,
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
        live: Default::default(),
    };
    let text = super::deliver::delivery_text(&notice, &report);
    assert!(text.starts_with("[orbit-delivery task-1/server-notice #1]"));
    assert!(text.contains("origin: server"));
    assert!(text.contains(r#""origin":"server""#));
    notice.origin = Some(ReportOrigin::Worker);
    let text = super::deliver::delivery_text(&notice, &report);
    assert!(text.contains("worker-provided data"));
    assert!(text.contains("```text\nWorker worker was lost before reporting\n```"));
}

#[test]
fn ledger_clock_is_monotonic_follows_the_wall_after_sleep_and_maps_back_exactly() {
    let base = Instant::now();
    let mut clock = Clock::new(base, 1_000_000);
    // Driven by `Instant` while the wall clock agrees.
    assert_eq!(
        clock.now_ms(base + Duration::from_secs(5), 1_000_000),
        1_005_000
    );
    // Small drift is not a sleep: time keeps following the instant.
    assert_eq!(
        clock.now_ms(base + Duration::from_secs(5), 1_005_000 + CLOCK_JUMP_MS),
        1_005_000
    );
    // A monotonic clock that paused during sleep: follow the wall clock.
    let after_sleep = base + Duration::from_secs(6);
    let now = clock.now_ms(after_sleep, 1_000_000 + 3_600_000);
    assert_eq!(now, 1_000_000 + 3_600_000);
    // Gate windows stay on the same clock: the timestamp maps back to the instant.
    assert_eq!(clock.instant(now), after_sleep);
    assert_eq!(
        clock.now_ms(after_sleep + Duration::from_secs(1), 0),
        now + 1_000
    );
    // A wall clock stepping backwards or an earlier instant never moves time back.
    assert_eq!(clock.now_ms(base, 0), now + 1_000);
}

#[tokio::test]
async fn flow_step_projection_resolves_parallel_current_roster_and_missing_panes() {
    let (mut app, _worker_rx, _lead_rx) = setup();
    app.ensure_orbit_team_loaded();
    let lead = app.orbit_lead_terminal().unwrap().terminal_id;
    let worker = app.state.workspaces[0]
        .terminal_id(app.state.workspaces[0].tabs[1].root_pane)
        .unwrap()
        .to_string();
    app.orbit_team.roster.roles.push(
        serde_json::from_value(serde_json::json!({
            "name": "design", "members": [
                {"name":"old", "kind":"codex", "terminal_id": lead},
                {"name":"gone", "kind":"codex", "terminal_id":"missing-terminal"}
            ]
        }))
        .unwrap(),
    );
    app.orbit_notifications.tasks.push(
        serde_json::from_value(serde_json::json!({
            "task_id":"projection", "worker_id":worker, "lead_id":lead,
            "submission":"submitted", "delivery":"awaiting_report", "error":null,
            "created_at":1000,
            "steps":[
                {"index":0,"member":"parallel","kind":"codex","state":"done",
                 "parallel":{"prompt":"p","files":[],"phase":"running","worker_id":lead}},
                {"index":1,"member":"current","kind":"codex","state":"active"},
                {"index":2,"member":"old","kind":"codex","state":"done"},
                {"index":3,"member":"gone","kind":"codex","state":"done"},
                {"index":4,"member":"unknown","kind":"codex","state":"active",
             "parallel":{"prompt":"p","files":[],"phase":"running"}}
            ]
        }))
        .unwrap(),
    );
    app.refresh_orbit_shell_view(Instant::now() + Duration::from_secs(1));
    let view = app.orbit_shell_view().unwrap();
    let steps = &view
        .active_tasks
        .iter()
        .find(|task| task.task_id == "projection")
        .unwrap()
        .steps;
    assert_eq!(steps[0].worker_terminal_id.as_deref(), Some(lead.as_str()));
    assert_eq!(steps[0].pane_id, view.lead.pane_id);
    assert_eq!(
        steps[1].worker_terminal_id.as_deref(),
        Some(worker.as_str())
    );
    assert!(steps[1].pane_id.is_some());
    assert_ne!(steps[1].pane_id, steps[0].pane_id);
    assert_eq!(steps[2].worker_terminal_id.as_deref(), Some(lead.as_str()));
    assert_eq!(steps[2].pane_id, view.lead.pane_id);
    assert_eq!(
        steps[3].worker_terminal_id.as_deref(),
        Some("missing-terminal")
    );
    assert!(steps[3].pane_id.is_none());
    assert!(steps[4].worker_terminal_id.is_none() && steps[4].pane_id.is_none());
}
