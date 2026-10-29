//! Restart safety for isolated Orbit workers. Worker records are runtime-only,
//! so after a restart an isolated worker's pane is restored without one. A
//! worker whose checkout is a linked Git worktree, or lies under the configured
//! worktrees directory, is recorded again as isolated so it is never reused for
//! other work and `orbit.agent_stop {remove_worktree}` still works. Recovery
//! reads only small Git metadata files; it never runs Git.
use std::path::{Path, PathBuf};

use super::orbit_lifecycle::{OrbitWorktree, WorkerRecord};
use crate::app::App;

/// `gitdir: <path>` in a linked worktree's `.git` file.
fn linked_git_dir(checkout: &Path) -> Option<PathBuf> {
    let content = std::fs::read_to_string(checkout.join(".git")).ok()?;
    let git_dir = Path::new(content.trim().strip_prefix("gitdir:")?.trim());
    Some(checkout.join(git_dir))
}

/// The branch checked out in a worktree's Git directory; `None` when detached.
fn head_branch(git_dir: &Path) -> Option<String> {
    let head = std::fs::read_to_string(git_dir.join("HEAD")).ok()?;
    head.trim()
        .strip_prefix("ref: refs/heads/")
        .map(str::to_owned)
}

/// The main checkout a linked worktree belongs to, from its `commondir`.
fn main_checkout(git_dir: &Path) -> Option<PathBuf> {
    let common = std::fs::read_to_string(git_dir.join("commondir")).ok()?;
    let common = git_dir.join(common.trim());
    let common = common.canonicalize().unwrap_or(common);
    match common.file_name() {
        Some(name) if name == ".git" => common.parent().map(Path::to_path_buf),
        _ => Some(common),
    }
}

/// The isolated checkout holding `cwd`: the nearest linked worktree above it,
/// or any directory under `worktrees_dir`. A main checkout is not isolated.
pub(super) fn isolated_checkout(cwd: &Path, worktrees_dir: &Path) -> Option<OrbitWorktree> {
    let under_worktrees = !worktrees_dir.as_os_str().is_empty() && cwd.starts_with(worktrees_dir);
    for dir in cwd.ancestors() {
        let marker = dir.join(".git");
        if marker.is_dir() {
            break;
        }
        if !marker.is_file() {
            continue;
        }
        let git_dir = linked_git_dir(dir)?;
        return Some(OrbitWorktree {
            path: dir.to_path_buf(),
            branch: head_branch(&git_dir),
            repo_root: main_checkout(&git_dir).unwrap_or_else(|| dir.to_path_buf()),
            recovered: true,
            created_branch: false,
        });
    }
    under_worktrees.then(|| OrbitWorktree {
        path: cwd.to_path_buf(),
        branch: None,
        repo_root: cwd.to_path_buf(),
        recovered: true,
        created_branch: false,
    })
}

impl App {
    /// Records unrecorded Orbit agents that run in an isolated checkout.
    pub(super) fn recover_orbit_worktree_workers(&mut self) {
        let worktrees_dir = self.state.worktree_directory.clone();
        for (terminal_id, _, _) in self.orbit_tab_terminals() {
            if self.orbit_workers.contains_key(&terminal_id)
                || self.orbit_is_lead_terminal(&terminal_id)
            {
                continue;
            }
            let Some(terminal) = self.state.terminals.get(terminal_id.as_str()) else {
                continue;
            };
            if !terminal.is_agent_terminal() {
                continue;
            }
            let Some(worktree) = isolated_checkout(&terminal.cwd, &worktrees_dir) else {
                continue;
            };
            let kind = terminal
                .managed_agent_kind()
                .or_else(|| terminal.effective_known_agent())
                .map_or("unknown", crate::detect::agent_label);
            let name = terminal
                .agent_name
                .clone()
                .unwrap_or_else(|| terminal_id.clone());
            tracing::info!(%terminal_id, path = %worktree.path.display(), "orbit isolated worker recovered");
            let mut record = WorkerRecord::new(name, kind.to_owned(), None);
            record.worktree = Some(worktree);
            self.orbit_workers.insert(terminal_id, record);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::super::orbit_lifecycle::tests::{orbit_app, orbit_panes};
    use super::*;
    use crate::detect::{Agent, AgentState};

    fn scratch(case: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "aster-orbit-recovery-{case}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// A main checkout with one linked worktree on `branch`, as Git lays them out.
    fn linked_worktree(root: &Path, branch: &str) -> (PathBuf, PathBuf) {
        let repo = root.join("repo");
        let git_dir = repo.join(".git/worktrees/editor");
        std::fs::create_dir_all(&git_dir).unwrap();
        std::fs::write(git_dir.join("HEAD"), format!("ref: refs/heads/{branch}\n")).unwrap();
        std::fs::write(git_dir.join("commondir"), "../..\n").unwrap();
        let checkout = root.join("wt/editor");
        std::fs::create_dir_all(checkout.join("src")).unwrap();
        std::fs::write(
            checkout.join(".git"),
            format!("gitdir: {}\n", git_dir.display()),
        )
        .unwrap();
        (repo, checkout)
    }

    #[test]
    fn linked_worktree_and_worktrees_directory_are_isolated_main_checkout_is_not() {
        let root = scratch("detect");
        let (repo, checkout) = linked_worktree(&root, "feature/x");
        let found = isolated_checkout(&checkout.join("src"), Path::new("/nonexistent")).unwrap();
        assert_eq!(found.path, checkout);
        assert_eq!(found.branch.as_deref(), Some("feature/x"));
        assert_eq!(found.repo_root, repo.canonicalize().unwrap());
        assert!(found.recovered);
        assert!(isolated_checkout(&repo, Path::new("/nonexistent")).is_none());
        let plain = root.join("worktrees/loose");
        std::fs::create_dir_all(&plain).unwrap();
        let found = isolated_checkout(&plain, &root.join("worktrees")).unwrap();
        assert_eq!(found.branch, None);
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn restored_worker_in_a_worktree_is_never_reused_and_shows_its_checkout() {
        let root = scratch("reuse");
        let (_, checkout) = linked_worktree(&root, "worktree/calm");
        let mut app = orbit_app();
        let (_, _, terminal_id) = orbit_panes(&mut app)[1].clone();
        let terminal = app.state.terminals.get_mut(terminal_id.as_str()).unwrap();
        terminal.restore_managed_agent("editor".into(), Agent::Claude);
        terminal.set_detected_state(Some(Agent::Claude), AgentState::Idle);
        terminal.cwd = checkout.clone();
        assert!(app
            .find_reusable_orbit_worker("claude", None, None)
            .is_none());
        let record = &app.orbit_workers[&terminal_id];
        let worktree = record.worktree.as_ref().unwrap();
        assert_eq!(worktree.path, checkout);
        assert_eq!(worktree.branch.as_deref(), Some("worktree/calm"));
        let shown = app.orbit_worker_json(&terminal_id).unwrap();
        assert_eq!(shown["worktree"]["recovered"], true, "{shown}");
        assert_eq!(shown["worktree"]["branch"], "worktree/calm");
        // The same worker in a main checkout is reusable.
        app.orbit_workers.clear();
        let terminal = app.state.terminals.get_mut(terminal_id.as_str()).unwrap();
        terminal.cwd = root.join("repo");
        assert_eq!(
            app.find_reusable_orbit_worker("claude", None, None)
                .map(|found| found.2),
            Some(terminal_id)
        );
        let _ = std::fs::remove_dir_all(root);
    }
}
