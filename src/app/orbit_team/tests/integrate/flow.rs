use super::*;

#[tokio::test]
async fn clean_patches_apply_in_order_and_the_integrators_report_is_the_only_one() {
    let scratch = Scratch::new();
    let one = scratch.patch("one", &[], &[("a.txt", "a edited\n")]);
    let two = scratch.patch("two", &[], &[("b.txt", "b edited\n"), ("new.txt", "new\n")]);
    let mut team = team();
    let task_id = parallel_task(&mut team.app, 2);
    team.app.finish_parallel_steps_for_test(
        &task_id,
        scratch.patch_dir(),
        vec![
            scratch.finished("did a", Some(one.clone())),
            scratch.finished("did b", Some(two.clone())),
        ],
    );

    let prompt = integrator_prompt(&mut team).await;

    assert!(prompt.contains("applied all 2 patches cleanly"), "{prompt}");
    assert!(
        prompt.contains("did a") && prompt.contains("did b"),
        "{prompt}"
    );
    assert!(
        prompt.contains(&one.path.display().to_string()) && prompt.contains("git apply -R"),
        "{prompt}"
    );
    assert_eq!(
        (
            scratch.read("a.txt").as_str(),
            scratch.read("b.txt").as_str()
        ),
        ("a edited\n", "b edited\n")
    );
    assert_eq!(scratch.read("new.txt"), "new\n");
    let staged = scratch.git(&["diff", "--cached", "--name-only"]);
    assert!(
        staged.contains("a.txt") && staged.contains("new.txt"),
        "{staged}"
    );
    let current = status(&mut team.app, &task_id);
    assert_eq!(current["delivery"], "awaiting_report");
    assert_eq!(current["worker_id"], json!(team.integrator));
    assert_eq!(current["step_line"], "integrating by rv");
    let step = integrate_step(&current);
    assert_eq!(step["member"], "rv");
    assert_eq!(step["integrate"]["applied"], json!([0, 1]));
    assert!(step["integrate"].get("stopped").is_none());
    assert!(no_report_yet(&current));

    let answer = report(
        &mut team.app,
        &task_id,
        &team.integrator,
        2,
        json!({"changed_files": ["fixup.txt", "a.txt"], "checks": [{"name": "full tests", "status": "passed"}], "tests_run": true}),
    );

    assert!(answer.get("error").is_none(), "{answer}");
    let done = status(&mut team.app, &task_id);
    assert_eq!(done["delivery"], "queued");
    assert_eq!(
        done["report"]["changed_files"],
        json!(["a.txt", "b.txt", "fixup.txt", "new.txt"])
    );
    assert_eq!(done["report"]["checks"][0]["name"], "full tests");
    let summary = done["report"]["summary"].as_str().unwrap();
    assert!(
        summary.contains("integrated and tested") && summary.contains("git apply -R"),
        "{summary}"
    );
    assert!(
        summary.find("two.patch").unwrap() < summary.rfind("one.patch").unwrap(),
        "undo lists the patches in reverse: {summary}"
    );
    // A repeat is acknowledged; exactly one task, one report.
    assert!(report(&mut team.app, &task_id, &team.integrator, 2, json!({"changed_files": ["fixup.txt", "a.txt"], "checks": [{"name": "full tests", "status": "passed"}], "tests_run": true})).get("error").is_none());
    let activity = call(&mut team.app, "orbit.activity", json!({}))["result"].clone();
    assert_eq!(activity["tasks"].as_array().unwrap().len(), 1, "{activity}");
}

#[tokio::test]
async fn patches_of_an_overlapping_chain_are_applied_in_step_order_once_each() {
    let scratch = Scratch::new();
    let first = scratch.patch("first", &[], &[("a.txt", "a one\n")]);
    let second = scratch.patch(
        "second",
        std::slice::from_ref(&first.path),
        &[("a.txt", "a one\na two\n")],
    );
    let mut team = team();
    let task_id = parallel_task(&mut team.app, 2);
    team.app.finish_parallel_steps_for_test(
        &task_id,
        scratch.patch_dir(),
        vec![
            scratch.finished("one", Some(first)),
            scratch.finished("two", Some(second)),
        ],
    );

    let prompt = integrator_prompt(&mut team).await;

    assert!(prompt.contains("applied all 2 patches cleanly"), "{prompt}");
    assert_eq!(scratch.read("a.txt"), "a one\na two\n");
    let step = integrate_step(&status(&mut team.app, &task_id));
    assert_eq!(step["integrate"]["applied"], json!([0, 1]));
}

#[tokio::test]
async fn a_conflict_stops_at_the_first_conflicting_patch_and_a_failed_integration_keeps_patches() {
    let scratch = Scratch::new();
    let one = scratch.patch("one", &[], &[("a.txt", "a from step\n")]);
    let two = scratch.patch("two", &[], &[("b.txt", "b edited\n")]);
    // The main tree moved on in the same place.
    scratch.write("a.txt", "a from main\n");
    scratch.git(&["commit", "-q", "-am", "main moves"]);
    let mut team = team();
    let task_id = parallel_task(&mut team.app, 2);
    team.app.finish_parallel_steps_for_test(
        &task_id,
        scratch.patch_dir(),
        vec![
            scratch.finished("did a", Some(one.clone())),
            scratch.finished("did b", Some(two.clone())),
        ],
    );

    let prompt = integrator_prompt(&mut team).await;

    assert!(
        prompt.contains("stopped at the patch of step 1"),
        "{prompt}"
    );
    assert!(
        prompt.contains("conflicts in a.txt") && prompt.contains("conflict markers"),
        "{prompt}"
    );
    assert!(
        prompt.contains(&two.path.display().to_string()),
        "the unapplied patch is listed: {prompt}"
    );
    assert!(
        scratch.read("a.txt").contains("<<<<<<<"),
        "markers left: {}",
        scratch.read("a.txt")
    );
    assert_eq!(scratch.read("b.txt"), "b\n", "later patches are not tried");
    let step = integrate_step(&status(&mut team.app, &task_id));
    assert_eq!(step["integrate"]["stopped"]["step"], 0);
    assert_eq!(step["integrate"]["stopped"]["files"], json!(["a.txt"]));
    assert_eq!(step["integrate"]["stopped"]["conflict"], true);
    assert_eq!(step["integrate"]["applied"], json!([]));

    let answer = report(
        &mut team.app,
        &task_id,
        &team.integrator,
        2,
        json!({"outcome": "failed", "summary": "could not resolve"}),
    );

    assert!(answer.get("error").is_none(), "{answer}");
    let done = status(&mut team.app, &task_id);
    assert_eq!(done["report"]["outcome"], "failed");
    assert!(
        one.path.exists() && two.path.exists(),
        "saved patches are kept"
    );
    assert!(done["report"]["summary"]
        .as_str()
        .unwrap()
        .contains(&one.path.display().to_string()));
}

#[tokio::test]
async fn a_patch_that_does_not_apply_stops_without_markers_and_names_its_files() {
    let scratch = Scratch::new();
    let one = scratch.patch("one", &[], &[("a.txt", "a edited\n")]);
    // The file the patch edits is gone from the main tree.
    scratch.git(&["rm", "-q", "a.txt"]);
    scratch.git(&["commit", "-q", "-m", "remove a"]);
    let mut team = team();
    let task_id = parallel_task(&mut team.app, 1);
    team.app.finish_parallel_steps_for_test(
        &task_id,
        scratch.patch_dir(),
        vec![scratch.finished("did a", Some(one))],
    );

    let prompt = integrator_prompt(&mut team).await;

    assert!(
        prompt.contains("did not apply and changed nothing"),
        "{prompt}"
    );
    let step = integrate_step(&status(&mut team.app, &task_id));
    assert_eq!(step["integrate"]["stopped"]["conflict"], false);
    assert_eq!(step["integrate"]["stopped"]["files"], json!(["a.txt"]));
}

#[tokio::test]
async fn the_integrator_reports_directly_and_steps_without_patches_are_noted() {
    let scratch = Scratch::new();
    let one = scratch.patch("one", &[], &[("a.txt", "a edited\n")]);
    let mut team = team();
    let task_id = parallel_task(&mut team.app, 2);
    team.app.finish_parallel_steps_for_test(
        &task_id,
        scratch.patch_dir(),
        vec![
            scratch.finished("did a", Some(one)),
            scratch.finished("", None),
        ],
    );
    let prompt = integrator_prompt(&mut team).await;
    assert!(prompt.contains("Not included: step 2"), "{prompt}");

    let handoff = json!({"handoff": {"done": "d", "next": "n"}});
    let refused = report(&mut team.app, &task_id, &team.integrator, 2, handoff);

    assert!(
        refused["error"]["message"]
            .as_str()
            .unwrap()
            .contains("directly"),
        "{refused}"
    );
    assert_eq!(
        status(&mut team.app, &task_id)["delivery"],
        "awaiting_report"
    );
}

#[tokio::test]
async fn with_nothing_to_integrate_the_task_reports_its_steps_directly() {
    let scratch = Scratch::new();
    let mut team = team();
    let task_id = parallel_task(&mut team.app, 2);
    team.app.finish_parallel_steps_for_test(
        &task_id,
        scratch.patch_dir(),
        vec![scratch.finished("", None), scratch.finished("", None)],
    );

    team.app.pump_orbit_for_test();

    let current = status(&mut team.app, &task_id);
    assert!(current["steps"]
        .as_array()
        .unwrap()
        .iter()
        .all(|s| s.get("integrate").is_none()));
    assert!(!no_report_yet(&current) && current["report"]["outcome"] == "failed");
}

#[tokio::test]
async fn the_integrator_is_the_configured_member_else_the_reviewers_first_else_the_roles_first() {
    let mut team = team();
    let app = &mut team.app;
    // No configuration: the reviewer role's first member.
    assert_eq!(
        app.orbit_integrator(Some("builder")),
        Some(("rv".into(), "claude".into()))
    );
    assert_eq!(
        app.orbit_integrator(None),
        Some(("rv".into(), "claude".into()))
    );
    // Configured on the task's pinned role: that member, even from another role.
    let toml = "[[team]]\nname = \"builder\"\nintegrator = \"bb\"\n[[team.members]]\nname = \"ba\"\nkind = \"claude\"\n[[team.members]]\nname = \"bb\"\nkind = \"codex\"\n";
    app.orbit_team.pinned = toml::from_str::<crate::config::PolarisConfig>(toml)
        .unwrap()
        .team;
    app.orbit_team
        .roster
        .roles
        .iter_mut()
        .find(|r| r.name == "builder")
        .unwrap()
        .pinned = true;
    assert_eq!(
        app.orbit_integrator(Some("builder")),
        Some(("bb".into(), "codex".into()))
    );
    // A configured member that does not exist falls back to the reviewer.
    app.orbit_team.pinned[0].integrator = Some("ghost".into());
    assert_eq!(
        app.orbit_integrator(Some("builder")),
        Some(("rv".into(), "claude".into()))
    );
}

#[tokio::test]
async fn without_a_reviewer_role_the_tasks_own_role_provides_the_integrator() {
    let mut app = team_app();
    app.ensure_orbit_team_loaded();
    let _queues = add_role(
        &mut app,
        spec("builder", &[("ba", "claude"), ("bb", "codex")]),
    );
    assert_eq!(
        app.orbit_integrator(Some("builder")),
        Some(("ba".into(), "claude".into()))
    );
    assert_eq!(app.orbit_integrator(Some("nobody")), None);
    assert_eq!(app.orbit_integrator(None), None);
}

/// The "Still to apply" list of an integrator prompt.
fn still_to_apply(prompt: &str) -> String {
    let start = prompt
        .find("Still to apply")
        .expect("a stopped integration lists the rest");
    let tail = &prompt[start..];
    tail[..tail.find(". Resolve").unwrap()].to_owned()
}

#[tokio::test]
async fn after_a_conflict_every_later_patch_is_listed_in_order() {
    // Conflict at patch 1 of 3, then at patch 2 of 3.
    for conflict_at in [0usize, 1] {
        let scratch = Scratch::new();
        let files = ["a.txt", "b.txt", "c.txt"];
        let patches: Vec<_> = files
            .iter()
            .enumerate()
            .map(|(n, file)| scratch.patch(&format!("p{n}"), &[], &[(file, "from the step\n")]))
            .collect();
        scratch.write(files[conflict_at], "from main\n");
        scratch.git(&["commit", "-q", "-am", "main moves"]);
        let mut team = team();
        let task_id = parallel_task(&mut team.app, 3);
        let finished = patches
            .iter()
            .map(|patch| scratch.finished("did a part", Some(patch.clone())))
            .collect();
        team.app
            .finish_parallel_steps_for_test(&task_id, scratch.patch_dir(), finished);

        let prompt = integrator_prompt(&mut team).await;

        let stopped = format!("stopped at the patch of step {}", conflict_at + 1);
        assert!(prompt.contains(&stopped), "{prompt}");
        let remaining = still_to_apply(&prompt);
        let expected: Vec<String> = (conflict_at + 1..3)
            .map(|n| format!("{} (step {})", patches[n].path.display(), n + 1))
            .collect();
        let listed: Vec<usize> = (0..3)
            .filter(|n| remaining.contains(&patches[*n].path.display().to_string()))
            .collect();
        assert_eq!(
            listed,
            (conflict_at + 1..3).collect::<Vec<_>>(),
            "{remaining}"
        );
        assert!(
            remaining.contains(&expected.join("; ")),
            "in order: {remaining} vs {expected:?}"
        );
        assert!(prompt.contains("authorises you"), "{prompt}");
    }
}
