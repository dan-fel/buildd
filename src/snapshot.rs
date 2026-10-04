//! Snapshots: the content of a worktree, as a git tree.
//!
//! A snapshot holds every tracked file and every untracked file git does not
//! ignore, as they are on disk, whether committed or not. Its tree id is the
//! [`Revision`] a build reports: two snapshots with the same id have the same
//! content.

use std::fmt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use serde::{Deserialize, Serialize};

use crate::git;

/// The id of a snapshot's git tree.
#[derive(Clone, Debug, Eq, PartialEq, Ord, PartialOrd, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct Revision(String);

impl Revision {
    /// The first ten digits, enough to tell revisions apart in messages.
    #[must_use]
    pub fn short(&self) -> &str {
        &self.0[..self.0.len().min(10)]
    }
}

impl fmt::Display for Revision {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
    }
}

/// Where a request comes from.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Source {
    /// The repository's common git directory. Worktrees of one repository
    /// share it, and with it their build slots.
    pub repository: PathBuf,
    /// The top of the worktree the request came from.
    pub worktree: PathBuf,
    /// The request's directory relative to the top of the worktree; Cargo
    /// runs in the same directory of the slot's checkout.
    pub prefix: PathBuf,
    /// The worktree's index, whose cached file stats make snapshots fast.
    pub(crate) index: PathBuf,
}

/// The worktree that contains `directory`.
///
/// # Errors
/// When `directory` is not inside a git worktree, or git fails.
pub fn resolve(directory: &Path) -> Result<Source, String> {
    let mut command = git::command(directory);
    command.args([
        "rev-parse",
        "--show-prefix",
        "--path-format=absolute",
        "--show-toplevel",
        "--git-common-dir",
        "--git-path",
        "index",
    ]);
    let output = git::run(command)?;
    let mut lines = output.lines();
    let mut next = |what: &str| {
        lines
            .next()
            .ok_or_else(|| format!("git rev-parse printed no {what}"))
    };
    let prefix = PathBuf::from(next("prefix")?);
    let worktree = canonical(next("worktree")?)?;
    let repository = canonical(next("git directory")?)?;
    let index = PathBuf::from(next("index")?);
    Ok(Source {
        repository,
        worktree,
        prefix,
        index,
    })
}

fn canonical(path: &str) -> Result<PathBuf, String> {
    std::fs::canonicalize(path).map_err(|error| format!("{path}: {error}"))
}

impl Source {
    /// Writes the worktree's current content as a tree into the repository.
    ///
    /// It builds the tree in a private copy of the worktree's index, under
    /// `scratch`, so the worktree's own index and staged changes stay
    /// untouched.
    ///
    /// # Errors
    /// When git fails or the private index cannot be prepared.
    pub fn snapshot(&self, scratch: &Path) -> Result<Revision, String> {
        let index = ScratchIndex::new(scratch)?;
        if self.index.exists() {
            std::fs::copy(&self.index, &index.0).map_err(|error| {
                format!("could not copy the index {}: {error}", self.index.display())
            })?;
        }
        let mut add = git::command(&self.worktree);
        add.env("GIT_INDEX_FILE", &index.0).args(["add", "--all"]);
        git::run(add)?;
        let mut write = git::command(&self.worktree);
        write.env("GIT_INDEX_FILE", &index.0).arg("write-tree");
        let tree = git::run(write)?;
        Ok(Revision(tree.trim().to_owned()))
    }
}

/// A private index file, removed when dropped.
struct ScratchIndex(PathBuf);

impl ScratchIndex {
    fn new(scratch: &Path) -> Result<Self, String> {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        std::fs::create_dir_all(scratch)
            .map_err(|error| format!("could not create {}: {error}", scratch.display()))?;
        let name = format!(
            "{}-{}.index",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        );
        Ok(Self(scratch.join(name)))
    }
}

impl Drop for ScratchIndex {
    fn drop(&mut self) {
        // The file is absent when git failed before writing it.
        let _ = std::fs::remove_file(&self.0);
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    /// A temporary directory, removed when dropped.
    pub(crate) struct TempDir(pub(crate) PathBuf);

    impl TempDir {
        pub(crate) fn new() -> Self {
            static NEXT: AtomicU64 = AtomicU64::new(0);
            let path = std::env::temp_dir().join(format!(
                "buildd-unit-{}-{}",
                std::process::id(),
                NEXT.fetch_add(1, Ordering::Relaxed)
            ));
            std::fs::create_dir_all(&path).expect("create a temporary directory");
            Self(std::fs::canonicalize(&path).expect("canonicalize it"))
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    /// Runs git in `directory` with a fixed identity and no user config.
    pub(crate) fn git(directory: &Path, arguments: &[&str]) -> String {
        let mut command = git::command(directory);
        command
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .args(["-c", "user.name=t", "-c", "user.email=t@t"])
            .args(arguments);
        git::run(command).expect("git succeeds")
    }

    /// A repository with one commit holding `a.txt` and `b.txt`.
    pub(crate) fn repository() -> TempDir {
        let directory = TempDir::new();
        git(&directory.0, &["init", "-q", "-b", "main"]);
        std::fs::write(directory.0.join("a.txt"), "a\n").unwrap();
        std::fs::write(directory.0.join("b.txt"), "b\n").unwrap();
        std::fs::write(directory.0.join(".gitignore"), "ignored/\n").unwrap();
        git(&directory.0, &["add", "--all"]);
        git(&directory.0, &["commit", "-q", "-m", "first"]);
        directory
    }

    #[test]
    fn a_snapshot_holds_uncommitted_and_untracked_files_but_not_ignored_ones() {
        let repository = repository();
        let scratch = TempDir::new();
        std::fs::create_dir(repository.0.join("sub")).unwrap();
        let source = resolve(&repository.0.join("sub")).unwrap();
        assert_eq!(source.worktree, repository.0);
        assert_eq!(source.prefix, PathBuf::from("sub/"));
        assert_eq!(source.repository, repository.0.join(".git"));
        let committed = source.snapshot(&scratch.0).unwrap();
        assert_eq!(
            committed.0,
            git(&repository.0, &["rev-parse", "HEAD^{tree}"]).trim()
        );

        std::fs::write(repository.0.join("a.txt"), "changed\n").unwrap();
        std::fs::write(repository.0.join("new.txt"), "new\n").unwrap();
        std::fs::create_dir(repository.0.join("ignored")).unwrap();
        std::fs::write(repository.0.join("ignored/x.txt"), "x\n").unwrap();
        let edited = source.snapshot(&scratch.0).unwrap();
        let files = git(&repository.0, &["ls-tree", "-r", "--name-only", &edited.0]);
        assert_eq!(
            files.lines().collect::<Vec<_>>(),
            [".gitignore", "a.txt", "b.txt", "new.txt"]
        );
        let a = git(
            &repository.0,
            &["cat-file", "-p", &format!("{edited}:a.txt")],
        );
        assert_eq!(a, "changed\n");

        // The worktree's own index is untouched and no scratch index remains.
        assert_eq!(git(&repository.0, &["diff", "--cached", "--name-only"]), "");
        assert_eq!(std::fs::read_dir(&scratch.0).unwrap().count(), 0);
        assert_eq!(source.snapshot(&scratch.0).unwrap(), edited);
    }

    #[test]
    fn worktrees_of_one_repository_share_it() {
        let repository = repository();
        let other = TempDir::new();
        let linked = other.0.join("linked");
        git(
            &repository.0,
            &[
                "worktree",
                "add",
                "-q",
                "--detach",
                linked.to_str().unwrap(),
            ],
        );
        let main = resolve(&repository.0).unwrap();
        let linked = resolve(&linked).unwrap();
        assert_eq!(main.repository, linked.repository);
        assert_ne!(main.worktree, linked.worktree);
        assert_eq!(linked.prefix, PathBuf::new());
    }

    #[test]
    fn a_directory_outside_any_worktree_is_rejected() {
        let directory = TempDir::new();
        let error = resolve(&directory.0).unwrap_err();
        assert!(error.contains("rev-parse"), "{error}");
    }
}
