//! What the daemon did, for people watching: its recent events and the
//! totals derived from them, and every event in a file for hindsight.

use std::collections::{HashSet, VecDeque};
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use crate::log::log;
use crate::protocol::{Activity, Event, EventKind, Status, Totals};

/// How many recent events the log keeps in memory.
const KEPT_EVENTS: usize = 200;
/// How large the event file grows before it moves aside.
const EVENT_FILE_LIMIT: u64 = 10 << 20;

pub(crate) struct ActivityLog {
    started: SystemTime,
    totals: Totals,
    events: VecDeque<Event>,
    worktrees: HashSet<PathBuf>,
    file: EventFile,
}

/// `events.jsonl` in the daemon's home: every event as a JSON line, so
/// hindsight reaches past the events kept in memory and past restarts. At
/// [`EVENT_FILE_LIMIT`] it moves to `events.jsonl.1`, replacing the older
/// one there.
pub(crate) struct EventFile {
    path: PathBuf,
}

impl EventFile {
    pub(crate) fn new(home: &Path) -> Self {
        Self {
            path: home.join("events.jsonl"),
        }
    }

    fn append(&self, event: &Event) {
        let mut line = serde_json::to_string(event).expect("events serialize");
        line.push('\n');
        let full =
            std::fs::metadata(&self.path).is_ok_and(|metadata| metadata.len() >= EVENT_FILE_LIMIT);
        let written = if full {
            std::fs::rename(&self.path, self.path.with_extension("jsonl.1"))
        } else {
            Ok(())
        }
        .and_then(|()| {
            std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(&self.path)
        })
        .and_then(|mut file| file.write_all(line.as_bytes()));
        if let Err(error) = written {
            log!("could not write {}: {error}", self.path.display());
        }
    }
}

impl ActivityLog {
    pub(crate) fn new(started: SystemTime, file: EventFile) -> Self {
        Self {
            started,
            totals: Totals::default(),
            events: VecDeque::new(),
            worktrees: HashSet::new(),
            file,
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
            EventKind::Pruned { .. } | EventKind::Reclaimed { .. } | EventKind::Held { .. } => {}
        }
        let event = Event {
            at_ms: epoch_millis(at),
            kind,
        };
        self.file.append(&event);
        if self.events.len() == KEPT_EVENTS {
            self.events.pop_front();
        }
        self.events.push_back(event);
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
    use crate::snapshot::tests::TempDir;

    fn check() -> Operation {
        Operation {
            command: Command::Check,
            args: Vec::new(),
            rustflags: Vec::new(),
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
        let home = TempDir::new();
        let mut log = ActivityLog::new(SystemTime::now(), EventFile::new(&home.0));
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
                test_ms: None,
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
        // The file keeps every event, not only the latest.
        let file = std::fs::read_to_string(home.0.join("events.jsonl")).unwrap();
        assert_eq!(file.lines().count(), 5 + KEPT_EVENTS);
        let first: Event = serde_json::from_str(file.lines().next().unwrap()).unwrap();
        assert_eq!(first.kind, requested("/a", false));
    }

    #[test]
    fn a_full_event_file_moves_aside_and_a_new_one_starts() {
        let home = TempDir::new();
        let path = home.0.join("events.jsonl");
        std::fs::write(
            &path,
            vec![b'x'; usize::try_from(EVENT_FILE_LIMIT).unwrap()],
        )
        .unwrap();
        let mut log = ActivityLog::new(SystemTime::now(), EventFile::new(&home.0));
        log.record(SystemTime::now(), requested("/a", false));
        assert_eq!(
            std::fs::metadata(home.0.join("events.jsonl.1"))
                .unwrap()
                .len(),
            EVENT_FILE_LIMIT
        );
        assert_eq!(std::fs::read_to_string(&path).unwrap().lines().count(), 1);
    }

    fn empty_status() -> Status {
        Status {
            jobs: 1,
            idle_jobs: 1,
            capacity: 1,
            slot_limit: 1,
            free_disk: None,
            min_free: 0,
            memory: 1,
            memory_in_use: 0,
            slots: Vec::new(),
            queue: Vec::new(),
        }
    }
}
