use std::time::Instant;

use serde_json::json;

use super::*;

fn define(app: &mut App, members: Value) -> Value {
    call(
        app,
        "orbit.team_define",
        json!({"name": "reviewer", "area": "Reviews", "prompt": "Be strict", "members": members}),
    )
}

#[tokio::test]
async fn define_creates_the_role_tab_before_orbit_without_changing_focus() {
    let mut app = team_app();
    let before = focus(&app);
    let defined = define(&mut app, json!([{"name": "rc", "kind": "claude"}]));
    assert_eq!(defined["result"]["created_tab"], true, "{defined}");
    assert_eq!(tab_labels(&app, 0), ["Polaris", "Reviewer", "Orbit"]);
    assert_eq!(focus(&app), before);
    assert_eq!(app.resolve_orbit().unwrap(), (0, 2), "Orbit binding moved");
    let tab_id = defined["result"]["role"]["tab_id"].clone();
    assert_eq!(tab_id, json!(app.public_tab_id(0, 1)));
    // Updating the role reuses its tab and reports released members.
    let updated = define(
        &mut app,
        json!([{"name": "rx", "kind": "codex"}, {"name": "rc2", "kind": "claude"}]),
    );
    assert_eq!(updated["result"]["created_tab"], false, "{updated}");
    assert_eq!(updated["result"]["released"], json!(["rc"]));
    assert_eq!(updated["result"]["role"]["tab_id"], tab_id);
    assert_eq!(app.state.workspaces[0].tabs.len(), 3);
    // The binding survives renames and reordering; a missing tab is an error.
    app.state.workspaces[0].tabs[1].set_custom_name("renamed".into());
    app.state.workspaces[0].move_tab(1, 3);
    let role = app.orbit_team.roster.role("reviewer").unwrap().clone();
    assert_eq!(app.resolve_role_tab(&role).unwrap(), (0, 2));
    let invalid = define(&mut app, json!([{"name": "polaris", "kind": "claude"}]));
    assert_eq!(invalid["error"]["code"], "invalid_params");
    app.state.assert_invariants_for_test();
}

#[tokio::test]
async fn define_needs_a_bound_orbit_tab() {
    let mut app = new_app();
    app.state.workspaces.push(Workspace::test_new("plain"));
    let response = define(&mut app, json!([{"name": "rc", "kind": "claude"}]));
    assert_eq!(
        response["error"]["code"], "orbit_not_configured",
        "{response}"
    );
    assert_eq!(app.state.workspaces[0].tabs.len(), 1);
}

#[tokio::test]
async fn startup_recreates_a_missing_role_tab_rebinds_and_notifies() {
    let mut app = team_app();
    app.state.toast_config.delivery = crate::config::ToastDelivery::Aster;
    let _queues = add_role(&mut app, spec("reviewer", &[("rc", "claude")]));
    let old_number = app
        .orbit_team
        .roster
        .role("reviewer")
        .unwrap()
        .tab
        .clone()
        .unwrap()
        .tab_number;
    assert!(app.state.workspaces[0].close_tab(1));
    let before = focus(&app);
    app.prepare_orbit_team_startup();
    assert_eq!(tab_labels(&app, 0), ["Polaris", "Reviewer", "Orbit"]);
    assert_eq!(focus(&app), before);
    let binding = app
        .orbit_team
        .roster
        .role("reviewer")
        .unwrap()
        .tab
        .clone()
        .unwrap();
    assert_ne!(binding.tab_number, old_number);
    assert_eq!(binding.tab_number, app.state.workspaces[0].tabs[1].number);
    app.pump_orbit_notifications_at(Instant::now());
    let notices = app.take_orbit_human_notices();
    assert!(
        notices
            .iter()
            .any(|notice| notice.title == "Orbit team: reviewer"
                && notice.body == "role tab recreated."),
        "{notices:?}"
    );
    // A second startup finds the tab and does not recreate or announce it.
    app.prepare_orbit_team_startup();
    assert_eq!(app.state.workspaces[0].tabs.len(), 3);
    app.state.assert_invariants_for_test();
}

#[tokio::test]
async fn config_roles_create_tabs_on_first_start_and_no_roster_means_no_change() {
    let mut config = crate::config::Config::default();
    config.polaris.team = toml::from_str::<crate::config::PolarisConfig>(
        "[[team]]\nname = \"actor\"\narea = \"Edits\"\n[[team.members]]\nname = \"ac\"\nkind = \"claude\"\n",
    )
    .unwrap()
    .team;
    let (_, rx) = tokio::sync::mpsc::unbounded_channel();
    let mut app = App::new(
        &config,
        crate::app::AppPolicy::TEST,
        None,
        rx,
        crate::api::EventHub::default(),
    );
    app.state.workspaces.push(Workspace::test_new("polaris"));
    bind_polaris(&mut app, 0, 0);
    app.prepare_orbit_team_startup();
    assert_eq!(tab_labels(&app, 0), ["Polaris", "Actor", "Orbit"]);

    let mut plain = team_app();
    let tabs = tab_labels(&plain, 0);
    plain.prepare_orbit_team_startup();
    assert!(!plain.supervise_orbit_team(Instant::now()));
    assert_eq!(tab_labels(&plain, 0), tabs);
}

#[tokio::test]
async fn define_remove_and_restore_keep_identity_invariants_with_adversarial_state() {
    let mut app = new_app();
    app.state = crate::app::state::AppState::test_with_adversarial_identity_state();
    app.state.workspaces.push(Workspace::test_new("elsewhere"));
    app.state.ensure_test_terminals();
    bind_polaris(&mut app, 0, 1);
    let numbers: Vec<usize> = app.state.workspaces[0]
        .tabs
        .iter()
        .map(|tab| tab.number)
        .collect();
    app.state.assert_invariants_for_test();
    let defined = define(&mut app, json!([{"name": "rc", "kind": "claude"}]));
    assert!(defined.get("error").is_none(), "{defined}");
    app.state.assert_invariants_for_test();
    let role = app.orbit_team.roster.role("reviewer").unwrap().clone();
    let (ws, tab) = app.resolve_role_tab(&role).unwrap();
    let (orbit_ws, orbit_tab) = app.resolve_orbit().unwrap();
    assert_eq!(
        (ws, tab + 1),
        (orbit_ws, orbit_tab),
        "role tab is not before Orbit"
    );
    for number in &numbers {
        assert!(
            app.state.workspaces[0]
                .tabs
                .iter()
                .any(|tab| tab.number == *number),
            "tab {number} lost"
        );
    }
    // Restore after the tab was lost.
    let tab_id = app.public_tab_id(ws, tab).unwrap();
    let closed = call(&mut app, "tab.close", json!({"tab_id": tab_id}));
    assert!(closed.get("error").is_none(), "{closed}");
    app.prepare_orbit_team_startup();
    app.state.assert_invariants_for_test();
    let role = app.orbit_team.roster.role("reviewer").unwrap().clone();
    assert!(app.resolve_role_tab(&role).is_ok());
    let removed = call(
        &mut app,
        "orbit.team_remove",
        json!({"name": "reviewer", "stop_members": true}),
    );
    assert_eq!(removed["result"]["closed_tab"], true, "{removed}");
    app.state.assert_invariants_for_test();
    assert!(app.resolve_orbit().is_ok());
    assert_eq!(app.state.active, Some(1));
}
