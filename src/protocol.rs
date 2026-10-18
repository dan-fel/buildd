//! The daemon's wire protocol: one JSON object per line over a Unix socket.
//!
//! A client sends one [`Request`]. For [`Request::Build`] the daemon answers
//! with [`Message`]s ending in [`Message::Finished`] or
//! [`Message::Rejected`]. Closing the connection earlier withdraws the
//! request; a build nobody waits for any more is cancelled. For
//! [`Request::Status`] it answers with one [`Status`], for
//! [`Request::Activity`] with one [`Activity`], for [`Request::Remotes`]
//! with a list of [`RemoteActivity`].

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::cargo::Operation;
use crate::snapshot::Revision;

/// What a client asks the daemon.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Request {
    /// Cache-owner operations. `host` is a configured remote name, not a path.
    Cache {
        host: Option<String>,
        operation: crate::cache::Operation,
    },
    /// Build the current content of the worktree holding `directory`.
    // A new sender must be rejected by an older daemon that cannot honor
    // host constraints. Existing clients without that field still submit.
    #[serde(rename = "schedule", alias = "build")]
    Build(BuildRequest),
    /// Build a tree another machine's daemon pushed into this daemon's
    /// mirror of a project; answered like [`Request::Build`].
    BuildRevision(RevisionRequest),
    /// The bare repository this daemon keeps for `project`, created when
    /// absent: where another daemon pushes trees. Answered with one
    /// [`Mirror`].
    Mirror { project: String },
    /// Stop taking builds (`drain`), letting running and queued ones end,
    /// or take them again. Answered with one [`Status`].
    Drain { drain: bool },
    /// Describe the slots and the queue.
    Status,
    /// Describe the slots and the queue, what the daemon did since it
    /// started, and its recent events.
    Activity,
    /// What each remote host's daemon does, asked in parallel: as slow as
    /// the slowest host, so apart from [`Request::Activity`].
    Remotes,
    /// Configured hosts and the coordinator's persisted admission policy.
    Hosts,
    /// Change policy for new placements; accepted work keeps running.
    SetHost { name: String, enabled: bool },
    /// Read the local scheduler's resource estimate. Never forwards work.
    Capacity {
        project: String,
        prefix: PathBuf,
        operation: Operation,
    },
    /// Sum up the builds that failed in the last `hours`, from the event
    /// file: on the remote host for `os` when that is not this machine's.
    /// Answered with one [`Failures`].
    Failures {
        hours: u64,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        os: Option<String>,
    },
}

/// The builds that failed since `since_ms`, summed up to find what keeps
/// failing: tests, errors, and code whose outcome changed between runs.
#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
pub struct Failures {
    pub since_ms: u64,
    /// Where the daemon keeps build logs.
    pub logs: PathBuf,
    pub builds: u64,
    pub failed: u64,
    /// The latest failed builds, newest first.
    pub recent: Vec<FailedBuild>,
    /// Tests by how many builds they failed in, most first.
    pub tests: Vec<FailingTest>,
    /// Errors by how many builds reported them, most first; without the
    /// file and line they point at.
    pub errors: Vec<CommonError>,
    /// The same tree and command, built more than once, passing one time and
    /// failing another: flaky tests, or trouble outside the code.
    pub mixed: Vec<MixedOutcome>,
    /// The slowest tests, by their latest time, slowest first.
    pub slowest: Vec<TestTime>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct FailedBuild {
    pub at_ms: u64,
    pub who: Vec<String>,
    pub operation: Operation,
    pub revision: Revision,
    pub outcome: Outcome,
    pub report: BuildReport,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct FailingTest {
    pub test: String,
    /// Builds it failed in.
    pub failures: u64,
    /// Distinct trees among them.
    pub trees: u64,
    pub who: Vec<String>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct CommonError {
    pub error: String,
    pub builds: u64,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct MixedOutcome {
    pub revision: Revision,
    pub operation: Operation,
    pub passed: u64,
    pub failed: u64,
}

/// A build of a worktree's current content.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct BuildRequest {
    /// A directory inside a git worktree. Cargo runs in the same directory
    /// of the snapshot.
    pub directory: PathBuf,
    pub operation: Operation,
    /// Who asks, for people watching: an agent's or a task's name. The
    /// worktree's folder name when absent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
    /// An absolute directory on the machine that builds (this one, or the
    /// remote host for `os`) to copy the executables Cargo produced into,
    /// with their debug information, once the build succeeds; for `run`,
    /// what the command left in `BUILDD_OUT`. Requests that copy elsewhere
    /// are different builds.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub copy_to: Option<PathBuf>,
    /// For a `nextest` run of every test: run the test binaries that passed
    /// before and are unchanged too.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub rerun_all: bool,
    /// The OS to build on, as Rust names it (`linux`, `macos`): a remote
    /// host or this machine can satisfy it. Any enabled OS when absent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub os: Option<String>,
    /// A configured host name, or `local`. Absent means any enabled host
    /// matching `os`. Artifact exports without an OS or host stay local.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub host: Option<String>,
    /// Optional work, such as prewarming a branch: it waits behind every
    /// other build and starts only while another slot stays free. A build
    /// anyone else asks for is no longer optional.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub optional: bool,
}

impl BuildRequest {
    /// Whether policy and explicit constraints permit this destination.
    #[must_use]
    pub fn accepts_host(&self, host: &Host) -> bool {
        host.enabled
            && self.os.as_ref().is_none_or(|os| os == &host.os)
            && self.host.as_ref().is_none_or(|name| name == &host.name)
            && !(self.copy_to.is_some()
                && self.os.is_none()
                && self.host.is_none()
                && host.name != "local")
    }
}

/// A build of a tree pushed into a project's mirror.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct RevisionRequest {
    /// The mirror's name, as [`Request::Mirror`] gave it.
    pub project: String,
    pub revision: Revision,
    /// The worktree the tree came from on the other machine: who is
    /// building, for supersession and people watching.
    pub worktree: PathBuf,
    /// Where in the tree Cargo runs.
    pub prefix: PathBuf,
    pub operation: Operation,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
    /// An absolute directory on this machine, as [`BuildRequest::copy_to`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub copy_to: Option<PathBuf>,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub rerun_all: bool,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub optional: bool,
}

/// Where a project's mirror is.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct Mirror {
    pub path: PathBuf,
}

/// One host's local admission information, freshly read by the router.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct Capacity {
    pub status: Status,
    /// A measured peak of this exact compilation. An unmeasured request
    /// uses the host's configured reservation, exclusive by default.
    pub peak_memory: Option<u64>,
}

/// A host in the coordinator's policy. Reachability is observed separately.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct Host {
    pub name: String,
    pub os: String,
    pub enabled: bool,
}

/// What the daemon tells a client about its build.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Message {
    /// The request waits for a slot. Sent again with a newer revision when a
    /// later request from the same worktree supersedes it.
    Queued { revision: Revision },
    /// Cargo runs for `revision` in `slot`.
    Started { revision: Revision, slot: String },
    /// A line of Cargo's standard output: JSON messages (diagnostics, the
    /// final build message) and test output; for `run`, of the command's.
    Stdout { line: String },
    /// A line of Cargo's standard error, or the command's.
    Stderr { line: String },
    /// An executable was copied out of the slot as the request asked:
    /// Cargo's `compiler-artifact` message for it, with every path naming the
    /// copy.
    Copied { artifact: String },
    /// An output a `run` command left in `BUILDD_OUT` was moved to `path`,
    /// on the machine that ran it, as the request asked.
    Delivered { path: PathBuf },
    /// The build of `revision` ended. For `test`, `test_ms` is the part of
    /// `build_ms` after compilation finished: running the tests.
    Finished {
        revision: Revision,
        outcome: Outcome,
        queued_ms: u64,
        build_ms: u64,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        test_ms: Option<u64>,
    },
    /// The request was not accepted.
    Rejected { reason: String },
}

impl Message {
    /// Whether this is the last message of a build.
    #[must_use]
    pub fn is_final(&self) -> bool {
        matches!(self, Self::Finished { .. } | Self::Rejected { .. })
    }
}

/// What a build left for later: the log of its whole output, and what that
/// output said about its tests and errors.
#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
pub struct BuildReport {
    /// Its log's file name in the daemon's `logs` directory, when one was
    /// written.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub log: Option<String>,
    /// The tests that failed: nextest's `binary test`, or libtest's test
    /// path.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub failed_tests: Vec<String>,
    /// The slowest tests nextest timed, slowest first.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub slowest_tests: Vec<TestTime>,
    /// The first errors the compiler or Cargo reported.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub errors: Vec<String>,
}

/// How long a test took.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct TestTime {
    pub test: String,
    pub ms: u64,
}

/// How a build ended.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Outcome {
    /// Cargo, or a `run` command, exited with `code`.
    Exited { code: i32 },
    /// Cargo, or a `run` command, was killed by `signal`.
    Signaled { signal: i32 },
    /// Cargo or the command could not run (preparing the slot or starting
    /// it failed), or it succeeded and copying its products out failed.
    Failed { reason: String },
}

impl Outcome {
    /// Whether Cargo succeeded.
    #[must_use]
    pub fn success(&self) -> bool {
        matches!(self, Self::Exited { code: 0 })
    }
}

/// The daemon's state.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct Status {
    /// Jobs the daemon's budget deals out across all builds.
    pub jobs: usize,
    /// Of those, the ones no build holds now.
    pub idle_jobs: usize,
    /// Builds that run at once, and slots per repository.
    pub capacity: usize,
    /// Disk the targets of all slots may keep together, in bytes, and what
    /// they kept when last measured.
    #[serde(default)]
    pub disk_budget: u64,
    #[serde(default)]
    pub disk_used: u64,
    /// Disk free on the volume holding the slots, in bytes, when it could be
    /// read, and the floor below which idle slots give up what builds used
    /// longest ago.
    #[serde(default)]
    pub free_disk: Option<u64>,
    #[serde(default)]
    pub min_free: u64,
    /// Free space reserved for additional output per running build.
    #[serde(default)]
    pub build_headroom: u64,
    /// Disk maintenance or space is holding build admission.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub disk_wait: Option<DiskWait>,
    /// The latest eviction took caches or compiled units builds used in the
    /// last ten minutes: the budget or free disk is below what builds use,
    /// so they compile from scratch.
    #[serde(default)]
    pub undersized: bool,
    /// Memory admission budget, in bytes, and the running builds' reservations
    /// (measured peaks, or the host's policy for unmeasured compilations).
    #[serde(default)]
    pub memory: u64,
    #[serde(default)]
    pub memory_in_use: u64,
    /// Reservation for each unmeasured compilation; None learns exclusively.
    #[serde(default)]
    pub unmeasured_memory: Option<u64>,
    pub slots: Vec<SlotStatus>,
    /// Waiting builds, next first.
    pub queue: Vec<QueuedBuild>,
    /// It takes no new builds; running and queued ones end.
    #[serde(default)]
    pub draining: bool,
}

/// A build slot.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct SlotStatus {
    pub name: String,
    /// The worktree the slot last built for.
    pub worktree: Option<PathBuf>,
    /// Disk its target used after its last build, in bytes, when measured.
    pub size: Option<u64>,
    /// What it does while no build runs in it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub chore: Option<Chore>,
    pub build: Option<RunningBuild>,
    /// Crates its target holds in more than one variant, when counted.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub duplicated: Option<usize>,
    /// How its latest build ended, since the daemon started.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last: Option<LastBuild>,
}

/// What an idle slot is busy with.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Chore {
    /// Its target is read: what it holds and what removing each part frees.
    Measuring,
    /// It gives up what builds used longest ago.
    Evicting,
    /// Its target is reserved for a cache inventory or approved cleanup.
    Cache,
}

/// Why disk admission is waiting. No running slot is reclaimed.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum DiskWait {
    Measuring,
    /// Cache-owner work holds an inventory needed for disk admission.
    Protected,
    MaintenanceFailed {
        slot: String,
        reason: String,
    },
    Evicting,
    /// The cache budget cannot yet be met by idle artifacts.
    Budget,
    /// The floor and output reservations require this much free space.
    Headroom {
        required: u64,
    },
    /// Reading the volume's free space failed. Admission fails closed.
    Unavailable,
}

impl std::fmt::Display for DiskWait {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Measuring => f.write_str("measuring build artifacts"),
            Self::Protected => f.write_str("waiting for protected artifact inventories"),
            Self::MaintenanceFailed { slot, reason } => {
                write!(f, "artifact maintenance failed for {slot}: {reason}")
            }
            Self::Evicting => f.write_str("reclaiming idle build artifacts"),
            Self::Budget => f.write_str("cache disk budget exceeded"),
            Self::Headroom { required } => write!(
                f,
                "need {} free for the disk floor and build output",
                crate::log::gib(*required)
            ),
            Self::Unavailable => f.write_str("free disk could not be read"),
        }
    }
}

/// A slot's latest finished build.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct LastBuild {
    pub operation: Operation,
    pub outcome: Outcome,
    pub build_ms: u64,
    /// For `test`, the part of `build_ms` spent running the tests.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub test_ms: Option<u64>,
}

/// A build in a slot.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct RunningBuild {
    pub revision: Revision,
    pub operation: Operation,
    /// Who waits for it, one entry per request it serves.
    pub who: Vec<String>,
    pub elapsed_ms: u64,
    /// Crates Cargo compiled so far, and crates it found up to date.
    pub compiled: u64,
    pub fresh: u64,
    /// Nobody waits for it any more; it is being stopped.
    pub cancelled: bool,
    /// Jobs of the budget it uses now: Cargo's own and the tokens its
    /// processes took, and at least the tests' share while its tests run.
    #[serde(default)]
    pub tokens: usize,
    pub phase: Phase,
}

/// What a running build does.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Phase {
    Compiling,
    /// A `test` build whose compilation finished runs its tests.
    Testing,
    /// A `run` command runs.
    Running,
    /// Cargo or the command succeeded; its products are copied out.
    Copying,
}

/// A build waiting for a slot.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct QueuedBuild {
    pub revision: Revision,
    pub operation: Operation,
    /// Who waits for it, one entry per request it serves.
    pub who: Vec<String>,
    pub waited_ms: u64,
    /// How long it is expected to take, from earlier builds.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub estimate_ms: Option<u64>,
    /// It waits for a busy slot that has what it needs, although another
    /// slot is idle.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub held: Option<Hold>,
    /// It waits for memory: it is expected to need this many bytes at its
    /// peak, more than running builds leave.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub memory_needed: Option<u64>,
}

/// Why a build waits for a busy slot: building cold in an idle one was
/// expected to take `cold_ms`, waiting for `slot` and building there
/// `wait_ms`.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct Hold {
    pub slot: String,
    pub wait_ms: u64,
    pub cold_ms: u64,
}

/// What `buildd top` shows.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct Activity {
    pub status: Status,
    /// When the daemon started, in milliseconds since the Unix epoch.
    pub started_at_ms: u64,
    /// Everything the daemon did since it started.
    pub totals: Totals,
    /// Its most recent events, oldest first.
    pub events: Vec<Event>,
    /// How fast builds went lately.
    #[serde(default)]
    pub speed: Speed,
}

/// How fast builds went over the last [`Speed::window_ms`]: queue waits, and
/// wall time per kind of build, as medians and 90th percentiles.
#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
pub struct Speed {
    pub window_ms: u64,
    pub builds: u64,
    pub wait: Percentiles,
    pub kinds: Vec<KindSpeed>,
}

/// The wall time of one kind of build.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct KindSpeed {
    /// `checks`, `scoped tests`, `workspace tests` or `builds`.
    pub kind: String,
    pub builds: u64,
    pub wall: Percentiles,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
pub struct Percentiles {
    pub median_ms: u64,
    pub p90_ms: u64,
}

/// A remote host as `buildd top` shows it.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct RemoteActivity {
    pub name: String,
    pub os: String,
    pub activity: Result<Box<Activity>, String>,
}

/// Counts since the daemon started, derived from its events.
#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
pub struct Totals {
    /// Build requests accepted.
    pub requests: u64,
    /// Of those, the ones served by a build another request started.
    pub shared: u64,
    /// Queued requests moved to a newer tree of their own worktree.
    pub replaced: u64,
    /// Cargo runs started.
    pub builds: u64,
    /// Queued builds nobody waited for any more.
    pub dropped: u64,
    /// Running builds stopped because nobody waited for them any more.
    pub cancelled: u64,
    /// Crates Cargo compiled, and crates it found up to date.
    pub compiled: u64,
    pub fresh: u64,
    /// CPU time of every Cargo run and the compilers it started.
    pub cpu_ms: u64,
    /// Test binaries not run because they were unchanged since they passed.
    #[serde(default)]
    pub skipped: u64,
    /// Worktrees that asked for a build.
    pub worktrees: u64,
    /// Builds that were the first of their worktree, and of those, the ones
    /// that ran in a slot that had already done their compilation.
    pub first_builds: u64,
    pub warm_first_builds: u64,
}

/// Something the daemon did.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct Event {
    /// When, in milliseconds since the Unix epoch.
    pub at_ms: u64,
    pub kind: EventKind,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum EventKind {
    /// `who` asked for a build of their worktree; `shared` when an equal
    /// build was already queued or running and serves it too.
    Requested {
        who: String,
        worktree: PathBuf,
        operation: Operation,
        revision: Revision,
        shared: bool,
    },
    /// `who`'s queued request now waits for a newer tree of its worktree.
    Replaced {
        who: String,
        operation: Operation,
        from: Revision,
        to: Revision,
    },
    /// A build waits for busy `slot` although another slot is idle, as
    /// `hold` explains.
    Held {
        who: Vec<String>,
        operation: Operation,
        hold: Hold,
    },
    /// Cargo started. `warm` when the slot had done this compilation before;
    /// `first` when this is the first build of its worktree.
    Started {
        slot: String,
        who: Vec<String>,
        operation: Operation,
        revision: Revision,
        warm: bool,
        first: bool,
    },
    /// Cargo ended.
    Finished {
        slot: String,
        who: Vec<String>,
        operation: Operation,
        revision: Revision,
        outcome: Outcome,
        build_ms: u64,
        /// For `test`, the part of `build_ms` spent running the tests.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        test_ms: Option<u64>,
        /// Test binaries not run: unchanged since they passed.
        #[serde(default)]
        skipped: u64,
        /// How long its longest-waiting request waited in the queue.
        #[serde(default)]
        queued_ms: u64,
        compiled: u64,
        fresh: u64,
        /// What the build and its processes used, when it ran.
        usage: Option<Box<Usage>>,
        #[serde(default)]
        report: BuildReport,
    },
    /// A queued build nobody waited for any more was dropped.
    Dropped {
        operation: Operation,
        revision: Revision,
    },
    /// A running build nobody waited for any more was stopped.
    Cancelled {
        slot: String,
        operation: Operation,
        revision: Revision,
    },
    /// More crates than before are compiled in several variants in `slot`:
    /// builds there select different features.
    Duplicated { slot: String, crates: usize },
    /// Idle `slots` gave up what builds used longest ago to cover
    /// `shortage`: `caches` incremental caches, `units` compiled units and
    /// `test_files` directories tests left, `in_use` of them used in the last
    /// ten minutes, freeing at least `freed` bytes.
    Evicted {
        shortage: Shortage,
        freed: u64,
        caches: usize,
        units: usize,
        test_files: usize,
        in_use: usize,
        slots: Vec<String>,
    },
}

/// How far build disk is from its limits, in bytes: the slots' targets
/// over their budget, and free disk under its floor.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
pub struct Shortage {
    pub over_budget: u64,
    /// Shortfall against the volume floor plus build output reservations.
    pub under_floor: u64,
}

impl Shortage {
    /// What eviction must free to meet both limits.
    #[must_use]
    pub fn needed(self) -> u64 {
        self.over_budget.max(self.under_floor)
    }
}

/// What a build and the processes it started used.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct Usage {
    pub cpu_ms: u64,
    /// Peak summed resident memory of its process tree and process group,
    /// sampled every second, in bytes.
    pub peak_memory: u64,
    /// Its test phase, when it was sampled separately from compilation.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub test_memory: Option<TestMemory>,
}

/// A measured test-phase peak and the concurrency under which it was measured.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct TestMemory {
    pub jobs: usize,
    pub peak_memory: u64,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cargo::Command;

    #[test]
    fn requests_and_messages_are_tagged_json_lines() {
        let request = Request::Build(BuildRequest {
            directory: "/work".into(),
            operation: Operation {
                command: Command::Check,
                args: vec!["-p".into(), "a".into()],
                rustflags: Vec::new(),
            },
            label: None,
            copy_to: None,
            rerun_all: false,
            optional: false,
            os: None,
            host: None,
        });
        let text = serde_json::to_string(&request).unwrap();
        assert_eq!(
            text,
            r#"{"type":"schedule","directory":"/work","operation":{"command":"check","args":["-p","a"]}}"#
        );
        assert_eq!(serde_json::from_str::<Request>(&text).unwrap(), request);
        assert_eq!(
            serde_json::to_string(&Request::Status).unwrap(),
            r#"{"type":"status"}"#
        );
        let finished: Message = serde_json::from_str(
            r#"{"type":"finished","revision":"abc","outcome":{"kind":"exited","code":0},"queued_ms":1,"build_ms":2}"#,
        )
        .unwrap();
        assert!(finished.is_final());
    }
}
