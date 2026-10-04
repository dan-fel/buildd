//! Build slots: a checkout at a fixed path and the target directory only it
//! uses.
//!
//! A slot is an ordinary git repository whose objects come from the source
//! repository through `objects/info/alternates`, so checking out a snapshot
//! copies nothing but the files that differ from the slot's previous
//! checkout. Cargo then sees ordinary edits, and the slot's path, and with it
//! every fingerprint in its target, never changes.

use std::path::{Path, PathBuf};

use crate::git;
use crate::snapshot::Revision;

/// Fixed identity and date for slot commits, so one tree is always one
/// commit.
const COMMIT_ENVIRONMENT: [(&str, &str); 6] = [
    ("GIT_AUTHOR_NAME", "buildd"),
    ("GIT_AUTHOR_EMAIL", "buildd@localhost"),
    ("GIT_AUTHOR_DATE", "1970-01-01T00:00:00Z"),
    ("GIT_COMMITTER_NAME", "buildd"),
    ("GIT_COMMITTER_EMAIL", "buildd@localhost"),
    ("GIT_COMMITTER_DATE", "1970-01-01T00:00:00Z"),
];

/// The directory of slot `index` of `repository` under `home`.
#[derive(Clone, Debug)]
pub(crate) struct SlotDirectory(PathBuf);

impl SlotDirectory {
    pub(crate) fn new(home: &Path, repository: &Path, index: usize) -> Self {
        Self(
            home.join("slots")
                .join(project_name(repository))
                .join(index.to_string()),
        )
    }

    /// The slot's checkout.
    pub(crate) fn source(&self) -> PathBuf {
        self.0.join("src")
    }

    /// The slot's Cargo target directory.
    pub(crate) fn target(&self) -> PathBuf {
        self.0.join("target")
    }

    /// Makes the checkout hold exactly `revision`: files that differ are
    /// written, files the tree lacks are removed, and every other file keeps
    /// its modification time.
    pub(crate) fn materialize(&self, repository: &Path, revision: &Revision) -> Result<(), String> {
        let source = self.source();
        if !source.join(".git").exists() {
            std::fs::create_dir_all(&source)
                .map_err(|error| format!("could not create {}: {error}", source.display()))?;
            let mut init = git::command(&source);
            init.args(["init", "-q", "--template="]);
            git::run(init)?;
        }
        let alternates = source.join(".git/objects/info/alternates");
        std::fs::create_dir_all(alternates.parent().expect("alternates has a parent"))
            .and_then(|()| {
                std::fs::write(
                    &alternates,
                    format!("{}\n", repository.join("objects").display()),
                )
            })
            .map_err(|error| format!("could not write {}: {error}", alternates.display()))?;

        let mut commit = git::command(&source);
        commit
            .envs(COMMIT_ENVIRONMENT)
            .args([
                "-c",
                "commit.gpgSign=false",
                "commit-tree",
                "-m",
                "buildd snapshot",
            ])
            .arg(revision.to_string());
        let commit = git::run(commit)?;
        let mut reset = git::command(&source);
        reset.args(["reset", "-q", "--hard", commit.trim()]);
        git::run(reset)?;
        let mut clean = git::command(&source);
        clean.args(["clean", "-q", "-ffdx"]);
        git::run(clean)?;
        Ok(())
    }
}

/// A slot's display name: its project and index.
pub(crate) fn slot_name(repository: &Path, index: usize) -> String {
    format!("{}/{index}", project_name(repository))
}

/// The directory name of a repository's slots: a readable name and a hash of
/// the repository's path, so two repositories never share slots.
fn project_name(repository: &Path) -> String {
    let readable = if repository.file_name().is_some_and(|name| name == ".git") {
        repository.parent().and_then(Path::file_name)
    } else {
        repository.file_name()
    };
    let readable = readable.map_or_else(|| "repository".into(), |name| name.to_string_lossy());
    format!(
        "{readable}-{:016x}",
        fnv1a(repository.as_os_str().as_encoded_bytes())
    )
}

/// The 64-bit FNV-1a hash: stable across builds and platforms.
fn fnv1a(bytes: &[u8]) -> u64 {
    bytes.iter().fold(0xcbf2_9ce4_8422_2325, |hash, byte| {
        (hash ^ u64::from(*byte)).wrapping_mul(0x0000_0100_0000_01b3)
    })
}

#[cfg(test)]
mod tests {
    use std::time::SystemTime;

    use super::*;
    use crate::snapshot::resolve;
    use crate::snapshot::tests::{TempDir, git, repository};

    fn modified(path: &Path) -> SystemTime {
        std::fs::metadata(path).unwrap().modified().unwrap()
    }

    #[test]
    fn a_checkout_holds_exactly_its_revision_and_rewrites_only_what_changed() {
        let repository = repository();
        let home = TempDir::new();
        let scratch = TempDir::new();
        let source = resolve(&repository.0).unwrap();
        let first = source.snapshot(&scratch.0).unwrap();
        let slot = SlotDirectory::new(&home.0, &source.repository, 0);
        slot.materialize(&source.repository, &first).unwrap();
        let checkout = slot.source();
        assert_eq!(
            std::fs::read_to_string(checkout.join("a.txt")).unwrap(),
            "a\n"
        );
        let b_written = modified(&checkout.join("b.txt"));

        std::fs::write(repository.0.join("a.txt"), "changed\n").unwrap();
        std::fs::remove_file(repository.0.join("b.txt")).unwrap();
        std::fs::write(repository.0.join("c.txt"), "c\n").unwrap();
        let second = source.snapshot(&scratch.0).unwrap();
        // Leftovers of a build in the checkout: an untracked file, an ignored
        // one and an edit to a tracked one.
        std::fs::write(checkout.join("junk.txt"), "junk\n").unwrap();
        std::fs::create_dir(checkout.join("ignored")).unwrap();
        std::fs::write(checkout.join("ignored/x.txt"), "x\n").unwrap();
        std::fs::write(checkout.join(".gitignore"), "edited\n").unwrap();
        slot.materialize(&source.repository, &second).unwrap();

        let mut files = std::fs::read_dir(&checkout)
            .unwrap()
            .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
            .collect::<Vec<_>>();
        files.sort();
        assert_eq!(files, [".git", ".gitignore", "a.txt", "c.txt"]);
        assert_eq!(
            std::fs::read_to_string(checkout.join("a.txt")).unwrap(),
            "changed\n"
        );
        assert_eq!(
            std::fs::read_to_string(checkout.join(".gitignore")).unwrap(),
            "ignored/\n"
        );

        // Restoring b.txt writes b.txt alone.
        let c_written = modified(&checkout.join("c.txt"));
        std::fs::write(repository.0.join("b.txt"), "b\n").unwrap();
        let third = source.snapshot(&scratch.0).unwrap();
        slot.materialize(&source.repository, &third).unwrap();
        assert_eq!(modified(&checkout.join("c.txt")), c_written);
        assert!(modified(&checkout.join("b.txt")) > b_written);
        assert_eq!(
            git(&checkout, &["rev-parse", "HEAD^{tree}"]).trim(),
            third.to_string()
        );
    }

    #[test]
    fn repositories_get_distinct_readable_slot_directories() {
        let a = slot_name(Path::new("/work/jaide/.git"), 0);
        let b = slot_name(Path::new("/other/jaide/.git"), 1);
        assert!(a.starts_with("jaide-") && a.ends_with("/0"), "{a}");
        assert!(b.starts_with("jaide-") && b.ends_with("/1"), "{b}");
        assert_ne!(a.trim_end_matches("/0"), b.trim_end_matches("/1"));
        assert!(slot_name(Path::new("/srv/bare.git"), 0).starts_with("bare.git-"));
    }
}
