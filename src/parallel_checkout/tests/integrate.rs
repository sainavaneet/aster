//! Applying saved patches to the main tree, and undoing them.
use std::process::Command;

use super::*;
use crate::parallel_checkout::integrate::{apply_in_order, probe, PatchToApply, Probe};

fn to_apply(step: usize, summary: &PatchSummary) -> PatchToApply {
    PatchToApply {
        step,
        path: summary.path.clone(),
        files: summary.files.clone(),
        after: Vec::new(),
    }
}

impl Scratch {
    /// A step's patch: `edits` in a fresh checkout, which is then removed.
    fn patch(&self, name: &str, edits: &[(&str, &str)]) -> PatchSummary {
        let checkout = self.create_named(name, &[], no_clone);
        for (file, text) in edits {
            std::fs::write(checkout.path.join(file), text).unwrap();
        }
        let summary =
            capture_patch(&checkout, &self.root.join(format!("patches/{name}.patch"))).unwrap();
        remove(&checkout, &self.checkouts()).unwrap();
        summary
    }

    pub(super) fn state(&self) -> (String, String) {
        (
            self.git(&["status", "--porcelain"]),
            self.git(&["diff", "--cached"]),
        )
    }

    fn git_apply(&self, args: &[&str], patch: &PatchSummary) -> bool {
        Command::new("git")
            .arg("-C")
            .arg(self.repo())
            .arg("apply")
            .args(args)
            .arg(&patch.path)
            .status()
            .unwrap()
            .success()
    }
}

#[test]
fn undoing_with_index_restores_files_and_index_and_keeps_unrelated_staged_changes() {
    let scratch = Scratch::new();
    let one = scratch.patch("one", &[("a.txt", "a edited\n")]);
    let two = scratch.patch("two", &[("b.txt", "b edited\n"), ("new.txt", "new\n")]);
    scratch.write("unrelated.txt", "somebody's staged work\n");
    scratch.git(&["add", "unrelated.txt"]);
    let before = scratch.state();
    let patches = [to_apply(0, &one), to_apply(1, &two)];

    let report = apply_in_order(&scratch.repo(), &patches);

    assert_eq!(report.applied, [0, 1]);
    assert!(report.stopped.is_none());
    assert_ne!(scratch.state(), before);
    // The integration is staged along with the unrelated change.
    assert!(scratch
        .git(&["diff", "--cached", "--name-only"])
        .contains("a.txt"));
    // Reverse order, with --index: files and index return to where they were.
    assert!(scratch.git_apply(&["-R", "--index"], &two));
    assert!(scratch.git_apply(&["-R", "--index"], &one));
    assert_eq!(scratch.state(), before);
    assert_eq!(
        std::fs::read_to_string(scratch.repo().join("a.txt")).unwrap(),
        "a\n"
    );
    assert!(!scratch.repo().join("new.txt").exists());
    assert_eq!(
        std::fs::read_to_string(scratch.repo().join("unrelated.txt")).unwrap(),
        "somebody's staged work\n"
    );
}

#[test]
fn undoing_without_index_would_leave_the_integration_staged() {
    let scratch = Scratch::new();
    let one = scratch.patch("one", &[("a.txt", "a edited\n")]);
    let before = scratch.state();
    apply_in_order(&scratch.repo(), &[to_apply(0, &one)]);

    assert!(scratch.git_apply(&["-R"], &one));

    assert_ne!(
        scratch.state(),
        before,
        "a plain -R leaves the index changed"
    );
}

/// An unrelated merge conflict that was in the tree before the integration.
fn leave_an_unrelated_conflict(scratch: &Scratch) {
    scratch.write("c.txt", "c\n");
    scratch.git(&["add", "c.txt"]);
    scratch.git(&["commit", "-q", "-m", "add c"]);
    scratch.git(&["checkout", "-q", "-b", "side"]);
    scratch.write("c.txt", "c from side\n");
    scratch.git(&["commit", "-q", "-am", "side"]);
    scratch.git(&["checkout", "-q", "-"]);
    scratch.write("c.txt", "c from main\n");
    scratch.git(&["commit", "-q", "-am", "main"]);
    let merge = Command::new("git")
        .arg("-C")
        .arg(scratch.repo())
        .args([
            "-c",
            "user.name=T",
            "-c",
            "user.email=t@example.invalid",
            "merge",
            "side",
        ])
        .output()
        .unwrap();
    assert!(!merge.status.success(), "the merge must conflict");
}

#[test]
fn conflicts_that_were_already_in_the_tree_are_not_blamed_on_the_failed_patch() {
    let scratch = Scratch::new();
    let one = scratch.patch("one", &[("a.txt", "a edited\n")]);
    // The file the patch edits is gone from the main tree, so it cannot apply.
    scratch.git(&["rm", "-q", "a.txt"]);
    scratch.git(&["commit", "-q", "-m", "remove a"]);
    leave_an_unrelated_conflict(&scratch);

    let report = apply_in_order(&scratch.repo(), &[to_apply(0, &one)]);

    let stopped = report.stopped.expect("the patch cannot apply");
    assert!(!stopped.conflict, "{stopped:?}");
    assert_eq!(stopped.files, ["a.txt"]);
    assert!(report.applied.is_empty());
}

#[test]
fn a_conflict_the_patch_caused_is_reported_among_its_own_files_only() {
    let scratch = Scratch::new();
    let one = scratch.patch("one", &[("a.txt", "a from step\n")]);
    scratch.write("a.txt", "a from main\n");
    scratch.git(&["commit", "-q", "-am", "main moves"]);
    leave_an_unrelated_conflict(&scratch);

    let report = apply_in_order(&scratch.repo(), &[to_apply(0, &one)]);

    // Git may refuse the patch outright with a conflict already present; either
    // way c.txt, unmerged before, is never listed for this patch.
    let stopped = report.stopped.expect("the patch cannot apply cleanly");
    assert!(!stopped.files.contains(&"c.txt".to_owned()), "{stopped:?}");
}

#[test]
fn an_interrupted_patch_is_found_applied_not_applied_or_unclear() {
    let scratch = Scratch::new();
    let one = scratch.patch("one", &[("a.txt", "a edited\n")]);
    let patch = to_apply(0, &one);
    assert_eq!(probe(&scratch.repo(), &patch), Probe::NotApplied);

    assert!(
        apply_in_order(&scratch.repo(), std::slice::from_ref(&patch))
            .stopped
            .is_none()
    );
    assert_eq!(probe(&scratch.repo(), &patch), Probe::Applied);

    // Changed since: neither reversible nor applicable.
    scratch.write("a.txt", "a something else\n");
    assert_eq!(probe(&scratch.repo(), &patch), Probe::Unclear);

    // Conflict markers from a half-finished application.
    let scratch = Scratch::new();
    let two = scratch.patch("two", &[("a.txt", "a from step\n")]);
    scratch.write("a.txt", "a from main\n");
    scratch.git(&["commit", "-q", "-am", "main moves"]);
    let conflicting = to_apply(0, &two);
    assert!(
        apply_in_order(&scratch.repo(), std::slice::from_ref(&conflicting))
            .stopped
            .unwrap()
            .conflict
    );
    assert_eq!(probe(&scratch.repo(), &conflicting), Probe::Unclear);
}

#[cfg(unix)]
#[test]
fn a_repository_reached_through_a_symlink_is_recorded_by_its_real_path() {
    let scratch = Scratch::new();
    let link = scratch.root.join("repo-link");
    std::os::unix::fs::symlink(scratch.repo(), &link).unwrap();
    let make = |repo: &std::path::Path, name: &str| {
        create_with(
            repo,
            repo,
            &scratch.checkouts(),
            "repo",
            name,
            &[],
            no_clone,
        )
        .unwrap()
    };

    let direct = make(&scratch.repo(), "par-direct");
    let linked = make(&link, "par-linked");

    assert_eq!(direct.repo_root, linked.repo_root);
    assert_eq!(
        direct.repo_root,
        std::fs::canonicalize(scratch.repo()).unwrap()
    );
    remove(&direct, &scratch.checkouts()).unwrap();
    remove(&linked, &scratch.checkouts()).unwrap();
}

#[test]
fn applying_a_patch_that_is_already_in_the_tree_does_not_apply_it_again() {
    let scratch = Scratch::new();
    let one = scratch.patch("one", &[("a.txt", "a edited\n")]);
    let patch = to_apply(0, &one);
    assert!(
        crate::parallel_checkout::integrate::apply_once(&scratch.repo(), &patch)
            .stopped
            .is_none()
    );
    let state = scratch.state();

    // Again, as a resume after a crash that lost the record of it would.
    let again = crate::parallel_checkout::integrate::apply_once(&scratch.repo(), &patch);

    assert!(again.applied.is_empty());
    assert_eq!(again.present, [0], "present, not applied by Aster");
    assert!(again.stopped.is_none());
    assert_eq!(scratch.state(), state);
    assert_eq!(
        std::fs::read_to_string(scratch.repo().join("a.txt")).unwrap(),
        "a edited\n"
    );
}
