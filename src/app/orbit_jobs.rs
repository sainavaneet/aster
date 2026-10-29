//! Tracked Orbit subprocesses. Runtime ownership stays on the server; output is
//! captured independently of terminal scrollback and bounded per job.
use std::collections::VecDeque;
use std::path::PathBuf;
use std::process::Stdio;
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use base64::Engine;
use serde_json::{json, Value};
use tokio::io::{AsyncRead, AsyncReadExt};
use tokio::sync::watch;

const MAX_RUNNING: usize = 4;
const MAX_RETAINED: usize = 32;
const MAX_OUTPUT: usize = 4 * 1024 * 1024;
const MAX_PAGE: usize = 64 * 1024;
const DEFAULT_TIMEOUT_MS: u64 = 30 * 60 * 1000;
const MAX_TIMEOUT_MS: u64 = 24 * 60 * 60 * 1000;

#[derive(Default)]
pub(crate) struct JobManager {
    jobs: VecDeque<Job>,
    sequence: u64,
}

struct Job {
    id: String,
    kind: &'static str,
    command: String,
    cwd: PathBuf,
    created: Instant,
    state: Arc<Mutex<JobState>>,
    cancel: watch::Sender<bool>,
}

type AllocatedJob = (String, Arc<Mutex<JobState>>, watch::Receiver<bool>);

struct JobState {
    status: &'static str,
    exit_code: Option<i32>,
    output: Vec<u8>,
    observed_bytes: u64,
    error: Option<String>,
    elapsed_ms: Option<u128>,
}

impl Default for JobState {
    fn default() -> Self {
        Self {
            status: "running",
            exit_code: None,
            output: Vec::new(),
            observed_bytes: 0,
            error: None,
            elapsed_ms: None,
        }
    }
}

fn lock(state: &Mutex<JobState>) -> MutexGuard<'_, JobState> {
    state.lock().unwrap_or_else(|poison| poison.into_inner())
}

impl JobManager {
    #[cfg(test)]
    pub(crate) fn start(
        &mut self,
        cwd: PathBuf,
        command: String,
        timeout_ms: Option<u64>,
    ) -> Result<Value, String> {
        self.start_with_preflight(cwd, command, timeout_ms, Box::new(|| Ok(())))
    }

    pub(crate) fn start_with_preflight(
        &mut self,
        cwd: PathBuf,
        command: String,
        timeout_ms: Option<u64>,
        preflight: Box<dyn FnOnce() -> Result<(), String> + Send + 'static>,
    ) -> Result<Value, String> {
        if command.trim().is_empty() || command.contains('\0') {
            return Err("command must be non-empty and contain no NUL bytes".into());
        }
        let timeout_ms = timeout_ms.unwrap_or(DEFAULT_TIMEOUT_MS);
        if !(1..=MAX_TIMEOUT_MS).contains(&timeout_ms) {
            return Err("timeout_ms must be between 1 and 86400000".into());
        }
        let runtime = tokio::runtime::Handle::try_current()
            .map_err(|_| "Orbit job execution requires the server runtime")?;
        let (id, state, receiver) = self.allocate(cwd.clone(), command.clone(), "process")?;
        runtime.spawn(run_job(
            cwd,
            command,
            Duration::from_millis(timeout_ms),
            state,
            receiver,
            preflight,
        ));
        self.status(&id, 0, MAX_PAGE)
    }

    fn allocate(
        &mut self,
        cwd: PathBuf,
        command: String,
        kind: &'static str,
    ) -> Result<AllocatedJob, String> {
        if self
            .jobs
            .iter()
            .filter(|job| lock(&job.state).status == "running")
            .count()
            >= MAX_RUNNING
        {
            return Err("Orbit already has four running jobs; wait or cancel one".into());
        }
        while self.jobs.len() >= MAX_RETAINED {
            let index = self
                .jobs
                .iter()
                .position(|job| lock(&job.state).status != "running")
                .ok_or("Orbit job history is full")?;
            self.jobs.remove(index);
        }
        self.sequence += 1;
        let stamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        let id = format!("job-{}-{stamp}-{}", std::process::id(), self.sequence);
        let state = Arc::new(Mutex::new(JobState::default()));
        let (cancel, receiver) = watch::channel(false);
        let job = Job {
            id: id.clone(),
            kind,
            command: command.clone(),
            cwd: cwd.clone(),
            created: Instant::now(),
            state: state.clone(),
            cancel,
        };
        self.jobs.push_back(job);
        Ok((id, state, receiver))
    }

    pub(crate) fn start_task(
        &mut self,
        cwd: PathBuf,
        label: String,
        task: Box<dyn FnOnce() -> Result<String, String> + Send + 'static>,
    ) -> Result<Value, String> {
        let runtime = tokio::runtime::Handle::try_current()
            .map_err(|_| "Orbit job execution requires the server runtime")?;
        let (id, state, receiver) = self.allocate(cwd, label, "task")?;
        runtime.spawn(async move {
            let started = Instant::now();
            let result = if *receiver.borrow() {
                None
            } else {
                Some(tokio::task::spawn_blocking(task).await)
            };
            let mut state = lock(&state);
            if *receiver.borrow() {
                state.status = "cancelled";
            } else {
                match result {
                    Some(Ok(Ok(output))) => {
                        state.observed_bytes = output.len() as u64;
                        state
                            .output
                            .extend_from_slice(&output.as_bytes()[..output.len().min(MAX_OUTPUT)]);
                        state.status = "completed";
                        state.exit_code = Some(0);
                    }
                    Some(Ok(Err(error))) => {
                        state.status = "failed";
                        state.error = Some(error);
                    }
                    Some(Err(error)) => {
                        state.status = "failed";
                        state.error = Some(error.to_string());
                    }
                    None => state.status = "cancelled",
                }
            }
            state.elapsed_ms = Some(started.elapsed().as_millis());
        });
        self.status(&id, 0, MAX_PAGE)
    }

    pub(crate) fn status(&self, id: &str, offset: u64, limit: usize) -> Result<Value, String> {
        let job = self
            .jobs
            .iter()
            .find(|job| job.id == id)
            .ok_or("Orbit job not found in this server session")?;
        if limit == 0 || limit > MAX_PAGE {
            return Err("limit must be between 1 and 65536 bytes".into());
        }
        let state = lock(&job.state);
        let offset = usize::try_from(offset).map_err(|_| "offset exceeds captured output")?;
        if offset > state.output.len() {
            return Err("offset exceeds captured output".into());
        }
        let end = offset.saturating_add(limit).min(state.output.len());
        let mut value = job.summary(&state);
        value["output"] = json!(String::from_utf8_lossy(&state.output[offset..end]));
        value["output_base64"] =
            json!(base64::engine::general_purpose::STANDARD.encode(&state.output[offset..end]));
        value["offset"] = json!(offset);
        value["next_offset"] = json!(end);
        value["has_more"] = json!(end < state.output.len());
        Ok(value)
    }

    pub(crate) fn cancel(&self, id: &str) -> Result<Value, String> {
        let job = self
            .jobs
            .iter()
            .find(|job| job.id == id)
            .ok_or("Orbit job not found in this server session")?;
        if lock(&job.state).status == "running" {
            let _ = job.cancel.send(true);
        }
        let mut value = self.status(id, 0, MAX_PAGE)?;
        value["cancel_requested"] = json!(*job.cancel.borrow());
        Ok(value)
    }

    pub(crate) fn list(&self) -> Value {
        json!(self
            .jobs
            .iter()
            .map(|job| job.summary(&lock(&job.state)))
            .collect::<Vec<_>>())
    }
}

impl Job {
    fn summary(&self, state: &JobState) -> Value {
        json!({"job_id":self.id,"command":self.command,"cwd":self.cwd,"kind":self.kind,
            "cancel_mode":if self.kind == "process" { "process_tree" } else { "cooperative" },
            "output_complete":state.status != "running" && state.error.is_none() && state.observed_bytes == state.output.len() as u64,
            "status":state.status,"exit_code":state.exit_code,"error":state.error,
            "elapsed_ms":state.elapsed_ms.unwrap_or_else(||self.created.elapsed().as_millis()),
            "captured_bytes":state.output.len(),"observed_bytes":state.observed_bytes,
            "output_truncated":state.observed_bytes > state.output.len() as u64,
            "output_limit_bytes":MAX_OUTPUT,"output_order":"stdout and stderr combined in arrival order"})
    }
}

impl Drop for JobManager {
    fn drop(&mut self) {
        for job in &self.jobs {
            if lock(&job.state).status == "running" {
                let _ = job.cancel.send(true);
            }
        }
    }
}

async fn capture(
    mut reader: impl AsyncRead + Unpin,
    state: Arc<Mutex<JobState>>,
) -> std::io::Result<()> {
    let mut buffer = [0; 8192];
    loop {
        let bytes = reader.read(&mut buffer).await?;
        if bytes == 0 {
            return Ok(());
        }
        let mut state = lock(&state);
        state.observed_bytes = state.observed_bytes.saturating_add(bytes as u64);
        let keep = bytes.min(MAX_OUTPUT.saturating_sub(state.output.len()));
        state.output.extend_from_slice(&buffer[..keep]);
    }
}

async fn run_job(
    cwd: PathBuf,
    command: String,
    timeout: Duration,
    state: Arc<Mutex<JobState>>,
    mut cancel: watch::Receiver<bool>,
    preflight: Box<dyn FnOnce() -> Result<(), String> + Send + 'static>,
) {
    let started = Instant::now();
    let result = async {
        if *cancel.borrow() {
            return Ok(("cancelled", None));
        }
        tokio::task::spawn_blocking(preflight)
            .await
            .map_err(|error| error.to_string())??;
        execute(cwd, command, timeout, state.clone(), &mut cancel).await
    }
    .await;
    let mut state = lock(&state);
    match result {
        Ok((status, exit_code)) => {
            state.status = status;
            state.exit_code = exit_code;
        }
        Err(error) => {
            state.status = "failed";
            state.error = Some(error);
        }
    }
    state.elapsed_ms = Some(started.elapsed().as_millis());
}

async fn execute(
    cwd: PathBuf,
    command: String,
    timeout: Duration,
    state: Arc<Mutex<JobState>>,
    cancel: &mut watch::Receiver<bool>,
) -> Result<(&'static str, Option<i32>), String> {
    if *cancel.borrow() {
        return Ok(("cancelled", None));
    }
    let mut process = crate::platform::detached_custom_command_process(&command);
    process
        .current_dir(cwd)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    crate::platform::configure_status_command(&mut process);
    let mut process = tokio::process::Command::from(process);
    process.kill_on_drop(true);
    let mut child = process.spawn().map_err(|error| error.to_string())?;
    let mut guard =
        crate::platform::StatusCommandGuard::new(&child).map_err(|error| error.to_string())?;
    let stdout = child.stdout.take().ok_or("stdout capture unavailable")?;
    let stderr = child.stderr.take().ok_or("stderr capture unavailable")?;
    let mut readers = tokio::task::JoinSet::new();
    readers.spawn(capture(stdout, state.clone()));
    readers.spawn(capture(stderr, state.clone()));
    let outcome = tokio::select! {
        result = child.wait() => {
            let result = result.map_err(|error|error.to_string())?;
            (if result.success() {"completed"} else {"failed"}, result.code())
        },
        _ = cancel.changed() => ("cancelled", None),
        _ = tokio::time::sleep(timeout) => ("timed_out", None),
    };
    guard.terminate();
    let _ = tokio::time::timeout(Duration::from_secs(1), child.wait()).await;
    let drained = tokio::time::timeout(Duration::from_secs(1), async {
        while let Some(result) = readers.join_next().await {
            result
                .map_err(|error| error.to_string())?
                .map_err(|error| error.to_string())?;
        }
        Ok::<_, String>(())
    })
    .await;
    match drained {
        Ok(result) => result?,
        Err(_) => {
            readers.abort_all();
            lock(&state).error = Some("Output drain timed out after the command exited; a detached descendant may still hold its pipes".into());
        }
    }
    Ok(outcome)
}

#[cfg(test)]
mod tests {
    use super::*;
    async fn finished(manager: &JobManager, id: &str) -> Value {
        tokio::time::timeout(Duration::from_secs(10), async {
            loop {
                let status = manager.status(id, 0, MAX_PAGE).unwrap();
                if status["status"] != "running" {
                    return status;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("job completes")
    }
    #[tokio::test]
    async fn orbit_jobs_capture_exit_status_and_page_output() {
        let mut manager = JobManager::default();
        let job = manager
            .start(
                std::env::temp_dir(),
                "echo first && echo second".into(),
                None,
            )
            .unwrap();
        let id = job["job_id"].as_str().unwrap();
        let status = finished(&manager, id).await;
        assert_eq!(status["status"], "completed");
        assert_eq!(status["exit_code"], 0);
        let page = manager.status(id, 0, 3).unwrap();
        assert_eq!(page["output"], "fir");
        assert_eq!(page["next_offset"], 3);
        assert_eq!(page["has_more"], true);
        assert!(manager.status(id, 10000, 3).is_err());
        let failed = manager
            .start(std::env::temp_dir(), "exit 7".into(), None)
            .unwrap();
        let failed = finished(&manager, failed["job_id"].as_str().unwrap()).await;
        assert_eq!(failed["status"], "failed");
        assert_eq!(failed["exit_code"], 7);
    }
    #[tokio::test]
    async fn orbit_jobs_cancel_before_spawn_and_validate_limits() {
        let mut manager = JobManager::default();
        assert!(manager
            .start(std::env::temp_dir(), "echo x".into(), Some(0))
            .is_err());
        assert!(manager
            .start(std::env::temp_dir(), " ".into(), None)
            .is_err());
        let job = manager
            .start(std::env::temp_dir(), "echo should-not-run".into(), None)
            .unwrap();
        let id = job["job_id"].as_str().unwrap();
        manager.cancel(id).unwrap();
        assert_eq!(finished(&manager, id).await["status"], "cancelled");
        assert!(manager.cancel("unknown-job").is_err());
    }
    #[tokio::test]
    async fn orbit_jobs_preflight_failure_prevents_execution_and_task_captures_result() {
        let mut manager = JobManager::default();
        let job = manager
            .start_with_preflight(
                std::env::temp_dir(),
                "echo never".into(),
                None,
                Box::new(|| Err("baseline unavailable".into())),
            )
            .unwrap();
        let result = finished(&manager, job["job_id"].as_str().unwrap()).await;
        assert_eq!(result["status"], "failed");
        assert_eq!(result["error"], "baseline unavailable");
        assert_eq!(result["output"], "");
        let job = manager
            .start_task(
                std::env::temp_dir(),
                "review".into(),
                Box::new(|| Ok("diff result".into())),
            )
            .unwrap();
        let result = finished(&manager, job["job_id"].as_str().unwrap()).await;
        assert_eq!(result["status"], "completed");
        assert_eq!(result["output"], "diff result");
    }

    #[tokio::test]
    async fn orbit_jobs_capacity_and_drop_cancel_owned_work() {
        let mut manager = JobManager::default();
        for _ in 0..MAX_RUNNING {
            manager
                .start(std::env::temp_dir(), "echo queued".into(), None)
                .unwrap();
        }
        assert!(manager
            .start(std::env::temp_dir(), "echo excess".into(), None)
            .is_err());
        let state = manager.jobs[0].state.clone();
        drop(manager);
        tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                if lock(&state).status != "running" {
                    break;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        assert_eq!(lock(&state).status, "cancelled");
    }

    #[tokio::test]
    async fn orbit_jobs_output_capture_is_bounded() {
        let state = Arc::new(Mutex::new(JobState::default()));
        let bytes = vec![b'x'; MAX_OUTPUT + 100];
        capture(bytes.as_slice(), state.clone()).await.unwrap();
        let state = lock(&state);
        assert_eq!(state.output.len(), MAX_OUTPUT);
        assert_eq!(state.observed_bytes, (MAX_OUTPUT + 100) as u64);
    }
    #[cfg(unix)]
    #[tokio::test]
    async fn orbit_jobs_timeout_and_cancel_running_process_groups() {
        let mut manager = JobManager::default();
        let job = manager
            .start(std::env::temp_dir(), "sleep 20".into(), Some(30))
            .unwrap();
        assert_eq!(
            finished(&manager, job["job_id"].as_str().unwrap()).await["status"],
            "timed_out"
        );
        let job = manager
            .start(std::env::temp_dir(), "sleep 20 & wait".into(), None)
            .unwrap();
        let id = job["job_id"].as_str().unwrap();
        tokio::time::sleep(Duration::from_millis(30)).await;
        manager.cancel(id).unwrap();
        assert_eq!(finished(&manager, id).await["status"], "cancelled");
    }
}
