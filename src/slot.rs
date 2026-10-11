//! Build slots: a checkout at a fixed path and the target directory only it
//! uses.
//!
//! A slot is an ordinary git repository whose objects come from the source
//! repository through `objects/info/alternates`, so checking out a snapshot
//! copies nothing but the files that differ from the slot's previous
//! checkout. Cargo then sees ordinary edits, and the slot's path, and with it
//! every fingerprint in its target, never changes.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::os::unix::ffi::OsStringExt as _;
use std::os::unix::fs::{MetadataExt as _, PermissionsExt as _};
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use serde::{Deserialize, Serialize};

use crate::cargo::Compilation;
use crate::git;
use crate::log::log;
use crate::snapshot::Revision;

/// What a slot keeps on disk that its checkout and target do not tell: the
/// worktree it last built for, what its builds of each compilation used, and
/// the compiled units its target holds. A daemon reads it back when it
/// starts.
#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
pub(crate) struct SlotRecord {
    pub(crate) worktree: Option<PathBuf>,
    #[serde(default)]
    pub(crate) compilations: Vec<CompilationRun>,
    /// When a build last used each compiled unit, by [`unit_key`].
    #[serde(default)]
    pub(crate) units: BTreeMap<String, LastUse>,
    /// The test binaries that passed in it, as later runs may skip them.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub(crate) passed: Vec<crate::passed::PassedBinary>,
}

/// When a build last used a compiled unit: when that build ended, in seconds
/// since the Unix epoch, and how many of the units it used Cargo reported
/// after this one. Cargo reports a unit after every unit it depends on, so
/// among the units one build used last, those with fewer after them go
/// first: removing a unit makes everything depending on it compile again,
/// so it outlasts them.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Ord, PartialOrd, Serialize, Deserialize)]
pub(crate) struct LastUse {
    pub(crate) at: u64,
    pub(crate) after: u32,
}

/// The compiled units, by [`unit_key`], that a slot's latest build of
/// `compilation` used, `at` seconds since the Unix epoch. Unit keys name the
/// same unit in every slot of a repository, so this is what the compilation
/// needs wherever it runs.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub(crate) struct CompilationRun {
    pub(crate) compilation: Compilation,
    pub(crate) at: u64,
    pub(crate) units: BTreeSet<String>,
    /// The wall time of the latest successful build, in milliseconds.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) build_ms: Option<u64>,
    /// The peak memory of the latest build, its processes summed, in bytes.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) peak_memory: Option<u64>,
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
            log!(
                "{} names no repository; leaving it alone",
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

    pub(crate) fn path(&self) -> &Path {
        &self.0
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

        let commit = git::snapshot_commit(git::command(&source), revision, None)?;
        let mut reset = git::command(&source);
        reset.args(["reset", "-q", "--hard", &commit]);
        git::run(reset)?;
        let mut clean = git::command(&source);
        clean.args(["clean", "-q", "-ffdx"]);
        git::run(clean)?;
        Ok(())
    }

    /// What the slot's target holds: everything eviction may remove, with
    /// what removing each frees, and the disk it takes in all. Only call
    /// this while no build runs in the slot.
    pub(crate) fn inventory(&self) -> Result<Inventory, String> {
        let target = self.target();
        if !target.exists() {
            return Ok(Inventory::default());
        }
        let mut items = evictables(&target)?;
        let usage = disk_usage(&target, &items)?;
        for (item, freeable) in items.iter_mut().zip(usage.freeable) {
            item.freeable = freeable;
        }
        Ok(Inventory {
            items,
            total: usage.total,
        })
    }
}

/// What a slot's target holds: everything eviction may remove, and the disk
/// the target takes in all, every file counted once however many hard links
/// it has. It stays true while no build runs in the slot.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub(crate) struct Inventory {
    pub(crate) items: Vec<Evictable>,
    pub(crate) total: u64,
}

impl Inventory {
    /// How many crates have their library compiled in more than one variant
    /// in a profile: what builds selecting different features leave behind,
    /// besides crates the lockfile holds in two versions.
    pub(crate) fn duplicated_crates(&self) -> usize {
        let mut variants = BTreeMap::<(&str, &str), usize>::new();
        for item in &self.items {
            let Kind::Unit(key) = &item.kind else {
                continue;
            };
            let (profile, _) = key.rsplit_once('/').expect("a unit key names its profile");
            let library = item.paths.iter().find_map(|path| {
                let in_deps = path
                    .parent()
                    .and_then(Path::file_name)
                    .is_some_and(|parent| parent == "deps");
                let (name, _) = path
                    .file_name()?
                    .to_str()?
                    .strip_prefix("lib")?
                    .strip_suffix(".rlib")?
                    .rsplit_once('-')?;
                in_deps.then_some(name)
            });
            if let Some(name) = library {
                *variants.entry((profile, name)).or_default() += 1;
            }
        }
        variants.values().filter(|count| **count > 1).count()
    }
}

/// Something eviction may remove from a target.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct Evictable {
    pub(crate) paths: Vec<PathBuf>,
    pub(crate) kind: Kind,
    /// When it was last written: how long ago builds used it, unless they
    /// are known to have used it later.
    pub(crate) written: SystemTime,
    /// What removing it frees for certain: its directories and the files
    /// with no hard link outside it. Object files kept for debug info and
    /// uplifted binaries are such links.
    pub(crate) freeable: u64,
}

/// What an [`Evictable`] is.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum Kind {
    /// One compilation unit's incremental cache: removing it makes the
    /// unit's next compilation start from scratch.
    Cache,
    /// One compiled unit's files, by [`unit_key`]: a build that needs the
    /// unit compiles it again.
    Unit(String),
    /// A directory or file a test left in the target's `tmp` directory
    /// (`CARGO_TARGET_TMPDIR`), whose content tests manage themselves.
    TestFiles,
}

/// Removes `paths`, files or directories with everything under them; one
/// already gone counts as removed.
///
/// # Errors
/// When one cannot be removed; the ones before it are gone.
pub(crate) fn remove(paths: &[PathBuf]) -> Result<(), String> {
    paths.iter().try_for_each(|path| remove_path(path))
}

/// The disk space free for unprivileged use on the volume holding `path`.
///
/// # Errors
/// When the volume cannot be queried.
pub(crate) fn free_disk(path: &Path) -> Result<u64, String> {
    let stat = rustix::fs::statvfs(path)
        .map_err(|error| format!("could not query free disk at {}: {error}", path.display()))?;
    Ok(stat.f_bavail.saturating_mul(stat.f_frsize))
}

/// The directories of a profile whose entries are named with the hash of the
/// compiled unit they belong to.
pub(crate) const UNIT_DIRECTORIES: [&str; 4] = ["deps", ".fingerprint", "build", "examples"];

/// The key of the compiled unit a file under `target` belongs to: its
/// profile directory and the hash Cargo names all of the unit's files with,
/// as in `debug/deps/libfoo-0123456789abcdef.rlib`,
/// `debug/.fingerprint/foo-0123456789abcdef`,
/// `debug/build/foo-0123456789abcdef/out` and
/// `debug/examples/foo-0123456789abcdef`. None for files outside a unit,
/// such as uplifted binaries.
pub(crate) fn unit_key(target: &Path, path: &Path) -> Option<String> {
    let relative = path.strip_prefix(target).ok()?;
    let parts = relative
        .components()
        .map(|part| part.as_os_str().to_str())
        .collect::<Option<Vec<_>>>()?;
    // Cargo's kind directory follows a profile, optionally preceded by a
    // target triple. A custom profile itself may be named `build` or `deps`.
    parts
        .iter()
        .enumerate()
        .skip(1)
        .take(2)
        .find_map(|(at, part)| {
            if !UNIT_DIRECTORIES.contains(part) {
                return None;
            }
            let hash = unit_hash(parts.get(at + 1)?)?;
            Some(format!("{}/{hash}", parts[..at].join("/")))
        })
}

/// The 16-digit hash in a unit's file or directory name: after its last
/// `-`, before any extension.
fn unit_hash(name: &str) -> Option<&str> {
    let stem = name.split('.').next()?;
    let hash = stem.rsplit_once('-')?.1;
    (hash.len() == 16 && hash.bytes().all(|byte| byte.is_ascii_hexdigit())).then_some(hash)
}

/// The directory of a target where tests keep files (`CARGO_TARGET_TMPDIR`).
const TEST_FILES: &str = "tmp";

/// The profile directories under `target`, for the host and each target
/// triple: `<target>/[<triple>/]<profile>`, each with the target-relative
/// name used in unit keys.
fn real_directory(path: &Path) -> bool {
    std::fs::symlink_metadata(path).is_ok_and(|metadata| metadata.is_dir())
}

fn profile_directories(target: &Path) -> Result<Vec<(PathBuf, String)>, String> {
    if !real_directory(target) {
        return Err(format!(
            "{} is not a real target directory",
            target.display()
        ));
    }
    let is_profile = |path: &Path| {
        ["deps", ".fingerprint", "incremental"]
            .iter()
            .any(|part| real_directory(&path.join(part)))
    };
    let mut profiles = Vec::new();
    for entry in read_directory(target)? {
        let name = entry
            .file_name()
            .and_then(|name| name.to_str())
            .map(str::to_owned);
        let Some(name) = name.filter(|name| name != TEST_FILES) else {
            continue;
        };
        if real_directory(&entry) && is_profile(&entry) {
            profiles.push((entry, name));
        } else if real_directory(&entry) {
            for profile in read_directory(&entry)? {
                if real_directory(&profile)
                    && is_profile(&profile)
                    && let Some(inner) = profile.file_name().and_then(|inner| inner.to_str())
                {
                    let relative = format!("{name}/{inner}");
                    profiles.push((profile, relative));
                }
            }
        }
    }
    Ok(profiles)
}

/// Everything under `target` eviction may remove; what each frees is
/// filled in later.
fn evictables(target: &Path) -> Result<Vec<Evictable>, String> {
    let modified = |path: &Path| {
        std::fs::symlink_metadata(path)
            .and_then(|metadata| metadata.modified())
            .map_err(|error| format!("{}: {error}", path.display()))
    };
    let mut items = Vec::new();
    for (profile, relative) in profile_directories(target)? {
        let incremental = profile.join("incremental");
        if real_directory(&incremental) {
            // rustc replaces a cache's session directory on every
            // incremental compilation of its unit.
            for path in read_directory(&incremental)? {
                items.push(Evictable {
                    written: modified(&path)?,
                    paths: vec![path],
                    kind: Kind::Cache,
                    freeable: 0,
                });
            }
        }
        let mut units = BTreeMap::<String, (Vec<PathBuf>, SystemTime)>::new();
        for kind in UNIT_DIRECTORIES {
            let directory = profile.join(kind);
            if !real_directory(&directory) {
                continue;
            }
            for path in read_directory(&directory)? {
                let Some(hash) = path
                    .file_name()
                    .and_then(|name| name.to_str())
                    .and_then(unit_hash)
                else {
                    continue;
                };
                let written = modified(&path)?;
                let unit = units
                    .entry(format!("{relative}/{hash}"))
                    .or_insert_with(|| (Vec::new(), SystemTime::UNIX_EPOCH));
                unit.0.push(path);
                unit.1 = unit.1.max(written);
            }
        }
        for (key, (paths, written)) in units {
            items.push(Evictable {
                paths,
                kind: Kind::Unit(key),
                written,
                freeable: 0,
            });
        }
    }
    let test_files = target.join(TEST_FILES);
    if real_directory(&test_files) {
        for path in read_directory(&test_files)? {
            items.push(Evictable {
                written: modified(&path)?,
                paths: vec![path],
                kind: Kind::TestFiles,
                freeable: 0,
            });
        }
    }
    Ok(items)
}

/// The disk space under a directory.
struct DiskUsage {
    /// Every file counted once, however many hard links it has.
    total: u64,
    /// For each item asked about, what removing it frees: its directories
    /// and the files with no hard link outside it.
    freeable: Vec<u64>,
}

/// One file's blocks and where its hard links were found.
struct Inode {
    bytes: u64,
    links: u64,
    seen: u64,
    /// The item holding every link seen so far, if one does.
    item: Option<usize>,
}

/// The disk space under `path`, not following symbolic links. rustc's
/// incremental sessions and Cargo's artifacts are hard links, so each file
/// counts once.
fn disk_usage(path: &Path, items: &[Evictable]) -> Result<DiskUsage, String> {
    fn walk(
        path: &Path,
        item: Option<usize>,
        items: &HashMap<&Path, usize>,
        inodes: &mut HashMap<(u64, u64), Inode>,
        usage: &mut DiskUsage,
    ) -> Result<(), String> {
        let item = items.get(path).copied().or(item);
        let metadata = std::fs::symlink_metadata(path)
            .map_err(|error| format!("{}: {error}", path.display()))?;
        let bytes = metadata.blocks() * 512;
        if metadata.is_dir() {
            usage.total += bytes;
            if let Some(item) = item {
                usage.freeable[item] += bytes;
            }
            for child in read_directory(path)? {
                walk(&child, item, items, inodes, usage)?;
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
                    item,
                }
            });
        inode.seen += 1;
        if inode.item != item {
            inode.item = None;
        }
        Ok(())
    }
    let index = items
        .iter()
        .enumerate()
        .flat_map(|(index, item)| item.paths.iter().map(move |path| (path.as_path(), index)))
        .collect::<HashMap<_, _>>();
    let mut inodes = HashMap::new();
    let mut usage = DiskUsage {
        total: 0,
        freeable: vec![0; items.len()],
    };
    walk(path, None, &index, &mut inodes, &mut usage)?;
    for inode in inodes.values() {
        if let Some(item) = inode.item
            && inode.seen == inode.links
        {
            usage.freeable[item] += inode.bytes;
        }
    }
    Ok(usage)
}

/// Removes a file, or a directory with everything under it; one already
/// gone counts as removed.
fn remove_path(path: &Path) -> Result<(), String> {
    match std::fs::symlink_metadata(path) {
        Ok(metadata) if metadata.is_dir() => remove_tree(path),
        Ok(_) => std::fs::remove_file(path)
            .map_err(|error| format!("could not remove {}: {error}", path.display())),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(format!("could not remove {}: {error}", path.display())),
    }
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
pub(crate) fn project_name(repository: &Path) -> String {
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
    fn crates_compiled_in_several_variants_are_counted_per_profile() {
        let home = TempDir::new();
        let slot = SlotDirectory::new(&home.0, Path::new("/repo/.git"), 0);
        let target = slot.target();
        assert_eq!(slot.inventory().unwrap().duplicated_crates(), 0);
        for name in [
            "debug/deps/libserde-0123456789abcdef.rlib",
            "debug/deps/libserde-fedcba9876543210.rlib",
            "debug/deps/libserde-fedcba9876543210.rmeta",
            "debug/deps/libone-0123456789abcdef.rlib",
            "release/deps/libserde-0123456789abcdef.rlib",
        ] {
            file(&target.join(name), 10);
        }
        assert_eq!(slot.inventory().unwrap().duplicated_crates(), 1);
    }

    /// The inventory's items by what they are, with the file names of their
    /// paths, in a stable order.
    fn listed(inventory: &Inventory) -> Vec<(String, Vec<String>)> {
        let mut items = inventory
            .items
            .iter()
            .map(|item| {
                let kind = match &item.kind {
                    Kind::Cache => "cache".to_owned(),
                    Kind::Unit(key) => format!("unit {key}"),
                    Kind::TestFiles => "test files".to_owned(),
                };
                let mut names = item
                    .paths
                    .iter()
                    .map(|path| path.file_name().unwrap().to_string_lossy().into_owned())
                    .collect::<Vec<_>>();
                names.sort();
                (kind, names)
            })
            .collect::<Vec<_>>();
        items.sort();
        items
    }

    #[test]
    fn an_inventory_lists_caches_units_and_test_files_of_every_profile() {
        let home = TempDir::new();
        let slot = SlotDirectory::new(&home.0, Path::new("/repo/.git"), 0);
        assert_eq!(slot.inventory(), Ok(Inventory::default()));
        let target = slot.target();
        for name in [
            "debug/incremental/app-1abc/s-1/query.bin",
            "debug/deps/libapp-0123456789abcdef.rlib",
            "debug/deps/libapp-0123456789abcdef.rmeta",
            "debug/.fingerprint/app-0123456789abcdef/lib-app",
            "debug/build/app-fedcba9876543210/out/generated.rs",
            "debug/examples/demo-1111111111111111",
            "debug/examples/demo-1111111111111111.demo.1a2b-cgu.0.rcgu.o",
            "debug/examples/demo",
            "debug/app",
            "aarch64-apple-darwin/release/incremental/x-2def/s-1/q.bin",
            "tmp/fixture-4242/data.json",
            "CACHEDIR.TAG",
        ] {
            file(&target.join(name), 1000);
        }
        set_modified(&target.join("debug/incremental/app-1abc"), 3600);
        let inventory = slot.inventory().unwrap();
        assert_eq!(
            listed(&inventory),
            [
                ("cache".to_owned(), vec!["app-1abc".to_owned()]),
                ("cache".to_owned(), vec!["x-2def".to_owned()]),
                ("test files".to_owned(), vec!["fixture-4242".to_owned()]),
                (
                    "unit debug/0123456789abcdef".to_owned(),
                    vec![
                        "app-0123456789abcdef".to_owned(),
                        "libapp-0123456789abcdef.rlib".to_owned(),
                        "libapp-0123456789abcdef.rmeta".to_owned(),
                    ]
                ),
                (
                    "unit debug/1111111111111111".to_owned(),
                    vec![
                        "demo-1111111111111111".to_owned(),
                        "demo-1111111111111111.demo.1a2b-cgu.0.rcgu.o".to_owned(),
                    ]
                ),
                (
                    "unit debug/fedcba9876543210".to_owned(),
                    vec!["app-fedcba9876543210".to_owned()]
                ),
            ]
        );
        assert_eq!(inventory.total, disk_usage(&target, &[]).unwrap().total);
        let cache = inventory
            .items
            .iter()
            .find(|item| item.paths[0].ends_with("app-1abc"))
            .unwrap();
        let hour = std::time::Duration::from_secs(3600);
        assert!(cache.written <= SystemTime::now() - hour + std::time::Duration::from_secs(5));
        assert!(cache.freeable >= 1000);
    }

    #[test]
    fn an_item_frees_only_the_files_with_no_link_outside_it() {
        let home = TempDir::new();
        let slot = SlotDirectory::new(&home.0, Path::new("/repo/.git"), 0);
        let target = slot.target();
        // The cache's object file is also linked into deps, as rustc does
        // for debug info: removing the cache alone frees only its query file.
        file(
            &target.join("debug/incremental/app-1abc/s-1/cgu.o"),
            400_000,
        );
        file(
            &target.join("debug/incremental/app-1abc/s-1/query.bin"),
            100_000,
        );
        std::fs::create_dir_all(target.join("debug/deps")).unwrap();
        std::fs::hard_link(
            target.join("debug/incremental/app-1abc/s-1/cgu.o"),
            target.join("debug/deps/app-0123456789abcdef.cgu.o"),
        )
        .unwrap();
        let inventory = slot.inventory().unwrap();
        let freeable = |name: &str| {
            inventory
                .items
                .iter()
                .find(|item| item.paths.iter().any(|path| path.ends_with(name)))
                .unwrap()
                .freeable
        };
        let cache = freeable("app-1abc");
        assert!((100_000..400_000).contains(&cache), "{cache}");
        assert!(freeable("app-0123456789abcdef.cgu.o") < 100_000);
        assert!(inventory.total >= 500_000 && inventory.total < 900_000);
    }

    #[test]
    fn units_are_named_by_profile_and_hash() {
        let target = Path::new("/slot/target");
        for (path, key) in [
            (
                "debug/deps/libapp_model-0123456789abcdef.rlib",
                Some("debug/0123456789abcdef"),
            ),
            (
                "debug/deps/app-fedcba9876543210",
                Some("debug/fedcba9876543210"),
            ),
            (
                "debug/build/foo-00112233aabbccdd/out",
                Some("debug/00112233aabbccdd"),
            ),
            (
                "debug/.fingerprint/foo-00112233aabbccdd",
                Some("debug/00112233aabbccdd"),
            ),
            (
                "aarch64-apple-darwin/release/deps/libx-0123456789abcdef.rmeta",
                Some("aarch64-apple-darwin/release/0123456789abcdef"),
            ),
            (
                "debug/examples/demo-0123456789abcdef",
                Some("debug/0123456789abcdef"),
            ),
            ("debug/examples/demo", None),
            (
                "build/deps/libapp-0123456789abcdef.rlib",
                Some("build/0123456789abcdef"),
            ),
            (
                "deps/.fingerprint/app-0123456789abcdef",
                Some("deps/0123456789abcdef"),
            ),
            (
                "x86_64-unknown-linux-gnu/build/build/app-0123456789abcdef/out",
                Some("x86_64-unknown-linux-gnu/build/0123456789abcdef"),
            ),
            ("deps/app-0123456789abcdef", None),
            (
                "debug/out/deps/app-0123456789abcdef",
                Some("debug/out/0123456789abcdef"),
            ),
            ("debug/app", None),
            ("debug/deps/libnohash.rlib", None),
            ("debug/deps/libshort-0123.rlib", None),
        ] {
            assert_eq!(
                unit_key(target, &target.join(path)).as_deref(),
                key,
                "{path}"
            );
        }
        assert_eq!(
            unit_key(
                target,
                Path::new("/elsewhere/debug/deps/libx-0123456789abcdef.rlib")
            ),
            None
        );
    }

    #[test]
    fn removing_an_items_paths_takes_read_only_directories_too() {
        let home = TempDir::new();
        let slot = SlotDirectory::new(&home.0, Path::new("/repo/.git"), 0);
        let target = slot.target();
        let locked = target.join("tmp/test-data/runtime");
        file(&locked.join("bin/tool"), 1000);
        file(
            &target.join("debug/deps/libbig-0123456789abcdef.rlib"),
            1000,
        );
        for directory in [locked.join("bin"), locked.clone()] {
            std::fs::set_permissions(&directory, std::fs::Permissions::from_mode(0o555)).unwrap();
        }
        let paths = slot
            .inventory()
            .unwrap()
            .items
            .into_iter()
            .flat_map(|item| item.paths)
            .collect::<Vec<_>>();
        remove(&paths).unwrap();
        assert!(!target.join("tmp/test-data").exists());
        assert!(
            !target
                .join("debug/deps/libbig-0123456789abcdef.rlib")
                .exists()
        );
        // Paths already gone count as removed.
        remove(&paths).unwrap();
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
            compilations: vec![CompilationRun {
                compilation: Compilation::new(
                    Path::new("crates/x"),
                    &crate::cargo::Operation {
                        command: crate::cargo::Command::Test,
                        args: vec!["--lib".into()],
                        rustflags: Vec::new(),
                    },
                ),
                at: 7,
                units: ["debug/0123456789abcdef".to_owned()].into(),
                build_ms: Some(4200),
                peak_memory: None,
            }],
            units: [(
                "debug/0123456789abcdef".to_owned(),
                LastUse { at: 7, after: 0 },
            )]
            .into(),
            passed: Vec::new(),
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
        let a = slot_name(Path::new("/work/app/.git"), 0);
        let b = slot_name(Path::new("/other/app/.git"), 1);
        assert!(a.starts_with("app-") && a.ends_with("/0"), "{a}");
        assert!(b.starts_with("app-") && b.ends_with("/1"), "{b}");
        assert_ne!(a.trim_end_matches("/0"), b.trim_end_matches("/1"));
        assert!(slot_name(Path::new("/srv/bare.git"), 0).starts_with("bare.git-"));
    }
}
