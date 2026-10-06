//! The Cargo commands buildd runs and how it runs them in a slot.

use std::fmt;
use std::path::{Path, PathBuf};
use std::process::Stdio;

use serde::{Deserialize, Serialize};

/// A Cargo subcommand buildd runs.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Ord, PartialOrd, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Command {
    Check,
    Clippy,
    Build,
    Test,
}

impl Command {
    /// Every command, in the order usage lists them.
    pub const ALL: [Self; 4] = [Self::Check, Self::Clippy, Self::Build, Self::Test];

    /// The subcommand's name.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Check => "check",
            Self::Clippy => "clippy",
            Self::Build => "build",
            Self::Test => "test",
        }
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

/// Cargo options that would take the slot's target, checkout, output format
/// or parallelism out of buildd's hands.
const RESERVED_OPTIONS: [&str; 8] = [
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
/// `--no-run` and `--no-fail-fast`. For
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
        let args = match operation.command {
            Command::Test => test_compilation_args(&operation.args),
            Command::Check | Command::Clippy | Command::Build => operation.args.clone(),
        };
        Self {
            prefix: prefix.to_owned(),
            command: operation.command,
            args,
            rustflags: operation.rustflags.clone(),
        }
    }
}

/// Strip execution-only test arguments without mistaking an option's value
/// for a test name. Unknown options keep the original selection: Cargo owns
/// its argument language, and an uncertain reuse estimate must stay distinct.
fn test_compilation_args(args: &[String]) -> Vec<String> {
    let original: Vec<_> = args
        .iter()
        .take_while(|argument| *argument != "--")
        .filter(|argument| !matches!(argument.as_str(), "--no-run" | "--no-fail-fast"))
        .cloned()
        .collect();
    let mut compiled = Vec::new();
    let mut arguments = original.iter();
    while let Some(argument) = arguments.next() {
        if !argument.starts_with('-') {
            // Cargo's only free argument before `--` is TESTNAME.
            continue;
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

/// A Cargo command and its arguments as the client gave them. Two equal
/// operations on one revision are one build.
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
    ///
    /// # Errors
    /// Names the first reserved argument.
    pub fn validate(&self) -> Result<(), String> {
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
/// building into the slot's `target`. It runs in its own process group so a
/// cancellation reaches everything it started.
pub(crate) fn command(
    directory: &Path,
    target: &Path,
    operation: &Operation,
) -> std::process::Command {
    use std::os::unix::process::CommandExt as _;

    let mut command = std::process::Command::new("cargo");
    command
        .current_dir(directory)
        .arg(operation.command.as_str())
        .arg("--message-format=json")
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
    /// Cargo's message that compilation ended; for `test`, the tests run
    /// next. The client gets it too.
    BuildFinished,
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
                ..
            }) => Self::BuildFinished,
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
            Line::BuildFinished
        );
        for forwarded in [
            r#"{"reason":"compiler-message","message":{"rendered":"x"}}"#,
            "test tests::it_works ... ok",
            "{ not json",
        ] {
            assert_eq!(Line::of(forwarded), Line::Forward, "{forwarded}");
        }
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
        assert_eq!(Command::parse("run"), None);
    }
}
