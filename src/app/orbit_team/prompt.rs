//! Role instructions and launch arguments for team members.
//!
//! Each kind receives the instructions as part of its launch, so a relaunch or a
//! native resume always carries them:
//! - Claude: `--append-system-prompt <text>` (the same mechanism Polaris uses),
//!   after `--allowedTools` with the rule that pre-allows the report command.
//! - Codex: `-c developer_instructions=<TOML string>`, Codex's config override for
//!   extra developer instructions (`codex --help`: `-c, --config <key=value>`).
use crate::agent_resume::AgentResumePlan;
use crate::app::orbit_worker_policy::WorkerLaunch;
use crate::detect::Agent;

use super::roster::{Member, MemberSession, Role};

/// Where a later milestone keeps each role's notes and handoffs.
pub(crate) const NOTES_PLACEHOLDER: &str =
    "no notes directory yet (a later Aster milestone adds one per role)";

/// Agent CLI arguments must not carry control characters: collapse whitespace.
fn single_line(text: &str) -> String {
    text.split(|ch: char| ch.is_control() || ch.is_whitespace())
        .filter(|word| !word.is_empty())
        .collect::<Vec<_>>()
        .join(" ")
}

/// The role text a member is launched with, on one line.
pub(crate) fn role_instructions(role: &Role, member: &Member) -> String {
    let members = role
        .members
        .iter()
        .map(|other| format!("{} ({})", other.name, other.kind))
        .collect::<Vec<_>>()
        .join(", ");
    let area = if role.area.trim().is_empty() {
        "not described".to_owned()
    } else {
        single_line(&role.area)
    };
    let mut text = format!(
        "You are {}, a member of the {} role on Aster's Orbit team. Polaris, the lead, routes work to your role; you never exit and are resumed with this conversation after a restart. Area you own: {area}. Members of this role: {members}.",
        member.name,
        role.label(),
    );
    if !role.prompt.trim().is_empty() {
        text.push_str(" Role instructions: ");
        text.push_str(&single_line(&role.prompt));
    }
    text.push_str(&format!(
        " When the user types in your pane, answer questions about your area directly. When Polaris sends an [Aster assignment], do it and report with the supplied command exactly as instructed. Your notes and handoffs: {NOTES_PLACEHOLDER}."
    ));
    text
}

/// A TOML basic string, accepted by Codex `-c key=value`.
fn toml_string(text: &str) -> String {
    toml::Value::String(text.to_owned()).to_string()
}

/// The arguments that hand `text` to the agent at launch. Claude members also
/// get the rule that pre-allows their report command; it precedes another
/// option, so the variadic `--allowedTools` takes exactly one rule.
fn instruction_args(agent: Agent, text: &str) -> Vec<String> {
    match agent {
        Agent::Claude => {
            let mut args = crate::app::orbit_notifications::report_allow_args(agent);
            args.extend(["--append-system-prompt".into(), text.into()]);
            args
        }
        Agent::Codex => vec![
            "-c".into(),
            format!("developer_instructions={}", toml_string(text)),
        ],
        _ => Vec::new(),
    }
}

/// The native resume arguments for `session`, without the executable, when it
/// belongs to `agent`.
pub(crate) fn resume_args(agent: Agent, session: &MemberSession) -> Option<Vec<String>> {
    let persisted = session.to_persisted()?;
    if crate::detect::parse_agent_label(&persisted.agent) != Some(agent) {
        return None;
    }
    let plan =
        crate::agent_resume::plan(&persisted.source, &persisted.agent, &persisted.session_ref)?;
    Some(plan.argv.into_iter().skip(1).collect())
}

/// Arguments after the agent executable: native resume first (Codex's `resume`
/// is a subcommand), then model and permission mode, then the role text.
pub(crate) fn launch_args(
    role: &Role,
    member: &Member,
    launch: &WorkerLaunch,
    resume: bool,
) -> Vec<String> {
    let Some(agent) = member.agent() else {
        return Vec::new();
    };
    let mut args = member
        .session
        .as_ref()
        .filter(|_| resume)
        .and_then(|session| resume_args(agent, session))
        .unwrap_or_default();
    args.extend(launch.args());
    args.extend(instruction_args(agent, &role_instructions(role, member)));
    args
}

/// Adds the member's model, permission mode, and role text to a restored
/// pane's native resume plan, so a restart keeps them. Returns whether the plan
/// was changed; a plan for another agent kind is left alone.
pub(crate) fn augment_resume_plan(
    plan: &mut AgentResumePlan,
    role: &Role,
    member: &Member,
    launch: &WorkerLaunch,
) -> bool {
    let Some(agent) = member.agent() else {
        return false;
    };
    if crate::detect::parse_agent_label(&plan.agent) != Some(agent)
        || plan.argv.iter().any(|arg| arg == "--append-system-prompt")
        || plan
            .argv
            .iter()
            .any(|arg| arg.starts_with("developer_instructions="))
    {
        return false;
    }
    plan.argv.extend(launch.args());
    plan.argv
        .extend(instruction_args(agent, &role_instructions(role, member)));
    true
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent_resume::AgentSessionRefKind;

    /// Exactly the report command's prefix, and no other rule.
    fn report_rule() -> String {
        let executable = std::env::current_exe().unwrap();
        format!("Bash({} orbit agent-report:*)", executable.display())
    }

    fn role() -> Role {
        Role {
            name: "reviewer".into(),
            area: "Code review\nof the core".into(),
            prompt: "Be strict.\nCite lines.".into(),
            members: vec![
                Member {
                    name: "rc".into(),
                    kind: "claude".into(),
                    model: Some("opus".into()),
                    permission_mode: None,
                    terminal_id: None,
                    session: Some(MemberSession {
                        source: "aster:claude".into(),
                        agent: "claude".into(),
                        kind: AgentSessionRefKind::Id,
                        value: "s-1".into(),
                    }),
                    pinned: false,
                    extra: Default::default(),
                },
                Member {
                    name: "rx".into(),
                    kind: "codex".into(),
                    model: None,
                    permission_mode: None,
                    terminal_id: None,
                    session: Some(MemberSession {
                        source: "aster:codex".into(),
                        agent: "codex".into(),
                        kind: AgentSessionRefKind::Id,
                        value: "c-1".into(),
                    }),
                    pinned: false,
                    extra: Default::default(),
                },
            ],
            tab: None,
            pinned: false,
            overridden: false,
            pipeline: Vec::new(),
            pipeline_overridden: false,
            used: false,
            extra: Default::default(),
        }
    }

    #[test]
    fn role_instructions_name_role_area_members_and_reporting_on_one_line() {
        let role = role();
        let text = role_instructions(&role, &role.members[0]);
        for expected in [
            "You are rc",
            "Reviewer role",
            "Code review of the core",
            "rc (claude), rx (codex)",
            "Be strict. Cite lines.",
            "answer questions about your area",
            "report with the supplied command",
            NOTES_PLACEHOLDER,
        ] {
            assert!(text.contains(expected), "{expected} missing from {text}");
        }
        assert!(!text.chars().any(char::is_control));
    }

    #[test]
    fn claude_resumes_then_appends_the_role_prompt() {
        let role = role();
        let launch = WorkerLaunch {
            model: Some("opus".into()),
            permission_mode: Some("manual"),
        };
        let args = launch_args(&role, &role.members[0], &launch, true);
        assert_eq!(
            &args[..6],
            [
                "--resume",
                "s-1",
                "--model",
                "opus",
                "--permission-mode",
                "manual"
            ]
        );
        assert_eq!(args[6], "--allowedTools");
        assert_eq!(args[7], report_rule());
        assert_eq!(args[8], "--append-system-prompt");
        assert!(args[9].contains("Reviewer role"));
        assert_eq!(args.len(), 10);
        let fresh = launch_args(&role, &role.members[0], &launch, false);
        assert_eq!(fresh[0], "--model");
    }

    #[test]
    fn codex_resumes_with_the_subcommand_and_developer_instructions() {
        let role = role();
        let args = launch_args(&role, &role.members[1], &WorkerLaunch::default(), true);
        assert_eq!(&args[..3], ["resume", "c-1", "-c"]);
        let value = args[3].strip_prefix("developer_instructions=").unwrap();
        let parsed: toml::Value = toml::from_str(&format!("v = {value}")).unwrap();
        assert!(parsed["v"].as_str().unwrap().contains("You are rx"));
        assert!(!args.iter().any(|arg| arg == "--allowedTools"));
        assert_eq!(args.len(), 4);
    }

    #[test]
    fn a_session_of_another_kind_is_never_resumed() {
        let mut role = role();
        role.members[1].session = role.members[0].session.clone();
        let args = launch_args(&role, &role.members[1], &WorkerLaunch::default(), true);
        assert_eq!(args[0], "-c");
    }

    #[test]
    fn restored_resume_plans_gain_role_text_once_and_only_for_the_same_kind() {
        let role = role();
        let mut plan = AgentResumePlan {
            agent: "claude".into(),
            argv: vec!["claude".into(), "--resume".into(), "s-1".into()],
            dedupe_key: "k".into(),
        };
        let launch = WorkerLaunch {
            model: Some("opus".into()),
            permission_mode: None,
        };
        assert!(augment_resume_plan(
            &mut plan,
            &role,
            &role.members[0],
            &launch
        ));
        assert_eq!(&plan.argv[3..5], ["--model", "opus"]);
        assert_eq!(&plan.argv[5..7], ["--allowedTools", report_rule().as_str()]);
        assert!(!augment_resume_plan(
            &mut plan,
            &role,
            &role.members[0],
            &launch
        ));
        let mut codex = plan.clone();
        codex.agent = "codex".into();
        assert!(!augment_resume_plan(
            &mut codex,
            &role,
            &role.members[0],
            &launch
        ));
    }
}
