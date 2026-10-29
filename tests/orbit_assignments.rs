//! End-to-end Orbit assignment and report flow against a real `aster server`,
//! driven only through the JSON API socket, the real `aster orbit` CLI, and
//! `aster orbit-mcp` over stdio.
//!
//! Fake agents: Polaris requires a Claude lead, so each server's PATH starts with
//! a directory holding `tests/fixtures/orbit/claude`, a bash script that Aster's
//! process identification recognises as Claude (`bash .../claude`) and whose
//! screen the bundled Claude manifest classifies: an idle prompt box (visible
//! idle) or a `✻ Working… (… esc to interrupt)` line. Polaris launches it exactly
//! as it would launch Claude; workers come from `orbit.agent_start`. The fake
//! logs every submitted prompt, so delivery to the lead is observed at the PTY,
//! and reports its session ID like Claude's SessionStart hook, so a restored
//! session resumes both agents in their original terminals. No model is run.
//!
//! Each test owns a unique session name, config home, and socket (see
//! `orbit_support`), so tests run in parallel without sharing a server.
#![cfg(unix)]

mod orbit_support;
pub mod support;

use std::io::{BufRead, BufReader, Write};
use std::process::Stdio;
use std::thread;
use std::time::Duration;

use orbit_support::*;
use serde_json::{json, Value};

/// Polaris tick is 250 ms; watching this long covers a dozen delivery ticks.
const QUIET_PERIOD: Duration = Duration::from_secs(3);
/// Longer than the 3 s spacing between prompts to one lead.
const DUPLICATE_WINDOW: Duration = Duration::from_millis(4_500);

fn marker(task_id: &str) -> String {
    format!("[orbit-delivery {task_id} #1]")
}

fn started(case: &str) -> Orbit {
    let mut orbit = Orbit::new(case);
    orbit.start();
    orbit
}

#[test]
fn report_from_worker_cli_is_delivered_once_to_idle_lead() {
    let orbit = started("happy");
    let worker = orbit.start_worker("w1");
    let assignment = orbit.assign(&worker, "Summarise the README");
    let status = orbit.status(&assignment.task_id)["task"].clone();
    assert_eq!(status["delivery"], "awaiting_report");
    assert_eq!(status["lead_id"], orbit.activity()["lead"]["terminal_id"]);
    assert!(
        !status.to_string().contains(&assignment.token),
        "API exposes the token"
    );

    let report = orbit.report(&assignment, "README summarised: 3 sections");
    assert!(report.success, "{}", report.raw);
    orbit.wait_delivery(&assignment.task_id, "submitted");

    let marker = marker(&assignment.task_id);
    let prompt = orbit.wait_lead_prompt(&marker);
    assert!(prompt.contains("README summarised: 3 sections"), "{prompt}");
    assert!(prompt.contains("Orbit worker report"), "{prompt}");
    assert!(!prompt.contains(&assignment.token), "token leaked to lead");
    assert!(orbit.read_pane(&orbit.lead_pane()).contains(&marker));
    thread::sleep(DUPLICATE_WINDOW);
    assert_eq!(orbit.lead_prompts_with(&assignment.task_id).len(), 1);

    let task = orbit.status(&assignment.task_id)["task"].clone();
    assert_eq!(task["delivery"], "submitted");
    assert_eq!(task["attempts"], 1);
    assert_eq!(task["origin"], "worker");
    assert_eq!(task["report"]["summary"], "README summarised: 3 sections");
    assert!(matches!(
        task["submission"].as_str(),
        Some("submitted" | "confirmed_by_report")
    ));
}

#[test]
fn structured_report_facts_reach_the_lead_and_activity() {
    let orbit = started("facts");
    let worker = orbit.start_worker("w1");
    let assignment = orbit.assign(&worker, "Fix the parser");
    let prompt = orbit.prompts("w1").join("\n");
    assert!(prompt.contains("\"tests_run\":false"), "{prompt}");
    assert!(prompt.contains("blocked_on"), "{prompt}");

    let facts = json!({
        "tests_run": false,
        "checks": [
            {"name": "cargo clippy", "status": "passed"},
            {"name": "cargo test", "status": "skipped", "detail": "no time"}
        ],
        "changed_files": ["src/parser.rs"],
    });
    let report = orbit.report_with(&assignment, "Parser fixed", facts);
    assert!(report.success, "{}", report.raw);
    orbit.wait_delivery(&assignment.task_id, "submitted");

    let marker = marker(&assignment.task_id);
    let prompt = orbit.wait_lead_prompt(&marker);
    for expected in [
        "\"tests_run\":false",
        "Facts:",
        "tests_run: false",
        "checks: 1 passed, 1 skipped",
        "- cargo test: skipped — no time",
        "- src/parser.rs",
    ] {
        assert!(prompt.contains(expected), "missing {expected}: {prompt}");
    }
    let lead_pane = orbit.lead_pane();
    orbit.wait_until("facts on the lead screen", STATE_TIMEOUT, |orbit| {
        orbit
            .read_pane(&lead_pane)
            .contains("checks: 1 passed, 1 skipped")
    });
    let screen = orbit.read_pane(&lead_pane);
    for expected in ["Facts:", "tests_run: false"] {
        assert!(screen.contains(expected), "missing {expected}: {screen}");
    }

    let task = orbit.status(&assignment.task_id)["task"].clone();
    assert_eq!(task["report"]["tests_run"], false);
    assert_eq!(task["report"]["checks"].as_array().unwrap().len(), 2);
    let activity = orbit.cli(&["orbit", "activity"]);
    let text = String::from_utf8_lossy(&activity.stdout);
    assert!(activity.status.success(), "{text}");
    assert!(
        text.contains("tests=no  checks=1 passed, 1 skipped"),
        "{text}"
    );
}

#[test]
fn mixed_kind_workers_report_to_the_same_lead() {
    let orbit = started("mixed");
    let kinds = orbit.activity()["kinds"].clone();
    for kind in ["claude", "codex"] {
        let entry = kinds
            .as_array()
            .unwrap()
            .iter()
            .find(|entry| entry["kind"] == kind)
            .unwrap_or_else(|| panic!("{kind} missing from kinds: {kinds}"));
        assert_eq!(entry["installed"], true, "{entry}");
        assert_eq!(entry["executable"], kind, "{entry}");
    }
    let refused = orbit.request(
        "orbit.agent_start",
        json!({"name": "g1", "kind": "gemini", "model": "pro"}),
    );
    assert_eq!(
        refused["error"]["code"], "orbit_worker_not_allowed",
        "{refused}"
    );

    let claude = orbit.start_kind_in("claude", "editor", None);
    let split = orbit.result("orbit.split", json!({"direction": "right"}));
    let pane = split["pane"]["pane_id"]
        .as_str()
        .or_else(|| split["pane_id"].as_str())
        .unwrap_or_else(|| panic!("split result has no pane: {split}"))
        .to_owned();
    let codex = orbit.start_kind_in("codex", "reviewer", Some(&pane));
    let agents = orbit.activity()["agents"].clone();
    let provider = |name: &str| {
        agents
            .as_array()
            .unwrap()
            .iter()
            .find(|agent| agent["name"] == name)
            .map(|agent| agent["agent"].clone())
    };
    assert_eq!(provider("editor"), Some(json!("claude")), "{agents}");
    assert_eq!(provider("reviewer"), Some(json!("codex")), "{agents}");

    let edit = orbit.assign(&claude, "Apply the edit");
    let review = orbit.assign(&codex, "Review the edit");
    assert!(orbit.report(&edit, "edit applied").success);
    assert!(orbit.report(&review, "review clean").success);
    let edit_prompt = orbit.wait_lead_prompt(&marker(&edit.task_id));
    let review_prompt = orbit.wait_lead_prompt(&marker(&review.task_id));
    assert!(edit_prompt.contains("edit applied"), "{edit_prompt}");
    assert!(review_prompt.contains("review clean"), "{review_prompt}");
    orbit.wait_delivery(&edit.task_id, "submitted");
    orbit.wait_delivery(&review.task_id, "submitted");
}

#[test]
fn restart_mid_task_restores_ledger_and_terminals_then_delivers() {
    let mut orbit = started("restart");
    let worker = orbit.start_worker("w1");
    let lead = orbit.activity()["lead"]["terminal_id"].clone();
    let assignment = orbit.assign(&worker, "Survive a restart");
    orbit.stop();

    let ledger = std::fs::read_to_string(orbit.ledger_path()).unwrap();
    assert!(ledger.contains(&assignment.task_id));
    assert!(
        !ledger.contains(&assignment.token),
        "ledger stores the raw token"
    );

    orbit.start();
    let resumed = orbit.wait_worker("w1");
    assert_eq!(resumed.terminal_id, worker.terminal_id);
    assert_eq!(orbit.activity()["lead"]["terminal_id"], lead);
    assert_eq!(orbit.delivery(&assignment.task_id), "awaiting_report");

    let report = orbit.report(&assignment, "done after restart");
    assert!(report.success, "{}", report.raw);
    let prompt = orbit.wait_lead_prompt(&marker(&assignment.task_id));
    assert!(prompt.contains("done after restart"), "{prompt}");
    orbit.wait_delivery(&assignment.task_id, "submitted");
}

#[test]
fn worker_pane_missing_after_restart_is_lost_and_lead_gets_server_notice() {
    let mut orbit = started("lost");
    let split = orbit.result("orbit.split", json!({"direction": "right"}));
    let pane = split["pane"]["pane_id"]
        .as_str()
        .or_else(|| split["pane_id"].as_str())
        .unwrap_or_else(|| panic!("split result has no pane: {split}"))
        .to_owned();
    let worker = orbit.start_worker_in("w1", Some(&pane));
    let assignment = orbit.assign(&worker, "Never reported");
    orbit.result("pane.close", json!({"pane_id": worker.pane_id}));
    // Lifecycle checks wait 15 s after startup, so the loss is first seen on restart.
    assert_eq!(orbit.delivery(&assignment.task_id), "awaiting_report");
    orbit.stop();

    orbit.start();
    orbit.wait_delivery(&assignment.task_id, "worker_lost");
    let task = orbit.status(&assignment.task_id)["task"].clone();
    assert_eq!(task["error"], "worker terminal missing after restart");

    let notice_id = format!("{}/server-notice", assignment.task_id);
    let prompt = orbit.wait_lead_prompt(&marker(&notice_id));
    assert!(prompt.contains("Orbit server notice"), "{prompt}");
    assert!(prompt.contains("was lost before reporting"), "{prompt}");
    orbit.wait_delivery(&notice_id, "submitted");
    let notice = orbit.status(&notice_id)["task"].clone();
    assert_eq!(notice["origin"], "server");
    assert_eq!(notice["notice_for"], json!(assignment.task_id));
}

#[test]
fn busy_lead_defers_delivery_until_idle() {
    let orbit = started("busy");
    let worker = orbit.start_worker("w1");
    let assignment = orbit.assign(&worker, "Report while the lead works");
    orbit.set_busy(LEAD, true);
    orbit.wait_until("lead working", STATE_TIMEOUT, |orbit| {
        orbit.activity()["lead"]["state"] == "working"
    });

    let report = orbit.report(&assignment, "deferred result");
    assert!(report.success, "{}", report.raw);
    let before = orbit.prompts(LEAD).len();
    thread::sleep(QUIET_PERIOD);
    let task = orbit.status(&assignment.task_id)["task"].clone();
    assert_eq!(task["delivery"], "queued");
    assert_eq!(task["attempts"], 0, "busy time counted as an attempt");
    assert_eq!(orbit.prompts(LEAD).len(), before, "typed into a busy lead");
    assert_eq!(orbit.activity()["lead"]["queued_reports"], 1);

    orbit.set_busy(LEAD, false);
    let prompt = orbit.wait_lead_prompt(&marker(&assignment.task_id));
    assert!(prompt.contains("deferred result"), "{prompt}");
    orbit.wait_delivery(&assignment.task_id, "submitted");
    assert_eq!(orbit.status(&assignment.task_id)["task"]["attempts"], 1);
}

#[test]
fn claude_workers_pre_allow_their_report_and_tokens_live_in_private_files() {
    let orbit = started("preallow");
    let worker = orbit.start_worker("w1");
    let argv = orbit.agent_argv("w1");
    assert!(allows_only_report(&argv), "{argv:?}");
    let lead = orbit.agent_argv(LEAD);
    assert!(!lead.iter().any(|arg| arg.contains("allowed")), "{lead:?}");
    let split = orbit.split_right();
    let codex = orbit.start_kind_in("codex", "x1", Some(&split));
    let codex_argv = orbit.agent_argv("x1");
    assert!(
        !codex_argv.iter().any(|arg| arg.contains("allowed")),
        "{codex_argv:?}"
    );

    let assignment = orbit.assign(&worker, "Report from the file");
    let path = orbit.token_path(&assignment.task_id);
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(&path).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o600);
    }
    // The CLI finds the token by task_id alone, with no token environment.
    let report = orbit.report(&assignment, "reported via the token file");
    assert!(report.success, "{}", report.raw);
    orbit.wait_delivery(&assignment.task_id, "submitted");
    assert!(
        path.exists(),
        "a reported task keeps its file for identical retries"
    );

    let cancelled = orbit.assign(&codex, "Cancel me");
    let cancelled_path = orbit.token_path(&cancelled.task_id);
    assert!(cancelled_path.exists());
    orbit.result("orbit.task_cancel", json!({"task_id": cancelled.task_id}));
    // Kept while the ledger knows the task, so a late report learns it was cancelled.
    assert!(cancelled_path.exists());
    let late = orbit.report(&cancelled, "too late");
    assert_eq!(late.error_message(), CANCELLED);
}

#[test]
fn cli_reports_retry_conflict_bad_token_and_out_of_scope() {
    let orbit = started("dupes");
    let worker = orbit.start_worker("w1");
    let assignment = orbit.assign(&worker, "Report twice");
    let lead = orbit.activity()["lead"]["terminal_id"]
        .as_str()
        .unwrap()
        .to_owned();

    let wrong_token = Assignment {
        token: "00".repeat(16),
        ..assignment.clone()
    };
    let rejected = orbit.report_api(&wrong_token, "forged");
    assert_eq!(rejected["error"]["message"], REPORT_REJECTED);
    let unknown = orbit.report(
        &Assignment {
            task_id: "task-unknown".into(),
            ..assignment.clone()
        },
        "guess",
    );
    assert_eq!(unknown.error_message(), REPORT_REJECTED, "messages differ");

    let outside = orbit.report(
        &Assignment {
            worker_id: lead,
            ..assignment.clone()
        },
        "from the lead tab",
    );
    assert!(!outside.success);
    assert_eq!(outside.error_message(), OUT_OF_SCOPE);
    assert_eq!(orbit.delivery(&assignment.task_id), "awaiting_report");

    let first = orbit.report(&assignment, "final answer");
    assert!(first.success, "{}", first.raw);
    orbit.wait_lead_prompt(&marker(&assignment.task_id));
    let retry = orbit.report(&assignment, "final answer");
    assert!(retry.success, "identical retry refused: {}", retry.raw);
    let conflict = orbit.report(&assignment, "a different answer");
    assert!(!conflict.success);
    assert_eq!(conflict.error_message(), CONFLICTING_REPORT);

    thread::sleep(DUPLICATE_WINDOW);
    assert_eq!(orbit.lead_prompts_with(&assignment.task_id).len(), 1);
    let task = orbit.status(&assignment.task_id)["task"].clone();
    assert_eq!(task["report"]["summary"], "final answer");
    assert_eq!(task["delivery"], "submitted");
}

#[test]
fn cancelled_assignment_rejects_worker_report() {
    let orbit = started("cancel");
    let worker = orbit.start_worker("w1");
    let assignment = orbit.assign(&worker, "Abandon me");

    let cancelled = orbit.orbit_cli("task-cancel", &json!({"task_id": assignment.task_id}));
    assert!(cancelled.success, "{}", cancelled.raw);
    assert_eq!(cancelled.body["result"]["delivery"], "cancelled");

    let report = orbit.report(&assignment, "too late");
    assert!(!report.success);
    assert_eq!(report.error_message(), CANCELLED);
    thread::sleep(QUIET_PERIOD);
    assert!(orbit.lead_prompts_with(&assignment.task_id).is_empty());
    assert_eq!(orbit.delivery(&assignment.task_id), "cancelled");
}

#[test]
fn full_ledger_refuses_until_a_report_resolves_and_evicts_to_tombstone() {
    let orbit = started("capacity");
    let worker = orbit.start_worker("w1");
    // A worker takes one assignment at a time, so fill the ledger with reports
    // that stay queued while the lead works.
    orbit.set_busy(LEAD, true);
    orbit.wait_until("lead working", STATE_TIMEOUT, |orbit| {
        orbit.activity()["lead"]["state"] == "working"
    });
    let first = orbit.assign(&worker, "capacity 1");
    assert!(orbit.report(&first, "first result").success);
    for index in 2..=64 {
        let assignment = orbit.assign(&worker, &format!("capacity {index}"));
        let report = orbit.report(&assignment, &format!("result {index}"));
        assert!(report.success, "assignment {index}: {}", report.raw);
    }
    let full = orbit.orbit_cli(
        "agent-assign",
        &json!({"name": worker.name, "prompt": "capacity 65"}),
    );
    assert!(!full.success);
    assert!(
        full.error_message().starts_with(LEDGER_FULL),
        "{}",
        full.raw
    );

    orbit.set_busy(LEAD, false);
    orbit.wait_delivery(&first.task_id, "submitted");
    let next = orbit.assign_raw(&worker.name, "capacity 65 after a result");
    assert!(next.get("error").is_none(), "{next}");

    let evicted = orbit.status(&first.task_id);
    assert!(evicted["task"].is_null(), "{evicted}");
    assert_eq!(evicted["tombstone"]["final_delivery"], "submitted");
    let late = orbit.report(&first, "first result");
    assert!(late.success, "identical late retry refused: {}", late.raw);
    assert_eq!(orbit.lead_prompts_with(&first.task_id).len(), 1);
}

fn mcp_call(lines: &mut impl BufRead, input: &mut impl Write, message: Value) -> Value {
    writeln!(input, "{message}").unwrap();
    input.flush().unwrap();
    let mut line = String::new();
    lines.read_line(&mut line).unwrap();
    serde_json::from_str(&line).unwrap_or_else(|_| panic!("bad MCP frame: {line}"))
}

fn tool_body(response: &Value) -> Value {
    let text = response["result"]["content"][0]["text"].as_str().unwrap();
    serde_json::from_str(text).unwrap()
}

#[test]
fn orbit_mcp_stdio_lists_task_tools_and_round_trips_status() {
    let orbit = started("mcp");
    let worker = orbit.start_worker("w1");
    let mut mcp = orbit
        .command(env!("CARGO_BIN_EXE_aster"))
        .arg("orbit-mcp")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    let mut input = mcp.stdin.take().unwrap();
    let mut output = BufReader::new(mcp.stdout.take().unwrap());

    let init = mcp_call(
        &mut output,
        &mut input,
        json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":{}}),
    );
    assert_eq!(init["result"]["serverInfo"]["name"], "aster-orbit");
    writeln!(
        input,
        "{}",
        json!({"jsonrpc":"2.0","method":"notifications/initialized"})
    )
    .unwrap();
    let list = mcp_call(
        &mut output,
        &mut input,
        json!({"jsonrpc":"2.0","id":2,"method":"tools/list"}),
    );
    assert_eq!(list["id"], 2, "notification produced a response");
    let names: Vec<_> = list["result"]["tools"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|tool| tool["name"].as_str())
        .collect();
    for tool in [
        "orbit_agent_assign",
        "orbit_agent_report",
        "orbit_task_status",
        "orbit_task_cancel",
        "orbit_activity",
    ] {
        assert!(names.contains(&tool), "{tool} missing from {names:?}");
    }

    let call = |id: u64, name: &str, arguments: Value| json!({"jsonrpc":"2.0","id":id,"method":"tools/call","params":{"name":name,"arguments":arguments}});
    let assigned = mcp_call(
        &mut output,
        &mut input,
        call(
            3,
            "orbit_agent_assign",
            json!({"name": worker.name, "prompt": "via MCP"}),
        ),
    );
    assert_eq!(assigned["result"]["isError"], false, "{assigned}");
    let task_id = tool_body(&assigned)["result"]["task_id"]
        .as_str()
        .unwrap()
        .to_owned();
    let status = mcp_call(
        &mut output,
        &mut input,
        call(4, "orbit_task_status", json!({"task_id": task_id})),
    );
    assert_eq!(status["result"]["isError"], false, "{status}");
    let body = tool_body(&status);
    assert_eq!(body["result"]["task"]["task_id"], json!(task_id));
    assert_eq!(
        body["result"]["task"]["worker_id"],
        json!(worker.terminal_id)
    );
    assert_eq!(
        body["result"]["task"]["delivery"],
        orbit.status(&task_id)["task"]["delivery"]
    );
    let activity = mcp_call(
        &mut output,
        &mut input,
        call(5, "orbit_activity", json!({})),
    );
    assert_eq!(activity["result"]["isError"], false, "{activity}");
    let unknown = mcp_call(
        &mut output,
        &mut input,
        call(6, "orbit_task_status", json!({"task_id": "task-unknown"})),
    );
    assert_eq!(unknown["result"]["isError"], true, "{unknown}");

    drop(input);
    assert!(mcp.wait().unwrap().success());
}

fn lead_focused(orbit: &Orbit) -> Value {
    orbit.result("agent.get", json!({"target": "polaris"}))["agent"]["focused"].clone()
}

fn stale_pane_ids(orbit: &Orbit) -> Vec<String> {
    orbit.activity()["stale_panes"]
        .as_array()
        .cloned()
        .unwrap_or_default()
        .iter()
        .filter_map(|pane| pane["pane_id"].as_str().map(str::to_owned))
        .collect()
}

#[test]
fn force_stop_cancels_assignment_and_closes_worker_pane_without_moving_focus() {
    let orbit = started("stop");
    let pane = orbit.split_right();
    let worker = orbit.start_worker_in("w1", Some(&pane));
    let assignment = orbit.assign(&worker, "Long review");
    let focused = lead_focused(&orbit);

    let refused = orbit.orbit_cli("agent-stop", &json!({"name": "w1"}));
    assert!(!refused.success, "{}", refused.raw);
    assert!(
        refused.error_message().contains(&assignment.task_id),
        "{}",
        refused.raw
    );
    assert!(orbit.pane(&worker.pane_id).is_some());

    let stopped = orbit.orbit_cli("agent-stop", &json!({"name": "w1", "force": true}));
    assert!(stopped.success, "{}", stopped.raw);
    let result = &stopped.body["result"];
    assert_eq!(result["stopped"], true, "{}", stopped.raw);
    assert_eq!(result["closed"], true);
    assert_eq!(result["pane_id"], json!(worker.pane_id));
    assert_eq!(result["cancelled_tasks"], json!([assignment.task_id]));
    assert!(result.get("pending").is_none(), "{}", stopped.raw);
    assert_eq!(orbit.delivery(&assignment.task_id), "cancelled");
    assert!(orbit.pane(&worker.pane_id).is_none());
    assert_eq!(lead_focused(&orbit), focused);
    // The worker's terminal is gone, so a late report is refused as out of scope.
    let report = orbit.report(&assignment, "too late");
    assert_eq!(report.error_message(), OUT_OF_SCOPE);
}

#[test]
fn stopping_a_worker_that_already_reported_keeps_its_report_for_the_lead() {
    let orbit = started("stop-reported");
    let pane = orbit.split_right();
    let worker = orbit.start_worker_in("w1", Some(&pane));
    let assignment = orbit.assign(&worker, "Report, then stop");
    orbit.set_busy(LEAD, true);
    orbit.wait_until("lead working", STATE_TIMEOUT, |orbit| {
        orbit.activity()["lead"]["state"] == "working"
    });
    assert!(
        orbit
            .report(&assignment, "reported before the stop")
            .success
    );
    assert_eq!(orbit.delivery(&assignment.task_id), "queued");

    // The report is not open work: no force needed, and force cancels nothing.
    let stopped = orbit.orbit_cli("agent-stop", &json!({"name": "w1", "force": true}));
    assert!(stopped.success, "{}", stopped.raw);
    let result = &stopped.body["result"];
    assert_eq!(result["cancelled_tasks"], json!([]), "{}", stopped.raw);
    assert_eq!(result["pending_deliveries"], json!([assignment.task_id]));
    assert!(orbit.pane(&worker.pane_id).is_none());
    assert_eq!(orbit.delivery(&assignment.task_id), "queued");

    orbit.set_busy(LEAD, false);
    let prompt = orbit.wait_lead_prompt(&marker(&assignment.task_id));
    assert!(prompt.contains("reported before the stop"), "{prompt}");
    orbit.wait_delivery(&assignment.task_id, "submitted");
}

#[test]
fn start_reuses_an_idle_worker_of_the_same_kind_and_model() {
    let orbit = started("reuse");
    let worker = orbit.start_worker("w1");
    let reused = orbit.result(
        "orbit.agent_start",
        json!({"name": "w2", "kind": "claude", "model": "w1"}),
    );
    assert_eq!(reused["reused"], true, "{reused}");
    assert_eq!(reused["pane_id"], json!(worker.pane_id));
    assert_eq!(reused["agent"]["terminal_id"], json!(worker.terminal_id));
    let fresh = orbit.request(
        "orbit.agent_start",
        json!({"name": "w2", "kind": "claude", "model": "w1", "reuse": false}),
    );
    assert_ne!(fresh["result"]["reused"], true, "{fresh}");
}

#[test]
fn exited_worker_pane_is_labelled_listed_and_closed() {
    let orbit = started("exited");
    let pane = orbit.split_right();
    let worker = orbit.start_worker_in("w1", Some(&pane));
    let pid = std::fs::read_to_string(orbit.base().join("state/w1.pid")).unwrap();
    let killed = std::process::Command::new("kill")
        .arg(pid.trim())
        .status()
        .unwrap();
    assert!(killed.success());
    orbit.wait_until("exited worker listed as stale", STATE_TIMEOUT, |orbit| {
        stale_pane_ids(orbit) == [worker.pane_id.clone()]
    });
    let label = orbit.pane(&worker.pane_id).unwrap()["label"].clone();
    assert_eq!(label, "w1 (exited)");
    let inspect = orbit.result("orbit.inspect", json!({}));
    let entry = inspect["panes"]
        .as_array()
        .unwrap()
        .iter()
        .find(|pane| pane["pane_id"] == json!(worker.pane_id))
        .cloned()
        .unwrap();
    assert_eq!(entry["stale"], true, "{entry}");

    let closed = orbit.orbit_cli("pane-close", &json!({"pane_id": worker.pane_id}));
    assert!(closed.success, "{}", closed.raw);
    assert!(orbit.pane(&worker.pane_id).is_none());
    assert!(stale_pane_ids(&orbit).is_empty());
}

#[test]
fn isolated_worker_runs_in_its_own_worktree_and_stop_removes_it() {
    let mut orbit = Orbit::new("worktree");
    let project = orbit.base().join("work");
    let worktrees = orbit.base().join("worktrees");
    orbit.git(&project, &["init", "-q"]);
    std::fs::write(project.join("README.md"), "orbit\n").unwrap();
    orbit.git(&project, &["add", "README.md"]);
    orbit.git(&project, &["commit", "-q", "-m", "init"]);
    orbit.append_config(&format!(
        "[worktrees]\ndirectory = \"{}\"\n",
        worktrees.display()
    ));
    orbit.start();
    orbit.wait_until("Orbit baseline", STATE_TIMEOUT, |orbit| {
        orbit.activity()["baseline_ready"] == true
    });

    let started = orbit.result(
        "orbit.agent_start",
        json!({"name": "iso", "kind": "claude", "model": "iso", "worktree": true}),
    );
    let path = started["worktree"]["path"].as_str().unwrap().to_owned();
    let branch = started["worktree"]["branch"].as_str().unwrap().to_owned();
    assert!(
        path.starts_with(&worktrees.display().to_string()),
        "{started}"
    );
    assert!(branch.starts_with("worktree/"), "{started}");
    let worker = orbit.wait_worker("iso");
    assert_eq!(started["pane_id"], json!(worker.pane_id));
    orbit.wait_until("worker shell inside the worktree", STATE_TIMEOUT, |orbit| {
        orbit.pane(&worker.pane_id).is_some_and(|pane| {
            pane["cwd"]
                .as_str()
                .is_some_and(|cwd| cwd.starts_with(&path))
        })
    });
    assert!(!orbit
        .git(&project, &["branch", "--list", &branch])
        .trim()
        .is_empty());
    let status = orbit.result("orbit.agent_status", json!({"name": "iso"}));
    assert_eq!(status["worker"]["worktree"]["branch"], json!(branch));

    let assignment = orbit.assign(&worker, "Edit in isolation");
    let prompt = orbit
        .prompts("iso")
        .into_iter()
        .find(|prompt| prompt.contains(&assignment.task_id))
        .unwrap();
    assert!(
        prompt.contains(&branch) && prompt.contains(&path),
        "{prompt}"
    );
    assert!(orbit.report(&assignment, "edited").success);
    orbit.wait_delivery(&assignment.task_id, "submitted");

    // Uncommitted work survives `force`; only discard_changes removes it.
    std::fs::write(std::path::Path::new(&path).join("draft.txt"), "wip\n").unwrap();
    let refused = orbit.orbit_cli(
        "agent-stop",
        &json!({"name": "iso", "remove_worktree": true, "force": true}),
    );
    assert!(!refused.success, "{}", refused.raw);
    assert!(
        refused.error_message().contains("discard_changes"),
        "{}",
        refused.raw
    );
    assert!(std::path::Path::new(&path).join("draft.txt").exists());
    assert!(orbit.pane(&worker.pane_id).is_some());

    let stopped = orbit.orbit_cli(
        "agent-stop",
        &json!({"name": "iso", "remove_worktree": true, "discard_changes": true}),
    );
    assert!(stopped.success, "{}", stopped.raw);
    assert_eq!(
        stopped.body["result"]["worktree_removed"], true,
        "{}",
        stopped.raw
    );
    assert!(!std::path::Path::new(&path).exists());
    assert!(!orbit
        .git(&project, &["worktree", "list", "--porcelain"])
        .contains(&path));
    // Discarding an Orbit-created checkout also drops the branch it created.
    assert_eq!(
        stopped.body["result"]["branch_deleted"], true,
        "{}",
        stopped.raw
    );
    assert!(orbit
        .git(&project, &["branch", "--list", &branch])
        .trim()
        .is_empty());
}

#[test]
fn stop_keeping_the_pane_ends_the_agent_and_leaves_an_unlabelled_shell() {
    let orbit = started("keep");
    let pane = orbit.split_right();
    let worker = orbit.start_worker_in("w1", Some(&pane));
    let stopped = orbit.orbit_cli("agent-stop", &json!({"name": "w1", "close_pane": false}));
    assert!(stopped.success, "{}", stopped.raw);
    assert_eq!(stopped.body["result"]["stopped"], true, "{}", stopped.raw);
    assert_eq!(stopped.body["result"]["closed"], false);
    let shell = orbit.pane(&worker.pane_id).expect("pane kept");
    assert!(shell["agent"].is_null(), "{shell}");
    assert!(shell["label"].is_null(), "{shell}");
    assert!(stale_pane_ids(&orbit).is_empty());
    // The kept shell accepts a new worker.
    orbit.start_kind_in("codex", "w2", Some(&worker.pane_id));
}

#[cfg(unix)]
fn mode(path: &std::path::Path) -> u32 {
    use std::os::unix::fs::PermissionsExt;
    std::fs::metadata(path)
        .unwrap_or_else(|error| panic!("{}: {error}", path.display()))
        .permissions()
        .mode()
        & 0o777
}

/// Token directory 0700, token, ledger, and binding files 0600; a crafted task
/// ID (traversal, absolute path) never makes the CLI read outside the token
/// directory and is refused like any unknown task.
#[test]
fn state_files_are_private_and_crafted_task_ids_stay_inside_the_token_directory() {
    let orbit = started("private-files");
    let worker = orbit.start_worker("w1");
    let assignment = orbit.assign(&worker, "Check the token files");
    let token = orbit.token_path(&assignment.task_id);
    assert_eq!(mode(&token), 0o600);
    assert_eq!(mode(token.parent().unwrap()), 0o700);
    let ledger = orbit.ledger_path();
    orbit.wait_until("ledger written", STATE_TIMEOUT, |orbit| {
        std::fs::read_to_string(orbit.ledger_path())
            .is_ok_and(|text| text.contains(&assignment.task_id))
    });
    assert_eq!(mode(&ledger), 0o600);
    assert_eq!(mode(&ledger.with_file_name("orbit-binding.json")), 0o600);
    for task_id in [
        "../orbit-tasks.json",
        "../../../../../../etc/passwd",
        "/etc/passwd",
        "..",
        "task-1/../../x",
    ] {
        let result = orbit.orbit_cli(
            "agent-report",
            &json!({
                "task_id": task_id,
                "worker_id": worker.terminal_id,
                "outcome": "completed",
                "summary": "crafted",
            }),
        );
        assert!(!result.success, "{task_id}: {}", result.raw);
        assert_eq!(result.error_message(), REPORT_REJECTED, "{task_id}");
    }
    // The genuine report still goes through.
    assert!(orbit.report(&assignment, "token files checked").success);
    orbit.wait_delivery(&assignment.task_id, "submitted");
}

/// Claude Code collapses a large paste into `[Pasted text #N +M lines]`; the
/// fake lead does the same. Reports to it are still delivered exactly once.
#[test]
fn a_lead_that_collapses_pastes_still_gets_each_report_once() {
    let orbit = started("collapsed-paste");
    std::fs::write(orbit.base().join("state/lead.collapse"), "200").unwrap();
    let worker = orbit.start_worker("w1");
    let assignment = orbit.assign(&worker, "Write a long summary");
    let summary = "line of findings\n".repeat(40);
    assert!(orbit.report(&assignment, &summary).success);
    orbit.wait_delivery(&assignment.task_id, "submitted");
    orbit.wait_until("collapsed paste on the lead", STATE_TIMEOUT, |orbit| {
        orbit
            .read_pane(&orbit.lead_pane())
            .contains("[Pasted text #")
    });
    assert!(!orbit
        .read_pane(&orbit.lead_pane())
        .contains(&marker(&assignment.task_id)));
    thread::sleep(QUIET_PERIOD);
    assert_eq!(orbit.lead_prompts_with(&assignment.task_id).len(), 1);
}
