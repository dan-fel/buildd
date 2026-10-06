//! Which build runs where and who hears about it.
//!
//! The scheduler is pure state: the daemon feeds it requests, withdrawals,
//! output and exits, and carries out the [`Effect`]s it returns, including
//! a report of every decision for people watching.
//!
//! - **Deduplication.** A request equal to a queued or running build (same
//!   repository, revision, directory and operation) waits for that build.
//! - **Merging.** `test` operations differing only in `--no-fail-fast`
//!   are one request; the build runs with the flag. A running build without
//!   it is not joined by a request that has it.
//! - **Supersession.** A request from a worktree replaces that worktree's
//!   queued requests for the same directory and operation at older
//!   revisions: their waiters wait for the new revision instead, at the
//!   oldest one's place in the queue. Waiters from other worktrees keep
//!   their revision.
//! - **Cancellation.** A running build nobody waits for any more is
//!   cancelled; a queued one is dropped.
//! - **Slots.** At most `capacity` slots are busy: running a build, or being
//!   kept within their disk limit. A build takes the idle slot of its
//!   repository where Cargo has the least to do: the fewest compiled units
//!   the build needs and the slot lacks, plus the distance between the
//!   slot's tree and the build's. A build needs what the latest build of its
//!   [`Compilation`] used, in any slot; a compilation no build ran yet needs,
//!   as far as anyone knows, what the latest builds of the same command in
//!   the same directory used. It waits for a busy slot while one is idle
//!   only when that is expected to be faster: the busy slot's build ends
//!   (its compilation's last wall time) and the build compiles its share
//!   there sooner than the idle slot compiles its share cold, units costing
//!   what they cost lately. Without estimates it never waits.
//! - **Queue order.** The shortest expected build first, less the time it
//!   has waited, so short builds go ahead of long ones and a long one goes
//!   once it has waited its own length.
//!   It gets a new slot only when every slot of its repository runs a build,
//!   so slot directories are created only when builds of a repository run
//!   concurrently.
//! - **Maintenance.** A slot is kept within its disk limit once it has been
//!   idle for [`MAINTENANCE_QUIET`], so the measurement does not delay the
//!   next build of a session that runs several in a row, and right after a
//!   build once [`MAINTENANCE_DUE`] builds went unmeasured, so the limit
//!   holds under constant load.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant, SystemTime};

use crate::cargo::{Compilation, Operation};
use crate::passed::PassedBinary;
use crate::protocol::{
    EventKind, Hold, LastBuild, Message, Outcome, Phase, QueuedBuild, RunningBuild, SlotStatus,
    Usage,
};
use crate::slot::{CompilationRun, Pruning, SlotRecord, slot_name};
use crate::snapshot::{Revision, Source};

/// How long a slot stays idle before it is kept within its disk limit.
pub(crate) const MAINTENANCE_QUIET: Duration = Duration::from_secs(2);
/// After this many unmeasured builds a slot is kept within its limit right
/// after its build, idle or not.
pub(crate) const MAINTENANCE_DUE: u32 = 8;
/// A build that compiled at least this many units tells what one costs.
const UNIT_COST_SAMPLE: u64 = 5;

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
    /// Who asks; the worktree's folder name when None.
    pub(crate) label: Option<String>,
    /// Where to copy the executables Cargo produces, when asked.
    pub(crate) copy_to: Option<PathBuf>,
    /// Run test binaries that passed before and are unchanged too.
    pub(crate) rerun_all: bool,
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
    /// Keep the slot within its disk limit.
    Maintain(IdleSlot),
    /// Free `needed` bytes of disk from these slots, what builds used
    /// longest ago first.
    Reclaim {
        slots: Vec<IdleSlot>,
        needed: u64,
    },
    /// Something happened, for people watching.
    Report(EventKind),
    /// Slot `slot` of `repository` has a new record to keep on disk.
    Persist {
        repository: PathBuf,
        slot: usize,
        record: SlotRecord,
    },
}

/// Slot `slot` of `repository`, idle and handed out to free disk, with when
/// builds last `used` each of its compiled units. It is busy until the
/// scheduler hears it was [`Scheduler::maintained`].
#[derive(Debug, PartialEq)]
pub(crate) struct IdleSlot {
    pub(crate) key: SlotKey,
    pub(crate) repository: PathBuf,
    pub(crate) slot: usize,
    pub(crate) used: HashMap<String, u64>,
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
    pub(crate) copy_to: Option<PathBuf>,
    /// The test binaries that passed in the slot, which a run of every test
    /// may skip; empty when it must run them all.
    pub(crate) passed: Vec<PassedBinary>,
}

impl Compilation {
    fn of(job: &Job) -> Self {
        Self::new(&job.prefix, &job.operation)
    }
}

struct Waiter {
    id: WaiterId,
    worktree: PathBuf,
    /// Who asked, for people watching.
    who: String,
    since: Instant,
}

struct Job {
    repository: PathBuf,
    prefix: PathBuf,
    revision: Revision,
    operation: Operation,
    copy_to: Option<PathBuf>,
    rerun_all: bool,
    waiters: Vec<Waiter>,
    /// The job's place in the queue: the earliest submission it carries.
    order: u64,
    state: State,
    /// Output so far, replayed to waiters who join while it runs.
    output: Vec<Message>,
    /// Crates Cargo compiled so far, and crates it found up to date.
    compiled: u64,
    fresh: u64,
    /// The compiled units it used so far.
    units: HashSet<String>,
    /// It waits for a busy slot although another is idle, and why.
    hold: Option<Hold>,
    /// It waits for memory: what it is expected to need at its peak.
    memory_needed: Option<u64>,
    /// Test binaries it did not run: unchanged since they passed.
    skipped: u64,
}

enum State {
    Queued,
    Running {
        /// Index into the scheduler's slots.
        slot: usize,
        started: Instant,
        /// When Cargo reported that compilation finished.
        compiled_at: Option<Instant>,
        /// Cargo succeeded and its executables are being copied out.
        copying: bool,
        cancelled: bool,
        /// The memory admission charged it: its expected peak.
        memory: u64,
    },
}

struct Slot {
    repository: PathBuf,
    /// The slot's index among its repository's slots.
    index: usize,
    job: Option<JobId>,
    /// The tree its checkout holds: that of its latest build.
    revision: Revision,
    /// The worktree its latest build was for, when it is known.
    worktree: Option<PathBuf>,
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
    /// How its latest build since the daemon started ended.
    last: Option<LastBuild>,
    /// Crates its target holds in several variants, when last counted.
    duplicated: Option<usize>,
    /// The test binaries that passed in it.
    passed: Vec<PassedBinary>,
    /// What its latest build of each compilation used.
    compilations: Vec<CompilationRun>,
    /// When a build last used each compiled unit, in seconds since the Unix
    /// epoch.
    units: HashMap<String, u64>,
}

impl Slot {
    fn busy(&self) -> bool {
        self.job.is_some() || self.maintaining
    }

    /// Its build of `compilation` used `units`: everything the compilation
    /// needs when the build `succeeded`. A failed build may have stopped
    /// early, so what earlier builds used stays.
    fn ran(
        &mut self,
        compilation: Compilation,
        units: HashSet<String>,
        succeeded: bool,
        build_ms: u64,
        peak_memory: Option<u64>,
    ) {
        // A build that used no units says nothing about what it needs.
        if units.is_empty() {
            return;
        }
        let at = now();
        match self
            .compilations
            .iter_mut()
            .find(|run| run.compilation == compilation)
        {
            Some(run) => {
                if succeeded {
                    run.units.clear();
                    run.build_ms = Some(build_ms);
                }
                if peak_memory.is_some() {
                    run.peak_memory = peak_memory;
                }
                run.units.extend(units);
                run.at = at;
            }
            None => self.compilations.push(CompilationRun {
                compilation,
                at,
                units: units.into_iter().collect(),
                build_ms: succeeded.then_some(build_ms),
                peak_memory,
            }),
        }
    }

    fn persist(&self) -> Effect {
        let mut compilations = self.compilations.clone();
        compilations.sort_by(|a, b| {
            let (a, b) = (&a.compilation, &b.compilation);
            (&a.prefix, a.command, &a.args, &a.rustflags).cmp(&(
                &b.prefix,
                b.command,
                &b.args,
                &b.rustflags,
            ))
        });
        Effect::Persist {
            repository: self.repository.clone(),
            slot: self.index,
            record: SlotRecord {
                worktree: self.worktree.clone(),
                compilations,
                units: self
                    .units
                    .iter()
                    .map(|(unit, used)| (unit.clone(), *used))
                    .collect(),
                passed: self.passed.clone(),
            },
        }
    }
}

pub(crate) struct Scheduler<D> {
    capacity: usize,
    /// Memory running builds may use together, by their expected peaks.
    memory: u64,
    distance: D,
    /// Worktrees a build has started for.
    built_worktrees: HashSet<PathBuf>,
    clock: u64,
    next_job: u64,
    jobs: BTreeMap<JobId, Job>,
    waiting: HashMap<WaiterId, JobId>,
    slots: Vec<Slot>,
    /// Per repository, the wall time compiling one unit took lately, in
    /// milliseconds: a moving average over builds that compiled enough.
    unit_ms: HashMap<PathBuf, u64>,
}

impl Job {
    /// Whether `submission` can wait for this job: it builds the same tree,
    /// and a running job already does all `submission` asks.
    fn same_build(&self, submission: &Submission) -> bool {
        self.same_request(submission)
            && self.revision == submission.revision
            && (self.queued()
                || self.operation.merged(&submission.operation).as_ref() == Some(&self.operation))
    }

    /// Whether one build can do what the job and `submission` ask, maybe
    /// of another tree.
    fn same_request(&self, submission: &Submission) -> bool {
        self.repository == submission.source.repository
            && self.prefix == submission.source.prefix
            && self.operation.merged(&submission.operation).is_some()
            && self.copy_to == submission.copy_to
            && self.rerun_all == submission.rerun_all
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

    /// How long its longest-waiting waiter has waited.
    fn waited_ms(&self) -> u64 {
        self.waiters
            .iter()
            .map(|waiter| millis(waiter.since.elapsed()))
            .max()
            .expect("a job has waiters")
    }

    fn who(&self) -> Vec<String> {
        self.waiters
            .iter()
            .map(|waiter| waiter.who.clone())
            .collect()
    }

    fn send_all(&self, message: &Message) -> impl Iterator<Item = Effect> {
        self.waiters.iter().map(move |waiter| Effect::Send {
            waiter: waiter.id,
            message: message.clone(),
        })
    }
}

impl<D: Distance> Scheduler<D> {
    /// The scheduler's measure of distance between trees.
    pub(crate) fn distance_mut(&mut self) -> &mut D {
        &mut self.distance
    }

    pub(crate) fn new(capacity: usize, memory: u64, distance: D) -> Self {
        assert!(capacity > 0, "a scheduler runs at least one build");
        Self {
            capacity,
            memory,
            distance,
            built_worktrees: HashSet::new(),
            clock: 0,
            next_job: 0,
            jobs: BTreeMap::new(),
            waiting: HashMap::new(),
            slots: Vec::new(),
            unit_ms: HashMap::new(),
        }
    }

    /// Takes back slot `index` of `repository`, left on disk by an earlier
    /// daemon with its checkout at `revision` and its `record`. It is due
    /// for measurement like a slot after a build.
    pub(crate) fn restore(
        &mut self,
        repository: PathBuf,
        index: usize,
        revision: Revision,
        record: SlotRecord,
    ) {
        assert!(
            index < self.capacity,
            "only slots within capacity are restored"
        );
        assert!(
            !self
                .slots
                .iter()
                .any(|slot| slot.repository == repository && slot.index == index),
            "a slot is restored once"
        );
        self.slots.push(Slot {
            repository,
            index,
            job: None,
            revision,
            worktree: record.worktree,
            used: 0,
            maintaining: false,
            unmeasured: 1,
            idle_since: Instant::now(),
            size: None,
            undersized: false,
            last: None,
            duplicated: None,
            passed: record.passed,
            compilations: record.compilations,
            units: record.units.into_iter().collect(),
        });
    }

    pub(crate) fn submit(&mut self, submission: Submission) -> Vec<Effect> {
        let superseded = self
            .jobs
            .iter()
            .filter(|(_, job)| {
                job.queued() && job.same_request(&submission) && job.revision != submission.revision
            })
            .map(|(id, _)| *id)
            .collect::<Vec<_>>();
        let existing = self
            .jobs
            .iter()
            .find(|(_, job)| job.same_build(&submission) && !job.cancelled())
            .map(|(id, _)| *id);
        let Submission {
            waiter,
            source,
            revision,
            mut operation,
            label,
            copy_to,
            rerun_all,
        } = submission;
        assert!(!self.waiting.contains_key(&waiter), "a waiter submits once");
        self.clock += 1;
        let mut order = self.clock;
        let who = label.unwrap_or_else(|| {
            source.worktree.file_name().map_or_else(
                || source.worktree.display().to_string(),
                |name| name.to_string_lossy().into_owned(),
            )
        });
        let mut joining = vec![Waiter {
            id: waiter,
            worktree: source.worktree.clone(),
            who: who.clone(),
            since: Instant::now(),
        }];
        let mut reports = Vec::new();
        for id in superseded {
            let job = self.jobs.get_mut(&id).expect("listed above");
            let (mine, others) = std::mem::take(&mut job.waiters)
                .into_iter()
                .partition::<Vec<_>, _>(|waiter| waiter.worktree == source.worktree);
            job.waiters = others;
            if !mine.is_empty() {
                order = order.min(job.order);
                operation = operation
                    .merged(&job.operation)
                    .expect("a superseded job does what this one asks");
                reports.extend(mine.iter().map(|moved| EventKind::Replaced {
                    who: moved.who.clone(),
                    operation: job.operation.clone(),
                    from: job.revision.clone(),
                    to: revision.clone(),
                }));
                joining.extend(mine);
            }
            if job.waiters.is_empty() {
                self.jobs.remove(&id);
            }
        }

        let requested = EventKind::Requested {
            who,
            worktree: source.worktree.clone(),
            operation: operation.clone(),
            revision: revision.clone(),
            shared: existing.is_some(),
        };
        let id = existing.unwrap_or_else(|| {
            let id = JobId(self.next_job);
            self.next_job += 1;
            self.jobs.insert(
                id,
                Job {
                    repository: source.repository.clone(),
                    prefix: source.prefix.clone(),
                    revision,
                    operation: operation.clone(),
                    copy_to,
                    rerun_all,
                    waiters: Vec::new(),
                    order,
                    state: State::Queued,
                    output: Vec::new(),
                    compiled: 0,
                    fresh: 0,
                    units: HashSet::new(),
                    hold: None,
                    memory_needed: None,
                    skipped: 0,
                },
            );
            id
        });

        let mut effects = std::iter::once(requested)
            .chain(reports)
            .map(Effect::Report)
            .collect::<Vec<_>>();
        let job = self.jobs.get_mut(&id).expect("found or inserted above");
        if job.queued() {
            job.operation = job
                .operation
                .merged(&operation)
                .expect("a joined job does what this one asks");
        }
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
                let job = self.jobs.remove(&id).expect("found above");
                vec![Effect::Report(EventKind::Dropped {
                    operation: job.operation,
                    revision: job.revision,
                })]
            }
            State::Running {
                cancelled, slot, ..
            } => {
                *cancelled = true;
                let slot = &self.slots[*slot];
                vec![
                    Effect::Cancel { job: id },
                    Effect::Report(EventKind::Cancelled {
                        slot: slot_name(&slot.repository, slot.index),
                        operation: job.operation.clone(),
                        revision: job.revision.clone(),
                    }),
                ]
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

    /// Cargo reported a crate of running job `id`: compiled, or up to date
    /// when `fresh`, as compiled `unit` when its files name one.
    pub(crate) fn crate_built(&mut self, id: JobId, fresh: bool, unit: Option<String>) {
        // Like output, a report can arrive after the build ended.
        if let Some(job) = self.jobs.get_mut(&id) {
            if fresh {
                job.fresh += 1;
            } else {
                job.compiled += 1;
            }
        }
        if let Some(unit) = unit {
            self.unit_used(id, unit);
        }
    }

    /// Cargo reported that running job `id` finished compiling. Returns
    /// whether its tests run now.
    pub(crate) fn compiled(&mut self, id: JobId) -> bool {
        // Like output, a report can arrive after the build ended.
        let Some(Job {
            state: State::Running { compiled_at, .. },
            operation,
            ..
        }) = self.jobs.get_mut(&id)
        else {
            return false;
        };
        compiled_at.get_or_insert_with(Instant::now);
        operation.command.runs_tests()
    }

    /// Cargo succeeded for running job `id`, whose executables are being
    /// copied out of its slot now.
    pub(crate) fn copying(&mut self, id: JobId) {
        let Some(Job {
            state: State::Running { copying, .. },
            ..
        }) = self.jobs.get_mut(&id)
        else {
            panic!("only a running job copies");
        };
        *copying = true;
    }

    /// Running job `id` ran its tests: the binaries in `passed` passed, and
    /// `skipped` were not run because they had passed unchanged before.
    pub(crate) fn tested(&mut self, id: JobId, passed: Vec<PassedBinary>, skipped: u64) {
        let job = self
            .jobs
            .get_mut(&id)
            .expect("a job reports its tests while it runs");
        let State::Running { slot, .. } = job.state else {
            panic!("only running jobs test");
        };
        job.skipped = skipped;
        let records = &mut self.slots[slot].passed;
        for record in passed {
            records.retain(|kept| kept.id != record.id);
            records.push(record);
        }
    }

    /// Running job `id` used compiled `unit`, such as a build script's output.
    pub(crate) fn unit_used(&mut self, id: JobId, unit: String) {
        // Like output, a report can arrive after the build ended.
        let Some(job) = self.jobs.get_mut(&id) else {
            return;
        };
        let State::Running { slot, .. } = job.state else {
            panic!("only running jobs use units");
        };
        job.units.insert(unit.clone());
        self.slots[slot].units.insert(unit, now());
    }

    /// Running job `id` ended with `outcome`, having used `usage` when Cargo
    /// ran.
    pub(crate) fn exited(
        &mut self,
        id: JobId,
        outcome: &Outcome,
        usage: Option<Usage>,
    ) -> Vec<Effect> {
        let mut job = self.jobs.remove(&id).expect("only tracked jobs run");
        let State::Running {
            slot,
            started,
            compiled_at,
            ..
        } = job.state
        else {
            panic!("only running jobs exit");
        };
        let test_ms = compiled_at
            .filter(|_| job.operation.command.runs_tests())
            .map(|compiled_at| millis(compiled_at.elapsed()));
        let compilation = Compilation::of(&job);
        let entry = &mut self.slots[slot];
        // Cargo ran to its end, also when the job's own crates did not
        // compile; a cancelled or killed build tells less.
        let build_ms = millis(started.elapsed());
        if let Outcome::Exited { code } = outcome {
            entry.ran(
                compilation,
                std::mem::take(&mut job.units),
                *code == 0,
                build_ms,
                usage.map(|usage| usage.peak_memory),
            );
        }
        let persist = entry.persist();
        entry.job = None;
        entry.unmeasured += 1;
        entry.idle_since = Instant::now();
        // What compiling one unit costs here, for weighing cold builds.
        let compile_ms = build_ms - test_ms.unwrap_or(0).min(build_ms);
        if job.compiled >= UNIT_COST_SAMPLE {
            let sample = compile_ms / job.compiled;
            let cost = self.unit_ms.entry(job.repository.clone()).or_insert(sample);
            *cost = (*cost * 3 + sample) / 4;
        }
        let entry = &mut self.slots[slot];
        entry.last = Some(LastBuild {
            operation: job.operation.clone(),
            outcome: outcome.clone(),
            build_ms,
            test_ms,
        });
        let mut effects = vec![Effect::Report(EventKind::Finished {
            slot: slot_name(&entry.repository, entry.index),
            who: job.who(),
            operation: job.operation.clone(),
            revision: job.revision.clone(),
            outcome: outcome.clone(),
            build_ms,
            test_ms,
            skipped: job.skipped,
            queued_ms: job
                .waiters
                .iter()
                .map(|waiter| millis(started.saturating_duration_since(waiter.since)))
                .max()
                // A cancelled build has no waiters left.
                .unwrap_or(0),
            compiled: job.compiled,
            fresh: job.fresh,
            usage,
        })];
        effects.push(persist);
        if entry.unmeasured >= MAINTENANCE_DUE {
            effects.push(self.maintain(slot));
        }
        for waiter in job.waiters {
            self.waiting.remove(&waiter.id);
            effects.push(Effect::Send {
                waiter: waiter.id,
                message: Message::Finished {
                    revision: job.revision.clone(),
                    outcome: outcome.clone(),
                    queued_ms: millis(started.saturating_duration_since(waiter.since)),
                    build_ms,
                    test_ms,
                },
            });
        }
        effects.extend(self.start_ready());
        effects
    }

    fn maintain(&mut self, index: usize) -> Effect {
        Effect::Maintain(self.hand_out(index))
    }

    /// Marks idle slot `index` busy while it frees disk.
    fn hand_out(&mut self, index: usize) -> IdleSlot {
        let slot = &mut self.slots[index];
        assert!(!slot.busy(), "only an idle slot is handed out");
        slot.maintaining = true;
        IdleSlot {
            key: SlotKey(index),
            repository: slot.repository.clone(),
            slot: slot.index,
            used: slot.units.clone(),
        }
    }

    /// Hands out every idle slot to free `needed` bytes of disk; None when
    /// every slot is busy. Each is [`Self::maintained`] afterwards.
    pub(crate) fn reclaim(&mut self, needed: u64) -> Option<Effect> {
        let idle = (0..self.slots.len())
            .filter(|index| !self.slots[*index].busy())
            .collect::<Vec<_>>();
        if idle.is_empty() {
            return None;
        }
        let slots = idle.into_iter().map(|index| self.hand_out(index)).collect();
        Some(Effect::Reclaim { slots, needed })
    }

    /// Slot `key`'s target holds `crates` crates in several variants; a
    /// rise is reported.
    pub(crate) fn duplicated(&mut self, key: SlotKey, crates: usize) -> Option<Effect> {
        let slot = &mut self.slots[key.0];
        let rose = crates > slot.duplicated.unwrap_or(0);
        slot.duplicated = Some(crates);
        rose.then(|| {
            Effect::Report(EventKind::Duplicated {
                slot: slot_name(&slot.repository, slot.index),
                crates,
            })
        })
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

    /// Slot `key` was kept within its disk limit by `pruning`, which removed
    /// compiled units `evicted`; None when that failed.
    pub(crate) fn maintained(
        &mut self,
        key: SlotKey,
        pruning: Option<Pruning>,
        evicted: &[String],
    ) -> Vec<Effect> {
        self.handed_back(key, pruning, evicted, true)
    }

    /// Slot `key` gave up what builds used longest ago to free disk, as
    /// `pruning` says, removing compiled units `evicted`. Its own limit was
    /// not looked at, so it stays due for maintenance.
    pub(crate) fn reclaimed(
        &mut self,
        key: SlotKey,
        pruning: Option<Pruning>,
        evicted: &[String],
    ) -> Vec<Effect> {
        self.handed_back(key, pruning, evicted, false)
    }

    /// A slot handed out to free disk is back; its limit was kept when
    /// `limit_kept`.
    fn handed_back(
        &mut self,
        key: SlotKey,
        pruning: Option<Pruning>,
        evicted: &[String],
        limit_kept: bool,
    ) -> Vec<Effect> {
        let slot = &mut self.slots[key.0];
        assert!(slot.maintaining, "only a slot handed out comes back");
        slot.maintaining = false;
        if limit_kept {
            slot.unmeasured = 0;
        } else {
            slot.unmeasured = slot.unmeasured.max(1);
        }
        slot.size = pruning.map(Pruning::size);
        slot.undersized = pruning.is_some_and(Pruning::undersized);
        let mut effects = Vec::new();
        for unit in evicted {
            slot.units.remove(unit);
        }
        // What compilations need stays known: it holds for every slot.
        if let Some(Pruning::Cleared { .. }) = pruning {
            slot.units.clear();
        }
        if !evicted.is_empty() || matches!(pruning, Some(Pruning::Cleared { .. })) {
            effects.push(slot.persist());
        }
        let name = slot_name(&slot.repository, slot.index);
        let report = match pruning {
            None | Some(Pruning::Within { .. }) => None,
            Some(Pruning::Evicted {
                before,
                after,
                caches,
                units,
                in_use,
            }) => Some(EventKind::Pruned {
                slot: name,
                before,
                after,
                caches,
                units,
                in_use,
                cleared: false,
            }),
            Some(Pruning::Cleared { before }) => Some(EventKind::Pruned {
                slot: name,
                before,
                after: 0,
                caches: 0,
                units: 0,
                in_use: 0,
                cleared: true,
            }),
        };
        effects.extend(report.map(Effect::Report));
        effects.extend(self.start_ready());
        effects
    }

    /// Starts queued builds, next first, while slots are free. A build that
    /// waits for its repository's slot lets later builds of other
    /// repositories go first.
    fn start_ready(&mut self) -> Vec<Effect> {
        let mut effects = Vec::new();
        let mut in_use = self.memory_in_use();
        for id in self.queue_order() {
            if self.slots.iter().filter(|slot| slot.busy()).count() >= self.capacity {
                break;
            }
            // A build that does not fit waits, and so does everything behind
            // it: smaller builds slipping past would keep it waiting for good.
            // With nothing running, it runs whatever it needs.
            let memory = self.memory_estimate(&self.jobs[&id]);
            if in_use > 0 && in_use.saturating_add(memory) > self.memory {
                self.jobs.get_mut(&id).expect("queued above").memory_needed = Some(memory);
                break;
            }
            let (slot, warm) = match self.choose_slot(id) {
                Placement::Start { slot, warm } => (slot, warm),
                Placement::Wait => {
                    self.jobs.get_mut(&id).expect("queued above").hold = None;
                    continue;
                }
                Placement::Hold(hold) => {
                    let job = self.jobs.get_mut(&id).expect("queued above");
                    if job.hold.is_none() {
                        effects.push(Effect::Report(EventKind::Held {
                            who: job.who(),
                            operation: job.operation.clone(),
                            hold: hold.clone(),
                        }));
                    }
                    job.hold = Some(hold);
                    continue;
                }
            };
            in_use += memory;
            self.clock += 1;
            let job = &self.jobs[&id];
            let first = self.built_worktrees.insert(job.waiters[0].worktree.clone());
            let entry = &mut self.slots[slot];
            entry.job = Some(id);
            entry.revision = job.revision.clone();
            entry.worktree = Some(job.waiters[0].worktree.clone());
            entry.used = self.clock;
            let job = self.jobs.get_mut(&id).expect("chosen above");
            job.state = State::Running {
                slot,
                started: Instant::now(),
                compiled_at: None,
                copying: false,
                cancelled: false,
                memory,
            };
            let name = slot_name(&self.slots[slot].repository, self.slots[slot].index);
            let started = Message::Started {
                revision: job.revision.clone(),
                slot: name.clone(),
            };
            effects.extend(job.send_all(&started));
            effects.push(Effect::Report(EventKind::Started {
                slot: name,
                who: job.who(),
                operation: job.operation.clone(),
                revision: job.revision.clone(),
                warm,
                first,
            }));
            effects.push(Effect::Start(Start {
                job: id,
                key: SlotKey(slot),
                repository: job.repository.clone(),
                slot: self.slots[slot].index,
                prefix: job.prefix.clone(),
                revision: job.revision.clone(),
                operation: job.operation.clone(),
                copy_to: job.copy_to.clone(),
                passed: if job.operation.runs_every_test() && !job.rerun_all {
                    self.slots[slot].passed.clone()
                } else {
                    Vec::new()
                },
            }));
        }
        effects
    }

    /// The queued jobs, the one to start first first: shortest expected
    /// build minus time waited, so short builds go ahead and a long one goes
    /// once it has waited its own length; then submission order.
    fn queue_order(&self) -> Vec<JobId> {
        let mut queued = self
            .jobs
            .iter()
            .filter(|(_, job)| job.queued())
            .map(|(id, job)| {
                let waited = job.waited_ms();
                let expected = self
                    .estimate(job)
                    .or_else(|| typical(&self.slots, &job.repository))
                    .unwrap_or(0);
                (expected.saturating_sub(waited), job.order, *id)
            })
            .collect::<Vec<_>>();
        queued.sort_unstable();
        queued.into_iter().map(|(_, _, id)| id).collect()
    }

    /// What the running builds are expected to use at their peaks.
    fn memory_in_use(&self) -> u64 {
        self.jobs
            .values()
            .filter_map(|job| match job.state {
                State::Running { memory, .. } => Some(memory),
                State::Queued => None,
            })
            .sum()
    }

    /// Memory running builds may use together, and what the running builds
    /// are expected to use.
    pub(crate) fn memory(&self) -> (u64, u64) {
        (self.memory, self.memory_in_use())
    }

    /// What a build of `job` is expected to use at its peak: what the latest
    /// build of its compilation used, or the most any build of the same
    /// command used; nothing when no build of it ran.
    fn memory_estimate(&self, job: &Job) -> u64 {
        let compilation = Compilation::of(job);
        let runs = || {
            self.slots
                .iter()
                .filter(|slot| slot.repository == job.repository)
                .flat_map(|slot| &slot.compilations)
        };
        runs()
            .filter(|run| run.compilation == compilation && run.peak_memory.is_some())
            .max_by_key(|run| run.at)
            .and_then(|run| run.peak_memory)
            .or_else(|| {
                runs()
                    .filter(|run| run.compilation.command == compilation.command)
                    .filter_map(|run| run.peak_memory)
                    .max()
            })
            .unwrap_or(0)
    }

    /// How long a build of `job` is expected to take: the latest successful
    /// build of its compilation, or the average of builds of the same
    /// command in the same directory.
    fn estimate(&self, job: &Job) -> Option<u64> {
        let compilation = Compilation::of(job);
        let runs = || {
            self.slots
                .iter()
                .filter(|slot| slot.repository == job.repository)
                .flat_map(|slot| &slot.compilations)
        };
        if let Some(run) = runs()
            .filter(|run| run.compilation == compilation && run.build_ms.is_some())
            .max_by_key(|run| run.at)
        {
            return run.build_ms;
        }
        average(runs().filter(|run| {
            run.compilation.prefix == compilation.prefix
                && run.compilation.command == compilation.command
        }))
    }

    /// The slot queued job `id` runs in now, as the module explains, and
    /// whether that slot holds everything the build needs; or why it waits.
    fn choose_slot(&mut self, id: JobId) -> Placement {
        let job = &self.jobs[&id];
        let repository = job.repository.clone();
        let revision = job.revision.clone();
        let (needed, known) = needed(&self.slots, &repository, &Compilation::of(job));
        // Ranked by the work Cargo has, then by recent use.
        let mut best: Option<((u64, u64, usize), bool)> = None;
        for (index, slot) in self.slots.iter().enumerate() {
            if slot.repository != repository || slot.busy() {
                continue;
            }
            let missing = needed
                .iter()
                .filter(|unit| !slot.units.contains_key(**unit))
                .count() as u64;
            let distance = self
                .distance
                .distance(&repository, &slot.revision, &revision);
            let rank = (
                missing.saturating_add(distance),
                u64::MAX - slot.used,
                index,
            );
            if best.is_none_or(|(best, _)| rank < best) {
                best = Some((rank, known && missing == 0));
            }
        }
        if let Some(((work, _, index), warm)) = best {
            // Owned: weighing busy slots measures distances, which need the
            // scheduler mutably.
            let needed = needed.into_iter().map(str::to_owned).collect();
            return match self.hold(id, &needed, work) {
                Some(hold) => Placement::Hold(hold),
                None => Placement::Start { slot: index, warm },
            };
        }
        let own = || {
            self.slots
                .iter()
                .filter(|slot| slot.repository == repository)
        };
        // A slot being maintained is free again soon, and warm.
        if own().any(|slot| slot.maintaining) || own().count() >= self.capacity {
            return Placement::Wait;
        }
        // Restored slots can leave gaps: take the lowest free index.
        let index = (0..self.capacity)
            .find(|index| !own().any(|slot| slot.index == *index))
            .expect("a repository with fewer than capacity slots has a free index");
        self.slots.push(Slot {
            repository,
            index,
            job: None,
            revision,
            worktree: Some(job.waiters[0].worktree.clone()),
            used: 0,
            maintaining: false,
            unmeasured: 0,
            idle_since: Instant::now(),
            size: None,
            undersized: false,
            last: None,
            duplicated: None,
            passed: Vec::new(),
            compilations: Vec::new(),
            units: HashMap::new(),
        });
        Placement::Start {
            slot: self.slots.len() - 1,
            warm: false,
        }
    }

    /// Whether queued job `id`, whose best idle slot leaves `work` units to
    /// compile, should wait for a busy slot of its repository instead: the
    /// busy slot's build is expected to end, and the job to build there,
    /// sooner than the idle slot would compile its share cold. Without
    /// estimates it never waits.
    fn hold(&mut self, id: JobId, needed: &HashSet<String>, work: u64) -> Option<Hold> {
        let job = &self.jobs[&id];
        let unit_ms = *self.unit_ms.get(&job.repository)?;
        let cold_ms = work.saturating_mul(unit_ms);
        let mut best: Option<Hold> = None;
        for slot in &self.slots {
            let Some(running) = slot.job.filter(|_| slot.repository == job.repository) else {
                continue;
            };
            let running = &self.jobs[&running];
            let State::Running { started, .. } = running.state else {
                panic!("a slot's job runs");
            };
            let elapsed = millis(started.elapsed());
            // A build far past its estimate says nothing about its end.
            let Some(expected) = self
                .estimate(running)
                .filter(|expected| elapsed <= expected.saturating_mul(2))
            else {
                continue;
            };
            let missing = needed
                .iter()
                .filter(|unit| !slot.units.contains_key(unit.as_str()))
                .count() as u64;
            let distance =
                self.distance
                    .distance(&job.repository, &running.revision, &job.revision);
            let wait_ms = expected
                .saturating_sub(elapsed)
                .saturating_add(missing.saturating_add(distance).saturating_mul(unit_ms));
            if wait_ms < cold_ms && best.as_ref().is_none_or(|best| wait_ms < best.wait_ms) {
                best = Some(Hold {
                    slot: slot_name(&slot.repository, slot.index),
                    wait_ms,
                    cold_ms,
                });
            }
        }
        best
    }

    /// The slots and the queue, next build first, with the jobs each running
    /// build is charged as `tokens` says.
    pub(crate) fn status(
        &self,
        tokens: impl Fn(JobId) -> usize,
    ) -> (Vec<SlotStatus>, Vec<QueuedBuild>) {
        let slots = self
            .slots
            .iter()
            .map(|slot| SlotStatus {
                name: slot_name(&slot.repository, slot.index),
                worktree: slot.worktree.clone(),
                size: slot.size,
                maintaining: slot.maintaining,
                undersized: slot.undersized,
                build: slot.job.map(|id| {
                    let tokens = tokens(id);
                    let job = &self.jobs[&id];
                    let State::Running {
                        started,
                        compiled_at,
                        copying,
                        cancelled,
                        ..
                    } = job.state
                    else {
                        panic!("a slot's job runs");
                    };
                    RunningBuild {
                        revision: job.revision.clone(),
                        operation: job.operation.clone(),
                        who: job.who(),
                        elapsed_ms: millis(started.elapsed()),
                        compiled: job.compiled,
                        fresh: job.fresh,
                        cancelled,
                        tokens,
                        phase: if copying {
                            Phase::Copying
                        } else if job.operation.command.runs_tests() && compiled_at.is_some() {
                            Phase::Testing
                        } else {
                            Phase::Compiling
                        },
                    }
                }),
                last: slot.last.clone(),
                duplicated: slot.duplicated,
            })
            .collect();
        let queue = self
            .queue_order()
            .into_iter()
            .map(|id| {
                let job = &self.jobs[&id];
                QueuedBuild {
                    revision: job.revision.clone(),
                    operation: job.operation.clone(),
                    who: job.who(),
                    waited_ms: job.waited_ms(),
                    estimate_ms: self.estimate(job),
                    held: job.hold.clone(),
                    memory_needed: job.memory_needed,
                }
            })
            .collect();
        (slots, queue)
    }
}

/// Where a queued job goes now.
enum Placement {
    Start {
        slot: usize,
        warm: bool,
    },
    /// No slot is free for it.
    Wait,
    /// It waits for a busy slot although one is idle.
    Hold(Hold),
}

/// The average wall time of `runs` that succeeded, if any did.
fn average<'a>(runs: impl Iterator<Item = &'a CompilationRun>) -> Option<u64> {
    let (total, count) = runs
        .filter_map(|run| run.build_ms)
        .fold((0, 0), |(total, count), ms| (total + ms, count + 1));
    (count > 0).then(|| total / count)
}

/// How long a build of `repository` takes on average, for a build nothing
/// is known about.
fn typical(slots: &[Slot], repository: &Path) -> Option<u64> {
    average(
        slots
            .iter()
            .filter(|slot| slot.repository == repository)
            .flat_map(|slot| &slot.compilations),
    )
}

/// The compiled units a build of `compilation` in `repository` needs, as the
/// module explains, and whether a build of it ran before.
fn needed<'a>(
    slots: &'a [Slot],
    repository: &Path,
    compilation: &Compilation,
) -> (HashSet<&'a str>, bool) {
    let mut latest = HashMap::<&Compilation, &CompilationRun>::new();
    for run in slots
        .iter()
        .filter(|slot| slot.repository == repository)
        .flat_map(|slot| &slot.compilations)
    {
        let kept = latest.entry(&run.compilation).or_insert(run);
        if run.at > kept.at {
            *kept = run;
        }
    }
    if let Some(run) = latest.get(compilation) {
        return (run.units.iter().map(String::as_str).collect(), true);
    }
    let alike = latest
        .values()
        .filter(|run| {
            run.compilation.prefix == compilation.prefix
                && run.compilation.command == compilation.command
        })
        .flat_map(|run| run.units.iter().map(String::as_str))
        .collect();
    (alike, false)
}

/// Now, in seconds since the Unix epoch.
fn now() -> u64 {
    SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .expect("the clock is past 1970")
        .as_secs()
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
        Scheduler::new(capacity, u64::MAX, Table::default())
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
            rustflags: Vec::new(),
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
            label: None,
            copy_to: None,
            rerun_all: false,
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
                rustflags: Vec::new(),
            },
            label: None,
            copy_to: None,
            rerun_all: false,
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
                Effect::Maintain(IdleSlot { slot, .. }) => Some(*slot),
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

    fn reports(effects: &[Effect]) -> Vec<&EventKind> {
        effects
            .iter()
            .filter_map(|effect| match effect {
                Effect::Report(kind) => Some(kind),
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
        scheduler.exited(job, &ok(), None);
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

        let finished = scheduler.exited(job, &ok(), None);
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
        let (_, queue) = scheduler.status(|_| 0);
        let order = queue
            .iter()
            .map(|build| (build.revision.to_string(), build.who.len()))
            .collect::<Vec<_>>();
        assert_eq!(
            order,
            [("t1".into(), 1), ("t2".into(), 2), ("u1".into(), 1)]
        );

        let next = scheduler.exited(running, &ok(), None);
        assert_eq!(
            starts(&next)
                .iter()
                .map(|s| s.2.clone())
                .collect::<Vec<_>>(),
            ["t1"]
        );
    }

    #[test]
    fn requests_copying_elsewhere_or_with_other_rustflags_are_other_builds() {
        let mut scheduler = scheduler(1);
        let request = |waiter: u64, copy_to: Option<&str>, rustflags: &[&str]| Submission {
            waiter: WaiterId(waiter),
            source: source("/a"),
            revision: revision("t1"),
            operation: Operation {
                rustflags: rustflags.iter().map(|flag| (*flag).to_owned()).collect(),
                ..check()
            },
            label: None,
            copy_to: copy_to.map(PathBuf::from),
            rerun_all: false,
        };
        let first = scheduler.submit(request(1, Some("/out"), &[]));
        assert_eq!(starts(&first).len(), 1);
        let joined = scheduler.submit(request(2, Some("/out"), &[]));
        assert!(matches!(
            reports(&joined)[..],
            [EventKind::Requested { shared: true, .. }]
        ));
        for (waiter, copy_to, rustflags) in [
            (3, None, &[][..]),
            (4, Some("/elsewhere"), &[]),
            (5, Some("/out"), &["--cfg", "x"]),
        ] {
            let other = scheduler.submit(request(waiter, copy_to, rustflags));
            assert!(
                matches!(
                    reports(&other)[..],
                    [EventKind::Requested { shared: false, .. }]
                ),
                "{other:?}"
            );
        }
        assert_eq!(scheduler.status(|_| 0).1.len(), 3);
    }

    #[test]
    fn test_requests_differing_in_no_fail_fast_merge_and_the_build_has_it() {
        let mut scheduler = scheduler(1);
        let busy = submit(&mut scheduler, 1, "/busy", "t0");
        let [(running, ..)] = starts(&busy)[..] else {
            panic!("{busy:?}");
        };
        submit_operation(
            &mut scheduler,
            2,
            "/a",
            "t1",
            Command::Test,
            &["--workspace"],
        );
        let newer = submit_operation(
            &mut scheduler,
            3,
            "/a",
            "t2",
            Command::Test,
            &["--workspace", "--no-fail-fast"],
        );
        assert!(matches!(
            reports(&newer)[..],
            [_, EventKind::Replaced { .. }]
        ));
        // Another worktree at the same tree, without the flag, joins.
        let joined = submit_operation(
            &mut scheduler,
            4,
            "/b",
            "t2",
            Command::Test,
            &["--workspace"],
        );
        assert!(matches!(
            reports(&joined)[..],
            [EventKind::Requested { shared: true, .. }]
        ));
        let (_, queue) = scheduler.status(|_| 0);
        let [build] = &queue[..] else {
            panic!("{queue:?}");
        };
        assert_eq!(build.operation.args, ["--no-fail-fast", "--workspace"]);
        assert_eq!(build.who.len(), 3);
        let next = scheduler.exited(running, &ok(), None);
        let [(job, ..)] = starts(&next)[..] else {
            panic!("{next:?}");
        };
        scheduler.exited(job, &ok(), None);
        // A running fail-fast build is not joined by a request wanting more.
        let running = submit_operation(
            &mut scheduler,
            5,
            "/c",
            "t3",
            Command::Test,
            &["--workspace"],
        );
        assert_eq!(starts(&running).len(), 1);
        let wanting_more = submit_operation(
            &mut scheduler,
            6,
            "/d",
            "t3",
            Command::Test,
            &["--no-fail-fast", "--workspace"],
        );
        assert!(matches!(
            reports(&wanting_more)[..],
            [EventKind::Requested { shared: false, .. }]
        ));
    }

    #[test]
    fn a_build_nobody_waits_for_is_dropped_or_cancelled_and_not_joined() {
        let mut scheduler = scheduler(1);
        let first = submit(&mut scheduler, 1, "/a", "t1");
        let [(job, ..)] = starts(&first)[..] else {
            panic!("{first:?}");
        };
        submit(&mut scheduler, 2, "/b", "t2");
        let dropped = scheduler.withdraw(WaiterId(2));
        assert!(
            matches!(reports(&dropped)[..], [EventKind::Dropped { revision, .. }] if revision.to_string() == "t2")
        );
        assert!(scheduler.status(|_| 0).1.is_empty());

        let cancelled = scheduler.withdraw(WaiterId(1));
        assert_eq!(cancelled[0], Effect::Cancel { job });
        assert!(matches!(
            reports(&cancelled)[..],
            [EventKind::Cancelled { .. }]
        ));
        // An equal request does not join the build being stopped.
        let again = submit(&mut scheduler, 3, "/a", "t1");
        assert!(matches!(sent(&again, 3)[..], [Message::Queued { .. }]));
        let next = scheduler.exited(job, &Outcome::Signaled { signal: 15 }, None);
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
        scheduler.exited(job_a, &ok(), None);
        scheduler.exited(job_b, &ok(), None);

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
        assert_eq!(scheduler.status(|_| 0).0.len(), 2);
    }

    #[test]
    fn an_idle_slot_is_maintained_after_a_quiet_period_and_waited_for_meanwhile() {
        let mut scheduler = scheduler(2);
        let first = submit(&mut scheduler, 1, "/a", "a1");
        let [(job, 0, _)] = starts(&first)[..] else {
            panic!("{first:?}");
        };
        let ended = Instant::now();
        assert!(maintains(&scheduler.exited(job, &ok(), None)).is_empty());
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
            label: None,
            copy_to: None,
            rerun_all: false,
        });
        assert_eq!(starts(&other).len(), 1);
        let (slots, queue) = scheduler.status(|_| 0);
        assert!(slots[0].maintaining && slots[0].build.is_none());
        assert_eq!(queue.len(), 1);
        let ready = scheduler.maintained(SlotKey(0), Some(Pruning::Within { size: 5 }), &[]);
        assert!(
            matches!(starts(&ready)[..], [(_, 0, ref tree)] if tree == "a2"),
            "the waiting build takes its warm slot"
        );
        assert_eq!(scheduler.status(|_| 0).0[0].size, Some(5));
    }

    #[test]
    fn a_slot_busy_without_pause_is_maintained_after_enough_builds() {
        let mut scheduler = scheduler(1);
        for build in 1..MAINTENANCE_DUE {
            let effects = submit(&mut scheduler, u64::from(build), "/a", &format!("a{build}"));
            let [(job, ..)] = starts(&effects)[..] else {
                panic!("{effects:?}");
            };
            assert!(maintains(&scheduler.exited(job, &ok(), None)).is_empty());
        }
        let last = submit(&mut scheduler, 99, "/a", "last");
        let [(job, ..)] = starts(&last)[..] else {
            panic!("{last:?}");
        };
        assert_eq!(maintains(&scheduler.exited(job, &ok(), None)), [0]);
    }

    /// Two slots: slot 0 did `check` at tree a1, slot 1 did `test` at b1.
    /// `count` unit keys starting with `prefix`.
    fn units(prefix: &str, count: usize) -> Vec<String> {
        (0..count)
            .map(|unit| format!("debug/{prefix}{unit:015}"))
            .collect()
    }

    /// Reports that running job `job` used `units`.
    fn used(scheduler: &mut Scheduler<Table>, job: JobId, units: &[String]) {
        for unit in units {
            scheduler.crate_built(job, true, Some(unit.clone()));
        }
    }

    /// Slot 0 checked tree a1, using 12 units, and slot 1 tested tree b1,
    /// using 12 others.
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
        used(&mut scheduler, job_a, &units("c", 12));
        used(&mut scheduler, job_b, &units("t", 12));
        scheduler.exited(job_a, &ok(), None);
        scheduler.exited(job_b, &ok(), None);
        scheduler
    }

    #[test]
    fn a_build_takes_the_slot_where_cargo_has_least_to_do() {
        // Slot 1 is 1 package away but lacks the 12 units `check` needs;
        // slot 0 holds them, 10 packages away.
        let mut scheduler = checked_and_tested();
        scheduler.distance.0.insert(("b1".into(), "c1".into()), 1);
        assert_eq!(run(&mut scheduler, 3, "/c", "c1"), 0);
        // Nearer still, slot 1 wins: 12 units to compile beat 15 packages.
        let mut scheduler = checked_and_tested();
        scheduler.distance.0.insert(("b1".into(), "c1".into()), 1);
        scheduler.distance.0.insert(("a1".into(), "c1".into()), 15);
        assert_eq!(run(&mut scheduler, 3, "/c", "c1"), 1);
    }

    #[test]
    fn a_new_compilation_goes_where_builds_of_its_command_left_the_most() {
        // `test -p x --lib` never ran: it likely needs what `test` used, which
        // slot 1 holds, although slot 0 is closer.
        let mut scheduler = checked_and_tested();
        scheduler.distance.0.insert(("a1".into(), "c1".into()), 1);
        let effects = submit_operation(
            &mut scheduler,
            3,
            "/c",
            "c1",
            Command::Test,
            &["-p", "x", "--lib"],
        );
        assert!(matches!(starts(&effects)[..], [(_, 1, _)]), "{effects:?}");
        // Only a build of the compilation itself makes a slot warm.
        assert!(matches!(
            reports(&effects)[..],
            [_, EventKind::Started { warm: false, .. }]
        ));
    }

    #[test]
    fn a_build_waits_for_a_busy_warm_slot_when_that_beats_building_cold() {
        let mut scheduler = checked_and_tested();
        scheduler.slots[1].compilations[0].build_ms = Some(5000);
        let running = submit_operation(&mut scheduler, 3, "/b", "b2", Command::Test, &[]);
        let [(job, 1, _)] = starts(&running)[..] else {
            panic!("{running:?}");
        };
        // Compiling a unit takes a second: slot 0 lacks 12 units and is 10
        // packages away, 22 s cold; slot 1 ends in about 5 s and is 10
        // packages away, 15 s.
        scheduler.unit_ms.insert("/repo/.git".into(), 1000);
        let held = submit_operation(&mut scheduler, 4, "/c", "c1", Command::Test, &[]);
        assert!(starts(&held).is_empty(), "{held:?}");
        assert!(matches!(
            reports(&held)[..],
            [
                _,
                EventKind::Held {
                    hold: Hold {
                        wait_ms: 14_000..=15_000,
                        cold_ms: 22_000,
                        ..
                    },
                    ..
                }
            ]
        ));
        let (_, queue) = scheduler.status(|_| 0);
        assert!(queue[0].held.is_some());
        let next = scheduler.exited(job, &ok(), None);
        assert!(matches!(starts(&next)[..], [(_, 1, _)]), "{next:?}");
    }

    #[test]
    fn the_queue_runs_the_build_expected_to_be_shortest_first() {
        let mut scheduler = checked_and_tested();
        scheduler.slots[0].compilations[0].build_ms = Some(2_000);
        scheduler.slots[1].compilations[0].build_ms = Some(600_000);
        let busy = [
            submit(&mut scheduler, 3, "/x", "x1"),
            submit(&mut scheduler, 4, "/y", "y1"),
        ];
        let running = busy
            .iter()
            .flat_map(|effects| starts(effects))
            .map(|(job, ..)| job)
            .collect::<Vec<_>>();
        assert_eq!(running.len(), 2);
        submit_operation(&mut scheduler, 5, "/t", "t1", Command::Test, &[]);
        submit(&mut scheduler, 6, "/c", "c1");
        let (_, queue) = scheduler.status(|_| 0);
        let order = queue
            .iter()
            .map(|build| (build.operation.command, build.estimate_ms))
            .collect::<Vec<_>>();
        assert_eq!(
            order,
            [
                (Command::Check, Some(2_000)),
                (Command::Test, Some(600_000))
            ]
        );
        let next = scheduler.exited(running[0], &ok(), None);
        assert!(
            matches!(&starts(&next)[..], [(_, _, tree)] if tree == "c1"),
            "{next:?}"
        );
    }

    #[test]
    fn a_build_that_would_not_fit_in_memory_waits_and_so_does_everything_behind_it() {
        let mut scheduler = checked_and_tested();
        scheduler.memory = 10;
        scheduler.slots[0].compilations[0].peak_memory = Some(4);
        scheduler.slots[1].compilations[0].peak_memory = Some(8);
        let first = submit(&mut scheduler, 3, "/a", "a2");
        let [(job, ..)] = starts(&first)[..] else {
            panic!("{first:?}");
        };
        // 4 run; a test needs 8 and waits, and a check behind it waits too.
        let test = submit_operation(&mut scheduler, 4, "/t", "t1", Command::Test, &[]);
        assert!(starts(&test).is_empty());
        let check = submit(&mut scheduler, 5, "/c", "c1");
        assert!(starts(&check).is_empty(), "{check:?}");
        let (_, queue) = scheduler.status(|_| 0);
        assert_eq!(queue[0].memory_needed, Some(8), "{queue:?}");
        assert_eq!(scheduler.memory(), (10, 4));
        let next = scheduler.exited(job, &ok(), None);
        assert_eq!(starts(&next).len(), 1, "the test runs alone: {next:?}");
    }

    #[test]
    fn evicted_units_count_as_missing() {
        let mut scheduler = checked_and_tested();
        scheduler.distance.0.insert(("b1".into(), "c1".into()), 1);
        let maintain = scheduler.maintenance_due(Instant::now() + MAINTENANCE_QUIET);
        assert_eq!(maintains(&maintain), [0, 1]);
        let pruning = Pruning::Evicted {
            before: 2,
            after: 1,
            caches: 0,
            units: 12,
            in_use: 0,
        };
        scheduler.maintained(SlotKey(0), Some(pruning), &units("c", 12));
        scheduler.maintained(SlotKey(1), Some(Pruning::Within { size: 1 }), &[]);
        // Both slots lack what `check` needs; slot 1 is closer.
        assert_eq!(run(&mut scheduler, 3, "/c", "c1"), 1);
    }

    #[test]
    fn a_cleared_slot_forgets_its_units_but_not_what_compilations_need() {
        let mut scheduler = checked_and_tested();
        let maintain = scheduler.maintenance_due(Instant::now() + MAINTENANCE_QUIET);
        assert_eq!(maintains(&maintain), [0, 1]);
        scheduler.maintained(SlotKey(0), Some(Pruning::Cleared { before: 9 }), &[]);
        scheduler.maintained(SlotKey(1), Some(Pruning::Within { size: 1 }), &[]);
        assert!(scheduler.slots[0].units.is_empty());
        assert!(scheduler.status(|_| 0).0[0].undersized);
        assert_eq!(scheduler.slots[0].compilations.len(), 1);
        assert_eq!(scheduler.slots[1].units.len(), 12);
    }

    #[test]
    fn a_failed_build_adds_to_what_its_compilation_needs_and_a_successful_one_replaces_it() {
        let mut scheduler = scheduler(1);
        let mut build = |waiter: u64, tree: &str, units: &[String], code: i32| {
            let effects = submit(&mut scheduler, waiter, "/a", tree);
            let [(job, ..)] = starts(&effects)[..] else {
                panic!("{effects:?}");
            };
            used(&mut scheduler, job, units);
            scheduler.exited(job, &Outcome::Exited { code }, None);
            scheduler.slots[0].compilations[0].units.len()
        };
        let all = units("u", 6);
        assert_eq!(build(1, "a1", &all, 0), 6);
        // Cargo stopped at a compile error after 2 units, one of them new.
        assert_eq!(
            build(
                2,
                "a2",
                &[all[0].clone(), "debug/ffffffffffffffff".into()],
                101
            ),
            7
        );
        assert_eq!(build(3, "a3", &all[..4], 0), 4);
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
        let next = scheduler.exited(job, &ok(), None);
        let [(next, ..)] = starts(&next)[..] else {
            panic!("{next:?}")
        };
        assert_eq!(Compilation::of(&scheduler.jobs[&next]), first);
        assert_eq!(first.args, ["-p", "x"]);
    }

    #[test]
    fn every_decision_is_reported_for_people_watching() {
        let mut scheduler = scheduler(1);
        let first = scheduler.submit(Submission {
            waiter: WaiterId(1),
            source: source("/work/a"),
            revision: revision("t1"),
            operation: check(),
            label: Some("agent-1".into()),
            copy_to: None,
            rerun_all: false,
        });
        let [(job, ..)] = starts(&first)[..] else {
            panic!("{first:?}");
        };
        assert!(matches!(
            reports(&first)[..],
            [
                EventKind::Requested { who, shared: false, .. },
                EventKind::Started { warm: false, first: true, .. },
            ] if who == "agent-1"
        ));
        // Without a label, a request is named by its worktree's folder.
        let joined = submit(&mut scheduler, 2, "/work/b", "t1");
        assert!(matches!(
            reports(&joined)[..],
            [EventKind::Requested { who, shared: true, .. }] if who == "b"
        ));
        submit(&mut scheduler, 3, "/work/c", "c1");
        let replaced = submit(&mut scheduler, 4, "/work/c", "c2");
        assert!(matches!(
            reports(&replaced)[..],
            [
                EventKind::Requested { shared: false, .. },
                EventKind::Replaced { who, from, to, .. },
            ] if who == "c" && from.to_string() == "c1" && to.to_string() == "c2"
        ));

        scheduler.crate_built(job, false, Some("debug/0000000000000001".into()));
        scheduler.crate_built(job, true, Some("debug/0000000000000002".into()));
        scheduler.crate_built(job, true, None);
        let (slots, _) = scheduler.status(|_| 0);
        let running = slots[0].build.as_ref().expect("t1 runs");
        assert_eq!((running.compiled, running.fresh), (1, 2));
        assert_eq!(running.who, ["agent-1", "b"]);

        let usage = Usage {
            cpu_ms: 1200,
            peak_memory: 1 << 20,
        };
        let finished = scheduler.exited(job, &ok(), Some(usage));
        assert!(matches!(
            reports(&finished)[..],
            [
                EventKind::Finished { compiled: 1, fresh: 2, usage: Some(Usage { cpu_ms: 1200, .. }), who, .. },
                // The next build starts in the slot that just did `check`.
                EventKind::Started { warm: true, first: true, .. },
            ] if who.len() == 2
        ));
    }

    #[test]
    fn a_test_build_reports_the_time_its_tests_ran_and_its_slot_remembers_it() {
        let mut scheduler = scheduler(1);
        let effects = submit_operation(&mut scheduler, 1, "/a", "a1", Command::Test, &[]);
        let [(job, ..)] = starts(&effects)[..] else {
            panic!("{effects:?}");
        };
        assert_eq!(
            scheduler.status(|_| 0).0[0].build.as_ref().unwrap().phase,
            Phase::Compiling
        );
        scheduler.compiled(job);
        assert_eq!(
            scheduler.status(|_| 0).0[0].build.as_ref().unwrap().phase,
            Phase::Testing
        );
        std::thread::sleep(Duration::from_millis(20));
        let finished = scheduler.exited(job, &ok(), None);
        let [
            Message::Finished {
                test_ms: Some(test_ms),
                build_ms,
                ..
            },
        ] = sent(&finished, 1)[..]
        else {
            panic!("{finished:?}");
        };
        assert!(
            *test_ms >= 20 && test_ms <= build_ms,
            "{test_ms} of {build_ms}"
        );
        let last = scheduler.status(|_| 0).0[0]
            .last
            .clone()
            .expect("the slot built");
        assert_eq!(last.test_ms, Some(*test_ms));

        // Other commands finish compiling at their end: no test time.
        let effects = submit(&mut scheduler, 2, "/a", "a2");
        let [(job, ..)] = starts(&effects)[..] else {
            panic!("{effects:?}");
        };
        scheduler.compiled(job);
        let finished = scheduler.exited(job, &ok(), None);
        assert!(matches!(
            sent(&finished, 2)[..],
            [Message::Finished { test_ms: None, .. }]
        ));
    }

    fn persisted(effects: &[Effect]) -> Vec<(usize, &SlotRecord)> {
        effects
            .iter()
            .filter_map(|effect| match effect {
                Effect::Persist { slot, record, .. } => Some((*slot, record)),
                _ => None,
            })
            .collect()
    }

    #[test]
    fn a_finished_build_persists_its_slots_record() {
        let mut scheduler = scheduler(1);
        let first = submit(&mut scheduler, 1, "/work/a", "a1");
        let [(job, ..)] = starts(&first)[..] else {
            panic!("{first:?}");
        };
        used(&mut scheduler, job, &units("a", 3));
        let finished = scheduler.exited(job, &ok(), None);
        let [(0, record)] = persisted(&finished)[..] else {
            panic!("{finished:?}");
        };
        assert_eq!(record.worktree.as_deref(), Some(Path::new("/work/a")));
        let [run] = &record.compilations[..] else {
            panic!("{record:?}");
        };
        assert_eq!(run.compilation, Compilation::new(Path::new(""), &check()));
        assert_eq!(run.units.len(), 3);
        assert_eq!(record.units.len(), 3);
        // A cleared target forgets its units, and that is persisted too.
        scheduler.maintenance_due(Instant::now() + MAINTENANCE_QUIET);
        let cleared = scheduler.maintained(SlotKey(0), Some(Pruning::Cleared { before: 1 }), &[]);
        assert!(matches!(
            persisted(&cleared)[..],
            [(0, record)] if record.units.is_empty() && record.compilations.len() == 1
        ));
    }

    #[test]
    fn restored_slots_are_placed_like_warm_ones_and_new_slots_fill_gaps() {
        let mut scheduler = scheduler(3);
        let record = SlotRecord {
            worktree: Some("/work/a".into()),
            compilations: vec![CompilationRun {
                compilation: Compilation::new(Path::new(""), &check()),
                at: 1,
                units: units("c", 12).into_iter().collect(),
                build_ms: None,
                peak_memory: None,
            }],
            units: units("c", 12).into_iter().map(|unit| (unit, 1)).collect(),
            passed: Vec::new(),
        };
        scheduler.restore(
            "/repo/.git".into(),
            0,
            revision("x1"),
            SlotRecord::default(),
        );
        scheduler.restore("/repo/.git".into(), 2, revision("y1"), record);
        let (slots, _) = scheduler.status(|_| 0);
        assert_eq!(slots.len(), 2);
        assert!(slots.iter().all(|slot| slot.size.is_none()));
        // Slot 2 holds what `check` needs, so it wins over slot 0, which is
        // closer.
        scheduler.distance.0.insert(("x1".into(), "a2".into()), 1);
        let first = submit(&mut scheduler, 1, "/work/a", "a2");
        assert!(matches!(starts(&first)[..], [(_, 2, _)]));
        assert!(matches!(
            reports(&first)[..],
            [
                EventKind::Requested { .. },
                EventKind::Started { warm: true, .. }
            ]
        ));
        let second = submit(&mut scheduler, 2, "/work/b", "b1");
        assert!(matches!(starts(&second)[..], [(_, 0, _)]));
        // Both restored slots are busy: the new one takes the free index 1.
        let third = submit(&mut scheduler, 3, "/work/c", "c1");
        assert!(matches!(starts(&third)[..], [(_, 1, _)]));
    }

    #[test]
    fn reclaiming_disk_hands_out_every_idle_slot_and_starts_waiting_builds_after() {
        let mut scheduler = checked_and_tested();
        let busy = submit(&mut scheduler, 3, "/c", "c1");
        assert_eq!(starts(&busy).len(), 1);
        let Some(Effect::Reclaim { slots, needed: 7 }) = scheduler.reclaim(7) else {
            panic!("one slot is idle");
        };
        let [IdleSlot { key, .. }] = &slots[..] else {
            panic!("{slots:?}");
        };
        assert_eq!(scheduler.reclaim(7), None, "no slot is idle any more");
        let waiting = submit(&mut scheduler, 4, "/d", "d1");
        assert!(starts(&waiting).is_empty());
        let done = scheduler.reclaimed(*key, Some(Pruning::Within { size: 1 }), &[]);
        assert_eq!(starts(&done).len(), 1, "{done:?}");
        // Its limit still has to be kept after that build.
        assert!(scheduler.slots[key.0].unmeasured > 0);
    }

    #[test]
    fn units_builds_use_are_recorded_handed_to_maintenance_and_forgotten_when_evicted() {
        let mut scheduler = scheduler(1);
        let first = submit(&mut scheduler, 1, "/work/a", "a1");
        let [(job, ..)] = starts(&first)[..] else {
            panic!("{first:?}");
        };
        scheduler.crate_built(job, true, Some("debug/aaaaaaaaaaaaaaaa".into()));
        scheduler.unit_used(job, "debug/bbbbbbbbbbbbbbbb".into());
        let finished = scheduler.exited(job, &ok(), None);
        let [(0, record)] = persisted(&finished)[..] else {
            panic!("{finished:?}");
        };
        assert_eq!(
            record.units.keys().collect::<Vec<_>>(),
            ["debug/aaaaaaaaaaaaaaaa", "debug/bbbbbbbbbbbbbbbb"]
        );
        let maintain = scheduler.maintenance_due(Instant::now() + MAINTENANCE_QUIET);
        let [Effect::Maintain(IdleSlot { used, .. })] = &maintain[..] else {
            panic!("{maintain:?}");
        };
        assert_eq!(used.len(), 2);
        let pruning = Pruning::Evicted {
            before: 2,
            after: 1,
            caches: 0,
            units: 1,
            in_use: 0,
        };
        let done = scheduler.maintained(
            SlotKey(0),
            Some(pruning),
            &["debug/aaaaaaaaaaaaaaaa".into()],
        );
        assert!(matches!(persisted(&done)[..], [(0, record)] if record.units.len() == 1));
        assert!(matches!(
            reports(&done)[..],
            [EventKind::Pruned { units: 1, .. }]
        ));
    }
}
