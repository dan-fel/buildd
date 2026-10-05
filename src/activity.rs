//! What the daemon did, for people watching: its recent events and the
//! totals derived from them.

use std::collections::{HashSet, VecDeque};
use std::path::PathBuf;
use std::time::SystemTime;

use crate::protocol::{Activity, Event, EventKind, Status, Totals};

/// How many recent events the log keeps.
const KEPT_EVENTS: usize = 200;

pub(crate) struct ActivityLog {
    started: SystemTime,
    totals: Totals,
    events: VecDeque<Event>,
    worktrees: HashSet<PathBuf>,
}

impl ActivityLog {
    pub(crate) fn new(started: SystemTime) -> Self {
        Self {
            started,
            totals: Totals::default(),
            events: VecDeque::new(),
            worktrees: HashSet::new(),
        }
    }

    /// Records that `kind` happened `at`.
    pub(crate) fn record(&mut self, at: SystemTime, kind: EventKind) {
        let totals = &mut self.totals;
        match &kind {
            EventKind::Requested {
                worktree, shared, ..
            } => {
                totals.requests += 1;
                totals.shared += u64::from(*shared);
                self.worktrees.insert(worktree.clone());
                totals.worktrees = self.worktrees.len() as u64;
            }
            EventKind::Replaced { .. } => totals.replaced += 1,
            EventKind::Started { warm, first, .. } => {
                totals.builds += 1;
                totals.first_builds += u64::from(*first);
                totals.warm_first_builds += u64::from(*first && *warm);
            }
            EventKind::Finished {
                compiled,
                fresh,
                usage,
                ..
            } => {
                totals.compiled += compiled;
                totals.fresh += fresh;
                totals.cpu_ms += usage.map_or(0, |usage| usage.cpu_ms);
            }
            EventKind::Dropped { .. } => totals.dropped += 1,
            EventKind::Cancelled { .. } => totals.cancelled += 1,
            EventKind::Pruned { .. } => {}
        }
        if self.events.len() == KEPT_EVENTS {
            self.events.pop_front();
        }
        self.events.push_back(Event {
            at_ms: epoch_millis(at),
            kind,
        });
    }

    /// What `buildd top` shows, with the daemon's current `status`.
    pub(crate) fn activity(&self, status: Status) -> Activity {
        Activity {
            status,
            started_at_ms: epoch_millis(self.started),
            totals: self.totals.clone(),
            events: self.events.iter().cloned().collect(),
        }
    }
}

fn epoch_millis(time: SystemTime) -> u64 {
    let since = time
        .duration_since(SystemTime::UNIX_EPOCH)
        .expect("the clock is past 1970");
    u64::try_from(since.as_millis()).expect("milliseconds since 1970 fit 64 bits")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cargo::{Command, Operation};
    use crate::protocol::{Outcome, Usage};
    use crate::snapshot::Revision;

    fn check() -> Operation {
        Operation {
            command: Command::Check,
            args: Vec::new(),
        }
    }

    fn tree(name: &str) -> Revision {
        serde_json::from_value(serde_json::Value::String(name.into())).unwrap()
    }

    fn requested(worktree: &str, shared: bool) -> EventKind {
        EventKind::Requested {
            who: worktree.into(),
            worktree: worktree.into(),
            operation: check(),
            revision: tree("t"),
            shared,
        }
    }

    #[test]
    fn totals_follow_the_events_and_the_log_keeps_the_latest() {
        let mut log = ActivityLog::new(SystemTime::now());
        log.record(SystemTime::now(), requested("/a", false));
        log.record(SystemTime::now(), requested("/b", true));
        log.record(SystemTime::now(), requested("/a", false));
        log.record(
            SystemTime::now(),
            EventKind::Started {
                slot: "s/0".into(),
                who: vec!["a".into(), "b".into()],
                operation: check(),
                revision: tree("t"),
                warm: true,
                first: true,
            },
        );
        log.record(
            SystemTime::now(),
            EventKind::Finished {
                slot: "s/0".into(),
                who: vec!["a".into(), "b".into()],
                operation: check(),
                revision: tree("t"),
                outcome: Outcome::Exited { code: 0 },
                build_ms: 10,
                compiled: 3,
                fresh: 97,
                usage: Some(Usage {
                    cpu_ms: 1500,
                    peak_memory: 1,
                }),
            },
        );
        let totals = log.activity(empty_status()).totals;
        assert_eq!(
            totals,
            Totals {
                requests: 3,
                shared: 1,
                builds: 1,
                compiled: 3,
                fresh: 97,
                cpu_ms: 1500,
                worktrees: 2,
                first_builds: 1,
                warm_first_builds: 1,
                ..Totals::default()
            }
        );
        for _ in 0..KEPT_EVENTS {
            log.record(SystemTime::now(), requested("/c", false));
        }
        let activity = log.activity(empty_status());
        assert_eq!(activity.events.len(), KEPT_EVENTS);
        assert_eq!(activity.totals.requests, 3 + KEPT_EVENTS as u64);
    }

    fn empty_status() -> Status {
        Status {
            jobs: 1,
            idle_jobs: 1,
            capacity: 1,
            slot_limit: 1,
            slots: Vec::new(),
            queue: Vec::new(),
        }
    }
}
