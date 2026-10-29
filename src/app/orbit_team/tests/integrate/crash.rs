//! Crash recovery, reservation edge cases, and legacy repository names.
use super::restart::apply_by_hand;
use super::*;

#[tokio::test]
async fn an_overlapping_chain_applied_before_a_crash_is_not_replayed() {
    // (journal lines the crashed run had written, if any)
    let journals: [Option<&str>; 3] = [
        None,
        Some("about 0\napplied 0\nabout 1\n"),
        Some("about 0\napplied 0\nabout 1\napplied 1\n"),
    ];
    for journal in journals {
        let scratch = Scratch::new();
        let one = scratch.patch("one", &[], &[("a.txt", "a one\n")]);
        let two = scratch.patch(
            "two",
            std::slice::from_ref(&one.path),
            &[("a.txt", "a one\na two\n")],
        );
        assert!(apply_by_hand(&scratch, &one));
        assert!(apply_by_hand(&scratch, &two));
        let mut team = team();
        let task_id = overlapping_task(&mut team.app, 2);
        team.app.finish_parallel_steps_for_test(
            &task_id,
            scratch.patch_dir(),
            vec![
                scratch.finished("one", Some(one)),
                scratch.finished("two", Some(two)),
            ],
        );
        if let Some(lines) = journal {
            let path = crate::parallel_checkout::integrate_resume::journal_path(
                &scratch.patch_dir(),
                &task_id,
            );
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, lines).unwrap();
        }
        assert!(team.app.begin_integration_for_test(&task_id));
        // Both are in the tree, but the ledger recorded nothing.
        team.app
            .simulate_integration_restart_for_test(&task_id, vec![], None);

        let prompt = integrator_prompt(&mut team).await;

        let case = format!("{journal:?}");
        assert!(!prompt.contains("stopped at the patch"), "{case}: {prompt}");
        assert_eq!(scratch.read("a.txt"), "a one\na two\n", "{case}");
        assert!(!scratch.read("a.txt").contains("<<<<<<<"), "{case}");
        let step = integrate_step(&status(&mut team.app, &task_id));
        assert!(step["integrate"].get("stopped").is_none(), "{case}: {step}");
        let done = |key: &str| step["integrate"].get(key).cloned().unwrap_or(json!([]));
        let mut all: Vec<Value> = [done("applied"), done("recovered")]
            .iter()
            .flat_map(|v| v.as_array().cloned().unwrap())
            .collect();
        all.sort_by_key(|v| v.as_u64());
        assert_eq!(all, [json!(0), json!(1)], "{case}");
    }
}

#[tokio::test]
async fn a_patch_present_by_other_means_is_not_counted_as_applied_by_aster() {
    let scratch = Scratch::new();
    let one = scratch.patch("one", &[], &[("a.txt", "a edited\n")]);
    let two = scratch.patch("two", &[], &[("b.txt", "b edited\n")]);
    // Someone made the second change by hand.
    scratch.write("b.txt", "b edited\n");
    let mut team = team();
    let task_id = parallel_task(&mut team.app, 2);
    team.app.finish_parallel_steps_for_test(
        &task_id,
        scratch.patch_dir(),
        vec![
            scratch.finished("did a", Some(one)),
            scratch.finished("did b", Some(two.clone())),
        ],
    );

    let prompt = integrator_prompt(&mut team).await;

    assert!(
        prompt.contains("Aster applied 1 patches cleanly"),
        "{prompt}"
    );
    assert!(
        prompt.contains("neither applied nor staged them") && prompt.contains("must not reverse"),
        "{prompt}"
    );
    assert_eq!(scratch.read("a.txt"), "a edited\n");
    assert!(!scratch
        .git(&["diff", "--cached", "--name-only"])
        .contains("b.txt"));
    let step = integrate_step(&status(&mut team.app, &task_id));
    assert_eq!(step["integrate"]["applied"], json!([0]));
    assert_eq!(step["integrate"]["present"], json!([1]));

    let done = report(&mut team.app, &task_id, &team.integrator, 2, json!({}));
    assert!(done.get("error").is_none(), "{done}");
    let summary = status(&mut team.app, &task_id)["report"]["summary"]
        .as_str()
        .unwrap()
        .to_owned();
    assert!(
        summary.contains("Already present in the tree, not applied by Aster"),
        "{summary}"
    );
    let undo = summary
        .split("To undo")
        .nth(1)
        .unwrap()
        .split("Already present")
        .next()
        .unwrap();
    assert!(!undo.contains("two.patch"), "{undo}");
}

#[tokio::test]
async fn an_evicted_task_with_a_running_git_thread_still_reserves_the_repository() {
    let scratch = Scratch::new();
    let one = scratch.patch("one", &[], &[("a.txt", "a edited\n")]);
    let two = scratch.patch("two", &[], &[("b.txt", "b edited\n")]);
    let mut team = team();
    let first = parallel_task(&mut team.app, 1);
    let second = parallel_task(&mut team.app, 1);
    team.app.finish_parallel_steps_for_test(
        &first,
        scratch.patch_dir(),
        vec![scratch.finished("did a", Some(one))],
    );
    team.app.finish_parallel_steps_for_test(
        &second,
        scratch.patch_dir(),
        vec![scratch.finished("did b", Some(two))],
    );
    crate::parallel_checkout::integrate::HOLD_APPLY
        .store(true, std::sync::atomic::Ordering::SeqCst);
    // Until the first patch is in flight (its thread is held by the gate).
    for _ in 0..200 {
        team.app.pump_orbit_for_test();
        let step = integrate_step(&status(&mut team.app, &first));
        if step["integrate"].get("current").is_some() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    let cancelled = call(
        &mut team.app,
        "orbit.task_cancel",
        json!({"task_id": first}),
    );
    assert!(cancelled.get("error").is_none(), "{cancelled}");
    // The ledger makes room: the cancelled task is gone, its thread is not.
    team.app.evict_task_for_test(&first);

    for _ in 0..10 {
        team.app.pump_orbit_for_test();
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert_eq!(
        integrate_step(&status(&mut team.app, &second))["integrate"]["phase"],
        "waiting"
    );
    assert_eq!(scratch.read("b.txt"), "b\n");

    crate::parallel_checkout::integrate::HOLD_APPLY
        .store(false, std::sync::atomic::Ordering::SeqCst);
    let prompt = integrator_prompt(&mut team).await;
    assert!(prompt.contains("did b"), "{prompt}");
    assert_eq!(scratch.read("b.txt"), "b edited\n");
}

#[cfg(unix)]
#[tokio::test]
async fn repositories_recorded_through_a_symlink_by_an_older_build_are_normalized_on_load() {
    let scratch = Scratch::new();
    let link = scratch.root.join("repo-link");
    std::os::unix::fs::symlink(scratch.repo(), &link).unwrap();
    let one = scratch.patch("one", &[], &[("a.txt", "a edited\n")]);
    let two = scratch.patch("two", &[], &[("b.txt", "b edited\n")]);
    let mut team = team();
    let first = parallel_task(&mut team.app, 1);
    let second = parallel_task(&mut team.app, 1);
    team.app.finish_parallel_steps_for_test(
        &first,
        scratch.patch_dir(),
        vec![scratch.finished("did a", Some(one))],
    );
    let prompt = integrator_prompt(&mut team).await;
    assert!(prompt.contains("did a"), "{prompt}");
    // The ledger on disk names the first task's repository through the link.
    team.app.alias_integration_root_for_test(&first, link);
    team.app.finish_parallel_steps_for_test(
        &second,
        scratch.patch_dir(),
        vec![scratch.finished("did b", Some(two))],
    );

    for _ in 0..30 {
        team.app.pump_orbit_for_test();
        tokio::time::sleep(Duration::from_millis(10)).await;
    }

    let held = integrate_step(&status(&mut team.app, &first));
    assert_eq!(
        held["integrate"]["repo_root"],
        json!(std::fs::canonicalize(scratch.repo()).unwrap())
    );
    assert_eq!(
        integrate_step(&status(&mut team.app, &second))["integrate"]["phase"],
        "waiting"
    );
    assert_eq!(scratch.read("b.txt"), "b\n");
}
