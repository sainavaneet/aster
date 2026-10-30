//! Parallel steps on the ledger: a fanned-out task is one entry with one final
//! report; overlapping steps wait; a reported step's patch is saved and its
//! checkout removed.
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{SystemTime, UNIX_EPOCH};

use serde_json::{json, Value};

use super::*;
use crate::app::orbit_notifications::parallel::Phase;
use crate::app::orbit_notifications::steps::StepState;
use crate::events::AppEvent;

fn parallel_task(app: &mut App, steps: Value) -> String {
    let response = api(
        app,
        "orbit.agent_assign",
        json!({"prompt": "Build two things", "parallel": steps}),
    );
    assert!(response.get("error").is_none(), "{response}");
    response["result"]["task_id"].as_str().unwrap().to_owned()
}

fn step(files: &[&str]) -> Value {
    json!({"member_or_kind": "claude", "prompt": "Do the part", "files": files})
}

/// Pretends step `index` is running, as the tick would have left it.
fn run_step(app: &mut App, task_id: &str, index: usize, worker: &str) {
    let step = &mut task_mut(app, task_id).steps[index];
    step.state = StepState::Active;
    let parallel = step.parallel.as_mut().unwrap();
    parallel.phase = Phase::Running;
    parallel.worker_id = Some(worker.to_owned());
}

fn reviewer(app: &App) -> String {
    let ws = &app.state.workspaces[0];
    ws.terminal_id(ws.tabs[1].root_pane).unwrap().to_string()
}

fn step_report(app: &App, task_id: &str, worker: &str, index: usize) -> OrbitAgentReportParams {
    OrbitAgentReportParams {
        token: app.orbit_last_issued_token(),
        step: Some(index),
        ..OrbitAgentReportParams::new(
            task_id.into(),
            worker.into(),
            OrbitOutcome::Completed,
            "Part done".into(),
        )
    }
}

/// A scratch directory that is removed when the test ends, pass or fail.
struct TempDir(PathBuf);

impl TempDir {
    fn new(name: &str) -> Self {
        let path = std::env::temp_dir().join(format!(
            "aster-parallel-{name}-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&path).unwrap();
        Self(path)
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

struct Repo {
    root: PathBuf,
}

impl Repo {
    fn new() -> Self {
        let root = std::env::temp_dir().join(format!(
            "aster-parallel-ledger-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(root.join("repo")).unwrap();
        let root = std::fs::canonicalize(root).unwrap();
        let repo = Self { root };
        repo.git(&["init", "-q"]);
        std::fs::write(repo.path().join("a.txt"), "a\n").unwrap();
        repo.git(&["add", "."]);
        repo.git(&["commit", "-q", "-m", "base"]);
        repo
    }

    fn path(&self) -> PathBuf {
        self.root.join("repo")
    }

    fn git(&self, args: &[&str]) {
        let status = Command::new("git")
            .args(["-c", "user.name=T", "-c", "user.email=t@example.invalid"])
            .args(["-c", "commit.gpgsign=false", "-C"])
            .arg(self.path())
            .args(args)
            .status()
            .unwrap();
        assert!(status.success(), "git {args:?}");
    }
}

impl Drop for Repo {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

fn no_clone(_: &Path, _: &Path) -> std::io::Result<()> {
    Err(std::io::Error::other("no clone here"))
}

#[tokio::test]
async fn parallel_assignment_is_one_task_with_waiting_steps_and_overlap_notes() {
    let (mut app, _worker, _lead) = setup();

    let task_id = parallel_task(
        &mut app,
        json!([
            step(&["src/a.rs"]),
            step(&["src/a.rs", "docs"]),
            step(&["src/b.rs"])
        ]),
    );

    let task = task(&app, &task_id);
    assert_eq!(task.delivery, Delivery::AwaitingReport);
    assert_eq!(task.steps.len(), 3);
    assert!(task.steps.iter().all(|s| s.state == StepState::Pending));
    let logs: Vec<_> = task
        .steps
        .iter()
        .map(|s| s.parallel.as_ref().unwrap().log.clone())
        .collect();
    assert!(logs[0].is_empty() && logs[2].is_empty(), "{logs:?}");
    assert_eq!(logs[1], ["overlaps step 1 on src/a.rs; runs after it"]);
    assert_eq!(task.parallel_line().as_deref(), Some("parallel 0/3 done"));
    let status = api(&mut app, "orbit.task_status", json!({"task_id": task_id}));
    assert_eq!(status["result"]["task"]["step_line"], "parallel 0/3 done");
    assert_eq!(
        status["result"]["task"]["steps"][1]["parallel"]["files"][1],
        "docs"
    );
}

#[tokio::test]
async fn parallel_requests_that_cannot_run_are_refused_without_a_task() {
    let (mut app, _worker, _lead) = setup();
    let refuse = |app: &mut App, params: Value| {
        let response = api(app, "orbit.agent_assign", params);
        assert!(response.get("error").is_some(), "{response}");
        assert!(app.orbit_notifications.tasks.is_empty());
    };
    refuse(&mut app, json!({"prompt": "x", "parallel": []}));
    refuse(&mut app, json!({"prompt": "x", "parallel": [step(&[])]}));
    refuse(
        &mut app,
        json!({"prompt": "x", "parallel": [step(&["/etc"])]}),
    );
    refuse(
        &mut app,
        json!({"prompt": "x", "parallel": [{"member_or_kind": "nobody", "prompt": "p", "files": ["a"]}]}),
    );
    refuse(
        &mut app,
        json!({"prompt": "x", "name": "reviewer", "parallel": [step(&["a"])]}),
    );
    refuse(
        &mut app,
        json!({"prompt": "x", "parallel": vec![step(&["a"]); 5]}),
    );
}

#[tokio::test]
async fn a_step_waits_while_an_earlier_step_or_another_tasks_running_step_shares_files() {
    let (mut app, _worker, _lead) = setup();
    let first = parallel_task(&mut app, json!([step(&["src/a.rs"]), step(&["src/a.rs"])]));
    let worker = reviewer(&app);
    run_step(&mut app, &first, 0, &worker);
    let other = parallel_task(&mut app, json!([step(&["src"])]));

    pump(&mut app);

    for (task_id, index, note) in [
        (
            &first,
            1,
            "overlaps step 1 on src/a.rs; runs after it".to_owned(),
        ),
        (
            &other,
            0,
            format!("overlaps step 1 of {first} on src/a.rs; waits for it"),
        ),
    ] {
        let parallel = task(&app, task_id).steps[index].parallel.clone().unwrap();
        assert_eq!(parallel.phase, Phase::Waiting, "{task_id}");
        assert_eq!(parallel.log, [note]);
        assert_eq!(task(&app, task_id).steps[index].state, StepState::Pending);
    }
    assert!(app.orbit_notifications.parallel.live.is_empty());
}

#[tokio::test]
async fn steps_that_cannot_launch_fail_and_the_task_reports_failure_once() {
    let (mut app, _worker, _lead) = setup();
    // Never the repository the tests run in: a directory that is no repository.
    let outside = TempDir::new("nogit");
    app.state.workspaces[0].identity_cwd = outside.0.clone();
    let task_id = parallel_task(&mut app, json!([step(&["a.rs"]), step(&["b.rs"])]));

    // Each step's checkout stage fails (the project is not a Git repository);
    // a step that found no room to start yet launches on a later tick.
    for _ in 0..50 {
        pump(&mut app);
        while let Ok(event) = app.event_rx.try_recv() {
            if let AppEvent::OrbitWorktreeReady(stage) = event {
                app.finish_orbit_worktree_start(*stage);
            }
        }
        if task(&app, &task_id).delivery != Delivery::AwaitingReport {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }

    let task = task(&app, &task_id);
    // Queued, or already on its way to the idle lead.
    assert_ne!(task.delivery, Delivery::AwaitingReport, "{:?}", task.steps);
    let report = task.report.as_ref().unwrap();
    assert_eq!(report.outcome, OrbitOutcome::Failed);
    assert!(report.summary.contains("Step 1") && report.summary.contains("Step 2"));
    assert!(task.steps.iter().all(|s| s.state == StepState::Failed));
    let log = &task.steps[0].parallel.as_ref().unwrap().log;
    assert!(log[0].contains("could not start"), "{log:?}");
    assert_eq!(
        app.orbit_notifications
            .tasks
            .iter()
            .filter(|t| t.report.is_some())
            .count(),
        1
    );
}

#[tokio::test]
async fn a_reported_step_saves_its_patch_and_its_checkout_is_removed() {
    let (mut app, _worker, _lead) = setup();
    let repo = Repo::new();
    let patches = repo.root.join("patches");
    app.state.worktree_directory = repo.root.join("checkouts");
    app.orbit_notifications.parallel.patch_dir = Some(patches.clone());
    let task_id = parallel_task(&mut app, json!([step(&["new.txt"])]));
    let worker = reviewer(&app);
    let checkout = crate::parallel_checkout::create_with(
        &repo.path(),
        &repo.path(),
        &repo.root.join("checkouts"),
        "repo",
        "par-test",
        &[],
        no_clone,
    )
    .unwrap();
    run_step(&mut app, &task_id, 0, &worker);
    task_mut(&mut app, &task_id).steps[0]
        .parallel
        .as_mut()
        .unwrap()
        .checkout = Some(checkout.clone());
    std::fs::write(checkout.path.join("new.txt"), "from the step\n").unwrap();
    std::fs::write(checkout.path.join("a.txt"), "a changed\n").unwrap();

    let sent = step_report(&app, &task_id, &worker, 0);
    let answer = report(&mut app, sent);

    assert!(answer.get("error").is_none(), "{answer}");
    assert_eq!(task(&app, &task_id).delivery, Delivery::AwaitingReport);
    // Queued, then delivered to the idle lead: once either way.
    pump_until(&mut app, &task_id, Delivery::Submitted).await;
    let done = task(&app, &task_id);
    assert_eq!(done.steps[0].state, StepState::Done);
    let patch = done.steps[0]
        .parallel
        .as_ref()
        .unwrap()
        .patch
        .clone()
        .unwrap();
    assert_eq!(patch.path, patches.join(format!("{task_id}-step1.patch")));
    let text = std::fs::read_to_string(&patch.path).unwrap();
    assert!(
        text.contains("+from the step") && text.contains("+a changed"),
        "{text}"
    );
    let report_text = done.report.as_ref().unwrap();
    assert_eq!(report_text.outcome, OrbitOutcome::Completed);
    assert!(
        report_text.summary.contains("Part done"),
        "{}",
        report_text.summary
    );
    assert!(report_text
        .summary
        .contains(&patch.path.display().to_string()));
    let mut files = report_text.changed_files.clone().unwrap();
    files.sort();
    assert_eq!(files, ["a.txt", "new.txt"]);
    // The checkout goes once the patch is safe; the original tree is untouched.
    for _ in 0..200 {
        pump(&mut app);
        if !checkout.path.exists() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert!(!checkout.path.exists());
    assert!(
        task(&app, &task_id).steps[0]
            .parallel
            .as_ref()
            .unwrap()
            .released
    );
    assert_eq!(
        std::fs::read_to_string(repo.path().join("a.txt")).unwrap(),
        "a\n"
    );
    assert!(!repo.path().join("new.txt").exists());

    // The same report again is acknowledged; a different one, or a handoff, is not.
    let sent = step_report(&app, &task_id, &worker, 0);
    let again = report(&mut app, sent);
    assert!(again.get("error").is_none(), "{again}");
    let mut changed = step_report(&app, &task_id, &worker, 0);
    changed.summary = "Something else".into();
    assert!(report(&mut app, changed).get("error").is_some());
    assert_eq!(
        app.orbit_notifications
            .tasks
            .iter()
            .filter(|t| t.report.is_some())
            .count(),
        1
    );
}

#[tokio::test]
async fn parallel_step_reports_must_come_from_the_step_s_own_agent() {
    let (mut app, _worker, _lead) = setup();
    let task_id = parallel_task(&mut app, json!([step(&["a.rs"])]));
    let worker = reviewer(&app);
    run_step(&mut app, &task_id, 0, &worker);
    let lead = lead_id(&app);

    let sent = step_report(&app, &task_id, &lead, 0);
    let stranger = report(&mut app, sent);
    let mut with_handoff = step_report(&app, &task_id, &worker, 0);
    with_handoff.handoff = Some(crate::api::schema::OrbitHandoff {
        done: "d".into(),
        next: "n".into(),
        changed_files: Vec::new(),
        known_issues: None,
        to: None,
        parallel: None,
    });
    let handoff = report(&mut app, with_handoff);
    let mut stepless = step_report(&app, &task_id, &worker, 0);
    stepless.step = None;
    let stepless = report(&mut app, stepless);

    assert!(stranger.get("error").is_some(), "{stranger}");
    assert!(handoff["error"]["message"]
        .as_str()
        .unwrap()
        .contains("without handoff"));
    assert!(stepless.get("error").is_some(), "{stepless}");
    assert_eq!(task(&app, &task_id).steps[0].state, StepState::Active);
    assert_eq!(task(&app, &task_id).delivery, Delivery::AwaitingReport);
}

#[tokio::test]
async fn cancelling_a_parallel_task_fails_its_steps_and_releases_them() {
    let (mut app, _worker, _lead) = setup();
    let task_id = parallel_task(&mut app, json!([step(&["a.rs"]), step(&["b.rs"])]));

    let cancelled = api(&mut app, "orbit.task_cancel", json!({"task_id": task_id}));

    assert!(cancelled.get("error").is_none(), "{cancelled}");
    assert!(task(&app, &task_id)
        .steps
        .iter()
        .all(|s| s.state == StepState::Failed));
    pump(&mut app);
    let released = task(&app, &task_id)
        .steps
        .iter()
        .all(|s| s.parallel.as_ref().unwrap().released);
    assert!(released);
    assert!(!task(&app, &task_id).holds_parallel_resources());
    assert_eq!(task(&app, &task_id).delivery, Delivery::Cancelled);
}

/// A task with one finished parallel step whose checkout still exists.
fn finished_step_with_checkout(
    app: &mut App,
    repo: &Repo,
) -> (String, crate::parallel_checkout::Checkout) {
    let task_id = parallel_task(app, json!([step(&["a.rs"])]));
    let checkout = crate::parallel_checkout::create_with(
        &repo.path(),
        &repo.path(),
        &repo.root.join("checkouts"),
        "repo",
        "par-done",
        &[],
        no_clone,
    )
    .unwrap();
    let step = &mut task_mut(app, &task_id).steps[0];
    step.state = StepState::Done;
    let parallel = step.parallel.as_mut().unwrap();
    parallel.phase = Phase::Capturing;
    parallel.checkout = Some(checkout.clone());
    (task_id, checkout)
}

fn parallel_of<'a>(
    app: &'a App,
    task_id: &str,
) -> &'a crate::app::orbit_notifications::parallel::Parallel {
    task(app, task_id).steps[0].parallel.as_ref().unwrap()
}

#[cfg(unix)]
#[tokio::test]
async fn a_checkout_that_cannot_be_removed_is_retried_and_only_then_released() {
    use std::os::unix::fs::PermissionsExt;
    let (mut app, _worker, _lead) = setup();
    let repo = Repo::new();
    app.state.worktree_directory = repo.root.join("checkouts");
    let (task_id, checkout) = finished_step_with_checkout(&mut app, &repo);
    let parent = checkout.path.parent().unwrap().to_path_buf();
    std::fs::set_permissions(&parent, std::fs::Permissions::from_mode(0o555)).unwrap();
    let blocked = std::fs::File::create(parent.join("probe")).is_err();
    if !blocked {
        // Running with rights that ignore the mode (root): nothing to prove.
        std::fs::set_permissions(&parent, std::fs::Permissions::from_mode(0o755)).unwrap();
        let _ = std::fs::remove_file(parent.join("probe"));
        return;
    }

    for _ in 0..200 {
        pump(&mut app);
        if !parallel_of(&app, &task_id).log.is_empty() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }

    let failed = parallel_of(&app, &task_id);
    assert!(!failed.released && !failed.removed);
    assert_eq!(failed.removals, 1);
    assert!(
        failed.log[0].starts_with("could not remove the checkout (try 1)"),
        "{:?}",
        failed.log
    );
    assert!(task(&app, &task_id).holds_parallel_resources());

    // Once removal can succeed, the next try (not before its delay) confirms it.
    std::fs::set_permissions(&parent, std::fs::Permissions::from_mode(0o755)).unwrap();
    pump(&mut app);
    assert!(
        !parallel_of(&app, &task_id).released,
        "retried before its delay"
    );
    app.orbit_notifications.parallel.removal_after.clear();
    for _ in 0..200 {
        pump(&mut app);
        if parallel_of(&app, &task_id).released {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert!(parallel_of(&app, &task_id).removed && parallel_of(&app, &task_id).released);
    assert!(!checkout.path.exists());
}

#[tokio::test]
async fn startup_retries_checkouts_that_were_never_confirmed_removed() {
    let (mut app, _worker, _lead) = setup();
    let repo = Repo::new();
    app.state.worktree_directory = repo.root.join("checkouts");
    let (task_id, checkout) = finished_step_with_checkout(&mut app, &repo);
    // As an older build, or a removal that failed for good, left it.
    {
        let parallel = task_mut(&mut app, &task_id).steps[0]
            .parallel
            .as_mut()
            .unwrap();
        parallel.released = true;
        parallel.removals = 5;
    }
    assert!(checkout.path.exists());

    let now = app.orbit_notifications.now_ms(Instant::now());
    app.reconcile_loaded_orbit_ledger(now);

    assert!(!parallel_of(&app, &task_id).released);
    assert_eq!(parallel_of(&app, &task_id).removals, 0);
    for _ in 0..200 {
        pump(&mut app);
        if parallel_of(&app, &task_id).removed {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert!(parallel_of(&app, &task_id).removed);
    assert!(!checkout.path.exists());
}

#[tokio::test]
async fn a_checkout_recorded_after_its_step_was_released_is_still_removed() {
    let (mut app, _worker, _lead) = setup();
    let repo = Repo::new();
    app.state.worktree_directory = repo.root.join("checkouts");
    let task_id = parallel_task(&mut app, json!([step(&["a.rs"])]));
    let checkout = crate::parallel_checkout::create_with(
        &repo.path(),
        &repo.path(),
        &repo.root.join("checkouts"),
        "repo",
        "par-late",
        &[],
        no_clone,
    )
    .unwrap();
    // The step already failed and released before its slow start finished.
    task_mut(&mut app, &task_id).steps[0].fail("timed out".into(), 1);
    task_mut(&mut app, &task_id).steps[0].released();

    app.record_parallel_checkout(&crate::events::ParallelLaunch {
        task_id: task_id.clone(),
        step: 0,
        label: "par-late".into(),
        apply: Vec::new(),
        checkout: Some(checkout.clone()),
    });

    assert!(!parallel_of(&app, &task_id).released);
    for _ in 0..200 {
        pump(&mut app);
        if parallel_of(&app, &task_id).removed {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert!(!checkout.path.exists());
}

#[tokio::test]
async fn a_corrupt_recorded_checkout_path_is_refused_untouched_and_not_retried() {
    let (mut app, _worker, _lead) = setup();
    let repo = Repo::new();
    app.state.worktree_directory = repo.root.join("checkouts");
    std::fs::create_dir_all(&app.state.worktree_directory).unwrap();
    let (task_id, checkout) = finished_step_with_checkout(&mut app, &repo);
    let precious = TempDir::new("precious");
    std::fs::write(precious.0.join("keep.txt"), "keep").unwrap();
    // A stale or corrupt ledger entry now points at an unrelated directory.
    task_mut(&mut app, &task_id).steps[0]
        .parallel
        .as_mut()
        .unwrap()
        .checkout
        .as_mut()
        .unwrap()
        .path = precious.0.clone();

    for _ in 0..200 {
        pump(&mut app);
        if parallel_of(&app, &task_id).released {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }

    let parallel = parallel_of(&app, &task_id);
    assert!(parallel.released && !parallel.removed);
    assert_eq!(parallel.removals, 1, "refusal is final, not retried");
    assert!(
        parallel.log[0].starts_with("refusing to remove"),
        "{:?}",
        parallel.log
    );
    assert_eq!(
        std::fs::read_to_string(precious.0.join("keep.txt")).unwrap(),
        "keep"
    );
    assert!(checkout.path.exists());
    crate::parallel_checkout::remove(&checkout, &repo.root.join("checkouts")).unwrap();
}

fn made_checkout(repo: &Repo, name: &str) -> crate::parallel_checkout::Checkout {
    crate::parallel_checkout::create_with(
        &repo.path(),
        &repo.path(),
        &repo.root.join("checkouts"),
        "repo",
        name,
        &[],
        no_clone,
    )
    .unwrap()
}

#[tokio::test]
async fn a_full_ledger_never_evicts_a_task_that_still_holds_a_checkout() {
    let (mut app, _worker, _lead) = setup();
    let repo = Repo::new();
    let (holder, checkout) = finished_step_with_checkout(&mut app, &repo);
    // Resolved, but its checkout is not confirmed removed yet.
    task_mut(&mut app, &holder).delivery = Delivery::Cancelled;
    let template = serde_json::to_value(task(&app, &holder)).unwrap();
    let copy = |id: &str| -> Task {
        let mut value = template.clone();
        value["task_id"] = json!(id);
        value["steps"] = json!([]);
        serde_json::from_value(value).unwrap()
    };
    for index in 0..CAPACITY - 1 {
        app.orbit_notifications
            .tasks
            .push(copy(&format!("filler-{index}")));
    }

    app.orbit_notifications.insert(copy("newcomer"));

    assert!(
        app.orbit_notifications.find(&holder).is_some(),
        "the task owning {} was evicted",
        checkout.path.display()
    );
    assert!(app.orbit_notifications.find("filler-0").is_none());
    crate::parallel_checkout::remove(&checkout, &repo.root.join("checkouts")).unwrap();
}

#[tokio::test]
async fn a_task_resolved_without_its_steps_still_releases_the_active_steps_checkout() {
    let (mut app, _worker, _lead) = setup();
    let repo = Repo::new();
    app.state.worktree_directory = repo.root.join("checkouts");
    let task_id = parallel_task(&mut app, json!([step(&["a.rs"])]));
    let checkout = made_checkout(&repo, "par-active");
    let worker = reviewer(&app);
    run_step(&mut app, &task_id, 0, &worker);
    task_mut(&mut app, &task_id).steps[0]
        .parallel
        .as_mut()
        .unwrap()
        .checkout = Some(checkout.clone());
    // A delivery path resolves the task while the step is still active.
    let stored = task_mut(&mut app, &task_id);
    stored.delivery = Delivery::Queued;
    stored.set_delivery(Delivery::Failed, 1, "delivery attempts exhausted");

    for _ in 0..200 {
        pump(&mut app);
        if !checkout.path.exists() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }

    assert!(
        !checkout.path.exists(),
        "the checkout of a step of a resolved task was left behind"
    );
    assert_eq!(task(&app, &task_id).steps[0].state, StepState::Failed);
}

#[tokio::test]
async fn startup_reclaims_marked_checkouts_no_ledger_entry_owns() {
    let (mut app, _worker, _lead) = setup();
    let repo = Repo::new();
    app.state.worktree_directory = repo.root.join("checkouts");
    let orphan = made_checkout(&repo, "par-orphan");
    let (task_id, owned) = finished_step_with_checkout(&mut app, &repo);
    // Not ours: no marker.
    let foreign = repo.root.join("checkouts/repo/par-foreign");
    std::fs::create_dir_all(&foreign).unwrap();
    std::fs::write(foreign.join("keep.txt"), "keep").unwrap();

    // Too young to be sure nothing is still creating it.
    app.reclaim_orphaned_checkouts(Duration::from_secs(3600));
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert!(orphan.path.exists());

    app.reclaim_orphaned_checkouts(Duration::ZERO);
    for _ in 0..200 {
        if !orphan.path.exists() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }

    assert!(!orphan.path.exists(), "the orphaned clone was kept");
    assert!(owned.path.exists(), "a clone the ledger owns was removed");
    assert!(foreign.join("keep.txt").exists());
    assert!(task(&app, &task_id).holds_parallel_resources());
    crate::parallel_checkout::remove(&owned, &repo.root.join("checkouts")).unwrap();
}

#[tokio::test]
async fn a_checkout_recorded_for_a_task_the_ledger_lost_is_removed_not_dropped() {
    let (mut app, _worker, _lead) = setup();
    let repo = Repo::new();
    app.state.worktree_directory = repo.root.join("checkouts");
    let checkout = made_checkout(&repo, "par-lost");

    app.record_parallel_checkout(&crate::events::ParallelLaunch {
        task_id: "task-that-is-gone".into(),
        step: 0,
        label: "par-lost".into(),
        apply: Vec::new(),
        checkout: Some(checkout.clone()),
    });

    for _ in 0..200 {
        if !checkout.path.exists() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert!(!checkout.path.exists());
}
