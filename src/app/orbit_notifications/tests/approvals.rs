//! `orbit.agent_approve`/`orbit.agent_deny`: every refusal sends nothing; an
//! allowed answer sends the kind's keys and waits for the worker to leave Blocked.
use std::sync::mpsc::Receiver;

use super::*;
use crate::api::schema::{Method, OrbitAgentApprovalParams};

fn worker_id(app: &App) -> String {
    let ws = &app.state.workspaces[0];
    ws.terminal_id(ws.tabs[1].root_pane).unwrap().to_string()
}

fn allow(app: &mut App, allowed: bool) {
    let mut config = crate::config::PolarisConfig::default();
    config.approvals.lead_may_approve = allowed;
    app.configure_orbit_notifications(&config);
}

fn answer(app: &mut App, approve: bool, name: &str) -> Receiver<String> {
    let params = OrbitAgentApprovalParams { name: name.into() };
    let method = if approve {
        Method::OrbitAgentApprove(params)
    } else {
        Method::OrbitAgentDeny(params)
    };
    let (tx, rx) = std::sync::mpsc::channel();
    let request = Request {
        id: "approval".into(),
        method,
    };
    assert!(app.handle_deferred_orbit_approval(request, tx));
    rx
}

/// Claude's Bash permission dialog, as the manifest's bash_permission_prompt sees it.
const PERMISSION_PROMPT: &str = "\r\n Bash command\r\n\r\n   rm -rf build\r\n\r\n Do you want to proceed?\r\n \u{276f} 1. Yes\r\n   2. Yes, and don't ask again for rm commands\r\n   3. No, and tell Claude what to do differently (esc)\r\n\r\n Esc to cancel \u{b7} Tab to amend\r\n";

/// Claude's folder-trust dialog: blocked, but not a permission prompt.
const TRUST_PROMPT: &str = "\r\n\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\r\n Do you trust the files in this folder?\r\n \u{276f} 1. Yes, proceed\r\n   2. No, exit\r\n\r\n Enter to confirm \u{b7} Esc to cancel\r\n";

/// Draws `screen` on the worker's terminal, replacing what was there.
fn show(app: &mut App, terminal_id: &str, screen: &str) {
    let key = app
        .state
        .terminals
        .get_key_value(terminal_id)
        .map(|(key, _)| key.clone())
        .unwrap();
    let runtime = app.terminal_runtimes.get(&key).unwrap();
    runtime.test_process_pty_bytes(format!("\x1b[2J\x1b[H{screen}").as_bytes());
}

fn response(rx: &Receiver<String>) -> serde_json::Value {
    serde_json::from_str(&rx.try_recv().expect("response sent")).unwrap()
}

fn refusal(app: &mut App, name: &str) -> String {
    let value = response(&answer(app, true, name));
    value["error"]["code"]
        .as_str()
        .unwrap_or("<none>")
        .to_owned()
}

#[tokio::test]
async fn refusals_send_no_keys() {
    let (mut app, mut worker, _lead) = setup();
    let worker = &mut worker;
    let id = worker_id(&app);
    allow(&mut app, true);
    assert_eq!(refusal(&mut app, "polaris"), "orbit_lead_refused");
    // Not blocked: nothing to answer.
    assert_eq!(refusal(&mut app, "reviewer"), "orbit_no_pending_approval");
    set_state(&mut app, &id, AgentState::Blocked);
    // Not allowed: Polaris is told to ask the user.
    allow(&mut app, false);
    let value = response(&answer(&mut app, true, "reviewer"));
    assert_eq!(value["error"]["code"], "orbit_approval_not_allowed");
    let message = value["error"]["message"].as_str().unwrap();
    assert!(message.contains("lead_may_approve = false"), "{message}");
    assert!(message.contains("Ask the user to answer"), "{message}");
    // No known key map for the kind.
    allow(&mut app, true);
    let terminal = app.state.terminals.get_mut(id.as_str()).unwrap();
    terminal.set_detected_state(Some(Agent::Pi), AgentState::Blocked);
    terminal.set_agent_name("reviewer".into());
    let value = response(&answer(&mut app, true, "reviewer"));
    assert_eq!(
        value["error"]["code"], "orbit_approval_unsupported",
        "{value}"
    );
    assert_eq!(refusal(&mut app, "missing-worker"), "agent_not_found");
    tokio::time::sleep(Duration::from_millis(20)).await;
    assert_eq!(drain(worker), "");
}

#[tokio::test]
async fn allowed_answers_send_kind_keys_and_wait_for_the_prompt_to_close() {
    let (mut app, mut worker, _lead) = setup();
    let id = worker_id(&app);
    allow(&mut app, true);
    show(&mut app, &id, PERMISSION_PROMPT);
    set_state(&mut app, &id, AgentState::Blocked);
    let pending = answer(&mut app, true, "reviewer");
    assert!(pending.try_recv().is_err(), "waits while still blocked");
    assert_eq!(drain_until(&mut worker, |text| !text.is_empty()).await, "1");
    set_state(&mut app, &id, AgentState::Working);
    pump(&mut app);
    let value = response(&pending);
    assert_eq!(value["result"]["answered"], true);
    assert_eq!(value["result"]["state"], "working");

    // Deny sends Esc; a prompt that stays open answers false after the wait.
    set_state(&mut app, &id, AgentState::Blocked);
    let pending = answer(&mut app, false, "reviewer");
    assert_eq!(
        drain_until(&mut worker, |text| !text.is_empty()).await,
        "\u{1b}"
    );
    app.poll_orbit_answers(Instant::now() + super::super::ANSWER_WAIT);
    let value = response(&pending);
    assert_eq!(value["result"]["answered"], false);
    assert_eq!(value["result"]["state"], "blocked");
}

#[tokio::test]
async fn blocked_dialogs_other_than_permission_prompts_are_refused() {
    let (mut app, mut worker, _lead) = setup();
    let id = worker_id(&app);
    allow(&mut app, true);
    for screen in [TRUST_PROMPT, ""] {
        show(&mut app, &id, screen);
        set_state(&mut app, &id, AgentState::Blocked);
        for approve in [true, false] {
            let value = response(&answer(&mut app, approve, "reviewer"));
            assert_eq!(
                value["error"]["code"], "orbit_approval_unsupported_prompt",
                "{value}"
            );
        }
    }
    show(&mut app, &id, TRUST_PROMPT);
    let value = response(&answer(&mut app, true, "reviewer"));
    let message = value["error"]["message"].as_str().unwrap();
    assert!(message.contains("live_blocked_form"), "{message}");
    assert!(message.contains("ask the user"), "{message}");
    tokio::time::sleep(Duration::from_millis(20)).await;
    assert_eq!(drain(&mut worker), "");
    // The same worker on a permission prompt is answered.
    show(&mut app, &id, PERMISSION_PROMPT);
    let _pending = answer(&mut app, true, "reviewer");
    assert_eq!(drain_until(&mut worker, |text| !text.is_empty()).await, "1");
}

#[tokio::test]
async fn synchronous_dispatch_requires_the_deferred_path() {
    let (mut app, _worker, _lead) = setup();
    let value = api(
        &mut app,
        "orbit.agent_approve",
        serde_json::json!({"name":"reviewer"}),
    );
    assert_eq!(value["error"]["code"], "deferred_required");
}
