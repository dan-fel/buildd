//! What a build runs: Cargo's subcommands, and how buildd runs them in a
//! slot, or `run`, a command of the client's own.

use std::fmt;
use std::path::{Path, PathBuf};
use std::process::Stdio;

use serde::{Deserialize, Serialize};

/// What a build runs: a Cargo subcommand, or a command of the client's own.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Ord, PartialOrd, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Command {
    Check,
    Clippy,
    Build,
    Test,
    /// `cargo nextest run`: the tests `test` compiles, each in a process of
    /// its own, `test_jobs` at a time.
    Nextest,
    /// Not Cargo: the program and arguments the operation's `args` name,
    /// run in the slot's checkout of the snapshot.
    Run,
}

impl Command {
    /// Every command, in the order usage lists them.
    pub const ALL: [Self; 6] = [
        Self::Check,
        Self::Clippy,
        Self::Build,
        Self::Test,
        Self::Nextest,
        Self::Run,
    ];

    /// The subcommand's name.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Check => "check",
            Self::Clippy => "clippy",
            Self::Build => "build",
            Self::Test => "test",
            Self::Nextest => "nextest",
            Self::Run => "run",
        }
    }

    /// Whether Cargo runs it, in the slot's target.
    #[must_use]
    pub fn uses_cargo(self) -> bool {
        self != Self::Run
    }

    /// Whether it runs tests once compilation finishes.
    #[must_use]
    pub fn runs_tests(self) -> bool {
        matches!(self, Self::Test | Self::Nextest)
    }

    /// The command named `name`.
    #[must_use]
    pub fn parse(name: &str) -> Option<Self> {
        Self::ALL
            .into_iter()
            .find(|command| command.as_str() == name)
    }
}

impl fmt::Display for Command {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

/// Cargo and nextest options that would take the slot's target, checkout,
/// output format or parallelism out of buildd's hands.
const RESERVED_OPTIONS: [&str; 11] = [
    "--test-threads",
    "--cargo-message-format",
    "--target-dir-remap",
    "--target-dir",
    "--manifest-path",
    "--message-format",
    "--config",
    "--artifact-dir",
    "--out-dir",
    "--lockfile-path",
    "--jobs",
];

/// What Cargo compiles for a build: its directory, command, rustc flags and
/// arguments, less those that only change what runs afterwards: for `test`, the
/// positional test name, arguments after `--` (they go to the test harness),
/// `--no-run` and `--no-fail-fast`. `nextest` compiles what `test` does, so
/// it is that compilation, less its own filters and run options. For
/// Clippy the arguments after `--` stay: they are lint settings that change
/// what Clippy checks. Builds of a compilation use the same compiled units,
/// so the scheduler learns from each build what the next one needs.
#[derive(Clone, Debug, Eq, PartialEq, Hash, Serialize, Deserialize)]
pub(crate) struct Compilation {
    pub(crate) prefix: PathBuf,
    pub(crate) command: Command,
    pub(crate) args: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub(crate) rustflags: Vec<String>,
}

impl Compilation {
    pub(crate) fn new(prefix: &Path, operation: &Operation) -> Self {
        let (command, args) = match operation.command {
            Command::Test => (Command::Test, test_compilation_args(&operation.args, false)),
            Command::Nextest => (Command::Test, test_compilation_args(&operation.args, true)),
            Command::Check | Command::Clippy | Command::Build | Command::Run => {
                (operation.command, operation.args.clone())
            }
        };
        Self {
            prefix: prefix.to_owned(),
            command,
            args,
            rustflags: operation.rustflags.clone(),
        }
    }
}

/// Strip execution-only test arguments without mistaking an option's value
/// for a test name. Unknown options keep the original selection: Cargo owns
/// its argument language, and an uncertain reuse estimate must stay distinct.
/// For `nextest`, its filters and run options go too, and its
/// `--cargo-profile` is Cargo's `--profile`.
fn test_compilation_args(args: &[String], nextest: bool) -> Vec<String> {
    let original: Vec<_> = args
        .iter()
        .take_while(|argument| *argument != "--")
        .filter(|argument| !matches!(argument.as_str(), "--no-run" | "--no-fail-fast"))
        .filter(|argument| {
            !(nextest && matches!(argument.as_str(), "--no-capture" | "--nocapture"))
        })
        .cloned()
        .collect();
    let mut compiled = Vec::new();
    let mut arguments = original.iter();
    while let Some(argument) = arguments.next() {
        if !argument.starts_with('-') {
            // The only free arguments before `--` are test name filters.
            continue;
        }
        if nextest {
            let (name, inline) = argument
                .split_once('=')
                .map_or((argument.as_str(), None), |(name, value)| {
                    (name, Some(value))
                });
            if nextest_run_value(name) {
                if inline.is_none() {
                    arguments.next();
                }
                continue;
            }
            match name {
                "--cargo-profile" => {
                    compiled.push("--profile".to_owned());
                    if let Some(value) = inline.or_else(|| arguments.next().map(String::as_str)) {
                        compiled.push(value.to_owned());
                    }
                    continue;
                }
                "--ignore-default-filter" => continue,
                _ => {}
            }
        }
        compiled.push(argument.clone());
        match argument.as_str() {
            "-p" | "--package" | "--exclude" | "--bin" | "--example" | "--test" | "--bench"
            | "-F" | "--features" | "--profile" | "--target" | "--color" => {
                if let Some(value) = arguments.next() {
                    compiled.push(value.clone());
                }
            }
            "--workspace"
            | "--all"
            | "--lib"
            | "--bins"
            | "--examples"
            | "--tests"
            | "--benches"
            | "--all-targets"
            | "--doc"
            | "--all-features"
            | "--no-default-features"
            | "-r"
            | "--release"
            | "--ignore-rust-version"
            | "--locked"
            | "--offline"
            | "--frozen"
            | "--timings"
            | "--future-incompat-report"
            | "-q"
            | "--quiet"
            | "-v"
            | "-vv"
            | "--verbose" => {}
            // These spellings contain their own value.
            _ if argument.contains('=')
                || argument.starts_with("-p") && !argument.starts_with("--")
                || argument.starts_with("-F") => {}
            _ => return original,
        }
    }
    compiled
}

/// Nextest options whose value changes execution or reporting, not Cargo's
/// compilation. Keep unknown options distinct until their meaning is known.
fn nextest_run_value(name: &str) -> bool {
    matches!(
        name,
        "-E" | "--filterset"
            | "-P"
            | "--profile"
            | "--retries"
            | "--max-fail"
            | "--run-ignored"
            | "--partition"
            | "--stress-count"
            | "--stress-duration"
            | "--success-output"
            | "--failure-output"
            | "--status-level"
            | "--final-status-level"
            | "--no-tests"
    )
}

/// A command and its arguments as the client gave them: Cargo's arguments,
/// or for `run`, the program and its arguments. Two equal operations on one
/// revision are one build.
#[derive(Clone, Debug, Eq, PartialEq, Ord, PartialOrd, Hash, Serialize, Deserialize)]
pub struct Operation {
    pub command: Command,
    pub args: Vec<String>,
    /// Flags for every rustc Cargo runs, as `RUSTFLAGS` would give them:
    /// they change what is compiled, so they are part of the compilation.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub rustflags: Vec<String>,
}

impl Operation {
    /// Checks that the arguments leave the slot's target directory,
    /// checkout, output format and parallelism to buildd. Arguments after
    /// `--` belong to rustc, Clippy or the test harness and are not checked.
    /// A `run` names a program, and has no rustc to pass flags to.
    ///
    /// # Errors
    /// Names the first reserved argument, or what a `run` lacks.
    pub fn validate(&self) -> Result<(), String> {
        if !self.command.uses_cargo() {
            if self.args.first().is_none_or(String::is_empty) {
                return Err("run needs a command to run".into());
            }
            if !self.rustflags.is_empty() {
                return Err("run compiles nothing: it takes no rustc flags".into());
            }
            return Ok(());
        }
        for argument in self.args.iter().take_while(|argument| *argument != "--") {
            let reserved = RESERVED_OPTIONS.iter().any(|option| {
                argument == option
                    || argument
                        .strip_prefix(option)
                        .is_some_and(|rest| rest.starts_with('='))
            }) || argument.starts_with("-Z")
                || argument.starts_with("-j")
                || argument == "-C";
            if reserved {
                return Err(format!(
                    "`{argument}` is not allowed: buildd chooses the target directory, \
                     checkout, output format and parallelism"
                ));
            }
        }
        Ok(())
    }
}

impl Operation {
    /// The operation that serves both `self` and `other`, when one does: they
    /// are equal, or `test` runs that differ only in `--no-fail-fast`, which
    /// the run serving both has. Its exit status answers both: a run that
    /// does not stop at the first failing test binary fails exactly when one
    /// that does would.
    #[must_use]
    pub fn merged(&self, other: &Self) -> Option<Self> {
        if self == other {
            return Some(self.clone());
        }
        if self.command != other.command || !self.command.runs_tests() {
            return None;
        }
        let (mine, my_flag) = self.without_no_fail_fast();
        let (theirs, their_flag) = other.without_no_fail_fast();
        (mine == theirs).then(|| {
            let mut merged = mine;
            if my_flag || their_flag {
                merged.args.insert(0, "--no-fail-fast".into());
            }
            merged
        })
    }

    /// Whether it is a `nextest` run of every test it compiles: no test name
    /// filters, filtersets or arguments after `--`.
    #[must_use]
    pub fn runs_every_test(&self) -> bool {
        if self.command != Command::Nextest {
            return false;
        }
        // A nextest profile other than the default may choose other tests
        // (its default filter), so its run is not the default run of every
        // test whose passes later default runs skip.
        let other_profile = |name: Option<&String>| name.is_some_and(|name| name != "default");
        let mut arguments = self.args.iter();
        while let Some(argument) = arguments.next() {
            let name = argument
                .split_once('=')
                .map_or(argument.as_str(), |(name, _)| name);
            if matches!(
                name,
                "-E" | "--filterset"
                    | "--run-ignored"
                    | "--partition"
                    | "--stress-count"
                    | "--stress-duration"
            ) {
                // A stress run must execute even previously passing binaries.
                return false;
            }
            if let Some(name) = argument.strip_prefix("--profile=")
                && name != "default"
            {
                return false;
            }
            match argument.as_str() {
                "--" | "-E" | "--filterset" | "--ignore-default-filter" => return false,
                "-P" | "--profile" if other_profile(arguments.next()) => return false,
                "-P" | "--profile" => {}
                "-p" | "--package" | "--exclude" | "--bin" | "--example" | "--test" | "--bench"
                | "-F" | "--features" | "--cargo-profile" | "--target" | "--color" => {
                    arguments.next();
                }
                _ if nextest_run_value(name) && !argument.contains('=') => {
                    arguments.next();
                }
                _ if !argument.starts_with('-') => return false,
                _ => {}
            }
        }
        true
    }

    /// The operation without `--no-fail-fast` before `--`, and whether it
    /// had it.
    fn without_no_fail_fast(&self) -> (Self, bool) {
        let separator = self
            .args
            .iter()
            .position(|argument| argument == "--")
            .unwrap_or(self.args.len());
        let (cargo, harness) = self.args.split_at(separator);
        let kept = cargo
            .iter()
            .filter(|argument| *argument != "--no-fail-fast")
            .chain(harness)
            .cloned()
            .collect::<Vec<_>>();
        let had = kept.len() != self.args.len();
        (
            Self {
                args: kept,
                ..self.clone()
            },
            had,
        )
    }
}

impl fmt::Display for Operation {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        if !self.rustflags.is_empty() {
            write!(formatter, "RUSTFLAGS='{}' ", self.rustflags.join(" "))?;
        }
        formatter.write_str(self.command.as_str())?;
        for argument in &self.args {
            write!(formatter, " {argument}")?;
        }
        Ok(())
    }
}

/// The `cargo` process for `operation` in `directory` of a slot's checkout,
/// its tests running on `test_jobs` threads or processes,
/// building into the slot's `target`. It runs in its own process group so a
/// cancellation reaches everything it started.
pub(crate) fn command(
    directory: &Path,
    target: &Path,
    operation: &Operation,
    test_jobs: usize,
) -> std::process::Command {
    use std::os::unix::process::CommandExt as _;

    let mut command = std::process::Command::new("cargo");
    command.current_dir(directory);
    match operation.command {
        Command::Nextest => command
            .args(["nextest", "run", "--cargo-message-format", "json"])
            .arg(format!("--test-threads={test_jobs}")),
        Command::Check | Command::Clippy | Command::Build | Command::Test => command
            .arg(operation.command.as_str())
            .arg("--message-format=json"),
        Command::Run => panic!("a command of the client's own runs without Cargo"),
    };
    if operation.command == Command::Test {
        command.env("RUST_TEST_THREADS", test_jobs.to_string());
    }
    command
        .args(&operation.args)
        .env("CARGO_TARGET_DIR", target)
        .env("CARGO_TERM_COLOR", "never")
        .env("CARGO_TERM_PROGRESS_WHEN", "never")
        .env_remove("CARGO_BUILD_TARGET_DIR")
        .env_remove("CARGO_BUILD_JOBS")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .process_group(0);
    if !operation.rustflags.is_empty() {
        command.env(
            "CARGO_ENCODED_RUSTFLAGS",
            operation.rustflags.join("\u{1f}"),
        );
    }
    command
}

/// A line of Cargo's standard output, as the daemon treats it.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum Line {
    /// A crate Cargo compiled, or found up to date when `fresh`, with the
    /// files it produced. Its message names paths inside the slot, which
    /// mean nothing to a client.
    Crate {
        fresh: bool,
        outputs: Vec<PathBuf>,
        /// The executable among them, for a binary or test target.
        executable: Option<PathBuf>,
    },
    /// A build script's report, with the directory it wrote; also inside the
    /// slot.
    BuildScript { out_dir: PathBuf },
    /// Cargo's compilation result. Tests can run only after success.
    /// The client gets it too.
    BuildFinished { success: bool },
    /// Diagnostics and every other line (test output): for the client.
    Forward,
}

impl Line {
    pub(crate) fn of(line: &str) -> Self {
        #[derive(Deserialize)]
        struct Message<'a> {
            reason: &'a str,
            #[serde(default)]
            fresh: bool,
            #[serde(default)]
            filenames: Vec<PathBuf>,
            executable: Option<PathBuf>,
            out_dir: Option<PathBuf>,
            success: Option<bool>,
        }
        match serde_json::from_str::<Message<'_>>(line) {
            Ok(Message {
                reason: "compiler-artifact",
                fresh,
                filenames,
                executable,
                ..
            }) => Self::Crate {
                fresh,
                outputs: filenames,
                executable,
            },
            Ok(Message {
                reason: "build-script-executed",
                out_dir: Some(out_dir),
                ..
            }) => Self::BuildScript { out_dir },
            Ok(Message {
                reason: "build-finished",
                success: Some(success),
                ..
            }) => Self::BuildFinished { success },
            _ => Self::Forward,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn operation(args: &[&str]) -> Operation {
        Operation {
            command: Command::Test,
            args: args.iter().map(|argument| (*argument).to_owned()).collect(),
            rustflags: Vec::new(),
        }
    }

    #[test]
    fn test_runs_differing_only_in_no_fail_fast_merge_into_one_that_has_it() {
        let merged = operation(&["--workspace"])
            .merged(&operation(&["--workspace", "--no-fail-fast"]))
            .unwrap();
        assert_eq!(merged.args, ["--no-fail-fast", "--workspace"]);
        assert_eq!(
            merged.merged(&operation(&["--workspace"])),
            Some(merged.clone())
        );
        // A test name, a harness argument or another command never merges.
        assert_eq!(operation(&["--workspace"]).merged(&operation(&["x"])), None);
        assert_eq!(
            operation(&["--", "--no-fail-fast"]).merged(&operation(&[])),
            None
        );
        let check = Operation {
            command: Command::Check,
            ..operation(&["--no-fail-fast"])
        };
        assert_eq!(
            check.merged(&Operation {
                command: Command::Check,
                ..operation(&[])
            }),
            None
        );
    }

    #[test]
    fn arguments_that_take_over_the_slot_are_rejected_before_the_separator_only() {
        for args in [
            &["--target-dir", "x"][..],
            &["--target-dir=x"],
            &["-p", "a", "--manifest-path=../Cargo.toml"],
            &["--message-format", "short"],
            &["--config", "build.jobs=2"],
            &["-Zunstable-options"],
            &["-j4"],
            &["--jobs=4"],
        ] {
            assert!(operation(args).validate().is_err(), "{args:?}");
        }
        for args in [
            &[][..],
            &["-p", "app-ui", "--lib"],
            &["--release", "--features", "x"],
            &["--", "--nocapture", "-Zunstable-options", "--target-dir"],
            &["--target-dirs"],
        ] {
            assert_eq!(operation(args).validate(), Ok(()), "{args:?}");
        }
    }

    #[test]
    fn crates_are_counted_and_slot_paths_held_back() {
        assert_eq!(
            Line::of(r#"{"reason":"compiler-artifact","filenames":["/slot/x"],"fresh":true}"#),
            Line::Crate {
                fresh: true,
                outputs: vec!["/slot/x".into()],
                executable: None,
            }
        );
        assert_eq!(
            Line::of(
                r#"{"reason":"compiler-artifact","fresh":false,"filenames":["/slot/app"],"executable":"/slot/app"}"#
            ),
            Line::Crate {
                fresh: false,
                outputs: vec!["/slot/app".into()],
                executable: Some("/slot/app".into()),
            }
        );
        assert_eq!(
            Line::of(r#"{"reason":"build-script-executed","out_dir":"/slot/x"}"#),
            Line::BuildScript {
                out_dir: "/slot/x".into()
            }
        );
        assert_eq!(
            Line::of(r#"{"reason":"build-finished","success":true}"#),
            Line::BuildFinished { success: true }
        );
        assert_eq!(
            Line::of(r#"{"reason":"build-finished","success":false}"#),
            Line::BuildFinished { success: false }
        );
        assert_eq!(Line::of(r#"{"reason":"build-finished"}"#), Line::Forward);
        for forwarded in [
            r#"{"reason":"compiler-message","message":{"rendered":"x"}}"#,
            "test tests::it_works ... ok",
            "{ not json",
        ] {
            assert_eq!(Line::of(forwarded), Line::Forward, "{forwarded}");
        }
    }

    #[test]
    fn only_unfiltered_nextest_runs_run_every_test() {
        let nextest = |args: &[&str]| Operation {
            command: Command::Nextest,
            ..operation(args)
        };
        assert!(nextest(&["--workspace", "--no-fail-fast", "-p", "x"]).runs_every_test());
        assert!(nextest(&["--workspace", "--profile", "default"]).runs_every_test());
        for filtered in [
            &["name"][..],
            &["-E", "test(x)"],
            &["--", "--exact", "x"],
            &["--profile", "scale"],
            &["-P", "scale"],
            &["--profile=scale"],
            &["--ignore-default-filter"],
            &["--filterset=test(x)"],
            &["--partition=count:1/2"],
            &["--run-ignored=only"],
            &["--stress-count", "20"],
            &["--stress-duration=1m"],
        ] {
            assert!(!nextest(filtered).runs_every_test(), "{filtered:?}");
        }
        assert!(!operation(&["--workspace"]).runs_every_test());
    }

    #[test]
    fn nextest_runs_are_the_compilation_of_test() {
        let of = |command: Command, args: &[&str]| {
            Compilation::new(
                Path::new(""),
                &Operation {
                    command,
                    ..operation(args)
                },
            )
        };
        assert_eq!(
            of(
                Command::Nextest,
                &["--workspace", "-E", "test(x)", "name", "--no-capture"]
            ),
            of(Command::Test, &["--workspace"])
        );
        assert_eq!(
            of(
                Command::Nextest,
                &["-p", "x", "--cargo-profile", "ci", "-P", "slow"]
            ),
            of(Command::Test, &["-p", "x", "--profile", "ci"])
        );
        assert_eq!(
            of(Command::Nextest, &["-p", "x", "--", "--exact", "a"]),
            of(Command::Test, &["-p", "x"])
        );
        for execution in [
            vec!["--profile", "full", "-E", "test(x)", "--stress-count", "20"],
            vec![
                "--profile=full",
                "--filterset=test(x)",
                "--stress-duration=1m",
            ],
            vec!["--retries=2", "--partition=count:1/2", "--run-ignored=only"],
            vec![
                "--success-output",
                "never",
                "--failure-output=immediate",
                "--status-level",
                "fail",
                "--final-status-level=all",
                "--no-tests=pass",
            ],
            vec!["--ignore-default-filter"],
        ] {
            let mut args = vec!["-p", "x"];
            args.extend(execution);
            assert_eq!(
                of(Command::Nextest, &args),
                of(Command::Test, &["-p", "x"]),
                "{args:?}"
            );
        }
        assert_eq!(
            of(Command::Nextest, &["-p", "x", "--cargo-profile=ci"]),
            of(Command::Test, &["-p", "x", "--profile", "ci"])
        );
        assert!(
            Operation {
                command: Command::Nextest,
                ..operation(&["--status-level", "fail", "--retries=2"])
            }
            .runs_every_test()
        );
    }

    #[test]
    fn a_compilation_ignores_only_what_changes_what_runs() {
        let of = |command: Command, args: &[&str]| {
            Compilation::new(
                Path::new(""),
                &Operation {
                    command,
                    args: args.iter().map(|argument| (*argument).to_owned()).collect(),
                    rustflags: Vec::new(),
                },
            )
        };
        assert_eq!(
            of(Command::Test, &["-p", "x", "--", "filter"]),
            of(Command::Test, &["-p", "x"])
        );
        assert_eq!(
            of(Command::Test, &["-p", "x", "--no-run"]),
            of(Command::Test, &["-p", "x"])
        );
        assert_ne!(
            of(Command::Clippy, &["-p", "x", "--", "-D", "warnings"]),
            of(Command::Clippy, &["-p", "x"])
        );
    }

    #[test]
    fn test_names_share_compilation_but_option_values_keep_their_identity() {
        let of = |args: &[&str]| Compilation::new(Path::new(""), &operation(args));
        for selection in [
            vec!["-p", "engine", "--test", "serve"],
            vec!["--features", "a,b", "--target", "aarch64-apple-darwin"],
            vec!["-pengine", "-Fa,b", "--profile=dev"],
            vec!["--workspace", "--exclude", "app", "--all-targets"],
        ] {
            let expected = of(&selection);
            let mut filtered = selection.clone();
            filtered.extend(["some::test", "--no-run", "--no-fail-fast", "--", "--exact"]);
            assert_eq!(of(&filtered), expected);
            let mut prefixed = vec!["other::test"];
            prefixed.extend(selection);
            assert_eq!(of(&prefixed), expected);
        }
        assert_ne!(of(&["--test", "first"]), of(&["--test", "second"]));
        assert_ne!(of(&["-F", "first"]), of(&["-F", "second"]));
        assert_ne!(of(&["--profile", "dev"]), of(&["--profile", "release"]));
        assert_ne!(of(&["--doc"]), of(&["--lib"]));
        // A future option could consume the following word as a value.
        assert_ne!(
            of(&["--new-option", "first"]),
            of(&["--new-option", "second"])
        );
    }

    #[test]
    fn commands_round_trip_their_names() {
        for command in Command::ALL {
            assert_eq!(Command::parse(command.as_str()), Some(command));
        }
        assert_eq!(Command::parse("bench"), None);
    }

    #[test]
    fn a_run_names_a_program_and_its_arguments_are_its_own() {
        let run = |args: &[&str]| Operation {
            command: Command::Run,
            ..operation(args)
        };
        // Its arguments are the program's, Cargo's options included.
        assert_eq!(
            run(&["make", "--jobs=4", "--target-dir", "-C", "x"]).validate(),
            Ok(())
        );
        assert!(run(&[]).validate().is_err());
        assert!(run(&[""]).validate().is_err());
        let mut flagged = run(&["make"]);
        flagged.rustflags = vec!["--cfg".into(), "x".into()];
        assert!(flagged.validate().is_err());
        // Its identity is all of its arguments.
        assert_ne!(
            Compilation::new(Path::new(""), &run(&["make", "a"])),
            Compilation::new(Path::new(""), &run(&["make", "b"]))
        );
        assert_eq!(run(&["make", "-j"]).to_string(), "run make -j");
        assert_eq!(run(&["make"]).merged(&run(&["make", "x"])), None);
    }
}
