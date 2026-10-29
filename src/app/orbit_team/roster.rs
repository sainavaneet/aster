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
    /// Declared by the role's `[[polaris.team]]` entry rather than added by the
    /// session; such members follow config and are released when config drops them.
    #[serde(default, skip_serializing_if = "is_false")]
    pub(crate) pinned: bool,
    /// Fields from a newer build, kept so they survive this build's saves.
    #[serde(flatten)]
    pub(crate) extra: Map<String, Value>,
}

fn is_false(value: &bool) -> bool {
    !*value
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
    /// Defined in `[[polaris.team]]`: present in every session, refreshed from
    /// config, and never removed by `orbit_team_remove`.
    #[serde(default, skip_serializing_if = "is_false")]
    pub(crate) pinned: bool,
    /// A pinned role whose prompt this session replaced; reloads keep it.
    #[serde(default, skip_serializing_if = "is_false")]
    pub(crate) overridden: bool,
    /// Agent kinds a task relayed within the role passes through, from
    /// `orbit_team_define`. Empty for a pinned role that follows its config
    /// `pipeline`; see `TeamState::pipeline`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub(crate) pipeline: Vec<String>,
    /// A pinned role whose config pipeline this session replaced; reloads keep it.
    #[serde(default, skip_serializing_if = "is_false")]
    pub(crate) pipeline_overridden: bool,
    /// A task was given to a member of the role in this session. An unused
    /// role whose config no longer applies to the session's project is retired.
    #[serde(default, skip_serializing_if = "is_false")]
    pub(crate) used: bool,
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
    /// Kinds a relayed task passes through; empty for none.
    pub(crate) pipeline: Vec<String>,
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
            // The config pipeline is read from config, not copied to the role.
            pipeline: Vec::new(),
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
            pipeline: params.pipeline.unwrap_or_default(),
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

/// A role pipeline lists supported agent kinds, at most one per member slot.
pub(crate) fn check_pipeline(pipeline: &[String]) -> Result<(), Rejection> {
    if pipeline.len() > MAX_MEMBERS {
        return Err(invalid(format!(
            "pipeline allows at most {MAX_MEMBERS} kinds"
        )));
    }
    pipeline.iter().try_for_each(|kind| check_kind(kind))
}

/// `kind` in its canonical spelling, for a kind `check_kind` accepted.
fn canonical_kind(kind: &str) -> String {
    crate::detect::agent_label(crate::detect::parse_agent_label(kind).unwrap_or(Agent::Claude))
        .to_owned()
}

/// A session-supplied pipeline: kinds the worker allowlist admits, each the
/// kind of a member of the role, in canonical spelling.
fn check_role_pipeline(
    pipeline: &[String],
    members: &[Member],
    policy: &WorkerPolicy,
) -> Result<Vec<String>, Rejection> {
    check_pipeline(pipeline)?;
    pipeline
        .iter()
        .map(|kind| {
            policy.admit(kind, None)?;
            let agent = crate::detect::parse_agent_label(kind);
            if !members.iter().any(|member| member.agent() == agent) {
                return Err(invalid(format!(
                    "pipeline kind {kind:?} is not the kind of any member of the role"
                )));
            }
            Ok(canonical_kind(kind))
        })
        .collect()
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
            pinned: false,
            extra: kept.map(|old| old.extra.clone()).unwrap_or_default(),
        });
    }
    let pipeline = check_role_pipeline(&spec.pipeline, &members, policy)?;
    Ok(Role {
        name: spec.name,
        area: spec.area,
        prompt: spec.prompt,
        members,
        tab: previous.and_then(|role| role.tab.clone()),
        pinned: false,
        overridden: false,
        pipeline,
        pipeline_overridden: false,
        used: previous.is_some_and(|role| role.used),
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

impl From<&Member> for MemberSpec {
    fn from(member: &Member) -> Self {
        Self {
            name: member.name.clone(),
            kind: member.kind.clone(),
            model: member.model.clone(),
            permission_mode: member.permission_mode.clone(),
        }
    }
}

/// The pinned role `config` merged onto `previous`: area and the config
/// members' kinds, models, and permission modes come from config; observed
/// state and the tab binding are kept; `extras` (members the session added) are
/// appended; `prompt` and a non-empty `pipeline` replace the config prompt and
/// pipeline for this session only.
pub(crate) fn pinned_role(
    config: &crate::config::PolarisTeamRoleConfig,
    previous: Option<&Role>,
    extras: Vec<MemberSpec>,
    prompt: Option<String>,
    pipeline: Option<Vec<String>>,
    policy: &WorkerPolicy,
) -> Result<Role, Rejection> {
    check_pipeline(&config.pipeline)?;
    if let Some(integrator) = &config.integrator {
        check_name("Integrator", integrator)?;
    }
    let configured = |name: &str| config.members.iter().any(|member| member.name == name);
    let mut spec = RoleSpec::from(config);
    spec.members
        .extend(extras.into_iter().filter(|extra| !configured(&extra.name)));
    if let Some(prompt) = prompt {
        spec.prompt = prompt;
    }
    if let Some(pipeline) = pipeline {
        spec.pipeline = pipeline;
    }
    let role = validate(spec, policy, previous)?;
    let configured_pipeline: Vec<String> = config
        .pipeline
        .iter()
        .map(|kind| canonical_kind(kind))
        .collect();
    let pipeline_overridden = !role.pipeline.is_empty() && role.pipeline != configured_pipeline;
    let members = role
        .members
        .into_iter()
        .map(|member| Member {
            pinned: configured(&member.name),
            ..member
        })
        .collect();
    Ok(Role {
        pinned: true,
        overridden: role.prompt != config.prompt,
        pipeline: if pipeline_overridden {
            role.pipeline
        } else {
            Vec::new()
        },
        pipeline_overridden,
        members,
        ..role
    })
}

/// The result of pinning config roles onto a session roster.
#[derive(Debug, Default)]
pub(crate) struct Pinned {
    pub(crate) roster: Roster,
    /// Members no longer in the roster; their agents keep running unsupervised.
    pub(crate) released: Vec<String>,
    /// Config roles that could not be pinned, as diagnostics.
    pub(crate) errors: Vec<String>,
}

/// The pinned role for `config` on a reload or session start: the session's
/// prompt and pipeline overrides and added members survive; config members
/// dropped from config, and added members beyond `MAX_MEMBERS`, are released.
/// A pipeline override that no longer fits the role's members is dropped.
fn repin(
    config: &crate::config::PolarisTeamRoleConfig,
    previous: Option<&Role>,
    policy: &WorkerPolicy,
) -> Result<Role, Rejection> {
    let room = MAX_MEMBERS.saturating_sub(config.members.len());
    let extras: Vec<MemberSpec> = previous
        .iter()
        .flat_map(|role| &role.members)
        .filter(|member| !member.pinned)
        .filter(|member| !config.members.iter().any(|other| other.name == member.name))
        .take(room)
        .map(MemberSpec::from)
        .collect();
    let prompt = previous
        .filter(|role| role.pinned && role.overridden)
        .map(|role| role.prompt.clone());
    let pipeline = previous
        .filter(|role| role.pinned && role.pipeline_overridden)
        .map(|role| role.pipeline.clone());
    let keeps_pipeline = pipeline.is_some();
    pinned_role(
        config,
        previous,
        extras.clone(),
        prompt.clone(),
        pipeline,
        policy,
    )
    .or_else(|error| {
        if !keeps_pipeline {
            return Err(error);
        }
        tracing::warn!(role = %config.name, "orbit role pipeline override no longer fits; config pipeline used");
        pinned_role(config, previous, extras, prompt, None, policy)
    })
}

/// `current` with every `[[polaris.team]]` role present and up to date, in
/// config order ahead of the session's own roles. Roles the session defined are
/// untouched; a role no longer in config stays, unpinned. A config role that
/// cannot be pinned is reported and its previous state, if any, kept.
pub(crate) fn pin(
    current: &Roster,
    configs: &[crate::config::PolarisTeamRoleConfig],
    policy: &WorkerPolicy,
) -> Pinned {
    let configured = |name: &str| configs.iter().any(|config| config.name == name);
    let session: Vec<Role> = current
        .roles
        .iter()
        .filter(|role| !configured(&role.name))
        .map(|role| Role {
            pinned: false,
            overridden: false,
            // A session pipeline override stays the role's pipeline; a role
            // that followed its config pipeline has none of its own.
            pipeline_overridden: false,
            members: role
                .members
                .iter()
                .map(|member| Member {
                    pinned: false,
                    ..member.clone()
                })
                .collect(),
            ..role.clone()
        })
        .collect();
    let mut errors = Vec::new();
    let mut pinned: Vec<Role> = Vec::new();
    for config in configs {
        if pinned.iter().any(|role| role.name == config.name) {
            errors.push(format!("[[polaris.team]] {} is defined twice", config.name));
            continue;
        }
        let previous = current.role(&config.name);
        let candidate = repin(config, previous, policy).and_then(|role| {
            if previous.is_none() && session.len() + pinned.len() >= MAX_ROLES {
                return Err(invalid(format!(
                    "The team allows at most {MAX_ROLES} roles"
                )));
            }
            let mut check = Roster {
                roles: session.iter().chain(&pinned).cloned().collect(),
                extra: Map::new(),
            };
            check.upsert(role.clone()).map(|()| role)
        });
        match candidate {
            Ok(role) => pinned.push(role),
            Err((_, message)) => {
                errors.push(format!("[[polaris.team]] {}: {message}", config.name));
                pinned.extend(previous.cloned());
            }
        }
    }
    let roles: Vec<Role> = pinned.into_iter().chain(session).collect();
    let released = current
        .roles
        .iter()
        .flat_map(|role| &role.members)
        .filter(|old| {
            !roles
                .iter()
                .flat_map(|role| &role.members)
                .any(|member| member.name == old.name)
        })
        .map(|old| old.name.clone())
        .collect();
    Pinned {
        roster: Roster {
            roles,
            extra: current.extra.clone(),
        },
        released,
        errors,
    }
}
