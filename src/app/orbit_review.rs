//! Git snapshots use a private index; the user's index and worktree are untouched.
use std::fs;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::{
    atomic::{AtomicU64, Ordering},
    Mutex,
};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

static SEQUENCE: AtomicU64 = AtomicU64::new(0);
#[derive(Default)]
pub(crate) struct ReviewBaseline {
    baseline: Mutex<Option<Result<(PathBuf, String), String>>>,
}
struct Scratch(PathBuf);
impl Scratch {
    fn new() -> Result<Self, String> {
        let path = std::env::temp_dir().join(format!(
            "aster-review-{}-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap_or_default()
                .as_nanos(),
            SEQUENCE.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir(&path).map_err(|e| e.to_string())?;
        Ok(Self(path))
    }
}
impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}
fn git(cwd: &Path, args: &[&str], index: Option<&Path>) -> Result<String, String> {
    let scratch = Scratch::new()?;
    let out = scratch.0.join("stdout");
    let err = scratch.0.join("stderr");
    let mut command = crate::noninteractive_process::command("git");
    command
        .current_dir(cwd)
        .args(args)
        .stdin(Stdio::null())
        .stdout(fs::File::create(&out).map_err(|e| e.to_string())?)
        .stderr(fs::File::create(&err).map_err(|e| e.to_string())?)
        .env_remove("GIT_DIR")
        .env_remove("GIT_WORK_TREE")
        .env_remove("GIT_INDEX_FILE")
        .env_remove("GIT_COMMON_DIR")
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("GIT_OPTIONAL_LOCKS", "0");
    if let Some(index) = index {
        command.env("GIT_INDEX_FILE", index);
    }
    let mut child = command.spawn().map_err(|e| e.to_string())?;
    let deadline = Instant::now() + Duration::from_secs(20);
    let status = loop {
        if let Some(status) = child.try_wait().map_err(|e| e.to_string())? {
            break status;
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            return Err("Git review operation timed out".into());
        }
        std::thread::sleep(Duration::from_millis(20));
    };
    let path = if status.success() { &out } else { &err };
    let size = fs::metadata(path).map_err(|e| e.to_string())?.len();
    let mut bytes = Vec::new();
    fs::File::open(path)
        .map_err(|e| e.to_string())?
        .take(4 * 1024 * 1024)
        .read_to_end(&mut bytes)
        .map_err(|e| e.to_string())?;
    let mut text = String::from_utf8_lossy(&bytes).into_owned();
    if size > bytes.len() as u64 {
        text.push_str("\n[Git review output truncated at 4 MiB]\n");
    }
    if status.success() {
        Ok(text)
    } else {
        Err(text)
    }
}
fn snapshot(root: &Path) -> Result<String, String> {
    let scratch = Scratch::new()?;
    let index = scratch.0.join("index");
    git(root, &["read-tree", "--empty"], Some(&index))?;
    git(root, &["add", "--all", "--", "."], Some(&index))?;
    Ok(git(root, &["write-tree"], Some(&index))?.trim().to_owned())
}
impl ReviewBaseline {
    /// A failed/non-Git snapshot is remembered, but never blocks ordinary jobs.
    pub(crate) fn capture(&self, cwd: &Path) -> Result<(), String> {
        let mut state = self
            .baseline
            .lock()
            .map_err(|_| "Review baseline lock failed")?;
        if state.is_none() {
            *state = Some((|| {
                let root = PathBuf::from(git(cwd, &["rev-parse", "--show-toplevel"], None)?.trim());
                let tree = snapshot(&root)?;
                Ok((root, tree))
            })());
        }
        Ok(())
    }
    pub(crate) fn ready(&self) -> bool {
        self.baseline.try_lock().is_ok_and(|state| state.is_some())
    }
    pub(crate) fn diff(&self, cwd: &Path, path: Option<&str>) -> Result<String, String> {
        self.capture(cwd)?;
        let (root, before) = self
            .baseline
            .lock()
            .map_err(|_| "Review baseline lock failed")?
            .as_ref()
            .ok_or("No review baseline")?
            .clone()?;
        let after = snapshot(&root)?;
        let mut args = vec![
            "--literal-pathspecs",
            "diff",
            "--no-ext-diff",
            "--no-textconv",
            before.as_str(),
            after.as_str(),
            "--",
        ];
        if let Some(path) = path {
            args.push(path);
        }
        let diff = git(&root, &args, None)?;
        Ok(format!("Changes since this Orbit server's first review baseline. Existing dirty content is included in that baseline; later external edits are also shown.\nBaseline: {before}\nCurrent: {after}\n\n{diff}"))
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn orbit_review_tracks_new_edits_without_changing_user_index() {
        let scratch = Scratch::new().unwrap();
        git(&scratch.0, &["init", "--quiet"], None).unwrap();
        fs::write(scratch.0.join("file.txt"), "existing dirty content\n").unwrap();
        let baseline = ReviewBaseline::default();
        baseline.capture(&scratch.0).unwrap();
        fs::write(scratch.0.join("file.txt"), "new content\n").unwrap();
        let diff = baseline.diff(&scratch.0, None).unwrap();
        assert!(diff.contains("-existing dirty content"));
        assert!(diff.contains("+new content"));
        assert!(!scratch.0.join(".git/index").exists());
    }
}
