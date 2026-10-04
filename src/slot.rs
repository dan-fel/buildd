//! Build slots: a checkout at a fixed path and the target directory only it
//! uses.
//!
//! A slot is an ordinary git repository whose objects come from the source
//! repository through `objects/info/alternates`, so checking out a snapshot
//! copies nothing but the files that differ from the slot's previous
//! checkout. Cargo then sees ordinary edits, and the slot's path, and with it
//! every fingerprint in its target, never changes.

use std::os::unix::fs::MetadataExt as _;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

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

    /// Keeps the slot's target within `limit` bytes of disk. Incremental
    /// compilation caches go first, least recently compiled first: removing
    /// one only makes rustc compile that unit from scratch once. The whole
    /// target goes only when its compiled artifacts alone exceed the limit.
    ///
    /// Only call this while no build runs in the slot.
    pub(crate) fn enforce_limit(&self, limit: u64) -> Result<Pruning, String> {
        let target = self.target();
        if !target.exists() {
            return Ok(Pruning::Within { size: 0 });
        }
        let before = disk_usage(&target)?;
        if before <= limit {
            return Ok(Pruning::Within { size: before });
        }
        let mut caches = incremental_caches(&target)?;
        caches.sort_by_key(|cache| cache.compiled);
        let mut size = before;
        let mut removed = 0;
        for cache in caches {
            if size <= limit {
                break;
            }
            std::fs::remove_dir_all(&cache.path)
                .map_err(|error| format!("could not remove {}: {error}", cache.path.display()))?;
            size = size.saturating_sub(cache.size);
            removed += 1;
        }
        if size <= limit {
            return Ok(Pruning::Incremental {
                before,
                after: size,
                removed,
            });
        }
        std::fs::remove_dir_all(&target)
            .map_err(|error| format!("could not remove {}: {error}", target.display()))?;
        Ok(Pruning::Cleared { before })
    }
}

/// What keeping a slot within its limit did.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum Pruning {
    /// The target was within the limit.
    Within { size: u64 },
    /// `removed` incremental caches went.
    Incremental {
        before: u64,
        after: u64,
        removed: usize,
    },
    /// The compiled artifacts alone exceeded the limit; the target went.
    Cleared { before: u64 },
}

impl Pruning {
    /// The target's size afterwards.
    pub(crate) fn size(self) -> u64 {
        match self {
            Self::Within { size } | Self::Incremental { after: size, .. } => size,
            Self::Cleared { .. } => 0,
        }
    }
}

/// One compilation unit's incremental cache.
struct IncrementalCache {
    path: PathBuf,
    size: u64,
    /// When rustc last compiled the unit: it replaces the cache's session
    /// directory on every incremental compilation.
    compiled: SystemTime,
}

/// The incremental caches of every profile, for the host and each target
/// triple: `<target>/[<triple>/]<profile>/incremental/<unit>`.
fn incremental_caches(target: &Path) -> Result<Vec<IncrementalCache>, String> {
    let mut caches = Vec::new();
    let mut profiles = Vec::new();
    for entry in read_directory(target)? {
        if entry.join("incremental").is_dir() {
            profiles.push(entry);
        } else if entry.is_dir() {
            profiles.extend(
                read_directory(&entry)?
                    .into_iter()
                    .filter(|profile| profile.join("incremental").is_dir()),
            );
        }
    }
    for profile in profiles {
        for path in read_directory(&profile.join("incremental"))? {
            let compiled = std::fs::symlink_metadata(&path)
                .and_then(|metadata| metadata.modified())
                .map_err(|error| format!("{}: {error}", path.display()))?;
            caches.push(IncrementalCache {
                size: disk_usage(&path)?,
                path,
                compiled,
            });
        }
    }
    Ok(caches)
}

fn read_directory(path: &Path) -> Result<Vec<PathBuf>, String> {
    std::fs::read_dir(path)
        .and_then(|entries| {
            entries
                .map(|entry| entry.map(|entry| entry.path()))
                .collect()
        })
        .map_err(|error| format!("could not read {}: {error}", path.display()))
}

/// The disk space the files under `path` occupy, not following links.
fn disk_usage(path: &Path) -> Result<u64, String> {
    let metadata =
        std::fs::symlink_metadata(path).map_err(|error| format!("{}: {error}", path.display()))?;
    let own = metadata.blocks() * 512;
    if !metadata.is_dir() {
        return Ok(own);
    }
    read_directory(path)?
        .iter()
        .try_fold(own, |total, child| Ok(total + disk_usage(child)?))
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

    /// Writes `bytes` of data to `path`, creating its directories.
    fn file(path: &Path, bytes: usize) {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, vec![7; bytes]).unwrap();
    }

    fn set_modified(path: &Path, seconds_ago: u64) {
        let time = SystemTime::now() - std::time::Duration::from_secs(seconds_ago);
        std::fs::File::open(path)
            .unwrap()
            .set_modified(time)
            .unwrap();
    }

    #[test]
    fn a_target_over_its_limit_loses_the_least_recently_compiled_caches_first() {
        let home = TempDir::new();
        let slot = SlotDirectory::new(&home.0, Path::new("/repo/.git"), 0);
        let target = slot.target();
        assert_eq!(slot.enforce_limit(0), Ok(Pruning::Within { size: 0 }));
        file(&target.join("debug/deps/libbig.rlib"), 400_000);
        for (unit, age) in [("old-1", 300), ("mid-2", 200), ("new-3", 100)] {
            file(
                &target.join(format!("debug/incremental/{unit}/s-1/query.bin")),
                200_000,
            );
            set_modified(&target.join(format!("debug/incremental/{unit}")), age);
        }
        file(
            &target.join("aarch64-apple-darwin/debug/incremental/cross-4/s-1/q.bin"),
            200_000,
        );
        set_modified(
            &target.join("aarch64-apple-darwin/debug/incremental/cross-4"),
            400,
        );

        let size = disk_usage(&target).unwrap();
        assert_eq!(slot.enforce_limit(size), Ok(Pruning::Within { size }));
        // Two caches of about 200 kB must go: the cross-compiled one and old-1.
        let Pruning::Incremental {
            removed: 2, after, ..
        } = slot.enforce_limit(size - 300_000).unwrap()
        else {
            panic!("two caches go");
        };
        assert!(after <= size - 300_000);
        assert!(
            !target
                .join("aarch64-apple-darwin/debug/incremental/cross-4")
                .exists()
        );
        assert!(!target.join("debug/incremental/old-1").exists());
        assert!(target.join("debug/incremental/mid-2").exists());
        assert!(target.join("debug/deps/libbig.rlib").exists());

        assert!(matches!(
            slot.enforce_limit(100_000),
            Ok(Pruning::Cleared { .. })
        ));
        assert!(!target.exists());
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
