//! The `buildd` command: the daemon and its command-line client.

use std::io::ErrorKind;
use std::os::unix::net::UnixStream;
use std::os::unix::process::CommandExt as _;
use std::path::{Path, PathBuf};
use std::process::{ExitCode, Stdio};
use std::time::{Duration, Instant};

use buildd::cargo::{Command, Operation};
use buildd::log::gib;
use buildd::protocol::{BuildRequest, Chore, Failures, Message, Outcome, Status};
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
                           into DIR on the machine that builds: with --os,
                           the remote host, and DIR must be absolute
        --rustflags FLAGS  pass FLAGS to every rustc, as RUSTFLAGS would
        --rerun-all        nextest: also run test binaries that are unchanged
                           since they passed (they are skipped otherwise)
        --os OS            use any enabled host running OS (linux, macos)
        --host NAME        use this configured host, or local
        --exclusive        wait for active builds, then run alone on that host;
                           later submissions wait even with spare capacity
      Without a constraint, use an enabled host with available capacity.
      Artifact exports without a host or OS constraint build locally.
  buildd run [--json] [--copy-to DIR] [--os OS] [--host NAME] [--exclusive] -- COMMAND [ARGS...]
      Run COMMAND, not Cargo, on the same snapshot of this worktree a build
      would compile, in a build slot, queued and cancelled like builds, and
      exit with its status. It runs in the slot's checkout, in the
      directory matching this one; COMMAND with a `/` is a path from there,
      a bare name is looked up in the daemon's PATH. It gets the daemon's
      environment plus BUILDD_TREE (the tree id), BUILDD_JOBS (the cores it
      may keep busy), BUILDD_OUT (an empty directory for its outputs) and,
      for Cargo it starts, CARGO_TARGET_DIR (a target of its own, deleted
      afterwards; never the slot's) and CARGO_BUILD_JOBS. With --copy-to,
      what it left in BUILDD_OUT is moved into DIR on the machine that ran
      it once it succeeds. Withdrawing it stops its whole process group.
  buildd status
      Show the build slots and the queue.
  buildd plan check|clippy|build|test|nextest|run [OPTIONS] [ARGS...]
      Explain the current host and slot choices for this worktree's exact
      content, without submitting work, reserving slots or uploading source.
      Takes the same options as the corresponding build; --json prints one
      plan object. Unknown timings stay unknown. Placement is reassessed
      when a build is submitted; a plan grants no priority or reservation.
  buildd hosts [enable|disable NAME]
      Show or persistently change host policy for new builds. Accepted work
      continues. Host policy takes effect immediately, without a restart.
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
        "daemon" | "status" | "top" | "drain" | "undrain" | "serve" | "failures" | "hosts" => None,
        "help" | "--help" | "-h" => {
            println!("{USAGE}");
            return ExitCode::SUCCESS;
        }
        "plan" => match args.next().as_deref().and_then(Command::parse) {
            Some(command) => Some(command),
            None => {
                eprintln!("usage: buildd plan COMMAND [OPTIONS] [ARGS...]");
                return ExitCode::from(2);
            }
        },
        other => match Command::parse(other) {
            Some(command) => Some(command),
            None => {
                eprintln!("buildd: unknown command `{other}`\n{USAGE}");
                return ExitCode::from(2);
            }
        },
    };
    let result = config::home().and_then(|home| match command {
        Some(command) => build(&home, command, args.collect(), name == "plan"),
        None if name == "daemon" => daemon(&home),
        None if name == "top" => {
            let remote_home = home.clone();
            top::run(
                || client::activity(connect(&home)?),
                move || client::remotes(connect(&remote_home)?),
            )
            .map(|()| ExitCode::SUCCESS)
        }
        None if name == "drain" || name == "undrain" => {
            client::drain(connect(&home)?, name == "drain")?;
            status(&home)
        }
        None if name == "serve" => serve(&home),
        None if name == "failures" => failures(&home, args.collect()),
        None if name == "hosts" => hosts(&home, args.collect()),
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

fn hosts(home: &Path, args: Vec<String>) -> Result<ExitCode, String> {
    let hosts = match args.as_slice() {
        [] => client::hosts(connect(home)?)?,
        [action, name] if action == "enable" || action == "disable" => {
            client::set_host(connect(home)?, name.clone(), action == "enable")?
        }
        _ => return Err("usage: buildd hosts [enable|disable NAME]".into()),
    };
    for host in hosts {
        println!(
            "{} ({}) {}",
            host.name,
            host.os,
            if host.enabled { "enabled" } else { "disabled" }
        );
    }
    Ok(ExitCode::SUCCESS)
}

/// The names of buildd's own options of a build.
const OPTIONS: [&str; 7] = [
    "--json",
    "--copy-to",
    "--rustflags",
    "--rerun-all",
    "--os",
    "--host",
    "--exclusive",
];

/// buildd's own options of a build, which come before Cargo's arguments.
#[derive(Debug, Default, PartialEq)]
struct Options {
    json: bool,
    copy_to: Option<PathBuf>,
    rustflags: Vec<String>,
    rerun_all: bool,
    os: Option<String>,
    host: Option<String>,
    exclusive: bool,
}

impl Options {
    /// Takes the options off the front of `args`, leaving Cargo's
    /// arguments. A relative directory to copy to is taken relative to
    /// `directory`, except with `--os`: it names a directory on the machine
    /// that builds, which may be another one.
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
                "--exclusive" if inline.is_none() => options.exclusive = true,
                "--copy-to" => options.copy_to = Some(value(&mut taken)?.into()),
                "--os" => options.os = Some(value(&mut taken)?),
                "--host" => options.host = Some(value(&mut taken)?),
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
        if let Some(copy_to) = options.copy_to.take() {
            if copy_to.is_relative()
                && (options.os.is_some()
                    || options.host.as_deref().is_some_and(|host| host != "local"))
            {
                return Err(format!(
                    "with --os, --copy-to names a directory on the machine that builds: \
                     give it as an absolute path, not {}",
                    copy_to.display()
                ));
            }
            options.copy_to = Some(directory.join(copy_to));
        }
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

fn build(
    home: &Path,
    command: Command,
    mut args: Vec<String>,
    planning: bool,
) -> Result<ExitCode, String> {
    let directory = std::env::current_dir()
        .map_err(|error| format!("could not read the current directory: {error}"))?;
    let options = Options::take(&mut args, &directory)?;
    if command == Command::Run {
        args = run_arguments(&options, args)?;
    }
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
        host: options.host,
        exclusive: options.exclusive,
        optional: false,
    };
    if planning {
        let plan = client::plan(connect(home)?, request)?;
        if options.json {
            println!("{}", serde_json::to_string(&plan).expect("plans serialize"));
        } else {
            print!("{}", render_plan(&plan));
        }
        return Ok(ExitCode::SUCCESS);
    }
    let last = if options.json {
        client::build(connect(home)?, request, |message| {
            println!(
                "{}",
                serde_json::to_string(message).expect("messages serialize")
            );
        })?
    } else {
        client::build(connect(home)?, request, |message| render(message, command))?
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

fn render_plan(plan: &buildd::protocol::Plan) -> String {
    use std::fmt::Write as _;
    let mut out = format!(
        "Plan for tree {} (observation only; no reservation)\n",
        plan.revision
    );
    let _ = writeln!(
        out,
        "Recommended host: {}",
        plan.recommended_host.as_deref().unwrap_or("none")
    );
    let _ = writeln!(out, "{}", plan.reason);
    for host in &plan.hosts {
        let _ = writeln!(
            out,
            "\n{} ({}){}",
            host.host.name,
            host.host.os,
            host.exclusion
                .as_ref()
                .map_or(String::new(), |reason| format!(": {reason}"))
        );
        let Some(forecast) = &host.forecast else {
            continue;
        };
        let status = &forecast.capacity.status;
        let reservation = forecast
            .capacity
            .peak_memory
            .or(status.unmeasured_memory)
            .unwrap_or(status.memory);
        let _ = writeln!(
            out,
            "  {} of {} CPU jobs idle; memory {} reserved + {} requested / {}; {} queued builds",
            status.idle_jobs,
            status.jobs,
            gib(status.memory_in_use),
            gib(reservation),
            gib(status.memory),
            status.queue.len()
        );
        let _ = writeln!(
            out,
            "  queue wait: {}; transfer: {}; historical build: {}",
            timing(forecast.queue_ms),
            timing(host.transfer_ms),
            timing(forecast.historical_build_ms)
        );
        for slot in &forecast.slots {
            let selected = forecast.recommended_slot.as_ref() == Some(&slot.slot);
            let _ = writeln!(
                out,
                "  {} {}{}: {} units reused, {} missing ({} history), source work {}; slot wait {}; compile {}{}",
                if selected { "*" } else { " " },
                slot.slot,
                if slot.new {
                    " [new]"
                } else if slot.available {
                    " [idle]"
                } else {
                    " [busy]"
                },
                slot.reused_units,
                slot.missing_units,
                if slot.exact_history {
                    "exact"
                } else {
                    "inferred"
                },
                slot.source_work
                    .map_or_else(|| "unknown".into(), |work| work.to_string()),
                timing(slot.slot_wait_ms),
                timing(slot.compile_ms),
                slot.note
                    .as_ref()
                    .map_or(String::new(), |note| format!("; {note}"))
            );
        }
        for note in &forecast.notes {
            let _ = writeln!(out, "  {note}");
        }
    }
    out
}

fn timing(ms: Option<u64>) -> String {
    ms.map_or_else(|| "unknown".into(), |ms| format!("{ms} ms"))
}

/// The program and arguments of `buildd run`, from what follows its
/// options: `-- COMMAND [ARGS...]`.
fn run_arguments(options: &Options, args: Vec<String>) -> Result<Vec<String>, String> {
    if !options.rustflags.is_empty() || options.rerun_all {
        return Err(
            "run compiles and tests nothing: it takes --json, --copy-to, --os and --host".into(),
        );
    }
    match args.split_first() {
        Some((separator, command)) if separator == "--" && !command.is_empty() => {
            Ok(command.to_vec())
        }
        _ => Err("usage: buildd run [OPTIONS] -- COMMAND [ARGS...]".into()),
    }
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

/// Prints a message of a build of `command` the way Cargo would have:
/// diagnostics and Cargo's own lines to standard error, test output to
/// standard output. A command of the client's own prints as it would alone.
fn render(message: &Message, command: Command) {
    match message {
        Message::Queued { revision } => eprintln!("buildd: tree {} queued", revision.short()),
        Message::Started { revision, slot } => {
            let doing = if command.uses_cargo() {
                "building"
            } else {
                "running on"
            };
            eprintln!("buildd: {doing} tree {} in slot {slot}", revision.short());
        }
        Message::Stdout { line } if !command.uses_cargo() => println!("{line}"),
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
        Message::Delivered { path } => eprintln!("buildd: delivered {}", path.display()),
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
        disk_budget,
        disk_used,
        free_disk,
        min_free,
        build_headroom,
        disk_wait,
        undersized,
        memory,
        memory_in_use,
        unmeasured_memory,
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
        "{capacity} slots, build disk: {} of {}{}, jobs: {idle_jobs} of {jobs} idle, \
         memory reserved: {} of {}, disk: {disk} (floor {}){}",
        gib(disk_used),
        gib(disk_budget),
        if undersized {
            " (below what builds use)"
        } else {
            ""
        },
        gib(memory_in_use),
        gib(memory),
        gib(min_free),
        if draining {
            ", draining: no new builds"
        } else {
            ""
        }
    );
    if let Some(reserved) = unmeasured_memory {
        println!(
            "unmeasured builds: {} reserved each (not a measured peak)",
            gib(reserved)
        );
    }
    if let Some(wait) = disk_wait {
        println!(
            "disk admission: {wait} ({} headroom per build)",
            gib(build_headroom)
        );
    }
    if slots
        .iter()
        .any(|slot| slot.build.as_ref().is_some_and(|build| build.exclusive))
    {
        println!("exclusive build active: all other builds wait on this host");
    } else if queue.iter().any(|build| build.exclusive) {
        println!("exclusive build queued: later submissions wait on this host");
    }
    for slot in slots {
        let worktree = slot.worktree.map_or_else(String::new, |worktree| {
            format!(" for {}", worktree.display())
        });
        let size = slot
            .size
            .map_or_else(String::new, |size| format!(" [{}]", gib(size)));
        match slot.build {
            Some(build) => println!(
                "slot {}{size}: {} {} for {} ({}, compiled {} reused {}{}{})",
                slot.name,
                build.revision.short(),
                build.operation,
                build.who.join(", "),
                seconds(build.elapsed_ms),
                build.compiled,
                build.fresh,
                if build.exclusive { ", exclusive" } else { "" },
                if build.cancelled { ", cancelled" } else { "" },
            ),
            None => {
                let doing = match slot.chore {
                    Some(Chore::Measuring) => "measuring",
                    Some(Chore::Evicting) => "evicting",
                    Some(Chore::Cache) => "cache reserved",
                    None => "idle",
                };
                println!("slot {}{size}: {doing}{worktree}", slot.name);
            }
        }
    }
    for (position, build) in queue.iter().enumerate() {
        let why = match (&build.memory_needed, &build.held) {
            (Some(needed), _) => format!(", waiting for memory (needs {})", gib(*needed)),
            (None, Some(hold)) => format!(", waiting for slot {}", hold.slot),
            (None, None) => String::new(),
        };
        println!(
            "queued {}: {} {} (waited {}{}{why}) for {}",
            position + 1,
            build.revision.short(),
            build.operation,
            seconds(build.waited_ms),
            if build.exclusive { ", exclusive" } else { "" },
            build.who.join(", "),
        );
    }
    Ok(ExitCode::SUCCESS)
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
            let _ = forward_lines(std::io::stdin().lock(), &mut to_daemon);
            let _ = to_daemon.shutdown(std::net::Shutdown::Write);
        })
        .map_err(|error| format!("could not read standard input: {error}"))?;
    forward_lines(std::io::BufReader::new(stream), std::io::stdout().lock())
        .map_err(|error| format!("lost the daemon: {error}"))?;
    Ok(ExitCode::SUCCESS)
}

/// Publish each newline-delimited protocol frame before waiting for another.
/// A bulk socket-to-pipe splice can wait with the pipe locked, preventing
/// SSH from reading even the frame already delivered and answering keepalives.
fn forward_lines(
    mut input: impl std::io::BufRead,
    mut output: impl std::io::Write,
) -> std::io::Result<()> {
    let mut frame = Vec::new();
    loop {
        frame.clear();
        if input.read_until(b'\n', &mut frame)? == 0 {
            return Ok(());
        }
        output.write_all(&frame)?;
        output.flush()?;
    }
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
                host: None,
                exclusive: false,
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

    #[test]
    fn exclusive_applies_to_builds_plans_and_runs_before_their_arguments() {
        let (options, rest) = take(&["--host", "m4", "--exclusive", "-p", "app"]);
        let options = options.unwrap();
        assert!(options.exclusive);
        assert_eq!(options.host.as_deref(), Some("m4"));
        assert_eq!(rest, ["-p", "app"]);
        assert!(take(&["-p", "app", "--exclusive"]).0.is_err());
        assert!(take(&["--exclusive=true"]).0.is_err());
        let (options, rest) = take(&["--exclusive", "--", "benchmark", "--exclusive"]);
        let options = options.unwrap();
        assert!(options.exclusive);
        assert_eq!(
            run_arguments(&options, rest).unwrap(),
            ["benchmark", "--exclusive"]
        );
    }

    #[test]
    fn run_takes_its_command_after_the_separator() {
        let (options, rest) = take(&["--os", "linux", "--copy-to=/out", "--", "make", "-j"]);
        let options = options.unwrap();
        assert_eq!(run_arguments(&options, rest).unwrap(), ["make", "-j"]);
        for missing in [&[][..], &["--"], &["make"]] {
            let (options, rest) = take(missing);
            assert!(
                run_arguments(&options.unwrap(), rest)
                    .unwrap_err()
                    .contains("usage"),
                "{missing:?}"
            );
        }
        for compiling in [
            &["--rustflags=-g", "--", "make"][..],
            &["--rerun-all", "--", "make"],
        ] {
            let (options, rest) = take(compiling);
            assert!(
                run_arguments(&options.unwrap(), rest).is_err(),
                "{compiling:?}"
            );
        }
        // After the separator, buildd's option names are the command's.
        let (options, rest) = take(&["--", "tool", "--json"]);
        assert_eq!(
            run_arguments(&options.unwrap(), rest).unwrap(),
            ["tool", "--json"]
        );
    }

    #[test]
    fn with_os_the_directory_to_copy_to_is_on_the_building_machine_and_absolute() {
        let (options, _) = take(&["--copy-to", "/srv/bin", "--os", "linux", "-p", "x"]);
        assert_eq!(
            options.unwrap(),
            Options {
                copy_to: Some("/srv/bin".into()),
                os: Some("linux".into()),
                ..Options::default()
            }
        );
        for relative in [
            &["--copy-to", "bin", "--os", "linux"][..],
            &["--os=linux", "--copy-to=bin"],
        ] {
            let (refused, _) = take(relative);
            assert!(
                refused.unwrap_err().contains("absolute path"),
                "{relative:?}"
            );
        }
        let (options, _) = take(&["--host=air", "--os=macos", "--copy-to=/out"]);
        assert_eq!(options.unwrap().host.as_deref(), Some("air"));
        assert!(take(&["--host=air", "--copy-to=out"]).0.is_err());
        assert_eq!(
            take(&["--host=local", "--copy-to=out"]).0.unwrap().copy_to,
            Some("/work/out".into())
        );
    }
}
