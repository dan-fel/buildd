//! Starting the process a build runs: Cargo, or a command of the client's
//! own. It leads a process group of its own so a cancellation reaches
//! everything it started, reads nothing, has its output piped back, and runs
//! at the build's [`Priority`], which everything it starts keeps.

use std::fs::File;
use std::os::fd::{OwnedFd, RawFd};
use std::process::Command;

use rustix::process::Pid;

use crate::config::Priority;

/// A started build process. Whoever started it reaps it.
pub(crate) struct Process {
    pub(crate) pid: Pid,
    pub(crate) stdout: File,
    pub(crate) stderr: File,
}

/// Starts `command`, with `inherit` open in it as well as its standard
/// streams, at `priority`. Of `command`, only its program, arguments,
/// environment changes and directory count: this decides the rest.
///
/// # Errors
/// When the process cannot be started.
#[cfg(target_os = "linux")]
pub(crate) fn spawn(
    mut command: Command,
    inherit: &[RawFd],
    priority: Priority,
) -> std::io::Result<Process> {
    use std::os::unix::process::CommandExt as _;
    use std::process::Stdio;

    let inherit = inherit.to_vec();
    command
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .process_group(0);
    // SAFETY: the closure makes only async-signal-safe system calls, on
    // descriptors the caller keeps open until the process started.
    unsafe {
        command.pre_exec(move || {
            for &fd in &inherit {
                let fd = std::os::fd::BorrowedFd::borrow_raw(fd);
                rustix::io::fcntl_setfd(fd, rustix::io::FdFlags::empty())?;
            }
            match priority {
                Priority::Low => lower(),
                Priority::Normal => Ok(()),
            }
        });
    }
    let mut child = command.spawn()?;
    let stdout = child.stdout.take().expect("the output is piped");
    let stderr = child.stderr.take().expect("the errors are piped");
    Ok(Process {
        pid: Pid::from_child(&child),
        stdout: File::from(OwnedFd::from(stdout)),
        stderr: File::from(OwnedFd::from(stderr)),
    })
}

/// Lowers the calling process, between fork and exec, below interactive
/// work: nice at least 10, `SCHED_BATCH`, and the lowest best-effort I/O
/// priority. Its children inherit all three.
#[cfg(target_os = "linux")]
fn lower() -> std::io::Result<()> {
    const NICE: i32 = 10;
    /// `IOPRIO_PRIO_VALUE(IOPRIO_CLASS_BE, 7)`, for `IOPRIO_WHO_PROCESS`.
    const IOPRIO_WHO_PROCESS: libc::c_int = 1;
    const BEST_EFFORT_LOWEST: libc::c_int = (2 << 13) | 7;

    let nice = rustix::process::getpriority_process(None)?;
    if nice < NICE {
        rustix::process::setpriority_process(None, NICE)?;
    }
    let parameters = libc::sched_param { sched_priority: 0 };
    // SAFETY: `parameters` is valid for reads for the call.
    if unsafe { libc::sched_setscheduler(0, libc::SCHED_BATCH, &raw const parameters) } != 0 {
        return Err(std::io::Error::last_os_error());
    }
    // SAFETY: ioprio_set takes three integers and touches no memory.
    if unsafe {
        libc::syscall(
            libc::SYS_ioprio_set,
            IOPRIO_WHO_PROCESS,
            0,
            BEST_EFFORT_LOWEST,
        )
    } != 0
    {
        return Err(std::io::Error::last_os_error());
    }
    Ok(())
}

/// Starts `command`, with `inherit` open in it as well as its standard
/// streams, at `priority`. Of `command`, only its program, arguments,
/// environment changes and directory count: this decides the rest.
///
/// macOS sets a process's QoS class only as it spawns it, so this uses
/// `posix_spawn` rather than [`Command::spawn`]. The class clamps every
/// thread of the process and of every process it starts.
///
/// # Errors
/// When the process cannot be started.
#[cfg(target_os = "macos")]
pub(crate) fn spawn(
    command: Command,
    inherit: &[RawFd],
    priority: Priority,
) -> std::io::Result<Process> {
    use std::collections::BTreeMap;
    use std::ffi::{CString, OsString};
    use std::os::fd::AsRawFd as _;
    use std::os::unix::ffi::OsStrExt as _;

    /// `QOS_CLASS_UTILITY` from `<sys/qos.h>`.
    const QOS_CLASS_UTILITY: libc::c_uint = 0x11;
    unsafe extern "C" {
        // From <pthread/spawn.h>.
        fn posix_spawnattr_set_qos_class_np(
            attr: *mut libc::posix_spawnattr_t,
            qos_class: libc::c_uint,
        ) -> libc::c_int;
        // From <spawn.h>.
        fn posix_spawn_file_actions_addchdir_np(
            actions: *mut libc::posix_spawn_file_actions_t,
            path: *const libc::c_char,
        ) -> libc::c_int;
        // From <spawn.h>: keeps `fd` open under POSIX_SPAWN_CLOEXEC_DEFAULT.
        fn posix_spawn_file_actions_addinherit_np(
            actions: *mut libc::posix_spawn_file_actions_t,
            fd: libc::c_int,
        ) -> libc::c_int;
    }

    /// A spawn attribute and file action list, destroyed when dropped.
    struct Spawn {
        attributes: libc::posix_spawnattr_t,
        actions: libc::posix_spawn_file_actions_t,
    }
    impl Drop for Spawn {
        fn drop(&mut self) {
            // SAFETY: both were initialized when the value was made.
            unsafe {
                libc::posix_spawnattr_destroy(&raw mut self.attributes);
                libc::posix_spawn_file_actions_destroy(&raw mut self.actions);
            }
        }
    }
    fn check(code: libc::c_int) -> std::io::Result<()> {
        match code {
            0 => Ok(()),
            code => Err(std::io::Error::from_raw_os_error(code)),
        }
    }
    fn c_string(text: &std::ffi::OsStr) -> std::io::Result<CString> {
        CString::new(text.as_bytes()).map_err(|_| {
            std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "an argument, variable or directory contains a NUL byte",
            )
        })
    }

    let program = c_string(command.get_program())?;
    let arguments = std::iter::once(Ok(program.clone()))
        .chain(command.get_args().map(c_string))
        .collect::<std::io::Result<Vec<_>>>()?;
    let mut environment = std::env::vars_os().collect::<BTreeMap<OsString, OsString>>();
    for (name, value) in command.get_envs() {
        match value {
            Some(value) => environment.insert(name.to_owned(), value.to_owned()),
            None => environment.remove(name),
        };
    }
    let environment = environment
        .into_iter()
        .map(|(name, value)| {
            let mut entry = name;
            entry.push("=");
            entry.push(value);
            c_string(&entry)
        })
        .collect::<std::io::Result<Vec<_>>>()?;
    let pointers = |strings: &[CString]| {
        strings
            .iter()
            .map(|string| string.as_ptr().cast_mut())
            .chain([std::ptr::null_mut()])
            .collect::<Vec<_>>()
    };
    let argv = pointers(&arguments);
    let envp = pointers(&environment);
    let directory = command
        .get_current_dir()
        .map(|directory| c_string(directory.as_os_str()))
        .transpose()?;

    let (stdout, stdout_writer) = std::io::pipe()?;
    let (stderr, stderr_writer) = std::io::pipe()?;
    let mut attributes = std::mem::MaybeUninit::uninit();
    let mut actions = std::mem::MaybeUninit::uninit();
    // SAFETY: each init call gets a pointer valid for writes; what one
    // initialized is destroyed if the other fails, and by `Spawn` after.
    let mut spawn = unsafe {
        check(libc::posix_spawnattr_init(attributes.as_mut_ptr()))?;
        if let Err(error) = check(libc::posix_spawn_file_actions_init(actions.as_mut_ptr())) {
            libc::posix_spawnattr_destroy(attributes.as_mut_ptr());
            return Err(error);
        }
        Spawn {
            attributes: attributes.assume_init(),
            actions: actions.assume_init(),
        }
    };
    let attributes = &raw mut spawn.attributes;
    let actions = &raw mut spawn.actions;
    let mut no_signals = std::mem::MaybeUninit::<libc::sigset_t>::uninit();
    let mut pipe_signal = std::mem::MaybeUninit::<libc::sigset_t>::uninit();
    // SAFETY: every pointer is valid for the call it is passed to, and the
    // strings outlive the spawn.
    unsafe {
        // Rust ignores SIGPIPE; the process gets the default, as with
        // `Command::spawn`, and no blocked signals.
        libc::sigemptyset(no_signals.as_mut_ptr());
        libc::sigemptyset(pipe_signal.as_mut_ptr());
        libc::sigaddset(pipe_signal.as_mut_ptr(), libc::SIGPIPE);
        check(libc::posix_spawnattr_setsigmask(
            attributes,
            no_signals.as_ptr(),
        ))?;
        check(libc::posix_spawnattr_setsigdefault(
            attributes,
            pipe_signal.as_ptr(),
        ))?;
        check(libc::posix_spawnattr_setpgroup(attributes, 0))?;
        #[expect(clippy::cast_possible_truncation, reason = "the flags fit a short")]
        check(libc::posix_spawnattr_setflags(
            attributes,
            (libc::POSIX_SPAWN_SETPGROUP
                | libc::POSIX_SPAWN_SETSIGMASK
                | libc::POSIX_SPAWN_SETSIGDEF
                | libc::POSIX_SPAWN_CLOEXEC_DEFAULT) as libc::c_short,
        ))?;
        match priority {
            Priority::Low => check(posix_spawnattr_set_qos_class_np(
                attributes,
                QOS_CLASS_UTILITY,
            ))?,
            Priority::Normal => {}
        }
        check(libc::posix_spawn_file_actions_addopen(
            actions,
            libc::STDIN_FILENO,
            c"/dev/null".as_ptr(),
            libc::O_RDONLY,
            0,
        ))?;
        check(libc::posix_spawn_file_actions_adddup2(
            actions,
            stdout_writer.as_raw_fd(),
            libc::STDOUT_FILENO,
        ))?;
        check(libc::posix_spawn_file_actions_adddup2(
            actions,
            stderr_writer.as_raw_fd(),
            libc::STDERR_FILENO,
        ))?;
        for &fd in inherit {
            check(posix_spawn_file_actions_addinherit_np(actions, fd))?;
        }
        if let Some(directory) = &directory {
            check(posix_spawn_file_actions_addchdir_np(
                actions,
                directory.as_ptr(),
            ))?;
        }
        let mut pid = 0;
        check(libc::posix_spawnp(
            &raw mut pid,
            program.as_ptr(),
            actions,
            attributes,
            argv.as_ptr(),
            envp.as_ptr(),
        ))?;
        Ok(Process {
            pid: Pid::from_raw(pid).expect("posix_spawn reports the started process"),
            stdout: File::from(OwnedFd::from(stdout)),
            stderr: File::from(OwnedFd::from(stderr)),
        })
    }
}

#[cfg(test)]
mod tests {
    use std::io::Read as _;

    use super::*;

    /// What the scheduler gives a process `sh` starts for the build
    /// process: on macOS its priority, on Linux its nice value and policy,
    /// then its I/O priority.
    fn report() -> Command {
        let mut command = Command::new("sh");
        if cfg!(target_os = "macos") {
            command.args(["-c", "sh -c 'ps -o pri= -p $$'"]);
        } else {
            command.args(["-c", "awk '{ print $19, $41 }' /proc/self/stat; ionice"]);
        }
        command
    }

    fn reported(priority: Option<Priority>) -> String {
        let Some(priority) = priority else {
            let output = report().output().unwrap();
            assert!(output.status.success());
            return String::from_utf8(output.stdout).unwrap();
        };
        let mut process = spawn(report(), &[], priority).unwrap();
        let mut text = String::new();
        process.stdout.read_to_string(&mut text).unwrap();
        let status =
            rustix::process::waitpid(Some(process.pid), rustix::process::WaitOptions::empty())
                .unwrap()
                .unwrap()
                .1;
        assert!(
            status.exited() && status.exit_status() == Some(0),
            "{status:?}"
        );
        text
    }

    #[test]
    fn low_priority_reaches_the_processes_a_build_starts_and_normal_is_the_daemons() {
        let low = reported(Some(Priority::Low));
        if cfg!(target_os = "macos") {
            // The utility QoS class's base priority.
            assert_eq!(low.trim(), "20");
        } else {
            let mut lines = low.lines();
            let mut scheduling = lines.next().unwrap().split(' ');
            let nice = scheduling.next().unwrap().parse::<i32>().unwrap();
            assert!(nice >= 10, "{low}");
            assert_eq!(scheduling.next(), Some("3"), "SCHED_BATCH: {low}");
            assert_eq!(lines.next(), Some("best-effort: prio 7"));
        }
        assert_eq!(reported(Some(Priority::Normal)), reported(None));
    }

    #[test]
    fn a_build_process_reads_nothing_and_runs_in_its_directory_with_its_environment() {
        let directory = std::fs::canonicalize(std::env::temp_dir()).unwrap();
        let mut command = Command::new("sh");
        command
            .args([
                "-c",
                // A group's leader has the group's id; standard input is
                // empty.
                "pwd; echo \"$BUILDD_SPAWN_TEST\"; echo error >&2; \
                 test \"$(ps -o pgid= -p $$ | tr -d ' ')\" = $$ && echo leader; cat",
            ])
            .current_dir(&directory)
            .env("BUILDD_SPAWN_TEST", "set");
        let mut process = spawn(command, &[], Priority::Low).unwrap();
        let mut output = String::new();
        process.stdout.read_to_string(&mut output).unwrap();
        let mut errors = String::new();
        process.stderr.read_to_string(&mut errors).unwrap();
        assert_eq!(output, format!("{}\nset\nleader\n", directory.display()));
        assert_eq!(errors, "error\n");
        rustix::process::waitpid(Some(process.pid), rustix::process::WaitOptions::empty()).unwrap();
        assert!(spawn(Command::new("buildd-no-such-program"), &[], Priority::Low).is_err());
    }
}
