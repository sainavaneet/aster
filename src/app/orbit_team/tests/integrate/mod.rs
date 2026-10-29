//! The integrate step: the patches of finished parallel steps are applied to
//! the main tree in order, the role's integrator finishes and tests, and
//! Polaris gets that one report.
use std::path::PathBuf;
use std::process::Command;
use std::time::{Duration, Instant};

use serde_json::{json, Value};

use super::*;
use crate::app::orbit_notifications::FinishedStep;
use crate::parallel_checkout::{capture_patch, create_with, remove, PatchSummary};

pub(super) struct Scratch {
    root: PathBuf,
}

impl Scratch {
    fn new() -> Self {
        let root = std::env::temp_dir().join(format!(
            "aster-integrate-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(root.join("repo")).unwrap();
        let scratch = Self {
            root: std::fs::canonicalize(root).unwrap(),
        };
        scratch.git(&["init", "-q"]);
        scratch.write("a.txt", "a\n");
        scratch.write("b.txt", "b\n");
        scratch.write("c.txt", "c\n");
        scratch.git(&["add", "."]);
        scratch.git(&["commit", "-q", "-m", "base"]);
        scratch
    }

    fn repo(&self) -> PathBuf {
        self.root.join("repo")
    }

    fn write(&self, file: &str, text: &str) {
        std::fs::write(self.repo().join(file), text).unwrap();
    }

    fn read(&self, file: &str) -> String {
        std::fs::read_to_string(self.repo().join(file)).unwrap()
    }

    fn git(&self, args: &[&str]) -> String {
        let output = Command::new("git")
            .args(["-c", "user.name=T", "-c", "user.email=t@example.invalid"])
            .args(["-c", "commit.gpgsign=false", "-C"])
            .arg(self.repo())
            .args(args)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "git {args:?}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8_lossy(&output.stdout).trim().to_owned()
    }

    fn patch_dir(&self) -> PathBuf {
        self.root.join("patches")
    }

    /// A step's patch: `edits` made in a fresh checkout of the repository
    /// (after applying `after`, the patches it builds on).
    fn patch(&self, name: &str, after: &[PathBuf], edits: &[(&str, &str)]) -> PatchSummary {
        let directory = self.root.join("checkouts");
        let checkout = create_with(
            &self.repo(),
            &self.repo(),
            &directory,
            "repo",
            name,
            after,
            |_, _| Err(std::io::Error::other("no clone")),
        )
        .unwrap();
        for (file, text) in edits {
            std::fs::write(checkout.path.join(file), text).unwrap();
        }
        let summary =
            capture_patch(&checkout, &self.patch_dir().join(format!("{name}.patch"))).unwrap();
        remove(&checkout, &directory).unwrap();
        summary
    }

    fn finished(&self, summary: &str, patch: Option<PatchSummary>) -> FinishedStep {
        FinishedStep {
            summary: summary.into(),
            patch,
            repo_root: self.repo(),
        }
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

pub(super) struct Team {
    app: App,
    /// Queues of `ba`, `bb`, and the reviewer `rv`.
    queues: Vec<Queue>,
    integrator: String,
}

/// Roles `builder` (ba, bb) and `reviewer` (rv), all running and idle.
pub(super) fn team() -> Team {
    let mut app = team_app();
    app.ensure_orbit_team_loaded();
    let mut queues = add_role(
        &mut app,
        spec("builder", &[("ba", "claude"), ("bb", "codex")]),
    );
    queues.extend(add_role(&mut app, spec("reviewer", &[("rv", "claude")])));
    app.supervise_orbit_team(Instant::now());
    for queue in &mut queues {
        drain(queue);
    }
    for (role, member) in [("builder", "ba"), ("builder", "bb"), ("reviewer", "rv")] {
        ready_member(&mut app, role, member);
    }
    let integrator = ready_member(&mut app, "reviewer", "rv");
    // Visibly at its idle prompt, as a real agent is when it can take a prompt.
    let terminal = app.state.terminals.get_mut(integrator.as_str()).unwrap();
    terminal.set_detected_state_with_screen_signals_at(
        Some(Agent::Claude),
        AgentState::Idle,
        false,
        true,
        false,
        false,
        Instant::now(),
    );
    Team {
        app,
        queues,
        integrator,
    }
}

pub(super) fn parallel_task(app: &mut App, parts: usize) -> String {
    parallel_task_on(app, parts, |n| format!("f{n}.txt"))
}

/// Parts that all declare the same file: each is built on the one before.
pub(super) fn overlapping_task(app: &mut App, parts: usize) -> String {
    parallel_task_on(app, parts, |_| "a.txt".to_owned())
}

fn parallel_task_on(app: &mut App, parts: usize, file: impl Fn(usize) -> String) -> String {
    let steps: Vec<Value> = (0..parts)
        .map(|n| json!({"member_or_kind": "claude", "prompt": format!("part {n}"), "files": [file(n)]}))
        .collect();
    let response = call(
        app,
        "orbit.agent_assign",
        json!({"prompt": "Build in parallel", "role": "builder", "parallel": steps}),
    );
    assert!(response.get("error").is_none(), "{response}");
    response["result"]["task_id"].as_str().unwrap().to_owned()
}

pub(super) fn status(app: &mut App, task_id: &str) -> Value {
    call(app, "orbit.task_status", json!({"task_id": task_id}))["result"]["task"].clone()
}

/// Ticks until the integrator's queue shows its prompt.
pub(super) async fn integrator_prompt(team: &mut Team) -> String {
    let mut text = String::new();
    for _ in 0..300 {
        team.app.pump_orbit_for_test();
        text.push_str(&drain(&mut team.queues[2]));
        if text.contains("[Aster assignment") {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    text
}

pub(super) fn integrate_step(task: &Value) -> Value {
    task["steps"]
        .as_array()
        .unwrap()
        .iter()
        .find(|s| s.get("integrate").is_some())
        .cloned()
        .unwrap()
}

pub(super) fn report(
    app: &mut App,
    task_id: &str,
    worker: &str,
    step: usize,
    extra: Value,
) -> Value {
    let token = app.orbit_last_issued_token();
    report_as(app, task_id, worker, step, token, extra)
}

pub(super) fn report_as(
    app: &mut App,
    task_id: &str,
    worker: &str,
    step: usize,
    token: Option<String>,
    extra: Value,
) -> Value {
    let mut params = json!({
        "task_id": task_id, "worker_id": worker, "token": token,
        "outcome": "completed", "summary": "integrated and tested", "step": step,
    });
    for (key, value) in extra.as_object().cloned().unwrap_or_default() {
        params[key] = value;
    }
    call(app, "orbit.agent_report", params)
}

pub(super) fn no_report_yet(task: &Value) -> bool {
    task["report"].is_null()
}

mod crash;
mod flow;
mod restart;
