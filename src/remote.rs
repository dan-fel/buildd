//! Builds on another machine: its buildd daemon, reached over SSH.
//!
//! The remote daemon knows nothing about this one. This daemon pushes the
//! snapshot's commit, on the worktree's `HEAD`, into the remote's mirror of
//! the project (`git push` over the same SSH connection, so only objects the
//! mirror lacks travel),
//! then asks it, through `buildd serve` on the other end, to build that
//! tree, and passes its messages on. Closing the client closes the SSH
//! session, which withdraws the build there as a closed socket does here.

use std::io::{BufRead as _, BufReader, Read as _, Write as _};
use std::path::Path;
use std::process::{Child, Command, Stdio};

use serde::de::DeserializeOwned;

use crate::config::Remote;
use crate::git;
use crate::protocol::{Activity, Failures, Message, Mirror, Request, RevisionRequest};
use crate::snapshot::{Revision, Source};

/// Which of the two SSH connections to a host a session goes over: builds
/// push trees and stream output, so status questions get a connection of
/// their own instead of queueing behind that traffic.
#[derive(Clone, Copy)]
enum Connection {
    Builds,
    Status,
}

/// SSH options: no prompts, and `connection` to each host kept open between
/// sessions, in a control socket under the daemon's home.
fn ssh_options(home: &Path, connection: Connection) -> Vec<String> {
    // Short names: %C is 40 characters, SSH appends 17 while creating the
    // socket, and a socket path has at most 103 (104 on macOS, with NUL).
    let control = match connection {
        Connection::Builds => "ssh-%C",
        Connection::Status => "ssh-s-%C",
    };
    [
        "-o",
        "BatchMode=yes",
        "-o",
        "ConnectTimeout=10",
        "-o",
        "ControlMaster=auto",
        "-o",
        "ControlPersist=600",
    ]
    .into_iter()
    .map(str::to_owned)
    .chain([
        "-o".to_owned(),
        format!("ControlPath={}", home.join(control).display()),
    ])
    .collect()
}

/// `buildd serve` on `remote`, with its standard input and output piped.
fn serve(home: &Path, remote: &Remote, connection: Connection) -> Result<Child, String> {
    Command::new("ssh")
        .args(ssh_options(home, connection))
        .arg(&remote.ssh)
        .arg(&remote.command)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|error| format!("could not run ssh: {error}"))
}

/// Sends `request` to `remote`'s daemon and reads its one-line answer.
fn ask<T: DeserializeOwned>(
    home: &Path,
    remote: &Remote,
    connection: Connection,
    request: &Request,
) -> Result<T, String> {
    let mut child = serve(home, remote, connection)?;
    send(&mut child, request)?;
    let mut line = String::new();
    BufReader::new(child.stdout.take().expect("piped"))
        .read_line(&mut line)
        .map_err(|error| format!("lost {}: {error}", remote.name))?;
    let answer = serde_json::from_str(&line).map_err(|_| failure(&mut child, remote, &line));
    let _ = child.kill();
    let _ = child.wait();
    answer
}

const CACHE_REQUEST_TIME: std::time::Duration = std::time::Duration::from_secs(10);

pub(crate) fn cache(
    home: &Path,
    remote: &Remote,
    operation: crate::cache::Operation,
    requester: &std::os::unix::net::UnixStream,
) -> Result<crate::cache::Response, crate::client::CacheError> {
    let deadline = std::time::Instant::now() + CACHE_REQUEST_TIME;
    let child =
        serve(home, remote, Connection::Status).map_err(crate::client::CacheError::Transport)?;
    cache_request(
        child,
        operation,
        requester,
        deadline,
        crate::client::MAX_CACHE_RESPONSE_BYTES,
    )
    .map_err(|mut error| {
        let (crate::client::CacheError::Transport(reason)
        | crate::client::CacheError::Rejected(reason)
        | crate::client::CacheError::InvalidResponse(reason)) = &mut error;
        *reason = format!("{} ({}): {reason}", remote.name, remote.ssh);
        error
    })
}

/// One SSH session owns one cache exchange. Every pipe stays nonblocking:
/// readiness allows progress, never an unbounded read or write. The same
/// absolute deadline and requester lifetime cover writing and reading alike.
/// Retiring transport never cancels, retries or replaces a cleanup approval.
fn cache_request(
    mut child: Child,
    operation: crate::cache::Operation,
    requester: &std::os::unix::net::UnixStream,
    deadline: std::time::Instant,
    limit: usize,
) -> Result<crate::cache::Response, crate::client::CacheError> {
    use crate::client::CacheError;
    use std::os::fd::AsRawFd as _;

    let mut diagnostics = Vec::new();
    let mut response = (|| {
        let mut input = child.stdin.take().expect("piped");
        let mut output = child.stdout.take().expect("piped");
        let mut errors = child.stderr.take().expect("piped");
        for fd in [input.as_raw_fd(), output.as_raw_fd(), errors.as_raw_fd()] {
            // SAFETY: each descriptor is owned by a live pipe above. Preserve
            // its other flags while making all subsequent I/O nonblocking.
            let flags = unsafe { libc::fcntl(fd, libc::F_GETFL) };
            if flags == -1
                || unsafe { libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK) } == -1
            {
                return Err(CacheError::Transport(
                    std::io::Error::last_os_error().to_string(),
                ));
            }
        }
        let request = Request::Cache {
            host: None,
            operation,
        };
        let mut request = serde_json::to_vec(&request).expect("requests serialize");
        request.push(b'\n');
        let mut written = 0;
        let mut answer = Vec::new();
        let mut stderr_open = true;
        loop {
            let remaining = deadline.saturating_duration_since(std::time::Instant::now());
            if remaining.is_zero() {
                return Err(CacheError::Transport(
                    "SSH cache session exceeded its deadline".into(),
                ));
            }
            let mut fds = [
                libc::pollfd {
                    fd: if written < request.len() {
                        input.as_raw_fd()
                    } else {
                        -1
                    },
                    events: libc::POLLOUT,
                    revents: 0,
                },
                libc::pollfd {
                    fd: output.as_raw_fd(),
                    events: libc::POLLIN,
                    revents: 0,
                },
                libc::pollfd {
                    fd: if stderr_open { errors.as_raw_fd() } else { -1 },
                    events: libc::POLLIN,
                    revents: 0,
                },
                libc::pollfd {
                    fd: requester.as_raw_fd(),
                    events: libc::POLLIN,
                    revents: 0,
                },
            ];
            let timeout =
                i32::try_from(remaining.as_millis().saturating_add(1)).unwrap_or(i32::MAX);
            // SAFETY: fds is writable for the supplied count and every
            // nonnegative descriptor stays owned for the complete exchange.
            let ready = unsafe { libc::poll(fds.as_mut_ptr(), fds.len() as libc::nfds_t, timeout) };
            if ready == -1 {
                let error = std::io::Error::last_os_error();
                if error.kind() == std::io::ErrorKind::Interrupted {
                    continue;
                }
                return Err(CacheError::Transport(error.to_string()));
            }
            if std::time::Instant::now() >= deadline {
                return Err(CacheError::Transport(
                    "SSH cache session exceeded its deadline".into(),
                ));
            }
            if fds[3].revents != 0 {
                let mut byte = 0_u8;
                // SAFETY: peek into one writable byte without blocking or
                // consuming input from the requester's live Unix socket.
                let received = unsafe {
                    libc::recv(
                        requester.as_raw_fd(),
                        (&mut byte as *mut u8).cast(),
                        1,
                        libc::MSG_PEEK | libc::MSG_DONTWAIT,
                    )
                };
                if received == 0
                    || fds[3].revents & (libc::POLLHUP | libc::POLLERR | libc::POLLNVAL) != 0
                {
                    return Err(CacheError::Transport("cache requester disconnected".into()));
                }
                if received > 0 {
                    return Err(CacheError::Transport(
                        "cache requester sent data after its request".into(),
                    ));
                }
                let error = std::io::Error::last_os_error();
                if !matches!(
                    error.kind(),
                    std::io::ErrorKind::WouldBlock | std::io::ErrorKind::Interrupted
                ) {
                    return Err(CacheError::Transport(error.to_string()));
                }
            }
            if fds[2].revents != 0 {
                let mut bytes = [0_u8; 8192];
                match errors.read(&mut bytes) {
                    Ok(0) => stderr_open = false,
                    Ok(count) => {
                        // Keep bounded diagnostic context, but continue
                        // draining so a full stderr pipe cannot stall SSH.
                        let keep = count.min(4097_usize.saturating_sub(diagnostics.len()));
                        diagnostics.extend_from_slice(&bytes[..keep]);
                    }
                    Err(error)
                        if matches!(
                            error.kind(),
                            std::io::ErrorKind::WouldBlock | std::io::ErrorKind::Interrupted
                        ) => {}
                    Err(error) => return Err(CacheError::Transport(error.to_string())),
                }
            }
            if fds[0].revents != 0 {
                match input.write(&request[written..]) {
                    Ok(0) => {
                        return Err(CacheError::Transport(
                            "could not send the cache request".into(),
                        ));
                    }
                    Ok(count) => written += count,
                    Err(error)
                        if matches!(
                            error.kind(),
                            std::io::ErrorKind::WouldBlock | std::io::ErrorKind::Interrupted
                        ) => {}
                    Err(error) => {
                        return Err(CacheError::Transport(format!(
                            "could not send the request: {error}"
                        )));
                    }
                }
            }
            if fds[1].revents != 0 {
                let mut bytes = [0_u8; 8192];
                let room = (limit + 1 - answer.len()).min(bytes.len());
                match output.read(&mut bytes[..room]) {
                    Ok(0) => {
                        return Err(CacheError::Transport(
                            "the daemon closed the connection before its answer ended".into(),
                        ));
                    }
                    Ok(count) => {
                        let end = bytes[..count].iter().position(|byte| *byte == b'\n');
                        answer.extend_from_slice(&bytes[..end.map_or(count, |at| at + 1)]);
                        if answer.len() > limit {
                            return Err(CacheError::InvalidResponse(format!(
                                "answer exceeds {limit} bytes"
                            )));
                        }
                        if end.is_some() {
                            return crate::client::decode_cache_response(&answer);
                        }
                    }
                    Err(error)
                        if matches!(
                            error.kind(),
                            std::io::ErrorKind::WouldBlock | std::io::ErrorKind::Interrupted
                        ) => {}
                    Err(error) => return Err(CacheError::Transport(error.to_string())),
                }
            }
        }
    })();
    let _ = child.kill();
    let _ = child.wait();
    if let Err(CacheError::Transport(reason)) = &mut response {
        let details = String::from_utf8_lossy(&diagnostics[..diagnostics.len().min(4096)]);
        if !details.trim().is_empty() {
            reason.push_str(": ");
            reason.push_str(details.trim());
        }
        if diagnostics.len() > 4096 {
            reason.push_str(" [SSH diagnostics exceeded 4 KiB]");
        }
    }
    response
}

fn send(child: &mut Child, request: &Request) -> Result<(), String> {
    let mut text = serde_json::to_string(request).expect("requests serialize");
    text.push('\n');
    child
        .stdin
        .as_mut()
        .expect("piped")
        .write_all(text.as_bytes())
        .map_err(|error| format!("could not send the request: {error}"))
}

/// Why `remote` answered `line` instead of a message: what SSH or the
/// remote command said.
fn failure(child: &mut Child, remote: &Remote, line: &str) -> String {
    let _ = child.kill();
    let mut errors = String::new();
    if let Some(stderr) = child.stderr.take() {
        let _ = BufReader::new(stderr).read_line(&mut errors);
    }
    let said = if line.trim().is_empty() {
        errors.trim()
    } else {
        line.trim()
    };
    format!("{} ({}) did not answer: {said}", remote.name, remote.ssh)
}

/// What `remote`'s daemon is doing.
pub(crate) fn activity(home: &Path, remote: &Remote) -> Result<Activity, String> {
    ask(home, remote, Connection::Status, &Request::Activity)
}

/// The builds that failed on `remote` in the last `hours`.
pub(crate) fn failures(home: &Path, remote: &Remote, hours: u64) -> Result<Failures, String> {
    ask::<Result<Failures, String>>(
        home,
        remote,
        Connection::Status,
        &Request::Failures { hours, os: None },
    )?
}

/// Pushes tree `revision` of `source`'s repository into `remote`'s mirror
/// of it, and returns the mirror's project name.
fn push(
    home: &Path,
    remote: &Remote,
    source: &Source,
    revision: &Revision,
) -> Result<String, String> {
    let project = crate::slot::project_name(&source.repository);
    let mirror: Mirror = ask(
        home,
        remote,
        Connection::Builds,
        &Request::Mirror {
            project: project.clone(),
        },
    )?;
    let ssh = std::iter::once("ssh".to_owned())
        .chain(ssh_options(home, Connection::Builds))
        .collect::<Vec<_>>()
        .join(" ");
    let git = || {
        let mut command = git::command(&source.repository);
        command
            .env("GIT_SSH_COMMAND", &ssh)
            .arg("--git-dir")
            .arg(&source.repository);
        command
    };
    let destination = format!("{}:{}", remote.ssh, mirror.path.display());
    push_snapshot(git, source, revision, &destination)?;
    Ok(project)
}

/// Pushes tree `revision` of `source` to the repository at `destination`,
/// as a commit under a reference of its own; `git` makes a git command for
/// `source`'s repository.
///
/// The commit sits on the worktree's `HEAD`, so it shares history with what
/// earlier pushes sent and git sends only the objects that changed since. A
/// commit without a parent shares no history, and git sent its whole tree.
fn push_snapshot(
    git: impl Fn() -> Command,
    source: &Source,
    revision: &Revision,
    destination: &str,
) -> Result<(), String> {
    let parent = git::head_commit(&source.worktree)?;
    let commit = git::snapshot_commit(git(), revision, parent.as_deref())?;
    let reference = format!("refs/buildd/{commit}");
    let mut push = git();
    push.args(["push", "--quiet", "--no-verify", destination])
        .arg(format!("{commit}:{reference}"));
    let Err(failed) = git::run(push) else {
        return Ok(());
    };
    // Two builds of one tree on one HEAD push its commit at once: the mirror
    // refuses the second creation of the reference, though both name the
    // same commit. That push failed only in form.
    let mut listed = git();
    listed.args(["ls-remote", destination, &reference]);
    let there = git::run(listed)?;
    if there.split_whitespace().next() == Some(commit.as_str()) {
        Ok(())
    } else {
        Err(failed)
    }
}

/// Builds tree `revision` of `source` on `remote` as `request` asks,
/// passing each message to `pass` until the last one or until `pass` says
/// the client is gone. `started` gets the SSH session once the build is
/// asked for: killing it withdraws the build.
pub(crate) fn build(
    home: &Path,
    remote: &Remote,
    source: &Source,
    revision: &Revision,
    request: RevisionRequest,
    started: impl FnOnce(&std::sync::Arc<std::sync::Mutex<Child>>),
    mut pass: impl FnMut(&Message) -> bool,
) -> Result<(), String> {
    let project = push(home, remote, source, revision)?;
    let mut child = serve(home, remote, Connection::Builds)?;
    send(
        &mut child,
        &Request::BuildRevision(RevisionRequest { project, ..request }),
    )?;
    let stdout = child.stdout.take().expect("piped");
    let child = std::sync::Arc::new(std::sync::Mutex::new(child));
    started(&child);
    let mut last = None;
    for line in BufReader::new(stdout).lines() {
        let Ok(line) = line else { break };
        let message = serde_json::from_str::<Message>(&line).map_err(|_| {
            failure(
                &mut child
                    .lock()
                    .expect("no thread panics holding the ssh child"),
                remote,
                &line,
            )
        })?;
        let message = match message {
            Message::Started { revision, slot } => Message::Started {
                revision,
                slot: format!("{}:{slot}", remote.name),
            },
            other => other,
        };
        let is_final = message.is_final();
        if !pass(&message) || is_final {
            last = Some(is_final);
            break;
        }
    }
    let mut child = child
        .lock()
        .expect("no thread panics holding the ssh child");
    let _ = child.kill();
    let _ = child.wait();
    match last {
        Some(_) => Ok(()),
        None => Err(format!("lost {} before the build ended", remote.name)),
    }
}

#[cfg(test)]
mod cache_end_to_end {
    use super::*;
    use crate::cache::{Operation, Refusal, Response};
    use crate::client::CacheError;

    // A fixture-owned subprocess supplies the same stdin/stdout service
    // protocol as the SSH session. No SSH configuration or live host changes.
    fn fixture(reply: &str) -> Child {
        let script = format!(
            "import json, sys\n\
             request = json.loads(sys.stdin.readline())\n\
             assert request == {{'type':'cache', 'host':None, 'operation':{{'operation':'capabilities'}}}}\n\
             {reply}\n"
        );
        Command::new("python3")
            .args(["-c", &script])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap()
    }

    fn ask(reply: &str) -> Result<Response, CacheError> {
        let child = fixture(reply);
        let pid = rustix::process::Pid::from_raw(child.id().try_into().unwrap()).unwrap();
        let (_requester, socket) = std::os::unix::net::UnixStream::pair().unwrap();
        let response = cache_request(
            child,
            Operation::Capabilities,
            &socket,
            std::time::Instant::now() + std::time::Duration::from_secs(2),
            4096,
        );
        assert!(
            matches!(
                rustix::process::waitpid(Some(pid), rustix::process::WaitOptions::NOHANG),
                Err(rustix::io::Errno::CHILD)
            ),
            "the completed fixture session has already been reaped"
        );
        response
    }

    #[test]
    fn cache_session_decodes_bounded_replies_and_reaps_all_outcomes() {
        assert_eq!(
            ask("print('{\"Ok\":{\"type\":\"refused\",\"reason\":\"busy\"}}', flush=True)"),
            Ok(Response::Refused {
                reason: Refusal::Busy
            })
        );
        assert_eq!(
            ask(
                "print('{\"type\":\"rejected\",\"reason\":\"unsupported cache protocol\"}', flush=True)"
            ),
            Err(CacheError::Rejected("unsupported cache protocol".into()))
        );
        assert!(matches!(ask("pass"), Err(CacheError::Transport(_))));
        // The fixture descendant holds stderr until the session's stdin closes.
        // Waiting for stderr EOF here would prevent that very close forever.
        assert!(matches!(
            ask(
                "import os\npid = os.fork()\nif pid == 0:\n os.close(1)\n sys.stdin.read()\n os._exit(0)"
            ),
            Err(CacheError::Transport(_))
        ));
        assert!(matches!(
            ask("sys.stderr.write('fixture transport denied'); sys.stderr.flush()"),
            Err(CacheError::Transport(reason)) if reason.ends_with("fixture transport denied")
        ));
        assert!(matches!(
            ask("print('{}', flush=True)"),
            Err(CacheError::InvalidResponse(_))
        ));
        assert_eq!(
            ask("sys.stdout.write('x' * 8192); sys.stdout.flush()"),
            Err(CacheError::InvalidResponse(
                "answer exceeds 4096 bytes".into()
            ))
        );
    }

    // The fixture signals readiness and request progress over a separate
    // socket. Neither startup timing nor sleeps decide the stalled boundary.
    fn event_fixture(body: &str) -> (Child, std::os::unix::net::UnixStream) {
        let directory = crate::snapshot::tests::TempDir::new();
        let path = directory.0.join("events");
        let listener = std::os::unix::net::UnixListener::bind(&path).unwrap();
        let script = format!(
            "import json, signal, socket, sys\n\
             event = socket.socket(socket.AF_UNIX)\n\
             event.connect(sys.argv[1])\n\
             event.sendall(b'R')\n{body}\n"
        );
        let child = Command::new("python3")
            .args(["-c", &script])
            .arg(&path)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        let (mut event, _) = listener.accept().unwrap();
        event
            .set_read_timeout(Some(std::time::Duration::from_secs(2)))
            .unwrap();
        let mut ready = [0];
        event.read_exact(&mut ready).unwrap();
        assert_eq!(&ready, b"R");
        (child, event)
    }

    fn reaped(pid: rustix::process::Pid) {
        assert!(
            matches!(
                rustix::process::waitpid(Some(pid), rustix::process::WaitOptions::NOHANG),
                Err(rustix::io::Errno::CHILD)
            ),
            "the cache session has been reaped"
        );
    }

    #[test]
    fn cache_deadline_covers_partial_replies_and_stalled_sessions() {
        for body in [
            "json.loads(sys.stdin.readline()); event.sendall(b'A'); signal.pause()",
            r#"json.loads(sys.stdin.readline()); sys.stdout.write('{"Ok":'); sys.stdout.flush(); event.sendall(b'A'); signal.pause()"#,
        ] {
            let (child, mut event) = event_fixture(body);
            let pid = rustix::process::Pid::from_raw(child.id().try_into().unwrap()).unwrap();
            let (_requester, socket) = std::os::unix::net::UnixStream::pair().unwrap();
            let response = cache_request(
                child,
                Operation::Capabilities,
                &socket,
                std::time::Instant::now() + std::time::Duration::from_millis(50),
                4096,
            );
            let mut accepted = [0];
            event.read_exact(&mut accepted).unwrap();
            assert_eq!(&accepted, b"A");
            assert!(
                matches!(response, Err(CacheError::Transport(reason)) if reason.contains("deadline"))
            );
            reaped(pid);
        }
    }

    #[test]
    fn cache_deadline_covers_a_partially_written_request() {
        let (child, mut event) =
            event_fixture("sys.stdin.buffer.read(1); event.sendall(b'A'); signal.pause()");
        let pid = rustix::process::Pid::from_raw(child.id().try_into().unwrap()).unwrap();
        let (_requester, socket) = std::os::unix::net::UnixStream::pair().unwrap();
        let response = cache_request(
            child,
            Operation::Preview {
                owner: crate::cache::Owner {
                    host: "fixture".into(),
                    incarnation: "fixture".into(),
                },
                inventory: "x".repeat(512 * 1024),
                items: Vec::new(),
            },
            &socket,
            std::time::Instant::now() + std::time::Duration::from_millis(100),
            4096,
        );
        let mut accepted = [0];
        event.read_exact(&mut accepted).unwrap();
        assert_eq!(
            &accepted, b"A",
            "the session received bytes before its input pipe stalled"
        );
        assert!(
            matches!(response, Err(CacheError::Transport(reason)) if reason.contains("deadline"))
        );
        reaped(pid);
    }

    #[test]
    fn requester_disconnect_retires_an_in_flight_session() {
        let (child, mut event) =
            event_fixture("json.loads(sys.stdin.readline()); event.sendall(b'A'); signal.pause()");
        let pid = rustix::process::Pid::from_raw(child.id().try_into().unwrap()).unwrap();
        let (requester, socket) = std::os::unix::net::UnixStream::pair().unwrap();
        let (result, received) = crossbeam_channel::bounded(1);
        let worker = std::thread::spawn(move || {
            result
                .send(cache_request(
                    child,
                    Operation::Capabilities,
                    &socket,
                    std::time::Instant::now() + std::time::Duration::from_secs(10),
                    4096,
                ))
                .unwrap();
        });
        let mut accepted = [0];
        event.read_exact(&mut accepted).unwrap();
        assert_eq!(&accepted, b"A");
        drop(requester);
        let response = received
            .recv_timeout(std::time::Duration::from_secs(2))
            .unwrap();
        worker.join().unwrap();
        assert!(
            matches!(response, Err(CacheError::Transport(reason)) if reason.contains("requester disconnected"))
        );
        reaped(pid);
    }

    #[test]
    fn stderr_pressure_is_drained_with_bounded_diagnostics() {
        assert!(matches!(ask(
            "sys.stderr.write('x' * (1024 * 1024)); sys.stderr.flush()"
        ), Err(CacheError::Transport(reason)) if reason.ends_with("[SSH diagnostics exceeded 4 KiB]") && reason.len() < 4300));
    }

    #[test]
    fn failed_cache_write_still_reaps_its_session() {
        let mut child = Command::new("python3")
            .args([
                "-c",
                "import os, sys; os.close(0); print('closed', flush=True)",
            ])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        let pid = rustix::process::Pid::from_raw(child.id().try_into().unwrap()).unwrap();
        let mut event = [0; 7];
        child
            .stdout
            .as_mut()
            .unwrap()
            .read_exact(&mut event)
            .unwrap();
        assert_eq!(&event, b"closed\n");
        let (_requester, socket) = std::os::unix::net::UnixStream::pair().unwrap();
        assert!(matches!(
            cache_request(child, Operation::Capabilities, &socket,
                std::time::Instant::now() + std::time::Duration::from_secs(2), 4096),
            Err(CacheError::Transport(reason)) if reason.starts_with("could not send the request:")
        ));
        assert!(matches!(
            rustix::process::waitpid(Some(pid), rustix::process::WaitOptions::NOHANG),
            Err(rustix::io::Errno::CHILD)
        ));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::snapshot::tests::{TempDir, git, repository};

    /// The objects in the repository at `directory`, loose and packed.
    fn objects(directory: &Path) -> u64 {
        git(directory, &["count-objects", "-v"])
            .lines()
            .filter_map(|line| {
                let (name, value) = line.split_once(": ")?;
                matches!(name, "count" | "in-pack").then(|| value.parse::<u64>().unwrap())
            })
            .sum()
    }

    #[test]
    fn a_push_sends_only_what_changed_since_the_last() {
        let repository = repository();
        for directory in 0..10 {
            std::fs::create_dir(repository.0.join(format!("d{directory}"))).unwrap();
            for file in 0..30 {
                std::fs::write(
                    repository.0.join(format!("d{directory}/{file}.txt")),
                    format!("{directory} {file}\n"),
                )
                .unwrap();
            }
        }
        git(&repository.0, &["add", "--all"]);
        git(&repository.0, &["commit", "-q", "-m", "files"]);
        let mirror = TempDir::new();
        git(&mirror.0, &["init", "-q", "--bare"]);
        let scratch = TempDir::new();
        let source = crate::snapshot::resolve(&repository.0).unwrap();
        let source_git = || {
            let mut command = git::command(&source.repository);
            command.arg("--git-dir").arg(&source.repository);
            command
        };
        let destination = mirror.0.to_str().unwrap();

        let first = source.snapshot(&scratch.0).unwrap();
        push_snapshot(source_git, &source, &first, destination).unwrap();
        let after_first = objects(&mirror.0);
        assert!(after_first > 300, "the first push sends every file");

        std::fs::write(repository.0.join("d3/7.txt"), "changed\n").unwrap();
        let second = source.snapshot(&scratch.0).unwrap();
        push_snapshot(source_git, &source, &second, destination).unwrap();
        // The changed file, its directory, the root tree and the commit.
        assert_eq!(objects(&mirror.0) - after_first, 4);
        git(
            &mirror.0,
            &["cat-file", "-e", &format!("{second}^{{tree}}")],
        );

        // Pushing a tree again is no error.
        push_snapshot(source_git, &source, &second, destination).unwrap();
    }
}
