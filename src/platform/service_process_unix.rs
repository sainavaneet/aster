//! Service group ownership is pinned by an unreaped group leader. Never pass
//! this child to Tokio's process reaper or reap it before disarming signaling.
use super::ServicePhase;
use std::io;
use std::process::{Child, Command};

pub(crate) struct ServiceProcess {
    child: Child,
    owned: bool,
    #[cfg(test)]
    signals: usize,
}
impl ServiceProcess {
    pub(crate) fn spawn(
        mut command: Command,
    ) -> io::Result<(
        Self,
        tokio::process::ChildStdout,
        tokio::process::ChildStderr,
    )> {
        let child = command.spawn()?;
        let mut process = Self {
            child,
            owned: true,
            #[cfg(test)]
            signals: 0,
        };
        let stdout = process
            .child
            .stdout
            .take()
            .ok_or_else(|| io::Error::other("missing service stdout"))?;
        let stderr = process
            .child
            .stderr
            .take()
            .ok_or_else(|| io::Error::other("missing service stderr"))?;
        Ok((
            process,
            tokio::process::ChildStdout::from_std(stdout)?,
            tokio::process::ChildStderr::from_std(stderr)?,
        ))
    }
    pub(crate) fn pid(&self) -> u32 {
        self.child.id()
    }
    pub(crate) fn poll(&mut self) -> io::Result<ServicePhase> {
        if !self.owned {
            return Ok(ServicePhase::Exited);
        }
        let mut info: libc::siginfo_t = unsafe { std::mem::zeroed() };
        let result = unsafe {
            libc::waitid(
                libc::P_PID,
                self.pid(),
                &mut info,
                libc::WEXITED | libc::WNOHANG | libc::WNOWAIT,
            )
        };
        if result != 0 {
            let error = io::Error::last_os_error();
            if error.raw_os_error() == Some(libc::ECHILD) {
                // An external reaper released the identity. Never signal it.
                self.owned = false;
                return Ok(ServicePhase::Exited);
            }
            return Err(error);
        }
        if unsafe { info.si_pid() } == 0 {
            return Ok(ServicePhase::Running);
        }
        let has_members = super::service_group_has_members(self.pid()).unwrap_or_else(|error| {
            tracing::warn!(%error, pid = self.pid(), "cannot inspect service group; retaining its pinned leader");
            true
        });
        if has_members {
            return Ok(ServicePhase::LauncherExited);
        }
        // The zombie still pins PID/PGID here. Disarm before reaping, so no
        // future stop, restart, shutdown or Drop can signal a recycled ID.
        self.owned = false;
        self.child.wait()?;
        Ok(ServicePhase::Exited)
    }
    pub(crate) fn terminate(&mut self) {
        if self.owned {
            #[cfg(test)]
            {
                self.signals += 1;
            }
            unsafe {
                libc::kill(-(self.pid() as i32), libc::SIGKILL);
            }
            self.owned = false;
            let _ = self.child.wait();
        }
    }
}
impl Drop for ServiceProcess {
    fn drop(&mut self) {
        self.terminate();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn exited_service_disarms_before_reaping_and_cannot_signal_again() {
        let mut command = crate::platform::detached_custom_command_process("true");
        crate::platform::configure_status_command(&mut command);
        command
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped());
        // Pipe conversion needs a Tokio I/O driver, but spawning and polling do not.
        let child = command.spawn().unwrap();
        let mut process = ServiceProcess {
            child,
            owned: true,
            signals: 0,
        };
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        loop {
            if process.poll().unwrap() == ServicePhase::Exited {
                break;
            }
            assert!(std::time::Instant::now() < deadline);
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        assert!(!process.owned);
        assert_eq!(unsafe { libc::kill(process.pid() as i32, 0) }, -1);
        process.terminate();
        process.terminate();
        assert_eq!(process.signals, 0);
    }
    #[test]
    fn background_group_retains_unreaped_leader_until_termination() {
        let mut command = crate::platform::detached_custom_command_process("sleep 60 &");
        crate::platform::configure_status_command(&mut command);
        let child = command.spawn().unwrap();
        let mut process = ServiceProcess {
            child,
            owned: true,
            signals: 0,
        };
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        loop {
            if process.poll().unwrap() == ServicePhase::LauncherExited {
                break;
            }
            assert!(std::time::Instant::now() < deadline);
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        assert!(process.owned);
        // The exited leader still exists: its numeric ID cannot be reused.
        assert_eq!(unsafe { libc::kill(process.pid() as i32, 0) }, 0);
        process.terminate();
        assert!(!process.owned);
        assert_eq!(process.signals, 1);
        process.terminate();
        assert_eq!(process.signals, 1);
    }
    #[test]
    fn unexpected_external_reap_disarms_without_signaling() {
        let mut command = crate::platform::detached_custom_command_process("true");
        crate::platform::configure_status_command(&mut command);
        let child = command.spawn().unwrap();
        let mut process = ServiceProcess {
            child,
            owned: true,
            signals: 0,
        };
        process.child.wait().unwrap();
        assert_eq!(process.poll().unwrap(), ServicePhase::Exited);
        process.terminate();
        assert!(!process.owned);
        assert_eq!(process.signals, 0);
    }
}
