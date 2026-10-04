//! Which build runs where and who hears about it.
//!
//! The scheduler is pure state: the daemon feeds it requests, withdrawals,
//! output and exits, and carries out the [`Effect`]s it returns.
//!
//! - **Deduplication.** A request equal to a queued or running build (same
//!   repository, revision, directory and operation) waits for that build.
//! - **Supersession.** A request from a worktree replaces that worktree's
//!   queued requests for the same directory and operation at older
//!   revisions: their waiters wait for the new revision instead, at the
//!   oldest one's place in the queue. Waiters from other worktrees keep
//!   their revision.
//! - **Cancellation.** A running build nobody waits for any more is
//!   cancelled; a queued one is dropped.
//! - **Slots.** At most `capacity` slots are busy: running a build, or being
//!   kept within their disk limit. A build takes the idle slot of its
//!   repository where Cargo has the least to do: one that already did its
//!   [`Compilation`] before one that did not, and among those the one whose
//!   tree is closest to the build's, usually the slot that last built its
//!   worktree. It never waits for a busy slot while one is idle: measured,
//!   waiting for a warm slot cost more than warming another. It gets a new
//!   slot only when every slot of its repository runs a build, so slot
//!   directories are created only when builds of a repository run
//!   concurrently.
//! - **Maintenance.** A slot is kept within its disk limit once it has been
//!   idle for [`MAINTENANCE_QUIET`], so the measurement does not delay the
//!   next build of a session that runs several in a row, and right after a
//!   build once [`MAINTENANCE_DUE`] builds went unmeasured, so the limit
//!   holds under constant load.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use crate::cargo::{Command, Operation};
use crate::protocol::{Message, Outcome, QueuedBuild, RunningBuild, SlotStatus};
use crate::slot::{Pruning, slot_name};
use crate::snapshot::{Revision, Source};

/// How long a slot stays idle before it is kept within its disk limit.
pub(crate) const MAINTENANCE_QUIET: Duration = Duration::from_secs(2);
/// After this many unmeasured builds a slot is kept within its limit right
/// after its build, idle or not.
pub(crate) const MAINTENANCE_DUE: u32 = 8;

/// How far apart two trees of a repository are, as the work Cargo redoes
/// when a slot holding one is checked out to the other.
pub(crate) trait Distance {
    fn distance(&mut self, repository: &Path, from: &Revision, to: &Revision) -> u64;
}

/// A build the scheduler tracks.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Ord, PartialOrd, Hash)]
pub(crate) struct JobId(u64);

/// A slot the scheduler tracks.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash)]
pub(crate) struct SlotKey(usize);

/// A client waiting for a build.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Ord, PartialOrd, Hash)]
pub(crate) struct WaiterId(pub(crate) u64);

/// A request to build `revision` of `source`.
pub(crate) struct Submission {
    pub(crate) waiter: WaiterId,
    pub(crate) source: Source,
    pub(crate) revision: Revision,
    pub(crate) operation: Operation,
}

/// What the daemon must do.
#[derive(Debug, PartialEq)]
pub(crate) enum Effect {
    Send {
        waiter: WaiterId,
        message: Message,
    },
    Start(Start),
    Cancel {
        job: JobId,
    },
    /// Keep slot `slot` of `repository` within its disk limit.
    Maintain {
        key: SlotKey,
        repository: PathBuf,
        slot: usize,
    },
}

/// A build to start in slot `slot` of `repository`.
#[derive(Debug, PartialEq)]
pub(crate) struct Start {
    pub(crate) job: JobId,
    pub(crate) key: SlotKey,
    pub(crate) repository: PathBuf,
    /// The slot's index among its repository's slots.
    pub(crate) slot: usize,
    pub(crate) prefix: PathBuf,
    pub(crate) revision: Revision,
    pub(crate) operation: Operation,
}

/// What Cargo compiles for a build: its directory, command, and the
/// arguments before `--` (those after go to the test harness or the
/// compiler driver). A slot that did a compilation keeps its artifacts until
/// its target is cleared, so doing it again there is incremental.
#[derive(Clone, Debug, Eq, PartialEq, Hash)]
pub(crate) struct Compilation {
    prefix: PathBuf,
    command: Command,
    args: Vec<String>,
}

impl Compilation {
    fn of(job: &Job) -> Self {
        Self {
            prefix: job.prefix.clone(),
            command: job.operation.command,
            args: job
                .operation
                .args
                .iter()
                .take_while(|argument| *argument != "--")
                .cloned()
                .collect(),
        }
    }
}

struct Waiter {
    id: WaiterId,
    worktree: PathBuf,
    since: Instant,
}

struct Job {
    repository: PathBuf,
    prefix: PathBuf,
    revision: Revision,
    operation: Operation,
    waiters: Vec<Waiter>,
    /// The job's place in the queue: the earliest submission it carries.
    order: u64,
    state: State,
    /// Output so far, replayed to waiters who join while it runs.
    output: Vec<Message>,
}

enum State {
    Queued,
    Running {
        /// Index into the scheduler's slots.
        slot: usize,
        started: Instant,
        cancelled: bool,
    },
}

struct Slot {
    repository: PathBuf,
    /// The slot's index among its repository's slots.
    index: usize,
    job: Option<JobId>,
    /// The tree its checkout holds: that of its latest build.
    revision: Revision,
    /// The worktree its latest build was for.
    worktree: PathBuf,
    /// When it last started a build, on the scheduler's clock.
    used: u64,
    /// Its target is being kept within its disk limit.
    maintaining: bool,
    /// Builds since its target was last measured.
    unmeasured: u32,
    /// When its latest build ended.
    idle_since: Instant,
    /// Its target's disk usage when last measured.
    size: Option<u64>,
    /// Its last maintenance took caches in use: its limit is too small.
    undersized: bool,
    /// The compilations it did since its target was last cleared.
    compiled: HashSet<Compilation>,
}

impl Slot {
    fn busy(&self) -> bool {
        self.job.is_some() || self.maintaining
    }
}

pub(crate) struct Scheduler<D> {
    capacity: usize,
    distance: D,
    clock: u64,
    next_job: u64,
    jobs: BTreeMap<JobId, Job>,
    waiting: HashMap<WaiterId, JobId>,
    slots: Vec<Slot>,
}

impl Job {
    fn same_build(&self, source: &Source, revision: &Revision, operation: &Operation) -> bool {
        self.repository == source.repository
            && self.prefix == source.prefix
            && self.operation == *operation
            && self.revision == *revision
    }

    fn queued(&self) -> bool {
        matches!(self.state, State::Queued)
    }

    fn cancelled(&self) -> bool {
        matches!(
            self.state,
            State::Running {
                cancelled: true,
                ..
            }
        )
    }

    fn send_all(&self, message: &Message) -> impl Iterator<Item = Effect> {
        self.waiters.iter().map(move |waiter| Effect::Send {
            waiter: waiter.id,
            message: message.clone(),
        })
    }
}

impl<D: Distance> Scheduler<D> {
    pub(crate) fn new(capacity: usize, distance: D) -> Self {
        assert!(capacity > 0, "a scheduler runs at least one build");
        Self {
            capacity,
            distance,
            clock: 0,
            next_job: 0,
            jobs: BTreeMap::new(),
            waiting: HashMap::new(),
            slots: Vec::new(),
        }
    }

    pub(crate) fn submit(&mut self, submission: Submission) -> Vec<Effect> {
        let Submission {
            waiter,
            source,
            revision,
            operation,
        } = submission;
        assert!(!self.waiting.contains_key(&waiter), "a waiter submits once");
        self.clock += 1;
        let mut order = self.clock;
        let mut joining = vec![Waiter {
            id: waiter,
            worktree: source.worktree.clone(),
            since: Instant::now(),
        }];

        let superseded = self
            .jobs
            .iter()
            .filter(|(_, job)| {
                job.queued()
                    && job.repository == source.repository
                    && job.prefix == source.prefix
                    && job.operation == operation
                    && job.revision != revision
            })
            .map(|(id, _)| *id)
            .collect::<Vec<_>>();
        for id in superseded {
            let job = self.jobs.get_mut(&id).expect("listed above");
            let (mine, others) = std::mem::take(&mut job.waiters)
                .into_iter()
                .partition::<Vec<_>, _>(|waiter| waiter.worktree == source.worktree);
            job.waiters = others;
            if !mine.is_empty() {
                order = order.min(job.order);
                joining.extend(mine);
            }
            if job.waiters.is_empty() {
                self.jobs.remove(&id);
            }
        }

        let existing = self
            .jobs
            .iter()
            .find(|(_, job)| job.same_build(&source, &revision, &operation) && !job.cancelled())
            .map(|(id, _)| *id);
        let id = existing.unwrap_or_else(|| {
            let id = JobId(self.next_job);
            self.next_job += 1;
            self.jobs.insert(
                id,
                Job {
                    repository: source.repository.clone(),
                    prefix: source.prefix.clone(),
                    revision,
                    operation,
                    waiters: Vec::new(),
                    order,
                    state: State::Queued,
                    output: Vec::new(),
                },
            );
            id
        });

        let mut effects = Vec::new();
        let job = self.jobs.get_mut(&id).expect("found or inserted above");
        job.order = job.order.min(order);
        for waiter in joining {
            self.waiting.insert(waiter.id, id);
            effects.extend(self.greeting(id, waiter.id));
            self.jobs
                .get_mut(&id)
                .expect("found or inserted above")
                .waiters
                .push(waiter);
        }
        effects.extend(self.start_ready());
        effects
    }

    /// What a waiter joining job `id` is told first.
    fn greeting(&self, id: JobId, waiter: WaiterId) -> Vec<Effect> {
        let job = &self.jobs[&id];
        match job.state {
            State::Queued => vec![Effect::Send {
                waiter,
                message: Message::Queued {
                    revision: job.revision.clone(),
                },
            }],
            State::Running { slot, .. } => {
                let started = Message::Started {
                    revision: job.revision.clone(),
                    slot: slot_name(&self.slots[slot].repository, self.slots[slot].index),
                };
                std::iter::once(started)
                    .chain(job.output.iter().cloned())
                    .map(|message| Effect::Send { waiter, message })
                    .collect()
            }
        }
    }

    /// The client of `waiter` went away.
    pub(crate) fn withdraw(&mut self, waiter: WaiterId) -> Vec<Effect> {
        // A waiter whose build already finished is no longer tracked.
        let Some(id) = self.waiting.remove(&waiter) else {
            return Vec::new();
        };
        let job = self.jobs.get_mut(&id).expect("a waiter's job is tracked");
        job.waiters.retain(|other| other.id != waiter);
        if !job.waiters.is_empty() {
            return Vec::new();
        }
        match &mut job.state {
            State::Queued => {
                self.jobs.remove(&id);
                Vec::new()
            }
            State::Running { cancelled, .. } => {
                *cancelled = true;
                vec![Effect::Cancel { job: id }]
            }
        }
    }

    /// A line of output of running job `id`.
    pub(crate) fn output(&mut self, id: JobId, message: Message) -> Vec<Effect> {
        // Output of a process that escaped its build's process group can
        // arrive after the build ended; nobody waits for it.
        let Some(job) = self.jobs.get_mut(&id) else {
            return Vec::new();
        };
        let effects = job.send_all(&message).collect();
        job.output.push(message);
        effects
    }

    /// Running job `id` ended with `outcome`.
    pub(crate) fn exited(&mut self, id: JobId, outcome: &Outcome) -> Vec<Effect> {
        let job = self.jobs.remove(&id).expect("only tracked jobs run");
        let State::Running { slot, started, .. } = job.state else {
            panic!("only running jobs exit");
        };
        let compilation = Compilation::of(&job);
        let entry = &mut self.slots[slot];
        // Cargo ran to its end: its artifacts are in the slot, also when the
        // job's own crates did not compile.
        if let Outcome::Exited { .. } = outcome {
            entry.compiled.insert(compilation);
        }
        entry.job = None;
        entry.unmeasured += 1;
        entry.idle_since = Instant::now();
        let mut effects = Vec::new();
        if entry.unmeasured >= MAINTENANCE_DUE {
            effects.push(self.maintain(slot));
        }
        let build_ms = millis(started.elapsed());
        for waiter in job.waiters {
            self.waiting.remove(&waiter.id);
            effects.push(Effect::Send {
                waiter: waiter.id,
                message: Message::Finished {
                    revision: job.revision.clone(),
                    outcome: outcome.clone(),
                    queued_ms: millis(started.saturating_duration_since(waiter.since)),
                    build_ms,
                },
            });
        }
        effects.extend(self.start_ready());
        effects
    }

    fn maintain(&mut self, index: usize) -> Effect {
        let slot = &mut self.slots[index];
        assert!(!slot.busy(), "only an idle slot is maintained");
        slot.maintaining = true;
        Effect::Maintain {
            key: SlotKey(index),
            repository: slot.repository.clone(),
            slot: slot.index,
        }
    }

    /// When the next idle slot is due for maintenance, if one will be.
    pub(crate) fn next_maintenance(&self) -> Option<Instant> {
        self.slots
            .iter()
            .filter(|slot| !slot.busy() && slot.unmeasured > 0)
            .map(|slot| slot.idle_since + MAINTENANCE_QUIET)
            .min()
    }

    /// Maintains the slots that have been idle for [`MAINTENANCE_QUIET`] by
    /// `now`.
    pub(crate) fn maintenance_due(&mut self, now: Instant) -> Vec<Effect> {
        let due = self
            .slots
            .iter()
            .enumerate()
            .filter(|(_, slot)| {
                !slot.busy() && slot.unmeasured > 0 && slot.idle_since + MAINTENANCE_QUIET <= now
            })
            .map(|(index, _)| index)
            .collect::<Vec<_>>();
        due.into_iter().map(|index| self.maintain(index)).collect()
    }

    /// Slot `key` was kept within its disk limit by `pruning`; None when
    /// that failed.
    pub(crate) fn maintained(&mut self, key: SlotKey, pruning: Option<Pruning>) -> Vec<Effect> {
        let slot = &mut self.slots[key.0];
        assert!(
            slot.maintaining,
            "only a slot being maintained is maintained"
        );
        slot.maintaining = false;
        slot.unmeasured = 0;
        slot.size = pruning.map(Pruning::size);
        slot.undersized = pruning.is_some_and(Pruning::undersized);
        if let Some(Pruning::Cleared { .. }) = pruning {
            slot.compiled.clear();
        }
        self.start_ready()
    }

    /// Starts queued builds, next first, while slots are free. A build that
    /// waits for its repository's slot lets later builds of other
    /// repositories go first.
    fn start_ready(&mut self) -> Vec<Effect> {
        let mut effects = Vec::new();
        let mut queued = self
            .jobs
            .iter()
            .filter(|(_, job)| job.queued())
            .map(|(id, job)| (job.order, *id))
            .collect::<Vec<_>>();
        queued.sort_unstable();
        for (_, id) in queued {
            if self.slots.iter().filter(|slot| slot.busy()).count() >= self.capacity {
                break;
            }
            let Some(slot) = self.choose_slot(id) else {
                continue;
            };
            self.clock += 1;
            let job = &self.jobs[&id];
            let entry = &mut self.slots[slot];
            entry.job = Some(id);
            entry.revision = job.revision.clone();
            entry.worktree = job.waiters[0].worktree.clone();
            entry.used = self.clock;
            let job = self.jobs.get_mut(&id).expect("chosen above");
            job.state = State::Running {
                slot,
                started: Instant::now(),
                cancelled: false,
            };
            let started = Message::Started {
                revision: job.revision.clone(),
                slot: slot_name(&self.slots[slot].repository, self.slots[slot].index),
            };
            effects.extend(job.send_all(&started));
            effects.push(Effect::Start(Start {
                job: id,
                key: SlotKey(slot),
                repository: job.repository.clone(),
                slot: self.slots[slot].index,
                prefix: job.prefix.clone(),
                revision: job.revision.clone(),
                operation: job.operation.clone(),
            }));
        }
        effects
    }

    /// The slot queued job `id` runs in now, as the module explains, or None
    /// while it waits.
    fn choose_slot(&mut self, id: JobId) -> Option<usize> {
        let job = &self.jobs[&id];
        let repository = job.repository.clone();
        let revision = job.revision.clone();
        let compilation = Compilation::of(job);
        // Ranked by: did the compilation, closeness, recent use.
        let mut best: Option<(bool, u64, u64, usize)> = None;
        for (index, slot) in self.slots.iter().enumerate() {
            if slot.repository != repository || slot.busy() {
                continue;
            }
            let distance = self
                .distance
                .distance(&repository, &slot.revision, &revision);
            let rank = (
                !slot.compiled.contains(&compilation),
                distance,
                u64::MAX - slot.used,
                index,
            );
            if best.is_none_or(|best| rank < best) {
                best = Some(rank);
            }
        }
        if let Some((_, _, _, index)) = best {
            return Some(index);
        }
        let own = || {
            self.slots
                .iter()
                .filter(|slot| slot.repository == repository)
        };
        // A slot being maintained is free again soon, and warm.
        if own().any(|slot| slot.maintaining) || own().count() >= self.capacity {
            return None;
        }
        let index = own().count();
        self.slots.push(Slot {
            repository,
            index,
            job: None,
            revision,
            worktree: job.waiters[0].worktree.clone(),
            used: 0,
            maintaining: false,
            unmeasured: 0,
            idle_since: Instant::now(),
            size: None,
            undersized: false,
            compiled: HashSet::new(),
        });
        Some(self.slots.len() - 1)
    }

    /// The slots and the queue, next build first.
    pub(crate) fn status(&self) -> (Vec<SlotStatus>, Vec<QueuedBuild>) {
        let slots = self
            .slots
            .iter()
            .map(|slot| SlotStatus {
                name: slot_name(&slot.repository, slot.index),
                worktree: Some(slot.worktree.clone()),
                size: slot.size,
                maintaining: slot.maintaining,
                undersized: slot.undersized,
                build: slot.job.map(|id| {
                    let job = &self.jobs[&id];
                    let State::Running {
                        started, cancelled, ..
                    } = job.state
                    else {
                        panic!("a slot's job runs");
                    };
                    RunningBuild {
                        revision: job.revision.clone(),
                        operation: job.operation.clone(),
                        waiters: job.waiters.len(),
                        elapsed_ms: millis(started.elapsed()),
                        cancelled,
                    }
                }),
            })
            .collect();
        let mut queued = self
            .jobs
            .iter()
            .filter(|(_, job)| job.queued())
            .collect::<Vec<_>>();
        queued.sort_by_key(|(id, job)| (job.order, **id));
        let queue = queued
            .into_iter()
            .map(|(_, job)| QueuedBuild {
                revision: job.revision.clone(),
                operation: job.operation.clone(),
                worktrees: job
                    .waiters
                    .iter()
                    .map(|waiter| waiter.worktree.clone())
                    .collect(),
                waited_ms: job
                    .waiters
                    .iter()
                    .map(|waiter| millis(waiter.since.elapsed()))
                    .max()
                    .unwrap_or(0),
            })
            .collect();
        (slots, queue)
    }
}

fn millis(duration: Duration) -> u64 {
    u64::try_from(duration.as_millis()).unwrap_or(u64::MAX)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cargo::Command;

    /// Distances from a table; trees not in it are equal or 10 apart.
    #[derive(Default)]
    struct Table(HashMap<(String, String), u64>);

    impl Distance for Table {
        fn distance(&mut self, _: &Path, from: &Revision, to: &Revision) -> u64 {
            let (from, to) = (from.to_string(), to.to_string());
            if from == to {
                return 0;
            }
            self.0
                .get(&(from.clone(), to.clone()))
                .or_else(|| self.0.get(&(to, from)))
                .copied()
                .unwrap_or(10)
        }
    }

    fn scheduler(capacity: usize) -> Scheduler<Table> {
        Scheduler::new(capacity, Table::default())
    }

    fn source(worktree: &str) -> Source {
        Source {
            repository: "/repo/.git".into(),
            worktree: worktree.into(),
            prefix: PathBuf::new(),
            index: PathBuf::new(),
        }
    }

    fn check() -> Operation {
        Operation {
            command: Command::Check,
            args: Vec::new(),
        }
    }

    fn revision(name: &str) -> Revision {
        serde_json::from_value(serde_json::Value::String(name.into())).unwrap()
    }

    fn submit(
        scheduler: &mut Scheduler<Table>,
        waiter: u64,
        worktree: &str,
        tree: &str,
    ) -> Vec<Effect> {
        scheduler.submit(Submission {
            waiter: WaiterId(waiter),
            source: source(worktree),
            revision: revision(tree),
            operation: check(),
        })
    }

    fn submit_operation(
        scheduler: &mut Scheduler<Table>,
        waiter: u64,
        worktree: &str,
        tree: &str,
        command: Command,
        args: &[&str],
    ) -> Vec<Effect> {
        scheduler.submit(Submission {
            waiter: WaiterId(waiter),
            source: source(worktree),
            revision: revision(tree),
            operation: Operation {
                command,
                args: args.iter().map(|argument| (*argument).to_owned()).collect(),
            },
        })
    }

    fn starts(effects: &[Effect]) -> Vec<(JobId, usize, String)> {
        effects
            .iter()
            .filter_map(|effect| match effect {
                Effect::Start(start) => Some((start.job, start.slot, start.revision.to_string())),
                _ => None,
            })
            .collect()
    }

    fn maintains(effects: &[Effect]) -> Vec<usize> {
        effects
            .iter()
            .filter_map(|effect| match effect {
                Effect::Maintain { slot, .. } => Some(*slot),
                _ => None,
            })
            .collect()
    }

    fn sent(effects: &[Effect], waiter: u64) -> Vec<&Message> {
        effects
            .iter()
            .filter_map(|effect| match effect {
                Effect::Send {
                    waiter: to,
                    message,
                } if *to == WaiterId(waiter) => Some(message),
                _ => None,
            })
            .collect()
    }

    fn ok() -> Outcome {
        Outcome::Exited { code: 0 }
    }

    /// Runs `tree` for `worktree` to its end and returns the slot it ran in.
    fn run(scheduler: &mut Scheduler<Table>, waiter: u64, worktree: &str, tree: &str) -> usize {
        let effects = submit(scheduler, waiter, worktree, tree);
        let [(job, slot, _)] = starts(&effects)[..] else {
            panic!("{tree} starts at once: {effects:?}");
        };
        scheduler.exited(job, &ok());
        slot
    }

    #[test]
    fn equal_requests_share_one_build_and_a_late_joiner_gets_the_output_so_far() {
        let mut scheduler = scheduler(1);
        let first = submit(&mut scheduler, 1, "/a", "t1");
        let [(job, 0, _)] = starts(&first)[..] else {
            panic!("{first:?}");
        };
        let line = Message::Stdout { line: "x".into() };
        assert_eq!(scheduler.output(job, line.clone()).len(), 1);

        let second = submit(&mut scheduler, 2, "/b", "t1");
        assert!(starts(&second).is_empty());
        assert!(matches!(sent(&second, 2)[..], [Message::Started { .. }, m] if *m == line));

        let finished = scheduler.exited(job, &ok());
        assert!(matches!(sent(&finished, 1)[..], [Message::Finished { .. }]));
        assert!(matches!(sent(&finished, 2)[..], [Message::Finished { .. }]));
    }

    #[test]
    fn a_newer_request_supersedes_its_worktrees_queued_ones_and_keeps_their_place() {
        let mut scheduler = scheduler(1);
        let busy = submit(&mut scheduler, 1, "/busy", "t0");
        let [(running, ..)] = starts(&busy)[..] else {
            panic!("{busy:?}");
        };
        submit(&mut scheduler, 2, "/a", "t1");
        submit(&mut scheduler, 3, "/b", "t1");
        submit(&mut scheduler, 4, "/c", "u1");
        let newer = submit(&mut scheduler, 5, "/a", "t2");
        // /a's waiter moves to t2; /b's keeps t1, which is still its content.
        assert!(
            matches!(sent(&newer, 2)[..], [Message::Queued { revision }] if revision.to_string() == "t2")
        );
        let (_, queue) = scheduler.status();
        let order = queue
            .iter()
            .map(|build| (build.revision.to_string(), build.worktrees.len()))
            .collect::<Vec<_>>();
        assert_eq!(
            order,
            [("t1".into(), 1), ("t2".into(), 2), ("u1".into(), 1)]
        );

        let next = scheduler.exited(running, &ok());
        assert_eq!(
            starts(&next)
                .iter()
                .map(|s| s.2.clone())
                .collect::<Vec<_>>(),
            ["t1"]
        );
    }

    #[test]
    fn a_build_nobody_waits_for_is_dropped_or_cancelled_and_not_joined() {
        let mut scheduler = scheduler(1);
        let first = submit(&mut scheduler, 1, "/a", "t1");
        let [(job, ..)] = starts(&first)[..] else {
            panic!("{first:?}");
        };
        submit(&mut scheduler, 2, "/b", "t2");
        assert!(scheduler.withdraw(WaiterId(2)).is_empty());
        assert!(scheduler.status().1.is_empty());

        assert_eq!(scheduler.withdraw(WaiterId(1)), [Effect::Cancel { job }]);
        // An equal request does not join the build being stopped.
        let again = submit(&mut scheduler, 3, "/a", "t1");
        assert!(matches!(sent(&again, 3)[..], [Message::Queued { .. }]));
        let next = scheduler.exited(job, &Outcome::Signaled { signal: 15 });
        assert!(sent(&next, 1).is_empty());
        assert_eq!(starts(&next).len(), 1);
        assert!(scheduler.withdraw(WaiterId(1)).is_empty());
    }

    #[test]
    fn a_build_takes_the_idle_slot_whose_tree_is_closest() {
        let mut scheduler = scheduler(2);
        // Two concurrent builds create two slots.
        let a = submit(&mut scheduler, 1, "/a", "a1");
        let b = submit(&mut scheduler, 2, "/b", "b1");
        let [(job_a, 0, _)] = starts(&a)[..] else {
            panic!("{a:?}")
        };
        let [(job_b, 1, _)] = starts(&b)[..] else {
            panic!("{b:?}")
        };
        scheduler.exited(job_a, &ok());
        scheduler.exited(job_b, &ok());

        // A worktree's next tree is closest to its previous one.
        scheduler.distance.0.insert(("b1".into(), "b2".into()), 1);
        assert_eq!(run(&mut scheduler, 3, "/b", "b2"), 1);
        // A new worktree goes to the slot closest to its tree, whoever used it.
        scheduler.distance.0.insert(("a1".into(), "c1".into()), 2);
        scheduler.distance.0.insert(("b2".into(), "c1".into()), 7);
        assert_eq!(run(&mut scheduler, 4, "/c", "c1"), 0);
        // Equally close slots: the most recently used one.
        assert_eq!(run(&mut scheduler, 5, "/d", "d1"), 0);
    }

    #[test]
    fn slots_grow_only_while_every_slot_runs_a_build() {
        let mut scheduler = scheduler(2);
        assert_eq!(run(&mut scheduler, 1, "/a", "a1"), 0);
        assert_eq!(run(&mut scheduler, 2, "/b", "b1"), 0);

        let c = submit(&mut scheduler, 3, "/c", "c1");
        let d = submit(&mut scheduler, 4, "/d", "d1");
        let e = submit(&mut scheduler, 5, "/e", "e1");
        assert!(matches!(starts(&c)[..], [(_, 0, _)]));
        assert!(matches!(starts(&d)[..], [(_, 1, _)]));
        assert!(starts(&e).is_empty());
        assert_eq!(scheduler.status().0.len(), 2);
    }

    #[test]
    fn an_idle_slot_is_maintained_after_a_quiet_period_and_waited_for_meanwhile() {
        let mut scheduler = scheduler(2);
        let first = submit(&mut scheduler, 1, "/a", "a1");
        let [(job, 0, _)] = starts(&first)[..] else {
            panic!("{first:?}");
        };
        let ended = Instant::now();
        assert!(maintains(&scheduler.exited(job, &ok())).is_empty());
        let due = scheduler.next_maintenance().expect("the slot is due");
        assert!(due >= ended + MAINTENANCE_QUIET);
        assert!(scheduler.maintenance_due(ended).is_empty());
        assert_eq!(maintains(&scheduler.maintenance_due(due)), [0]);
        assert_eq!(scheduler.next_maintenance(), None);

        let waiting = submit(&mut scheduler, 2, "/a", "a2");
        assert!(starts(&waiting).is_empty(), "no second, cold slot");
        // Another repository's build does not wait behind it.
        let other = scheduler.submit(Submission {
            waiter: WaiterId(3),
            source: Source {
                repository: "/other/.git".into(),
                ..source("/o")
            },
            revision: revision("o1"),
            operation: check(),
        });
        assert_eq!(starts(&other).len(), 1);
        let (slots, queue) = scheduler.status();
        assert!(slots[0].maintaining && slots[0].build.is_none());
        assert_eq!(queue.len(), 1);
        let ready = scheduler.maintained(SlotKey(0), Some(Pruning::Within { size: 5 }));
        assert!(
            matches!(starts(&ready)[..], [(_, 0, ref tree)] if tree == "a2"),
            "the waiting build takes its warm slot"
        );
        assert_eq!(scheduler.status().0[0].size, Some(5));
    }

    #[test]
    fn a_slot_busy_without_pause_is_maintained_after_enough_builds() {
        let mut scheduler = scheduler(1);
        for build in 1..MAINTENANCE_DUE {
            let effects = submit(&mut scheduler, u64::from(build), "/a", &format!("a{build}"));
            let [(job, ..)] = starts(&effects)[..] else {
                panic!("{effects:?}");
            };
            assert!(maintains(&scheduler.exited(job, &ok())).is_empty());
        }
        let last = submit(&mut scheduler, 99, "/a", "last");
        let [(job, ..)] = starts(&last)[..] else {
            panic!("{last:?}");
        };
        assert_eq!(maintains(&scheduler.exited(job, &ok())), [0]);
    }

    /// Two slots: slot 0 did `check` at tree a1, slot 1 did `test` at b1.
    fn checked_and_tested() -> Scheduler<Table> {
        let mut scheduler = scheduler(2);
        let a = submit(&mut scheduler, 1, "/a", "a1");
        let b = submit_operation(&mut scheduler, 2, "/b", "b1", Command::Test, &[]);
        let [(job_a, 0, _)] = starts(&a)[..] else {
            panic!("{a:?}")
        };
        let [(job_b, 1, _)] = starts(&b)[..] else {
            panic!("{b:?}")
        };
        scheduler.exited(job_a, &ok());
        scheduler.exited(job_b, &ok());
        scheduler
    }

    #[test]
    fn a_build_prefers_a_slot_that_did_its_compilation_over_a_closer_one() {
        let mut scheduler = checked_and_tested();
        scheduler.distance.0.insert(("b1".into(), "c1".into()), 1);
        assert_eq!(run(&mut scheduler, 3, "/c", "c1"), 0);
    }

    #[test]
    fn a_cleared_slot_forgets_its_compilations() {
        let mut scheduler = checked_and_tested();
        let maintain = scheduler.maintenance_due(Instant::now() + MAINTENANCE_QUIET);
        assert_eq!(maintains(&maintain), [0, 1]);
        scheduler.maintained(SlotKey(0), Some(Pruning::Cleared { before: 9 }));
        scheduler.maintained(SlotKey(1), Some(Pruning::Within { size: 1 }));
        assert!(scheduler.slots[0].compiled.is_empty());
        assert!(scheduler.status().0[0].undersized);
        assert_eq!(scheduler.slots[1].compiled.len(), 1);
    }

    #[test]
    fn test_harness_arguments_do_not_change_the_compilation() {
        let mut scheduler = scheduler(1);
        let first = submit_operation(
            &mut scheduler,
            1,
            "/a",
            "a1",
            Command::Test,
            &["-p", "x", "--", "one"],
        );
        let second = submit_operation(
            &mut scheduler,
            2,
            "/a",
            "a2",
            Command::Test,
            &["-p", "x", "--", "two"],
        );
        let [(job, ..)] = starts(&first)[..] else {
            panic!("{first:?}")
        };
        assert!(starts(&second).is_empty());
        let first = Compilation::of(&scheduler.jobs[&job]);
        let next = scheduler.exited(job, &ok());
        let [(next, ..)] = starts(&next)[..] else {
            panic!("{next:?}")
        };
        assert_eq!(Compilation::of(&scheduler.jobs[&next]), first);
        assert_eq!(first.args, ["-p", "x"]);
    }
}
