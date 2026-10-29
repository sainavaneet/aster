use super::super::roster::{self, Roster, MAX_MEMBERS, MAX_ROLES, MAX_ROLE_PROMPT_BYTES};
use super::super::store;
use super::spec;
use crate::app::orbit_worker_policy::WorkerPolicy;

fn policy(toml: &str) -> WorkerPolicy {
    WorkerPolicy::from_config(&toml::from_str(toml).unwrap())
}

fn rejected(spec: roster::RoleSpec, policy: &WorkerPolicy) -> (&'static str, String) {
    roster::validate(spec, policy, None).unwrap_err()
}

#[test]
fn role_validation_checks_names_limits_and_the_worker_allowlist() {
    let default = WorkerPolicy::default();
    let role = roster::validate(
        spec("reviewer", &[("rc", "claude"), ("rx", "codex")]),
        &default,
        None,
    )
    .unwrap();
    assert_eq!(role.label(), "Reviewer");
    assert_eq!(role.members[1].kind, "codex");
    for name in [
        "Reviewer",
        "polaris",
        "orbit",
        "",
        "a-name-that-is-far-too-long-for-aster",
    ] {
        assert_eq!(
            rejected(spec(name, &[("rc", "claude")]), &default).0,
            "invalid_params",
            "{name}"
        );
    }
    assert_eq!(
        rejected(spec("reviewer", &[]), &default).0,
        "invalid_params"
    );
    let five: Vec<(String, &str)> = (0..=MAX_MEMBERS)
        .map(|i| (format!("m{i}"), "claude"))
        .collect();
    let five: Vec<(&str, &str)> = five.iter().map(|(n, k)| (n.as_str(), *k)).collect();
    assert!(rejected(spec("reviewer", &five), &default)
        .1
        .contains("1 to 4 members"));
    let (_, message) = rejected(
        spec("reviewer", &[("rc", "claude"), ("rc", "codex")]),
        &default,
    );
    assert!(message.contains("used twice"), "{message}");
    let (_, message) = rejected(spec("reviewer", &[("polaris", "claude")]), &default);
    assert!(message.contains("reserved"), "{message}");
    let mut long = spec("reviewer", &[("rc", "claude")]);
    long.prompt = "x".repeat(MAX_ROLE_PROMPT_BYTES + 1);
    assert!(rejected(long, &default).1.contains("4096 bytes"));
    // Kinds and models follow [polaris.workers]; members are Claude or Codex.
    let (code, message) = rejected(
        spec("reviewer", &[("rx", "codex")]),
        &policy("kinds = [\"claude\"]\n"),
    );
    assert_eq!(code, "orbit_worker_not_allowed");
    assert!(message.contains("allowed kinds: claude"), "{message}");
    let gemini = policy("kinds = [\"claude\", \"gemini\"]\n");
    assert!(rejected(spec("reviewer", &[("rg", "gemini")]), &gemini)
        .1
        .contains("use claude or codex"));
    let models = policy("[models]\nclaude = [\"opus\"]\n");
    assert_eq!(
        rejected(spec("reviewer", &[("rc", "claude")]), &models).0,
        "orbit_worker_not_allowed"
    );
    let mut bypass = spec("reviewer", &[("rc", "claude")]);
    bypass.members[0].permission_mode = Some("bypassPermissions".into());
    assert_eq!(
        rejected(bypass, &default).0,
        "orbit_permission_mode_refused"
    );
}

#[test]
fn roster_keeps_member_names_unique_and_caps_roles() {
    let policy = WorkerPolicy::default();
    let mut roster = Roster::default();
    roster
        .upsert(roster::validate(spec("reviewer", &[("rc", "claude")]), &policy, None).unwrap())
        .unwrap();
    let taken = roster::validate(spec("actor", &[("rc", "claude")]), &policy, None).unwrap();
    assert!(roster
        .upsert(taken)
        .unwrap_err()
        .1
        .contains("already belongs to role reviewer"));
    let clash = roster::validate(spec("rc", &[("ac", "claude")]), &policy, None).unwrap();
    assert!(roster
        .upsert(clash)
        .unwrap_err()
        .1
        .contains("already a member name"));
    for index in 1..MAX_ROLES {
        let role = roster::validate(
            spec(&format!("role{index}"), &[(&format!("m{index}"), "claude")]),
            &policy,
            None,
        )
        .unwrap();
        roster.upsert(role).unwrap();
    }
    let extra = roster::validate(spec("extra", &[("mx", "claude")]), &policy, None).unwrap();
    assert!(roster
        .upsert(extra)
        .unwrap_err()
        .1
        .contains("at most 8 roles"));
    // Replacing an existing role is always possible.
    let update = roster::validate(
        spec("reviewer", &[("rc", "claude"), ("rx", "codex")]),
        &policy,
        roster.role("reviewer"),
    )
    .unwrap();
    roster.upsert(update).unwrap();
    assert_eq!(roster.role("reviewer").unwrap().members.len(), 2);
}

#[test]
fn redefinition_keeps_observed_state_of_unchanged_members_only() {
    let policy = WorkerPolicy::default();
    let mut role = roster::validate(
        spec("reviewer", &[("rc", "claude"), ("rx", "codex")]),
        &policy,
        None,
    )
    .unwrap();
    role.members[0].terminal_id = Some("t-1".into());
    role.members[1].terminal_id = Some("t-2".into());
    let next = roster::validate(
        spec("reviewer", &[("rc", "claude"), ("rx", "claude")]),
        &policy,
        Some(&role),
    )
    .unwrap();
    assert_eq!(next.members[0].terminal_id.as_deref(), Some("t-1"));
    assert_eq!(
        next.members[1].terminal_id, None,
        "a changed kind starts over"
    );
}

#[test]
fn roster_file_round_trips_atomically_and_rejects_invalid_files() {
    let directory = std::env::temp_dir().join(format!("aster-orbit-team-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&directory);
    let path = directory.join(store::FILE_NAME);
    assert_eq!(store::load(&path), Ok(None));
    let policy = WorkerPolicy::default();
    let mut role = roster::validate(
        spec("reviewer", &[("rc", "claude"), ("rx", "codex")]),
        &policy,
        None,
    )
    .unwrap();
    role.tab = Some(roster::TabBinding {
        workspace_id: "w7".into(),
        tab_number: 12,
    });
    role.members[0].terminal_id = Some("term-1".into());
    role.members[0].session = Some(roster::MemberSession {
        source: "aster:claude".into(),
        agent: "claude".into(),
        kind: crate::agent_resume::AgentSessionRefKind::Id,
        value: "session-1".into(),
    });
    let mut original = Roster::default();
    original.upsert(role).unwrap();
    store::save(&path, &original).unwrap();
    assert!(!directory.join(format!("{}.tmp", store::FILE_NAME)).exists());
    assert_eq!(store::load(&path), Ok(Some(original.clone())));
    let text = std::fs::read_to_string(&path).unwrap();
    assert!(text.contains("\"version\": 1"), "{text}");
    std::fs::write(&path, text.replace("\"version\": 1", "\"version\": 2")).unwrap();
    assert!(store::load(&path)
        .unwrap_err()
        .contains("unsupported version 2"));
    std::fs::write(&path, "{not json").unwrap();
    assert!(store::load(&path)
        .unwrap_err()
        .contains("Invalid Orbit team roster"));
    std::fs::write(
        &path,
        r#"{"version":1,"roles":[{"name":"Bad","members":[]}]}"#,
    )
    .unwrap();
    assert!(store::load(&path).is_err());
    let _ = std::fs::remove_dir_all(&directory);
}

#[test]
fn config_roles_are_validated_one_by_one_when_pinned() {
    let config: crate::config::PolarisConfig = toml::from_str(
        "[[team]]\nname = \"reviewer\"\narea = \"Reviews\"\nprompt = \"Be strict\"\n\
         [[team.members]]\nname = \"rc\"\nkind = \"claude\"\nmodel = \"opus\"\n\
         [[team.members]]\nname = \"rx\"\nkind = \"codex\"\n\
         [[team]]\nname = \"actor\"\narea = \"Edits\"\n[[team.members]]\nname = \"ac\"\nkind = \"claude\"\n",
    )
    .unwrap();
    let pinned = roster::pin(&Roster::default(), &config.team, &WorkerPolicy::default());
    assert!(pinned.errors.is_empty(), "{:?}", pinned.errors);
    let roster = pinned.roster;
    assert_eq!(roster.roles.len(), 2);
    assert!(roster
        .roles
        .iter()
        .all(|role| role.pinned && !role.overridden));
    assert_eq!(
        roster.role("reviewer").unwrap().members[0].model.as_deref(),
        Some("opus")
    );
    assert!(roster.roles.iter().all(|role| role.tab.is_none()));
    let mut invalid = config.team.clone();
    invalid[1].members[0].name = "rc".into();
    let pinned = roster::pin(&Roster::default(), &invalid, &WorkerPolicy::default());
    assert_eq!(
        pinned.roster.roles.len(),
        1,
        "the valid role is still pinned"
    );
    assert!(
        pinned.errors[0].contains("[[polaris.team]] actor"),
        "{:?}",
        pinned.errors
    );
    assert!(crate::config::PolarisConfig::default().team.is_empty());
}

fn state_fixture(name: &str) -> std::path::PathBuf {
    std::path::PathBuf::from(format!(
        "{}/tests/fixtures/orbit/state/{name}",
        env!("CARGO_MANIFEST_DIR")
    ))
}

#[test]
fn this_builds_roster_format_loads() {
    let roster = store::load(&state_fixture("orbit-team.v1.json"))
        .unwrap()
        .unwrap();
    let role = roster.role("reviewer").unwrap();
    assert_eq!(role.members.len(), 2);
    assert!(role.members.iter().all(roster::Member::supported));
    assert!(role.members[0].session.is_some());
    assert_eq!(role.tab.as_ref().unwrap().tab_number, 2);
}

#[test]
fn a_roster_from_a_newer_build_loads_and_writes_its_extra_fields_back() {
    let roster = store::load(&state_fixture("orbit-team.newer.json"))
        .unwrap()
        .expect("loads despite unknown fields and kinds");
    let role = roster.role("reviewer").unwrap();
    assert_eq!(role.extra["budget"]["daily_usd"], 5);
    let (_, rc) = roster.member("rc").unwrap();
    assert!(rc.supported());
    assert_eq!(rc.extra["priority"], 1);
    assert!(
        rc.session.is_none(),
        "unreadable session dropped, starts fresh"
    );
    let (_, rg) = roster.member("rg").unwrap();
    assert!(!rg.supported(), "unknown kind is unsupported, not an error");
    let directory =
        std::env::temp_dir().join(format!("aster-orbit-team-newer-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&directory);
    let path = directory.join(store::FILE_NAME);
    store::save(&path, &roster).unwrap();
    let text = std::fs::read_to_string(&path).unwrap();
    for kept in [
        "team_schema",
        "budget",
        "priority",
        "sandbox",
        "gemini-ultra",
    ] {
        assert!(text.contains(kept), "{kept} lost: {text}");
    }
    assert_eq!(store::load(&path).unwrap().unwrap(), roster);
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(&path).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o600, "roster is private");
    }
    let _ = std::fs::remove_dir_all(&directory);
}

#[test]
fn a_failed_roster_write_is_surfaced_as_a_diagnostic() {
    let mut app = super::new_app();
    app.note_orbit_team_saved(Ok(()));
    assert!(app.state.config_diagnostic.is_none());
    app.note_orbit_team_saved(Err("No space left on device".into()));
    let diagnostic = app.state.config_diagnostic.clone().unwrap();
    assert_eq!(
        diagnostic,
        "Could not save the Orbit team roster: No space left on device"
    );
}
