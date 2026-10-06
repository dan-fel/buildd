//! Memory as admission sees it: the machine's physical memory, and what a
//! build's process group has resident.

/// The machine's physical memory, in bytes.
#[cfg(target_os = "macos")]
pub(crate) fn physical() -> u64 {
    let mut bytes: u64 = 0;
    let mut size = std::mem::size_of::<u64>();
    // SAFETY: the name is NUL-terminated and `bytes` is valid for `size`
    // bytes of writes.
    let read = unsafe {
        libc::sysctlbyname(
            c"hw.memsize".as_ptr(),
            (&raw mut bytes).cast(),
            &raw mut size,
            std::ptr::null_mut(),
            0,
        )
    };
    assert_eq!(read, 0, "hw.memsize is readable on every Mac");
    bytes
}

/// The machine's physical memory, in bytes.
#[cfg(not(target_os = "macos"))]
pub(crate) fn physical() -> u64 {
    // SAFETY: sysconf has no preconditions.
    let (pages, size) = unsafe {
        (
            libc::sysconf(libc::_SC_PHYS_PAGES),
            libc::sysconf(libc::_SC_PAGESIZE),
        )
    };
    let pages = u64::try_from(pages).expect("the system knows its physical pages");
    let size = u64::try_from(size).expect("the system knows its page size");
    pages * size
}

/// The resident memory of every process in process group `group`, summed,
/// in bytes. Pages processes share count once per process, which errs on
/// the safe side for admission.
#[cfg(target_os = "macos")]
pub(crate) fn group_resident(group: i32) -> u64 {
    /// `PROC_PGRP_ONLY` of `<sys/proc_info.h>`: processes of one group.
    const PROC_PGRP_ONLY: u32 = 2;
    let group = u32::try_from(group).expect("process group ids are positive");
    let mut pids = vec![0 as libc::pid_t; 256];
    let listed = loop {
        let capacity =
            i32::try_from(pids.len() * std::mem::size_of::<libc::pid_t>()).expect("a small buffer");
        // SAFETY: `pids` is valid for `capacity` bytes of writes.
        let bytes = unsafe {
            libc::proc_listpids(PROC_PGRP_ONLY, group, pids.as_mut_ptr().cast(), capacity)
        };
        // The group is gone, or listing failed: nothing is resident.
        let Ok(bytes) = usize::try_from(bytes) else {
            return 0;
        };
        let count = bytes / std::mem::size_of::<libc::pid_t>();
        if count < pids.len() {
            break count;
        }
        pids.resize(pids.len() * 2, 0);
    };
    pids[..listed]
        .iter()
        .filter(|pid| **pid > 0)
        .map(|pid| {
            // SAFETY: proc_taskinfo holds integers, for which zeroes are valid.
            let mut info: libc::proc_taskinfo = unsafe { std::mem::zeroed() };
            let size = i32::try_from(std::mem::size_of::<libc::proc_taskinfo>())
                .expect("proc_taskinfo is small");
            // SAFETY: `info` is valid for `size` bytes of writes.
            let read = unsafe {
                libc::proc_pidinfo(
                    *pid,
                    libc::PROC_PIDTASKINFO,
                    0,
                    (&raw mut info).cast(),
                    size,
                )
            };
            // A process that exited since it was listed holds nothing.
            if read == size {
                info.pti_resident_size
            } else {
                0
            }
        })
        .sum()
}

/// The resident memory of every process in process group `group`, summed,
/// in bytes. Pages processes share count once per process, which errs on
/// the safe side for admission.
#[cfg(not(target_os = "macos"))]
pub(crate) fn group_resident(group: i32) -> u64 {
    // SAFETY: sysconf has no preconditions.
    let page = u64::try_from(unsafe { libc::sysconf(libc::_SC_PAGESIZE) })
        .expect("the system knows its page size");
    let Ok(processes) = std::fs::read_dir("/proc") else {
        return 0;
    };
    processes
        .filter_map(|entry| {
            let path = entry.ok()?.path();
            path.file_name()?.to_str()?.parse::<u32>().ok()?;
            // A process that exited since it was listed holds nothing.
            let stat = std::fs::read_to_string(path.join("stat")).ok()?;
            // Fields after the parenthesized name: state, ppid, pgrp, ...
            let after_name = &stat[stat.rfind(')')? + 1..];
            let pgrp = after_name.split_whitespace().nth(2)?.parse::<i32>().ok()?;
            if pgrp != group {
                return None;
            }
            let statm = std::fs::read_to_string(path.join("statm")).ok()?;
            let resident = statm.split_whitespace().nth(1)?.parse::<u64>().ok()?;
            Some(resident * page)
        })
        .sum()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_process_groups_resident_memory_includes_its_members() {
        assert!(physical() > 1 << 30);
        // This test runs in a group of its own or its harness's.
        let group = rustix::process::getpgrp().as_raw_nonzero().get();
        assert!(group_resident(group) > 0);
        assert_eq!(group_resident(i32::MAX), 0, "no such group");
    }
}
