//! Builds on another machine: its buildd daemon, reached over SSH.
//!
//! The remote daemon knows nothing about this one. This daemon pushes the
//! snapshot's commit, on the worktree's `HEAD`, into the remote's mirror of
//! the project (`git push` over the same SSH connection, so only objects the
//! mirror lacks travel),
//! then asks it, through `buildd serve` on the other end, to build that
//! tree, and passes its messages on. Closing the client closes the SSH
//! session, which withdraws the build there as a closed socket does here.

use std::io::{BufRead as _, BufReader, Write as _};
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
