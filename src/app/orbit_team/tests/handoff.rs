//! A task relayed across the members of a role: a report with a handoff ends
//! the reporter's step, the next member is chosen and prompted, and Polaris
//! still gets one task and one final report.
use std::time::Instant;

use serde_json::{json, Value};

use super::*;

struct Team {
    app: App,
    queues: Vec<Queue>,
    terminals: Vec<String>,
}

/// Role `builder` with the given `(name, kind)` members, all ready and idle;
/// `pipeline` makes it a pinned role with that `[[polaris.team]]` pipeline.
fn team(members: &[(&str, &str)], pipeline: &[&str]) -> Team {
    let mut app = team_app();
    app.ensure_orbit_team_loaded();
    let mut queues = add_role(&mut app, spec("builder", members));
    app.supervise_orbit_team(Instant::now());
    for queue in &mut queues {
        drain(queue);
    }
    if !pipeline.is_empty() {
        let kinds: Vec<String> = pipeline.iter().map(|kind| format!("{kind:?}")).collect();
        let mut toml = format!(
            "[[team]]\nname = \"builder\"\npipeline = [{}]\n",
            kinds.join(", ")
        );
        for (name, kind) in members {
            toml.push_str(&format!(
                "[[team.members]]\nname = {name:?}\nkind = {kind:?}\n"
            ));
        }
        app.orbit_team.pinned = toml::from_str::<crate::config::PolarisConfig>(&toml)
            .unwrap()
            .team;
        let role = app.orbit_team.roster.roles.iter_mut().next().unwrap();
        role.pinned = true;
    }
    let terminals = members
        .iter()
        .map(|(name, _)| ready_member(&mut app, "builder", name))
        .collect();
    Team {
        app,
        queues,
        terminals,
    }
}

fn assign(app: &mut App, params: Value) -> (String, String) {
    let response = call(app, "orbit.agent_assign", params);
    assert!(response.get("error").is_none(), "{response}");
    let task = &response["result"];
    (
        task["task_id"].as_str().unwrap().into(),
        task["worker_id"].as_str().unwrap().into(),
    )
}

fn report(
    app: &mut App,
    task_id: &str,
    worker: &str,
    token: &str,
    handoff: Option<Value>,
) -> Value {
    report_for_step(app, task_id, worker, token, None, handoff)
}

fn report_for_step(
    app: &mut App,
    task_id: &str,
    worker: &str,
    token: &str,
    step: Option<usize>,
    handoff: Option<Value>,
) -> Value {
    let mut params = json!({
        "task_id": task_id, "worker_id": worker, "token": token,
        "outcome": "completed", "summary": "my part is done",
    });
    if let Some(handoff) = handoff {
        params["handoff"] = handoff;
    }
    if let Some(step) = step {
        params["step"] = json!(step);
    }
    call(app, "orbit.agent_report", params)
}

fn note(to: Option<&str>) -> Value {
    json!({
        "done": "Wrote the parser",
        "next": "Review the parser",
        "changed_files": ["src/parser.rs"],
        "known_issues": "no fuzzing yet",
        "to": to,
    })
}

fn task(app: &mut App, task_id: &str) -> Value {
    call(app, "orbit.task_status", json!({"task_id": task_id}))["result"]["task"].clone()
}

fn states(task: &Value) -> Vec<(String, String)> {
    task["steps"]
        .as_array()
        .map(|steps| {
            steps
                .iter()
                .map(|step| {
                    (
                        step["member"].as_str().unwrap().into(),
                        step["state"].as_str().unwrap().into(),
                    )
                })
                .collect()
        })
        .unwrap_or_default()
}

fn pair(member: &str, state: &str) -> (String, String) {
    (member.into(), state.into())
}

fn set_working(app: &mut App, terminal: &str, agent: Agent) {
    let terminal = app.state.terminals.get_mut(terminal).unwrap();
    terminal.set_detected_state(Some(agent), AgentState::Working);
}

const MEMBERS: [(&str, &str); 3] = [("ba", "claude"), ("bb", "codex"), ("bc", "codex")];

#[tokio::test]
async fn handoff_to_a_named_member_relays_one_task_with_the_note() {
    let Team {
        mut app,
        mut queues,
        terminals,
    } = team(&MEMBERS, &[]);
    let (task_id, worker) = assign(
        &mut app,
        json!({"role": "builder", "kind": "claude", "prompt": "Build the parser\nsecond line"}),
    );
    assert_eq!(worker, terminals[0]);
    let first = wait_text(&mut queues[0], "[Aster assignment").await;
    assert!(first.contains("Your role has other members"), "{first}");
    let token = app.orbit_last_issued_token().unwrap();

    let relayed = report(&mut app, &task_id, &worker, &token, Some(note(Some("bc"))));

    assert!(relayed.get("error").is_none(), "{relayed}");
    let text = wait_text(&mut queues[2], "[Aster assignment").await;
    assert!(
        text.contains("Continue task \"Build the parser\""),
        "{text}"
    );
    assert!(text.contains("Wrote the parser") && text.contains("Review the parser"));
    assert!(
        text.contains("src/parser.rs") && text.contains("no fuzzing yet"),
        "{text}"
    );
    assert!(
        text.contains(&format!("\"worker_id\":\"{}\"", terminals[2])),
        "{text}"
    );
    assert!(text.contains("Your role has other members"), "{text}");
    assert!(drain(&mut queues[1]).is_empty());
    let current = task(&mut app, &task_id);
    assert_eq!(current["delivery"], "awaiting_report");
    assert_eq!(current["worker_id"], json!(terminals[2]));
    assert_eq!(current["report"], Value::Null);
    assert_eq!(
        states(&current),
        [pair("ba", "handed_off"), pair("bc", "active")]
    );
    assert_eq!(current["steps"][0]["handoff"]["done"], "Wrote the parser");
    assert_eq!(current["step_line"], "step 2/2 bc");
    let activity = call(&mut app, "orbit.activity", json!({}))["result"].clone();
    assert_eq!(activity["tasks"].as_array().unwrap().len(), 1, "{activity}");
    assert_eq!(activity["tasks"][0]["step_line"], "step 2/2 bc");

    let finished = report(&mut app, &task_id, &terminals[2], &token, None);

    assert!(finished.get("error").is_none(), "{finished}");
    let done = task(&mut app, &task_id);
    assert_eq!(done["delivery"], "queued");
    assert_eq!(done["report"]["outcome"], "completed");
    assert_eq!(
        states(&done),
        [pair("ba", "handed_off"), pair("bc", "done")]
    );
}

#[tokio::test]
async fn handoff_without_a_target_follows_the_role_pipeline() {
    let members = [("pa", "claude"), ("pb", "claude"), ("pc", "codex")];
    let Team {
        mut app,
        mut queues,
        terminals,
    } = team(&members, &["claude", "codex"]);
    // No kind is given: the pipeline's first kind starts the task.
    let (task_id, worker) = assign(&mut app, json!({"role": "builder", "prompt": "Build"}));
    assert_eq!(worker, terminals[0]);
    let token = app.orbit_last_issued_token().unwrap();

    let relayed = report(&mut app, &task_id, &worker, &token, Some(note(None)));

    assert!(relayed.get("error").is_none(), "{relayed}");
    wait_text(&mut queues[2], "Continue task").await;
    let current = task(&mut app, &task_id);
    assert_eq!(current["worker_id"], json!(terminals[2]));
    assert_eq!(
        states(&current),
        [pair("pa", "handed_off"), pair("pc", "active")]
    );
    assert_eq!(current["step_line"], "step 2/2 pc");
    assert_eq!(current["steps"][1]["kind"], "codex");
}

#[tokio::test]
async fn handoff_falls_back_to_another_kind_when_the_target_is_busy_or_unnamed() {
    let Team {
        mut app,
        mut queues,
        terminals,
    } = team(&MEMBERS, &[]);
    let (task_id, worker) = assign(
        &mut app,
        json!({"role": "builder", "kind": "claude", "prompt": "Go"}),
    );
    assert_eq!(worker, terminals[0]);
    let token = app.orbit_last_issued_token().unwrap();
    // The named member is busy: the first idle member of the other kind takes over.
    set_working(&mut app, &terminals[2], Agent::Codex);

    let relayed = report(&mut app, &task_id, &worker, &token, Some(note(Some("bc"))));

    assert!(relayed.get("error").is_none(), "{relayed}");
    wait_text(&mut queues[1], "Continue task").await;
    assert!(drain(&mut queues[2]).is_empty());
    let current = task(&mut app, &task_id);
    assert_eq!(current["worker_id"], json!(terminals[1]));
    assert_eq!(
        states(&current),
        [pair("ba", "handed_off"), pair("bb", "active")]
    );
}

#[tokio::test]
async fn handoff_with_no_member_available_blocks_the_task_and_notifies_polaris() {
    let members = [("na", "claude"), ("nb", "codex")];
    let Team {
        mut app, terminals, ..
    } = team(&members, &[]);
    let (task_id, worker) = assign(
        &mut app,
        json!({"role": "builder", "kind": "claude", "prompt": "Go"}),
    );
    let token = app.orbit_last_issued_token().unwrap();
    set_working(&mut app, &terminals[1], Agent::Codex);

    let relayed = report(&mut app, &task_id, &worker, &token, Some(note(None)));

    assert!(relayed.get("error").is_none(), "{relayed}");
    let blocked = task(&mut app, &task_id);
    assert_eq!(blocked["delivery"], "queued");
    assert_eq!(blocked["report"]["outcome"], "blocked");
    assert_eq!(
        blocked["report"]["blocked_on"],
        "no member available for handoff"
    );
    assert_eq!(states(&blocked), [pair("na", "handed_off")]);
    let activity = call(&mut app, "orbit.activity", json!({}))["result"].clone();
    let notices: Vec<&Value> = activity["tasks"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|task| task["notice_for"] == json!(task_id))
        .collect();
    assert_eq!(notices.len(), 1, "{activity}");
    assert_eq!(notices[0]["origin"], "server");
    let summary = notices[0]["report"]["summary"].as_str().unwrap();
    assert!(
        summary.contains("no member available for handoff"),
        "{summary}"
    );
    assert!(summary.contains("Review the parser"), "{summary}");
}

#[tokio::test]
async fn final_report_finishes_once_and_a_repeated_handoff_is_not_relayed_again() {
    let Team {
        mut app,
        mut queues,
        terminals,
    } = team(&MEMBERS, &[]);
    let (task_id, worker) = assign(
        &mut app,
        json!({"role": "builder", "kind": "claude", "prompt": "Go"}),
    );
    let token = app.orbit_last_issued_token().unwrap();
    report(&mut app, &task_id, &worker, &token, Some(note(Some("bb"))));
    wait_text(&mut queues[1], "Continue task").await;

    // The first member retries its identical handoff: acknowledged, not relayed again.
    let retry = report(&mut app, &task_id, &worker, &token, Some(note(Some("bb"))));
    assert!(retry.get("error").is_none(), "{retry}");
    assert_eq!(
        task(&mut app, &task_id)["steps"].as_array().unwrap().len(),
        2
    );

    let finished = report(&mut app, &task_id, &terminals[1], &token, None);
    assert!(finished.get("error").is_none(), "{finished}");
    let again = report(&mut app, &task_id, &terminals[1], &token, None);
    assert!(again.get("error").is_none(), "{again}");
    let mut changed = json!({
        "task_id": task_id, "worker_id": terminals[1], "token": token,
        "outcome": "completed", "summary": "a different result",
    });
    changed["tests_run"] = json!(true);
    let conflict = call(&mut app, "orbit.agent_report", changed);
    assert!(conflict.get("error").is_some(), "{conflict}");
    let activity = call(&mut app, "orbit.activity", json!({}))["result"].clone();
    assert_eq!(activity["tasks"].as_array().unwrap().len(), 1, "{activity}");
    let done = task(&mut app, &task_id);
    assert_eq!(done["delivery"], "queued");
    assert_eq!(
        states(&done),
        [pair("ba", "handed_off"), pair("bb", "done")]
    );
}

#[tokio::test]
async fn handoff_from_a_member_that_is_not_the_current_step_is_rejected() {
    let Team {
        mut app,
        mut queues,
        terminals,
    } = team(&MEMBERS, &[]);
    let (task_id, worker) = assign(
        &mut app,
        json!({"role": "builder", "kind": "claude", "prompt": "Go"}),
    );
    let token = app.orbit_last_issued_token().unwrap();
    report(&mut app, &task_id, &worker, &token, Some(note(Some("bb"))));
    wait_text(&mut queues[1], "Continue task").await;

    // The first member already handed off; a different handoff from it is refused.
    let mut other = note(None);
    other["next"] = json!("Something else");
    let stale = report(&mut app, &task_id, &worker, &token, Some(other.clone()));
    assert!(
        stale["error"]["message"]
            .as_str()
            .unwrap()
            .contains("not found"),
        "{stale}"
    );
    // So is a handoff from a member that never worked on the task.
    let outsider = report(&mut app, &task_id, &terminals[2], &token, Some(other));
    assert!(outsider.get("error").is_some(), "{outsider}");
    let current = task(&mut app, &task_id);
    assert_eq!(current["worker_id"], json!(terminals[1]));
    assert_eq!(
        states(&current),
        [pair("ba", "handed_off"), pair("bb", "active")]
    );
}

#[tokio::test]
async fn oversized_or_unsafe_handoff_notes_are_refused_before_anything_changes() {
    let Team { mut app, .. } = team(&MEMBERS, &[]);
    let (task_id, worker) = assign(
        &mut app,
        json!({"role": "builder", "kind": "claude", "prompt": "Go"}),
    );
    let token = app.orbit_last_issued_token().unwrap();
    let cases = [
        ("done", json!("x".repeat(1001))),
        ("next", json!("")),
        ("known_issues", json!("y".repeat(501))),
        ("changed_files", json!(vec!["a.rs"; 201])),
        ("changed_files", json!(["/etc/passwd"])),
        ("to", json!("Not A Name")),
    ];
    for (field, value) in cases {
        let mut bad = note(None);
        bad[field] = value;
        let refused = report(&mut app, &task_id, &worker, &token, Some(bad));
        let message = refused["error"]["message"].as_str().unwrap_or_default();
        assert!(
            message.contains(&format!("handoff.{field}")),
            "{field}: {refused}"
        );
    }
    let unchanged = task(&mut app, &task_id);
    assert_eq!(unchanged["delivery"], "awaiting_report");
    assert_eq!(unchanged["worker_id"], json!(worker));
}

#[tokio::test]
async fn delayed_reports_from_an_earlier_step_never_act_on_a_later_step() {
    let members = [("ba", "claude"), ("bb", "codex")];
    let Team {
        mut app,
        mut queues,
        terminals,
    } = team(&members, &[]);
    let (task_id, first) = assign(
        &mut app,
        json!({"role": "builder", "kind": "claude", "prompt": "Go"}),
    );
    assert_eq!(first, terminals[0]);
    let token = app.orbit_last_issued_token().unwrap();
    let text = wait_text(&mut queues[0], "[Aster assignment").await;
    assert!(text.contains("\"step\":0"), "{text}");

    // Step 0 (ba) hands to bb, step 1 (bb) hands back to ba: step 2 is ba's.
    let relay = |to: &str| note(Some(to));
    report_for_step(
        &mut app,
        &task_id,
        &terminals[0],
        &token,
        Some(0),
        Some(relay("bb")),
    );
    let text = wait_text(&mut queues[1], "Continue task").await;
    assert!(text.contains("\"step\":1"), "{text}");
    let back = report_for_step(
        &mut app,
        &task_id,
        &terminals[1],
        &token,
        Some(1),
        Some(relay("ba")),
    );
    assert!(back.get("error").is_none(), "{back}");
    let text = wait_text(&mut queues[0], "Continue task").await;
    assert!(text.contains("\"step\":2"), "{text}");
    let current = task(&mut app, &task_id);
    assert_eq!(current["worker_id"], json!(terminals[0]));
    assert_eq!(
        states(&current),
        [
            pair("ba", "handed_off"),
            pair("bb", "handed_off"),
            pair("ba", "active")
        ]
    );

    // A delayed step-0 handoff is acknowledged without relaying again.
    let repeated = report_for_step(
        &mut app,
        &task_id,
        &terminals[0],
        &token,
        Some(0),
        Some(relay("bb")),
    );
    assert!(repeated.get("error").is_none(), "{repeated}");
    // A delayed step-0 final report, or a different step-0 handoff, is refused.
    let late_final = report_for_step(&mut app, &task_id, &terminals[0], &token, Some(0), None);
    assert!(
        late_final["error"]["message"]
            .as_str()
            .unwrap()
            .contains("earlier step"),
        "{late_final}"
    );
    let mut other = note(None);
    other["next"] = json!("Something else");
    let late_other = report_for_step(
        &mut app,
        &task_id,
        &terminals[0],
        &token,
        Some(0),
        Some(other),
    );
    assert!(late_other.get("error").is_some(), "{late_other}");
    let unchanged = task(&mut app, &task_id);
    assert_eq!(unchanged["delivery"], "awaiting_report");
    assert_eq!(unchanged["worker_id"], json!(terminals[0]));
    assert_eq!(states(&unchanged), states(&current));

    // The current step still finishes the task, once.
    let done = report_for_step(&mut app, &task_id, &terminals[0], &token, Some(2), None);
    assert!(done.get("error").is_none(), "{done}");
    let finished = task(&mut app, &task_id);
    assert_eq!(finished["delivery"], "queued");
    assert_eq!(
        states(&finished),
        [
            pair("ba", "handed_off"),
            pair("bb", "handed_off"),
            pair("ba", "done")
        ]
    );
}

/// ba -> bb -> ba on one task: step 2 belongs to ba, with ba's step-0 reports delayed.
async fn relayed_back_to_the_first_member() -> (Team, String, String) {
    let members = [("ba", "claude"), ("bb", "codex")];
    let mut team = team(&members, &[]);
    let (task_id, _) = assign(
        &mut team.app,
        json!({"role": "builder", "kind": "claude", "prompt": "Go"}),
    );
    let token = team.app.orbit_last_issued_token().unwrap();
    let ids = team.terminals.clone();
    report_for_step(
        &mut team.app,
        &task_id,
        &ids[0],
        &token,
        Some(0),
        Some(note(Some("bb"))),
    );
    wait_text(&mut team.queues[1], "Continue task").await;
    report_for_step(
        &mut team.app,
        &task_id,
        &ids[1],
        &token,
        Some(1),
        Some(note(Some("ba"))),
    );
    wait_text(&mut team.queues[0], "Continue task").await;
    (team, task_id, token)
}

#[tokio::test]
async fn stepless_reports_on_a_relayed_task_are_refused_when_ambiguous() {
    let (mut team, task_id, token) = relayed_back_to_the_first_member().await;
    let first = team.terminals[0].clone();
    let before = task(&mut team.app, &task_id);

    let handoff = report(
        &mut team.app,
        &task_id,
        &first,
        &token,
        Some(note(Some("bb"))),
    );
    let finished = report(&mut team.app, &task_id, &first, &token, None);

    for refused in [&handoff, &finished] {
        let message = refused["error"]["message"].as_str().unwrap_or_default();
        assert!(message.contains("report with the step"), "{refused}");
    }
    let after = task(&mut team.app, &task_id);
    assert_eq!(after["delivery"], "awaiting_report");
    assert_eq!(after["worker_id"], before["worker_id"]);
    assert_eq!(states(&after), states(&before));
    // Naming the current step works.
    let done = report_for_step(&mut team.app, &task_id, &first, &token, Some(2), None);
    assert!(done.get("error").is_none(), "{done}");
    assert_eq!(task(&mut team.app, &task_id)["delivery"], "queued");
}

#[tokio::test]
async fn stepless_reports_still_work_for_unambiguous_and_non_relayed_tasks() {
    // A relayed task whose only step is the reporter's current one.
    let Team {
        mut app, terminals, ..
    } = team(&[("ba", "claude"), ("bb", "codex")], &[]);
    let (task_id, _) = assign(
        &mut app,
        json!({"role": "builder", "kind": "claude", "prompt": "Go"}),
    );
    let token = app.orbit_last_issued_token().unwrap();
    let done = report(&mut app, &task_id, &terminals[0], &token, None);
    assert!(done.get("error").is_none(), "{done}");
    assert_eq!(task(&mut app, &task_id)["delivery"], "queued");

    // A single-member role never relays: no steps, and a step-less report works.
    let Team {
        mut app, terminals, ..
    } = team(&[("solo", "claude")], &[]);
    let (task_id, _) = assign(&mut app, json!({"role": "builder", "prompt": "Go"}));
    let token = app.orbit_last_issued_token().unwrap();
    assert!(task(&mut app, &task_id).get("steps").is_none());
    let done = report(&mut app, &task_id, &terminals[0], &token, None);
    assert!(done.get("error").is_none(), "{done}");
    assert_eq!(task(&mut app, &task_id)["delivery"], "queued");
}

fn parallel_parts() -> Value {
    json!([
        {"member_or_kind": "codex", "prompt": "Write the tests", "files": ["tests"]},
        {"member_or_kind": "claude", "prompt": "Write the docs", "files": ["docs/parser.md"]},
    ])
}

#[tokio::test]
async fn a_handoff_can_fan_out_into_parallel_steps() {
    let Team {
        mut app, terminals, ..
    } = team(&MEMBERS, &[]);
    let (task_id, worker) = assign(
        &mut app,
        json!({"role": "builder", "kind": "claude", "prompt": "Build the parser"}),
    );
    let token = app.orbit_last_issued_token().unwrap();
    assert_eq!(worker, terminals[0]);
    let mut empty_files = note(None);
    empty_files["parallel"] = json!([{"member_or_kind": "codex", "prompt": "x", "files": []}]);
    let refused = report_for_step(
        &mut app,
        &task_id,
        &worker,
        &token,
        Some(0),
        Some(empty_files),
    );
    assert!(
        refused["error"]["message"]
            .as_str()
            .unwrap()
            .contains("parallel[0].files"),
        "{refused}"
    );
    assert_eq!(
        task(&mut app, &task_id)["steps"].as_array().unwrap().len(),
        1
    );

    let mut fanned = note(None);
    fanned["parallel"] = parallel_parts();
    let answer = report_for_step(
        &mut app,
        &task_id,
        &worker,
        &token,
        Some(0),
        Some(fanned.clone()),
    );

    assert!(answer.get("error").is_none(), "{answer}");
    let current = task(&mut app, &task_id);
    assert_eq!(current["delivery"], "awaiting_report");
    assert_eq!(current["worker_id"], "");
    assert_eq!(
        states(&current),
        [
            pair("ba", "handed_off"),
            pair("codex", "pending"),
            pair("claude", "pending")
        ]
    );
    assert_eq!(current["step_line"], "parallel 0/2 done");
    let prompt = current["steps"][1]["parallel"]["prompt"].as_str().unwrap();
    assert!(
        prompt.contains("Wrote the parser") && prompt.contains("Write the tests"),
        "{prompt}"
    );
    assert_eq!(
        current["steps"][2]["parallel"]["files"][0],
        "docs/parser.md"
    );
    // A delayed repeat of the handoff is acknowledged, not fanned out again.
    let repeat = report_for_step(&mut app, &task_id, &worker, &token, Some(0), Some(fanned));
    assert!(repeat.get("error").is_none(), "{repeat}");
    assert_eq!(
        task(&mut app, &task_id)["steps"].as_array().unwrap().len(),
        3
    );
}
