//! Orbit team roster operations: persistent roles that each own a Polaris tab.
use serde::{Deserialize, Serialize};

/// One member of a role: an agent that lives in the role's tab and is resumed
/// rather than relaunched after a restart.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct OrbitTeamMemberParams {
    /// Agent name, unique across the team, also used as the pane title.
    pub name: String,
    /// Agent kind: claude or codex, within `[polaris.workers] kinds`.
    pub kind: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    /// Claude `--permission-mode`: manual, acceptEdits, auto, plan, or dontAsk.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub permission_mode: Option<String>,
}

/// Creates or replaces a role. The first definition creates the role's tab
/// without changing focus; members are started by the team supervisor.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct OrbitTeamDefineParams {
    pub name: String,
    /// What the role owns, in free text.
    pub area: String,
    /// Role instructions given to every member, at most 4096 bytes.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub prompt: Option<String>,
    pub members: Vec<OrbitTeamMemberParams>,
    /// Agent kinds a task relayed within the role passes through, for example
    /// ["claude", "codex"]: each must be the kind of a member and allowed by
    /// `[polaris.workers]`. For a pinned role it replaces the config pipeline
    /// for this session only; an empty list returns to the config pipeline.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pipeline: Option<Vec<String>>,
}

/// Retires a role. Refused while a member has an assignment awaiting its
/// report unless `force` cancels those assignments.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct OrbitTeamRemoveParams {
    pub name: String,
    /// Close the role's tab, ending its members. Otherwise the tab and its
    /// agents stay as ordinary, unsupervised panes.
    #[serde(default, skip_serializing_if = "super::is_false")]
    pub stop_members: bool,
    #[serde(default, skip_serializing_if = "super::is_false")]
    pub force: bool,
}

/// `orbit.inspect` target: the bound Orbit tab, or a role's tab.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema, Default)]
pub struct OrbitInspectParams {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub role: Option<String>,
}
