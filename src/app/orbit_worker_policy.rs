//! The `[polaris.workers]` allowlist: which agent kinds, models, and permission
//! modes Orbit may start, and which of their executables are installed.
use std::collections::BTreeMap;
use std::ffi::OsStr;
use std::time::{Duration, Instant};

use serde::Serialize;
use serde_json::Value;

use crate::config::{parse_worker_permission_mode, PermissionModeError, PolarisWorkersConfig};
use crate::detect::{agent_label, interactive_agent_executable, parse_agent_label, Agent};

use super::App;

/// How long a PATH scan for worker executables is reused by `orbit.activity`.
const INSTALL_SCAN_TTL: Duration = Duration::from_secs(30);
pub(crate) const NOT_ALLOWED_CODE: &str = "orbit_worker_not_allowed";
const MODEL_UNSUPPORTED: &str = "A model override currently supports Claude and Codex workers";
const MODEL_INVALID: &str =
    "Model must be a non-empty value without control characters that does not start with '-'";
pub(crate) const PERMISSION_MODE_REFUSED: &str = "orbit_permission_mode_refused";
pub(crate) const PERMISSION_MODE_UNSUPPORTED: &str = "orbit_permission_mode_unsupported";
const PERMISSION_MODE_KINDS: &str = "permission_mode currently supports Claude workers only; Codex approval flags have no matching mode";

#[derive(Debug, Clone, PartialEq, Eq)]
struct KindPolicy {
    agent: Agent,
    models: Vec<String>,
    default_model: Option<String>,
}

/// Normalized worker allowlist; config keys may use any kind alias.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct WorkerPolicy {
    kinds: Vec<KindPolicy>,
    /// Live Orbit worker limit, excluding the lead; 0 is unlimited.
    pub(crate) max_concurrent: usize,
    /// Claude permission mode applied when a start names none.
    permission_mode: Option<&'static str>,
}

/// What an admitted start launches with: the model and Claude permission mode,
/// requested or configured.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct WorkerLaunch {
    pub(crate) model: Option<String>,
    pub(crate) permission_mode: Option<&'static str>,
}

impl WorkerLaunch {
    /// Agent CLI arguments: `--model` and `--permission-mode` when set.
    pub(crate) fn args(&self) -> Vec<String> {
        let mut args = Vec::new();
        if let Some(model) = &self.model {
            args.extend(["--model".into(), model.clone()]);
        }
        if let Some(mode) = self.permission_mode {
            args.extend(["--permission-mode".into(), mode.into()]);
        }
        args
    }
}

/// A rejected start: the API error code and message.
pub(crate) type Rejection = (&'static str, String);

#[derive(Serialize)]
struct KindActivity {
    kind: &'static str,
    executable: &'static str,
    installed: bool,
    models: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    default_model: Option<String>,
}

pub(crate) struct InstallScan {
    at: Instant,
    installed: Vec<bool>,
}

fn supports_model(agent: Agent) -> bool {
    matches!(agent, Agent::Claude | Agent::Codex)
}

fn for_agent<T>(map: &BTreeMap<String, T>, agent: Agent) -> Option<&T> {
    map.iter()
        .find(|(key, _)| parse_agent_label(key) == Some(agent))
        .map(|(_, value)| value)
}

fn list_or(values: impl IntoIterator<Item = impl AsRef<str>>, empty: &str) -> String {
    let joined = values
        .into_iter()
        .map(|value| value.as_ref().to_owned())
        .collect::<Vec<_>>()
        .join(", ");
    if joined.is_empty() {
        empty.into()
    } else {
        joined
    }
}

impl Default for WorkerPolicy {
    fn default() -> Self {
        Self::from_config(&PolarisWorkersConfig::default())
    }
}

impl WorkerPolicy {
    pub(crate) fn from_config(config: &PolarisWorkersConfig) -> Self {
        let mut agents: Vec<Agent> = Vec::new();
        for agent in config
            .kinds
            .iter()
            .filter_map(|kind| parse_agent_label(kind))
        {
            if !agents.contains(&agent) {
                agents.push(agent);
            }
        }
        let kinds = agents
            .into_iter()
            .map(|agent| {
                let model_capable = supports_model(agent);
                KindPolicy {
                    agent,
                    models: for_agent(&config.models, agent)
                        .filter(|_| model_capable)
                        .cloned()
                        .unwrap_or_default(),
                    default_model: for_agent(&config.defaults, agent)
                        .filter(|model| model_capable && !model.trim().is_empty())
                        .cloned(),
                }
            })
            .collect();
        Self {
            kinds,
            max_concurrent: config.max_concurrent as usize,
            // Load already rejected anything else; an invalid value is ignored.
            permission_mode: config
                .permission_mode
                .as_deref()
                .and_then(|mode| parse_worker_permission_mode(mode).ok()),
        }
    }

    fn allowed_kinds(&self) -> String {
        list_or(
            self.kinds.iter().map(|kind| agent_label(kind.agent)),
            "none",
        )
    }

    /// Checks `kind` and `model` against the allowlist and returns the model
    /// to launch with, applying the kind's configured default.
    pub(crate) fn admit(
        &self,
        kind: &str,
        model: Option<String>,
    ) -> Result<Option<String>, Rejection> {
        let Some(policy) = parse_agent_label(kind)
            .and_then(|agent| self.kinds.iter().find(|policy| policy.agent == agent))
        else {
            return Err((
                NOT_ALLOWED_CODE,
                format!(
                    "Worker kind {kind:?} is not allowed; allowed kinds: {}",
                    self.allowed_kinds()
                ),
            ));
        };
        let Some(model) = model else {
            return Ok(policy.default_model.clone());
        };
        if !supports_model(policy.agent) {
            return Err(("invalid_params", MODEL_UNSUPPORTED.into()));
        }
        // A leading '-' would reach the agent CLI as an option, even with no allowlist.
        if model.trim().is_empty()
            || model.trim_start().starts_with('-')
            || model.chars().any(char::is_control)
        {
            return Err(("invalid_params", MODEL_INVALID.into()));
        }
        if !policy.models.is_empty() && !policy.models.contains(&model) {
            return Err((
                NOT_ALLOWED_CODE,
                format!(
                    "Model {model:?} is not allowed for {} workers; allowed models: {}",
                    agent_label(policy.agent),
                    list_or(&policy.models, "any")
                ),
            ));
        }
        Ok(Some(model))
    }

    /// Checks `kind`, `model`, and `permission_mode` and returns what to launch
    /// with, applying the configured defaults.
    pub(crate) fn admit_launch(
        &self,
        kind: &str,
        model: Option<String>,
        permission_mode: Option<&str>,
    ) -> Result<WorkerLaunch, Rejection> {
        let model = self.admit(kind, model)?;
        let claude = parse_agent_label(kind) == Some(Agent::Claude);
        let permission_mode = match permission_mode {
            None => self.permission_mode.filter(|_| claude),
            Some(requested) => match parse_worker_permission_mode(requested) {
                Err(error @ PermissionModeError::Refused) => {
                    return Err((PERMISSION_MODE_REFUSED, error.message(requested)))
                }
                _ if !claude => {
                    return Err((PERMISSION_MODE_UNSUPPORTED, PERMISSION_MODE_KINDS.into()))
                }
                Err(error) => return Err(("invalid_params", error.message(requested))),
                Ok(mode) => Some(mode),
            },
        };
        Ok(WorkerLaunch {
            model,
            permission_mode,
        })
    }

    fn scan_installed(&self, path: Option<&OsStr>) -> Vec<bool> {
        self.kinds
            .iter()
            .map(|kind| {
                path.is_some_and(|path| {
                    crate::integration::command_available_in(
                        path,
                        interactive_agent_executable(kind.agent),
                    )
                })
            })
            .collect()
    }

    fn activity(&self, installed: &[bool]) -> Value {
        let kinds: Vec<_> = self
            .kinds
            .iter()
            .zip(installed)
            .map(|(kind, &installed)| KindActivity {
                kind: agent_label(kind.agent),
                executable: interactive_agent_executable(kind.agent),
                installed,
                models: kind.models.clone(),
                default_model: kind.default_model.clone(),
            })
            .collect();
        serde_json::to_value(kinds).unwrap_or_default()
    }
}

impl App {
    /// Applies a loaded `[polaris.workers]` section; a change forgets the PATH scan.
    pub(crate) fn set_orbit_worker_policy(&mut self, config: &PolarisWorkersConfig) {
        let policy = WorkerPolicy::from_config(config);
        if policy != self.orbit_worker_policy {
            self.orbit_worker_policy = policy;
            self.orbit_worker_scan = None;
        }
    }

    /// `orbit.activity` `kinds`. Scans the server's PATH at most every
    /// `INSTALL_SCAN_TTL`; only reached from the activity request.
    pub(crate) fn orbit_worker_kinds(&mut self, now: Instant) -> Value {
        let stale = self
            .orbit_worker_scan
            .as_ref()
            .is_none_or(|scan| now.saturating_duration_since(scan.at) >= INSTALL_SCAN_TTL);
        if stale {
            let path = std::env::var_os("PATH");
            self.orbit_worker_scan = Some(InstallScan {
                at: now,
                installed: self.orbit_worker_policy.scan_installed(path.as_deref()),
            });
        }
        let installed = self
            .orbit_worker_scan
            .as_ref()
            .map(|scan| scan.installed.as_slice())
            .unwrap_or_default();
        self.orbit_worker_policy.activity(installed)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn policy(toml: &str) -> WorkerPolicy {
        WorkerPolicy::from_config(&toml::from_str(toml).unwrap())
    }

    #[test]
    fn default_policy_allows_claude_and_codex_with_any_model() {
        let policy = WorkerPolicy::default();
        assert_eq!(policy.max_concurrent, 4);
        assert_eq!(policy.admit("claude", None), Ok(None));
        assert_eq!(
            policy.admit("codex", Some("gpt-5-codex".into())),
            Ok(Some("gpt-5-codex".into()))
        );
        let (code, message) = policy.admit("gemini", None).unwrap_err();
        assert_eq!(code, NOT_ALLOWED_CODE);
        assert_eq!(
            message,
            "Worker kind \"gemini\" is not allowed; allowed kinds: claude, codex"
        );
    }

    #[test]
    fn model_allowlist_and_defaults_are_applied_per_kind() {
        let policy = policy(
            "kinds = [\"claude-code\", \"codex\", \"gemini\"]\n\
             [models]\nclaude = [\"fable\", \"opus\", \"sonnet\"]\ncodex = []\n\
             [defaults]\nclaude = \"fable\"\ngemini = \"ignored\"\n",
        );
        assert_eq!(policy.admit("claude", None), Ok(Some("fable".into())));
        assert_eq!(
            policy.admit("claude", Some("opus".into())),
            Ok(Some("opus".into()))
        );
        let (code, message) = policy.admit("claude", Some("haiku".into())).unwrap_err();
        assert_eq!(code, NOT_ALLOWED_CODE);
        assert_eq!(
            message,
            "Model \"haiku\" is not allowed for claude workers; allowed models: fable, opus, sonnet"
        );
        assert_eq!(
            policy.admit("codex", Some("anything".into())),
            Ok(Some("anything".into()))
        );
        // Kinds without model overrides keep today's rejection and ignore defaults.
        assert_eq!(policy.admit("gemini", None), Ok(None));
        assert_eq!(
            policy.admit("gemini", Some("pro".into())),
            Err(("invalid_params", MODEL_UNSUPPORTED.into()))
        );
        assert_eq!(
            policy.admit("claude", Some(" ".into())),
            Err(("invalid_params", MODEL_INVALID.into()))
        );
    }

    #[test]
    fn option_like_models_are_rejected_even_without_an_allowlist() {
        let policy = WorkerPolicy::default();
        for model in ["--dangerously-skip-permissions", "-m", " -x"] {
            assert_eq!(
                policy.admit("claude", Some(model.into())),
                Err(("invalid_params", MODEL_INVALID.into())),
                "{model}"
            );
        }
        assert_eq!(
            policy.admit("codex", Some("gpt-5-codex".into())),
            Ok(Some("gpt-5-codex".into()))
        );
    }

    #[test]
    fn permission_modes_are_validated_and_limited_to_claude() {
        let policy = WorkerPolicy::default();
        let launch = policy
            .admit_launch("claude", Some("opus".into()), Some("manual"))
            .unwrap();
        assert_eq!(launch.permission_mode, Some("manual"));
        assert_eq!(
            launch.args(),
            ["--model", "opus", "--permission-mode", "manual"]
        );
        for mode in ["manual", "acceptEdits", "auto", "plan", "dontAsk"] {
            assert_eq!(
                policy
                    .admit_launch("claude", None, Some(mode))
                    .unwrap()
                    .permission_mode,
                Some(mode)
            );
        }
        for mode in [
            "bypassPermissions",
            "bypasspermissions",
            " bypassPermissions",
        ] {
            let (code, message) = policy.admit_launch("claude", None, Some(mode)).unwrap_err();
            assert_eq!(code, PERMISSION_MODE_REFUSED, "{mode}");
            assert!(message.contains("never allowed"), "{message}");
            // Refused before the kind check, so Codex callers learn the same.
            let (code, _) = policy.admit_launch("codex", None, Some(mode)).unwrap_err();
            assert_eq!(code, PERMISSION_MODE_REFUSED);
        }
        for mode in ["Manual", "default", "--dangerously-skip-permissions", ""] {
            let (code, message) = policy.admit_launch("claude", None, Some(mode)).unwrap_err();
            assert_eq!(code, "invalid_params", "{mode}");
            assert!(message.contains("manual, acceptEdits, auto, plan, dontAsk"));
        }
        let (code, _) = policy
            .admit_launch("codex", None, Some("manual"))
            .unwrap_err();
        assert_eq!(code, PERMISSION_MODE_UNSUPPORTED);
        assert_eq!(
            policy.admit_launch("claude", None, None).unwrap().args(),
            Vec::<String>::new()
        );
    }

    #[test]
    fn configured_permission_mode_applies_to_claude_when_none_is_requested() {
        let policy = policy("permission_mode = \"manual\"\n");
        let launch = policy.admit_launch("claude", None, None).unwrap();
        assert_eq!(launch.args(), ["--permission-mode", "manual"]);
        assert_eq!(
            policy
                .admit_launch("claude", None, Some("plan"))
                .unwrap()
                .permission_mode,
            Some("plan")
        );
        // Other kinds ignore the Claude default.
        assert_eq!(
            policy.admit_launch("codex", None, None).unwrap(),
            WorkerLaunch::default()
        );
        assert!(toml::from_str::<PolarisWorkersConfig>(
            "permission_mode = \"bypassPermissions\"\n"
        )
        .is_err());
        assert!(toml::from_str::<PolarisWorkersConfig>("permission_mode = \"ask\"\n").is_err());
    }

    #[test]
    fn empty_kind_list_names_no_allowed_kinds() {
        let (_, message) = policy("kinds = []\n").admit("claude", None).unwrap_err();
        assert!(message.ends_with("allowed kinds: none"), "{message}");
    }

    #[test]
    fn kinds_activity_reports_shape_and_missing_executables() {
        let policy = policy(
            "kinds = [\"claude\", \"codex\"]\n\
             [models]\nclaude = [\"fable\"]\n[defaults]\nclaude = \"fable\"\n",
        );
        let bogus = std::env::temp_dir().join("aster-no-such-worker-path");
        let installed = policy.scan_installed(Some(bogus.as_os_str()));
        assert_eq!(installed, [false, false]);
        assert_eq!(policy.scan_installed(None), [false, false]);
        let activity = policy.activity(&installed);
        assert_eq!(
            activity,
            serde_json::json!([
                {"kind":"claude","executable":"claude","installed":false,"models":["fable"],"default_model":"fable"},
                {"kind":"codex","executable":"codex","installed":false,"models":[]}
            ])
        );
    }

    #[cfg(unix)]
    #[test]
    fn kinds_activity_finds_executables_on_the_given_path() {
        use std::os::unix::fs::PermissionsExt;
        let dir = std::env::temp_dir().join(format!("aster-worker-path-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let codex = dir.join("codex");
        std::fs::write(&codex, "#!/bin/sh\n").unwrap();
        std::fs::set_permissions(&codex, std::fs::Permissions::from_mode(0o755)).unwrap();
        let installed = WorkerPolicy::default().scan_installed(Some(dir.as_os_str()));
        let _ = std::fs::remove_dir_all(&dir);
        assert_eq!(installed, [false, true]);
    }
}
