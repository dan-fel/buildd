//! The daemon end to end: a real daemon process, git repositories and Cargo.

use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::{Child, Command as Process};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use buildd::cargo::{Command, Operation};
use buildd::client;
use buildd::protocol::{Activity, BuildRequest, EventKind, Message, Outcome, Status};

/// A temporary directory, removed when dropped. Its path is short: socket
/// paths are limited to about a hundred bytes.
struct TempDir(PathBuf);

impl TempDir {
    fn new() -> Self {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let path = std::env::temp_dir().join(format!(
            "bd-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir_all(&path).unwrap();
        Self(std::fs::canonicalize(&path).unwrap())
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// A daemon process with its own home, killed when dropped.
struct Daemon {
    home: TempDir,
    process: Child,
}

impl Daemon {
    fn start(slots: usize) -> Self {
        Self::configured(&format!("slots = {slots}\njobs = 4\n"))
    }

    fn configured(config: &str) -> Self {
        let home = TempDir::new();
        std::fs::write(home.0.join("config.toml"), config).unwrap();
        let process = Process::new(env!("CARGO_BIN_EXE_buildd"))
            .arg("daemon")
            .env("BUILDD_HOME", &home.0)
            .spawn()
            .unwrap();
        let daemon = Self { home, process };
        let deadline = Instant::now() + Duration::from_secs(10);
        while client::connect(&daemon.home.0).is_err() {
            assert!(Instant::now() < deadline, "the daemon listens");
            std::thread::sleep(Duration::from_millis(20));
        }
        daemon
    }

    fn connect(&self) -> UnixStream {
        client::connect(&self.home.0).unwrap()
    }

    fn status(&self) -> Status {
        client::status(self.connect()).unwrap()
    }

    fn activity(&self) -> Activity {
        client::activity(self.connect()).unwrap()
    }

    /// Builds and collects every message.
    fn build(&self, directory: &Path, command: Command, args: &[&str]) -> Vec<Message> {
        let mut messages = Vec::new();
        client::build(
            self.connect(),
            request(directory, command, args),
            |message| {
                messages.push(message.clone());
            },
        )
        .unwrap();
        messages
    }
}

impl Drop for Daemon {
    fn drop(&mut self) {
        let _ = self.process.kill();
        let _ = self.process.wait();
    }
}

fn request(directory: &Path, command: Command, args: &[&str]) -> BuildRequest {
    BuildRequest {
        directory: directory.to_owned(),
        operation: Operation {
            command,
            args: args.iter().map(|argument| (*argument).to_owned()).collect(),
        },
        label: None,
    }
}

fn git(directory: &Path, arguments: &[&str]) -> String {
    let output = Process::new("git")
        .current_dir(directory)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .args(["-c", "user.name=t", "-c", "user.email=t@t"])
        .args(arguments)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout).unwrap()
}

/// A committed crate named `fixture`, with `build_script` as its build.rs.
fn crate_repository(build_script: Option<&str>) -> TempDir {
    let directory = TempDir::new();
    std::fs::create_dir(directory.0.join("src")).unwrap();
    std::fs::write(
        directory.0.join("Cargo.toml"),
        "[package]\nname = \"fixture\"\nversion = \"0.1.0\"\nedition = \"2024\"\n\n[workspace]\n",
    )
    .unwrap();
    std::fs::write(
        directory.0.join("src/lib.rs"),
        "pub fn answer() -> u32 {\n    42\n}\n",
    )
    .unwrap();
    std::fs::write(directory.0.join(".gitignore"), "/target/\n").unwrap();
    if let Some(script) = build_script {
        std::fs::write(directory.0.join("build.rs"), script).unwrap();
    }
    git(&directory.0, &["init", "-q", "-b", "main"]);
    git(&directory.0, &["add", "--all"]);
    git(&directory.0, &["commit", "-q", "-m", "fixture"]);
    directory
}

fn outcome(messages: &[Message]) -> &Outcome {
    match messages.last() {
        Some(Message::Finished { outcome, .. }) => outcome,
        other => panic!("the last message is Finished: {other:?}"),
    }
}

fn started(messages: &[Message]) -> String {
    messages
        .iter()
        .find_map(|message| match message {
            Message::Started { revision, .. } => Some(revision.to_string()),
            _ => None,
        })
        .expect("the build started")
}

fn wait_until(what: &str, mut done: impl FnMut() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(20);
    while !done() {
        assert!(Instant::now() < deadline, "timed out waiting until {what}");
        std::thread::sleep(Duration::from_millis(50));
    }
}

#[test]
fn a_build_compiles_the_worktrees_uncommitted_content_outside_the_worktree() {
    let daemon = Daemon::start(1);
    let repository = crate_repository(None);
    std::fs::write(
        repository.0.join("src/lib.rs"),
        "pub fn answer() -> u32 {\n    \"forty-two\"\n}\n",
    )
    .unwrap();
    let broken = daemon.build(&repository.0, Command::Check, &[]);
    assert_eq!(outcome(&broken), &Outcome::Exited { code: 101 });
    let diagnostics = broken
        .iter()
        .filter_map(|message| match message {
            Message::Stdout { line } if line.contains("compiler-message") => Some(line.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>();
    assert!(
        diagnostics
            .iter()
            .any(|line| line.contains("mismatched types") && line.contains("src/lib.rs")),
        "{diagnostics:#?}"
    );
    // The reported revision is the tree of what was on disk.
    git(&repository.0, &["add", "--all"]);
    assert_eq!(started(&broken), git(&repository.0, &["write-tree"]).trim());

    std::fs::write(
        repository.0.join("src/lib.rs"),
        "pub fn answer() -> u32 {\n    7\n}\n",
    )
    .unwrap();
    let fixed = daemon.build(&repository.0, Command::Test, &[]);
    assert!(outcome(&fixed).success(), "{fixed:#?}");
    assert!(
        fixed.iter().any(|message| matches!(message, Message::Stdout { line } if line.contains("test result: ok"))),
        "{fixed:#?}"
    );
    assert!(
        matches!(
            fixed.last(),
            Some(Message::Finished {
                test_ms: Some(_),
                ..
            })
        ),
        "a test build reports how long its tests ran: {fixed:#?}"
    );
    assert!(
        !repository.0.join("target").exists(),
        "nothing builds in the worktree"
    );
    let status = daemon.status();
    assert_eq!(status.slots.len(), 1);
    assert_eq!(
        status.slots[0].worktree.as_deref(),
        Some(repository.0.as_path())
    );
    // Every event also went to the event file, for hindsight.
    let events = std::fs::read_to_string(daemon.home.0.join("events.jsonl")).unwrap();
    assert_eq!(
        events.lines().count(),
        daemon.activity().events.len(),
        "{events}"
    );
}

#[test]
fn equal_requests_share_one_cargo_run() {
    let daemon = Daemon::start(2);
    let marks = TempDir::new();
    let script = format!(
        "fn main() {{\n    \
             let path = {path:?};\n    \
             let mut text = std::fs::read_to_string(path).unwrap_or_default();\n    \
             text.push_str(\"ran\\n\");\n    \
             std::fs::write(path, text).unwrap();\n    \
             std::thread::sleep(std::time::Duration::from_secs(2));\n\
         }}\n",
        path = marks.0.join("runs").display().to_string()
    );
    let repository = crate_repository(Some(&script));

    let (first_started, first) = std::sync::mpsc::channel();
    let directory = repository.0.clone();
    let stream = daemon.connect();
    let first_build = std::thread::spawn(move || {
        let mut messages = Vec::new();
        client::build(
            stream,
            request(&directory, Command::Check, &[]),
            |message| {
                if matches!(message, Message::Started { .. }) {
                    first_started.send(()).unwrap();
                }
                messages.push(message.clone());
            },
        )
        .unwrap();
        messages
    });
    first.recv().unwrap();
    let second = daemon.build(&repository.0, Command::Check, &[]);
    let first = first_build.join().unwrap();

    assert!(outcome(&first).success(), "{first:#?}");
    assert!(outcome(&second).success(), "{second:#?}");
    assert_eq!(started(&first), started(&second));
    assert_eq!(
        std::fs::read_to_string(marks.0.join("runs")).unwrap(),
        "ran\n"
    );
    // The second request never needed a second slot.
    assert_eq!(daemon.status().slots.len(), 1);

    // The daemon tells people watching what it did.
    let activity = daemon.activity();
    let totals = &activity.totals;
    assert_eq!((totals.requests, totals.shared, totals.builds), (2, 1, 1));
    assert_eq!((totals.worktrees, totals.first_builds), (1, 1));
    assert!(
        totals.compiled >= 2,
        "the build script and the library: {totals:?}"
    );
    assert!(totals.cpu_ms > 0, "{totals:?}");
    let finished = activity.events.iter().find_map(|event| match &event.kind {
        EventKind::Finished { who, usage, .. } => Some((who.len(), usage.is_some())),
        _ => None,
    });
    assert_eq!(finished, Some((2, true)));
}

#[test]
fn a_build_nobody_waits_for_is_stopped_with_everything_it_started() {
    let daemon = Daemon::start(1);
    let marks = TempDir::new();
    let hold = marks.0.join("hold");
    let pid = marks.0.join("pid");
    std::fs::write(&hold, "").unwrap();
    // The build script sleeps while `hold` exists, after recording its pid.
    let script = format!(
        "fn main() {{\n    \
             std::fs::write({pid:?}, std::process::id().to_string()).unwrap();\n    \
             if std::path::Path::new({hold:?}).exists() {{\n        \
                 std::thread::sleep(std::time::Duration::from_secs(120));\n    \
             }}\n\
         }}\n",
        pid = pid.display().to_string(),
        hold = hold.display().to_string(),
    );
    let repository = crate_repository(Some(&script));
    let stream = daemon.connect();
    let reader = stream.try_clone().unwrap();
    let directory = repository.0.clone();
    let client = std::thread::spawn(move || {
        client::build(stream, request(&directory, Command::Check, &[]), |_| {})
    });
    wait_until("the build script runs", || {
        pid.exists() && !std::fs::read_to_string(&pid).unwrap().is_empty()
    });
    let script_process =
        rustix::process::Pid::from_raw(std::fs::read_to_string(&pid).unwrap().parse().unwrap())
            .expect("a pid is positive");

    let stopping = Instant::now();
    reader.shutdown(std::net::Shutdown::Both).unwrap();
    assert!(
        client.join().unwrap().is_err(),
        "the withdrawn client hears no end"
    );
    wait_until("the slot is idle", || {
        daemon.status().slots[0].build.is_none()
    });
    wait_until("the build script is gone", || {
        rustix::process::test_kill_process(script_process).is_err()
    });
    assert!(
        stopping.elapsed() < Duration::from_secs(10),
        "{:?}",
        stopping.elapsed()
    );

    // The slot builds again.
    std::fs::remove_file(&hold).unwrap();
    let next = daemon.build(&repository.0, Command::Check, &[]);
    assert!(outcome(&next).success(), "{next:#?}");
}

#[test]
fn requests_outside_a_worktree_or_taking_over_the_slot_are_rejected() {
    let daemon = Daemon::start(1);
    let plain = TempDir::new();
    let outside = daemon.build(&plain.0, Command::Check, &[]);
    assert!(
        matches!(&outside[..], [Message::Rejected { reason }] if reason.contains("rev-parse")),
        "{outside:?}"
    );
    let repository = crate_repository(None);
    let taking_over = daemon.build(&repository.0, Command::Build, &["--target-dir", "/tmp/x"]);
    assert!(
        matches!(&taking_over[..], [Message::Rejected { reason }] if reason.contains("--target-dir")),
        "{taking_over:?}"
    );
    assert!(daemon.status().slots.is_empty());
}

#[test]
fn a_slot_over_its_disk_limit_is_pruned_once_it_is_quiet() {
    // About 10 kB, less than any build leaves behind.
    let daemon = Daemon::configured("slots = 1\njobs = 4\nslot_limit_gib = 0.00001\n");
    let repository = crate_repository(None);
    let messages = daemon.build(&repository.0, Command::Check, &[]);
    assert!(outcome(&messages).success(), "{messages:#?}");
    wait_until("the slot is measured", || {
        let slot = &daemon.status().slots[0];
        !slot.maintaining && slot.size.is_some()
    });
    let size = daemon.status().slots[0].size.expect("measured above");
    assert!(size <= 10_738, "{size}");
    let again = daemon.build(&repository.0, Command::Check, &[]);
    assert!(outcome(&again).success(), "{again:#?}");
}

#[test]
fn a_restarted_daemon_keeps_its_slots_warm_and_drops_slots_beyond_its_count() {
    let mut daemon = Daemon::start(2);
    let repository = crate_repository(None);
    let first = daemon.build(&repository.0, Command::Check, &[]);
    assert!(outcome(&first).success(), "{first:#?}");
    // A slot beyond a smaller configuration, as a larger one left it.
    let project = std::fs::read_dir(daemon.home.0.join("slots"))
        .unwrap()
        .next()
        .unwrap()
        .unwrap()
        .path();
    std::fs::create_dir(project.join("1")).unwrap();

    let _ = daemon.process.kill();
    let _ = daemon.process.wait();
    std::fs::write(daemon.home.0.join("config.toml"), "slots = 1\njobs = 4\n").unwrap();
    daemon.process = Process::new(env!("CARGO_BIN_EXE_buildd"))
        .arg("daemon")
        .env("BUILDD_HOME", &daemon.home.0)
        .spawn()
        .unwrap();
    wait_until("the restarted daemon listens", || {
        client::connect(&daemon.home.0).is_ok()
    });
    assert!(!project.join("1").exists(), "slot 1 is beyond one slot");
    let slots = daemon.status().slots;
    assert_eq!(slots.len(), 1, "slot 0 is back before any build");
    assert_eq!(slots[0].worktree.as_deref(), Some(repository.0.as_path()));

    let again = daemon.build(&repository.0, Command::Check, &[]);
    assert!(outcome(&again).success(), "{again:#?}");
    let warm = daemon
        .activity()
        .events
        .iter()
        .find_map(|event| match &event.kind {
            EventKind::Started { warm, .. } => Some(*warm),
            _ => None,
        });
    assert_eq!(warm, Some(true));
}
