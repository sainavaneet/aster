//! Restarts mid-integration, and tasks taking turns on one repository.
use super::*;

/// Applies `patch` to the main tree the way Aster does, outside Aster.
pub(super) fn apply_by_hand(scratch: &Scratch, patch: &PatchSummary) -> bool {
    Command::new("git")
        .arg("-C")
        .arg(scratch.repo())
        .args(["apply", "--3way"])
        .arg(&patch.path)
        .status()
        .unwrap()
        .success()
}

#[tokio::test]
async fn a_restart_never_applies_a_recorded_patch_again() {
    // (patch 1 already in the tree?, recorded as applied, patch in flight,
    //  steps Aster applies now, steps found in the tree)
    type Case = (bool, Vec<usize>, Option<usize>, Vec<usize>, Vec<usize>);
    let cases: [Case; 4] = [
        (true, vec![0], None, vec![0, 1], vec![]),
        // The patch was applied but the checkpoint never reached the ledger,
        // and there is no journal: it is found in the tree, not applied again.
        (true, vec![], None, vec![1], vec![0]),
        (true, vec![], Some(0), vec![1], vec![0]),
        (false, vec![], Some(0), vec![0, 1], vec![]),
    ];
    for (in_tree, applied, current, applies, found) in cases {
        let scratch = Scratch::new();
        let one = scratch.patch("one", &[], &[("a.txt", "a edited\n")]);
        let two = scratch.patch("two", &[], &[("b.txt", "b edited\n")]);
        if in_tree {
            assert!(apply_by_hand(&scratch, &one));
        }
        let mut team = team();
        let task_id = parallel_task(&mut team.app, 2);
        team.app.finish_parallel_steps_for_test(
            &task_id,
            scratch.patch_dir(),
            vec![
                scratch.finished("did a", Some(one)),
                scratch.finished("did b", Some(two)),
            ],
        );
        assert!(team.app.begin_integration_for_test(&task_id));
        team.app
            .simulate_integration_restart_for_test(&task_id, applied.clone(), current);

        let prompt = integrator_prompt(&mut team).await;

        let case = format!("{in_tree} {applied:?} {current:?}");
        if found.is_empty() {
            assert!(
                prompt.contains("applied all 2 patches cleanly"),
                "{case}: {prompt}"
            );
        } else {
            assert!(
                prompt.contains("found in the tree and are assumed"),
                "{case}: {prompt}"
            );
        }
        assert_eq!(scratch.read("a.txt"), "a edited\n", "{case}");
        assert_eq!(scratch.read("b.txt"), "b edited\n", "{case}");
        let step = integrate_step(&status(&mut team.app, &task_id));
        assert_eq!(step["integrate"]["applied"], json!(applies), "{case}");
        let recovered = step["integrate"]
            .get("recovered")
            .cloned()
            .unwrap_or(json!([]));
        assert_eq!(recovered, json!(found), "{case}");
    }
}

#[tokio::test]
async fn a_restart_that_cannot_tell_whether_a_patch_applied_says_so() {
    let scratch = Scratch::new();
    let one = scratch.patch("one", &[], &[("a.txt", "a from step\n")]);
    let two = scratch.patch("two", &[], &[("b.txt", "b edited\n")]);
    scratch.write("a.txt", "a from main\n");
    scratch.git(&["commit", "-q", "-am", "main moves"]);
    // The server died with the patch half applied: markers in the tree.
    assert!(!apply_by_hand(&scratch, &one));
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
    assert!(team.app.begin_integration_for_test(&task_id));
    team.app
        .simulate_integration_restart_for_test(&task_id, vec![], Some(0));

    let prompt = integrator_prompt(&mut team).await;

    assert!(
        prompt.contains("tree does not show whether it was applied"),
        "{prompt}"
    );
    assert!(prompt.contains("check git status"), "{prompt}");
    assert!(
        prompt.contains(&two.path.display().to_string()),
        "later patches are still listed: {prompt}"
    );
    assert_eq!(scratch.read("b.txt"), "b\n", "nothing more is applied");
}

#[tokio::test]
async fn two_tasks_integrating_into_one_repository_take_turns() {
    let scratch = Scratch::new();
    let one = scratch.patch("one", &[], &[("a.txt", "a edited\n")]);
    let two = scratch.patch("two", &[], &[("b.txt", "b edited\n")]);
    let mut team = team();
    let first = parallel_task(&mut team.app, 1);
    let first_token = team.app.orbit_last_issued_token();
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

    let prompt = integrator_prompt(&mut team).await;

    assert!(
        prompt.contains("did a") && !prompt.contains("did b"),
        "{prompt}"
    );
    assert_eq!(scratch.read("a.txt"), "a edited\n");
    // The second task waits, with its reason, and the tree is not touched for it.
    for _ in 0..5 {
        team.app.pump_orbit_for_test();
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    let waiting = integrate_step(&status(&mut team.app, &second));
    assert_eq!(waiting["integrate"]["phase"], "waiting");
    assert_eq!(
        waiting["integrate"]["log"],
        json!([format!("waiting for integration of task {first}")])
    );
    assert_eq!(scratch.read("b.txt"), "b\n");
    assert!(drain(&mut team.queues[2]).is_empty());

    // The first integrator reports; the second integration then runs.
    let done = report_as(
        &mut team.app,
        &first,
        &team.integrator,
        1,
        first_token,
        json!({}),
    );
    assert!(done.get("error").is_none(), "{done}");
    let prompt = integrator_prompt(&mut team).await;
    assert!(prompt.contains("did b"), "{prompt}");
    assert_eq!(scratch.read("b.txt"), "b edited\n");
    let step = integrate_step(&status(&mut team.app, &second));
    assert_eq!(step["integrate"]["applied"], json!([0]));
}

#[tokio::test]
async fn a_cancelled_integration_keeps_the_repository_until_its_git_thread_finishes() {
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
    assert_eq!(
        integrate_step(&status(&mut team.app, &first))["integrate"]["phase"],
        "applying"
    );

    let cancelled = call(
        &mut team.app,
        "orbit.task_cancel",
        json!({"task_id": first}),
    );
    assert!(cancelled.get("error").is_none(), "{cancelled}");

    // The step is over, but its git thread still runs: the second task waits.
    for _ in 0..10 {
        team.app.pump_orbit_for_test();
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert_eq!(
        integrate_step(&status(&mut team.app, &first))["state"],
        "failed"
    );
    let waiting = integrate_step(&status(&mut team.app, &second));
    assert_eq!(waiting["integrate"]["phase"], "waiting");
    assert_eq!(
        waiting["integrate"]["log"],
        json!([format!("waiting for integration of task {first}")])
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
async fn a_symlinked_path_to_the_same_repository_does_not_bypass_the_reservation() {
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
    let via_link = FinishedStep {
        summary: "did b".into(),
        patch: Some(two),
        repo_root: link,
    };
    team.app
        .finish_parallel_steps_for_test(&second, scratch.patch_dir(), vec![via_link]);

    let prompt = integrator_prompt(&mut team).await;

    assert!(
        prompt.contains("did a") && !prompt.contains("did b"),
        "{prompt}"
    );
    for _ in 0..5 {
        team.app.pump_orbit_for_test();
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    let waiting = integrate_step(&status(&mut team.app, &second));
    assert_eq!(waiting["integrate"]["phase"], "waiting");
    assert_eq!(scratch.read("b.txt"), "b\n");
}
