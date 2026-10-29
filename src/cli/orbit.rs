//! Small stdio MCP adapter for the server-owned Orbit operations.
use std::io::{self, BufRead, Write};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use serde_json::{json, Value};

const MAX_MESSAGE: u64 = 1024 * 1024;
const OPERATIONS: &str = "inspect|run|read|split|service-start|service-stop|service-list|exec|job-status|job-cancel|agent-start|agent-prompt|agent-assign|agent-status|task-status|task-cancel|diff|activity|agent-approve|agent-deny|agent-stop|pane-resize|pane-zoom|pane-close|team-define|team-remove|team-list|route|decisions [--check]";
static REQUEST_SEQUENCE: AtomicU64 = AtomicU64::new(1);
/// Report token variable older assignment instructions set; still accepted when
/// the task has no token file.
const REPORT_TOKEN_ENV: &str = "ASTER_ORBIT_TOKEN";

fn catalog() -> Value {
    let pane = json!({"type":"string","description":"Optional Orbit pane ID from orbit_inspect; defaults to Orbit's root pane."});
    let mut tools = json!([
        {"name":"orbit_inspect","description":"Inspect the bound Orbit work tab and its panes, or with role a team role's tab and its member panes. Each Orbit pane has stale (its worker exited; close it with orbit_pane_close) and, for workers you started, worker with kind, model, and worktree. Does not change focus.","inputSchema":{"type":"object","properties":{"role":{"type":"string","description":"Team role whose tab to inspect instead of Orbit."}},"additionalProperties":false}},
        {"name":"orbit_run","description":"Submit a command to an idle shell inside Orbit. Success means queued, not completed; use orbit_read to inspect results.","inputSchema":{"type":"object","properties":{"pane_id":pane,"command":{"type":"string","minLength":1}},"required":["command"],"additionalProperties":false}},
        {"name":"orbit_read","description":"Read terminal output from a pane inside Orbit without changing focus.","inputSchema":{"type":"object","properties":{"pane_id":pane,"lines":{"type":"integer","minimum":1,"maximum":2000}},"additionalProperties":false}},
        {"name":"orbit_split","description":"Create a shell pane inside Orbit, preserving user focus.","inputSchema":{"type":"object","properties":{"pane_id":pane,"direction":{"type":"string","enum":["right","down"]}},"additionalProperties":false}}
    ]);
    let extras = [
        ("orbit_service_start", "Start a server or watcher independently of jobs. Replacing a name stops its old service first. Readiness times out after timeout_ms (default 30000) without stopping the service.", json!({"name":{"type":"string"},"command":{"type":"string"},"cwd":{"type":"string"},"ready_url":{"type":"string"},"ready_log":{"type":"string"},"timeout_ms":{"type":"integer","minimum":1,"maximum":86400000}}), vec!["name","command"]),
        ("orbit_service_stop", "Stop a named service process group.", json!({"name":{"type":"string"}}), vec!["name"]),
        ("orbit_service_list", "List services, readiness, pid, uptime and recent output.", json!({}), vec![]),
        ("orbit_exec", "Run a tracked noninteractive command in Orbit's project directory. Returns a job ID; poll orbit_job_status. Captures output separately from terminal scrollback.", json!({"command":{"type":"string"},"timeout_ms":{"type":"integer","minimum":1,"maximum":86400000}}), vec!["command"]),
        ("orbit_job_status", "Read job state, exit code and a page of captured output. Follow next_offset while has_more. Output capture is capped and reports truncation.", json!({"job_id":{"type":"string"},"offset":{"type":"integer","minimum":0},"limit":{"type":"integer","minimum":1,"maximum":65536}}), vec!["job_id"]),
        ("orbit_job_cancel", "Cancel an Orbit-owned job. Process jobs stop their process tree; review tasks finish their bounded operation before cancellation completes.", json!({"job_id":{"type":"string"}}), vec!["job_id"]),
        ("orbit_agent_start", "Start a named worker in an idle Orbit pane. The required name automatically becomes the visible pane title. Split first if occupied. kind is an agent CLI: claude, codex, gemini, cursor, devin, agy (Antigravity), cline, opencode, copilot, kimi, pi, and others Aster detects. model is that agent's own CLI model name (claude: \"opus\", \"sonnet\", \"fable\"; codex: \"gpt-5-codex\"); only claude and codex accept it, and an omitted model may get a configured default. Kinds or models outside the configured allowlist, or starts beyond the worker limit, are rejected with the allowed values in the error; check orbit_activity.kinds for what is allowed and installed. By default an idle, unassigned worker of the same kind (and model, when given) is returned with reused true instead of starting another; pass reuse false to force a new one. worktree true (generated branch) or a branch name starts the worker in a new Orbit pane on its own Git worktree of the project, for edits that could conflict; it needs a Git project and returns worktree {path, branch}. permission_mode (claude only: manual, acceptEdits, auto, plan, dontAsk; bypassPermissions is refused) sets the worker's Claude permission mode; use manual to make a worker ask before every tool call, useful for reviewing or for testing approvals. Returns launch admission; poll status before prompting. A pane split a moment ago can answer agent_shell_starting while its shell boots: retry after a second.", json!({"name":{"type":"string","description":"Worker name, also applied automatically as the pane title."},"kind":{"type":"string","description":"Agent kind, for example claude or codex; see orbit_activity.kinds."},"pane_id":pane,"model":{"type":"string","description":"The agent CLI's own model name, for example opus or gpt-5-codex."},"reuse":{"type":"boolean","description":"Return a matching idle worker instead of starting one; default true."},"worktree":{"type":["boolean","string"],"description":"Boolean true for a generated branch (the text \"true\" is treated the same), or a branch name for an isolated Git worktree. Stopping with remove_worktree and discard_changes also deletes a branch Orbit generated."},"permission_mode":{"type":"string","enum":["manual","acceptEdits","auto","plan","dontAsk"],"description":"Claude workers only. Omitted uses [polaris.workers] permission_mode, else the user's Claude settings."}}), vec!["name","kind"]),
        ("orbit_agent_prompt", "Send untracked text to an idle worker inside Orbit or a team role. Prefer orbit_agent_assign for tasks that must notify Polaris. name accepts stable terminal_id, name, or pane ID; role instead picks an idle member of that team role (kind prefers one kind). Acknowledges submission, not completion.", json!({"name":{"type":"string"},"role":{"type":"string","description":"Team role whose idle member receives the prompt, instead of name."},"kind":{"type":"string","description":"With role, prefer a member of this kind."},"prompt":{"type":"string"}}), vec!["prompt"]),
        ("orbit_agent_assign", "Assign a tracked task to an idle worker, then continue other work: do not poll or wait. Give role to route the task to the responsible team role: an idle member without an open assignment takes it (kind prefers one kind) and is told it acts for that role; or give name for a specific worker. Aster adds reporting instructions, and the worker's report arrives automatically as a prompt when you are idle. If the worker is lost, stays idle without reporting after one reminder, or exceeds timeout_ms (default 2 hours, max 24 hours), Aster sends a notice marked origin server instead. Returns task_id.", json!({"name":{"type":"string"},"role":{"type":"string","description":"Team role that takes the task, instead of name."},"kind":{"type":"string","description":"With role, prefer a member of this kind."},"prompt":{"type":"string"},"timeout_ms":{"type":"integer","minimum":1,"maximum":86400000}}), vec!["prompt"]),
        ("orbit_agent_report", "Report an assigned task result once: completed, failed, or blocked. Use the exact task_id and worker_id from your assignment instructions; Aster reads the task's token from its private token file, so omit token unless your instructions gave one (ASTER_ORBIT_TOKEN in older assignments); a token belongs to that task only. Reports are accepted only from workers inside Orbit, and refused reports are rate limited. Identical retries are safe, and a retry may add the optional structured fields to a report sent without them (stored, not redelivered); any other change is refused. Set tests_run honestly and use blocked_on with outcome blocked. Aster queues the report for Polaris when idle.", json!({"task_id":{"type":"string"},"worker_id":{"type":"string"},"token":{"type":"string","description":"Only for assignments that listed an ASTER_ORBIT_TOKEN value; pass it exactly. Omit it otherwise: Aster reads the task's token file."},"outcome":{"type":"string","enum":["completed","failed","blocked"]},"summary":{"type":"string","minLength":1,"maxLength":4000,"description":"Concise result, at most 4000 bytes (UTF-8), without terminal control characters."},"tests_run":{"type":"boolean","description":"true only if you ran the project's tests for this task."},"checks":{"type":"array","maxItems":20,"description":"Tests, lints, or builds you ran.","items":{"type":"object","properties":{"name":{"type":"string","minLength":1,"maxLength":120},"status":{"type":"string","enum":["passed","failed","skipped"]},"detail":{"type":"string","minLength":1,"maxLength":500}},"required":["name","status"],"additionalProperties":false}},"changed_files":{"type":"array","maxItems":200,"description":"Paths you changed, relative to the project.","items":{"type":"string","minLength":1,"maxLength":512}},"branch":{"type":"string","minLength":1,"maxLength":200,"description":"Git branch holding your changes."},"job_ids":{"type":"array","maxItems":20,"description":"Orbit job IDs you started.","items":{"type":"string","minLength":1,"maxLength":128}},"blocked_on":{"type":"string","minLength":1,"maxLength":500,"description":"What you need to continue; use with outcome blocked."}}), vec!["task_id","worker_id","outcome","summary"]),
        ("orbit_agent_status", "Inspect an Orbit worker's state and approval needs, plus worker.model and worker.worktree when you started it. name accepts stable terminal_id, name, or pane ID; role instead returns the role and every member's agent. Use orbit_read for output.", json!({"name":{"type":"string"},"role":{"type":"string","description":"Team role: the status of each member."}}), vec![]),
        ("orbit_diff", "Start a tracked Git review job showing edits since the initial Orbit baseline, including untracked non-ignored files. Poll orbit_job_status. Concurrent external edits also appear.", json!({"path":{"type":"string"}}), vec![]),
        ("orbit_activity", "List tracked Orbit jobs, worker agents in the Orbit tab, team (each role with its members, pane_id, state, and open_tasks), assignments with delivery states, the lead's delivery status, stale_panes (panes whose worker exited; close them with orbit_pane_close), and kinds: the allowed worker kinds with executable, installed, allowed models (empty = any), and default_model.", json!({}), vec![]),
        ("orbit_task_status", "Inspect one assignment: submission and delivery state, attempts, timestamps, deadline, report, and error. Only for inspection; reports are delivered automatically without polling.", json!({"task_id":{"type":"string"}}), vec!["task_id"]),
        ("orbit_task_cancel", "Abandon an unresolved assignment. A later report from the worker is rejected, and no result is delivered. Does not stop the worker; prompt it separately if needed.", json!({"task_id":{"type":"string"}}), vec!["task_id"]),
        ("orbit_agent_approve", "Approve a blocked Orbit worker's pending approval prompt, only when [polaris.approvals] lead_may_approve is true; otherwise ask the user to answer in the worker's pane. Refused for the lead and for workers that are not blocked. Waits up to 3 seconds and returns answered and the worker's state.", json!({"name":{"type":"string","description":"Worker terminal_id, name, or pane ID."}}), vec!["name"]),
        ("orbit_agent_deny", "Decline a blocked Orbit worker's pending approval prompt, only when [polaris.approvals] lead_may_approve is true; otherwise ask the user to answer in the worker's pane. Refused for the lead and for workers that are not blocked. Waits up to 3 seconds and returns answered and the worker's state.", json!({"name":{"type":"string","description":"Worker terminal_id, name, or pane ID."}}), vec!["name"]),
        ("orbit_agent_stop", "Stop a finished Orbit worker. Team role members are refused: retire a role with orbit_team_remove; force stops a member in place and the team supervisor relaunches it. Closes its pane by default (close_pane false ends only the agent and keeps the shell). Refused for the lead, outside Orbit, for panes with neither a worker nor a live agent, and while the worker has assignments awaiting its report unless force cancels them. Reports the worker already made are never cancelled: they keep delivering and are listed in pending_deliveries. remove_worktree also removes its Orbit worktree; a worktree with uncommitted or untracked changes is refused unless discard_changes is true (force never discards changes). Returns stopped, pane_id, closed, cancelled_tasks, pending_deliveries.", json!({"name":{"type":"string","description":"Worker terminal_id, name, or pane ID."},"close_pane":{"type":"boolean","description":"Close the pane; default true."},"force":{"type":"boolean","description":"Cancel assignments still awaiting the worker's report."},"remove_worktree":{"type":"boolean","description":"Remove the worker's Orbit worktree after closing its pane."},"discard_changes":{"type":"boolean","description":"With remove_worktree, remove a dirty worktree and discard its changes."}}), vec!["name"]),
        ("orbit_pane_resize", "Grow or shrink an Orbit pane by amount terminal cells toward direction, in one call. Refused for the lead pane and panes outside Orbit; focus is unchanged.", json!({"pane_id":{"type":"string","description":"Orbit pane ID from orbit_inspect."},"direction":{"type":"string","enum":["left","right","up","down"]},"amount":{"type":"integer","minimum":1,"maximum":200,"description":"Cells; default 10."}}), vec!["pane_id","direction"]),
        ("orbit_pane_zoom", "Zoom an Orbit pane to fill the Orbit tab, or restore the layout. Refused for the lead pane; the user's focus stays where it is.", json!({"pane_id":{"type":"string","description":"Orbit pane ID from orbit_inspect."},"zoomed":{"type":"boolean"}}), vec!["pane_id","zoomed"]),
        ("orbit_pane_close", "Close an Orbit pane, for example a stale pane from orbit_activity.stale_panes. Refused for the lead and Orbit's last pane, and for a live worker unless force, which stops it like orbit_agent_stop and cancels its assignments still awaiting a report; reports it already made keep delivering and are listed in pending_deliveries. Never removes a worktree; use orbit_agent_stop with remove_worktree.", json!({"pane_id":{"type":"string","description":"Orbit pane ID from orbit_inspect."},"force":{"type":"boolean","description":"Close a pane hosting a live worker, cancelling assignments awaiting its report."}}), vec!["pane_id"]),
    ];
    for (name, description, properties, required) in extras
        .into_iter()
        .chain(super::orbit_team::tools())
        .chain(super::orbit_decisions::tools())
    {
        tools.as_array_mut().unwrap().push(json!({"name":name,"description":description,"inputSchema":{"type":"object","properties":properties,"required":required,"additionalProperties":false}}));
    }
    tools
}

/// Lead models often write `agent` or `worker` for the worker `name`; accept both
/// so a guessed key does not cost a failed tool call.
fn normalize_worker_aliases(mut arguments: Value) -> Value {
    if let Some(object) = arguments.as_object_mut() {
        if !object.contains_key("name") {
            for alias in ["agent", "worker"] {
                if let Some(value) = object.remove(alias) {
                    object.insert("name".into(), value);
                    break;
                }
            }
        }
    }
    arguments
}

fn request(name: &str, arguments: Value) -> Result<crate::api::schema::Request, String> {
    let operation = match name {
        "orbit_inspect" => "inspect",
        "orbit_run" => "run",
        "orbit_read" => "read",
        "orbit_split" => "split",
        "orbit_service_start" => "service_start",
        "orbit_service_stop" => "service_stop",
        "orbit_service_list" => "service_list",
        "orbit_exec" => "exec",
        "orbit_job_status" => "job_status",
        "orbit_job_cancel" => "job_cancel",
        "orbit_agent_start" => "agent_start",
        "orbit_agent_prompt" => "agent_prompt",
        "orbit_agent_status" => "agent_status",
        "orbit_agent_assign" => "agent_assign",
        "orbit_agent_report" => "agent_report",
        "orbit_diff" => "diff",
        "orbit_activity" => "activity",
        "orbit_task_status" => "task_status",
        "orbit_task_cancel" => "task_cancel",
        "orbit_agent_approve" => "agent_approve",
        "orbit_agent_deny" => "agent_deny",
        "orbit_agent_stop" => "agent_stop",
        "orbit_pane_resize" => "pane_resize",
        "orbit_pane_zoom" => "pane_zoom",
        "orbit_pane_close" => "pane_close",
        "orbit_team_define" => "team_define",
        "orbit_team_remove" => "team_remove",
        "orbit_team_list" => "team_list",
        "orbit_route" => "route",
        "orbit_decisions" => "decisions",
        _ => return Err(format!("Unknown Orbit tool: {name}")),
    };
    let arguments = normalize_worker_aliases(arguments);
    let object = arguments
        .as_object()
        .ok_or("Tool arguments must be an object")?;
    let allowed: &[&str] = match operation {
        _ if super::orbit_team::allowed(operation).is_some() => {
            super::orbit_team::allowed(operation).unwrap_or(&[])
        }
        _ if super::orbit_decisions::allowed(operation).is_some() => {
            super::orbit_decisions::allowed(operation).unwrap_or(&[])
        }
        "inspect" => &["role"],
        "activity" => &[],
        "service_start" => &[
            "name",
            "command",
            "cwd",
            "ready_url",
            "ready_log",
            "timeout_ms",
        ],
        "service_stop" => &["name"],
        "service_list" => &[],
        "exec" => &["command", "timeout_ms"],
        "job_status" => &["job_id", "offset", "limit"],
        "job_cancel" => &["job_id"],
        "agent_start" => &[
            "name",
            "kind",
            "pane_id",
            "model",
            "reuse",
            "worktree",
            "permission_mode",
        ],
        "agent_stop" => &[
            "name",
            "close_pane",
            "force",
            "remove_worktree",
            "discard_changes",
        ],
        "pane_resize" => &["pane_id", "direction", "amount"],
        "pane_zoom" => &["pane_id", "zoomed"],
        "pane_close" => &["pane_id", "force"],
        "agent_prompt" => &["name", "role", "kind", "prompt"],
        "agent_assign" => &["name", "role", "kind", "prompt", "timeout_ms"],
        "task_status" | "task_cancel" => &["task_id"],
        "agent_report" => &[
            "task_id",
            "worker_id",
            "token",
            "outcome",
            "summary",
            "tests_run",
            "checks",
            "changed_files",
            "branch",
            "job_ids",
            "blocked_on",
        ],
        "agent_status" => &["name", "role"],
        "agent_approve" | "agent_deny" => &["name"],
        "diff" => &["path"],
        "run" => &["pane_id", "command"],
        "read" => &["pane_id", "lines"],
        _ => &["pane_id", "direction"],
    };
    if let Some(key) = object.keys().find(|key| !allowed.contains(&key.as_str())) {
        return Err(format!(
            "Unexpected Orbit tool argument {key:?}; {name} accepts: {}",
            allowed.join(", ")
        ));
    }
    if operation == "read"
        && object
            .get("lines")
            .is_some_and(|v| !v.as_u64().is_some_and(|n| (1..=2000).contains(&n)))
    {
        return Err("lines must be between 1 and 2000".into());
    }
    if operation == "run"
        && object
            .get("command")
            .and_then(Value::as_str)
            .is_none_or(|s| s.trim().is_empty())
    {
        return Err("command must be non-empty".into());
    }
    if operation == "pane_resize"
        && object
            .get("amount")
            .is_some_and(|v| !v.as_u64().is_some_and(|n| (1..=200).contains(&n)))
    {
        return Err("amount must be between 1 and 200 cells".into());
    }
    if operation == "split"
        && object
            .get("direction")
            .is_some_and(|v| !matches!(v.as_str(), Some("right" | "down")))
    {
        return Err("direction must be right or down".into());
    }
    serde_json::from_value(json!({
        "id":format!("orbit:{}:{}",std::process::id(),REQUEST_SEQUENCE.fetch_add(1,Ordering::Relaxed)),
        "method":format!("orbit.{operation}"),"params":arguments
    })).map_err(|e| e.to_string())
}

/// The report token, first found of: the task's token file, `ASTER_ORBIT_TOKEN`,
/// then the JSON `token` field. The file means the worker's command never
/// carries the token.
fn with_report_token(
    arguments: Value,
    file_token: Option<String>,
    env_token: Option<String>,
) -> Value {
    let token = file_token
        .into_iter()
        .chain(env_token)
        .find(|token| !token.is_empty());
    match (arguments, token) {
        (Value::Object(mut object), Some(token)) => {
            object.insert("token".into(), Value::String(token));
            Value::Object(object)
        }
        (arguments, _) => arguments,
    }
}

/// The token file named by the report's `task_id`, in this session's data directory.
fn report_token_file(arguments: &Value) -> Option<crate::app::OrbitTokenFile> {
    let task_id = arguments.get("task_id")?.as_str()?;
    let dir = crate::app::orbit_token_dir(&crate::session::data_dir());
    crate::app::orbit_read_token_file(&dir, task_id)
}

/// A report's token and the socket of the session that issued it, when its
/// token file names one; other calls use the local session.
fn prepare_call(name: &str, arguments: Value) -> (Value, crate::api::client::ApiClient) {
    use crate::api::client::{ApiClient, ConnectionTarget};
    if name != "orbit_agent_report" {
        return (arguments, ApiClient::local());
    }
    let file = report_token_file(&arguments);
    let (file_token, socket) = match file {
        Some(file) => (Some(file.token), file.socket),
        None => (None, None),
    };
    let arguments = with_report_token(arguments, file_token, std::env::var(REPORT_TOKEN_ENV).ok());
    let client = socket.map_or_else(ApiClient::local, |socket| {
        ApiClient::for_target(ConnectionTarget::SocketPath(socket))
    });
    (arguments, client)
}

fn call(name: &str, arguments: Value) -> Result<Value, String> {
    let (arguments, client) = prepare_call(name, arguments);
    let request = request(name, arguments)?;
    // Direct JSON API only: stdout must remain exclusively MCP framing.
    client
        .request_value_with_timeout(&request, Duration::from_secs(15))
        .map_err(|error| error.to_string())
}

fn dispatch(
    message: Value,
    mut invoke: impl FnMut(&str, Value) -> Result<Value, String>,
) -> Option<Value> {
    let id = message.get("id")?.clone();
    let result = match message["method"].as_str() {
        Some("initialize") => {
            json!({"protocolVersion":"2024-11-05","capabilities":{"tools":{"listChanged":false}},"serverInfo":{"name":"aster-orbit","version":env!("CARGO_PKG_VERSION")}})
        }
        Some("ping") => json!({}),
        Some("tools/list") => json!({"tools":catalog()}),
        Some("tools/call") => {
            let name = message["params"]["name"].as_str().unwrap_or("");
            let arguments = message["params"]
                .get("arguments")
                .cloned()
                .unwrap_or_else(|| json!({}));
            match invoke(name, arguments) {
                Ok(value) => {
                    json!({"content":[{"type":"text","text":value.to_string()}],"isError":value.get("error").is_some()})
                }
                Err(error) => json!({"content":[{"type":"text","text":error}],"isError":true}),
            }
        }
        _ => {
            return Some(
                json!({"jsonrpc":"2.0","id":id,"error":{"code":-32601,"message":"Method not found"}}),
            )
        }
    };
    Some(json!({"jsonrpc":"2.0","id":id,"result":result}))
}

pub(super) fn serve() -> io::Result<i32> {
    let stdin = io::stdin();
    let mut input = stdin.lock();
    let stdout = io::stdout();
    let mut output = stdout.lock();
    loop {
        let mut line = String::new();
        let bytes = std::io::Read::take(&mut input, MAX_MESSAGE + 1).read_line(&mut line)?;
        if bytes == 0 {
            break;
        }
        if bytes as u64 > MAX_MESSAGE {
            return Err(io::Error::other("MCP message exceeds 1 MiB"));
        }
        let response = match serde_json::from_str(&line) {
            Ok(message) => dispatch(message, call),
            Err(_) => Some(
                json!({"jsonrpc":"2.0","id":null,"error":{"code":-32700,"message":"Invalid JSON"}}),
            ),
        };
        if let Some(response) = response {
            serde_json::to_writer(&mut output, &response)?;
            output.write_all(b"\n")?;
            output.flush()?;
        }
    }
    Ok(0)
}

pub(super) fn run(args: &[String]) -> io::Result<i32> {
    if args.first().is_some_and(|arg| arg == "activity") {
        if args.len() > 2 || args.get(1).is_some_and(|arg| arg != "--watch") {
            return Err(io::Error::other("usage: aster orbit activity [--watch]"));
        }
        return super::orbit_activity::run(args.len() == 2);
    }
    let Some(operation) = args.first() else {
        eprintln!("usage: aster orbit <{OPERATIONS}> [JSON arguments]");
        return Ok(2);
    };
    if matches!(operation.as_str(), "--help" | "-h") {
        println!("aster orbit <{OPERATIONS}> [JSON arguments]\nExample: aster orbit run '{{\"command\":\"pwd\"}}'\nExample: aster orbit task-status '{{\"task_id\":\"ID\"}}'");
        return Ok(0);
    }
    if let Some(arguments) = super::orbit_decisions::check_arguments(args) {
        return match call("orbit_decisions", arguments) {
            Ok(value) => super::print_response(&value),
            Err(error) => {
                eprintln!("{error}");
                Ok(1)
            }
        };
    }
    if args.len() > 2 {
        return Err(io::Error::other("Expected one JSON arguments object"));
    }
    let arguments = match args.get(1) {
        Some(text) => serde_json::from_str(text).map_err(io::Error::other)?,
        None => json!({}),
    };
    match call(&format!("orbit_{}", operation.replace('-', "_")), arguments) {
        Ok(value) => super::print_response(&value),
        Err(error) => {
            eprintln!("{error}");
            Ok(1)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn orbit_mcp_requests_have_fresh_ids_and_preserve_explicit_targets() {
        let first = request("orbit_run", json!({"pane_id":"w1:p9","command":"pwd"})).unwrap();
        let second = request("orbit_run", json!({"pane_id":"w1:p9","command":"pwd"})).unwrap();
        assert_ne!(first.id, second.id);
        let value = serde_json::to_value(first).unwrap();
        assert_eq!(value["method"], "orbit.run");
        assert_eq!(value["params"]["pane_id"], "w1:p9");
    }
    #[test]
    fn orbit_mcp_preserves_inspection_panes() {
        let response = dispatch(
            json!({"id":1,"method":"tools/call","params":{"name":"orbit_inspect"}}),
            |_, _| Ok(json!({"result":{"panes":[{"pane_id":"w1:p2"}]}})),
        )
        .unwrap();
        let body: Value =
            serde_json::from_str(response["result"]["content"][0]["text"].as_str().unwrap())
                .unwrap();
        assert_eq!(body["result"]["panes"][0]["pane_id"], "w1:p2");
    }
    #[test]
    fn orbit_mcp_notifications_have_no_response() {
        assert!(dispatch(
            json!({"jsonrpc":"2.0","method":"notifications/initialized"}),
            |_, _| panic!()
        )
        .is_none());
    }
    #[test]
    fn orbit_mcp_errors_are_tool_errors() {
        let response = dispatch(
            json!({"id":2,"method":"tools/call","params":{"name":"orbit_run","arguments":{}}}),
            |_, _| Err("outside Orbit".into()),
        )
        .unwrap();
        assert_eq!(response["result"]["isError"], true);
    }
    #[test]
    fn orbit_mcp_catalog_has_only_scoped_tools() {
        let names: Vec<_> = catalog()
            .as_array()
            .unwrap()
            .iter()
            .map(|v| v["name"].as_str().unwrap().to_owned())
            .collect();
        assert_eq!(
            names,
            [
                "orbit_inspect",
                "orbit_run",
                "orbit_read",
                "orbit_split",
                "orbit_service_start",
                "orbit_service_stop",
                "orbit_service_list",
                "orbit_exec",
                "orbit_job_status",
                "orbit_job_cancel",
                "orbit_agent_start",
                "orbit_agent_prompt",
                "orbit_agent_assign",
                "orbit_agent_report",
                "orbit_agent_status",
                "orbit_diff",
                "orbit_activity",
                "orbit_task_status",
                "orbit_task_cancel",
                "orbit_agent_approve",
                "orbit_agent_deny",
                "orbit_agent_stop",
                "orbit_pane_resize",
                "orbit_pane_zoom",
                "orbit_pane_close",
                "orbit_team_define",
                "orbit_team_remove",
                "orbit_team_list",
                "orbit_route",
                "orbit_decisions"
            ]
        );
    }
    #[test]
    fn orbit_mcp_agent_start_advertises_kinds_and_models() {
        let tools = catalog();
        let description = |name: &str| {
            tools
                .as_array()
                .unwrap()
                .iter()
                .find(|tool| tool["name"] == name)
                .unwrap()["description"]
                .as_str()
                .unwrap()
                .to_owned()
        };
        let start = description("orbit_agent_start");
        for text in [
            "claude",
            "codex",
            "gemini",
            "gpt-5-codex",
            "fable",
            "allowlist",
            "orbit_activity.kinds",
            "permission_mode",
            "use manual to make a worker ask before every tool call",
            "bypassPermissions is refused",
        ] {
            assert!(start.contains(text), "{text} missing from {start}");
        }
        assert!(description("orbit_activity").contains("kinds"));
    }
    #[test]
    fn orbit_mcp_maps_services() {
        for (tool, args, method) in [
            (
                "orbit_service_start",
                json!({"name":"web","command":"serve","ready_log":"listening","cwd":"/tmp","timeout_ms":500}),
                "orbit.service_start",
            ),
            (
                "orbit_service_stop",
                json!({"name":"web"}),
                "orbit.service_stop",
            ),
            ("orbit_service_list", json!({}), "orbit.service_list"),
        ] {
            let value = serde_json::to_value(request(tool, args.clone()).unwrap()).unwrap();
            assert_eq!(value["method"], method);
            for (key, expected) in args.as_object().unwrap() {
                assert_eq!(&value["params"][key], expected);
            }
        }
    }
    #[test]
    fn orbit_mcp_maps_task_tools_and_assignment_timeout() {
        for (tool, method) in [
            ("orbit_task_status", "orbit.task_status"),
            ("orbit_task_cancel", "orbit.task_cancel"),
        ] {
            let value =
                serde_json::to_value(request(tool, json!({"task_id":"task-1"})).unwrap()).unwrap();
            assert_eq!(value["method"], method);
            assert_eq!(value["params"]["task_id"], "task-1");
            assert!(request(tool, json!({"task_id":"task-1","extra":1})).is_err());
        }
        let assign = request(
            "orbit_agent_assign",
            json!({"name":"reviewer","prompt":"Review","timeout_ms":60000}),
        )
        .unwrap();
        let value = serde_json::to_value(assign).unwrap();
        assert_eq!(value["method"], "orbit.agent_assign");
        assert_eq!(value["params"]["timeout_ms"], 60000);
        assert!(request(
            "orbit_agent_prompt",
            json!({"name":"r","prompt":"p","timeout_ms":1})
        )
        .is_err());
        let tools = catalog();
        let assign_tool = tools
            .as_array()
            .unwrap()
            .iter()
            .find(|tool| tool["name"] == "orbit_agent_assign")
            .unwrap();
        assert!(assign_tool["inputSchema"]["properties"]["timeout_ms"].is_object());
        let report_tool = tools
            .as_array()
            .unwrap()
            .iter()
            .find(|tool| tool["name"] == "orbit_agent_report")
            .unwrap();
        let required = report_tool["inputSchema"]["required"].as_array().unwrap();
        assert!(
            !required.contains(&json!("token")),
            "the token file supplies it"
        );
        assert!(report_tool["inputSchema"]["properties"]["token"].is_object());
        let report = request(
            "orbit_agent_report",
            json!({"task_id":"t","worker_id":"w","token":"00ff","outcome":"completed","summary":"done"}),
        )
        .unwrap();
        let value = serde_json::to_value(report).unwrap();
        assert_eq!(value["method"], "orbit.agent_report");
        assert_eq!(value["params"]["token"], "00ff");
    }
    #[test]
    fn agent_report_token_comes_from_the_file_then_the_environment_then_json() {
        let base = json!({"task_id":"t","worker_id":"w","outcome":"completed","summary":"done"});
        let explicit = json!({"task_id":"t","worker_id":"w","token":"aa","outcome":"completed","summary":"done"});
        let file = || Some("f1".to_owned());
        let env = || Some("e2".to_owned());
        assert_eq!(
            with_report_token(explicit.clone(), file(), env())["token"],
            "f1"
        );
        assert_eq!(with_report_token(base.clone(), file(), None)["token"], "f1");
        assert_eq!(
            with_report_token(explicit.clone(), None, env())["token"],
            "e2"
        );
        assert_eq!(
            with_report_token(base.clone(), Some(String::new()), env())["token"],
            "e2"
        );
        assert_eq!(
            with_report_token(explicit.clone(), None, None)["token"],
            "aa"
        );
        assert_eq!(
            with_report_token(explicit, None, Some(String::new()))["token"],
            "aa"
        );
        // Without any source, nothing is invented.
        assert!(with_report_token(base.clone(), None, None)
            .get("token")
            .is_none());
        let filled = with_report_token(base, None, env());
        let request = request("orbit_agent_report", filled).unwrap();
        assert_eq!(
            serde_json::to_value(request).unwrap()["params"]["token"],
            "e2"
        );
        assert_eq!(REPORT_TOKEN_ENV, "ASTER_ORBIT_TOKEN");
    }
    #[test]
    fn orbit_mcp_report_summary_limit_is_described_in_bytes() {
        let tools = catalog();
        let report = tools
            .as_array()
            .unwrap()
            .iter()
            .find(|tool| tool["name"] == "orbit_agent_report")
            .unwrap();
        let summary = &report["inputSchema"]["properties"]["summary"];
        assert_eq!(summary["maxLength"], 4000);
        assert!(summary["description"]
            .as_str()
            .unwrap()
            .contains("4000 bytes"));
        assert!(report["description"]
            .as_str()
            .unwrap()
            .contains("token file"));
    }
    #[test]
    fn orbit_mcp_report_accepts_the_structured_fields() {
        let tools = catalog();
        let report = tools
            .as_array()
            .unwrap()
            .iter()
            .find(|tool| tool["name"] == "orbit_agent_report")
            .unwrap();
        let properties = &report["inputSchema"]["properties"];
        let keys = [
            "tests_run",
            "checks",
            "changed_files",
            "branch",
            "job_ids",
            "blocked_on",
        ];
        for key in keys {
            assert!(properties.get(key).is_some(), "{key} missing");
        }
        assert_eq!(report["inputSchema"]["additionalProperties"], false);
        assert_eq!(properties["checks"]["maxItems"], 20);
        assert_eq!(properties["changed_files"]["maxItems"], 200);
        let arguments = json!({"task_id":"t","worker_id":"w","token":"00ff","outcome":"blocked",
            "summary":"waiting","tests_run":false,
            "checks":[{"name":"cargo test","status":"failed","detail":"2 failures"}],
            "changed_files":["src/a.rs"],"branch":"orbit/x","job_ids":["job-1"],
            "blocked_on":"API key"});
        let value = serde_json::to_value(request("orbit_agent_report", arguments.clone()).unwrap())
            .unwrap();
        assert_eq!(value["params"], arguments);
    }
    #[test]
    fn orbit_mcp_maps_approval_tools_with_only_a_name() {
        for (tool, method) in [
            ("orbit_agent_approve", "orbit.agent_approve"),
            ("orbit_agent_deny", "orbit.agent_deny"),
        ] {
            let value =
                serde_json::to_value(request(tool, json!({"name":"reviewer"})).unwrap()).unwrap();
            assert_eq!(value["method"], method);
            assert_eq!(value["params"]["name"], "reviewer");
            assert!(request(tool, json!({"name":"reviewer","keys":["y"]})).is_err());
        }
        let tools = catalog();
        let approve = tools
            .as_array()
            .unwrap()
            .iter()
            .find(|tool| tool["name"] == "orbit_agent_approve")
            .unwrap();
        assert!(approve["description"]
            .as_str()
            .unwrap()
            .contains("lead_may_approve"));
    }
    #[test]
    fn orbit_mcp_maps_lifecycle_and_layout_tools() {
        for (tool, arguments, method) in [
            (
                "orbit_agent_stop",
                json!({"name":"w1","close_pane":false,"force":true}),
                "orbit.agent_stop",
            ),
            (
                "orbit_pane_resize",
                json!({"pane_id":"w1:p2","direction":"up","amount":200}),
                "orbit.pane_resize",
            ),
            (
                "orbit_pane_zoom",
                json!({"pane_id":"w1:p2","zoomed":true}),
                "orbit.pane_zoom",
            ),
            (
                "orbit_pane_close",
                json!({"pane_id":"w1:p2","force":true}),
                "orbit.pane_close",
            ),
            (
                "orbit_agent_start",
                json!({"name":"w1","kind":"claude","reuse":false,"worktree":"fix/a"}),
                "orbit.agent_start",
            ),
            (
                "orbit_agent_start",
                json!({"name":"w1","kind":"claude","worktree":true}),
                "orbit.agent_start",
            ),
            (
                "orbit_agent_start",
                json!({"name":"w1","kind":"claude","permission_mode":"manual"}),
                "orbit.agent_start",
            ),
        ] {
            let value = serde_json::to_value(request(tool, arguments.clone()).unwrap()).unwrap();
            assert_eq!(value["method"], method);
            for (key, expected) in arguments.as_object().unwrap() {
                assert_eq!(&value["params"][key], expected, "{tool} {key}");
            }
        }
        for amount in [0, 201] {
            assert!(request(
                "orbit_pane_resize",
                json!({"pane_id":"w1:p2","direction":"up","amount":amount})
            )
            .is_err());
        }
        assert!(request(
            "orbit_pane_zoom",
            json!({"pane_id":"w1:p2","zoomed":true,"x":1})
        )
        .is_err());
    }
    #[test]
    fn orbit_mcp_rejects_unknown_tools_and_bad_arguments() {
        assert!(request("pane_run", json!({})).is_err());
        assert!(request("orbit_run", json!({"command":" "})).is_err());
        assert!(request("orbit_read", json!({"lines":2001})).is_err());
        assert!(request("orbit_inspect", json!({"pane_id":"w1:p1"})).is_err());
        let error = request("orbit_agent_assign", json!({"nme":"x","prompt":"y"})).unwrap_err();
        assert!(
            error.contains("\"nme\"") && error.contains("name, role, kind, prompt"),
            "{error}"
        );
    }

    #[test]
    fn orbit_mcp_maps_team_tools_and_role_routing() {
        let define = json!({"name":"reviewer","area":"Reviews","prompt":"Be strict",
            "members":[{"name":"rc","kind":"claude","model":"opus"},{"name":"rx","kind":"codex"}]});
        for (tool, arguments, method) in [
            ("orbit_team_define", define, "orbit.team_define"),
            (
                "orbit_team_remove",
                json!({"name":"reviewer","stop_members":true,"force":true}),
                "orbit.team_remove",
            ),
            ("orbit_team_list", json!({}), "orbit.team_list"),
            (
                "orbit_agent_assign",
                json!({"role":"reviewer","kind":"codex","prompt":"Review"}),
                "orbit.agent_assign",
            ),
            (
                "orbit_agent_prompt",
                json!({"role":"reviewer","prompt":"Hello"}),
                "orbit.agent_prompt",
            ),
            (
                "orbit_agent_status",
                json!({"role":"reviewer"}),
                "orbit.agent_status",
            ),
            ("orbit_inspect", json!({"role":"reviewer"}), "orbit.inspect"),
        ] {
            let value = serde_json::to_value(request(tool, arguments.clone()).unwrap()).unwrap();
            assert_eq!(value["method"], method);
            for (key, expected) in arguments.as_object().unwrap() {
                assert_eq!(&value["params"][key], expected, "{tool} {key}");
            }
        }
        assert!(request("orbit_team_list", json!({"name":"x"})).is_err());
        assert!(request("orbit_team_remove", json!({"name":"x","close":true})).is_err());
        let tools = catalog();
        let assign = tools
            .as_array()
            .unwrap()
            .iter()
            .find(|tool| tool["name"] == "orbit_agent_assign")
            .unwrap();
        assert_eq!(assign["inputSchema"]["required"], json!(["prompt"]));
        assert!(assign["description"]
            .as_str()
            .unwrap()
            .contains("responsible team role"));
    }

    #[test]
    fn orbit_mcp_accepts_agent_and_worker_as_aliases_for_name() {
        for alias in ["agent", "worker"] {
            let value = serde_json::to_value(
                request("orbit_agent_status", json!({alias: "checker"})).unwrap(),
            )
            .unwrap();
            assert_eq!(value["params"]["name"], "checker");
            assert!(value["params"].get(alias).is_none());
        }
        let value = serde_json::to_value(
            request(
                "orbit_agent_assign",
                json!({"agent":"checker","prompt":"go"}),
            )
            .unwrap(),
        )
        .unwrap();
        assert_eq!(value["params"]["name"], "checker");
    }
}
