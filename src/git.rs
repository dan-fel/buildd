//! Running `git` with an environment buildd controls.

use std::path::Path;
use std::process::{Command, Stdio};

/// Variables that would point git at a repository other than the one the
/// working directory names.
const REPOSITORY_VARIABLES: [&str; 7] = [
    "GIT_DIR",
    "GIT_WORK_TREE",
    "GIT_INDEX_FILE",
    "GIT_COMMON_DIR",
    "GIT_OBJECT_DIRECTORY",
    "GIT_ALTERNATE_OBJECT_DIRECTORIES",
    "GIT_PREFIX",
];

/// A `git` command run in `directory`.
pub(crate) fn command(directory: &Path) -> Command {
    let mut command = Command::new("git");
    command.current_dir(directory).stdin(Stdio::null());
    for variable in REPOSITORY_VARIABLES {
        command.env_remove(variable);
    }
    command
}

/// Runs `command` and returns its standard output.
pub(crate) fn run(mut command: Command) -> Result<String, String> {
    let output = command
        .output()
        .map_err(|error| format!("could not run git: {error}"))?;
    if !output.status.success() {
        let arguments = command
            .get_args()
            .map(|argument| argument.to_string_lossy())
            .collect::<Vec<_>>()
            .join(" ");
        return Err(format!(
            "git {arguments} failed in {}: {}",
            command
                .get_current_dir()
                .map_or_else(String::new, |directory| directory.display().to_string()),
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }
    String::from_utf8(output.stdout).map_err(|_| "git printed output that is not UTF-8".to_owned())
}

/// The paths that differ between trees `from` and `to` of the repository
/// whose git directory is `repository`, relative to its top.
pub(crate) fn changed_paths(
    repository: &Path,
    from: &crate::snapshot::Revision,
    to: &crate::snapshot::Revision,
) -> Result<Vec<String>, String> {
    let mut diff = command(repository);
    diff.arg("--git-dir")
        .arg(repository)
        .args(["diff-tree", "-r", "--name-only", "--no-renames"])
        .arg(from.to_string())
        .arg(to.to_string());
    run(diff).map(|paths| paths.lines().map(str::to_owned).collect())
}

/// Fixed identity and date for snapshot commits, so one tree is always one
/// commit, in every repository.
const COMMIT_ENVIRONMENT: [(&str, &str); 6] = [
    ("GIT_AUTHOR_NAME", "buildd"),
    ("GIT_AUTHOR_EMAIL", "buildd@localhost"),
    ("GIT_AUTHOR_DATE", "1970-01-01T00:00:00Z"),
    ("GIT_COMMITTER_NAME", "buildd"),
    ("GIT_COMMITTER_EMAIL", "buildd@localhost"),
    ("GIT_COMMITTER_DATE", "1970-01-01T00:00:00Z"),
];

/// Makes the commit of tree `revision` alone with `git`, a git command for
/// the repository to make it in, and returns its id.
pub(crate) fn snapshot_commit(
    mut git: Command,
    revision: &crate::snapshot::Revision,
) -> Result<String, String> {
    git.envs(COMMIT_ENVIRONMENT)
        .args([
            "-c",
            "commit.gpgSign=false",
            "commit-tree",
            "-m",
            "buildd snapshot",
        ])
        .arg(revision.to_string());
    run(git).map(|commit| commit.trim().to_owned())
}
