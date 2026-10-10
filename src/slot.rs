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
use std::time::{Duration, SystemTime};

use serde::{Deserialize, Serialize};

use crate::cargo::Compilation;
use crate::git;
use crate::log::log;
use crate::snapshot::Revision;

/// An incremental cache compiled this recently belongs to builds in use:
/// removing it makes their next compilation start from scratch.
const IN_USE: Duration = Duration::from_secs(10 * 60);

/// What a slot keeps on disk that its checkout and target do not tell: the
/// worktree it last built for, what its builds of each compilation used, and
/// the compiled units its target holds. A daemon reads it back when it
/// starts.
#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
pub(crate) struct SlotRecord {
    pub(crate) worktree: Option<PathBuf>,
    #[serde(default)]
    pub(crate) compilations: Vec<CompilationRun>,
    /// When a build last used each compiled unit, by [`unit_key`], in
    /// seconds since the Unix epoch.
    #[serde(default)]
    pub(crate) units: BTreeMap<String, u64>,
    /// The test binaries that passed in it, as later runs may skip them.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub(crate) passed: Vec<crate::passed::PassedBinary>,
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

    /// Keeps the slot's target within `limit` bytes of disk by removing what
    /// builds used longest ago first: incremental caches and compiled units.
    /// `used` says when a build last used each unit, by [`unit_key`]; a unit
    /// no build was seen using is aged by when it was written. Removing a
    /// unit only makes Cargo compile it again when a build needs it; the
    /// whole target goes only when nothing else is left to remove.
    ///
    /// Returns what it did and the units it removed. Only call this while
    /// no build runs in the slot.
    pub(crate) fn enforce_limit(
        &self,
        limit: u64,
        used: &HashMap<String, u64>,
    ) -> Result<(Pruning, Vec<String>), String> {
        let target = self.target();
        if !target.exists() {
            return Ok((Pruning::Within { size: 0 }, Vec::new()));
        }
        let items = evictables(&target, used)?;
        let usage = disk_usage(&target, &items)?;
        if usage.total <= limit {
            return Ok((Pruning::Within { size: usage.total }, Vec::new()));
        }
        let mut order = (0..items.len()).collect::<Vec<_>>();
        order.sort_by_key(|&index| items[index].used);
        // Space a removal frees for certain: files hard-linked from outside
        // the item (object files kept for debug info, uplifted binaries) stay
        // on disk.
        let mut estimate = usage.total;
        let (mut caches, mut in_use) = (0, 0);
        let mut units = Vec::new();
        let recent = SystemTime::now() - IN_USE;
        for index in order {
            if estimate <= limit {
                break;
            }
            let item = &items[index];
            for path in &item.paths {
                remove_path(path)?;
            }
            estimate = estimate.saturating_sub(usage.freeable[index]);
            match &item.unit {
                Some(unit) => units.push(unit.clone()),
                None => caches += 1,
            }
            if item.used > recent {
                in_use += 1;
            }
        }
        let after = disk_usage(&target, &[])?.total;
        if after <= limit {
            let pruning = Pruning::Evicted {
                before: usage.total,
                after,
                caches,
                units: units.len(),
                in_use,
            };
            return Ok((pruning, units));
        }
        remove_tree(&target)?;
        Ok((
            Pruning::Cleared {
                before: usage.total,
            },
            units,
        ))
    }
}

/// What a slot gave up to bring free disk back to the floor: how it was
/// kept, and the compiled units it lost; or why it could not be.
pub(crate) type Reclaimed = Result<(Pruning, Vec<String>), String>;

/// Frees `needed` bytes of disk from `slots`, each with when builds last
/// used its compiled units: what builds used longest ago goes first, across
/// all of them, and nothing a build used in the last ten minutes. Returns
/// what it did in each slot, in order. Only call this while no build runs
/// in any of them.
pub(crate) fn reclaim(
    slots: &[(SlotDirectory, HashMap<String, u64>)],
    needed: u64,
) -> Vec<Reclaimed> {
    let prepared = slots
        .iter()
        .map(|(slot, used)| {
            let target = slot.target();
            if !target.exists() {
                let nothing = DiskUsage::new(0);
                return Ok((Vec::new(), nothing));
            }
            let items = evictables(&target, used)?;
            let usage = disk_usage(&target, &items)?;
            Ok((items, usage))
        })
        .collect::<Vec<Result<_, String>>>();
    let recent = SystemTime::now() - IN_USE;
    let mut candidates = prepared
        .iter()
        .enumerate()
        .filter_map(|(slot, prepared)| prepared.as_ref().ok().map(|(items, _)| (slot, items)))
        .flat_map(|(slot, items)| {
            items
                .iter()
                .enumerate()
                .filter(|(_, item)| item.used <= recent)
                .map(move |(index, item)| (item.used, slot, index))
        })
        .collect::<Vec<_>>();
    candidates.sort_unstable();
    let mut freed = 0;
    let mut removed = vec![(0, Vec::new(), None); slots.len()];
    for (_, slot, index) in candidates {
        if freed >= needed {
            break;
        }
        let Ok((items, usage)) = &prepared[slot] else {
            unreachable!("only prepared slots have candidates");
        };
        let (caches, units, error) = &mut removed[slot];
        if error.is_some() {
            continue;
        }
        let item = &items[index];
        if let Err(failed) = item.paths.iter().try_for_each(|path| remove_path(path)) {
            *error = Some(failed);
            continue;
        }
        freed += usage.freeable[index];
        match &item.unit {
            Some(unit) => units.push(unit.clone()),
            None => *caches += 1,
        }
    }
    prepared
        .into_iter()
        .zip(removed)
        .zip(slots)
        .map(|((prepared, (caches, units, error)), (slot, _))| {
            let (_, usage) = prepared?;
            if let Some(error) = error {
                return Err(error);
            }
            if caches == 0 && units.is_empty() {
                return Ok((Pruning::Within { size: usage.total }, units));
            }
            let after = disk_usage(&slot.target(), &[])?.total;
            let pruning = Pruning::Evicted {
                before: usage.total,
                after,
                caches,
                units: units.len(),
                in_use: 0,
            };
            Ok((pruning, units))
        })
        .collect()
}

/// How many crates have their library compiled in more than one variant in
/// a profile of `target`: what builds selecting different features leave
/// behind, besides crates the lockfile holds in two versions.
///
/// # Errors
/// When the target cannot be read.
pub(crate) fn duplicated_crates(target: &Path) -> Result<usize, String> {
    if !target.exists() {
        return Ok(0);
    }
    let mut duplicated = 0;
    for (profile, _) in profile_directories(target)? {
        let deps = profile.join("deps");
        if !deps.is_dir() {
            continue;
        }
        let mut variants = BTreeMap::<String, usize>::new();
        for path in read_directory(&deps)? {
            let Some(name) = path.file_name().and_then(|name| name.to_str()) else {
                continue;
            };
            let Some(stem) = name
                .strip_prefix("lib")
                .and_then(|name| name.strip_suffix(".rlib"))
            else {
                continue;
            };
            if let Some((crate_name, hash)) = stem.rsplit_once('-')
                && unit_hash(name).is_some_and(|unit| unit == hash)
            {
                *variants.entry(crate_name.to_owned()).or_default() += 1;
            }
        }
        duplicated += variants.values().filter(|count| **count > 1).count();
    }
    Ok(duplicated)
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

/// What keeping a slot within its limit did.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum Pruning {
    /// The target was within the limit.
    Within { size: u64 },
    /// `caches` incremental caches and `units` compiled units went, `in_use`
    /// of them used within the last ten minutes.
    Evicted {
        before: u64,
        after: u64,
        caches: usize,
        units: usize,
        in_use: usize,
    },
    /// Nothing was left to remove; the target went.
    Cleared { before: u64 },
}

impl Pruning {
    /// The target's size afterwards.
    pub(crate) fn size(self) -> u64 {
        match self {
            Self::Within { size } | Self::Evicted { after: size, .. } => size,
            Self::Cleared { .. } => 0,
        }
    }

    /// Whether the limit is below what the slot's builds use: keeping to it
    /// took items in use, or the whole target.
    pub(crate) fn undersized(self) -> bool {
        matches!(
            self,
            Self::Evicted { in_use: 1.., .. } | Self::Cleared { .. }
        )
    }
}

/// The key of the compiled unit a file under `target` belongs to: its
/// profile directory and the hash Cargo names all of the unit's files with,
/// as in `debug/deps/libfoo-0123456789abcdef.rlib`,
/// `debug/.fingerprint/foo-0123456789abcdef` and
/// `debug/build/foo-0123456789abcdef/out`. None for files outside a unit,
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
            if !matches!(*part, "deps" | ".fingerprint" | "build") {
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

/// Something keeping a slot within its limit may remove: one compilation
/// unit's incremental cache, or one compiled unit's files.
pub(crate) struct Evictable {
    pub(crate) paths: Vec<PathBuf>,
    /// The compiled unit's key; None for an incremental cache.
    pub(crate) unit: Option<String>,
    /// When a build last used it, as far as is known.
    used: SystemTime,
}

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
        let Some(name) = name else { continue };
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

/// Everything under `target` keeping to a limit may remove.
pub(crate) fn evictables(
    target: &Path,
    used: &HashMap<String, u64>,
) -> Result<Vec<Evictable>, String> {
    let modified = |path: &Path| {
        std::fs::symlink_metadata(path)
            .and_then(|metadata| metadata.modified())
            .map_err(|error| format!("{}: {error}", path.display()))
    };
    let mut candidates = Vec::new();
    for (profile, _) in profile_directories(target)? {
        let incremental = profile.join("incremental");
        if real_directory(&incremental) {
            // rustc replaces a cache's session directory on every
            // incremental compilation of its unit.
            for path in read_directory(&incremental)? {
                let written = modified(&path)?;
                candidates.push((path, written));
            }
        }
        for kind in ["deps", ".fingerprint", "build"] {
            let directory = profile.join(kind);
            if !real_directory(&directory) {
                continue;
            }
            for path in read_directory(&directory)? {
                let written = modified(&path)?;
                candidates.push((path, written));
            }
        }
    }
    Ok(group_eviction_paths(target, candidates, used))
}

fn group_eviction_paths(
    target: &Path,
    candidates: Vec<(PathBuf, SystemTime)>,
    used: &HashMap<String, u64>,
) -> Vec<Evictable> {
    let mut items = Vec::new();
    let mut units = BTreeMap::<String, (Vec<PathBuf>, SystemTime)>::new();
    for (path, written) in candidates {
        if path
            .parent()
            .and_then(Path::file_name)
            .is_some_and(|name| name == "incremental")
        {
            items.push(Evictable {
                paths: vec![path],
                unit: None,
                used: written,
            });
        } else if let Some(key) = unit_key(target, &path) {
            let unit = units
                .entry(key)
                .or_insert_with(|| (Vec::new(), SystemTime::UNIX_EPOCH));
            unit.0.push(path);
            unit.1 = unit.1.max(written);
        }
    }
    for (key, (mut paths, written)) in units {
        paths.sort();
        let used = used.get(&key).map_or(written, |seconds| {
            SystemTime::UNIX_EPOCH + Duration::from_secs(*seconds)
        });
        items.push(Evictable {
            paths,
            unit: Some(key),
            used,
        });
    }
    items
}

/// The disk space under a directory.
pub(crate) struct DiskUsage {
    /// Every file counted once, however many hard links it has.
    total: u64,
    /// For each item asked about, what removing it frees: its directories
    /// and the files with no hard link outside it.
    pub(crate) freeable: Vec<u64>,
    inodes: HashMap<(u64, u64), Inode>,
}

impl DiskUsage {
    fn new(caches: usize) -> Self {
        Self {
            total: 0,
            freeable: vec![0; caches],
            inodes: HashMap::new(),
        }
    }

    fn record(&mut self, metadata: &std::fs::Metadata, cache: Option<usize>) {
        let bytes = metadata.blocks() * 512;
        if metadata.is_dir() {
            self.total += bytes;
            if let Some(cache) = cache {
                self.freeable[cache] += bytes;
            }
            return;
        }
        let inode = self
            .inodes
            .entry((metadata.dev(), metadata.ino()))
            .or_insert_with(|| {
                self.total += bytes;
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
    }

    fn finish(mut self) -> Self {
        for inode in self.inodes.values() {
            if let Some(cache) = inode.cache
                && inode.seen == inode.links
            {
                self.freeable[cache] += inode.bytes;
            }
        }
        self.inodes.clear();
        self
    }
}

/// One file's blocks and where its hard links were found.
struct Inode {
    bytes: u64,
    links: u64,
    seen: u64,
    /// The item holding every link seen so far, if one does.
    cache: Option<usize>,
}

/// The disk space under `path`, not following symbolic links. rustc's
/// incremental sessions and Cargo's artifacts are hard links, so each file
/// counts once.
pub(crate) fn disk_usage(path: &Path, caches: &[Evictable]) -> Result<DiskUsage, String> {
    fn walk(
        path: &Path,
        cache: Option<usize>,
        caches: &HashMap<&Path, usize>,
        usage: &mut DiskUsage,
    ) -> Result<(), String> {
        let cache = caches.get(path).copied().or(cache);
        let metadata = std::fs::symlink_metadata(path)
            .map_err(|error| format!("{}: {error}", path.display()))?;
        usage.record(&metadata, cache);
        if metadata.is_dir() {
            for child in read_directory(path)? {
                walk(&child, cache, caches, usage)?;
            }
        }
        Ok(())
    }
    let index = eviction_roots(caches);
    let mut usage = DiskUsage::new(caches.len());
    walk(path, None, &index, &mut usage)?;
    Ok(usage.finish())
}

fn eviction_roots(caches: &[Evictable]) -> HashMap<&Path, usize> {
    caches
        .iter()
        .enumerate()
        .flat_map(|(index, item)| item.paths.iter().map(move |path| (path.as_path(), index)))
        .collect()
}

/// Removes a file, or a directory with everything under it.
fn remove_path(path: &Path) -> Result<(), String> {
    if std::fs::symlink_metadata(path).is_ok_and(|metadata| metadata.is_dir()) {
        remove_tree(path)
    } else {
        std::fs::remove_file(path)
            .map_err(|error| format!("could not remove {}: {error}", path.display()))
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

    impl SlotDirectory {
        /// Keeps to `limit` with no record of use: units age by their files.
        fn enforce_limit_now(&self, limit: u64) -> Result<Pruning, String> {
            self.enforce_limit(limit, &HashMap::new())
                .map(|(pruning, _)| pruning)
        }
    }

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
        let target = SlotDirectory::new(&home.0, Path::new("/repo/.git"), 0).target();
        assert_eq!(duplicated_crates(&target), Ok(0));
        for name in [
            "debug/deps/libserde-0123456789abcdef.rlib",
            "debug/deps/libserde-fedcba9876543210.rlib",
            "debug/deps/libserde-fedcba9876543210.rmeta",
            "debug/deps/libone-0123456789abcdef.rlib",
            "release/deps/libserde-0123456789abcdef.rlib",
        ] {
            file(&target.join(name), 10);
        }
        assert_eq!(duplicated_crates(&target), Ok(1));
    }

    #[test]
    fn reclaiming_takes_what_builds_used_longest_ago_across_slots_but_nothing_in_use() {
        let home = TempDir::new();
        let slots = [0, 1].map(|index| SlotDirectory::new(&home.0, Path::new("/repo/.git"), index));
        // Slot 0 holds an old and a recent cache, slot 1 a middle-aged one.
        let cache = |slot: &SlotDirectory, name: &str, age: u64| {
            let path = slot.target().join(format!("debug/incremental/{name}"));
            file(&path.join("s-1/query.bin"), 200_000);
            set_modified(&path, age);
            path
        };
        let old = cache(&slots[0], "old", 3600);
        let recent = cache(&slots[0], "recent", 10);
        let middle = cache(&slots[1], "middle", 1800);
        let with_use = slots.map(|slot| (slot, HashMap::new()));

        // About one cache's worth: the oldest goes.
        let results = reclaim(&with_use, 150_000);
        assert!(!old.exists() && middle.exists() && recent.exists());
        assert!(matches!(
            results[0],
            Ok((Pruning::Evicted { caches: 1, .. }, _))
        ));
        assert!(matches!(results[1], Ok((Pruning::Within { .. }, _))));
        // Far more than there is: everything but what is in use goes.
        let results = reclaim(&with_use, u64::MAX);
        assert!(!middle.exists() && recent.exists());
        assert!(matches!(
            results[1],
            Ok((
                Pruning::Evicted {
                    caches: 1,
                    in_use: 0,
                    ..
                },
                _
            ))
        ));
        assert!(free_disk(&home.0).unwrap() > 0);
    }

    #[test]
    fn a_target_over_its_limit_loses_the_least_recently_compiled_caches_first() {
        let home = TempDir::new();
        let slot = SlotDirectory::new(&home.0, Path::new("/repo/.git"), 0);
        let target = slot.target();
        assert_eq!(slot.enforce_limit_now(0), Ok(Pruning::Within { size: 0 }));
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
        assert_eq!(slot.enforce_limit_now(size), Ok(Pruning::Within { size }));
        // Two caches of about 200 kB must go: the cross-compiled one and old-1.
        let Pruning::Evicted {
            caches: 2,
            units: 0,
            after,
            ..
        } = slot.enforce_limit_now(size - 300_000).unwrap()
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
            slot.enforce_limit_now(100_000),
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
        let Pruning::Evicted {
            caches: 2,
            units: 0,
            after,
            ..
        } = slot.enforce_limit_now(limit).unwrap()
        else {
            panic!("both caches go");
        };
        assert!(after <= limit, "{after} > {limit}");
        assert_eq!(after, disk_usage(&target, &[]).unwrap().total);
        assert!(target.join("debug/deps/cgu.o").exists());
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
    fn stale_compiled_units_go_before_used_ones_and_their_keys_are_returned() {
        let home = TempDir::new();
        let slot = SlotDirectory::new(&home.0, Path::new("/repo/.git"), 0);
        let target = slot.target();
        let unit = |name: &str, hash: &str, bytes: usize| {
            file(
                &target.join(format!("debug/deps/lib{name}-{hash}.rlib")),
                bytes,
            );
            file(
                &target.join(format!("debug/.fingerprint/{name}-{hash}/lib-{name}")),
                100,
            );
        };
        unit("used", "1111111111111111", 200_000);
        unit("stale", "2222222222222222", 200_000);
        // Never seen used: aged by its files, written long ago.
        unit("unrecorded", "3333333333333333", 200_000);
        for written in [
            "debug/deps/libunrecorded-3333333333333333.rlib",
            "debug/.fingerprint/unrecorded-3333333333333333",
        ] {
            set_modified(&target.join(written), 30 * 24 * 3600);
        }
        let now = SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .unwrap()
            .as_secs();
        let used = HashMap::from([
            ("debug/1111111111111111".to_owned(), now),
            ("debug/2222222222222222".to_owned(), now - 7 * 24 * 3600),
        ]);
        let size = disk_usage(&target, &[]).unwrap().total;

        let (pruning, evicted) = slot.enforce_limit(size - 300_000, &used).unwrap();
        assert!(
            matches!(
                pruning,
                Pruning::Evicted {
                    caches: 0,
                    units: 2,
                    in_use: 0,
                    ..
                }
            ),
            "{pruning:?}"
        );
        assert_eq!(
            evicted,
            ["debug/3333333333333333", "debug/2222222222222222"]
        );
        assert!(
            target
                .join("debug/deps/libused-1111111111111111.rlib")
                .exists()
        );
        assert!(
            target
                .join("debug/.fingerprint/used-1111111111111111")
                .exists()
        );
        assert!(
            !target
                .join("debug/.fingerprint/stale-2222222222222222")
                .exists()
        );
        assert!(
            !target
                .join("debug/deps/libunrecorded-3333333333333333.rlib")
                .exists()
        );
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
            slot.enforce_limit_now(1000),
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
            units: [("debug/0123456789abcdef".to_owned(), 7)].into(),
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
