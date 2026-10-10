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

pub(crate) fn cache(
    home: &Path,
    remote: &Remote,
    operation: crate::cache::Operation,
) -> Result<crate::cache::Response, String> {
    cache_request(
        serve(home, remote, Connection::Status)?,
        operation,
        crate::client::MAX_CACHE_RESPONSE_BYTES,
    )
    .map_err(|error| format!("{} ({}) {error}", remote.name, remote.ssh))
}

/// This SSH session owns only one cache request. Reap it on every path,
/// including failed writes, oversized replies and daemon rejections. Closing
/// transport never requests a new cleanup or changes the service receipt.
fn cache_request(
    mut child: Child,
    operation: crate::cache::Operation,
    limit: usize,
) -> Result<crate::cache::Response, crate::client::CacheError> {
    let mut response = (|| {
        send(
            &mut child,
            &Request::Cache {
                host: None,
                operation,
            },
        )
        .map_err(crate::client::CacheError::Transport)?;
        crate::client::read_cache_response(
            BufReader::new(child.stdout.take().expect("piped")),
            limit,
        )
    })();
    let _ = child.kill();
    let _ = child.wait();
    if let Err(crate::client::CacheError::Transport(reason)) = &mut response {
        let mut stderr = Vec::new();
        let pipe = child.stderr.take().expect("piped");
        // An SSH control process may outlive this session and keep its stderr
        // pipe open. Diagnostics must not add another wait after retirement.
        if let Ok(flags) = rustix::fs::fcntl_getfl(&pipe)
            && rustix::fs::fcntl_setfl(&pipe, flags | rustix::fs::OFlags::NONBLOCK).is_ok()
        {
            let _ = pipe.take(4097).read_to_end(&mut stderr);
            let details = String::from_utf8_lossy(&stderr[..stderr.len().min(4096)]);
            if !details.trim().is_empty() {
                reason.push_str(": ");
                reason.push_str(details.trim());
            }
            if stderr.len() > 4096 {
                reason.push_str(" [SSH diagnostics exceeded 4 KiB]");
            }
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
        let response = cache_request(child, Operation::Capabilities, 4096);
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
            ask("print('{\"type\":\"refused\",\"reason\":\"busy\"}', flush=True)"),
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
        assert!(matches!(
            cache_request(child, Operation::Capabilities, 4096),
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
