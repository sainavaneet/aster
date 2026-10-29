//! Called only by the background service supervisor while its zombie group
//! leader pins the PGID. Failure retains ownership rather than assuming empty.
pub(crate) fn service_group_has_members(leader: u32) -> std::io::Result<bool> {
    let mut capacity = 16;
    loop {
        let mut pids = vec![0 as libc::pid_t; capacity];
        let bytes = pids.len() * std::mem::size_of::<libc::pid_t>();
        let returned = unsafe {
            libc::proc_listpids(
                super::PROC_PGRP_ONLY,
                leader,
                pids.as_mut_ptr().cast(),
                bytes as i32,
            )
        };
        if returned < 0 {
            return Err(std::io::Error::last_os_error());
        }
        if (returned as usize) < bytes {
            return Ok(pids.into_iter().any(|pid| pid > 0 && pid as u32 != leader));
        }
        capacity *= 2;
        if capacity > 1_048_576 {
            return Err(std::io::Error::other("service process group too large"));
        }
    }
}
