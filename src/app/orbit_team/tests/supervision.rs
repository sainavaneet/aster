use std::time::{Duration, Instant};

use serde_json::json;

use super::super::supervisor::{MAX_RELAUNCHES, MEMBER_RELAUNCH_DELAY_MS, RELAUNCH_WINDOW};
use super::*;

const DELAY: Duration = Duration::from_millis(MEMBER_RELAUNCH_DELAY_MS);
const TICK: Duration = Duration::from_millis(300);

fn reviewer(app: &mut App) -> Vec<Queue> {
    add_role(app, spec("reviewer", &[("rc", "claude"), ("rx", "codex")]))
}

fn member_terminal(app: &App, member: &str) -> String {
    app.orbit_team
        .roster
        .member(member)
        .unwrap()
        .1
        .terminal_id
        .clone()
        .expect("member terminal recorded")
}

#[tokio::test]
async fn startup_launches_each_member_in_its_role_tab_with_its_role_text() {
    let mut app = team_app();
    let mut queues = reviewer(&mut app);
    let before = focus(&app);
    assert!(app.supervise_orbit_team(Instant::now()));
    let claude = drain(&mut queues[0]);
    assert!(
        claude.contains("claude") && claude.contains("--model") && claude.contains("rc"),
        "{claude}"
    );
    assert!(
        claude.contains("--append-system-prompt") && claude.contains("Reviewer role"),
        "{claude}"
    );
    assert!(!claude.contains("--resume"), "no session yet: {claude}");
    let codex = drain(&mut queues[1]);
    assert!(
        codex.contains("codex") && codex.contains("developer_instructions="),
        "{codex}"
    );
    assert_eq!(focus(&app), before);
    let rc = member_terminal(&app, "rc");
    assert_eq!(
        app.state.terminals[rc.as_str()].manual_label.as_deref(),
        Some("rc")
    );
    assert_eq!(
        app.state.terminals[rc.as_str()].agent_name.as_deref(),
        Some("rc")
    );
    assert_ne!(rc, member_terminal(&app, "rx"));
    app.state.assert_invariants_for_test();
}

#[tokio::test]
async fn a_member_already_hosted_in_its_tab_is_left_alone() {
    let mut app = team_app();
    let mut queues = reviewer(&mut app);
    let role = app.orbit_team.roster.role("reviewer").unwrap().clone();
    let (ws, tab) = app.resolve_role_tab(&role).unwrap();
    let pane = app.state.workspaces[ws].tabs[tab].layout.pane_ids()[0];
    let id = terminal(&app, ws, tab, pane);
    // A restored member: its name and kind came back with the pane.
    app.state
        .terminals
        .get_mut(id.as_str())
        .unwrap()
        .restore_managed_agent("rc".into(), Agent::Claude);
    app.supervise_orbit_team(Instant::now());
    assert_eq!(drain(&mut queues[0]), "", "restored member was relaunched");
    assert_eq!(member_terminal(&app, "rc"), id);
    assert!(drain(&mut queues[1]).contains("codex"));
}

#[tokio::test]
async fn the_observed_session_is_stored_and_resumed_by_the_next_launch() {
    let mut app = team_app();
    let mut queues = reviewer(&mut app);
    let now = Instant::now();
    app.supervise_orbit_team(now);
    drain(&mut queues[0]);
    let rc = ready_member(&mut app, "reviewer", "rc");
    app.state
        .terminals
        .get_mut(rc.as_str())
        .unwrap()
        .set_persisted_agent_session(crate::agent_resume::PersistedAgentSession {
            source: "aster:claude".into(),
            agent: "claude".into(),
            session_ref: crate::agent_resume::AgentSessionRef::id("session-7").unwrap(),
        });
    app.supervise_orbit_team(now + TICK);
    assert_eq!(
        app.orbit_team
            .roster
            .member("rc")
            .unwrap()
            .1
            .session
            .as_ref()
            .unwrap()
            .value,
        "session-7"
    );
    // Exits well after the launch, so the session is trusted for the relaunch.
    let later = now + Duration::from_secs(60);
    exit_member(&mut app, &rc);
    app.supervise_orbit_team(later);
    app.supervise_orbit_team(later + DELAY);
    let relaunch = drain(&mut queues[0]);
    assert!(relaunch.contains("--resume session-7"), "{relaunch}");
    assert!(relaunch.contains("--append-system-prompt"), "{relaunch}");
    assert_eq!(
        member_terminal(&app, "rc"),
        rc,
        "relaunched in the same terminal"
    );
}

#[tokio::test]
async fn an_exited_member_is_relaunched_after_the_delay_then_given_up_after_the_budget() {
    let mut app = team_app();
    app.state.toast_config.delivery = crate::config::ToastDelivery::Aster;
    let mut queues = reviewer(&mut app);
    let mut now = Instant::now();
    app.supervise_orbit_team(now);
    drain(&mut queues[0]);
    let rc = member_terminal(&app, "rc");
    for attempt in 0..MAX_RELAUNCHES {
        now += Duration::from_secs(30);
        exit_member(&mut app, &rc);
        app.supervise_orbit_team(now);
        assert_eq!(
            app.orbit_team_member_phase("rc", false),
            "relaunching",
            "{attempt}"
        );
        assert!(app.orbit_team_member_recovering(&rc));
        app.supervise_orbit_team(now + DELAY - TICK);
        assert_eq!(drain(&mut queues[0]), "", "relaunched before the delay");
        now += DELAY;
        app.supervise_orbit_team(now);
        assert!(
            drain(&mut queues[0]).contains("claude"),
            "attempt {attempt} not relaunched"
        );
    }
    now += Duration::from_secs(30);
    exit_member(&mut app, &rc);
    app.supervise_orbit_team(now);
    assert_eq!(app.orbit_team_member_phase("rc", false), "gave_up");
    assert!(!app.orbit_team_member_recovering(&rc));
    app.supervise_orbit_team(now + RELAUNCH_WINDOW);
    assert_eq!(drain(&mut queues[0]), "", "relaunched after giving up");
    app.pump_orbit_notifications_at(Instant::now());
    let notices = app.take_orbit_human_notices();
    assert!(
        notices.iter().any(
            |notice| notice.title == "Orbit team: rc" && notice.body.contains("relaunch limit")
        ),
        "{notices:?}"
    );
    let tasks = app.orbit_task_activity()["tasks"].clone();
    let notice = tasks
        .as_array()
        .unwrap()
        .iter()
        .find(|task| {
            task["task_id"]
                .as_str()
                .unwrap()
                .starts_with("team-rc-gave-up")
        })
        .cloned();
    let notice = notice.unwrap_or_else(|| panic!("no lead notice: {tasks}"));
    assert_eq!(notice["origin"], "server");
    assert!(notice["report"]["summary"]
        .as_str()
        .unwrap()
        .contains("no longer relaunched"));
    // Defining the role again revives the member.
    app.reset_orbit_team_member("rc");
    app.supervise_orbit_team(now + RELAUNCH_WINDOW + TICK);
    assert!(drain(&mut queues[0]).contains("claude"));
}

#[tokio::test]
async fn exits_spread_beyond_the_window_never_exhaust_the_budget() {
    let mut app = team_app();
    let mut queues = reviewer(&mut app);
    let mut now = Instant::now();
    app.supervise_orbit_team(now);
    let rc = member_terminal(&app, "rc");
    for _ in 0..(MAX_RELAUNCHES * 2) {
        now += RELAUNCH_WINDOW / 2 + Duration::from_secs(1);
        exit_member(&mut app, &rc);
        app.supervise_orbit_team(now);
        now += DELAY;
        app.supervise_orbit_team(now);
        assert_eq!(app.orbit_team_member_phase("rc", true), "running");
    }
    assert!(drain(&mut queues[0]).matches("claude").count() > MAX_RELAUNCHES);
}

#[tokio::test]
async fn stop_refuses_members_and_force_keeps_the_pane_for_the_relaunch() {
    let mut app = team_app();
    let _queues = reviewer(&mut app);
    app.supervise_orbit_team(Instant::now());
    let rc = ready_member(&mut app, "reviewer", "rc");
    let before = focus(&app);
    let refused = call(&mut app, "orbit.agent_stop", json!({"name": "rc"}));
    assert_eq!(
        refused["error"]["code"], "orbit_team_member_protected",
        "{refused}"
    );
    assert!(refused["error"]["message"]
        .as_str()
        .unwrap()
        .contains("orbit_team_remove"));
    let stopped = call(
        &mut app,
        "orbit.agent_stop",
        json!({"name": "rc", "force": true}),
    );
    assert_eq!(stopped["result"]["stopped"], true, "{stopped}");
    assert_eq!(stopped["result"]["closed"], false);
    assert_eq!(stopped["result"]["role"], "reviewer");
    assert!(
        app.state.terminals.contains_key(rc.as_str()),
        "member pane was closed"
    );
    assert!(
        app.orbit_team.roster.role("reviewer").is_some(),
        "stop removed the role"
    );
    assert_eq!(focus(&app), before);
}

#[tokio::test]
async fn a_relaunching_member_keeps_its_assignment_until_the_supervisor_gives_up() {
    let mut app = team_app();
    let _queues = reviewer(&mut app);
    let start = Instant::now();
    app.supervise_orbit_team(start);
    let rc = ready_member(&mut app, "reviewer", "rc");
    let assigned = call(
        &mut app,
        "orbit.agent_assign",
        json!({"role": "reviewer", "kind": "claude", "prompt": "Review"}),
    );
    let task_id = assigned["result"]["task_id"]
        .as_str()
        .unwrap_or_else(|| panic!("{assigned}"))
        .to_owned();
    exit_member(&mut app, &rc);
    let after_grace = Instant::now() + Duration::from_secs(20);
    app.supervise_orbit_team(after_grace);
    app.pump_orbit_notifications_at(after_grace);
    app.pump_orbit_notifications_at(after_grace + TICK);
    let delivery = |app: &mut App| {
        call(app, "orbit.task_status", json!({"task_id": task_id}))["result"]["task"]["delivery"]
            .clone()
    };
    assert_eq!(
        delivery(&mut app),
        "awaiting_report",
        "relaunch counted as a lost worker"
    );
    app.supervise_orbit_team(after_grace + DELAY);
    assert_eq!(
        app.state.terminals[rc.as_str()].agent_name.as_deref(),
        Some("rc")
    );
    app.pump_orbit_notifications_at(after_grace + DELAY + TICK);
    assert_eq!(delivery(&mut app), "awaiting_report");
    // Exhaust the budget: the open task is then lost like any worker's.
    let mut now = after_grace + DELAY;
    for _ in 0..MAX_RELAUNCHES {
        now += Duration::from_secs(10);
        exit_member(&mut app, &rc);
        app.supervise_orbit_team(now);
        now += DELAY;
        app.supervise_orbit_team(now);
    }
    exit_member(&mut app, &rc);
    app.supervise_orbit_team(now + TICK);
    assert_eq!(app.orbit_team_member_phase("rc", false), "gave_up");
    app.pump_orbit_notifications_at(now + Duration::from_secs(1));
    assert_eq!(delivery(&mut app), "worker_lost");
}

/// A member kind from a newer build is listed as unsupported in the team list,
/// never launched, and never picked for a role assignment.
#[tokio::test]
async fn an_unsupported_member_kind_is_listed_but_never_launched() {
    let mut app = team_app();
    let mut queues = reviewer(&mut app);
    let member = app.orbit_team.roster.member_mut("rx").unwrap();
    member.kind = "gemini-ultra".into();
    assert!(app.supervise_orbit_team(Instant::now()));
    assert!(drain(&mut queues[0]).contains("claude"));
    assert!(
        drain(&mut queues[1]).is_empty(),
        "unsupported member launched"
    );
    let list = call(&mut app, "orbit.team_list", json!({}));
    let members = list["result"]["roles"][0]["members"]
        .as_array()
        .unwrap()
        .clone();
    let rx = members
        .iter()
        .find(|member| member["name"] == "rx")
        .unwrap();
    assert_eq!(rx["unsupported"], true, "{list}");
    assert_eq!(rx["state"], "unsupported");
    let rc = members
        .iter()
        .find(|member| member["name"] == "rc")
        .unwrap();
    assert_eq!(rc["unsupported"], false);
    // Later passes leave it alone too.
    app.supervise_orbit_team(Instant::now() + DELAY * 4);
    assert!(drain(&mut queues[1]).is_empty());
    assert!(app.orbit_team_member_error("rx").is_none());
}

/// A member that exits with an open assignment is relaunched in the same
/// terminal; its report then reaches the lead exactly once.
#[tokio::test]
async fn a_relaunched_member_reports_its_open_assignment_to_the_lead_once() {
    let mut app = team_app();
    let _queues = reviewer(&mut app);
    let lead = app.orbit_lead_terminal().unwrap().terminal_id;
    let (runtime, mut lead_queue) =
        crate::terminal::TerminalRuntime::test_with_channel_capacity(80, 24, 16);
    let key = app
        .state
        .terminals
        .get_key_value(lead.as_str())
        .unwrap()
        .0
        .clone();
    app.terminal_runtimes.insert(key, runtime);
    let start = Instant::now();
    app.supervise_orbit_team(start);
    let rc = ready_member(&mut app, "reviewer", "rc");
    let assigned = call(
        &mut app,
        "orbit.agent_assign",
        json!({"role": "reviewer", "kind": "claude", "prompt": "Review"}),
    );
    let task_id = assigned["result"]["task_id"].as_str().unwrap().to_owned();
    let token = app.orbit_last_issued_token().unwrap();
    exit_member(&mut app, &rc);
    let later = Instant::now() + Duration::from_secs(20);
    app.supervise_orbit_team(later);
    app.supervise_orbit_team(later + DELAY);
    let relaunched = ready_member(&mut app, "reviewer", "rc");
    assert_eq!(relaunched, rc, "same terminal");
    let reported = call(
        &mut app,
        "orbit.agent_report",
        json!({"task_id": task_id, "worker_id": rc, "token": token,
               "outcome": "completed", "summary": "reviewed after relaunch"}),
    );
    assert_eq!(reported["result"]["delivery"], "queued", "{reported}");
    // The test lead shows no idle prompt: delivery waits for 10 s of idleness.
    let base = later + DELAY + Duration::from_secs(1);
    app.pump_orbit_notifications_at(base);
    let mut text = String::new();
    for step in 0..200 {
        app.pump_orbit_notifications_at(base + Duration::from_secs(11) + TICK * step);
        text.push_str(&drain(&mut lead_queue));
        if text.contains("reviewed after relaunch") {
            break;
        }
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    assert_eq!(text.matches("reviewed after relaunch").count(), 1, "{text}");
}

/// A hand-edited roster where two roles' members claim one terminal: loading
/// it is not an error, and the supervisor gives each member its own pane and
/// records its own terminal.
#[tokio::test]
async fn two_members_claiming_one_terminal_each_get_their_own() {
    let mut app = team_app();
    let mut reviewers = reviewer(&mut app);
    let mut actors = add_role(&mut app, spec("actor", &[("ac", "claude")]));
    for name in ["rc", "ac"] {
        app.orbit_team.roster.member_mut(name).unwrap().terminal_id = Some("term-shared".into());
    }
    app.orbit_team.roster.check_loaded().unwrap();
    assert_eq!(
        app.orbit_team_member_of_terminal("term-shared"),
        Some(("reviewer".into(), "rc".into())),
        "one deterministic owner"
    );
    app.supervise_orbit_team(Instant::now());
    assert!(drain(&mut reviewers[0]).contains("claude"));
    assert!(drain(&mut actors[0]).contains("claude"));
    let rc = member_terminal(&app, "rc");
    let ac = member_terminal(&app, "ac");
    assert_ne!(rc, ac);
    assert!(rc != "term-shared" && ac != "term-shared");
}

/// `orbit_team_define` while a relaunch is pending: the definition resets the
/// member, the supervisor launches it once, and the old schedule never fires a
/// second launch.
#[tokio::test]
async fn a_definition_during_a_pending_relaunch_launches_the_member_once() {
    let mut app = team_app();
    let mut queues = reviewer(&mut app);
    let now = Instant::now();
    app.supervise_orbit_team(now);
    drain(&mut queues[0]);
    let rc = member_terminal(&app, "rc");
    exit_member(&mut app, &rc);
    app.supervise_orbit_team(now + TICK);
    assert_eq!(app.orbit_team_member_phase("rc", false), "relaunching");
    let defined = call(
        &mut app,
        "orbit.team_define",
        json!({"name": "reviewer", "area": "Reviews", "prompt": "Be strict",
               "members": [{"name": "rc", "kind": "claude"}, {"name": "rx", "kind": "codex"}]}),
    );
    assert!(defined.get("error").is_none(), "{defined}");
    let mut launches = 0;
    for step in 2..=(2 + 4 * (DELAY.as_millis() / TICK.as_millis()) as u32) {
        app.supervise_orbit_team(now + TICK * step);
        launches += drain(&mut queues[0])
            .matches("--append-system-prompt")
            .count();
    }
    assert_eq!(launches, 1, "member launched {launches} times");
}
