//! Config roles scoped to projects: `[[polaris.team]] projects` pins a role
//! only in sessions whose Orbit project is one of them or inside one; a role
//! that stops applying is unpinned, and retired when it was never used.
use std::path::{Path, PathBuf};
use std::time::Instant;

use serde_json::json;

use super::*;
use crate::config::PolarisTeamRoleConfig;

/// An existing directory `name` under a per-process temp root.
fn project_dir(name: &str) -> PathBuf {
    let dir = std::env::temp_dir()
        .join(format!("aster-orbit-scoped-{}", std::process::id()))
        .join(name);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

fn actor(projects: &[&Path]) -> String {
    let projects: Vec<String> = projects
        .iter()
        .map(|path| format!("{:?}", path.display().to_string()))
        .collect();
    format!(
        "[[team]]\nname = \"actor\"\narea = \"Edits\"\nprojects = [{}]\n\
         [[team.members]]\nname = \"ac\"\nkind = \"claude\"\n\
         [[team.members]]\nname = \"ad\"\nkind = \"codex\"\n",
        projects.join(", ")
    )
}

fn configs(toml: &str) -> Vec<PolarisTeamRoleConfig> {
    toml::from_str::<crate::config::PolarisConfig>(toml)
        .unwrap()
        .team
}

/// A Polaris session whose Orbit project directory is `project`.
fn session_in(project: &Path) -> App {
    let mut app = team_app();
    app.state.workspaces[0].identity_cwd = project.to_path_buf();
    app
}

fn start_with(app: &mut App, toml: &str) {
    app.orbit_team.pinned = configs(toml);
    app.orbit_team.loaded = false;
    app.prepare_orbit_team_startup();
}

fn reload_with(app: &mut App, toml: &str) {
    let mut config = crate::config::Config::default();
    config.polaris.team = configs(toml);
    app.apply_live_config(&config, &[], &[], false);
}

fn pinned(app: &App, role: &str) -> Option<bool> {
    app.orbit_team.roster.role(role).map(|role| role.pinned)
}

#[tokio::test]
async fn a_role_is_pinned_only_in_sessions_of_its_project_or_inside_it() {
    let aster = project_dir("aster");
    let nested = project_dir("aster/crates/core");
    let other = project_dir("other");
    let toml = actor(&[&aster]);

    let mut matching = session_in(&aster);
    start_with(&mut matching, &toml);
    assert_eq!(pinned(&matching, "actor"), Some(true));
    assert_eq!(tab_labels(&matching, 0), ["Polaris", "Actor", "Orbit"]);
    assert!(
        matching.state.config_diagnostic.is_none(),
        "a scoped role needs no hint"
    );

    let mut inside = session_in(&nested);
    start_with(&mut inside, &toml);
    assert_eq!(
        pinned(&inside, "actor"),
        Some(true),
        "a nested directory matches"
    );

    let mut elsewhere = session_in(&other);
    start_with(&mut elsewhere, &toml);
    assert_eq!(pinned(&elsewhere, "actor"), None);
    assert_eq!(tab_labels(&elsewhere, 0), ["Polaris", "Orbit"]);
}

#[tokio::test]
async fn a_role_without_projects_is_pinned_everywhere_with_one_hint() {
    let toml = "[[team]]\nname = \"tester\"\narea = \"Tests\"\n\
        [[team.members]]\nname = \"tc\"\nkind = \"claude\"\n";
    let mut app = session_in(&project_dir("anywhere"));
    start_with(&mut app, toml);
    assert_eq!(pinned(&app, "tester"), Some(true));
    let hint = app.state.config_diagnostic.take().expect("hint shown");
    assert!(
        hint.contains("tester") && hint.contains("projects"),
        "{hint}"
    );
    reload_with(&mut app, &toml.replace("Tests", "All tests"));
    start_with(&mut app, toml);
    assert_eq!(app.state.config_diagnostic, None, "the hint is shown once");
    assert_eq!(pinned(&app, "tester"), Some(true));
}

#[tokio::test]
async fn an_unused_role_whose_project_no_longer_matches_is_removed_with_its_tab() {
    let aster = project_dir("retire-aster");
    let other = project_dir("retire-other");
    let mut app = session_in(&aster);
    start_with(&mut app, &actor(&[&aster]));
    assert_eq!(tab_labels(&app, 0), ["Polaris", "Actor", "Orbit"]);

    reload_with(&mut app, &actor(&[&other]));

    assert_eq!(pinned(&app, "actor"), None, "removed");
    assert_eq!(
        tab_labels(&app, 0),
        ["Polaris", "Orbit"],
        "its tab is closed"
    );
    app.state.assert_invariants_for_test();
}

#[tokio::test]
async fn a_used_role_whose_project_no_longer_matches_is_unpinned_but_kept() {
    let aster = project_dir("kept-aster");
    let other = project_dir("kept-other");
    let mut app = session_in(&aster);
    app.ensure_orbit_team_loaded();
    let _queues = add_role(
        &mut app,
        spec("actor", &[("ac", "claude"), ("ad", "codex")]),
    );
    start_with(&mut app, &actor(&[&aster]));
    assert_eq!(pinned(&app, "actor"), Some(true));
    app.supervise_orbit_team(Instant::now());
    ready_member(&mut app, "actor", "ac");
    let assigned = call(
        &mut app,
        "orbit.agent_assign",
        json!({"role": "actor", "kind": "claude", "prompt": "Edit"}),
    );
    assert!(assigned.get("error").is_none(), "{assigned}");
    assert!(app.orbit_team.roster.role("actor").unwrap().used);

    reload_with(&mut app, &actor(&[&other]));

    assert_eq!(pinned(&app, "actor"), Some(false), "unpinned, not removed");
    assert_eq!(tab_labels(&app, 0), ["Polaris", "Actor", "Orbit"]);
    // A restart in the same project keeps it too.
    start_with(&mut app, &actor(&[&other]));
    assert_eq!(pinned(&app, "actor"), Some(false));
}
