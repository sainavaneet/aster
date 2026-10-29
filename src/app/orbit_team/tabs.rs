//! Role tabs: created in the Polaris workspace in roster order (pinned roles in
//! config order, then the session's own roles) before the Orbit tab, without
//! changing focus, and bound by workspace id and stable tab number.
use crate::api::schema::{TabCreateParams, TabMoveParams};
use crate::app::App;

use super::roster::TabBinding;

/// Human notification detail when startup recreates a role's missing tab.
pub(crate) const TAB_RECREATED: &str = "role tab recreated";

impl App {
    /// Where a new tab of the role at `role_index` goes in `ws`: before the
    /// first tab of a role later in the roster, or else before the Orbit tab.
    fn orbit_role_tab_slot(&self, role_index: usize, ws: usize, orbit_tab: usize) -> usize {
        self.orbit_team.roster.roles[role_index + 1..]
            .iter()
            .filter_map(|role| self.resolve_role_tab(role).ok())
            .filter(|(role_ws, _)| *role_ws == ws)
            .map(|(_, tab)| tab)
            .fold(orbit_tab, usize::min)
    }

    /// Creates the tab of the role at `role_index` in roster order before the
    /// Orbit tab and binds it. Focus and the active tab are left where they were.
    pub(crate) fn create_orbit_role_tab(
        &mut self,
        role_index: usize,
    ) -> Result<(usize, usize), (&'static str, String)> {
        let (ws, orbit_tab) = self.resolve_orbit()?;
        let slot = self.orbit_role_tab_slot(role_index, ws, orbit_tab);
        let label = self.orbit_team.roster.roles[role_index].label();
        let response = self.handle_tab_create(
            "orbit:team:tab".into(),
            TabCreateParams {
                workspace_id: Some(self.public_workspace_id(ws)),
                cwd: None,
                focus: false,
                label: Some(label),
                env: Default::default(),
            },
        );
        let value: serde_json::Value = serde_json::from_str(&response).unwrap_or_default();
        if value.get("error").is_some() || value.get("result").is_none() {
            return Err((
                "orbit_team_tab_failed",
                format!("Could not create the role tab: {response}"),
            ));
        }
        let created = self.state.workspaces[ws].tabs.len() - 1;
        let number = self.state.workspaces[ws].tabs[created].number;
        if slot < created {
            if let Some(tab_id) = self.public_tab_id(ws, created) {
                self.handle_tab_move(
                    "orbit:team:tab".into(),
                    TabMoveParams {
                        tab_id,
                        insert_index: slot,
                    },
                );
            }
        }
        let tab = self.state.workspaces[ws]
            .tabs
            .iter()
            .position(|tab| tab.number == number)
            .unwrap_or(created);
        self.orbit_team.roster.roles[role_index].tab = Some(TabBinding {
            workspace_id: self.state.workspaces[ws].id.clone(),
            tab_number: number,
        });
        self.save_orbit_team();
        self.schedule_session_save();
        Ok((ws, tab))
    }

    /// Gives each pinned role that never had a tab its tab, so a role added to
    /// config appears without a restart. A role tab the user closed stays closed
    /// until the next startup. Returns whether a tab was created.
    pub(crate) fn ensure_pinned_role_tabs(&mut self) -> bool {
        let mut created = false;
        for index in 0..self.orbit_team.roster.roles.len() {
            let role = &self.orbit_team.roster.roles[index];
            if !role.pinned || role.tab.is_some() {
                continue;
            }
            let name = role.name.clone();
            match self.create_orbit_role_tab(index) {
                Ok(_) => {
                    tracing::info!(role = %name, "pinned orbit role tab created");
                    created = true;
                }
                Err((_, error)) => {
                    tracing::warn!(role = %name, %error, "pinned orbit role tab unavailable");
                    self.state.config_diagnostic = Some(error);
                    return created;
                }
            }
        }
        created
    }

    /// Session start, after Polaris and Orbit: every role gets its tab back (a
    /// recreated tab is announced), restored members keep their role text across
    /// native resume, and the supervisor starts keeping members alive.
    pub(crate) fn prepare_orbit_team_startup(&mut self) {
        self.ensure_orbit_team_loaded();
        if self.resolve_orbit().is_err() {
            return;
        }
        self.pin_orbit_team_for_project();
        if self.orbit_team.roster.roles.is_empty() {
            return;
        }
        for index in 0..self.orbit_team.roster.roles.len() {
            let role = &self.orbit_team.roster.roles[index];
            if self.resolve_role_tab(role).is_ok() {
                continue;
            }
            let (name, rebound) = (role.name.clone(), role.tab.is_some());
            match self.create_orbit_role_tab(index) {
                Ok(_) if rebound => {
                    tracing::info!(role = %name, "orbit role tab recreated");
                    self.record_orbit_team_notice(
                        &format!("role-tab-{name}"),
                        &name,
                        TAB_RECREATED,
                    );
                }
                Ok(_) => {}
                Err((_, error)) => {
                    tracing::warn!(role = %name, %error, "orbit role tab unavailable");
                    self.state.config_diagnostic = Some(error);
                }
            }
        }
        self.augment_restored_member_plans();
        self.activate_orbit_team_supervisor();
    }

    /// A restored member resumes natively with only its session argument; add
    /// its model, permission mode, and role text to the pending plan.
    fn augment_restored_member_plans(&mut self) {
        let roles = self.orbit_team.roster.roles.clone();
        for role in &roles {
            let Ok((ws, tab)) = self.resolve_role_tab(role) else {
                continue;
            };
            for member in &role.members {
                let Some((_, terminal_id)) = self.orbit_member_pane(ws, tab, &member.name) else {
                    continue;
                };
                let Ok(launch) = super::roster::admit_member(&self.orbit_worker_policy, member)
                else {
                    continue;
                };
                let Some(terminal) = self.state.terminals.get_mut(terminal_id.as_str()) else {
                    continue;
                };
                if let Some(plan) = terminal.pending_agent_resume_plan.as_mut() {
                    if super::prompt::augment_resume_plan(plan, role, member, &launch) {
                        tracing::info!(member = %member.name, "orbit member resumes with its role");
                    }
                }
            }
        }
    }
}
