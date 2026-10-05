//! The Cargo commands buildd runs and how it runs them in a slot.

use std::fmt;
use std::path::Path;
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

/// A Cargo command and its arguments as the client gave them. Two equal
/// operations on one revision are one build.
#[derive(Clone, Debug, Eq, PartialEq, Ord, PartialOrd, Hash, Serialize, Deserialize)]
pub struct Operation {
    pub command: Command,
    pub args: Vec<String>,
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
    command
}

/// A line of Cargo's standard output, as the daemon treats it.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum Line {
    /// A crate Cargo compiled, or found up to date when `fresh`. Its message
    /// names paths inside the slot, which mean nothing to a client.
    Crate { fresh: bool },
    /// A build script's report, also naming paths inside the slot.
    BuildScript,
    /// Diagnostics, the final build message and every other line (test
    /// output): for the client.
    Forward,
}

impl Line {
    pub(crate) fn of(line: &str) -> Self {
        #[derive(Deserialize)]
        struct Message<'a> {
            reason: &'a str,
            #[serde(default)]
            fresh: bool,
        }
        match serde_json::from_str::<Message<'_>>(line) {
            Ok(Message {
                reason: "compiler-artifact",
                fresh,
            }) => Self::Crate { fresh },
            Ok(Message {
                reason: "build-script-executed",
                ..
            }) => Self::BuildScript,
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
            &["-p", "jaide-gui", "--lib"],
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
            Line::Crate { fresh: true }
        );
        assert_eq!(
            Line::of(r#"{"reason":"compiler-artifact","fresh":false}"#),
            Line::Crate { fresh: false }
        );
        assert_eq!(
            Line::of(r#"{"reason":"build-script-executed","out_dir":"/slot/x"}"#),
            Line::BuildScript
        );
        for forwarded in [
            r#"{"reason":"compiler-message","message":{"rendered":"x"}}"#,
            r#"{"reason":"build-finished","success":true}"#,
            "test tests::it_works ... ok",
            "{ not json",
        ] {
            assert_eq!(Line::of(forwarded), Line::Forward, "{forwarded}");
        }
    }

    #[test]
    fn commands_round_trip_their_names() {
        for command in Command::ALL {
            assert_eq!(Command::parse(command.as_str()), Some(command));
        }
        assert_eq!(Command::parse("run"), None);
    }
}
