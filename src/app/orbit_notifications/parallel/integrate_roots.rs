//! Repositories recorded by older builds may be named through a symlink. Their
//! real paths are looked up once on load, on a background thread, and the
//! integrate steps are rewritten to them, so every integration compares the
//! same names. No integration starts until that is done.
use std::collections::BTreeSet;
use std::path::PathBuf;
use std::sync::mpsc::{self, TryRecvError};

use crate::app::App;

impl App {
    /// Starts looking up the real paths of the repositories in the ledger.
    pub(in crate::app::orbit_notifications) fn normalize_integration_roots(&mut self) {
        let roots: BTreeSet<PathBuf> = self
            .orbit_notifications
            .tasks
            .iter()
            .flat_map(|task| &task.steps)
            .filter_map(|step| step.integrate.as_ref())
            .map(|integrate| integrate.repo_root.clone())
            .collect();
        if roots.is_empty() {
            return;
        }
        let (send, found) = mpsc::channel();
        let spawned = crate::thread_spawn::spawn_named("aster-orbit-repo-roots", move || {
            let real: Vec<(PathBuf, PathBuf)> = roots
                .into_iter()
                .filter_map(|root| {
                    Some((
                        root.clone(),
                        crate::parallel_checkout::checked::real_path(&root).ok()?,
                    ))
                })
                .filter(|(root, real)| root != real)
                .collect();
            let _ = send.send(real);
        });
        match spawned {
            Ok(_) => self.orbit_notifications.parallel.normalizing = Some(found),
            Err(error) => tracing::warn!(%error, "cannot normalize integration repositories"),
        }
    }

    /// Applies the real paths once found.
    pub(super) fn poll_root_normalization(&mut self) {
        let Some(found) = self.orbit_notifications.parallel.normalizing.as_ref() else {
            return;
        };
        let real = match found.try_recv() {
            Ok(real) => real,
            Err(TryRecvError::Empty) => return,
            Err(TryRecvError::Disconnected) => Vec::new(),
        };
        self.orbit_notifications.parallel.normalizing = None;
        for index in 0..self.orbit_notifications.tasks.len() {
            let task = &self.orbit_notifications.tasks[index];
            let stale = task.steps.iter().any(|step| {
                step.integrate
                    .as_ref()
                    .is_some_and(|i| real.iter().any(|(root, _)| *root == i.repo_root))
            });
            if !stale {
                continue;
            }
            for step in self.orbit_notifications.task_mut(index).steps.iter_mut() {
                let Some(integrate) = step.integrate.as_mut() else {
                    continue;
                };
                if let Some((_, canonical)) =
                    real.iter().find(|(root, _)| *root == integrate.repo_root)
                {
                    integrate.repo_root = canonical.clone();
                }
            }
        }
    }
}
