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
//! - **Slots.** At most `capacity` slots are busy: running a build, being
//!   measured or giving up disk. A build takes the idle slot of its
//!   repository where Cargo has the least to do: the fewest compiled units
//!   the build needs and the slot lacks, plus the distance between the
//!   slot's tree and the build's. A build needs what the latest build of its
//!   [`Compilation`] used, in any slot; a compilation no build ran yet needs,
//!   as far as anyone knows, what the latest builds of the same command in
//!   the same directory used. It waits for a busy slot while one is idle
//!   only when that is expected to be faster: the busy slot's build ends
//!   (its compilation's last wall time) and the build compiles its share
//!   there sooner than the idle slot compiles its share cold, as recent
//!   builds' wall times grew with the units they compiled. Without
//!   estimates it never waits.
//! - **Queue order.** The shortest expected build first, less the time it
//!   has waited, so short builds go ahead of long ones and a long one goes
//!   once it has waited its own length.
//!   It gets a new slot only when every slot of its repository runs a build,
//!   so slot directories are created only when builds of a repository run
//!   concurrently.
//! - **Measurement.** A slot's target is measured once the slot has been
//!   idle for [`MEASURE_QUIET`], so the measurement does not delay the next
//!   build of a session that runs several in a row, and right after a build
//!   once [`MEASURE_DUE`] builds went unmeasured, so disk stays known under
//!   constant load. What an idle slot's target holds stays true until its
//!   next build.
//! - **Eviction.** When the slots' targets are over their budget, or free
//!   disk is under its floor, idle slots give up what builds used longest
//!   ago, across all of them, from what their measurements found.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant, SystemTime};

use crate::cargo::{Compilation, Operation};
use crate::passed::PassedBinary;
use crate::protocol::{
    BuildReport, Chore, EventKind, Hold, LastBuild, Message, Outcome, Phase, QueuedBuild,
    RunningBuild, Shortage, SlotStatus, Usage,
};
use crate::slot::{CompilationRun, Evictable, Inventory, Kind, LastUse, SlotRecord, slot_name};
use crate::snapshot::{Revision, Source};

/// How long a slot stays idle before its target is measured.
pub(crate) const MEASURE_QUIET: Duration = Duration::from_secs(2);
/// After this many unmeasured builds a slot is measured right after its
/// build, idle or not.
pub(crate) const MEASURE_DUE: u32 = 8;
/// What builds used this recently is in use: evicting it means the budget
/// or free disk is below what builds use.
const IN_USE: Duration = Duration::from_secs(10 * 60);
/// How many compilations a slot remembers what their latest build used,
/// the most recently run ones: many one-off selections can grow the record
/// written after every build.
const KEPT_COMPILATIONS: usize = 256;
/// How much an earlier build weighs in [`CompileCost`] each time a new one
/// is added: about the last hundred count.
const COMPILE_COST_DECAY: f64 = 0.99;

/// How far apart two trees of a repository are, as the work Cargo redoes
/// when a slot holding one is checked out to the other.
pub(crate) trait Distance {
    fn distance(&mut self, repository: &Path, from: &Revision, to: &Revision) -> u64;
}

/// A build the scheduler tracks.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Ord, PartialOrd, Hash)]
pub(crate) struct JobId(u64);

impl std::fmt::Display for JobId {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.0.fmt(formatter)
    }
}

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
    /// Optional work: see [`crate::protocol::BuildRequest::optional`].
    pub(crate) optional: bool,
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
    /// Measure the slot's target.
    Measure(Measurement),
    /// Remove what idle slots give up.
    Evict(Vec<Removal>),
    /// Something happened, for people watching.
    Report(EventKind),
    /// Slot `slot` of `repository` has a new record to keep on disk.
    Persist {
        repository: PathBuf,
        slot: usize,
        record: SlotRecord,
    },
}

/// Idle slot `slot` of `repository`, whose target is to be measured. It is
/// busy until the scheduler hears it was [`Scheduler::measured`].
#[derive(Debug, PartialEq)]
pub(crate) struct Measurement {
    pub(crate) key: SlotKey,
    pub(crate) repository: PathBuf,
    pub(crate) slot: usize,
}

/// What idle slot `key`, named `name`, gives up: `paths` in its target. It
/// is busy until the scheduler hears it was [`Scheduler::evicted`].
#[derive(Debug, PartialEq)]
pub(crate) struct Removal {
    pub(crate) key: SlotKey,
    pub(crate) name: String,
    pub(crate) paths: Vec<PathBuf>,
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
    /// Who asked, for its log.
    pub(crate) who: Vec<String>,
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
    /// It asked for optional work.
    optional: bool,
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
    /// The compiled units it used so far, each with its place in the order
    /// Cargo reported them.
    units: HashMap<String, usize>,
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
    /// What it does while no build runs in it.
    chore: Option<Chore>,
    /// Incarnation-local identity of the target contents. Advance before writers start.
    generation: u64,
    /// Builds since its target was last measured.
    unmeasured: u32,
    /// When its latest build ended.
    idle_since: Instant,
    /// Its target's disk usage when last measured, less what it gave up
    /// since.
    size: Option<u64>,
    /// What its target holds, from its measurement since its latest build.
    inventory: Option<Inventory>,
    /// How its latest build since the daemon started ended.
    last: Option<LastBuild>,
    /// Crates its target holds in several variants, when last counted.
    duplicated: Option<usize>,
    /// The test binaries that passed in it.
    passed: Vec<PassedBinary>,
    /// What its latest build of each compilation used, least recently run
    /// first. This order breaks ties in the recorded wall-clock times.
    compilations: Vec<CompilationRun>,
    /// When a build last used each compiled unit.
    units: HashMap<String, LastUse>,
}

impl Slot {
    fn advance_generation(&mut self) {
        self.generation = self
            .generation
            .checked_add(1)
            .expect("target generation exhausted");
    }

    fn busy(&self) -> bool {
        self.job.is_some() || self.chore.is_some()
    }

    /// When builds last used `item` of its target, as far as is known, and
    /// among the units one build used last, how many it reported after it:
    /// the order eviction goes in.
    fn last_use(&self, item: &Evictable) -> (SystemTime, u32) {
        let recorded = match &item.kind {
            Kind::Unit(unit) => self.units.get(unit),
            Kind::Cache | Kind::TestFiles => None,
        };
        recorded.map_or((item.written, 0), |used| {
            (
                SystemTime::UNIX_EPOCH + Duration::from_secs(used.at),
                used.after,
            )
        })
    }

    /// Its build of `compilation` used `units`: everything the compilation
    /// needs when the build `succeeded`. A failed build may have stopped
    /// early, so what earlier builds used stays.
    fn ran(
        &mut self,
        compilation: Compilation,
        units: Vec<String>,
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
            .iter()
            .position(|run| run.compilation == compilation)
        {
            Some(index) => {
                let mut run = self.compilations.remove(index);
                if succeeded {
                    run.units.clear();
                    run.build_ms = Some(build_ms);
                }
                if peak_memory.is_some() {
                    run.peak_memory = peak_memory;
                }
                run.units.extend(units);
                run.at = at;
                self.compilations.push(run);
            }
            None => self.compilations.push(CompilationRun {
                compilation,
                at,
                units: units.into_iter().collect(),
                build_ms: succeeded.then_some(build_ms),
                peak_memory,
            }),
        }
        while self.compilations.len() > KEPT_COMPILATIONS {
            self.compilations.remove(0);
        }
    }

    fn persist(&self) -> Effect {
        Effect::Persist {
            repository: self.repository.clone(),
            slot: self.index,
            record: SlotRecord {
                worktree: self.worktree.clone(),
                compilations: self.compilations.clone(),
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

/// How a repository's compile time grows with the units a build compiles:
/// a fixed part (starting Cargo, checking what is fresh, linking) and a part
/// per unit, fitted by least squares over its builds, earlier ones weighing
/// less. Cargo compiles units side by side, so one more unit costs far less
/// than a build's wall time divided by its units: in one workspace, about
/// 0.2 s against a 0.8 s average.
#[derive(Debug, Default)]
struct CompileCost {
    /// Weighted sums over the builds: of the weights, units, wall times in
    /// milliseconds, units squared, and units times wall time.
    weight: f64,
    units: f64,
    ms: f64,
    units_squared: f64,
    units_ms: f64,
}

impl CompileCost {
    /// A build compiled `units` units in `ms` milliseconds of wall time.
    fn add(&mut self, units: u64, ms: u64) {
        #[expect(
            clippy::cast_precision_loss,
            reason = "counts and times far below 2^52"
        )]
        let (units, ms) = (units as f64, ms as f64);
        for sum in [
            &mut self.weight,
            &mut self.units,
            &mut self.ms,
            &mut self.units_squared,
            &mut self.units_ms,
        ] {
            *sum *= COMPILE_COST_DECAY;
        }
        self.weight += 1.0;
        self.units += units;
        self.ms += ms;
        self.units_squared += units * units;
        self.units_ms += units * ms;
    }

    /// The wall time compiling `units` is expected to take, in milliseconds;
    /// None until builds that compiled different amounts were seen.
    fn ms(&self, units: u64) -> Option<u64> {
        // The units' weighted variance, times the weight squared: below one
        // unit squared, the builds tell nothing about one more unit.
        let spread = self.weight * self.units_squared - self.units * self.units;
        if spread < self.weight * self.weight {
            return None;
        }
        let per_unit = ((self.weight * self.units_ms - self.units * self.ms) / spread).max(0.0);
        let fixed = ((self.ms - per_unit * self.units) / self.weight).max(0.0);
        #[expect(clippy::cast_precision_loss, reason = "a unit count far below 2^52")]
        let units = units as f64;
        #[expect(
            clippy::cast_possible_truncation,
            clippy::cast_sign_loss,
            reason = "a positive number of milliseconds far below 2^64"
        )]
        Some((fixed + per_unit * units).round() as u64)
    }
}

/// An eviction under way: the shortage it covers, and the items of each
/// slot's inventory it removes, by slot index.
struct Eviction {
    shortage: Shortage,
    chosen: Vec<(usize, Vec<usize>)>,
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
    /// Per repository, how compile time grows with the units compiled.
    compile_cost: HashMap<PathBuf, CompileCost>,
    /// The eviction under way.
    evicting: Option<Eviction>,
    /// The latest eviction took what builds used in the last ten minutes.
    undersized: bool,
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

    /// Whether only optional work waits for it.
    fn optional(&self) -> bool {
        self.waiters.iter().all(|waiter| waiter.optional)
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
            compile_cost: HashMap::new(),
            evicting: None,
            undersized: false,
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
        let mut compilations = record.compilations;
        compilations.drain(..compilations.len().saturating_sub(KEPT_COMPILATIONS));
        self.slots.push(Slot {
            repository,
            index,
            job: None,
            revision,
            worktree: record.worktree,
            used: 0,
            chore: None,
            generation: 0,
            unmeasured: 1,
            idle_since: Instant::now(),
            size: None,
            inventory: None,
            last: None,
            duplicated: None,
            passed: record.passed,
            compilations,
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
            optional,
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
            optional,
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
                    units: HashMap::new(),
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
        let place = job.units.len();
        job.units.entry(unit.clone()).or_insert(place);
        // Held from now on, for builds weighing a wait for this slot; its
        // use is stamped when the build ends.
        self.slots[slot].units.entry(unit).or_insert(LastUse {
            at: now(),
            after: 0,
        });
    }

    /// Running job `id` ended with `outcome`, having used `usage` when Cargo
    /// ran.
    pub(crate) fn exited(
        &mut self,
        id: JobId,
        outcome: &Outcome,
        usage: Option<Usage>,
        report: BuildReport,
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
        // Every unit the build used ends its use now, in the order Cargo
        // reported them.
        let at = now();
        let reported = job.units.len();
        for (unit, place) in &job.units {
            let after = u32::try_from(reported - 1 - place).expect("a build uses few units");
            entry.units.insert(unit.clone(), LastUse { at, after });
        }
        // Cargo ran to its end, also when the job's own crates did not
        // compile; a cancelled or killed build tells less.
        let build_ms = millis(started.elapsed());
        if let Outcome::Exited { code } = outcome {
            entry.ran(
                compilation,
                std::mem::take(&mut job.units).into_keys().collect(),
                *code == 0,
                build_ms,
                usage.map(|usage| usage.peak_memory),
            );
        }
        let persist = entry.persist();
        entry.job = None;
        entry.unmeasured += 1;
        entry.idle_since = Instant::now();
        // How compile time grows with units here, for weighing cold builds.
        // A cancelled or killed build's time says nothing, and neither does
        // a command of the client's own, which compiles nothing.
        if let Outcome::Exited { .. } = outcome
            && job.operation.command.uses_cargo()
        {
            let compile_ms = build_ms - test_ms.unwrap_or(0).min(build_ms);
            self.compile_cost
                .entry(job.repository.clone())
                .or_default()
                .add(job.compiled, compile_ms);
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
            report,
        })];
        effects.push(persist);
        if entry.unmeasured >= MEASURE_DUE {
            effects.push(self.measure(slot));
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

    /// Marks idle slot `index` busy while its target is measured.
    fn measure(&mut self, index: usize) -> Effect {
        let slot = &mut self.slots[index];
        assert!(!slot.busy(), "only an idle slot is measured");
        slot.chore = Some(Chore::Measuring);
        Effect::Measure(Measurement {
            key: SlotKey(index),
            repository: slot.repository.clone(),
            slot: slot.index,
        })
    }

    /// Only the cache owner resolves wire slot identities to scheduler slots.
    pub(crate) fn cache_slots(&self) -> impl Iterator<Item = (String, bool)> + '_ {
        self.slots
            .iter()
            .map(|slot| (slot_name(&slot.repository, slot.index), slot.busy()))
    }

    pub(crate) fn cache_acquire(&mut self, name: &str) -> Option<SlotKey> {
        let index = self
            .slots
            .iter()
            .position(|slot| slot_name(&slot.repository, slot.index) == name && !slot.busy())?;
        self.slots[index].chore = Some(Chore::Cache);
        Some(SlotKey(index))
    }

    pub(crate) fn cache_identity(&self, key: SlotKey) -> (&Path, usize, u64) {
        let slot = &self.slots[key.0];
        assert_eq!(
            slot.chore,
            Some(Chore::Cache),
            "cache identity requires exclusion"
        );
        (&slot.repository, slot.index, slot.generation)
    }

    pub(crate) fn cache_generation(&self, name: &str) -> Option<u64> {
        self.slots
            .iter()
            .find(|slot| slot_name(&slot.repository, slot.index) == name)
            .map(|slot| slot.generation)
    }

    pub(crate) fn cache_writing(&mut self, key: SlotKey) {
        let slot = &mut self.slots[key.0];
        assert_eq!(
            slot.chore,
            Some(Chore::Cache),
            "cache writing requires exclusion"
        );
        slot.advance_generation();
    }

    pub(crate) fn cache_release(&mut self, key: SlotKey, mutated: bool) -> Vec<Effect> {
        let mut effects = Vec::new();
        let slot = &mut self.slots[key.0];
        assert_eq!(
            slot.chore,
            Some(Chore::Cache),
            "only an acquired cache slot comes back"
        );
        slot.chore = None;
        if mutated {
            // A partially removed unit cannot substantiate a warm compilation
            // or a previously passed test. Retain no successful-test claims.
            slot.units.clear();
            slot.passed.clear();
            slot.compilations.clear();
            slot.size = None;
            slot.inventory = None;
            slot.duplicated = None;
            // Selected cleanup never schedules whole-target pruning. The next
            // build schedules the usual independent maintenance again.
            slot.unmeasured = 0;
            effects.push(slot.persist());
        }
        effects.extend(self.start_ready());
        effects
    }

    /// When the next idle slot is due for measurement, if one will be.
    pub(crate) fn next_measurement(&self) -> Option<Instant> {
        self.slots
            .iter()
            .filter(|slot| !slot.busy() && slot.unmeasured > 0)
            .map(|slot| slot.idle_since + MEASURE_QUIET)
            .min()
    }

    /// Measures the slots that have been idle for [`MEASURE_QUIET`] by
    /// `now`.
    pub(crate) fn measurement_due(&mut self, now: Instant) -> Vec<Effect> {
        let due = self
            .slots
            .iter()
            .enumerate()
            .filter(|(_, slot)| {
                !slot.busy() && slot.unmeasured > 0 && slot.idle_since + MEASURE_QUIET <= now
            })
            .map(|(index, _)| index)
            .collect::<Vec<_>>();
        due.into_iter().map(|index| self.measure(index)).collect()
    }

    /// Slot `key`'s target holds `inventory`, or could not be measured. Its
    /// record of units builds used keeps only the units still there.
    pub(crate) fn measured(
        &mut self,
        key: SlotKey,
        inventory: Result<Inventory, String>,
    ) -> Vec<Effect> {
        let slot = &mut self.slots[key.0];
        assert_eq!(
            slot.chore,
            Some(Chore::Measuring),
            "only a slot being measured is measured"
        );
        slot.chore = None;
        slot.unmeasured = 0;
        let mut effects = Vec::new();
        match inventory {
            Ok(inventory) => {
                let present = inventory
                    .items
                    .iter()
                    .filter_map(|item| match &item.kind {
                        Kind::Unit(unit) => Some(unit.as_str()),
                        Kind::Cache | Kind::TestFiles => None,
                    })
                    .collect::<HashSet<_>>();
                let recorded = slot.units.len();
                slot.units.retain(|unit, _| present.contains(unit.as_str()));
                if slot.units.len() != recorded {
                    effects.push(slot.persist());
                }
                let crates = inventory.duplicated_crates();
                if crates > slot.duplicated.unwrap_or(0) {
                    effects.push(Effect::Report(EventKind::Duplicated {
                        slot: slot_name(&slot.repository, slot.index),
                        crates,
                    }));
                }
                slot.duplicated = Some(crates);
                slot.size = Some(inventory.total);
                slot.inventory = Some(inventory);
            }
            Err(_) => {
                slot.size = None;
                slot.inventory = None;
            }
        }
        effects.extend(self.start_ready());
        effects
    }

    /// What the slots' targets hold together, as last measured.
    pub(crate) fn disk_used(&self) -> u64 {
        self.slots.iter().filter_map(|slot| slot.size).sum()
    }

    /// Whether an eviction is under way.
    pub(crate) fn evicting(&self) -> bool {
        self.evicting.is_some()
    }

    /// Chooses what idle slots give up to cover `shortage`: what builds used
    /// longest ago first, across every measured idle slot. None while an
    /// eviction is under way, or when no idle slot has anything to give up.
    /// The slots it chooses from are busy until [`Self::evicted`].
    pub(crate) fn evict(&mut self, shortage: Shortage) -> Option<Effect> {
        let needed = shortage.needed();
        assert!(needed > 0, "an eviction covers a shortage");
        if self.evicting.is_some() {
            return None;
        }
        let mut candidates = self
            .slots
            .iter()
            .enumerate()
            .filter(|(_, slot)| !slot.busy())
            .filter_map(|(index, slot)| Some((index, slot, slot.inventory.as_ref()?)))
            .flat_map(|(index, slot, inventory)| {
                inventory
                    .items
                    .iter()
                    .enumerate()
                    .map(move |(item, evictable)| (slot.last_use(evictable), index, item))
            })
            .collect::<Vec<_>>();
        candidates.sort_unstable();
        // Each shared file contributes only after its last chosen owner.
        let mut linked = HashMap::<(usize, usize), Vec<usize>>::new();
        let mut remaining = HashMap::new();
        for (index, slot) in self
            .slots
            .iter()
            .enumerate()
            .filter(|(_, slot)| !slot.busy())
        {
            let Some(inventory) = &slot.inventory else {
                continue;
            };
            for (file, shared) in inventory.shared.iter().enumerate() {
                remaining.insert((index, file), shared.owners.len());
                for owner in &shared.owners {
                    linked.entry((index, *owner)).or_default().push(file);
                }
            }
        }
        let (mut bytes, mut freed) = (0, 0);
        let mut chosen = BTreeMap::<usize, Vec<usize>>::new();
        for (_, index, item) in candidates {
            if bytes >= shortage.over_budget && freed >= shortage.under_floor {
                break;
            }
            let inventory = self.slots[index].inventory.as_ref().expect("listed above");
            bytes += inventory.items[item].bytes;
            freed += inventory.items[item].freeable;
            for file in linked.get(&(index, item)).into_iter().flatten() {
                let owners = remaining.get_mut(&(index, *file)).expect("listed above");
                *owners -= 1;
                if *owners == 0 {
                    let shared = &inventory.shared[*file];
                    bytes += shared.bytes;
                    if !shared.external {
                        freed += shared.bytes;
                    }
                }
            }
            chosen.entry(index).or_default().push(item);
        }
        if chosen.is_empty() {
            return None;
        }
        let removals = chosen
            .iter()
            .map(|(index, items)| {
                let slot = &mut self.slots[*index];
                slot.advance_generation();
                slot.chore = Some(Chore::Evicting);
                let inventory = slot.inventory.as_ref().expect("listed above");
                Removal {
                    key: SlotKey(*index),
                    name: slot_name(&slot.repository, slot.index),
                    paths: items
                        .iter()
                        .flat_map(|item| inventory.items[*item].paths.iter().cloned())
                        .collect(),
                }
            })
            .collect();
        self.evicting = Some(Eviction {
            shortage,
            chosen: chosen.into_iter().collect(),
        });
        Some(Effect::Evict(removals))
    }

    /// The slots of the eviction under way gave up what it chose, or failed
    /// to as `results` say. A slot that failed is measured again.
    pub(crate) fn evicted(&mut self, results: Vec<(SlotKey, Result<(), String>)>) -> Vec<Effect> {
        let Eviction { shortage, chosen } = self.evicting.take().expect("an eviction is under way");
        assert_eq!(
            results.len(),
            chosen.len(),
            "every slot of an eviction ends"
        );
        let recent = SystemTime::now() - IN_USE;
        let (mut freed, mut caches, mut units, mut test_files, mut in_use) = (0, 0, 0, 0, 0);
        let mut names = Vec::new();
        let mut effects = Vec::new();
        for (key, result) in results {
            let (_, items) = chosen
                .iter()
                .find(|(index, _)| *index == key.0)
                .expect("a slot of the eviction");
            let slot = &mut self.slots[key.0];
            assert_eq!(
                slot.chore,
                Some(Chore::Evicting),
                "only a slot evicting is done evicting"
            );
            slot.chore = None;
            let mut inventory = slot
                .inventory
                .take()
                .expect("an evicting slot was measured");
            if result.is_err() {
                // What is left is unknown until it is measured again.
                slot.unmeasured = slot.unmeasured.max(1);
                continue;
            }
            let (gone, released) = inventory.remove(items);
            let mut forgotten = false;
            for item in gone {
                in_use += usize::from(slot.last_use(&item).0 > recent);
                match item.kind {
                    Kind::Cache => caches += 1,
                    Kind::Unit(unit) => {
                        units += 1;
                        forgotten |= slot.units.remove(&unit).is_some();
                    }
                    Kind::TestFiles => test_files += 1,
                }
            }
            freed += released;
            slot.size = Some(inventory.total);
            slot.duplicated = Some(inventory.duplicated_crates());
            slot.inventory = Some(inventory);
            if forgotten {
                effects.push(slot.persist());
            }
            names.push(slot_name(&slot.repository, slot.index));
        }
        self.undersized = in_use > 0;
        effects.push(Effect::Report(EventKind::Evicted {
            shortage,
            freed,
            caches,
            units,
            test_files,
            in_use,
            slots: names,
        }));
        effects.extend(self.start_ready());
        effects
    }

    /// Whether the latest eviction took what builds used in the last ten
    /// minutes.
    pub(crate) fn undersized(&self) -> bool {
        self.undersized
    }

    /// Starts queued builds, next first, while slots are free. A build that
    /// waits for its repository's slot lets later builds of other
    /// repositories go first.
    fn start_ready(&mut self) -> Vec<Effect> {
        let mut effects = Vec::new();
        let mut in_use = self.memory_in_use();
        // What the builds held for each busy slot in this pass are expected
        // to take there: a later build waiting for that slot waits for them
        // too.
        let mut claimed = HashMap::<usize, u64>::new();
        // Optional work starts only while no other build waits and another
        // slot stays free for one that arrives.
        let others_wait = self
            .jobs
            .values()
            .any(|job| job.queued() && !job.optional());
        // Queue reasons belong to this pass. Remember earlier holds only to
        // avoid reporting the same decision again while it remains valid.
        let mut previously_held = HashSet::new();
        for (id, job) in self.jobs.iter_mut().filter(|(_, job)| job.queued()) {
            job.memory_needed = None;
            if job.hold.take().is_some() {
                previously_held.insert(*id);
            }
        }
        for id in self.queue_order() {
            let busy = self.slots.iter().filter(|slot| slot.busy()).count();
            if busy >= self.capacity {
                break;
            }
            if self.jobs[&id].optional() && (others_wait || self.capacity - busy < 2) {
                continue;
            }
            // A build that does not fit waits, and so does everything behind
            // it: smaller builds slipping past would keep it waiting for good.
            // With nothing running, it runs whatever it needs.
            let memory = self.memory_estimate(&self.jobs[&id]);
            if in_use > 0 && in_use.saturating_add(memory) > self.memory {
                self.jobs.get_mut(&id).expect("queued above").memory_needed = Some(memory);
                break;
            }
            let (slot, warm) = match self.choose_slot(id, &claimed) {
                Placement::Start { slot, warm } => (slot, warm),
                Placement::Wait => continue,
                Placement::Hold { hold, slot } => {
                    let expected = self.estimate(&self.jobs[&id]).unwrap_or(0);
                    *claimed.entry(slot).or_default() += expected;
                    let job = self.jobs.get_mut(&id).expect("queued above");
                    if !previously_held.contains(&id) {
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
            // A command of the client's own compiles nothing: it is never a
            // worktree's warm or cold first build.
            let first = job.operation.command.uses_cargo()
                && self.built_worktrees.insert(job.waiters[0].worktree.clone());
            let entry = &mut self.slots[slot];
            entry.advance_generation();
            entry.job = Some(id);
            // Its build changes what its target holds.
            entry.inventory = None;
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
                who: job.who(),
                passed: if job.operation.runs_every_test() && !job.rerun_all {
                    self.slots[slot].passed.clone()
                } else {
                    Vec::new()
                },
            }));
        }
        // A later build may have taken the last idle slot after earlier
        // builds chose to wait for warmth. That choice no longer delays them.
        if self.slots.iter().filter(|slot| slot.busy()).count() >= self.capacity {
            for job in self.jobs.values_mut().filter(|job| job.queued()) {
                job.hold = None;
            }
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
                // Optional work comes after every other build.
                (
                    job.optional(),
                    expected.saturating_sub(waited),
                    job.order,
                    *id,
                )
            })
            .collect::<Vec<_>>();
        queued.sort_unstable();
        queued.into_iter().map(|(.., id)| id).collect()
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
    /// `claimed` is what builds already held for each busy slot are expected
    /// to take there.
    fn choose_slot(&mut self, id: JobId, claimed: &HashMap<usize, u64>) -> Placement {
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
            return match self.hold(id, &needed, work, claimed) {
                Some((hold, slot)) => Placement::Hold { hold, slot },
                None => Placement::Start { slot: index, warm },
            };
        }
        let own = || {
            self.slots
                .iter()
                .filter(|slot| slot.repository == repository)
        };
        // A slot being measured or evicting is free again soon, and warm.
        if own().any(|slot| slot.chore.is_some()) || own().count() >= self.capacity {
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
            chore: None,
            generation: 0,
            unmeasured: 0,
            idle_since: Instant::now(),
            size: None,
            inventory: None,
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
    /// compile, should wait for a busy slot of its repository instead, and
    /// for which: the busy slot's build, and the builds already held for it
    /// (`claimed`), are expected to end and the job to build there sooner
    /// than the idle slot would compile its share cold. Without estimates it
    /// never waits.
    fn hold(
        &mut self,
        id: JobId,
        needed: &HashSet<String>,
        work: u64,
        claimed: &HashMap<usize, u64>,
    ) -> Option<(Hold, usize)> {
        let job = &self.jobs[&id];
        // Optional work never waits for a busy slot: it holds an idle one
        // at most.
        if job.optional() {
            return None;
        }
        let cost = self.compile_cost.get(&job.repository)?;
        let cold_ms = cost.ms(work)?;
        let mut best: Option<(Hold, usize)> = None;
        for (index, slot) in self.slots.iter().enumerate() {
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
                .saturating_add(claimed.get(&index).copied().unwrap_or(0))
                .saturating_add(cost.ms(missing.saturating_add(distance))?);
            if wait_ms < cold_ms && best.as_ref().is_none_or(|(best, _)| wait_ms < best.wait_ms) {
                let hold = Hold {
                    slot: slot_name(&slot.repository, slot.index),
                    wait_ms,
                    cold_ms,
                };
                best = Some((hold, index));
            }
        }
        best
    }

    /// The slots and the queue, next build first, with the jobs each running
    /// build uses as `tokens` says.
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
                chore: slot.chore,
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
                        } else if !job.operation.command.uses_cargo() {
                            Phase::Running
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
    /// It waits for busy slot `slot` although one is idle.
    Hold {
        hold: Hold,
        slot: usize,
    },
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
    use crate::slot::{Evictable, Inventory, Kind};

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
            optional: false,
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
            optional: false,
        })
    }

    fn submit_optional(
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
            optional: true,
        })
    }

    #[test]
    fn optional_work_waits_behind_other_builds_and_leaves_a_slot_free() {
        let mut scheduler = scheduler(3);
        let [(running, ..)] = starts(&submit(&mut scheduler, 1, "/a", "a1"))[..] else {
            panic!("a build starts in an idle slot");
        };
        // Two slots are idle: optional work may take one.
        assert_eq!(
            starts(&submit_optional(&mut scheduler, 2, "/p", "p1")).len(),
            1
        );
        // One is left: it stays free for a build someone asks for.
        assert!(starts(&submit_optional(&mut scheduler, 3, "/q", "q1")).is_empty());
        assert_eq!(starts(&submit(&mut scheduler, 4, "/b", "b1")).len(), 1);
        // A queued build waits behind the optional one until someone asks
        // for the optional one too.
        assert!(starts(&submit(&mut scheduler, 5, "/c", "c1")).is_empty());
        let (_, queue) = scheduler.status(|_| 0);
        let order = queue
            .iter()
            .map(|build| build.revision.to_string())
            .collect::<Vec<_>>();
        assert_eq!(order, ["c1", "q1"], "optional work queues last");
        submit(&mut scheduler, 6, "/q", "q1");
        let (_, queue) = scheduler.status(|_| 0);
        assert_eq!(queue[0].revision.to_string(), "q1", "joined: ordinary work");
        let next = scheduler.exited(running, &ok(), None, BuildReport::default());
        assert_eq!(starts(&next)[0].2, "q1");
    }

    #[test]
    fn cache_exclusion_counts_against_optional_work_and_release_starts_it() {
        let mut scheduler = scheduler(3);
        let [(running, ..)] = starts(&submit(&mut scheduler, 1, "/a", "a1"))[..] else {
            panic!("the first build starts");
        };
        scheduler.exited(running, &ok(), None, BuildReport::default());
        let name = scheduler.status(|_| 0).0[0].name.clone();
        let acquired = scheduler.cache_acquire(&name).unwrap();
        for (waiter, tree, optional) in [(2, "b1", false), (3, "p1", true)] {
            let mut source = source(&format!("/{tree}"));
            source.repository = PathBuf::from(format!("/{tree}/.git"));
            let effects = scheduler.submit(Submission {
                waiter: WaiterId(waiter),
                source,
                revision: revision(tree),
                operation: check(),
                label: None,
                copy_to: None,
                rerun_all: false,
                optional,
            });
            assert_eq!(starts(&effects).len(), usize::from(!optional));
        }
        let ready = scheduler.cache_release(acquired, false);
        let started = starts(&ready);
        assert_eq!(started.len(), 1);
        assert_eq!(started[0].2, "p1");
        assert_eq!(
            scheduler
                .status(|_| 0)
                .0
                .iter()
                .filter(|slot| slot.build.is_some())
                .count(),
            2,
            "cache exclusion is released and optional work leaves one slot free"
        );
    }

    #[test]
    fn target_generation_advances_before_every_writer_and_not_for_read_exclusion() {
        let mut scheduler = scheduler(1);
        let [(job, ..)] = starts(&submit(&mut scheduler, 1, "/a", "a1"))[..] else {
            panic!("start");
        };
        let name = scheduler.status(|_| 0).0[0].name.clone();
        assert_eq!(
            scheduler.cache_generation(&name),
            Some(1),
            "Start already owns the new generation"
        );
        scheduler.exited(job, &ok(), None, BuildReport::default());
        let read = scheduler.cache_acquire(&name).unwrap();
        assert_eq!(scheduler.cache_identity(read).2, 1);
        scheduler.cache_release(read, false);
        assert_eq!(scheduler.cache_generation(&name), Some(1));
        let measurements = scheduler.measurement_due(Instant::now() + MEASURE_QUIET);
        let [Effect::Measure(idle)] = &measurements[..] else {
            panic!("measurement");
        };
        assert_eq!(scheduler.cache_generation(&name), Some(1));
        scheduler.measured(
            idle.key,
            Ok(inventory(&["debug/1111111111111111".into()], 100)),
        );
        let Some(Effect::Evict(removals)) = scheduler.evict(over_budget(1)) else {
            panic!("eviction");
        };
        assert_eq!(scheduler.cache_generation(&name), Some(2));
        scheduler.evicted(vec![(removals[0].key, Ok(()))]);
        let cleanup = scheduler.cache_acquire(&name).unwrap();
        scheduler.cache_writing(cleanup);
        assert_eq!(
            scheduler.cache_generation(&name),
            Some(3),
            "advance before invalidation/deletion"
        );
        scheduler.cache_release(cleanup, true);
        assert_eq!(scheduler.cache_generation(&name), Some(3));
        assert_eq!(starts(&submit(&mut scheduler, 2, "/a", "a2")).len(), 1);
        assert_eq!(scheduler.cache_generation(&name), Some(4));
    }

    #[test]
    fn queued_writer_advances_generation_as_read_exclusion_is_released() {
        let mut scheduler = scheduler(1);
        let [(job, ..)] = starts(&submit(&mut scheduler, 1, "/a", "a1"))[..] else {
            panic!("start");
        };
        scheduler.exited(job, &ok(), None, BuildReport::default());
        let name = scheduler.status(|_| 0).0[0].name.clone();
        let held = scheduler.cache_acquire(&name).unwrap();
        assert!(starts(&submit(&mut scheduler, 2, "/a", "a2")).is_empty());
        let generation = scheduler.cache_identity(held).2;
        assert_eq!(scheduler.cache_generation(&name), Some(generation));
        assert_eq!(starts(&scheduler.cache_release(held, false)).len(), 1);
        assert_eq!(scheduler.cache_generation(&name), Some(generation + 1));
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

    fn measures(effects: &[Effect]) -> Vec<usize> {
        effects
            .iter()
            .filter_map(|effect| match effect {
                Effect::Measure(Measurement { slot, .. }) => Some(*slot),
                _ => None,
            })
            .collect()
    }

    /// A target holding compiled `units` of `bytes` each, written long ago.
    fn inventory(units: &[String], bytes: u64) -> Inventory {
        Inventory {
            items: units
                .iter()
                .map(|unit| Evictable {
                    paths: vec![PathBuf::from(format!("/target/{unit}"))],
                    kind: Kind::Unit(unit.clone()),
                    written: SystemTime::UNIX_EPOCH,
                    freeable: bytes,
                    bytes,
                })
                .collect(),
            total: bytes * units.len() as u64,
            shared: Vec::new(),
        }
    }

    /// Measures every idle slot, each holding what its record says it used,
    /// `bytes` per unit.
    fn measure_all(scheduler: &mut Scheduler<Table>, bytes: u64) {
        let due = scheduler.measurement_due(Instant::now() + MEASURE_QUIET);
        for index in measures(&due) {
            let key = SlotKey(
                scheduler
                    .slots
                    .iter()
                    .position(|slot| slot.index == index)
                    .unwrap(),
            );
            let mut held = scheduler.slots[key.0]
                .units
                .keys()
                .cloned()
                .collect::<Vec<_>>();
            held.sort();
            scheduler.measured(key, Ok(inventory(&held, bytes)));
        }
    }

    fn removed(effect: &Effect) -> Vec<(usize, Vec<String>)> {
        let Effect::Evict(removals) = effect else {
            panic!("{effect:?}");
        };
        removals
            .iter()
            .map(|removal| {
                let names = removal
                    .paths
                    .iter()
                    .map(|path| path.strip_prefix("/target").unwrap().display().to_string())
                    .collect();
                (removal.key.0, names)
            })
            .collect()
    }

    fn over_budget(bytes: u64) -> Shortage {
        Shortage {
            over_budget: bytes,
            under_floor: 0,
        }
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
        scheduler.exited(job, &ok(), None, BuildReport::default());
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

        let finished = scheduler.exited(job, &ok(), None, BuildReport::default());
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

        let next = scheduler.exited(running, &ok(), None, BuildReport::default());
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
            optional: false,
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
        let next = scheduler.exited(running, &ok(), None, BuildReport::default());
        let [(job, ..)] = starts(&next)[..] else {
            panic!("{next:?}");
        };
        scheduler.exited(job, &ok(), None, BuildReport::default());
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
        let next = scheduler.exited(
            job,
            &Outcome::Signaled { signal: 15 },
            None,
            BuildReport::default(),
        );
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
        scheduler.exited(job_a, &ok(), None, BuildReport::default());
        scheduler.exited(job_b, &ok(), None, BuildReport::default());

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
    fn an_idle_slot_is_measured_after_a_quiet_period_and_waited_for_meanwhile() {
        let mut scheduler = scheduler(2);
        let first = submit(&mut scheduler, 1, "/a", "a1");
        let [(job, 0, _)] = starts(&first)[..] else {
            panic!("{first:?}");
        };
        let ended = Instant::now();
        assert!(measures(&scheduler.exited(job, &ok(), None, BuildReport::default())).is_empty());
        let due = scheduler.next_measurement().expect("the slot is due");
        assert!(due >= ended + MEASURE_QUIET);
        assert!(scheduler.measurement_due(ended).is_empty());
        assert_eq!(measures(&scheduler.measurement_due(due)), [0]);
        assert_eq!(scheduler.next_measurement(), None);

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
            optional: false,
        });
        assert_eq!(starts(&other).len(), 1);
        let (slots, queue) = scheduler.status(|_| 0);
        assert!(slots[0].chore == Some(Chore::Measuring) && slots[0].build.is_none());
        assert_eq!(queue.len(), 1);
        let ready = scheduler.measured(SlotKey(0), Ok(inventory(&units("a", 1), 5)));
        assert!(
            matches!(starts(&ready)[..], [(_, 0, ref tree)] if tree == "a2"),
            "the waiting build takes its warm slot"
        );
        assert_eq!(scheduler.status(|_| 0).0[0].size, Some(5));
        assert_eq!(scheduler.disk_used(), 5);
    }

    #[test]
    fn a_slot_busy_without_pause_is_measured_after_enough_builds() {
        let mut scheduler = scheduler(1);
        for build in 1..MEASURE_DUE {
            let effects = submit(&mut scheduler, u64::from(build), "/a", &format!("a{build}"));
            let [(job, ..)] = starts(&effects)[..] else {
                panic!("{effects:?}");
            };
            assert!(
                measures(&scheduler.exited(job, &ok(), None, BuildReport::default())).is_empty()
            );
        }
        let last = submit(&mut scheduler, 99, "/a", "last");
        let [(job, ..)] = starts(&last)[..] else {
            panic!("{last:?}");
        };
        assert_eq!(
            measures(&scheduler.exited(job, &ok(), None, BuildReport::default())),
            [0]
        );
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
        scheduler.exited(job_a, &ok(), None, BuildReport::default());
        scheduler.exited(job_b, &ok(), None, BuildReport::default());
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
    fn a_command_of_the_clients_own_runs_in_a_slot_and_says_nothing_about_compiling() {
        let mut scheduler = scheduler(1);
        let effects = submit_operation(&mut scheduler, 1, "/a", "a1", Command::Run, &["make"]);
        let [(job, 0, _)] = starts(&effects)[..] else {
            panic!("{effects:?}");
        };
        let (slots, _) = scheduler.status(|_| 0);
        assert_eq!(slots[0].build.as_ref().unwrap().phase, Phase::Running);
        assert!(matches!(
            reports(&effects)[..],
            [
                _,
                EventKind::Started {
                    first: false,
                    warm: false,
                    ..
                }
            ]
        ));
        scheduler.exited(
            job,
            &Outcome::Exited { code: 0 },
            None,
            BuildReport::default(),
        );
        assert!(scheduler.compile_cost.is_empty());
        assert!(scheduler.slots[0].compilations.is_empty());
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
        let cost = scheduler
            .compile_cost
            .entry("/repo/.git".into())
            .or_default();
        cost.add(10, 10_000);
        cost.add(20, 20_000);
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
        // A second build waits behind the first held one too: about 5 s more
        // there, still sooner than 22 s cold.
        let second = submit_operation(&mut scheduler, 5, "/d", "d1", Command::Test, &[]);
        assert!(starts(&second).is_empty(), "{second:?}");
        assert!(matches!(
            reports(&second)[..],
            [
                _,
                EventKind::Held {
                    hold: Hold {
                        wait_ms: 19_000..=20_000,
                        ..
                    },
                    ..
                }
            ]
        ));
        // A third would wait about 25 s there: the idle slot is sooner.
        let third = submit_operation(&mut scheduler, 6, "/e", "e1", Command::Test, &[]);
        assert!(matches!(starts(&third)[..], [(_, 0, _)]), "{third:?}");
        let (_, queue) = scheduler.status(|_| 0);
        assert!(
            queue.iter().all(|build| build.held.is_none()),
            "every slot is busy: waiting for warmth no longer delays a build"
        );
        let next = scheduler.exited(job, &ok(), None, BuildReport::default());
        assert!(matches!(starts(&next)[..], [(_, 1, _)]), "{next:?}");
    }

    #[test]
    fn compile_cost_is_what_one_more_unit_adds_not_the_average() {
        let mut cost = CompileCost::default();
        cost.add(10, 22_000);
        assert_eq!(cost.ms(10), None, "one size of build tells nothing more");
        // 20 s to start, check what is fresh and link, then 0.2 s a unit:
        // averaged per unit, the small builds would say 4.4 s.
        for units in [5, 40, 120, 400] {
            cost.add(units, 20_000 + 200 * units);
        }
        assert_eq!(cost.ms(0), Some(20_000));
        assert_eq!(cost.ms(400), Some(100_000));
        // Bigger builds that were not slower cost nothing more per unit.
        let mut flat = CompileCost::default();
        flat.add(10, 30_000);
        flat.add(100, 20_000);
        assert_eq!(flat.ms(1_000), flat.ms(0));
    }

    #[test]
    fn a_build_starts_cold_when_waiting_costs_more_than_its_missing_units() {
        let mut scheduler = checked_and_tested();
        scheduler.slots[1].compilations[0].build_ms = Some(5000);
        let running = submit_operation(&mut scheduler, 3, "/b", "b2", Command::Test, &[]);
        let [(_, 1, _)] = starts(&running)[..] else {
            panic!("{running:?}");
        };
        // 20 s fixed and 0.2 s a unit: 22 units cold in slot 0 take 24.4 s,
        // slot 1 ends in about 5 s and then takes 22 s for its 10.
        let cost = scheduler
            .compile_cost
            .entry("/repo/.git".into())
            .or_default();
        cost.add(10, 22_000);
        cost.add(100, 40_000);
        let cold = submit_operation(&mut scheduler, 4, "/c", "c1", Command::Test, &[]);
        assert!(matches!(starts(&cold)[..], [(_, 0, _)]), "{cold:?}");
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
        let next = scheduler.exited(running[0], &ok(), None, BuildReport::default());
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
        let next = scheduler.exited(job, &ok(), None, BuildReport::default());
        assert_eq!(starts(&next).len(), 1, "the test runs alone: {next:?}");
        let (_, queue) = scheduler.status(|_| 0);
        assert_eq!(queue[0].memory_needed, Some(4), "{queue:?}");
        // With memory to spare and every slot busy, it waits for a slot.
        scheduler.memory = 100;
        let idle = scheduler
            .slots
            .iter()
            .position(|slot| !slot.busy())
            .unwrap();
        scheduler.slots[idle].chore = Some(Chore::Measuring);
        submit_optional(&mut scheduler, 6, "/p", "p1");
        let (_, queue) = scheduler.status(|_| 0);
        assert!(
            queue.iter().all(|build| build.memory_needed.is_none()),
            "{queue:?}"
        );
    }

    #[test]
    fn evicted_units_count_as_missing() {
        let mut scheduler = checked_and_tested();
        scheduler.distance.0.insert(("b1".into(), "c1".into()), 1);
        measure_all(&mut scheduler, 10);
        // The check in slot 0 ended a minute before the test in slot 1.
        for used in scheduler.slots[0].units.values_mut() {
            used.at -= 60;
        }
        let eviction = scheduler.evict(over_budget(120)).unwrap();
        let [(0, gone)] = &removed(&eviction)[..] else {
            panic!("{eviction:?}");
        };
        assert_eq!(gone.len(), 12);
        scheduler.evicted(vec![(SlotKey(0), Ok(()))]);
        // Both slots lack what `check` needs; slot 1 is closer.
        assert_eq!(run(&mut scheduler, 3, "/c", "c1"), 1);
    }

    #[test]
    fn an_eviction_takes_what_builds_used_longest_ago_across_idle_slots() {
        let mut scheduler = checked_and_tested();
        measure_all(&mut scheduler, 10);
        let now = super::now();
        // Slot 1's first test unit was used an hour ago; slot 0 holds a unit
        // no build was seen using, written a day ago.
        let first_test = units("t", 1).remove(0);
        scheduler.slots[1].units.insert(
            first_test.clone(),
            LastUse {
                at: now - 3600,
                after: 0,
            },
        );
        let inventory = scheduler.slots[0].inventory.as_mut().unwrap();
        inventory.items.push(Evictable {
            paths: vec!["/target/debug/stale".into()],
            kind: Kind::Unit("debug/5555555555555555".into()),
            written: SystemTime::now() - Duration::from_secs(24 * 3600),
            freeable: 10,
            bytes: 10,
        });
        inventory.total += 10;
        let eviction = scheduler.evict(over_budget(15)).unwrap();
        assert_eq!(
            removed(&eviction),
            [
                (0, vec!["debug/stale".to_owned()]),
                (1, vec![first_test.clone()])
            ]
        );
        let (slots, _) = scheduler.status(|_| 0);
        assert!(slots.iter().all(|slot| slot.chore == Some(Chore::Evicting)));
        assert_eq!(
            scheduler.evict(over_budget(1)),
            None,
            "one eviction at a time"
        );
        assert!(scheduler.evicting());

        let done = scheduler.evicted(vec![(SlotKey(0), Ok(())), (SlotKey(1), Ok(()))]);
        assert!(matches!(
            reports(&done)[..],
            [EventKind::Evicted {
                freed: 20,
                units: 2,
                in_use: 0,
                ..
            }]
        ));
        assert!(!scheduler.undersized());
        // What compilations need stays known: it holds for every slot.
        assert_eq!(scheduler.slots[1].compilations.len(), 1);
        assert!(!scheduler.slots[1].units.contains_key(&first_test));
        assert_eq!(scheduler.slots[1].units.len(), 11);
        assert_eq!(scheduler.disk_used(), 250 - 20);
        assert_eq!(
            scheduler.slots[1].inventory.as_ref().unwrap().items.len(),
            11
        );
    }

    #[test]
    fn eviction_counts_shared_blocks_once_and_distinguishes_external_links_from_target_usage() {
        for external in [false, true] {
            let mut scheduler = scheduler(1);
            run(&mut scheduler, 1, "/a", "a1");
            let measured = scheduler.measurement_due(Instant::now() + MEASURE_QUIET);
            let [Effect::Measure(measurement)] = &measured[..] else {
                panic!("measurement")
            };
            let units = units("a", 3);
            let mut inventory = inventory(&units, 10);
            inventory.total += 100;
            inventory.shared.push(crate::slot::SharedFile {
                bytes: 100,
                owners: vec![0, 1],
                external,
            });
            scheduler.measured(measurement.key, Ok(inventory));
            let shortage = Shortage {
                over_budget: 100,
                under_floor: if external { 20 } else { 110 },
            };
            let eviction = scheduler.evict(shortage).unwrap();
            assert_eq!(removed(&eviction), [(0, units[..2].to_vec())]);
            let done = scheduler.evicted(vec![(SlotKey(0), Ok(()))]);
            assert_eq!(
                scheduler.disk_used(),
                10,
                "shared blocks have left the target"
            );
            let [EventKind::Evicted { freed, .. }] = reports(&done)[..] else {
                panic!("evicted")
            };
            assert_eq!(*freed, if external { 20 } else { 120 });
            assert_eq!(
                scheduler.slots[0].inventory.as_ref().unwrap().items.len(),
                1
            );
        }
    }

    #[test]
    fn units_a_build_used_last_go_from_the_top_of_its_graph_down() {
        let mut scheduler = scheduler(1);
        let first = submit(&mut scheduler, 1, "/a", "a1");
        let [(job, ..)] = starts(&first)[..] else {
            panic!("{first:?}");
        };
        // Cargo reports a unit after everything it depends on.
        let [library, dependent, binary] =
            ["1", "2", "3"].map(|digit| format!("debug/{}", digit.repeat(16)));
        for unit in [&library, &dependent, &binary] {
            scheduler.crate_built(job, true, Some(unit.clone()));
        }
        scheduler.crate_built(job, false, Some(library.clone()));
        scheduler.exited(job, &ok(), None, BuildReport::default());
        let at = scheduler.slots[0].units[&library].at;
        assert_eq!(
            [&library, &dependent, &binary].map(|unit| scheduler.slots[0].units[unit]),
            [2, 1, 0].map(|after| LastUse { at, after })
        );
        measure_all(&mut scheduler, 10);
        let eviction = scheduler.evict(over_budget(20)).unwrap();
        assert_eq!(removed(&eviction), [(0, vec![binary, dependent])]);
    }

    #[test]
    fn an_eviction_skips_busy_and_unmeasured_slots_and_reports_taking_what_is_in_use() {
        let mut scheduler = checked_and_tested();
        measure_all(&mut scheduler, 10);
        let busy = submit(&mut scheduler, 3, "/c", "c1");
        let [(_, 0, _)] = starts(&busy)[..] else {
            panic!("{busy:?}");
        };
        // Slot 0 builds; slot 1's units were all used a moment ago.
        let eviction = scheduler.evict(over_budget(1_000)).unwrap();
        let [(1, gone)] = &removed(&eviction)[..] else {
            panic!("{eviction:?}");
        };
        assert_eq!(gone.len(), 12, "everything slot 1 has, still short");
        let waiting = submit(&mut scheduler, 4, "/d", "d1");
        assert!(starts(&waiting).is_empty(), "the evicting slot is busy");
        let done = scheduler.evicted(vec![(SlotKey(1), Ok(()))]);
        assert!(
            reports(&done)
                .iter()
                .any(|report| matches!(report, EventKind::Evicted { in_use: 12, .. })),
            "{done:?}"
        );
        assert!(scheduler.undersized());
        assert_eq!(starts(&done).len(), 1, "{done:?}");
        // Slot 1 builds now, slot 0 is unmeasured: nothing is left to evict.
        assert_eq!(scheduler.evict(over_budget(1)), None);
        assert!(!scheduler.evicting());
    }

    #[test]
    fn a_slot_that_failed_to_evict_is_measured_again() {
        let mut scheduler = checked_and_tested();
        measure_all(&mut scheduler, 10);
        scheduler.evict(over_budget(10)).unwrap();
        let done = scheduler.evicted(vec![(SlotKey(0), Err("busy device".into()))]);
        assert!(matches!(
            reports(&done)[..],
            [EventKind::Evicted { freed: 0, .. }]
        ));
        let due = scheduler.measurement_due(Instant::now() + MEASURE_QUIET);
        assert_eq!(measures(&due), [0]);
    }

    #[test]
    fn a_measurement_keeps_only_the_recorded_units_its_target_still_holds() {
        let mut scheduler = checked_and_tested();
        let due = scheduler.measurement_due(Instant::now() + MEASURE_QUIET);
        assert_eq!(measures(&due), [0, 1]);
        let kept = units("c", 3);
        let done = scheduler.measured(SlotKey(0), Ok(inventory(&kept, 1)));
        assert!(matches!(persisted(&done)[..], [(0, record)] if record.units.len() == 3));
        scheduler.measured(SlotKey(1), Err("unreadable".into()));
        assert_eq!(scheduler.slots[1].size, None);
        assert_eq!(scheduler.slots[1].units.len(), 12);
        assert_eq!(scheduler.disk_used(), 3);
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
            scheduler.exited(job, &Outcome::Exited { code }, None, BuildReport::default());
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
    fn a_slot_remembers_the_compilations_run_most_recently() {
        let mut scheduler = scheduler(1);
        for build in 0..=KEPT_COMPILATIONS {
            let args = ["-p".to_owned(), format!("crate-{build}")];
            let effects = submit_operation(
                &mut scheduler,
                build as u64,
                "/a",
                "a1",
                Command::Check,
                &args.iter().map(String::as_str).collect::<Vec<_>>(),
            );
            let [(job, ..)] = starts(&effects)[..] else {
                panic!("{effects:?}");
            };
            used(&mut scheduler, job, &units("a", 1));
            let ended = scheduler.exited(job, &ok(), None, BuildReport::default());
            if !measures(&ended).is_empty() {
                scheduler.measured(SlotKey(0), Ok(inventory(&units("a", 1), 1)));
            }
        }
        let compilations = &scheduler.slots[0].compilations;
        assert_eq!(compilations.len(), KEPT_COMPILATIONS);
        assert!(
            !compilations
                .iter()
                .any(|run| run.compilation.args == ["-p", "crate-0"]),
            "the first, run longest ago, is forgotten"
        );
    }

    #[test]
    fn compilation_recency_survives_timestamp_ties_and_restart() {
        let mut scheduler = scheduler(1);
        run(&mut scheduler, 1, "/a", "a1");
        let slot = &mut scheduler.slots[0];
        let compilation = |number: usize| {
            let mut operation = check();
            operation.args = vec!["-p".into(), format!("crate-{number}")];
            Compilation::new(Path::new(""), &operation)
        };
        for number in 0..KEPT_COMPILATIONS {
            slot.ran(compilation(number), units("a", 1), true, 1, None);
        }
        slot.ran(compilation(0), units("a", 1), true, 1, None);
        // Force every timestamp to tie; execution order is still known.
        let at = now();
        for run in &mut slot.compilations {
            run.at = at;
        }
        let Effect::Persist { record, .. } = slot.persist() else {
            panic!("persist")
        };
        let mut restarted = super::Scheduler::new(1, u64::MAX, Table::default());
        restarted.restore("/repo/.git".into(), 0, revision("a1"), record);
        let slot = &mut restarted.slots[0];
        slot.ran(compilation(KEPT_COMPILATIONS), units("a", 1), true, 1, None);
        assert_eq!(slot.compilations.len(), KEPT_COMPILATIONS);
        assert!(
            slot.compilations
                .iter()
                .any(|run| run.compilation == compilation(0))
        );
        assert!(
            !slot
                .compilations
                .iter()
                .any(|run| run.compilation == compilation(1))
        );
        assert_eq!(
            slot.compilations.last().unwrap().compilation,
            compilation(KEPT_COMPILATIONS)
        );
        // Loading an oversized record also obeys the bound.
        let Effect::Persist { mut record, .. } = slot.persist() else {
            panic!("persist")
        };
        // Wall-clock rollback cannot make the last run the oldest.
        record.compilations.last_mut().unwrap().at = 0;
        record.compilations.insert(
            0,
            CompilationRun {
                compilation: compilation(1),
                at: 0,
                units: units("a", 1).into_iter().collect(),
                build_ms: Some(1),
                peak_memory: None,
            },
        );
        let mut restored = super::Scheduler::new(1, u64::MAX, Table::default());
        restored.restore("/repo/.git".into(), 0, revision("a1"), record);
        assert_eq!(restored.slots[0].compilations.len(), KEPT_COMPILATIONS);
        assert_eq!(
            restored.slots[0].compilations.last().unwrap().compilation,
            compilation(KEPT_COMPILATIONS)
        );
        assert!(
            !restored.slots[0]
                .compilations
                .iter()
                .any(|run| run.compilation == compilation(1))
        );
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
        let next = scheduler.exited(job, &ok(), None, BuildReport::default());
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
            optional: false,
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
        let finished = scheduler.exited(job, &ok(), Some(usage), BuildReport::default());
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
        let finished = scheduler.exited(job, &ok(), None, BuildReport::default());
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
        let finished = scheduler.exited(job, &ok(), None, BuildReport::default());
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
        let finished = scheduler.exited(job, &ok(), None, BuildReport::default());
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
        // Evicted units are forgotten, and that is persisted too.
        measure_all(&mut scheduler, 1);
        scheduler.evict(over_budget(3)).unwrap();
        let evicted = scheduler.evicted(vec![(SlotKey(0), Ok(()))]);
        assert!(matches!(
            persisted(&evicted)[..],
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
            units: units("c", 12)
                .into_iter()
                .map(|unit| (unit, LastUse { at: 1, after: 0 }))
                .collect(),
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
    fn units_builds_use_are_recorded_and_forgotten_when_evicted() {
        let mut scheduler = scheduler(1);
        let first = submit(&mut scheduler, 1, "/work/a", "a1");
        let [(job, ..)] = starts(&first)[..] else {
            panic!("{first:?}");
        };
        scheduler.crate_built(job, true, Some("debug/aaaaaaaaaaaaaaaa".into()));
        scheduler.unit_used(job, "debug/bbbbbbbbbbbbbbbb".into());
        let finished = scheduler.exited(job, &ok(), None, BuildReport::default());
        let [(0, record)] = persisted(&finished)[..] else {
            panic!("{finished:?}");
        };
        assert_eq!(
            record.units.keys().collect::<Vec<_>>(),
            ["debug/aaaaaaaaaaaaaaaa", "debug/bbbbbbbbbbbbbbbb"]
        );
        measure_all(&mut scheduler, 1);
        // Reported last, the build script's output goes first.
        let eviction = scheduler.evict(over_budget(1)).unwrap();
        assert_eq!(
            removed(&eviction),
            [(0, vec!["debug/bbbbbbbbbbbbbbbb".to_owned()])]
        );
        let done = scheduler.evicted(vec![(SlotKey(0), Ok(()))]);
        assert!(matches!(persisted(&done)[..], [(0, record)] if record.units.len() == 1));
        assert!(matches!(
            reports(&done)[..],
            [EventKind::Evicted { units: 1, .. }]
        ));
    }
}
