//! Build processes on Windows: each runs in a Job Object, which holds
//! everything it starts and measures what they use. A job cannot be escaped
//! by a process that outlives its parent, so stopping a build needs no walk of
//! its process tree.

use std::fs::File;
use std::os::windows::io::{AsRawHandle as _, FromRawHandle as _, OwnedHandle};
use std::os::windows::process::{CommandExt as _, ExitStatusExt as _};
use std::process::{Command, ExitStatus, Stdio};
use std::sync::Arc;

use windows_sys::Win32::Foundation::{CloseHandle, HANDLE, INVALID_HANDLE_VALUE, WAIT_OBJECT_0};
use windows_sys::Win32::System::Diagnostics::ToolHelp::{
    CreateToolhelp32Snapshot, TH32CS_SNAPTHREAD, THREADENTRY32, Thread32First, Thread32Next,
};
use windows_sys::Win32::System::JobObjects::{
    AssignProcessToJobObject, CreateJobObjectW, JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
    JOBOBJECT_BASIC_PROCESS_ID_LIST, JOBOBJECT_EXTENDED_LIMIT_INFORMATION,
    JobObjectBasicProcessIdList, JobObjectExtendedLimitInformation, QueryInformationJobObject,
    SetInformationJobObject, TerminateJobObject,
};
use windows_sys::Win32::System::ProcessStatus::{GetProcessMemoryInfo, PROCESS_MEMORY_COUNTERS};
use windows_sys::Win32::System::Threading::{
    BELOW_NORMAL_PRIORITY_CLASS, CREATE_NO_WINDOW, CREATE_SUSPENDED, GetExitCodeProcess, INFINITE,
    OpenProcess, OpenThread, PROCESS_QUERY_LIMITED_INFORMATION, ResumeThread,
    THREAD_SUSPEND_RESUME, WaitForSingleObject,
};

use super::Process;
use crate::config::Priority;
use crate::log::log;
use crate::protocol::Usage;

/// Windows build processes inherit nothing beyond their standard streams: a
/// jobserver is a named semaphore they open by its name.
pub(crate) type Descriptor = std::convert::Infallible;

/// A build process and everything it started, in one job. Closing the job's
/// last handle kills whatever still runs in it.
#[derive(Clone)]
pub(crate) struct Group(Arc<Handles>);

struct Handles {
    job: OwnedHandle,
    process: OwnedHandle,
}

impl Group {
    /// Stops the group at once: Windows has no signal a detached daemon can
    /// send a process tree to ask it to stop.
    pub(crate) fn terminate(&self) {
        self.kill();
    }

    pub(crate) fn kill(&self) {
        // SAFETY: the job's handle is open for as long as `self`.
        if unsafe { TerminateJobObject(self.0.job.as_raw_handle(), 1) } == 0 {
            log!(
                "could not stop a build's job: {}",
                std::io::Error::last_os_error()
            );
        }
    }

    /// Waits for the build process itself to exit.
    pub(crate) fn wait_exited(&self) {
        // SAFETY: the process's handle is open for as long as `self`.
        let waited = unsafe { WaitForSingleObject(self.0.process.as_raw_handle(), INFINITE) };
        assert_eq!(
            waited, WAIT_OBJECT_0,
            "waiting for our own child fails only by a bug"
        );
    }

    /// Kills what remains of the job after its first process exited.
    pub(crate) fn kill_leftovers(&self) {
        self.kill();
    }

    /// How the build process exited, and what the whole job used. Peak memory
    /// is committed memory, the figure a job accounts for.
    pub(crate) fn reap(self) -> (ExitStatus, Usage) {
        let mut code = 0;
        // SAFETY: the handle is open and `code` is valid for writes.
        let read = unsafe { GetExitCodeProcess(self.0.process.as_raw_handle(), &raw mut code) };
        assert_ne!(read, 0, "an exited child has an exit code");
        // SAFETY: the structure holds integers, for which zeroes are valid.
        let mut limits: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = unsafe { std::mem::zeroed() };
        // SAFETY: the job is open and `limits` is valid for the size given.
        let queried = unsafe {
            QueryInformationJobObject(
                self.0.job.as_raw_handle(),
                JobObjectExtendedLimitInformation,
                (&raw mut limits).cast(),
                size_of_u32::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>(),
                std::ptr::null_mut(),
            )
        };
        assert_ne!(queried, 0, "a job we created can be queried");
        let usage = Usage {
            cpu_ms: job_cpu_ms(&self.0.job),
            peak_memory: u64::try_from(limits.PeakJobMemoryUsed).expect("a size fits u64"),
            test_memory: None,
        };
        (ExitStatus::from_raw(code), usage)
    }

    /// The working sets of the job's processes now. Shared pages count once
    /// per process.
    pub(crate) fn resident(&self) -> u64 {
        process_ids(&self.0.job).into_iter().map(resident).sum()
    }
}

/// The CPU time of the job's processes, finished ones too, in milliseconds.
fn job_cpu_ms(job: &OwnedHandle) -> u64 {
    use windows_sys::Win32::System::JobObjects::{
        JOBOBJECT_BASIC_ACCOUNTING_INFORMATION, JobObjectBasicAccountingInformation,
    };
    // SAFETY: the structure holds integers, for which zeroes are valid.
    let mut accounting: JOBOBJECT_BASIC_ACCOUNTING_INFORMATION = unsafe { std::mem::zeroed() };
    // SAFETY: the job is open and `accounting` is valid for the size given.
    let queried = unsafe {
        QueryInformationJobObject(
            job.as_raw_handle(),
            JobObjectBasicAccountingInformation,
            (&raw mut accounting).cast(),
            size_of_u32::<JOBOBJECT_BASIC_ACCOUNTING_INFORMATION>(),
            std::ptr::null_mut(),
        )
    };
    assert_ne!(queried, 0, "a job we created can be queried");
    let ticks = accounting.TotalUserTime + accounting.TotalKernelTime;
    u64::try_from(ticks / 10_000).expect("CPU time is positive")
}

fn size_of_u32<T>() -> u32 {
    u32::try_from(std::mem::size_of::<T>()).expect("a small structure")
}

/// The ids of the processes running in `job`.
fn process_ids(job: &OwnedHandle) -> Vec<u32> {
    let mut capacity = 256_usize;
    loop {
        let header = std::mem::offset_of!(JOBOBJECT_BASIC_PROCESS_ID_LIST, ProcessIdList);
        let size = header + capacity * std::mem::size_of::<usize>();
        let mut buffer = vec![0_usize; size.div_ceil(std::mem::size_of::<usize>())];
        // SAFETY: the job is open and the buffer, aligned for the structure,
        // is valid for `size` bytes.
        let queried = unsafe {
            QueryInformationJobObject(
                job.as_raw_handle(),
                JobObjectBasicProcessIdList,
                buffer.as_mut_ptr().cast(),
                u32::try_from(size).expect("a small list"),
                std::ptr::null_mut(),
            )
        };
        // SAFETY: the buffer starts with the structure, and its header is
        // written on success and on a too-small buffer alike.
        let list = unsafe { &*buffer.as_ptr().cast::<JOBOBJECT_BASIC_PROCESS_ID_LIST>() };
        let assigned = usize::try_from(list.NumberOfAssignedProcesses).expect("a small count");
        if queried != 0 && assigned <= capacity {
            let listed = usize::try_from(list.NumberOfProcessIdsInList).expect("a small count");
            return buffer[header / std::mem::size_of::<usize>()..][..listed]
                .iter()
                .filter_map(|&id| u32::try_from(id).ok())
                .collect();
        }
        if assigned <= capacity {
            return Vec::new();
        }
        capacity = assigned * 2;
    }
}

fn resident(pid: u32) -> u64 {
    // SAFETY: OpenProcess takes integers. Processes may exit before this.
    let process = unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid) };
    if process.is_null() {
        return 0;
    }
    // SAFETY: the handle was just opened and is owned here alone.
    let process = unsafe { OwnedHandle::from_raw_handle(process) };
    // SAFETY: the structure holds integers, for which zeroes are valid.
    let mut counters: PROCESS_MEMORY_COUNTERS = unsafe { std::mem::zeroed() };
    // SAFETY: the handle is open and `counters` is valid for the size given.
    let read = unsafe {
        GetProcessMemoryInfo(
            process.as_raw_handle(),
            &raw mut counters,
            size_of_u32::<PROCESS_MEMORY_COUNTERS>(),
        )
    };
    if read == 0 {
        return 0;
    }
    u64::try_from(counters.WorkingSetSize).expect("a size fits u64")
}

fn new_job() -> std::io::Result<OwnedHandle> {
    // SAFETY: both arguments may be null: default security and no name.
    let job = unsafe { CreateJobObjectW(std::ptr::null(), std::ptr::null()) };
    if job.is_null() {
        return Err(std::io::Error::last_os_error());
    }
    // SAFETY: the handle was just created and is owned here alone.
    let job = unsafe { OwnedHandle::from_raw_handle(job) };
    // SAFETY: the structure holds integers, for which zeroes are valid.
    let mut limits: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = unsafe { std::mem::zeroed() };
    limits.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
    // SAFETY: the job is open and `limits` is valid for the size given.
    let set = unsafe {
        SetInformationJobObject(
            job.as_raw_handle(),
            JobObjectExtendedLimitInformation,
            (&raw const limits).cast(),
            size_of_u32::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>(),
        )
    };
    if set == 0 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(job)
}

/// Resumes the one thread of a process started suspended.
fn resume(pid: u32) -> std::io::Result<()> {
    // SAFETY: both arguments are integers.
    let snapshot: HANDLE = unsafe { CreateToolhelp32Snapshot(TH32CS_SNAPTHREAD, 0) };
    if snapshot == INVALID_HANDLE_VALUE {
        return Err(std::io::Error::last_os_error());
    }
    // SAFETY: the handle was just created and is owned here alone.
    let snapshot = unsafe { OwnedHandle::from_raw_handle(snapshot) };
    // SAFETY: the structure holds integers, for which zeroes are valid.
    let mut entry: THREADENTRY32 = unsafe { std::mem::zeroed() };
    entry.dwSize = size_of_u32::<THREADENTRY32>();
    // SAFETY: the snapshot is open and `entry` states its own size.
    let mut more = unsafe { Thread32First(snapshot.as_raw_handle(), &raw mut entry) } != 0;
    while more {
        if entry.th32OwnerProcessID == pid {
            // SAFETY: OpenThread takes integers.
            let thread = unsafe { OpenThread(THREAD_SUSPEND_RESUME, 0, entry.th32ThreadID) };
            if thread.is_null() {
                return Err(std::io::Error::last_os_error());
            }
            // SAFETY: the thread's handle was just opened and is closed here.
            let resumed = unsafe {
                let resumed = ResumeThread(thread);
                CloseHandle(thread);
                resumed
            };
            if resumed == u32::MAX {
                return Err(std::io::Error::last_os_error());
            }
            return Ok(());
        }
        // SAFETY: as for `Thread32First`.
        more = unsafe { Thread32Next(snapshot.as_raw_handle(), &raw mut entry) } != 0;
    }
    Err(std::io::Error::other(
        "a new process has no thread to resume",
    ))
}

/// Starts `command` in a job of its own at `priority`, which everything it
/// starts keeps. Of `command`, only its program, arguments, environment
/// changes and directory count: this decides the rest.
///
/// # Errors
/// When the process cannot be started.
pub(crate) fn spawn(
    mut command: Command,
    _inherit: &[Descriptor],
    priority: Priority,
) -> std::io::Result<Process> {
    let priority = match priority {
        Priority::Low => BELOW_NORMAL_PRIORITY_CLASS,
        Priority::Normal => 0,
    };
    let job = new_job()?;
    // Started suspended, so it can start nothing outside the job.
    let mut child = command
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .creation_flags(CREATE_SUSPENDED | CREATE_NO_WINDOW | priority)
        .spawn()?;
    let pid = child.id();
    // SAFETY: both handles are open.
    let assigned =
        unsafe { AssignProcessToJobObject(job.as_raw_handle(), child.as_raw_handle()) } != 0;
    let started = if assigned {
        resume(pid)
    } else {
        Err(std::io::Error::last_os_error())
    };
    if let Err(error) = started {
        let _ = child.kill();
        let _ = child.wait();
        return Err(error);
    }
    let stdout = child.stdout.take().expect("the output is piped");
    let stderr = child.stderr.take().expect("the errors are piped");
    Ok(Process {
        group: Group(Arc::new(Handles {
            job,
            process: OwnedHandle::from(child),
        })),
        stdout: File::from(OwnedHandle::from(stdout)),
        stderr: File::from(OwnedHandle::from(stderr)),
    })
}

#[cfg(test)]
mod tests {
    use std::io::Read as _;

    use super::*;

    fn cmd(script: &str) -> Command {
        let mut command = Command::new("cmd");
        command.args(["/d", "/c", script]);
        command
    }

    #[test]
    fn a_process_reads_nothing_and_runs_in_its_directory_with_its_environment() {
        let directory = std::env::temp_dir();
        let mut command = cmd("cd & echo %BUILDD_SPAWN_TEST% & echo error 1>&2");
        command
            .current_dir(&directory)
            .env("BUILDD_SPAWN_TEST", "set");
        let mut process = spawn(command, &[], Priority::Low).unwrap();
        let mut output = String::new();
        process.stdout.read_to_string(&mut output).unwrap();
        let mut errors = String::new();
        process.stderr.read_to_string(&mut errors).unwrap();
        assert!(output.contains("set"), "{output}");
        assert!(errors.contains("error"), "{errors}");
        process.group.wait_exited();
        let (status, _) = process.group.reap();
        assert!(status.success());
        assert!(spawn(Command::new("buildd-no-such-program"), &[], Priority::Low).is_err());
    }

    #[test]
    fn stopping_the_group_stops_what_the_process_started() {
        // The child outlives `cmd` unless the job stops it, and holds the
        // output open until it ends.
        let process = spawn(
            cmd("start /b ping -n 60 127.0.0.1 >nul & ping -n 60 127.0.0.1"),
            &[],
            Priority::Normal,
        )
        .unwrap();
        assert!(process.group.resident() > 0);
        process.group.terminate();
        process.group.wait_exited();
        let mut output = process.stdout;
        let mut text = Vec::new();
        output.read_to_end(&mut text).unwrap();
        assert!(process_ids(&process.group.0.job).is_empty());
        let (status, usage) = process.group.reap();
        assert_eq!(status.code(), Some(1));
        assert!(usage.peak_memory > 0);
    }

    #[test]
    fn a_low_priority_reaches_the_processes_a_build_starts() {
        let mut process = spawn(
            {
                let mut command = Command::new("powershell");
                command.args([
                    "-NoProfile",
                    "-Command",
                    "(Get-Process -Id $PID).PriorityClass",
                ]);
                command
            },
            &[],
            Priority::Low,
        )
        .unwrap();
        let mut output = String::new();
        process.stdout.read_to_string(&mut output).unwrap();
        assert_eq!(output.trim(), "BelowNormal");
        process.group.wait_exited();
    }
}
