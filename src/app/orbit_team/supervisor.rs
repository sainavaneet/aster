//! Keeps roster members alive. A member found in its role tab is left alone; a
//! missing one is launched there, continuing its native session when one was
//! observed. A member whose agent exits is relaunched in the same pane after
//! `MEMBER_RELAUNCH_DELAY_MS`, at most `MAX_RELAUNCHES` times per
//! `RELAUNCH_WINDOW`, then the supervisor gives up and tells the user and Polaris.
//! Launches retry like Polaris's startup, so the event loop never blocks.
use std::collections::{HashMap, VecDeque};
use std::time::{Duration, Instant};

use crate::api::schema::{AgentStartParams, PaneRenameParams};
use crate::app::agents::AgentStartError;
use crate::app::App;
use crate::detect::Agent;
use crate::terminal::TerminalState;

use super::roster::{Member, MemberSession, Role};

pub(crate) const MEMBER_RELAUNCH_DELAY_MS: u64 = 5_000;
const MEMBER_RELAUNCH_DELAY: Duration = Duration::from_millis(MEMBER_RELAUNCH_DELAY_MS);
pub(crate) const MAX_RELAUNCHES: usize = 3;
pub(crate) const RELAUNCH_WINDOW: Duration = Duration::from_secs(10 * 60);
const SUPERVISE_INTERVAL: Duration = Duration::from_millis(250);
const LAUNCH_RETRY: Duration = Duration::from_millis(250);
/// How long a launch waits for its pane's shell before it counts as failed.
const LAUNCH_TIMEOUT: Duration = Duration::from_secs(15);
/// A resumed member that exits this soon most likely could not resume; its
/// session is dropped so the next relaunch starts fresh.
const RESUME_FAILURE_WINDOW: Duration = Duration::from_secs(15);
pub(crate) const GAVE_UP: &str = "member stopped; relaunch limit reached";

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(super) enum Phase {
    /// Not yet seen this server run.
    #[default]
    Unchecked,
    Running,
    /// A launch is due at `at`; `deadline` is set once it waits for a shell.
    Due {
        at: Instant,
        deadline: Option<Instant>,
    },
    GaveUp,
}

#[derive(Debug, Default)]
pub(super) struct MemberRuntime {
    pub(super) phase: Phase,
    relaunches: VecDeque<Instant>,
    /// When the last launch with a native resume happened.
    resumed_at: Option<Instant>,
    pub(super) last_error: Option<String>,
}

#[derive(Debug, Default)]
pub(super) struct Supervisor {
    active: bool,
    next_tick: Option<Instant>,
    pub(super) members: HashMap<String, MemberRuntime>,
}

/// A native session as the terminal reports it, borrowed so a supervision pass
/// that finds nothing new allocates nothing.
struct SessionView<'a> {
    source: &'a str,
    agent: &'a str,
    kind: crate::agent_resume::AgentSessionRefKind,
    value: &'a str,
}

impl SessionView<'_> {
    fn matches(&self, stored: Option<&MemberSession>) -> bool {
        stored.is_some_and(|stored| {
            stored.source == self.source
                && stored.agent == self.agent
                && stored.kind == self.kind
                && stored.value == self.value
        })
    }

    fn into_session(self) -> MemberSession {
        MemberSession {
            source: self.source.to_owned(),
            agent: self.agent.to_owned(),
            kind: self.kind,
            value: self.value.to_owned(),
        }
    }
}

/// The native session the terminal's agent reported, when it is `agent`.
fn observed_session(terminal: &TerminalState, agent: Agent) -> Option<SessionView<'_>> {
    if let Some(authority) = terminal.hook_authority.as_ref() {
        if let Some(session_ref) = authority.session_ref.as_ref().filter(|_| {
            crate::detect::parse_agent_label(&authority.agent_label) == Some(agent)
                && crate::agent_resume::is_official_agent_source(
                    &authority.source,
                    &authority.agent_label,
                )
        }) {
            return Some(SessionView {
                source: &authority.source,
                agent: &authority.agent_label,
                kind: session_ref.kind,
                value: &session_ref.value,
            });
        }
    }
    terminal
        .persisted_agent_session
        .as_ref()
        .filter(|session| crate::detect::parse_agent_label(&session.agent) == Some(agent))
        .map(|session| SessionView {
            source: &session.source,
            agent: &session.agent,
            kind: session.session_ref.kind,
            value: &session.session_ref.value,
        })
}

impl Supervisor {
    pub(super) fn is_active(&self) -> bool {
        self.active
    }

    fn phase(&self, member: &str) -> Phase {
        self.members
            .get(member)
            .map_or(Phase::Unchecked, |runtime| runtime.phase)
    }

    fn runtime(&mut self, member: &str) -> &mut MemberRuntime {
        if !self.members.contains_key(member) {
            self.members
                .insert(member.to_owned(), MemberRuntime::default());
        }
        self.members
            .get_mut(member)
            .expect("runtime inserted above")
    }
}

impl App {
    pub(crate) fn activate_orbit_team_supervisor(&mut self) {
        self.orbit_team.supervisor.active = true;
        self.orbit_team.supervisor.next_tick = None;
    }

    /// Clears the give-up and relaunch history of `member`, as a definition does.
    pub(crate) fn reset_orbit_team_member(&mut self, member: &str) {
        self.orbit_team.supervisor.members.remove(member);
    }

    /// The member's supervision state for `orbit.team_list`.
    pub(crate) fn orbit_team_member_phase(&self, member: &str, hosted: bool) -> &'static str {
        if hosted {
            return "running";
        }
        if !self.orbit_team.supervisor.active {
            return "unsupervised";
        }
        match self
            .orbit_team
            .supervisor
            .members
            .get(member)
            .map(|runtime| runtime.phase)
        {
            None | Some(Phase::Unchecked) => "pending",
            Some(Phase::Running) => "exited",
            Some(Phase::Due {
                deadline: Some(_), ..
            }) => "starting",
            Some(Phase::Due { .. }) => "relaunching",
            Some(Phase::GaveUp) => "gave_up",
        }
    }

    pub(crate) fn orbit_team_member_error(&self, member: &str) -> Option<String> {
        self.orbit_team
            .supervisor
            .members
            .get(member)?
            .last_error
            .clone()
    }

    /// Whether the member last seen in `terminal_id` is about to be relaunched
    /// there, so an assignment to it is not lost yet.
    pub(crate) fn orbit_team_member_recovering(&self, terminal_id: &str) -> bool {
        let supervisor = &self.orbit_team.supervisor;
        supervisor.active
            && self
                .orbit_team
                .roster
                .roles
                .iter()
                .flat_map(|role| &role.members)
                .filter(|member| member.terminal_id.as_deref() == Some(terminal_id))
                .any(|member| {
                    matches!(
                        supervisor
                            .members
                            .get(&member.name)
                            .map(|runtime| runtime.phase),
                        None | Some(Phase::Unchecked | Phase::Running | Phase::Due { .. })
                    )
                })
    }

    /// One supervision pass at most every `SUPERVISE_INTERVAL`, after Polaris
    /// started. Returns whether anything visible changed.
    pub(crate) fn supervise_orbit_team(&mut self, now: Instant) -> bool {
        let supervisor = &mut self.orbit_team.supervisor;
        if !supervisor.active || self.orbit_team.roster.roles.is_empty() {
            return false;
        }
        if supervisor.next_tick.is_some_and(|next| now < next) {
            return false;
        }
        supervisor.next_tick = Some(now + SUPERVISE_INTERVAL);
        if self.pending_polaris_start.is_some() {
            return false;
        }
        let mut changed = self.ensure_pinned_role_tabs();
        let mut dirty = false;
        // By index, reading the roster in place: the role is cloned only when a
        // member must be launched or rescheduled, never on a quiet pass.
        for role_index in 0..self.orbit_team.roster.roles.len() {
            let role = &self.orbit_team.roster.roles[role_index];
            let Ok((ws, tab)) = self.resolve_role_tab(role) else {
                continue;
            };
            for index in 0..role.members.len() {
                let role = &self.orbit_team.roster.roles[role_index];
                let member = &role.members[index];
                // A kind from a newer build is listed but never launched.
                if !member.supported() {
                    continue;
                }
                let hosted = self.orbit_member_pane(ws, tab, &member.name);
                let phase = self.orbit_team.supervisor.phase(&member.name);
                match (hosted, phase) {
                    (Some((_, terminal_id)), phase) => {
                        if phase != Phase::Running {
                            let name = member.name.clone();
                            self.orbit_team.supervisor.runtime(&name).phase = Phase::Running;
                        }
                        dirty |= self.observe_orbit_member(role_index, index, &terminal_id);
                    }
                    (None, Phase::Unchecked) => {
                        let role = role.clone();
                        changed |= self.launch_orbit_member(&role, index, ws, tab, now);
                    }
                    (None, Phase::Running) => {
                        let role = role.clone();
                        self.orbit_member_down(&role, &role.members[index], now, "agent exited")
                    }
                    (None, Phase::Due { at, .. }) if now >= at => {
                        let role = role.clone();
                        changed |= self.launch_orbit_member(&role, index, ws, tab, now);
                    }
                    (None, _) => {}
                }
            }
        }
        if dirty {
            self.save_orbit_team();
        }
        changed
    }

    /// Records the member's terminal and native session; returns whether the
    /// roster changed. Compares in place and allocates only on a change.
    fn observe_orbit_member(&mut self, role: usize, index: usize, terminal_id: &str) -> bool {
        let member = &self.orbit_team.roster.roles[role].members[index];
        let session = self
            .state
            .terminals
            .get(terminal_id)
            .zip(member.agent())
            .and_then(|(terminal, agent)| observed_session(terminal, agent))
            .filter(|seen| !seen.matches(member.session.as_ref()))
            .map(SessionView::into_session);
        let moved = member.terminal_id.as_deref() != Some(terminal_id);
        if !moved && session.is_none() {
            return false;
        }
        let member = &mut self.orbit_team.roster.roles[role].members[index];
        if moved {
            member.terminal_id = Some(terminal_id.to_owned());
        }
        if session.is_some() {
            member.session = session;
        }
        true
    }

    /// The member's agent is gone: schedule a relaunch within the budget, or give up.
    fn orbit_member_down(&mut self, role: &Role, member: &Member, now: Instant, reason: &str) {
        let runtime = self
            .orbit_team
            .supervisor
            .members
            .entry(member.name.clone())
            .or_default();
        let failed_resume = runtime
            .resumed_at
            .take()
            .is_some_and(|at| now.saturating_duration_since(at) < RESUME_FAILURE_WINDOW);
        while runtime
            .relaunches
            .front()
            .is_some_and(|at| now.saturating_duration_since(*at) >= RELAUNCH_WINDOW)
        {
            runtime.relaunches.pop_front();
        }
        if runtime.relaunches.len() >= MAX_RELAUNCHES {
            runtime.phase = Phase::GaveUp;
            tracing::warn!(member = %member.name, reason, "orbit member relaunch limit reached");
            self.give_up_orbit_member(role, member);
            return;
        }
        runtime.relaunches.push_back(now);
        runtime.phase = Phase::Due {
            at: now + MEMBER_RELAUNCH_DELAY,
            deadline: None,
        };
        tracing::info!(member = %member.name, reason, "orbit member relaunch scheduled");
        if failed_resume {
            if let Some(member) = self.orbit_team.roster.member_mut(&member.name) {
                tracing::warn!(member = %member.name, "orbit member could not resume; next launch starts fresh");
                member.session = None;
                self.save_orbit_team();
            }
        }
    }

    fn give_up_orbit_member(&mut self, role: &Role, member: &Member) {
        let worker_id = member
            .terminal_id
            .clone()
            .unwrap_or_else(|| member.name.clone());
        self.record_orbit_team_notice(
            &format!("member-{}-gave-up", member.name),
            &member.name,
            GAVE_UP,
        );
        let summary = format!(
            "Team member {} of role {} exited {MAX_RELAUNCHES} times within 10 minutes and is no longer relaunched. Ask the user to check its pane; orbit_team_define for the role restarts it.",
            member.name, role.name
        );
        self.send_orbit_team_lead_notice(
            format!("team-{}-gave-up", member.name),
            &worker_id,
            summary,
        );
    }

    fn orbit_member_launch_failed(
        &mut self,
        role: &Role,
        member: &Member,
        now: Instant,
        error: String,
    ) {
        tracing::warn!(member = %member.name, %error, "orbit member launch failed");
        if let Some(runtime) = self.orbit_team.supervisor.members.get_mut(&member.name) {
            runtime.last_error = Some(error);
        }
        self.orbit_member_down(role, member, now, "launch failed");
    }

    /// Attempts the launch of `role.members[index]`; returns whether a pane changed.
    fn launch_orbit_member(
        &mut self,
        role: &Role,
        index: usize,
        ws: usize,
        tab: usize,
        now: Instant,
    ) -> bool {
        let member = &role.members[index];
        let deadline = match self
            .orbit_team
            .supervisor
            .members
            .get(&member.name)
            .map(|runtime| runtime.phase)
        {
            Some(Phase::Due {
                deadline: Some(deadline),
                ..
            }) => deadline,
            _ => now + LAUNCH_TIMEOUT,
        };
        let launch = match super::roster::admit_member(&self.orbit_worker_policy, member) {
            Ok(launch) => launch,
            Err((_, error)) => {
                self.orbit_member_launch_failed(role, member, now, error);
                return false;
            }
        };
        let (pane_id, split) = match self.orbit_member_launch_pane(role, index, ws, tab) {
            Ok(target) => target,
            Err(error) => {
                self.orbit_member_launch_failed(role, member, now, error);
                return false;
            }
        };
        // Observed state may have changed since the roster was cloned.
        let member = self
            .orbit_team
            .roster
            .member(&member.name)
            .map(|(_, member)| member.clone())
            .unwrap_or_else(|| member.clone());
        let args = super::prompt::launch_args(role, &member, &launch, true);
        let resumed = member
            .agent()
            .zip(member.session.as_ref())
            .and_then(|(agent, session)| {
                super::prompt::resume_args(agent, session)?;
                session.to_persisted()
            });
        let started = self.start_agent(AgentStartParams {
            name: member.name.clone(),
            kind: member.kind.clone(),
            pane_id: pane_id.clone(),
            args,
            timeout_ms: None,
        });
        match started {
            Ok((agent, _)) => {
                if let (Some(session), Some(terminal)) = (
                    resumed.clone(),
                    self.state.terminals.get_mut(agent.terminal_id.as_str()),
                ) {
                    terminal.set_managed_agent_launch_session(session);
                }
                self.handle_pane_rename(
                    "orbit:team:label".into(),
                    PaneRenameParams {
                        pane_id,
                        label: Some(member.name.clone()),
                    },
                );
                if let Some(stored) = self.orbit_team.roster.member_mut(&member.name) {
                    stored.terminal_id = Some(agent.terminal_id.clone());
                }
                self.save_orbit_team();
                let runtime = self
                    .orbit_team
                    .supervisor
                    .members
                    .entry(member.name.clone())
                    .or_default();
                runtime.phase = Phase::Running;
                runtime.last_error = None;
                runtime.resumed_at = resumed.is_some().then_some(now);
                tracing::info!(member = %member.name, resumed = resumed.is_some(), "orbit member launched");
                true
            }
            Err(
                AgentStartError::TargetBusy(_)
                | AgentStartError::ShellNotReady(_)
                | AgentStartError::TargetUnavailable(_),
            ) if now < deadline => {
                if let Some(runtime) = self.orbit_team.supervisor.members.get_mut(&member.name) {
                    runtime.phase = Phase::Due {
                        at: now + LAUNCH_RETRY,
                        deadline: Some(deadline),
                    };
                }
                split
            }
            Err(error) => {
                let message = self.agent_start_error_body(error).message;
                self.orbit_member_launch_failed(role, &member, now, message);
                split
            }
        }
    }
}
