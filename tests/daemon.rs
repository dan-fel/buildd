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
    /// A daemon with no floor of free disk: the machine's own free disk must
    /// not decide what these tests see.
    fn start(slots: usize) -> Self {
        Self::configured(&format!("slots = {slots}\njobs = 4\nmin_free_gib = 0\n"))
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
            rustflags: Vec::new(),
        },
        label: None,
        copy_to: None,
        rerun_all: false,
        optional: false,
        os: None,
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
fn executables_are_copied_out_and_rustflags_reach_the_compiler() {
    let daemon = Daemon::start(1);
    let repository = crate_repository(None);
    std::fs::create_dir(repository.0.join("src/bin")).unwrap();
    // The binary compiles only with the flag the request passes.
    std::fs::write(
        repository.0.join("src/bin/tool.rs"),
        "#[cfg(not(buildd_flag))]\ncompile_error!(\"no flag\");\nfn main() {}\n",
    )
    .unwrap();
    let out = TempDir::new();
    let mut request = request(&repository.0, Command::Build, &["--bins"]);
    request.copy_to = Some(out.0.join("bin"));
    let build = |request: BuildRequest| {
        let mut messages = Vec::new();
        client::build(daemon.connect(), request, |message| {
            messages.push(message.clone());
        })
        .unwrap();
        messages
    };

    let refused = build(request.clone());
    assert_eq!(outcome(&refused), &Outcome::Exited { code: 101 });
    assert!(!out.0.join("bin").exists(), "a failed build copies nothing");

    request.operation.rustflags = vec!["--cfg".into(), "buildd_flag".into()];
    let built = build(request);
    assert!(outcome(&built).success(), "{built:#?}");
    let copy = out.0.join("bin/tool");
    let artifacts = built
        .iter()
        .filter_map(|message| match message {
            Message::Copied { artifact } => {
                Some(serde_json::from_str::<serde_json::Value>(artifact).unwrap())
            }
            _ => None,
        })
        .collect::<Vec<_>>();
    let [artifact] = &artifacts[..] else {
        panic!("one executable: {built:#?}");
    };
    assert_eq!(artifact["executable"], copy.to_str().unwrap());
    assert_eq!(artifact["target"]["name"], "tool");
    assert!(
        std::process::Command::new(&copy)
            .status()
            .unwrap()
            .success(),
        "the copy runs"
    );
}

#[test]
fn nextest_runs_the_tests_test_compiles_and_reports_the_phase() {
    let daemon = Daemon::start(1);
    let repository = crate_repository(None);
    std::fs::write(
        repository.0.join("src/lib.rs"),
        "pub fn answer() -> u32 {\n    42\n}\n\n#[test]\nfn answers() {\n    assert_eq!(answer(), 42);\n}\n",
    )
    .unwrap();
    let compiled = daemon.build(&repository.0, Command::Test, &["--no-run"]);
    assert!(outcome(&compiled).success(), "{compiled:#?}");
    let run = daemon.build(&repository.0, Command::Nextest, &[]);
    assert!(outcome(&run).success(), "{run:#?}");
    assert!(
        run.iter().any(|message| matches!(
            message,
            Message::Stderr { line } | Message::Stdout { line } if line.contains("answers")
        )),
        "{run:#?}"
    );
    assert!(
        matches!(
            run.last(),
            Some(Message::Finished {
                test_ms: Some(_),
                ..
            })
        ),
        "{run:#?}"
    );
    let activity = daemon.activity();
    let started = activity
        .events
        .iter()
        .filter_map(|event| match &event.kind {
            EventKind::Started { warm, .. } => Some(*warm),
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(
        started,
        [false, true],
        "nextest reuses what test --no-run compiled"
    );
}

#[test]
fn every_build_leaves_a_log_and_reports_its_errors_and_failed_tests() {
    let daemon = Daemon::start(1);
    let repository = crate_repository(None);
    let reports = || {
        daemon
            .activity()
            .events
            .into_iter()
            .filter_map(|event| match event.kind {
                EventKind::Finished { report, .. } => Some(report),
                _ => None,
            })
            .collect::<Vec<_>>()
    };
    std::fs::write(
        repository.0.join("src/lib.rs"),
        "pub fn answer() -> u32 {\n    \"forty-two\"\n}\n",
    )
    .unwrap();
    daemon.build(&repository.0, Command::Check, &[]);
    let broken = reports().pop().unwrap();
    assert_eq!(
        broken.errors.first().map(String::as_str),
        Some("mismatched types (src/lib.rs:2)"),
        "{broken:#?}"
    );
    let log = std::fs::read_to_string(
        daemon
            .home
            .0
            .join("logs")
            .join(broken.log.as_deref().unwrap()),
    )
    .unwrap();
    assert!(log.starts_with("# buildd check for "), "{log}");
    assert!(log.contains("error[E0308]: mismatched types"), "{log}");

    std::fs::write(
        repository.0.join("src/lib.rs"),
        "pub fn answer() -> u32 {\n    42\n}\n\n#[test]\nfn answers() {\n    assert_eq!(answer(), 41);\n}\n",
    )
    .unwrap();
    daemon.build(&repository.0, Command::Test, &[]);
    daemon.build(&repository.0, Command::Nextest, &[]);
    let reports = reports();
    let [.., test, nextest] = reports.as_slice() else {
        panic!("three builds: {reports:#?}");
    };
    assert_eq!(test.failed_tests, ["answers"], "{test:#?}");
    assert_eq!(nextest.failed_tests, ["fixture answers"], "{nextest:#?}");
    assert_eq!(nextest.slowest_tests[0].test, "fixture answers");

    let failures = buildd::client::failures(daemon.connect(), 1, None).unwrap();
    assert_eq!((failures.builds, failures.failed), (3, 3), "{failures:#?}");
    assert_eq!(failures.recent[0].report.failed_tests, ["fixture answers"]);
    assert_eq!(failures.logs, daemon.home.0.join("logs"));
}

#[test]
fn a_configured_branch_is_built_before_anyone_asks() {
    let repository = crate_repository(None);
    let daemon = Daemon::configured(&format!(
        "slots = 2\njobs = 4\nmin_free_gib = 0\n\
         [[prewarm]]\nrepository = {:?}\nbranch = \"main\"\nbuilds = [[\"check\"]]\n",
        repository.0.display().to_string()
    ));
    let head = git(&repository.0, &["rev-parse", "HEAD^{tree}"]);
    let finished = || {
        daemon
            .activity()
            .events
            .into_iter()
            .find_map(|event| match event.kind {
                EventKind::Finished {
                    who,
                    revision,
                    outcome,
                    ..
                } if who == ["prewarm main"] => Some((revision, outcome)),
                _ => None,
            })
    };
    wait_until("the prewarm build finishes", || finished().is_some());
    let (revision, outcome) = finished().unwrap();
    assert_eq!(revision.to_string(), head.trim(), "the branch's commit");
    assert!(outcome.success(), "{outcome:?}");
    // The worktree it built from is buildd's own, at that commit.
    let prewarmed = std::fs::read_dir(daemon.home.0.join("prewarm"))
        .unwrap()
        .next()
        .unwrap()
        .unwrap()
        .path();
    assert_eq!(
        git(&prewarmed, &["rev-parse", "HEAD^{tree}"]).trim(),
        head.trim()
    );
}

/// A committed workspace of two independent packages, `a` and `b`, each
/// with a unit test.
fn two_package_workspace() -> TempDir {
    let directory = TempDir::new();
    std::fs::write(
        directory.0.join("Cargo.toml"),
        "[workspace]\nmembers = [\"a\", \"b\"]\nresolver = \"2\"\n",
    )
    .unwrap();
    for name in ["a", "b"] {
        std::fs::create_dir_all(directory.0.join(name).join("src")).unwrap();
        std::fs::write(
            directory.0.join(name).join("Cargo.toml"),
            format!("[package]\nname = \"{name}\"\nversion = \"0.1.0\"\nedition = \"2024\"\n"),
        )
        .unwrap();
        std::fs::write(
            directory.0.join(name).join("src/lib.rs"),
            "#[test]\nfn works() {}\n",
        )
        .unwrap();
    }
    std::fs::write(directory.0.join(".gitignore"), "/target/\n").unwrap();
    git(&directory.0, &["init", "-q", "-b", "main"]);
    git(&directory.0, &["add", "--all"]);
    git(&directory.0, &["commit", "-q", "-m", "workspace"]);
    directory
}

#[test]
fn test_binaries_that_passed_unchanged_are_skipped_until_their_package_changes() {
    let daemon = Daemon::start(1);
    let repository = two_package_workspace();
    let skipped = |messages: &[Message]| {
        messages.iter().find_map(|message| match message {
            Message::Stderr { line } if line.contains("unchanged since they passed") => {
                Some(line.split(' ').nth(1).unwrap().to_owned())
            }
            _ => None,
        })
    };
    let first = daemon.build(&repository.0, Command::Nextest, &["--workspace"]);
    assert!(outcome(&first).success(), "{first:#?}");
    assert_eq!(skipped(&first), None, "nothing passed before");
    // The slot is measured once quiet, which reads the workspace's packages.
    wait_until("the workspace is known", || {
        daemon.status().slots[0].size.is_some()
    });
    let second = daemon.build(&repository.0, Command::Nextest, &["--workspace"]);
    assert!(outcome(&second).success(), "{second:#?}");
    assert_eq!(skipped(&second).as_deref(), Some("2"), "{second:#?}");

    std::fs::write(
        repository.0.join("b/src/lib.rs"),
        "#[test]\nfn works() {}\n\n#[test]\nfn also() {}\n",
    )
    .unwrap();
    let third = daemon.build(&repository.0, Command::Nextest, &["--workspace"]);
    assert!(outcome(&third).success(), "{third:#?}");
    assert_eq!(
        skipped(&third).as_deref(),
        Some("1"),
        "a stays skipped: {third:#?}"
    );

    let mut request = request(&repository.0, Command::Nextest, &["--workspace"]);
    request.rerun_all = true;
    let mut all = Vec::new();
    client::build(daemon.connect(), request, |message| {
        all.push(message.clone())
    })
    .unwrap();
    assert!(outcome(&all).success(), "{all:#?}");
    assert_eq!(skipped(&all), None, "--rerun-all runs everything");
    assert_eq!(daemon.activity().totals.skipped, 3);
}

#[test]
fn a_tree_pushed_into_a_mirror_builds_like_a_worktree_and_drain_stops_new_builds() {
    use buildd::protocol::{Mirror, Request, RevisionRequest};
    use std::io::{BufRead as _, BufReader, Write as _};

    let daemon = Daemon::start(1);
    let ask = |request: &Request| {
        let mut stream = daemon.connect();
        let mut text = serde_json::to_string(request).unwrap();
        text.push('\n');
        stream.write_all(text.as_bytes()).unwrap();
        BufReader::new(stream)
            .lines()
            .map(Result::unwrap)
            .collect::<Vec<_>>()
    };
    let rejected = ask(&Request::Mirror {
        project: "../escape".into(),
    });
    assert!(rejected[0].contains("not a project name"), "{rejected:?}");
    let answer = ask(&Request::Mirror {
        project: "fixture-1".into(),
    });
    let mirror: Mirror = serde_json::from_str(&answer[0]).unwrap();

    // Another machine's daemon pushes a commit of the tree it built.
    let repository = crate_repository(None);
    let tree = git(&repository.0, &["rev-parse", "HEAD^{tree}"])
        .trim()
        .to_owned();
    git(
        &repository.0,
        &[
            "push",
            "-q",
            mirror.path.to_str().unwrap(),
            &format!("HEAD:refs/buildd/{tree}"),
        ],
    );
    let build = |tree: &str| {
        let request = Request::BuildRevision(RevisionRequest {
            project: "fixture-1".into(),
            revision: serde_json::from_value(serde_json::Value::String(tree.into())).unwrap(),
            worktree: "/elsewhere/worktree".into(),
            prefix: "".into(),
            operation: Operation {
                command: Command::Check,
                args: Vec::new(),
                rustflags: Vec::new(),
            },
            label: Some("remote".into()),
            rerun_all: false,
            optional: false,
        });
        ask(&request)
            .iter()
            .map(|line| serde_json::from_str::<Message>(line).unwrap())
            .collect::<Vec<_>>()
    };
    let built = build(&tree);
    assert!(outcome(&built).success(), "{built:#?}");
    assert_eq!(started(&built), tree);
    let unknown = build(&"0".repeat(40));
    assert!(
        matches!(&unknown[..], [Message::Rejected { reason }] if reason.contains("was not pushed")),
        "{unknown:#?}"
    );

    let drained = client::drain(daemon.connect(), true).unwrap();
    assert!(drained.draining);
    let refused = build(&tree);
    assert!(
        matches!(&refused[..], [Message::Rejected { reason }] if reason.contains("draining")),
        "{refused:#?}"
    );
    assert!(!client::drain(daemon.connect(), false).unwrap().draining);
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
    // The build script takes two job tokens and keeps them, records its
    // pid, and sleeps while `hold` exists.
    let script = format!(
        "use std::io::Read as _;\n\
         use std::os::fd::FromRawFd as _;\n\
         fn main() {{\n    \
             let flags = std::env::var(\"CARGO_MAKEFLAGS\").unwrap();\n    \
             let auth = flags.split(' ').find_map(|flag| flag.strip_prefix(\"--jobserver-auth=\")).unwrap();\n    \
             let read: i32 = auth.split(',').next().unwrap().parse().unwrap();\n    \
             let mut jobs = unsafe {{ std::fs::File::from_raw_fd(read) }};\n    \
             jobs.read_exact(&mut [0; 2]).unwrap();\n    \
             std::mem::forget(jobs);\n    \
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
    let status = daemon.status();
    assert!(
        status.idle_jobs + 3 <= status.jobs,
        "Cargo's job and the script's two are charged: {status:?}"
    );

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
    // The killed script never gave its tokens back; the budget has them.
    wait_until("every job is free again", || {
        let status = daemon.status();
        status.idle_jobs == status.jobs
    });

    // The slot builds again.
    std::fs::remove_file(&hold).unwrap();
    let next = daemon.build(&repository.0, Command::Check, &[]);
    assert!(outcome(&next).success(), "{next:#?}");
}

#[test]
fn below_the_disk_floor_idle_slots_give_up_what_is_not_in_use() {
    // No volume has this much free: every build ends below the floor.
    let daemon = Daemon::configured("slots = 1\njobs = 4\nmin_free_gib = 1000000\n");
    let repository = crate_repository(None);
    let built = daemon.build(&repository.0, Command::Check, &[]);
    assert!(outcome(&built).success(), "{built:#?}");
    wait_until("the idle slot gave up what it could", || {
        daemon.activity().events.iter().any(|event| {
            matches!(
                event.kind,
                // Everything was used minutes ago: nothing goes.
                EventKind::Reclaimed {
                    freed: 0,
                    units: 0,
                    ..
                }
            )
        })
    });
    let status = daemon.status();
    assert!(status.free_disk.is_some_and(|free| free < status.min_free));
    // The slot is handed back and builds again.
    let again = daemon.build(&repository.0, Command::Check, &[]);
    assert!(outcome(&again).success(), "{again:#?}");
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
    let daemon =
        Daemon::configured("slots = 1\njobs = 4\nslot_limit_gib = 0.00001\nmin_free_gib = 0\n");
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
