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
use std::time::Duration;

use crossbeam_channel::{Receiver, Sender};
use rustix::process::{Pid, Signal, WaitId, WaitIdOptions};

use crate::cargo;
use crate::config::{self, Config};
use crate::protocol::{BuildRequest, Message, Outcome, Request, Status};
use crate::scheduler::{Effect, JobId, Scheduler, Start, Submission, WaiterId};
use crate::slot::SlotDirectory;
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
    Output {
        job: JobId,
        message: Message,
    },
    Exited {
        job: JobId,
        outcome: Outcome,
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
        "buildd: listening on {} with {} slots and {} jobs",
        socket.display(),
        config.slots,
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
    Daemon {
        home: home.to_owned(),
        config,
        jobserver,
        events,
        scheduler: Scheduler::new(config.slots),
        waiters: HashMap::new(),
        runs: HashMap::new(),
    }
    .drive(&received)
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
    scheduler: Scheduler,
    waiters: HashMap<WaiterId, Sender<Message>>,
    runs: HashMap<JobId, Arc<Run>>,
}

impl Daemon {
    fn drive(mut self, received: &Receiver<Event>) -> ! {
        loop {
            let event = received
                .recv()
                .expect("the daemon holds a sender of its own events");
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
                    let (slots, queue) = self.scheduler.status();
                    let _ = reply.send(Status {
                        jobs: self.config.jobs,
                        idle_jobs: self
                            .jobserver
                            .available()
                            .expect("the daemon's own jobserver pipe can be queried"),
                        slots,
                        queue,
                    });
                    Vec::new()
                }
                Event::Output { job, message } => self.scheduler.output(job, message),
                Event::Exited { job, outcome } => {
                    self.runs.remove(&job);
                    self.scheduler.exited(job, &outcome)
                }
            };
            for effect in effects {
                self.apply(effect);
            }
        }
    }

    fn apply(&mut self, effect: Effect) {
        match effect {
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
                    let _ = self.events.send(Event::Exited { job, outcome });
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
        let outcome = self.outcome();
        let _ = self.events.send(Event::Exited {
            job: self.start.job,
            outcome,
        });
    }

    fn outcome(&self) -> Outcome {
        let failed = |reason: String| Outcome::Failed { reason };
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
        self.forward(stdout, true, done.clone());
        self.forward(stderr, false, done);

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
        signal_group(pid, Signal::KILL);
        self.run.lock().exited = true;
        let status = child.wait().expect("an exited child can be reaped");
        let deadline = std::time::Instant::now() + OUTPUT_GRACE;
        for _ in 0..2 {
            if finished.recv_deadline(deadline).is_err() {
                break;
            }
        }
        outcome(status)
    }

    /// Sends each line of `output` to the scheduler on a thread of its own.
    fn forward(&self, output: impl std::io::Read + Send + 'static, stdout: bool, done: Sender<()>) {
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
                    let message = if !stdout {
                        Message::Stderr { line }
                    } else if cargo::forwarded(&line) {
                        Message::Stdout { line }
                    } else {
                        continue;
                    };
                    if events.send(Event::Output { job, message }).is_err() {
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

fn outcome(status: ExitStatus) -> Outcome {
    use std::os::unix::process::ExitStatusExt as _;

    match (status.code(), status.signal()) {
        (Some(code), _) => Outcome::Exited { code },
        (None, Some(signal)) => Outcome::Signaled { signal },
        (None, None) => panic!("a reaped process exited or was signaled"),
    }
}
