//! The Orbit team roster: roles, their members, and each role's tab binding,
//! with the validation shared by API definitions, config seeds, and loaded
//! rosters. Persistence lives in `store`.
use serde::{Deserialize, Deserializer, Serialize};
use serde_json::{Map, Value};

use crate::agent_resume::{AgentSessionRefKind, PersistedAgentSession};
use crate::app::orbit_worker_policy::{WorkerLaunch, WorkerPolicy};
use crate::detect::Agent;

pub(crate) const MAX_ROLES: usize = 8;
pub(crate) const MAX_MEMBERS: usize = 4;
pub(crate) const MAX_ROLE_PROMPT_BYTES: usize = 4096;
pub(crate) const MAX_AREA_BYTES: usize = 1024;
/// Role names that would collide with the lead or the Orbit scratch tab.
const RESERVED_ROLE_NAMES: [&str; 2] = ["polaris", "orbit"];

/// A rejected roster change: the API error code and message.
pub(crate) type Rejection = (&'static str, String);

/// A role tab, bound by workspace id and stable tab number like the Orbit tab.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct TabBinding {
    pub(crate) workspace_id: String,
    pub(crate) tab_number: usize,
}

/// A member's native agent session, observed while it ran, so a relaunch
/// continues its conversation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct MemberSession {
    pub(crate) source: String,
    pub(crate) agent: String,
    pub(crate) kind: AgentSessionRefKind,
    pub(crate) value: String,
}

impl MemberSession {
    pub(crate) fn to_persisted(&self) -> Option<PersistedAgentSession> {
        crate::agent_resume::session_ref_from_snapshot(
            &self.source,
            &self.agent,
            self.kind,
            &self.value,
        )
    }
}

/// Persisted roster records tolerate a newer build: unknown fields are kept
/// (`extra`) and written back, an unknown member kind loads as unsupported, and
/// an unreadable session is dropped so the member starts fresh.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct Member {
    pub(crate) name: String,
    pub(crate) kind: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) model: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) permission_mode: Option<String>,
    /// The terminal the member last ran in; terminal ids survive a restart.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) terminal_id: Option<String>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "lenient_session"
    )]
    pub(crate) session: Option<MemberSession>,
    /// Fields from a newer build, kept so they survive this build's saves.
    #[serde(flatten)]
    pub(crate) extra: Map<String, Value>,
}

/// A session this build cannot read is dropped; the member starts fresh.
fn lenient_session<'de, D: Deserializer<'de>>(
    deserializer: D,
) -> Result<Option<MemberSession>, D::Error> {
    let value = Option::<Value>::deserialize(deserializer)?;
    Ok(
        value.and_then(|value| match MemberSession::deserialize(&value) {
            Ok(session) => Some(session),
            Err(error) => {
                tracing::warn!(%error, "unreadable Orbit team member session dropped");
                None
            }
        }),
    )
}

impl Member {
    pub(crate) fn agent(&self) -> Option<Agent> {
        crate::detect::parse_agent_label(&self.kind)
    }

    /// Whether this build can run the member: a Claude or Codex kind. Others
    /// (from a newer build) are listed as unsupported and never launched.
    pub(crate) fn supported(&self) -> bool {
        check_kind(&self.kind).is_ok()
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct Role {
    pub(crate) name: String,
    #[serde(default)]
    pub(crate) area: String,
    #[serde(default)]
    pub(crate) prompt: String,
    pub(crate) members: Vec<Member>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) tab: Option<TabBinding>,
    /// Fields from a newer build, kept so they survive this build's saves.
    #[serde(flatten)]
    pub(crate) extra: Map<String, Value>,
}

impl Role {
    /// The tab label: the role name with its first letter capitalised.
    pub(crate) fn label(&self) -> String {
        role_label(&self.name)
    }
}

pub(crate) fn role_label(name: &str) -> String {
    let mut chars = name.chars();
    chars
        .next()
        .map(|first| first.to_uppercase().chain(chars).collect())
        .unwrap_or_default()
}

/// What a caller asks for; validated into a `Role`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct RoleSpec {
    pub(crate) name: String,
    pub(crate) area: String,
    pub(crate) prompt: String,
    pub(crate) members: Vec<MemberSpec>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct MemberSpec {
    pub(crate) name: String,
    pub(crate) kind: String,
    pub(crate) model: Option<String>,
    pub(crate) permission_mode: Option<String>,
}

impl From<&crate::config::PolarisTeamRoleConfig> for RoleSpec {
    fn from(config: &crate::config::PolarisTeamRoleConfig) -> Self {
        Self {
            name: config.name.clone(),
            area: config.area.clone(),
            prompt: config.prompt.clone(),
            members: config
                .members
                .iter()
                .map(|member| MemberSpec {
                    name: member.name.clone(),
                    kind: member.kind.clone(),
                    model: member.model.clone(),
                    permission_mode: member.permission_mode.clone(),
                })
                .collect(),
        }
    }
}

impl From<crate::api::schema::OrbitTeamDefineParams> for RoleSpec {
    fn from(params: crate::api::schema::OrbitTeamDefineParams) -> Self {
        Self {
            name: params.name,
            area: params.area,
            prompt: params.prompt.unwrap_or_default(),
            members: params
                .members
                .into_iter()
                .map(|member| MemberSpec {
                    name: member.name,
                    kind: member.kind,
                    model: member.model,
                    permission_mode: member.permission_mode,
                })
                .collect(),
        }
    }
}

fn invalid(message: impl Into<String>) -> Rejection {
    ("invalid_params", message.into())
}

fn check_name(what: &str, name: &str) -> Result<(), Rejection> {
    if crate::app::agents::valid_agent_name(name) {
        return Ok(());
    }
    Err(invalid(format!(
        "{what} {name:?} must start with a lowercase letter and contain only lowercase letters, digits, '-' or '_' (1-32 characters)"
    )))
}

/// Members run Claude or Codex: both take role instructions at launch.
fn check_kind(kind: &str) -> Result<(), Rejection> {
    match crate::detect::parse_agent_label(kind) {
        Some(Agent::Claude | Agent::Codex) => Ok(()),
        _ => Err((
            crate::app::orbit_worker_policy::NOT_ALLOWED_CODE,
            format!("Team member kind {kind:?} is not supported; use claude or codex"),
        )),
    }
}

/// Whether `check_shape` rejects member kinds this build cannot run.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Kinds {
    /// API definitions and config seeds.
    Supported,
    /// Loaded rosters, possibly from a newer build.
    Any,
}

/// Shape checks shared by API definitions, config seeds, and loaded rosters.
fn check_shape(
    name: &str,
    area: &str,
    prompt: &str,
    members: &[(&str, &str)],
    kinds: Kinds,
) -> Result<(), Rejection> {
    check_name("Role name", name)?;
    if RESERVED_ROLE_NAMES.contains(&name) {
        return Err(invalid(format!("Role name {name:?} is reserved")));
    }
    if area.len() > MAX_AREA_BYTES {
        return Err(invalid(format!(
            "area allows at most {MAX_AREA_BYTES} bytes"
        )));
    }
    if prompt.len() > MAX_ROLE_PROMPT_BYTES {
        return Err(invalid(format!(
            "prompt allows at most {MAX_ROLE_PROMPT_BYTES} bytes"
        )));
    }
    if members.is_empty() || members.len() > MAX_MEMBERS {
        return Err(invalid(format!("A role needs 1 to {MAX_MEMBERS} members")));
    }
    for (index, (member, kind)) in members.iter().enumerate() {
        check_name("Member name", member)?;
        if *member == crate::app::polaris::NAME {
            return Err(invalid("The polaris name is reserved for the lead agent"));
        }
        if members[..index].iter().any(|(other, _)| other == member) {
            return Err(invalid(format!("Member name {member:?} is used twice")));
        }
        if kinds == Kinds::Supported {
            check_kind(kind)?;
        }
    }
    Ok(())
}

/// Validates `spec` against the worker allowlist; existing observed state of
/// members with the same name and kind is kept.
pub(crate) fn validate(
    spec: RoleSpec,
    policy: &WorkerPolicy,
    previous: Option<&Role>,
) -> Result<Role, Rejection> {
    let names: Vec<(&str, &str)> = spec
        .members
        .iter()
        .map(|member| (member.name.as_str(), member.kind.as_str()))
        .collect();
    check_shape(
        &spec.name,
        &spec.area,
        &spec.prompt,
        &names,
        Kinds::Supported,
    )?;
    let mut members = Vec::with_capacity(spec.members.len());
    for member in spec.members {
        let launch = policy.admit_launch(
            &member.kind,
            member.model.clone(),
            member.permission_mode.as_deref(),
        )?;
        let kept = previous
            .and_then(|role| role.members.iter().find(|old| old.name == member.name))
            .filter(|old| old.agent() == crate::detect::parse_agent_label(&member.kind));
        members.push(Member {
            name: member.name,
            kind: crate::detect::agent_label(
                crate::detect::parse_agent_label(&member.kind).unwrap_or(Agent::Claude),
            )
            .to_owned(),
            model: member.model,
            permission_mode: launch
                .permission_mode
                .filter(|_| member.permission_mode.is_some())
                .map(str::to_owned),
            terminal_id: kept.and_then(|old| old.terminal_id.clone()),
            session: kept.and_then(|old| old.session.clone()),
            extra: kept.map(|old| old.extra.clone()).unwrap_or_default(),
        });
    }
    Ok(Role {
        name: spec.name,
        area: spec.area,
        prompt: spec.prompt,
        members,
        tab: previous.and_then(|role| role.tab.clone()),
        extra: previous.map(|role| role.extra.clone()).unwrap_or_default(),
    })
}

/// The launch model and permission mode for a member under the current policy.
pub(crate) fn admit_member(
    policy: &WorkerPolicy,
    member: &Member,
) -> Result<WorkerLaunch, Rejection> {
    policy.admit_launch(
        &member.kind,
        member.model.clone(),
        member.permission_mode.as_deref(),
    )
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct Roster {
    pub(crate) roles: Vec<Role>,
    /// Top-level fields of the roster file from a newer build.
    pub(crate) extra: Map<String, Value>,
}

impl Roster {
    pub(crate) fn role(&self, name: &str) -> Option<&Role> {
        self.roles.iter().find(|role| role.name == name)
    }

    pub(crate) fn role_index(&self, name: &str) -> Option<usize> {
        self.roles.iter().position(|role| role.name == name)
    }

    /// The role and member named `name`.
    pub(crate) fn member(&self, name: &str) -> Option<(&Role, &Member)> {
        self.roles.iter().find_map(|role| {
            role.members
                .iter()
                .find(|member| member.name == name)
                .map(|member| (role, member))
        })
    }

    pub(crate) fn member_mut(&mut self, name: &str) -> Option<&mut Member> {
        self.roles
            .iter_mut()
            .flat_map(|role| role.members.iter_mut())
            .find(|member| member.name == name)
    }

    /// Inserts or replaces `role`; member names stay unique across roles.
    pub(crate) fn upsert(&mut self, role: Role) -> Result<(), Rejection> {
        for member in &role.members {
            if let Some((other, _)) = self
                .member(&member.name)
                .filter(|(other, _)| other.name != role.name)
            {
                return Err(invalid(format!(
                    "Member name {:?} already belongs to role {}",
                    member.name, other.name
                )));
            }
            if self.role(&member.name).is_some() {
                return Err(invalid(format!(
                    "Member name {:?} is already a role name",
                    member.name
                )));
            }
        }
        if self
            .member(&role.name)
            .is_some_and(|(owner, _)| owner.name != role.name)
        {
            return Err(invalid(format!(
                "Role name {:?} is already a member name",
                role.name
            )));
        }
        match self.role_index(&role.name) {
            Some(index) => self.roles[index] = role,
            None if self.roles.len() >= MAX_ROLES => {
                return Err(invalid(format!(
                    "The team allows at most {MAX_ROLES} roles"
                )))
            }
            None => self.roles.push(role),
        }
        Ok(())
    }

    pub(super) fn check_loaded(&self) -> Result<(), String> {
        if self.roles.len() > MAX_ROLES {
            return Err(format!("more than {MAX_ROLES} roles"));
        }
        let mut check = Roster::default();
        for role in &self.roles {
            let names: Vec<(&str, &str)> = role
                .members
                .iter()
                .map(|member| (member.name.as_str(), member.kind.as_str()))
                .collect();
            check_shape(&role.name, &role.area, &role.prompt, &names, Kinds::Any)
                .map_err(|(_, message)| message)?;
            if check.role(&role.name).is_some() {
                return Err(format!("role {} is defined twice", role.name));
            }
            check.upsert(role.clone()).map_err(|(_, message)| message)?;
        }
        Ok(())
    }
}

/// Seeds a roster from `[[polaris.team]]`; any invalid entry rejects the seed.
pub(crate) fn seed(
    configs: &[crate::config::PolarisTeamRoleConfig],
    policy: &WorkerPolicy,
) -> Result<Roster, String> {
    let mut roster = Roster::default();
    for config in configs {
        let role = validate(RoleSpec::from(config), policy, None)
            .map_err(|(_, message)| format!("[[polaris.team]] {}: {message}", config.name))?;
        if roster.role(&role.name).is_some() {
            return Err(format!("[[polaris.team]] {} is defined twice", role.name));
        }
        roster
            .upsert(role)
            .map_err(|(_, message)| format!("[[polaris.team]] {}: {message}", config.name))?;
    }
    Ok(roster)
}
