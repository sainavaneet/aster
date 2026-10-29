//! `orbit.team_define`, `orbit.team_remove`, `orbit.team_list`, and routing by
//! role for assignments, prompts, status, and inspection. Roles live in the
//! session roster (`app::orbit_team`); every operation runs on the event loop.
use serde_json::{json, Value};

use super::responses::encode_error;
use crate::api::schema::{
    AgentTarget, OrbitTeamDefineParams, OrbitTeamRemoveParams, ResponseResult, SuccessResponse,
    TabTarget,
};
use crate::app::orbit_team::roster::{self, Role, RoleSpec};
use crate::app::App;
use crate::detect::AgentState;

type Failure = (&'static str, String);

fn reply(id: String, result: Result<Value, Failure>) -> String {
    match result {
        Ok(result) => json!({"id": id, "result": result}).to_string(),
        Err((code, message)) => encode_error(id, code, message),
    }
}

impl App {
    fn orbit_team_ready(&mut self) -> Result<(), Failure> {
        self.ensure_orbit_team_loaded();
        if let Some(error) = &self.orbit_team.error {
            return Err(("orbit_team_unavailable", error.clone()));
        }
        self.resolve_orbit().map(|_| ())
    }

    fn orbit_team_role(&self, name: &str) -> Result<Role, Failure> {
        self.orbit_team.roster.role(name).cloned().ok_or_else(|| {
            (
                "orbit_team_role_not_found",
                format!("No team role named {name}; see orbit_team_list"),
            )
        })
    }

    /// The terminals a role's members run in now, or last ran in.
    fn orbit_role_terminals(&self, role: &Role) -> Vec<(String, Option<String>)> {
        let place = self.resolve_role_tab(role).ok();
        role.members
            .iter()
            .map(|member| {
                let hosted = place
                    .and_then(|(ws, tab)| self.orbit_member_pane(ws, tab, &member.name))
                    .map(|(_, terminal_id)| terminal_id);
                (
                    member.name.clone(),
                    hosted.or_else(|| member.terminal_id.clone()),
                )
            })
            .collect()
    }

    pub(super) fn orbit_role_json(&mut self, role: &Role, with_prompt: bool) -> Value {
        let place = self.resolve_role_tab(role).ok();
        let tab_id = place.and_then(|(ws, tab)| self.public_tab_id(ws, tab));
        let mut members = Vec::new();
        for member in &role.members {
            let hosted = place.and_then(|(ws, tab)| {
                let (pane, terminal_id) = self.orbit_member_pane(ws, tab, &member.name)?;
                Some((
                    self.public_pane_id(ws, pane),
                    self.agent_info(ws, pane),
                    terminal_id,
                ))
            });
            let terminal_id = hosted
                .as_ref()
                .map(|(_, _, terminal_id)| terminal_id.clone())
                .or_else(|| member.terminal_id.clone());
            let open_tasks = terminal_id
                .as_deref()
                .map(|terminal_id| self.orbit_open_tasks_for_worker(terminal_id).len())
                .unwrap_or(0);
            let (pane_id, agent) = hosted
                .map(|(pane_id, agent, _)| (pane_id, agent))
                .unwrap_or_default();
            let state = if member.supported() {
                self.orbit_team_member_phase(&member.name, agent.is_some())
            } else {
                "unsupported"
            };
            members.push(json!({
                "name": member.name,
                "kind": member.kind,
                "model": member.model,
                "permission_mode": member.permission_mode,
                "pane_id": pane_id,
                "terminal_id": terminal_id,
                "state": state,
                "unsupported": !member.supported(),
                "pinned": member.pinned,
                "agent_status": agent.as_ref().map(|agent| agent.agent_status),
                "open_tasks": open_tasks,
                "resumable": member.session.is_some(),
                "error": self.orbit_team_member_error(&member.name),
            }));
        }
        let mut value = json!({
            "name": role.name,
            "label": role.label(),
            "area": role.area,
            "tab_id": tab_id,
            "tab_missing": place.is_none(),
            "pinned": role.pinned,
            "overridden": role.overridden,
            "pipeline": self.orbit_team.pipeline(&role.name),
            "pipeline_overridden": role.pipeline_overridden,
            "members": members,
        });
        if with_prompt {
            value["prompt"] = json!(role.prompt);
        }
        value
    }

    pub(super) fn handle_orbit_team_define(
        &mut self,
        id: String,
        params: OrbitTeamDefineParams,
    ) -> String {
        let result = self.orbit_team_define(params);
        reply(id, result)
    }

    fn orbit_team_define(&mut self, params: OrbitTeamDefineParams) -> Result<Value, Failure> {
        self.orbit_team_ready()?;
        let previous = self.orbit_team.roster.role(&params.name).cloned();
        let role = match self.orbit_team.pinned_config(&params.name).cloned() {
            // A pinned role keeps its config area and members; this session
            // may add members and replace the prompt and the pipeline.
            Some(config) => {
                let prompt = params.prompt.clone().or_else(|| {
                    previous
                        .as_ref()
                        .filter(|role| role.overridden)
                        .map(|role| role.prompt.clone())
                });
                let pipeline = params.pipeline.clone().or_else(|| {
                    previous
                        .as_ref()
                        .filter(|role| role.pipeline_overridden)
                        .map(|role| role.pipeline.clone())
                });
                roster::pinned_role(
                    &config,
                    previous.as_ref(),
                    RoleSpec::from(params).members,
                    prompt,
                    pipeline,
                    &self.orbit_worker_policy,
                )?
            }
            None => roster::validate(
                RoleSpec::from(params),
                &self.orbit_worker_policy,
                previous.as_ref(),
            )?,
        };
        let before = self.orbit_team.roster.clone();
        self.orbit_team.roster.upsert(role.clone())?;
        let index = self
            .orbit_team
            .roster
            .role_index(&role.name)
            .ok_or(("internal_error", "Role vanished".to_owned()))?;
        let mut created_tab = false;
        if self.resolve_role_tab(&role).is_err() {
            if let Err(error) = self.create_orbit_role_tab(index) {
                self.orbit_team.roster = before;
                return Err(error);
            }
            created_tab = true;
        }
        self.save_orbit_team();
        let released: Vec<String> = previous
            .iter()
            .flat_map(|old| &old.members)
            .filter(|old| !role.members.iter().any(|member| member.name == old.name))
            .map(|old| old.name.clone())
            .collect();
        for name in released
            .iter()
            .chain(role.members.iter().map(|member| &member.name))
        {
            self.reset_orbit_team_member(name);
        }
        self.activate_orbit_team_supervisor();
        let role = self.orbit_team.roster.roles[index].clone();
        tracing::info!(role = %role.name, created_tab, "orbit team role defined");
        Ok(json!({
            "role": self.orbit_role_json(&role, true),
            "created_tab": created_tab,
            "released": released,
        }))
    }

    pub(super) fn handle_orbit_team_remove(
        &mut self,
        id: String,
        params: OrbitTeamRemoveParams,
    ) -> String {
        let result = self.orbit_team_remove(&id, params);
        reply(id, result)
    }

    pub(crate) fn orbit_team_remove(
        &mut self,
        id: &str,
        params: OrbitTeamRemoveParams,
    ) -> Result<Value, Failure> {
        self.orbit_team_ready()?;
        let role = self.orbit_team_role(&params.name)?;
        if role.pinned {
            return Err((
                "orbit_role_pinned",
                format!(
                    "Role {} is pinned by [[polaris.team]] in the user's Aster config ({}) and cannot be removed; remove it there and run aster server reload-config",
                    role.name,
                    crate::config::config_path().display()
                ),
            ));
        }
        let terminals: Vec<String> = self
            .orbit_role_terminals(&role)
            .into_iter()
            .filter_map(|(_, terminal_id)| terminal_id)
            .collect();
        let open: Vec<String> = terminals
            .iter()
            .flat_map(|terminal_id| self.orbit_open_tasks_for_worker(terminal_id))
            .collect();
        if !open.is_empty() && !params.force {
            return Err((
                "orbit_worker_assigned",
                format!(
                    "Role members have assignments awaiting their report: {}; wait for the reports, cancel them, or remove with force",
                    open.join(", ")
                ),
            ));
        }
        let cancelled: Vec<String> = terminals
            .iter()
            .flat_map(|terminal_id| self.orbit_cancel_worker_tasks(terminal_id))
            .collect();
        let tab = self.resolve_role_tab(&role).ok();
        let index = self.orbit_team.roster.role_index(&role.name);
        if let Some(index) = index {
            self.orbit_team.roster.roles.remove(index);
        }
        self.save_orbit_team();
        for member in &role.members {
            self.reset_orbit_team_member(&member.name);
        }
        let mut closed_tab = false;
        if let Some((ws, tab)) = tab.filter(|_| params.stop_members) {
            if let Some(tab_id) = self.public_tab_id(ws, tab) {
                let response: Value =
                    serde_json::from_str(&self.handle_tab_close(id.into(), TabTarget { tab_id }))
                        .unwrap_or_default();
                closed_tab = response.get("error").is_none();
            }
        }
        tracing::info!(role = %role.name, closed_tab, ?cancelled, "orbit team role removed");
        Ok(json!({"removed": role.name, "cancelled_tasks": cancelled, "closed_tab": closed_tab}))
    }

    pub(super) fn handle_orbit_team_list(&mut self, id: String) -> String {
        self.ensure_orbit_team_loaded();
        let roles = self.orbit_team.roster.roles.clone();
        let roles: Vec<Value> = roles
            .iter()
            .map(|role| self.orbit_role_json(role, true))
            .collect();
        json!({"id": id, "result": {"roles": roles, "error": self.orbit_team.error}}).to_string()
    }

    /// `orbit.activity` `team`: roles with members, without role prompts.
    pub(super) fn orbit_team_activity(&mut self) -> Value {
        self.ensure_orbit_team_loaded();
        let roles = self.orbit_team.roster.roles.clone();
        Value::Array(
            roles
                .iter()
                .map(|role| self.orbit_role_json(role, false))
                .collect(),
        )
    }

    /// An idle member of `role`, preferring `kind`; for an assignment, also one
    /// without an open assignment. Returns its terminal id.
    pub(super) fn orbit_team_pick_member(
        &mut self,
        role: &str,
        kind: Option<&str>,
        for_assignment: bool,
    ) -> Result<String, Failure> {
        self.ensure_orbit_team_loaded();
        let role = self.orbit_team_role(role)?;
        let (ws, tab) = self.resolve_role_tab(&role)?;
        let wanted = kind.and_then(crate::detect::parse_agent_label);
        let mut candidates = Vec::new();
        let mut busy = Vec::new();
        for member in &role.members {
            if !member.supported() {
                busy.push(format!(
                    "{} (unsupported kind {})",
                    member.name, member.kind
                ));
                continue;
            }
            let Some((_, terminal_id)) = self.orbit_member_pane(ws, tab, &member.name) else {
                busy.push(format!("{} (not running)", member.name));
                continue;
            };
            let idle = self
                .state
                .terminals
                .get(terminal_id.as_str())
                .is_some_and(|terminal| terminal.state == AgentState::Idle);
            let assigned = self.orbit_worker_has_open_task(&terminal_id);
            if !idle || (for_assignment && assigned) {
                busy.push(format!(
                    "{} ({})",
                    member.name,
                    if idle { "assigned" } else { "busy" }
                ));
                continue;
            }
            candidates.push((
                wanted.is_some() && member.agent() == wanted,
                assigned,
                terminal_id,
            ));
        }
        candidates.sort_by_key(|(preferred, assigned, _)| (!preferred, *assigned));
        candidates
            .into_iter()
            .next()
            .map(|(_, _, terminal_id)| terminal_id)
            .ok_or_else(|| {
                (
                    "orbit_role_busy",
                    format!(
                        "No idle member of role {} can take this now: {}; retry when one is idle",
                        role.name,
                        busy.join(", ")
                    ),
                )
            })
    }

    /// `orbit.agent_status {role}`: the role and each member's agent.
    pub(super) fn handle_orbit_role_status(&mut self, id: String, role: &str) -> String {
        self.ensure_orbit_team_loaded();
        let result = self.orbit_team_role(role).and_then(|role| {
            let mut value = self.orbit_role_json(&role, false);
            let (ws, tab) = self.resolve_role_tab(&role)?;
            let panes: Vec<String> = role
                .members
                .iter()
                .filter_map(|member| self.orbit_member_pane(ws, tab, &member.name))
                .filter_map(|(pane, _)| self.public_pane_id(ws, pane))
                .collect();
            let agents: Vec<Value> = panes
                .into_iter()
                .filter_map(|target| {
                    let response =
                        self.handle_agent_get("orbit:team:status".into(), AgentTarget { target });
                    serde_json::from_str::<Value>(&response)
                        .ok()?
                        .get("result")
                        .cloned()
                })
                .collect();
            value["agents"] = json!(agents);
            Ok(value)
        });
        reply(id, result)
    }

    /// `orbit.inspect {role}`: the role's tab and its panes, like the Orbit tab.
    pub(super) fn handle_orbit_role_inspect(&mut self, id: String, role: &str) -> String {
        self.ensure_orbit_team_loaded();
        let target = self
            .orbit_team_role(role)
            .and_then(|role| self.resolve_role_tab(&role).map(|place| (role, place)));
        let (role, (ws, tab)) = match target {
            Ok(target) => target,
            Err((code, message)) => return encode_error(id, code, message),
        };
        let Some(tab_info) = self.tab_info(ws, tab) else {
            return encode_error(id, "orbit_team_tab_missing", "Role tab is unavailable");
        };
        let panes: Vec<_> = self.state.workspaces[ws].tabs[tab]
            .layout
            .pane_ids()
            .into_iter()
            .filter_map(|pane| self.pane_info(ws, pane))
            .collect();
        let mut response = serde_json::to_value(SuccessResponse {
            id,
            result: ResponseResult::TabInfo { tab: tab_info },
        })
        .unwrap_or_default();
        let mut panes = serde_json::to_value(panes).unwrap_or_default();
        if let Some(panes) = panes.as_array_mut() {
            for pane in panes {
                let terminal_id = pane["terminal_id"].as_str().unwrap_or_default().to_owned();
                pane["member"] = json!(self
                    .orbit_team_member_of_terminal(&terminal_id)
                    .filter(|(owner, _)| *owner == role.name)
                    .map(|(_, member)| member));
            }
        }
        response["result"]["panes"] = panes;
        response["result"]["role"] = json!(role.name);
        response.to_string()
    }
}
