//! Parallel Codex steps and Codex's folder-trust dialog, against a real
//! `aster server` with the fake Codex of `orbit_support`: a step's prompt is
//! never typed into the dialog, and the checkout of a project the user trusts
//! in Codex is trusted with it and untrusted when it is removed.
#![cfg(unix)]

mod orbit_support;
pub mod support;

use std::path::{Path, PathBuf};

use orbit_support::*;
use serde_json::{json, Value};

/// A committed project under `<base>/work`, with checkouts under `<base>/worktrees`.
fn parallel_orbit(case: &str, codex_config: Option<&str>) -> (Orbit, PathBuf) {
    let mut orbit = Orbit::new(case);
    let project = orbit.base().join("work");
    orbit.git(&project, &["init", "-q"]);
    std::fs::create_dir_all(project.join("src")).unwrap();
    std::fs::write(project.join("src/a.txt"), "a\n").unwrap();
    orbit.git(&project, &["add", "."]);
    orbit.git(&project, &["commit", "-q", "-m", "init"]);
    if let Some(config) = codex_config {
        let codex = orbit.base().join("home/.codex");
        std::fs::create_dir_all(&codex).unwrap();
        std::fs::write(codex.join("config.toml"), config).unwrap();
    }
    orbit.append_config(&format!(
        "[worktrees]\ndirectory = \"{}\"\n",
        orbit.base().join("worktrees").display()
    ));
    orbit.start();
    orbit.wait_until("Orbit baseline", STATE_TIMEOUT, |orbit| {
        orbit.activity()["baseline_ready"] == true
    });
    (orbit, project)
}

fn assign_codex_step(orbit: &Orbit, model: &str) -> String {
    let response = orbit.request(
        "orbit.agent_assign",
        json!({
            "prompt": "Edit a",
            "parallel": [{"member_or_kind": "codex", "model": model, "prompt": "Edit a.txt", "files": ["src/a.txt"]}],
        }),
    );
    response["result"]["task_id"]
        .as_str()
        .unwrap_or_else(|| panic!("{response}"))
        .to_owned()
}

fn step(orbit: &Orbit, task_id: &str) -> Value {
    orbit.status(task_id)["task"]["steps"][0].clone()
}

fn agent_status(orbit: &Orbit, step: &Value) -> Value {
    let worker = step["parallel"]["worker_id"].as_str().unwrap_or_default();
    orbit.request("orbit.agent_status", json!({"name": worker}))["result"]["agent"]["agent_status"]
        .clone()
}

#[test]
fn a_parallel_step_waits_out_the_codex_trust_dialog_instead_of_typing_into_it() {
    let (orbit, _) = parallel_orbit("parallel-trust-dialog", None);
    orbit.set_mode("pt", "trust");
    let task_id = assign_codex_step(&orbit, "pt");

    orbit.wait_until(
        "the step's agent blocked on the dialog",
        STATE_TIMEOUT,
        |orbit| {
            let step = step(orbit, &task_id);
            step["parallel"]["worker_id"].is_string() && agent_status(orbit, &step) == "blocked"
        },
    );
    // Well past the shell and agent start-up: still nothing was typed.
    std::thread::sleep(std::time::Duration::from_secs(3));
    let state = orbit.base().join("state");
    assert!(
        !state.join("pt.lost").exists(),
        "a prompt went into the dialog"
    );
    assert!(orbit.prompts("pt").is_empty());
    assert_eq!(step(&orbit, &task_id)["parallel"]["phase"], "starting");

    // The user trusts the folder: the step's prompt follows.
    orbit.set_mode("pt", "idle");
    orbit.wait_until("the step prompted", STATE_TIMEOUT, |orbit| {
        orbit
            .prompts("pt")
            .iter()
            .any(|prompt| prompt.contains(&task_id))
    });
    assert!(!state.join("pt.lost").exists());
    assert_eq!(step(&orbit, &task_id)["parallel"]["phase"], "running");
}

#[test]
fn the_checkout_of_a_codex_trusted_project_is_trusted_until_it_is_removed() {
    let (orbit, project) = parallel_orbit("parallel-trust-entry", Some(""));
    // The project the server sees, as Codex would record it once trusted.
    let config_path = orbit.base().join("home/.codex/config.toml");
    let original = format!(
        "model = \"gpt-5-codex\"\n\n[projects.{}]\ntrust_level = \"trusted\"\n",
        toml_string(&project)
    );
    std::fs::write(&config_path, &original).unwrap();
    let task_id = assign_codex_step(&orbit, "pz");
    orbit.wait_until("the step prompted", STATE_TIMEOUT, |orbit| {
        orbit
            .prompts("pz")
            .iter()
            .any(|prompt| prompt.contains(&task_id))
    });
    let checkout: PathBuf = step(&orbit, &task_id)["parallel"]["checkout"]["path"]
        .as_str()
        .unwrap()
        .into();
    let trusted = std::fs::read_to_string(&config_path).unwrap();
    assert!(trusted.starts_with(&original), "{trusted}");
    assert!(
        trusted.contains(&format!("[projects.{}]", toml_string(&checkout))),
        "{trusted}"
    );

    let assignment = orbit.instruction("pz", &task_id).unwrap();
    let report = orbit.report_with(&assignment, "a edited", json!({"step": 0}));
    assert!(report.success, "{}", report.raw);
    orbit.wait_until("the checkout removed", STATE_TIMEOUT, |_| {
        !checkout.exists()
    });
    orbit.wait_until("the trust entry removed", STATE_TIMEOUT, |_| {
        std::fs::read_to_string(&config_path).unwrap() == original
    });
}

fn toml_string(path: &Path) -> String {
    format!("{:?}", path.display().to_string())
}
