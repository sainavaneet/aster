use serde_json::json;

use super::report::aggregate;
use super::*;

fn paths(items: &[&str]) -> Vec<String> {
    items.iter().map(|item| (*item).to_owned()).collect()
}

#[test]
fn declared_paths_overlap_when_equal_or_nested_and_never_by_name_prefix() {
    let overlap = |a: &[&str], b: &[&str]| shared_path(&paths(a), &paths(b)).is_some();
    assert!(overlap(&["src/a.rs"], &["src/a.rs"]));
    assert!(overlap(&["src"], &["src/a.rs"]));
    assert!(overlap(&["src/a.rs"], &["./src/"]));
    assert!(overlap(&["docs", "src/a.rs"], &["src/a.rs"]));
    assert!(overlap(&["."], &["anything/at/all"]));
    assert!(!overlap(&["src/a.rs"], &["src/ab.rs"]));
    assert!(!overlap(&["src/a"], &["src/b"]));
    assert!(!overlap(&["src"], &["srcs/a.rs"]));
    assert_eq!(
        shared_path(&paths(&["x", "src/a.rs"]), &paths(&["src"])).as_deref(),
        Some("src/a.rs")
    );
}

fn task_with_steps(steps: serde_json::Value) -> Task {
    serde_json::from_value(json!({
        "task_id": "t", "worker_id": "", "lead_id": "l",
        "submission": "submitted", "delivery": "awaiting_report", "error": null,
        "steps": steps,
    }))
    .unwrap()
}

fn parallel_step(
    index: usize,
    state: &str,
    outcome: Option<&str>,
    files: &[&str],
) -> serde_json::Value {
    let patch = outcome.map(|_| {
        json!({"path": format!("/patches/t-step{}.patch", index + 1), "bytes": 10,
               "files": files, "file_count": files.len()})
    });
    json!({
        "index": index, "member": format!("m{index}"), "kind": "claude", "state": state,
        "parallel": {
            "prompt": "p", "files": files, "phase": "capturing",
            "worker_id": format!("w{index}"), "outcome": outcome,
            "summary": outcome.map(|_| format!("summary {index}")),
            "patch": patch,
        },
    })
}

#[test]
fn final_report_takes_the_worst_outcome_and_lists_every_patch() {
    let completed = task_with_steps(json!([
        parallel_step(0, "done", Some("completed"), &["b.rs", "a.rs"]),
        parallel_step(1, "done", Some("completed"), &["a.rs", "c.rs"]),
    ]));
    let report = aggregate(&completed);
    assert_eq!(report.outcome, OrbitOutcome::Completed);
    assert_eq!(report.changed_files.unwrap(), ["a.rs", "b.rs", "c.rs"]);
    assert!(
        report.summary.contains("/patches/t-step1.patch"),
        "{}",
        report.summary
    );
    assert!(report.summary.contains("summary 1"), "{}", report.summary);
    assert_eq!(report.worker_id, "w1");

    let blocked = task_with_steps(json!([
        parallel_step(0, "done", Some("completed"), &["a.rs"]),
        parallel_step(1, "failed", Some("blocked"), &["b.rs"]),
    ]));
    let report = aggregate(&blocked);
    assert_eq!(report.outcome, OrbitOutcome::Blocked);
    assert!(report.blocked_on.unwrap().starts_with("step 2:"));

    let failed = task_with_steps(json!([
        parallel_step(0, "done", Some("completed"), &["a.rs"]),
        parallel_step(1, "failed", Some("blocked"), &["b.rs"]),
        parallel_step(2, "failed", None, &["c.rs"]),
    ]));
    assert_eq!(aggregate(&failed).outcome, OrbitOutcome::Failed);
}

#[test]
fn final_report_stays_within_the_summary_limit() {
    let mut steps = Vec::new();
    for index in 0..4 {
        let mut step = parallel_step(index, "done", Some("completed"), &["a.rs"]);
        step["parallel"]["summary"] = json!("x".repeat(4000));
        steps.push(step);
    }
    let report = aggregate(&task_with_steps(json!(steps)));
    assert!(report.summary.len() <= 4000, "{}", report.summary.len());
}

#[test]
fn a_task_without_parallel_steps_is_not_a_fanout() {
    let task = task_with_steps(json!([
        {"index": 0, "member": "a", "kind": "claude", "state": "active"}
    ]));
    assert!(!task.is_fanout() && !task.holds_parallel_resources());
    assert_eq!(task.parallel_line(), None);
}
