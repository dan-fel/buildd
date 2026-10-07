//! The `buildd` command: the daemon and its command-line client.

use std::io::ErrorKind;
use std::os::unix::net::UnixStream;
use std::os::unix::process::CommandExt as _;
use std::path::{Path, PathBuf};
use std::process::{ExitCode, Stdio};
use std::time::{Duration, Instant};

use buildd::cargo::{Command, Operation};
use buildd::protocol::{BuildRequest, Failures, Message, Outcome, Status};
use buildd::{client, config};
use serde::Deserialize;

mod top;

const USAGE: &str = "\
usage:
  buildd check|clippy|build|test|nextest [OPTIONS] [CARGO ARGS...]
      Build the current content of this worktree, committed or not, in a
      build slot, and print Cargo's diagnostics and test output. nextest
      runs `cargo nextest run` (installed separately): the tests `test`
      compiles, each in its own process, test_jobs at a time.
      Options, before Cargo's arguments:
        --json             print every message from the daemon as a line of
                           JSON on standard output instead
        --copy-to DIR      after a successful build, copy the executables
                           Cargo produced, with their debug information,
                           into DIR
        --rustflags FLAGS  pass FLAGS to every rustc, as RUSTFLAGS would
        --rerun-all        nextest: also run test binaries that are unchanged
                           since they passed (they are skipped otherwise)
        --os OS            build on OS (linux, macos): on the remote host
                           config.toml names for it when it is not this one
  buildd status
      Show the build slots and the queue.
  buildd failures [--hours N] [--os OS]
      Sum up the builds that failed in the last N hours (24): the latest,
      the tests and errors that fail most often, trees whose outcome
      changed between runs (flaky tests), the slowest tests, and where each
      build's log is. With --os, on the remote host for OS.
  buildd drain | undrain
      Stop taking new builds, letting running and queued ones end (to keep
      the machine quiet, as for a benchmark), or take them again.
  buildd serve
      Pass one request from standard input to this machine's daemon and its
      answers to standard output: what another machine's daemon runs over
      SSH. Closing standard input withdraws the request.
  buildd top
      Watch the slots, the queue, what sharing saved and recent events.
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
        "daemon" | "status" | "top" | "drain" | "undrain" | "serve" | "failures" => None,
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
        Some(command) => build(&home, command, args.collect()),
        None if name == "daemon" => daemon(&home),
        None if name == "top" => {
            top::run(|| client::activity(connect(&home)?)).map(|()| ExitCode::SUCCESS)
        }
        None if name == "drain" || name == "undrain" => {
            client::drain(connect(&home)?, name == "drain")?;
            status(&home)
        }
        None if name == "serve" => serve(&home),
        None if name == "failures" => failures(&home, args.collect()),
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

/// The names of buildd's own options of a build.
const OPTIONS: [&str; 5] = ["--json", "--copy-to", "--rustflags", "--rerun-all", "--os"];

/// buildd's own options of a build, which come before Cargo's arguments.
#[derive(Debug, Default, PartialEq)]
struct Options {
    json: bool,
    copy_to: Option<PathBuf>,
    rustflags: Vec<String>,
    rerun_all: bool,
    os: Option<String>,
}

impl Options {
    /// Takes the options off the front of `args`, leaving Cargo's
    /// arguments; a relative directory is taken relative to `directory`.
    fn take(args: &mut Vec<String>, directory: &Path) -> Result<Self, String> {
        let mut options = Self::default();
        let mut taken = 0;
        while let Some(argument) = args.get(taken) {
            let (name, inline) = match argument.split_once('=') {
                Some((name, value)) => (name, Some(value.to_owned())),
                None => (argument.as_str(), None),
            };
            let value = |taken: &mut usize| {
                let value = match inline.clone() {
                    Some(value) => value,
                    None => {
                        *taken += 1;
                        args.get(*taken)
                            .cloned()
                            .ok_or_else(|| format!("{name} needs a value"))?
                    }
                };
                Ok::<_, String>(value)
            };
            match name {
                "--json" if inline.is_none() => options.json = true,
                "--rerun-all" if inline.is_none() => options.rerun_all = true,
                "--copy-to" => options.copy_to = Some(directory.join(value(&mut taken)?)),
                "--os" => options.os = Some(value(&mut taken)?),
                "--rustflags" => {
                    options.rustflags = value(&mut taken)?
                        .split_whitespace()
                        .map(str::to_owned)
                        .collect();
                }
                _ => break,
            }
            taken += 1;
        }
        args.drain(..taken);
        let misplaced = args
            .iter()
            .take_while(|argument| *argument != "--")
            .find(|argument| {
                let name = argument
                    .split_once('=')
                    .map_or(argument.as_str(), |(name, _)| name);
                OPTIONS.contains(&name)
            });
        if let Some(argument) = misplaced {
            return Err(format!(
                "`{argument}` is buildd's option: put it before Cargo's arguments"
            ));
        }
        Ok(options)
    }
}

fn build(home: &Path, command: Command, mut args: Vec<String>) -> Result<ExitCode, String> {
    let directory = std::env::current_dir()
        .map_err(|error| format!("could not read the current directory: {error}"))?;
    let options = Options::take(&mut args, &directory)?;
    let request = BuildRequest {
        directory,
        operation: Operation {
            command,
            args,
            rustflags: options.rustflags,
        },
        label: std::env::var("BUILDD_LABEL")
            .ok()
            .filter(|label| !label.is_empty()),
        copy_to: options.copy_to,
        rerun_all: options.rerun_all,
        os: options.os,
    };
    let last = if options.json {
        client::build(connect(home)?, request, |message| {
            println!(
                "{}",
                serde_json::to_string(message).expect("messages serialize")
            );
        })?
    } else {
        client::build(connect(home)?, request, render)?
    };
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
        Message::Copied { artifact } => {
            let copy = serde_json::from_str::<serde_json::Value>(artifact)
                .ok()
                .and_then(|artifact| artifact["executable"].as_str().map(str::to_owned));
            eprintln!("buildd: copied {}", copy.as_deref().unwrap_or(artifact));
        }
        Message::Finished {
            revision,
            outcome,
            queued_ms,
            build_ms,
            test_ms,
        } => {
            let ended = match outcome {
                Outcome::Exited { code: 0 } => "succeeded".to_owned(),
                Outcome::Exited { code } => format!("failed with exit code {code}"),
                Outcome::Signaled { signal } => format!("was killed by signal {signal}"),
                Outcome::Failed { reason } => format!("could not run: {reason}"),
            };
            let phases = test_ms.map_or_else(String::new, |test_ms| {
                format!(
                    " ({} compiling, {} testing)",
                    seconds(build_ms.saturating_sub(test_ms)),
                    seconds(test_ms)
                )
            });
            eprintln!(
                "buildd: tree {} {ended} after {} in the build{phases} and {} in the queue",
                revision.short(),
                seconds(*build_ms),
                seconds(*queued_ms),
            );
        }
        Message::Rejected { reason } => eprintln!("buildd: {reason}"),
    }
}

fn failures(home: &Path, args: Vec<String>) -> Result<ExitCode, String> {
    let mut hours = 24;
    let mut os = None;
    let mut args = args.into_iter();
    while let Some(argument) = args.next() {
        let mut value = || {
            args.next()
                .ok_or_else(|| format!("{argument} needs a value"))
        };
        match argument.as_str() {
            "--hours" => {
                hours = value()?
                    .parse()
                    .map_err(|_| "--hours takes a whole number".to_owned())?;
            }
            "--os" => os = Some(value()?),
            other => return Err(format!("unknown option `{other}`\n{USAGE}")),
        }
    }
    let summary = client::failures(connect(home)?, hours, os)?;
    print!("{}", render_failures(&summary, hours));
    Ok(ExitCode::SUCCESS)
}

fn render_failures(summary: &Failures, hours: u64) -> String {
    use std::fmt::Write as _;

    let mut out = String::new();
    let _ = writeln!(
        out,
        "last {hours} h: {} of {} builds failed · logs in {}",
        summary.failed,
        summary.builds,
        summary.logs.display()
    );
    if !summary.recent.is_empty() {
        let _ = writeln!(out, "\nrecent failures, newest first");
    }
    for build in &summary.recent {
        let at =
            buildd::log::local_time(std::time::UNIX_EPOCH + Duration::from_millis(build.at_ms));
        let outcome = match &build.outcome {
            Outcome::Exited { code } => format!("exit {code}"),
            Outcome::Signaled { signal } => format!("signal {signal}"),
            Outcome::Failed { reason } => format!("buildd: {reason}"),
        };
        let _ = writeln!(
            out,
            "  {}  {} for {}  {outcome}  tree {}",
            &at[5..16],
            build.operation,
            build.who.join(", "),
            build.revision.short(),
        );
        let failed = &build.report.failed_tests;
        if !failed.is_empty() {
            let shown = failed
                .iter()
                .take(3)
                .cloned()
                .collect::<Vec<_>>()
                .join(", ");
            let more = failed.len().saturating_sub(3);
            let more = if more > 0 {
                format!(" (+{more})")
            } else {
                String::new()
            };
            let _ = writeln!(out, "               failed: {shown}{more}");
        }
        if let Some(error) = build.report.errors.first() {
            let _ = writeln!(out, "               error: {error}");
        }
        if let Some(log) = &build.report.log {
            let _ = writeln!(out, "               log: {log}");
        }
    }
    if !summary.tests.is_empty() {
        let _ = writeln!(out, "\ntests failing most often");
    }
    for test in &summary.tests {
        let _ = writeln!(
            out,
            "  {:>3} builds {:>3} trees  {}  ({})",
            test.failures,
            test.trees,
            test.test,
            test.who.join(", ")
        );
    }
    if !summary.mixed.is_empty() {
        let _ = writeln!(
            out,
            "\nsame tree and command, passed and failed (flaky, or trouble outside the code)"
        );
    }
    for mixed in &summary.mixed {
        let _ = writeln!(
            out,
            "  tree {}  {}  {} passed, {} failed",
            mixed.revision.short(),
            mixed.operation,
            mixed.passed,
            mixed.failed
        );
    }
    if !summary.errors.is_empty() {
        let _ = writeln!(out, "\nerrors reported most often");
    }
    for error in &summary.errors {
        let _ = writeln!(out, "  {:>3} builds  {}", error.builds, error.error);
    }
    if !summary.slowest.is_empty() {
        let _ = writeln!(out, "\nslowest tests");
    }
    for test in &summary.slowest {
        #[expect(clippy::cast_precision_loss, reason = "a duration for people")]
        let seconds = test.ms as f64 / 1000.0;
        let _ = writeln!(out, "  {seconds:>7.1} s  {}", test.test);
    }
    out
}

fn status(home: &Path) -> Result<ExitCode, String> {
    let Status {
        jobs,
        idle_jobs,
        capacity,
        slot_limit,
        free_disk,
        min_free,
        memory,
        memory_in_use,
        slots,
        queue,
        draining,
    } = client::status(connect(home)?)?;
    let disk = free_disk.map_or_else(
        || "unknown".to_owned(),
        |free| {
            let below = if free < min_free { ", below" } else { "" };
            format!("{} free{below}", gib(free))
        },
    );
    println!(
        "{capacity} slots of {}, jobs: {idle_jobs} of {jobs} idle, memory: {} of {}, \
         disk: {disk} (floor {}){}",
        gib(slot_limit),
        gib(memory_in_use),
        gib(memory),
        gib(min_free),
        if draining {
            ", draining: no new builds"
        } else {
            ""
        }
    );
    for slot in slots {
        let worktree = slot.worktree.map_or_else(String::new, |worktree| {
            format!(" for {}", worktree.display())
        });
        let size = slot.size.map_or_else(String::new, |size| {
            let warning = if slot.undersized {
                ", limit below what its builds use"
            } else {
                ""
            };
            format!(" [{}{warning}]", gib(size))
        });
        match slot.build {
            Some(build) => println!(
                "slot {}{size}: {} {} for {} ({}, compiled {} reused {}{})",
                slot.name,
                build.revision.short(),
                build.operation,
                build.who.join(", "),
                seconds(build.elapsed_ms),
                build.compiled,
                build.fresh,
                if build.cancelled { ", cancelled" } else { "" },
            ),
            None if slot.maintaining => println!("slot {}{size}: pruning{worktree}", slot.name),
            None => println!("slot {}{size}: idle{worktree}", slot.name),
        }
    }
    for (position, build) in queue.iter().enumerate() {
        let why = match (&build.memory_needed, &build.held) {
            (Some(needed), _) => format!(", waiting for memory (needs {})", gib(*needed)),
            (None, Some(hold)) => format!(", waiting for slot {}", hold.slot),
            (None, None) => String::new(),
        };
        println!(
            "queued {}: {} {} (waited {}{why}) for {}",
            position + 1,
            build.revision.short(),
            build.operation,
            seconds(build.waited_ms),
            build.who.join(", "),
        );
    }
    Ok(ExitCode::SUCCESS)
}

#[expect(clippy::cast_precision_loss, reason = "a size in GiB for people")]
fn gib(bytes: u64) -> String {
    format!("{:.1} GiB", bytes as f64 / f64::from(1 << 30))
}

fn seconds(millis: u64) -> String {
    format!("{}.{} s", millis / 1000, millis % 1000 / 100)
}

/// Passes one request from standard input to the daemon and everything it
/// answers to standard output, until either side ends.
fn serve(home: &Path) -> Result<ExitCode, String> {
    let stream = connect(home)?;
    let mut to_daemon = stream
        .try_clone()
        .map_err(|error| format!("could not use the daemon's socket: {error}"))?;
    // Standard input's end is the other side withdrawing: the daemon hears
    // it as the end of this connection's stream.
    std::thread::Builder::new()
        .name("buildd-serve-input".into())
        .spawn(move || {
            let _ = std::io::copy(&mut std::io::stdin().lock(), &mut to_daemon);
            let _ = to_daemon.shutdown(std::net::Shutdown::Write);
        })
        .map_err(|error| format!("could not read standard input: {error}"))?;
    let mut from_daemon = stream;
    std::io::copy(&mut from_daemon, &mut std::io::stdout().lock())
        .map_err(|error| format!("lost the daemon: {error}"))?;
    Ok(ExitCode::SUCCESS)
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

#[cfg(test)]
mod tests {
    use super::*;

    fn take(args: &[&str]) -> (Result<Options, String>, Vec<String>) {
        let mut args = args.iter().map(|argument| (*argument).to_owned()).collect();
        let options = Options::take(&mut args, Path::new("/work"));
        (options, args)
    }

    #[test]
    fn options_come_off_the_front_and_cargos_arguments_stay() {
        let (options, rest) = take(&[
            "--json",
            "--copy-to",
            "out",
            "--rustflags=-C force-frame-pointers=yes",
            "--release",
            "--",
            "--json",
        ]);
        assert_eq!(
            options.unwrap(),
            Options {
                json: true,
                copy_to: Some("/work/out".into()),
                rustflags: vec!["-C".into(), "force-frame-pointers=yes".into()],
                rerun_all: false,
                os: None,
            }
        );
        assert_eq!(rest, ["--release", "--", "--json"]);
        let (options, rest) = take(&["-p", "x", "--", "--json"]);
        assert_eq!(options.unwrap(), Options::default());
        assert_eq!(rest, ["-p", "x", "--", "--json"]);
        let (misplaced, _) = take(&["-p", "x", "--copy-to=/abs"]);
        assert!(misplaced.unwrap_err().contains("before Cargo's arguments"));
        assert!(
            take(&["--copy-to"])
                .0
                .unwrap_err()
                .contains("needs a value")
        );
    }
}
