//! The modern `aster orbit activity` view: the same lanes, glyphs and age units
//! as the TUI, derived from the activity payload the classic view prints. Plain
//! text only (no color, as everywhere in this command), so pipes and files stay
//! clean; `ui.glyphs = "ascii"` swaps every glyph for its ASCII stand-in. The
//! payload is never changed and machine-readable output is untouched.

use std::fmt::Write as _;

use serde_json::Value;

use super::orbit_activity::{
    cell, checks_cell, redacted_cell, render_footer, render_jobs, render_kinds, render_lead,
    render_stale, render_team, MAX_ROWS,
};
use crate::ui::chrome::age::{age_text, AGE_BUFFER_LEN};
use crate::ui::chrome::state::StateKind;
use crate::ui::chrome::GlyphSet;

/// One line of a lane: a task, or a worker waiting on a decision.
struct Row {
    state: StateKind,
    id: String,
    who: String,
    age: String,
    text: String,
    /// The report line under a finished task.
    detail: Option<String>,
}

#[derive(Default)]
struct Lanes {
    needs_you: Vec<Row>,
    working: Vec<Row>,
    queued: Vec<Row>,
    done: Vec<Row>,
}

/// Elapsed time since the unix-millisecond `since`, in the TUI's units.
fn age_since(since: Option<&Value>, now_ms: u64) -> String {
    let Some(since) = since.and_then(Value::as_u64).filter(|ms| *ms > 0) else {
        return "—".into();
    };
    let mut buffer = [0u8; AGE_BUFFER_LEN];
    age_text(now_ms.saturating_sub(since), &mut buffer).to_owned()
}

fn str_field<'a>(value: &'a Value, key: &str) -> Option<&'a str> {
    value.get(key).and_then(Value::as_str)
}

fn report_detail(report: &Value) -> String {
    format!(
        "{}: {}  tests={}{}",
        cell(report.get("outcome"), 12),
        redacted_cell(report.get("summary"), 120),
        super::orbit_activity::yes_no(report.get("tests_run")),
        checks_cell(report),
    )
}

fn task_row(task: &Value, now_ms: u64) -> (Lane, Row) {
    let report = task.get("report").filter(|report| !report.is_null());
    let outcome = report.and_then(|report| str_field(report, "outcome"));
    let queued = matches!(str_field(task, "submission"), Some("pending" | "uncertain"));
    // Without a report the delivery state says how the task ended: every
    // terminal state leaves the WORKING lane.
    let delivery = str_field(task, "delivery");
    let (lane, state) = match (report, outcome) {
        (Some(_), Some("blocked")) => (Lane::NeedsYou, StateKind::Blocked),
        (Some(_), Some("failed")) => (Lane::Done, StateKind::Failed),
        (Some(_), _) => (Lane::Done, StateKind::Done),
        (None, _)
            if matches!(
                delivery,
                Some("failed" | "worker_lost" | "unreported" | "timed_out")
            ) =>
        {
            (Lane::Done, StateKind::Failed)
        }
        (None, _) if delivery == Some("cancelled") => (Lane::Done, StateKind::Exited),
        (None, _) if delivery == Some("submitted") => (Lane::Done, StateKind::Done),
        (None, _) if queued => (Lane::Queued, StateKind::Queued),
        (None, _) => (Lane::Working, StateKind::Working),
    };
    let text = match report {
        Some(report) => redacted_cell(report.get("summary"), 80),
        None => cell(task.get("delivery"), 24).replace('_', " "),
    };
    let row = Row {
        state,
        id: cell(task.get("task_id"), 28),
        who: cell(task.get("worker_id"), 18),
        age: age_since(task.get("created_at"), now_ms),
        text,
        detail: report.filter(|_| lane == Lane::Done).map(report_detail),
    };
    (lane, row)
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Lane {
    NeedsYou,
    Working,
    Queued,
    Done,
}

fn lanes(result: &Value, now_ms: u64) -> Lanes {
    let mut lanes = Lanes::default();
    let blocked: Vec<&Value> = result
        .get("blocked_workers")
        .and_then(Value::as_array)
        .map(|workers| workers.iter().collect())
        .unwrap_or_default();
    let blocked_task = |task: &Value| {
        let id = str_field(task, "task_id");
        id.is_some()
            && blocked
                .iter()
                .any(|worker| str_field(worker, "task_id") == id)
    };
    let blocked_worker = |task: &Value| {
        let worker = str_field(task, "worker_id");
        worker.is_some()
            && blocked.iter().any(|blocked| {
                let pane = str_field(blocked, "pane_id");
                result
                    .get("agents")
                    .and_then(Value::as_array)
                    .is_some_and(|agents| {
                        agents.iter().any(|agent| {
                            str_field(agent, "terminal_id") == worker
                                && pane.is_some()
                                && str_field(agent, "pane_id") == pane
                        })
                    })
                    || str_field(blocked, "name") == worker
            })
    };
    for worker in &blocked {
        lanes.needs_you.push(Row {
            state: StateKind::Blocked,
            id: cell(worker.get("pane_id"), 28),
            who: cell(worker.get("name"), 18),
            age: age_since(worker.get("since_ms"), now_ms),
            text: match worker.get("task_id").filter(|id| !id.is_null()) {
                Some(id) => format!("waiting for a decision on {}", cell(Some(id), 40)),
                None => "waiting for a decision".to_owned(),
            },
            detail: None,
        });
    }
    let tasks = result
        .get("tasks")
        .and_then(Value::as_array)
        .map(Vec::as_slice)
        .unwrap_or_default();
    for task in tasks.iter().rev() {
        let (lane, row) = task_row(task, now_ms);
        // A blocked worker already has its row; not a second one for its task
        // or the notice the server filed about it.
        if (lane == Lane::Working && blocked_task(task))
            || (lane == Lane::NeedsYou && blocked_worker(task))
        {
            continue;
        }
        match lane {
            Lane::NeedsYou => lanes.needs_you.push(row),
            Lane::Working => lanes.working.push(row),
            Lane::Queued => lanes.queued.push(row),
            Lane::Done => lanes.done.push(row),
        }
    }
    lanes
}

fn header(out: &mut String, result: &Value, lanes: &Lanes, glyphs: &GlyphSet) {
    let workers = result
        .get("agents")
        .and_then(Value::as_array)
        .map_or(0, Vec::len);
    let plural = if workers == 1 { "" } else { "s" };
    let _ = write!(
        out,
        "Orbit  {sep}  {workers} worker{plural}",
        sep = glyphs.separator
    );
    let parts = [
        (StateKind::Blocked, lanes.needs_you.len(), "needs you"),
        (StateKind::Working, lanes.working.len(), "working"),
        (StateKind::Queued, lanes.queued.len(), "queued"),
    ];
    let mut first = true;
    for (state, count, label) in parts {
        if count == 0 {
            continue;
        }
        let lead = if first {
            format!("  {}  ", glyphs.separator)
        } else {
            "   ".to_owned()
        };
        first = false;
        let _ = write!(out, "{lead}{} {count} {label}", glyphs.state(state));
    }
    out.push('\n');
}

fn lane(out: &mut String, title: &str, state: StateKind, rows: &[Row], glyphs: &GlyphSet) {
    if rows.is_empty() {
        return;
    }
    let _ = writeln!(out, "{} {title}", glyphs.state(state));
    for row in rows.iter().take(MAX_ROWS) {
        let _ = writeln!(
            out,
            "  {} {:<28} {:<18} {:>4}  {}",
            glyphs.state(row.state),
            row.id,
            row.who,
            row.age,
            row.text,
        );
        if let Some(detail) = &row.detail {
            let _ = writeln!(out, "      {detail}");
        }
    }
    if rows.len() > MAX_ROWS {
        let _ = writeln!(out, "  {} {} more", glyphs.ellipsis, rows.len() - MAX_ROWS);
    }
}

fn workers(out: &mut String, result: &Value, glyphs: &GlyphSet) {
    out.push_str("\nWorkers\n");
    let agents = result.get("agents").and_then(Value::as_array);
    let Some(agents) = agents.filter(|agents| !agents.is_empty()) else {
        out.push_str("No workers yet.\n");
        return;
    };
    for agent in agents.iter().take(MAX_ROWS) {
        let state = StateKind::from_member_state(str_field(agent, "agent_status").unwrap_or(""));
        let _ = writeln!(
            out,
            "  {} {:<22} {:<10} {:<14} {}",
            glyphs.state(state),
            cell(agent.get("name"), 22),
            cell(agent.get("agent_status"), 10),
            cell(agent.get("pane_id"), 14),
            cell(agent.get("agent"), 18),
        );
    }
    if agents.len() > MAX_ROWS {
        let _ = writeln!(out, "Showing {MAX_ROWS} of {} workers.", agents.len());
    }
}

pub(super) fn render(result: &Value, watch: bool, now_ms: u64, glyphs: &GlyphSet) -> String {
    let lanes = lanes(result, now_ms);
    let mut out = String::new();
    header(&mut out, result, &lanes, glyphs);
    if let Some(error) = result.get("ledger_error").filter(|error| !error.is_null()) {
        let _ = writeln!(out, "Ledger problem: {}", cell(Some(error), 200));
    }
    out.push('\n');
    let any = !(lanes.needs_you.is_empty()
        && lanes.working.is_empty()
        && lanes.queued.is_empty()
        && lanes.done.is_empty());
    if any {
        lane(
            &mut out,
            "NEEDS YOU",
            StateKind::Blocked,
            &lanes.needs_you,
            glyphs,
        );
        if !lanes.needs_you.is_empty() {
            out.push_str(
                "  Answer in the worker's pane; Polaris answers only with [polaris.approvals] lead_may_approve.\n",
            );
        }
        lane(
            &mut out,
            "WORKING",
            StateKind::Working,
            &lanes.working,
            glyphs,
        );
        lane(&mut out, "QUEUED", StateKind::Queued, &lanes.queued, glyphs);
        lane(&mut out, "DONE", StateKind::Done, &lanes.done, glyphs);
    } else {
        out.push_str("No assignments yet.\n");
    }
    render_jobs(&mut out, result);
    workers(&mut out, result, glyphs);
    render_team(&mut out, result);
    render_stale(&mut out, result);
    render_kinds(&mut out, result);
    render_lead(&mut out, result);
    render_footer(&mut out, watch);
    ascii_fold(out, glyphs)
}

/// The shared classic sections print an em dash for a missing value and an
/// ellipsis for a cut one; under `ui.glyphs = "ascii"` they become `-` and the
/// glyph set's ellipsis, so generated punctuation is ASCII. The same punctuation in payload text
/// is normalized too; other Unicode payload characters are retained.
fn ascii_fold(out: String, glyphs: &GlyphSet) -> String {
    if glyphs.ellipsis.is_ascii() {
        out.replace('—', glyphs.unknown)
            .replace('…', glyphs.ellipsis)
    } else {
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ui::chrome::glyphs::{ASCII, UNICODE};
    use serde_json::json;

    fn payload() -> Value {
        json!({
            "jobs": [],
            "agents": [
                {"name":"cy","agent_status":"blocked","pane_id":"w1:p3","agent":"claude"},
                {"name":"bo","agent_status":"working","pane_id":"w1:p4","agent":"codex"}
            ],
            "blocked_workers": [{"name":"cy","pane_id":"w1:p3","task_id":"t-1","since_ms":60_000}],
            "tasks": [
                {"task_id":"t-1","worker_id":"cy","submission":"submitted","delivery":"awaiting_report","created_at":50_000},
                {"task_id":"t-2","worker_id":"bo","submission":"submitted","delivery":"awaiting_report","created_at":100_000},
                {"task_id":"t-3","worker_id":"rae","submission":"pending","delivery":"queued","created_at":170_000},
                {"task_id":"t-4","worker_id":"ada","submission":"submitted","delivery":"submitted","created_at":0,
                 "report":{"outcome":"completed","summary":"bump deps","tests_run":true,
                           "checks":[{"name":"a","status":"passed"}]}}
            ]
        })
    }

    #[test]
    fn tasks_land_in_the_lane_of_their_state() {
        let text = render(&payload(), false, 181_000, &UNICODE);
        let lines: Vec<&str> = text.lines().collect();
        assert_eq!(
            lines[0],
            "Orbit  ·  2 workers  ·  ▲ 1 needs you   ◐ 1 working   ◌ 1 queued"
        );
        for expected in ["▲ NEEDS YOU", "◐ WORKING", "◌ QUEUED", "✓ DONE"] {
            assert!(text.contains(expected), "missing {expected}: {text}");
        }
        // The blocked worker is the one NEEDS YOU row; its task is not repeated
        // under WORKING.
        assert_eq!(text.matches("t-1").count(), 1, "{text}");
        assert!(text.contains("waiting for a decision on t-1"), "{text}");
        assert!(text.contains("completed: bump deps  tests=yes  checks=1 passed"));
        assert!(text.contains("lead_may_approve"));
    }

    #[test]
    fn a_blocked_notice_is_not_duplicated_when_worker_id_is_a_terminal_id() {
        let payload = json!({
            "agents":[{"terminal_id":"term_worker", "pane_id":"p1", "name":"human-name"}],
            "blocked_workers":[{"pane_id":"p1", "name":"human-name", "task_id":"t1"}],
            "tasks":[
                {"task_id":"t1", "worker_id":"term_worker", "submission":"submitted"},
                {"task_id":"t1/blocked-1", "worker_id":"term_worker", "report":{"outcome":"blocked", "summary":"waiting"}}
            ]
        });
        let lanes = lanes(&payload, 1000);
        assert_eq!(lanes.needs_you.len(), 1);
        assert!(lanes.working.is_empty());
    }

    #[test]
    fn ascii_glyphs_replace_every_unicode_state_glyph() {
        let text = render(&payload(), false, 181_000, &ASCII);
        let line = text.lines().next().unwrap();
        assert_eq!(
            line,
            "Orbit  .  2 workers  .  ! 1 needs you   * 1 working   . 1 queued"
        );
        assert!(text.contains("! NEEDS YOU") && text.contains("+ DONE"));
        assert!(!text.contains('▲') && !text.contains('◐') && !text.contains('✓'));
    }

    #[test]
    fn age_boundaries_and_missing_or_future_timestamps_match_the_tui() {
        for (elapsed, expected) in [
            (0, "0s"),
            (59_999, "59s"),
            (60_000, "1m"),
            (3_600_000, "1h"),
            (86_400_000, "1d"),
        ] {
            assert_eq!(age_since(Some(&json!(1)), elapsed + 1), expected);
        }
        assert_eq!(age_since(Some(&json!(10)), 1), "0s");
        assert_eq!(age_since(None, 1), "—");
    }

    #[test]
    fn ages_use_the_tui_units() {
        let text = render(&payload(), false, 181_000 + 3 * 3_600_000, &UNICODE);
        assert!(text.contains(" 3h"), "{text}");
        let two_days = render(&payload(), false, 181_000 + 2 * 86_400_000, &UNICODE);
        assert!(two_days.contains("2d"), "{two_days}");
    }

    #[test]
    fn an_empty_payload_says_so_and_keeps_the_command_hints() {
        let text = render(&json!({}), false, 0, &UNICODE);
        assert!(text.starts_with("Orbit  ·  0 workers\n"), "{text}");
        assert!(text.contains("No assignments yet."));
        assert!(text.contains("task-cancel"));
        assert!(!text.contains('\u{1b}'), "no terminal controls");
    }

    #[test]
    fn long_lanes_are_cut_with_a_count() {
        let tasks: Vec<Value> = (0..25)
            .map(|n| json!({"task_id":format!("t-{n}"),"worker_id":"w","submission":"submitted","delivery":"x","created_at":1}))
            .collect();
        let text = render(&json!({"tasks":tasks}), false, 1_000, &UNICODE);
        assert!(text.contains("… 5 more"), "{text}");
    }

    #[test]
    fn report_text_is_redacted_and_controls_are_stripped() {
        let text = render(
            &json!({"tasks":[{"task_id":"t","worker_id":"w","submission":"submitted","delivery":"submitted",
                "report":{"outcome":"completed","summary":"password: hunter2\u{1b}[2J"}}]}),
            false,
            0,
            &UNICODE,
        );
        assert!(
            !text.contains("hunter2") && !text.contains('\u{1b}'),
            "{text}"
        );
    }

    fn lane_of(text: &str, id: &str) -> String {
        let mut lane = String::new();
        for line in text.lines() {
            if !line.starts_with(' ') && !line.is_empty() {
                lane = line.to_owned();
            }
            if line.contains(id) {
                return lane;
            }
        }
        panic!("{id} not listed:\n{text}");
    }

    #[test]
    fn every_delivery_state_without_a_report_lands_in_the_right_lane() {
        let cases = [
            ("awaiting_report", "submitted", "WORKING"),
            ("awaiting_report", "pending", "QUEUED"),
            ("queued", "submitted", "WORKING"),
            ("submitting", "submitted", "WORKING"),
            ("uncertain", "uncertain", "QUEUED"),
            ("submitted", "submitted", "DONE"),
            ("failed", "submitted", "DONE"),
            ("cancelled", "submitted", "DONE"),
            ("worker_lost", "submitted", "DONE"),
            ("unreported", "submitted", "DONE"),
            ("timed_out", "submitted", "DONE"),
        ];
        for (delivery, submission, lane) in cases {
            let text = render(
                &json!({"tasks":[{"task_id":"t-x","worker_id":"w","submission":submission,
                    "delivery":delivery,"created_at":1}]}),
                false,
                1_000,
                &UNICODE,
            );
            assert!(
                lane_of(&text, "t-x").ends_with(lane),
                "{delivery}/{submission} expected {lane}:\n{text}"
            );
        }
        // Failed endings show the failed glyph; a cancel is neutral, not an error.
        let text = |delivery: &str| {
            render(
                &json!({"tasks":[{"task_id":"t-x","worker_id":"w","submission":"submitted",
                    "delivery":delivery,"created_at":1}]}),
                false,
                1_000,
                &UNICODE,
            )
        };
        for delivery in ["failed", "worker_lost", "unreported", "timed_out"] {
            assert!(text(delivery).contains("  ✗ t-x"), "{delivery}");
        }
        assert!(text("cancelled").contains("  · t-x"));
        assert!(text("timed_out").contains("timed out"));
    }

    #[test]
    fn ascii_glyph_output_is_ascii_even_for_missing_and_cut_values() {
        let mut payload = payload();
        payload["tasks"].as_array_mut().unwrap().extend([
            json!({"task_id":"no-time","worker_id":"w","submission":"submitted","delivery":"awaiting_report"}),
            json!({"task_id":"x".repeat(60),"worker_id":"w","submission":"submitted","delivery":"cancelled","created_at":5}),
        ]);
        payload["lead"] = json!({"terminal_id":"t","present":true});
        payload["jobs"] = json!([{"job_id":"j","status":"running","command":"c".repeat(80)}]);
        let text = render(&payload, true, 181_000, &ASCII);
        assert!(text.is_ascii(), "{text}");
        let text = render(&json!({"tasks":[],"lead":null}), false, 0, &ASCII);
        assert!(text.is_ascii(), "{text}");
    }
}
