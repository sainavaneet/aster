//! Where a member launches: its last terminal, the first pane for the first
//! member, another free shell in the role tab, or a new split (right for the
//! second member, then down).
use crate::api::schema::{PaneSplitParams, SplitDirection};
use crate::app::App;

use super::roster::Role;

/// A pane a member may launch into: a live shell with no agent or pending resume.
pub(super) fn free_shell(app: &App, terminal_id: &str) -> bool {
    app.state
        .terminals
        .get_key_value(terminal_id)
        .is_some_and(|(key, terminal)| {
            app.terminal_runtimes.get(key).is_some()
                && !terminal.is_agent_terminal()
                && terminal.managed_agent_kind().is_none()
                && terminal.pending_agent_resume_plan.is_none()
                && terminal.restore_error.is_none()
        })
}

impl App {
    /// The public pane id a member launches into, and whether a pane was split
    /// for it: its last terminal, the root pane for the first member, another
    /// free shell, or a new split (right for the second member, then down).
    pub(super) fn orbit_member_launch_pane(
        &mut self,
        role: &Role,
        index: usize,
        ws: usize,
        tab: usize,
    ) -> Result<(String, bool), String> {
        let member = &role.members[index];
        let stored = self
            .orbit_team
            .roster
            .member(&member.name)
            .and_then(|(_, member)| member.terminal_id.clone());
        let claimed: Vec<String> = role
            .members
            .iter()
            .filter(|other| other.name != member.name)
            .filter_map(|other| {
                self.orbit_team
                    .roster
                    .member(&other.name)?
                    .1
                    .terminal_id
                    .clone()
            })
            .collect();
        let workspace = &self.state.workspaces[ws];
        let layout = workspace.tabs[tab].layout.pane_ids();
        let terminal_of = |pane| workspace.terminal_id(pane).map(ToString::to_string);
        let usable =
            |terminal_id: &String| free_shell(self, terminal_id) && !claimed.contains(terminal_id);
        let root = workspace.tabs[tab].root_pane;
        let chosen = layout
            .iter()
            .copied()
            .find(|pane| {
                terminal_of(*pane).is_some_and(|id| Some(&id) == stored.as_ref() && usable(&id))
            })
            .or_else(|| {
                (index == 0)
                    .then_some(root)
                    .filter(|pane| terminal_of(*pane).is_some_and(|id| usable(&id)))
            })
            .or_else(|| {
                layout
                    .iter()
                    .copied()
                    .find(|pane| terminal_of(*pane).is_some_and(|id| usable(&id)))
            });
        if let Some(pane) = chosen {
            let pane_id = self
                .public_pane_id(ws, pane)
                .ok_or("Member pane identity is unavailable")?;
            return Ok((pane_id, false));
        }
        self.split_for_orbit_member(role, index, ws, tab)
            .map(|pane_id| (pane_id, true))
    }

    fn split_for_orbit_member(
        &mut self,
        role: &Role,
        index: usize,
        ws: usize,
        tab: usize,
    ) -> Result<String, String> {
        let root = self.state.workspaces[ws].tabs[tab].root_pane;
        let anchor = index
            .checked_sub(2)
            .and_then(|other| self.orbit_member_pane(ws, tab, &role.members[other].name))
            .map(|(pane, _)| pane)
            .unwrap_or(root);
        let target = self
            .public_pane_id(ws, anchor)
            .ok_or("Role tab pane identity is unavailable")?;
        let response = self.handle_pane_split(
            "orbit:team:split".into(),
            PaneSplitParams {
                workspace_id: None,
                target_pane_id: Some(target),
                direction: if index == 1 {
                    SplitDirection::Right
                } else {
                    SplitDirection::Down
                },
                ratio: None,
                cwd: None,
                focus: false,
                right_click: Default::default(),
                env: Default::default(),
            },
        );
        let value: serde_json::Value = serde_json::from_str(&response).unwrap_or_default();
        let pane = &value["result"]["pane"];
        let (Some(pane_id), Some(terminal_id)) =
            (pane["pane_id"].as_str(), pane["terminal_id"].as_str())
        else {
            return Err(format!("Could not split the role tab: {response}"));
        };
        if let Some(stored) = self.orbit_team.roster.member_mut(&role.members[index].name) {
            stored.terminal_id = Some(terminal_id.to_owned());
        }
        self.save_orbit_team();
        Ok(pane_id.to_owned())
    }
}
