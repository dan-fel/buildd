//! The daemon: accepts clients on a Unix socket, keeps the scheduler, and
//! runs builds in their slots under one job budget.

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

use crate::activity::{ActivityLog, EventFile};
use crate::budget::{Budget, Lease};
use crate::build_log::{self, BuildLog};
use crate::cargo;
use crate::cargo::Line;
use crate::cargo::{Compilation, Operation};
use crate::config::{self, Config, Remote};
use crate::distance::{GitDistance, Workspace};
use crate::failures;
use crate::git;
use crate::log::{gib, log};
use crate::memory;
use crate::passed::{self, PassedBinary};
use crate::products::{self, Product};
use crate::protocol::{
    Activity, BuildReport, BuildRequest, Capacity, EventKind, Failures, Message, Mirror, Outcome,
    RemoteActivity, Request, RevisionRequest, Status, TestMemory, Usage,
};
use crate::remote;
use crate::run::{self, RunDirectory};
use crate::scheduler::{
    Effect, JobId, Measurement, Removal, Scheduler, SlotKey, Start, Stop, Submission, WaiterId,
};
use crate::slot::{self, Inventory, SlotDirectory, SlotRecord, slot_name};
use crate::snapshot::{self, Source};

/// How long a cancelled build may take to stop after `SIGTERM` before its
/// process group is killed.
const TERMINATION_GRACE: Duration = Duration::from_secs(5);
/// How long output is still read after Cargo exits. Only a process that left
/// the build's process group can hold the output open longer; its remaining
/// output is abandoned.
const OUTPUT_GRACE: Duration = Duration::from_secs(2);
/// Space can disappear without a build finishing or a client asking for it.
const DISK_INTERVAL: Duration = Duration::from_secs(1);

/// What the scheduler thread hears about.
enum Event {
    Cache {
        operation: crate::cache::Operation,
        reply: Sender<crate::cache::Response>,
    },
    CacheCompleted {
        work: crate::cache::Work,
        result: Result<bool, crate::cache::Refusal>,
    },
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
    Capacity {
        repository: PathBuf,
        prefix: PathBuf,
        operation: Operation,
        reply: Sender<Capacity>,
    },
    Activity {
        reply: Sender<Activity>,
    },
    /// Stop taking builds, or take them again.
    Drain {
        drain: bool,
        reply: Sender<Status>,
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
    /// Cargo finished compiling for `job`; a test build runs its tests now.
    Testing {
        job: JobId,
    },
    /// Cargo succeeded for `job`; its executables are copied out now.
    Copying {
        job: JobId,
    },
    /// `job` ran every test: the binaries in `passed` passed, and `skipped`
    /// were not run because they had passed unchanged before.
    Tested {
        job: JobId,
        passed: Vec<PassedBinary>,
        skipped: u64,
    },
    /// Cargo ended, having used `usage` when it ran and left `report`.
    Exited {
        job: JobId,
        outcome: Outcome,
        usage: Option<Usage>,
        report: BuildReport,
    },
    /// Idle slots removed what the scheduler chose for them to give up;
    /// each did as its result says.
    Evicted {
        results: Vec<(SlotKey, Result<(), String>)>,
    },
    /// Slot `key` of `repository` was measured: its target holds
    /// `inventory`, and its checkout's `workspace` was read.
    Measured {
        key: SlotKey,
        repository: PathBuf,
        inventory: Result<Inventory, String>,
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
    let budget = Budget::new(config.jobs, config.test_jobs, &home.join("tmp"))?;
    // What commands left when an earlier daemon stopped: no run goes on.
    match std::fs::remove_dir_all(home.join("runs")) {
        Ok(()) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => {
            return Err(format!(
                "could not remove {}: {error}",
                home.join("runs").display()
            ));
        }
    }
    log!(
        "listening on {} with {} slots, {} of build disk and {} jobs",
        socket.display(),
        config.slots,
        gib(config.disk),
        config.jobs
    );

    let (events, received) = crossbeam_channel::unbounded();
    let (cache_work, cache_received) = crossbeam_channel::bounded::<crate::cache::Work>(1);
    let cache_events = events.clone();
    std::thread::Builder::new()
        .name("buildd-cache".into())
        .spawn(move || {
            while let Ok(work) = cache_received.recv() {
                let (work, result) = work.run();
                if cache_events
                    .send(Event::CacheCompleted { work, result })
                    .is_err()
                {
                    break;
                }
            }
        })
        .map_err(|error| format!("could not start the cache worker: {error}"))?;
    let accepting = Accepting {
        home: home.to_owned(),
        remotes: Arc::new(config.remotes.clone()),
        events: events.clone(),
        next_waiter: Arc::new(AtomicU64::new(0)),
        router: Arc::new(Mutex::new(crate::routing::Router::load(home)?)),
    };
    std::thread::Builder::new()
        .name("buildd-accept".into())
        .spawn(move || accepting.accept(&listener))
        .map_err(|error| format!("could not start accepting clients: {error}"))?;
    let mut scheduler = Scheduler::new(&config, GitDistance::default());
    restore_slots(home, &config, &mut scheduler)?;
    crate::prewarm::spawn(home, config.prewarm.clone());
    Daemon {
        home: home.to_owned(),
        cache: crate::cache::Cache::new(home)?,
        cache_work,
        config,
        budget,
        events,
        scheduler,
        waiters: HashMap::new(),
        runs: HashMap::new(),
        log: ActivityLog::new(SystemTime::now(), EventFile::new(home)),
        draining: false,
    }
    .drive(&received)
}

/// Takes back the slots an earlier daemon left in `home`: their checkouts,
/// targets and records stay warm across restarts. Slots beyond the
/// configured count, left from a larger configuration, are removed: no
/// build would use them again.
fn restore_slots(
    home: &Path,
    config: &Config,
    scheduler: &mut Scheduler<GitDistance>,
) -> Result<(), String> {
    for existing in slot::existing_slots(home)? {
        let name = slot_name(&existing.repository, existing.index);
        if existing.index >= config.slots {
            match existing.directory.remove() {
                Ok(()) => log!("removed slot {name}: beyond {} slots", config.slots),
                Err(error) => log!("could not remove slot {name}: {error}"),
            }
            continue;
        }
        let revision = match existing.directory.tree() {
            Ok(revision) => Some(revision),
            Err(error) => {
                log!("slot {name}: restoring cache with unavailable checkout: {error}");
                None
            }
        };
        // Cached artifacts still consume disk when their original repository
        // has disappeared. Only checkout availability affects placement;
        // every slot remains owned, inventoried and subject to retention.
        let record = existing.directory.read_record().unwrap_or_else(|error| {
            log!("slot {name}: {error}; restoring it without its record");
            SlotRecord::default()
        });
        log!(
            "restored slot {name} with {} compilations",
            record.compilations.len()
        );
        scheduler.restore(existing.repository, existing.index, revision, record);
    }
    Ok(())
}

struct Accepting {
    home: PathBuf,
    remotes: Arc<Vec<Remote>>,
    events: Sender<Event>,
    next_waiter: Arc<AtomicU64>,
    router: Arc<Mutex<crate::routing::Router>>,
}

impl Accepting {
    fn accept(&self, listener: &UnixListener) {
        for stream in listener.incoming() {
            let stream = match stream {
                Ok(stream) => stream,
                Err(error) => {
                    log!("could not accept a client: {error}");
                    continue;
                }
            };
            let connection = Connection {
                home: self.home.clone(),
                remotes: Arc::clone(&self.remotes),
                events: self.events.clone(),
                waiter: WaiterId(self.next_waiter.fetch_add(1, Ordering::Relaxed)),
                router: Arc::clone(&self.router),
            };
            let spawned = std::thread::Builder::new()
                .name("buildd-client".into())
                .spawn(move || connection.serve(stream));
            if let Err(error) = spawned {
                log!("could not serve a client: {error}");
            }
        }
    }
}

/// One client's connection.
struct Connection {
    home: PathBuf,
    remotes: Arc<Vec<Remote>>,
    events: Sender<Event>,
    waiter: WaiterId,
    router: Arc<Mutex<crate::routing::Router>>,
}

impl Connection {
    fn serve(self, mut stream: UnixStream) {
        let mut reader = match stream.try_clone() {
            Ok(reader) => BufReader::new(reader),
            Err(error) => {
                log!("could not read from a client: {error}");
                return;
            }
        };
        let mut line = String::new();
        if (&mut reader).take(1_048_577).read_line(&mut line).is_err()
            || line.len() > 1_048_576
            || !line.ends_with('\n')
        {
            return;
        }
        match serde_json::from_str::<Request>(&line) {
            Ok(Request::Cache { host, operation }) => {
                let response = if let Some(host) = host {
                    self.remotes
                        .iter()
                        .find(|remote| remote.name == host)
                        .ok_or_else(|| {
                            crate::client::CacheError::Rejected(format!(
                                "unknown cache host {host}"
                            ))
                        })
                        .and_then(|remote| remote::cache(&self.home, remote, operation, &stream))
                } else {
                    let (reply, response) = crossbeam_channel::bounded(1);
                    self.events
                        .send(Event::Cache { operation, reply })
                        .map_err(|_| crate::client::CacheError::Transport("daemon ended".into()))
                        .and_then(|()| {
                            response.recv().map_err(|_| {
                                crate::client::CacheError::Transport("daemon ended".into())
                            })
                        })
                };
                let _ = write_line(&mut stream, &response);
            }
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
            Ok(Request::Remotes) => {
                let _ = write_line(&mut stream, &self.remote_activity());
            }
            Ok(Request::Hosts) => {
                let hosts = self
                    .router
                    .lock()
                    .expect("router cannot panic")
                    .hosts(&self.remotes);
                let _ = write_line(&mut stream, &Ok::<_, String>(hosts));
            }
            Ok(Request::SetHost { name, enabled }) => {
                let hosts = self
                    .router
                    .lock()
                    .expect("router cannot panic")
                    .set_enabled(&self.home, &self.remotes, &name, enabled);
                let _ = write_line(&mut stream, &hosts);
            }
            Ok(Request::Capacity {
                project,
                prefix,
                operation,
            }) => {
                let capacity = operation
                    .validate()
                    .and_then(|()| mirror_path(&self.home, &project))
                    .and_then(|repository| self.capacity(repository, prefix, operation));
                let _ = write_line(&mut stream, &capacity);
            }
            Ok(Request::Failures { hours, os }) => {
                let failures = match os {
                    Some(os) if os != std::env::consts::OS => self
                        .remote_for(&os)
                        .and_then(|remote| remote::failures(&self.home, remote, hours)),
                    _ => Ok(self.failures(hours)),
                };
                let _ = write_line(&mut stream, &failures);
            }
            Ok(Request::Drain { drain }) => {
                let (reply, status) = crossbeam_channel::bounded(1);
                if self.events.send(Event::Drain { drain, reply }).is_ok()
                    && let Ok(status) = status.recv()
                {
                    let _ = write_line(&mut stream, &status);
                }
            }
            Ok(Request::Mirror { project }) => match mirror(&self.home, &project) {
                Ok(path) => {
                    let _ = write_line(&mut stream, &Mirror { path: path.clone() });
                    // After the answer: the push that asked need not wait.
                    if let Err(error) = maintain_mirror(&path) {
                        log!("mirror {project}: {error}");
                    }
                }
                Err(reason) => {
                    let _ = write_line(&mut stream, &Message::Rejected { reason });
                }
            },
            Ok(Request::Build(request)) => self.build(stream, reader, request),
            Ok(Request::BuildRevision(request)) => self.build_revision(stream, reader, request),
        }
    }

    fn capacity(
        &self,
        repository: PathBuf,
        prefix: PathBuf,
        operation: Operation,
    ) -> Result<Capacity, String> {
        let (reply, capacity) = crossbeam_channel::bounded(1);
        self.events
            .send(Event::Capacity {
                repository,
                prefix,
                operation,
                reply,
            })
            .map_err(|_| "daemon ended")?;
        capacity.recv().map_err(|_| "daemon ended".to_owned())
    }

    /// The remote host that builds for `os`.
    fn remote_for(&self, os: &str) -> Result<&Remote, String> {
        self.remotes
            .iter()
            .find(|remote| remote.os == os)
            .ok_or_else(|| format!("no remote host builds for {os}; see [[remote]] in config.toml"))
    }

    /// The builds that failed here in the last `hours`, from the event file.
    fn failures(&self, hours: u64) -> Failures {
        let since = SystemTime::now()
            .checked_sub(Duration::from_secs(hours.saturating_mul(3600)))
            .unwrap_or(SystemTime::UNIX_EPOCH);
        let since_ms = since
            .duration_since(SystemTime::UNIX_EPOCH)
            .expect("the clock is past 1970")
            .as_millis();
        failures::summarize(
            &EventFile::new(&self.home).read(),
            u64::try_from(since_ms).expect("milliseconds since 1970 fit u64"),
            self.home.join("logs"),
        )
    }

    /// What each remote host's daemon does, asked in parallel.
    fn remote_activity(&self) -> Vec<RemoteActivity> {
        std::thread::scope(|scope| {
            let asked = self
                .remotes
                .iter()
                .map(|remote| scope.spawn(|| remote::activity(&self.home, remote)))
                .collect::<Vec<_>>();
            self.remotes
                .iter()
                .zip(asked)
                .map(|(remote, asked)| RemoteActivity {
                    name: remote.name.clone(),
                    os: remote.os.clone(),
                    activity: asked
                        .join()
                        .expect("asking a remote does not panic")
                        .map(Box::new),
                })
                .collect()
        })
    }

    /// Builds a tree another daemon pushed into this daemon's mirror.
    fn build_revision(
        self,
        mut stream: UnixStream,
        reader: BufReader<UnixStream>,
        request: RevisionRequest,
    ) {
        let prepared = request.operation.validate().and_then(|()| {
            if !self
                .router
                .lock()
                .expect("router cannot panic")
                .enabled("local")
            {
                return Err("local build host is disabled".into());
            }
            if let Some(directory) = &request.copy_to {
                copy_destination_here(directory, &self.home)?;
            }
            let repository = existing_mirror(&self.home, &request.project)?;
            if request.prefix.is_absolute()
                || request
                    .prefix
                    .components()
                    .any(|part| part == std::path::Component::ParentDir)
            {
                return Err(format!(
                    "the directory to build in is outside the tree: {}",
                    request.prefix.display()
                ));
            }
            let mut exists = git::command(&repository);
            exists
                .arg("--git-dir")
                .arg(&repository)
                .args(["cat-file", "-e"])
                .arg(format!("{}^{{tree}}", request.revision));
            git::run(exists)
                .map_err(|_| format!("tree {} was not pushed", request.revision.short()))?;
            Ok(Source {
                repository,
                worktree: request.worktree.clone(),
                prefix: request.prefix.clone(),
                index: PathBuf::new(),
            })
        });
        let source = match prepared {
            Ok(source) => source,
            Err(reason) => {
                let _ = write_line(&mut stream, &Message::Rejected { reason });
                return;
            }
        };
        let submission = Submission {
            waiter: self.waiter,
            source,
            revision: request.revision,
            operation: request.operation,
            label: request.label,
            copy_to: request.copy_to,
            rerun_all: request.rerun_all,
            optional: request.optional,
        };
        self.submit(stream, reader, submission);
    }

    /// Forwards the already captured tree to its selected host.
    fn build_remotely(
        self,
        mut stream: UnixStream,
        reader: BufReader<UnixStream>,
        request: BuildRequest,
        remote: &Remote,
        source: Source,
        revision: snapshot::Revision,
    ) {
        let label = request.label.or_else(|| {
            source
                .worktree
                .file_name()
                .map(|name| name.to_string_lossy().into_owned())
        });
        let revision_request = RevisionRequest {
            project: String::new(),
            revision: revision.clone(),
            worktree: source.worktree.clone(),
            prefix: source.prefix.clone(),
            operation: request.operation,
            label,
            copy_to: request.copy_to,
            rerun_all: request.rerun_all,
            optional: request.optional,
        };
        let pushing = format!(
            "buildd: pushing tree {} to {} ({})",
            revision.short(),
            remote.name,
            remote.ssh
        );
        let _ = write_line(&mut stream, &Message::Stderr { line: pushing });
        let mut writer = match stream.try_clone() {
            Ok(writer) => writer,
            Err(error) => {
                log!("could not write to a client: {error}");
                return;
            }
        };
        let built = remote::build(
            remote,
            &source,
            &revision,
            revision_request,
            &stream,
            |child| {
                // The end of the client's stream withdraws the build there.
                let child = Arc::clone(child);
                let mut reader = reader
                    .get_ref()
                    .try_clone()
                    .expect("client stream was cloned");
                let _ = std::thread::Builder::new()
                    .name("buildd-client-watch".into())
                    .spawn(move || {
                        let mut buffer = [0; 256];
                        while matches!(reader.read(&mut buffer), Ok(read) if read > 0) {}
                        let mut child = child.lock().expect("child cannot panic");
                        if matches!(child.try_wait(), Ok(None)) {
                            let pid = Pid::from_raw(
                                i32::try_from(child.id()).expect("process ID fits i32"),
                            )
                            .expect("nonzero PID");
                            let _ = rustix::process::kill_process_group(pid, Signal::KILL);
                        }
                    });
            },
            |message| write_line(&mut writer, message).is_ok(),
        );
        if let Err(reason) = built {
            let _ = write_line(&mut stream, &Message::Rejected { reason });
        }
        let _ = stream.shutdown(std::net::Shutdown::Both);
    }

    fn build(self, mut stream: UnixStream, reader: BufReader<UnixStream>, request: BuildRequest) {
        let prepared = request.operation.validate().and_then(|()| {
            if let Some(directory) = &request.copy_to {
                copy_destination(directory)?;
            }
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
        let hosts = self
            .router
            .lock()
            .expect("router cannot panic")
            .hosts(&self.remotes);
        if let Some(name) = &request.host
            && !hosts.iter().any(|host| host.name == *name)
        {
            let _ = write_line(
                &mut stream,
                &Message::Rejected {
                    reason: format!("unknown build host {name}"),
                },
            );
            return;
        }
        let project = slot::project_name(&source.repository);
        let observations = std::thread::scope(|scope| {
            let asked = hosts
                .into_iter()
                .filter(|host| request.accepts_host(host))
                .map(|host| {
                    let project = &project;
                    let source = &source;
                    let operation = &request.operation;
                    let connection = &self;
                    scope.spawn(move || {
                        let capacity = if host.name == "local" {
                            connection.capacity(
                                source.repository.clone(),
                                source.prefix.clone(),
                                operation.clone(),
                            )
                        } else {
                            let remote = connection
                                .remotes
                                .iter()
                                .find(|remote| remote.name == host.name)
                                .expect("configured host");
                            remote::capacity(
                                remote,
                                project.clone(),
                                source.prefix.clone(),
                                operation.clone(),
                            )
                        };
                        (host, capacity)
                    })
                })
                .collect::<Vec<_>>();
            asked
                .into_iter()
                .map(|asked| asked.join().expect("host probe cannot panic"))
                .collect::<Vec<_>>()
        });
        let placement = match self.router.lock().expect("router cannot panic").place(
            &source,
            &revision,
            &request,
            &observations,
        ) {
            Ok(placement) => placement,
            Err(reason) => {
                let _ = write_line(&mut stream, &Message::Rejected { reason });
                return;
            }
        };
        if placement.host != "local" {
            let remote = self
                .remotes
                .iter()
                .find(|remote| remote.name == placement.host)
                .expect("placed on configured host")
                .clone();
            self.build_remotely(stream, reader, request, &remote, source, revision);
            return;
        }
        if let Some(directory) = &request.copy_to
            && let Err(reason) = copy_destination_here(directory, &self.home)
        {
            let _ = write_line(&mut stream, &Message::Rejected { reason });
            return;
        }
        let submission = Submission {
            waiter: self.waiter,
            source,
            revision,
            operation: request.operation,
            label: request.label,
            copy_to: request.copy_to,
            rerun_all: request.rerun_all,
            optional: request.optional,
        };
        self.submit(stream, reader, submission);
    }

    /// Hands `submission` to the scheduler and passes its messages to the
    /// client until the last one; the end of the client's stream withdraws
    /// it.
    fn submit(
        self,
        mut stream: UnixStream,
        mut reader: BufReader<UnixStream>,
        submission: Submission,
    ) {
        let (messages, received) = crossbeam_channel::unbounded();
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

/// Checks a directory a request names to copy products into: an absolute
/// path without `..`, which means the same on every machine.
fn copy_destination(directory: &Path) -> Result<(), String> {
    if !directory.is_absolute()
        || directory
            .components()
            .any(|part| part == std::path::Component::ParentDir)
    {
        return Err(format!(
            "the directory to copy to must be absolute, without `..`: {}",
            directory.display()
        ));
    }
    Ok(())
}

/// Checks a directory on this machine to copy products into, as
/// [`copy_destination`] does, and that it is outside buildd's `home`, whose
/// slots no copy may write into.
fn copy_destination_here(directory: &Path, home: &Path) -> Result<(), String> {
    copy_destination(directory)?;
    if directory.starts_with(home) {
        return Err(format!(
            "the directory to copy to is inside buildd's home {}: {}",
            home.display(),
            directory.display()
        ));
    }
    Ok(())
}

/// How many pushed trees a mirror keeps referenced, newest first: enough for
/// the next push to send only what changed.
const MIRROR_REFS: usize = 20;

/// The path of `project`'s mirror under `home`, after checking the name.
fn mirror_path(home: &Path, project: &str) -> Result<PathBuf, String> {
    let valid = !project.is_empty()
        && !project.starts_with('.')
        && project
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.'));
    if !valid {
        return Err(format!("not a project name: {project:?}"));
    }
    Ok(home.join("mirrors").join(project))
}

/// `project`'s mirror, which must exist.
fn existing_mirror(home: &Path, project: &str) -> Result<PathBuf, String> {
    let path = mirror_path(home, project)?;
    std::fs::canonicalize(&path).map_err(|_| format!("no mirror of {project}: push to it first"))
}

/// `project`'s mirror under `home`, created when absent, with only its
/// newest [`MIRROR_REFS`] pushed trees still referenced.
fn mirror(home: &Path, project: &str) -> Result<PathBuf, String> {
    let path = mirror_path(home, project)?;
    if !path.exists() {
        std::fs::create_dir_all(&path)
            .map_err(|error| format!("could not create {}: {error}", path.display()))?;
        let mut init = git::command(&path);
        init.args(["init", "--bare", "-q", "--template="]);
        git::run(init)?;
        // Loose refs keep their push times, which pruning goes by: no
        // packing after each push. [`maintain_mirror`] packs objects.
        let mut config = git::command(&path);
        config.args(["config", "gc.auto", "0"]);
        git::run(config)?;
    }
    let refs = path.join("refs/buildd");
    if let Ok(entries) = std::fs::read_dir(&refs) {
        let mut pushed = entries
            .filter_map(Result::ok)
            .filter_map(|entry| {
                let modified = entry.metadata().ok()?.modified().ok()?;
                Some((modified, entry.file_name()))
            })
            .collect::<Vec<_>>();
        pushed.sort_unstable_by(|a, b| b.cmp(a));
        for (_, name) in pushed.into_iter().skip(MIRROR_REFS) {
            let mut delete = git::command(&path);
            delete
                .args(["update-ref", "-d"])
                .arg(format!("refs/buildd/{}", name.to_string_lossy()));
            git::run(delete)?;
        }
    }
    std::fs::canonicalize(&path).map_err(|error| format!("{}: {error}", path.display()))
}

/// Lets git pack `mirror` once pushes have left many loose objects or packs
/// in it, dropping the objects no reference keeps; objects of the last hour
/// stay, for a push still arriving. Its references stay loose: [`mirror`]
/// prunes them by their push times. Git's lock keeps it to one at a time.
fn maintain_mirror(mirror: &Path) -> Result<(), String> {
    let mut gc = git::command(mirror);
    gc.args([
        "-c",
        "gc.auto=6700",
        "-c",
        "gc.autoDetach=false",
        "-c",
        "gc.packRefs=false",
        "-c",
        "gc.pruneExpire=1.hour.ago",
        "gc",
        "--auto",
        "--quiet",
    ]);
    git::run(gc).map(drop)
}

fn write_line(stream: &mut UnixStream, value: &impl serde::Serialize) -> std::io::Result<()> {
    let mut text = serde_json::to_string(value).expect("protocol values serialize");
    text.push('\n');
    stream.write_all(text.as_bytes())
}

struct Daemon {
    cache: crate::cache::Cache,
    cache_work: Sender<crate::cache::Work>,
    home: PathBuf,
    config: Config,
    budget: Budget,
    events: Sender<Event>,
    scheduler: Scheduler<GitDistance>,
    waiters: HashMap<WaiterId, Sender<Message>>,
    runs: HashMap<JobId, Arc<Run>>,
    log: ActivityLog,
    /// It takes no new builds.
    draining: bool,
}

impl Daemon {
    fn drive(mut self, received: &Receiver<Event>) -> ! {
        self.scheduler
            .observe_disk(slot::free_disk(&self.home).ok());
        for effect in self.scheduler.maintain() {
            self.apply(effect);
        }
        let mut next_disk = Instant::now() + DISK_INTERVAL;
        loop {
            self.cache.expire(Instant::now());
            let due = self
                .scheduler
                .next_measurement()
                .into_iter()
                .chain(self.cache.next_expiry())
                .chain(std::iter::once(next_disk))
                .min()
                .expect("the disk timer always sets a deadline");
            let event = match received.recv_deadline(due) {
                Ok(event) => event,
                Err(RecvTimeoutError::Timeout) => {
                    for effect in self.scheduler.measurement_due(Instant::now()) {
                        self.apply(effect);
                    }
                    self.scheduler
                        .observe_disk(slot::free_disk(&self.home).ok());
                    for effect in self.scheduler.maintain() {
                        self.apply(effect);
                    }
                    next_disk = Instant::now() + DISK_INTERVAL;
                    continue;
                }
                Err(RecvTimeoutError::Disconnected) => {
                    panic!("the daemon holds a sender of its own events")
                }
            };
            self.scheduler
                .observe_disk(slot::free_disk(&self.home).ok());
            let effects = match event {
                Event::Cache { operation, reply } => {
                    let (response, effects) =
                        self.cache
                            .operate(&self.home, &mut self.scheduler, operation);
                    let _ = reply.send(response);
                    effects
                }
                Event::CacheCompleted { work, result } => {
                    self.cache
                        .completed(&self.home, &mut self.scheduler, work, result)
                }
                Event::Submit {
                    submission,
                    messages,
                } => {
                    if self.draining {
                        let reason = "this daemon is draining: it takes no new builds".to_owned();
                        let _ = messages.send(Message::Rejected { reason });
                        continue;
                    }
                    self.waiters.insert(submission.waiter, messages);
                    self.scheduler.submit(submission)
                }
                Event::Drain { drain, reply } => {
                    if drain != self.draining {
                        log!(
                            "{}",
                            if drain {
                                "draining"
                            } else {
                                "taking builds again"
                            }
                        );
                    }
                    self.draining = drain;
                    let _ = reply.send(self.status());
                    Vec::new()
                }
                Event::Withdraw { waiter } => {
                    self.waiters.remove(&waiter);
                    self.scheduler.withdraw(waiter)
                }
                Event::Status { reply } => {
                    let _ = reply.send(self.status());
                    Vec::new()
                }
                Event::Capacity {
                    repository,
                    prefix,
                    operation,
                    reply,
                } => {
                    let peak_memory =
                        self.scheduler
                            .forecast_memory(&repository, &prefix, &operation);
                    let _ = reply.send(Capacity {
                        status: self.status(),
                        peak_memory,
                    });
                    Vec::new()
                }
                Event::Activity { reply } => {
                    let status = self.status();
                    let _ = reply.send(self.log.activity(status));
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
                Event::Testing { job } => {
                    if let Some(run) = self.runs.get(&job) {
                        run.testing();
                    }
                    self.scheduler.testing(job)
                }
                Event::Copying { job } => {
                    self.scheduler.copying(job);
                    Vec::new()
                }
                Event::Tested {
                    job,
                    passed,
                    skipped,
                } => {
                    self.scheduler.tested(job, passed, skipped);
                    Vec::new()
                }
                Event::Exited {
                    job,
                    outcome,
                    usage,
                    report,
                } => {
                    self.runs.remove(&job);
                    let effects = self.scheduler.exited(job, &outcome, usage, report);
                    for effect in effects {
                        self.apply(effect);
                    }
                    Vec::new()
                }
                Event::Evicted { results } => {
                    for effect in self.scheduler.evicted(results) {
                        self.apply(effect);
                    }
                    Vec::new()
                }
                Event::Measured {
                    key,
                    repository,
                    inventory,
                    workspace,
                } => {
                    if let Some(workspace) = workspace {
                        self.scheduler.distance_mut().learn(repository, workspace);
                    }
                    for effect in self.scheduler.measured(key, inventory) {
                        self.apply(effect);
                    }
                    Vec::new()
                }
            };
            for effect in effects {
                self.apply(effect);
            }
            self.scheduler
                .observe_disk(slot::free_disk(&self.home).ok());
            for effect in self.scheduler.maintain() {
                self.apply(effect);
            }
            if let Some(work) = self.cache.take_work() {
                self.cache_work
                    .try_send(work)
                    .expect("one live cache worker owns its work");
            }
        }
    }

    fn status(&self) -> Status {
        let (slots, queue) = self
            .scheduler
            .status(|job| self.runs.get(&job).map_or(0, |run| run.in_use()));
        let (jobs, idle_jobs) = self.budget.jobs();
        let (memory, memory_in_use) = self.scheduler.memory();
        Status {
            jobs,
            idle_jobs,
            capacity: self.config.slots,
            disk_budget: self.config.disk,
            disk_used: self.scheduler.disk_used(),
            free_disk: self.scheduler.disk_space(),
            min_free: self.config.min_free,
            build_headroom: self.config.build_headroom,
            disk_wait: self.scheduler.disk_wait().cloned(),
            undersized: self.scheduler.undersized(),
            memory,
            memory_in_use,
            draining: self.draining,
            slots,
            queue,
        }
    }

    fn apply(&mut self, effect: Effect) {
        match effect {
            Effect::Report(kind) => {
                if let EventKind::Evicted { .. } = &kind {
                    log_eviction(&kind, self.config.disk, self.config.min_free);
                }
                self.log.record(SystemTime::now(), kind);
            }
            Effect::Persist {
                repository,
                slot,
                record,
            } => {
                let directory = SlotDirectory::new(&self.home, &repository, slot);
                if let Err(error) = directory.write_record(&record) {
                    log!("{error}");
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
                let workspace = self
                    .scheduler
                    .distance_mut()
                    .workspace(&start.repository)
                    .cloned();
                let logs = self.home.join("logs");
                let log = BuildLog::create(&logs, log_name(start.job), &log_header(&start));
                let building = Building {
                    slot: SlotDirectory::new(&self.home, &start.repository, start.slot),
                    start,
                    log: Arc::new(Mutex::new(Some(log))),
                    logs,
                    runs: self.home.join("runs"),
                    budget: self.budget.clone(),
                    test_jobs: self.config.test_jobs,
                    workspace,
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
                        report: BuildReport::default(),
                    });
                }
            }
            Effect::Measure(Measurement {
                key,
                repository,
                slot,
            }) => {
                let directory = SlotDirectory::new(&self.home, &repository, slot);
                let name = slot_name(&repository, slot);
                let events = self.events.clone();
                let spawned = std::thread::Builder::new()
                    .name("buildd-measure".into())
                    .spawn(move || {
                        let inventory = directory.inventory().inspect_err(|error| {
                            log!("slot {name} could not be measured: {error}");
                        });
                        // Knowing the packages lets slot choice weigh changes
                        // by what Cargo compiles again.
                        let workspace =
                            Workspace::read(&directory.source()).unwrap_or_else(|error| {
                                log!("slot {name}: {error}");
                                None
                            });
                        let _ = events.send(Event::Measured {
                            key,
                            repository,
                            inventory,
                            workspace,
                        });
                    });
                if let Err(error) = spawned {
                    let error = format!("could not start measuring a slot: {error}");
                    log!("{error}");
                    let _ = self.events.send(Event::Measured {
                        key,
                        repository: PathBuf::new(),
                        inventory: Err(error),
                        workspace: None,
                    });
                }
            }
            Effect::Evict(removals) => {
                let keys = removals
                    .iter()
                    .map(|removal| removal.key)
                    .collect::<Vec<_>>();
                let events = self.events.clone();
                let spawned = std::thread::Builder::new()
                    .name("buildd-evict".into())
                    .spawn(move || {
                        let results = removals
                            .into_iter()
                            .map(|Removal { key, name, paths }| {
                                let removed = slot::remove(&paths);
                                if let Err(error) = &removed {
                                    log!("slot {name} could not give up what it was to: {error}");
                                }
                                (key, removed)
                            })
                            .collect();
                        let _ = events.send(Event::Evicted { results });
                    });
                if let Err(error) = spawned {
                    log!("could not start evicting: {error}");
                    let results = keys
                        .into_iter()
                        .map(|key| (key, Err(error.to_string())))
                        .collect();
                    let _ = self.events.send(Event::Evicted { results });
                }
            }
            Effect::Cancel { job, reason } => {
                if let Some(run) = self.runs.get(&job) {
                    run.cancel(reason);
                }
            }
        }
    }
}

/// Writes what an eviction did to the daemon's log.
fn log_eviction(kind: &EventKind, budget: u64, floor: u64) {
    let EventKind::Evicted {
        shortage,
        freed,
        caches,
        units,
        test_files,
        in_use,
        slots,
    } = kind
    else {
        panic!("only evictions are logged as evictions");
    };
    let why = match (shortage.over_budget, shortage.under_floor) {
        (0, under) => format!(
            "{} short of free-space reservations (floor {})",
            gib(under),
            gib(floor)
        ),
        (over, 0) => format!("{} over the budget of {}", gib(over), gib(budget)),
        (over, under) => format!(
            "{} over the budget of {} and {} short of free-space reservations (floor {})",
            gib(over),
            gib(budget),
            gib(under),
            gib(floor)
        ),
    };
    log!(
        "disk: {why}: {} gave up {caches} incremental caches, {units} compiled units \
         and {test_files} test files ({in_use} in use), {}",
        slots.join(", "),
        gib(*freed)
    );
    if *in_use > 0 {
        let remedy = if shortage.over_budget > 0 {
            "raise disk_gib or lower slots"
        } else {
            "free disk on the volume or reduce build concurrency"
        };
        log!(
            "disk: builds lost caches and units they used in the last ten minutes, so \
             they compile from scratch: {remedy}"
        );
    }
}

/// A running build's process, as cancellation sees it.
#[derive(Default)]
struct Run(Mutex<RunState>);

#[derive(Default)]
struct RunState {
    stopped: Option<Stop>,
    /// The process group of Cargo or the command, once it runs.
    group: Option<Pid>,
    /// Its leader has exited; its group id may be reused.
    exited: bool,
    /// Its account in the job budget, once it has one.
    lease: Option<Arc<Lease>>,
}

impl Run {
    fn lock(&self) -> std::sync::MutexGuard<'_, RunState> {
        self.0
            .lock()
            .expect("no thread panics holding a run's state")
    }

    /// The build's tests run: it is charged their share of the machine.
    fn testing(&self) {
        if let Some(lease) = &self.lock().lease {
            lease.fixed_share();
        }
    }

    /// The jobs the build uses now.
    fn in_use(&self) -> usize {
        self.lock().lease.as_ref().map_or(0, |lease| lease.in_use())
    }

    /// Client withdrawal sends `SIGTERM`, then `SIGKILL` after a grace
    /// period. Resource failures stop writes immediately with `SIGKILL`.
    fn cancel(self: &Arc<Self>, reason: Stop) {
        let mut state = self.lock();
        if state.stopped.is_some() {
            return;
        }
        let graceful = matches!(reason, Stop::Withdrawn);
        state.stopped = Some(reason);
        let Some(group) = state.group.filter(|_| !state.exited) else {
            return;
        };
        signal_group(group, if graceful { Signal::TERM } else { Signal::KILL });
        if !graceful {
            return;
        }
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
        Err(error) => log!("could not signal process group {group:?}: {error}"),
    }
}

/// Kills what remains of process group `group`, whose leader (Cargo or the
/// command) has exited but is not reaped yet: processes it started and left
/// behind.
fn kill_leftovers(group: Pid) {
    match rustix::process::kill_process_group(group, Signal::KILL) {
        // No members left, or (macOS) only the unreaped leader itself, which
        // a signal cannot reach.
        Ok(()) | Err(rustix::io::Errno::SRCH | rustix::io::Errno::PERM) => {}
        Err(error) => log!("could not kill process group {group:?}: {error}"),
    }
}

/// A new build log's name: when the build started, then its job, so names
/// sort by age.
fn log_name(job: JobId) -> String {
    let started = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .expect("the clock is past 1970")
        .as_millis();
    format!("{started}-{job}.log")
}

fn log_header(start: &Start) -> String {
    format!(
        "# buildd {} for {}\n# tree {} in slot {}, directory ./{}\n# started {}\n",
        start.operation,
        start.who.join(", "),
        start.revision,
        slot_name(&start.repository, start.slot),
        start.prefix.display(),
        crate::log::local_time(SystemTime::now()),
    )
}

/// A build on its own thread.
struct Building {
    start: Start,
    /// Its log until the build ends; output read after that is not logged.
    log: Arc<Mutex<Option<BuildLog>>>,
    /// Where build logs are kept.
    logs: PathBuf,
    /// Where `run` commands get directories of their own.
    runs: PathBuf,
    slot: SlotDirectory,
    budget: Budget,
    /// Threads a test build's tests run on, and the cores a `run` command
    /// may keep busy.
    test_jobs: usize,
    /// The repository's workspace, once known: what decides which test
    /// binaries a run of every test may skip.
    workspace: Option<Workspace>,
    run: Arc<Run>,
    events: Sender<Event>,
}

impl Building {
    fn build(self) {
        let (outcome, usage) = self.outcome();
        let outcome = match self.run.lock().stopped.as_ref() {
            Some(Stop::Failed(reason)) => Outcome::Failed {
                reason: reason.clone(),
            },
            _ => outcome,
        };
        let log = self
            .log
            .lock()
            .expect("no thread panics holding a build's log")
            .take();
        let mut log = log.expect("a build's log ends once, here");
        if let Outcome::Failed { reason } = &outcome {
            log.stderr(&format!("buildd: {reason}"));
        }
        let report = log.finish();
        build_log::prune(&self.logs, build_log::LOG_LIMIT);
        let _ = self.events.send(Event::Exited {
            job: self.start.job,
            outcome,
            usage,
            report,
        });
    }

    /// How the build ended and, when Cargo or the command ran, what it
    /// used.
    fn outcome(&self) -> (Outcome, Option<Usage>) {
        let failed = |reason: String| (Outcome::Failed { reason }, None);
        if let Err(reason) = self
            .slot
            .materialize(&self.start.repository, &self.start.revision)
        {
            return failed(reason);
        }
        // Cargo runs on its implicit job and takes the rest from the build's
        // account as it goes.
        let lease = match self.budget.open() {
            Ok(lease) => Arc::new(lease),
            Err(reason) => return failed(reason),
        };
        self.run.lock().lease = Some(Arc::clone(&lease));
        let operation = &self.start.operation;
        if !operation.command.uses_cargo() {
            lease.fixed_share();
            return self.run_command();
        }
        let skipping = match &self.workspace {
            Some(workspace) if !self.start.passed.is_empty() => Some(workspace),
            _ => None,
        };
        let (step, usage, products, skipped) = match skipping {
            Some(workspace) => match self.run_unpassed(&lease, workspace) {
                Ok(ran) => ran,
                Err(outcome) => return (outcome, None),
            },
            None => match self.cargo(&lease, operation) {
                Ok((status, usage, products)) => (outcome(status), usage, products, Vec::new()),
                Err(reason) => return failed(reason),
            },
        };
        if step.success() && operation.runs_every_test() {
            let checkout = self.slot.source();
            let passed = passed::test_binaries(&products, &checkout)
                .iter()
                .filter(|binary| !skipped.contains(&binary.id))
                .filter_map(|binary| binary.passed(&self.start.revision))
                .collect();
            let _ = self.events.send(Event::Tested {
                job: self.start.job,
                passed,
                skipped: skipped.len() as u64,
            });
        }
        let outcome = match &self.start.copy_to {
            Some(directory) if step.success() => self.copy(&products, directory),
            _ => step,
        };
        (outcome, Some(usage))
    }

    /// A `nextest` run of every test that skips the test binaries unchanged
    /// since they passed: compiles the tests first to learn which binaries
    /// changed, then runs nextest for the rest. Returns how the build
    /// ended, what it used, Cargo's executables and the ids skipped.
    fn run_unpassed(
        &self,
        lease: &Lease,
        workspace: &Workspace,
    ) -> Result<(Outcome, Usage, Vec<Product>, Vec<String>), Outcome> {
        let failed = |reason| Outcome::Failed { reason };
        let compile = Operation {
            command: cargo::Command::Test,
            args: Compilation::new(&self.start.prefix, &self.start.operation)
                .args
                .into_iter()
                .chain(["--no-run".to_owned()])
                .collect(),
            rustflags: self.start.operation.rustflags.clone(),
        };
        let (status, compiled, products) = self.cargo(lease, &compile).map_err(failed)?;
        if !status.success() {
            return Ok((outcome(status), compiled, products, Vec::new()));
        }
        let checkout = self.slot.source();
        let binaries = passed::test_binaries(&products, &checkout);
        let repository = &self.start.repository;
        let skipped = passed::skippable(
            &binaries,
            &self.start.passed,
            &self.start.revision,
            workspace,
            |earlier| git::changed_paths(repository, earlier, &self.start.revision),
        );
        let ids = skipped
            .iter()
            .map(|binary| binary.id.clone())
            .collect::<Vec<_>>();
        self.tell(format!(
            "buildd: {} of {} test binaries are unchanged since they passed; \
             running the rest (--rerun-all runs them too)",
            skipped.len(),
            binaries.len()
        ));
        if skipped.len() == binaries.len() {
            return Ok((Outcome::Exited { code: 0 }, compiled, products, ids));
        }
        let mut run = self.start.operation.clone();
        if !skipped.is_empty() {
            run.args.extend([
                "--no-tests=pass".to_owned(),
                "-E".to_owned(),
                passed::excluding(&skipped),
            ]);
        }
        let (status, mut usage, _) = self.cargo(lease, &run).map_err(failed)?;
        usage.cpu_ms += compiled.cpu_ms;
        usage.peak_memory = usage.peak_memory.max(compiled.peak_memory);
        Ok((outcome(status), usage, products, ids))
    }

    /// Tells the build's waiters `line`, as standard error.
    fn tell(&self, line: String) {
        log_line(&self.log, &line, BuildLog::stderr);
        let _ = self.events.send(Event::Output {
            job: self.start.job,
            message: Message::Stderr { line },
        });
    }

    /// Runs Cargo for `operation` in the slot as a client of `lease`,
    /// forwarding its output; returns how it exited, what it used and the
    /// executables it reported.
    fn cargo(
        &self,
        lease: &Lease,
        operation: &Operation,
    ) -> Result<(ExitStatus, Usage, Vec<Product>), String> {
        let target = self.slot.target();
        let mut command = cargo::command(
            &self.slot.source().join(&self.start.prefix),
            &target,
            operation,
            self.test_jobs,
        );
        lease.configure(&mut command);
        let products = Arc::new(Mutex::new(Vec::new()));
        let (status, usage) = self.execute(
            command,
            Stream::Cargo {
                target,
                products: Arc::clone(&products),
                tests: operation.command.runs_tests()
                    && !operation
                        .args
                        .iter()
                        .take_while(|arg| arg.as_str() != "--")
                        .any(|arg| arg == "--no-run"),
            },
        )?;
        let products = std::mem::take(
            &mut *products
                .lock()
                .expect("no thread panics holding a build's products"),
        );
        Ok((status, usage, products))
    }

    /// Runs the client's own command in the slot, on the build's fixed
    /// share of the machine, as [`crate::run`] explains: forwards its
    /// output, puts the slot's checkout back to the build's tree, and moves
    /// its outputs where the request asked once it succeeds.
    fn run_command(&self) -> (Outcome, Option<Usage>) {
        let failed = |reason: String| (Outcome::Failed { reason }, None);
        let directory = self.slot.source().join(&self.start.prefix);
        if !directory.is_dir() {
            return failed(format!(
                "tree {} has no directory ./{} to run in",
                self.start.revision.short(),
                self.start.prefix.display()
            ));
        }
        let own = match RunDirectory::create(&self.runs, &self.start.job.to_string()) {
            Ok(own) => own,
            Err(reason) => return failed(reason),
        };
        let command = run::command(
            &directory,
            &self.start.operation,
            &self.start.revision,
            &own,
            self.test_jobs,
        );
        let ran = self.execute(command, Stream::Stdout);
        // Whatever the command changed in the checkout, the slot holds the
        // tree it names again. A failure here leaves that to the next
        // build's checkout, which makes the same reset.
        if let Err(error) = self
            .slot
            .materialize(&self.start.repository, &self.start.revision)
        {
            log!("{}: {error}", self.slot.path().display());
        }
        let (status, usage) = match ran {
            Ok(ran) => ran,
            Err(reason) => return failed(reason),
        };
        let step = outcome(status);
        let outcome = match &self.start.copy_to {
            Some(destination) if step.success() => self.deliver(&own.out(), destination),
            _ => step,
        };
        (outcome, Some(usage))
    }

    /// Starts `command`, which runs in a process group of its own, and
    /// forwards its standard output as `stdout` says and its standard error
    /// until it exits; a cancellation stops the whole group. Returns how it
    /// exited and what it used.
    fn execute(
        &self,
        mut command: std::process::Command,
        stdout: Stream,
    ) -> Result<(ExitStatus, Usage), String> {
        let program = command.get_program().to_string_lossy().into_owned();
        let mut child = {
            let mut state = self.run.lock();
            if let Some(reason) = &state.stopped {
                return Err(match reason {
                    Stop::Withdrawn => format!("cancelled before {program} started"),
                    Stop::Failed(reason) => reason.clone(),
                });
            }
            let child = command
                .spawn()
                .map_err(|error| format!("could not start {program}: {error}"))?;
            state.group = Some(Pid::from_child(&child));
            state.exited = false;
            child
        };
        let pid = Pid::from_child(&child);
        let monitor = match memory::Monitor::start(child.id()) {
            Ok(monitor) => monitor,
            Err(error) => {
                kill_leftovers(pid);
                self.run.lock().exited = true;
                reap(&child);
                return Err(format!("could not start memory sampling: {error}"));
            }
        };
        let (done, finished) = crossbeam_channel::bounded(2);
        let output = child.stdout.take().expect("the process's output is piped");
        let errors = child.stderr.take().expect("the process's errors are piped");
        self.forward(output, stdout, done.clone(), Some(monitor.events.clone()));
        self.forward(errors, Stream::Stderr, done, None);
        // Wait without reaping, so the process group id cannot be reused
        // while the group's remaining processes are killed and before a
        // pending cancellation learns that the process exited.
        loop {
            match rustix::process::waitid(
                WaitId::Pid(pid),
                WaitIdOptions::EXITED | WaitIdOptions::NOWAIT,
            ) {
                Err(rustix::io::Errno::INTR) => {}
                Ok(_) => break,
                Err(error) => {
                    panic!("waiting for our own child fails only by a bug: {error}")
                }
            }
        }
        let (peak, test_peak) = monitor.finish();
        kill_leftovers(pid);
        self.run.lock().exited = true;
        let (status, mut usage) = reap(&child);
        usage.peak_memory = usage.peak_memory.max(peak);
        usage.test_memory = test_peak.map(|peak_memory| TestMemory {
            jobs: self.test_jobs,
            peak_memory,
        });
        let deadline = std::time::Instant::now() + OUTPUT_GRACE;
        for _ in 0..2 {
            if finished.recv_deadline(deadline).is_err() {
                break;
            }
        }
        Ok((status, usage))
    }

    /// Copies `products` into `directory` and tells the build's waiters
    /// where they went; how the build failed when copying did.
    fn copy(&self, products: &[Product], directory: &Path) -> Outcome {
        let job = self.start.job;
        let _ = self.events.send(Event::Copying { job });
        match products::copy(products, directory) {
            Ok(messages) => {
                for artifact in messages {
                    let _ = self.events.send(Event::Output {
                        job,
                        message: Message::Copied { artifact },
                    });
                }
                Outcome::Exited { code: 0 }
            }
            Err(reason) => Outcome::Failed { reason },
        }
    }

    /// Moves what the command left in `staged` into `destination` and
    /// tells the build's waiters where each output went; how the build
    /// failed when moving did.
    fn deliver(&self, staged: &Path, destination: &Path) -> Outcome {
        let job = self.start.job;
        let _ = self.events.send(Event::Copying { job });
        match products::deliver(staged, destination) {
            Ok(delivered) => {
                for path in delivered {
                    let _ = self.events.send(Event::Output {
                        job,
                        message: Message::Delivered { path },
                    });
                }
                Outcome::Exited { code: 0 }
            }
            Err(reason) => Outcome::Failed { reason },
        }
    }

    /// Sends each line of `output`, read as `stream` says, to the scheduler
    /// on a thread of its own.
    fn forward(
        &self,
        output: impl std::io::Read + Send + 'static,
        stream: Stream,
        done: Sender<()>,
        memory: Option<Sender<memory::Sample>>,
    ) {
        let events = self.events.clone();
        let job = self.start.job;
        let log = Arc::clone(&self.log);
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
                    let stdout = |line| Event::Output {
                        job,
                        message: Message::Stdout { line },
                    };
                    let reported = match &stream {
                        Stream::Stderr => {
                            log_line(&log, &line, BuildLog::stderr);
                            vec![Event::Output {
                                job,
                                message: Message::Stderr { line },
                            }]
                        }
                        Stream::Stdout => {
                            log_line(&log, &line, BuildLog::stdout);
                            vec![stdout(line)]
                        }
                        Stream::Cargo {
                            target,
                            products,
                            tests,
                        } => match Line::of(&line) {
                            Line::Crate {
                                fresh,
                                outputs,
                                executable,
                            } => {
                                let unit =
                                    outputs.iter().find_map(|path| slot::unit_key(target, path));
                                if executable.is_some() {
                                    products
                                        .lock()
                                        .expect("no thread panics holding a build's products")
                                        .push(Product {
                                            message: line,
                                            files: outputs,
                                        });
                                }
                                vec![Event::Crate { job, fresh, unit }]
                            }
                            Line::BuildScript { out_dir } => slot::unit_key(target, &out_dir)
                                .map(|unit| Event::BuildScript { job, unit })
                                .into_iter()
                                .collect(),
                            Line::BuildFinished { success } => {
                                log_line(&log, &line, BuildLog::stdout);
                                if *tests && success {
                                    let _ = memory
                                        .as_ref()
                                        .expect("Cargo has a sampler")
                                        .send(memory::Sample::Testing);
                                    vec![Event::Testing { job }, stdout(line)]
                                } else {
                                    vec![stdout(line)]
                                }
                            }
                            Line::Forward => {
                                log_line(&log, &line, BuildLog::stdout);
                                vec![stdout(line)]
                            }
                        },
                    };
                    if reported
                        .into_iter()
                        .any(|event| events.send(event).is_err())
                    {
                        break;
                    }
                }
                let _ = done.send(());
            });
        if let Err(error) = spawned {
            log!("could not read a build's output: {error}");
        }
    }
}

/// How a process's output is read.
enum Stream {
    /// Cargo's standard output: its JSON messages name files in `target`,
    /// and the executables they report go to `products`.
    Cargo {
        target: PathBuf,
        products: Arc<Mutex<Vec<Product>>>,
        tests: bool,
    },
    /// A command's standard output, passed on as it is.
    Stdout,
    /// Standard error, passed on as it is.
    Stderr,
}

/// Adds `line` to a build's `log` as `stream` while the build runs.
fn log_line(log: &Mutex<Option<BuildLog>>, line: &str, stream: fn(&mut BuildLog, &str)) {
    if let Some(log) = log
        .lock()
        .expect("no thread panics holding a build's log")
        .as_mut()
    {
        stream(log, line);
    }
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
        test_memory: None,
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::snapshot::tests::{TempDir, git, repository};

    fn packs(mirror: &Path) -> usize {
        std::fs::read_dir(mirror.join("objects/pack"))
            .unwrap()
            .filter_map(Result::ok)
            .filter(|entry| {
                entry
                    .path()
                    .extension()
                    .is_some_and(|extension| extension == "pack")
            })
            .count()
    }

    #[test]
    fn a_resource_stop_kills_the_process_group_and_preserves_the_failure_reason() {
        use std::os::unix::process::{CommandExt as _, ExitStatusExt as _};
        let mut child = std::process::Command::new("sh")
            .args(["-c", "sleep 60"])
            .process_group(0)
            .spawn()
            .unwrap();
        let run = Arc::new(Run::default());
        run.lock().group = Some(Pid::from_child(&child));
        run.cancel(Stop::Failed("free disk floor breached".into()));
        assert_eq!(child.wait().unwrap().signal(), Some(libc::SIGKILL));
        run.lock().exited = true;
        run.cancel(Stop::Withdrawn);
        assert_eq!(
            run.lock().stopped,
            Some(Stop::Failed("free disk floor breached".into()))
        );
    }

    #[test]
    fn products_are_copied_only_to_absolute_directories_outside_the_home() {
        let home = Path::new("/srv/buildd");
        assert_eq!(copy_destination_here(Path::new("/srv/bin"), home), Ok(()));
        for refused in [
            "bin",
            "./bin",
            "/srv/out/../buildd/slots",
            "/srv/buildd/slots/x/0/target",
        ] {
            assert!(
                copy_destination_here(Path::new(refused), home).is_err(),
                "{refused}"
            );
        }
        // Another machine's home is that machine's to check.
        assert_eq!(copy_destination(Path::new("/srv/buildd/out")), Ok(()));
        assert!(copy_destination(Path::new("out")).is_err());
    }

    #[test]
    fn a_mirror_with_many_packs_is_packed_again_keeping_its_references_loose() {
        let home = TempDir::new();
        let mirror = mirror(&home.0, "app-1").unwrap();
        let repository = repository();
        git(
            &repository.0,
            &[
                "push",
                "-q",
                mirror.to_str().unwrap(),
                "HEAD:refs/buildd/pushed",
            ],
        );
        maintain_mirror(&mirror).unwrap();
        let few = packs(&mirror);

        let scratch = TempDir::new();
        for push in 0..60 {
            let file = scratch.0.join("blob");
            std::fs::write(&file, format!("push {push}\n")).unwrap();
            let blob = git(&mirror, &["hash-object", "-w", file.to_str().unwrap()]);
            // Repacking without -a packs only what references reach.
            git(
                &mirror,
                &["update-ref", &format!("refs/kept/{push}"), blob.trim()],
            );
            git(&mirror, &["repack", "-q"]);
        }
        assert!(packs(&mirror) > 50);
        maintain_mirror(&mirror).unwrap();
        assert!(packs(&mirror) <= few + 1, "{} packs", packs(&mirror));
        git(&mirror, &["cat-file", "-e", "refs/buildd/pushed^{tree}"]);
        assert!(mirror.join("refs/buildd/pushed").is_file());
    }
}
