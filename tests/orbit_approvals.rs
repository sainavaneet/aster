//! Orbit approval relay against a real `aster server`: a fake Claude worker shows
//! a Bash permission prompt (`blocked` mode of `tests/fixtures/orbit/claude`),
//! Polaris answers it through `orbit.agent_approve` when the config allows, and
//! the worker leaves `Blocked`. See `orbit_assignments.rs` for the fake agents.
#![cfg(unix)]

mod orbit_support;
pub mod support;

use orbit_support::*;
use serde_json::json;

const ALLOW: &str = "[polaris.approvals]\nlead_may_approve = true\n";
/// How long a worker may take to leave `Blocked` once answered; the request
/// itself waits at most 3 s, which a loaded machine can exceed.
const ANSWER_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10);

fn wait_status(orbit: &Orbit, name: &str, status: &str) {
    orbit.wait_until(&format!("{name} {status}"), STATE_TIMEOUT, |orbit| {
        orbit.request("orbit.agent_status", json!({"name": name}))["result"]["agent"]
            ["agent_status"]
            == status
    });
}

#[test]
fn allowed_lead_approves_a_blocked_worker_through_the_cli() {
    let mut orbit = Orbit::with_config("approve", ALLOW);
    orbit.start();
    let worker = orbit.start_worker("w1");
    let task = orbit.assign(&worker, "Clean the build");
    // Nothing to answer yet.
    let early = orbit.orbit_cli("agent-approve", &json!({"name": "w1"}));
    assert!(!early.success);
    assert!(early.raw.contains("no pending approval"), "{}", early.raw);

    orbit.set_mode("w1", "blocked");
    wait_status(&orbit, "w1", "blocked");
    orbit.wait_until("blocked worker in activity", STATE_TIMEOUT, |orbit| {
        let rows = orbit.activity()["blocked_workers"].clone();
        rows[0]["name"] == "w1" && rows[0]["task_id"] == task.task_id.as_str()
    });
    let answered = orbit.orbit_cli("agent-approve", &json!({"name": "w1"}));
    assert!(answered.success, "{}", answered.raw);
    orbit.wait_until("w1 leaves blocked", ANSWER_TIMEOUT, |orbit| {
        orbit.request("orbit.agent_status", json!({"name": "w1"}))["result"]["agent"]
            ["agent_status"]
            != "blocked"
    });
    assert_eq!(orbit.answers("w1"), ["approve"]);
    assert!(orbit.activity()["blocked_workers"]
        .as_array()
        .unwrap()
        .is_empty());
    // The assignment stays open for the worker's report.
    assert_eq!(orbit.delivery(&task.task_id), "awaiting_report");
}

#[test]
fn a_blocked_dialog_that_is_not_a_permission_prompt_is_refused() {
    let mut orbit = Orbit::with_config("approve-trust", ALLOW);
    orbit.start();
    orbit.start_worker("w1");
    orbit.set_mode("w1", "trust");
    wait_status(&orbit, "w1", "blocked");
    for method in ["orbit.agent_approve", "orbit.agent_deny"] {
        let refused = orbit.request(method, json!({"name": "w1"}));
        assert_eq!(
            refused["error"]["code"], "orbit_approval_unsupported_prompt",
            "{refused}"
        );
        assert!(
            refused["error"]["message"]
                .as_str()
                .unwrap()
                .contains("live_blocked_form"),
            "{refused}"
        );
    }
    assert!(orbit.answers("w1").is_empty());
    // The same worker on a permission prompt is answered.
    orbit.set_mode("w1", "blocked");
    orbit.wait_until("permission prompt approved", ANSWER_TIMEOUT, |orbit| {
        let answered = orbit.request("orbit.agent_approve", json!({"name": "w1"}));
        answered["error"]["code"] != "orbit_approval_unsupported_prompt"
            && !orbit.answers("w1").is_empty()
    });
    assert_eq!(orbit.answers("w1"), ["approve"]);
}

#[test]
fn default_config_refuses_and_sends_nothing() {
    let mut orbit = Orbit::new("approve-off");
    orbit.start();
    orbit.start_worker("w1");
    orbit.set_mode("w1", "blocked");
    wait_status(&orbit, "w1", "blocked");
    let refused = orbit.request("orbit.agent_deny", json!({"name": "w1"}));
    assert_eq!(
        refused["error"]["code"], "orbit_approval_not_allowed",
        "{refused}"
    );
    assert!(orbit.answers("w1").is_empty());
    let lead = orbit.request("orbit.agent_approve", json!({"name": "polaris"}));
    assert_eq!(lead["error"]["code"], "orbit_lead_refused", "{lead}");
}

/// Startup grace plus the 30 s blocked threshold, with room for a loaded machine.
const RELAY_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(90);

#[test]
fn started_worker_on_a_startup_dialog_without_an_assignment_is_relayed() {
    let mut orbit = Orbit::new("blocked-unassigned");
    orbit.start();
    // The fake starts on Claude's folder-trust dialog, before any assignment.
    orbit.set_mode("w1", "trust");
    let started = orbit.result(
        "orbit.agent_start",
        json!({"name": "w1", "kind": "claude", "model": "w1", "permission_mode": "manual"}),
    );
    assert_eq!(started["worker"]["permission_mode"], "manual", "{started}");
    wait_status(&orbit, "w1", "blocked");
    let status = orbit.result("orbit.agent_status", json!({"name": "w1"}));
    assert_eq!(status["worker"]["permission_mode"], "manual", "{status}");
    orbit.wait_until(
        "unassigned blocked worker tracked",
        STATE_TIMEOUT,
        |orbit| {
            let rows = orbit.activity()["blocked_workers"].clone();
            rows[0]["name"] == "w1" && rows[0]["task_id"].is_null() && rows[0]["since_ms"].is_u64()
        },
    );
    let marker = "is blocked on a dialog and has no assignment";
    orbit.wait_until("lead receives the worker notice", RELAY_TIMEOUT, |orbit| {
        !orbit.lead_prompts_with(marker).is_empty()
    });
    let prompt = orbit.lead_prompts_with(marker).remove(0);
    assert!(prompt.contains("origin: server"), "{prompt}");
    assert!(prompt.contains("Worker w1 ("), "{prompt}");
    assert!(
        prompt.contains("Do you trust the files in this folder?"),
        "{prompt}"
    );
    let activity = orbit.activity();
    let notice = activity["tasks"]
        .as_array()
        .unwrap()
        .iter()
        .find(|task| {
            task["task_id"]
                .as_str()
                .is_some_and(|id| id.starts_with("worker-") && id.ends_with("/server-notice"))
        })
        .cloned()
        .unwrap_or_else(|| panic!("no worker notice in {activity}"));
    assert!(notice["notice_for"].is_null(), "{notice}");
    assert_eq!(notice["origin"], "server");
    assert_eq!(notice["report"]["outcome"], "blocked");
    assert_eq!(notice["report"]["task_id"], notice["task_id"]);
    // One notice per blocked stretch.
    assert_eq!(orbit.lead_prompts_with(marker).len(), 1);
    assert_eq!(activity["blocked_workers"][0]["name"], "w1", "{activity}");
}

/// A minimal Jev on loopback: every `POST /v1/systemone` gets the recorded
/// approval of `date`; requests are kept for inspection.
fn fake_jev() -> (String, std::sync::Arc<std::sync::Mutex<Vec<String>>>) {
    use std::io::{BufRead, BufReader, Read, Write};
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let endpoint = format!("http://{}", listener.local_addr().unwrap());
    let answer = std::fs::read(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/tests/fixtures/decisions/approve_date.json"
    ))
    .unwrap();
    let requests = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
    let seen = std::sync::Arc::clone(&requests);
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut stream) = stream else { return };
            let mut reader = BufReader::new(stream.try_clone().unwrap());
            let (mut head, mut length) = (String::new(), 0);
            loop {
                let mut line = String::new();
                if reader.read_line(&mut line).unwrap_or(0) == 0 || line == "\r\n" {
                    break;
                }
                if let Some(value) = line.to_ascii_lowercase().strip_prefix("content-length:") {
                    length = value.trim().parse().unwrap_or(0);
                }
                head.push_str(&line);
            }
            let mut body = vec![0; length];
            let _ = reader.read_exact(&mut body);
            seen.lock()
                .unwrap()
                .push(format!("{head}\r\n{}", String::from_utf8_lossy(&body)));
            let reply = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                answer.len()
            );
            let _ = stream.write_all(reply.as_bytes());
            let _ = stream.write_all(&answer);
        }
    });
    (endpoint, requests)
}

/// The decision layer end to end through the real server: a worker blocked on
/// a Bash prompt, rules undecided, the provider (a loopback Jev) approves, the
/// approval keys reach the worker, and the lead's notice carries the decision.
#[test]
fn an_approving_provider_answers_a_blocked_worker_end_to_end() {
    let (endpoint, requests) = fake_jev();
    let mut orbit = Orbit::new("decision-e2e");
    let key = orbit.base().join("home/jev.key");
    std::fs::write(&key, "sk-e2e-test-key\n").unwrap();
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&key, std::fs::Permissions::from_mode(0o600)).unwrap();
    }
    orbit.append_config(&format!(
        "{ALLOW}[polaris.decisions]\nprovider = \"jev\"\nendpoint = \"{endpoint}\"\napi_key_file = \"{}\"\ntimeout_ms = 5000\n[polaris.decisions.approvals]\nenabled = true\n",
        key.display()
    ));
    orbit.start();
    let worker = orbit.start_worker("w1");
    let task = orbit.assign(&worker, "Print the date");
    std::fs::write(orbit.base().join("state/w1.command"), "date").unwrap();
    orbit.set_mode("w1", "blocked");
    wait_status(&orbit, "w1", "blocked");
    orbit.wait_until(
        "provider approval sent to the worker",
        RELAY_TIMEOUT,
        |orbit| orbit.answers("w1") == ["approve"],
    );
    let marker = format!("{}/blocked-", task.task_id);
    orbit.wait_until("lead receives the decided notice", RELAY_TIMEOUT, |orbit| {
        !orbit.lead_prompts_with(&marker).is_empty()
    });
    let prompt = orbit.lead_prompts_with(&marker).remove(0);
    assert!(prompt.contains("Decision: approve (jev, p="), "{prompt}");
    let requests = requests.lock().unwrap();
    let call = requests
        .iter()
        .find(|request| request.starts_with("POST /v1/systemone"))
        .expect("provider called");
    assert!(
        call.contains("Authorization: Bearer sk-e2e-test-key"),
        "{call}"
    );
    let body = call.split("\r\n\r\n").nth(1).unwrap();
    assert!(body.contains("date"), "{body}");
    assert!(!body.contains("sk-e2e-test-key"), "key in the request body");
    // The assignment stays open for the worker's report.
    assert_eq!(orbit.delivery(&task.task_id), "awaiting_report");
}
