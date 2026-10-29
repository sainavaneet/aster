//! `Jev` against a local HTTP server: status handling, malformed and oversized
//! bodies, refused connections, curl's own timeout, header-file cleanup, and a
//! key that only ever travels in the header file. Uses the real `curl`.
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use super::super::interrupt::{self, InterruptInput};
use super::super::provider::{DecisionError, DecisionProvider};
use super::*;

const KEY: &str = "sk-test-local-key-0123456789";

/// What the server does with one request.
#[derive(Clone)]
enum Reply {
    Status(u16, Vec<u8>),
    /// Accepts and never answers within the test.
    Hang(Duration),
}

struct Server {
    endpoint: String,
    requests: Arc<Mutex<Vec<String>>>,
}

fn read_request(stream: &mut TcpStream) -> String {
    let mut reader = BufReader::new(stream.try_clone().unwrap());
    let mut head = String::new();
    let mut length = 0;
    loop {
        let mut line = String::new();
        if reader.read_line(&mut line).unwrap_or(0) == 0 || line == "\r\n" {
            break;
        }
        if let Some(value) = line.to_ascii_lowercase().strip_prefix("content-length:") {
            length = value.trim().parse().unwrap_or(0);
        }
        head.push_str(&line);
    }
    let mut body = vec![0; length];
    let _ = reader.read_exact(&mut body);
    head + "\r\n" + &String::from_utf8_lossy(&body)
}

/// Serves `replies` in order, one connection each.
fn serve(replies: Vec<Reply>) -> Server {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let endpoint = format!("http://{}", listener.local_addr().unwrap());
    let requests = Arc::new(Mutex::new(Vec::new()));
    let seen = Arc::clone(&requests);
    std::thread::spawn(move || {
        for reply in replies {
            let Ok((mut stream, _)) = listener.accept() else {
                return;
            };
            let request = read_request(&mut stream);
            seen.lock().unwrap().push(request);
            match reply {
                Reply::Status(status, body) => {
                    let head = format!(
                        "HTTP/1.1 {status} Test\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                        body.len()
                    );
                    let _ = stream.write_all(head.as_bytes());
                    let _ = stream.write_all(&body);
                }
                Reply::Hang(duration) => std::thread::sleep(duration),
            }
        }
    });
    Server { endpoint, requests }
}

fn key_file() -> PathBuf {
    let path = std::env::temp_dir().join(format!(
        "aster-jev-http-key-{}-{:?}",
        std::process::id(),
        std::thread::current().id()
    ));
    let _ = std::fs::remove_file(&path);
    std::fs::write(&path, KEY).unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
    }
    path
}

fn jev(endpoint: &str, timeout: Duration) -> Jev {
    Jev {
        settings: JevSettings {
            endpoint: endpoint.into(),
            model: "jev-latest".into(),
            api_key_env: "ASTER_TEST_JEV_HTTP_KEY_THAT_IS_NEVER_SET".into(),
            api_key_file: Some(key_file()),
            timeout,
        },
    }
}

/// No header file of this process is left in the temporary directory.
fn assert_header_files_removed() {
    let prefix = format!("aster-decisions-{}-", std::process::id());
    let left: Vec<_> = std::fs::read_dir(std::env::temp_dir())
        .unwrap()
        .filter_map(Result::ok)
        .map(|entry| entry.file_name().to_string_lossy().into_owned())
        .filter(|name| name.starts_with(&prefix))
        .collect();
    assert!(left.is_empty(), "header files left behind: {left:?}");
}

fn fixture(name: &str) -> Vec<u8> {
    std::fs::read(format!(
        "{}/tests/fixtures/decisions/{name}.json",
        env!("CARGO_MANIFEST_DIR")
    ))
    .unwrap()
}

fn interrupt_request() -> DecisionRequest {
    interrupt::request(&InterruptInput {
        lead_state: "idle",
        screen_tail: "done".into(),
        outcomes: vec!["completed".into()],
        blocked: false,
    })
}

#[test]
fn check_and_evaluate_succeed_and_the_key_travels_only_in_the_header() {
    let server = serve(vec![
        Reply::Status(200, fixture("models")),
        Reply::Status(200, fixture("interrupt_idle")),
    ]);
    let jev = jev(&server.endpoint, Duration::from_secs(5));
    let check = jev.check();
    assert_eq!(check["ok"], true, "{check}");
    assert_eq!(check["model_available"], true);
    assert!(!check.to_string().contains(KEY));
    let answers = jev.evaluate(interrupt_request()).unwrap();
    assert!(answers.0.contains_key("deliver_now"));
    let requests = server.requests.lock().unwrap();
    assert!(requests[0].starts_with("GET /v1/models"), "{}", requests[0]);
    assert!(requests[0].contains(&format!("Authorization: Bearer {KEY}")));
    assert!(
        requests[1].starts_with("POST /v1/systemone"),
        "{}",
        requests[1]
    );
    let body = requests[1].split("\r\n\r\n").nth(1).unwrap();
    let body: Value = serde_json::from_str(body).unwrap();
    assert_eq!(body["model"], "jev-latest");
    assert!(body["questions"]["deliver_now"].is_object());
    assert!(!body.to_string().contains(KEY), "key never in the body");
    drop(requests);
    assert_header_files_removed();
}

#[test]
fn error_statuses_and_malformed_bodies_fail_closed_without_echoing_the_key() {
    let unauthorized = br#"{"detail":{"error_type":"auth","message":"invalid key"}}"#.to_vec();
    let server = serve(vec![
        Reply::Status(401, unauthorized),
        Reply::Status(429, b"{}".to_vec()),
        Reply::Status(503, b"<html>down</html>".to_vec()),
        Reply::Status(200, b"not json".to_vec()),
        Reply::Status(200, br#"{"answers":{}}"#.to_vec()),
    ]);
    let jev = jev(&server.endpoint, Duration::from_secs(5));
    let mut errors = Vec::new();
    for _ in 0..5 {
        errors.push(jev.evaluate(interrupt_request()).unwrap_err());
    }
    assert!(
        matches!(
            &errors[0],
            DecisionError::Http { status: 401, error_type: Some(kind), .. } if kind == "auth"
        ),
        "{:?}",
        errors[0]
    );
    assert!(matches!(errors[1], DecisionError::Http { status: 429, .. }));
    assert!(matches!(errors[2], DecisionError::Http { status: 503, .. }));
    assert!(
        matches!(errors[3], DecisionError::Malformed(_)),
        "{:?}",
        errors[3]
    );
    assert!(
        matches!(errors[4], DecisionError::Malformed(_)),
        "unanswered question"
    );
    for error in &errors {
        assert!(!error.to_string().contains(KEY), "{error}");
    }
    assert_header_files_removed();
}

#[test]
fn an_oversized_response_is_refused() {
    let mut huge = b"{\"answers\":{\"x\":\"".to_vec();
    huge.extend(std::iter::repeat_n(b'a', 2 * 1024 * 1024));
    huge.extend_from_slice(b"\"}}");
    let server = serve(vec![Reply::Status(200, huge)]);
    let error = jev(&server.endpoint, Duration::from_secs(10))
        .evaluate(interrupt_request())
        .unwrap_err();
    let DecisionError::Transport(message) = &error else {
        panic!("{error:?}");
    };
    assert!(
        message.to_ascii_lowercase().contains("maximum file size"),
        "{message}"
    );
    assert_header_files_removed();
}

#[test]
fn a_refused_connection_is_a_transport_error() {
    let port = TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port();
    let error = jev(&format!("http://127.0.0.1:{port}"), Duration::from_secs(5))
        .evaluate(interrupt_request())
        .unwrap_err();
    assert!(matches!(error, DecisionError::Transport(_)), "{error:?}");
    assert_header_files_removed();
}

/// curl's `--max-time` ends a call the server never answers; meanwhile its
/// argument list, as any local user could read it, holds no key.
#[cfg(unix)]
#[test]
fn a_silent_server_times_out_and_curl_argv_never_holds_the_key() {
    let server = serve(vec![Reply::Hang(Duration::from_secs(5))]);
    let jev = jev(&server.endpoint, Duration::from_millis(1_500));
    let call = std::thread::spawn(move || jev.evaluate(interrupt_request()));
    let deadline = Instant::now() + Duration::from_secs(3);
    let mut argv = String::new();
    while argv.is_empty() && Instant::now() < deadline {
        let pids = std::process::Command::new("pgrep")
            .args(["-P", &std::process::id().to_string(), "curl"])
            .output()
            .unwrap();
        for pid in String::from_utf8_lossy(&pids.stdout).split_whitespace() {
            let args = std::process::Command::new("ps")
                .args(["-o", "args=", "-p", pid])
                .output()
                .unwrap();
            argv.push_str(&String::from_utf8_lossy(&args.stdout));
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    assert!(argv.contains("curl"), "curl not observed: {argv:?}");
    assert!(!argv.contains(KEY), "key in argv: {argv}");
    assert!(argv.contains(".hdr"), "header file referenced: {argv}");
    let error = call.join().unwrap().unwrap_err();
    assert!(matches!(error, DecisionError::Timeout), "{error:?}");
    assert_header_files_removed();
}
