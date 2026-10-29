//! The Orbit section of the client shell snapshot: roles, scratch workers,
//! assignments, and when the view is rebuilt.
use std::time::{Duration, Instant};

use serde_json::json;

use super::*;
use crate::api::schema::{OrbitAgentReportParams, OrbitOutcome};
use crate::app::api::orbit_lifecycle::WorkerRecord;
use crate::protocol::OrbitShellView;

const TICK: Duration = Duration::from_millis(300);

struct Staffed {
    app: App,
    scratch: String,
    reviewer_claude: String,
    tester_claude: String,
}

/// Two roles with three ready members, plus a scratch worker in the Orbit tab.
fn staffed() -> Staffed {
    let mut app = team_app();
    let mut queues = add_role(
        &mut app,
        spec("reviewer", &[("rc", "claude"), ("rx", "codex")]),
    );
    queues.extend(add_role(&mut app, spec("tester", &[("tc", "claude")])));
    app.supervise_orbit_team(Instant::now());
    for queue in &mut queues {
        drain(queue);
    }
    let reviewer_claude = ready_member(&mut app, "reviewer", "rc");
    ready_member(&mut app, "reviewer", "rx");
    let tester_claude = ready_member(&mut app, "tester", "tc");

    let (scratch, ws, pane) = app.orbit_tab_terminals().remove(0);
    let scratch_id = app.state.workspaces[ws].terminal_id(pane).unwrap().clone();
    let terminal = app.state.terminals.get_mut(scratch.as_str()).unwrap();
    terminal.restore_managed_agent("scratcher".into(), Agent::Claude);
    terminal.set_detected_state(Some(Agent::Claude), AgentState::Idle);
    let (runtime, queue) = crate::terminal::TerminalRuntime::test_with_channel_capacity(80, 24, 16);
    app.terminal_runtimes.insert(scratch_id, runtime);
    std::mem::forget(queue);
    app.orbit_workers.insert(
        scratch.clone(),
        WorkerRecord {
            name: "scratcher".into(),
            kind: "claude".into(),
            model: None,
            permission_mode: None,
            worktree: None,
            exited: false,
        },
    );
    Staffed {
        app,
        scratch,
        reviewer_claude,
        tester_claude,
    }
}

fn assign(app: &mut App, params: serde_json::Value) -> String {
    let response = call(app, "orbit.agent_assign", params);
    assert!(response.get("error").is_none(), "{response}");
    response["result"]["task_id"].as_str().unwrap().to_owned()
}

fn view(app: &mut App) -> OrbitShellView {
    app.refresh_orbit_shell_view(Instant::now());
    app.orbit_shell_view().expect("lead present").clone()
}

#[tokio::test]
async fn team_scratch_worker_and_three_tasks_project_the_expected_view() {
    let Staffed {
        mut app,
        scratch,
        reviewer_claude,
        tester_claude,
    } = staffed();
    let long_title = format!("Ünïcödé {}", "tèst ".repeat(20));
    let first = assign(
        &mut app,
        json!({"role": "reviewer", "kind": "claude", "prompt": "Review the parser\nSECRET BODY LINE"}),
    );
    let second = assign(&mut app, json!({"role": "tester", "prompt": long_title}));
    let third = assign(&mut app, json!({"name": "scratcher", "prompt": "Probe it"}));
    let report = OrbitAgentReportParams {
        token: app.orbit_last_issued_token(),
        ..OrbitAgentReportParams::new(
            third.clone(),
            scratch.clone(),
            OrbitOutcome::Completed,
            format!("Done: {}", "ñ".repeat(120)),
        )
    };
    let reported: serde_json::Value =
        serde_json::from_str(&app.handle_orbit_agent_report("report".into(), report)).unwrap();
    assert!(reported.get("error").is_none(), "{reported}");
    app.state
        .terminals
        .get_mut(scratch.as_str())
        .unwrap()
        .set_detected_state(Some(Agent::Claude), AgentState::Working);
    app.state
        .terminals
        .get_mut(tester_claude.as_str())
        .unwrap()
        .set_detected_state(Some(Agent::Claude), AgentState::Blocked);

    let view = view(&mut app);

    assert_eq!(view.lead.name, "polaris");
    assert_eq!(view.lead.state, "idle");
    assert!(view.lead.pane_id.is_some());
    let roles: Vec<_> = view
        .roles
        .iter()
        .map(|role| {
            let members: Vec<_> = role
                .members
                .iter()
                .map(|member| {
                    (
                        member.name.as_str(),
                        member.kind.as_str(),
                        member.state.as_str(),
                    )
                })
                .collect();
            (role.name.as_str(), role.pinned, members)
        })
        .collect();
    assert_eq!(
        roles,
        vec![
            (
                "reviewer",
                false,
                vec![("rc", "claude", "idle"), ("rx", "codex", "idle")]
            ),
            ("tester", false, vec![("tc", "claude", "blocked")]),
        ]
    );
    assert!(view.roles[0].members[0].pane_id.is_some());
    assert_eq!(
        view.roles[0].members[0].terminal_id.as_deref(),
        Some(reviewer_claude.as_str())
    );
    assert_eq!(view.scratch.len(), 1);
    assert_eq!(view.scratch[0].name, "scratcher");
    assert_eq!(
        view.scratch[0].terminal_id.as_deref(),
        Some(scratch.as_str())
    );
    assert_eq!(view.scratch[0].state, "working");

    let active: Vec<_> = view
        .active_tasks
        .iter()
        .map(|task| (task.task_id.as_str(), task.role.as_deref()))
        .collect();
    assert_eq!(
        active,
        vec![
            (first.as_str(), Some("reviewer")),
            (second.as_str(), Some("tester"))
        ]
    );
    assert_eq!(view.active_tasks[0].title, "Review the parser");
    assert_eq!(view.active_tasks[0].worker_terminal_id, reviewer_claude);
    assert!(!view.active_tasks[0].blocked);
    // The prompt write is acknowledged in the background.
    assert!(
        ["assigning", "idle"].contains(&view.active_tasks[0].state.as_str()),
        "{:?}",
        view.active_tasks[0].state
    );
    assert!(view.active_tasks[1].blocked);
    assert!(!view.active_tasks[1].awaiting_approval);
    assert_eq!(view.active_tasks[1].title.chars().count(), 48);
    assert!(view.active_tasks[1].title.starts_with("Ünïcödé tèst"));
    assert!(view.active_tasks[1].title.ends_with('…'));
    assert!(view.active_tasks.iter().all(|task| task.created_at_ms > 0));

    assert_eq!(view.recent_tasks.len(), 1);
    assert_eq!(view.recent_tasks[0].task_id, third);
    assert_eq!(view.recent_tasks[0].outcome, "completed");
    assert_eq!(view.recent_tasks[0].summary.chars().count(), 80);
    assert!(view.recent_tasks[0].summary.starts_with("Done: ññ"));
    let json = serde_json::to_string(&view).unwrap();
    assert!(!json.contains("SECRET BODY LINE"), "{json}");
    assert!(!json.contains("Act as the"), "{json}");
}

#[tokio::test]
async fn labelled_secrets_never_reach_the_stored_title_or_the_view() {
    let mut app = staffed().app;
    let task_id = assign(
        &mut app,
        json!({"role": "reviewer", "kind": "claude", "prompt": "Rotate password: hunter2 on staging\nbody"}),
    );
    let prose = assign(
        &mut app,
        json!({"role": "tester", "prompt": "update the password reset flow"}),
    );

    assert_eq!(
        app.orbit_task_title(&task_id),
        Some("Rotate password: <redacted> on staging")
    );
    let view = view(&mut app);
    let titles: Vec<_> = view
        .active_tasks
        .iter()
        .map(|task| (task.task_id.as_str(), task.title.as_str()))
        .collect();
    assert!(titles.contains(&(task_id.as_str(), "Rotate password: <redacted> on staging")));
    assert!(titles.contains(&(prose.as_str(), "update the password reset flow")));
    let json = serde_json::to_string(&view).unwrap();
    assert!(!json.contains("hunter2"), "{json}");
}

#[test]
fn view_is_omitted_without_a_lead() {
    let mut app = new_app();
    app.state.workspaces.push(Workspace::test_new("plain"));
    app.state.ensure_test_terminals();

    assert!(!app.refresh_orbit_shell_view(Instant::now()));

    assert!(app.orbit_shell_view().is_none());
}

#[tokio::test]
async fn unchanged_tick_neither_rebuilds_early_nor_republishes() {
    let mut app = staffed().app;
    let start = Instant::now();

    assert!(app.refresh_orbit_shell_view(start), "first build publishes");
    let builds = app.orbit_shell_view_builds();
    assert!(
        !app.refresh_orbit_shell_view(start + Duration::from_millis(10)),
        "inside the interval"
    );
    assert_eq!(
        app.orbit_shell_view_builds(),
        builds,
        "no rebuild in-interval"
    );
    assert!(
        !app.refresh_orbit_shell_view(start + TICK),
        "same content is not republished"
    );
    assert_eq!(app.orbit_shell_view_builds(), builds + 1);

    let tc = app.orbit_team.roster.member("tc").unwrap().1.clone();
    let (ws, tab) = app
        .resolve_role_tab(&app.orbit_team.roster.role("tester").unwrap().clone())
        .unwrap();
    let (_, terminal_id) = app.orbit_member_pane(ws, tab, &tc.name).unwrap();
    app.state
        .terminals
        .get_mut(terminal_id.as_str())
        .unwrap()
        .set_detected_state(Some(Agent::Claude), AgentState::Working);
    assert!(
        app.refresh_orbit_shell_view(start + TICK * 2),
        "a state change is published"
    );
}

#[tokio::test]
async fn snapshots_stamp_the_ledger_clock_without_changing_the_cached_view() {
    let mut app = staffed().app;
    let task_id = assign(
        &mut app,
        json!({"role": "reviewer", "kind": "claude", "prompt": "Stamp me"}),
    );
    let cached = view(&mut app);
    assert_eq!(cached.server_now_ms, None, "the cache holds facts only");

    let stamped = app
        .orbit_shell_view_at(Instant::now())
        .expect("lead present");

    let now = stamped.server_now_ms.expect("snapshot carries the clock");
    let created = stamped
        .active_tasks
        .iter()
        .find(|task| task.task_id == task_id)
        .unwrap()
        .created_at_ms;
    assert!(now >= created, "ledger clock never runs behind its tasks");
    assert!(now - created < 60_000, "same clock as the task times");
    assert!(stamped.same_facts(&cached));
    assert_eq!(app.orbit_shell_view(), Some(&cached));
}
