//! Server-owned services, independent of finite Orbit jobs.
use crate::api::schema::OrbitServiceStartParams;
use serde_json::{json, Value};
use std::collections::BTreeMap;
use std::path::PathBuf;
use std::process::Stdio;
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant};
use tokio::io::{AsyncRead, AsyncReadExt};
use tokio::sync::{oneshot, watch};

const MAX_OUTPUT: usize = 4 * 1024 * 1024;
#[derive(Default)]
pub(crate) struct ServiceManager {
    services: BTreeMap<String, Service>,
}
struct Service {
    command: String,
    cwd: PathBuf,
    created: Instant,
    state: Arc<Mutex<State>>,
    cancel: watch::Sender<bool>,
    done: oneshot::Receiver<()>,
}
#[derive(Default)]
struct State {
    pid: Option<u32>,
    status: &'static str,
    ready: &'static str,
    output: Vec<u8>,
    observed: u64,
    error: Option<String>,
}
fn lock(state: &Mutex<State>) -> MutexGuard<'_, State> {
    state.lock().unwrap_or_else(|e| e.into_inner())
}
impl ServiceManager {
    pub(crate) fn start(
        &mut self,
        cwd: PathBuf,
        params: OrbitServiceStartParams,
    ) -> Result<Value, String> {
        if params.name.trim().is_empty() || params.name.len() > 128 || params.name.contains('\0') {
            return Err("name must contain 1..128 bytes without NUL".into());
        }
        if params.command.trim().is_empty() || params.command.contains('\0') {
            return Err("command must be non-empty without NUL".into());
        }
        if params.ready_url.is_some() && params.ready_log.is_some() {
            return Err("choose ready_url or ready_log".into());
        }
        if params.ready_log.as_ref().is_some_and(|s| s.is_empty())
            || params
                .ready_url
                .as_ref()
                .is_some_and(|s| !s.starts_with("http://") && !s.starts_with("https://"))
        {
            return Err("readiness requires a non-empty log string or HTTP(S) URL".into());
        }
        let timeout = params.timeout_ms.unwrap_or(30_000);
        if !(1..=86_400_000).contains(&timeout) {
            return Err("timeout_ms must be between 1 and 86400000".into());
        }
        let runtime = tokio::runtime::Handle::try_current()
            .map_err(|_| "services require the server runtime")?;
        if self.services.len() >= 32 && !self.services.contains_key(&params.name) {
            return Err("service limit reached (32)".into());
        }
        let previous = self.services.remove(&params.name);
        let cwd = params
            .cwd
            .as_ref()
            .map_or(cwd.clone(), |path| cwd.join(path));
        let state = Arc::new(Mutex::new(State {
            status: "starting",
            ready: if params.ready_url.is_some() || params.ready_log.is_some() {
                "pending"
            } else {
                "not_requested"
            },
            ..State::default()
        }));
        let (cancel, receiver) = watch::channel(false);
        let (finished, done) = oneshot::channel();
        self.services.insert(
            params.name.clone(),
            Service {
                command: params.command.clone(),
                cwd: cwd.clone(),
                created: Instant::now(),
                state: state.clone(),
                cancel,
                done,
            },
        );
        runtime.spawn(async move {
            if let Some(old) = previous {
                let _ = old.cancel.send(true);
                let _ = old.done.await;
            }
            let result = run(
                cwd,
                params,
                Duration::from_millis(timeout),
                state.clone(),
                receiver,
            )
            .await;
            if let Err(error) = result {
                let mut s = lock(&state);
                s.status = "failed";
                s.error = Some(error);
            }
            let _ = finished.send(());
        });
        Ok(self.list())
    }
    pub(crate) async fn shutdown(&mut self) {
        let services = std::mem::take(&mut self.services);
        for service in services.values() {
            let _ = service.cancel.send(true);
        }
        for (_, service) in services {
            let _ = service.done.await;
        }
    }
    pub(crate) fn stop(&self, name: &str) -> Result<Value, String> {
        let service = self.services.get(name).ok_or("service not found")?;
        let _ = service.cancel.send(true);
        Ok(json!({"name":name,"stop_requested":true}))
    }
    pub(crate) fn list(&self) -> Value {
        json!(self.services.iter().map(|(name, service)| {
            let s = lock(&service.state);
            let tail = &s.output[s.output.len().saturating_sub(64 * 1024)..];
            let output = String::from_utf8_lossy(tail);
            let lines: Vec<_> = output.lines().rev().take(20).collect();
            json!({"name":name,"command":service.command,"cwd":service.cwd,"pid":s.pid,
                "uptime_ms":service.created.elapsed().as_millis(),"status":s.status,"ready":s.ready,
                "last_output_lines":lines.into_iter().rev().collect::<Vec<_>>(),"error":s.error,
                "captured_bytes":s.output.len(),"observed_bytes":s.observed,"output_limit_bytes":MAX_OUTPUT,"output_truncated":s.observed > s.output.len() as u64})
        }).collect::<Vec<_>>())
    }
}
impl Drop for ServiceManager {
    fn drop(&mut self) {
        for service in self.services.values() {
            let _ = service.cancel.send(true);
        }
    }
}
async fn capture(
    mut reader: impl AsyncRead + Unpin,
    state: Arc<Mutex<State>>,
    ready_log: Option<String>,
) {
    let mut buffer = [0; 8192];
    let mut matching = Vec::new();
    while let Ok(n) = reader.read(&mut buffer).await {
        if n == 0 {
            break;
        }
        let mut s = lock(&state);
        s.observed = s.observed.saturating_add(n as u64);
        let excess = (s.output.len() + n).saturating_sub(MAX_OUTPUT);
        s.output.drain(..excess);
        s.output.extend_from_slice(&buffer[..n]);
        if let Some(pattern) = &ready_log {
            matching.extend_from_slice(&buffer[..n]);
            if matching
                .windows(pattern.len())
                .any(|w| w == pattern.as_bytes())
                && s.ready == "pending"
            {
                s.ready = "ready";
            }
            let discard = matching.len().saturating_sub(pattern.len());
            matching.drain(..discard);
        }
    }
}
async fn readiness(url: Option<String>, state: Arc<Mutex<State>>, timeout: Duration) {
    let deadline = Instant::now() + timeout;
    loop {
        if lock(&state).ready != "pending" {
            return;
        }
        if Instant::now() >= deadline {
            lock(&state).ready = "timed_out";
            return;
        }
        if let Some(url) = &url {
            let result = tokio::process::Command::new("curl")
                .args(["--fail", "--silent", "--max-time", "1", "--", url])
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .kill_on_drop(true)
                .status()
                .await;
            if result.is_ok_and(|s| s.success()) {
                lock(&state).ready = "ready";
                return;
            }
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}
async fn run(
    cwd: PathBuf,
    params: OrbitServiceStartParams,
    timeout: Duration,
    state: Arc<Mutex<State>>,
    mut cancel: watch::Receiver<bool>,
) -> Result<(), String> {
    if *cancel.borrow() {
        lock(&state).status = "stopped";
        return Ok(());
    }
    // Process creation and filesystem resolution are kept off the application event loop.
    let command = params.command;
    let (mut child, mut guard) = tokio::task::spawn_blocking(move || {
        let mut process = crate::platform::detached_custom_command_process(&command);
        process
            .current_dir(cwd)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        crate::platform::configure_status_command(&mut process);
        let mut process = tokio::process::Command::from(process);
        process.kill_on_drop(true);
        let child = process.spawn()?;
        let guard = crate::platform::StatusCommandGuard::new(&child)?;
        Ok::<_, std::io::Error>((child, guard))
    })
    .await
    .map_err(|e| e.to_string())?
    .map_err(|e| e.to_string())?;
    {
        let mut s = lock(&state);
        s.pid = child.id();
        s.status = "running";
    }
    let mut readers = tokio::task::JoinSet::new();
    if let Some(stdout) = child.stdout.take() {
        readers.spawn(capture(stdout, state.clone(), params.ready_log.clone()));
    }
    if let Some(stderr) = child.stderr.take() {
        readers.spawn(capture(stderr, state.clone(), params.ready_log));
    }
    let readiness = readiness(params.ready_url, state.clone(), timeout);
    tokio::pin!(readiness);
    let mut checked = false;
    let status = loop {
        tokio::select! {
            _ = cancel.changed() => break "stopped",
            result = child.wait() => { result.map_err(|e| e.to_string())?; break "exited"; },
            _ = &mut readiness, if !checked => checked = true,
        }
    };
    guard.terminate();
    let _ = child.wait().await;
    let _ = tokio::time::timeout(Duration::from_secs(1), async {
        while readers.join_next().await.is_some() {}
    })
    .await;
    readers.abort_all();
    lock(&state).status = status;
    Ok(())
}

#[cfg(test)]
#[path = "orbit_services_tests.rs"]
mod tests;
