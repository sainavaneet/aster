//! The Orbit team: persistent roles, each owning a tab in the Polaris workspace
//! between the Polaris tab and the Orbit scratch tab, with members that are
//! resumed rather than relaunched. All state changes happen on the server event
//! loop; a session without a roster behaves exactly as before.
use crate::config::PolarisTeamRoleConfig;
use crate::layout::PaneId;

use super::App;

mod panes;
mod projects;
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
    /// `[[polaris.team]]`: the config roles, each pinned in the sessions
    /// whose project it applies to.
    pub(crate) pinned: Vec<PolarisTeamRoleConfig>,
    supervisor: supervisor::Supervisor,
    /// Roles unpinned because their config no longer applies to the session's
    /// project; retired at the next startup or reload when never used.
    unpinned_by_project: Vec<String>,
    /// The hint to give config roles `projects` was shown.
    hinted_unscoped: bool,
}

impl TeamState {
    pub(crate) fn new(pinned: &[PolarisTeamRoleConfig]) -> Self {
        Self {
            pinned: pinned.to_vec(),
            ..Self::default()
        }
    }

    /// The `[[polaris.team]]` entry of `role`, when the role is pinned.
    pub(crate) fn pinned_config(&self, role: &str) -> Option<&PolarisTeamRoleConfig> {
        self.roster
            .role(role)
            .filter(|role| role.pinned)
            .and(self.pinned.iter().find(|config| config.name == role))
    }

    /// The kinds a task relayed within `role` passes through: the session's
    /// pipeline, else a pinned role's config pipeline; empty for none.
    pub(crate) fn pipeline(&self, role: &str) -> &[String] {
        match self.roster.role(role) {
            Some(found) if !found.pipeline.is_empty() => &found.pipeline,
            Some(_) => self
                .pinned_config(role)
                .map_or(&[], |config| config.pipeline.as_slice()),
            None => &[],
        }
    }
}

impl App {
    /// Loads the roster once, the session's `orbit-team.json` or an empty one,
    /// and pins the config roles into it.
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
            Ok(roster) => {
                if let Some(roster) = roster {
                    self.orbit_team.roster = roster;
                }
                self.pin_orbit_team_roles();
            }
            Err(error) => {
                tracing::warn!(%error, "orbit team roster unavailable");
                self.state.config_diagnostic = Some(error.clone());
                self.orbit_team.error = Some(error);
            }
        }
    }

    /// Ensures every `[[polaris.team]]` role that applies to the session's
    /// project is in the roster and current; a pinned role that no longer
    /// applies is unpinned. Released members stop being supervised. Returns
    /// whether the roster changed.
    fn pin_orbit_team_roles(&mut self) -> bool {
        if self.orbit_team.error.is_some() {
            return false;
        }
        self.hint_unscoped_team_roles();
        let (applicable, left) = self.applicable_team_roles();
        let pinned = roster::pin(
            &self.orbit_team.roster,
            &applicable,
            &self.orbit_worker_policy,
        );
        if !pinned.errors.is_empty() {
            let message = pinned.errors.join("; ");
            tracing::warn!(%message, "orbit team config roles rejected");
            self.state.config_diagnostic = Some(message);
        }
        if pinned.roster == self.orbit_team.roster {
            return false;
        }
        for name in left {
            if self
                .orbit_team
                .roster
                .role(&name)
                .is_some_and(|role| role.pinned)
                && !self.orbit_team.unpinned_by_project.contains(&name)
            {
                tracing::info!(role = %name, "orbit team role unpinned: not configured for this project");
                self.orbit_team.unpinned_by_project.push(name);
            }
        }
        self.orbit_team.roster = pinned.roster;
        for name in &pinned.released {
            tracing::info!(member = %name, "orbit team member released by config");
            self.reset_orbit_team_member(name);
        }
        self.save_orbit_team();
        true
    }

    /// The config roles that apply to this session, and the names of those
    /// that do not. Before the Orbit binding (and so the project) is known, a
    /// role with `projects` keeps the pinning it has.
    fn applicable_team_roles(&mut self) -> (Vec<PolarisTeamRoleConfig>, Vec<String>) {
        let project = self
            .orbit_job_cwd()
            .ok()
            .map(|cwd| projects::canonical_project(&cwd));
        let roster = &self.orbit_team.roster;
        let (applicable, left): (Vec<_>, Vec<_>) = self
            .orbit_team
            .pinned
            .iter()
            .cloned()
            .partition(|config| match &project {
                Some(project) => projects::applies(config, project),
                None => {
                    config.projects.is_empty()
                        || roster.role(&config.name).is_some_and(|role| role.pinned)
                }
            });
        (
            applicable,
            left.into_iter().map(|config| config.name).collect(),
        )
    }

    /// Once per server: config roles without `projects` are pinned in every
    /// session, so suggest scoping them.
    fn hint_unscoped_team_roles(&mut self) {
        if self.orbit_team.hinted_unscoped {
            return;
        }
        let names: Vec<&str> = self
            .orbit_team
            .pinned
            .iter()
            .filter(|config| config.projects.is_empty())
            .map(|config| config.name.as_str())
            .collect();
        if names.is_empty() {
            return;
        }
        let message = projects::unscoped_hint(&names);
        tracing::warn!(%message, "orbit team config roles without projects");
        self.state.config_diagnostic = Some(message);
        self.orbit_team.hinted_unscoped = true;
    }

    /// Removes, and closes the tab of, each role unpinned because its config no
    /// longer applies to this project, when no task was ever given to its
    /// members in this session. A used role stays, unpinned, for the user or
    /// Polaris to remove.
    fn retire_unused_orbit_roles(&mut self) {
        if self.resolve_orbit().is_err() {
            return;
        }
        for name in std::mem::take(&mut self.orbit_team.unpinned_by_project) {
            let Some(role) = self.orbit_team.roster.role(&name).cloned() else {
                continue;
            };
            if role.pinned || self.orbit_role_used(&role) {
                continue;
            }
            let params = crate::api::schema::OrbitTeamRemoveParams {
                name: name.clone(),
                stop_members: true,
                force: false,
            };
            match self.orbit_team_remove("orbit:team:retire", params) {
                Ok(_) => tracing::info!(role = %name, "unused orbit team role retired"),
                Err((_, error)) => {
                    tracing::warn!(role = %name, %error, "unused orbit team role kept");
                }
            }
        }
    }

    /// Whether a task was ever given to a member of `role` in this session:
    /// the role's own record, or a task in the ledger.
    fn orbit_role_used(&mut self, role: &roster::Role) -> bool {
        if role.used {
            return true;
        }
        let members: Vec<String> = role.members.iter().map(|m| m.name.clone()).collect();
        let terminals: Vec<String> = role
            .members
            .iter()
            .filter_map(|member| member.terminal_id.clone())
            .collect();
        self.orbit_ledger_mentions_role(&role.name, &members, &terminals)
    }

    /// Records that a task went to a member of `role`.
    pub(crate) fn note_orbit_role_used(&mut self, role: &str) {
        let Some(index) = self.orbit_team.roster.role_index(role) else {
            return;
        };
        if self.orbit_team.roster.roles[index].used {
            return;
        }
        self.orbit_team.roster.roles[index].used = true;
        self.save_orbit_team();
    }

    /// Session start with the Orbit binding resolved: the project is known, so
    /// config roles are pinned for it and unused roles of other projects go.
    pub(super) fn pin_orbit_team_for_project(&mut self) {
        self.pin_orbit_team_roles();
        self.retire_unused_orbit_roles();
    }

    /// Config reload: the new `[[polaris.team]]` applies at once, and a role new
    /// to config gets its tab when the team is already running.
    pub(crate) fn reload_orbit_team_config(&mut self, pinned: &[PolarisTeamRoleConfig]) {
        self.orbit_team.pinned = pinned.to_vec();
        if !self.orbit_team.loaded {
            return;
        }
        let changed = self.pin_orbit_team_roles();
        self.retire_unused_orbit_roles();
        if changed && self.orbit_team.supervisor.is_active() {
            self.ensure_pinned_role_tabs();
            self.activate_orbit_team_supervisor();
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
        self.loaded_orbit_team_member_of_terminal(terminal_id)
    }

    /// `orbit_team_member_of_terminal` for a roster that is already loaded.
    pub(crate) fn loaded_orbit_team_member_of_terminal(
        &self,
        terminal_id: &str,
    ) -> Option<(String, String)> {
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
