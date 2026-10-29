//! Socket-side completion of `orbit.agent_stop`. The event loop stops the worker
//! and returns its `pending` work, which finishes here on the connection thread
//! so neither waiting for a kept pane's agent to exit nor `git worktree remove`
//! blocks the server.
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use serde_json::{json, Value};

use crate::api::schema::{
    AgentTarget, Method, OrbitAgentStatusParams, OrbitAgentStopParams, PaneProcessInfoParams,
    Request,
};
use crate::api::server::{
    dispatch_to_app_with_timeout, APP_RESPONSE_TIMEOUT, CONNECTION_POLL_INTERVAL,
};
use crate::api::ApiRequestSender;

/// How long a kept pane may take to show no agent and its shell in the foreground.
const AGENT_EXIT_TIMEOUT: Duration = Duration::from_secs(5);

fn error(id: &str, code: &str, message: impl Into<String>) -> String {
    json!({"id": id, "error": {"code": code, "message": message.into()}}).to_string()
}

fn dispatch(id: &str, suffix: &str, method: Method, api_tx: &ApiRequestSender) -> Value {
    let request = Request {
        id: format!("{id}:{suffix}"),
        method,
    };
    let response = dispatch_to_app_with_timeout(request, api_tx, Some(APP_RESPONSE_TIMEOUT));
    serde_json::from_str(&response).unwrap_or_default()
}

/// Git's dirty-checkout message when the worker's worktree has changes, with the
/// Orbit option that discards them; `force` alone never does.
fn dirty_worktree(id: &str, name: &str, api_tx: &ApiRequestSender) -> Option<String> {
    let status = dispatch(
        id,
        "status",
        Method::OrbitAgentStatus(OrbitAgentStatusParams {
            name: name.into(),
            role: None,
        }),
        api_tx,
    );
    let path = Path::new(status["result"]["worker"]["worktree"]["path"].as_str()?);
    crate::worktree::checkout_has_dirty_files(path, false)
        .ok()
        .filter(|dirty| *dirty)
        .map(|_| {
            format!(
                "{}; commit or stash the changes, or stop with discard_changes true to discard them",
                crate::worktree::worktree_dirty_remove_message(path)
            )
        })
}

/// The pane hosts no agent (detection and managed launch state agree) and its
/// own shell is back in the foreground, ready for another worker.
fn agent_gone(id: &str, pane_id: &str, api_tx: &ApiRequestSender) -> bool {
    let agent = dispatch(
        id,
        "agent",
        Method::AgentGet(AgentTarget {
            target: pane_id.into(),
        }),
        api_tx,
    );
    if agent.get("error").is_none() {
        return false;
    }
    let process = dispatch(
        id,
        "process",
        Method::PaneProcessInfo(PaneProcessInfoParams {
            pane_id: Some(pane_id.into()),
        }),
        api_tx,
    );
    let info = &process["result"]["process_info"];
    match (
        info["shell_pid"].as_u64(),
        info["foreground_process_group_id"].as_u64(),
    ) {
        (Some(shell), Some(foreground)) => shell == foreground,
        _ => true,
    }
}

fn await_agent_exit(id: &str, result: &mut Value, api_tx: &ApiRequestSender) {
    let pane_id = result["pane_id"].as_str().unwrap_or_default().to_owned();
    let deadline = Instant::now() + AGENT_EXIT_TIMEOUT;
    while !agent_gone(id, &pane_id, api_tx) {
        if Instant::now() >= deadline {
            result["stopped"] = json!(false);
            result["warning"] =
                json!("The worker agent was signalled but the pane has not returned to its shell after 5 s");
            return;
        }
        std::thread::sleep(CONNECTION_POLL_INTERVAL);
    }
}

fn remove_worktree(removal: &Value, result: &mut Value) {
    let (Some(repo_root), Some(path)) = (removal["repo_root"].as_str(), removal["path"].as_str())
    else {
        return;
    };
    let (repo_root, path) = (PathBuf::from(repo_root), PathBuf::from(path));
    let force = removal["force"].as_bool().unwrap_or(false);
    let command = crate::worktree::build_worktree_remove_command(&repo_root, &path, force, false);
    match crate::worktree::run_worktree_remove_command_with_recovery(
        &command, &repo_root, &path, force, false,
    ) {
        Ok(()) => {
            crate::codex_trust::untrust_checkout(&path);
            result["worktree_removed"] = json!(true);
            if let Some(branch) = removal["delete_branch"].as_str() {
                result["branch_deleted"] = json!(delete_branch(&repo_root, branch));
            }
        }
        Err(error) => {
            tracing::warn!(%error, path = %path.display(), "orbit worktree removal failed");
            result["worktree_error"] = json!(error);
        }
    }
}

/// Deletes a branch Orbit created once its worktree is gone; failure is reported, not fatal.
fn delete_branch(repo_root: &Path, branch: &str) -> bool {
    let command = crate::worktree::build_branch_delete_command(repo_root, branch, false);
    match std::process::Command::new(&command.program)
        .args(&command.args)
        .current_dir(repo_root)
        .output()
    {
        Ok(output) if output.status.success() => true,
        Ok(output) => {
            let stderr = String::from_utf8_lossy(&output.stderr);
            tracing::warn!(branch, %stderr, "orbit branch delete failed");
            false
        }
        Err(error) => {
            tracing::warn!(branch, %error, "orbit branch delete failed");
            false
        }
    }
}

pub(super) fn stop_worker(
    request_id: String,
    params: OrbitAgentStopParams,
    api_tx: &ApiRequestSender,
) -> String {
    if params.remove_worktree && !params.discard_changes {
        if let Some(message) = dirty_worktree(&request_id, &params.name, api_tx) {
            return error(&request_id, "dirty_worktree_requires_force", message);
        }
    }
    let request = Request {
        id: request_id.clone(),
        method: Method::OrbitAgentStop(params),
    };
    let response = dispatch_to_app_with_timeout(request, api_tx, None);
    let Ok(mut value) = serde_json::from_str::<Value>(&response) else {
        return response;
    };
    let Some(pending) = value["result"]
        .as_object_mut()
        .and_then(|result| result.remove("pending"))
    else {
        return response;
    };
    let result = &mut value["result"];
    if pending["await_exit"] == true {
        await_agent_exit(&request_id, result, api_tx);
    }
    if pending["remove_worktree"].is_object() {
        remove_worktree(&pending["remove_worktree"], result);
    }
    value.to_string()
}
