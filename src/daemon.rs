//! The daemon: accepts clients on a Unix socket, keeps the scheduler, and
//! runs builds in their slots under one jobserver.

use std::collections::HashMap;
use std::convert::Infallible;
use std::io::{BufRead as _, BufReader, Read as _, Write as _};
use std::os::unix::fs::PermissionsExt as _;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::process::ExitStatus;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime};

use crossbeam_channel::{Receiver, RecvTimeoutError, Sender};
use rustix::process::{Pid, Signal, WaitId, WaitIdOptions};

use crate::activity::ActivityLog;
use crate::cargo;
use crate::cargo::Line;
use crate::config::{self, Config};
use crate::distance::{GitDistance, Workspace};
use crate::protocol::{Activity, BuildRequest, Message, Outcome, Request, Status, Usage};
use crate::scheduler::{Effect, JobId, Scheduler, SlotKey, Start, Submission, WaiterId};
use crate::slot::{self, Pruning, SlotDirectory, slot_name};
use crate::snapshot;

/// How long a cancelled build may take to stop after `SIGTERM` before its
/// process group is killed.
const TERMINATION_GRACE: Duration = Duration::from_secs(5);
/// How long output is still read after Cargo exits. Only a process that left
/// the build's process group can hold the output open longer; its remaining
/// output is abandoned.
const OUTPUT_GRACE: Duration = Duration::from_secs(2);

/// What the scheduler thread hears about.
enum Event {
    Submit {
        submission: Submission,
        messages: Sender<Message>,
    },
    Withdraw {
        waiter: WaiterId,
    },
    Status {
        reply: Sender<Status>,
    },
    Activity {
        reply: Sender<Activity>,
    },
    Output {
        job: JobId,
        message: Message,
    },
    /// Cargo reported a crate of `job`, up to date when `fresh`, as
    /// compiled `unit` when its files name one.
    Crate {
        job: JobId,
        fresh: bool,
        unit: Option<String>,
    },
    /// A build script of `job` ran, writing into compiled `unit`.
    BuildScript {
        job: JobId,
        unit: String,
    },
    /// Cargo ended, having used `usage` when it ran.
    Exited {
        job: JobId,
        outcome: Outcome,
        usage: Option<Usage>,
    },
    /// Slot `key` of `repository` was kept within its disk limit by
    /// `pruning`, removing compiled units `evicted` (None when that failed),
    /// and its checkout's `workspace` was read.
    Maintained {
        key: SlotKey,
        repository: PathBuf,
        pruning: Option<Pruning>,
        evicted: Vec<String>,
        workspace: Option<Workspace>,
    },
}

/// Runs the daemon for `home`. It returns only when it cannot start.
///
/// # Errors
/// When another daemon runs for `home`, or the home or socket cannot be
/// prepared.
pub fn run(home: &Path, config: Config) -> Result<Infallible, String> {
    std::fs::create_dir_all(home)
        .and_then(|()| std::fs::set_permissions(home, std::fs::Permissions::from_mode(0o700)))
        .map_err(|error| format!("could not prepare {}: {error}", home.display()))?;
    // Held for the daemon's life: one daemon per home.
    let lock = std::fs::File::create(home.join("daemon.lock"))
        .map_err(|error| format!("could not create the daemon lock: {error}"))?;
    rustix::fs::flock(&lock, rustix::fs::FlockOperation::NonBlockingLockExclusive)
        .map_err(|_| format!("another buildd daemon runs for {}", home.display()))?;
    let socket = config::socket(home);
    match std::fs::remove_file(&socket) {
        Ok(()) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(format!("could not remove {}: {error}", socket.display())),
    }
    let listener = UnixListener::bind(&socket)
        .map_err(|error| format!("could not listen on {}: {error}", socket.display()))?;
    let jobserver = jobserver::Client::new(config.jobs)
        .map_err(|error| format!("could not create the jobserver: {error}"))?;
    eprintln!(
        "buildd: listening on {} with {} slots of {} and {} jobs",
        socket.display(),
        config.slots,
        gib(config.slot_limit),
        config.jobs
    );

    let (events, received) = crossbeam_channel::unbounded();
    let accepting = Accepting {
        home: home.to_owned(),
        events: events.clone(),
        next_waiter: Arc::new(AtomicU64::new(0)),
    };
    std::thread::Builder::new()
        .name("buildd-accept".into())
        .spawn(move || accepting.accept(&listener))
        .map_err(|error| format!("could not start accepting clients: {error}"))?;
    let mut scheduler = Scheduler::new(config.slots, GitDistance::default());
    restore_slots(home, config, &mut scheduler)?;
    Daemon {
        home: home.to_owned(),
        config,
        jobserver,
        events,
        scheduler,
        waiters: HashMap::new(),
        runs: HashMap::new(),
        log: ActivityLog::new(SystemTime::now()),
    }
    .drive(&received)
}

/// Takes back the slots an earlier daemon left in `home`: their checkouts,
/// targets and records stay warm across restarts. Slots beyond the
/// configured count, left from a larger configuration, are removed so build
/// disk stays within `slots` times the limit.
fn restore_slots(
    home: &Path,
    config: Config,
    scheduler: &mut Scheduler<GitDistance>,
) -> Result<(), String> {
    for existing in slot::existing_slots(home)? {
        let name = slot_name(&existing.repository, existing.index);
        if existing.index >= config.slots {
            match existing.directory.remove() {
                Ok(()) => eprintln!("buildd: removed slot {name}: beyond {} slots", config.slots),
                Err(error) => eprintln!("buildd: could not remove slot {name}: {error}"),
            }
            continue;
        }
        if !existing.repository.exists() {
            eprintln!(
                "buildd: slot {name}: its repository {} is gone; leaving the slot alone",
                existing.repository.display()
            );
            continue;
        }
        let restored = existing.directory.tree().and_then(|revision| {
            existing
                .directory
                .read_record()
                .map(|record| (revision, record))
        });
        match restored {
            Ok((revision, record)) => {
                eprintln!(
                    "buildd: restored slot {name} with {} compilations",
                    record.compiled.len()
                );
                scheduler.restore(existing.repository, existing.index, revision, record);
            }
            // A slot whose checkout never completed is rebuilt when needed.
            Err(error) => eprintln!("buildd: slot {name} not restored: {error}"),
        }
    }
    Ok(())
}

struct Accepting {
    home: PathBuf,
    events: Sender<Event>,
    next_waiter: Arc<AtomicU64>,
}

impl Accepting {
    fn accept(&self, listener: &UnixListener) {
        for stream in listener.incoming() {
            let stream = match stream {
                Ok(stream) => stream,
                Err(error) => {
                    eprintln!("buildd: could not accept a client: {error}");
                    continue;
                }
            };
            let connection = Connection {
                home: self.home.clone(),
                events: self.events.clone(),
                waiter: WaiterId(self.next_waiter.fetch_add(1, Ordering::Relaxed)),
            };
            let spawned = std::thread::Builder::new()
                .name("buildd-client".into())
                .spawn(move || connection.serve(stream));
            if let Err(error) = spawned {
                eprintln!("buildd: could not serve a client: {error}");
            }
        }
    }
}

/// One client's connection.
struct Connection {
    home: PathBuf,
    events: Sender<Event>,
    waiter: WaiterId,
}

impl Connection {
    fn serve(self, mut stream: UnixStream) {
        let mut reader = match stream.try_clone() {
            Ok(reader) => BufReader::new(reader),
            Err(error) => {
                eprintln!("buildd: could not read from a client: {error}");
                return;
            }
        };
        let mut line = String::new();
        if reader.read_line(&mut line).is_err() || line.is_empty() {
            return;
        }
        match serde_json::from_str::<Request>(&line) {
            Err(error) => {
                let reason = format!("not a request: {error}");
                let _ = write_line(&mut stream, &Message::Rejected { reason });
            }
            Ok(Request::Status) => {
                let (reply, status) = crossbeam_channel::bounded(1);
                if self.events.send(Event::Status { reply }).is_ok()
                    && let Ok(status) = status.recv()
                {
                    let _ = write_line(&mut stream, &status);
                }
            }
            Ok(Request::Activity) => {
                let (reply, activity) = crossbeam_channel::bounded(1);
                if self.events.send(Event::Activity { reply }).is_ok()
                    && let Ok(activity) = activity.recv()
                {
                    let _ = write_line(&mut stream, &activity);
                }
            }
            Ok(Request::Build(request)) => self.build(stream, reader, request),
        }
    }

    fn build(
        self,
        mut stream: UnixStream,
        mut reader: BufReader<UnixStream>,
        request: BuildRequest,
    ) {
        let prepared = request.operation.validate().and_then(|()| {
            let source = snapshot::resolve(&request.directory)?;
            let revision = source.snapshot(&self.home.join("tmp"))?;
            Ok((source, revision))
        });
        let (source, revision) = match prepared {
            Ok(prepared) => prepared,
            Err(reason) => {
                let _ = write_line(&mut stream, &Message::Rejected { reason });
                return;
            }
        };
        let (messages, received) = crossbeam_channel::unbounded();
        let submission = Submission {
            waiter: self.waiter,
            source,
            revision,
            operation: request.operation,
            label: request.label,
        };
        if self
            .events
            .send(Event::Submit {
                submission,
                messages,
            })
            .is_err()
        {
            return;
        }
        // The client sends nothing more; the end of its stream is its
        // withdrawal.
        let events = self.events.clone();
        let waiter = self.waiter;
        let watching = std::thread::Builder::new()
            .name("buildd-client-watch".into())
            .spawn(move || {
                let mut buffer = [0; 256];
                while matches!(reader.read(&mut buffer), Ok(read) if read > 0) {}
                let _ = events.send(Event::Withdraw { waiter });
            });
        if watching.is_err() {
            let _ = self.events.send(Event::Withdraw { waiter });
            return;
        }
        for message in received {
            let last = message.is_final();
            if write_line(&mut stream, &message).is_err() {
                let _ = self.events.send(Event::Withdraw { waiter });
                break;
            }
            if last {
                break;
            }
        }
        let _ = stream.shutdown(std::net::Shutdown::Both);
    }
}

fn write_line(stream: &mut UnixStream, value: &impl serde::Serialize) -> std::io::Result<()> {
    let mut text = serde_json::to_string(value).expect("protocol values serialize");
    text.push('\n');
    stream.write_all(text.as_bytes())
}

struct Daemon {
    home: PathBuf,
    config: Config,
    jobserver: jobserver::Client,
    events: Sender<Event>,
    scheduler: Scheduler<GitDistance>,
    waiters: HashMap<WaiterId, Sender<Message>>,
    runs: HashMap<JobId, Arc<Run>>,
    log: ActivityLog,
}

impl Daemon {
    fn drive(mut self, received: &Receiver<Event>) -> ! {
        loop {
            let event = match self.scheduler.next_maintenance() {
                Some(due) => match received.recv_deadline(due) {
                    Ok(event) => event,
                    Err(RecvTimeoutError::Timeout) => {
                        for effect in self.scheduler.maintenance_due(Instant::now()) {
                            self.apply(effect);
                        }
                        continue;
                    }
                    Err(RecvTimeoutError::Disconnected) => {
                        panic!("the daemon holds a sender of its own events")
                    }
                },
                None => received
                    .recv()
                    .expect("the daemon holds a sender of its own events"),
            };
            let effects = match event {
                Event::Submit {
                    submission,
                    messages,
                } => {
                    self.waiters.insert(submission.waiter, messages);
                    self.scheduler.submit(submission)
                }
                Event::Withdraw { waiter } => {
                    self.waiters.remove(&waiter);
                    self.scheduler.withdraw(waiter)
                }
                Event::Status { reply } => {
                    let _ = reply.send(self.status());
                    Vec::new()
                }
                Event::Activity { reply } => {
                    let _ = reply.send(self.log.activity(self.status()));
                    Vec::new()
                }
                Event::Output { job, message } => self.scheduler.output(job, message),
                Event::Crate { job, fresh, unit } => {
                    self.scheduler.crate_built(job, fresh, unit);
                    Vec::new()
                }
                Event::BuildScript { job, unit } => {
                    self.scheduler.unit_used(job, unit);
                    Vec::new()
                }
                Event::Exited {
                    job,
                    outcome,
                    usage,
                } => {
                    self.runs.remove(&job);
                    self.scheduler.exited(job, &outcome, usage)
                }
                Event::Maintained {
                    key,
                    repository,
                    pruning,
                    evicted,
                    workspace,
                } => {
                    if let Some(workspace) = workspace {
                        self.scheduler.distance_mut().learn(repository, workspace);
                    }
                    self.scheduler.maintained(key, pruning, &evicted)
                }
            };
            for effect in effects {
                self.apply(effect);
            }
        }
    }

    fn status(&self) -> Status {
        let (slots, queue) = self.scheduler.status();
        Status {
            jobs: self.config.jobs,
            idle_jobs: self
                .jobserver
                .available()
                .expect("the daemon's own jobserver pipe can be queried"),
            capacity: self.config.slots,
            slot_limit: self.config.slot_limit,
            slots,
            queue,
        }
    }

    fn apply(&mut self, effect: Effect) {
        match effect {
            Effect::Report(kind) => self.log.record(SystemTime::now(), kind),
            Effect::Persist {
                repository,
                slot,
                record,
            } => {
                let directory = SlotDirectory::new(&self.home, &repository, slot);
                if let Err(error) = directory.write_record(&record) {
                    eprintln!("buildd: {error}");
                }
            }
            Effect::Send { waiter, message } => {
                let last = message.is_final();
                // A client that went away has its withdrawal on the way.
                if let Some(messages) = self.waiters.get(&waiter) {
                    let _ = messages.send(message);
                }
                if last {
                    self.waiters.remove(&waiter);
                }
            }
            Effect::Start(start) => {
                let run = Arc::new(Run::default());
                self.runs.insert(start.job, Arc::clone(&run));
                let building = Building {
                    slot: SlotDirectory::new(&self.home, &start.repository, start.slot),
                    start,
                    jobserver: self.jobserver.clone(),
                    run,
                    events: self.events.clone(),
                };
                let job = building.start.job;
                let spawned = std::thread::Builder::new()
                    .name("buildd-build".into())
                    .spawn(move || building.build());
                if let Err(error) = spawned {
                    let outcome = Outcome::Failed {
                        reason: format!("could not start the build thread: {error}"),
                    };
                    let _ = self.events.send(Event::Exited {
                        job,
                        outcome,
                        usage: None,
                    });
                }
            }
            Effect::Maintain {
                key,
                repository,
                slot,
                used,
            } => {
                let directory = SlotDirectory::new(&self.home, &repository, slot);
                let name = slot_name(&repository, slot);
                let limit = self.config.slot_limit;
                let events = self.events.clone();
                let spawned = std::thread::Builder::new()
                    .name("buildd-maintain".into())
                    .spawn(move || {
                        let (pruning, evicted) = maintain(&directory, &name, limit, &used);
                        // Knowing the packages lets slot choice weigh changes
                        // by what Cargo compiles again.
                        let workspace =
                            Workspace::read(&directory.source()).unwrap_or_else(|error| {
                                eprintln!("buildd: slot {name}: {error}");
                                None
                            });
                        let _ = events.send(Event::Maintained {
                            key,
                            repository,
                            pruning,
                            evicted,
                            workspace,
                        });
                    });
                if let Err(error) = spawned {
                    eprintln!("buildd: could not start maintaining a slot: {error}");
                    let _ = self.events.send(Event::Maintained {
                        key,
                        repository: PathBuf::new(),
                        pruning: None,
                        evicted: Vec::new(),
                        workspace: None,
                    });
                }
            }
            Effect::Cancel { job } => {
                if let Some(run) = self.runs.get(&job) {
                    run.cancel();
                }
            }
        }
    }
}

/// A running build's process, as cancellation sees it.
#[derive(Default)]
struct Run(Mutex<RunState>);

#[derive(Default)]
struct RunState {
    cancelled: bool,
    /// Cargo's process group, once it runs.
    group: Option<Pid>,
    /// Cargo has exited; its group id may be reused.
    exited: bool,
}

impl Run {
    fn lock(&self) -> std::sync::MutexGuard<'_, RunState> {
        self.0
            .lock()
            .expect("no thread panics holding a run's state")
    }

    /// Stops the build: `SIGTERM` to Cargo's process group now, `SIGKILL`
    /// after a grace period.
    fn cancel(self: &Arc<Self>) {
        let mut state = self.lock();
        state.cancelled = true;
        let Some(group) = state.group.filter(|_| !state.exited) else {
            return;
        };
        signal_group(group, Signal::TERM);
        drop(state);
        let run = Arc::clone(self);
        let _ = std::thread::Builder::new()
            .name("buildd-kill".into())
            .spawn(move || {
                std::thread::sleep(TERMINATION_GRACE);
                let state = run.lock();
                if !state.exited {
                    signal_group(group, Signal::KILL);
                }
            });
    }
}

fn signal_group(group: Pid, signal: Signal) {
    match rustix::process::kill_process_group(group, signal) {
        // The group has no members left.
        Ok(()) | Err(rustix::io::Errno::SRCH) => {}
        Err(error) => eprintln!("buildd: could not signal process group {group:?}: {error}"),
    }
}

/// Kills what remains of the process group of `cargo`, which has exited but
/// is not reaped yet: processes it started and left behind.
fn kill_leftovers(group: Pid) {
    match rustix::process::kill_process_group(group, Signal::KILL) {
        // No members left, or (macOS) only the unreaped Cargo itself, which
        // a signal cannot reach.
        Ok(()) | Err(rustix::io::Errno::SRCH | rustix::io::Errno::PERM) => {}
        Err(error) => eprintln!("buildd: could not kill process group {group:?}: {error}"),
    }
}

/// Keeps slot `directory` within `limit`, knowing when builds last `used`
/// each compiled unit: what that did when it worked, and the units it removed.
fn maintain(
    directory: &SlotDirectory,
    name: &str,
    limit: u64,
    used: &HashMap<String, u64>,
) -> (Option<Pruning>, Vec<String>) {
    match directory.enforce_limit(limit, used) {
        Ok((pruning, evicted)) => {
            match pruning {
                Pruning::Within { .. } => {}
                Pruning::Evicted {
                    before,
                    after,
                    caches,
                    units,
                    in_use,
                } => eprintln!(
                    "buildd: slot {name} used {}: removed {caches} incremental caches and \
                     {units} compiled units ({in_use} in use), {} left",
                    gib(before),
                    gib(after)
                ),
                Pruning::Cleared { before } => eprintln!(
                    "buildd: slot {name} used {} with nothing left to remove: cleared its target",
                    gib(before)
                ),
            }
            if pruning.undersized() {
                eprintln!(
                    "buildd: slot {name}'s limit of {} is below what its builds use, so \
                     they compile from scratch; raise slot_limit_gib or lower slots",
                    gib(limit)
                );
            }
            (Some(pruning), evicted)
        }
        Err(error) => {
            eprintln!("buildd: slot {name} could not be kept within its limit: {error}");
            (None, Vec::new())
        }
    }
}

/// A build on its own thread.
struct Building {
    start: Start,
    slot: SlotDirectory,
    jobserver: jobserver::Client,
    run: Arc<Run>,
    events: Sender<Event>,
}

impl Building {
    fn build(self) {
        let (outcome, usage) = self.outcome();
        let _ = self.events.send(Event::Exited {
            job: self.start.job,
            outcome,
            usage,
        });
    }

    /// How the build ended and, when Cargo ran, what it used.
    fn outcome(&self) -> (Outcome, Option<Usage>) {
        let failed = |reason: String| (Outcome::Failed { reason }, None);
        if let Err(reason) = self
            .slot
            .materialize(&self.start.repository, &self.start.revision)
        {
            return failed(reason);
        }
        // The token Cargo itself runs on; it takes the rest from the
        // jobserver as it goes.
        let _token = match self.jobserver.acquire() {
            Ok(token) => token,
            Err(error) => return failed(format!("could not take a job token: {error}")),
        };
        let mut command = cargo::command(
            &self.slot.source().join(&self.start.prefix),
            &self.slot.target(),
            &self.start.operation,
        );
        self.jobserver.configure(&mut command);
        let mut child = {
            let mut state = self.run.lock();
            if state.cancelled {
                return failed("cancelled before Cargo started".into());
            }
            let child = match command.spawn() {
                Ok(child) => child,
                Err(error) => return failed(format!("could not start cargo: {error}")),
            };
            state.group = Some(Pid::from_child(&child));
            child
        };
        let (done, finished) = crossbeam_channel::bounded(2);
        let stdout = child.stdout.take().expect("cargo's output is piped");
        let stderr = child.stderr.take().expect("cargo's errors are piped");
        let target = self.slot.target();
        self.forward(stdout, Some(target), done.clone());
        self.forward(stderr, None, done);

        let pid = Pid::from_child(&child);
        // Wait without reaping, so Cargo's process group id cannot be reused
        // while the group's remaining processes are killed and before a
        // pending cancellation learns that Cargo exited.
        loop {
            match rustix::process::waitid(
                WaitId::Pid(pid),
                WaitIdOptions::EXITED | WaitIdOptions::NOWAIT,
            ) {
                Err(rustix::io::Errno::INTR) => {}
                Ok(_) => break,
                Err(error) => {
                    panic!("waiting for our own cargo child fails only by a bug: {error}")
                }
            }
        }
        kill_leftovers(pid);
        self.run.lock().exited = true;
        let (status, usage) = reap(&child);
        let deadline = std::time::Instant::now() + OUTPUT_GRACE;
        for _ in 0..2 {
            if finished.recv_deadline(deadline).is_err() {
                break;
            }
        }
        (outcome(status), Some(usage))
    }

    /// Sends each line of `output` to the scheduler on a thread of its own:
    /// Cargo's standard output, whose messages name files in `target`, or
    /// its standard error when `target` is None.
    fn forward(
        &self,
        output: impl std::io::Read + Send + 'static,
        target: Option<PathBuf>,
        done: Sender<()>,
    ) {
        let events = self.events.clone();
        let job = self.start.job;
        let spawned = std::thread::Builder::new()
            .name("buildd-output".into())
            .spawn(move || {
                let mut reader = BufReader::new(output);
                let mut bytes = Vec::new();
                while matches!(reader.read_until(b'\n', &mut bytes), Ok(read) if read > 0) {
                    let line = String::from_utf8_lossy(&bytes)
                        .trim_end_matches(['\n', '\r'])
                        .to_owned();
                    bytes.clear();
                    let event = match &target {
                        None => Event::Output {
                            job,
                            message: Message::Stderr { line },
                        },
                        Some(target) => match Line::of(&line) {
                            Line::Crate { fresh, outputs } => Event::Crate {
                                job,
                                fresh,
                                unit: outputs.iter().find_map(|path| slot::unit_key(target, path)),
                            },
                            Line::BuildScript { out_dir } => match slot::unit_key(target, &out_dir)
                            {
                                Some(unit) => Event::BuildScript { job, unit },
                                None => continue,
                            },
                            Line::Forward => Event::Output {
                                job,
                                message: Message::Stdout { line },
                            },
                        },
                    };
                    if events.send(event).is_err() {
                        break;
                    }
                }
                let _ = done.send(());
            });
        if let Err(error) = spawned {
            eprintln!("buildd: could not read a build's output: {error}");
        }
    }
}

#[expect(clippy::cast_precision_loss, reason = "a size in GiB for people")]
fn gib(bytes: u64) -> String {
    format!("{:.1} GiB", bytes as f64 / f64::from(1 << 30))
}

/// The unit of `ru_maxrss`: bytes on macOS, kilobytes elsewhere.
#[cfg(target_os = "macos")]
const MAX_RSS_UNIT: u64 = 1;
#[cfg(not(target_os = "macos"))]
const MAX_RSS_UNIT: u64 = 1024;

/// Reaps `child`, which has exited, with what it and the processes it
/// waited for used: Cargo waits for every compiler it starts.
fn reap(child: &std::process::Child) -> (ExitStatus, Usage) {
    use std::os::unix::process::ExitStatusExt as _;

    let pid = libc::pid_t::try_from(child.id()).expect("process ids fit pid_t");
    let mut status = 0;
    // SAFETY: rusage holds only integers, for which all zeroes is valid.
    let mut usage: libc::rusage = unsafe { std::mem::zeroed() };
    loop {
        // SAFETY: `status` and `usage` are valid for writes for the call.
        let reaped = unsafe { libc::wait4(pid, &raw mut status, 0, &raw mut usage) };
        if reaped == pid {
            break;
        }
        let error = std::io::Error::last_os_error();
        assert_eq!(
            error.kind(),
            std::io::ErrorKind::Interrupted,
            "reaping our own exited child fails only by a bug: {error}"
        );
    }
    let millis = |time: libc::timeval| {
        u64::try_from(time.tv_sec).expect("CPU seconds are positive") * 1000
            + u64::try_from(time.tv_usec).expect("CPU microseconds are positive") / 1000
    };
    let usage = Usage {
        cpu_ms: millis(usage.ru_utime) + millis(usage.ru_stime),
        peak_memory: u64::try_from(usage.ru_maxrss).expect("memory sizes are positive")
            * MAX_RSS_UNIT,
    };
    (ExitStatus::from_raw(status), usage)
}

fn outcome(status: ExitStatus) -> Outcome {
    use std::os::unix::process::ExitStatusExt as _;

    match (status.code(), status.signal()) {
        (Some(code), _) => Outcome::Exited { code },
        (None, Some(signal)) => Outcome::Signaled { signal },
        (None, None) => panic!("a reaped process exited or was signaled"),
    }
}
