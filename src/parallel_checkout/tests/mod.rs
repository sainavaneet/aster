use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicU64, Ordering};

use super::*;

/// A scratch directory holding a repository (`repo`), a checkout directory
/// (`checkouts`), and a patch directory (`patches`).
struct Scratch {
    root: PathBuf,
}

impl Scratch {
    fn new() -> Self {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let root = std::env::temp_dir().join(format!(
            "aster-parallel-checkout-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join("repo")).unwrap();
        let scratch = Self {
            root: std::fs::canonicalize(&root).unwrap(),
        };
        scratch.git(&["init", "-q"]);
        scratch.write("a.txt", "a\n");
        scratch.write("b.txt", "b\n");
        scratch.write(".gitignore", "target/\n");
        scratch.git(&["add", "."]);
        scratch.git(&["commit", "-q", "-m", "base"]);
        scratch
    }

    fn repo(&self) -> PathBuf {
        self.root.join("repo")
    }

    fn checkouts(&self) -> PathBuf {
        self.root.join("checkouts")
    }

    fn write(&self, file: &str, text: &str) {
        let path = self.repo().join(file);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, text).unwrap();
    }

    fn git(&self, args: &[&str]) -> String {
        git_in(&self.repo(), args)
    }

    /// Uncommitted work: a tracked edit, an untracked file, ignored build output.
    fn work_in_progress(&self) {
        self.write("a.txt", "a edited\n");
        self.write("wip.txt", "untracked\n");
        self.write("target/debug/out", "build output\n");
    }

    fn create(&self, clone: CloneFn) -> Checkout {
        self.create_named("par-1", &[], clone)
    }

    fn create_named(&self, name: &str, apply: &[PathBuf], clone: CloneFn) -> Checkout {
        create_with(
            &self.repo(),
            &self.repo(),
            &self.checkouts(),
            "repo",
            name,
            apply,
            clone,
        )
        .unwrap()
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

fn git_in(dir: &Path, args: &[&str]) -> String {
    let output = Command::new("git")
        .args(["-c", "user.name=T", "-c", "user.email=t@example.invalid"])
        .args(["-c", "commit.gpgsign=false", "-C"])
        .arg(dir)
        .args(args)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8_lossy(&output.stdout).trim().to_owned()
}

fn no_clone(_: &Path, _: &Path) -> std::io::Result<()> {
    Err(std::io::Error::other("injected clone failure"))
}

fn fake_clone_that_writes_junk(_: &Path, destination: &Path) -> std::io::Result<()> {
    std::fs::create_dir_all(destination)?;
    std::fs::write(destination.join("partial"), "half a copy")?;
    Err(std::io::Error::other(
        "injected failure after a partial copy",
    ))
}

#[test]
fn snapshot_of_a_dirty_tree_leaves_it_byte_identical() {
    let scratch = Scratch::new();
    scratch.work_in_progress();
    let status = scratch.git(&["status", "--porcelain", "--untracked-files=all"]);
    let a = std::fs::read(scratch.repo().join("a.txt")).unwrap();
    let modified = std::fs::metadata(scratch.repo().join("a.txt"))
        .unwrap()
        .modified()
        .unwrap();

    let snapshot = snapshot(&scratch.repo()).unwrap();

    assert_ne!(snapshot, scratch.git(&["rev-parse", "HEAD"]));
    assert_eq!(
        scratch.git(&["status", "--porcelain", "--untracked-files=all"]),
        status
    );
    assert_eq!(std::fs::read(scratch.repo().join("a.txt")).unwrap(), a);
    let after = std::fs::metadata(scratch.repo().join("a.txt")).unwrap();
    assert_eq!(after.modified().unwrap(), modified);
    // No ref and no stash entry was created, and the edit is in the snapshot.
    assert_eq!(scratch.git(&["stash", "list"]), "");
    assert_eq!(
        scratch.git(&["show", &format!("{snapshot}:a.txt")]),
        "a edited"
    );
}

#[test]
fn snapshot_of_a_clean_tree_is_head() {
    let scratch = Scratch::new();
    let head = scratch.git(&["rev-parse", "HEAD"]);
    assert_eq!(snapshot(&scratch.repo()).unwrap(), head);
}

#[test]
fn failed_clone_falls_back_to_a_detached_worktree_on_the_snapshot() {
    let scratch = Scratch::new();
    scratch.work_in_progress();
    let before = scratch.git(&["status", "--porcelain", "--untracked-files=all"]);

    for clone in [no_clone as CloneFn, fake_clone_that_writes_junk] {
        let checkout = scratch.create(clone);

        assert_eq!(checkout.method, CheckoutMethod::Worktree);
        assert_eq!(
            std::fs::read_to_string(checkout.path.join("a.txt")).unwrap(),
            "a edited\n"
        );
        assert!(!checkout.path.join("partial").exists());
        assert_eq!(
            git_in(&checkout.path, &["rev-parse", "HEAD"]),
            checkout.snapshot
        );
        assert!(scratch
            .git(&["worktree", "list", "--porcelain"])
            .contains(&checkout.path.display().to_string()));
        remove(&checkout, &scratch.checkouts()).unwrap();
        assert!(!checkout.path.exists());
        assert!(!scratch
            .git(&["worktree", "list", "--porcelain"])
            .contains(&checkout.path.display().to_string()));
    }
    // The original tree was never touched.
    assert_eq!(
        scratch.git(&["status", "--porcelain", "--untracked-files=all"]),
        before
    );
}

#[cfg(target_os = "macos")]
#[test]
fn macos_clones_the_repository_directory_with_build_output_and_wip() {
    let scratch = Scratch::new();
    scratch.work_in_progress();
    let head = scratch.git(&["rev-parse", "HEAD"]);

    let checkout = scratch.create(crate::platform::clone_directory_cow);

    assert_eq!(checkout.method, CheckoutMethod::CopyOnWrite);
    assert_eq!(
        std::fs::read_to_string(checkout.path.join("target/debug/out")).unwrap(),
        "build output\n"
    );
    assert_eq!(
        std::fs::read_to_string(checkout.path.join("wip.txt")).unwrap(),
        "untracked\n"
    );
    assert_eq!(
        git_in(&checkout.path, &["rev-parse", "HEAD"]),
        checkout.snapshot
    );
    // The clone has its own repository: the original's HEAD did not move.
    assert_eq!(scratch.git(&["rev-parse", "HEAD"]), head);
    assert!(checkout.path.join(".git").is_dir());
    remove(&checkout, &scratch.checkouts()).unwrap();
    assert!(!checkout.path.exists());
}

#[test]
fn patch_holds_edits_and_new_files_but_not_the_starting_wip() {
    let scratch = Scratch::new();
    scratch.work_in_progress();
    // The real clone where the OS has one, the worktree fallback elsewhere.
    for clone in [crate::platform::clone_directory_cow as CloneFn, no_clone] {
        let checkout = scratch.create(clone);
        std::fs::write(checkout.path.join("b.txt"), "b changed\n").unwrap();
        std::fs::write(checkout.path.join("new.txt"), "brand new\n").unwrap();
        std::fs::create_dir_all(checkout.path.join("target")).unwrap();
        std::fs::write(checkout.path.join("target/ignored"), "x").unwrap();
        let patch_path = scratch.root.join("patches").join("one.patch");

        let summary = capture_patch(&checkout, &patch_path).unwrap();

        let patch = std::fs::read_to_string(&patch_path).unwrap();
        assert!(
            patch.contains("+b changed") && patch.contains("+brand new"),
            "{patch}"
        );
        assert!(patch.contains("new file mode"), "{patch}");
        assert!(
            !patch.contains("wip.txt") && !patch.contains("ignored"),
            "{patch}"
        );
        let mut files = summary.files.clone();
        files.sort();
        assert_eq!(files, ["b.txt", "new.txt"]);
        assert_eq!(summary.file_count, 2);
        assert_eq!(summary.bytes, patch.len() as u64);
        // The checkout's own index was not used to build it.
        assert_eq!(
            git_in(&checkout.path, &["diff", "--cached", "--name-only"]),
            ""
        );
        // The patch applies to the original tree.
        let check = Command::new("git")
            .arg("-C")
            .arg(scratch.repo())
            .args(["apply", "--check"])
            .arg(&patch_path)
            .output()
            .unwrap();
        assert!(
            check.status.success(),
            "{}",
            String::from_utf8_lossy(&check.stderr)
        );
        remove(&checkout, &scratch.checkouts()).unwrap();
        assert!(!checkout.path.exists(), "{:?}", checkout.method);
        std::fs::remove_file(patch_path).unwrap();
    }
}

#[test]
fn an_unchanged_checkout_captures_an_empty_patch() {
    let scratch = Scratch::new();
    let checkout = scratch.create(no_clone);
    let summary = capture_patch(&checkout, &scratch.root.join("p/empty.patch")).unwrap();
    assert_eq!((summary.bytes, summary.file_count), (0, 0));
    remove(&checkout, &scratch.checkouts()).unwrap();
}

#[test]
fn removal_refuses_the_repository_itself_or_a_parent_of_it() {
    let scratch = Scratch::new();
    let checkout = scratch.create(no_clone);
    for path in [
        scratch.repo(),
        scratch.root.clone(),
        PathBuf::from("relative"),
    ] {
        let hostile = Checkout {
            path: path.clone(),
            ..checkout.clone()
        };
        assert!(
            remove(&hostile, &scratch.checkouts()).is_err(),
            "{}",
            path.display()
        );
    }
    assert!(scratch.repo().join(".git").is_dir());
    remove(&checkout, &scratch.checkouts()).unwrap();
}

#[test]
fn a_linked_worktree_project_is_never_cloned() {
    let scratch = Scratch::new();
    let linked = scratch.root.join("linked");
    scratch.git(&["worktree", "add", "--detach", linked.to_str().unwrap()]);
    // A linked worktree's `.git` is a file; cloning it would share the original's state.
    assert!(!clonable(&linked, &scratch.checkouts()));
    assert!(clonable(&scratch.repo(), &scratch.checkouts()));
    assert!(!clonable(&scratch.repo(), &scratch.repo().join("inside")));
}

#[test]
fn concurrent_checkouts_of_one_repository_all_succeed() {
    let scratch = Scratch::new();
    scratch.work_in_progress();
    let results: Vec<_> = std::thread::scope(|scope| {
        let handles: Vec<_> = (0..4)
            .map(|n| {
                let scratch = &scratch;
                scope.spawn(move || {
                    create_with(
                        &scratch.repo(),
                        &scratch.repo(),
                        &scratch.checkouts(),
                        "repo",
                        &format!("par-c{n}"),
                        &[],
                        crate::platform::clone_directory_cow,
                    )
                })
            })
            .collect();
        handles.into_iter().map(|h| h.join().unwrap()).collect()
    });
    for result in results {
        let checkout = result.unwrap();
        assert_eq!(
            git_in(&checkout.path, &["rev-parse", "HEAD"]),
            checkout.snapshot
        );
        remove(&checkout, &scratch.checkouts()).unwrap();
    }
}

mod chain;
mod checked;
mod integrate;
mod resume;
mod safety;
