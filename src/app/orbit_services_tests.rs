use super::*;
fn params(command: &str) -> OrbitServiceStartParams {
    OrbitServiceStartParams {
        name: "web".into(),
        command: command.into(),
        cwd: None,
        ready_url: None,
        ready_log: Some("READY".into()),
        timeout_ms: Some(2000),
    }
}
async fn until(manager: &ServiceManager, key: &str, value: &str) -> Value {
    tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            let list = manager.list();
            if list[0][key] == value {
                return list[0].clone();
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("service transition")
}
#[tokio::test]
async fn start_list_stop_and_restart_by_name() {
    let mut manager = ServiceManager::default();
    manager
        .start(std::env::temp_dir(), params("echo READY; sleep 60"))
        .unwrap();
    let first = until(&manager, "ready", "ready").await;
    assert!(first["pid"].as_u64().is_some());
    manager
        .start(std::env::temp_dir(), params("echo READY; sleep 60"))
        .unwrap();
    let second = until(&manager, "ready", "ready").await;
    assert_ne!(first["pid"], second["pid"]);
    assert_eq!(manager.list().as_array().unwrap().len(), 1);
    manager.stop("web").unwrap();
    until(&manager, "status", "stopped").await;
    assert!(manager.stop("missing").is_err());
}
#[tokio::test]
async fn readiness_timeout_does_not_kill_service() {
    let mut manager = ServiceManager::default();
    let mut p = params("sleep 60");
    p.timeout_ms = Some(30);
    manager.start(std::env::temp_dir(), p).unwrap();
    let status = until(&manager, "ready", "timed_out").await;
    assert_eq!(status["status"], "running");
    manager.stop("web").unwrap();
    until(&manager, "status", "stopped").await;
}
#[tokio::test]
async fn capture_bounds_and_split_readiness_signal() {
    let state = Arc::new(Mutex::new(State {
        ready: "pending",
        ..State::default()
    }));
    let bytes = vec![b'x'; MAX_OUTPUT + 100];
    capture(bytes.as_slice(), state.clone(), None).await;
    {
        let s = lock(&state);
        assert_eq!(s.output.len(), MAX_OUTPUT);
        assert_eq!(s.observed, (MAX_OUTPUT + 100) as u64);
    }
    capture(&b"READY\n"[..], state.clone(), Some("READY".into())).await;
    assert_eq!(lock(&state).ready, "ready");
}
#[tokio::test]
async fn shutdown_cancels_services() {
    let mut manager = ServiceManager::default();
    manager
        .start(std::env::temp_dir(), params("echo READY; sleep 60"))
        .unwrap();
    let running = until(&manager, "ready", "ready").await;
    let pid = running["pid"].as_u64().unwrap() as i32;
    let service = manager.services.get_mut("web").unwrap();
    let state = service.state.clone();
    drop(manager);
    tokio::time::timeout(Duration::from_secs(5), async {
        while lock(&state).status != "stopped" {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    #[cfg(unix)]
    assert_eq!(unsafe { libc::kill(pid, 0) }, -1);
    #[cfg(not(unix))]
    let _ = pid;
}
#[tokio::test]
async fn http_service_survives_another_job_finishing() {
    let port = std::net::TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port();
    let url = format!("http://127.0.0.1:{port}");
    let mut manager = ServiceManager::default();
    let mut p = params(&format!(
        "python3 -u -m http.server {port} --bind 127.0.0.1"
    ));
    p.ready_log = None;
    p.ready_url = Some(url.clone());
    p.timeout_ms = Some(5000);
    manager.start(std::env::temp_dir(), p).unwrap();
    until(&manager, "ready", "ready").await;
    let mut jobs = crate::app::orbit_jobs::JobManager::default();
    let job = jobs
        .start(std::env::temp_dir(), "echo finished".into(), None)
        .unwrap();
    tokio::time::timeout(Duration::from_secs(5), async {
        while jobs
            .status(job["job_id"].as_str().unwrap(), 0, 1024)
            .unwrap()["status"]
            == "running"
        {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    let result = tokio::process::Command::new("curl")
        .args(["--fail", "--silent", "--max-time", "2", &url])
        .output()
        .await
        .unwrap();
    assert!(result.status.success());
    manager.stop("web").unwrap();
    until(&manager, "status", "stopped").await;
    let result = tokio::process::Command::new("curl")
        .args(["--fail", "--silent", "--max-time", "1", &url])
        .output()
        .await
        .unwrap();
    assert!(!result.status.success());
}

#[cfg(unix)]
#[tokio::test]
async fn background_server_survives_launcher_exit_until_stop_or_shutdown() {
    for shutdown in [false, true] {
        let port = std::net::TcpListener::bind("127.0.0.1:0")
            .unwrap()
            .local_addr()
            .unwrap()
            .port();
        let url = format!("http://127.0.0.1:{port}");
        let mut manager = ServiceManager::default();
        let mut p = params(&format!(
            "python3 -u -m http.server {port} --bind 127.0.0.1 >/dev/null 2>&1 &"
        ));
        p.ready_log = None;
        p.ready_url = Some(url.clone());
        p.timeout_ms = Some(5000);
        manager.start(std::env::temp_dir(), p).unwrap();
        until(&manager, "status", "launcher_exited").await;
        until(&manager, "ready", "ready").await;
        // Descendants continue to serve after their launcher has been reaped.
        let response = tokio::process::Command::new("curl")
            .args(["--fail", "--silent", "--max-time", "2", &url])
            .output()
            .await
            .unwrap();
        assert!(response.status.success());
        if shutdown {
            manager.shutdown().await;
            assert_eq!(manager.list(), json!([]));
        } else {
            manager.stop("web").unwrap();
            until(&manager, "status", "stopped").await;
        }
        let response = tokio::process::Command::new("curl")
            .args(["--fail", "--silent", "--max-time", "1", &url])
            .output()
            .await
            .unwrap();
        assert!(!response.status.success());
    }
}

#[tokio::test]
async fn heavy_output_retains_exact_tail_across_ring_wraps() {
    let state = Arc::new(Mutex::new(State::default()));
    let bytes: Vec<_> = (0..MAX_OUTPUT * 8 + 123).map(|i| (i % 251) as u8).collect();
    capture(bytes.as_slice(), state.clone(), None).await;
    let s = lock(&state);
    assert_eq!(s.observed, bytes.len() as u64);
    assert_eq!(s.output.len(), MAX_OUTPUT);
    assert!(s
        .output
        .iter()
        .copied()
        .eq(bytes[bytes.len() - MAX_OUTPUT..].iter().copied()));
}

#[cfg(unix)]
#[tokio::test]
async fn empty_service_groups_exit_and_release_ownership() {
    for command in ["true", "sleep 0.3 &"] {
        let mut manager = ServiceManager::default();
        let mut p = params(command);
        p.ready_log = None;
        manager.start(std::env::temp_dir(), p).unwrap();
        let exited = until(&manager, "status", "exited").await;
        assert!(exited["pid"].is_null(), "no retained group identity");
        manager.stop("web").unwrap();
        assert_eq!(manager.list()[0]["status"], "exited");
        manager.shutdown().await;
        assert_eq!(manager.list(), json!([]));
    }
}

#[cfg(unix)]
#[tokio::test]
async fn poll_failure_cleans_up_on_the_blocking_supervisor() {
    use std::sync::atomic::{AtomicU32, Ordering};
    let state = Arc::new(Mutex::new(State::default()));
    let pid = Arc::new(AtomicU32::new(0));
    let recorded_pid = pid.clone();
    let async_thread = std::thread::current().id();
    let (_sender, receiver) = watch::channel(false);
    let result = run_with_poll(
        std::env::temp_dir(),
        params("sleep 60"),
        Duration::from_secs(2),
        state.clone(),
        receiver,
        move |process| {
            assert_ne!(std::thread::current().id(), async_thread);
            recorded_pid.store(process.pid(), Ordering::SeqCst);
            Err(std::io::Error::other("injected poll failure"))
        },
    )
    .await;
    assert_eq!(result.unwrap_err(), "injected poll failure");
    assert_eq!(lock(&state).status, "failed");
    assert!(lock(&state).pid.is_none());
    // The worker has already killed/reaped its child before delivering error.
    let pid = pid.load(Ordering::SeqCst);
    assert_ne!(pid, 0);
    let probe = tokio::process::Command::new("kill")
        .args(["-0", &pid.to_string()])
        .output()
        .await
        .unwrap();
    assert!(!probe.status.success());
}
