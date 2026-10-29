//! Worktree isolation for Orbit workers: `orbit.agent_start {worktree}` creates
//! a Git worktree of the Orbit project under `worktrees.directory`, opens a new
//! Orbit pane there without changing focus, and starts the worker in it. Git and
//! the shell start-up wait run off the event loop; each stage resumes through
//! `AppEvent::OrbitWorktreeReady`.
use std::path::{Path, PathBuf};
use std::sync::mpsc::Sender;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use serde_json::{json, Value};

use super::orbit_lifecycle::OrbitWorktree;
use super::responses::encode_error;
use crate::api::schema::{
    Method, OrbitAgentStartParams, PaneSplitParams, PaneTarget, Request, SplitDirection,
};
use crate::app::App;
use crate::events::{AppEvent, OrbitWorktreeResult, ParallelLaunch};

/// How long a new worker pane's shell may take to reach its prompt.
const SHELL_READY_TIMEOUT: Duration = Duration::from_secs(5);
const SHELL_READY_POLL: Duration = Duration::from_millis(50);
const NOT_A_REPOSITORY: &str =
    "Worktree isolation requires the Orbit project directory to be inside a Git repository";

fn branch_name(requested: Option<&str>) -> Result<String, String> {
    let Some(branch) = requested else {
        let seed = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|elapsed| elapsed.as_micros().min(u128::from(u64::MAX)) as u64)
            .unwrap_or(0);
        return Ok(crate::worktree::generated_branch_slug(seed));
    };
    let branch = branch.trim();
    if branch.is_empty()
        || branch.starts_with('-')
        || branch
            .chars()
            .any(|ch| ch.is_control() || ch.is_whitespace())
    {
        return Err("worktree branch must be a non-empty Git branch name without spaces".into());
    }
    Ok(branch.to_owned())
}

/// A created worktree: its checkout, the repository checkout, and whether this
/// call created the branch.
struct CreatedWorktree {
    path: PathBuf,
    repo_root: PathBuf,
    created_branch: bool,
}

/// Creates the worktree, on a new branch unless `branch` already exists.
fn create_worktree(
    project: &Path,
    directory: &Path,
    branch: &str,
) -> Result<CreatedWorktree, (&'static str, String)> {
    let space = crate::workspace::git_space_metadata(project)
        .ok_or(("not_git_worktree", NOT_A_REPOSITORY.to_owned()))?;
    let path = crate::worktree::default_checkout_path(directory, &space.repo_name, branch);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|error| ("worktree_create_failed", error.to_string()))?;
    }
    let existed = crate::worktree::local_branch_exists(&space.repo_root, branch, false)
        .map_err(|error| ("worktree_create_failed", error))?;
    crate::worktree::run_worktree_add_command(&space.repo_root, &path, branch, "HEAD", false)
        .map_err(|error| ("worktree_create_failed", error))?;
    Ok(CreatedWorktree {
        path,
        repo_root: space.repo_root,
        created_branch: !existed,
    })
}

/// Hands a finished background stage to the event loop. When the loop is gone
/// (shutdown), nothing will ever own the checkout the stage created, so it is
/// removed here, on the background thread.
fn send_stage(
    event_tx: &tokio::sync::mpsc::Sender<AppEvent>,
    directory: &Path,
    stage: OrbitWorktreeResult,
) {
    let Err(error) = event_tx.blocking_send(AppEvent::OrbitWorktreeReady(Box::new(stage))) else {
        return;
    };
    let AppEvent::OrbitWorktreeReady(stage) = error.0 else {
        return;
    };
    // Whenever a checkout exists, whatever happened to the rest of the stage.
    let created = stage.result.is_ok();
    match stage
        .parallel
        .as_ref()
        .and_then(|plan| plan.checkout.as_ref())
    {
        Some(checkout) => match crate::parallel_checkout::remove(checkout, directory) {
            Ok(()) => {
                tracing::info!(path = %checkout.path.display(), "removed an unowned parallel checkout")
            }
            Err(error) => {
                tracing::warn!(%error, path = %checkout.path.display(), "could not remove an unowned parallel checkout")
            }
        },
        None if stage.parallel.is_none() && created => {
            let worktree = OrbitWorktree {
                path: stage.path.clone(),
                branch: Some(stage.branch.clone()),
                repo_root: stage.repo_root.clone(),
                recovered: false,
                created_branch: stage.created_branch,
            };
            discard_worktree(&worktree, stage.created_branch);
        }
        None => {}
    }
}

/// Creates a parallel step's snapshot checkout and records it on `plan`. The
/// returned path is where the step's pane starts.
fn create_checkout(
    project: &Path,
    directory: &Path,
    plan: &mut ParallelLaunch,
) -> Result<CreatedWorktree, (&'static str, String)> {
    let failed = |error: String| ("worktree_create_failed", error);
    let space = crate::workspace::git_space_metadata(project)
        .ok_or(("not_git_worktree", NOT_A_REPOSITORY.to_owned()))?;
    let checkout = crate::parallel_checkout::create(
        &space.repo_root,
        project,
        directory,
        &space.repo_name,
        &plan.label,
        &plan.apply,
    )
    .map_err(failed)?;
    let created = CreatedWorktree {
        path: checkout.workdir(),
        repo_root: space.repo_root,
        created_branch: false,
    };
    plan.checkout = Some(checkout);
    Ok(created)
}

/// Best-effort cleanup of a worktree whose worker never started, deleting the
/// branch only when this start created it.
fn discard_worktree(worktree: &OrbitWorktree, created_branch: bool) {
    let command = crate::worktree::build_worktree_remove_command(
        &worktree.repo_root,
        &worktree.path,
        false,
        false,
    );
    let branch = worktree
        .branch
        .as_deref()
        .filter(|_| created_branch)
        .map(|branch| {
            crate::worktree::build_branch_delete_command(&worktree.repo_root, branch, false)
        });
    let (repo_root, path) = (worktree.repo_root.clone(), worktree.path.clone());
    let spawned = crate::thread_spawn::spawn_named("aster-orbit-worktree-discard", move || {
        if let Err(error) = crate::worktree::run_worktree_remove_command_with_recovery(
            &command, &repo_root, &path, false, false,
        ) {
            tracing::warn!(%error, path = %path.display(), "could not discard Orbit worktree");
            return;
        }
        if let Some(command) = branch {
            if let Err(error) = crate::worktree::run_worktree_command(&command) {
                tracing::warn!(%error, "could not delete the discarded Orbit worktree branch");
            }
        }
    });
    if let Err(error) = spawned {
        tracing::warn!(%error, "could not start Orbit worktree cleanup");
    }
}

impl App {
    /// Starts `orbit.agent_start` with `worktree`; `false` for other requests.
    pub(crate) fn handle_deferred_orbit_start(
        &mut self,
        request: Request,
        respond_to: Sender<String>,
    ) -> bool {
        let Method::OrbitAgentStart(params) = request.method else {
            return false;
        };
        if params.worktree_branch().is_none() {
            return false;
        }
        if let Err(response) =
            self.begin_orbit_worktree_start(&request.id, params, &respond_to, None)
        {
            let _ = respond_to.send(response);
        }
        true
    }

    /// Whether another Orbit start reserved this pane's terminal for its launch.
    pub(super) fn orbit_terminal_reserved(&self, terminal_id: &str) -> bool {
        self.orbit_starts.reserved.contains(terminal_id)
    }

    /// Ends an isolated start: it no longer counts toward `max_concurrent`, and
    /// its pane is no longer reserved.
    fn end_orbit_worktree_start(&mut self, terminal_id: Option<&str>) {
        self.orbit_starts.in_flight = self.orbit_starts.in_flight.saturating_sub(1);
        if let Some(terminal_id) = terminal_id {
            self.orbit_starts.reserved.remove(terminal_id);
        }
    }

    /// Starts a parallel Orbit step's worker in a new pane inside a snapshot
    /// checkout; the response to `respond_to` is the worker start response.
    pub(crate) fn start_parallel_orbit_worker(
        &mut self,
        id: &str,
        params: OrbitAgentStartParams,
        plan: ParallelLaunch,
        respond_to: &Sender<String>,
    ) -> Result<(), String> {
        self.begin_orbit_worktree_start(id, params, respond_to, Some(plan))
    }

    fn begin_orbit_worktree_start(
        &mut self,
        id: &str,
        params: OrbitAgentStartParams,
        respond_to: &Sender<String>,
        parallel: Option<ParallelLaunch>,
    ) -> Result<(), String> {
        let launch = self.admit_orbit_worker(id, &params)?;
        if !crate::app::agents::valid_agent_name(&params.name) {
            return Err(super::responses::encode_error_body(
                id.into(),
                self.agent_start_error_body(crate::app::agents::AgentStartError::InvalidName),
            ));
        }
        self.check_orbit_worker_limit(id, 0)?;
        self.resolve_orbit_pane(params.pane_id.as_deref())
            .map_err(|(code, message)| encode_error(id.into(), code, message))?;
        let branch = match &parallel {
            Some(plan) => plan.label.clone(),
            None => branch_name(params.worktree_branch().flatten())
                .map_err(|message| encode_error(id.into(), "invalid_params", message))?,
        };
        let project = self
            .orbit_job_cwd()
            .map_err(|message| encode_error(id.into(), "orbit_missing", message))?;
        let directory = self.state.worktree_directory.clone();
        let event_tx = self.event_tx.clone();
        let mut stage = OrbitWorktreeResult {
            id: id.into(),
            params,
            launch,
            branch,
            path: PathBuf::new(),
            repo_root: PathBuf::new(),
            pane_id: None,
            terminal_id: None,
            created_branch: false,
            parallel,
            result: Ok(()),
            respond_to: respond_to.clone(),
        };
        // Counted from admission, so parallel starts cannot pass the limit together.
        self.orbit_starts.in_flight += 1;
        crate::thread_spawn::spawn_named("aster-orbit-worktree-add", move || {
            let created = match stage.parallel.as_mut() {
                Some(plan) => create_checkout(&project, &directory, plan),
                None => create_worktree(&project, &directory, &stage.branch),
            };
            match created {
                Ok(created) => {
                    stage.path = created.path;
                    stage.repo_root = created.repo_root;
                    stage.created_branch = created.created_branch;
                }
                Err(error) => stage.result = Err(error),
            }
            send_stage(&event_tx, &directory, stage);
        })
        .map(|_| ())
        .map_err(|error| {
            self.end_orbit_worktree_start(None);
            encode_error(
                id.into(),
                "worktree_create_failed",
                format!("could not start worktree creation: {error}"),
            )
        })
    }

    /// Resumes an isolated start on the event loop after a background stage.
    pub(crate) fn finish_orbit_worktree_start(&mut self, mut stage: OrbitWorktreeResult) {
        // A parallel step owns its checkout from here on, whether or not the
        // worker starts: the step releases it when it ends.
        if let Some(plan) = &stage.parallel {
            self.record_parallel_checkout(plan);
        }
        let worktree = OrbitWorktree {
            path: stage.path.clone(),
            branch: Some(stage.branch.clone()),
            repo_root: stage.repo_root.clone(),
            recovered: false,
            created_branch: stage.created_branch,
        };
        let respond_to = stage.respond_to.clone();
        let id = stage.id.clone();
        let created_branch = stage.created_branch;
        let is_parallel = stage.parallel.is_some();
        let response = match (stage.result.clone(), stage.pane_id.take()) {
            (Err((code, message)), _) => {
                self.end_orbit_worktree_start(None);
                encode_error(id, code, message)
            }
            (Ok(()), None) => match self.open_orbit_worktree_pane(stage, &worktree) {
                Ok(()) => return,
                Err(response) => {
                    if !is_parallel {
                        discard_worktree(&worktree, created_branch);
                    }
                    self.end_orbit_worktree_start(None);
                    response
                }
            },
            (Ok(()), Some(pane_id)) => {
                let terminal_id = stage.terminal_id.take();
                let response = self.start_isolated_orbit_worker(
                    stage,
                    pane_id,
                    terminal_id.as_deref(),
                    worktree,
                );
                self.end_orbit_worktree_start(terminal_id.as_deref());
                response
            }
        };
        let _ = respond_to.send(response);
    }

    /// Splits a new Orbit pane in the worktree, reserves it, then waits off the
    /// event loop for its shell before the launch stage.
    fn open_orbit_worktree_pane(
        &mut self,
        mut stage: OrbitWorktreeResult,
        worktree: &OrbitWorktree,
    ) -> Result<(), String> {
        let id = stage.id.clone();
        // Orbit may have changed while Git ran, so resolve the split target again.
        let (_, _, target) = self
            .resolve_orbit_pane(stage.params.pane_id.as_deref())
            .map_err(|(code, message)| encode_error(id.clone(), code, message))?;
        let split = self.handle_pane_split(
            id.clone(),
            PaneSplitParams {
                workspace_id: None,
                target_pane_id: Some(target),
                direction: SplitDirection::Right,
                ratio: None,
                cwd: Some(worktree.path.display().to_string()),
                focus: false,
                right_click: Default::default(),
                env: Default::default(),
            },
        );
        let value: Value = serde_json::from_str(&split).unwrap_or_default();
        let Some(pane_id) = value["result"]["pane"]["pane_id"]
            .as_str()
            .map(str::to_owned)
        else {
            return Err(split);
        };
        let terminal = self
            .parse_pane_id(&pane_id)
            .and_then(|(ws, pane)| self.state.workspaces[ws].terminal_id(pane).cloned());
        let shell_pid = terminal
            .as_ref()
            .and_then(|terminal| self.terminal_runtimes.get(terminal))
            .and_then(|runtime| runtime.child_pid());
        let terminal_id = terminal.map(|terminal| terminal.to_string());
        if let Some(terminal_id) = &terminal_id {
            self.orbit_starts.reserved.insert(terminal_id.clone());
        }
        stage.pane_id = Some(pane_id.clone());
        stage.terminal_id = terminal_id.clone();
        let directory = self.state.worktree_directory.clone();
        let event_tx = self.event_tx.clone();
        let wait = move || {
            let deadline = Instant::now() + SHELL_READY_TIMEOUT;
            while let Some(pid) = shell_pid {
                if crate::platform::available_pane_shell(pid).is_some()
                    || Instant::now() >= deadline
                {
                    break;
                }
                std::thread::sleep(SHELL_READY_POLL);
            }
            send_stage(&event_tx, &directory, stage);
        };
        crate::thread_spawn::spawn_named("aster-orbit-worktree-shell", wait)
            .map(|_| ())
            .map_err(|error| {
                if let Some(terminal_id) = &terminal_id {
                    self.orbit_starts.reserved.remove(terminal_id);
                }
                let _ = self.close_pane(id.clone(), &PaneTarget { pane_id });
                encode_error(
                    id,
                    "orbit_unavailable",
                    format!("could not wait for the worker shell: {error}"),
                )
            })
    }

    /// Whether the reserved pane is still what this start made: an unlabelled
    /// shell at its prompt with no agent, so closing it loses nothing.
    fn orbit_start_pane_is_untouched(&self, terminal_id: &str) -> bool {
        let Some((key, terminal)) = self.state.terminals.get_key_value(terminal_id) else {
            return false;
        };
        terminal.manual_label.is_none()
            && !terminal.is_agent_terminal()
            && terminal.managed_agent_kind().is_none()
            && !terminal.managed_agent_launch_pending()
            && self
                .terminal_runtimes
                .get(key)
                .and_then(crate::app::agents::available_shell_name)
                .is_some()
    }

    /// Closes a failed start's pane only when nothing else used it meanwhile.
    fn close_failed_orbit_start_pane(
        &mut self,
        id: &str,
        pane_id: String,
        terminal_id: Option<&str>,
    ) {
        if terminal_id.is_some_and(|terminal_id| self.orbit_start_pane_is_untouched(terminal_id)) {
            let _ = self.close_pane(id.into(), &PaneTarget { pane_id });
        } else {
            tracing::info!(%pane_id, "orbit start failed; pane kept because it is in use");
        }
    }

    fn start_isolated_orbit_worker(
        &mut self,
        stage: OrbitWorktreeResult,
        pane_id: String,
        terminal_id: Option<&str>,
        worktree: OrbitWorktree,
    ) -> String {
        let OrbitWorktreeResult {
            id,
            params,
            launch,
            created_branch,
            parallel,
            ..
        } = stage;
        let is_parallel = parallel.is_some();
        // Other workers may have started while Git and the shell ran; this
        // start itself is still counted in flight.
        let launched = self.check_orbit_worker_limit(&id, 1).and_then(|()| {
            self.launch_orbit_worker(
                id.clone(),
                params.name,
                params.kind,
                pane_id.clone(),
                launch,
            )
        });
        let response = match launched {
            Ok(response) => response,
            Err(response) => {
                self.close_failed_orbit_start_pane(&id, pane_id, terminal_id);
                if !is_parallel {
                    discard_worktree(&worktree, created_branch);
                }
                return response;
            }
        };
        let mut value: Value = serde_json::from_str(&response).unwrap_or_default();
        if let Some(terminal_id) = value["result"]["agent"]["terminal_id"].as_str() {
            // A parallel step's checkout is not a Git worktree to stop and remove.
            if let Some(record) = self
                .orbit_workers
                .get_mut(terminal_id)
                .filter(|_| !is_parallel)
            {
                record.worktree = Some(worktree.clone());
            }
            value["result"]["worker"] = json!(self.orbit_worker_json(terminal_id));
        }
        tracing::info!(branch = ?worktree.branch, path = %worktree.path.display(), "orbit worker isolated");
        value["result"]["worktree"] = json!(worktree);
        value.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::super::orbit_lifecycle::tests::{orbit_app, orbit_panes};
    use super::*;

    fn start_request(worktree: Value) -> Request {
        serde_json::from_value(json!({
            "id": "start",
            "method": "orbit.agent_start",
            "params": {"name": "editor", "kind": "claude", "worktree": worktree},
        }))
        .unwrap()
    }

    #[test]
    fn worktree_request_shapes_parse() {
        let branch = |value: Value| {
            let Method::OrbitAgentStart(params) = start_request(value).method else {
                unreachable!()
            };
            params
                .worktree_branch()
                .map(|branch| branch.map(str::to_owned))
        };
        assert_eq!(branch(json!(true)), Some(None));
        assert_eq!(branch(json!(false)), None);
        assert_eq!(branch(json!("feature/x")), Some(Some("feature/x".into())));
        // A lead model sending the flag as text must not get a branch called "true".
        for text in ["true", "True", " yes "] {
            assert_eq!(branch(json!(text)), Some(None), "{text:?}");
        }
        for text in ["false", "", "none"] {
            assert_eq!(branch(json!(text)), None, "{text:?}");
        }
    }

    #[test]
    fn branch_names_are_generated_or_validated() {
        assert!(branch_name(None).unwrap().starts_with("worktree/"));
        assert_eq!(branch_name(Some(" fix/a ")).unwrap(), "fix/a");
        for bad in ["", "two words", "-flag", "tab\tname"] {
            assert!(branch_name(Some(bad)).is_err(), "{bad:?}");
        }
    }

    #[test]
    fn worktree_start_requires_deferred_dispatch_on_the_direct_path() {
        let mut app = orbit_app();
        let response: Value =
            serde_json::from_str(&app.handle_api_request(start_request(json!(true)))).unwrap();
        assert_eq!(response["error"]["code"], "deferred_required", "{response}");
    }

    fn next_worktree_stage(app: &mut App) -> OrbitWorktreeResult {
        let deadline = Instant::now() + Duration::from_secs(10);
        while Instant::now() < deadline {
            match app.event_rx.try_recv() {
                Ok(AppEvent::OrbitWorktreeReady(stage)) => return *stage,
                Ok(_) => {}
                Err(_) => std::thread::sleep(Duration::from_millis(10)),
            }
        }
        panic!("timed out waiting for the Orbit worktree stage");
    }

    #[test]
    fn worktree_start_refuses_a_project_outside_git() {
        let mut app = orbit_app();
        let outside = std::env::temp_dir().join(format!(
            "aster-orbit-not-git-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&outside).unwrap();
        app.state.workspaces[0].identity_cwd = outside.clone();
        let (tx, rx) = std::sync::mpsc::channel();
        assert!(app.handle_deferred_orbit_start(start_request(json!(true)), tx));
        let stage = next_worktree_stage(&mut app);
        app.finish_orbit_worktree_start(stage);
        let response: Value = serde_json::from_str(&rx.recv().unwrap()).unwrap();
        assert_eq!(response["error"]["code"], "not_git_worktree", "{response}");
        assert_eq!(response["error"]["message"], NOT_A_REPOSITORY);
        assert_eq!(app.state.active, Some(1));
        let _ = std::fs::remove_dir_all(outside);
    }

    fn limit(app: &mut App, max: usize) {
        app.set_orbit_worker_policy(&toml::from_str(&format!("max_concurrent = {max}\n")).unwrap());
    }

    /// Makes Orbit pane `index` a fresh, unlabelled shell and returns its ids.
    fn idle_shell(app: &mut App, index: usize) -> (String, String) {
        let (_, pane_id, terminal_id) = orbit_panes(app)[index].clone();
        let key = app
            .state
            .terminals
            .get_key_value(terminal_id.as_str())
            .map(|(key, _)| key.clone())
            .unwrap();
        app.state.terminals.insert(
            key.clone(),
            crate::terminal::TerminalState::new(key.clone(), "/".into()),
        );
        let (runtime, _queue) =
            crate::terminal::TerminalRuntime::test_with_channel_capacity(80, 24, 4);
        app.terminal_runtimes.insert(key, runtime);
        (pane_id, terminal_id)
    }

    /// The launch stage of an isolated start whose pane is reserved.
    fn launch_stage(
        app: &mut App,
        kind: &str,
        pane: &(String, String),
    ) -> (OrbitWorktreeResult, std::sync::mpsc::Receiver<String>) {
        let (tx, rx) = std::sync::mpsc::channel();
        app.orbit_starts.in_flight += 1;
        app.orbit_starts.reserved.insert(pane.1.clone());
        let stage = OrbitWorktreeResult {
            id: "start".into(),
            params: OrbitAgentStartParams {
                name: "editor".into(),
                kind: kind.into(),
                pane_id: None,
                model: None,
                reuse: None,
                worktree: Some(crate::api::schema::OrbitWorktreeRequest::Enabled(true)),
                permission_mode: None,
            },
            launch: Default::default(),
            branch: "worktree/test".into(),
            path: std::env::temp_dir().join("aster-orbit-missing-worktree"),
            repo_root: std::env::temp_dir().join("aster-orbit-missing-repo"),
            pane_id: Some(pane.0.clone()),
            terminal_id: Some(pane.1.clone()),
            created_branch: false,
            parallel: None,
            result: Ok(()),
            respond_to: tx,
        };
        (stage, rx)
    }

    fn error_code(rx: &std::sync::mpsc::Receiver<String>) -> Value {
        let response: Value = serde_json::from_str(&rx.recv().unwrap()).unwrap();
        response["error"]["code"].clone()
    }

    #[tokio::test]
    async fn orbit_start_refuses_a_pane_reserved_for_a_starting_worker() {
        let mut app = orbit_app();
        let pane = idle_shell(&mut app, 1);
        app.orbit_starts.reserved.insert(pane.1.clone());
        let response: Value = serde_json::from_str(
            &app.handle_orbit_agent_start(
                "start".into(),
                serde_json::from_value(
                    json!({"name": "other", "kind": "claude", "pane_id": pane.0}),
                )
                .unwrap(),
            ),
        )
        .unwrap();
        assert_eq!(
            response["error"]["code"], "orbit_pane_reserved",
            "{response}"
        );
        assert!(app
            .find_reusable_orbit_worker("claude", None, None)
            .is_none());
    }

    #[tokio::test]
    async fn failed_launch_closes_only_an_untouched_pane_and_always_clears_the_reservation() {
        let mut app = orbit_app();
        let pane = idle_shell(&mut app, 1);
        let (stage, rx) = launch_stage(&mut app, "not-an-agent", &pane);
        app.finish_orbit_worktree_start(stage);
        assert!(error_code(&rx).is_string());
        assert_eq!(orbit_panes(&mut app).len(), 1, "untouched pane closed");
        assert!(app.orbit_starts.reserved.is_empty());
        assert_eq!(app.orbit_starts.in_flight, 0);

        // A pane the user labelled meanwhile is kept.
        let mut app = orbit_app();
        let pane = idle_shell(&mut app, 1);
        app.state
            .terminals
            .get_mut(pane.1.as_str())
            .unwrap()
            .set_manual_label("mine".into());
        let (stage, rx) = launch_stage(&mut app, "not-an-agent", &pane);
        app.finish_orbit_worktree_start(stage);
        assert!(error_code(&rx).is_string());
        assert_eq!(orbit_panes(&mut app).len(), 2, "labelled pane closed");
        assert!(app.orbit_starts.reserved.is_empty());
        assert_eq!(app.orbit_starts.in_flight, 0);
    }

    #[tokio::test]
    async fn parallel_worktree_starts_count_toward_max_concurrent() {
        let mut app = orbit_app();
        // Outside Git, so the admitted start fails at `git worktree add`.
        let outside = std::env::temp_dir().join(format!(
            "aster-orbit-parallel-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&outside).unwrap();
        app.state.workspaces[0].identity_cwd = outside.clone();
        app.state.worktree_directory = outside.join("worktrees");
        limit(&mut app, 1);
        let (first, first_rx) = std::sync::mpsc::channel();
        assert!(app.handle_deferred_orbit_start(start_request(json!(true)), first));
        let (second, second_rx) = std::sync::mpsc::channel();
        assert!(app.handle_deferred_orbit_start(start_request(json!(true)), second));
        assert_eq!(error_code(&second_rx), "orbit_worker_limit");
        assert_eq!(app.orbit_starts.in_flight, 1);
        assert!(
            first_rx.try_recv().is_err(),
            "the first start is still running"
        );
        // Non-isolated starts count the start in flight too.
        let response: Value = serde_json::from_str(
            &app.handle_orbit_agent_start(
                "start".into(),
                serde_json::from_value(json!({"name": "other", "kind": "claude", "reuse": false}))
                    .unwrap(),
            ),
        )
        .unwrap();
        assert_eq!(
            response["error"]["code"], "orbit_worker_limit",
            "{response}"
        );
        let stage = next_worktree_stage(&mut app);
        app.finish_orbit_worktree_start(stage);
        let response: Value = serde_json::from_str(&first_rx.recv().unwrap()).unwrap();
        assert_eq!(response["error"]["code"], "not_git_worktree", "{response}");
        assert_eq!(app.orbit_starts.in_flight, 0);
        let _ = std::fs::remove_dir_all(outside);
    }

    #[tokio::test]
    async fn launch_rechecks_the_limit_against_starts_that_finished_meanwhile() {
        let mut app = orbit_app();
        limit(&mut app, 1);
        let pane = idle_shell(&mut app, 1);
        // Another start is also in flight, so this one would exceed the limit.
        app.orbit_starts.in_flight += 1;
        let (stage, rx) = launch_stage(&mut app, "claude", &pane);
        app.finish_orbit_worktree_start(stage);
        assert_eq!(error_code(&rx), "orbit_worker_limit");
        assert_eq!(orbit_panes(&mut app).len(), 1);
        assert_eq!(app.orbit_starts.in_flight, 1);
        assert!(app.orbit_starts.reserved.is_empty());
    }

    #[test]
    fn worktree_start_validates_before_touching_git() {
        let mut app = orbit_app();
        for (params, code) in [
            (
                json!({"name": "polaris", "kind": "claude", "worktree": true}),
                "invalid_params",
            ),
            (
                json!({"name": "Bad Name", "kind": "claude", "worktree": true}),
                "invalid_agent_name",
            ),
            (
                json!({"name": "editor", "kind": "claude", "worktree": "two words"}),
                "invalid_params",
            ),
        ] {
            let request: Request = serde_json::from_value(
                json!({"id": "start", "method": "orbit.agent_start", "params": params}),
            )
            .unwrap();
            let (tx, rx) = std::sync::mpsc::channel();
            assert!(app.handle_deferred_orbit_start(request, tx));
            let response: Value = serde_json::from_str(&rx.recv().unwrap()).unwrap();
            assert_eq!(response["error"]["code"], code, "{response}");
        }
    }

    fn refuse_clone(_: &Path, _: &Path) -> std::io::Result<()> {
        Err(std::io::Error::other("no clone"))
    }

    /// A scratch repository with a parallel checkout in a stage whose result is `result`.
    fn parallel_stage(
        name: &str,
        result: Result<(), (&'static str, String)>,
    ) -> (
        OrbitWorktreeResult,
        crate::parallel_checkout::Checkout,
        PathBuf,
    ) {
        let root =
            std::env::temp_dir().join(format!("aster-send-stage-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let repo = root.join("repo");
        std::fs::create_dir_all(&repo).unwrap();
        for args in [
            vec!["init", "-q"],
            vec![
                "-c",
                "user.name=T",
                "-c",
                "user.email=t@example.invalid",
                "commit",
                "-q",
                "--allow-empty",
                "-m",
                "base",
            ],
        ] {
            let status = std::process::Command::new("git")
                .arg("-C")
                .arg(&repo)
                .args(args)
                .status()
                .unwrap();
            assert!(status.success());
        }
        let checkout = crate::parallel_checkout::create_with(
            &repo,
            &repo,
            &root.join("checkouts"),
            "repo",
            "par-x",
            &[],
            refuse_clone,
        )
        .unwrap();
        let (respond_to, _responses) = std::sync::mpsc::channel();
        let stage = OrbitWorktreeResult {
            id: "start".into(),
            params: OrbitAgentStartParams {
                name: "par-x".into(),
                kind: "claude".into(),
                pane_id: None,
                model: None,
                reuse: None,
                worktree: None,
                permission_mode: None,
            },
            launch: Default::default(),
            branch: "par-x".into(),
            path: checkout.path.clone(),
            repo_root: repo.clone(),
            pane_id: None,
            terminal_id: None,
            created_branch: false,
            parallel: Some(ParallelLaunch {
                task_id: "task".into(),
                step: 0,
                label: "par-x".into(),
                apply: Vec::new(),
                checkout: Some(checkout.clone()),
            }),
            result,
            respond_to,
        };
        (stage, checkout, root)
    }

    #[test]
    fn a_stage_the_event_loop_cannot_receive_has_its_parallel_checkout_removed() {
        // Whether the stage succeeded or failed after the checkout was made
        // (for example its shell never became ready).
        for (name, result) in [
            ("ok", Ok(())),
            (
                "failed",
                Err(("orbit_unavailable", "shell not ready".to_owned())),
            ),
        ] {
            let (stage, checkout, root) = parallel_stage(name, result);
            let (events, receiver) = tokio::sync::mpsc::channel(1);
            drop(receiver);

            send_stage(&events, &root.join("checkouts"), stage);

            assert!(!checkout.path.exists(), "{name}");
            let _ = std::fs::remove_dir_all(root);
        }
    }
}
