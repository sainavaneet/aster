//! Pinned roles: `[[polaris.team]]` roles merged into every session roster at
//! startup and on config reload, protected from removal, and placed in config
//! order ahead of the session's own roles.
use std::time::Instant;

use serde_json::json;

use super::super::roster::{self, MemberSession, Roster, TabBinding};
use super::*;
use crate::agent_resume::AgentSessionRefKind;
use crate::app::orbit_worker_policy::WorkerPolicy;
use crate::config::PolarisTeamRoleConfig;

fn configs(toml: &str) -> Vec<PolarisTeamRoleConfig> {
    toml::from_str::<crate::config::PolarisConfig>(toml)
        .unwrap()
        .team
}

const ACTOR: &str = "[[team]]\nname = \"actor\"\narea = \"Edits\"\nprompt = \"Edit carefully\"\n\
    [[team.members]]\nname = \"ac\"\nkind = \"claude\"\n\
    [[team.members]]\nname = \"ad\"\nkind = \"codex\"\n";

const TESTER: &str = "[[team]]\nname = \"tester\"\narea = \"Tests\"\n\
    [[team.members]]\nname = \"tc\"\nkind = \"claude\"\n";

/// A session start (fresh or restored) with `pinned` as `[[polaris.team]]`.
fn start_with(app: &mut App, pinned: &[PolarisTeamRoleConfig]) {
    app.orbit_team.pinned = pinned.to_vec();
    app.orbit_team.loaded = false;
    app.prepare_orbit_team_startup();
}

/// Config reload through the live-config path.
fn reload_with(app: &mut App, toml: &str) {
    let mut config = crate::config::Config::default();
    config.polaris.team = configs(toml);
    app.apply_live_config(&config, &[], &[], false);
}

fn role_names(app: &App) -> Vec<(String, bool)> {
    app.orbit_team
        .roster
        .roles
        .iter()
        .map(|role| (role.name.clone(), role.pinned))
        .collect()
}

#[tokio::test]
async fn config_roles_join_an_existing_roster_ahead_of_its_session_role() {
    let mut app = team_app();
    app.ensure_orbit_team_loaded();
    let _queues = add_role(&mut app, spec("reviewer", &[("rc", "claude")]));
    let reviewer = app.orbit_team.roster.role("reviewer").unwrap().clone();
    start_with(&mut app, &configs(&format!("{ACTOR}{TESTER}")));
    assert_eq!(
        tab_labels(&app, 0),
        ["Polaris", "Actor", "Tester", "Reviewer", "Orbit"]
    );
    assert_eq!(
        role_names(&app),
        [
            ("actor".into(), true),
            ("tester".into(), true),
            ("reviewer".into(), false)
        ]
    );
    assert_eq!(
        app.orbit_team.roster.role("reviewer").unwrap(),
        &reviewer,
        "the session role is untouched"
    );
    let listed = call(&mut app, "orbit.team_list", json!({}));
    let roles = listed["result"]["roles"].as_array().unwrap();
    assert_eq!(roles[0]["pinned"], true, "{listed}");
    assert_eq!(roles[0]["overridden"], false, "{listed}");
    assert_eq!(roles[0]["members"][0]["pinned"], true, "{listed}");
    assert_eq!(roles[2]["pinned"], false, "{listed}");
    // A restart finds every tab and creates nothing.
    start_with(&mut app, &configs(&format!("{ACTOR}{TESTER}")));
    assert_eq!(app.state.workspaces[0].tabs.len(), 5);
    app.state.assert_invariants_for_test();
}

#[test]
fn a_merge_follows_config_and_keeps_terminals_sessions_and_the_tab() {
    let policy = WorkerPolicy::default();
    let first = roster::pin(&Roster::default(), &configs(ACTOR), &policy);
    let mut current = first.roster;
    let session = MemberSession {
        source: "aster:claude".into(),
        agent: "claude".into(),
        kind: AgentSessionRefKind::Id,
        value: "s-1".into(),
    };
    let binding = TabBinding {
        workspace_id: "ws-1".into(),
        tab_number: 7,
    };
    current.roles[0].tab = Some(binding.clone());
    current.roles[0].members[0].terminal_id = Some("t-ac".into());
    current.roles[0].members[0].session = Some(session.clone());
    let changed = ACTOR
        .replace("Edits", "All edits")
        .replace("Edit carefully", "Edit boldly")
        .replace(
            "name = \"ac\"\nkind = \"claude\"\n",
            "name = \"ac\"\nkind = \"claude\"\nmodel = \"opus\"\n",
        )
        + "[[team.members]]\nname = \"ae\"\nkind = \"claude\"\n";
    let pinned = roster::pin(&current, &configs(&changed), &policy);
    assert!(pinned.errors.is_empty(), "{:?}", pinned.errors);
    assert!(pinned.released.is_empty());
    let role = pinned.roster.role("actor").unwrap();
    assert_eq!(
        (role.area.as_str(), role.prompt.as_str()),
        ("All edits", "Edit boldly")
    );
    assert_eq!(role.tab, Some(binding));
    let ac = &role.members[0];
    assert_eq!(ac.model.as_deref(), Some("opus"));
    assert_eq!(ac.terminal_id.as_deref(), Some("t-ac"));
    assert_eq!(ac.session, Some(session));
    let names: Vec<&str> = role.members.iter().map(|m| m.name.as_str()).collect();
    assert_eq!(names, ["ac", "ad", "ae"]);
    assert!(role.members.iter().all(|member| member.pinned));
    // Pinning what is already current changes nothing.
    let again = roster::pin(&pinned.roster, &configs(&changed), &policy);
    assert_eq!(again.roster, pinned.roster);
}

#[tokio::test]
async fn a_pinned_role_cannot_be_removed() {
    let mut app = team_app();
    start_with(&mut app, &configs(ACTOR));
    let refused = call(
        &mut app,
        "orbit.team_remove",
        json!({"name": "actor", "stop_members": true, "force": true}),
    );
    assert_eq!(refused["error"]["code"], "orbit_role_pinned", "{refused}");
    let message = refused["error"]["message"].as_str().unwrap();
    assert!(
        message.contains("[[polaris.team]]") && message.contains("reload-config"),
        "{message}"
    );
    assert!(app.orbit_team.roster.role("actor").is_some());
    assert_eq!(tab_labels(&app, 0), ["Polaris", "Actor", "Orbit"]);
}

#[tokio::test]
async fn a_session_override_of_a_pinned_prompt_survives_reloads() {
    let mut app = team_app();
    start_with(&mut app, &configs(ACTOR));
    let defined = call(
        &mut app,
        "orbit.team_define",
        json!({
            "name": "actor",
            "area": "Ignored",
            "prompt": "Only touch docs",
            "members": [{"name": "ac", "kind": "codex"}, {"name": "ax", "kind": "codex"}]
        }),
    );
    assert!(defined.get("error").is_none(), "{defined}");
    let role = &defined["result"]["role"];
    assert_eq!(role["overridden"], true, "{defined}");
    assert_eq!(role["pinned"], true, "{defined}");
    assert_eq!(role["area"], "Edits", "area comes from config");
    assert_eq!(role["prompt"], "Only touch docs");
    let members: Vec<(&str, &str)> = role["members"]
        .as_array()
        .unwrap()
        .iter()
        .map(|m| (m["name"].as_str().unwrap(), m["kind"].as_str().unwrap()))
        .collect();
    assert_eq!(
        members,
        [("ac", "claude"), ("ad", "codex"), ("ax", "codex")],
        "config members and kinds win; the session may add members"
    );
    assert_eq!(defined["result"]["released"], json!([]));

    reload_with(&mut app, &ACTOR.replace("Edits", "Every edit"));
    let role = app.orbit_team.roster.role("actor").unwrap();
    assert!(role.pinned && role.overridden);
    assert_eq!(role.prompt, "Only touch docs");
    assert_eq!(role.area, "Every edit");
    assert!(role.members.iter().any(|member| member.name == "ax"));
    // Redefining without a prompt keeps the override.
    let redefined = call(
        &mut app,
        "orbit.team_define",
        json!({"name": "actor", "area": "x", "members": [{"name": "ac", "kind": "claude"}]}),
    );
    assert_eq!(redefined["result"]["role"]["prompt"], "Only touch docs");
    assert_eq!(redefined["result"]["released"], json!(["ax"]));
}

#[tokio::test]
async fn reload_adds_a_config_role_at_once_and_unpins_a_dropped_one() {
    let mut app = team_app();
    app.ensure_orbit_team_loaded();
    let _queues = add_role(&mut app, spec("reviewer", &[("rc", "claude")]));
    start_with(&mut app, &configs(ACTOR));
    assert_eq!(
        tab_labels(&app, 0),
        ["Polaris", "Actor", "Reviewer", "Orbit"]
    );
    reload_with(&mut app, &format!("{ACTOR}{TESTER}"));
    assert_eq!(
        tab_labels(&app, 0),
        ["Polaris", "Actor", "Tester", "Reviewer", "Orbit"],
        "the new config role is placed in config order"
    );
    assert!(app.orbit_team.roster.role("tester").unwrap().pinned);
    // Dropped from config: the role stays, no longer pinned, and can be removed.
    reload_with(&mut app, TESTER);
    assert_eq!(
        role_names(&app),
        [
            ("tester".into(), true),
            ("actor".into(), false),
            ("reviewer".into(), false)
        ]
    );
    assert_eq!(app.state.workspaces[0].tabs.len(), 5, "nothing disappears");
    let removed = call(&mut app, "orbit.team_remove", json!({"name": "actor"}));
    assert_eq!(removed["result"]["removed"], "actor", "{removed}");
    app.state.assert_invariants_for_test();
}

#[tokio::test]
async fn a_config_role_appears_within_one_supervisor_pass() {
    let mut app = team_app();
    start_with(&mut app, &configs(ACTOR));
    // A config role pinned while its tab could not be created yet.
    app.orbit_team.pinned = configs(&format!("{ACTOR}{TESTER}"));
    let pinned = roster::pin(
        &app.orbit_team.roster,
        &app.orbit_team.pinned,
        &app.orbit_worker_policy,
    );
    app.orbit_team.roster = pinned.roster;
    assert!(app.orbit_team.roster.role("tester").unwrap().tab.is_none());
    assert!(app.supervise_orbit_team(Instant::now()));
    assert_eq!(tab_labels(&app, 0), ["Polaris", "Actor", "Tester", "Orbit"]);
}

#[tokio::test]
async fn a_member_dropped_from_config_is_released_not_killed() {
    let mut app = team_app();
    app.ensure_orbit_team_loaded();
    let _queues = add_role(
        &mut app,
        spec("actor", &[("ac", "claude"), ("ad", "codex")]),
    );
    start_with(&mut app, &configs(ACTOR));
    assert_eq!(tab_labels(&app, 0), ["Polaris", "Actor", "Orbit"]);
    assert!(app.supervise_orbit_team(Instant::now()));
    let ad = ready_member(&mut app, "actor", "ad");
    let without_ad = ACTOR.replace("[[team.members]]\nname = \"ad\"\nkind = \"codex\"\n", "");
    reload_with(&mut app, &without_ad);
    assert!(app.orbit_team.roster.member("ad").is_none(), "released");
    assert!(!app.orbit_team.supervisor.members.contains_key("ad"));
    let terminal = &app.state.terminals[ad.as_str()];
    assert_eq!(
        terminal.agent_name.as_deref(),
        Some("ad"),
        "its agent keeps running"
    );
    let role = app.orbit_team.roster.role("actor").unwrap().clone();
    let (ws, tab) = app.resolve_role_tab(&role).unwrap();
    assert_eq!(
        app.orbit_member_pane(ws, tab, "ad").map(|(_, id)| id),
        Some(ad),
        "its pane still hosts it"
    );
}

#[tokio::test]
async fn a_missing_pinned_tab_is_recreated_in_config_order() {
    let mut app = team_app();
    app.ensure_orbit_team_loaded();
    let _queues = add_role(&mut app, spec("reviewer", &[("rc", "claude")]));
    start_with(&mut app, &configs(&format!("{ACTOR}{TESTER}")));
    assert!(app.state.workspaces[0].close_tab(1));
    start_with(&mut app, &configs(&format!("{ACTOR}{TESTER}")));
    assert_eq!(
        tab_labels(&app, 0),
        ["Polaris", "Actor", "Tester", "Reviewer", "Orbit"]
    );
    app.state.assert_invariants_for_test();
}
