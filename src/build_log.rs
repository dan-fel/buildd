//! What a build leaves for later: its whole output in a log file under the
//! daemon's `logs` directory, and what that output said about its tests and
//! errors, a [`BuildReport`] its finished event carries.
//!
//! ```text
//! logs/<started ms>-<job>.log      header: who, command, tree, slot, start
//!                                  then every line its client was sent,
//!                                  diagnostics as the compiler rendered them
//! ```
//!
//! Logs are kept within [`LOG_LIMIT`] bytes in all, oldest removed first.

use std::collections::HashMap;
use std::fs::File;
use std::io::{BufWriter, Write as _};
use std::path::Path;

use serde::Deserialize;

use crate::log::log;
use crate::protocol::{BuildReport, TestTime};

/// The bytes all logs together may take.
pub(crate) const LOG_LIMIT: u64 = 2 << 30;
/// How many of the slowest tests a report keeps.
const SLOWEST: usize = 10;
/// How many errors a report keeps.
const ERRORS: usize = 5;

/// A running build's log and what its output reported so far.
pub(crate) struct BuildLog {
    name: String,
    /// None once writing failed: the build goes on without the rest of its
    /// log.
    file: Option<BufWriter<File>>,
    failed_tests: Vec<String>,
    test_ms: HashMap<String, u64>,
    errors: Vec<String>,
}

impl BuildLog {
    /// The log `name` in `directory`, starting with `header`.
    pub(crate) fn create(directory: &Path, name: String, header: &str) -> Self {
        let path = directory.join(&name);
        let file = std::fs::create_dir_all(directory)
            .and_then(|()| File::create(&path))
            .map(BufWriter::new)
            .and_then(|mut file| file.write_all(header.as_bytes()).map(|()| file));
        let file = match file {
            Ok(file) => Some(file),
            Err(error) => {
                log!("could not write the build log {}: {error}", path.display());
                None
            }
        };
        Self {
            name,
            file,
            failed_tests: Vec::new(),
            test_ms: HashMap::new(),
            errors: Vec::new(),
        }
    }

    /// A line of Cargo's standard output the client was sent: Cargo's JSON
    /// messages, and libtest's output.
    pub(crate) fn stdout(&mut self, line: &str) {
        match serde_json::from_str::<CompilerMessage>(line) {
            Ok(CompilerMessage {
                reason: "compiler-message",
                message,
            }) => {
                if message.level == "error" && !message.message.starts_with("aborting due to") {
                    let place = message
                        .spans
                        .iter()
                        .find(|span| span.is_primary)
                        .map(|span| format!(" ({}:{})", span.file_name, span.line_start))
                        .unwrap_or_default();
                    self.error(format!("{}{place}", message.message));
                }
                match message.rendered {
                    Some(rendered) => self.write(rendered.trim_end_matches('\n')),
                    None => self.write(line),
                }
            }
            _ => {
                if let Some(test) = libtest_failure(line) {
                    self.failed(test);
                }
                self.write(line);
            }
        }
    }

    /// A line of standard error the client was sent: Cargo's and nextest's
    /// own output, and buildd's notes.
    pub(crate) fn stderr(&mut self, line: &str) {
        if let Some((passed, ms, test)) = nextest_status(line) {
            self.test_ms.insert(test.to_owned(), ms);
            if !passed {
                self.failed(test.to_owned());
            }
        } else if let Some(error) = line.strip_prefix("error: ")
            // These repeat what the failed tests say.
            && !["test failed", "test run failed", "target failed", "targets failed"]
                .iter()
                .any(|repeat| error.contains(repeat))
        {
            self.error(error.to_owned());
        }
        self.write(line);
    }

    fn failed(&mut self, test: String) {
        if !self.failed_tests.contains(&test) {
            self.failed_tests.push(test);
        }
    }

    fn error(&mut self, error: String) {
        if self.errors.len() < ERRORS && !self.errors.contains(&error) {
            self.errors.push(error);
        }
    }

    fn write(&mut self, text: &str) {
        let Some(file) = &mut self.file else { return };
        if let Err(error) = writeln!(file, "{text}") {
            log!("could not write the build log {}: {error}", self.name);
            self.file = None;
        }
    }

    /// Ends the log: what the build reported.
    pub(crate) fn finish(mut self) -> BuildReport {
        let written = self.file.take().map(|mut file| file.flush());
        if let Some(Err(error)) = &written {
            log!("could not write the build log {}: {error}", self.name);
        }
        let mut slowest = self
            .test_ms
            .into_iter()
            .map(|(test, ms)| TestTime { test, ms })
            .collect::<Vec<_>>();
        slowest.sort_by(|a, b| b.ms.cmp(&a.ms).then_with(|| a.test.cmp(&b.test)));
        slowest.truncate(SLOWEST);
        BuildReport {
            log: written.map(|_| self.name),
            failed_tests: self.failed_tests,
            slowest_tests: slowest,
            errors: self.errors,
        }
    }
}

#[derive(Deserialize)]
struct CompilerMessage<'a> {
    reason: &'a str,
    message: Diagnostic,
}

#[derive(Deserialize)]
struct Diagnostic {
    level: String,
    message: String,
    #[serde(default)]
    spans: Vec<Span>,
    rendered: Option<String>,
}

#[derive(Deserialize)]
struct Span {
    file_name: String,
    line_start: u64,
    is_primary: bool,
}

/// The test of libtest's `test some::path ... FAILED`.
fn libtest_failure(line: &str) -> Option<String> {
    let test = line.strip_prefix("test ")?.strip_suffix(" ... FAILED")?;
    Some(test.to_owned())
}

/// Whether the test of a nextest status line passed, how long it took and
/// which it was: `    FAIL [   0.123s] ( 12/400) crate::bin test::name`.
fn nextest_status(line: &str) -> Option<(bool, u64, &str)> {
    let (status, rest) = line.trim_start().split_once(" [")?;
    // A retried test's status is the last word: `TRY 2 FAIL`.
    let passed = match status.rsplit(' ').next()? {
        "PASS" | "LEAK" => true,
        "FAIL" | "TIMEOUT" | "ABORT" | "LEAK-FAIL" => false,
        signal if signal.starts_with("SIG") => false,
        _ => return None,
    };
    let (seconds, rest) = rest.split_once("s] ")?;
    let seconds = seconds.trim().parse::<f64>().ok()?;
    let test = match rest.strip_prefix('(') {
        Some(counted) => counted.split_once(") ")?.1,
        None => rest,
    };
    #[expect(
        clippy::cast_possible_truncation,
        clippy::cast_sign_loss,
        reason = "a test's duration in whole milliseconds"
    )]
    let ms = (seconds * 1000.0).round() as u64;
    Some((passed, ms, test.trim()))
}

/// Removes the oldest logs in `directory` until all of them fit `limit`
/// bytes. Names start with the build's start time, so they sort by age.
pub(crate) fn prune(directory: &Path, limit: u64) {
    let Ok(entries) = std::fs::read_dir(directory) else {
        return;
    };
    let mut logs = entries
        .filter_map(|entry| {
            let entry = entry.ok()?;
            let name = entry.file_name().into_string().ok()?;
            let size = entry.metadata().ok()?.len();
            name.ends_with(".log").then_some((name, size))
        })
        .collect::<Vec<_>>();
    logs.sort();
    let mut total = logs.iter().map(|(_, size)| size).sum::<u64>();
    for (name, size) in logs {
        if total <= limit {
            break;
        }
        match std::fs::remove_file(directory.join(&name)) {
            Ok(()) => total -= size,
            // Another build's pruning got there first.
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => total -= size,
            Err(error) => log!("could not remove the build log {name}: {error}"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::snapshot::tests::TempDir;

    #[test]
    fn a_log_keeps_the_output_and_reports_failed_and_slow_tests_and_errors() {
        let home = TempDir::new();
        let mut log = BuildLog::create(&home.0, "1-2.log".into(), "# header\n");
        let error = serde_json::json!({
            "reason": "compiler-message",
            "message": {
                "level": "error",
                "message": "mismatched types",
                "spans": [{"file_name": "src/lib.rs", "line_start": 7, "is_primary": true}],
                "rendered": "error[E0308]: mismatched types\n --> src/lib.rs:7:5\n",
            },
        });
        log.stdout(&error.to_string());
        log.stdout(r#"{"reason":"compiler-message","message":{"level":"error","message":"aborting due to 1 previous error","spans":[],"rendered":"error: aborting\n"}}"#);
        log.stderr("error: could not compile `app` (lib) due to 1 previous error");
        log.stdout("test parse::rejects_empty ... FAILED");
        log.stdout("test parse::accepts ... ok");
        log.stderr("        PASS [   0.250s] ( 1/3) app::edge quick");
        log.stderr("        FAIL [  12.500s] ( 2/3) app::edge slow");
        log.stderr("        SLOW [> 60.000s] (─────) app::edge slower");
        log.stderr("     SIGSEGV [   1.000s] ( 3/3) app crashes");
        // The summary repeats failures.
        log.stderr("        FAIL [  12.500s] ( 2/3) app::edge slow");
        log.stderr("error: test run failed");
        let report = log.finish();
        assert_eq!(report.log.as_deref(), Some("1-2.log"));
        assert_eq!(
            report.failed_tests,
            ["parse::rejects_empty", "app::edge slow", "app crashes"]
        );
        assert_eq!(
            report.slowest_tests,
            [
                TestTime {
                    test: "app::edge slow".into(),
                    ms: 12_500
                },
                TestTime {
                    test: "app crashes".into(),
                    ms: 1000
                },
                TestTime {
                    test: "app::edge quick".into(),
                    ms: 250
                },
            ]
        );
        assert_eq!(
            report.errors,
            [
                "mismatched types (src/lib.rs:7)",
                "could not compile `app` (lib) due to 1 previous error"
            ]
        );
        let text = std::fs::read_to_string(home.0.join("1-2.log")).unwrap();
        assert!(
            text.starts_with("# header\nerror[E0308]: mismatched types\n --> src/lib.rs:7:5\n"),
            "{text}"
        );
        assert!(
            text.contains("test parse::rejects_empty ... FAILED\n"),
            "{text}"
        );
    }

    #[test]
    fn the_oldest_logs_go_first_until_the_rest_fit() {
        let home = TempDir::new();
        for (name, size) in [("1-1.log", 10), ("2-1.log", 10), ("3-1.log", 10)] {
            std::fs::write(home.0.join(name), vec![b'x'; size]).unwrap();
        }
        std::fs::write(home.0.join("notes.txt"), "kept").unwrap();
        prune(&home.0, 20);
        let mut left = std::fs::read_dir(&home.0)
            .unwrap()
            .map(|entry| entry.unwrap().file_name().into_string().unwrap())
            .collect::<Vec<_>>();
        left.sort();
        assert_eq!(left, ["2-1.log", "3-1.log", "notes.txt"]);
    }
}
