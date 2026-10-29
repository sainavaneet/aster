//! TypeSafe AI's Jev over HTTPS through `curl`. The API key is read on the
//! decision thread for each call and handed to curl in a header file created
//! with mode 0600 and removed afterwards (`-H @file`); it never appears in argv,
//! the environment of other processes, logs, the journal, or activity. The body
//! goes on stdin. Every failure is an error the caller treats as "ask the user".
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Child, Stdio};
use std::time::{Duration, Instant};

use serde_json::{json, Value};

use super::provider::{
    http_error, parse_answers, DecisionAnswers, DecisionError, DecisionProvider, DecisionRequest,
};

/// Responses larger than this are refused by curl.
const MAX_RESPONSE_BYTES: u64 = 1024 * 1024;
const HTTP_CODE_MARKER: &str = "\n@@aster-http-code:";
/// curl's exit status for `--max-time` expiry.
const CURL_TIMEOUT_EXIT: i32 = 28;

/// Where the key comes from and where requests go; holds no key material.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct JevSettings {
    pub(crate) endpoint: String,
    pub(crate) model: String,
    pub(crate) api_key_env: String,
    pub(crate) api_key_file: Option<PathBuf>,
    pub(crate) timeout: Duration,
}

pub(crate) struct Jev {
    pub(crate) settings: JevSettings,
}

/// The key must be one header-safe token: printable ASCII without spaces.
fn valid_key(key: &str) -> bool {
    !key.is_empty() && key.len() <= 4096 && key.chars().all(|ch| ch.is_ascii_graphic())
}

/// A key file must be a regular file that neither group nor others can read.
pub(crate) fn check_key_file(path: &Path) -> Result<(), String> {
    let metadata = std::fs::metadata(path)
        .map_err(|error| format!("cannot read api_key_file {}: {error}", path.display()))?;
    if !metadata.is_file() {
        return Err(format!(
            "api_key_file {} is not a regular file",
            path.display()
        ));
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = metadata.permissions().mode();
        if mode & 0o044 != 0 {
            return Err(format!(
                "api_key_file {} is readable by group or others (mode {:o}); run chmod 600 on it",
                path.display(),
                mode & 0o777
            ));
        }
    }
    Ok(())
}

impl JevSettings {
    /// The key from `api_key_env`, else from `api_key_file`.
    fn api_key(&self) -> Result<String, DecisionError> {
        if let Some(key) = std::env::var(&self.api_key_env)
            .ok()
            .map(|key| key.trim().to_owned())
            .filter(|key| !key.is_empty())
        {
            return valid_key(&key).then_some(key).ok_or_else(|| {
                DecisionError::Key(format!("{} holds an invalid key", self.api_key_env))
            });
        }
        let Some(path) = &self.api_key_file else {
            return Err(DecisionError::Key(format!(
                "{} is unset and no api_key_file is configured",
                self.api_key_env
            )));
        };
        check_key_file(path).map_err(DecisionError::Key)?;
        let text = std::fs::read_to_string(path).map_err(|error| {
            DecisionError::Key(format!(
                "cannot read api_key_file {}: {error}",
                path.display()
            ))
        })?;
        let key = text.trim();
        if valid_key(key) {
            Ok(key.to_owned())
        } else {
            Err(DecisionError::Key(format!(
                "api_key_file {} holds an invalid key",
                path.display()
            )))
        }
    }
}

/// A private temporary file removed on drop.
struct HeaderFile(PathBuf);

impl HeaderFile {
    fn create(contents: &str) -> std::io::Result<Self> {
        let mut last = None;
        for _ in 0..8 {
            let mut random = [0u8; 12];
            getrandom::fill(&mut random).map_err(std::io::Error::other)?;
            let suffix: String = random.iter().map(|byte| format!("{byte:02x}")).collect();
            let path = std::env::temp_dir().join(format!(
                "aster-decisions-{}-{suffix}.hdr",
                std::process::id()
            ));
            let mut options = std::fs::OpenOptions::new();
            options.write(true).create_new(true);
            #[cfg(unix)]
            {
                use std::os::unix::fs::OpenOptionsExt;
                options.mode(0o600);
            }
            match options.open(&path) {
                Ok(mut file) => {
                    let header = Self(path);
                    file.write_all(contents.as_bytes())?;
                    return Ok(header);
                }
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
                    last = Some(error);
                }
                Err(error) => return Err(error),
            }
        }
        Err(last.unwrap_or_else(|| std::io::Error::other("no temporary name")))
    }
}

impl Drop for HeaderFile {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

/// HTTP status and body of one finished curl call.
struct Response {
    status: u16,
    body: Vec<u8>,
}

/// The curl arguments for `url`; the key is only referenced through `header`.
pub(crate) fn curl_args(url: &str, header: &Path, timeout: Duration, body: bool) -> Vec<String> {
    let mut args = vec![
        // Ignore ~/.curlrc so no user option can trace headers.
        "-q".to_owned(),
        "--silent".into(),
        "--show-error".into(),
        "--max-time".into(),
        format!("{:.3}", timeout.as_secs_f64()),
        "--max-filesize".into(),
        MAX_RESPONSE_BYTES.to_string(),
        "--header".into(),
        format!("@{}", header.display()),
        "--write-out".into(),
        format!("{HTTP_CODE_MARKER}%{{http_code}}"),
    ];
    if url.starts_with("https://") {
        args.extend(["--proto".into(), "=https".into()]);
    }
    if body {
        args.extend([
            "--header".into(),
            "Content-Type: application/json".into(),
            "--data-binary".into(),
            "@-".into(),
        ]);
    }
    args.push(url.into());
    args
}

/// Splits curl's stdout into the body and the `--write-out` status code.
fn split_status(stdout: &[u8]) -> Option<(u16, Vec<u8>)> {
    let marker = HTTP_CODE_MARKER.as_bytes();
    let at = stdout
        .windows(marker.len())
        .rposition(|window| window == marker)?;
    let code = std::str::from_utf8(&stdout[at + marker.len()..])
        .ok()?
        .trim()
        .parse()
        .ok()?;
    Some((code, stdout[..at].to_vec()))
}

/// Writes the request body to the child's stdin and closes it. When the write
/// fails (curl exited or closed its input early) the child is killed and reaped,
/// so no zombie outlives the call, and curl's own error is reported if it gave one.
fn write_body(mut child: Child, body: &[u8]) -> Result<Child, DecisionError> {
    let Some(mut stdin) = child.stdin.take() else {
        return Ok(child);
    };
    let written = stdin.write_all(body);
    drop(stdin);
    let Err(error) = written else {
        return Ok(child);
    };
    let _ = child.kill();
    let stderr = child
        .wait_with_output()
        .map(|output| String::from_utf8_lossy(&output.stderr).trim().to_owned())
        .unwrap_or_default();
    let reason = if stderr.is_empty() {
        format!("cannot send the request to curl: {error}")
    } else {
        stderr
    };
    Err(DecisionError::Transport(super::journal::bounded(
        &reason, 200,
    )))
}

impl Jev {
    fn call(&self, path: &str, body: Option<Vec<u8>>) -> Result<Response, DecisionError> {
        let key = self.settings.api_key()?;
        let header = HeaderFile::create(&format!("Authorization: Bearer {key}\n"))
            .map_err(|error| DecisionError::Transport(format!("header file: {error}")))?;
        drop(key);
        let url = format!("{}{path}", self.settings.endpoint);
        let timeout = self.settings.timeout;
        let mut command = crate::noninteractive_process::curl_command();
        command
            .args(curl_args(&url, &header.0, timeout, body.is_some()))
            .stdin(if body.is_some() {
                Stdio::piped()
            } else {
                Stdio::null()
            })
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        let child = command
            .spawn()
            .map_err(|error| DecisionError::Transport(format!("cannot run curl: {error}")))?;
        let child = write_body(child, body.as_deref().unwrap_or_default())?;
        // curl enforces --max-time itself; wait_with_output drains both pipes.
        let output = child
            .wait_with_output()
            .map_err(|error| DecisionError::Transport(error.to_string()))?;
        drop(header);
        // A failed curl (oversized body, reset connection) may still print a
        // status line; its exit status decides.
        let status = split_status(&output.stdout)
            .filter(|(status, _)| *status != 0 && output.status.success());
        let Some((status, body)) = status else {
            // curl exit 28 is its own timeout.
            if output.status.code() == Some(CURL_TIMEOUT_EXIT) {
                return Err(DecisionError::Timeout);
            }
            let stderr = String::from_utf8_lossy(&output.stderr);
            return Err(DecisionError::Transport(super::journal::bounded(
                stderr.trim(),
                200,
            )));
        };
        Ok(Response { status, body })
    }

    /// `GET /v1/models`: whether the key and endpoint work, without the key.
    pub(crate) fn check(&self) -> Value {
        let started = Instant::now();
        let result = self.call("/v1/models", None).and_then(|response| {
            if !(200..300).contains(&response.status) {
                return Err(http_error(response.status, &response.body));
            }
            let value: Value = serde_json::from_slice(&response.body)
                .map_err(|error| DecisionError::Malformed(error.to_string()))?;
            let models: Vec<String> = value["models"]
                .as_array()
                .ok_or_else(|| DecisionError::Malformed("no models list".into()))?
                .iter()
                .filter_map(|model| model["name"].as_str().map(str::to_owned))
                .collect();
            Ok(models)
        });
        let latency_ms = started.elapsed().as_millis() as u64;
        match result {
            Ok(models) => json!({
                "ok": true,
                "provider": "jev",
                "endpoint": self.settings.endpoint,
                "model": self.settings.model,
                "model_available": models.contains(&self.settings.model),
                "models": models,
                "latency_ms": latency_ms,
            }),
            Err(error) => json!({
                "ok": false,
                "provider": "jev",
                "endpoint": self.settings.endpoint,
                "error": error.to_string(),
                "latency_ms": latency_ms,
            }),
        }
    }
}

impl DecisionProvider for Jev {
    fn name(&self) -> &'static str {
        "jev"
    }

    fn evaluate(&self, request: DecisionRequest) -> Result<DecisionAnswers, DecisionError> {
        let body = json!({
            "model": self.settings.model,
            "state": request.state,
            "questions": request.questions,
        });
        let body = serde_json::to_vec(&body)
            .map_err(|error| DecisionError::Malformed(error.to_string()))?;
        let response = self.call("/v1/systemone", Some(body))?;
        if !(200..300).contains(&response.status) {
            return Err(http_error(response.status, &response.body));
        }
        parse_answers(&response.body, Some(&request))
    }
}

#[cfg(test)]
#[path = "jev_http_tests.rs"]
mod http_tests;

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_file(name: &str, contents: &str, mode: u32) -> PathBuf {
        let path = std::env::temp_dir().join(format!(
            "aster-jev-key-{name}-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = std::fs::remove_file(&path);
        std::fs::write(&path, contents).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(mode)).unwrap();
        }
        let _ = mode;
        path
    }

    fn settings(file: Option<PathBuf>) -> JevSettings {
        JevSettings {
            endpoint: "https://api.typesafe.ai".into(),
            model: "jev-latest".into(),
            api_key_env: "ASTER_TEST_DECISIONS_KEY_THAT_IS_NEVER_SET".into(),
            api_key_file: file,
            timeout: Duration::from_secs(3),
        }
    }

    #[test]
    fn key_file_must_be_private_and_regular() {
        let private = temp_file("private", "  sk-test-123\n", 0o600);
        assert_eq!(
            settings(Some(private.clone())).api_key().unwrap(),
            "sk-test-123"
        );
        #[cfg(unix)]
        {
            let shared = temp_file("shared", "sk-test-123", 0o644);
            let error = settings(Some(shared.clone())).api_key().unwrap_err();
            let DecisionError::Key(message) = error else {
                panic!("{error:?}")
            };
            assert!(message.contains("readable by group or others"), "{message}");
            assert!(message.contains("chmod 600"), "{message}");
            assert!(!message.contains("sk-test"), "never echoes the key");
            let _ = std::fs::remove_file(shared);
        }
        let dir = std::env::temp_dir();
        let error = check_key_file(&dir).unwrap_err();
        assert!(error.contains("not a regular file"), "{error}");
        let invalid = temp_file("invalid", "two words", 0o600);
        assert!(matches!(
            settings(Some(invalid.clone())).api_key(),
            Err(DecisionError::Key(_))
        ));
        assert!(matches!(
            settings(None).api_key(),
            Err(DecisionError::Key(_))
        ));
        let _ = std::fs::remove_file(private);
        let _ = std::fs::remove_file(invalid);
    }

    #[test]
    fn curl_args_reference_the_header_file_and_never_the_key() {
        let header = HeaderFile::create("Authorization: Bearer sk-secret\n").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&header.0).unwrap().permissions().mode();
            assert_eq!(mode & 0o777, 0o600);
        }
        let args = curl_args(
            "https://api.typesafe.ai/v1/systemone",
            &header.0,
            Duration::from_millis(3_000),
            true,
        );
        assert_eq!(args[0], "-q");
        assert!(args.iter().all(|arg| !arg.contains("sk-secret")));
        assert!(args.contains(&format!("@{}", header.0.display())));
        assert!(args.windows(2).any(|pair| pair == ["--max-time", "3.000"]));
        assert!(args.windows(2).any(|pair| pair == ["--data-binary", "@-"]));
        assert!(args.windows(2).any(|pair| pair == ["--proto", "=https"]));
        let path = header.0.clone();
        drop(header);
        assert!(!path.exists(), "header file removed after the call");
    }

    /// A child that closes its input at once, as curl does when it fails early,
    /// then would linger: a failed body write must kill and reap it.
    #[cfg(unix)]
    #[test]
    fn failed_body_write_kills_and_reaps_the_child() {
        let child = std::process::Command::new("/bin/sh")
            .args(["-c", "echo early failure >&2; exec 0<&-; exec sleep 30"])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        let pid = child.id().to_string();
        let started = Instant::now();
        let Err(DecisionError::Transport(message)) = write_body(child, &vec![b'x'; 4 << 20]) else {
            panic!("the write into a closed pipe must fail");
        };
        assert!(
            started.elapsed() < Duration::from_secs(10),
            "child was waited on, not killed"
        );
        assert!(message.contains("early failure"), "{message}");
        // Reaped: the pid no longer names a process (not even a zombie).
        let alive = std::process::Command::new("/bin/kill")
            .args(["-0", &pid])
            .stderr(Stdio::null())
            .status()
            .unwrap()
            .success();
        assert!(!alive, "child {pid} still exists");
    }

    #[test]
    fn status_marker_is_split_from_the_body() {
        let stdout = format!("{{\"a\":1}}{HTTP_CODE_MARKER}200");
        let (status, body) = split_status(stdout.as_bytes()).unwrap();
        assert_eq!(status, 200);
        assert_eq!(body, b"{\"a\":1}");
        assert!(split_status(b"no marker").is_none());
    }
}
