//! The daemon's wire protocol: one JSON object per line over a Unix socket.
//!
//! A client sends one [`Request`]. For [`Request::Build`] the daemon answers
//! with [`Message`]s ending in [`Message::Finished`] or
//! [`Message::Rejected`]. Closing the connection earlier withdraws the
//! request; a build nobody waits for any more is cancelled. For
//! [`Request::Status`] it answers with one [`Status`], for
//! [`Request::Activity`] with one [`Activity`].

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::cargo::Operation;
use crate::snapshot::Revision;

/// What a client asks the daemon.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Request {
    /// Build the current content of the worktree holding `directory`.
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
    /// An absolute directory to copy the executables Cargo produced into,
    /// with their debug information, once the build succeeds. Requests that
    /// copy elsewhere are different builds.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub copy_to: Option<PathBuf>,
    /// For a `nextest` run of every test: run the test binaries that passed
    /// before and are unchanged too.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub rerun_all: bool,
    /// The OS to build on, as Rust names it (`linux`, `macos`): a remote
    /// host's daemon builds when it is not this machine's. This machine's
    /// when absent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub os: Option<String>,
    /// Optional work, such as prewarming a branch: it waits behind every
    /// other build and starts only while another slot stays free. A build
    /// anyone else asks for is no longer optional.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub optional: bool,
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
    /// final build message) and test output.
    Stdout { line: String },
    /// A line of Cargo's standard error.
    Stderr { line: String },
    /// An executable was copied out of the slot as the request asked:
    /// Cargo's `compiler-artifact` message for it, with every path naming the
    /// copy.
    Copied { artifact: String },
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
    /// Cargo exited with `code`.
    Exited { code: i32 },
    /// Cargo was killed by `signal`.
    Signaled { signal: i32 },
    /// Cargo could not run (preparing the slot or starting it failed), or
    /// it succeeded and copying its executables out failed.
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
    /// Disk a slot's target may keep between builds, in bytes.
    pub slot_limit: u64,
    /// Disk free on the volume holding the slots, in bytes, when it could be
    /// read, and the floor below which idle slots give up what builds used
    /// longest ago.
    #[serde(default)]
    pub free_disk: Option<u64>,
    #[serde(default)]
    pub min_free: u64,
    /// Memory running builds may use together, in bytes, and what the
    /// running builds are expected to use at their peaks.
    #[serde(default)]
    pub memory: u64,
    #[serde(default)]
    pub memory_in_use: u64,
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
    /// Its target is being kept within its disk limit after a build.
    pub maintaining: bool,
    /// Keeping to the limit last took caches its builds were using: the
    /// limit is below what they need, and they compile from scratch.
    pub undersized: bool,
    pub build: Option<RunningBuild>,
    /// Crates its target holds in more than one variant, when counted.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub duplicated: Option<usize>,
    /// How its latest build ended, since the daemon started.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last: Option<LastBuild>,
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
    /// Jobs of the budget it is charged now.
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
    /// Cargo succeeded; its executables are copied out of the slot.
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
    /// What each remote host's daemon does, or why it could not be asked.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub remotes: Vec<RemoteActivity>,
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
        /// What Cargo and its compilers used, when it ran.
        usage: Option<Usage>,
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
    /// Free disk was `needed` bytes below the `floor`; idle slots gave up
    /// `caches` incremental caches and `units` compiled units that builds
    /// used longest ago, freeing `freed` bytes.
    Reclaimed {
        needed: u64,
        freed: u64,
        floor: u64,
        caches: usize,
        units: usize,
    },
    /// More crates than before are compiled in several variants in `slot`:
    /// builds there select different features.
    Duplicated { slot: String, crates: usize },
    /// A slot was kept within its limit: `caches` incremental caches and
    /// `units` compiled units went, `in_use` of them used in the last ten
    /// minutes, or the whole target when `cleared`.
    Pruned {
        slot: String,
        before: u64,
        after: u64,
        caches: usize,
        units: usize,
        in_use: usize,
        cleared: bool,
    },
}

/// What a Cargo run and the compilers it started used.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct Usage {
    pub cpu_ms: u64,
    /// The largest resident memory of its process group, summed over its
    /// processes and sampled every second, in bytes.
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
        });
        let text = serde_json::to_string(&request).unwrap();
        assert_eq!(
            text,
            r#"{"type":"build","directory":"/work","operation":{"command":"check","args":["-p","a"]}}"#
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
