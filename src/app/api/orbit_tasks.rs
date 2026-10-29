use super::responses::encode_error;
use crate::api::schema::*;
use crate::app::App;
use serde_json::{json, Value};
use std::path::PathBuf;

fn reply(id: String, result: Result<Value, String>) -> String {
    match result {
        Ok(result) => json!({"id":id,"result":result}).to_string(),
        Err(error) => encode_error(id, "orbit_job_error", error),
    }
}
impl App {
    pub(crate) async fn shutdown_orbit_services(&mut self) {
        self.orbit_services.shutdown().await;
    }
    pub(super) fn handle_orbit_service_start(
        &mut self,
        id: String,
        params: OrbitServiceStartParams,
    ) -> String {
        let cwd = match self.orbit_job_cwd() {
            Ok(cwd) => cwd,
            Err(e) => return reply(id, Err(e)),
        };
        reply(id, self.orbit_services.start(cwd, params))
    }
    pub(super) fn handle_orbit_service_stop(
        &mut self,
        id: String,
        params: OrbitServiceStopParams,
    ) -> String {
        reply(id, self.orbit_services.stop(&params.name))
    }
    pub(super) fn handle_orbit_service_list(&mut self, id: String) -> String {
        if let Err(e) = self.orbit_job_cwd() {
            return reply(id, Err(e));
        }
        reply(id, Ok(self.orbit_services.list()))
    }

    pub(super) fn orbit_job_cwd(&mut self) -> Result<PathBuf, String> {
        let (ws, _, _) = self
            .resolve_orbit_pane(None)
            .map_err(|(_, message)| message)?;
        // Use the bound project's directory, independent of caller focus or shell cwd.
        Ok(self.state.workspaces[ws].identity_cwd.clone())
    }
    pub(crate) fn prepare_orbit_review(&mut self) {
        let Ok(cwd) = self.orbit_job_cwd() else {
            return;
        };
        let review = self.orbit_review.clone();
        tokio::task::spawn_blocking(move || {
            let _ = review.capture(&cwd);
        });
    }
    pub(super) fn handle_orbit_exec(&mut self, id: String, params: OrbitExecParams) -> String {
        let cwd = match self.orbit_job_cwd() {
            Ok(cwd) => cwd,
            Err(e) => return reply(id, Err(e)),
        };
        let review = self.orbit_review.clone();
        let before_cwd = cwd.clone();
        reply(
            id,
            self.orbit_jobs.start_with_preflight(
                cwd,
                params.command,
                params.timeout_ms,
                Box::new(move || review.capture(&before_cwd)),
            ),
        )
    }
    pub(super) fn handle_orbit_job_status(
        &mut self,
        id: String,
        params: OrbitJobStatusParams,
    ) -> String {
        reply(
            id,
            self.orbit_jobs.status(
                &params.job_id,
                params.offset.unwrap_or(0),
                params.limit.unwrap_or(16384),
            ),
        )
    }
    pub(super) fn handle_orbit_job_cancel(
        &mut self,
        id: String,
        params: OrbitJobCancelParams,
    ) -> String {
        reply(id, self.orbit_jobs.cancel(&params.job_id))
    }
    pub(super) fn handle_orbit_diff(&mut self, id: String, params: OrbitDiffParams) -> String {
        let cwd = match self.orbit_job_cwd() {
            Ok(cwd) => cwd,
            Err(e) => return reply(id, Err(e)),
        };
        if params.path.as_ref().is_some_and(|path| {
            path.contains('\0')
                || std::path::Path::new(path).is_absolute()
                || std::path::Path::new(path)
                    .components()
                    .any(|c| matches!(c, std::path::Component::ParentDir))
        }) {
            return reply(
                id,
                Err("Diff path must be relative to the project without parent traversal".into()),
            );
        }
        let review = self.orbit_review.clone();
        let task_cwd = cwd.clone();
        reply(
            id,
            self.orbit_jobs.start_task(
                cwd,
                "Review Orbit changes".into(),
                Box::new(move || review.diff(&task_cwd, params.path.as_deref())),
            ),
        )
    }
    pub(super) fn handle_orbit_activity(&mut self, id: String) -> String {
        let (ws, tab) = match self.resolve_orbit() {
            Ok(target) => target,
            Err((_, error)) => return reply(id, Err(error)),
        };
        let tab_id = self.public_tab_id(ws, tab).unwrap_or_default();
        let agents: Vec<_> = self
            .collect_agent_infos()
            .into_iter()
            .filter(|agent| agent.tab_id == tab_id)
            .collect();
        let mut activity = self.orbit_task_activity();
        activity["services"] = self.orbit_services.list();
        activity["jobs"] = self.orbit_jobs.list();
        activity["agents"] = json!(agents);
        activity["stale_panes"] = json!(self.orbit_stale_panes());
        activity["baseline_ready"] = json!(self.orbit_review.ready());
        activity["kinds"] = self.orbit_worker_kinds(std::time::Instant::now());
        activity["team"] = self.orbit_team_activity();
        activity["decisions"] = self.orbit_decision_activity();
        for (task_id, sanity) in self.orbit_task_sanity() {
            let tasks = activity["tasks"].as_array_mut().into_iter().flatten();
            for task in tasks.filter(|task| task["task_id"] == task_id.as_str()) {
                task["sanity"] = sanity.clone();
            }
        }
        reply(id, Ok(activity))
    }
}
