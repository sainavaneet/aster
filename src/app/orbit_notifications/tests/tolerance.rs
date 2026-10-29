//! Ledger files from this build's format and from a newer build: every
//! readable entry loads, unreadable ones are kept aside with a warning (never a
//! reset), and fields this build does not know survive its saves.
use super::super::store;
use super::persistence::{saved, with_store};
use super::*;

fn fixture(name: &str) -> String {
    std::fs::read_to_string(format!(
        "{}/tests/fixtures/orbit/state/{name}",
        env!("CARGO_MANIFEST_DIR")
    ))
    .unwrap()
}

fn ledger_at(name: &str, fixture_name: &str) -> std::path::PathBuf {
    let path = store::test_path(name);
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(&path, fixture(fixture_name)).unwrap();
    path
}

fn saved_entry<'a>(
    file: &'a serde_json::Value,
    list: &str,
    task_id: &str,
) -> &'a serde_json::Value {
    file[list]
        .as_array()
        .unwrap()
        .iter()
        .find(|entry| entry["task_id"] == task_id)
        .unwrap_or_else(|| panic!("{task_id} missing from {list}: {file}"))
}

#[test]
fn this_builds_ledger_format_loads_completely() {
    let path = ledger_at("v1", "orbit-tasks.v1.json");
    let loaded = store::load(&path, 1).unwrap().unwrap();
    assert_eq!(loaded.lead_terminal_id.as_deref(), Some("term-lead"));
    assert_eq!(loaded.tasks.len(), 2);
    assert_eq!(loaded.tombstones.len(), 1);
    assert_eq!(loaded.skipped.len(), 0);
    let report = loaded.tasks[1].report.as_ref().unwrap();
    assert_eq!(report.checks.as_ref().unwrap().len(), 1);
    assert!(loaded.tasks[0].token_hash.is_some());
    let _ = std::fs::remove_dir_all(path.parent().unwrap());
}

#[test]
fn a_newer_ledger_skips_only_unreadable_entries_and_keeps_them_aside() {
    let path = ledger_at("newer-store", "orbit-tasks.newer.json");
    let loaded = store::load(&path, 77).unwrap().unwrap();
    let ids: Vec<_> = loaded
        .tasks
        .iter()
        .map(|task| task.task_id.as_str())
        .collect();
    assert_eq!(ids, ["task-1-1-1", "task-1-1-2"]);
    assert_eq!(loaded.tombstones.len(), 1);
    assert_eq!(
        loaded.skipped.tasks.len(),
        2,
        "unknown delivery and submission"
    );
    assert_eq!(loaded.skipped.tombstones.len(), 1, "unknown final state");
    // Unknown task fields are kept; report keys and facts it cannot read are
    // dropped, keeping the outcome and summary.
    assert_eq!(loaded.tasks[0].extra["priority"], 2);
    let report = loaded.tasks[1].report.as_ref().unwrap();
    assert_eq!(report.summary, "Tests pass");
    assert!(report.checks.is_none(), "unreadable checks are dropped");
    assert_eq!(loaded.tombstones[0].extra["archived"], true);
    assert!(path.exists(), "never quarantined");
    let _ = std::fs::remove_dir_all(path.parent().unwrap());
}

#[tokio::test]
async fn app_loads_a_newer_ledger_warns_and_preserves_extra_fields_on_save() {
    let path = ledger_at("newer-app", "orbit-tasks.newer.json");
    let (mut app, _worker, _lead) = setup();
    with_store(&mut app, &path);
    let activity = api(&mut app, "orbit.activity", json!({}));
    let warning = activity["result"]["ledger_error"].as_str().unwrap();
    assert!(
        warning.contains("3 unreadable Orbit ledger entries were skipped on load"),
        "{warning}"
    );
    assert!(warning.contains(".skipped-"), "{warning}");
    let tasks = activity["result"]["tasks"].as_array().unwrap();
    assert!(tasks.iter().any(|task| task["task_id"] == "task-1-1-2"));
    // The skipped entries are kept in a side file, byte for byte as JSON.
    let side = std::fs::read_dir(path.parent().unwrap())
        .unwrap()
        .filter_map(Result::ok)
        .find(|entry| entry.file_name().to_string_lossy().contains(".skipped-"))
        .expect("side file");
    let kept = std::fs::read_to_string(side.path()).unwrap();
    assert!(
        kept.contains("escalated") && kept.contains("superseded"),
        "{kept}"
    );
    // Restart reconciliation changed task-1-1-1 (its worker is gone) and saved:
    // the newer build's fields are still there.
    app.persist_orbit_ledger();
    let file = saved(&path);
    let task = saved_entry(&file, "tasks", "task-1-1-1");
    assert_eq!(task["priority"], 2, "{task}");
    assert_eq!(task["labels"], json!(["ui"]));
    assert_eq!(task["delivery"], "worker_lost");
    assert_eq!(
        saved_entry(&file, "tombstones", "task-0-0-1")["archived"],
        true
    );
    assert!(task["token_hash"].is_string(), "hashes still saved");
    let _ = std::fs::remove_dir_all(path.parent().unwrap());
}

#[tokio::test]
async fn a_wrongly_shaped_ledger_is_still_reset_and_kept_aside() {
    for (name, contents) in [
        ("tasks-not-list", r#"{"version":1,"tasks":5}"#),
        ("truncated", r#"{"version":1,"tasks":[{"task_id":"t""#),
    ] {
        let path = store::test_path(name);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, contents).unwrap();
        let Err(store::LoadError::Reset(error)) = store::load(&path, 5) else {
            panic!("{name}: expected a reset");
        };
        assert!(error.contains("started empty"), "{error}");
        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }
}
