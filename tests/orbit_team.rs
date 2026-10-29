//! End-to-end Orbit team roles against a real `aster server` with the fake
//! Claude and Codex agents of `orbit_support`: a role owns a tab between the
//! Polaris tab and the Orbit tab, its members are resumed after a restart in
//! their own panes, and a member whose agent exits is relaunched there.
#![cfg(unix)]

mod orbit_support;
pub mod support;

use std::time::Duration;

use orbit_support::*;
use serde_json::{json, Value};

/// The member relaunch delay is 5 s; allow for detection and the fake's startup.
const RELAUNCH_TIMEOUT: Duration = Duration::from_secs(20);

fn reviewer() -> Value {
    json!({
        "name": "reviewer",
        "area": "Code review",
        "prompt": "Review strictly; never edit files.",
        "members": [
            {"name": "rc", "kind": "claude", "model": "rc"},
            {"name": "rx", "kind": "codex", "model": "rx"}
        ]
    })
}

/// A started server with the reviewer role defined and both members idle.
fn staffed(case: &str) -> (Orbit, Worker, Worker) {
    let mut orbit = Orbit::new(case);
    orbit.start();
    let defined = orbit.orbit_cli("team-define", &reviewer());
    assert!(defined.success, "{}", defined.raw);
    assert_eq!(
        defined.body["result"]["created_tab"], true,
        "{}",
        defined.raw
    );
    let rc = orbit.wait_worker("rc");
    let rx = orbit.wait_worker("rx");
    (orbit, rc, rx)
}

fn marker(task_id: &str) -> String {
    format!("[orbit-delivery {task_id} #1]")
}

#[test]
fn defined_role_owns_a_tab_after_polaris_and_takes_assignments_by_role() {
    let (orbit, rc, rx) = staffed("team-define");
    assert_eq!(orbit.polaris_tabs(), ["Polaris", "Reviewer", "Orbit"]);
    let role = orbit.team_role("reviewer");
    let members = role["members"].as_array().unwrap();
    assert_eq!(members.len(), 2, "{role}");
    assert_eq!(members[0]["pane_id"], json!(rc.pane_id));
    assert_eq!(members[1]["pane_id"], json!(rx.pane_id));
    assert!(
        members.iter().all(|member| member["state"] == "running"),
        "{role}"
    );
    assert_ne!(rc.pane_id, rx.pane_id);
    // Only the Claude member pre-allows its report command; never the lead.
    assert!(
        allows_only_report(&orbit.agent_argv("rc")),
        "{:?}",
        orbit.agent_argv("rc")
    );
    assert!(!orbit
        .agent_argv("rx")
        .iter()
        .any(|arg| arg.contains("allowed")));
    assert!(!orbit
        .agent_argv(LEAD)
        .iter()
        .any(|arg| arg.contains("allowed")));
    let inspect = orbit.result("orbit.inspect", json!({"role": "reviewer"}));
    assert_eq!(inspect["panes"].as_array().unwrap().len(), 2, "{inspect}");
    // Polaris kept focus.
    let lead = orbit.result("agent.get", json!({"target": "polaris"}))["agent"].clone();
    assert_eq!(lead["focused"], true, "{lead}");

    let response = orbit.request(
        "orbit.agent_assign",
        json!({"role": "reviewer", "kind": "codex", "prompt": "Review the parser"}),
    );
    let task_id = response["result"]["task_id"]
        .as_str()
        .unwrap_or_else(|| panic!("{response}"))
        .to_owned();
    assert_eq!(response["result"]["worker_id"], json!(rx.terminal_id));
    let mut found = None;
    orbit.wait_until("assignment at the codex member", STATE_TIMEOUT, |orbit| {
        found = orbit.instruction("rx", &task_id);
        found.is_some()
    });
    let prompt = orbit
        .prompts("rx")
        .into_iter()
        .find(|prompt| prompt.contains(&task_id))
        .unwrap();
    assert!(prompt.contains("You act for the Reviewer role"), "{prompt}");
    let report = orbit.report(&found.unwrap(), "parser reviewed");
    assert!(report.success, "{}", report.raw);
    let delivered = orbit.wait_lead_prompt(&marker(&task_id));
    assert!(delivered.contains("parser reviewed"), "{delivered}");

    let refused = orbit.orbit_cli("agent-stop", &json!({"name": "rc"}));
    assert!(!refused.success, "{}", refused.raw);
    assert_eq!(
        refused.body["error"]["code"], "orbit_team_member_protected",
        "{}",
        refused.raw
    );
    assert!(orbit.agent_pid("rc").is_some());
    assert_eq!(orbit.wait_worker("rc").terminal_id, rc.terminal_id);
}

#[test]
fn killed_member_is_relaunched_in_its_own_pane() {
    let (orbit, rc, _) = staffed("team-relaunch");
    let pid = orbit.agent_pid("rc").unwrap();
    assert!(std::process::Command::new("kill")
        .arg(&pid)
        .status()
        .unwrap()
        .success());
    orbit.wait_until("rc relaunched", RELAUNCH_TIMEOUT, |orbit| {
        orbit.agent_pid("rc").is_some_and(|current| current != pid)
            && orbit.team_role("reviewer")["members"][0]["state"] == "running"
    });
    let relaunched = orbit.wait_worker("rc");
    assert_eq!(relaunched.pane_id, rc.pane_id);
    // The relaunch continues the session the member reported, with its role text.
    let screen = orbit.read_pane(&rc.pane_id);
    assert!(screen.contains("--resume rc"), "{screen}");
    assert!(screen.contains("--append-system-prompt"), "{screen}");
    assert_eq!(relaunched.terminal_id, rc.terminal_id);
    assert!(allows_only_report(&orbit.agent_argv("rc")));
    let role = orbit.team_role("reviewer");
    assert_eq!(role["members"][0]["name"], "rc");
    assert_eq!(role["members"][0]["terminal_id"], json!(rc.terminal_id));
    assert_eq!(orbit.polaris_tabs(), ["Polaris", "Reviewer", "Orbit"]);
}

#[test]
fn restart_restores_the_role_tab_and_resumed_members_report_to_the_lead() {
    let (mut orbit, rc, rx) = staffed("team-restart");
    let tab_id = orbit.team_role("reviewer")["tab_id"].clone();
    let response = orbit.request(
        "orbit.agent_assign",
        json!({"role": "reviewer", "kind": "claude", "prompt": "Survive a restart"}),
    );
    let task_id = response["result"]["task_id"]
        .as_str()
        .unwrap_or_else(|| panic!("{response}"))
        .to_owned();
    let mut assignment = None;
    orbit.wait_until("assignment at the claude member", STATE_TIMEOUT, |orbit| {
        assignment = orbit.instruction("rc", &task_id);
        assignment.is_some()
    });
    orbit.stop();
    let roster = std::fs::read_to_string(orbit.team_path()).unwrap();
    assert!(
        roster.contains("\"version\": 1") && roster.contains("\"rc\""),
        "{roster}"
    );

    orbit.start();
    let resumed = orbit.wait_worker("rc");
    let codex = orbit.wait_worker("rx");
    assert_eq!(resumed.terminal_id, rc.terminal_id);
    assert_eq!(resumed.pane_id, rc.pane_id);
    assert_eq!(codex.terminal_id, rx.terminal_id);
    // Claude resumed natively; the restored plan kept its model and role text.
    let screen = orbit.read_pane(&resumed.pane_id);
    assert!(screen.contains("--resume rc"), "{screen}");
    assert!(screen.contains("--append-system-prompt"), "{screen}");
    let argv = orbit.agent_argv("rc");
    assert!(argv.iter().any(|arg| arg == "--resume"), "{argv:?}");
    assert!(allows_only_report(&argv), "{argv:?}");
    let screen = orbit.read_pane(&codex.pane_id);
    assert!(screen.contains("developer_instructions="), "{screen}");
    assert_eq!(orbit.polaris_tabs(), ["Polaris", "Reviewer", "Orbit"]);
    assert_eq!(orbit.team_role("reviewer")["tab_id"], tab_id);
    assert_eq!(orbit.delivery(&task_id), "awaiting_report");
    let report = orbit.report(&assignment.unwrap(), "reviewed after restart");
    assert!(report.success, "{}", report.raw);
    let delivered = orbit.wait_lead_prompt(&marker(&task_id));
    assert!(delivered.contains("reviewed after restart"), "{delivered}");
}

const PINNED_ACTOR: &str = "\n[[polaris.team]]\nname = \"actor\"\narea = \"Edits\"\nprompt = \"Edit carefully.\"\n\n[[polaris.team.members]]\nname = \"ac\"\nkind = \"claude\"\nmodel = \"ac\"\n";

#[test]
fn a_pinned_config_role_joins_a_session_roster_first_and_cannot_be_removed() {
    // A session whose roster already holds a role it defined itself.
    let (mut orbit, rc, _) = staffed("team-pinned");
    orbit.stop();
    orbit.append_config(PINNED_ACTOR);

    orbit.start();
    let ac = orbit.wait_worker("ac");
    assert_eq!(orbit.wait_worker("rc").terminal_id, rc.terminal_id);
    assert_eq!(
        orbit.polaris_tabs(),
        ["Polaris", "Actor", "Reviewer", "Orbit"]
    );
    let actor = orbit.team_role("actor");
    assert_eq!(actor["pinned"], true, "{actor}");
    assert_eq!(orbit.team_role("reviewer")["pinned"], false);

    orbit.stop();
    orbit.start();
    assert_eq!(orbit.wait_worker("ac").terminal_id, ac.terminal_id);
    orbit.wait_worker("rc");
    assert_eq!(
        orbit.polaris_tabs(),
        ["Polaris", "Actor", "Reviewer", "Orbit"]
    );

    let refused = orbit.orbit_cli("team-remove", &json!({"name": "actor", "force": true}));
    assert!(!refused.success, "{}", refused.raw);
    assert_eq!(
        refused.body["error"]["code"], "orbit_role_pinned",
        "{}",
        refused.raw
    );
    assert_eq!(orbit.team_role("actor")["pinned"], true);
}

const PIPELINE_BUILDER: &str = "\n[[polaris.team]]\nname = \"builder\"\narea = \"Build then review\"\nprompt = \"Build, then hand over.\"\npipeline = [\"claude\", \"codex\"]\n\n[[polaris.team.members]]\nname = \"bc\"\nkind = \"claude\"\nmodel = \"bc\"\n\n[[polaris.team.members]]\nname = \"bx\"\nkind = \"codex\"\nmodel = \"bx\"\n";

#[test]
fn a_task_relayed_through_the_role_pipeline_reaches_the_lead_as_one_report() {
    let mut orbit = Orbit::with_config("team-relay", PIPELINE_BUILDER);
    orbit.start();
    let bc = orbit.wait_worker("bc");
    let bx = orbit.wait_worker("bx");

    // No kind is given: the pipeline's first kind (claude) takes the task.
    let response = orbit.request(
        "orbit.agent_assign",
        json!({"role": "builder", "prompt": "Build and review the parser"}),
    );
    let task_id = response["result"]["task_id"]
        .as_str()
        .unwrap_or_else(|| panic!("{response}"))
        .to_owned();
    assert_eq!(response["result"]["worker_id"], json!(bc.terminal_id));
    let mut first = None;
    orbit.wait_until("assignment at the claude member", STATE_TIMEOUT, |orbit| {
        first = orbit.instruction("bc", &task_id);
        first.is_some()
    });
    let first = first.unwrap();
    assert!(orbit
        .prompts("bc")
        .iter()
        .any(|prompt| prompt.contains("handoff object")));

    let handoff = json!({"handoff": {
        "done": "Wrote the parser",
        "next": "Review the parser for edge cases",
        "changed_files": ["src/parser.rs"],
        "known_issues": "no fuzzing yet",
    }});
    let relayed = orbit.report_with(&first, "parser written", handoff);
    assert!(relayed.success, "{}", relayed.raw);

    // The codex member receives a follow-up for the same task with the note.
    let mut second = None;
    orbit.wait_until("follow-up at the codex member", STATE_TIMEOUT, |orbit| {
        second = orbit.instruction("bx", &task_id);
        second.is_some()
    });
    let second = second.unwrap();
    assert_eq!(second.worker_id, bx.terminal_id);
    let follow_up = orbit
        .prompts("bx")
        .into_iter()
        .find(|prompt| prompt.contains(&task_id))
        .unwrap();
    assert!(follow_up.contains("Continue task"), "{follow_up}");
    assert!(
        follow_up.contains("Review the parser for edge cases"),
        "{follow_up}"
    );
    assert!(follow_up.contains("src/parser.rs"), "{follow_up}");
    assert_eq!(orbit.delivery(&task_id), "awaiting_report");
    let task = orbit.status(&task_id)["task"].clone();
    assert_eq!(task["steps"].as_array().map(Vec::len), Some(2), "{task}");
    assert_eq!(task["steps"][0]["state"], "handed_off", "{task}");
    assert_eq!(task["steps"][1]["state"], "active", "{task}");
    assert_eq!(task["step_line"], "step 2/2 bx", "{task}");
    assert!(orbit.lead_prompts_with(&marker(&task_id)).is_empty());

    let done = orbit.report(&second, "parser reviewed; two edge cases fixed");
    assert!(done.success, "{}", done.raw);
    let delivered = orbit.wait_lead_prompt(&marker(&task_id));
    assert!(delivered.contains("parser reviewed"), "{delivered}");
    assert_eq!(orbit.lead_prompts_with(&marker(&task_id)).len(), 1);
    let task = orbit.status(&task_id)["task"].clone();
    assert_eq!(task["steps"][1]["state"], "done", "{task}");
}

/// A project repository with uncommitted work, and Orbit started on it with
/// checkouts under `<base>/worktrees`.
fn parallel_orbit(case: &str) -> (Orbit, std::path::PathBuf) {
    let mut orbit = Orbit::new(case);
    let project = orbit.base().join("work");
    let worktrees = orbit.base().join("worktrees");
    orbit.git(&project, &["init", "-q"]);
    std::fs::create_dir_all(project.join("src")).unwrap();
    std::fs::write(project.join("src/a.txt"), "a\n").unwrap();
    std::fs::write(project.join("src/b.txt"), "b\n").unwrap();
    orbit.git(&project, &["add", "."]);
    orbit.git(&project, &["commit", "-q", "-m", "init"]);
    std::fs::write(project.join("wip.txt"), "uncommitted\n").unwrap();
    orbit.append_config(&format!(
        "[worktrees]\ndirectory = \"{}\"\n",
        worktrees.display()
    ));
    orbit.start();
    orbit.wait_until("Orbit baseline", STATE_TIMEOUT, |orbit| {
        orbit.activity()["baseline_ready"] == true
    });
    (orbit, project)
}

fn parallel_step(model: &str, prompt: &str, file: &str) -> Value {
    json!({"member_or_kind": "claude", "model": model, "prompt": prompt, "files": [file]})
}

fn task_steps(orbit: &Orbit, task_id: &str) -> Vec<Value> {
    let task = orbit.status(task_id)["task"].clone();
    task["steps"].as_array().cloned().unwrap_or_default()
}

fn phase(step: &Value) -> &str {
    step["parallel"]["phase"].as_str().unwrap_or_default()
}

fn assignment_with_step(orbit: &Orbit, agent: &str, task_id: &str, step: usize) -> Assignment {
    let found = orbit
        .instruction(agent, task_id)
        .unwrap_or_else(|| panic!("{agent} has no assignment for {task_id}"));
    let command = orbit
        .prompts(agent)
        .into_iter()
        .find(|prompt| prompt.contains(task_id))
        .unwrap();
    assert!(command.contains(&format!("\"step\":{step}")), "{command}");
    found
}

fn report_step(orbit: &Orbit, assignment: &Assignment, step: usize, summary: &str) {
    let report = orbit.report_with(assignment, summary, json!({"step": step}));
    assert!(report.success, "{}", report.raw);
}

#[test]
fn parallel_steps_on_disjoint_files_run_in_their_own_checkouts_and_report_once() {
    let (orbit, project) = parallel_orbit("parallel-disjoint");
    let before = orbit.git(&project, &["status", "--porcelain"]);
    let response = orbit.request(
        "orbit.agent_assign",
        json!({
            "prompt": "Change a and b at once",
            "parallel": [
                parallel_step("pa", "Edit a.txt", "src/a.txt"),
                parallel_step("pb", "Edit b.txt", "src/b.txt"),
            ],
        }),
    );
    let task_id = response["result"]["task_id"]
        .as_str()
        .unwrap_or_else(|| panic!("{response}"))
        .to_owned();

    // Both agents start together, each in its own checkout of the project.
    orbit.wait_until("both steps running", STATE_TIMEOUT, |orbit| {
        let steps = task_steps(orbit, &task_id);
        steps.len() == 2 && steps.iter().all(|step| phase(step) == "running")
    });
    let first = assignment_with_step(&orbit, "pa", &task_id, 0);
    let second = assignment_with_step(&orbit, "pb", &task_id, 1);
    let steps = task_steps(&orbit, &task_id);
    let checkout = |index: usize| -> std::path::PathBuf {
        steps[index]["parallel"]["checkout"]["path"]
            .as_str()
            .unwrap()
            .into()
    };
    let worktrees = orbit.base().join("worktrees");
    assert_ne!(checkout(0), checkout(1));
    for index in 0..2 {
        assert!(
            checkout(index).starts_with(&worktrees),
            "{:?}",
            checkout(index)
        );
        // The checkout carries the uncommitted work the task started from.
        assert_eq!(
            std::fs::read_to_string(checkout(index).join("wip.txt")).unwrap(),
            "uncommitted\n"
        );
    }
    let prompt = orbit
        .prompts("pa")
        .into_iter()
        .find(|p| p.contains(&task_id))
        .unwrap();
    assert!(
        prompt.contains(&checkout(0).display().to_string()),
        "{prompt}"
    );
    assert!(prompt.contains("Edit a.txt"), "{prompt}");
    // Taking the snapshot left the working tree exactly as it was.
    assert_eq!(orbit.git(&project, &["status", "--porcelain"]), before);

    std::fs::write(checkout(0).join("src/a.txt"), "a from step one\n").unwrap();
    std::fs::write(checkout(1).join("src/b.txt"), "b from step two\n").unwrap();
    std::fs::write(checkout(1).join("src/new.txt"), "created by step two\n").unwrap();
    report_step(&orbit, &first, 0, "a edited");
    assert!(orbit.lead_prompts_with(&marker(&task_id)).is_empty());
    report_step(&orbit, &second, 1, "b edited");

    let delivered = orbit.wait_lead_prompt(&marker(&task_id));
    assert!(
        delivered.contains("a edited") && delivered.contains("b edited"),
        "{delivered}"
    );
    assert!(
        delivered.contains("Step 1") && delivered.contains("Step 2"),
        "{delivered}"
    );
    orbit.wait_delivery(&task_id, "submitted");
    assert_eq!(orbit.lead_prompts_with(&marker(&task_id)).len(), 1);
    let steps = task_steps(&orbit, &task_id);
    assert!(
        steps.iter().all(|step| step["state"] == "done"),
        "{steps:?}"
    );
    for (index, expected) in [(0, "a from step one"), (1, "b from step two")] {
        let patch =
            std::path::PathBuf::from(steps[index]["parallel"]["patch"]["path"].as_str().unwrap());
        let text = std::fs::read_to_string(&patch).unwrap();
        assert!(text.contains(expected), "{text}");
        orbit.git(&project, &["apply", "--check", patch.to_str().unwrap()]);
    }
    let second_patch = steps[1]["parallel"]["patch"]["path"].as_str().unwrap();
    assert!(std::fs::read_to_string(second_patch)
        .unwrap()
        .contains("created by step two"));

    // Each checkout is removed once its patch is saved, and nothing reached the project.
    orbit.wait_until("checkouts removed", STATE_TIMEOUT, |_| {
        !checkout(0).exists() && !checkout(1).exists()
    });
    assert_eq!(orbit.git(&project, &["status", "--porcelain"]), before);
    assert_eq!(
        std::fs::read_to_string(project.join("src/a.txt")).unwrap(),
        "a\n"
    );
    assert!(
        orbit
            .git(&project, &["worktree", "list", "--porcelain"])
            .lines()
            .filter(|l| l.starts_with("worktree "))
            .count()
            == 1
    );
}

#[test]
fn overlapping_parallel_steps_run_one_after_another() {
    let (orbit, project) = parallel_orbit("parallel-overlap");
    let response = orbit.request(
        "orbit.agent_assign",
        json!({
            "prompt": "Two edits to the same file",
            "parallel": [
                parallel_step("pa", "First edit", "src/a.txt"),
                parallel_step("pb", "Second edit", "src"),
            ],
        }),
    );
    let task_id = response["result"]["task_id"].as_str().unwrap().to_owned();

    orbit.wait_until("the first step running", STATE_TIMEOUT, |orbit| {
        task_steps(orbit, &task_id)
            .first()
            .is_some_and(|step| phase(step) == "running")
    });
    let steps = task_steps(&orbit, &task_id);
    assert_eq!(steps[1]["state"], "pending");
    assert_eq!(phase(&steps[1]), "waiting");
    assert_eq!(
        steps[1]["parallel"]["log"][0],
        "overlaps step 1 on src/a.txt; runs after it"
    );
    assert!(orbit.prompts("pb").is_empty());

    let first = assignment_with_step(&orbit, "pa", &task_id, 0);
    let checkout =
        std::path::PathBuf::from(steps[0]["parallel"]["checkout"]["path"].as_str().unwrap());
    std::fs::write(checkout.join("src/a.txt"), "a after step one\n").unwrap();
    report_step(&orbit, &first, 0, "first done");

    // The second step starts once the first has finished, and is told where its patch is.
    orbit.wait_until("the second step prompted", STATE_TIMEOUT, |orbit| {
        orbit.instruction("pb", &task_id).is_some()
    });
    let patch = task_steps(&orbit, &task_id)[0]["parallel"]["patch"]["path"]
        .as_str()
        .unwrap()
        .to_owned();
    let prompt = orbit
        .prompts("pb")
        .into_iter()
        .find(|p| p.contains(&task_id))
        .unwrap();
    assert!(
        prompt.contains("already applied in your checkout"),
        "{prompt}"
    );
    // Its checkout starts from the first step's result, so its patch builds on it.
    let second_checkout = std::path::PathBuf::from(
        task_steps(&orbit, &task_id)[1]["parallel"]["checkout"]["path"]
            .as_str()
            .unwrap(),
    );
    assert_eq!(
        std::fs::read_to_string(second_checkout.join("src/a.txt")).unwrap(),
        "a after step one\n"
    );
    assert!(std::path::Path::new(&patch).exists());
    let second = assignment_with_step(&orbit, "pb", &task_id, 1);
    report_step(&orbit, &second, 1, "second done");
    let delivered = orbit.wait_lead_prompt(&marker(&task_id));
    assert!(
        delivered.contains("first done") && delivered.contains("second done"),
        "{delivered}"
    );
    assert!(delivered.contains("runs after it"), "{delivered}");
    orbit.git(&project, &["status", "--porcelain"]);
}

#[test]
fn parallel_steps_are_integrated_by_the_reviewer_and_polaris_gets_one_report() {
    let (orbit, project) = parallel_orbit("parallel-integrate");
    for (role, member) in [("builder", "bc"), ("reviewer", "rv")] {
        let defined = orbit.orbit_cli(
            "team-define",
            &json!({"name": role, "area": role, "members": [{"name": member, "kind": "claude", "model": member}]}),
        );
        assert!(defined.success, "{}", defined.raw);
    }
    orbit.wait_worker("bc");
    orbit.wait_worker("rv");
    let response = orbit.request(
        "orbit.agent_assign",
        json!({
            "prompt": "Change a and b, then integrate",
            "role": "builder",
            "parallel": [
                parallel_step("pa", "Edit a.txt", "src/a.txt"),
                parallel_step("pb", "Edit b.txt", "src/b.txt"),
            ],
        }),
    );
    let task_id = response["result"]["task_id"]
        .as_str()
        .unwrap_or_else(|| panic!("{response}"))
        .to_owned();
    orbit.wait_until("both steps running", STATE_TIMEOUT, |orbit| {
        let steps = task_steps(orbit, &task_id);
        steps.len() == 2 && steps.iter().all(|step| phase(step) == "running")
    });
    let first = assignment_with_step(&orbit, "pa", &task_id, 0);
    let second = assignment_with_step(&orbit, "pb", &task_id, 1);
    let steps = task_steps(&orbit, &task_id);
    let checkout = |index: usize| -> std::path::PathBuf {
        steps[index]["parallel"]["checkout"]["path"]
            .as_str()
            .unwrap()
            .into()
    };
    std::fs::write(checkout(0).join("src/a.txt"), "a from step one\n").unwrap();
    std::fs::write(checkout(1).join("src/b.txt"), "b from step two\n").unwrap();
    report_step(&orbit, &first, 0, "a edited");
    report_step(&orbit, &second, 1, "b edited");

    // Aster applies both patches to the project; then the reviewer is prompted.
    orbit.wait_until("the integrator prompted", STATE_TIMEOUT, |orbit| {
        orbit.instruction("rv", &task_id).is_some()
    });
    assert!(orbit.lead_prompts_with(&marker(&task_id)).is_empty());
    let prompt = orbit
        .prompts("rv")
        .into_iter()
        .find(|p| p.contains(&task_id))
        .unwrap();
    assert!(prompt.contains("applied all 2 patches cleanly"), "{prompt}");
    assert!(
        prompt.contains("a edited") && prompt.contains("b edited"),
        "{prompt}"
    );
    assert_eq!(
        std::fs::read_to_string(project.join("src/a.txt")).unwrap(),
        "a from step one\n"
    );
    assert_eq!(
        std::fs::read_to_string(project.join("src/b.txt")).unwrap(),
        "b from step two\n"
    );
    let staged = orbit.git(&project, &["diff", "--cached", "--name-only"]);
    assert!(
        staged.contains("src/a.txt") && staged.contains("src/b.txt"),
        "{staged}"
    );
    let integrator = assignment_with_step(&orbit, "rv", &task_id, 2);
    let task = orbit.status(&task_id)["task"].clone();
    assert_eq!(task["step_line"], "integrating by rv", "{task}");
    assert_eq!(task["delivery"], "awaiting_report");

    let done = orbit.report_with(
        &integrator,
        "integrated and tested",
        json!({
            "step": 2,
            "changed_files": ["src/extra.txt"],
            "tests_run": true,
            "checks": [{"name": "full tests", "status": "passed"}],
        }),
    );
    assert!(done.success, "{}", done.raw);

    let delivered = orbit.wait_lead_prompt(&marker(&task_id));
    for expected in [
        "integrated and tested",
        "src/a.txt",
        "src/b.txt",
        "src/extra.txt",
        "full tests",
    ] {
        assert!(delivered.contains(expected), "{expected}: {delivered}");
    }
    orbit.wait_delivery(&task_id, "submitted");
    assert_eq!(orbit.lead_prompts_with(&marker(&task_id)).len(), 1);
    let task = orbit.status(&task_id)["task"].clone();
    assert_eq!(
        task["report"]["changed_files"],
        json!(["src/a.txt", "src/b.txt", "src/extra.txt"])
    );
    // The saved patches stay for an undo with git apply -R, in reverse order.
    for index in 0..2 {
        let patch = task["steps"][index]["parallel"]["patch"]["path"]
            .as_str()
            .unwrap();
        assert!(std::path::Path::new(patch).exists(), "{patch}");
    }
}
