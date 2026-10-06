//! Builds on another machine: its buildd daemon, reached over SSH.
//!
//! The remote daemon knows nothing about this one. This daemon pushes the
//! snapshot's commit into the remote's mirror of the project (`git push`
//! over the same SSH connection, so only objects the mirror lacks travel),
//! then asks it, through `buildd serve` on the other end, to build that
//! tree, and passes its messages on. Closing the client closes the SSH
//! session, which withdraws the build there as a closed socket does here.

use std::io::{BufRead as _, BufReader, Write as _};
use std::path::Path;
use std::process::{Child, Command, Stdio};

use serde::de::DeserializeOwned;

use crate::config::Remote;
use crate::git;
use crate::protocol::{Activity, Message, Mirror, Request, RevisionRequest};
use crate::snapshot::{Revision, Source};

/// SSH options: no prompts, and one connection per host kept open between
/// builds, in a control socket under the daemon's home.
fn ssh_options(home: &Path) -> Vec<String> {
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
        format!("ControlPath={}", home.join("ssh-%C").display()),
    ])
    .collect()
}

/// `buildd serve` on `remote`, with its standard input and output piped.
fn serve(home: &Path, remote: &Remote) -> Result<Child, String> {
    Command::new("ssh")
        .args(ssh_options(home))
        .arg(&remote.ssh)
        .arg(&remote.command)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|error| format!("could not run ssh: {error}"))
}

/// Sends `request` to `remote`'s daemon and reads its one-line answer.
fn ask<T: DeserializeOwned>(home: &Path, remote: &Remote, request: &Request) -> Result<T, String> {
    let mut child = serve(home, remote)?;
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
    ask(home, remote, &Request::Activity)
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
        &Request::Mirror {
            project: project.clone(),
        },
    )?;
    let git_dir = |mut command: Command| {
        command.arg("--git-dir").arg(&source.repository);
        command
    };
    let commit = git::snapshot_commit(git_dir(git::command(&source.repository)), revision)?;
    let ssh = std::iter::once("ssh".to_owned())
        .chain(ssh_options(home))
        .collect::<Vec<_>>()
        .join(" ");
    let mut push = git_dir(git::command(&source.repository));
    push.env("GIT_SSH_COMMAND", ssh)
        .args(["push", "--quiet", "--no-verify"])
        .arg(format!("{}:{}", remote.ssh, mirror.path.display()))
        .arg(format!("{commit}:refs/buildd/{revision}"));
    git::run(push)?;
    Ok(project)
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
    let mut child = serve(home, remote)?;
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
