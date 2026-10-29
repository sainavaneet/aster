//! Optional structured report fields ("facts"): validation at admission and the
//! bounded rendering Polaris receives after the fenced summary.
use std::fmt::Write as _;

use serde_json::{json, Map, Value};

use super::report::DELIVERY_MARKER_PREFIX;
use crate::api::schema::{OrbitAgentReportParams, OrbitHandoff};

const MAX_CHECKS: usize = 20;
const MAX_CHECK_NAME_BYTES: usize = 120;
const MAX_CHECK_DETAIL_BYTES: usize = 500;
const MAX_CHANGED_FILES: usize = 200;
const MAX_PATH_BYTES: usize = 512;
const MAX_BRANCH_BYTES: usize = 200;
const MAX_JOB_IDS: usize = 20;
const MAX_JOB_ID_BYTES: usize = 128;
const MAX_BLOCKED_ON_BYTES: usize = 500;
const MAX_HANDOFF_DONE_CHARS: usize = 1000;
const MAX_HANDOFF_NEXT_CHARS: usize = 1000;
const MAX_HANDOFF_ISSUES_CHARS: usize = 500;
/// Changed paths listed in a delivery; the count always covers all of them.
const MAX_RENDERED_FILES: usize = 20;

/// One line of 1..=`max` bytes with no control characters (newlines and tabs
/// included, so a value cannot add lines to the rendered facts) and no
/// delivery marker.
pub(super) fn check_text(field: &str, text: &str, max: usize) -> Result<(), String> {
    if text.trim().is_empty()
        || text.len() > max
        || text.chars().any(char::is_control)
        || text.contains(DELIVERY_MARKER_PREFIX)
    {
        return Err(format!(
            "{field} must be 1–{max} bytes on one line without control characters or delivery markers"
        ));
    }
    Ok(())
}

/// A handoff note: 1..=`max` characters; newlines and tabs are allowed since
/// the next member reads the note as prose, other control characters and
/// delivery markers are not.
pub(super) fn check_note(field: &str, text: &str, max: usize) -> Result<(), String> {
    if text.trim().is_empty()
        || text.chars().count() > max
        || text
            .chars()
            .any(|ch| ch.is_control() && ch != '\n' && ch != '\t')
        || text.contains(DELIVERY_MARKER_PREFIX)
    {
        return Err(format!(
            "{field} must be 1–{max} characters without terminal control characters or delivery markers"
        ));
    }
    Ok(())
}

fn validate_handoff(handoff: &OrbitHandoff) -> Result<(), String> {
    check_note("handoff.done", &handoff.done, MAX_HANDOFF_DONE_CHARS)?;
    check_note("handoff.next", &handoff.next, MAX_HANDOFF_NEXT_CHARS)?;
    check_count(
        "handoff.changed_files",
        &handoff.changed_files,
        MAX_CHANGED_FILES,
    )?;
    for (index, path) in handoff.changed_files.iter().enumerate() {
        check_path(&format!("handoff.changed_files[{index}]"), path)?;
    }
    if let Some(issues) = &handoff.known_issues {
        check_note("handoff.known_issues", issues, MAX_HANDOFF_ISSUES_CHARS)?;
    }
    if let Some(parallel) = &handoff.parallel {
        super::parallel::check_requests(parallel)?;
    }
    if let Some(to) = &handoff.to {
        if !crate::app::agents::valid_agent_name(to) {
            return Err("handoff.to must be the name of a member of your role".into());
        }
    }
    Ok(())
}

fn check_count<T>(field: &str, items: &[T], max: usize) -> Result<(), String> {
    if items.len() > max {
        return Err(format!("{field} must have at most {max} entries"));
    }
    Ok(())
}

/// Relative to the worker's project: no leading separator, drive prefix, or `..`.
pub(super) fn check_path(field: &str, path: &str) -> Result<(), String> {
    check_text(field, path, MAX_PATH_BYTES)?;
    let bytes = path.as_bytes();
    let absolute = path.starts_with(['/', '\\']) || (bytes.len() >= 2 && bytes[1] == b':');
    if absolute || path.split(['/', '\\']).any(|part| part == "..") {
        return Err(format!(
            "{field} must be a path relative to the project, without `..`"
        ));
    }
    Ok(())
}

/// A conservative subset of `git check-ref-format` for branch names.
fn valid_branch(branch: &str) -> bool {
    !branch.is_empty()
        && branch.len() <= MAX_BRANCH_BYTES
        && branch
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || b"._-/+".contains(&byte))
        && !branch.starts_with(['-', '/'])
        && !branch.ends_with(['/', '.'])
        && !branch.ends_with(".lock")
        && !branch.contains("..")
        && !branch.contains("//")
        && !branch.split('/').any(|part| part.starts_with('.'))
}

/// Validates every present structured field; the error names the first bad one.
pub(super) fn validate(report: &OrbitAgentReportParams) -> Result<(), String> {
    if let Some(checks) = &report.checks {
        check_count("checks", checks, MAX_CHECKS)?;
        for (index, check) in checks.iter().enumerate() {
            check_text(
                &format!("checks[{index}].name"),
                &check.name,
                MAX_CHECK_NAME_BYTES,
            )?;
            if let Some(detail) = &check.detail {
                let field = format!("checks[{index}].detail");
                check_text(&field, detail, MAX_CHECK_DETAIL_BYTES)?;
            }
        }
    }
    if let Some(files) = &report.changed_files {
        check_count("changed_files", files, MAX_CHANGED_FILES)?;
        for (index, path) in files.iter().enumerate() {
            check_path(&format!("changed_files[{index}]"), path)?;
        }
    }
    if let Some(branch) = &report.branch {
        if !valid_branch(branch) {
            return Err(format!(
                "branch must be a Git branch name of 1–{MAX_BRANCH_BYTES} bytes using letters, digits, and . _ - / +"
            ));
        }
    }
    if let Some(job_ids) = &report.job_ids {
        check_count("job_ids", job_ids, MAX_JOB_IDS)?;
        for (index, job_id) in job_ids.iter().enumerate() {
            check_text(&format!("job_ids[{index}]"), job_id, MAX_JOB_ID_BYTES)?;
        }
    }
    if let Some(blocked_on) = &report.blocked_on {
        check_text("blocked_on", blocked_on, MAX_BLOCKED_ON_BYTES)?;
    }
    if let Some(handoff) = &report.handoff {
        validate_handoff(handoff)?;
    }
    Ok(())
}

/// How many entries of each list a rendering shows.
#[derive(Debug, Clone, Copy)]
pub(super) struct Shown {
    checks: usize,
    files: usize,
    job_ids: usize,
}

fn len<T>(items: &Option<Vec<T>>) -> usize {
    items.as_ref().map_or(0, Vec::len)
}

impl Shown {
    pub(super) fn all(report: &OrbitAgentReportParams) -> Self {
        Self {
            checks: len(&report.checks),
            files: len(&report.changed_files).min(MAX_RENDERED_FILES),
            job_ids: len(&report.job_ids),
        }
    }

    /// Drops one more entry (paths first, then job IDs, then checks); `false`
    /// once every list is empty.
    pub(super) fn shrink(&mut self) -> bool {
        for count in [&mut self.files, &mut self.job_ids, &mut self.checks] {
            if *count > 0 {
                *count -= 1;
                return true;
            }
        }
        false
    }
}

fn first<T: Clone>(items: &Option<Vec<T>>, shown: usize) -> Option<Vec<T>> {
    items
        .as_ref()
        .map(|items| items.iter().take(shown).cloned().collect())
}

/// Adds the structured fields, lists cut to `shown`, to the delivery header.
pub(super) fn add_to_header(
    header: &mut Map<String, Value>,
    report: &OrbitAgentReportParams,
    shown: Shown,
) {
    let mut put = |key: &str, value: Value| {
        if !value.is_null() {
            header.insert(key.into(), value);
        }
    };
    put("tests_run", json!(report.tests_run));
    put("checks", json!(first(&report.checks, shown.checks)));
    put(
        "changed_files",
        json!(first(&report.changed_files, shown.files)),
    );
    if let Some(files) = &report.changed_files {
        put("changed_files_count", json!(files.len()));
    }
    put("branch", json!(report.branch));
    put("job_ids", json!(first(&report.job_ids, shown.job_ids)));
    put("blocked_on", json!(report.blocked_on));
    let omitted: Map<String, Value> = [
        ("checks", len(&report.checks).saturating_sub(shown.checks)),
        (
            "job_ids",
            len(&report.job_ids).saturating_sub(shown.job_ids),
        ),
    ]
    .into_iter()
    .filter(|(_, count)| *count > 0)
    .map(|(key, count)| (key.to_owned(), json!(count)))
    .collect();
    if !omitted.is_empty() {
        put("omitted", Value::Object(omitted));
    }
}

/// `3 passed, 1 failed`; statuses with no checks are left out.
pub(crate) fn checks_summary<'a>(statuses: impl IntoIterator<Item = &'a str>) -> String {
    let mut counts = [0usize; 3];
    for status in statuses {
        if let Some(slot) = ["passed", "failed", "skipped"]
            .iter()
            .position(|name| *name == status)
        {
            counts[slot] += 1;
        }
    }
    ["passed", "failed", "skipped"]
        .iter()
        .zip(counts)
        .filter(|(_, count)| *count > 0)
        .map(|(name, count)| format!("{count} {name}"))
        .collect::<Vec<_>>()
        .join(", ")
}

fn more(out: &mut String, total: usize, shown: usize, what: &str) {
    if total > shown {
        let _ = writeln!(out, "… {} more {what}", total - shown);
    }
}

/// The facts as plain lines, or `None` when the report has no structured field.
pub(super) fn lines(report: &OrbitAgentReportParams, shown: Shown) -> Option<String> {
    if !report.has_facts() {
        return None;
    }
    let mut out = String::new();
    if let Some(tests_run) = report.tests_run {
        let _ = writeln!(out, "tests_run: {tests_run}");
    }
    if let Some(checks) = &report.checks {
        let summary = checks_summary(checks.iter().map(|check| check.status.as_str()));
        let _ = writeln!(
            out,
            "checks: {}",
            if summary.is_empty() {
                "none".into()
            } else {
                summary
            }
        );
        for check in checks.iter().take(shown.checks) {
            let _ = write!(out, "- {}: {}", check.name, check.status.as_str());
            if let Some(detail) = &check.detail {
                let _ = write!(out, " — {detail}");
            }
            out.push('\n');
        }
        more(&mut out, checks.len(), shown.checks, "checks");
    }
    if let Some(files) = &report.changed_files {
        let _ = writeln!(out, "changed_files: {}", files.len());
        for path in files.iter().take(shown.files) {
            let _ = writeln!(out, "- {path}");
        }
        more(&mut out, files.len(), shown.files, "files");
    }
    if let Some(branch) = &report.branch {
        let _ = writeln!(out, "branch: {branch}");
    }
    if let Some(job_ids) = &report.job_ids {
        let listed: Vec<_> = job_ids
            .iter()
            .take(shown.job_ids)
            .map(String::as_str)
            .collect();
        let _ = writeln!(
            out,
            "job_ids: {}",
            if listed.is_empty() {
                "—".into()
            } else {
                listed.join(", ")
            }
        );
        more(&mut out, job_ids.len(), shown.job_ids, "job IDs");
    }
    if let Some(blocked_on) = &report.blocked_on {
        let _ = writeln!(out, "blocked_on: {blocked_on}");
    }
    out.pop();
    Some(out)
}
