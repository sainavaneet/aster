//! Harness for Orbit end-to-end tests: one isolated `aster server` per test, a
//! fake Claude lead and workers (`tests/fixtures/orbit/claude`), fake Codex
//! workers (`tests/fixtures/orbit/codex`), and the real CLI.
//!
//! Isolation: every test gets its own base directory under /tmp holding the
//! config home, runtime directory, API socket, HOME, and fake-agent state, plus
//! a unique `ASTER_SESSION`. The server runs with a scrubbed environment whose
//! PATH is the fake-agent directory followed by system directories only, and
//! pane shells are non-login, so `claude` and `codex` always resolve to the fakes
//! and never to installed agent CLIs.
#![allow(dead_code)]

use std::fs;
use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output, Stdio};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use serde_json::{json, Value};

use crate::support;

/// The fake lead's identity: `[polaris] model`, which Polaris passes as `--model`.
pub const LEAD: &str = "lead";
/// The report token variable older assignments set; still accepted by the CLI.
pub const TOKEN_ENV: &str = "ASTER_ORBIT_TOKEN";
/// The one Claude permission rule Orbit adds for its workers: the report command.
pub fn report_allow_rule() -> String {
    format!("Bash({} orbit agent-report:*)", env!("CARGO_BIN_EXE_aster"))
}

/// Whether `argv` pre-allows exactly the report command, once.
pub fn allows_only_report(argv: &[String]) -> bool {
    let flags: Vec<usize> = argv
        .iter()
        .enumerate()
        .filter(|(_, arg)| arg.starts_with("--allowedTools") || arg.starts_with("--allowed-tools"))
        .map(|(index, _)| index)
        .collect();
    matches!(flags.as_slice(), [index] if argv.get(index + 1) == Some(&report_allow_rule()))
}

pub const STATE_TIMEOUT: Duration = Duration::from_secs(30);
const READY_TIMEOUT: Duration = Duration::from_secs(40);
const STOP_TIMEOUT: Duration = Duration::from_secs(15);
const POLL: Duration = Duration::from_millis(100);
const READ_LINES: u32 = 2000;
const FAKE_AGENT: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/orbit/claude");
const FAKE_CODEX: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/orbit/codex");
const SYSTEM_PATH: &str = "/usr/bin:/bin:/usr/sbin:/sbin";
const CONFIG: &str = "onboarding = false\n\
[terminal]\ndefault_shell = \"/bin/sh\"\nshell_mode = \"non_login\"\n\
[polaris]\nmodel = \"lead\"\n";

pub const REPORT_REJECTED: &str = "Assignment not found or token invalid; report with the exact task_id, worker_id, and token from your assignment";
pub const CONFLICTING_REPORT: &str = "Assignment already has a different report";
pub const OUT_OF_SCOPE: &str = "Reporting worker is not inside the bound Orbit tab";
pub const CANCELLED: &str =
    "Assignment was cancelled by Polaris; stop this task and do not retry the report";
pub const LEDGER_FULL: &str = "Orbit assignment ledger is full (64)";

/// What the worker needs to report, as read from its own terminal.
#[derive(Debug, Clone)]
pub struct Assignment {
    pub task_id: String,
    pub worker_id: String,
    pub token: String,
}

#[derive(Debug, Clone)]
pub struct Worker {
    pub name: String,
    pub terminal_id: String,
    pub pane_id: String,
}

/// A CLI invocation: exit status plus the JSON it printed (stdout on success,
/// stderr on failure).
#[derive(Debug)]
pub struct CliResult {
    pub success: bool,
    pub body: Value,
    pub raw: String,
}

impl CliResult {
    pub fn error_message(&self) -> &str {
        self.body["error"]["message"].as_str().unwrap_or(&self.raw)
    }
}

pub struct Orbit {
    base: PathBuf,
    session: String,
    server: Option<Child>,
}

impl Orbit {
    pub fn new(case: &str) -> Self {
        Self::with_config(case, "")
    }

    /// Like `new`, with `extra` TOML appended to the shared test config.
    pub fn with_config(case: &str, extra: &str) -> Self {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|elapsed| elapsed.as_nanos())
            .unwrap_or(0);
        let base = PathBuf::from(format!("/tmp/aster-orbit-{}-{nanos}", std::process::id()));
        for dir in [
            "config/aster-dev",
            "runtime",
            "state",
            "work",
            "bin",
            "home",
        ] {
            fs::create_dir_all(base.join(dir)).unwrap();
        }
        let base = fs::canonicalize(base).unwrap();
        fs::copy(FAKE_AGENT, base.join("bin/claude")).unwrap();
        fs::copy(FAKE_CODEX, base.join("bin/codex")).unwrap();
        fs::write(
            base.join("config/aster-dev/config.toml"),
            format!("{CONFIG}{extra}"),
        )
        .unwrap();
        support::register_runtime_dir(&base.join("runtime"));
        Self {
            base,
            session: format!("orbit-{case}-{}", std::process::id()),
            server: None,
        }
    }

    pub fn socket(&self) -> PathBuf {
        self.base.join("runtime/a.sock")
    }

    /// `orbit-tasks.json` in the session data directory.
    pub fn ledger_path(&self) -> PathBuf {
        self.base
            .join("config/aster-dev/sessions")
            .join(&self.session)
            .join("orbit-tasks.json")
    }

    /// The task's report token file in the session data directory.
    pub fn token_path(&self, task_id: &str) -> PathBuf {
        self.base
            .join("config/aster-dev/sessions")
            .join(&self.session)
            .join("orbit-tokens")
            .join(task_id)
    }

    /// The arguments the named fake agent was launched with, one per entry.
    pub fn agent_argv(&self, name: &str) -> Vec<String> {
        fs::read_to_string(self.base.join(format!("state/{name}.argv")))
            .unwrap_or_default()
            .split('\0')
            .filter(|arg| !arg.is_empty())
            .map(str::to_owned)
            .collect()
    }

    /// `orbit-team.json` in the session data directory.
    pub fn team_path(&self) -> PathBuf {
        self.ledger_path().with_file_name("orbit-team.json")
    }

    /// The pid a fake agent named `name` wrote, once it exists.
    pub fn agent_pid(&self, name: &str) -> Option<String> {
        fs::read_to_string(self.base.join(format!("state/{name}.pid")))
            .ok()
            .map(|pid| pid.trim().to_owned())
            .filter(|pid| !pid.is_empty())
    }

    /// Tab labels of the Polaris workspace, in order.
    pub fn polaris_tabs(&self) -> Vec<String> {
        let lead = self.result("agent.get", json!({"target": "polaris"}))["agent"].clone();
        let tabs = self.result("tab.list", json!({"workspace_id": lead["workspace_id"]}));
        tabs["tabs"]
            .as_array()
            .unwrap()
            .iter()
            .map(|tab| tab["label"].as_str().unwrap_or_default().to_owned())
            .collect()
    }

    /// `orbit.team_list` role `name`.
    pub fn team_role(&self, name: &str) -> Value {
        let list = self.result("orbit.team_list", json!({}));
        list["roles"]
            .as_array()
            .unwrap()
            .iter()
            .find(|role| role["name"] == name)
            .cloned()
            .unwrap_or(Value::Null)
    }

    /// A command with the scrubbed environment shared by server and CLI.
    pub fn command(&self, program: impl AsRef<std::ffi::OsStr>) -> Command {
        let mut command = Command::new(program);
        command
            .env_clear()
            .current_dir(self.base.join("work"))
            .env("HOME", self.base.join("home"))
            .env("TERM", "xterm-256color")
            .env("SHELL", "/bin/sh")
            .env(
                "PATH",
                format!("{}:{SYSTEM_PATH}", self.base.join("bin").display()),
            )
            .env("XDG_CONFIG_HOME", self.base.join("config"))
            .env("XDG_RUNTIME_DIR", self.base.join("runtime"))
            .env("ASTER_SOCKET_PATH", self.socket())
            .env("ASTER_SESSION", &self.session)
            .env("ORBIT_FAKE_STATE", self.base.join("state"));
        command
    }

    /// Starts (or restarts) the server as an interactive startup, which opens
    /// Polaris and the bound Orbit tab, and waits for the lead's idle prompt.
    pub fn start(&mut self) {
        assert!(self.server.is_none(), "server already running");
        let log = fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(self.base.join("server.out"))
            .unwrap();
        let child = self
            .command(env!("CARGO_BIN_EXE_aster"))
            .arg("server")
            .env("ASTER_STARTUP_CWD", self.base.join("work"))
            .stdin(Stdio::null())
            .stdout(log.try_clone().unwrap())
            .stderr(log)
            .spawn()
            .unwrap();
        support::register_spawned_aster_pid(Some(child.id()));
        self.server = Some(child);
        support::wait_for_socket(&self.socket(), READY_TIMEOUT);
        self.wait_until("Polaris lead idle at its prompt", READY_TIMEOUT, |orbit| {
            let lead = &orbit.activity()["lead"];
            lead["state"] == "idle" && lead["visible_idle"] == true
        });
        assert!(
            self.base.join("state/lead.pid").exists(),
            "Polaris is not the fake agent"
        );
    }

    /// Graceful `server.stop`, which saves the session; falls back to SIGKILL.
    pub fn stop(&mut self) {
        let Some(mut child) = self.server.take() else {
            return;
        };
        let _ = self.try_request("server.stop", json!({}));
        let deadline = Instant::now() + STOP_TIMEOUT;
        while Instant::now() < deadline {
            if child.try_wait().ok().flatten().is_some() {
                support::unregister_spawned_aster_pid(Some(child.id()));
                return;
            }
            thread::sleep(POLL);
        }
        let _ = child.kill();
        let _ = child.wait();
        support::unregister_spawned_aster_pid(Some(child.id()));
        panic!("server did not stop within {STOP_TIMEOUT:?}");
    }

    fn try_request(&self, method: &str, params: Value) -> std::io::Result<Value> {
        let mut stream = UnixStream::connect(self.socket())?;
        stream.set_read_timeout(Some(Duration::from_secs(20)))?;
        let request =
            json!({"id": format!("orbit-test:{method}"), "method": method, "params": params});
        writeln!(stream, "{request}")?;
        let mut line = String::new();
        BufReader::new(stream).read_line(&mut line)?;
        serde_json::from_str(&line).map_err(std::io::Error::other)
    }

    pub fn request(&self, method: &str, params: Value) -> Value {
        self.try_request(method, params)
            .unwrap_or_else(|error| panic!("{method} failed: {error}"))
    }

    /// The `result` of a request that must succeed.
    pub fn result(&self, method: &str, params: Value) -> Value {
        let response = self.request(method, params);
        assert!(response.get("error").is_none(), "{method}: {response}");
        response["result"].clone()
    }

    pub fn activity(&self) -> Value {
        self.result("orbit.activity", json!({}))
    }

    /// `orbit.task_status`: `{task, tombstone, ledger_error}`.
    pub fn status(&self, task_id: &str) -> Value {
        self.result("orbit.task_status", json!({"task_id": task_id}))
    }

    pub fn delivery(&self, task_id: &str) -> String {
        self.status(task_id)["task"]["delivery"]
            .as_str()
            .unwrap_or("<none>")
            .to_owned()
    }

    /// Runs the real `aster` binary with this test's socket.
    pub fn cli(&self, args: &[&str]) -> Output {
        self.cli_with_env(args, &[])
    }

    pub fn cli_with_env(&self, args: &[&str], env: &[(&str, &str)]) -> Output {
        let mut command = self.command(env!("CARGO_BIN_EXE_aster"));
        command.args(args).stdin(Stdio::null());
        for (key, value) in env {
            command.env(key, value);
        }
        command.output().unwrap()
    }

    /// `aster orbit <operation> '<json>'`.
    pub fn orbit_cli(&self, operation: &str, arguments: &Value) -> CliResult {
        Self::cli_result(self.cli(&["orbit", operation, &arguments.to_string()]))
    }

    fn cli_result(output: Output) -> CliResult {
        let stream = if output.status.success() {
            &output.stdout
        } else {
            &output.stderr
        };
        let raw = String::from_utf8_lossy(stream).trim().to_owned();
        CliResult {
            success: output.status.success(),
            body: serde_json::from_str(&raw).unwrap_or(Value::Null),
            raw,
        }
    }

    /// Starts a fake worker in `pane_id` (Orbit's root pane when `None`).
    pub fn start_worker_in(&self, name: &str, pane_id: Option<&str>) -> Worker {
        self.start_kind_in("claude", name, pane_id)
    }

    /// Starts a fake `kind` worker (`claude` or `codex`); its model is its name.
    pub fn start_kind_in(&self, kind: &str, name: &str, pane_id: Option<&str>) -> Worker {
        let mut params = json!({"name": name, "kind": kind, "model": name});
        if let Some(pane_id) = pane_id {
            params["pane_id"] = json!(pane_id);
        }
        // A pane split a moment ago may still be starting its shell; the server
        // reports that as a retryable error, exactly as a lead would see it.
        let mut last = Value::Null;
        self.wait_until("shell ready for agent start", READY_TIMEOUT, |orbit| {
            last = orbit.request("orbit.agent_start", params.clone());
            last["error"]["code"] != "agent_shell_starting"
        });
        assert!(last.get("error").is_none(), "orbit.agent_start: {last}");
        self.wait_worker(name)
    }

    pub fn start_worker(&self, name: &str) -> Worker {
        self.start_worker_in(name, None)
    }

    /// Waits for a (possibly resumed) worker to be the idle fake agent.
    pub fn wait_worker(&self, name: &str) -> Worker {
        let mut agent = Value::Null;
        self.wait_until(&format!("worker {name} idle"), READY_TIMEOUT, |orbit| {
            let response = orbit.request("orbit.agent_status", json!({"name": name}));
            agent = response["result"]["agent"].clone();
            agent["agent_status"] == "idle" && agent["interactive_ready"] == true
        });
        assert!(
            self.base.join(format!("state/{name}.pid")).exists(),
            "worker {name} is not the fake agent"
        );
        Worker {
            name: name.to_owned(),
            terminal_id: agent["terminal_id"].as_str().unwrap().to_owned(),
            pane_id: agent["pane_id"].as_str().unwrap().to_owned(),
        }
    }

    /// Assigns through the API and returns the raw response.
    pub fn assign_raw(&self, worker: &str, prompt: &str) -> Value {
        self.request(
            "orbit.agent_assign",
            json!({"name": worker, "prompt": prompt}),
        )
    }

    /// Assigns, then extracts the report credentials from the assignment text
    /// the worker's PTY delivered (the fake agent's input log).
    pub fn assign(&self, worker: &Worker, prompt: &str) -> Assignment {
        let response = self.assign_raw(&worker.name, prompt);
        let task_id = response["result"]["task_id"]
            .as_str()
            .unwrap_or_else(|| panic!("assignment failed: {response}"))
            .to_owned();
        let mut found = None;
        self.wait_until(
            &format!("assignment {task_id} at the worker PTY"),
            STATE_TIMEOUT,
            |orbit| {
                found = orbit.instruction(&worker.name, &task_id);
                found.is_some()
            },
        );
        let assignment = found.unwrap();
        assert_eq!(assignment.worker_id, worker.terminal_id);
        assignment
    }

    pub fn read_pane(&self, pane_id: &str) -> String {
        let read = self.result(
            "pane.read",
            json!({"pane_id": pane_id, "source": "recent_unwrapped", "lines": READ_LINES}),
        );
        read["read"]["text"].as_str().unwrap_or_default().to_owned()
    }

    /// Parses the report command from the `[Aster assignment <id>]` block the
    /// named fake agent received. The command starts with the executable and
    /// carries no credentials; the token is read from the task's token file.
    pub fn instruction(&self, agent: &str, task_id: &str) -> Option<Assignment> {
        let header = format!("[Aster assignment {task_id}]");
        let prompt = self
            .prompts(agent)
            .into_iter()
            .find(|prompt| prompt.contains(&header))?;
        let command = prompt
            .lines()
            .skip_while(|line| !line.contains(&header))
            .filter_map(|line| serde_json::from_str::<Value>(line.trim()).ok())
            .find(|value| value["json_argument"]["task_id"] == task_id)?;
        let argument = &command["json_argument"];
        assert_eq!(command["arguments"], json!(["orbit", "agent-report"]));
        assert!(command.get("environment").is_none(), "{command}");
        assert!(argument.get("token").is_none(), "token in json_argument");
        let file = fs::read_to_string(self.token_path(task_id)).ok()?;
        let mut lines = file.lines();
        let token = lines.next()?.to_owned();
        assert_eq!(lines.next().map(PathBuf::from), Some(self.socket()));
        assert!(!prompt.contains(&token), "token in the worker prompt");
        Some(Assignment {
            task_id: task_id.to_owned(),
            worker_id: argument["worker_id"].as_str()?.to_owned(),
            token,
        })
    }

    /// The worker's report through the real CLI, as instructed: the JSON
    /// argument only, with no token and no environment of its own; the CLI
    /// reads the task's token file.
    pub fn report(&self, assignment: &Assignment, summary: &str) -> CliResult {
        self.report_with(assignment, summary, json!({}))
    }

    /// `report` with extra JSON keys, such as the structured report fields.
    pub fn report_with(&self, assignment: &Assignment, summary: &str, extra: Value) -> CliResult {
        let mut arguments = json!({
            "task_id": assignment.task_id,
            "worker_id": assignment.worker_id,
            "outcome": "completed",
            "summary": summary,
        });
        if let (Some(arguments), Value::Object(extra)) = (arguments.as_object_mut(), extra) {
            arguments.extend(extra);
        }
        Self::cli_result(self.cli(&["orbit", "agent-report", &arguments.to_string()]))
    }

    /// A report straight to the API presenting `assignment.token`, for checks
    /// the token file would otherwise answer, such as a wrong token.
    pub fn report_api(&self, assignment: &Assignment, summary: &str) -> Value {
        self.request(
            "orbit.agent_report",
            json!({
                "task_id": assignment.task_id,
                "worker_id": assignment.worker_id,
                "token": assignment.token,
                "outcome": "completed",
                "summary": summary,
            }),
        )
    }

    /// Every prompt the named fake agent received through its PTY.
    pub fn prompts(&self, agent: &str) -> Vec<String> {
        let log = fs::read_to_string(self.base.join(format!("state/{agent}.prompts")))
            .unwrap_or_default();
        log.split("=== prompt ")
            .filter(|chunk| !chunk.is_empty())
            .map(|chunk| {
                chunk
                    .split_once('\n')
                    .map_or("", |(_, body)| body)
                    .to_owned()
            })
            .collect()
    }

    /// Lead prompts carrying `marker`.
    pub fn lead_prompts_with(&self, marker: &str) -> Vec<String> {
        self.prompts(LEAD)
            .into_iter()
            .filter(|prompt| prompt.contains(marker))
            .collect()
    }

    /// Waits for the lead to receive `marker` and returns that prompt.
    pub fn wait_lead_prompt(&self, marker: &str) -> String {
        self.wait_until(
            &format!("lead prompt with {marker}"),
            STATE_TIMEOUT,
            |orbit| !orbit.lead_prompts_with(marker).is_empty(),
        );
        self.lead_prompts_with(marker).remove(0)
    }

    /// The bound lead's pane, checked against the ledger's lead binding.
    pub fn lead_pane(&self) -> String {
        let agent = self.result("agent.get", json!({"target": "polaris"}))["agent"].clone();
        assert_eq!(agent["terminal_id"], self.activity()["lead"]["terminal_id"]);
        agent["pane_id"].as_str().unwrap().to_owned()
    }

    /// Switches a fake agent between its working indicator and idle prompt.
    pub fn set_busy(&self, agent: &str, busy: bool) {
        let mode = if busy { "busy" } else { "idle" };
        fs::write(self.base.join(format!("state/{agent}.mode")), mode).unwrap();
    }

    /// Writes a fake agent's mode file: `idle`, `busy`, or (Claude) `blocked` or `trust`.
    pub fn set_mode(&self, agent: &str, mode: &str) {
        fs::write(self.base.join(format!("state/{agent}.mode")), mode).unwrap();
    }

    /// Permission-prompt answers the fake Claude received, one per line.
    pub fn answers(&self, agent: &str) -> Vec<String> {
        fs::read_to_string(self.base.join(format!("state/{agent}.answers")))
            .unwrap_or_default()
            .lines()
            .map(str::to_owned)
            .collect()
    }

    pub fn wait_until(&self, what: &str, timeout: Duration, mut done: impl FnMut(&Self) -> bool) {
        let deadline = Instant::now() + timeout;
        while Instant::now() < deadline {
            if done(self) {
                return;
            }
            thread::sleep(POLL);
        }
        let activity = self
            .try_request("orbit.activity", json!({}))
            .map(|value| value.to_string())
            .unwrap_or_else(|error| error.to_string());
        panic!("timed out after {timeout:?} waiting for {what}; activity: {activity}");
    }

    pub fn wait_delivery(&self, task_id: &str, expected: &str) {
        self.wait_until(
            &format!("{task_id} delivery {expected}"),
            STATE_TIMEOUT,
            |orbit| orbit.delivery(task_id) == expected,
        );
    }

    pub fn base(&self) -> &Path {
        &self.base
    }

    /// Appends TOML to the test config; call before `start`.
    pub fn append_config(&self, toml: &str) {
        let path = self.base.join("config/aster-dev/config.toml");
        let mut config = fs::read_to_string(&path).unwrap();
        config.push_str(toml);
        fs::write(path, config).unwrap();
    }

    /// A new shell pane to the right of Orbit's root pane.
    pub fn split_right(&self) -> String {
        let split = self.result("orbit.split", json!({"direction": "right"}));
        split["pane"]["pane_id"]
            .as_str()
            .unwrap_or_else(|| panic!("split result has no pane: {split}"))
            .to_owned()
    }

    /// `pane.get` for an existing pane, `None` once it is gone.
    pub fn pane(&self, pane_id: &str) -> Option<Value> {
        let response = self.request("pane.get", json!({"pane_id": pane_id}));
        response
            .get("error")
            .is_none()
            .then(|| response["result"]["pane"].clone())
    }

    /// Runs git in `dir` with a fixed identity; panics on failure.
    pub fn git(&self, dir: &Path, args: &[&str]) -> String {
        let output = Command::new("git")
            .args([
                "-c",
                "user.name=Orbit",
                "-c",
                "user.email=orbit@example.invalid",
            ])
            .args([
                "-c",
                "init.defaultBranch=main",
                "-c",
                "commit.gpgsign=false",
            ])
            .arg("-C")
            .arg(dir)
            .args(args)
            .env("HOME", self.base.join("home"))
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "git {args:?}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8_lossy(&output.stdout).into_owned()
    }
}

impl Drop for Orbit {
    fn drop(&mut self) {
        if let Some(mut child) = self.server.take() {
            let _ = self.try_request("server.stop", json!({}));
            let deadline = Instant::now() + STOP_TIMEOUT;
            while Instant::now() < deadline && child.try_wait().ok().flatten().is_none() {
                thread::sleep(POLL);
            }
            let _ = child.kill();
            let _ = child.wait();
            support::unregister_spawned_aster_pid(Some(child.id()));
        }
        support::cleanup_test_base(&self.base);
    }
}
