//! The Orbit team: persistent roles, each owning a tab in the Polaris workspace
//! between the Polaris tab and the Orbit scratch tab, with members that are
//! resumed rather than relaunched. All state changes happen on the server event
//! loop; a session without a roster behaves exactly as before.
use crate::config::PolarisTeamRoleConfig;
use crate::layout::PaneId;

use super::App;

mod panes;
pub(crate) mod prompt;
pub(crate) mod roster;
pub(crate) mod store;
mod supervisor;
mod tabs;
#[cfg(test)]
mod tests;

pub(crate) use roster::Roster;

#[derive(Default)]
pub(crate) struct TeamState {
    pub(crate) roster: Roster,
    loaded: bool,
    /// The roster file could not be read; nothing is saved over it.
    pub(crate) error: Option<String>,
    /// `[[polaris.team]]`, used only when the session has no roster yet.
    seed: Vec<PolarisTeamRoleConfig>,
    supervisor: supervisor::Supervisor,
}

impl TeamState {
    pub(crate) fn new(seed: &[PolarisTeamRoleConfig]) -> Self {
        Self {
            seed: seed.to_vec(),
            ..Self::default()
        }
    }

    pub(crate) fn set_seed(&mut self, seed: &[PolarisTeamRoleConfig]) {
        self.seed = seed.to_vec();
    }
}

impl App {
    /// Loads the roster once: the session's `orbit-team.json`, or the config
    /// seed when the session has none.
    pub(crate) fn ensure_orbit_team_loaded(&mut self) {
        if self.orbit_team.loaded {
            return;
        }
        self.orbit_team.loaded = true;
        let loaded = if self.policy.persist_session {
            store::load(&store::path())
        } else {
            Ok(None)
        };
        match loaded {
            Ok(Some(roster)) => self.orbit_team.roster = roster,
            Ok(None) if self.orbit_team.seed.is_empty() => {}
            Ok(None) => match roster::seed(&self.orbit_team.seed, &self.orbit_worker_policy) {
                Ok(roster) => {
                    self.orbit_team.roster = roster;
                    self.save_orbit_team();
                }
                Err(error) => {
                    tracing::warn!(%error, "orbit team seed rejected");
                    self.state.config_diagnostic = Some(error);
                }
            },
            Err(error) => {
                tracing::warn!(%error, "orbit team roster unavailable");
                self.state.config_diagnostic = Some(error.clone());
                self.orbit_team.error = Some(error);
            }
        }
    }

    /// Hands the roster to the Orbit writer; a failure is logged and surfaced
    /// as a diagnostic when its result arrives.
    pub(crate) fn save_orbit_team(&mut self) {
        if !self.policy.persist_session || self.orbit_team.error.is_some() {
            return;
        }
        let result = match store::serialize(&self.orbit_team.roster) {
            Ok(bytes) => self.orbit_writer.replace(
                crate::app::orbit_files::OrbitFile::Roster,
                store::path(),
                bytes,
            ),
            Err(error) => Some(Err(error)),
        };
        if let Some(result) = result {
            self.note_orbit_team_saved(result.map_err(|error| error.to_string()));
        }
    }

    /// Records a roster write.
    pub(crate) fn note_orbit_team_saved(&mut self, result: Result<(), String>) {
        if let Err(error) = result {
            let message = format!("Could not save the Orbit team roster: {error}");
            tracing::warn!(%message);
            self.state.config_diagnostic = Some(message);
        }
    }

    /// The bound tab of `role`; a missing tab is an error, never another tab.
    pub(crate) fn resolve_role_tab(
        &self,
        role: &roster::Role,
    ) -> Result<(usize, usize), (&'static str, String)> {
        let binding = role.tab.as_ref().ok_or_else(|| {
            (
                "orbit_team_tab_missing",
                format!("Role {} has no tab yet", role.name),
            )
        })?;
        let missing = || {
            (
                "orbit_team_tab_missing",
                format!(
                    "The tab of role {} no longer exists; it is recreated at the next session start or orbit_team_define",
                    role.name
                ),
            )
        };
        let ws = self
            .state
            .workspaces
            .iter()
            .position(|ws| ws.id == binding.workspace_id)
            .ok_or_else(missing)?;
        let tab = self.state.workspaces[ws]
            .tabs
            .iter()
            .position(|tab| tab.number == binding.tab_number)
            .ok_or_else(missing)?;
        Ok((ws, tab))
    }

    /// Every resolvable role tab, by role name.
    pub(crate) fn orbit_role_tabs(&mut self) -> Vec<(String, usize, usize)> {
        self.ensure_orbit_team_loaded();
        self.orbit_team
            .roster
            .roles
            .iter()
            .filter_map(|role| {
                let (ws, tab) = self.resolve_role_tab(role).ok()?;
                Some((role.name.clone(), ws, tab))
            })
            .collect()
    }

    /// The role whose tab is `(ws, tab)`.
    pub(crate) fn orbit_role_of_tab(&mut self, ws: usize, tab: usize) -> Option<String> {
        self.orbit_role_tabs()
            .into_iter()
            .find(|(_, role_ws, role_tab)| (*role_ws, *role_tab) == (ws, tab))
            .map(|(name, _, _)| name)
    }

    /// The pane in `(ws, tab)` whose terminal hosts the agent named `name`.
    pub(crate) fn orbit_member_pane(
        &self,
        ws: usize,
        tab: usize,
        name: &str,
    ) -> Option<(PaneId, String)> {
        let workspace = self.state.workspaces.get(ws)?;
        workspace
            .tabs
            .get(tab)?
            .layout
            .pane_ids()
            .into_iter()
            .find_map(|pane| {
                let terminal_id = workspace.terminal_id(pane)?;
                let terminal = self.state.terminals.get(terminal_id)?;
                (terminal.agent_name.as_deref() == Some(name)
                    && (self.terminal_runtimes.get(terminal_id).is_some()
                        || terminal.pending_agent_resume_plan.is_some()))
                .then(|| (pane, terminal_id.to_string()))
            })
    }

    /// The roster member whose agent runs, or last ran, in `terminal_id`.
    pub(crate) fn orbit_team_member_of_terminal(
        &mut self,
        terminal_id: &str,
    ) -> Option<(String, String)> {
        self.ensure_orbit_team_loaded();
        let agent_name = self
            .state
            .terminals
            .get(terminal_id)
            .and_then(|terminal| terminal.agent_name.clone());
        // A named agent is the member of that name or no member; an unnamed
        // terminal belongs to the member that last ran there.
        self.orbit_team.roster.roles.iter().find_map(|role| {
            role.members
                .iter()
                .find(|member| match agent_name.as_deref() {
                    Some(name) => name == member.name,
                    None => member.terminal_id.as_deref() == Some(terminal_id),
                })
                .map(|member| (role.name.clone(), member.name.clone()))
        })
    }

    /// Whether `(ws, tab)` is the bound Orbit tab or a role tab.
    pub(crate) fn orbit_scope_contains(&mut self, ws: usize, tab: usize) -> bool {
        self.resolve_orbit().is_ok_and(|orbit| orbit == (ws, tab))
            || self.orbit_role_of_tab(ws, tab).is_some()
    }

    /// The assignment line telling a member which role it acts for.
    pub(crate) fn orbit_team_role_instruction(&mut self, worker_id: &str) -> Option<String> {
        let (role, _) = self.orbit_team_member_of_terminal(worker_id)?;
        let role = self.orbit_team.roster.role(&role)?;
        Some(format!(
            "\n\nYou act for the {} role (area: {}); keep to that area and report as instructed below.",
            role.label(),
            if role.area.trim().is_empty() { "not described" } else { role.area.trim() }
        ))
    }
}
