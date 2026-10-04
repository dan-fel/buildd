//! The `buildd` command: the daemon and its command-line client.

use std::io::ErrorKind;
use std::os::unix::net::UnixStream;
use std::os::unix::process::CommandExt as _;
use std::path::Path;
use std::process::{ExitCode, Stdio};
use std::time::{Duration, Instant};

use buildd::cargo::{Command, Operation};
use buildd::protocol::{BuildRequest, Message, Outcome, Status};
use buildd::{client, config};
use serde::Deserialize;

const USAGE: &str = "\
usage:
  buildd check|clippy|build|test [CARGO ARGS...]
      Build the current content of this worktree, committed or not, in a
      build slot, and print Cargo's diagnostics and test output.
  buildd status
      Show the build slots and the queue.
  buildd daemon
      Run the daemon in the foreground. Clients start it when none runs.";

/// How long a client waits for a daemon it started to listen.
const DAEMON_STARTUP: Duration = Duration::from_secs(10);
/// The exit code of a failure that is not Cargo's own, as Cargo uses.
const FAILURE: u8 = 101;

fn main() -> ExitCode {
    let mut args = std::env::args().skip(1);
    let Some(name) = args.next() else {
        eprintln!("{USAGE}");
        return ExitCode::from(2);
    };
    let command = match name.as_str() {
        "daemon" | "status" => None,
        "help" | "--help" | "-h" => {
            println!("{USAGE}");
            return ExitCode::SUCCESS;
        }
        other => match Command::parse(other) {
            Some(command) => Some(command),
            None => {
                eprintln!("buildd: unknown command `{other}`\n{USAGE}");
                return ExitCode::from(2);
            }
        },
    };
    let result = config::home().and_then(|home| match command {
        Some(command) => build(
            &home,
            Operation {
                command,
                args: args.collect(),
            },
        ),
        None if name == "daemon" => daemon(&home),
        None => status(&home),
    });
    result.unwrap_or_else(|message| {
        eprintln!("buildd: {message}");
        ExitCode::from(FAILURE)
    })
}

fn daemon(home: &Path) -> Result<ExitCode, String> {
    let config = config::Config::load(home)?;
    match buildd::daemon::run(home, config)? {}
}

fn build(home: &Path, operation: Operation) -> Result<ExitCode, String> {
    let directory = std::env::current_dir()
        .map_err(|error| format!("could not read the current directory: {error}"))?;
    let request = BuildRequest {
        directory,
        operation,
    };
    let last = client::build(connect(home)?, request, render)?;
    Ok(match last {
        Message::Finished {
            outcome: Outcome::Exited { code },
            ..
        } => ExitCode::from(u8::try_from(code).expect("Unix exit codes fit a byte")),
        Message::Finished {
            outcome: Outcome::Signaled { signal },
            ..
        } => ExitCode::from(u8::try_from(128 + signal).expect("signal numbers are small")),
        Message::Finished {
            outcome: Outcome::Failed { .. },
            ..
        }
        | Message::Rejected { .. } => ExitCode::from(FAILURE),
        other => panic!("a build's last message is final: {other:?}"),
    })
}

/// A line of Cargo's JSON output.
#[derive(Deserialize)]
struct CargoMessage {
    reason: String,
    message: Option<Diagnostic>,
}

#[derive(Deserialize)]
struct Diagnostic {
    rendered: Option<String>,
}

/// Prints a message the way Cargo would have: diagnostics and Cargo's own
/// lines to standard error, test output to standard output.
fn render(message: &Message) {
    match message {
        Message::Queued { revision } => eprintln!("buildd: tree {} queued", revision.short()),
        Message::Started { revision, slot } => {
            eprintln!("buildd: building tree {} in slot {slot}", revision.short());
        }
        Message::Stdout { line } => match serde_json::from_str::<CargoMessage>(line) {
            Ok(cargo) => {
                if cargo.reason == "compiler-message"
                    && let Some(rendered) = cargo.message.and_then(|diagnostic| diagnostic.rendered)
                {
                    eprint!("{rendered}");
                }
            }
            Err(_) => println!("{line}"),
        },
        Message::Stderr { line } => eprintln!("{line}"),
        Message::Finished {
            revision,
            outcome,
            queued_ms,
            build_ms,
        } => {
            let ended = match outcome {
                Outcome::Exited { code: 0 } => "succeeded".to_owned(),
                Outcome::Exited { code } => format!("failed with exit code {code}"),
                Outcome::Signaled { signal } => format!("was killed by signal {signal}"),
                Outcome::Failed { reason } => format!("could not run: {reason}"),
            };
            eprintln!(
                "buildd: tree {} {ended} after {} in the build and {} in the queue",
                revision.short(),
                seconds(*build_ms),
                seconds(*queued_ms),
            );
        }
        Message::Rejected { reason } => eprintln!("buildd: {reason}"),
    }
}

fn status(home: &Path) -> Result<ExitCode, String> {
    let Status {
        jobs,
        idle_jobs,
        slots,
        queue,
    } = client::status(connect(home)?)?;
    println!("jobs: {idle_jobs} of {jobs} idle");
    for slot in slots {
        let worktree = slot.worktree.map_or_else(String::new, |worktree| {
            format!(" for {}", worktree.display())
        });
        let size = slot.size.map_or_else(String::new, |size| {
            #[expect(clippy::cast_precision_loss, reason = "a size in GiB for people")]
            let gib = size as f64 / f64::from(1 << 30);
            format!(" [{gib:.1} GiB]")
        });
        match slot.build {
            Some(build) => println!(
                "slot {}{size}: {} {} ({} waiting, {}{}){worktree}",
                slot.name,
                build.revision.short(),
                build.operation,
                build.waiters,
                seconds(build.elapsed_ms),
                if build.cancelled { ", cancelled" } else { "" },
            ),
            None if slot.maintaining => println!("slot {}{size}: pruning{worktree}", slot.name),
            None => println!("slot {}{size}: idle{worktree}", slot.name),
        }
    }
    for (position, build) in queue.iter().enumerate() {
        let worktrees = build
            .worktrees
            .iter()
            .map(|worktree| worktree.display().to_string())
            .collect::<Vec<_>>()
            .join(", ");
        println!(
            "queued {}: {} {} (waited {}) for {worktrees}",
            position + 1,
            build.revision.short(),
            build.operation,
            seconds(build.waited_ms),
        );
    }
    Ok(ExitCode::SUCCESS)
}

fn seconds(millis: u64) -> String {
    format!("{}.{} s", millis / 1000, millis % 1000 / 100)
}

/// Connects to the daemon, starting it when none runs.
fn connect(home: &Path) -> Result<UnixStream, String> {
    let absent = |error: &std::io::Error| {
        matches!(
            error.kind(),
            ErrorKind::NotFound | ErrorKind::ConnectionRefused
        )
    };
    match client::connect(home) {
        Ok(stream) => return Ok(stream),
        Err(error) if absent(&error) => {}
        Err(error) => return Err(format!("could not reach the daemon: {error}")),
    }
    start_daemon(home)?;
    let deadline = Instant::now() + DAEMON_STARTUP;
    loop {
        std::thread::sleep(Duration::from_millis(20));
        match client::connect(home) {
            Ok(stream) => return Ok(stream),
            Err(error) if absent(&error) && Instant::now() < deadline => {}
            Err(error) => {
                return Err(format!(
                    "the daemon did not start ({error}); see {}",
                    home.join("daemon.log").display()
                ));
            }
        }
    }
}

/// Starts `buildd daemon` for `home` in the background, detached from this
/// terminal's signals, logging to `daemon.log`.
fn start_daemon(home: &Path) -> Result<(), String> {
    let log = home.join("daemon.log");
    let open_log = || {
        std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&log)
    };
    std::fs::create_dir_all(home)
        .and_then(|()| Ok((open_log()?, open_log()?)))
        .and_then(|(stdout, stderr)| {
            std::process::Command::new(std::env::current_exe()?)
                .arg("daemon")
                .env("BUILDD_HOME", home)
                .stdin(Stdio::null())
                .stdout(stdout)
                .stderr(stderr)
                .process_group(0)
                .spawn()
        })
        .map(drop)
        .map_err(|error| format!("could not start the daemon: {error}"))
}
