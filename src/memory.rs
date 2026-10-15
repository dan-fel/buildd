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

/// Processes of the build, including descendants that start their own process
/// groups (as nextest does), and group members whose parent already exited.
fn related_processes(root: u32, processes: &[(u32, u32, u32)]) -> std::collections::HashSet<u32> {
    let mut children = std::collections::HashMap::<_, Vec<_>>::new();
    let mut pending = vec![root];
    for &(pid, parent, group) in processes {
        children.entry(parent).or_default().push(pid);
        if group == root {
            pending.push(pid);
        }
    }
    let mut related = std::collections::HashSet::new();
    while let Some(pid) = pending.pop() {
        if related.insert(pid) {
            pending.extend(children.get(&pid).into_iter().flatten());
        }
    }
    related
}

/// Summed resident memory of the build's process tree and group. Shared pages
/// count once per process. The caller keeps the root unreaped during sampling,
/// so another process cannot reuse its identity.
pub(crate) fn tree_resident(root: u32) -> u64 {
    related_processes(root, &processes())
        .into_iter()
        .map(resident)
        .sum()
}

#[cfg(target_os = "macos")]
fn processes() -> Vec<(u32, u32, u32)> {
    // PROC_ALL_PIDS of <sys/proc_info.h>.
    const ALL: u32 = 1;
    let mut pids = vec![0 as libc::pid_t; 256];
    let listed = loop {
        let capacity = i32::try_from(pids.len() * std::mem::size_of::<libc::pid_t>())
            .expect("a small process buffer");
        // SAFETY: pids is valid for capacity bytes of writes.
        let bytes = unsafe { libc::proc_listpids(ALL, 0, pids.as_mut_ptr().cast(), capacity) };
        let Ok(bytes) = usize::try_from(bytes) else {
            return Vec::new();
        };
        let count = bytes / std::mem::size_of::<libc::pid_t>();
        if count < pids.len() {
            break count;
        }
        pids.resize(pids.len() * 2, 0);
    };
    pids[..listed]
        .iter()
        .filter_map(|&pid| {
            // SAFETY: proc_bsdinfo holds integers and byte arrays, for which
            // zeroes are valid; info is valid for size bytes of writes.
            let mut info: libc::proc_bsdinfo = unsafe { std::mem::zeroed() };
            let size = i32::try_from(std::mem::size_of::<libc::proc_bsdinfo>())
                .expect("proc_bsdinfo is small");
            let read = unsafe {
                libc::proc_pidinfo(pid, libc::PROC_PIDTBSDINFO, 0, (&raw mut info).cast(), size)
            };
            (read == size).then_some((info.pbi_pid, info.pbi_ppid, info.pbi_pgid))
        })
        .collect()
}

#[cfg(target_os = "macos")]
fn resident(pid: u32) -> u64 {
    // SAFETY: proc_taskinfo holds integers, for which zeroes are valid.
    let mut info: libc::proc_taskinfo = unsafe { std::mem::zeroed() };
    let size =
        i32::try_from(std::mem::size_of::<libc::proc_taskinfo>()).expect("proc_taskinfo is small");
    // SAFETY: info is valid for size bytes of writes.
    let read = unsafe {
        libc::proc_pidinfo(
            i32::try_from(pid).expect("process ids fit pid_t"),
            libc::PROC_PIDTASKINFO,
            0,
            (&raw mut info).cast(),
            size,
        )
    };
    // Processes may exit between enumeration and sampling.
    if read == size {
        info.pti_resident_size
    } else {
        0
    }
}

#[cfg(not(target_os = "macos"))]
fn processes() -> Vec<(u32, u32, u32)> {
    let Ok(processes) = std::fs::read_dir("/proc") else {
        return Vec::new();
    };
    processes
        .filter_map(|entry| {
            let path = entry.ok()?.path();
            let pid = path.file_name()?.to_str()?.parse().ok()?;
            let stat = std::fs::read_to_string(path.join("stat")).ok()?;
            // Fields after the parenthesized name: state, ppid, pgrp, ...
            let mut fields = stat[stat.rfind(')')? + 1..].split_whitespace();
            fields.next()?;
            Some((
                pid,
                fields.next()?.parse().ok()?,
                fields.next()?.parse().ok()?,
            ))
        })
        .collect()
}

#[cfg(not(target_os = "macos"))]
fn resident(pid: u32) -> u64 {
    // SAFETY: sysconf has no preconditions.
    let page = u64::try_from(unsafe { libc::sysconf(libc::_SC_PAGESIZE) })
        .expect("the system knows its page size");
    std::fs::read_to_string(format!("/proc/{pid}/statm"))
        .ok()
        .and_then(|statm| statm.split_whitespace().nth(1)?.parse::<u64>().ok())
        .map_or(0, |pages| pages * page)
}

pub(crate) enum Sample {
    Testing,
    Stop,
}

/// Owns a sampler's thread and lifetime. It must stop before the root is reaped,
/// including when a build runs a second Cargo invocation to skip passed tests.
pub(crate) struct Monitor {
    pub(crate) events: crossbeam_channel::Sender<Sample>,
    thread: Option<std::thread::JoinHandle<(u64, Option<u64>)>>,
}

impl Monitor {
    pub(crate) fn start(root: u32) -> std::io::Result<Self> {
        let (events, received) = crossbeam_channel::unbounded();
        let thread = std::thread::Builder::new()
            .name("buildd-memory".into())
            .spawn(move || {
                let mut peak = 0;
                let mut tests: Option<u64> = None;
                let mut testing = false;
                loop {
                    let memory = tree_resident(root);
                    peak = peak.max(memory);
                    if testing && memory > 0 {
                        tests = Some(tests.unwrap_or(0).max(memory));
                    }
                    match received.recv_timeout(std::time::Duration::from_secs(1)) {
                        Ok(Sample::Testing) => testing = true,
                        Ok(Sample::Stop)
                        | Err(crossbeam_channel::RecvTimeoutError::Disconnected) => break,
                        Err(crossbeam_channel::RecvTimeoutError::Timeout) => {}
                    }
                }
                (peak, tests)
            })?;
        Ok(Self {
            events,
            thread: Some(thread),
        })
    }

    pub(crate) fn finish(mut self) -> (u64, Option<u64>) {
        let _ = self.events.send(Sample::Stop);
        self.thread
            .take()
            .expect("a sampler finishes once")
            .join()
            .expect("memory sampler panicked")
    }
}

impl Drop for Monitor {
    fn drop(&mut self) {
        if let Some(thread) = self.thread.take() {
            let _ = self.events.send(Sample::Stop);
            thread.join().expect("memory sampler panicked");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn descendants_in_other_groups_and_reparented_group_members_are_included() {
        assert_eq!(
            related_processes(
                10,
                &[
                    (10, 1, 10),
                    (20, 10, 20),
                    (30, 20, 30),
                    (40, 1, 10),
                    (50, 40, 50),
                    (60, 1, 60),
                ]
            ),
            [10, 20, 30, 40, 50].into_iter().collect()
        );
    }

    #[test]
    fn a_child_in_a_separate_group_is_found_in_the_live_process_tree() {
        use std::io::BufRead as _;
        use std::os::unix::process::CommandExt as _;
        use std::process::{Command, Stdio};
        let mut child = Command::new("sh")
            .args(["-c", "echo ready; read line"])
            .process_group(0)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .spawn()
            .unwrap();
        let mut ready = String::new();
        std::io::BufReader::new(child.stdout.take().unwrap())
            .read_line(&mut ready)
            .unwrap();
        assert_eq!(ready, "ready\n");
        let listed = processes();
        let included = related_processes(std::process::id(), &listed);
        let memory = tree_resident(child.id());
        drop(child.stdin.take());
        child.wait().unwrap();
        assert!(
            listed
                .iter()
                .any(|&(pid, _, group)| pid == child.id() && group == pid)
        );
        assert!(included.contains(&child.id()));
        assert!(memory > 0);
    }

    #[test]
    fn sampling_records_testing_and_stops_without_waiting_for_the_sample_interval() {
        assert!(physical() > 1 << 30);
        let monitor = Monitor::start(std::process::id()).unwrap();
        monitor.events.send(Sample::Testing).unwrap();
        let (peak, tests) = monitor.finish();
        assert!(tests.is_some_and(|tests| tests > 0 && tests <= peak));
    }
}
