//! `buildd run`: a command of the client's own, run against the same
//! snapshot a Cargo build would compile, in a build slot, under the same
//! queue, job budget, drain and cancellation.
//!
//! - **Directory.** The slot's checkout of the snapshot, in the directory
//!   the client asked from. A program named with a `/` is a path from
//!   there; a bare name is looked up in the daemon's `PATH`.
//! - **Environment.** The daemon's, plus `BUILDD_TREE` (the snapshot's tree
//!   id), `BUILDD_JOBS` (the cores it may keep busy: `test_jobs`, which the
//!   budget charges it for as it charges tests) and `BUILDD_OUT` (an empty
//!   directory for its outputs). Cargo it starts builds with
//!   `BUILDD_JOBS` jobs into a target of its own (`CARGO_TARGET_DIR`), which
//!   is deleted afterwards: the slot's target is never the command's.
//! - **Outputs.** What it leaves in `BUILDD_OUT` is moved into the
//!   request's directory to copy to once it succeeds, each entry replacing
//!   one of the same name as a whole, and deleted otherwise.
//! - **Checkout.** Once it ends, the checkout holds the snapshot again,
//!   whatever it changed there.
//!
//! The command runs as the daemon's user, so nothing but this contract keeps
//! it from writing the slot's target by its path; one that does voids what
//! buildd promises about that slot.

use std::path::{Path, PathBuf};
use std::process::Stdio;

use crate::cargo::Operation;
use crate::log::log;
use crate::snapshot::Revision;

/// The directories a running command has of its own: `out` for its outputs
/// and `target` for Cargo it starts. Removed when dropped.
pub(crate) struct RunDirectory(PathBuf);

impl RunDirectory {
    /// `runs`'s directory for job `job`, created empty.
    ///
    /// # Errors
    /// When it exists already or cannot be created.
    pub(crate) fn create(runs: &Path, job: &str) -> Result<Self, String> {
        let path = runs.join(job);
        std::fs::create_dir_all(runs)
            .and_then(|()| std::fs::create_dir(&path))
            .and_then(|()| std::fs::create_dir(path.join("out")))
            .map_err(|error| format!("could not create {}: {error}", path.display()))?;
        Ok(Self(path))
    }

    /// Where the command leaves its outputs.
    pub(crate) fn out(&self) -> PathBuf {
        self.0.join("out")
    }

    /// Cargo's target directory for Cargo the command starts.
    fn target(&self) -> PathBuf {
        self.0.join("target")
    }
}

impl Drop for RunDirectory {
    fn drop(&mut self) {
        if let Err(error) = std::fs::remove_dir_all(&self.0) {
            log!("could not remove {}: {error}", self.0.display());
        }
    }
}

/// The process of `operation`, a `run`, in `directory` of a slot's checkout
/// of `tree`, using `jobs` cores and the directories of `run`. It runs in
/// its own process group so a cancellation reaches everything it started.
pub(crate) fn command(
    directory: &Path,
    operation: &Operation,
    tree: &Revision,
    run: &RunDirectory,
    jobs: usize,
) -> std::process::Command {
    use std::os::unix::process::CommandExt as _;

    let (program, args) = operation
        .args
        .split_first()
        .expect("a validated run names its program");
    let program = if program.contains('/') {
        // Absolute, or a path in the checkout.
        directory.join(program)
    } else {
        PathBuf::from(program)
    };
    let mut command = std::process::Command::new(program);
    command
        .current_dir(directory)
        .args(args)
        .env("BUILDD_TREE", tree.to_string())
        .env("BUILDD_JOBS", jobs.to_string())
        .env("BUILDD_OUT", run.out())
        .env("CARGO_TARGET_DIR", run.target())
        .env("CARGO_BUILD_JOBS", jobs.to_string())
        .env_remove("CARGO_BUILD_TARGET_DIR")
        .env_remove("CARGO_MAKEFLAGS")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .process_group(0);
    command
}

#[cfg(test)]
mod tests {
    use std::os::unix::fs::PermissionsExt as _;

    use super::*;
    use crate::cargo::Command;
    use crate::snapshot::tests::TempDir;

    fn run(args: &[&str]) -> Operation {
        Operation {
            command: Command::Run,
            args: args.iter().map(|argument| (*argument).to_owned()).collect(),
            rustflags: Vec::new(),
        }
    }

    #[test]
    fn a_command_runs_in_the_checkout_with_its_own_directories_and_share() {
        let runs = TempDir::new();
        let checkout = TempDir::new();
        let directory = RunDirectory::create(&runs.0, "7").unwrap();
        assert!(
            RunDirectory::create(&runs.0, "7").is_err(),
            "a run's directory is its own"
        );
        let tree = Revision::of_tree("0123456789abcdef");
        let output = command(
            &checkout.0,
            &run(&[
                "sh",
                "-c",
                "echo \"$PWD $BUILDD_TREE $BUILDD_JOBS $CARGO_BUILD_JOBS\"; \
                 echo \"$BUILDD_OUT\"; echo \"$CARGO_TARGET_DIR\"; echo \"$1\"",
                "sh",
                "an argument",
            ]),
            &tree,
            &directory,
            3,
        )
        .output()
        .unwrap();
        assert!(output.status.success());
        let text = String::from_utf8(output.stdout).unwrap();
        let lines = text.lines().collect::<Vec<_>>();
        assert_eq!(
            lines,
            [
                format!("{} 0123456789abcdef 3 3", checkout.0.display()).as_str(),
                directory.out().to_str().unwrap(),
                directory.target().to_str().unwrap(),
                "an argument",
            ]
        );

        // A path with a slash is the checkout's.
        let script = checkout.0.join("tools/hello.sh");
        std::fs::create_dir(script.parent().unwrap()).unwrap();
        std::fs::write(&script, "#!/bin/sh\necho hello\n").unwrap();
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
        let output = command(
            &checkout.0,
            &run(&["./tools/hello.sh"]),
            &tree,
            &directory,
            1,
        )
        .output()
        .unwrap();
        assert_eq!(String::from_utf8(output.stdout).unwrap(), "hello\n");

        let path = directory.0.clone();
        drop(directory);
        assert!(!path.exists(), "a run's directory goes with it");
    }
}
