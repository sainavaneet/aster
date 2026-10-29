//! The Orbit projection published to client shells: lead, team, scratch
//! workers, and assignments as a read-only snapshot section. It is rebuilt on
//! the server tick, never while a frame renders; the snapshot only clones the
//! cached result. Free text is redacted and shortened, and prompt bodies,
//! report tokens, and role prompts never enter it.
use std::collections::{HashMap, HashSet};
use std::time::{Duration, Instant};

use super::lead::state_name;
use super::redact::redact_secrets;
use super::state::{Delivery, ReportOrigin, Submission, Task};
use super::App;
use crate::api::schema::OrbitOutcome;
use crate::detect::AgentState;
use crate::protocol::{
    OrbitShellActiveTask, OrbitShellLead, OrbitShellMember, OrbitShellRecentTask, OrbitShellRole,
    OrbitShellView,
};

/// Shortest time between two builds of the view.
const REFRESH_INTERVAL: Duration = Duration::from_millis(250);
const TITLE_MAX_CHARS: usize = 48;
const SUMMARY_MAX_CHARS: usize = 80;
const MAX_ACTIVE_TASKS: usize = 32;
const MAX_RECENT_TASKS: usize = 5;
const ELLIPSIS: char = '…';

#[derive(Default)]
pub(super) struct ShellViewCache {
    view: Option<OrbitShellView>,
    next_build: Option<Instant>,
    #[cfg(test)]
    builds: u32,
}

/// `text` on one line, cut to at most `max` characters (an ellipsis counts).
/// Counts characters, so a cut never lands inside a multi-byte one.
fn shorten(text: &str, max: usize) -> String {
    let line = text.split_whitespace().collect::<Vec<_>>().join(" ");
    if line.chars().count() <= max {
        return line;
    }
    let mut cut: String = line.chars().take(max.saturating_sub(1)).collect();
    cut.truncate(cut.trim_end().len());
    cut.push(ELLIPSIS);
    cut
}

/// Redacted, then shortened: a cut never splits a secret into a part the
/// redaction no longer recognises.
pub(super) fn display_text(text: &str, max: usize) -> String {
    shorten(&redact_secrets(text), max)
}

/// Redact the complete report before selecting a line, so wrapped credentials
/// are never exposed by truncation at a line boundary.
pub(super) fn display_first_line(text: &str, max: usize) -> String {
    let redacted = redact_secrets(text);
    shorten(
        redacted
            .lines()
            .find(|line| !line.trim().is_empty())
            .unwrap_or_default(),
        max,
    )
}

/// The assignment's first non-empty line, redacted and shortened. Stored as the
/// task's title at assignment, so the ledger never holds the raw line.
pub(super) fn assignment_title(prompt: &str) -> Option<String> {
    let first = prompt.lines().find(|line| !line.trim().is_empty())?;
    let title = display_text(first, TITLE_MAX_CHARS);
    (!title.is_empty()).then_some(title)
}

/// A stored title for the view. Redacted again, so a title saved by an older
/// build with weaker redaction is still covered.
fn active_title(task: &Task) -> String {
    task.title
        .as_deref()
        .map(|title| display_text(title, TITLE_MAX_CHARS))
        .filter(|title| !title.is_empty())
        .unwrap_or_else(|| task.task_id.clone())
}

fn outcome_name(outcome: &OrbitOutcome) -> &'static str {
    match outcome {
        OrbitOutcome::Completed => "completed",
        OrbitOutcome::Failed => "failed",
        OrbitOutcome::Blocked => "blocked",
    }
}

fn recent_task(task: &Task) -> OrbitShellRecentTask {
    let worker_report = task
        .report
        .as_ref()
        .filter(|_| task.origin != Some(ReportOrigin::Server));
    let outcome = worker_report.map_or_else(
        || task.delivery.as_str(),
        |report| outcome_name(&report.outcome),
    );
    let summary = task
        .report
        .as_ref()
        .map(|report| report.summary.as_str())
        .or(task.error.as_deref())
        .unwrap_or_default();
    OrbitShellRecentTask {
        task_id: task.task_id.clone(),
        outcome: outcome.to_owned(),
        summary: display_text(summary, SUMMARY_MAX_CHARS),
        finished_at_ms: task.updated_at,
    }
}

impl App {
    /// The cached Orbit section, absent when the session has no lead.
    pub(crate) fn orbit_shell_view(&self) -> Option<&OrbitShellView> {
        self.orbit_notifications.shell_view.view.as_ref()
    }

    /// The cached view stamped with the ledger clock at `now`, for a snapshot.
    pub(crate) fn orbit_shell_view_at(&self, now: Instant) -> Option<OrbitShellView> {
        let mut view = self.orbit_shell_view()?.clone();
        view.server_now_ms = Some(self.orbit_notifications.peek_now_ms(now));
        Some(view)
    }

    /// Rebuilds the view at most every `REFRESH_INTERVAL` and publishes it only
    /// when it differs; whether it changed, so the caller can schedule a render.
    pub(crate) fn refresh_orbit_shell_view(&mut self, now: Instant) -> bool {
        let cache = &mut self.orbit_notifications.shell_view;
        if cache.next_build.is_some_and(|next| now < next) {
            return false;
        }
        cache.next_build = Some(now + REFRESH_INTERVAL);
        let view = self.build_orbit_shell_view();
        let cache = &mut self.orbit_notifications.shell_view;
        #[cfg(test)]
        {
            cache.builds += 1;
        }
        if cache.view == view {
            return false;
        }
        cache.view = view;
        true
    }

    #[cfg(test)]
    pub(crate) fn orbit_shell_view_builds(&self) -> u32 {
        self.orbit_notifications.shell_view.builds
    }

    /// The title stored on the ledger task, as assignment saved it.
    #[cfg(test)]
    pub(crate) fn orbit_task_title(&self, task_id: &str) -> Option<&str> {
        self.orbit_notifications
            .tasks
            .iter()
            .find(|task| task.task_id == task_id)?
            .title
            .as_deref()
    }

    fn build_orbit_shell_view(&mut self) -> Option<OrbitShellView> {
        let lead = self.orbit_lead_terminal()?;
        self.ensure_orbit_team_loaded();
        let pane_ids = self.orbit_terminal_pane_ids();
        let (roles, member_terminals) = self.orbit_shell_roles(&pane_ids);
        let scratch = self.orbit_shell_scratch(&lead.terminal_id, &member_terminals, &pane_ids);
        let lead_state = self
            .state
            .terminals
            .get(lead.terminal_id.as_str())
            .map_or("unknown", |terminal| state_name(terminal.state));
        Some(OrbitShellView {
            lead: OrbitShellLead {
                name: crate::app::polaris::NAME.to_owned(),
                pane_id: pane_ids.get(&lead.terminal_id).cloned(),
                terminal_id: lead.terminal_id,
                state: lead_state.to_owned(),
            },
            roles,
            scratch,
            active_tasks: self.orbit_shell_active_tasks(),
            recent_tasks: self.orbit_shell_recent_tasks(),
            // Stamped per snapshot (`orbit_shell_view_at`), not per build, so an
            // unchanged view stays equal to the cached one.
            server_now_ms: None,
        })
    }

    /// Public pane id of every terminal that sits in a pane.
    fn orbit_terminal_pane_ids(&self) -> HashMap<String, String> {
        let mut ids = HashMap::new();
        for (ws, workspace) in self.state.workspaces.iter().enumerate() {
            for tab in &workspace.tabs {
                for pane in tab.layout.pane_ids() {
                    let (Some(terminal_id), Some(pane_id)) =
                        (workspace.terminal_id(pane), self.public_pane_id(ws, pane))
                    else {
                        continue;
                    };
                    ids.insert(terminal_id.to_string(), pane_id);
                }
            }
        }
        ids
    }

    /// Roles with their members, and every terminal a member is known to use.
    fn orbit_shell_roles(
        &self,
        pane_ids: &HashMap<String, String>,
    ) -> (Vec<OrbitShellRole>, HashSet<String>) {
        let mut terminals = HashSet::new();
        let roles = self
            .orbit_team
            .roster
            .roles
            .iter()
            .map(|role| {
                let place = self.resolve_role_tab(role).ok();
                let members = role
                    .members
                    .iter()
                    .map(|member| {
                        let hosted = place
                            .and_then(|(ws, tab)| self.orbit_member_pane(ws, tab, &member.name))
                            .map(|(_, terminal_id)| terminal_id);
                        let live = hosted
                            .as_deref()
                            .and_then(|id| self.state.terminals.get(id))
                            .map(|terminal| state_name(terminal.state));
                        let terminal_id = hosted.or_else(|| member.terminal_id.clone());
                        let state = if !member.supported() {
                            "unsupported"
                        } else {
                            live.unwrap_or_else(|| {
                                self.orbit_team_member_phase(&member.name, false)
                            })
                        };
                        terminals.extend(terminal_id.clone());
                        OrbitShellMember {
                            model: member.model.clone(),
                            name: member.name.clone(),
                            kind: member.kind.clone(),
                            pane_id: terminal_id
                                .as_ref()
                                .and_then(|id| pane_ids.get(id))
                                .cloned(),
                            terminal_id,
                            state: state.to_owned(),
                        }
                    })
                    .collect();
                OrbitShellRole {
                    name: role.name.clone(),
                    pinned: role.pinned,
                    members,
                }
            })
            .collect();
        (roles, terminals)
    }

    /// Agents in the Orbit tab other than the lead and team members.
    fn orbit_shell_scratch(
        &mut self,
        lead_terminal_id: &str,
        member_terminals: &HashSet<String>,
        pane_ids: &HashMap<String, String>,
    ) -> Vec<OrbitShellMember> {
        self.orbit_tab_terminals()
            .into_iter()
            .filter(|(terminal_id, _, _)| {
                terminal_id != lead_terminal_id && !member_terminals.contains(terminal_id)
            })
            .filter_map(|(terminal_id, _, _)| {
                let terminal = self.state.terminals.get(terminal_id.as_str())?;
                let record = self.orbit_workers.get(&terminal_id);
                let hosts_agent = terminal.is_agent_terminal();
                let exited = record.is_some_and(|record| record.exited) && !hosts_agent;
                if !hosts_agent && !exited {
                    return None;
                }
                let name = record
                    .map(|record| record.name.clone())
                    .or_else(|| terminal.agent_name.clone())
                    .unwrap_or_else(|| terminal_id.clone());
                let kind = record
                    .map(|record| record.kind.clone())
                    .or_else(|| {
                        terminal
                            .effective_known_agent()
                            .map(|agent| crate::detect::agent_label(agent).to_owned())
                    })
                    .unwrap_or_else(|| "unknown".to_owned());
                let state = if exited {
                    "exited"
                } else {
                    state_name(terminal.state)
                };
                Some(OrbitShellMember {
                    model: None,
                    name,
                    kind,
                    pane_id: pane_ids.get(&terminal_id).cloned(),
                    terminal_id: Some(terminal_id),
                    state: state.to_owned(),
                })
            })
            .collect()
    }

    fn orbit_shell_active_tasks(&self) -> Vec<OrbitShellActiveTask> {
        self.orbit_notifications
            .tasks
            .iter()
            .filter(|task| task.awaits_worker())
            .take(MAX_ACTIVE_TASKS)
            .map(|task| {
                let terminal = self.state.terminals.get(task.worker_id.as_str());
                let blocked = terminal.is_some_and(|terminal| {
                    terminal.state == AgentState::Blocked && terminal.is_agent_terminal()
                });
                let state = if task.submission == Submission::Pending {
                    "assigning"
                } else {
                    terminal.map_or("unknown", |terminal| state_name(terminal.state))
                };
                let shell_steps = task.shell_steps(self.orbit_step_total(task));
                OrbitShellActiveTask {
                    task_id: task.task_id.clone(),
                    worker_terminal_id: task.worker_id.clone(),
                    role: self
                        .loaded_orbit_team_member_of_terminal(&task.worker_id)
                        .map(|(role, _)| role),
                    title: active_title(task),
                    state: state.to_owned(),
                    created_at_ms: task.created_at,
                    blocked,
                    awaiting_approval: blocked
                        && self
                            .orbit_permission_prompt_agent(&task.worker_id)
                            .is_some(),
                    steps: shell_steps.steps,
                    step_total: shell_steps.step_total,
                    parallel: shell_steps.parallel,
                    steps_omitted: shell_steps.omitted,
                }
            })
            .collect()
    }

    /// The relay's length as `orbit_step_line` counts it: the pipeline of the
    /// last step's role.
    fn orbit_step_total(&self, task: &Task) -> usize {
        task.steps
            .last()
            .and_then(|step| self.orbit_team.roster.member(&step.member))
            .map_or(0, |(role, _)| self.orbit_team.pipeline(&role.name).len())
    }

    fn orbit_shell_recent_tasks(&self) -> Vec<OrbitShellRecentTask> {
        let mut resolved: Vec<&Task> = self
            .orbit_notifications
            .tasks
            .iter()
            .filter(|task| task.notice_for.is_none() && task.delivery != Delivery::AwaitingReport)
            .collect();
        resolved.sort_by_key(|task| std::cmp::Reverse(task.updated_at));
        resolved
            .into_iter()
            .take(MAX_RECENT_TASKS)
            .map(recent_task)
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shorten_keeps_short_text_and_collapses_whitespace() {
        assert_eq!(shorten("  a \n b\tc ", 48), "a b c");
    }

    #[test]
    fn shorten_cuts_on_character_boundaries_within_the_limit() {
        let text = "é".repeat(100);

        let cut = shorten(&text, 48);

        assert_eq!(cut.chars().count(), 48);
        assert!(cut.ends_with(ELLIPSIS));
        assert!(cut.starts_with("éé"));
    }

    #[test]
    fn shorten_handles_wide_and_combining_characters() {
        for text in ["日本語".repeat(40), "e\u{301}".repeat(60), "👩‍👩‍👧".repeat(30)]
        {
            let cut = shorten(&text, 10);
            assert!(cut.chars().count() <= 10, "{cut}");
        }
    }

    #[test]
    fn title_is_the_first_nonempty_line_only() {
        assert_eq!(
            assignment_title("\n  Fix the parser  \nbody with details").as_deref(),
            Some("Fix the parser")
        );
        assert_eq!(assignment_title("  \n \n"), None);
    }

    #[test]
    fn title_redacts_labelled_secrets_and_keeps_prose() {
        assert_eq!(
            assignment_title("Log in with password: hunter2 then deploy").as_deref(),
            Some("Log in with password: <redacted> then deploy")
        );
        assert_eq!(
            assignment_title("Use CLIENT_SECRET=abc; api key \"k 1\"").as_deref(),
            Some("Use CLIENT_SECRET=<redacted>; api key <redacted>")
        );
        assert_eq!(
            assignment_title("update the password reset flow").as_deref(),
            Some("update the password reset flow")
        );
        assert_eq!(
            assignment_title("fix the tokenizer").as_deref(),
            Some("fix the tokenizer")
        );
    }

    #[test]
    fn summary_text_redacts_labelled_secrets() {
        assert_eq!(
            display_text("Done. token: abc123, Authorization: Basic xyz", 80),
            "Done. token: <redacted> Authorization: <redacted>"
        );
        assert_eq!(
            display_text("Reworked the password reset flow", 80),
            "Reworked the password reset flow"
        );
    }

    #[test]
    fn title_redacts_token_like_text_before_cutting() {
        let title = assignment_title(&format!("token {}", "ab".repeat(16))).unwrap();

        assert!(!title.contains("abab"), "{title}");
    }
}
