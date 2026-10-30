//! Terminal presentation of server-owned Orbit jobs and workers.
use std::fmt::Write as _;
use std::io::{self, IsTerminal, Write};
use std::time::Duration;

use serde_json::Value;

use crate::api::client::ApiClient;
use crate::api::schema::{EmptyParams, Method, Request};

pub(super) const MAX_ROWS: usize = 20;

/// Keep arbitrary command/output metadata from injecting terminal controls.
/// Like `cell`, for worker-provided free text: labelled secrets are hidden on
/// screen. The stored report and the copy delivered to the lead stay verbatim.
pub(super) fn redacted_cell(value: Option<&Value>, width: usize) -> String {
    match value.and_then(Value::as_str) {
        Some(text) => cell(
            Some(&Value::String(crate::app::orbit_redact_secrets(text))),
            width,
        ),
        None => cell(value, width),
    }
}

pub(super) fn cell(value: Option<&Value>, width: usize) -> String {
    let text = value
        .and_then(Value::as_str)
        .map(str::to_owned)
        .or_else(|| value.filter(|v| v.is_number()).map(ToString::to_string))
        .unwrap_or_else(|| "—".into());
    let mut chars = text.chars().map(|c| if c.is_control() { ' ' } else { c });
    let mut rendered: String = chars.by_ref().take(width).collect();
    if chars.next().is_some() && width > 0 {
        rendered.pop();
        rendered.push('…');
    }
    rendered
}

/// Compact age such as `42s`, `7m`, or `3h` from unix-millisecond timestamps.
fn age(created_at: Option<&Value>, now_ms: u64) -> String {
    let Some(created) = created_at.and_then(Value::as_u64).filter(|ms| *ms > 0) else {
        return "—".into();
    };
    let seconds = now_ms.saturating_sub(created) / 1000;
    match seconds {
        0..=59 => format!("{seconds}s"),
        60..=3599 => format!("{}m", seconds / 60),
        _ => format!("{}h", seconds / 3600),
    }
}

pub(super) fn yes_no(value: Option<&Value>) -> &'static str {
    match value.and_then(Value::as_bool) {
        Some(true) => "yes",
        Some(false) => "no",
        None => "—",
    }
}

/// `  checks=3 passed, 1 failed` when the report lists checks.
pub(super) fn checks_cell(report: &Value) -> String {
    let Some(checks) = report.get("checks").and_then(Value::as_array) else {
        return String::new();
    };
    let statuses = checks
        .iter()
        .filter_map(|check| check.get("status").and_then(Value::as_str));
    let summary = crate::app::orbit_checks_summary(statuses);
    let summary = if summary.is_empty() {
        "none".into()
    } else {
        summary
    };
    format!("  checks={summary}")
}

pub(super) fn render_lead(out: &mut String, result: &Value) {
    out.push_str("\nPolaris lead\n");
    if let Some(lead) = result.get("lead").filter(|lead| lead.is_object()) {
        let _ = writeln!(
            out,
            "terminal={}  present={}  state={}  visible_idle={}  visible_working={}  queued_reports={}",
            cell(lead.get("terminal_id"), 40),
            yes_no(lead.get("present")),
            cell(lead.get("state"), 12),
            yes_no(lead.get("visible_idle")),
            yes_no(lead.get("visible_working")),
            cell(lead.get("queued_reports"), 4),
        );
    } else {
        out.push_str("No lead status.\n");
    }
    if let Some(error) = result.get("lead_error").filter(|error| !error.is_null()) {
        let _ = writeln!(out, "Lead problem: {}", cell(Some(error), 200));
    }
}

pub(super) fn render_kinds(out: &mut String, result: &Value) {
    out.push_str("\nWorker kinds\n");
    let Some(kinds) = result
        .get("kinds")
        .and_then(Value::as_array)
        .filter(|kinds| !kinds.is_empty())
    else {
        out.push_str("No worker kinds allowed.\n");
        return;
    };
    let _ = writeln!(
        out,
        "{:<12} {:<18} {:<10} {:<10} Models",
        "Kind", "Executable", "Installed", "Default"
    );
    for kind in kinds.iter().take(MAX_ROWS) {
        let models: Vec<_> = kind
            .get("models")
            .and_then(Value::as_array)
            .map(|models| models.iter().map(|model| cell(Some(model), 24)).collect())
            .unwrap_or_default();
        let models = if models.is_empty() {
            "any".to_owned()
        } else {
            cell(Some(&Value::String(models.join(", "))), 60)
        };
        let _ = writeln!(
            out,
            "{:<12} {:<18} {:<10} {:<10} {models}",
            cell(kind.get("kind"), 12),
            cell(kind.get("executable"), 18),
            yes_no(kind.get("installed")),
            cell(kind.get("default_model"), 10),
        );
    }
}

/// Team roles with their members, grouped by role.
pub(super) fn render_team(out: &mut String, result: &Value) {
    let Some(roles) = result
        .get("team")
        .and_then(Value::as_array)
        .filter(|roles| !roles.is_empty())
    else {
        return;
    };
    out.push_str("\nTeam roles\n");
    for role in roles.iter().take(MAX_ROWS) {
        let missing = if role.get("tab_missing").and_then(Value::as_bool) == Some(true) {
            "  (tab missing)"
        } else {
            ""
        };
        let _ = writeln!(
            out,
            "{}  tab={}  area={}{missing}",
            cell(role.get("label"), 22),
            cell(role.get("tab_id"), 14),
            cell(role.get("area"), 60),
        );
        let members = role.get("members").and_then(Value::as_array);
        for member in members.into_iter().flatten().take(MAX_ROWS) {
            let _ = writeln!(
                out,
                "  {:<20} {:<8} {:<12} {:<12} pane={}  open_tasks={}",
                cell(member.get("name"), 20),
                cell(member.get("kind"), 8),
                cell(member.get("state"), 12),
                cell(member.get("agent_status"), 12),
                cell(member.get("pane_id"), 14),
                cell(member.get("open_tasks"), 4),
            );
        }
    }
}

fn render_blocked(out: &mut String, result: &Value, now_ms: u64) {
    out.push_str("\nBlocked workers\n");
    let Some(blocked) = result
        .get("blocked_workers")
        .and_then(Value::as_array)
        .filter(|blocked| !blocked.is_empty())
    else {
        out.push_str("No blocked workers.\n");
        return;
    };
    for worker in blocked.iter().take(MAX_ROWS) {
        let _ = writeln!(
            out,
            "{}  pane={}  task={}  blocked_for={}",
            cell(worker.get("name"), 22),
            cell(worker.get("pane_id"), 14),
            cell(worker.get("task_id"), 64),
            age(worker.get("since_ms"), now_ms),
        );
    }
    out.push_str("Answer in the worker's pane; Polaris answers only with [polaris.approvals] lead_may_approve.\n");
}

fn render_tasks(out: &mut String, result: &Value, now_ms: u64) {
    out.push_str("\nAssignments / Polaris delivery\n");
    if let Some(error) = result.get("ledger_error").filter(|error| !error.is_null()) {
        let _ = writeln!(out, "Ledger problem: {}", cell(Some(error), 200));
    }
    let Some(tasks) = result
        .get("tasks")
        .and_then(Value::as_array)
        .filter(|tasks| !tasks.is_empty())
    else {
        out.push_str("No assignments yet.\n");
        return;
    };
    for task in tasks.iter().rev().take(MAX_ROWS) {
        let _ = writeln!(
            out,
            "{}  worker={}  submission={}  Polaris={}  attempts={}  age={}  origin={}",
            cell(task.get("task_id"), 64),
            cell(task.get("worker_id"), 32),
            cell(task.get("submission"), 20),
            cell(task.get("delivery"), 20),
            cell(task.get("attempts"), 4),
            age(task.get("created_at"), now_ms),
            cell(task.get("origin"), 8),
        );
        if let Some(report) = task.get("report").filter(|report| !report.is_null()) {
            let _ = writeln!(
                out,
                "  {}: {}  tests={}{}",
                cell(report.get("outcome"), 12),
                redacted_cell(report.get("summary"), 120),
                yes_no(report.get("tests_run")),
                checks_cell(report),
            );
        }
        if let Some(error) = task.get("error").filter(|error| !error.is_null()) {
            let _ = writeln!(
                out,
                "  Delivery detail: {}",
                redacted_cell(Some(error), 160)
            );
        }
    }
    if tasks.len() > MAX_ROWS {
        let _ = writeln!(
            out,
            "Showing the latest {MAX_ROWS} of {} assignments.",
            tasks.len()
        );
    }
}

/// Panes whose worker agent exited; Polaris closes them with orbit_pane_close.
pub(super) fn render_stale(out: &mut String, result: &Value) {
    let Some(stale) = result
        .get("stale_panes")
        .and_then(Value::as_array)
        .filter(|stale| !stale.is_empty())
    else {
        return;
    };
    out.push_str("\nExited worker panes\n");
    for pane in stale.iter().take(MAX_ROWS) {
        let _ = writeln!(
            out,
            "{}  pane={}",
            cell(pane.get("label"), 32),
            cell(pane.get("pane_id"), 14),
        );
    }
    out.push_str("Close one: aster orbit pane-close '{\"pane_id\":\"ID\"}'\n");
}

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

pub(super) fn render_jobs(out: &mut String, result: &Value) {
    out.push_str("\nJobs\n");
    let jobs = result.get("jobs").and_then(Value::as_array);
    if let Some(jobs) = jobs.filter(|jobs| !jobs.is_empty()) {
        let _ = writeln!(out, "{:<48} {:<12} {:<6} Command", "ID", "Status", "Exit");
        for job in jobs.iter().rev().take(MAX_ROWS) {
            let _ = writeln!(
                out,
                "{:<48} {:<12} {:<6} {}",
                cell(job.get("job_id"), 48),
                cell(job.get("status"), 12),
                cell(job.get("exit_code"), 6),
                cell(job.get("command"), 52),
            );
        }
        if jobs.len() > MAX_ROWS {
            let _ = writeln!(out, "Showing the latest {MAX_ROWS} of {} jobs.", jobs.len());
        }
    } else {
        out.push_str("No jobs yet.\n");
    }
}

fn render_workers(out: &mut String, result: &Value) {
    out.push_str("\nWorkers\n");
    let agents = result.get("agents").and_then(Value::as_array);
    if let Some(agents) = agents.filter(|agents| !agents.is_empty()) {
        let _ = writeln!(
            out,
            "{:<22} {:<14} {:<14} Provider",
            "Name", "Status", "Pane"
        );
        for agent in agents.iter().take(MAX_ROWS) {
            let _ = writeln!(
                out,
                "{:<22} {:<14} {:<14} {}",
                cell(agent.get("name"), 22),
                cell(agent.get("agent_status"), 14),
                cell(agent.get("pane_id"), 14),
                cell(agent.get("agent"), 18),
            );
        }
        if agents.len() > MAX_ROWS {
            let _ = writeln!(out, "Showing {MAX_ROWS} of {} workers.", agents.len());
        }
    } else {
        out.push_str("No workers yet.\n");
    }
}

/// The command hints every view ends with.
pub(super) fn render_footer(out: &mut String, watch: bool) {
    out.push_str("\nInspect output: aster orbit job-status '{\"job_id\":\"ID\"}'\n");
    out.push_str("Cancel a job:   aster orbit job-cancel '{\"job_id\":\"ID\"}'\n");
    out.push_str("Inspect a task: aster orbit task-status '{\"task_id\":\"ID\"}'\n");
    out.push_str("Cancel a task:  aster orbit task-cancel '{\"task_id\":\"ID\"}'\n");
    if watch {
        out.push_str("\nRefreshing every second. Ctrl+C closes this view; jobs keep running.\n");
    }
}

fn render(result: &Value, watch: bool, now_ms: u64) -> String {
    let mut out = String::from("Orbit activity\n");
    render_jobs(&mut out, result);
    render_workers(&mut out, result);
    render_team(&mut out, result);
    render_blocked(&mut out, result, now_ms);
    render_stale(&mut out, result);
    render_kinds(&mut out, result);
    render_lead(&mut out, result);
    render_tasks(&mut out, result, now_ms);
    render_footer(&mut out, watch);
    out
}

/// The glyphs of the modern view, or `None` for the classic one. Follows
/// `ui.style` and `ui.glyphs`; an unreadable config yields the defaults.
fn style_glyphs() -> Option<&'static crate::ui::chrome::GlyphSet> {
    let ui = crate::config::Config::load().config.ui;
    ui.style
        .is_modern()
        .then(|| crate::ui::chrome::GlyphSet::for_config(ui.glyphs))
}

fn render_styled(
    result: &Value,
    watch: bool,
    now_ms: u64,
    glyphs: Option<&crate::ui::chrome::GlyphSet>,
) -> String {
    match glyphs {
        Some(glyphs) => super::orbit_activity_modern::render(result, watch, now_ms, glyphs),
        None => render(result, watch, now_ms),
    }
}

pub(super) fn run(watch: bool) -> io::Result<i32> {
    if watch && !io::stdout().is_terminal() {
        return Err(io::Error::other(
            "activity --watch requires a terminal; use activity for a single snapshot",
        ));
    }
    let client = ApiClient::local();
    let glyphs = style_glyphs();
    let mut sequence = 0_u64;
    loop {
        sequence += 1;
        let request = Request {
            id: format!("orbit-activity:{}:{sequence}", std::process::id()),
            method: Method::OrbitActivity(EmptyParams {}),
        };
        let response = client
            .request_value_with_timeout(&request, Duration::from_secs(10))
            .map_err(io::Error::other)?;
        if let Some(error) = response.get("error") {
            eprintln!("Orbit activity: {}", cell(error.get("message"), 240));
            return Ok(1);
        }
        let result = response
            .get("result")
            .ok_or_else(|| io::Error::other("Orbit activity response has no result"))?;
        let mut output = io::stdout().lock();
        if watch {
            output.write_all(b"\x1b[2J\x1b[H")?;
        }
        let text = render_styled(result, watch, now_ms(), glyphs);
        let text = if glyphs.is_some() && io::stdout().is_terminal() {
            crossterm::terminal::size()
                .map(|(cols, _)| super::orbit_activity_modern::fit_terminal(&text, cols))
                .unwrap_or(text)
        } else {
            text
        };
        output.write_all(text.as_bytes())?;
        output.flush()?;
        drop(output);
        if !watch {
            return Ok(0);
        }
        std::thread::sleep(Duration::from_secs(1));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn orbit_activity_groups_members_under_their_role() {
        let text = render(
            &json!({"jobs":[],"agents":[],"team":[{"label":"Reviewer","tab_id":"w1:t2","area":"Reviews","tab_missing":false,
                "members":[{"name":"rc","kind":"claude","state":"running","agent_status":"idle","pane_id":"w1:p3","open_tasks":1},
                           {"name":"rx","kind":"codex","state":"relaunching","agent_status":null,"pane_id":null,"open_tasks":0}]}]}),
            false,
            0,
        );
        let team = text.split("Team roles").nth(1).unwrap();
        assert!(team.contains("Reviewer  tab=w1:t2  area=Reviews"), "{text}");
        assert!(team.contains("rc") && team.contains("running") && team.contains("open_tasks=1"));
        assert!(team.contains("rx") && team.contains("relaunching"));
        assert!(!render(&json!({"jobs":[],"agents":[]}), false, 0).contains("Team roles"));
    }

    #[test]
    fn orbit_activity_renders_empty_state_and_actions() {
        let text = render(&json!({"jobs":[],"agents":[]}), false, 0);
        assert!(text.contains("No jobs yet."));
        assert!(text.contains("No workers yet."));
        assert!(text.contains("job-status"));
        assert!(text.contains("job-cancel"));
        assert!(text.contains("task-status"));
        assert!(text.contains("task-cancel"));
        assert!(text.contains("No assignments yet."));
        assert!(!text.contains("Refreshing"));
    }

    #[test]
    fn orbit_activity_lists_exited_worker_panes_only_when_present() {
        let text = render(
            &json!({"stale_panes":[{"pane_id":"w1:p3","label":"reviewer (exited)","name":"reviewer"}]}),
            false,
            0,
        );
        assert!(text.contains("Exited worker panes"), "{text}");
        assert!(text.contains("reviewer (exited)  pane=w1:p3"), "{text}");
        assert!(text.contains("pane-close"), "{text}");
        assert!(!render(&json!({"stale_panes":[]}), false, 0).contains("Exited worker panes"));
    }

    #[test]
    fn orbit_activity_shows_results_and_workers() {
        let text = render(
            &json!({
                "jobs":[{"job_id":"job-1","status":"failed","exit_code":2,"command":"cargo test"}],
                "agents":[{"name":"reviewer","agent_status":"blocked","pane_id":"w1:p3","agent":"claude"}]
            }),
            true,
            0,
        );
        for expected in [
            "job-1",
            "failed",
            "2",
            "cargo test",
            "reviewer",
            "blocked",
            "w1:p3",
            "claude",
            "Ctrl+C",
        ] {
            assert!(text.contains(expected), "missing {expected}");
        }
    }

    #[test]
    fn orbit_activity_shows_task_attempts_age_origin_and_ledger_error() {
        let text = render(
            &json!({
                "jobs":[],
                "agents":[],
                "ledger_error":"Invalid Orbit task ledger",
                "tasks":[{
                    "task_id":"task-1/server-notice","worker_id":"w7","submission":"submitted",
                    "delivery":"queued","attempts":2,"created_at":1_000,"origin":"server",
                    "report":{"outcome":"failed","summary":"Worker w7 was lost before reporting"},
                    "error":null
                },{
                    "task_id":"worker-w8-5000/server-notice","worker_id":"w8","submission":"submitted",
                    "delivery":"queued","attempts":0,"created_at":1_000,"origin":"server",
                    "notice_for":null,
                    "report":{"task_id":"worker-w8-5000/server-notice","outcome":"blocked",
                              "summary":"Worker w8 (w1:p4) is blocked on a dialog and has no assignment"},
                    "error":null
                }],
                "blocked_workers":[{"name":"w8","pane_id":"w1:p4","task_id":null,"since_ms":121_000}]
            }),
            false,
            181_000,
        );
        for expected in [
            "Ledger problem: Invalid Orbit task ledger",
            "attempts=2",
            "age=3m",
            "origin=server",
            "Polaris=queued",
            "was lost before reporting",
            "worker-w8-5000/server-notice",
            "has no assignment",
            "blocked_for=1m",
        ] {
            assert!(text.contains(expected), "missing {expected}: {text}");
        }
        assert_eq!(age(Some(&json!(0)), 5), "—");
        assert_eq!(age(Some(&json!(1_000)), 8_200_000), "2h");
    }

    #[test]
    fn orbit_activity_shows_tests_and_checks_on_the_report_line() {
        let task = |report: Value| json!({"tasks":[{"task_id":"task-1","worker_id":"w1","delivery":"submitted","report":report}]});
        let text = render(
            &task(
                json!({"outcome":"completed","summary":"Parser fixed","tests_run":false,
                "checks":[{"name":"a","status":"passed"},{"name":"b","status":"passed"},
                          {"name":"c","status":"passed"},{"name":"d","status":"failed"}]}),
            ),
            false,
            0,
        );
        assert!(
            text.contains("completed: Parser fixed  tests=no  checks=3 passed, 1 failed"),
            "{text}"
        );
        let text = render(
            &task(json!({"outcome":"completed","summary":"Done","tests_run":true})),
            false,
            0,
        );
        assert!(text.contains("Done  tests=yes\n"), "{text}");
        let text = render(
            &task(json!({"outcome":"failed","summary":"Lost"})),
            false,
            0,
        );
        assert!(text.contains("Lost  tests=—\n"), "{text}");
    }

    #[test]
    fn orbit_activity_shows_lead_status_and_startup_error() {
        let text = render(
            &json!({
                "jobs":[],"agents":[],
                "lead":{"terminal_id":"term-7","present":true,"state":"idle","visible_idle":false,"visible_working":true,"queued_reports":3},
                "lead_error":"Polaris could not start: busy"
            }),
            false,
            0,
        );
        for expected in [
            "Polaris lead",
            "terminal=term-7",
            "present=yes",
            "state=idle",
            "visible_idle=no",
            "visible_working=yes",
            "queued_reports=3",
            "Lead problem: Polaris could not start: busy",
        ] {
            assert!(text.contains(expected), "missing {expected}: {text}");
        }
        assert!(render(&json!({}), false, 0).contains("No lead status."));
    }

    #[test]
    fn orbit_activity_shows_worker_kinds() {
        let text = render(
            &json!({"kinds":[
                {"kind":"claude","executable":"claude","installed":true,"models":["fable","opus"],"default_model":"fable"},
                {"kind":"codex","executable":"codex","installed":false,"models":[]}
            ]}),
            false,
            0,
        );
        for expected in [
            "Worker kinds",
            "claude",
            "yes",
            "fable, opus",
            "codex",
            "no",
            "any",
        ] {
            assert!(text.contains(expected), "missing {expected}: {text}");
        }
        assert!(render(&json!({}), false, 0).contains("No worker kinds allowed."));
    }

    #[test]
    fn orbit_activity_shows_blocked_workers() {
        let text = render(
            &json!({"blocked_workers":[
                {"name":"reviewer","pane_id":"w1:p3","task_id":"task-7","since_ms":1_000},
                {"name":"helper","pane_id":"w1:p4","task_id":null,"since_ms":121_000}
            ]}),
            false,
            181_000,
        );
        for expected in [
            "Blocked workers",
            "reviewer  pane=w1:p3  task=task-7  blocked_for=3m",
            "helper  pane=w1:p4  task=—  blocked_for=1m",
            "lead_may_approve",
        ] {
            assert!(text.contains(expected), "missing {expected}: {text}");
        }
        assert!(render(&json!({}), false, 0).contains("No blocked workers."));
    }

    #[test]
    fn orbit_activity_sanitizes_and_bounds_terminal_metadata() {
        let value = json!("bad\u{1b}[2J\ncommandxxxxxxxxxxxxxxxxxxxxxxxx");
        let text = cell(Some(&value), 15);
        assert!(text.chars().all(|c| !c.is_control()));
        assert_eq!(text.chars().count(), 15);
        assert!(text.ends_with('…'));
    }

    #[test]
    fn activity_hides_labelled_secrets_in_report_summaries() {
        let text = render(
            &json!({"jobs":[],"agents":[],"tasks":[{
                "task_id":"task-1","worker_id":"w","submission":"submitted",
                "delivery":"submitted","attempts":1,"created_at":0,"origin":"worker",
                "report":{"outcome":"completed","summary":"output was password: hunter2"}
            }]}),
            false,
            0,
        );
        assert!(!text.contains("hunter2"), "{text}");
        assert!(text.contains("password: <redacted>"), "{text}");
    }
}

#[cfg(test)]
mod phase5_goldens {
    use super::*;
    use crate::ui::chrome::glyphs::{ASCII, UNICODE};
    use serde_json::json;

    fn payload(dense: bool) -> Value {
        let mut tasks = Vec::new();
        let mut blocked = Vec::new();
        for lane in ["working", "queued", "done", "blocked"] {
            for index in 0..if dense { 25 } else { 1 } {
                let id = format!("{lane}-{index}");
                if lane == "blocked" {
                    blocked.push(
                        json!({"name":id,"pane_id":format!("p-{index}"),"task_id":id,"since_ms":1}),
                    );
                    continue;
                }
                let mut task = json!({"task_id":id,"worker_id":"worker","created_at":1,"submission":"submitted","delivery":"awaiting_report"});
                if lane == "queued" {
                    task["submission"] = json!("pending");
                }
                if lane == "done" {
                    task["report"] = json!({"outcome":"completed","summary":"checked the result","tests_run":true,"checks":[{"name":"unit","status":"passed"}]});
                }
                tasks.push(task);
            }
        }
        json!({"jobs":[],"agents":[{"name":"worker","agent":"codex","agent_status":"working","pane_id":"p-worker"}],"tasks":tasks,"blocked_workers":blocked})
    }

    #[test]
    fn classic_modern_and_ascii_activity_goldens() {
        let mut actual = String::new();
        for (scene, payload) in [
            ("empty", json!({})),
            ("lanes", payload(false)),
            ("overflow", payload(true)),
        ] {
            for watch in [false, true] {
                for (style, glyphs) in [
                    ("classic", None),
                    ("modern", Some(&UNICODE)),
                    ("ascii", Some(&ASCII)),
                ] {
                    let text = render_styled(&payload, watch, 172_800_001, glyphs);
                    assert!(!text.contains('\u{1b}'));
                    if scene == "overflow" && style != "classic" {
                        assert_eq!(text.matches("5 more").count(), 4);
                    }
                    actual.push_str(&format!("=== {scene}/{watch}/{style}\n{text}\n"));
                }
            }
        }
        let _ = actual.pop(); // Keep one final newline in the golden envelope.
        let path = concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/src/client/shell/tests/fixtures/orbit-activity-phase5.txt"
        );
        if std::env::var_os("ASTER_CAPTURE_PHASE5").is_some() {
            std::fs::write(path, actual).unwrap();
        } else {
            assert_eq!(actual, std::fs::read_to_string(path).unwrap());
        }
    }
}
