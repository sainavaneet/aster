use std::time::Instant;

use serde_json::json;

use super::*;

/// Reviewer with an idle Claude and an idle Codex member.
fn staffed() -> (App, Vec<Queue>, String, String) {
    let mut app = team_app();
    let mut queues = add_role(
        &mut app,
        spec("reviewer", &[("rc", "claude"), ("rx", "codex")]),
    );
    app.supervise_orbit_team(Instant::now());
    for queue in &mut queues {
        drain(queue);
    }
    let rc = ready_member(&mut app, "reviewer", "rc");
    let rx = ready_member(&mut app, "reviewer", "rx");
    (app, queues, rc, rx)
}

#[tokio::test]
async fn assignment_by_role_picks_an_idle_member_preferring_kind_and_names_the_role() {
    let (mut app, mut queues, rc, rx) = staffed();
    let before = focus(&app);
    let assigned = call(
        &mut app,
        "orbit.agent_assign",
        json!({"role": "reviewer", "kind": "codex", "prompt": "Review the parser"}),
    );
    let task = &assigned["result"];
    assert_eq!(task["worker_id"], json!(rx), "{assigned}");
    let text = wait_text(&mut queues[1], "[Aster assignment").await;
    assert!(text.contains("Review the parser"), "{text}");
    assert!(
        text.contains("You act for the Reviewer role (area: reviewer area)"),
        "{text}"
    );
    assert!(text.contains("[Aster assignment"), "{text}");
    // The Codex member is booked; the next assignment goes to the Claude member.
    let second = call(
        &mut app,
        "orbit.agent_assign",
        json!({"role": "reviewer", "kind": "codex", "prompt": "Second"}),
    );
    assert_eq!(second["result"]["worker_id"], json!(rc), "{second}");
    let third = call(
        &mut app,
        "orbit.agent_assign",
        json!({"role": "reviewer", "prompt": "Third"}),
    );
    assert_eq!(third["error"]["code"], "orbit_role_busy", "{third}");
    assert!(third["error"]["message"]
        .as_str()
        .unwrap()
        .contains("rc (assigned)"));
    let unknown = call(
        &mut app,
        "orbit.agent_assign",
        json!({"role": "nobody", "prompt": "x"}),
    );
    assert_eq!(unknown["error"]["code"], "orbit_team_role_not_found");
    let missing = call(&mut app, "orbit.agent_assign", json!({"prompt": "x"}));
    assert_eq!(missing["error"]["code"], "invalid_params");
    assert_eq!(focus(&app), before);
}

#[tokio::test]
async fn busy_members_are_skipped_and_members_are_addressable_by_name() {
    let (mut app, _queues, rc, rx) = staffed();
    app.state
        .terminals
        .get_mut(rx.as_str())
        .unwrap()
        .set_detected_state(Some(Agent::Codex), AgentState::Working);
    let assigned = call(
        &mut app,
        "orbit.agent_assign",
        json!({"role": "reviewer", "kind": "codex", "prompt": "Go"}),
    );
    assert_eq!(assigned["result"]["worker_id"], json!(rc), "{assigned}");
    let status = call(&mut app, "orbit.agent_status", json!({"name": "rc"}));
    assert_eq!(
        status["result"]["agent"]["terminal_id"],
        json!(rc),
        "{status}"
    );
    assert!(
        app.orbit_terminal_in_scope(&rc),
        "member reports would be refused"
    );
    let prompt = app
        .orbit_agent_prompt_params(
            "p",
            crate::api::schema::OrbitAgentPromptParams {
                name: String::new(),
                prompt: "hello".into(),
                role: Some("reviewer".into()),
                kind: None,
            },
            false,
        )
        .unwrap();
    // Prompts may reach an idle member even while it has an assignment.
    let role = app.orbit_team.roster.role("reviewer").unwrap().clone();
    let (ws, tab) = app.resolve_role_tab(&role).unwrap();
    let (pane, _) = app.orbit_member_pane(ws, tab, "rc").unwrap();
    assert_eq!(Some(prompt.target), app.public_pane_id(ws, pane));
}

#[tokio::test]
async fn role_status_and_inspect_cover_every_member() {
    let (mut app, _queues, rc, rx) = staffed();
    let status = call(&mut app, "orbit.agent_status", json!({"role": "reviewer"}));
    let result = &status["result"];
    assert_eq!(result["name"], "reviewer", "{status}");
    assert_eq!(result["members"][0]["state"], "running");
    assert_eq!(result["members"][1]["terminal_id"], json!(rx));
    assert_eq!(result["agents"].as_array().unwrap().len(), 2);
    let inspect = call(&mut app, "orbit.inspect", json!({"role": "reviewer"}));
    assert_eq!(inspect["result"]["tab"]["label"], "Reviewer", "{inspect}");
    let panes = inspect["result"]["panes"].as_array().unwrap();
    assert_eq!(panes.len(), 2);
    assert!(panes
        .iter()
        .any(|pane| pane["terminal_id"] == json!(rc) && pane["member"] == "rc"));
    let orbit = call(&mut app, "orbit.inspect", json!({}));
    assert_eq!(orbit["result"]["tab"]["label"], "Orbit");
    let list = call(&mut app, "orbit.team_list", json!({}));
    let role = &list["result"]["roles"][0];
    assert_eq!(role["prompt"], "Act as the reviewer.");
    assert_eq!(role["members"][0]["open_tasks"], 0);
    let activity = call(&mut app, "orbit.activity", json!({}));
    assert_eq!(
        activity["result"]["team"][0]["members"][1]["name"], "rx",
        "{activity}"
    );
    assert!(activity["result"]["team"][0].get("prompt").is_none());
}

#[tokio::test]
async fn scratch_starts_cannot_take_a_member_name() {
    let (mut app, _queues, _, _) = staffed();
    let response = call(
        &mut app,
        "orbit.agent_start",
        json!({"name": "rc", "kind": "claude"}),
    );
    assert_eq!(response["error"]["code"], "orbit_team_member", "{response}");
}

#[tokio::test]
async fn remove_refuses_open_assignments_unless_forced_and_keeps_the_tab_by_default() {
    let (mut app, _queues, rc, _) = staffed();
    let assigned = call(
        &mut app,
        "orbit.agent_assign",
        json!({"name": "rc", "prompt": "Work"}),
    );
    let task_id = assigned["result"]["task_id"].as_str().unwrap().to_owned();
    let refused = call(&mut app, "orbit.team_remove", json!({"name": "reviewer"}));
    assert_eq!(
        refused["error"]["code"], "orbit_worker_assigned",
        "{refused}"
    );
    assert!(app.orbit_team.roster.role("reviewer").is_some());
    let removed = call(
        &mut app,
        "orbit.team_remove",
        json!({"name": "reviewer", "force": true}),
    );
    assert_eq!(
        removed["result"]["cancelled_tasks"],
        json!([task_id]),
        "{removed}"
    );
    assert_eq!(removed["result"]["closed_tab"], false);
    assert!(app.orbit_team.roster.roles.is_empty());
    assert_eq!(tab_labels(&app, 0), ["Polaris", "Reviewer", "Orbit"]);
    // The pane stays as an ordinary agent pane, no longer in scope or protected.
    assert!(!app.orbit_terminal_in_scope(&rc));
    app.state.assert_invariants_for_test();
}
