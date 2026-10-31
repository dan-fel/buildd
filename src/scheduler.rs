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
//! - **Disk admission.** A build starts only while free disk covers the
//!   volume's floor, what running builds reserve for their output, and
//!   what it is expected to add itself: the larger of what its kind of
//!   build (its compilation, else the same command in the same directory)
//!   last added to the slot it takes, and how far that slot is below the
//!   size it had after that build. A slot without such history, or a new
//!   one, assumes the host's `build_headroom`. Growth is measured when
//!   exactly one build ran between two measurements of its slot.
//! - **Eviction.** When the slots' targets are over their budget, or free
//!   disk cannot cover its floor and build output reservations, idle slots
//!   give up what builds used longest ago; never the slot a build is being
//!   admitted to, whose artifacts it would only compile again. Maintenance
//!   has first claim on idle slots before any queued build starts. Pressure
//!   forces measurement of dirty idle slots; a failed scan holds admission
//!   until it recovers. A breached volume floor stops running process
//!   groups with a resource failure. Only after they exit can their
//!   artifacts be reclaimed.
//! - **Waiting.** Every queued build knows why it waits, and its clients
//!   hear it when it first waits and whenever the reason changes. When
//!   free disk is short, no build runs and idle artifacts cannot cover the
//!   shortfall, only someone freeing space helps: after `disk_wait` of
//!   that, a build that does not fit is refused instead of waiting on.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant, SystemTime};

use crate::cargo::{Compilation, Operation};
use crate::passed::PassedBinary;
use crate::protocol::{
    BuildReport, Capacity, Chore, DiskWait, EventKind, Forecast, Growth, Headroom, Hold, LastBuild,
    Message, Outcome, Phase, QueuedBuild, RunningBuild, Shortage, SlotPlan, SlotStatus, Usage,
    Wait,
};
use crate::slot::{
    CompilationRun, DiskUse, Evictable, Inventory, Kind, LastUse, SlotRecord, slot_name,
};
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
    /// Run alone on this host, across repositories.
    pub(crate) exclusive: bool,
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
        reason: Stop,
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

/// Why a process group is stopped. Resource failures remain visible to
/// callers, while a disconnected client has no caller left to notify.
#[derive(Clone, Debug, PartialEq)]
pub(crate) enum Stop {
    Withdrawn,
    Failed(String),
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
    /// It has the host to itself.
    pub(crate) exclusive: bool,
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
    exclusive: bool,
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
    /// Why it waits, while it is queued: what its clients last heard, with
    /// the latest numbers.
    wait: Option<Wait>,
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
        testing_at: Option<Instant>,
        /// Cargo succeeded and its executables are being copied out.
        copying: bool,
        cancelled: bool,
        /// Measured peak, or None while learning under the host's reservation.
        memory: Option<u64>,
        /// Free disk reserved for its output: what it was expected to add.
        disk: u64,
    },
}

struct Slot {
    repository: PathBuf,
    /// The slot's index among its repository's slots.
    index: usize,
    job: Option<JobId>,
    /// The tree its checkout holds: that of its latest build.
    /// None when only the cache can be restored. Its original repository
    /// or checkout may have disappeared; its artifacts still consume disk.
    revision: Option<Revision>,
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
    /// The compilation of its latest build and its target's measured size
    /// when that build started, while that build is the only one since the
    /// measurement: the next measurement tells what it added.
    growing: Option<(Compilation, u64)>,
    /// What its target holds, from its measurement since its latest build.
    inventory: Option<Inventory>,
    maintenance_error: Option<String>,
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
        usage: Option<Usage>,
    ) {
        // Resource measurements remain useful when a command compiles nothing.
        if units.is_empty() && usage.is_none() {
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
                    if !units.is_empty() {
                        run.units.clear();
                    }
                    run.build_ms = Some(build_ms);
                }
                if let Some(usage) = usage {
                    run.peak_memory = Some(run.peak_memory.unwrap_or(0).max(usage.peak_memory));
                    if let Some(mut tests) = usage.test_memory {
                        if let Some(earlier) = run.test_memory.filter(|old| old.jobs == tests.jobs)
                        {
                            tests.peak_memory = tests.peak_memory.max(earlier.peak_memory);
                        }
                        run.test_memory = Some(tests);
                    }
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
                peak_memory: usage.map(|usage| usage.peak_memory),
                test_memory: usage.and_then(|usage| usage.test_memory),
                disk: None,
            }),
        }
        while self.compilations.len() > KEPT_COMPILATIONS {
            self.compilations.remove(0);
        }
    }

    /// Its latest build of `compilation` left its target at `disk`.
    fn grew(&mut self, compilation: Compilation, disk: DiskUse) {
        match self
            .compilations
            .iter_mut()
            .find(|run| run.compilation == compilation)
        {
            Some(run) => run.disk = Some(disk),
            None => {
                self.compilations.push(CompilationRun {
                    compilation,
                    at: now(),
                    units: std::collections::BTreeSet::new(),
                    build_ms: None,
                    peak_memory: None,
                    test_memory: None,
                    disk: Some(disk),
                });
                if self.compilations.len() > KEPT_COMPILATIONS {
                    self.compilations.remove(0);
                }
            }
        }
    }

    /// What a build of `compilation` is expected to add to its target, as
    /// far as its history tells: the larger of what the latest measured
    /// build of the compilation (else of the same command in the same
    /// directory) added, and how far the target is below the size that
    /// build left. A target of unknown size counts as empty.
    fn growth(&self, compilation: &Compilation) -> Option<u64> {
        let size = self.size.unwrap_or(0);
        let expected = |alike: &dyn Fn(&Compilation) -> bool| {
            self.compilations
                .iter()
                .filter(|run| alike(&run.compilation))
                .filter_map(|run| run.disk)
                .map(|disk| disk.added.max(disk.total.saturating_sub(size)))
                .max()
        };
        expected(&|run| run == compilation).or_else(|| {
            expected(&|run| run.prefix == compilation.prefix && run.command == compilation.command)
        })
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
    /// Explicit reservation for unknown peaks; None learns alone.
    unmeasured_memory: Option<u64>,
    /// Test peaks are reusable only at the same test concurrency.
    test_jobs: usize,
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
    disk_budget: u64,
    min_free: u64,
    build_headroom: u64,
    /// The volume observation for this scheduling turn. Unknown space
    /// cannot authorize a build.
    free_disk: Option<u64>,
    disk_wait: Option<DiskWait>,
    /// How long admission waits for disk only someone freeing space can
    /// provide before refusing builds that do not fit.
    disk_wait_limit: Duration,
    /// Since when admission has waited for such disk.
    disk_stalled: Option<Instant>,
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
            && self.exclusive == submission.exclusive
    }

    fn queued(&self) -> bool {
        matches!(self.state, State::Queued)
    }

    /// The busy slot it waits for although another is idle.
    fn held(&self) -> Option<&Hold> {
        match &self.wait {
            Some(Wait::Warm { hold }) => Some(hold),
            _ => None,
        }
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

    pub(crate) fn new(config: &crate::config::Config, distance: D) -> Self {
        assert!(config.slots > 0, "a scheduler runs at least one build");
        assert!(config.test_jobs > 0, "tests have a positive concurrency");
        Self {
            capacity: config.slots,
            memory: config.memory,
            unmeasured_memory: config.unmeasured_memory,
            test_jobs: config.test_jobs,
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
            disk_budget: config.disk,
            min_free: config.min_free,
            build_headroom: config.build_headroom,
            free_disk: None,
            disk_wait: None,
            disk_wait_limit: config.disk_wait,
            disk_stalled: None,
        }
    }

    /// Updates the external observation before applying this turn's event.
    pub(crate) fn observe_disk(&mut self, free: Option<u64>) {
        self.free_disk = free;
    }

    pub(crate) fn disk_space(&self) -> Option<u64> {
        self.free_disk
    }

    pub(crate) fn disk_wait(&self) -> Option<&DiskWait> {
        self.disk_wait.as_ref()
    }

    /// Maintenance also runs without builds finishing: a running build or
    /// another process can consume the volume while the queue waits.
    pub(crate) fn maintain(&mut self) -> Vec<Effect> {
        self.start_ready()
    }

    /// Takes back slot `index` of `repository`, left on disk by an earlier
    /// daemon with its checkout at `revision` and its `record`. It is due
    /// for measurement like a slot after a build.
    pub(crate) fn restore(
        &mut self,
        repository: PathBuf,
        index: usize,
        revision: Option<Revision>,
        record: SlotRecord,
    ) {
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
            growing: None,
            inventory: None,
            maintenance_error: None,
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
            exclusive,
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
                    exclusive,
                    waiters: Vec::new(),
                    order,
                    state: State::Queued,
                    output: Vec::new(),
                    compiled: 0,
                    fresh: 0,
                    units: HashMap::new(),
                    wait: None,
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
            State::Queued => std::iter::once(Message::Queued {
                revision: job.revision.clone(),
            })
            .chain(job.wait.clone().map(|wait| Message::Waiting { wait }))
            .map(|message| Effect::Send { waiter, message })
            .collect(),
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
            // Withdrawing an ordinary waiter can make this optional again,
            // moving an exclusive barrier behind ordinary queued work.
            return self.start_ready();
        }
        let mut effects = match &mut job.state {
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
                    Effect::Cancel {
                        job: id,
                        reason: Stop::Withdrawn,
                    },
                    Effect::Report(EventKind::Cancelled {
                        slot: slot_name(&slot.repository, slot.index),
                        operation: job.operation.clone(),
                        revision: job.revision.clone(),
                    }),
                ]
            }
        };
        effects.extend(self.start_ready());
        effects
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

    /// The running job entered testing. Release its compilation reservation
    /// when a test-phase peak is known, and admit work that now fits.
    pub(crate) fn testing(&mut self, id: JobId) -> Vec<Effect> {
        let test_memory = self.jobs.get(&id).and_then(|job| {
            let compilation = Compilation::of(job);
            self.slots
                .iter()
                .filter(|slot| slot.repository == job.repository)
                .flat_map(|slot| &slot.compilations)
                .filter(|run| run.compilation == compilation)
                .filter_map(|run| run.test_memory)
                .filter(|tests| tests.jobs == self.test_jobs)
                .map(|tests| tests.peak_memory)
                .max()
        });
        // Like output, a report can arrive after the build ended.
        let Some(Job {
            state: State::Running {
                testing_at, memory, ..
            },
            operation,
            ..
        }) = self.jobs.get_mut(&id)
        else {
            return Vec::new();
        };
        if !operation.command.runs_tests() || testing_at.is_some() {
            return Vec::new();
        }
        *testing_at = Some(Instant::now());
        if let Some(peak) = test_memory {
            *memory = Some(peak);
        }
        self.start_ready()
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
            testing_at,
            ..
        } = job.state
        else {
            panic!("only running jobs exit");
        };
        let test_ms = testing_at
            .filter(|_| job.operation.command.runs_tests())
            .map(|testing_at| millis(testing_at.elapsed()));
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
                usage,
            );
        }
        // What a killed or cancelled build added says little about the next.
        if !matches!(outcome, Outcome::Exited { .. }) {
            entry.growing = None;
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
            usage: usage.map(Box::new),
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
            slot.growing = None;
            slot.inventory = None;
            slot.maintenance_error = None;
            slot.duplicated = None;
            // The cleanup receipt covers its selection. Independently, the
            // scheduler must account for what remains before admitting work.
            slot.unmeasured = 1;
            slot.idle_since = Instant::now();
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
                slot.maintenance_error = None;
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
                if let Some((compilation, before)) = slot.growing.take() {
                    slot.grew(
                        compilation,
                        DiskUse {
                            added: inventory.total.saturating_sub(before),
                            total: inventory.total,
                        },
                    );
                    effects.push(slot.persist());
                }
                slot.inventory = Some(inventory);
            }
            Err(error) => {
                slot.size = None;
                slot.growing = None;
                slot.inventory = None;
                slot.maintenance_error = Some(error);
                // Filesystem failures hold admission. A quiet-period scan
                // observes recovery without spinning under disk pressure.
                slot.unmeasured = 1;
                slot.idle_since = Instant::now();
            }
        }
        effects.extend(self.start_ready());
        effects
    }

    /// What the slots' targets hold together, as last measured.
    pub(crate) fn disk_used(&self) -> u64 {
        self.slots.iter().filter_map(|slot| slot.size).sum()
    }

    /// Chooses what idle slots give up to cover `shortage`: what builds used
    /// longest ago first, across every measured idle slot but `keep`. None
    /// while an eviction is under way, or when no such slot has anything to
    /// give up. The slots it chooses from are busy until [`Self::evicted`].
    pub(crate) fn evict(&mut self, shortage: Shortage, keep: Option<usize>) -> Option<Effect> {
        let needed = shortage.needed();
        assert!(needed > 0, "an eviction covers a shortage");
        if self.evicting.is_some() {
            return None;
        }
        let mut candidates = self
            .slots
            .iter()
            .enumerate()
            .filter(|(index, slot)| !slot.busy() && keep != Some(*index))
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
            .filter(|(index, slot)| !slot.busy() && keep != Some(*index))
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
            if let Err(error) = result {
                // What is left is unknown until it is measured again.
                slot.unmeasured = slot.unmeasured.max(1);
                slot.idle_since = Instant::now();
                slot.size = None;
                slot.maintenance_error = Some(error);
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
    /// repositories go first. Every build left queued learns why it waits.
    fn start_ready(&mut self) -> Vec<Effect> {
        let mut effects = Vec::new();
        if let Some(free) = self.free_disk.filter(|free| *free < self.min_free) {
            for (id, job) in &mut self.jobs {
                if let State::Running {
                    cancelled, slot, ..
                } = &mut job.state
                    && !*cancelled
                {
                    *cancelled = true;
                    effects.push(Effect::Cancel {
                        job: *id,
                        reason: Stop::Failed(format!(
                            "stopped to protect the free disk floor: {} free, floor {}; \
                             build output exceeded the reserved headroom or the volume filled",
                            crate::log::gib(free),
                            crate::log::gib(self.min_free),
                        )),
                    });
                    effects.push(Effect::Report(EventKind::Cancelled {
                        slot: slot_name(&self.slots[*slot].repository, self.slots[*slot].index),
                        operation: job.operation.clone(),
                        revision: job.revision.clone(),
                    }));
                }
            }
        }
        let mut waits = HashMap::new();
        self.disk_wait = self.keep_disk(None, &mut effects);
        if let Some(wait) = self.disk_wait.clone() {
            let refuse = self.disk_stalled(&wait);
            let queued = self.queue_order();
            for id in queued {
                if refuse {
                    effects.extend(self.refuse(id, &wait));
                } else {
                    waits.insert(id, Wait::Disk { disk: wait.clone() });
                }
            }
            effects.extend(self.tell(waits));
            return effects;
        }
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
        // Reasons belong to this pass. Remember earlier holds only to avoid
        // reporting the same decision again while it remains valid.
        let previously_held = self
            .jobs
            .iter()
            .filter(|(_, job)| job.queued() && job.held().is_some())
            .map(|(id, _)| *id)
            .collect::<HashSet<_>>();
        // Why the builds this pass does not reach wait.
        let mut rest = None;
        for id in self.queue_order() {
            let busy = self.slots.iter().filter(|slot| slot.busy()).count();
            if busy >= self.capacity {
                rest = Some(Wait::Slot);
                break;
            }
            if self.jobs[&id].optional() && (others_wait || self.capacity - busy < 2) {
                waits.insert(id, if others_wait { Wait::Queue } else { Wait::Slot });
                continue;
            }
            // An exclusive build is a host-wide queue barrier. Do not let
            // later work prolong its wait while existing process groups end.
            if self
                .jobs
                .values()
                .any(|job| !job.queued() && (job.exclusive || self.jobs[&id].exclusive))
            {
                rest = Some(Wait::Exclusive);
                break;
            }
            // A build that does not fit waits, and so does everything behind
            // it: smaller builds slipping past would keep it waiting for good.
            // An unmeasured build uses the host's explicit reservation, or
            // learns alone when the host has not sized unfamiliar builds.
            // With nothing running, it runs whatever it needs.
            let memory = self.memory_estimate(&self.jobs[&id]);
            if let Some(reserved) = self.memory_hold(memory, in_use) {
                waits.insert(
                    id,
                    Wait::Memory {
                        needed: reserved,
                        available: self.memory.saturating_sub(in_use),
                    },
                );
                rest = Some(Wait::Queue);
                break;
            }
            let reserved = memory.or(self.unmeasured_memory).unwrap_or(self.memory);
            let (chosen, warm) = match self.choose_slot(id, &claimed) {
                Placement::Start { slot, warm } => (Some(slot), warm),
                Placement::Create => (None, false),
                Placement::Wait => {
                    waits.insert(id, Wait::Slot);
                    continue;
                }
                Placement::Hold { hold, slot } => {
                    let expected = self.estimate(&self.jobs[&id]).unwrap_or(0);
                    *claimed.entry(slot).or_default() += expected;
                    let job = &self.jobs[&id];
                    if !previously_held.contains(&id) {
                        effects.push(Effect::Report(EventKind::Held {
                            who: job.who(),
                            operation: job.operation.clone(),
                            hold: hold.clone(),
                        }));
                    }
                    waits.insert(id, Wait::Warm { hold });
                    continue;
                }
            };
            let growth = self.growth(chosen, &Compilation::of(&self.jobs[&id]));
            // All callers, including exits, measurements and cache releases,
            // make the same disk decision before a slot becomes busy again.
            self.disk_wait = self.keep_disk(Some((growth, chosen)), &mut effects);
            if let Some(wait) = self.disk_wait.clone() {
                if self.disk_stalled(&wait) {
                    effects.extend(self.refuse(id, &wait));
                    continue;
                }
                waits.insert(id, Wait::Disk { disk: wait });
                rest = Some(Wait::Queue);
                break;
            }
            let slot = chosen.unwrap_or_else(|| self.create_slot(id));
            in_use += reserved;
            self.clock += 1;
            let job = &self.jobs[&id];
            // A command of the client's own compiles nothing: it is never a
            // worktree's warm or cold first build.
            let first = job.operation.command.uses_cargo()
                && self.built_worktrees.insert(job.waiters[0].worktree.clone());
            let entry = &mut self.slots[slot];
            entry.advance_generation();
            entry.job = Some(id);
            // Its build changes what its target holds. Measured right before
            // it, the next measurement tells what it added.
            entry.inventory = None;
            entry.growing = entry
                .size
                .filter(|_| entry.unmeasured == 0)
                .map(|size| (Compilation::of(job), size));
            entry.revision = Some(job.revision.clone());
            entry.worktree = Some(job.waiters[0].worktree.clone());
            entry.used = self.clock;
            let job = self.jobs.get_mut(&id).expect("chosen above");
            job.wait = None;
            job.state = State::Running {
                slot,
                started: Instant::now(),
                testing_at: None,
                copying: false,
                cancelled: false,
                memory,
                disk: growth.bytes,
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
                exclusive: job.exclusive,
                who: job.who(),
                passed: if job.operation.runs_every_test() && !job.rerun_all {
                    self.slots[slot].passed.clone()
                } else {
                    Vec::new()
                },
            }));
        }
        if let Some(rest) = rest {
            for (id, _) in self.jobs.iter().filter(|(_, job)| job.queued()) {
                waits.entry(*id).or_insert_with(|| rest.clone());
            }
        }
        // A later build may have taken the last idle slot after earlier
        // builds chose to wait for warmth. That choice no longer delays them.
        if self.slots.iter().filter(|slot| slot.busy()).count() >= self.capacity {
            for wait in waits.values_mut() {
                if let Wait::Warm { .. } = wait {
                    *wait = Wait::Slot;
                }
            }
        }
        if self.disk_wait.is_none() {
            self.disk_stalled = None;
        }
        effects.extend(self.tell(waits));
        effects
    }

    /// Records why each queued job waits, from `waits`, which names them
    /// all, and tells the clients of those whose reason changed.
    fn tell(&mut self, mut waits: HashMap<JobId, Wait>) -> Vec<Effect> {
        let mut effects = Vec::new();
        for (id, job) in self.jobs.iter_mut().filter(|(_, job)| job.queued()) {
            let wait = waits.remove(id).expect("every queued job has a reason");
            if job
                .wait
                .as_ref()
                .is_none_or(|told| !told.same_reason(&wait))
            {
                effects.extend(job.send_all(&Message::Waiting { wait: wait.clone() }));
            }
            job.wait = Some(wait);
        }
        assert!(waits.is_empty(), "only queued jobs wait");
        effects
    }

    /// Whether queued builds have waited for disk only someone freeing
    /// space can provide for `disk_wait_limit`: `wait` is a shortfall idle
    /// artifacts cannot cover, and no build runs whose end would release
    /// its reservation. Any other wait, or an empty queue, restarts the
    /// count.
    fn disk_stalled(&mut self, wait: &DiskWait) -> bool {
        let lasting = matches!(wait, DiskWait::Headroom(_) | DiskWait::Budget { .. })
            && self.jobs.values().all(Job::queued)
            && !self.jobs.is_empty();
        if !lasting {
            self.disk_stalled = None;
            return false;
        }
        self.disk_stalled.get_or_insert_with(Instant::now).elapsed() >= self.disk_wait_limit
    }

    /// Gives up queued job `id`, which `wait` keeps from starting.
    fn refuse(&mut self, id: JobId, wait: &DiskWait) -> Vec<Effect> {
        let job = self.jobs.remove(&id).expect("a queued job is refused");
        assert!(job.queued(), "only a queued job is refused");
        let reason = format!(
            "refused: for {} s no build ran on this host and idle build artifacts could not \
             cover the disk this build needs: {wait}. Free space and retry, or adjust \
             min_free_gib, build_headroom_gib or disk_wait_secs in buildd's config.toml",
            self.disk_wait_limit.as_secs()
        );
        for waiter in &job.waiters {
            self.waiting.remove(&waiter.id);
        }
        let mut effects = job
            .send_all(&Message::Rejected {
                reason: reason.clone(),
            })
            .collect::<Vec<_>>();
        effects.push(Effect::Report(EventKind::Refused {
            who: job.who(),
            operation: job.operation,
            revision: job.revision,
            reason,
        }));
        effects
    }

    /// What a build of `compilation` is expected to add to slot `slot`, or
    /// to a new slot when None: its history there, else the host's
    /// headroom for a build without history.
    fn growth(&self, slot: Option<usize>, compilation: &Compilation) -> Growth {
        match slot.and_then(|slot| self.slots[slot].growth(compilation)) {
            Some(bytes) => Growth {
                bytes,
                measured: true,
            },
            None => Growth {
                bytes: self.build_headroom,
                measured: false,
            },
        }
    }

    /// Gives maintenance first claim on idle slots. Measurements are forced
    /// under pressure rather than waiting for the quiet/periodic scan. A
    /// prospective `build`, expected to grow by its growth in the slot it
    /// takes (a new one when None), reserves output space together with
    /// running builds; that slot gives up nothing for it.
    fn keep_disk(
        &mut self,
        build: Option<(Growth, Option<usize>)>,
        effects: &mut Vec<Effect>,
    ) -> Option<DiskWait> {
        let Some(free) = self.free_disk else {
            return Some(DiskWait::Unavailable);
        };
        let headroom = Headroom {
            free,
            floor: self.min_free,
            running: self.disk_reserved(),
            build: build.map(|(growth, _)| growth),
        };
        let shortage = Shortage {
            over_budget: self.disk_used().saturating_sub(self.disk_budget),
            under_floor: headroom.required().saturating_sub(free),
        };
        if let Some(slot) = self
            .slots
            .iter()
            .find(|slot| slot.maintenance_error.is_some())
        {
            return Some(DiskWait::MaintenanceFailed {
                slot: slot_name(&slot.repository, slot.index),
                reason: slot.maintenance_error.clone().expect("selected above"),
            });
        }
        // Unmeasured completed builds reserve one output allowance each.
        // Near the budget this forces a scan before stale sizes admit more
        // work; away from it, short edit/build loops keep their quiet period.
        let unmeasured = self
            .slots
            .iter()
            .filter(|slot| slot.job.is_none())
            .map(|slot| u64::from(slot.unmeasured))
            .sum::<u64>();
        let near_budget = self
            .disk_used()
            .saturating_add(self.build_headroom.saturating_mul(unmeasured))
            > self.disk_budget;
        let unknown = build.is_some()
            && self
                .slots
                .iter()
                .any(|slot| slot.size.is_none() && slot.unmeasured > 0);
        if shortage.needed() > 0 || near_budget || unknown {
            let dirty = self
                .slots
                .iter()
                .enumerate()
                .filter(|(_, slot)| !slot.busy() && slot.unmeasured > 0)
                .map(|(index, _)| index)
                .collect::<Vec<_>>();
            if !dirty.is_empty() {
                effects.extend(dirty.into_iter().map(|index| self.measure(index)));
                return Some(DiskWait::Measuring);
            }
        }
        if self.evicting.is_some() {
            return Some(DiskWait::Evicting);
        }
        // Wait for every in-flight scan before choosing across idle caches.
        // Its result may contain the oldest artifacts, or the needed space.
        if (shortage.needed() > 0 || near_budget || unknown)
            && self
                .slots
                .iter()
                .any(|slot| slot.chore == Some(Chore::Measuring))
        {
            return Some(DiskWait::Measuring);
        }
        if (shortage.needed() > 0 || near_budget || unknown)
            && self
                .slots
                .iter()
                .any(|slot| slot.chore == Some(Chore::Cache) && slot.unmeasured > 0)
        {
            return Some(DiskWait::Protected);
        }
        if shortage.needed() == 0 {
            return None;
        }
        if let Some(effect) = self.evict(shortage, build.and_then(|(_, slot)| slot)) {
            effects.push(effect);
            return Some(DiskWait::Evicting);
        }
        Some(if shortage.under_floor > 0 {
            DiskWait::Headroom(headroom)
        } else {
            DiskWait::Budget {
                used: self.disk_used(),
                budget: self.disk_budget,
            }
        })
    }

    /// The queued jobs, the one to start first first: shortest expected
    /// build minus time waited, so short builds go ahead and a long one goes
    /// once it has waited its own length; then submission order.
    fn queue_order(&self) -> Vec<JobId> {
        let barriers = self
            .jobs
            .values()
            .filter(|job| job.queued() && job.exclusive)
            .map(|job| (job.optional(), job.order))
            .collect::<Vec<_>>();
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
                // Keep the existing shortest-first policy between exclusive
                // submissions, without letting later work cross a barrier.
                // Optional work still comes after every ordinary build.
                let barrier = barriers
                    .iter()
                    .filter(|(optional, order)| *optional == job.optional() && *order >= job.order)
                    .map(|(_, order)| *order)
                    .min()
                    .unwrap_or(u64::MAX);
                (
                    job.optional(),
                    barrier,
                    job.exclusive,
                    expected.saturating_sub(waited),
                    job.order,
                    *id,
                )
            })
            .collect::<Vec<_>>();
        queued.sort_unstable();
        queued.into_iter().map(|(.., id)| id).collect()
    }

    /// The running builds' measured peaks or configured reservations.
    fn memory_in_use(&self) -> u64 {
        self.jobs
            .values()
            .filter_map(|job| match job.state {
                State::Running { memory, .. } => {
                    Some(memory.or(self.unmeasured_memory).unwrap_or(self.memory))
                }
                State::Queued => None,
            })
            .sum()
    }

    /// Admission and planning use the same reservation and exclusive-learning
    /// policy. None means memory does not currently hold the request.
    fn memory_hold(&self, peak: Option<u64>, in_use: u64) -> Option<u64> {
        let running = self.jobs.values().any(|job| !job.queued());
        let exclusive = self.unmeasured_memory.is_none()
            && (peak.is_none()
                || self
                    .jobs
                    .values()
                    .any(|job| matches!(job.state, State::Running { memory: None, .. })));
        let reserved = peak.or(self.unmeasured_memory).unwrap_or(self.memory);
        (running && (exclusive || in_use.saturating_add(reserved) > self.memory))
            .then_some(reserved)
    }

    /// Memory running builds may use together, and what the running builds
    /// are expected to use.
    pub(crate) fn memory(&self) -> (u64, u64) {
        (self.memory, self.memory_in_use())
    }

    /// Largest recorded peak of this exact compilation. Execution filters
    /// cannot lower a broader run's peak; another package selection is unknown.
    fn memory_estimate(&self, job: &Job) -> Option<u64> {
        let compilation = Compilation::of(job);
        self.compilation_memory(&job.repository, &compilation)
    }

    /// The same history admission uses, without adding a queued job.
    pub(crate) fn forecast_memory(
        &self,
        repository: &Path,
        prefix: &Path,
        operation: &Operation,
    ) -> Option<u64> {
        self.compilation_memory(repository, &Compilation::new(prefix, operation))
    }

    /// What a build of `operation` is expected to add to the slot it takes:
    /// the least any slot of its repository expects, as the warm slot it
    /// prefers usually has its history; without history, the headroom.
    pub(crate) fn forecast_growth(
        &self,
        repository: &Path,
        prefix: &Path,
        operation: &Operation,
    ) -> u64 {
        let compilation = Compilation::new(prefix, operation);
        self.slots
            .iter()
            .filter(|slot| slot.repository == repository)
            .filter_map(|slot| slot.growth(&compilation))
            .min()
            .unwrap_or(self.build_headroom)
    }

    /// What running builds reserve for their output.
    pub(crate) fn disk_reserved(&self) -> u64 {
        self.jobs
            .values()
            .filter_map(|job| match job.state {
                State::Running { disk, .. } => Some(disk),
                State::Queued => None,
            })
            .sum()
    }

    fn compilation_memory(&self, repository: &Path, compilation: &Compilation) -> Option<u64> {
        self.slots
            .iter()
            .filter(|slot| slot.repository == repository)
            .flat_map(|slot| &slot.compilations)
            .filter(|run| &run.compilation == compilation)
            .filter_map(|run| run.peak_memory)
            .max()
    }

    /// How long a build of `job` is expected to take: the latest successful
    /// build of its compilation, or the average of builds of the same
    /// command in the same directory.
    fn estimate(&self, job: &Job) -> Option<u64> {
        let compilation = Compilation::of(job);
        if let Some(ms) = self.compilation_time(&job.repository, &compilation) {
            return Some(ms);
        }
        average(
            self.slots
                .iter()
                .filter(|slot| slot.repository == job.repository)
                .flat_map(|slot| &slot.compilations)
                .filter(|run| {
                    run.compilation.prefix == compilation.prefix
                        && run.compilation.command == compilation.command
                }),
        )
    }

    fn compilation_time(&self, repository: &Path, compilation: &Compilation) -> Option<u64> {
        self.slots
            .iter()
            .filter(|slot| slot.repository == repository)
            .flat_map(|slot| &slot.compilations)
            .filter(|run| &run.compilation == compilation && run.build_ms.is_some())
            .max_by_key(|run| run.at)
            .and_then(|run| run.build_ms)
    }

    /// The same cache evidence and ordering used by `choose_slot`. Reading
    /// a forecast may populate the distance cache, but never changes slots,
    /// jobs, queue order, admission or compilation history.
    fn slot_plans(
        &mut self,
        repository: &Path,
        revision: &Revision,
        compilation: &Compilation,
        include_busy: bool,
    ) -> Vec<(usize, SlotPlan)> {
        let (needed, known) = needed(&self.slots, repository, compilation);
        let mut plans = Vec::new();
        for (index, slot) in self.slots.iter().enumerate() {
            if slot.repository != repository || (!include_busy && slot.busy()) {
                continue;
            }
            let missing = needed
                .iter()
                .filter(|unit| !slot.units.contains_key(**unit))
                .count() as u64;
            let distance = slot.revision.as_ref().and_then(|from| {
                let distance = self.distance.distance(repository, from, revision);
                (distance != u64::MAX).then_some(distance)
            });
            let wait = match slot.job {
                Some(id) => self.remaining_ms(&self.jobs[&id]),
                None if slot.chore.is_some() => None,
                None => Some(0),
            };
            let compile_ms = distance
                .filter(|_| compilation.command.uses_cargo())
                .and_then(|distance| {
                    self.compile_cost
                        .get(repository)?
                        .ms(missing.saturating_add(distance))
                });
            plans.push((
                index,
                SlotPlan {
                    slot: slot_name(repository, slot.index),
                    available: !slot.busy(),
                    new: false,
                    exact_history: known,
                    reused_units: needed.len() as u64 - missing,
                    missing_units: missing,
                    source_work: distance,
                    slot_wait_ms: wait,
                    compile_ms,
                    note: slot.chore.map(|chore| format!("{chore:?}")),
                },
            ));
        }
        plans.sort_by_key(|(index, plan)| {
            (
                !plan.available,
                plan.missing_units
                    .saturating_add(plan.source_work.unwrap_or(u64::MAX)),
                u64::MAX - self.slots[*index].used,
                *index,
            )
        });
        plans
    }

    fn remaining_ms(&self, job: &Job) -> Option<u64> {
        let State::Running {
            started,
            cancelled: false,
            ..
        } = job.state
        else {
            return None;
        };
        let elapsed = millis(started.elapsed());
        self.estimate(job)
            .filter(|expected| elapsed <= expected.saturating_mul(2))
            .map(|expected| expected.saturating_sub(elapsed))
    }

    pub(crate) fn forecast(&mut self, submission: &Submission, capacity: Capacity) -> Forecast {
        let repository = &submission.source.repository;
        let compilation = Compilation::new(&submission.source.prefix, &submission.operation);
        let plans = self.slot_plans(repository, &submission.revision, &compilation, true);
        let mut notes = Vec::new();
        let status = &capacity.status;
        if status.draining {
            notes.push("host is draining".into());
        }
        if !status.queue.is_empty() {
            notes.push(format!(
                "{} queued builds; their completion and admission order may change the slot choice",
                status.queue.len()
            ));
        }
        if submission.exclusive {
            if self.jobs.values().any(|job| !job.queued()) {
                notes.push(
                    "exclusive build waits for all active builds on this host to finish".into(),
                );
            }
        } else if self.jobs.values().any(|job| job.exclusive) {
            notes.push("exclusive work holds admission for later builds on this host".into());
        }
        if self.slots.iter().filter(|slot| slot.busy()).count() >= self.capacity {
            notes.push("all concurrent build capacity is occupied".into());
        }
        if submission.optional
            && (self
                .jobs
                .values()
                .any(|job| job.queued() && !job.optional())
                || self
                    .capacity
                    .saturating_sub(self.slots.iter().filter(|slot| slot.busy()).count())
                    < 2)
        {
            notes.push(
                "optional work must leave another slot free and wait behind ordinary builds".into(),
            );
        }
        if let Some(reserved) = self.memory_hold(capacity.peak_memory, self.memory_in_use()) {
            notes.push(format!(
                "waiting for memory admission ({reserved} bytes reserved)"
            ));
        }
        if let Some(wait) = &status.disk_wait {
            notes.push(wait.to_string());
        }
        if self.disk_used() > self.disk_budget {
            notes.push("cache disk budget must be met before admission".into());
        }
        if self
            .slots
            .iter()
            .any(|slot| slot.size.is_none() && slot.unmeasured > 0)
        {
            notes.push("artifact inventories must be measured before admission".into());
        }
        let shared_slot = self.jobs.values().find_map(|job| {
            if !job.cancelled() && job.same_build(submission) {
                match job.state {
                    State::Running { slot, .. } => {
                        Some(slot_name(repository, self.slots[slot].index))
                    }
                    State::Queued => None,
                }
            } else {
                None
            }
        });
        let (needed, known) = needed(&self.slots, repository, &compilation);
        let needed = needed
            .into_iter()
            .map(str::to_owned)
            .collect::<HashSet<_>>();
        let mut recommended_slot = plans
            .iter()
            .find(|(_, plan)| plan.available)
            .map(|(_, plan)| plan.slot.clone());
        let mut held = false;
        let mut new_slot = None;
        if let Some((_, idle)) = plans.iter().find(|(_, plan)| plan.available) {
            let claimed =
                self.jobs
                    .values()
                    .filter_map(|job| {
                        let hold = job.held()?;
                        let slot = self.slots.iter().position(|slot| {
                            slot_name(&slot.repository, slot.index) == hold.slot
                        })?;
                        Some((slot, self.estimate(job).unwrap_or(0)))
                    })
                    .fold(HashMap::<usize, u64>::new(), |mut claimed, (slot, ms)| {
                        *claimed.entry(slot).or_default() += ms;
                        claimed
                    });
            let work = idle
                .missing_units
                .saturating_add(idle.source_work.unwrap_or(u64::MAX));
            if let Some((hold, _)) = self.hold(
                repository,
                &submission.revision,
                submission.optional,
                &needed,
                work,
                &claimed,
            ) {
                recommended_slot = Some(hold.slot.clone());
                notes.push(format!(
                    "waiting for warm slot {} is estimated at {} ms through compilation, versus {} ms compiling in the best idle slot",
                    hold.slot, hold.wait_ms, hold.cold_ms,
                ));
                held = true;
            }
        } else if plans.len() < self.capacity
            && !plans
                .iter()
                .any(|(index, _)| self.slots[*index].chore.is_some())
        {
            let index = (0..self.capacity)
                .find(|index| {
                    !plans
                        .iter()
                        .any(|(slot, _)| self.slots[*slot].index == *index)
                })
                .expect("fewer than capacity slots leave an index free");
            let slot = slot_name(repository, index);
            recommended_slot = Some(slot.clone());
            new_slot = Some(SlotPlan {
                slot,
                available: true,
                new: true,
                exact_history: known,
                reused_units: 0,
                missing_units: needed.len() as u64,
                source_work: Some(0),
                slot_wait_ms: Some(0),
                compile_ms: None,
                note: Some("a cold slot would be created on admission".into()),
            });
        }
        let target = recommended_slot.as_ref().and_then(|name| {
            self.slots
                .iter()
                .position(|slot| slot_name(&slot.repository, slot.index) == *name)
        });
        if let Some(free) = status.free_disk {
            let headroom = Headroom {
                free,
                floor: self.min_free,
                running: self.disk_reserved(),
                build: Some(self.growth(target, &compilation)),
            };
            if !headroom.covered() {
                notes.push(format!("disk admission: {headroom}"));
            }
        }
        let historical_build_ms = self.compilation_time(repository, &compilation);
        let queue_ms =
            if shared_slot.is_some() || (notes.is_empty() && !held && recommended_slot.is_some()) {
                Some(0)
            } else {
                None
            };
        if let Some(shared) = &shared_slot {
            recommended_slot = Some(shared.clone());
            notes.clear();
            notes.push(
                "would join an identical running build without reserving more resources".into(),
            );
        }
        if submission.exclusive {
            notes.push(
                "runs alone across every repository on this host; later submissions wait".into(),
            );
        }
        if submission.operation.command.uses_cargo() && !known {
            notes.push(
                "no compiled-unit history for this exact package, feature and flag selection"
                    .into(),
            );
        }
        notes.push(
            "slot waits exclude queued work; compilation estimates exclude test execution".into(),
        );
        let mut slots = plans.into_iter().map(|(_, plan)| plan).collect::<Vec<_>>();
        if let Some(new_slot) = new_slot {
            slots.insert(0, new_slot);
        }
        Forecast {
            capacity,
            recommended_slot,
            shared_slot,
            historical_build_ms,
            queue_ms,
            slots,
            notes,
        }
    }

    /// The slot queued job `id` runs in now, as the module explains, and
    /// whether that slot holds everything the build needs; or why it waits.
    /// `claimed` is what builds already held for each busy slot are expected
    /// to take there.
    fn choose_slot(&mut self, id: JobId, claimed: &HashMap<usize, u64>) -> Placement {
        let job = &self.jobs[&id];
        let repository = job.repository.clone();
        let revision = job.revision.clone();
        let compilation = Compilation::of(job);
        let optional = job.optional();
        let plans = self.slot_plans(&repository, &revision, &compilation, false);
        if let Some((index, plan)) = plans.iter().find(|(_, plan)| plan.available) {
            let work = plan
                .missing_units
                .saturating_add(plan.source_work.unwrap_or(u64::MAX));
            let warm = plan.exact_history
                && plan.missing_units == 0
                && self.slots[*index].revision.is_some();
            let (needed, _) = needed(&self.slots, &repository, &compilation);
            let needed = needed.into_iter().map(str::to_owned).collect();
            return match self.hold(&repository, &revision, optional, &needed, work, claimed) {
                Some((hold, slot)) => Placement::Hold { hold, slot },
                None => Placement::Start { slot: *index, warm },
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
        Placement::Create
    }

    /// Adds a slot for queued job `id`, which [`Self::choose_slot`] placed
    /// in a new one, and returns its index.
    fn create_slot(&mut self, id: JobId) -> usize {
        let job = &self.jobs[&id];
        // Restored slots can leave gaps: take the lowest free index.
        let index = (0..self.capacity)
            .find(|index| {
                !self
                    .slots
                    .iter()
                    .any(|slot| slot.repository == job.repository && slot.index == *index)
            })
            .expect("a repository with fewer than capacity slots has a free index");
        self.slots.push(Slot {
            repository: job.repository.clone(),
            index,
            job: None,
            revision: Some(job.revision.clone()),
            worktree: Some(job.waiters[0].worktree.clone()),
            used: 0,
            chore: None,
            generation: 0,
            unmeasured: 0,
            idle_since: Instant::now(),
            size: None,
            growing: None,
            inventory: None,
            maintenance_error: None,
            last: None,
            duplicated: None,
            passed: Vec::new(),
            compilations: Vec::new(),
            units: HashMap::new(),
        });
        self.slots.len() - 1
    }

    /// Whether a request whose best idle slot leaves `work` units to
    /// compile, should wait for a busy slot of its repository instead, and
    /// for which: the busy slot's build, and the builds already held for it
    /// (`claimed`), are expected to end and the job to build there sooner
    /// than the idle slot would compile its share cold. Without estimates it
    /// never waits.
    fn hold(
        &mut self,
        repository: &Path,
        revision: &Revision,
        optional: bool,
        needed: &HashSet<String>,
        work: u64,
        claimed: &HashMap<usize, u64>,
    ) -> Option<(Hold, usize)> {
        // Optional work never waits for a busy slot: it holds an idle one
        // at most.
        if optional || work == u64::MAX {
            return None;
        }
        let cost = self.compile_cost.get(repository)?;
        let cold_ms = cost.ms(work)?;
        let mut best: Option<(Hold, usize)> = None;
        for (index, slot) in self.slots.iter().enumerate() {
            let Some(running) = slot.job.filter(|_| slot.repository == repository) else {
                continue;
            };
            let running = &self.jobs[&running];
            let Some(remaining) = self.remaining_ms(running) else {
                continue;
            };
            let missing = needed
                .iter()
                .filter(|unit| !slot.units.contains_key(unit.as_str()))
                .count() as u64;
            let distance = self
                .distance
                .distance(repository, &running.revision, revision);
            if distance == u64::MAX {
                continue;
            }
            let wait_ms = remaining
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
                        testing_at,
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
                        exclusive: job.exclusive,
                        tokens,
                        phase: if copying {
                            Phase::Copying
                        } else if !job.operation.command.uses_cargo() {
                            Phase::Running
                        } else if job.operation.command.runs_tests() && testing_at.is_some() {
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
                    exclusive: job.exclusive,
                    estimate_ms: self.estimate(job),
                    wait: job.wait.clone(),
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
    /// A new slot of its repository, which has fewer than capacity.
    Create,
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
        .filter(|run| !run.units.is_empty())
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
    use std::collections::BTreeSet;

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
        let config = crate::config::Config {
            slots: capacity,
            jobs: 4,
            test_jobs: 2,
            memory: u64::MAX,
            unmeasured_memory: None,
            priority: crate::config::Priority::Low,
            disk: u64::MAX,
            min_free: 0,
            build_headroom: 0,
            disk_wait: Duration::from_secs(120),
            remotes: Vec::new(),
            prewarm: Vec::new(),
        };
        let mut scheduler = Scheduler::new(&config, Table::default());
        scheduler.observe_disk(Some(u64::MAX));
        scheduler
    }

    /// Restored measurements for tests of concurrent placement/disk policy.
    /// No compiled units are assumed: only this command's peak is known.
    fn known_check_memory(scheduler: &mut Scheduler<Table>, repository: &str) {
        scheduler.restore(
            repository.into(),
            0,
            None,
            SlotRecord {
                compilations: vec![CompilationRun {
                    compilation: Compilation::new(Path::new(""), &check()),
                    at: now(),
                    units: BTreeSet::new(),
                    build_ms: None,
                    peak_memory: Some(1),
                    test_memory: None,
                    disk: None,
                }],
                ..SlotRecord::default()
            },
        );
        let key = SlotKey(scheduler.slots.len() - 1);
        scheduler.measure(key.0);
        scheduler.measured(key, Ok(Inventory::default()));
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
            exclusive: false,
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
            exclusive: false,
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
            exclusive: false,
            optional: true,
        })
    }

    #[test]
    fn optional_work_waits_behind_other_builds_and_leaves_a_slot_free() {
        let mut scheduler = scheduler(3);
        known_check_memory(&mut scheduler, "/repo/.git");
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
        let next = complete_measurements(&mut scheduler, next);
        assert_eq!(starts(&next)[0].2, "q1");
    }

    #[test]
    fn cache_exclusion_counts_against_optional_work_and_release_starts_it() {
        let mut scheduler = scheduler(3);
        for repository in ["/repo/.git", "/b1/.git", "/p1/.git"] {
            known_check_memory(&mut scheduler, repository);
        }
        let [(running, ..)] = starts(&submit(&mut scheduler, 1, "/a", "a1"))[..] else {
            panic!("the first build starts");
        };
        scheduler.exited(running, &ok(), None, BuildReport::default());
        measure_all(&mut scheduler, 1);
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
                exclusive: false,
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
        let Some(Effect::Evict(removals)) = scheduler.evict(over_budget(1), None) else {
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
        let waiting = submit(&mut scheduler, 2, "/a", "a2");
        assert!(starts(&waiting).is_empty());
        assert_eq!(measures(&waiting), [0]);
        assert_eq!(scheduler.cache_generation(&name), Some(3));
        assert_eq!(
            starts(&scheduler.measured(SlotKey(0), Ok(Inventory::default()))).len(),
            1
        );
        assert_eq!(scheduler.cache_generation(&name), Some(4));
    }

    #[test]
    fn queued_writer_advances_generation_as_read_exclusion_is_released() {
        let mut scheduler = scheduler(1);
        let [(job, ..)] = starts(&submit(&mut scheduler, 1, "/a", "a1"))[..] else {
            panic!("start");
        };
        scheduler.exited(job, &ok(), None, BuildReport::default());
        measure_all(&mut scheduler, 1);
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

    /// Drive filesystem replies in tests of queue/placement behavior. Disk
    /// policy tests instead supply inventories with their explicit sizes.
    fn complete_measurements(
        scheduler: &mut Scheduler<Table>,
        mut effects: Vec<Effect>,
    ) -> Vec<Effect> {
        let pending = effects
            .iter()
            .filter_map(|effect| match effect {
                Effect::Measure(measurement) => Some(measurement.key),
                _ => None,
            })
            .collect::<Vec<_>>();
        for key in pending {
            let held = scheduler.slots[key.0]
                .units
                .keys()
                .cloned()
                .collect::<Vec<_>>();
            effects.extend(scheduler.measured(key, Ok(inventory(&held, 1))));
        }
        effects
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

    fn required(wait: &DiskWait) -> Option<u64> {
        match wait {
            DiskWait::Headroom(headroom) => Some(headroom.required()),
            _ => None,
        }
    }

    fn held_for(build: &QueuedBuild) -> Option<&Hold> {
        match &build.wait {
            Some(Wait::Warm { hold }) => Some(hold),
            _ => None,
        }
    }

    fn memory_needed(build: &QueuedBuild) -> Option<u64> {
        match build.wait {
            Some(Wait::Memory { needed, .. }) => Some(needed),
            _ => None,
        }
    }

    /// Runs `tree` for `worktree` to its end and returns the slot it ran in.
    fn run(scheduler: &mut Scheduler<Table>, waiter: u64, worktree: &str, tree: &str) -> usize {
        let effects = submit(scheduler, waiter, worktree, tree);
        let effects = complete_measurements(scheduler, effects);
        let [(job, slot, _)] = starts(&effects)[..] else {
            panic!("{tree} starts at once: {effects:?}");
        };
        scheduler.exited(job, &ok(), None, BuildReport::default());
        slot
    }

    mod exclusive {
        use super::*;

        fn request(waiter: u64, repository: &str, tree: &str, exclusive: bool) -> Submission {
            let mut source = source(&format!("/work/{waiter}"));
            source.repository = repository.into();
            Submission {
                waiter: WaiterId(waiter),
                source,
                revision: revision(tree),
                operation: check(),
                label: None,
                copy_to: None,
                rerun_all: false,
                exclusive,
                optional: false,
            }
        }

        #[test]
        fn waits_for_every_repository_and_blocks_spare_capacity() {
            let mut scheduler = scheduler(4);
            scheduler.unmeasured_memory = Some(1);
            let first = scheduler.submit(request(1, "/a", "a1", false));
            let second = scheduler.submit(request(2, "/b", "b1", false));
            let first = starts(&first)[0].0;
            let second = starts(&second)[0].0;
            assert!(starts(&scheduler.submit(request(3, "/c", "c1", true))).is_empty());
            assert!(starts(&scheduler.submit(request(4, "/d", "d1", false))).is_empty());
            let (_, queue) = scheduler.status(|_| 0);
            assert!(queue[0].exclusive);
            assert!(!queue[1].exclusive);
            assert!(
                starts(&scheduler.exited(first, &ok(), None, BuildReport::default())).is_empty()
            );
            let next = scheduler.exited(second, &ok(), None, BuildReport::default());
            let next = complete_measurements(&mut scheduler, next);
            let [(exclusive, _, ref tree)] = starts(&next)[..] else {
                panic!("only the exclusive build starts: {next:?}");
            };
            assert_eq!(tree, "c1");
            let (slots, _) = scheduler.status(|_| 0);
            assert_eq!(slots.iter().filter(|slot| slot.build.is_some()).count(), 1);
            assert!(
                slots
                    .iter()
                    .any(|slot| slot.build.as_ref().is_some_and(|build| build.exclusive))
            );
            assert!(starts(&scheduler.submit(request(5, "/e", "e1", false))).is_empty());
            let next = scheduler.exited(exclusive, &ok(), None, BuildReport::default());
            let next = complete_measurements(&mut scheduler, next);
            assert_eq!(
                starts(&next).len(),
                2,
                "ordinary concurrency resumes: {next:?}"
            );
        }

        #[test]
        fn later_short_builds_cannot_overtake_exclusive_work() {
            let mut scheduler = scheduler(4);
            scheduler.unmeasured_memory = Some(1);
            let first = scheduler.submit(request(1, "/a", "a1", false));
            let job = starts(&first)[0].0;
            assert!(starts(&scheduler.submit(request(2, "/a", "b1", true))).is_empty());
            // Exact history makes the exclusive build much longer than the
            // new work. Resource availability and shortest-first cannot bypass it.
            scheduler.slots[0].compilations.push(CompilationRun {
                compilation: Compilation::new(Path::new(""), &check()),
                at: now(),
                units: BTreeSet::new(),
                build_ms: Some(1_000_000),
                peak_memory: Some(1),
                test_memory: None,
                disk: None,
            });
            assert!(starts(&scheduler.submit(request(3, "/c", "c1", false))).is_empty());
            assert!(starts(&scheduler.submit(request(4, "/d", "d1", true))).is_empty());
            let next = scheduler.exited(job, &ok(), None, BuildReport::default());
            let next = complete_measurements(&mut scheduler, next);
            let [(exclusive, _, ref tree)] = starts(&next)[..] else {
                panic!("{next:?}");
            };
            assert_eq!(tree, "b1");
            let next = scheduler.exited(exclusive, &ok(), None, BuildReport::default());
            let next = complete_measurements(&mut scheduler, next);
            let [(ordinary, _, ref tree)] = starts(&next)[..] else {
                panic!("work before the second barrier runs first: {next:?}");
            };
            assert_eq!(tree, "c1");
            let next = scheduler.exited(ordinary, &ok(), None, BuildReport::default());
            let next = complete_measurements(&mut scheduler, next);
            assert_eq!(starts(&next)[0].2, "d1");
        }

        #[test]
        fn queued_withdrawal_releases_the_barrier_and_running_withdrawal_waits_for_exit() {
            let mut scheduler = scheduler(4);
            scheduler.unmeasured_memory = Some(1);
            let first = scheduler.submit(request(1, "/a", "a1", false));
            let first = starts(&first)[0].0;
            scheduler.submit(request(2, "/b", "b1", true));
            scheduler.submit(request(3, "/c", "c1", false));
            let released = scheduler.withdraw(WaiterId(2));
            let second = starts(&released)[0].0;
            scheduler.exited(first, &ok(), None, BuildReport::default());
            scheduler.exited(second, &ok(), None, BuildReport::default());
            let next = scheduler.submit(request(4, "/d", "d1", true));
            let next = complete_measurements(&mut scheduler, next);
            let exclusive = starts(&next)[0].0;
            scheduler.submit(request(5, "/e", "e1", false));
            let cancelled = scheduler.withdraw(WaiterId(4));
            assert!(
                cancelled.iter().any(
                    |effect| matches!(effect, Effect::Cancel { job, .. } if *job == exclusive)
                )
            );
            assert!(starts(&cancelled).is_empty());
            assert!(starts(&scheduler.maintain()).is_empty());
            let released = scheduler.exited(
                exclusive,
                &Outcome::Signaled { signal: 15 },
                None,
                BuildReport::default(),
            );
            let released = complete_measurements(&mut scheduler, released);
            assert_eq!(starts(&released)[0].2, "e1");
        }

        #[test]
        fn shared_and_exclusive_requests_neither_join_nor_supersede_each_other() {
            let mut scheduler = scheduler(4);
            scheduler.unmeasured_memory = Some(1);
            let first = scheduler.submit(request(1, "/a", "a1", false));
            let first = starts(&first)[0].0;
            let mut exclusive = request(2, "/a", "a1", true);
            exclusive.source.worktree = "/work/1".into();
            scheduler.submit(exclusive);
            assert_eq!(scheduler.jobs.len(), 2);
            let joined = scheduler.submit(request(3, "/a", "a1", true));
            assert!(starts(&joined).is_empty());
            assert_eq!(scheduler.jobs.len(), 2);
            assert_eq!(scheduler.status(|_| 0).1[0].who.len(), 2);
            let mut newer = request(5, "/a", "a2", false);
            newer.source.worktree = "/work/1".into();
            assert!(starts(&scheduler.submit(newer)).is_empty());
            assert_eq!(scheduler.status(|_| 0).1[0].revision, revision("a1"));
            let next = scheduler.exited(first, &ok(), None, BuildReport::default());
            let next = complete_measurements(&mut scheduler, next);
            let exclusive = starts(&next)[0].0;
            let joined = scheduler.submit(request(4, "/a", "a1", true));
            assert!(matches!(sent(&joined, 4)[0], Message::Started { .. }));
            let next = scheduler.exited(exclusive, &ok(), None, BuildReport::default());
            let next = complete_measurements(&mut scheduler, next);
            assert_eq!(starts(&next)[0].2, "a2");
        }

        #[test]
        fn optional_prewarming_waits_and_testing_and_copying_keep_the_host_exclusive() {
            let mut scheduler = scheduler(4);
            scheduler.unmeasured_memory = Some(1);
            let mut exclusive = request(1, "/a", "a1", true);
            exclusive.operation.command = Command::Test;
            let next = scheduler.submit(exclusive);
            let job = starts(&next)[0].0;
            let mut optional = request(2, "/b", "b1", false);
            optional.optional = true;
            assert!(starts(&scheduler.submit(optional)).is_empty());
            assert!(starts(&scheduler.testing(job)).is_empty());
            scheduler.copying(job);
            assert!(starts(&scheduler.maintain()).is_empty());
            let next = scheduler.exited(job, &ok(), None, BuildReport::default());
            let next = complete_measurements(&mut scheduler, next);
            assert_eq!(starts(&next)[0].2, "b1");
        }
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
            matches!(sent(&newer, 2)[..], [Message::Queued { revision }, Message::Waiting { wait: Wait::Slot }] if revision.to_string() == "t2")
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
        let next = complete_measurements(&mut scheduler, next);
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
            exclusive: false,
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
        let next = complete_measurements(&mut scheduler, next);
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
        assert_eq!(
            cancelled[0],
            Effect::Cancel {
                job,
                reason: Stop::Withdrawn
            }
        );
        assert!(matches!(
            reports(&cancelled)[..],
            [EventKind::Cancelled { .. }]
        ));
        // An equal request does not join the build being stopped.
        let again = submit(&mut scheduler, 3, "/a", "t1");
        assert!(matches!(
            sent(&again, 3)[..],
            [
                Message::Queued { .. },
                Message::Waiting { wait: Wait::Slot }
            ]
        ));
        let next = scheduler.exited(
            job,
            &Outcome::Signaled { signal: 15 },
            None,
            BuildReport::default(),
        );
        let next = complete_measurements(&mut scheduler, next);
        assert!(sent(&next, 1).is_empty());
        assert_eq!(starts(&next).len(), 1);
        assert!(scheduler.withdraw(WaiterId(1)).is_empty());
    }

    #[test]
    fn a_build_takes_the_idle_slot_whose_tree_is_closest() {
        let mut scheduler = scheduler(2);
        known_check_memory(&mut scheduler, "/repo/.git");
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
        known_check_memory(&mut scheduler, "/repo/.git");
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
    fn a_fresh_measurement_reclaims_old_cache_before_restarting_queued_work() {
        let mut scheduler = scheduler(1);
        scheduler.disk_budget = 50;
        let [(job, ..)] = starts(&submit(&mut scheduler, 1, "/a", "a1"))[..] else {
            panic!("the first build starts");
        };
        let unit = "debug/1111111111111111".to_owned();
        used(&mut scheduler, job, std::slice::from_ref(&unit));
        assert!(starts(&submit(&mut scheduler, 2, "/a", "a2")).is_empty());
        scheduler.slots[0].unmeasured = MEASURE_DUE - 1;
        let finished = scheduler.exited(job, &ok(), None, BuildReport::default());
        assert_eq!(measures(&finished), [0]);
        let mut held = inventory(std::slice::from_ref(&unit), 20);
        held.total += 80;
        held.items.push(Evictable {
            paths: vec!["/target/debug/incremental/old".into()],
            kind: Kind::Cache,
            written: SystemTime::UNIX_EPOCH,
            bytes: 80,
            freeable: 80,
        });
        let measured = scheduler.measured(SlotKey(0), Ok(held));
        assert!(
            starts(&measured).is_empty(),
            "queued work cannot steal the idle slot"
        );
        let eviction = measured
            .iter()
            .find(|effect| matches!(effect, Effect::Evict(_)))
            .unwrap();
        assert_eq!(
            removed(eviction),
            [(0, vec!["debug/incremental/old".into()])]
        );
        assert_eq!(scheduler.disk_wait(), Some(&DiskWait::Evicting));
        let ready = scheduler.evicted(vec![(SlotKey(0), Ok(()))]);
        assert_eq!(starts(&ready).len(), 1);
        assert!(
            scheduler.slots[0].units.contains_key(&unit),
            "recent compiled dependencies stay warm"
        );
        assert_eq!(scheduler.disk_used(), 20);
    }

    #[test]
    fn pressure_measures_a_completed_build_without_waiting_eight_builds() {
        let mut scheduler = scheduler(1);
        scheduler.disk_budget = 50;
        scheduler.build_headroom = 20;
        let [(job, ..)] = starts(&submit(&mut scheduler, 1, "/a", "a1"))[..] else {
            panic!("start");
        };
        scheduler.slots[0].size = Some(40);
        assert!(starts(&submit(&mut scheduler, 2, "/a", "a2")).is_empty());
        let finished = scheduler.exited(job, &ok(), None, BuildReport::default());
        assert!(starts(&finished).is_empty());
        assert_eq!(measures(&finished), [0]);
        assert_eq!(scheduler.slots[0].unmeasured, 1);
        assert_eq!(scheduler.disk_wait(), Some(&DiskWait::Measuring));
    }

    #[test]
    fn disk_pressure_waits_for_its_inventory_before_starting_another_repository() {
        let mut scheduler = scheduler(2);
        scheduler.disk_budget = 50;
        scheduler.build_headroom = 20;
        let [(job, ..)] = starts(&submit(&mut scheduler, 1, "/a", "a1"))[..] else {
            panic!("start");
        };
        scheduler.slots[0].size = Some(40);
        let finished = scheduler.exited(job, &ok(), None, BuildReport::default());
        assert_eq!(measures(&finished), [0]);
        let mut other = source("/other");
        other.repository = "/other/.git".into();
        let waiting = scheduler.submit(Submission {
            waiter: WaiterId(2),
            source: other,
            revision: revision("b1"),
            operation: check(),
            label: None,
            copy_to: None,
            rerun_all: false,
            exclusive: false,
            optional: false,
        });
        assert!(starts(&waiting).is_empty());
        assert_eq!(scheduler.disk_wait(), Some(&DiskWait::Measuring));
        let measured = scheduler.measured(
            SlotKey(0),
            Ok(Inventory {
                total: 40,
                ..Inventory::default()
            }),
        );
        assert_eq!(starts(&measured).len(), 1);
    }

    #[test]
    fn protected_dirty_cache_keeps_its_disk_reservation_until_it_is_released_and_measured() {
        let mut scheduler = scheduler(2);
        let [(job, ..)] = starts(&submit(&mut scheduler, 1, "/a", "a1"))[..] else {
            panic!("start");
        };
        scheduler.exited(job, &ok(), None, BuildReport::default());
        let name = scheduler.status(|_| 0).0[0].name.clone();
        let held = scheduler.cache_acquire(&name).unwrap();
        scheduler.slots[0].size = Some(40);
        scheduler.disk_budget = 50;
        scheduler.build_headroom = 20;
        assert!(starts(&submit(&mut scheduler, 2, "/b", "b1")).is_empty());
        assert_eq!(scheduler.disk_wait(), Some(&DiskWait::Protected));
        let released = scheduler.cache_release(held, false);
        assert!(starts(&released).is_empty());
        assert_eq!(measures(&released), [0]);
        let measured = scheduler.measured(
            SlotKey(0),
            Ok(Inventory {
                total: 40,
                ..Inventory::default()
            }),
        );
        assert_eq!(starts(&measured).len(), 1);
    }

    #[test]
    fn output_headroom_is_reserved_per_running_build_and_rechecked_for_queued_work() {
        let mut scheduler = scheduler(2);
        known_check_memory(&mut scheduler, "/repo/.git");
        scheduler.min_free = 5;
        scheduler.build_headroom = 10;
        scheduler.observe_disk(Some(24));
        let [(first, ..)] = starts(&submit(&mut scheduler, 1, "/a", "a1"))[..] else {
            panic!("one build fits");
        };
        assert!(starts(&submit(&mut scheduler, 2, "/b", "b1")).is_empty());
        assert_eq!(scheduler.disk_wait().and_then(required), Some(25));
        scheduler.observe_disk(Some(25));
        let second = scheduler.maintain();
        assert_eq!(
            starts(&second).len(),
            1,
            "space recovery admits the accepted queue"
        );
        assert!(starts(&submit(&mut scheduler, 3, "/c", "c1")).is_empty());
        scheduler.observe_disk(Some(14));
        let finished = scheduler.exited(first, &ok(), None, BuildReport::default());
        assert!(starts(&finished).is_empty());
        assert_eq!(measures(&finished), [0]);
        let measured = scheduler.measured(SlotKey(0), Ok(Inventory::default()));
        assert!(
            starts(&measured).is_empty(),
            "accepted work still needs disk admission"
        );
        assert_eq!(scheduler.disk_wait().and_then(required), Some(15));
        assert!(
            !scheduler.status(|_| 0).0[1]
                .build
                .as_ref()
                .unwrap()
                .cancelled
        );
        scheduler.observe_disk(Some(25));
        assert_eq!(starts(&scheduler.maintain()).len(), 1);
    }

    /// Measures idle slot `key`, now holding `total` bytes.
    fn measure_total(scheduler: &mut Scheduler<Table>, key: SlotKey, total: u64) -> Vec<Effect> {
        let due = scheduler.measurement_due(Instant::now() + MEASURE_QUIET);
        assert!(
            measures(&due).contains(&scheduler.slots[key.0].index),
            "{due:?}"
        );
        scheduler.measured(
            key,
            Ok(Inventory {
                total,
                ..Inventory::default()
            }),
        )
    }

    #[test]
    fn a_build_reserves_what_its_kind_of_build_added_to_its_slot_or_the_default() {
        let mut scheduler = scheduler(1);
        scheduler.min_free = 5;
        scheduler.build_headroom = 10;
        scheduler.observe_disk(Some(14));
        // Without history, the default headroom.
        assert!(starts(&submit(&mut scheduler, 1, "/a", "a1")).is_empty());
        let unknown = Growth {
            bytes: 10,
            measured: false,
        };
        assert_eq!(
            scheduler.disk_wait(),
            Some(&DiskWait::Headroom(Headroom {
                free: 14,
                floor: 5,
                running: 0,
                build: Some(unknown),
            }))
        );
        scheduler.observe_disk(Some(100));
        let [(first, ..)] = starts(&scheduler.maintain())[..] else {
            panic!("space admits it");
        };
        scheduler.exited(first, &ok(), None, BuildReport::default());
        // The slot was new: there is nothing to compare its size with.
        measure_total(&mut scheduler, SlotKey(0), 3);
        assert!(scheduler.slots[0].compilations.is_empty());
        // Measured before and after, a build tells what it added.
        let [(second, ..)] = starts(&submit(&mut scheduler, 2, "/a", "a2"))[..] else {
            panic!("space admits it");
        };
        scheduler.exited(second, &ok(), None, BuildReport::default());
        let measured = measure_total(&mut scheduler, SlotKey(0), 4);
        let added = DiskUse { added: 1, total: 4 };
        assert!(measured.iter().any(|effect| matches!(
            effect,
            Effect::Persist { record, .. } if record.compilations[0].disk == Some(added)
        )));
        // A warm slot reserves what its incremental build added.
        scheduler.observe_disk(Some(6));
        let [(third, ..)] = starts(&submit(&mut scheduler, 3, "/a", "a3"))[..] else {
            panic!("1 byte of growth fits");
        };
        scheduler.exited(third, &ok(), None, BuildReport::default());
        measure_total(&mut scheduler, SlotKey(0), 4);
        assert_eq!(
            scheduler.slots[0].compilations[0].disk,
            Some(DiskUse { added: 0, total: 4 })
        );
        // Another kind of build has no history in the slot.
        assert!(
            starts(&submit_operation(
                &mut scheduler,
                4,
                "/a",
                "a4",
                Command::Test,
                &[]
            ))
            .is_empty()
        );
        assert!(matches!(
            scheduler.disk_wait(),
            Some(DiskWait::Headroom(Headroom { build: Some(growth), .. })) if *growth == unknown
        ));
        scheduler.withdraw(WaiterId(4));
        // A slot below the size its build left grows back to it.
        scheduler.slots[0].size = Some(1);
        assert!(starts(&submit(&mut scheduler, 5, "/a", "a5")).is_empty());
        assert_eq!(
            scheduler.disk_wait(),
            Some(&DiskWait::Headroom(Headroom {
                free: 6,
                floor: 5,
                running: 0,
                build: Some(Growth {
                    bytes: 3,
                    measured: true
                }),
            }))
        );
        scheduler.observe_disk(Some(8));
        assert_eq!(starts(&scheduler.maintain()).len(), 1);
        assert_eq!(scheduler.disk_reserved(), 3);
    }

    #[test]
    fn admission_does_not_evict_the_slot_the_build_takes() {
        let mut scheduler = scheduler(2);
        scheduler.min_free = 5;
        scheduler.build_headroom = 10;
        scheduler.observe_disk(Some(14));
        for (key, repository, prefix) in [(0, "/repo/.git", "a"), (1, "/other/.git", "o")] {
            scheduler.restore(
                repository.into(),
                0,
                Some(revision("old")),
                SlotRecord::default(),
            );
            scheduler.measure(key);
            scheduler.measured(SlotKey(key), Ok(inventory(&units(prefix, 2), 4)));
        }
        let effects = submit(&mut scheduler, 1, "/a", "a1");
        assert!(starts(&effects).is_empty());
        let eviction = effects
            .iter()
            .find(|effect| matches!(effect, Effect::Evict(_)))
            .expect("the shortfall is evicted");
        assert_eq!(
            removed(eviction)
                .iter()
                .map(|(slot, _)| *slot)
                .collect::<Vec<_>>(),
            [1],
            "only the other repository's slot gives up artifacts"
        );
    }

    #[test]
    fn a_queued_build_hears_why_it_waits_and_again_when_the_reason_changes() {
        const GIB: u64 = 1 << 30;
        let mut scheduler = scheduler(1);
        scheduler.min_free = 15 * GIB;
        scheduler.build_headroom = 10 * GIB;
        scheduler.observe_disk(Some(100 * GIB));
        assert_eq!(starts(&submit(&mut scheduler, 1, "/a", "a1")).len(), 1);
        let queued = submit(&mut scheduler, 2, "/b", "b1");
        assert_eq!(
            sent(&queued, 2),
            [
                &Message::Queued {
                    revision: revision("b1")
                },
                &Message::Waiting { wait: Wait::Slot }
            ]
        );
        // The volume fills: it no longer covers the floor and the running
        // build's reservation.
        scheduler.observe_disk(Some(25 * GIB - 400 * (1 << 20)));
        let filled = scheduler.maintain();
        let [Message::Waiting { wait }] = sent(&filled, 2)[..] else {
            panic!("{filled:?}");
        };
        assert_eq!(
            wait.to_string(),
            "waiting for disk: 24.6 GiB free, needs 25.0 GiB (floor 15.0 GiB + 10.0 GiB \
             reserved by running builds); free 0.4 GiB more on this volume"
        );
        // New numbers for the same reason are no news; status has them.
        scheduler.observe_disk(Some(24 * GIB));
        assert!(sent(&scheduler.maintain(), 2).is_empty());
        assert!(matches!(
            scheduler.status(|_| 0).1[0].wait,
            Some(Wait::Disk {
                disk: DiskWait::Headroom(Headroom { free, .. })
            }) if free == 24 * GIB
        ));
        // A client joining the queued build hears the reason at once.
        let joined = submit(&mut scheduler, 3, "/b", "b1");
        assert!(matches!(
            sent(&joined, 3)[..],
            [
                Message::Queued { .. },
                Message::Waiting {
                    wait: Wait::Disk { .. }
                }
            ]
        ));
    }

    #[test]
    fn a_build_waiting_for_disk_nothing_will_free_is_refused_after_the_bound() {
        let mut scheduler = scheduler(2);
        scheduler.unmeasured_memory = Some(1);
        scheduler.min_free = 5;
        scheduler.build_headroom = 10;
        scheduler.disk_wait_limit = Duration::from_secs(3600);
        scheduler.observe_disk(Some(20));
        let [(running, ..)] = starts(&submit(&mut scheduler, 1, "/a", "a1"))[..] else {
            panic!("the first build fits");
        };
        assert!(
            starts(&submit_operation(
                &mut scheduler,
                2,
                "/b",
                "b1",
                Command::Test,
                &[]
            ))
            .is_empty()
        );
        // The running build's end releases its reservation: no bound applies.
        scheduler.disk_wait_limit = Duration::ZERO;
        assert!(sent(&scheduler.maintain(), 2).is_empty());
        scheduler.disk_wait_limit = Duration::from_secs(3600);
        scheduler.observe_disk(Some(14));
        let exited = scheduler.exited(running, &ok(), None, BuildReport::default());
        assert_eq!(measures(&exited), [0], "pressure measures the idle slot");
        let measured = scheduler.measured(SlotKey(0), Ok(Inventory::default()));
        assert!(starts(&measured).is_empty());
        assert!(
            matches!(
                sent(&measured, 2)[..],
                [Message::Waiting {
                    wait: Wait::Disk {
                        disk: DiskWait::Headroom(_)
                    }
                }]
            ),
            "{measured:?}"
        );
        // Nothing runs and nothing idle is left to evict: only someone
        // freeing space helps. Within the bound the build waits.
        assert!(sent(&scheduler.maintain(), 2).is_empty());
        scheduler.disk_wait_limit = Duration::ZERO;
        let refused = scheduler.maintain();
        let [Message::Rejected { reason }] = sent(&refused, 2)[..] else {
            panic!("{refused:?}");
        };
        assert!(
            reason.starts_with(
                "refused: for 0 s no build ran on this host and idle build artifacts could not \
                 cover the disk this build needs: 0.0 GiB free, needs 0.0 GiB"
            ),
            "{reason}"
        );
        assert!(matches!(
            reports(&refused)[..],
            [EventKind::Refused { revision, .. }] if revision.to_string() == "b1"
        ));
        assert!(scheduler.status(|_| 0).1.is_empty());
        assert!(
            scheduler.withdraw(WaiterId(2)).is_empty(),
            "its client was answered"
        );
    }

    #[test]
    fn a_budget_below_unreclaimable_files_cannot_admit_more_work() {
        let mut scheduler = scheduler(1);
        scheduler.disk_budget = 50;
        scheduler.restore(
            "/repo/.git".into(),
            0,
            Some(revision("old")),
            SlotRecord::default(),
        );
        assert!(starts(&submit(&mut scheduler, 1, "/a", "a1")).is_empty());
        let measured = scheduler.measured(
            SlotKey(0),
            Ok(Inventory {
                total: 51,
                ..Inventory::default()
            }),
        );
        assert!(starts(&measured).is_empty());
        assert_eq!(
            scheduler.disk_wait(),
            Some(&DiskWait::Budget {
                used: 51,
                budget: 50
            })
        );
    }

    #[test]
    fn unknown_free_space_and_unknown_restored_targets_cannot_admit_builds() {
        let mut scheduler = scheduler(1);
        scheduler.observe_disk(None);
        assert!(starts(&submit(&mut scheduler, 1, "/a", "a1")).is_empty());
        assert_eq!(scheduler.disk_wait(), Some(&DiskWait::Unavailable));
        scheduler.restore(
            "/repo/.git".into(),
            0,
            Some(revision("old")),
            SlotRecord::default(),
        );
        scheduler.observe_disk(Some(u64::MAX));
        let measured = scheduler.maintain();
        assert!(starts(&measured).is_empty());
        assert_eq!(measures(&measured), [0]);
        assert_eq!(
            starts(&scheduler.measured(SlotKey(0), Ok(Inventory::default()))).len(),
            1
        );
    }

    #[test]
    fn a_failed_inventory_holds_admission_and_recovers_on_a_quiet_period_scan() {
        let mut scheduler = scheduler(1);
        let [(job, ..)] = starts(&submit(&mut scheduler, 1, "/a", "a1"))[..] else {
            panic!("start");
        };
        scheduler.exited(job, &ok(), None, BuildReport::default());
        scheduler.measurement_due(Instant::now() + MEASURE_QUIET);
        scheduler.measured(SlotKey(0), Err("permission denied".into()));
        assert!(starts(&submit(&mut scheduler, 2, "/a", "a2")).is_empty());
        assert!(
            matches!(scheduler.disk_wait(), Some(DiskWait::MaintenanceFailed { reason, .. }) if reason == "permission denied")
        );
        assert!(
            scheduler.maintain().is_empty(),
            "do not spin on a filesystem failure"
        );
        assert_eq!(
            measures(&scheduler.measurement_due(Instant::now() + MEASURE_QUIET)),
            [0]
        );
        let recovered = scheduler.measured(SlotKey(0), Ok(Inventory::default()));
        assert_eq!(starts(&recovered).len(), 1);
        assert_eq!(scheduler.disk_wait(), None);
    }

    #[test]
    fn breaching_the_disk_floor_stops_running_groups_before_their_artifacts_are_reclaimed() {
        let mut scheduler = scheduler(2);
        known_check_memory(&mut scheduler, "/repo/.git");
        scheduler.min_free = 5;
        scheduler.build_headroom = 10;
        scheduler.observe_disk(Some(25));
        let [(first, ..)] = starts(&submit(&mut scheduler, 1, "/a", "a1"))[..] else {
            panic!("start");
        };
        assert_eq!(starts(&submit(&mut scheduler, 2, "/b", "b1")).len(), 1);
        assert!(starts(&submit(&mut scheduler, 3, "/c", "c1")).is_empty());
        scheduler.observe_disk(Some(4));
        let stopped = scheduler.maintain();
        assert_eq!(stopped.iter().filter(|effect| matches!(effect, Effect::Cancel { reason: Stop::Failed(reason), .. } if reason.contains("free disk floor"))).count(), 2);
        assert!(
            !stopped
                .iter()
                .any(|effect| matches!(effect, Effect::Evict(_) | Effect::Start(_)))
        );
        assert!(scheduler.maintain().is_empty(), "stop each group once");
        assert_eq!(scheduler.status(|_| 0).1.len(), 1);
        let failed = Outcome::Failed {
            reason: "disk floor".into(),
        };
        let exited = scheduler.exited(first, &failed, None, BuildReport::default());
        assert_eq!(measures(&exited), [0]);
        let measured = scheduler.measured(SlotKey(0), Ok(inventory(&units("a", 1), 8)));
        let eviction = measured
            .iter()
            .find(|effect| matches!(effect, Effect::Evict(_)))
            .unwrap();
        assert_eq!(
            removed(eviction)
                .iter()
                .map(|(slot, _)| *slot)
                .collect::<Vec<_>>(),
            [0]
        );
        assert!(
            scheduler.slots[1].job.is_some(),
            "the other group has not exited yet"
        );
    }

    #[test]
    fn an_idle_slot_is_measured_after_a_quiet_period_and_waited_for_meanwhile() {
        let mut scheduler = scheduler(2);
        known_check_memory(&mut scheduler, "/repo/.git");
        known_check_memory(&mut scheduler, "/other/.git");
        run(&mut scheduler, 0, "/a", "baseline");
        measure_all(&mut scheduler, 1);
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
            exclusive: false,
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
        run(&mut scheduler, 0, "/a", "baseline");
        measure_all(&mut scheduler, 1);
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
        for (index, worktree, tree, command, stem) in [
            (0, "/a", "a1", Command::Check, "c"),
            (1, "/b", "b1", Command::Test, "t"),
        ] {
            let held = units(stem, 12);
            let at = now();
            scheduler.restore(
                "/repo/.git".into(),
                index,
                Some(revision(tree)),
                SlotRecord {
                    worktree: Some(worktree.into()),
                    compilations: vec![CompilationRun {
                        compilation: Compilation::new(
                            Path::new(""),
                            &Operation {
                                command,
                                args: Vec::new(),
                                rustflags: Vec::new(),
                            },
                        ),
                        at,
                        units: held.iter().cloned().collect(),
                        build_ms: Some(0),
                        peak_memory: Some(1),
                        test_memory: None,
                        disk: None,
                    }],
                    units: held
                        .into_iter()
                        .enumerate()
                        .map(|(place, unit)| {
                            (
                                unit,
                                LastUse {
                                    at,
                                    after: 11 - u32::try_from(place).unwrap(),
                                },
                            )
                        })
                        .collect(),
                    passed: Vec::new(),
                },
            );
            scheduler.slots[index].used = u64::try_from(index + 1).unwrap();
        }
        scheduler.clock = 2;
        scheduler
    }

    mod planning {
        use super::*;
        use crate::protocol::Status;

        fn forecast(
            scheduler: &mut Scheduler<Table>,
            tree: &str,
            operation: Operation,
        ) -> Forecast {
            let submission = Submission {
                waiter: WaiterId(900),
                source: source("/planning"),
                revision: revision(tree),
                operation,
                label: None,
                copy_to: None,
                rerun_all: false,
                exclusive: false,
                optional: false,
            };
            let (slots, queue) = scheduler.status(|_| 1);
            let capacity = Capacity {
                disk_growth: scheduler.forecast_growth(
                    &submission.source.repository,
                    &submission.source.prefix,
                    &submission.operation,
                ),
                peak_memory: scheduler.forecast_memory(
                    &submission.source.repository,
                    &submission.source.prefix,
                    &submission.operation,
                ),
                status: Status {
                    jobs: 4,
                    idle_jobs: 4,
                    capacity: scheduler.capacity,
                    disk_budget: scheduler.disk_budget,
                    disk_used: scheduler.disk_used(),
                    free_disk: scheduler.free_disk,
                    min_free: scheduler.min_free,
                    build_headroom: scheduler.build_headroom,
                    disk_reserved: scheduler.disk_reserved(),
                    disk_wait: scheduler.disk_wait.clone(),
                    undersized: scheduler.undersized,
                    memory: scheduler.memory,
                    memory_in_use: scheduler.memory_in_use(),
                    unmeasured_memory: scheduler.unmeasured_memory,
                    slots,
                    queue,
                    draining: false,
                },
            };
            scheduler.forecast(&submission, capacity)
        }

        #[test]
        fn a_cold_plan_creates_no_slots_jobs_or_history() {
            let mut scheduler = scheduler(2);
            let plan = forecast(&mut scheduler, "new", check());
            let repeated = forecast(&mut scheduler, "new", check());
            assert_eq!(plan, repeated);
            assert!(scheduler.slots.is_empty());
            assert!(scheduler.jobs.is_empty());
            assert!(scheduler.waiting.is_empty());
            assert_eq!(scheduler.next_job, 0);
            assert_eq!(scheduler.clock, 0);
            assert_eq!(plan.queue_ms, Some(0));
            assert!(plan.historical_build_ms.is_none());
            assert_eq!(plan.slots.len(), 1);
            assert!(plan.slots[0].new);
            assert!(plan.slots[0].compile_ms.is_none());
            assert!(!plan.slots[0].exact_history);
        }

        #[test]
        fn the_plan_and_submission_use_the_same_cache_order() {
            let mut scheduler = checked_and_tested();
            measure_all(&mut scheduler, 1);
            let plan = forecast(
                &mut scheduler,
                "b1",
                Operation {
                    command: Command::Test,
                    ..check()
                },
            );
            assert_eq!(plan.slots[0].reused_units, 12);
            assert_eq!(plan.slots[0].missing_units, 0);
            assert_eq!(plan.slots[0].source_work, Some(0));
            assert!(plan.slots[0].exact_history);
            assert_eq!(plan.recommended_slot.as_ref(), Some(&plan.slots[0].slot));
            let started =
                submit_operation(&mut scheduler, 3, "/planning", "b1", Command::Test, &[]);
            assert!(matches!(starts(&started)[..], [(_, 1, _)]));
        }

        #[test]
        fn planning_a_warm_wait_does_not_claim_the_slot() {
            let mut scheduler = checked_and_tested();
            measure_all(&mut scheduler, 1);
            scheduler.slots[1].compilations[0].build_ms = Some(5000);
            submit_operation(&mut scheduler, 3, "/b", "b2", Command::Test, &[]);
            let cost = scheduler
                .compile_cost
                .entry("/repo/.git".into())
                .or_default();
            cost.add(10, 10_000);
            cost.add(20, 20_000);
            let jobs = scheduler.jobs.len();
            let next = scheduler.next_job;
            let plan = forecast(
                &mut scheduler,
                "c1",
                Operation {
                    command: Command::Test,
                    ..check()
                },
            );
            assert_eq!(scheduler.jobs.len(), jobs);
            assert_eq!(scheduler.next_job, next);
            assert!(scheduler.jobs.values().all(|job| job.held().is_none()));
            assert_eq!(
                plan.recommended_slot,
                Some(slot_name(Path::new("/repo/.git"), 1))
            );
            assert!(
                plan.notes
                    .iter()
                    .any(|note| note.contains("waiting for warm slot"))
            );
            assert_eq!(plan.slots[0].compile_ms, Some(22_000));
            let effects =
                submit_operation(&mut scheduler, 4, "/planning", "c1", Command::Test, &[]);
            assert!(starts(&effects).is_empty());
            let (_, queue) = scheduler.status(|_| 0);
            assert_eq!(
                held_for(&queue[0]).map(|hold| &hold.slot),
                plan.recommended_slot.as_ref()
            );
        }

        #[test]
        fn an_unavailable_tree_comparison_does_not_invent_a_cost() {
            let mut scheduler = checked_and_tested();
            measure_all(&mut scheduler, 1);
            scheduler
                .distance
                .0
                .insert(("a1".into(), "unknown".into()), u64::MAX);
            let cost = scheduler
                .compile_cost
                .entry("/repo/.git".into())
                .or_default();
            cost.add(10, 100);
            cost.add(20, 200);
            let plan = forecast(&mut scheduler, "unknown", check());
            let slot = plan
                .slots
                .iter()
                .find(|slot| slot.slot == slot_name(Path::new("/repo/.git"), 0))
                .unwrap();
            assert!(slot.source_work.is_none());
            assert!(slot.compile_ms.is_none());
        }

        #[test]
        fn shared_work_needs_no_more_memory_but_unknown_new_work_waits() {
            let mut scheduler = scheduler(2);
            submit(&mut scheduler, 1, "/a", "a1");
            let shared = forecast(&mut scheduler, "a1", check());
            assert!(shared.shared_slot.is_some());
            assert_eq!(shared.queue_ms, Some(0));
            assert!(
                !shared
                    .notes
                    .iter()
                    .any(|note| note.contains("waiting for memory"))
            );
            let different = forecast(
                &mut scheduler,
                "a2",
                Operation {
                    args: vec!["-p".into(), "unmeasured".into()],
                    ..check()
                },
            );
            assert_eq!(different.capacity.peak_memory, None);
            assert_eq!(different.queue_ms, None);
            assert!(
                different
                    .notes
                    .iter()
                    .any(|note| note.contains("waiting for memory"))
            );
            assert_eq!(scheduler.slots.len(), 1);
            assert_eq!(scheduler.jobs.len(), 1);
        }
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
        measure_all(&mut scheduler, 1);
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
        measure_all(&mut scheduler, 1);
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
        assert!(held_for(&queue[0]).is_some());
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
            queue.iter().all(|build| held_for(build).is_none()),
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
        measure_all(&mut scheduler, 1);
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
        measure_all(&mut scheduler, 1);
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
    fn an_unknown_package_selection_does_not_inherit_a_workspace_peak() {
        let mut scheduler = scheduler(2);
        scheduler.memory = 10;
        let broad = submit_operation(
            &mut scheduler,
            1,
            "/a",
            "a1",
            Command::Test,
            &["--workspace"],
        );
        let [(job, ..)] = starts(&broad)[..] else {
            panic!("{broad:?}")
        };
        used(&mut scheduler, job, &units("b", 1));
        scheduler.exited(
            job,
            &ok(),
            Some(Usage {
                cpu_ms: 1,
                peak_memory: 20,
                test_memory: None,
            }),
            BuildReport::default(),
        );
        measure_all(&mut scheduler, 1);
        let operation = Operation {
            command: Command::Nextest,
            args: vec!["-p".into(), "small".into()],
            rustflags: Vec::new(),
        };
        assert_eq!(
            scheduler.forecast_memory(Path::new("/repo/.git"), Path::new(""), &operation),
            None
        );
        let scoped = submit_operation(
            &mut scheduler,
            2,
            "/b",
            "b1",
            Command::Nextest,
            &["-p", "small"],
        );
        assert_eq!(starts(&scoped).len(), 1, "{scoped:?}");
        assert_eq!(
            scheduler.memory(),
            (10, 10),
            "unknown reserves the whole budget"
        );
    }

    #[test]
    fn configured_unknown_reservations_share_capacity_and_yield_to_measured_peaks() {
        let mut scheduler = scheduler(4);
        scheduler.memory = 10;
        scheduler.unmeasured_memory = Some(5);
        let first = submit(&mut scheduler, 1, "/a", "a1");
        let [(a, ..)] = starts(&first)[..] else {
            panic!("{first:?}")
        };
        let second = submit_operation(&mut scheduler, 2, "/b", "b1", Command::Test, &[]);
        let [(b, ..)] = starts(&second)[..] else {
            panic!("{second:?}")
        };
        assert_eq!(scheduler.memory(), (10, 10));
        let third = submit_operation(&mut scheduler, 3, "/c", "c1", Command::Build, &[]);
        assert!(
            starts(&third).is_empty(),
            "two reservations fill the budget"
        );
        assert_eq!(memory_needed(&scheduler.status(|_| 0).1[0]), Some(5));

        // Learning never publishes the configured reservation as a measured peak.
        assert_eq!(
            scheduler.forecast_memory(Path::new("/repo/.git"), Path::new(""), &check()),
            None
        );
        let observed = Usage {
            cpu_ms: 1,
            peak_memory: 7,
            test_memory: None,
        };
        let finished = scheduler.exited(a, &ok(), Some(observed), BuildReport::default());
        let finished = complete_measurements(&mut scheduler, finished);
        assert_eq!(
            starts(&finished).len(),
            1,
            "released reservation admits the third build"
        );
        assert_eq!(scheduler.memory(), (10, 10));
        let measured = submit(&mut scheduler, 4, "/a", "a2");
        assert!(starts(&measured).is_empty());
        assert_eq!(memory_needed(&scheduler.status(|_| 0).1[0]), Some(7));
        let finished = scheduler.exited(b, &ok(), None, BuildReport::default());
        let finished = complete_measurements(&mut scheduler, finished);
        assert!(
            starts(&finished).is_empty(),
            "7 measured + 5 unknown does not fit"
        );
    }

    #[test]
    fn learning_is_exclusive_even_with_a_measured_zero_peak_and_then_uses_its_measurement() {
        let mut scheduler = scheduler(2);
        scheduler.memory = 10;
        let first = submit(&mut scheduler, 1, "/a", "a1");
        let [(job, ..)] = starts(&first)[..] else {
            panic!("{first:?}")
        };
        used(&mut scheduler, job, &units("c", 1));
        let zero = Usage {
            cpu_ms: 1,
            peak_memory: 0,
            test_memory: None,
        };
        scheduler.exited(job, &ok(), Some(zero), BuildReport::default());
        measure_all(&mut scheduler, 1);
        let known = submit(&mut scheduler, 2, "/a", "a2");
        let [(known, ..)] = starts(&known)[..] else {
            panic!("{known:?}")
        };
        assert!(
            starts(&submit_operation(
                &mut scheduler,
                3,
                "/b",
                "b1",
                Command::Test,
                &[]
            ))
            .is_empty()
        );
        let learning = complete_measurements(&mut scheduler, Vec::new());
        assert!(starts(&learning).is_empty());
        let effects = scheduler.exited(known, &ok(), Some(zero), BuildReport::default());
        let effects = complete_measurements(&mut scheduler, effects);
        let [(learning, ..)] = starts(&effects)[..] else {
            panic!("{effects:?}")
        };
        assert_eq!(scheduler.memory(), (10, 10));
        assert!(
            starts(&submit(&mut scheduler, 4, "/a", "a3")).is_empty(),
            "zero-byte knowledge cannot share an exclusive reservation"
        );
        used(&mut scheduler, learning, &units("t", 1));
        let effects = scheduler.exited(
            learning,
            &ok(),
            Some(Usage {
                cpu_ms: 1,
                peak_memory: 2,
                test_memory: None,
            }),
            BuildReport::default(),
        );
        let effects = complete_measurements(&mut scheduler, effects);
        assert_eq!(starts(&effects).len(), 1, "{effects:?}");
        let measured = submit_operation(&mut scheduler, 5, "/b", "b2", Command::Test, &[]);
        let measured = complete_measurements(&mut scheduler, measured);
        assert_eq!(starts(&measured).len(), 1, "{measured:?}");
        assert_eq!(scheduler.memory(), (10, 2));
    }

    #[test]
    fn resource_only_observations_persist_without_inventing_or_erasing_compile_units() {
        let mut scheduler = scheduler(1);
        let effects = submit_operation(&mut scheduler, 1, "/a", "a1", Command::Run, &["make"]);
        let [(job, ..)] = starts(&effects)[..] else {
            panic!("{effects:?}")
        };
        scheduler.exited(
            job,
            &ok(),
            Some(Usage {
                cpu_ms: 1,
                peak_memory: 3,
                test_memory: None,
            }),
            BuildReport::default(),
        );
        let operation = Operation {
            command: Command::Run,
            args: vec!["make".into()],
            rustflags: Vec::new(),
        };
        assert_eq!(
            scheduler.forecast_memory(Path::new("/repo/.git"), Path::new(""), &operation),
            Some(3)
        );
        assert!(
            !needed(
                &scheduler.slots,
                Path::new("/repo/.git"),
                &Compilation::new(Path::new(""), &operation)
            )
            .1
        );
        let slot = &mut scheduler.slots[0];
        let compilation = Compilation::new(Path::new(""), &check());
        slot.ran(
            compilation.clone(),
            units("c", 2),
            true,
            1,
            Some(Usage {
                cpu_ms: 1,
                peak_memory: 4,
                test_memory: None,
            }),
        );
        slot.ran(
            compilation.clone(),
            Vec::new(),
            true,
            1,
            Some(Usage {
                cpu_ms: 1,
                peak_memory: 5,
                test_memory: None,
            }),
        );
        let record = persisted(&[slot.persist()])[0].1.clone();
        let mut restored = self::scheduler(1);
        restored.restore("/repo/.git".into(), 0, Some(revision("a1")), record);
        assert_eq!(
            restored.forecast_memory(Path::new("/repo/.git"), Path::new(""), &check()),
            Some(5)
        );
        let (held, known) = needed(&restored.slots, Path::new("/repo/.git"), &compilation);
        assert!(known);
        assert_eq!(held, units("c", 2).iter().map(String::as_str).collect());
    }

    #[test]
    fn nextest_stress_runs_use_the_selected_packages_memory_history() {
        let mut scheduler = checked_and_tested();
        measure_all(&mut scheduler, 1);
        scheduler.memory = 14;
        scheduler.slots[0].compilations[0].peak_memory = Some(5);
        let run = &mut scheduler.slots[1].compilations[0];
        run.peak_memory = Some(6);
        run.compilation.args = vec!["-p".into(), "engine-server".into()];
        let mut broad = run.clone();
        broad.compilation.args = vec!["--workspace".into()];
        broad.peak_memory = Some(15);
        scheduler.slots[1].compilations.push(broad);
        assert_eq!(starts(&submit(&mut scheduler, 3, "/a", "a2")).len(), 1);
        let stress = submit_operation(
            &mut scheduler,
            4,
            "/t",
            "t1",
            Command::Nextest,
            &[
                "-p",
                "engine-server",
                "--profile",
                "full",
                "-E",
                "test(x)",
                "--stress-count",
                "20",
            ],
        );
        assert_eq!(
            starts(&stress).len(),
            1,
            "six plus five fits the budget: {stress:?}"
        );
        assert_eq!(scheduler.memory(), (14, 11));
    }

    #[test]
    fn a_build_that_would_not_fit_in_memory_waits_and_so_does_everything_behind_it() {
        let mut scheduler = checked_and_tested();
        measure_all(&mut scheduler, 1);
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
        assert_eq!(memory_needed(&queue[0]), Some(8), "{queue:?}");
        assert_eq!(scheduler.memory(), (10, 4));
        let next = scheduler.exited(job, &ok(), None, BuildReport::default());
        assert_eq!(starts(&next).len(), 1, "the test runs alone: {next:?}");
        let (_, queue) = scheduler.status(|_| 0);
        assert_eq!(memory_needed(&queue[0]), Some(4), "{queue:?}");
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
            queue.iter().all(|build| memory_needed(build).is_none()),
            "{queue:?}"
        );
    }

    #[test]
    fn entering_testing_releases_the_compile_peak_and_starts_waiting_work() {
        let mut scheduler = checked_and_tested();
        measure_all(&mut scheduler, 1);
        scheduler.memory = 10;
        scheduler.slots[0].compilations[0].peak_memory = Some(4);
        let history = &mut scheduler.slots[1].compilations[0];
        history.peak_memory = Some(8);
        history.test_memory = Some(crate::protocol::TestMemory {
            jobs: 2,
            peak_memory: 2,
        });
        let running = submit_operation(&mut scheduler, 3, "/t", "t1", Command::Nextest, &[]);
        let [(job, ..)] = starts(&running)[..] else {
            panic!("{running:?}");
        };
        assert!(starts(&submit(&mut scheduler, 4, "/c", "c1")).is_empty());
        assert_eq!(scheduler.memory(), (10, 8));
        let admitted = scheduler.testing(job);
        assert_eq!(starts(&admitted).len(), 1, "{admitted:?}");
        assert_eq!(scheduler.memory(), (10, 6));
        assert!(scheduler.status(|_| 0).1.is_empty());
        assert!(
            scheduler.testing(job).is_empty(),
            "the transition is idempotent"
        );
    }

    #[test]
    fn unknown_test_peaks_and_other_test_concurrency_keep_the_full_reservation() {
        for previous_jobs in [None, Some(1)] {
            let mut scheduler = checked_and_tested();
            measure_all(&mut scheduler, 1);
            scheduler.memory = 10;
            scheduler.slots[0].compilations[0].peak_memory = Some(4);
            let history = &mut scheduler.slots[1].compilations[0];
            history.peak_memory = Some(8);
            history.test_memory = previous_jobs.map(|jobs| crate::protocol::TestMemory {
                jobs,
                peak_memory: 2,
            });
            let running = submit_operation(&mut scheduler, 3, "/t", "t1", Command::Test, &[]);
            let [(job, ..)] = starts(&running)[..] else {
                panic!("{running:?}");
            };
            assert!(starts(&submit(&mut scheduler, 4, "/c", "c1")).is_empty());
            assert!(scheduler.testing(job).is_empty());
            assert_eq!(scheduler.memory(), (10, 8));
            assert_eq!(scheduler.status(|_| 0).1.len(), 1);
        }
    }

    #[test]
    fn filtered_runs_cannot_erase_a_broader_runs_memory_peaks() {
        let mut scheduler = checked_and_tested();
        let slot = &mut scheduler.slots[1];
        let compilation = slot.compilations[0].compilation.clone();
        let history = &mut slot.compilations[0];
        history.peak_memory = Some(8);
        history.test_memory = Some(crate::protocol::TestMemory {
            jobs: 2,
            peak_memory: 4,
        });
        slot.ran(
            compilation.clone(),
            units("t", 12),
            true,
            1,
            Some(Usage {
                cpu_ms: 1,
                peak_memory: 3,
                test_memory: Some(crate::protocol::TestMemory {
                    jobs: 2,
                    peak_memory: 1,
                }),
            }),
        );
        assert_eq!(slot.compilations[0].peak_memory, Some(8));
        assert_eq!(slot.compilations[0].test_memory.unwrap().peak_memory, 4);
        let record = persisted(&[slot.persist()])[0].1.clone();
        let mut restored = self::scheduler(2);
        restored.memory = 10;
        restored.restore("/repo/.git".into(), 0, Some(revision("r1")), record);
        assert_eq!(
            restored.forecast_memory(
                Path::new("/repo/.git"),
                Path::new(""),
                &Operation {
                    command: Command::Nextest,
                    args: Vec::new(),
                    rustflags: Vec::new(),
                }
            ),
            Some(8)
        );
        assert_eq!(
            restored.slots[0].compilations[0]
                .test_memory
                .unwrap()
                .peak_memory,
            4
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
        let eviction = scheduler.evict(over_budget(120), None).unwrap();
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
        let eviction = scheduler.evict(over_budget(15), None).unwrap();
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
            scheduler.evict(over_budget(1), None),
            None,
            "one eviction at a time"
        );
        assert!(scheduler.evicting.is_some());

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
            let eviction = scheduler.evict(shortage, None).unwrap();
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
        let eviction = scheduler.evict(over_budget(20), None).unwrap();
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
        let eviction = scheduler.evict(over_budget(1_000), None).unwrap();
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
        assert_eq!(scheduler.evict(over_budget(1), None), None);
        assert!(scheduler.evicting.is_none());
    }

    #[test]
    fn a_slot_that_failed_to_evict_is_measured_again() {
        let mut scheduler = checked_and_tested();
        measure_all(&mut scheduler, 10);
        scheduler.evict(over_budget(10), None).unwrap();
        let failed_at = Instant::now();
        let done = scheduler.evicted(vec![(SlotKey(0), Err("busy device".into()))]);
        assert!(matches!(
            reports(&done)[..],
            [EventKind::Evicted { freed: 0, .. }]
        ));
        assert!(
            matches!(scheduler.disk_wait(), Some(DiskWait::MaintenanceFailed { reason, .. }) if reason == "busy device")
        );
        assert!(scheduler.maintain().is_empty());
        assert!(scheduler.next_measurement().unwrap() >= failed_at + MEASURE_QUIET);
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
            measure_all(&mut scheduler, 1);
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
            } else {
                measure_all(&mut scheduler, 1);
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
        let mut restarted = self::scheduler(1);
        restarted.restore("/repo/.git".into(), 0, Some(revision("a1")), record);
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
                test_memory: None,
                disk: None,
            },
        );
        let mut restored = self::scheduler(1);
        restored.restore("/repo/.git".into(), 0, Some(revision("a1")), record);
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
        let next = complete_measurements(&mut scheduler, next);
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
            exclusive: false,
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
            test_memory: None,
        };
        let finished = scheduler.exited(job, &ok(), Some(usage), BuildReport::default());
        let finished = complete_measurements(&mut scheduler, finished);
        assert!(matches!(
            reports(&finished)[..],
            [
                EventKind::Finished { compiled: 1, fresh: 2, usage: Some(usage), who, .. },
                // The next build starts in the slot that just did `check`.
                EventKind::Started { warm: true, first: true, .. },
            ] if who.len() == 2 && usage.cpu_ms == 1200
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
        scheduler.testing(job);
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

        measure_all(&mut scheduler, 1);

        // Other commands finish compiling at their end: no test time.
        let effects = submit(&mut scheduler, 2, "/a", "a2");
        let [(job, ..)] = starts(&effects)[..] else {
            panic!("{effects:?}");
        };
        scheduler.testing(job);
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
        scheduler.evict(over_budget(3), None).unwrap();
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
                peak_memory: Some(1),
                test_memory: None,
                disk: None,
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
            Some(revision("x1")),
            SlotRecord::default(),
        );
        // A larger earlier configuration left a cache above today's limit.
        scheduler.restore("/repo/.git".into(), 7, Some(revision("y1")), record);
        let (slots, _) = scheduler.status(|_| 0);
        assert_eq!(slots.len(), 2);
        assert!(slots.iter().all(|slot| slot.size.is_none()));
        measure_all(&mut scheduler, 1);
        // Slot 7 holds what `check` needs, so it wins over slot 0, which is
        // closer.
        scheduler.distance.0.insert(("x1".into(), "a2".into()), 1);
        let first = submit(&mut scheduler, 1, "/work/a", "a2");
        assert!(matches!(starts(&first)[..], [(_, 7, _)]));
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
        let fourth = submit(&mut scheduler, 4, "/work/d", "d1");
        assert!(
            starts(&fourth).is_empty(),
            "the concurrency limit still applies"
        );
        assert_eq!(scheduler.status(|_| 0).1.len(), 1);
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
        let eviction = scheduler.evict(over_budget(1), None).unwrap();
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
