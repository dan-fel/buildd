//! What went wrong lately: the failed builds among the daemon's events,
//! summed up to find what keeps failing.
//!
//! ```text
//! finished events since a time ─┬─▶ recent    the latest failed builds
//!                               ├─▶ tests     failed in how many builds, trees
//!                               ├─▶ errors    reported by how many builds
//!                               ├─▶ mixed     same tree + command: passed and failed
//!                               └─▶ slowest   tests by their latest time
//! ```

use std::collections::{BTreeSet, HashMap};
use std::path::PathBuf;

use crate::cargo::Operation;
use crate::protocol::{
    CommonError, Event, EventKind, FailedBuild, FailingTest, Failures, MixedOutcome, TestTime,
};
use crate::snapshot::Revision;

const RECENT: usize = 15;
const TESTS: usize = 15;
const ERRORS: usize = 10;
const MIXED: usize = 10;
const SLOWEST: usize = 10;

/// The failures among `events`, oldest first, at or after `since_ms`; build
/// logs live in `logs`.
pub(crate) fn summarize(events: &[Event], since_ms: u64, logs: PathBuf) -> Failures {
    let mut summary = Failures {
        since_ms,
        logs,
        ..Failures::default()
    };
    let mut tests = HashMap::<&str, (u64, BTreeSet<&Revision>, BTreeSet<&str>)>::new();
    let mut errors = HashMap::<&str, u64>::new();
    let mut outcomes = HashMap::<(&Revision, &Operation), (u64, u64)>::new();
    let mut times = HashMap::<&str, u64>::new();
    for event in events.iter().filter(|event| event.at_ms >= since_ms) {
        let EventKind::Finished {
            who,
            operation,
            revision,
            outcome,
            report,
            ..
        } = &event.kind
        else {
            continue;
        };
        summary.builds += 1;
        for time in &report.slowest_tests {
            times.insert(&time.test, time.ms);
        }
        let counts = outcomes.entry((revision, operation)).or_default();
        if outcome.success() {
            counts.0 += 1;
            continue;
        }
        counts.1 += 1;
        summary.failed += 1;
        for test in &report.failed_tests {
            let (failures, trees, by) = tests.entry(test).or_default();
            *failures += 1;
            trees.insert(revision);
            by.extend(who.iter().map(String::as_str));
        }
        for error in &report.errors {
            *errors.entry(without_place(error)).or_default() += 1;
        }
        summary.recent.push(FailedBuild {
            at_ms: event.at_ms,
            who: who.clone(),
            operation: operation.clone(),
            revision: revision.clone(),
            outcome: outcome.clone(),
            report: report.clone(),
        });
    }
    summary.recent.reverse();
    summary.recent.truncate(RECENT);
    summary.tests = tests
        .into_iter()
        .map(|(test, (failures, trees, by))| FailingTest {
            test: test.to_owned(),
            failures,
            trees: trees.len() as u64,
            who: by.into_iter().map(str::to_owned).collect(),
        })
        .collect();
    summary.tests.sort_by(|a, b| {
        b.failures
            .cmp(&a.failures)
            .then_with(|| a.test.cmp(&b.test))
    });
    summary.tests.truncate(TESTS);
    summary.errors = errors
        .into_iter()
        .map(|(error, builds)| CommonError {
            error: error.to_owned(),
            builds,
        })
        .collect();
    summary
        .errors
        .sort_by(|a, b| b.builds.cmp(&a.builds).then_with(|| a.error.cmp(&b.error)));
    summary.errors.truncate(ERRORS);
    summary.mixed = outcomes
        .into_iter()
        .filter(|(_, (passed, failed))| *passed > 0 && *failed > 0)
        .map(|((revision, operation), (passed, failed))| MixedOutcome {
            revision: revision.clone(),
            operation: operation.clone(),
            passed,
            failed,
        })
        .collect();
    summary.mixed.sort_by(|a, b| {
        (b.failed + b.passed)
            .cmp(&(a.failed + a.passed))
            .then_with(|| a.revision.cmp(&b.revision))
    });
    summary.mixed.truncate(MIXED);
    summary.slowest = times
        .into_iter()
        .map(|(test, ms)| TestTime {
            test: test.to_owned(),
            ms,
        })
        .collect();
    summary
        .slowest
        .sort_by(|a, b| b.ms.cmp(&a.ms).then_with(|| a.test.cmp(&b.test)));
    summary.slowest.truncate(SLOWEST);
    summary
}

/// `error` without the ` (file:line)` a report adds, so one error at
/// different places counts as one.
fn without_place(error: &str) -> &str {
    match error.rsplit_once(" (") {
        Some((message, place)) if place.ends_with(')') && place.contains(':') => message,
        _ => error,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cargo::Command;
    use crate::protocol::{BuildReport, Outcome};

    fn finished(at_ms: u64, tree: &str, code: i32, report: BuildReport) -> Event {
        Event {
            at_ms,
            kind: EventKind::Finished {
                slot: "s/0".into(),
                who: vec![format!("agent-{at_ms}")],
                operation: Operation {
                    command: Command::Nextest,
                    args: vec!["--workspace".into()],
                    rustflags: Vec::new(),
                },
                revision: serde_json::from_value(serde_json::Value::String(tree.into())).unwrap(),
                outcome: Outcome::Exited { code },
                build_ms: 1,
                test_ms: None,
                skipped: 0,
                queued_ms: 0,
                compiled: 0,
                fresh: 0,
                usage: None,
                report,
            },
        }
    }

    fn failing(tests: &[&str], errors: &[&str]) -> BuildReport {
        BuildReport {
            failed_tests: tests.iter().map(|test| (*test).to_owned()).collect(),
            errors: errors.iter().map(|error| (*error).to_owned()).collect(),
            ..BuildReport::default()
        }
    }

    #[test]
    fn failures_are_counted_by_test_error_and_tree_within_the_window() {
        let slow = BuildReport {
            slowest_tests: vec![TestTime {
                test: "app slow".into(),
                ms: 9000,
            }],
            ..BuildReport::default()
        };
        let events = [
            // Before the window.
            finished(1, "t0", 100, failing(&["app old"], &[])),
            finished(10, "t1", 0, slow),
            finished(11, "t1", 100, failing(&["app flaky"], &[])),
            finished(12, "t2", 100, failing(&["app flaky", "app other"], &[])),
            finished(
                13,
                "t3",
                101,
                failing(&[], &["mismatched types (src/a.rs:1)"]),
            ),
            finished(
                14,
                "t4",
                101,
                failing(&[], &["mismatched types (src/b.rs:9)"]),
            ),
        ];
        let summary = summarize(&events, 10, PathBuf::from("/logs"));
        assert_eq!((summary.builds, summary.failed), (5, 4));
        assert_eq!(summary.recent[0].at_ms, 14, "newest first");
        assert_eq!(summary.tests[0].test, "app flaky");
        assert_eq!((summary.tests[0].failures, summary.tests[0].trees), (2, 2));
        assert_eq!(summary.tests[0].who, ["agent-11", "agent-12"]);
        assert_eq!(
            summary.errors,
            [CommonError {
                error: "mismatched types".into(),
                builds: 2
            }]
        );
        assert_eq!(summary.mixed.len(), 1);
        assert_eq!((summary.mixed[0].passed, summary.mixed[0].failed), (1, 1));
        assert_eq!(summary.slowest[0].test, "app slow");
    }
}
