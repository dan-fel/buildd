//! Bounded, service-issued cache selection and approval receipts.
//!
//! This module never accepts filesystem paths. The scheduler owns artifact
//! exclusion; receipts own approvals, not slots. A disconnected executor keeps
//! running and its exact receipt can be queried until expiry. A new daemon has
//! no knowledge of an old daemon's outcomes and refuses its receipts.

use std::collections::{BTreeMap, HashMap, VecDeque};
use std::io::Read as _;
use std::os::unix::fs::MetadataExt as _;
use std::path::{Path, PathBuf};
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

use crate::scheduler::{Distance, Effect, Scheduler};
use crate::slot::{self, SlotDirectory};

const MAX_ENTRIES: usize = 8192;
const MAX_PATH_BYTES: usize = 512 * 1024;
const MAX_ITEMS: usize = 64;
const MAX_INVENTORY_PAGE_ITEMS: usize = 64;
const MAX_SLOTS: usize = 16;
const MAX_RECEIPTS: usize = 8;
const TTL: Duration = Duration::from_secs(60);
const SCAN_TIME: Duration = Duration::from_millis(250);

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct Owner {
    pub host: String,
    pub incarnation: String,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(tag = "operation", rename_all = "snake_case")]
pub enum Operation {
    Capabilities,
    Slots {
        owner: Owner,
        after: Option<String>,
    },
    Inventory {
        owner: Owner,
        slots: Vec<String>,
    },
    ReleaseInventory {
        owner: Owner,
        inventory: String,
    },
    InventoryPage {
        owner: Owner,
        inventory: String,
        after: String,
    },
    Preview {
        owner: Owner,
        inventory: String,
        items: Vec<String>,
    },
    PreviewStatus {
        owner: Owner,
        preparation: String,
    },
    Execute {
        preview: Preview,
    },
    Receipt {
        preview: Preview,
    },
    Cancel {
        preview: Preview,
    },
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct Capabilities {
    pub owner: Owner,
    pub protocol: u32,
    pub max_slots: usize,
    pub max_items: usize,
    pub max_inventory_page_items: usize,
    pub max_entries: usize,
    pub max_path_bytes: usize,
    pub scan_ms: u64,
    pub receipt_ttl_ms: u64,
    pub max_receipts: usize,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct Inventory {
    pub owner: Owner,
    pub revision: String,
    pub measured_ms: u64,
    pub expires_ms: u64,
    pub complete: bool,
    pub slots: Vec<Slot>,
    pub items: Vec<Item>,
    /// Service-issued cursor for scan progress or the next completed-item page.
    pub next: Option<String>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct Slot {
    pub id: String,
    /// Busy slots have no inventory: cached status sizes are never substituted.
    pub protected: bool,
    pub incomplete: Option<Refusal>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct Item {
    pub id: String,
    pub slot: String,
    pub unit: Option<String>,
    pub reclaimable_bytes: Option<u64>,
    pub incomplete: Option<Refusal>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct Preview {
    pub owner: Owner,
    pub token: String,
    pub inventory: String,
    pub expires_ms: u64,
    pub items: Vec<Item>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(tag = "outcome", rename_all = "snake_case")]
pub enum Outcome {
    Completed {
        removed: Vec<String>,
    },
    Partial {
        removed: Vec<String>,
        failed: String,
        reason: Refusal,
    },
    Cancelled,
    Refused {
        reason: Refusal,
    },
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Refusal {
    ForeignOwner,
    UnknownReceipt,
    Expired,
    PayloadChanged,
    Busy,
    Changed,
    Protected,
    Limit,
    Io,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Response {
    Capabilities(Capabilities),
    Slots {
        owner: Owner,
        slots: Vec<Slot>,
        next: Option<String>,
    },
    Inventory(Inventory),
    Released {
        inventory: String,
    },
    Preview(Preview),
    Preparing {
        preparation: String,
    },
    Receipt {
        preview: Preview,
        result: Option<Outcome>,
        executing: bool,
    },
    /// UnknownReceipt/ForeignOwner do not claim an earlier execute failed.
    Refused {
        reason: Refusal,
    },
    /// No operation is executed, and an earlier attempt's outcome is unknown.
    Unknown {
        reason: Refusal,
    },
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct Fingerprint {
    dev: u64,
    inode: u64,
    mode: u32,
    len: u64,
    links: u64,
    blocks: u64,
    modified: (i64, i64),
    changed: (i64, i64),
}

fn fingerprint(metadata: &std::fs::Metadata) -> Fingerprint {
    Fingerprint {
        dev: metadata.dev(),
        inode: metadata.ino(),
        mode: metadata.mode(),
        len: metadata.len(),
        links: metadata.nlink(),
        blocks: metadata.blocks(),
        modified: (metadata.mtime(), metadata.mtime_nsec()),
        changed: (metadata.ctime(), metadata.ctime_nsec()),
    }
}

/// A Cargo artifact group, not a caller-supplied pathname. Compiled units
/// include every matching hash in the profile's Cargo kind directories.
#[derive(Clone, Debug, Eq, PartialEq, Ord, PartialOrd, Hash)]
enum Scope {
    Unit(String),
    Incremental(PathBuf),
}

impl Scope {
    fn path_bytes(&self) -> usize {
        match self {
            Self::Unit(key) => key.len(),
            Self::Incremental(path) => path.as_os_str().len(),
        }
    }
    fn profile(&self) -> &Path {
        match self {
            Self::Unit(key) => Path::new(key).parent().expect("unit has a profile"),
            Self::Incremental(path) => path
                .parent()
                .and_then(Path::parent)
                .expect("incremental has a profile"),
        }
    }
    fn unit(&self) -> Option<String> {
        match self {
            Self::Unit(key) => Some(key.clone()),
            Self::Incremental(_) => None,
        }
    }
}

#[derive(Debug)]
struct Snapshot {
    root: PathBuf,
    ancestors: BTreeMap<PathBuf, std::fs::Metadata>,
    roots: BTreeMap<Scope, Vec<PathBuf>>,
    entries: BTreeMap<PathBuf, std::fs::Metadata>,
}

#[derive(Debug)]
struct Measurement {
    identity: usize,
    bytes: u64,
    incomplete: Option<Refusal>,
}

#[derive(Debug)]
struct Link {
    bytes: u64,
    links: u64,
    seen: u64,
    scope: usize,
}

#[derive(Debug)]
enum Directory {
    Target,
    Triple,
    Kind { profile: PathBuf, incremental: bool },
    Artifact(Scope),
}

/// Owns directory iterators across bounded filesystem work slices. Inventory
/// retains compact group keys and accounting, never a complete target manifest.
/// A selected scan alone retains the exact bounded approval manifest.
#[derive(Debug)]
struct Scan {
    snapshot: Snapshot,
    selected: Option<Vec<Scope>>,
    directories: Vec<(PathBuf, std::fs::ReadDir, Directory)>,
    measurements: BTreeMap<Scope, Measurement>,
    links: HashMap<(u64, u64), Link>,
    expected: Option<Box<Snapshot>>,
    initialized: bool,
    max_entries: usize,
    max_path_bytes: usize,
    incomplete: Option<Refusal>,
}

impl Scan {
    fn new(root: PathBuf, selected: Option<Vec<Scope>>) -> Self {
        Self {
            snapshot: Snapshot {
                root,
                ancestors: BTreeMap::new(),
                roots: BTreeMap::new(),
                entries: BTreeMap::new(),
            },
            selected,
            directories: Vec::new(),
            measurements: BTreeMap::new(),
            links: HashMap::new(),
            expected: None,
            initialized: false,
            max_entries: MAX_ENTRIES,
            max_path_bytes: MAX_PATH_BYTES,
            incomplete: None,
        }
    }

    fn verify(mut expected: Snapshot, selected: Vec<Scope>) -> Self {
        let mut scan = Self::new(std::mem::take(&mut expected.root), Some(selected));
        scan.expected = Some(Box::new(expected));
        scan
    }
    fn text_bytes(&self) -> usize {
        snapshot_path_bytes(&self.snapshot)
            + self
                .expected
                .as_ref()
                .map_or(0, |snapshot| snapshot_path_bytes(snapshot))
            + self
                .selected
                .as_ref()
                .map_or(0, |scopes| scopes.iter().map(Scope::path_bytes).sum())
            + self
                .directories
                .iter()
                .map(|(path, _, kind)| path.as_os_str().len() + directory_text_bytes(kind))
                .sum::<usize>()
            + self
                .measurements
                .keys()
                .map(Scope::path_bytes)
                .sum::<usize>()
    }
    fn admit(&self, additional: usize) -> Result<(), Refusal> {
        if self.text_bytes() + additional > self.max_path_bytes {
            Err(Refusal::Limit)
        } else {
            Ok(())
        }
    }

    fn incomplete(&mut self, scope: Option<&Scope>, reason: Refusal) -> Result<(), Refusal> {
        if self.selected.is_some() {
            return Err(reason);
        }
        if let Some(scope) = scope {
            if !self.measurements.contains_key(scope) {
                if self.measurements.len() >= self.max_entries {
                    return Err(Refusal::Limit);
                }
                self.admit(scope.path_bytes())?;
            }
            let identity = self.measurements.len();
            self.measurements
                .entry(scope.clone())
                .or_insert(Measurement {
                    identity,
                    bytes: 0,
                    incomplete: None,
                })
                .incomplete = Some(reason);
            let identity = self.measurements[scope].identity;
            self.links.retain(|_, link| link.scope != identity);
        } else {
            self.incomplete = Some(reason);
        }
        Ok(())
    }

    fn ancestor(&mut self, path: &Path, metadata: std::fs::Metadata) -> Result<(), Refusal> {
        if !metadata.is_dir() {
            return Err(Refusal::Protected);
        }
        if let Some(expected) = &mut self.expected {
            let (path, before) = expected
                .ancestors
                .remove_entry(path)
                .ok_or(Refusal::Changed)?;
            if (before.dev(), before.ino(), before.mode())
                != (metadata.dev(), metadata.ino(), metadata.mode())
            {
                return Err(Refusal::Protected);
            }
            self.snapshot.ancestors.insert(path, before);
        } else if self.selected.is_some() && !self.snapshot.ancestors.contains_key(path) {
            self.admit(path.as_os_str().len())?;
            self.snapshot.ancestors.insert(path.to_owned(), metadata);
        }
        Ok(())
    }

    fn wanted_profile(&self, path: &Path) -> bool {
        self.selected
            .as_ref()
            .is_none_or(|selected| selected.iter().any(|scope| scope.profile() == path))
    }
    fn wanted_ancestor(&self, path: &Path) -> bool {
        self.selected.as_ref().is_none_or(|selected| {
            selected
                .iter()
                .any(|scope| scope.profile().starts_with(path))
        })
    }
    fn wanted_kind(&self, profile: &Path, incremental: bool) -> bool {
        self.selected.as_ref().is_none_or(|selected| {
            selected.iter().any(|scope| {
                scope.profile() == profile && matches!(scope, Scope::Incremental(_)) == incremental
            })
        })
    }
    fn push_directory(
        &mut self,
        path: PathBuf,
        entries: std::fs::ReadDir,
        kind: Directory,
    ) -> Result<(), Refusal> {
        self.admit(path.as_os_str().len() + directory_text_bytes(&kind))?;
        if self.directories.len() >= self.max_entries {
            return Err(Refusal::Limit);
        }
        self.directories.push((path, entries, kind));
        Ok(())
    }

    fn profile(&mut self, path: &Path) -> Result<bool, Refusal> {
        let mut kinds = Vec::new();
        let mut is_profile = false;
        for kind in ["deps", ".fingerprint", "incremental", "build"] {
            let child = path.join(kind);
            match std::fs::symlink_metadata(self.snapshot.root.join(&child)) {
                Ok(metadata) if metadata.is_dir() => {
                    is_profile |= kind != "build";
                    kinds.push((child, metadata, kind == "incremental"));
                }
                Ok(metadata) if metadata.file_type().is_symlink() => {
                    // Never discover a profile or an artifact through a link.
                    if self.wanted_kind(path, kind == "incremental") {
                        self.incomplete(None, Refusal::Protected)?;
                    }
                }
                Ok(_) => {}
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(_) => self.incomplete(None, Refusal::Io)?,
            }
        }
        if !is_profile {
            return Ok(false);
        }
        let metadata =
            std::fs::symlink_metadata(self.snapshot.root.join(path)).map_err(|_| Refusal::Io)?;
        self.ancestor(path, metadata)?;
        for (child, metadata, incremental) in kinds {
            if !self.wanted_kind(path, incremental) {
                continue;
            }
            self.ancestor(&child, metadata)?;
            let entries =
                std::fs::read_dir(self.snapshot.root.join(&child)).map_err(|_| Refusal::Io)?;
            self.push_directory(
                child,
                entries,
                Directory::Kind {
                    profile: path.to_owned(),
                    incremental,
                },
            )?;
        }
        Ok(true)
    }

    fn record(
        &mut self,
        path: &Path,
        scope: &Scope,
        metadata: std::fs::Metadata,
    ) -> Result<(), Refusal> {
        if let Some(expected) = &mut self.expected {
            let (path, before) = expected
                .entries
                .remove_entry(path)
                .ok_or(Refusal::Changed)?;
            if fingerprint(&before) != fingerprint(&metadata) {
                return Err(Refusal::Changed);
            }
            self.snapshot.entries.insert(path, before);
            return Ok(());
        }
        if !self.measurements.contains_key(scope) {
            self.admit(scope.path_bytes())?;
            if self.measurements.len() >= self.max_entries {
                return Err(Refusal::Limit);
            }
            self.measurements.insert(
                scope.clone(),
                Measurement {
                    identity: self.measurements.len(),
                    bytes: 0,
                    incomplete: None,
                },
            );
        }
        if !(metadata.is_dir() || metadata.is_file() || metadata.file_type().is_symlink()) {
            return self.incomplete(Some(scope), Refusal::Protected);
        }
        if self.measurements[scope].incomplete.is_none() {
            let bytes = metadata.blocks() * 512;
            if metadata.is_dir() || metadata.nlink() == 1 {
                self.measurements.get_mut(scope).expect("inserted").bytes += bytes;
            } else {
                let inode = (metadata.dev(), metadata.ino());
                if !self.links.contains_key(&inode) && self.links.len() >= self.max_entries {
                    self.incomplete(Some(scope), Refusal::Limit)?;
                } else {
                    let link = self.links.entry(inode).or_insert_with(|| Link {
                        bytes,
                        links: metadata.nlink(),
                        seen: 0,
                        scope: self.measurements[scope].identity,
                    });
                    link.seen += 1;
                    if link.scope != self.measurements[scope].identity {
                        // Links split across groups cannot substantiate an
                        // individual estimate. Later links remain conservative.
                        self.links.remove(&inode);
                    } else if link.seen == link.links {
                        self.measurements.get_mut(scope).expect("inserted").bytes += link.bytes;
                        self.links.remove(&inode);
                    }
                }
            }
        }
        if self.selected.is_some() {
            self.admit(path.as_os_str().len())?;
            if self.snapshot.entries.len() >= self.max_entries {
                return Err(Refusal::Limit);
            }
            self.snapshot.entries.insert(path.to_owned(), metadata);
        }
        Ok(())
    }

    fn step(&mut self, budget: &mut usize, until: Instant) -> Result<bool, Refusal> {
        self.admit(0)?;
        if !self.initialized {
            let metadata =
                std::fs::symlink_metadata(&self.snapshot.root).map_err(|_| Refusal::Io)?;
            self.ancestor(Path::new(""), metadata)?;
            let entries = std::fs::read_dir(&self.snapshot.root).map_err(|_| Refusal::Io)?;
            self.push_directory(PathBuf::new(), entries, Directory::Target)?;
            self.initialized = true;
        }
        while *budget > 0 && Instant::now() < until {
            let Some((parent, entries, directory)) = self.directories.last_mut() else {
                if self.expected.as_ref().is_some_and(|expected| {
                    !expected.entries.is_empty()
                        || !expected.roots.is_empty()
                        || !expected.ancestors.is_empty()
                }) {
                    return Err(Refusal::Changed);
                }
                for roots in self.snapshot.roots.values_mut() {
                    roots.sort();
                }
                if let Some(selected) = &self.selected
                    && selected
                        .iter()
                        .any(|scope| !self.snapshot.roots.contains_key(scope))
                {
                    return Err(Refusal::Changed);
                }
                return Ok(true);
            };
            *budget -= 1;
            let Some(entry) = entries.next() else {
                self.directories.pop().expect("live directory");
                continue;
            };
            let entry = entry.map_err(|_| Refusal::Io)?;
            let path = parent.join(entry.file_name());
            let context = match directory {
                Directory::Target => 0,
                Directory::Triple => 1,
                Directory::Kind { .. } => 2,
                Directory::Artifact(_) => 3,
            };
            if context < 2 {
                if (context == 1 && !self.wanted_profile(&path))
                    || (context == 0 && !self.wanted_ancestor(&path))
                {
                    continue;
                }
                let metadata = std::fs::symlink_metadata(entry.path()).map_err(|_| Refusal::Io)?;
                if metadata.file_type().is_symlink() {
                    if self.selected.is_some() {
                        return Err(Refusal::Protected);
                    }
                    self.incomplete(None, Refusal::Protected)?;
                    continue;
                }
                if !metadata.is_dir() {
                    continue;
                }
                if self.wanted_profile(&path) && self.profile(&path)? {
                    continue;
                }
                if context == 0 {
                    let entries = std::fs::read_dir(entry.path()).map_err(|_| Refusal::Io)?;
                    self.ancestor(&path, metadata)?;
                    self.push_directory(path, entries, Directory::Triple)?;
                }
                continue;
            }
            let (scope, new_root) = match &self.directories.last().expect("live directory").2 {
                Directory::Kind {
                    profile,
                    incremental: true,
                } => (
                    Scope::Incremental(profile.join("incremental").join(entry.file_name())),
                    true,
                ),
                Directory::Kind {
                    incremental: false, ..
                } => {
                    let Some(key) = slot::unit_key(Path::new(""), &path) else {
                        continue;
                    };
                    (Scope::Unit(key), true)
                }
                Directory::Artifact(scope) => (scope.clone(), false),
                _ => unreachable!("artifact context"),
            };
            if self
                .selected
                .as_ref()
                .is_some_and(|selected| !selected.contains(&scope))
            {
                continue;
            }
            if new_root && self.selected.is_some() {
                let root = if let Some(expected) = &mut self.expected {
                    let roots = expected.roots.get_mut(&scope).ok_or(Refusal::Changed)?;
                    let index = roots
                        .iter()
                        .position(|root| root == &path)
                        .ok_or(Refusal::Changed)?;
                    let root = roots.swap_remove(index);
                    if roots.is_empty() {
                        expected.roots.remove(&scope);
                    }
                    root
                } else {
                    self.admit(
                        path.as_os_str().len()
                            + if self.snapshot.roots.contains_key(&scope) {
                                0
                            } else {
                                scope.path_bytes()
                            },
                    )?;
                    path.clone()
                };
                // During verification a transferred root may introduce a map key
                // while the same key still owns other expected roots.
                self.admit(
                    root.as_os_str().len()
                        + if self.snapshot.roots.contains_key(&scope) {
                            0
                        } else {
                            scope.path_bytes()
                        },
                )?;
                self.snapshot
                    .roots
                    .entry(scope.clone())
                    .or_default()
                    .push(root);
            }
            let metadata = match std::fs::symlink_metadata(entry.path()) {
                Ok(metadata) => metadata,
                Err(_) => {
                    self.incomplete(Some(&scope), Refusal::Io)?;
                    continue;
                }
            };
            let directory = metadata.is_dir();
            self.record(&path, &scope, metadata)?;
            if directory {
                match std::fs::read_dir(entry.path()) {
                    Ok(entries) => {
                        self.push_directory(path, entries, Directory::Artifact(scope))?
                    }
                    Err(_) => self.incomplete(Some(&scope), Refusal::Io)?,
                }
            }
        }
        Ok(false)
    }
}

#[derive(Clone, Debug)]
struct ScopedItem {
    item: Item,
    scope: Scope,
}

struct Measured {
    expires: Instant,
    wire: Inventory,
    scopes: Vec<ScopedItem>,
    generations: HashMap<String, u64>,
    remaining: VecDeque<String>,
    progress: String,
    scan: Option<Scan>,
    ready: bool,
}

struct Preparation {
    expires: Instant,
    inventory: String,
    items: Vec<ScopedItem>,
    result: Option<Refusal>,
}

struct Receipt {
    expires: Instant,
    preview: Preview,
    snapshots: HashMap<String, Snapshot>,
    scopes: Vec<ScopedItem>,
    result: Option<Outcome>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
enum Kind {
    Inventory { inventory: String, slot: String },
    Prepare(String),
    Execute(String),
}

struct Active {
    id: u64,
    kind: Kind,
    slots: Vec<crate::scheduler::SlotKey>,
    cancel: Arc<AtomicBool>,
    writing: bool,
}

#[derive(Debug)]
enum Stage {
    Inventory(Scan),
    Selected {
        pending: VecDeque<(String, Scan)>,
        snapshots: HashMap<String, Snapshot>,
        measurements: HashMap<String, BTreeMap<Scope, Measurement>>,
    },
    Invalidate {
        directories: VecDeque<SlotDirectory>,
        snapshots: HashMap<String, Snapshot>,
    },
    Remove {
        roots: Vec<PathBuf>,
        paths: VecDeque<(usize, usize, PathBuf, bool)>,
        remaining: Vec<usize>,
        removed: Vec<usize>,
    },
    Finished,
}

/// A single bounded integration worker owns its filesystem iterators. The
/// event-loop owner retains all scheduler leases and accepts its exact result.
#[derive(Debug)]
pub(crate) struct Work {
    id: u64,
    kind: Kind,
    stage: Stage,
    expires: Instant,
    cancel: Arc<AtomicBool>,
    max_text_bytes: usize,
}

impl Work {
    pub(crate) fn run(self) -> (Self, Result<bool, Refusal>) {
        self.run_bounded(MAX_ENTRIES, SCAN_TIME)
    }

    fn run_bounded(mut self, entries: usize, time: Duration) -> (Self, Result<bool, Refusal>) {
        let until = self.expires.min(Instant::now() + time);
        let mut budget = entries;
        let result = (|| {
            if matches!(&self.stage, Stage::Remove { paths,.. } if paths.is_empty()) {
                return Ok(true);
            }
            if self.cancel.load(Ordering::Acquire) || Instant::now() >= self.expires {
                return Err(Refusal::Expired);
            }
            if kind_text_bytes(&self.kind) + stage_text_bytes(&self.stage) > self.max_text_bytes {
                return Err(Refusal::Limit);
            }
            match &mut self.stage {
                Stage::Inventory(scan) => {
                    scan.max_path_bytes = scan.max_path_bytes.min(
                        self.max_text_bytes
                            .checked_sub(kind_text_bytes(&self.kind))
                            .ok_or(Refusal::Limit)?,
                    );
                    scan.step(&mut budget, until)
                }
                Stage::Selected {
                    pending,
                    snapshots,
                    measurements,
                } => {
                    while !pending.is_empty() {
                        let other = selected_text_bytes(pending, snapshots, measurements)
                            - pending[0].1.text_bytes();
                        let other_entries = snapshots
                            .values()
                            .map(|snapshot| snapshot.entries.len())
                            .sum::<usize>()
                            + pending
                                .iter()
                                .skip(1)
                                .map(|(_, scan)| {
                                    scan.snapshot.entries.len()
                                        + scan
                                            .expected
                                            .as_ref()
                                            .map_or(0, |snapshot| snapshot.entries.len())
                                })
                                .sum::<usize>();
                        let scan = &mut pending.front_mut().expect("pending").1;
                        scan.max_entries = MAX_ENTRIES
                            .checked_sub(other_entries)
                            .ok_or(Refusal::Limit)?;
                        scan.max_path_bytes = self
                            .max_text_bytes
                            .checked_sub(kind_text_bytes(&self.kind) + other)
                            .ok_or(Refusal::Limit)?;
                        if !scan.step(&mut budget, until)? {
                            return Ok(false);
                        }
                        let (name, scan) = pending.pop_front().expect("front exists");
                        let entries = snapshots
                            .values()
                            .map(|snapshot| snapshot.entries.len())
                            .sum::<usize>()
                            + scan.snapshot.entries.len();
                        if entries > MAX_ENTRIES {
                            return Err(Refusal::Limit);
                        }
                        let final_text = selected_text_bytes(pending, snapshots, measurements)
                            + name.len() * 2
                            + snapshot_path_bytes(&scan.snapshot)
                            + scan
                                .measurements
                                .keys()
                                .map(Scope::path_bytes)
                                .sum::<usize>()
                            + kind_text_bytes(&self.kind);
                        if final_text > self.max_text_bytes {
                            return Err(Refusal::Limit);
                        }
                        snapshots.insert(name.clone(), scan.snapshot);
                        measurements.insert(name, scan.measurements);
                        if budget == 0 || Instant::now() >= until {
                            return Ok(pending.is_empty());
                        }
                    }
                    Ok(true)
                }
                Stage::Invalidate { directories, .. } => {
                    while budget > 0 && Instant::now() < until {
                        let Some(directory) = directories.pop_front() else {
                            return Ok(true);
                        };
                        budget -= 1;
                        let mut record = directory.read_record().map_err(|_| Refusal::Io)?;
                        record.units.clear();
                        record.passed.clear();
                        record.compilations.clear();
                        directory.write_record(&record).map_err(|_| Refusal::Io)?;
                    }
                    Ok(false)
                }
                Stage::Remove {
                    roots,
                    paths,
                    remaining,
                    removed,
                } => {
                    while budget > 0 && Instant::now() < until {
                        if Instant::now() >= self.expires {
                            return Err(Refusal::Expired);
                        }
                        let Some((item, target, path, directory)) = paths.front() else {
                            return Ok(true);
                        };
                        if *directory {
                            std::fs::remove_dir(roots[*target].join(path))
                        } else {
                            std::fs::remove_file(roots[*target].join(path))
                        }
                        .map_err(|_| Refusal::Io)?;
                        let item = *item;
                        paths.pop_front();
                        budget -= 1;
                        let count = &mut remaining[item];
                        *count -= 1;
                        if *count == 0 {
                            removed.push(item);
                        }
                    }
                    Ok(paths.is_empty())
                }
                Stage::Finished => unreachable!("finished work is never dispatched"),
            }
        })();
        (self, result)
    }
}

/// Incarnation-scoped owner state. Discovery is derived and expiring; exact
/// preview payloads are approval truth. Active owns scheduler exclusion, while
/// Work alone owns filesystem resources and never mutates scheduler state.
pub(crate) struct Cache {
    owner: Owner,
    sequence: u64,
    inventories: HashMap<String, Measured>,
    preparations: HashMap<String, Preparation>,
    receipts: HashMap<String, Receipt>,
    active: Option<Active>,
    work: Option<Work>,
    max_text_bytes: usize,
}

impl Cache {
    pub(crate) fn new(home: &Path) -> Result<Self, String> {
        let mut random = [0_u8; 16];
        std::fs::File::open("/dev/urandom")
            .and_then(|mut file| file.read_exact(&mut random))
            .map_err(|error| format!("could not identify daemon incarnation: {error}"))?;
        let mut hostname = [0_u8; 256];
        // SAFETY: the buffer is writable for its exact supplied length.
        if unsafe { libc::gethostname(hostname.as_mut_ptr().cast(), hostname.len()) } != 0 {
            return Err("could not identify cache host".into());
        }
        let end = hostname
            .iter()
            .position(|byte| *byte == 0)
            .unwrap_or(hostname.len());
        let host = format!(
            "{}:{}",
            String::from_utf8_lossy(&hostname[..end]),
            home.canonicalize()
                .map_err(|error| error.to_string())?
                .display()
        );
        Ok(Self {
            owner: Owner {
                host,
                incarnation: random.iter().map(|byte| format!("{byte:02x}")).collect(),
            },
            sequence: 0,
            inventories: HashMap::new(),
            preparations: HashMap::new(),
            receipts: HashMap::new(),
            active: None,
            work: None,
            max_text_bytes: MAX_PATH_BYTES,
        })
    }
    fn id(&mut self) -> String {
        self.sequence = self
            .sequence
            .checked_add(1)
            .expect("cache identity exhausted");
        format!("{}:{}", self.owner.incarnation, self.sequence)
    }
    pub(crate) fn take_work(&mut self) -> Option<Work> {
        self.work.take()
    }

    fn start(
        &mut self,
        kind: Kind,
        slots: Vec<crate::scheduler::SlotKey>,
        stage: Stage,
        expires: Instant,
    ) {
        assert!(
            self.active.is_none() && self.work.is_none(),
            "one cache worker owns filesystem resources"
        );
        let cancel = Arc::new(AtomicBool::new(false));
        self.sequence = self
            .sequence
            .checked_add(1)
            .expect("cache work identity exhausted");
        let id = self.sequence;
        self.active = Some(Active {
            id,
            kind: kind.clone(),
            slots,
            cancel: cancel.clone(),
            writing: false,
        });
        self.work = Some(Work {
            id,
            kind,
            stage,
            expires,
            cancel,
            max_text_bytes: self
                .max_text_bytes
                .checked_sub(self.budget_bytes())
                .expect("owner admission precedes work"),
        });
    }

    fn text_bytes(&self) -> usize {
        owner_text_bytes(&self.owner)
            + self
                .inventories
                .iter()
                .map(|(id, measured)| {
                    id.len()
                        + owner_text_bytes(&measured.wire.owner)
                        + measured.wire.revision.len()
                        + measured.wire.next.as_ref().map_or(0, String::len)
                        + measured
                            .wire
                            .slots
                            .iter()
                            .map(|slot| slot.id.len())
                            .sum::<usize>()
                        + measured.scopes.iter().map(scoped_text_bytes).sum::<usize>()
                        + measured.generations.keys().map(String::len).sum::<usize>()
                        + measured.remaining.iter().map(String::len).sum::<usize>()
                        + measured.progress.len()
                        + measured.scan.as_ref().map_or(0, Scan::text_bytes)
                })
                .sum::<usize>()
            + self
                .preparations
                .iter()
                .map(|(id, preparation)| {
                    id.len()
                        + preparation.inventory.len()
                        + preparation
                            .items
                            .iter()
                            .map(scoped_text_bytes)
                            .sum::<usize>()
                })
                .sum::<usize>()
            + self
                .receipts
                .iter()
                .map(|(id, receipt)| {
                    id.len()
                        + preview_text_bytes(&receipt.preview)
                        + receipt
                            .snapshots
                            .iter()
                            .map(|(name, snapshot)| name.len() + snapshot_path_bytes(snapshot))
                            .sum::<usize>()
                        + receipt.scopes.iter().map(scoped_text_bytes).sum::<usize>()
                        + outcome_text_bytes(receipt.result.as_ref())
                })
                .sum::<usize>()
            + self
                .active
                .as_ref()
                .map_or(0, |active| kind_text_bytes(&active.kind))
    }

    // Approval publication and a terminal receipt may add text after a worker
    // has consumed its allowance. Reserve those known payloads before dispatch.
    fn budget_bytes(&self) -> usize {
        self.text_bytes()
            + self
                .preparations
                .iter()
                .filter(|(_, preparation)| preparation.result.is_none())
                .map(|(token, preparation)| {
                    owner_text_bytes(&self.owner)
                        + token.len()
                        + preparation
                            .items
                            .iter()
                            .map(|item| item_text_bytes(&item.item))
                            .sum::<usize>()
                        + outcome_reserve(
                            preparation.items.iter().map(|item| item.item.id.as_str()),
                        )
                })
                .sum::<usize>()
            + self
                .receipts
                .values()
                .map(|receipt| {
                    outcome_reserve(receipt.preview.items.iter().map(|item| item.id.as_str()))
                        - outcome_text_bytes(receipt.result.as_ref())
                })
                .sum::<usize>()
    }

    pub(crate) fn expire(&mut self, now: Instant) {
        if let Some(active) = &self.active {
            let expires = match &active.kind {
                Kind::Inventory { inventory, .. } => self.inventories[inventory].expires,
                Kind::Prepare(token) => self.preparations[token].expires,
                Kind::Execute(token) => self.receipts[token].expires,
            };
            if now >= expires {
                active.cancel.store(true, Ordering::Release);
            }
        }
        let active = self.active.as_ref().map(|active| active.kind.clone());
        self.inventories.retain(|id,inventory|inventory.expires > now || matches!(&active,Some(Kind::Inventory { inventory: pending,.. }) if pending == id));
        self.preparations.retain(|id, preparation| {
            preparation.expires > now
                || matches!(&active,Some(Kind::Prepare(pending)) if pending == id)
        });
        self.receipts.retain(|id, receipt| {
            receipt.expires > now || matches!(&active,Some(Kind::Execute(pending)) if pending == id)
        });
    }
    pub(crate) fn next_expiry(&self) -> Option<Instant> {
        let cancelled = self
            .active
            .as_ref()
            .filter(|active| active.cancel.load(Ordering::Acquire))
            .map(|active| &active.kind);
        self.inventories.iter().filter(|(id,_)| !matches!(cancelled, Some(Kind::Inventory {inventory,..}) if inventory == *id)).map(|(_, state)| state.expires)
            .chain(self.preparations.iter().filter(|(id,_)| !matches!(cancelled, Some(Kind::Prepare(token)) if token == *id)).map(|(_, state)| state.expires))
            .chain(self.receipts.iter().filter(|(id,_)| !matches!(cancelled, Some(Kind::Execute(token)) if token == *id)).map(|(_, state)| state.expires)).min()
    }

    pub(crate) fn operate<D: Distance>(
        &mut self,
        home: &Path,
        scheduler: &mut Scheduler<D>,
        operation: Operation,
    ) -> (Response, Vec<Effect>) {
        self.expire(Instant::now());
        let receipt = matches!(
            operation,
            Operation::Execute { .. } | Operation::Receipt { .. } | Operation::Cancel { .. }
        );
        let mut effects = Vec::new();
        let result = self.perform(home, scheduler, operation, &mut effects);
        (
            result.unwrap_or_else(|reason| {
                if receipt
                    && matches!(
                        reason,
                        Refusal::ForeignOwner | Refusal::UnknownReceipt | Refusal::Expired
                    )
                {
                    Response::Unknown { reason }
                } else {
                    Response::Refused { reason }
                }
            }),
            effects,
        )
    }

    fn owner(&self, owner: &Owner) -> Result<(), Refusal> {
        if *owner == self.owner {
            Ok(())
        } else {
            Err(Refusal::ForeignOwner)
        }
    }

    fn perform<D: Distance>(
        &mut self,
        home: &Path,
        scheduler: &mut Scheduler<D>,
        operation: Operation,
        effects: &mut Vec<Effect>,
    ) -> Result<Response, Refusal> {
        match operation {
            Operation::Capabilities => Ok(Response::Capabilities(Capabilities {
                owner: self.owner.clone(),
                protocol: 2,
                max_slots: MAX_SLOTS,
                max_items: MAX_ITEMS,
                max_inventory_page_items: MAX_INVENTORY_PAGE_ITEMS,
                max_entries: MAX_ENTRIES,
                max_path_bytes: MAX_PATH_BYTES,
                scan_ms: SCAN_TIME.as_millis() as u64,
                receipt_ttl_ms: TTL.as_millis() as u64,
                max_receipts: MAX_RECEIPTS,
            })),
            Operation::Slots { owner, after } => {
                self.owner(&owner)?;
                let start = match after {
                    None => 0,
                    Some(after) => {
                        let start = scheduler
                            .cache_slots()
                            .position(|(name, _)| name == after)
                            .ok_or(Refusal::PayloadChanged)?
                            + 1;
                        if start % MAX_SLOTS != 0 {
                            return Err(Refusal::PayloadChanged);
                        }
                        start
                    }
                };
                let mut slots = scheduler
                    .cache_slots()
                    .skip(start)
                    .take(MAX_SLOTS + 1)
                    .map(|(id, protected)| Slot {
                        id,
                        protected,
                        incomplete: None,
                    })
                    .collect::<Vec<_>>();
                if start > 0 && slots.is_empty() {
                    return Err(Refusal::PayloadChanged);
                }
                let next = if slots.len() > MAX_SLOTS {
                    slots.pop();
                    Some(slots.last().expect("full slot page").id.clone())
                } else {
                    None
                };
                Ok(Response::Slots { owner, slots, next })
            }
            Operation::ReleaseInventory { owner, inventory } => {
                self.owner(&owner)?;
                let measured = self
                    .inventories
                    .get_mut(&inventory)
                    .ok_or(Refusal::Expired)?;
                // Reuse expiry's cancellation and completion ownership. Issued
                // preparations and receipts own their selection independently.
                let now = Instant::now();
                measured.expires = now;
                self.expire(now);
                Ok(Response::Released { inventory })
            }
            Operation::Inventory { owner, slots } => {
                self.owner(&owner)?;
                if self.active.is_some() {
                    return Err(Refusal::Busy);
                }
                if slots.is_empty() {
                    return Err(Refusal::PayloadChanged);
                }
                if slots.len() > MAX_SLOTS || self.inventories.len() >= MAX_RECEIPTS {
                    return Err(Refusal::Limit);
                }
                for (index, name) in slots.iter().enumerate() {
                    if slots[..index].contains(name) || scheduler.cache_generation(name).is_none() {
                        return Err(Refusal::PayloadChanged);
                    }
                }
                let slots = slots
                    .into_iter()
                    .map(|name| {
                        let busy = scheduler
                            .cache_slots()
                            .find(|(id, _)| *id == name)
                            .expect("validated scheduler slot")
                            .1;
                        (name, busy)
                    })
                    .collect::<Vec<_>>();
                let revision = self.id();
                let progress = self.id();
                let measured_ms = milliseconds();
                let mut wire = Inventory {
                    owner,
                    revision: revision.clone(),
                    measured_ms,
                    expires_ms: measured_ms + TTL.as_millis() as u64,
                    complete: true,
                    slots: Vec::new(),
                    items: Vec::new(),
                    next: Some(progress.clone()),
                };
                let mut remaining = VecDeque::new();
                let mut generations = HashMap::new();
                for (name, busy) in slots {
                    wire.slots.push(Slot {
                        id: name.clone(),
                        protected: busy,
                        incomplete: None,
                    });
                    if busy {
                        wire.complete = false;
                    } else {
                        generations.insert(
                            name.clone(),
                            scheduler.cache_generation(&name).expect("scheduler slot"),
                        );
                        remaining.push_back(name);
                    }
                }
                self.inventories.insert(
                    revision.clone(),
                    Measured {
                        expires: Instant::now() + TTL,
                        wire,
                        scopes: Vec::new(),
                        generations,
                        remaining,
                        progress,
                        scan: None,
                        ready: false,
                    },
                );
                if self.budget_bytes() > self.max_text_bytes {
                    self.inventories.remove(&revision);
                    return Err(Refusal::Limit);
                }
                self.advance_inventory(home, scheduler, &revision, effects)?;
                self.page(&revision, None)
            }
            Operation::InventoryPage {
                owner,
                inventory,
                after,
            } => {
                self.owner(&owner)?;
                let measured = self.inventories.get(&inventory).ok_or(Refusal::Expired)?;
                if Instant::now() >= measured.expires {
                    return Err(Refusal::Expired);
                }
                if measured
                    .generations
                    .iter()
                    .any(|(name, generation)| scheduler.cache_generation(name) != Some(*generation))
                {
                    return Err(Refusal::Changed);
                }
                if after == measured.progress && !measured.ready {
                    self.advance_inventory(home, scheduler, &inventory, effects)?;
                    return self.page(&inventory, None);
                }
                self.page(&inventory, Some(&after))
            }
            Operation::Preview {
                owner,
                inventory,
                items,
            } => {
                self.owner(&owner)?;
                if self.active.is_some() {
                    return Err(Refusal::Busy);
                }
                if items.is_empty()
                    || items.len() > MAX_ITEMS
                    || self.receipts.len() + self.preparations.len() >= MAX_RECEIPTS
                {
                    return Err(Refusal::Limit);
                }
                let measured = self.inventories.get(&inventory).ok_or(Refusal::Expired)?;
                if Instant::now() >= measured.expires {
                    return Err(Refusal::Expired);
                }
                let mut selected = Vec::new();
                for id in items {
                    if selected.iter().any(|item: &ScopedItem| item.item.id == id) {
                        return Err(Refusal::PayloadChanged);
                    }
                    let item = measured
                        .scopes
                        .iter()
                        .find(|item| item.item.id == id)
                        .ok_or(Refusal::PayloadChanged)?;
                    if let Some(reason) = &item.item.incomplete {
                        return Err(reason.clone());
                    }
                    selected.push(item.clone());
                }
                let expires = measured.expires;
                if selected.iter().any(|item| {
                    scheduler.cache_generation(&item.item.slot)
                        != measured.generations.get(&item.item.slot).copied()
                }) {
                    return Err(Refusal::Changed);
                }
                let slots = self.acquire(scheduler, &selected, effects)?;
                let pending = scans(home, scheduler, &slots, &selected);
                let token = self.id();
                self.preparations.insert(
                    token.clone(),
                    Preparation {
                        expires,
                        inventory,
                        items: selected,
                        result: None,
                    },
                );
                if self.budget_bytes()
                    + selected_text_bytes(&pending, &HashMap::new(), &HashMap::new())
                    + token.len() * 2
                    > self.max_text_bytes
                {
                    self.preparations.remove(&token);
                    for slot in slots {
                        effects.extend(scheduler.cache_release(slot, false));
                    }
                    return Err(Refusal::Limit);
                }
                self.start(
                    Kind::Prepare(token.clone()),
                    slots,
                    Stage::Selected {
                        pending,
                        snapshots: HashMap::new(),
                        measurements: HashMap::new(),
                    },
                    expires,
                );
                Ok(Response::Preparing { preparation: token })
            }
            Operation::PreviewStatus { owner, preparation } => {
                self.owner(&owner)?;
                if let Some(receipt) = self.receipts.get(&preparation) {
                    if Instant::now() >= receipt.expires {
                        return Err(Refusal::Expired);
                    }
                    return Ok(Response::Preview(receipt.preview.clone()));
                }
                let state = self
                    .preparations
                    .get(&preparation)
                    .ok_or(Refusal::Expired)?;
                if Instant::now() >= state.expires {
                    return Err(Refusal::Expired);
                }
                match &state.result {
                    None => Ok(Response::Preparing { preparation }),
                    Some(reason) => Err(reason.clone()),
                }
            }
            Operation::Execute { preview } => {
                self.validate(&preview)?;
                if self.receipts[&preview.token].result.is_some() || self.executing(&preview.token)
                {
                    return Ok(self.receipt(preview));
                }
                if self.active.is_some() {
                    return Err(Refusal::Busy);
                }
                let selected = &self.receipts[&preview.token].scopes;
                let slots = self.acquire(scheduler, selected, effects)?;
                let mut snapshots = std::mem::take(
                    &mut self
                        .receipts
                        .get_mut(&preview.token)
                        .expect("validated")
                        .snapshots,
                );
                let pending = snapshots
                    .drain()
                    .map(|(name, snapshot)| {
                        let scopes = self.receipts[&preview.token]
                            .scopes
                            .iter()
                            .filter(|item| item.item.slot == name)
                            .map(|item| item.scope.clone())
                            .collect();
                        (name, Scan::verify(snapshot, scopes))
                    })
                    .collect();
                if self.budget_bytes()
                    + selected_text_bytes(&pending, &HashMap::new(), &HashMap::new())
                    + preview.token.len() * 2
                    > self.max_text_bytes
                {
                    self.receipts
                        .get_mut(&preview.token)
                        .expect("validated")
                        .result = Some(Outcome::Refused {
                        reason: Refusal::Limit,
                    });
                    for slot in slots {
                        effects.extend(scheduler.cache_release(slot, false));
                    }
                    return Ok(self.receipt(preview));
                }
                let expires = self.receipts[&preview.token].expires;
                self.start(
                    Kind::Execute(preview.token.clone()),
                    slots,
                    Stage::Selected {
                        pending,
                        snapshots: HashMap::new(),
                        measurements: HashMap::new(),
                    },
                    expires,
                );
                Ok(self.receipt(preview))
            }
            Operation::Receipt { preview } => {
                self.validate(&preview)?;
                Ok(self.receipt(preview))
            }
            Operation::Cancel { preview } => {
                self.validate(&preview)?;
                if !self.executing(&preview.token) && self.receipts[&preview.token].result.is_none()
                {
                    self.receipts
                        .get_mut(&preview.token)
                        .expect("validated")
                        .result = Some(Outcome::Cancelled);
                }
                Ok(self.receipt(preview))
            }
        }
    }

    fn acquire<D: Distance>(
        &self,
        scheduler: &mut Scheduler<D>,
        items: &[ScopedItem],
        effects: &mut Vec<Effect>,
    ) -> Result<Vec<crate::scheduler::SlotKey>, Refusal> {
        let names = items
            .iter()
            .map(|item| item.item.slot.clone())
            .collect::<std::collections::BTreeSet<_>>();
        let mut acquired = Vec::new();
        for name in names {
            match scheduler.cache_acquire(&name) {
                Some(slot) => acquired.push(slot),
                None => {
                    for slot in acquired {
                        effects.extend(scheduler.cache_release(slot, false));
                    }
                    return Err(Refusal::Busy);
                }
            }
        }
        Ok(acquired)
    }

    fn validate(&self, preview: &Preview) -> Result<(), Refusal> {
        self.owner(&preview.owner)?;
        let receipt = self
            .receipts
            .get(&preview.token)
            .ok_or(Refusal::UnknownReceipt)?;
        if receipt.preview != *preview {
            return Err(Refusal::PayloadChanged);
        }
        if receipt.expires <= Instant::now() || preview.expires_ms <= milliseconds() {
            return Err(Refusal::Expired);
        }
        Ok(())
    }
    fn executing(&self, token: &str) -> bool {
        matches!(&self.active,Some(active) if active.kind == Kind::Execute(token.to_owned()))
    }
    fn receipt(&self, preview: Preview) -> Response {
        let result = self.receipts[&preview.token].result.clone();
        let executing = self.executing(&preview.token);
        Response::Receipt {
            preview,
            result,
            executing,
        }
    }

    fn page(&self, revision: &str, after: Option<&str>) -> Result<Response, Refusal> {
        let measured = self.inventories.get(revision).ok_or(Refusal::Expired)?;
        let wire = &measured.wire;
        let (start, end, next, complete) = if measured.ready {
            let start = match after {
                None => 0,
                Some(id) if id == measured.progress => 0,
                Some(id) => {
                    let start = measured
                        .scopes
                        .iter()
                        .position(|item| item.item.id == id)
                        .ok_or(Refusal::PayloadChanged)?
                        + 1;
                    if start >= measured.scopes.len() || start % MAX_INVENTORY_PAGE_ITEMS != 0 {
                        return Err(Refusal::PayloadChanged);
                    }
                    start
                }
            };
            let end = (start + MAX_INVENTORY_PAGE_ITEMS).min(measured.scopes.len());
            (
                start,
                end,
                (end < measured.scopes.len()).then(|| measured.scopes[end - 1].item.id.clone()),
                wire.complete,
            )
        } else {
            (0, 0, Some(measured.progress.clone()), false)
        };
        Ok(Response::Inventory(Inventory {
            owner: wire.owner.clone(),
            revision: wire.revision.clone(),
            measured_ms: wire.measured_ms,
            expires_ms: wire.expires_ms,
            complete,
            slots: wire.slots.clone(),
            items: measured.scopes[start..end]
                .iter()
                .map(|item| item.item.clone())
                .collect(),
            next,
        }))
    }

    fn advance_inventory<D: Distance>(
        &mut self,
        home: &Path,
        scheduler: &mut Scheduler<D>,
        revision: &str,
        effects: &mut Vec<Effect>,
    ) -> Result<(), Refusal> {
        if self.active.is_some() {
            return Ok(());
        }
        let measured = self.inventories.get_mut(revision).ok_or(Refusal::Expired)?;
        if measured.ready {
            return Ok(());
        }
        if measured
            .generations
            .iter()
            .any(|(name, generation)| scheduler.cache_generation(name) != Some(*generation))
        {
            return Err(Refusal::Changed);
        }
        let Some(name) = measured.remaining.front().cloned() else {
            measured.ready = true;
            measured.wire.next = None;
            return Ok(());
        };
        let Some(slot) = scheduler.cache_acquire(&name) else {
            return Err(Refusal::Busy);
        };
        assert_eq!(
            measured.generations[&name],
            scheduler.cache_identity(slot).2,
            "acquisition preserves target generation"
        );
        let scan = measured.scan.take().unwrap_or_else(|| {
            Scan::new(
                {
                    let (repository, index, _) = scheduler.cache_identity(slot);
                    SlotDirectory::new(home, repository, index).target()
                },
                None,
            )
        });
        let expires = measured.expires;
        if self.budget_bytes() + scan.text_bytes() + (revision.len() + name.len()) * 2
            > self.max_text_bytes
        {
            effects.extend(scheduler.cache_release(slot, false));
            let measured = self.inventories.get_mut(revision).expect("known inventory");
            measured.remaining.pop_front();
            measured.wire.complete = false;
            measured
                .wire
                .slots
                .iter_mut()
                .find(|record| record.id == name)
                .expect("known slot")
                .incomplete = Some(Refusal::Limit);
            measured.ready = measured.remaining.is_empty();
            return Ok(());
        }
        self.start(
            Kind::Inventory {
                inventory: revision.to_owned(),
                slot: name,
            },
            vec![slot],
            Stage::Inventory(scan),
            expires,
        );
        let _ = effects;
        Ok(())
    }

    pub(crate) fn completed<D: Distance>(
        &mut self,
        home: &Path,
        scheduler: &mut Scheduler<D>,
        mut work: Work,
        result: Result<bool, Refusal>,
    ) -> Vec<Effect> {
        let result = if (work.cancel.load(Ordering::Acquire) || Instant::now() >= work.expires)
            && !(matches!(work.stage, Stage::Remove { .. }) && result == Ok(true))
        {
            Err(Refusal::Expired)
        } else {
            result
        };
        let active = self.active.take().expect("a worker owns exclusion");
        assert_eq!(active.id, work.id, "only the active worker publishes");
        assert_eq!(active.kind, work.kind, "worker identity is immutable");
        let retained_text = self.budget_bytes();
        let mut effects = Vec::new();
        match work.kind.clone() {
            Kind::Inventory { inventory, slot } => {
                let Stage::Inventory(scan) = work.stage else {
                    unreachable!("inventory stage");
                };
                let measured = self
                    .inventories
                    .get_mut(&inventory)
                    .expect("active inventory retained");
                if result == Ok(false) {
                    measured.scan = Some(scan);
                } else {
                    measured.remaining.pop_front();
                    match result {
                        Ok(true) => {
                            let incomplete = scan.incomplete.clone();
                            let mut records = Vec::new();
                            for (scope, measurement) in scan.measurements {
                                self.sequence = self
                                    .sequence
                                    .checked_add(1)
                                    .expect("cache identity exhausted");
                                let item = Item {
                                    id: format!("{}:{}", self.owner.incarnation, self.sequence),
                                    slot: slot.clone(),
                                    unit: scope.unit(),
                                    reclaimable_bytes: measurement
                                        .incomplete
                                        .is_none()
                                        .then_some(measurement.bytes),
                                    incomplete: measurement.incomplete,
                                };
                                records.push(ScopedItem { item, scope });
                            }
                            let incoming = records.iter().map(scoped_text_bytes).sum::<usize>();
                            if measured.scopes.len() + records.len() > MAX_ENTRIES
                                || retained_text + incoming > self.max_text_bytes
                            {
                                measured.wire.complete = false;
                                measured
                                    .wire
                                    .slots
                                    .iter_mut()
                                    .find(|record| record.id == slot)
                                    .expect("known slot")
                                    .incomplete = Some(Refusal::Limit);
                            } else {
                                measured.wire.complete &= records
                                    .iter()
                                    .all(|record| record.item.incomplete.is_none());
                                measured.scopes.extend(records);
                                if let Some(reason) = incomplete {
                                    measured.wire.complete = false;
                                    measured
                                        .wire
                                        .slots
                                        .iter_mut()
                                        .find(|record| record.id == slot)
                                        .expect("known slot")
                                        .incomplete = Some(reason);
                                }
                            }
                        }
                        Err(reason) => {
                            measured.wire.complete = false;
                            measured
                                .wire
                                .slots
                                .iter_mut()
                                .find(|record| record.id == slot)
                                .expect("known slot")
                                .incomplete = Some(reason);
                        }
                        Ok(false) => unreachable!(),
                    }
                }
                for idle in active.slots {
                    effects.extend(scheduler.cache_release(idle, false));
                }
                // No lease survives a progress response. A queued writer can
                // start here and invalidate this session before continuation.
                if measured.remaining.is_empty() {
                    measured.ready = true;
                }
            }
            Kind::Prepare(token) => {
                if result == Ok(false) {
                    self.active = Some(active);
                    self.work = Some(work);
                    return effects;
                }
                let mut preparation = self
                    .preparations
                    .remove(&token)
                    .expect("active preparation retained");
                let prepared = result.and_then(|_| {
                    let Stage::Selected {
                        snapshots,
                        measurements,
                        ..
                    } = std::mem::replace(&mut work.stage, Stage::Finished)
                    else {
                        unreachable!("preparation stage");
                    };
                    for item in &mut preparation.items {
                        let measurement = &measurements[&item.item.slot][&item.scope];
                        if let Some(reason) = &measurement.incomplete {
                            return Err(reason.clone());
                        }
                        item.item.reclaimable_bytes = Some(measurement.bytes);
                    }
                    let preview = Preview {
                        owner: self.owner.clone(),
                        token: token.clone(),
                        inventory: preparation.inventory.clone(),
                        expires_ms: milliseconds()
                            + preparation
                                .expires
                                .saturating_duration_since(Instant::now())
                                .as_millis() as u64,
                        items: preparation
                            .items
                            .iter()
                            .map(|item| item.item.clone())
                            .collect(),
                    };
                    self.receipts.insert(
                        token.clone(),
                        Receipt {
                            expires: preparation.expires,
                            preview,
                            snapshots,
                            scopes: std::mem::take(&mut preparation.items),
                            result: None,
                        },
                    );
                    Ok(())
                });
                if let Err(reason) = prepared {
                    preparation.result = Some(reason);
                    preparation.items.clear();
                    self.preparations.insert(token, preparation);
                }
                for idle in active.slots {
                    effects.extend(scheduler.cache_release(idle, false));
                }
            }
            Kind::Execute(token) => {
                let mut active = active;
                let receipt = self
                    .receipts
                    .get_mut(&token)
                    .expect("active receipt retained");
                let terminal = match result {
                    Err(reason) => Some(match &work.stage {
                        Stage::Remove { paths, removed, .. } => Outcome::Partial {
                            removed: removed
                                .iter()
                                .map(|index| receipt.preview.items[*index].id.clone())
                                .collect(),
                            failed: paths.front().map_or_else(
                                || receipt.preview.items[0].id.clone(),
                                |(index, ..)| receipt.preview.items[*index].id.clone(),
                            ),
                            reason,
                        },
                        _ => Outcome::Refused { reason },
                    }),
                    Ok(false) => None,
                    Ok(true) => match std::mem::replace(&mut work.stage, Stage::Finished) {
                        Stage::Selected { snapshots, .. } => {
                            for slot in &active.slots {
                                scheduler.cache_writing(*slot);
                            }
                            active.writing = true;
                            work.stage = Stage::Invalidate {
                                directories: active
                                    .slots
                                    .iter()
                                    .map(|slot| {
                                        let (repository, index, _) =
                                            scheduler.cache_identity(*slot);
                                        SlotDirectory::new(home, repository, index)
                                    })
                                    .collect(),
                                snapshots,
                            };
                            None
                        }
                        Stage::Invalidate { snapshots, .. } => {
                            work.stage = removal(&receipt.scopes, snapshots);
                            None
                        }
                        Stage::Remove { removed, .. } => Some(Outcome::Completed {
                            removed: removed
                                .into_iter()
                                .map(|index| receipt.preview.items[index].id.clone())
                                .collect(),
                        }),
                        _ => unreachable!("execute stage"),
                    },
                };
                if let Some(outcome) = terminal {
                    receipt.result = Some(outcome);
                    for slot in active.slots {
                        effects.extend(scheduler.cache_release(slot, active.writing));
                    }
                } else if kind_text_bytes(&work.kind) + stage_text_bytes(&work.stage)
                    > work.max_text_bytes
                {
                    receipt.result = Some(Outcome::Refused {
                        reason: Refusal::Limit,
                    });
                    for slot in active.slots {
                        effects.extend(scheduler.cache_release(slot, active.writing));
                    }
                } else {
                    self.active = Some(active);
                    self.work = Some(work);
                }
            }
        }
        assert!(
            self.budget_bytes()
                + self
                    .work
                    .as_ref()
                    .map_or(0, |work| kind_text_bytes(&work.kind)
                        + stage_text_bytes(&work.stage))
                <= self.max_text_bytes,
            "cache owner text admission is aggregate"
        );
        effects
    }
}

fn directory_text_bytes(directory: &Directory) -> usize {
    match directory {
        Directory::Kind { profile, .. } => profile.as_os_str().len(),
        Directory::Artifact(scope) => scope.path_bytes(),
        _ => 0,
    }
}

fn item_text_bytes(item: &Item) -> usize {
    item.id.len() + item.slot.len() + item.unit.as_ref().map_or(0, String::len)
}
fn scoped_text_bytes(item: &ScopedItem) -> usize {
    item_text_bytes(&item.item) + item.scope.path_bytes()
}
fn owner_text_bytes(owner: &Owner) -> usize {
    owner.host.len() + owner.incarnation.len()
}
fn preview_text_bytes(preview: &Preview) -> usize {
    owner_text_bytes(&preview.owner)
        + preview.token.len()
        + preview.inventory.len()
        + preview.items.iter().map(item_text_bytes).sum::<usize>()
}
fn kind_text_bytes(kind: &Kind) -> usize {
    match kind {
        Kind::Inventory { inventory, slot } => inventory.len() + slot.len(),
        Kind::Prepare(id) | Kind::Execute(id) => id.len(),
    }
}

fn snapshot_path_bytes(snapshot: &Snapshot) -> usize {
    snapshot.root.as_os_str().len()
        + snapshot
            .entries
            .keys()
            .chain(snapshot.ancestors.keys())
            .map(|path| path.as_os_str().len())
            .sum::<usize>()
        + snapshot.roots.keys().map(Scope::path_bytes).sum::<usize>()
        + snapshot
            .roots
            .values()
            .flatten()
            .map(|path| path.as_os_str().len())
            .sum::<usize>()
}

fn scans<D: Distance>(
    home: &Path,
    scheduler: &Scheduler<D>,
    slots: &[crate::scheduler::SlotKey],
    selected: &[ScopedItem],
) -> VecDeque<(String, Scan)> {
    slots
        .iter()
        .map(|key| {
            let (repository, index, _) = scheduler.cache_identity(*key);
            let name = slot::slot_name(repository, index);
            let scopes = selected
                .iter()
                .filter(|item| item.item.slot == name)
                .map(|item| item.scope.clone())
                .collect();
            (
                name,
                Scan::new(
                    SlotDirectory::new(home, repository, index).target(),
                    Some(scopes),
                ),
            )
        })
        .collect()
}

fn removal(items: &[ScopedItem], snapshots: HashMap<String, Snapshot>) -> Stage {
    let mut roots = Vec::new();
    let mut paths = VecDeque::new();
    let mut remaining = vec![0; items.len()];
    for (name, snapshot) in snapshots {
        let target = roots.len();
        roots.push(snapshot.root);
        let roots_by_item = items
            .iter()
            .enumerate()
            .filter(|(_, item)| item.item.slot == name)
            .flat_map(|(index, item)| {
                snapshot.roots[&item.scope]
                    .iter()
                    .map(move |root| (root.as_path(), index))
            })
            .collect::<HashMap<_, _>>();
        let mut selected = snapshot
            .entries
            .into_iter()
            .map(|(path, metadata)| {
                let item = path
                    .ancestors()
                    .find_map(|ancestor| roots_by_item.get(ancestor))
                    .copied()
                    .expect("manifest entry belongs to selected scope");
                remaining[item] += 1;
                (item, target, path, metadata.is_dir())
            })
            .collect::<Vec<_>>();
        selected.sort_by_key(|(_, _, path, _)| std::cmp::Reverse(path.components().count()));
        paths.extend(selected);
    }
    let mut paths = paths.into_iter().collect::<Vec<_>>();
    paths.sort_by_key(|(item, _, path, _)| (*item, std::cmp::Reverse(path.components().count())));
    Stage::Remove {
        roots,
        paths: paths.into(),
        remaining,
        removed: Vec::new(),
    }
}

fn outcome_reserve<'a>(ids: impl Iterator<Item = &'a str>) -> usize {
    let lengths = ids.map(str::len).collect::<Vec<_>>();
    lengths.iter().sum::<usize>() + lengths.iter().max().copied().unwrap_or(0)
}
fn stage_text_bytes(stage: &Stage) -> usize {
    match stage {
        Stage::Inventory(scan) => scan.text_bytes(),
        Stage::Selected {
            pending,
            snapshots,
            measurements,
        } => selected_text_bytes(pending, snapshots, measurements),
        Stage::Invalidate {
            directories,
            snapshots,
        } => {
            directories
                .iter()
                .map(|directory| directory.path().as_os_str().len())
                .sum::<usize>()
                + snapshots
                    .iter()
                    .map(|(name, snapshot)| name.len() + snapshot_path_bytes(snapshot))
                    .sum::<usize>()
        }
        Stage::Remove { roots, paths, .. } => {
            roots
                .iter()
                .map(|root| root.as_os_str().len())
                .sum::<usize>()
                + paths
                    .iter()
                    .map(|(_, _, path, _)| path.as_os_str().len())
                    .sum::<usize>()
        }
        Stage::Finished => 0,
    }
}
fn outcome_text_bytes(outcome: Option<&Outcome>) -> usize {
    match outcome {
        Some(Outcome::Completed { removed }) => removed.iter().map(String::len).sum(),
        Some(Outcome::Partial {
            removed, failed, ..
        }) => removed.iter().map(String::len).sum::<usize>() + failed.len(),
        _ => 0,
    }
}
fn selected_text_bytes(
    pending: &VecDeque<(String, Scan)>,
    snapshots: &HashMap<String, Snapshot>,
    measurements: &HashMap<String, BTreeMap<Scope, Measurement>>,
) -> usize {
    pending
        .iter()
        .map(|(name, scan)| name.len() + scan.text_bytes())
        .sum::<usize>()
        + snapshots
            .iter()
            .map(|(name, snapshot)| name.len() + snapshot_path_bytes(snapshot))
            .sum::<usize>()
        + measurements
            .iter()
            .map(|(name, groups)| name.len() + groups.keys().map(Scope::path_bytes).sum::<usize>())
            .sum::<usize>()
}

fn milliseconds() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("clock is after epoch")
        .as_millis() as u64
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::slot::{CompilationRun, SlotRecord};
    use crate::snapshot::Revision;
    use crate::snapshot::tests::TempDir;
    struct EqualDistance;
    impl Distance for EqualDistance {
        fn distance(&mut self, _: &Path, _: &Revision, _: &Revision) -> u64 {
            0
        }
    }

    struct Fixture {
        home: TempDir,
        scheduler: Scheduler<EqualDistance>,
        cache: Cache,
        target: PathBuf,
        source: PathBuf,
    }

    impl Fixture {
        fn new() -> Self {
            let home = TempDir::new();
            let repository = home.0.join("project/.git");
            let directory = SlotDirectory::new(&home.0, &repository, 0);
            let target = directory.target();
            let source = directory.source();
            std::fs::create_dir_all(&source).unwrap();
            std::fs::write(source.join("keep"), "source").unwrap();
            for directory in [
                "debug/incremental/a",
                "debug/incremental/b",
                "debug/deps",
                "debug/.fingerprint/pkg-aaaaaaaaaaaaaaaa",
            ] {
                std::fs::create_dir_all(target.join(directory)).unwrap();
            }
            for path in [
                "debug/incremental/a/file",
                "debug/incremental/b/file",
                "debug/deps/pkg-aaaaaaaaaaaaaaaa",
                "debug/.fingerprint/pkg-aaaaaaaaaaaaaaaa/file",
                "debug/product",
            ] {
                std::fs::write(target.join(path), [7_u8; 8192]).unwrap();
            }
            std::fs::write(home.0.join("keep.log"), "logs").unwrap();
            let mut scheduler = Scheduler::new(1, u64::MAX, EqualDistance);
            let revision = Revision::of_tree("1234");
            let compilation = crate::cargo::Compilation::new(
                Path::new(""),
                &crate::cargo::Operation {
                    command: crate::cargo::Command::Check,
                    args: Vec::new(),
                    rustflags: Vec::new(),
                },
            );
            scheduler.restore(
                repository,
                0,
                revision.clone(),
                SlotRecord {
                    units: [("debug/aaaaaaaaaaaaaaaa".to_owned(), 1)]
                        .into_iter()
                        .collect(),
                    compilations: vec![CompilationRun {
                        compilation,
                        at: 1,
                        units: ["debug/aaaaaaaaaaaaaaaa".into()].into_iter().collect(),
                        build_ms: None,
                        peak_memory: None,
                    }],
                    passed: vec![crate::passed::PassedBinary {
                        id: "test".into(),
                        executable: "debug/product".into(),
                        file: crate::passed::FileIdentity {
                            size: 8192,
                            modified_ns: 0,
                            inode: 1,
                        },
                        package: PathBuf::new(),
                        tree: revision,
                    }],
                    ..SlotRecord::default()
                },
            );
            let cache = Cache::new(&home.0).unwrap();
            Self {
                home,
                scheduler,
                cache,
                target,
                source,
            }
        }

        fn operate(&mut self, operation: Operation) -> (Response, Vec<Effect>) {
            self.cache
                .operate(&self.home.0, &mut self.scheduler, operation)
        }
        fn drain(&mut self, entries: usize) -> Vec<Effect> {
            let mut effects = Vec::new();
            while let Some(work) = self.cache.take_work() {
                let (work, result) = work.run_bounded(entries, SCAN_TIME);
                effects.extend(self.cache.completed(
                    &self.home.0,
                    &mut self.scheduler,
                    work,
                    result,
                ));
            }
            effects
        }
        fn slots(&self) -> Vec<String> {
            self.scheduler.cache_slots().map(|(name, _)| name).collect()
        }
        fn inventory(&mut self) -> Inventory {
            self.inventory_slots(self.slots())
        }
        fn inventory_slots(&mut self, slots: Vec<String>) -> Inventory {
            let (Response::Inventory(mut inventory), _) = self.operate(Operation::Inventory {
                owner: self.cache.owner.clone(),
                slots,
            }) else {
                panic!("inventory")
            };
            while !self.cache.inventories[&inventory.revision].ready {
                self.drain(MAX_ENTRIES);
                let (Response::Inventory(next), _) = self.operate(Operation::InventoryPage {
                    owner: inventory.owner.clone(),
                    inventory: inventory.revision.clone(),
                    after: inventory.next.clone().unwrap(),
                }) else {
                    panic!("inventory progress")
                };
                inventory = next;
            }
            inventory
        }
        fn all_items(&mut self, first: &Inventory) -> Vec<Item> {
            let mut all = first.items.clone();
            let mut next = first.next.clone();
            while let Some(after) = next {
                let (Response::Inventory(page), effects) = self.operate(Operation::InventoryPage {
                    owner: first.owner.clone(),
                    inventory: first.revision.clone(),
                    after,
                }) else {
                    panic!("page")
                };
                assert!(effects.is_empty());
                assert_eq!(page.revision, first.revision);
                assert_eq!(page.measured_ms, first.measured_ms);
                assert_eq!(page.expires_ms, first.expires_ms);
                assert!(page.items.len() <= MAX_INVENTORY_PAGE_ITEMS);
                all.extend(page.items);
                next = page.next;
            }
            all
        }
        fn preview(&mut self, inventory: &Inventory, items: Vec<String>) -> Preview {
            let (Response::Preparing { preparation }, effects) = self.operate(Operation::Preview {
                owner: inventory.owner.clone(),
                inventory: inventory.revision.clone(),
                items,
            }) else {
                panic!("prepare")
            };
            assert!(effects.is_empty());
            self.drain(MAX_ENTRIES);
            let (Response::Preview(preview), _) = self.operate(Operation::PreviewStatus {
                owner: inventory.owner.clone(),
                preparation,
            }) else {
                panic!("preview")
            };
            assert!(self.scheduler.cache_slots().all(|(_, busy)| !busy));
            preview
        }
        fn execute(&mut self, preview: Preview) -> (Response, Vec<Effect>) {
            let (response, mut effects) = self.operate(Operation::Execute {
                preview: preview.clone(),
            });
            if matches!(
                response,
                Response::Receipt {
                    executing: true,
                    ..
                }
            ) {
                effects.extend(self.drain(MAX_ENTRIES));
                (self.operate(Operation::Receipt { preview }).0, effects)
            } else {
                (response, effects)
            }
        }
        fn item(&self, inventory: &Inventory, path: &str) -> Item {
            let scope = Scope::Incremental(PathBuf::from(path));
            self.cache.inventories[&inventory.revision]
                .scopes
                .iter()
                .find(|item| item.scope == scope)
                .unwrap()
                .item
                .clone()
        }
    }

    #[test]
    fn bounded_inventory_releases_each_slice_and_retains_only_compact_scopes() {
        let mut fixture = Fixture::new();
        let (Response::Inventory(mut inventory), _) = fixture.operate(Operation::Inventory {
            owner: fixture.cache.owner.clone(),
            slots: fixture.slots(),
        }) else {
            panic!("inventory")
        };
        assert!(inventory.items.is_empty());
        assert!(!inventory.complete);
        let mut slices = 0;
        while !fixture.cache.inventories[&inventory.revision].ready {
            fixture.drain(2);
            slices += 1;
            assert!(fixture.scheduler.cache_slots().all(|(_, busy)| !busy));
            let measured = &fixture.cache.inventories[&inventory.revision];
            if let Some(scan) = &measured.scan {
                assert!(scan.snapshot.entries.is_empty());
                assert!(scan.snapshot.roots.is_empty());
            }
            let (Response::Inventory(next), _) = fixture.operate(Operation::InventoryPage {
                owner: inventory.owner.clone(),
                inventory: inventory.revision.clone(),
                after: inventory.next.clone().unwrap(),
            }) else {
                panic!("progress")
            };
            inventory = next;
        }
        assert!(slices > 1);
        assert!(inventory.complete);
        assert_eq!(inventory.items.len(), 3);
    }

    #[test]
    fn inventory_generation_changes_refuse_continuation_and_old_selection() {
        let mut fixture = Fixture::new();
        let inventory = fixture.inventory();
        let slot = fixture
            .scheduler
            .cache_acquire(&inventory.slots[0].id)
            .unwrap();
        fixture.scheduler.cache_writing(slot);
        fixture.scheduler.cache_release(slot, true);
        assert_eq!(
            fixture
                .operate(Operation::InventoryPage {
                    owner: inventory.owner.clone(),
                    inventory: inventory.revision.clone(),
                    after: fixture.cache.inventories[&inventory.revision]
                        .progress
                        .clone()
                })
                .0,
            Response::Refused {
                reason: Refusal::Changed
            }
        );
        assert_eq!(
            fixture
                .operate(Operation::Preview {
                    owner: inventory.owner,
                    inventory: inventory.revision,
                    items: vec![inventory.items[0].id.clone()]
                })
                .0,
            Response::Refused {
                reason: Refusal::Changed
            }
        );
    }

    #[test]
    fn preview_is_inert_and_execute_removes_only_selected_with_exact_receipt_replay() {
        let mut fixture = Fixture::new();
        let inventory = fixture.inventory();
        let item = inventory
            .items
            .iter()
            .find(|item| item.unit.is_some())
            .unwrap();
        let preview = fixture.preview(&inventory, vec![item.id.clone()]);
        assert!(
            fixture
                .target
                .join("debug/deps/pkg-aaaaaaaaaaaaaaaa")
                .exists()
        );
        let (response, effects) = fixture.execute(preview.clone());
        assert!(
            matches!(response,Response::Receipt { result: Some(Outcome::Completed { ref removed }),executing: false,.. } if removed == std::slice::from_ref(&item.id))
        );
        assert!(effects.iter().any(|effect|matches!(effect,Effect::Persist { record,.. } if record.units.is_empty() && record.passed.is_empty() && record.compilations.is_empty())));
        for path in [
            "debug/deps/pkg-aaaaaaaaaaaaaaaa",
            "debug/.fingerprint/pkg-aaaaaaaaaaaaaaaa",
        ] {
            assert!(!fixture.target.join(path).exists());
        }
        for path in [
            "debug/incremental/a/file",
            "debug/incremental/b/file",
            "debug/product",
        ] {
            assert!(fixture.target.join(path).exists());
        }
        assert!(fixture.source.join("keep").exists());
        assert!(fixture.home.0.join("keep.log").exists());
        std::fs::write(
            fixture.target.join("debug/deps/pkg-aaaaaaaaaaaaaaaa"),
            "new artifact",
        )
        .unwrap();
        assert_eq!(fixture.execute(preview.clone()).0, response);
        assert_eq!(fixture.operate(Operation::Receipt { preview }).0, response);
        assert_eq!(
            std::fs::read_to_string(fixture.target.join("debug/deps/pkg-aaaaaaaaaaaaaaaa"))
                .unwrap(),
            "new artifact"
        );
    }

    #[test]
    fn preview_refreshes_selected_measurement_and_ignores_unrelated_target_changes() {
        let mut fixture = Fixture::new();
        let inventory = fixture.inventory();
        let item = fixture.item(&inventory, "debug/incremental/a");
        std::fs::write(
            fixture.target.join("debug/incremental/a/file"),
            [1_u8; 32768],
        )
        .unwrap();
        let preview = fixture.preview(&inventory, vec![item.id.clone()]);
        assert!(preview.items[0].reclaimable_bytes > item.reclaimable_bytes);
        std::fs::create_dir_all(fixture.target.join("tmp/unrelated")).unwrap();
        std::fs::write(fixture.target.join("tmp/unrelated/keep"), "unrelated").unwrap();
        assert!(matches!(
            fixture.execute(preview).0,
            Response::Receipt {
                result: Some(Outcome::Completed { .. }),
                ..
            }
        ));
        assert!(fixture.target.join("tmp/unrelated/keep").exists());
    }

    #[test]
    fn changed_selected_contents_and_added_unit_roots_refuse_before_deletion() {
        for added_root in [false, true] {
            let mut fixture = Fixture::new();
            let inventory = fixture.inventory();
            let item = inventory
                .items
                .iter()
                .find(|item| item.unit.is_some())
                .unwrap();
            let preview = fixture.preview(&inventory, vec![item.id.clone()]);
            let path = if added_root {
                "debug/deps/libpkg-aaaaaaaaaaaaaaaa.rlib"
            } else {
                "debug/.fingerprint/pkg-aaaaaaaaaaaaaaaa/file"
            };
            std::fs::write(fixture.target.join(path), "changed").unwrap();
            assert!(matches!(
                fixture.execute(preview).0,
                Response::Receipt {
                    result: Some(Outcome::Refused {
                        reason: Refusal::Changed
                    }),
                    ..
                }
            ));
            assert!(
                fixture
                    .target
                    .join("debug/deps/pkg-aaaaaaaaaaaaaaaa")
                    .exists()
            );
            assert!(fixture.scheduler.cache_slots().all(|(_, busy)| !busy));
        }
    }

    #[test]
    fn busy_foreign_mutated_and_duplicate_selection_do_not_execute() {
        let mut fixture = Fixture::new();
        let inventory = fixture.inventory();
        let preview = fixture.preview(&inventory, vec![inventory.items[0].id.clone()]);
        let held = fixture
            .scheduler
            .cache_acquire(&inventory.slots[0].id)
            .unwrap();
        assert_eq!(
            fixture.execute(preview.clone()).0,
            Response::Refused {
                reason: Refusal::Busy
            }
        );
        fixture.scheduler.cache_release(held, false);
        let mut foreign = preview.clone();
        foreign.owner.host = "foreign".into();
        assert_eq!(
            fixture.execute(foreign).0,
            Response::Unknown {
                reason: Refusal::ForeignOwner
            }
        );
        let mut changed = preview.clone();
        changed.items[0].reclaimable_bytes = Some(changed.items[0].reclaimable_bytes.unwrap() + 1);
        assert_eq!(
            fixture.execute(changed).0,
            Response::Refused {
                reason: Refusal::PayloadChanged
            }
        );
        assert_eq!(
            fixture
                .operate(Operation::Preview {
                    owner: inventory.owner,
                    inventory: inventory.revision,
                    items: vec![inventory.items[0].id.clone(), inventory.items[0].id.clone()]
                })
                .0,
            Response::Refused {
                reason: Refusal::PayloadChanged
            }
        );
        assert!(
            fixture
                .target
                .join("debug/deps/pkg-aaaaaaaaaaaaaaaa")
                .exists()
        );
    }

    #[test]
    fn cancellation_after_execute_starts_preserves_the_owner_execution() {
        let mut fixture = Fixture::new();
        let inventory = fixture.inventory();
        let preview = fixture.preview(&inventory, vec![inventory.items[0].id.clone()]);
        let initial = fixture
            .operate(Operation::Execute {
                preview: preview.clone(),
            })
            .0;
        assert!(matches!(
            initial,
            Response::Receipt {
                executing: true,
                result: None,
                ..
            }
        ));
        assert_eq!(
            fixture
                .operate(Operation::Cancel {
                    preview: preview.clone()
                })
                .0,
            initial
        );
        fixture.drain(2);
        assert!(matches!(
            fixture.operate(Operation::Receipt { preview }).0,
            Response::Receipt {
                executing: false,
                result: Some(Outcome::Completed { .. }),
                ..
            }
        ));
    }

    #[test]
    fn cancelled_approval_expiry_and_restart_keep_outcomes_honest() {
        let mut fixture = Fixture::new();
        let inventory = fixture.inventory();
        let preview = fixture.preview(&inventory, vec![inventory.items[0].id.clone()]);
        let cancelled = fixture
            .operate(Operation::Cancel {
                preview: preview.clone(),
            })
            .0;
        assert!(matches!(
            cancelled,
            Response::Receipt {
                result: Some(Outcome::Cancelled),
                ..
            }
        ));
        assert_eq!(fixture.execute(preview.clone()).0, cancelled);
        fixture
            .cache
            .receipts
            .get_mut(&preview.token)
            .unwrap()
            .expires = Instant::now();
        assert_eq!(
            fixture
                .operate(Operation::Receipt {
                    preview: preview.clone()
                })
                .0,
            Response::Unknown {
                reason: Refusal::UnknownReceipt
            }
        );
        fixture.cache = Cache::new(&fixture.home.0).unwrap();
        assert_eq!(
            fixture.execute(preview).0,
            Response::Unknown {
                reason: Refusal::ForeignOwner
            }
        );
        assert!(
            fixture
                .target
                .join("debug/deps/pkg-aaaaaaaaaaaaaaaa")
                .exists()
        );
    }

    #[test]
    fn expired_worker_drops_iterators_and_releases_exclusion() {
        let mut fixture = Fixture::new();
        let (Response::Inventory(inventory), _) = fixture.operate(Operation::Inventory {
            owner: fixture.cache.owner.clone(),
            slots: fixture.slots(),
        }) else {
            panic!("inventory")
        };
        fixture.cache.work.as_mut().unwrap().expires = Instant::now();
        fixture.drain(2);
        assert!(fixture.cache.active.is_none());
        assert!(fixture.scheduler.cache_slots().all(|(_, busy)| !busy));
        assert_eq!(
            fixture.cache.inventories[&inventory.revision].wire.slots[0].incomplete,
            Some(Refusal::Expired)
        );
    }

    #[test]
    fn ordinary_symlink_leaves_are_removed_without_following_their_referents() {
        let mut fixture = Fixture::new();
        let directory = fixture
            .target
            .join("debug/build/pkg-aaaaaaaaaaaaaaaa/out/lib");
        std::fs::create_dir_all(&directory).unwrap();
        std::fs::write(directory.join("libpkg.0.1.dylib"), "library").unwrap();
        std::os::unix::fs::symlink("libpkg.0.1.dylib", directory.join("libpkg.0.dylib")).unwrap();
        std::os::unix::fs::symlink("libpkg.0.dylib", directory.join("libpkg.dylib")).unwrap();
        std::os::unix::fs::symlink(fixture.source.join("keep"), directory.join("external-tool"))
            .unwrap();
        let inventory = fixture.inventory();
        assert!(inventory.complete);
        let item = inventory
            .items
            .iter()
            .find(|item| item.unit.is_some())
            .unwrap();
        let preview = fixture.preview(&inventory, vec![item.id.clone()]);
        assert!(matches!(
            fixture.execute(preview).0,
            Response::Receipt {
                result: Some(Outcome::Completed { .. }),
                ..
            }
        ));
        assert!(!directory.exists());
        assert_eq!(
            std::fs::read_to_string(fixture.source.join("keep")).unwrap(),
            "source"
        );
    }

    #[test]
    fn symlink_ancestors_refuse_selected_revalidation_and_preserve_external_data() {
        let mut fixture = Fixture::new();
        let inventory = fixture.inventory();
        let item = fixture.item(&inventory, "debug/incremental/a");
        let preview = fixture.preview(&inventory, vec![item.id]);
        let debug = fixture.target.join("debug");
        std::fs::rename(&debug, fixture.target.join("saved-debug")).unwrap();
        std::os::unix::fs::symlink(&fixture.source, &debug).unwrap();
        assert!(matches!(
            fixture.execute(preview).0,
            Response::Receipt {
                result: Some(Outcome::Refused {
                    reason: Refusal::Protected
                }),
                ..
            }
        ));
        assert!(fixture.source.join("keep").exists());
        assert!(
            fixture
                .target
                .join("saved-debug/incremental/a/file")
                .exists()
        );
    }

    #[test]
    fn hardlink_estimates_match_native_accounting_and_external_links_survive() {
        let mut fixture = Fixture::new();
        let linked = fixture.target.join("debug/incremental/a/file");
        std::fs::hard_link(&linked, fixture.home.0.join("external-link")).unwrap();
        std::fs::hard_link(
            fixture.target.join("debug/incremental/b/file"),
            fixture.target.join("debug/incremental/b/another-link"),
        )
        .unwrap();
        let inventory = fixture.inventory();
        let original = slot::evictables(&fixture.target, &HashMap::new()).unwrap();
        let sizes = slot::disk_usage(&fixture.target, &original).unwrap();
        for (native, size) in original.iter().zip(sizes.freeable) {
            let scope = native.unit.clone().map(Scope::Unit).unwrap_or_else(|| {
                Scope::Incremental(
                    native.paths[0]
                        .strip_prefix(&fixture.target)
                        .unwrap()
                        .to_owned(),
                )
            });
            let item = fixture.cache.inventories[&inventory.revision]
                .scopes
                .iter()
                .find(|item| item.scope == scope)
                .unwrap();
            assert_eq!(item.item.reclaimable_bytes, Some(size));
        }
        let item = fixture.item(&inventory, "debug/incremental/a");
        let preview = fixture.preview(&inventory, vec![item.id]);
        fixture.execute(preview);
        assert!(fixture.home.0.join("external-link").exists());
    }

    #[test]
    fn partial_filesystem_failure_preserves_exact_completed_items() {
        let mut fixture = Fixture::new();
        let inventory = fixture.inventory();
        let a = fixture.item(&inventory, "debug/incremental/a");
        let b = fixture.item(&inventory, "debug/incremental/b");
        let preview = fixture.preview(&inventory, vec![a.id.clone(), b.id.clone()]);
        fixture.operate(Operation::Execute {
            preview: preview.clone(),
        });
        loop {
            let mut work = fixture.cache.take_work().unwrap();
            if matches!(work.stage, Stage::Remove { .. }) {
                std::fs::remove_file(fixture.target.join("debug/incremental/b/file")).unwrap();
                let (work, result) = work.run();
                fixture
                    .cache
                    .completed(&fixture.home.0, &mut fixture.scheduler, work, result);
                break;
            }
            let (next, result) = work.run();
            work = next;
            fixture
                .cache
                .completed(&fixture.home.0, &mut fixture.scheduler, work, result);
        }
        assert!(
            matches!(fixture.operate(Operation::Receipt { preview }).0,Response::Receipt { result: Some(Outcome::Partial { removed,failed,reason: Refusal::Io }),.. } if removed == vec![a.id] && failed == b.id)
        );
        assert!(
            fixture
                .target
                .join("debug/deps/pkg-aaaaaaaaaaaaaaaa")
                .exists()
        );
    }

    #[test]
    fn custom_profile_kind_names_resolve_the_same_compiled_groups() {
        let mut fixture = Fixture::new();
        for profile in ["build", "deps", "x86_64-unknown-linux-gnu/build"] {
            let deps = fixture.target.join(profile).join("deps");
            let fingerprint = fixture
                .target
                .join(profile)
                .join(".fingerprint/pkg-0123456789abcdef");
            std::fs::create_dir_all(&deps).unwrap();
            std::fs::create_dir_all(&fingerprint).unwrap();
            std::fs::write(deps.join("libpkg-0123456789abcdef.rlib"), "artifact").unwrap();
            std::fs::write(fingerprint.join("lib-pkg"), "fingerprint").unwrap();
        }
        let inventory = fixture.inventory();
        let original = slot::evictables(&fixture.target, &HashMap::new()).unwrap();
        assert_eq!(original.len(), inventory.items.len());
        for profile in ["build", "deps", "x86_64-unknown-linux-gnu/build"] {
            assert!(
                inventory.items.iter().any(
                    |item| item.unit.as_deref() == Some(&format!("{profile}/0123456789abcdef"))
                )
            );
        }
    }

    #[test]
    fn selected_manifest_limits_use_small_configured_budgets_without_partial_approval() {
        let fixture = Fixture::new();
        for (entries, paths) in [(1, MAX_PATH_BYTES), (MAX_ENTRIES, 256)] {
            let mut scan = Scan::new(
                fixture.target.clone(),
                Some(vec![Scope::Incremental(PathBuf::from(
                    "debug/incremental/a",
                ))]),
            );
            scan.max_entries = entries;
            scan.max_path_bytes = paths;
            loop {
                match scan.step(&mut 2, Instant::now() + Duration::from_millis(50)) {
                    Ok(false) => {}
                    Err(reason) => {
                        assert_eq!(reason, Refusal::Limit);
                        break;
                    }
                    Ok(true) => panic!("oversized selected manifest approved"),
                }
            }
        }
        assert!(fixture.target.join("debug/incremental/a/file").exists());
    }

    #[test]
    fn expiry_deadline_drops_paused_iterators_without_another_request() {
        let mut fixture = Fixture::new();
        let (Response::Inventory(inventory), _) = fixture.operate(Operation::Inventory {
            owner: fixture.cache.owner.clone(),
            slots: fixture.slots(),
        }) else {
            panic!("inventory")
        };
        fixture.drain(2);
        assert!(
            fixture.cache.inventories[&inventory.revision]
                .scan
                .is_some()
        );
        let deadline = fixture
            .cache
            .next_expiry()
            .expect("retained resource owns an expiry deadline");
        fixture.cache.expire(deadline);
        assert!(fixture.cache.inventories.is_empty());
        assert!(fixture.cache.next_expiry().is_none());
        assert!(fixture.cache.active.is_none());
        assert!(fixture.scheduler.cache_slots().all(|(_, busy)| !busy));
    }

    #[test]
    fn slot_listing_pages_service_ids_and_refuses_unknown_foreign_or_duplicate_selection() {
        let mut fixture = Fixture::new();
        for index in 1..=MAX_SLOTS {
            fixture.scheduler.restore(
                fixture.home.0.join(format!("project-{index}/.git")),
                0,
                Revision::of_tree("1234"),
                SlotRecord::default(),
            );
        }
        let owner = fixture.cache.owner.clone();
        let (
            Response::Slots {
                owner: listed_owner,
                slots,
                next,
            },
            _,
        ) = fixture.operate(Operation::Slots {
            owner: owner.clone(),
            after: None,
        })
        else {
            panic!("slot listing")
        };
        assert_eq!(listed_owner, owner);
        assert_eq!(slots.len(), MAX_SLOTS);
        assert!(fixture.cache.inventories.is_empty());
        assert!(fixture.cache.active.is_none());
        let (
            Response::Slots {
                slots: last,
                next: None,
                ..
            },
            _,
        ) = fixture.operate(Operation::Slots {
            owner: owner.clone(),
            after: next,
        })
        else {
            panic!("last slot page")
        };
        assert_eq!(last.len(), 1);
        assert_eq!(last[0].id, fixture.slots()[MAX_SLOTS]);
        let mut foreign = owner.clone();
        foreign.incarnation = "foreign".into();
        for operation in [
            Operation::Slots {
                owner: owner.clone(),
                after: Some(slots[0].id.clone()),
            },
            Operation::Slots {
                owner: owner.clone(),
                after: Some("unknown".into()),
            },
            Operation::Inventory {
                owner: owner.clone(),
                slots: vec!["unknown".into()],
            },
            Operation::Inventory {
                owner: owner.clone(),
                slots: Vec::new(),
            },
            Operation::Inventory {
                owner: owner.clone(),
                slots: vec![slots[0].id.clone(); 2],
            },
        ] {
            assert_eq!(
                fixture.operate(operation).0,
                Response::Refused {
                    reason: Refusal::PayloadChanged
                }
            );
        }
        assert_eq!(
            fixture
                .operate(Operation::Inventory {
                    owner: owner.clone(),
                    slots: fixture.slots()
                })
                .0,
            Response::Refused {
                reason: Refusal::Limit
            }
        );
        for operation in [
            Operation::Slots {
                owner: foreign.clone(),
                after: None,
            },
            Operation::Inventory {
                owner: foreign,
                slots: vec![slots[0].id.clone()],
            },
        ] {
            assert_eq!(
                fixture.operate(operation).0,
                Response::Refused {
                    reason: Refusal::ForeignOwner
                }
            );
        }
        assert!(fixture.cache.inventories.is_empty());
        assert!(fixture.scheduler.cache_slots().all(|(_, busy)| !busy));
    }

    #[test]
    fn chosen_later_slot_is_reachable_after_release_without_removing_earlier_cache() {
        let mut fixture = Fixture::new();
        let repository = fixture.home.0.join("second-project/.git");
        let second = SlotDirectory::new(&fixture.home.0, &repository, 0).target();
        fixture.scheduler.restore(
            repository,
            0,
            Revision::of_tree("5678"),
            SlotRecord::default(),
        );
        let profile = "p".repeat(180);
        for target in [&fixture.target, &second] {
            let deps = target.join(&profile).join("deps");
            std::fs::create_dir_all(&deps).unwrap();
            for index in 0..70 {
                std::fs::write(deps.join(format!("libpkg-{index:016x}.rlib")), "artifact").unwrap();
            }
        }
        fixture.cache.max_text_bytes = 40 * 1024;
        let combined = fixture.inventory();
        assert_eq!(combined.slots[1].incomplete, Some(Refusal::Limit));
        assert!(
            !fixture
                .all_items(&combined)
                .iter()
                .any(|item| item.slot == combined.slots[1].id)
        );
        assert_eq!(
            fixture
                .operate(Operation::ReleaseInventory {
                    owner: combined.owner.clone(),
                    inventory: combined.revision.clone()
                })
                .0,
            Response::Released {
                inventory: combined.revision
            }
        );
        assert!(fixture.cache.inventories.is_empty());
        let selected = fixture.inventory_slots(vec![combined.slots[1].id.clone()]);
        assert!(selected.complete);
        assert_eq!(selected.slots.len(), 1);
        let items = fixture.all_items(&selected);
        assert_eq!(items.len(), 70);
        assert!(items.iter().all(|item| item.slot == combined.slots[1].id));
        let preview = fixture.preview(&selected, vec![items[0].id.clone()]);
        fixture.operate(Operation::ReleaseInventory {
            owner: selected.owner,
            inventory: selected.revision,
        });
        assert!(matches!(
            fixture.execute(preview).0,
            Response::Receipt {
                result: Some(Outcome::Completed { .. }),
                ..
            }
        ));
        assert_eq!(
            std::fs::read_dir(fixture.target.join(&profile).join("deps"))
                .unwrap()
                .count(),
            70
        );
        assert!(fixture.target.join("debug/incremental/a/file").exists());
        assert_eq!(
            std::fs::read_dir(second.join(&profile).join("deps"))
                .unwrap()
                .count(),
            69
        );
        assert!(fixture.cache.budget_bytes() <= fixture.cache.max_text_bytes);
    }

    #[test]
    fn releasing_active_and_paused_discovery_retires_iterators_and_exclusion() {
        for active in [true, false] {
            let mut fixture = Fixture::new();
            let (Response::Inventory(inventory), _) = fixture.operate(Operation::Inventory {
                owner: fixture.cache.owner.clone(),
                slots: fixture.slots(),
            }) else {
                panic!("inventory")
            };
            let pending = if active {
                let work = fixture.cache.take_work().unwrap();
                let (work, result) = work.run_bounded(2, Duration::from_millis(50));
                assert_eq!(result, Ok(false));
                Some(work)
            } else {
                fixture.drain(2);
                assert!(
                    fixture.cache.inventories[&inventory.revision]
                        .scan
                        .is_some()
                );
                None
            };
            let retained = fixture.cache.budget_bytes();
            assert_eq!(
                fixture
                    .operate(Operation::ReleaseInventory {
                        owner: inventory.owner.clone(),
                        inventory: inventory.revision.clone(),
                    })
                    .0,
                Response::Released {
                    inventory: inventory.revision.clone()
                }
            );
            if let Some(work) = pending {
                assert!(work.cancel.load(Ordering::Acquire));
                let (work, result) = work.run_bounded(2, Duration::from_millis(50));
                assert_eq!(result, Err(Refusal::Expired));
                fixture
                    .cache
                    .completed(&fixture.home.0, &mut fixture.scheduler, work, result);
                fixture.cache.expire(Instant::now());
            }
            assert!(fixture.cache.inventories.is_empty());
            assert!(fixture.cache.active.is_none());
            assert!(fixture.cache.work.is_none());
            assert!(fixture.cache.next_expiry().is_none());
            assert!(fixture.cache.budget_bytes() < retained);
            assert!(fixture.scheduler.cache_slots().all(|(_, busy)| !busy));
            assert!(fixture.target.join("debug/incremental/a/file").exists());
            assert_eq!(
                fixture
                    .operate(Operation::InventoryPage {
                        owner: inventory.owner,
                        inventory: inventory.revision,
                        after: inventory.next.unwrap(),
                    })
                    .0,
                Response::Refused {
                    reason: Refusal::Expired
                }
            );
        }
    }

    #[test]
    fn releasing_completed_discovery_preserves_issued_preparations_and_receipts() {
        for preparing in [true, false] {
            let mut fixture = Fixture::new();
            let inventory = fixture.inventory();
            let item = fixture.item(&inventory, "debug/incremental/a");
            let (Response::Preparing { preparation }, _) = fixture.operate(Operation::Preview {
                owner: inventory.owner.clone(),
                inventory: inventory.revision.clone(),
                items: vec![item.id],
            }) else {
                panic!("preparation")
            };
            if !preparing {
                fixture.drain(MAX_ENTRIES);
            }
            let mut foreign = inventory.owner.clone();
            foreign.host = "foreign".into();
            assert_eq!(
                fixture
                    .operate(Operation::ReleaseInventory {
                        owner: foreign,
                        inventory: inventory.revision.clone()
                    })
                    .0,
                Response::Refused {
                    reason: Refusal::ForeignOwner
                }
            );
            assert_eq!(
                fixture
                    .operate(Operation::ReleaseInventory {
                        owner: inventory.owner.clone(),
                        inventory: "unknown".into()
                    })
                    .0,
                Response::Refused {
                    reason: Refusal::Expired
                }
            );
            assert!(fixture.cache.inventories.contains_key(&inventory.revision));
            fixture.operate(Operation::ReleaseInventory {
                owner: inventory.owner.clone(),
                inventory: inventory.revision,
            });
            assert!(fixture.cache.inventories.is_empty());
            fixture.drain(MAX_ENTRIES);
            let (Response::Preview(preview), _) = fixture.operate(Operation::PreviewStatus {
                owner: inventory.owner,
                preparation,
            }) else {
                panic!("self-contained preview")
            };
            let completed = fixture.execute(preview.clone()).0;
            assert!(matches!(
                completed,
                Response::Receipt {
                    result: Some(Outcome::Completed { .. }),
                    ..
                }
            ));
            assert_eq!(fixture.execute(preview).0, completed);
            assert!(!fixture.target.join("debug/incremental/a").exists());
            assert!(fixture.target.join("debug/incremental/b/file").exists());
        }
    }

    #[test]
    fn global_text_budget_covers_multiple_slot_catalogs_and_cross_page_long_names() {
        let mut fixture = Fixture::new();
        let repository = fixture.home.0.join("second-project/.git");
        let second = SlotDirectory::new(&fixture.home.0, &repository, 0).target();
        fixture.scheduler.restore(
            repository,
            0,
            Revision::of_tree("5678"),
            SlotRecord::default(),
        );
        let profile = "p".repeat(180);
        for target in [&fixture.target, &second] {
            let deps = target.join(&profile).join("deps");
            std::fs::create_dir_all(&deps).unwrap();
            for index in 0..70 {
                std::fs::write(deps.join(format!("libpkg-{index:016x}.rlib")), "artifact").unwrap();
            }
        }
        fixture.cache.max_text_bytes = 40 * 1024;
        let inventory = fixture.inventory();
        let items = fixture.all_items(&inventory);
        assert!(!inventory.complete);
        assert!(
            inventory
                .slots
                .iter()
                .any(|slot| slot.incomplete == Some(Refusal::Limit))
        );
        assert!(
            items.len() > MAX_INVENTORY_PAGE_ITEMS,
            "one slot remains fully measured across pages"
        );
        assert!(items.iter().any(|item| {
            item.unit
                .as_ref()
                .is_some_and(|unit| unit.starts_with(&profile))
        }));
        assert!(fixture.cache.text_bytes() <= fixture.cache.max_text_bytes);
        assert!(fixture.cache.active.is_none());
        assert!(fixture.scheduler.cache_slots().all(|(_, busy)| !busy));
        let eligible = items.iter().find(|item| item.unit.is_none()).unwrap();
        let preview = fixture.preview(&inventory, vec![eligible.id.clone()]);
        assert!(matches!(
            fixture.execute(preview).0,
            Response::Receipt {
                result: Some(Outcome::Completed { .. }),
                ..
            }
        ));
        assert!(fixture.cache.budget_bytes() <= fixture.cache.max_text_bytes);
    }

    #[test]
    fn hardlink_scope_identity_fits_discovery_while_oversized_manifest_refuses_approval() {
        let mut fixture = Fixture::new();
        let scope = fixture
            .target
            .join("debug/incremental")
            .join("h".repeat(180));
        std::fs::create_dir_all(&scope).unwrap();
        let external = fixture.home.0.join("external-links");
        std::fs::create_dir_all(&external).unwrap();
        for index in 0..40 {
            let path = scope.join(index.to_string());
            std::fs::write(&path, "owned artifact").unwrap();
            std::fs::hard_link(path, external.join(index.to_string())).unwrap();
        }
        fixture.cache.max_text_bytes = 8 * 1024;
        let inventory = fixture.inventory();
        assert!(inventory.complete);
        let item = fixture.item(
            &inventory,
            &format!("debug/incremental/{}", "h".repeat(180)),
        );
        assert!(item.reclaimable_bytes.is_some());
        let (Response::Preparing { preparation }, _) = fixture.operate(Operation::Preview {
            owner: inventory.owner.clone(),
            inventory: inventory.revision.clone(),
            items: vec![item.id],
        }) else {
            panic!("selected measurement")
        };
        fixture.drain(MAX_ENTRIES);
        assert_eq!(
            fixture
                .operate(Operation::PreviewStatus {
                    owner: inventory.owner,
                    preparation
                })
                .0,
            Response::Refused {
                reason: Refusal::Limit
            }
        );
        assert!(fixture.cache.text_bytes() <= fixture.cache.max_text_bytes);
        assert!(fixture.scheduler.cache_slots().all(|(_, busy)| !busy));
        assert_eq!(std::fs::read_dir(&external).unwrap().count(), 40);
        assert_eq!(std::fs::read_dir(&scope).unwrap().count(), 40);
    }

    #[test]
    fn multiple_hardlink_heavy_slots_keep_normal_scopes_selectable_with_one_text_budget() {
        let mut fixture = Fixture::new();
        let repository = fixture.home.0.join("hardlinked-project/.git");
        let second = SlotDirectory::new(&fixture.home.0, &repository, 0).target();
        fixture.scheduler.restore(
            repository,
            0,
            Revision::of_tree("5678"),
            SlotRecord::default(),
        );
        let external = fixture.home.0.join("external-links");
        std::fs::create_dir_all(&external).unwrap();
        for (slot, target) in [&fixture.target, &second].into_iter().enumerate() {
            for group in 0..16 {
                let directory = target
                    .join("debug/incremental")
                    .join(format!("{}-{group:02}", "h".repeat(180)));
                std::fs::create_dir_all(&directory).unwrap();
                for file in 0..32 {
                    let path = directory.join(file.to_string());
                    std::fs::write(&path, "artifact").unwrap();
                    std::fs::hard_link(path, external.join(format!("{slot}-{group}-{file}")))
                        .unwrap();
                }
            }
        }
        fixture.cache.max_text_bytes = 32 * 1024;
        let inventory = fixture.inventory();
        assert!(
            inventory.complete,
            "compact hardlink identities fit alongside the completed first-slot catalog"
        );
        let item = fixture.item(
            &inventory,
            &format!("debug/incremental/{}-00", "h".repeat(180)),
        );
        let preview = fixture.preview(&inventory, vec![item.id]);
        assert!(matches!(
            fixture.execute(preview).0,
            Response::Receipt {
                result: Some(Outcome::Completed { .. }),
                ..
            }
        ));
        assert_eq!(std::fs::read_dir(&external).unwrap().count(), 1024);
        assert!(fixture.cache.budget_bytes() <= fixture.cache.max_text_bytes);
        assert!(fixture.scheduler.cache_slots().all(|(_, busy)| !busy));
    }

    #[test]
    fn unresolved_hardlink_entry_limit_is_explicit_without_hiding_selectable_scopes() {
        let mut fixture = Fixture::new();
        let external = fixture.home.0.join("external-links");
        std::fs::create_dir_all(&external).unwrap();
        for group in 0..10 {
            let directory = fixture
                .target
                .join(format!("debug/incremental/hardlinks-{group:02}"));
            std::fs::create_dir_all(&directory).unwrap();
            for file in 0..4 {
                let path = directory.join(file.to_string());
                std::fs::write(&path, "artifact").unwrap();
                std::fs::hard_link(path, external.join(format!("{group}-{file}"))).unwrap();
            }
        }
        let (Response::Inventory(mut inventory), _) = fixture.operate(Operation::Inventory {
            owner: fixture.cache.owner.clone(),
            slots: fixture.slots(),
        }) else {
            panic!("inventory")
        };
        let Stage::Inventory(scan) = &mut fixture.cache.work.as_mut().unwrap().stage else {
            panic!("scan")
        };
        scan.max_entries = 16;
        loop {
            fixture.drain(2);
            let (Response::Inventory(next), _) = fixture.operate(Operation::InventoryPage {
                owner: inventory.owner.clone(),
                inventory: inventory.revision.clone(),
                after: inventory.next.clone().unwrap(),
            }) else {
                panic!("progress")
            };
            inventory = next;
            if fixture.cache.inventories[&inventory.revision].ready {
                break;
            }
        }
        assert!(!inventory.complete);
        assert!(
            inventory.slots.iter().all(|slot| slot.incomplete.is_none()),
            "accounting limits name individual scopes"
        );
        assert!(inventory.items.iter().any(
            |item| item.incomplete == Some(Refusal::Limit) && item.reclaimable_bytes.is_none()
        ));
        let item = fixture.cache.inventories[&inventory.revision].scopes.iter().find(|item| matches!(&item.scope, Scope::Incremental(path) if path.to_string_lossy().contains("hardlinks-")) && item.item.incomplete.is_none()).unwrap().item.clone();
        let preview = fixture.preview(&inventory, vec![item.id]);
        assert!(matches!(
            fixture.execute(preview).0,
            Response::Receipt {
                result: Some(Outcome::Completed { .. }),
                ..
            }
        ));
        assert_eq!(std::fs::read_dir(&external).unwrap().count(), 40);
    }

    #[test]
    fn selected_manifest_transfer_preserves_global_budget_through_exact_receipt() {
        let mut fixture = Fixture::new();
        let root = fixture.target.join("debug/incremental/a");
        for index in 0..40 {
            std::fs::write(
                root.join(format!("{}-{index}", "p".repeat(140))),
                "artifact",
            )
            .unwrap();
        }
        fixture.cache.max_text_bytes = 16 * 1024;
        let inventory = fixture.inventory();
        let item = fixture.item(&inventory, "debug/incremental/a");
        let preview = fixture.preview(&inventory, vec![item.id]);
        let manifest = &fixture.cache.receipts[&preview.token].snapshots;
        assert!(
            manifest
                .values()
                .all(|snapshot| snapshot.entries.len() == 42)
        );
        let (response, _) = fixture.operate(Operation::Execute {
            preview: preview.clone(),
        });
        assert!(matches!(
            response,
            Response::Receipt {
                executing: true,
                ..
            }
        ));
        assert!(
            fixture.cache.receipts[&preview.token].snapshots.is_empty(),
            "execute transfers rather than clones approval manifests"
        );
        while let Some(work) = fixture.cache.take_work() {
            assert!(
                fixture.cache.budget_bytes()
                    + kind_text_bytes(&work.kind)
                    + stage_text_bytes(&work.stage)
                    <= fixture.cache.max_text_bytes
            );
            let (work, result) = work.run_bounded(2, Duration::from_millis(50));
            fixture
                .cache
                .completed(&fixture.home.0, &mut fixture.scheduler, work, result);
        }
        assert!(matches!(
            fixture.operate(Operation::Receipt { preview }).0,
            Response::Receipt {
                result: Some(Outcome::Completed { .. }),
                ..
            }
        ));
        assert!(!root.exists());
        assert!(fixture.cache.budget_bytes() <= fixture.cache.max_text_bytes);
    }

    #[test]
    fn unsupported_artifact_group_is_visible_without_invented_size_or_approval() {
        let mut fixture = Fixture::new();
        let fifo = fixture.target.join("debug/incremental/a/fifo");
        let name = std::ffi::CString::new(fifo.as_os_str().as_encoded_bytes()).unwrap();
        // SAFETY: the NUL-terminated fixture path is valid for this call.
        assert_eq!(unsafe { libc::mkfifo(name.as_ptr(), 0o600) }, 0);
        let inventory = fixture.inventory();
        assert!(!inventory.complete);
        let item = fixture.item(&inventory, "debug/incremental/a");
        assert_eq!(item.incomplete, Some(Refusal::Protected));
        assert_eq!(item.reclaimable_bytes, None);
        assert_eq!(
            fixture
                .operate(Operation::Preview {
                    owner: inventory.owner,
                    inventory: inventory.revision,
                    items: vec![item.id]
                })
                .0,
            Response::Refused {
                reason: Refusal::Protected
            }
        );
        assert!(fixture.target.join("debug/incremental/a/file").exists());
        assert!(
            inventory
                .items
                .iter()
                .any(|item| item.unit.is_some() && item.reclaimable_bytes.is_some())
        );
    }

    #[test]
    fn root_pages_keep_identity_and_enforce_cross_page_selection_and_expiry() {
        let mut fixture = Fixture::new();
        for number in 0..129 {
            std::fs::write(
                fixture
                    .target
                    .join(format!("debug/deps/libpackage{number}-{number:016x}.rlib")),
                [number as u8; 32],
            )
            .unwrap();
        }
        let first = fixture.inventory();
        assert!(first.complete);
        assert_eq!(first.items.len(), MAX_INVENTORY_PAGE_ITEMS);
        let all = fixture.all_items(&first);
        assert_eq!(all.len(), 132);
        assert_eq!(
            fixture
                .operate(Operation::Preview {
                    owner: first.owner.clone(),
                    inventory: first.revision.clone(),
                    items: all
                        .iter()
                        .take(MAX_ITEMS + 1)
                        .map(|item| item.id.clone())
                        .collect()
                })
                .0,
            Response::Refused {
                reason: Refusal::Limit
            }
        );
        let chosen = [&all[10], &all[100]];
        let preview = fixture.preview(&first, chosen.iter().map(|item| item.id.clone()).collect());
        assert_eq!(
            preview.items,
            chosen.into_iter().cloned().collect::<Vec<_>>()
        );
        assert!(matches!(
            fixture.execute(preview).0,
            Response::Receipt {
                result: Some(Outcome::Completed { .. }),
                ..
            }
        ));
        // Mutating the service invalidates the inventory; a separate fresh
        // inventory proves malformed and expired cursors without that mutation.
        let fresh = fixture.inventory();
        assert_eq!(
            fixture
                .operate(Operation::InventoryPage {
                    owner: fresh.owner.clone(),
                    inventory: fresh.revision.clone(),
                    after: fresh.items[0].id.clone()
                })
                .0,
            Response::Refused {
                reason: Refusal::PayloadChanged
            }
        );
        fixture
            .cache
            .inventories
            .get_mut(&fresh.revision)
            .unwrap()
            .expires = Instant::now();
        assert_eq!(
            fixture
                .operate(Operation::InventoryPage {
                    owner: fresh.owner,
                    inventory: fresh.revision,
                    after: "expired".into()
                })
                .0,
            Response::Refused {
                reason: Refusal::Expired
            }
        );
    }

    mod torture {
        use super::*;
        #[test]
        fn large_cargo_target_has_resumable_inventory_and_small_selected_cleanup() {
            let mut fixture = Fixture::new();
            for unit in 0..500 {
                for variant in 0..20 {
                    std::fs::write(
                        fixture.target.join(format!(
                            "debug/deps/libpackage{unit}-{unit:016x}.variant{variant}.rlib"
                        )),
                        [unit as u8; 32],
                    )
                    .unwrap();
                }
            }
            let unrelated = fixture.target.join("tmp/unrelated");
            std::fs::create_dir_all(&unrelated).unwrap();
            std::os::unix::fs::symlink(&fixture.source, unrelated.join("tools")).unwrap();
            let first = fixture.inventory();
            assert!(first.complete);
            let all = fixture.all_items(&first);
            assert_eq!(all.len(), 503);
            let selected = all
                .iter()
                .find(|item| item.unit.as_deref() == Some("debug/0000000000000001"))
                .unwrap();
            let preview = fixture.preview(&first, vec![selected.id.clone()]);
            assert!(matches!(
                fixture.execute(preview).0,
                Response::Receipt {
                    result: Some(Outcome::Completed { .. }),
                    ..
                }
            ));
            for variant in 0..20 {
                assert!(
                    !fixture
                        .target
                        .join(format!(
                            "debug/deps/libpackage1-0000000000000001.variant{variant}.rlib"
                        ))
                        .exists()
                );
            }
            assert!(
                fixture
                    .target
                    .join("debug/deps/libpackage499-00000000000001f3.variant0.rlib")
                    .exists()
            );
            assert!(fixture.source.join("keep").exists());
        }
    }
}
