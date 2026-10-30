//! Startup reclamation deletes only what this session provably owns and has
//! abandoned: never another session's checkout, never through a link.
use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use super::super::reclaim::reclaim_orphans;
use super::super::safety::{marker_path, write_marker};
use super::super::{Checkout, CheckoutMethod};

const AGE: Duration = Duration::from_secs(3600);
const ME: &str = "/sessions/a";

struct Temp(PathBuf);

impl Temp {
    fn new(label: &str) -> Self {
        let path = std::env::temp_dir().join(format!(
            "aster-reclaim-{label}-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(SystemTime::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&path).unwrap();
        Self(std::fs::canonicalize(path).unwrap())
    }

    fn worktrees(&self) -> PathBuf {
        self.0.join("worktrees")
    }

    fn source(&self) -> PathBuf {
        let source = self.0.join("source");
        std::fs::create_dir_all(&source).unwrap();
        source
    }
}

impl Drop for Temp {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn age_marker(path: &Path, age: Duration) {
    let times = std::fs::FileTimes::new().set_modified(SystemTime::now() - age);
    std::fs::File::open(marker_path(path))
        .unwrap()
        .set_times(times)
        .unwrap();
}

/// A clone-shaped directory with a marker naming `owner`, two hours old.
fn marked_clone(temp: &Temp, repo: &str, name: &str, owner: &str) -> PathBuf {
    let path = temp.worktrees().join(repo).join(name);
    std::fs::create_dir_all(path.join(".git")).unwrap();
    std::fs::write(path.join("work.txt"), "work").unwrap();
    write_marker(&path, &temp.source(), owner).unwrap();
    age_marker(&path, Duration::from_secs(7200));
    path
}

fn reclaim(temp: &Temp, owned: &HashSet<PathBuf>) -> Vec<super::super::reclaim::Reclaimed> {
    reclaim_orphans(&temp.worktrees(), ME, owned, AGE)
}

#[test]
fn an_abandoned_checkout_of_this_session_is_reclaimed() {
    let temp = Temp::new("mine");
    let mine = marked_clone(&temp, "repo", "par-mine", ME);

    let done = reclaim(&temp, &HashSet::new());

    assert!(done.iter().all(|item| item.result.is_ok()), "{done:?}");
    assert!(!mine.exists());
    assert!(!marker_path(&mine).exists());
}

#[test]
fn another_sessions_and_ownerless_checkouts_are_left_alone() {
    let temp = Temp::new("others");
    let foreign = marked_clone(&temp, "repo-b", "par-live", "/sessions/b");
    let ownerless = marked_clone(&temp, "repo-b", "par-old", "");

    let done = reclaim(&temp, &HashSet::new());

    assert!(done.is_empty(), "{done:?}");
    assert!(foreign.join("work.txt").exists());
    assert!(ownerless.join("work.txt").exists());
}

#[test]
fn an_unparseable_marker_is_left_alone() {
    let temp = Temp::new("garbled");
    let path = marked_clone(&temp, "repo", "par-garbled", ME);
    std::fs::write(marker_path(&path), "garbage").unwrap();
    age_marker(&path, Duration::from_secs(7200));

    assert!(reclaim(&temp, &HashSet::new()).is_empty());
    assert!(path.join("work.txt").exists());
}

#[test]
fn a_checkout_the_ledger_references_is_kept() {
    let temp = Temp::new("owned");
    let path = marked_clone(&temp, "repo", "par-owned", ME);

    reclaim(&temp, &HashSet::from([path.clone()]));

    assert!(path.exists());
}

#[test]
fn young_and_mismatched_markers_do_not_authorize_deletion() {
    let temp = Temp::new("markers");
    let young = marked_clone(&temp, "repo", "young", ME);
    age_marker(&young, Duration::ZERO);
    let mismatch = marked_clone(&temp, "repo", "mismatch", ME);
    std::fs::write(
        marker_path(&mismatch),
        "/not/the/checkout\n/source\nowner=/sessions/a\n",
    )
    .unwrap();
    age_marker(&mismatch, Duration::from_secs(7200));

    reclaim(&temp, &HashSet::new());

    assert!(young.exists());
    assert!(mismatch.exists());
}

#[cfg(unix)]
#[test]
fn a_symlinked_checkout_never_redirects_deletion_to_an_unmarked_target() {
    let temp = Temp::new("alias");
    let victim = temp.worktrees().join("foreign/unmarked");
    std::fs::create_dir_all(victim.join(".git")).unwrap();
    std::fs::write(victim.join("keep.txt"), "not a checkout").unwrap();
    let repo = temp.worktrees().join("repo-a");
    std::fs::create_dir_all(&repo).unwrap();
    let alias = repo.join("par-alias");
    std::os::unix::fs::symlink(&victim, &alias).unwrap();
    write_marker(&alias, &temp.source(), ME).unwrap();
    age_marker(&alias, Duration::from_secs(7200));

    let done = reclaim(&temp, &HashSet::new());

    assert!(victim.join("keep.txt").exists(), "{done:?}");
    assert!(done.iter().all(|item| item.result.is_err()), "{done:?}");
}

#[cfg(unix)]
#[test]
fn a_symlink_to_outside_the_root_is_refused() {
    let temp = Temp::new("outside");
    let victim = temp.0.join("outside");
    std::fs::create_dir_all(victim.join(".git")).unwrap();
    let alias = temp.worktrees().join("repo/par");
    std::fs::create_dir_all(alias.parent().unwrap()).unwrap();
    std::os::unix::fs::symlink(&victim, &alias).unwrap();
    write_marker(&alias, &temp.source(), ME).unwrap();
    age_marker(&alias, Duration::from_secs(7200));

    let done = reclaim(&temp, &HashSet::new());

    assert!(victim.exists());
    assert!(done[0].result.is_err(), "{done:?}");
}

#[cfg(unix)]
#[test]
fn remove_refuses_a_symlinked_checkout_for_every_caller() {
    let temp = Temp::new("remove-alias");
    let victim = temp.worktrees().join("foreign/unmarked");
    std::fs::create_dir_all(victim.join(".git")).unwrap();
    std::fs::write(victim.join("keep.txt"), "keep").unwrap();
    let alias = temp.worktrees().join("repo/par-alias");
    std::fs::create_dir_all(alias.parent().unwrap()).unwrap();
    std::os::unix::fs::symlink(&victim, &alias).unwrap();
    write_marker(&alias, &temp.source(), ME).unwrap();
    let checkout = Checkout {
        path: alias,
        repo_root: temp.source(),
        subdir: PathBuf::new(),
        snapshot: String::new(),
        baseline: String::new(),
        method: CheckoutMethod::CopyOnWrite,
    };

    assert!(super::super::remove(&checkout, &temp.worktrees()).is_err());
    assert!(victim.join("keep.txt").exists());
}
