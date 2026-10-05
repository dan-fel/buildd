//! Build slots: a checkout at a fixed path and the target directory only it
//! uses.
//!
//! A slot is an ordinary git repository whose objects come from the source
//! repository through `objects/info/alternates`, so checking out a snapshot
//! copies nothing but the files that differ from the slot's previous
//! checkout. Cargo then sees ordinary edits, and the slot's path, and with it
//! every fingerprint in its target, never changes.

use std::collections::HashMap;
use std::os::unix::ffi::OsStringExt as _;
use std::os::unix::fs::{MetadataExt as _, PermissionsExt as _};
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use serde::{Deserialize, Serialize};

use crate::cargo::Compilation;
use crate::git;
use crate::snapshot::Revision;

/// An incremental cache compiled this recently belongs to builds in use:
/// removing it makes their next compilation start from scratch.
const IN_USE: Duration = Duration::from_secs(10 * 60);

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

/// What a slot keeps on disk that its checkout and target do not tell: the
/// compilations it ran since its target was last cleared, and the worktree
/// it last built for. A daemon reads it back when it starts.
#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
pub(crate) struct SlotRecord {
    pub(crate) worktree: Option<PathBuf>,
    pub(crate) compiled: Vec<Compilation>,
}

/// A slot directory a daemon left behind.
#[derive(Debug)]
pub(crate) struct ExistingSlot {
    pub(crate) repository: PathBuf,
    pub(crate) index: usize,
    pub(crate) directory: SlotDirectory,
}

/// The slot directories under `home` whose repository is known: each
/// project directory names its repository in a `repository` file.
pub(crate) fn existing_slots(home: &Path) -> Result<Vec<ExistingSlot>, String> {
    let slots = home.join("slots");
    if !slots.exists() {
        return Ok(Vec::new());
    }
    let mut existing = Vec::new();
    for project in read_directory(&slots)? {
        let Ok(repository) = std::fs::read(project.join("repository")) else {
            eprintln!(
                "buildd: {} names no repository; leaving it alone",
                project.display()
            );
            continue;
        };
        let repository = PathBuf::from(std::ffi::OsString::from_vec(repository));
        for slot in read_directory(&project)? {
            let Some(index) = slot
                .file_name()
                .and_then(|name| name.to_str())
                .and_then(|name| name.parse::<usize>().ok())
            else {
                continue;
            };
            existing.push(ExistingSlot {
                repository: repository.clone(),
                index,
                directory: SlotDirectory(slot),
            });
        }
    }
    existing.sort_by(|a, b| (&a.repository, a.index).cmp(&(&b.repository, b.index)));
    Ok(existing)
}

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

    /// The tree the slot's checkout holds.
    pub(crate) fn tree(&self) -> Result<Revision, String> {
        let mut command = git::command(&self.source());
        command.args(["rev-parse", "HEAD^{tree}"]);
        git::run(command).map(|tree| Revision::of_tree(&tree))
    }

    /// The slot's record; empty when it has none.
    pub(crate) fn read_record(&self) -> Result<SlotRecord, String> {
        let path = self.0.join("record.json");
        match std::fs::read_to_string(&path) {
            Ok(text) => {
                serde_json::from_str(&text).map_err(|error| format!("{}: {error}", path.display()))
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(SlotRecord::default()),
            Err(error) => Err(format!("{}: {error}", path.display())),
        }
    }

    /// Replaces the slot's record, never leaving a partly written one.
    pub(crate) fn write_record(&self, record: &SlotRecord) -> Result<(), String> {
        let path = self.0.join("record.json");
        let partial = self.0.join("record.json.partial");
        let text = serde_json::to_string(record).expect("records serialize");
        std::fs::create_dir_all(&self.0)
            .and_then(|()| std::fs::write(&partial, text))
            .and_then(|()| std::fs::rename(&partial, &path))
            .map_err(|error| format!("could not write {}: {error}", path.display()))
    }

    /// Removes the slot: its checkout, target and record.
    pub(crate) fn remove(&self) -> Result<(), String> {
        remove_tree(&self.0)
    }

    /// Makes the checkout hold exactly `revision`: files that differ are
    /// written, files the tree lacks are removed, and every other file keeps
    /// its modification time.
    pub(crate) fn materialize(&self, repository: &Path, revision: &Revision) -> Result<(), String> {
        let source = self.source();
        let project = self
            .0
            .parent()
            .expect("a slot lives in a project directory");
        std::fs::create_dir_all(project)
            .and_then(|()| {
                std::fs::write(
                    project.join("repository"),
                    repository.as_os_str().as_encoded_bytes(),
                )
            })
            .map_err(|error| {
                format!(
                    "could not note the repository of {}: {error}",
                    project.display()
                )
            })?;
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
        let mut caches = incremental_caches(&target)?;
        let usage = disk_usage(&target, &caches)?;
        if usage.total <= limit {
            return Ok(Pruning::Within { size: usage.total });
        }
        let mut order = (0..caches.len()).collect::<Vec<_>>();
        order.sort_by_key(|&index| caches[index].compiled);
        // Space a removal frees for certain: files hard-linked from outside
        // the cache (object files kept for debug info) stay on disk.
        let mut estimate = usage.total;
        let mut removed = 0;
        let mut in_use = 0;
        let recent = SystemTime::now() - IN_USE;
        for index in order {
            if estimate <= limit {
                break;
            }
            let cache = &mut caches[index];
            remove_tree(&cache.path)?;
            estimate = estimate.saturating_sub(usage.freeable[index]);
            removed += 1;
            if cache.compiled > recent {
                in_use += 1;
            }
        }
        let after = disk_usage(&target, &[])?.total;
        if after <= limit {
            return Ok(Pruning::Incremental {
                before: usage.total,
                after,
                removed,
                in_use,
            });
        }
        remove_tree(&target)?;
        Ok(Pruning::Cleared {
            before: usage.total,
        })
    }
}

/// What keeping a slot within its limit did.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum Pruning {
    /// The target was within the limit.
    Within { size: u64 },
    /// `removed` incremental caches went, `in_use` of them compiled within
    /// the last ten minutes.
    Incremental {
        before: u64,
        after: u64,
        removed: usize,
        in_use: usize,
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

    /// Whether the limit is below what the slot's builds use: keeping to it
    /// took caches in use, or the whole target.
    pub(crate) fn undersized(self) -> bool {
        matches!(
            self,
            Self::Incremental { in_use: 1.., .. } | Self::Cleared { .. }
        )
    }
}

/// One compilation unit's incremental cache.
struct IncrementalCache {
    path: PathBuf,
    /// When rustc last compiled the unit: it replaces the cache's session
    /// directory on every incremental compilation.
    compiled: SystemTime,
}

/// The incremental caches of every profile, for the host and each target
/// triple: `<target>/[<triple>/]<profile>/incremental/<unit>`.
fn incremental_caches(target: &Path) -> Result<Vec<IncrementalCache>, String> {
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
    let mut caches = Vec::new();
    for profile in profiles {
        for path in read_directory(&profile.join("incremental"))? {
            let compiled = std::fs::symlink_metadata(&path)
                .and_then(|metadata| metadata.modified())
                .map_err(|error| format!("{}: {error}", path.display()))?;
            caches.push(IncrementalCache { path, compiled });
        }
    }
    Ok(caches)
}

/// The disk space under a directory.
struct DiskUsage {
    /// Every file counted once, however many hard links it has.
    total: u64,
    /// For each of the caches asked about, what removing it frees: its
    /// directories and the files with no hard link outside it.
    freeable: Vec<u64>,
}

/// One file's blocks and where its hard links were found.
struct Inode {
    bytes: u64,
    links: u64,
    seen: u64,
    /// The cache holding every link seen so far, if one does.
    cache: Option<usize>,
}

/// The disk space under `path`, not following symbolic links. rustc's
/// incremental sessions and Cargo's artifacts are hard links, so each file
/// counts once.
fn disk_usage(path: &Path, caches: &[IncrementalCache]) -> Result<DiskUsage, String> {
    fn walk(
        path: &Path,
        cache: Option<usize>,
        caches: &HashMap<&Path, usize>,
        inodes: &mut HashMap<(u64, u64), Inode>,
        usage: &mut DiskUsage,
    ) -> Result<(), String> {
        let cache = caches.get(path).copied().or(cache);
        let metadata = std::fs::symlink_metadata(path)
            .map_err(|error| format!("{}: {error}", path.display()))?;
        let bytes = metadata.blocks() * 512;
        if metadata.is_dir() {
            usage.total += bytes;
            if let Some(cache) = cache {
                usage.freeable[cache] += bytes;
            }
            for child in read_directory(path)? {
                walk(&child, cache, caches, inodes, usage)?;
            }
            return Ok(());
        }
        let inode = inodes
            .entry((metadata.dev(), metadata.ino()))
            .or_insert_with(|| {
                usage.total += bytes;
                Inode {
                    bytes,
                    links: metadata.nlink(),
                    seen: 0,
                    cache,
                }
            });
        inode.seen += 1;
        if inode.cache != cache {
            inode.cache = None;
        }
        Ok(())
    }
    let index = caches
        .iter()
        .enumerate()
        .map(|(index, cache)| (cache.path.as_path(), index))
        .collect::<HashMap<_, _>>();
    let mut inodes = HashMap::new();
    let mut usage = DiskUsage {
        total: 0,
        freeable: vec![0; caches.len()],
    };
    walk(path, None, &index, &mut inodes, &mut usage)?;
    for inode in inodes.values() {
        if let Some(cache) = inode.cache
            && inode.seen == inode.links
        {
            usage.freeable[cache] += inode.bytes;
        }
    }
    Ok(usage)
}

/// Removes `path` and everything under it. Builds can leave directories
/// without write permission behind, such as a test's read-only install of a
/// tool, which would make the removal fail: they are made writable first.
fn remove_tree(path: &Path) -> Result<(), String> {
    fn writable(path: &Path) -> std::io::Result<()> {
        let metadata = std::fs::symlink_metadata(path)?;
        if !metadata.is_dir() {
            return Ok(());
        }
        let mut permissions = metadata.permissions();
        if permissions.mode() & 0o700 != 0o700 {
            permissions.set_mode(permissions.mode() | 0o700);
            std::fs::set_permissions(path, permissions)?;
        }
        for entry in std::fs::read_dir(path)? {
            writable(&entry?.path())?;
        }
        Ok(())
    }
    writable(path)
        .and_then(|()| std::fs::remove_dir_all(path))
        .map_err(|error| format!("could not remove {}: {error}", path.display()))
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

        let size = disk_usage(&target, &[]).unwrap().total;
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
    fn a_cache_whose_files_are_linked_elsewhere_frees_less_and_more_caches_go() {
        let home = TempDir::new();
        let slot = SlotDirectory::new(&home.0, Path::new("/repo/.git"), 0);
        let target = slot.target();
        // The oldest cache's object file is also linked into deps, as rustc
        // does for debug info: removing that cache frees almost nothing.
        file(&target.join("debug/incremental/old-1/s-1/cgu.o"), 400_000);
        std::fs::create_dir_all(target.join("debug/deps")).unwrap();
        std::fs::hard_link(
            target.join("debug/incremental/old-1/s-1/cgu.o"),
            target.join("debug/deps/cgu.o"),
        )
        .unwrap();
        file(
            &target.join("debug/incremental/new-2/s-1/query.bin"),
            200_000,
        );
        set_modified(&target.join("debug/incremental/old-1"), 200);
        set_modified(&target.join("debug/incremental/new-2"), 100);
        let size = disk_usage(&target, &[]).unwrap().total;

        let limit = size - 150_000;
        let Pruning::Incremental {
            removed: 2, after, ..
        } = slot.enforce_limit(limit).unwrap()
        else {
            panic!("both caches go");
        };
        assert!(after <= limit, "{after} > {limit}");
        assert_eq!(after, disk_usage(&target, &[]).unwrap().total);
        assert!(target.join("debug/deps/cgu.o").exists());
    }

    #[test]
    fn a_target_with_read_only_directories_is_still_cleared() {
        let home = TempDir::new();
        let slot = SlotDirectory::new(&home.0, Path::new("/repo/.git"), 0);
        let target = slot.target();
        let locked = target.join("tmp/test-data/runtime");
        file(&locked.join("bin/tool"), 1000);
        file(&target.join("debug/deps/libbig.rlib"), 100_000);
        for directory in [locked.join("bin"), locked.clone()] {
            std::fs::set_permissions(&directory, std::fs::Permissions::from_mode(0o555)).unwrap();
        }
        assert!(matches!(
            slot.enforce_limit(1000),
            Ok(Pruning::Cleared { .. })
        ));
        assert!(!target.exists());
    }

    #[test]
    fn hard_linked_files_count_once() {
        let directory = TempDir::new();
        file(&directory.0.join("a/data"), 100_000);
        let single = disk_usage(&directory.0, &[]).unwrap().total;
        std::fs::create_dir(directory.0.join("b")).unwrap();
        std::fs::hard_link(directory.0.join("a/data"), directory.0.join("b/data")).unwrap();
        let linked = disk_usage(&directory.0, &[]).unwrap().total;
        assert!(linked < single + 50_000, "{single} then {linked}");
    }

    #[test]
    fn slots_are_found_again_with_their_record_and_tree() {
        let repository = repository();
        let home = TempDir::new();
        let scratch = TempDir::new();
        let source = resolve(&repository.0).unwrap();
        let tree = source.snapshot(&scratch.0).unwrap();
        let slot = SlotDirectory::new(&home.0, &source.repository, 1);
        assert_eq!(slot.read_record(), Ok(SlotRecord::default()));
        slot.materialize(&source.repository, &tree).unwrap();
        let record = SlotRecord {
            worktree: Some(repository.0.clone()),
            compiled: Vec::new(),
        };
        slot.write_record(&record).unwrap();

        let [existing] = &existing_slots(&home.0).unwrap()[..] else {
            panic!("one slot");
        };
        assert_eq!(existing.repository, source.repository);
        assert_eq!(existing.index, 1);
        assert_eq!(existing.directory.read_record(), Ok(record));
        assert_eq!(existing.directory.tree(), Ok(tree));
        existing.directory.remove().unwrap();
        assert!(existing_slots(&home.0).unwrap().is_empty());
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
