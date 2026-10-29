//! Patches of overlapping steps build on one another.
use std::path::Path;
use std::process::Command;

use super::*;

#[test]
fn patches_of_an_overlapping_chain_apply_in_order_to_the_snapshot() {
    let scratch = Scratch::new();
    for clone in [crate::platform::clone_directory_cow as CloneFn, no_clone] {
        let patches = scratch.root.join("patches");
        let first = scratch.create_named("par-one", &[], clone);
        std::fs::write(first.path.join("a.txt"), "a one\n").unwrap();
        std::fs::write(first.path.join("only-one.txt"), "one\n").unwrap();
        let one = capture_patch(&first, &patches.join("one.patch")).unwrap();
        remove(&first, &scratch.checkouts()).unwrap();

        // The second step starts from the first step's result, not the snapshot.
        let second = scratch.create_named("par-two", std::slice::from_ref(&one.path), clone);
        assert_eq!(
            std::fs::read_to_string(second.path.join("a.txt")).unwrap(),
            "a one\n"
        );
        std::fs::write(second.path.join("a.txt"), "a one\na two\n").unwrap();
        let two = capture_patch(&second, &patches.join("two.patch")).unwrap();
        remove(&second, &scratch.checkouts()).unwrap();

        let second_text = std::fs::read_to_string(&two.path).unwrap();
        assert!(
            second_text.contains("+a two") && !second_text.contains("only-one"),
            "{second_text}"
        );
        assert_eq!(two.files, ["a.txt"]);
        // In order, onto a fresh checkout of the snapshot: the final tree.
        let target = scratch.create_named("par-target", &[], no_clone);
        let apply = |patch: &Path| {
            Command::new("git")
                .arg("-C")
                .arg(&target.path)
                .args(["apply", "--check"])
                .arg(patch)
                .status()
                .unwrap()
                .success()
        };
        assert!(apply(&one.path));
        assert!(!apply(&two.path), "the second patch builds on the first");
        git_in(&target.path, &["apply", one.path.to_str().unwrap()]);
        assert!(apply(&two.path));
        git_in(&target.path, &["apply", two.path.to_str().unwrap()]);
        assert_eq!(
            std::fs::read_to_string(target.path.join("a.txt")).unwrap(),
            "a one\na two\n"
        );
        assert_eq!(
            std::fs::read_to_string(target.path.join("only-one.txt")).unwrap(),
            "one\n"
        );
        remove(&target, &scratch.checkouts()).unwrap();
        std::fs::remove_dir_all(patches).unwrap();
    }
}

#[test]
fn a_patch_that_does_not_apply_fails_the_checkout_and_leaves_nothing() {
    let scratch = Scratch::new();
    let bad = scratch.root.join("bad.patch");
    std::fs::write(&bad, "not a patch\n").unwrap();
    let result = create_with(
        &scratch.repo(),
        &scratch.repo(),
        &scratch.checkouts(),
        "repo",
        "par-bad",
        &[bad],
        no_clone,
    );
    assert!(result.unwrap_err().contains("could not apply"));
    assert!(!scratch.checkouts().join("repo/par-bad").exists());
    assert!(!scratch.git(&["worktree", "list"]).contains("par-bad"));
}
