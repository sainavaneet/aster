//! Ledger persistence through the App, restart reconciliation, and corrupt files.
use super::super::deliver::POSSIBLE_DUPLICATE;
use super::super::lifecycle::STARTUP_GRACE_MS;
use super::super::pump::VERIFY_DELAY_MS;
use super::super::store;
use super::*;

pub(super) fn with_store(app: &mut App, path: &std::path::Path) {
    assert!(
        !app.orbit_notifications.loaded,
        "ledger used before test store"
    );
    app.orbit_notifications.store_path = Some(path.to_owned());
}

pub(super) fn saved(path: &std::path::Path) -> serde_json::Value {
    serde_json::from_slice(&std::fs::read(path).unwrap()).unwrap()
}

/// A new App over the same terminals and runtimes, as after a server restart.
pub(super) fn restart(previous: &mut App, path: &std::path::Path) -> App {
    let (_, rx) = tokio::sync::mpsc::unbounded_channel();
    let mut app = App::new(
        &crate::config::Config::default(),
        crate::app::AppPolicy::TEST,
        None,
        rx,
        crate::api::EventHub::default(),
    );
    std::mem::swap(&mut app.state, &mut previous.state);
    std::mem::swap(&mut app.terminal_runtimes, &mut previous.terminal_runtimes);
    with_store(&mut app, path);
    // Startup re-binds the existing Orbit tab, as the server does.
    app.prepare_orbit_startup();
    app
}

/// Two assignments: `awaiting` has no report; `delivering` was mid-delivery.
async fn persisted_pair(path: &std::path::Path) -> (App, OrbitAgentReportParams, String) {
    let (mut app, _worker, _lead) = setup();
    with_store(&mut app, path);
    // One assignment per worker at a time: `delivering` reports first.
    let delivering = assign(&mut app).await;
    assert!(report(&mut app, delivering.clone()).get("error").is_none());
    let awaiting = assign(&mut app).await;
    let task = task_mut(&mut app, &delivering.task_id);
    task.delivery = Delivery::Submitting;
    task.attempts = 1;
    app.persist_orbit_ledger();
    let file = saved(path);
    assert_eq!(file["version"], 1);
    assert_eq!(file["tasks"].as_array().unwrap().len(), 2);
    assert!(file["tasks"][0].get("submission_ack").is_none());
    (app, awaiting, delivering.task_id)
}

#[tokio::test]
async fn ledger_is_written_on_assign_and_report() {
    let path = store::test_path("app-writes");
    let (mut app, _worker, _lead) = setup();
    with_store(&mut app, &path);
    let result = assign(&mut app).await;
    assert_eq!(saved(&path)["tasks"][0]["delivery"], "awaiting_report");
    pump(&mut app);
    assert_eq!(saved(&path)["tasks"][0]["submission"], "submitted");
    assert!(report(&mut app, result).get("error").is_none());
    assert_eq!(saved(&path)["tasks"][0]["delivery"], "queued");
    let _ = std::fs::remove_dir_all(path.parent().unwrap());
}

#[tokio::test]
async fn restart_with_worker_present_keeps_task_and_accepts_late_report() {
    let path = store::test_path("restart-present");
    let (mut before, awaiting, delivering) = persisted_pair(&path).await;
    let mut app = restart(&mut before, &path);
    pump(&mut app);
    assert_eq!(
        task(&app, &awaiting.task_id).delivery,
        Delivery::AwaitingReport
    );
    assert_eq!(task(&app, &delivering).delivery, Delivery::Uncertain);
    assert_eq!(saved(&path)["tasks"][0]["delivery"], "uncertain");
    // Previously "Unknown assignment": the persisted task accepts the report.
    let response = report(&mut app, awaiting.clone());
    assert!(response.get("error").is_none(), "{response}");
    assert_eq!(task(&app, &awaiting.task_id).delivery, Delivery::Queued);
    let _ = std::fs::remove_dir_all(path.parent().unwrap());
}

#[tokio::test]
async fn restart_without_worker_marks_it_lost_and_queues_server_notice() {
    let path = store::test_path("restart-missing");
    let (mut before, awaiting, delivering) = persisted_pair(&path).await;
    before
        .state
        .terminals
        .remove(awaiting.worker_id.as_str())
        .unwrap();
    let mut app = restart(&mut before, &path);
    pump(&mut app);
    assert_eq!(task(&app, &awaiting.task_id).delivery, Delivery::WorkerLost);
    assert_eq!(task(&app, &delivering).delivery, Delivery::Uncertain);
    let notice = task(&app, &format!("{}/server-notice", awaiting.task_id));
    assert!(matches!(
        notice.delivery,
        Delivery::Queued | Delivery::Submitting
    ));
    assert_eq!(notice.origin, Some(ReportOrigin::Server));
    assert_eq!(saved(&path)["tasks"].as_array().unwrap().len(), 3);
    let _ = std::fs::remove_dir_all(path.parent().unwrap());
}

#[tokio::test]
async fn restart_turns_unacknowledged_assignment_into_uncertain_submission() {
    let path = store::test_path("restart-pending");
    let (mut before, awaiting, _) = persisted_pair(&path).await;
    task_mut(&mut before, &awaiting.task_id).submission = Submission::Pending;
    before.persist_orbit_ledger();
    let mut app = restart(&mut before, &path);
    pump_at(&mut app, Instant::now());
    assert_eq!(
        task(&app, &awaiting.task_id).submission,
        Submission::Uncertain
    );
    // Lifecycle checks wait out the startup grace before judging the worker.
    let now = app.orbit_notifications.now_ms(Instant::now());
    assert!(app.orbit_notifications.lifecycle_after + 1_000 >= now + STARTUP_GRACE_MS);
    let _ = std::fs::remove_dir_all(path.parent().unwrap());
}

#[tokio::test]
async fn corrupt_ledger_starts_empty_and_reports_ledger_error() {
    let path = store::test_path("app-corrupt");
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(&path, "{broken").unwrap();
    let (mut app, _worker, _lead) = setup();
    with_store(&mut app, &path);
    let activity = api(&mut app, "orbit.activity", json!({}));
    let error = activity["result"]["ledger_error"].as_str().unwrap();
    assert!(error.contains("started empty"), "{activity}");
    assert_eq!(activity["result"]["tasks"], json!([]));
    let kept = std::fs::read_dir(path.parent().unwrap())
        .unwrap()
        .filter_map(Result::ok)
        .any(|entry| entry.file_name().to_string_lossy().contains(".corrupt-"));
    assert!(kept, "corrupt ledger copy missing");
    // The server keeps working and writes a fresh ledger.
    assign(&mut app).await;
    assert_eq!(saved(&path)["tasks"].as_array().unwrap().len(), 1);
    let _ = std::fs::remove_dir_all(path.parent().unwrap());
}

#[tokio::test]
async fn delivery_requeued_after_restart_is_marked_as_a_possible_duplicate() {
    let path = store::test_path("restart-duplicate");
    let (mut before, _awaiting, delivering) = persisted_pair(&path).await;
    let mut app = restart(&mut before, &path);
    let start = Instant::now();
    pump_at(&mut app, start);
    assert_eq!(task(&app, &delivering).delivery, Delivery::Uncertain);
    pump_at(&mut app, start + Duration::from_millis(VERIFY_DELAY_MS + 1));
    let stored = task(&app, &delivering);
    assert!(stored.redelivered);
    assert_eq!(stored.recoveries, 1);
    assert_eq!(stored.attempts, 2);
    let text = super::super::deliver::delivery_text(stored, stored.report.as_ref().unwrap());
    // The marker stays the first line; the duplicate note follows it.
    let expected = format!("[orbit-delivery {delivering} #2]\n{POSSIBLE_DUPLICATE}\n");
    assert!(text.starts_with(&expected), "{text}");
    let file = saved(&path);
    let saved_task = file["tasks"]
        .as_array()
        .unwrap()
        .iter()
        .find(|task| task["task_id"] == delivering.as_str())
        .unwrap();
    assert_eq!(saved_task["redelivered"], true);
    // Ordinary tasks keep their existing shape.
    assert!(file["tasks"][1].get("redelivered").is_none());
    let _ = std::fs::remove_dir_all(path.parent().unwrap());
}

#[tokio::test]
async fn failed_save_keeps_the_ledger_dirty_and_is_retried() {
    let path = store::test_path("save-retry");
    let (mut app, _worker, _lead) = setup();
    with_store(&mut app, &path);
    api(&mut app, "orbit.activity", json!({}));
    // A path below a regular file cannot be created.
    let blocker = path.with_file_name("blocker");
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(&blocker, "file").unwrap();
    app.orbit_notifications.store_path = Some(blocker.join(store::FILE_NAME));
    let result = assign(&mut app).await;
    assert!(app.orbit_notifications.dirty, "failed save lost the change");
    let activity = api(&mut app, "orbit.activity", json!({}));
    assert!(activity["result"]["ledger_error"]
        .as_str()
        .unwrap()
        .contains("Cannot save"));
    app.orbit_notifications.store_path = Some(path.clone());
    pump_at(&mut app, Instant::now() + Duration::from_secs(2));
    assert!(!app.orbit_notifications.dirty);
    assert!(app.orbit_notifications.save_error.is_none());
    assert_eq!(saved(&path)["tasks"][0]["task_id"], result.task_id.as_str());
    let _ = std::fs::remove_dir_all(path.parent().unwrap());
}

#[tokio::test]
async fn unreadable_ledger_disables_saving_instead_of_overwriting() {
    let path = store::test_path("app-unreadable");
    std::fs::create_dir_all(&path).unwrap();
    let (mut app, _worker, _lead) = setup();
    with_store(&mut app, &path);
    let activity = api(&mut app, "orbit.activity", json!({}));
    let error = activity["result"]["ledger_error"].as_str().unwrap();
    assert!(error.contains("saving is disabled"), "{activity}");
    assert!(app.orbit_notifications.store_path.is_none());
    // The server keeps working in memory and leaves the original alone.
    assign(&mut app).await;
    assert!(path.is_dir());
    assert_eq!(std::fs::read_dir(&path).unwrap().count(), 0);
    let _ = std::fs::remove_dir_all(path.parent().unwrap());
}

/// The server's writer: a thread fed through a channel, results as events.
fn use_writer_thread(app: &mut App) {
    app.orbit_writer = crate::app::orbit_files::OrbitWriter::threaded(app.event_tx.clone());
}

#[tokio::test]
async fn ledger_transitions_in_one_tick_are_written_once_and_flushed_on_shutdown() {
    let path = store::test_path("writer-coalesce");
    let (mut app, _worker, _lead) = setup();
    with_store(&mut app, &path);
    let result = assign(&mut app).await;
    use_writer_thread(&mut app);
    // Three dirty transitions within one tick, each persisted at once.
    for nudged in 1..=3 {
        task_mut(&mut app, &result.task_id).nudged_at = Some(nudged);
        app.persist_orbit_ledger();
    }
    // What a shutdown does: everything pending is on disk when it returns.
    app.flush_orbit_files();
    assert_eq!(app.orbit_writer.writes(), 1, "coalesced into one write");
    assert_eq!(saved(&path)["tasks"][0]["nudged_at"], 3);
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(&path).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o600, "ledger is private");
    }
    app.drain_all_internal_events();
    assert!(app.orbit_notifications.save_error.is_none());
    let _ = std::fs::remove_dir_all(path.parent().unwrap());
}

#[tokio::test]
async fn writer_thread_save_errors_come_back_as_events_and_are_retried() {
    let path = store::test_path("writer-error");
    let (mut app, _worker, _lead) = setup();
    with_store(&mut app, &path);
    api(&mut app, "orbit.activity", json!({}));
    use_writer_thread(&mut app);
    let blocker = path.with_file_name("blocker");
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(&blocker, "file").unwrap();
    app.orbit_notifications.store_path = Some(blocker.join(store::FILE_NAME));
    let result = assign(&mut app).await;
    app.flush_orbit_files();
    assert!(
        app.orbit_notifications.save_error.is_none(),
        "not applied yet"
    );
    app.drain_all_internal_events();
    assert!(
        app.orbit_notifications.dirty,
        "failed save keeps the change"
    );
    let activity = api(&mut app, "orbit.activity", json!({}));
    let error = activity["result"]["ledger_error"].as_str().unwrap();
    assert!(error.contains("Cannot save Orbit task ledger"), "{error}");
    // Fixed path: the retry after one second succeeds.
    app.orbit_notifications.store_path = Some(path.clone());
    pump_at(&mut app, Instant::now() + Duration::from_secs(2));
    app.flush_orbit_files();
    app.drain_all_internal_events();
    assert!(app.orbit_notifications.save_error.is_none());
    assert_eq!(saved(&path)["tasks"][0]["task_id"], result.task_id.as_str());
    let _ = std::fs::remove_dir_all(path.parent().unwrap());
}

#[tokio::test]
async fn a_leftover_temporary_file_never_blocks_or_widens_a_save() {
    let path = store::test_path("leftover-tmp");
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    let tmp = path.with_file_name(format!("{}.tmp", store::FILE_NAME));
    std::fs::write(&tmp, "{\"version\":1,\"tasks\":[").unwrap();
    let (mut app, _worker, _lead) = setup();
    with_store(&mut app, &path);
    assign(&mut app).await;
    assert!(!tmp.exists(), "leftover replaced and renamed");
    assert_eq!(saved(&path)["tasks"].as_array().unwrap().len(), 1);
    let _ = std::fs::remove_dir_all(path.parent().unwrap());
}

#[tokio::test]
async fn a_failed_rename_surfaces_as_a_save_error_leaves_no_tmp_and_is_retried() {
    let path = store::test_path("rename-fail");
    let (mut app, _worker, _lead) = setup();
    with_store(&mut app, &path);
    let result = assign(&mut app).await;
    // Something else now occupies the ledger path: the rename cannot replace it.
    std::fs::remove_file(&path).unwrap();
    std::fs::create_dir_all(path.join("occupied")).unwrap();
    task_mut(&mut app, &result.task_id).nudged_at = Some(9);
    app.persist_orbit_ledger();
    let error = app.orbit_notifications.save_error.clone().unwrap();
    assert!(
        error.starts_with("Cannot save Orbit task ledger"),
        "{error}"
    );
    assert!(app.orbit_notifications.dirty);
    let tmp = path.with_file_name(format!("{}.tmp", store::FILE_NAME));
    assert!(!tmp.exists(), "partial temporary file left behind");
    std::fs::remove_dir_all(&path).unwrap();
    pump_at(&mut app, Instant::now() + Duration::from_secs(2));
    assert!(app.orbit_notifications.save_error.is_none());
    assert_eq!(saved(&path)["tasks"][0]["nudged_at"], 9);
    let _ = std::fs::remove_dir_all(path.parent().unwrap());
}
