//! A real daemon on Windows: real git repositories, Cargo, jobs and sockets.
#![cfg(windows)]

use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

struct Directory(PathBuf);

impl Directory {
    fn new(name: &str) -> Self {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let path = std::env::temp_dir().join(format!(
            "buildd-{name}-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir_all(&path).unwrap();
        Self(path)
    }
}

impl Drop for Directory {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

struct Daemon {
    home: Directory,
    process: Child,
}

impl Daemon {
    fn start() -> Self {
        let home = Directory::new("home");
        std::fs::write(
            home.0.join("config.toml"),
            "slots = 1\njobs = 4\nmin_free_gib = 0\n",
        )
        .unwrap();
        let process = Command::new(env!("CARGO_BIN_EXE_buildd"))
            .arg("daemon")
            .env("BUILDD_HOME", &home.0)
            .stdin(Stdio::null())
            .spawn()
            .unwrap();
        let until = Instant::now() + Duration::from_secs(20);
        while buildd::client::connect(&home.0).is_err() {
            assert!(Instant::now() < until, "the daemon listens");
            std::thread::sleep(Duration::from_millis(20));
        }
        Self { home, process }
    }

    fn buildd(&self, directory: &Path, args: &[&str]) -> Output {
        Command::new(env!("CARGO_BIN_EXE_buildd"))
            .args(args)
            .current_dir(directory)
            .env("BUILDD_HOME", &self.home.0)
            .output()
            .unwrap()
    }
}

impl Drop for Daemon {
    fn drop(&mut self) {
        let _ = self.process.kill();
        let _ = self.process.wait();
    }
}

fn git(directory: &Path, args: &[&str]) {
    let status = Command::new("git")
        .args(args)
        .current_dir(directory)
        .stdout(Stdio::null())
        .status()
        .unwrap();
    assert!(status.success(), "git {args:?}");
}

fn crate_repository() -> Directory {
    let repository = Directory::new("repository");
    std::fs::create_dir(repository.0.join("src")).unwrap();
    std::fs::write(
        repository.0.join("Cargo.toml"),
        "[package]\nname = \"tiny\"\nversion = \"0.1.0\"\nedition = \"2024\"\n",
    )
    .unwrap();
    std::fs::write(
        repository.0.join("src/lib.rs"),
        "pub fn answer() -> u32 { 42 }\n",
    )
    .unwrap();
    git(&repository.0, &["init", "--quiet"]);
    git(&repository.0, &["add", "."]);
    git(
        &repository.0,
        &[
            "-c",
            "user.name=test",
            "-c",
            "user.email=test@example.com",
            "commit",
            "--quiet",
            "-m",
            "first",
        ],
    );
    repository
}

fn text(output: &Output) -> String {
    format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    )
}

#[test]
fn a_crate_builds_in_a_slot_and_a_failure_keeps_its_diagnostics() {
    let daemon = Daemon::start();
    let repository = crate_repository();
    let built = daemon.buildd(&repository.0, &["check"]);
    assert!(built.status.success(), "{}", text(&built));

    std::fs::write(
        repository.0.join("src/lib.rs"),
        "pub fn answer() -> u32 { \"no\" }\n",
    )
    .unwrap();
    let failed = daemon.buildd(&repository.0, &["check"]);
    assert!(!failed.status.success(), "{}", text(&failed));
    assert!(text(&failed).contains("E0308"), "{}", text(&failed));

    std::fs::write(repository.0.join("src/lib.rs"), "#[test]\nfn passes() {}\n").unwrap();
    let tested = daemon.buildd(&repository.0, &["test"]);
    assert!(tested.status.success(), "{}", text(&tested));
    assert!(text(&tested).contains("1 passed"), "{}", text(&tested));
}

/// As `buildd serve` finds things on a machine nobody has built on yet, then
/// after the daemon died and left its socket behind.
#[test]
fn a_client_starts_the_daemon_when_none_runs_and_it_outlives_the_client() {
    let parent = Directory::new("fresh");
    let home = parent.0.join("home");
    let status = |home: &Path| {
        Command::new(env!("CARGO_BIN_EXE_buildd"))
            .arg("status")
            .env("BUILDD_HOME", home)
            .output()
            .unwrap()
    };
    let started = status(&home);
    assert!(started.status.success(), "{}", text(&started));
    let again = status(&home);
    assert!(again.status.success(), "{}", text(&again));
    stop_detached_daemons();
    std::thread::sleep(Duration::from_millis(200));
    let restarted = status(&home);
    assert!(restarted.status.success(), "{}", text(&restarted));
    stop_detached_daemons();
}

/// Stops the daemons of this test binary's build that a client started: those
/// other tests start are this process's children.
fn stop_detached_daemons() {
    let script = "Get-CimInstance Win32_Process -Filter \"Name='buildd.exe'\" | \
        Where-Object { $_.ExecutablePath -eq $env:BUILDD_TEST_EXE -and \
            $_.CommandLine -match 'daemon' -and \
            $_.ParentProcessId -ne [int]$env:BUILDD_TEST_PARENT } | \
        ForEach-Object { Stop-Process -Id $_.ProcessId -Force }";
    let stopped = Command::new("powershell")
        .args(["-NoProfile", "-Command", script])
        .env("BUILDD_TEST_EXE", env!("CARGO_BIN_EXE_buildd"))
        .env("BUILDD_TEST_PARENT", std::process::id().to_string())
        .status()
        .unwrap();
    assert!(stopped.success());
}

#[test]
fn a_command_runs_in_the_checkout_and_its_exit_code_comes_back() {
    let daemon = Daemon::start();
    let repository = crate_repository();
    let ran = daemon.buildd(
        &repository.0,
        &["run", "--", "cmd", "/d", "/c", "echo %BUILDD_JOBS%& exit 3"],
    );
    assert_eq!(ran.status.code(), Some(3), "{}", text(&ran));
}

#[test]
fn a_withdrawn_build_stops_and_frees_its_slot() {
    let daemon = Daemon::start();
    let repository = crate_repository();
    let mut client = Command::new(env!("CARGO_BIN_EXE_buildd"))
        .args(["run", "--", "ping", "-n", "120", "127.0.0.1"])
        .current_dir(&repository.0)
        .env("BUILDD_HOME", &daemon.home.0)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let running = |daemon: &Daemon| {
        let status = buildd::client::status(buildd::client::connect(&daemon.home.0).unwrap());
        status
            .unwrap()
            .slots
            .iter()
            .any(|slot| slot.build.is_some())
    };
    let until = Instant::now() + Duration::from_secs(30);
    while !running(&daemon) {
        assert!(Instant::now() < until, "the command starts");
        std::thread::sleep(Duration::from_millis(20));
    }
    client.kill().unwrap();
    client.wait().unwrap();
    let until = Instant::now() + Duration::from_secs(10);
    while running(&daemon) {
        assert!(Instant::now() < until, "the withdrawn command stops");
        std::thread::sleep(Duration::from_millis(20));
    }
}
