//! What the daemon did, for people watching: its recent events and the
//! totals derived from them, and every event in a file for hindsight.

use std::collections::{HashSet, VecDeque};
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use crate::cargo::{Command, Operation};
use crate::log::log;
use crate::protocol::{Activity, Event, EventKind, KindSpeed, Percentiles, Speed, Status, Totals};

/// How many recent events the log keeps in memory.
const KEPT_EVENTS: usize = 200;
/// How far back the speed summary looks.
const SPEED_WINDOW: std::time::Duration = std::time::Duration::from_secs(60 * 60);
/// How large the event file grows before it moves aside.
const EVENT_FILE_LIMIT: u64 = 10 << 20;

pub(crate) struct ActivityLog {
    started: SystemTime,
    totals: Totals,
    events: VecDeque<Event>,
    worktrees: HashSet<PathBuf>,
    file: EventFile,
    /// Builds that finished within [`SPEED_WINDOW`], oldest first.
    recent: VecDeque<Finished>,
}

/// A finished build, as the speed summary counts it.
struct Finished {
    at: SystemTime,
    kind: &'static str,
    build_ms: u64,
    queued_ms: u64,
}

/// What kind of build `operation` is, for the speed summary.
fn build_kind(operation: &Operation) -> &'static str {
    match operation.command {
        Command::Check | Command::Clippy => "checks",
        Command::Build => "builds",
        Command::Test | Command::Nextest
            if operation
                .args
                .iter()
                .any(|argument| argument == "--workspace") =>
        {
            "workspace tests"
        }
        Command::Test | Command::Nextest => "scoped tests",
    }
}

/// The median and 90th percentile of `values`, which must not be empty.
fn percentiles(mut values: Vec<u64>) -> Percentiles {
    assert!(!values.is_empty(), "percentiles of something");
    values.sort_unstable();
    let at = |fraction: f64| {
        #[expect(
            clippy::cast_possible_truncation,
            clippy::cast_sign_loss,
            clippy::cast_precision_loss,
            reason = "an index into a short list"
        )]
        let index = ((values.len() - 1) as f64 * fraction).round() as usize;
        values[index]
    };
    Percentiles {
        median_ms: at(0.5),
        p90_ms: at(0.9),
    }
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

    /// Every event in the file and the one before it, oldest first. A line
    /// that is not an event, such as one cut short by a crash, is left out.
    pub(crate) fn read(&self) -> Vec<Event> {
        [self.path.with_extension("jsonl.1"), self.path.clone()]
            .iter()
            .filter_map(|path| std::fs::read_to_string(path).ok())
            .flat_map(|text| {
                text.lines()
                    .filter_map(|line| serde_json::from_str(line).ok())
                    .collect::<Vec<_>>()
            })
            .collect()
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
            recent: VecDeque::new(),
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
                operation,
                compiled,
                fresh,
                usage,
                skipped,
                build_ms,
                queued_ms,
                ..
            } => {
                self.recent.push_back(Finished {
                    at,
                    kind: build_kind(operation),
                    build_ms: *build_ms,
                    queued_ms: *queued_ms,
                });
                totals.skipped += skipped;
                totals.compiled += compiled;
                totals.fresh += fresh;
                totals.cpu_ms += usage.map_or(0, |usage| usage.cpu_ms);
            }
            EventKind::Dropped { .. } => totals.dropped += 1,
            EventKind::Cancelled { .. } => totals.cancelled += 1,
            EventKind::Pruned { .. }
            | EventKind::Reclaimed { .. }
            | EventKind::Held { .. }
            | EventKind::Duplicated { .. } => {}
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
    pub(crate) fn activity(&mut self, status: Status) -> Activity {
        Activity {
            status,
            started_at_ms: epoch_millis(self.started),
            totals: self.totals.clone(),
            events: self.events.iter().cloned().collect(),
            speed: self.speed(SystemTime::now()),
            remotes: Vec::new(),
        }
    }

    /// How fast the builds that finished within [`SPEED_WINDOW`] of `now`
    /// went.
    fn speed(&mut self, now: SystemTime) -> Speed {
        while self.recent.front().is_some_and(|finished| {
            now.duration_since(finished.at).unwrap_or_default() > SPEED_WINDOW
        }) {
            self.recent.pop_front();
        }
        let window_ms = u64::try_from(SPEED_WINDOW.as_millis()).expect("an hour fits");
        if self.recent.is_empty() {
            return Speed {
                window_ms,
                ..Speed::default()
            };
        }
        let kinds = ["checks", "scoped tests", "workspace tests", "builds"]
            .into_iter()
            .filter_map(|kind| {
                let walls = self
                    .recent
                    .iter()
                    .filter(|finished| finished.kind == kind)
                    .map(|finished| finished.build_ms)
                    .collect::<Vec<_>>();
                (!walls.is_empty()).then(|| KindSpeed {
                    kind: kind.to_owned(),
                    builds: walls.len() as u64,
                    wall: percentiles(walls),
                })
            })
            .collect();
        Speed {
            window_ms,
            builds: self.recent.len() as u64,
            wait: percentiles(
                self.recent
                    .iter()
                    .map(|finished| finished.queued_ms)
                    .collect(),
            ),
            kinds,
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
    use crate::protocol::{BuildReport, Outcome, Usage};
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
                skipped: 0,
                queued_ms: 0,
                compiled: 3,
                fresh: 97,
                usage: Some(Usage {
                    cpu_ms: 1500,
                    peak_memory: 1,
                }),
                report: BuildReport::default(),
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
    fn speed_sums_up_the_last_hours_waits_and_walls_per_kind() {
        let home = TempDir::new();
        let mut log = ActivityLog::new(SystemTime::now(), EventFile::new(&home.0));
        let finished = |at: SystemTime, args: &[&str], build_ms: u64, queued_ms: u64| {
            (
                at,
                EventKind::Finished {
                    slot: "s/0".into(),
                    who: vec!["a".into()],
                    operation: Operation {
                        command: Command::Test,
                        args: args.iter().map(|arg| (*arg).to_owned()).collect(),
                        rustflags: Vec::new(),
                    },
                    revision: tree("t"),
                    outcome: Outcome::Exited { code: 0 },
                    build_ms,
                    test_ms: None,
                    skipped: 0,
                    queued_ms,
                    compiled: 0,
                    fresh: 0,
                    usage: None,
                    report: BuildReport::default(),
                },
            )
        };
        let now = SystemTime::now();
        let old = now - SPEED_WINDOW - std::time::Duration::from_secs(1);
        for (at, kind) in [
            finished(old, &["--workspace"], 999_000, 999_000),
            finished(now, &["--workspace"], 400_000, 10),
            finished(now, &["-p", "x"], 1_000, 0),
            finished(now, &["-p", "x"], 3_000, 0),
            finished(now, &["-p", "x"], 2_000, 5_000),
        ] {
            log.record(at, kind);
        }
        let speed = log.activity(empty_status()).speed;
        assert_eq!(speed.builds, 4, "the build outside the window is gone");
        // Nearest rank: of 0, 0, 10 and 5000 ms, the median is 10.
        assert_eq!(
            speed.wait,
            Percentiles {
                median_ms: 10,
                p90_ms: 5_000
            }
        );
        let scoped = speed
            .kinds
            .iter()
            .find(|kind| kind.kind == "scoped tests")
            .unwrap();
        assert_eq!(
            (scoped.builds, scoped.wall.median_ms, scoped.wall.p90_ms),
            (3, 2_000, 3_000)
        );
        let full = speed
            .kinds
            .iter()
            .find(|kind| kind.kind == "workspace tests")
            .unwrap();
        assert_eq!(full.builds, 1);
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
            draining: false,
        }
    }
}
