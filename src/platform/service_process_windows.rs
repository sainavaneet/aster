//! Windows job handles retain identity independently of process-ID reuse.
use super::{ServicePhase, StatusCommandGuard};
use std::{io, process::Command};
pub(crate) struct ServiceProcess {
    child: tokio::process::Child,
    guard: StatusCommandGuard,
}
impl ServiceProcess {
    pub(crate) fn spawn(
        command: Command,
    ) -> io::Result<(
        Self,
        tokio::process::ChildStdout,
        tokio::process::ChildStderr,
    )> {
        let mut command = tokio::process::Command::from(command);
        command.kill_on_drop(true);
        let mut child = command.spawn()?;
        let guard = StatusCommandGuard::new(&child)?;
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| io::Error::other("missing service stdout"))?;
        let stderr = child
            .stderr
            .take()
            .ok_or_else(|| io::Error::other("missing service stderr"))?;
        Ok((Self { child, guard }, stdout, stderr))
    }
    pub(crate) fn pid(&self) -> u32 {
        self.child.id().unwrap_or_default()
    }
    pub(crate) fn poll(&mut self) -> io::Result<ServicePhase> {
        if self.child.try_wait()?.is_none() {
            return Ok(ServicePhase::Running);
        }
        if self.guard.has_service_processes()? {
            return Ok(ServicePhase::LauncherExited);
        }
        self.guard.terminate();
        Ok(ServicePhase::Exited)
    }
    pub(crate) fn terminate(&mut self) {
        self.guard.terminate();
    }
}
