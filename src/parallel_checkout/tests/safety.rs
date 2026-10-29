//! Proofs that checkout operations stay inside what Aster created.
use std::path::{Path, PathBuf};
use std::process::Command;

use super::*;

/// Moves the repository's `.git` elsewhere and leaves a symlink to it.
#[cfg(unix)]
fn symlink_git_dir(scratch: &Scratch) -> PathBuf {
    let store = scratch.root.join("gitstore");
    std::fs::rename(scratch.repo().join(".git"), &store).unwrap();
    std::os::unix::fs::symlink(&store, scratch.repo().join(".git")).unwrap();
    store
}

#[cfg(unix)]
#[test]
fn a_symlinked_git_directory_is_never_cloned_and_the_source_stays_untouched() {
    let scratch = Scratch::new();
    scratch.work_in_progress();
    let store = symlink_git_dir(&scratch);
    let head = std::fs::read(store.join("HEAD")).unwrap();
    let index = std::fs::read(store.join("index")).unwrap();
    let branch = scratch.git(&["symbolic-ref", "HEAD"]);

    assert!(!clonable(&scratch.repo(), &scratch.checkouts()));
    // The real clone function would copy the symlink; it must not be used.
    let checkout = scratch.create(crate::platform::clone_directory_cow);

    assert_eq!(checkout.method, CheckoutMethod::Worktree);
    assert_eq!(std::fs::read(store.join("HEAD")).unwrap(), head);
    assert_eq!(std::fs::read(store.join("index")).unwrap(), index);
    assert_eq!(scratch.git(&["symbolic-ref", "HEAD"]), branch);
    remove(&checkout, &scratch.checkouts()).unwrap();
}

#[cfg(unix)]
#[test]
fn a_clone_whose_git_directory_resolves_outside_it_is_rejected() {
    let scratch = Scratch::new();
    // What `cp -c -R` would leave behind for a symlinked `.git`.
    let fake = scratch.root.join("fake-clone");
    std::fs::create_dir_all(&fake).unwrap();
    std::os::unix::fs::symlink(scratch.repo().join(".git"), fake.join(".git")).unwrap();
    let head = std::fs::read(scratch.repo().join(".git/HEAD")).unwrap();

    let error = verify_clone(&fake).unwrap_err();

    assert!(error.contains("link or shares state"), "{error}");
    // A genuine, independent copy passes.
    let copy = scratch.root.join("copy");
    let status = Command::new("cp")
        .arg("-R")
        .arg(scratch.repo())
        .arg(&copy)
        .status()
        .unwrap();
    assert!(status.success());
    verify_clone(&copy).unwrap();
    assert_eq!(
        std::fs::read(scratch.repo().join(".git/HEAD")).unwrap(),
        head
    );
}

#[test]
fn shared_git_state_is_detected() {
    let scratch = Scratch::new();
    let git_dir = scratch.repo().join(".git");
    assert!(!crate::parallel_checkout::safety::git_state_is_shared(
        &git_dir
    ));
    for shared in ["commondir", "objects/info/alternates"] {
        let path = git_dir.join(shared);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, "../elsewhere\n").unwrap();
        assert!(
            crate::parallel_checkout::safety::git_state_is_shared(&git_dir),
            "{shared}"
        );
        std::fs::remove_file(path).unwrap();
    }
    assert!(!crate::parallel_checkout::safety::git_state_is_shared(
        &git_dir
    ));
}

/// What a clone made while the source `.git` was being swapped to a symlink
/// looks like: the original's lock file appears, and the clone's `.git` links
/// to the original's.
#[cfg(unix)]
fn clone_made_during_a_swap(source: &Path, destination: &Path) -> std::io::Result<()> {
    std::fs::write(source.join(".git/index.lock"), "held by a git command\n")?;
    std::fs::create_dir_all(destination)?;
    std::os::unix::fs::symlink(source.join(".git"), destination.join(".git"))
}

#[cfg(unix)]
#[test]
fn a_clone_swapped_to_a_symlinked_git_never_deletes_the_sources_lock() {
    let scratch = Scratch::new();
    let lock = scratch.repo().join(".git/index.lock");
    let head = std::fs::read(scratch.repo().join(".git/HEAD")).unwrap();

    let checkout = scratch.create(clone_made_during_a_swap);

    assert!(lock.exists(), "the source's index.lock was deleted");
    assert_eq!(checkout.method, CheckoutMethod::Worktree);
    assert_eq!(
        std::fs::read(scratch.repo().join(".git/HEAD")).unwrap(),
        head
    );
    std::fs::remove_file(lock).unwrap();
    remove(&checkout, &scratch.checkouts()).unwrap();
}

#[test]
fn a_copied_lock_is_removed_only_from_a_verified_clone() {
    let scratch = Scratch::new();
    let copy = scratch.root.join("copy");
    let status = Command::new("cp")
        .arg("-R")
        .arg(scratch.repo())
        .arg(&copy)
        .status()
        .unwrap();
    assert!(status.success());
    std::fs::write(copy.join(".git/index.lock"), "x").unwrap();
    verify_clone(&copy).unwrap();
    assert!(crate::parallel_checkout::safety::remove_copied_lock(&copy));
    assert!(!copy.join(".git/index.lock").exists());
}

#[cfg(unix)]
#[test]
fn clones_with_links_or_shared_state_inside_their_git_directory_are_rejected() {
    let scratch = Scratch::new();
    let copy = |name: &str| {
        let path = scratch.root.join(name);
        let status = Command::new("cp")
            .arg("-R")
            .arg(scratch.repo())
            .arg(&path)
            .status()
            .unwrap();
        assert!(status.success());
        path
    };
    let linked_objects = copy("linked-objects");
    std::fs::rename(
        linked_objects.join(".git/objects"),
        scratch.root.join("objects-elsewhere"),
    )
    .unwrap();
    std::os::unix::fs::symlink(
        scratch.root.join("objects-elsewhere"),
        linked_objects.join(".git/objects"),
    )
    .unwrap();
    let common = copy("common");
    std::fs::write(common.join(".git/commondir"), "../../repo/.git\n").unwrap();
    let alternates = copy("alternates");
    std::fs::create_dir_all(alternates.join(".git/objects/info")).unwrap();
    std::fs::write(
        alternates.join(".git/objects/info/alternates"),
        "/elsewhere/objects\n",
    )
    .unwrap();
    let outside_worktree = copy("core-worktree");
    git_in(
        &outside_worktree,
        &["config", "core.worktree", scratch.repo().to_str().unwrap()],
    );
    for path in [linked_objects, common, alternates, outside_worktree] {
        assert!(verify_clone(&path).is_err(), "{}", path.display());
    }
}

#[test]
fn every_checkout_carries_an_ownership_marker_and_removal_deletes_it() {
    let scratch = Scratch::new();
    let checkout = scratch.create(no_clone);
    let marker = checkout.path.with_file_name("par-1.aster-checkout");
    assert!(std::fs::read_to_string(&marker)
        .unwrap()
        .starts_with(&checkout.path.display().to_string()));
    remove(&checkout, &scratch.checkouts()).unwrap();
    assert!(!checkout.path.exists() && !marker.exists());
}

#[test]
fn removal_refuses_paths_that_are_not_markered_checkouts_inside_the_worktrees_directory() {
    let scratch = Scratch::new();
    let genuine = scratch.create(no_clone);
    let hostile = |path: PathBuf| Checkout {
        path,
        ..genuine.clone()
    };
    let refused = |path: PathBuf| {
        let error = remove(&hostile(path.clone()), &scratch.checkouts()).unwrap_err();
        assert!(
            matches!(error, RemoveError::Refused(_)),
            "{}: {error}",
            path.display()
        );
    };
    // Outside the worktrees directory entirely.
    let outside = scratch.root.join("precious");
    std::fs::create_dir_all(&outside).unwrap();
    std::fs::write(outside.join("file"), "keep").unwrap();
    refused(outside.clone());
    // Inside it, but never created by Aster: no marker.
    let unmarked = scratch.checkouts().join("repo/unmarked");
    std::fs::create_dir_all(&unmarked).unwrap();
    std::fs::write(unmarked.join("file"), "keep").unwrap();
    refused(unmarked.clone());
    // The worktrees directory itself, and a parent of it.
    refused(scratch.checkouts());
    refused(scratch.root.clone());
    // A symlink inside the directory that leads outside it.
    #[cfg(unix)]
    {
        let link = scratch.checkouts().join("repo/link");
        std::os::unix::fs::symlink(&outside, &link).unwrap();
        std::fs::write(
            link.with_file_name("link.aster-checkout"),
            format!("{}\n", link.display()),
        )
        .unwrap();
        refused(link);
    }
    // A marker that names some other path does not authorise this one.
    std::fs::write(
        unmarked.with_file_name("unmarked.aster-checkout"),
        format!("{}\n", outside.display()),
    )
    .unwrap();
    refused(unmarked.clone());

    assert_eq!(
        std::fs::read_to_string(outside.join("file")).unwrap(),
        "keep"
    );
    assert_eq!(
        std::fs::read_to_string(unmarked.join("file")).unwrap(),
        "keep"
    );
    // The genuine checkout is still removed.
    remove(&genuine, &scratch.checkouts()).unwrap();
    assert!(!genuine.path.exists());
    // A checkout already gone is removed again without complaint.
    remove(&genuine, &scratch.checkouts()).unwrap();
}

#[cfg(unix)]
#[test]
fn an_existing_marker_path_refuses_the_checkout_and_is_never_written_through() {
    let scratch = Scratch::new();
    let config = scratch.repo().join(".git/config");
    let before = std::fs::read(&config).unwrap();
    let marker = scratch.checkouts().join("repo/par-1.aster-checkout");
    std::fs::create_dir_all(marker.parent().unwrap()).unwrap();
    for plant in ["symlink", "file"] {
        let _ = std::fs::remove_file(&marker);
        match plant {
            "symlink" => std::os::unix::fs::symlink(&config, &marker).unwrap(),
            _ => std::fs::write(&marker, "somebody else's\n").unwrap(),
        }

        let result = create_with(
            &scratch.repo(),
            &scratch.repo(),
            &scratch.checkouts(),
            "repo",
            "par-1",
            &[],
            no_clone,
        );

        let error = result.unwrap_err();
        assert!(error.contains("ownership marker"), "{plant}: {error}");
        assert_eq!(std::fs::read(&config).unwrap(), before, "{plant}");
        assert!(!scratch.checkouts().join("repo/par-1").exists(), "{plant}");
        assert!(
            std::fs::symlink_metadata(&marker).is_ok(),
            "{plant}: not ours to delete"
        );
        assert!(!scratch.git(&["worktree", "list"]).contains("par-1"));
    }
}

#[cfg(unix)]
#[test]
fn a_marker_that_is_a_symlink_does_not_authorise_removal() {
    let scratch = Scratch::new();
    let checkout = scratch.create(no_clone);
    let marker = checkout.path.with_file_name("par-1.aster-checkout");
    let copy = scratch.root.join("marker-copy");
    std::fs::rename(&marker, &copy).unwrap();
    std::os::unix::fs::symlink(&copy, &marker).unwrap();

    let error = remove(&checkout, &scratch.checkouts()).unwrap_err();

    assert!(matches!(error, RemoveError::Refused(_)), "{error}");
    assert!(checkout.path.exists() && copy.exists());
}

#[test]
fn a_stale_path_outside_the_worktrees_directory_leaves_leftovers_alone() {
    let scratch = Scratch::new();
    let genuine = scratch.create(no_clone);
    let elsewhere = scratch.root.join("elsewhere");
    std::fs::create_dir_all(&elsewhere).unwrap();
    let stale = elsewhere.join("missing");
    let marker = elsewhere.join("missing.aster-checkout");
    std::fs::write(
        &marker,
        format!("{}\n{}\n", stale.display(), scratch.repo().display()),
    )
    .unwrap();
    let stale = Checkout {
        path: stale,
        ..genuine.clone()
    };

    let error = remove(&stale, &scratch.checkouts()).unwrap_err();

    assert!(matches!(error, RemoveError::Refused(_)), "{error}");
    assert!(marker.exists());
    remove(&genuine, &scratch.checkouts()).unwrap();
}

#[test]
fn a_checkout_already_gone_inside_the_directory_has_only_its_marker_cleaned() {
    let scratch = Scratch::new();
    let genuine = scratch.create(no_clone);
    let marker = genuine.path.with_file_name("par-1.aster-checkout");
    std::fs::remove_dir_all(&genuine.path).unwrap();
    assert!(marker.exists());

    remove(&genuine, &scratch.checkouts()).unwrap();

    assert!(!marker.exists());
    assert!(!scratch
        .git(&["worktree", "list", "--porcelain"])
        .contains(&genuine.path.display().to_string()));
    // Without a marker there is nothing to do, and nothing is touched.
    remove(&genuine, &scratch.checkouts()).unwrap();
}

#[test]
fn a_marker_naming_another_repository_blocks_cleanup_of_a_gone_checkout() {
    let scratch = Scratch::new();
    let genuine = scratch.create(no_clone);
    let marker = genuine.path.with_file_name("par-1.aster-checkout");
    std::fs::remove_dir_all(&genuine.path).unwrap();
    let other = Checkout {
        repo_root: scratch.root.join("another-repo"),
        ..genuine.clone()
    };

    let error = remove(&other, &scratch.checkouts()).unwrap_err();

    assert!(matches!(error, RemoveError::Refused(_)), "{error}");
    assert!(marker.exists());
    remove(&genuine, &scratch.checkouts()).unwrap();
}
