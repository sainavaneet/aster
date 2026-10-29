//! Per-assignment report tokens. The ledger keeps only their SHA-256. The token
//! itself (16 random bytes in hex) is written, with the session socket, to a
//! private per-task file that `aster orbit agent-report` reads by task ID, so the
//! worker's report command never carries it. Tokens stop guessing and
//! cross-worker misreports; socket access itself remains trusted project access,
//! and the file is readable only by the user, the same trust level as the socket.
use std::collections::HashSet;
use std::fmt;
use std::io;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

const TOKEN_BYTES: usize = 16;

/// SHA-256 of a task's report token, hex encoded. Persisted in the ledger file only:
/// `Task` and `Tombstone` never serialize it, and `Debug` redacts it.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(transparent)]
pub(crate) struct TokenHash(pub(super) String);

impl fmt::Debug for TokenHash {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("TokenHash(<redacted>)")
    }
}

fn hex(bytes: &[u8]) -> String {
    use std::fmt::Write as _;
    bytes.iter().fold(String::new(), |mut out, byte| {
        let _ = write!(out, "{byte:02x}");
        out
    })
}

/// A fresh token from the operating system's CSPRNG.
pub(super) fn generate() -> Result<String, String> {
    let mut bytes = [0_u8; TOKEN_BYTES];
    getrandom::fill(&mut bytes)
        .map_err(|error| format!("Cannot generate report token: {error}"))?;
    Ok(hex(&bytes))
}

pub(super) fn hash(token: &str) -> TokenHash {
    TokenHash(hex(&Sha256::digest(token.as_bytes())))
}

/// Equal-length comparison without a data-dependent early return. Hash lengths are
/// fixed, so the length check reveals nothing about the token.
fn constant_time_eq(left: &[u8], right: &[u8]) -> bool {
    if left.len() != right.len() {
        return false;
    }
    left.iter()
        .zip(right)
        .fold(0_u8, |difference, (a, b)| difference | (a ^ b))
        == 0
}

/// Legacy tasks without a stored hash accept reports without a token.
pub(super) fn accepts(stored: Option<&TokenHash>, presented: Option<&str>) -> bool {
    let Some(stored) = stored else {
        return true;
    };
    // An absent token is compared like a wrong one.
    let presented = hash(presented.unwrap_or_default());
    constant_time_eq(stored.0.as_bytes(), presented.0.as_bytes())
}

/// Directory in the session data directory with one token file per open assignment.
pub(crate) const TOKEN_DIR_NAME: &str = "orbit-tokens";
/// Server task IDs are `task-<pid>-<nanos>-<seq>`; anything longer is not one.
const MAX_TASK_ID_BYTES: usize = 128;

/// The token directory of a session data directory.
pub(crate) fn token_dir(data_dir: &Path) -> PathBuf {
    data_dir.join(TOKEN_DIR_NAME)
}

/// Only server-shaped task IDs name a file, so a reported task ID can never
/// point outside the token directory.
fn is_task_file_name(task_id: &str) -> bool {
    !task_id.is_empty()
        && task_id.len() <= MAX_TASK_ID_BYTES
        && task_id
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-' || byte == b'_')
}

/// A task's report credentials as `aster orbit agent-report` reads them.
#[derive(PartialEq, Eq)]
pub(crate) struct TokenFile {
    pub(crate) token: String,
    /// The API socket of the session that issued the token.
    pub(crate) socket: Option<PathBuf>,
}

impl fmt::Debug for TokenFile {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("TokenFile")
            .field("token", &"<redacted>")
            .field("socket", &self.socket)
            .finish()
    }
}

/// Line one is the token, line two the socket path.
fn parse_token_file(text: &str) -> Option<TokenFile> {
    let mut lines = text.lines();
    let token = lines.next()?.trim();
    if token.is_empty() || !token.chars().all(|ch| ch.is_ascii_hexdigit()) {
        return None;
    }
    let socket = lines
        .next()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .map(PathBuf::from);
    Some(TokenFile {
        token: token.to_owned(),
        socket,
    })
}

/// The token file of `task_id` in `dir`, if it exists and is well formed.
pub(crate) fn read_token_file(dir: &Path, task_id: &str) -> Option<TokenFile> {
    if !is_task_file_name(task_id) {
        return None;
    }
    parse_token_file(&std::fs::read_to_string(dir.join(task_id)).ok()?)
}

/// The token directory must be a real directory (not a symlink to somewhere
/// else) that only the user can open.
fn check_private_dir(dir: &Path) -> io::Result<()> {
    let metadata = std::fs::symlink_metadata(dir)?;
    if !metadata.is_dir() {
        return Err(io::Error::other(format!(
            "{} is not a directory (a symlink is refused)",
            dir.display()
        )));
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = metadata.permissions().mode() & 0o777;
        if mode & 0o077 != 0 {
            return Err(io::Error::other(format!(
                "{} is open to other users (mode {mode:o}); run chmod 700 on it",
                dir.display()
            )));
        }
    }
    Ok(())
}

/// Written to a private temporary file, then renamed into place, so a reader
/// sees either no file or the whole file. The directory is private too.
fn write_token_file(dir: &Path, task_id: &str, token: &str, socket: &Path) -> io::Result<()> {
    if !is_task_file_name(task_id) {
        return Err(io::Error::other("invalid task id"));
    }
    if let Some(parent) = dir.parent() {
        std::fs::create_dir_all(parent)?;
    }
    match crate::platform::create_remote_private_dir(dir) {
        Err(error) if error.kind() != io::ErrorKind::AlreadyExists => return Err(error),
        _ => {}
    }
    check_private_dir(dir)?;
    let temporary = dir.join(format!("{task_id}.tmp"));
    let _ = std::fs::remove_file(&temporary);
    let result = (|| {
        let mut file = crate::platform::create_private_state_file(&temporary)?;
        io::Write::write_all(
            &mut file,
            format!("{token}\n{}\n", socket.display()).as_bytes(),
        )?;
        file.sync_all()?;
        crate::platform::replace_file(&temporary, &dir.join(task_id))
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(&temporary);
    }
    result
}

/// The token files this server wrote (or found at startup) and still has to
/// remove. Runtime-only: after a restart the directory listing restores it.
#[derive(Default)]
pub(super) struct TokenFiles {
    dir: Option<PathBuf>,
    live: HashSet<String>,
}

impl TokenFiles {
    /// Uses `dir` and adopts files a previous server left there, so the next
    /// `retire` removes those of resolved or evicted tasks.
    pub(super) fn open(&mut self, dir: PathBuf) {
        if let Ok(entries) = std::fs::read_dir(&dir) {
            self.live.extend(
                entries
                    .flatten()
                    .filter_map(|entry| entry.file_name().into_string().ok()),
            );
        }
        self.dir = Some(dir);
    }

    /// Writes the task's token file. Without a directory (unit tests that did
    /// not choose one) nothing is written.
    pub(super) fn issue(
        &mut self,
        task_id: &str,
        token: &str,
        socket: &Path,
    ) -> Result<(), String> {
        let Some(dir) = &self.dir else {
            return Ok(());
        };
        write_token_file(dir, task_id, token, socket)
            .map_err(|error| format!("Cannot write the report token file: {error}"))?;
        self.live.insert(task_id.to_owned());
        Ok(())
    }

    /// Removes every token file whose task `keep` rejects.
    pub(super) fn retire(&mut self, keep: impl Fn(&str) -> bool) {
        let Some(dir) = &self.dir else {
            return;
        };
        self.live.retain(|task_id| {
            if keep(task_id) {
                return true;
            }
            match std::fs::remove_file(dir.join(task_id)) {
                Err(error) if error.kind() != io::ErrorKind::NotFound => {
                    tracing::warn!(%error, task_id, "cannot remove orbit report token file");
                }
                _ => {}
            }
            false
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tokens_are_random_hex_of_sixteen_bytes() {
        let first = generate().unwrap();
        let second = generate().unwrap();
        assert_eq!(first.len(), TOKEN_BYTES * 2);
        assert!(first.chars().all(|ch| ch.is_ascii_hexdigit()));
        assert_ne!(first, second);
    }

    #[test]
    fn only_the_issued_token_matches_its_hash() {
        let token = generate().unwrap();
        let stored = hash(&token);
        assert_ne!(stored.0, token, "hash must not be the token");
        assert_eq!(stored.0.len(), 64);
        assert!(accepts(Some(&stored), Some(&token)));
        assert!(!accepts(Some(&stored), Some(&generate().unwrap())));
        assert!(!accepts(Some(&stored), Some("")));
        assert!(!accepts(Some(&stored), None));
        assert!(accepts(None, None), "legacy task without hash");
        assert!(!constant_time_eq(b"ab", b"abc"));
        assert!(format!("{stored:?}").contains("redacted"));
    }

    #[test]
    fn token_files_name_only_server_task_ids_and_parse_token_then_socket() {
        assert!(is_task_file_name("task-123-456789-1"));
        for bad in [
            "",
            "../x",
            "a/b",
            "task.tmp",
            "..",
            &"t".repeat(MAX_TASK_ID_BYTES + 1),
        ] {
            assert!(!is_task_file_name(bad), "{bad}");
        }
        let parsed = parse_token_file("00ff\n/tmp/a.sock\n").unwrap();
        assert_eq!(parsed.token, "00ff");
        assert_eq!(parsed.socket, Some(PathBuf::from("/tmp/a.sock")));
        assert_eq!(parse_token_file("00ff").unwrap().socket, None);
        assert!(parse_token_file("").is_none());
        assert!(parse_token_file("not hex\n/tmp/a.sock").is_none());
        assert!(
            !format!("{parsed:?}").contains("00ff"),
            "Debug shows the token"
        );
    }

    #[test]
    fn written_token_files_round_trip_and_reject_traversal() {
        let base = std::env::temp_dir().join(format!("aster-orbit-token-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        std::fs::create_dir_all(&base).unwrap();
        let dir = token_dir(&base);
        write_token_file(&dir, "task-1", "abcd", Path::new("/tmp/s.sock")).unwrap();
        let read = read_token_file(&dir, "task-1").unwrap();
        assert_eq!(read.token, "abcd");
        assert_eq!(read.socket, Some(PathBuf::from("/tmp/s.sock")));
        assert!(!dir.join("task-1.tmp").exists());
        assert!(write_token_file(&dir, "../escape", "abcd", Path::new("/s")).is_err());
        std::fs::write(base.join("outside"), "abcd\n").unwrap();
        assert!(read_token_file(&dir, "../outside").is_none());
        let _ = std::fs::remove_dir_all(&base);
    }
}
