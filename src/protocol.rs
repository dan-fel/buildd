//! The daemon's wire protocol: one JSON object per line over a Unix socket.
//!
//! A client sends one [`Request`]. For [`Request::Build`] the daemon answers
//! with [`Message`]s ending in [`Message::Finished`] or
//! [`Message::Rejected`]. Closing the connection earlier withdraws the
//! request; a build nobody waits for any more is cancelled. For
//! [`Request::Status`] it answers with one [`Status`].

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
    /// Describe the slots and the queue.
    Status,
}

/// A build of a worktree's current content.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct BuildRequest {
    /// A directory inside a git worktree. Cargo runs in the same directory
    /// of the snapshot.
    pub directory: PathBuf,
    pub operation: Operation,
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
    /// The build of `revision` ended.
    Finished {
        revision: Revision,
        outcome: Outcome,
        queued_ms: u64,
        build_ms: u64,
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

/// How a build ended.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Outcome {
    /// Cargo exited with `code`.
    Exited { code: i32 },
    /// Cargo was killed by `signal`.
    Signaled { signal: i32 },
    /// Cargo could not run: preparing the slot or starting it failed.
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
    /// Compiler jobs the daemon's jobserver hands out across all builds.
    pub jobs: usize,
    /// Of those, the ones no build holds now.
    pub idle_jobs: usize,
    pub slots: Vec<SlotStatus>,
    /// Waiting builds, next first.
    pub queue: Vec<QueuedBuild>,
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
}

/// A build in a slot.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct RunningBuild {
    pub revision: Revision,
    pub operation: Operation,
    pub waiters: usize,
    pub elapsed_ms: u64,
    /// Nobody waits for it any more; it is being stopped.
    pub cancelled: bool,
}

/// A build waiting for a slot.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct QueuedBuild {
    pub revision: Revision,
    pub operation: Operation,
    /// The worktrees of its waiters.
    pub worktrees: Vec<PathBuf>,
    pub waited_ms: u64,
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
            },
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
