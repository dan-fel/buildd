//! Bounded, service-issued cache selection and approval receipts.
//!
//! This module never accepts filesystem paths. The scheduler owns artifact
//! exclusion; receipts own approvals, not slots. A disconnected executor keeps
//! running and its exact receipt can be queried until expiry. A new daemon has
//! no knowledge of an old daemon's outcomes and refuses its receipts.

use std::collections::{BTreeMap, HashMap};
use std::io::Read as _;
use std::os::unix::fs::MetadataExt as _;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

use crate::scheduler::{Distance, Effect, Scheduler};
use crate::slot::{self, SlotDirectory};

const MAX_ENTRIES: usize = 8192;
const MAX_PATH_BYTES: usize = 512 * 1024;
const MAX_ITEMS: usize = 64;
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
    Inventory {
        owner: Owner,
    },
    Preview {
        owner: Owner,
        inventory: String,
        items: Vec<String>,
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
    pub reclaimable_bytes: u64,
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
    Inventory(Inventory),
    Preview(Preview),
    Receipt {
        preview: Preview,
        result: Option<Outcome>,
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

type Snapshot = BTreeMap<PathBuf, std::fs::Metadata>;

fn same_snapshot(a: &Snapshot, b: &Snapshot) -> bool {
    a.len() == b.len()
        && a.iter().zip(b).all(|((path, metadata), (other, current))| {
            path == other && fingerprint(metadata) == fingerprint(current)
        })
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

/// Scan before calling the existing eviction classifier and hardlink accounting.
/// An incomplete scan is never eligible for cleanup. Symlinks are protected:
/// the existing classifier must not discover a profile outside the target.
fn snapshot(root: &Path, deadline: Instant, remaining: &mut usize) -> Result<Snapshot, Refusal> {
    let mut found = BTreeMap::new();
    let mut pending = vec![root.to_owned()];
    let mut path_bytes = root.as_os_str().len();
    while let Some(path) = pending.pop() {
        if Instant::now() >= deadline || *remaining == 0 {
            return Err(Refusal::Limit);
        }
        *remaining -= 1;
        if path_bytes > MAX_PATH_BYTES {
            return Err(Refusal::Limit);
        }
        let metadata = std::fs::symlink_metadata(&path).map_err(|_| Refusal::Io)?;
        if metadata.file_type().is_symlink() || !(metadata.is_dir() || metadata.is_file()) {
            return Err(Refusal::Protected);
        }
        if metadata.is_dir() {
            let entries = std::fs::read_dir(&path).map_err(|_| Refusal::Io)?;
            for entry in entries {
                if pending.len() + found.len() >= MAX_ENTRIES || Instant::now() >= deadline {
                    return Err(Refusal::Limit);
                }
                let child = entry.map_err(|_| Refusal::Io)?.path();
                path_bytes += child.as_os_str().len();
                if path_bytes > MAX_PATH_BYTES {
                    return Err(Refusal::Limit);
                }
                pending.push(child);
            }
        }
        found.insert(path, metadata);
    }
    Ok(found)
}

struct Measured {
    expires: Instant,
    wire: Inventory,
    snapshots: HashMap<String, Snapshot>,
    paths: HashMap<String, Vec<PathBuf>>,
}

struct Receipt {
    expires: Instant,
    preview: Preview,
    result: Option<Outcome>,
}

/// Incarnation-scoped bounded approval state, exclusively owned by the daemon.
pub(crate) struct Cache {
    owner: Owner,
    sequence: u64,
    inventories: HashMap<String, Measured>,
    receipts: HashMap<String, Receipt>,
}

impl Cache {
    pub(crate) fn new(home: &Path) -> Result<Self, String> {
        let mut random = [0_u8; 16];
        std::fs::File::open("/dev/urandom")
            .and_then(|mut file| file.read_exact(&mut random))
            .map_err(|error| format!("could not identify daemon incarnation: {error}"))?;
        let mut hostname = [0_u8; 256];
        // SAFETY: a writable buffer and its exact capacity are supplied.
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
            receipts: HashMap::new(),
        })
    }

    fn id(&mut self) -> String {
        self.sequence = self
            .sequence
            .checked_add(1)
            .expect("cache identity exhausted");
        format!("{}:{}", self.owner.incarnation, self.sequence)
    }

    pub(crate) fn operate<D: Distance>(
        &mut self,
        home: &Path,
        scheduler: &mut Scheduler<D>,
        operation: Operation,
    ) -> (Response, Vec<Effect>) {
        let now = Instant::now();
        self.inventories
            .retain(|_, inventory| inventory.expires > now);
        self.receipts.retain(|_, receipt| receipt.expires > now);
        let mut effects = Vec::new();
        let receipt_operation = matches!(
            operation,
            Operation::Execute { .. } | Operation::Receipt { .. } | Operation::Cancel { .. }
        );
        let result = self.perform(home, scheduler, operation, &mut effects);
        (
            result.unwrap_or_else(|reason| {
                if receipt_operation
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
                protocol: 1,
                max_slots: MAX_SLOTS,
                max_items: MAX_ITEMS,
                max_entries: MAX_ENTRIES,
                max_path_bytes: MAX_PATH_BYTES,
                scan_ms: SCAN_TIME.as_millis() as u64,
                receipt_ttl_ms: TTL.as_millis() as u64,
                max_receipts: MAX_RECEIPTS,
            })),
            Operation::Inventory { owner } => {
                self.check_owner(&owner)?;
                self.inventory(home, scheduler, effects)
            }
            Operation::Preview {
                owner,
                inventory,
                items,
            } => {
                self.check_owner(&owner)?;
                if items.is_empty()
                    || items.len() > MAX_ITEMS
                    || self.receipts.len() >= MAX_RECEIPTS
                {
                    return Err(Refusal::Limit);
                }
                let measured = self.inventories.get(&inventory).ok_or(Refusal::Expired)?;
                if milliseconds() >= measured.wire.expires_ms {
                    return Err(Refusal::Expired);
                }
                let mut selected = Vec::new();
                for id in items {
                    if selected.iter().any(|item: &Item| item.id == id) {
                        return Err(Refusal::PayloadChanged);
                    }
                    let item = measured
                        .wire
                        .items
                        .iter()
                        .find(|item| item.id == id)
                        .ok_or(Refusal::PayloadChanged)?;
                    selected.push(item.clone());
                }
                // Preview has no filesystem effects and retains no scheduler slot.
                let expires = measured.expires;
                let expires_ms = measured.wire.expires_ms;
                let preview = Preview {
                    owner,
                    token: self.id(),
                    inventory,
                    expires_ms,
                    items: selected,
                };
                self.receipts.insert(
                    preview.token.clone(),
                    Receipt {
                        expires,
                        preview: preview.clone(),
                        result: None,
                    },
                );
                Ok(Response::Preview(preview))
            }
            Operation::Execute { preview } => {
                self.validate_receipt(&preview)?;
                if self.receipts[&preview.token].result.is_none() {
                    self.execute(home, scheduler, &preview, effects)?;
                }
                Ok(self.receipt(preview))
            }
            Operation::Receipt { preview } => {
                self.validate_receipt(&preview)?;
                Ok(self.receipt(preview))
            }
            Operation::Cancel { preview } => {
                self.validate_receipt(&preview)?;
                let receipt = self.receipts.get_mut(&preview.token).expect("validated");
                if receipt.result.is_none() {
                    receipt.result = Some(Outcome::Cancelled);
                }
                Ok(self.receipt(preview))
            }
        }
    }

    fn check_owner(&self, owner: &Owner) -> Result<(), Refusal> {
        if *owner == self.owner {
            Ok(())
        } else {
            Err(Refusal::ForeignOwner)
        }
    }

    fn validate_receipt(&self, preview: &Preview) -> Result<(), Refusal> {
        self.check_owner(&preview.owner)?;
        let receipt = self
            .receipts
            .get(&preview.token)
            .ok_or(Refusal::UnknownReceipt)?;
        if receipt.preview != *preview {
            return Err(Refusal::PayloadChanged);
        }
        if preview.expires_ms <= milliseconds() || receipt.expires <= Instant::now() {
            return Err(Refusal::Expired);
        }
        Ok(())
    }

    fn receipt(&self, preview: Preview) -> Response {
        let result = self.receipts[&preview.token].result.clone();
        Response::Receipt { preview, result }
    }

    fn inventory<D: Distance>(
        &mut self,
        home: &Path,
        scheduler: &mut Scheduler<D>,
        effects: &mut Vec<Effect>,
    ) -> Result<Response, Refusal> {
        if self.inventories.len() >= MAX_RECEIPTS {
            return Err(Refusal::Limit);
        }
        let measured_ms = milliseconds();
        let revision = self.id();
        let mut wire = Inventory {
            owner: self.owner.clone(),
            revision: revision.clone(),
            measured_ms,
            expires_ms: measured_ms + TTL.as_millis() as u64,
            complete: true,
            slots: Vec::new(),
            items: Vec::new(),
        };
        let mut snapshots = HashMap::new();
        let mut paths = HashMap::new();
        let deadline = Instant::now() + SCAN_TIME;
        let mut remaining = MAX_ENTRIES;
        let slots = scheduler
            .cache_slots()
            .take(MAX_SLOTS + 1)
            .collect::<Vec<_>>();
        if slots.len() > MAX_SLOTS {
            wire.complete = false;
        }
        for (name, busy) in slots.into_iter().take(MAX_SLOTS) {
            let mut record = Slot {
                id: name.clone(),
                protected: busy,
                incomplete: None,
            };
            if busy {
                wire.complete = false;
                wire.slots.push(record);
                continue;
            }
            let idle = scheduler
                .cache_acquire(&name)
                .expect("scheduler thread owns slot transitions");
            let target = SlotDirectory::new(home, &idle.repository, idle.slot).target();
            let measured = snapshot(&target, deadline, &mut remaining).and_then(|before| {
                let items = slot::evictables_measured(&target, &before, &idle.used);
                if wire.items.len() + items.len() > MAX_ITEMS {
                    return Err(Refusal::Limit);
                }
                let sizes = slot::disk_usage_measured(&before, &items);
                let after = snapshot(&target, deadline, &mut remaining)?;
                if !same_snapshot(&before, &after) {
                    return Err(Refusal::Changed);
                }
                Ok((before, items, sizes.freeable))
            });
            effects.extend(scheduler.cache_release(idle.key, false));
            match measured {
                Ok((snapshot, items, sizes)) => {
                    for (item, bytes) in items.into_iter().zip(sizes) {
                        let record = Item {
                            id: self.id(),
                            slot: name.clone(),
                            unit: item.unit,
                            reclaimable_bytes: bytes,
                        };
                        wire.items.push(record.clone());
                        paths.insert(record.id, item.paths);
                    }
                    snapshots.insert(name, snapshot);
                }
                Err(reason) => {
                    record.protected = reason == Refusal::Protected;
                    record.incomplete = Some(reason);
                    wire.complete = false;
                }
            }
            wire.slots.push(record);
        }
        self.inventories.insert(
            revision,
            Measured {
                expires: Instant::now() + TTL,
                wire: wire.clone(),
                snapshots,
                paths,
            },
        );
        Ok(Response::Inventory(wire))
    }

    fn execute<D: Distance>(
        &mut self,
        home: &Path,
        scheduler: &mut Scheduler<D>,
        preview: &Preview,
        effects: &mut Vec<Effect>,
    ) -> Result<(), Refusal> {
        let measured = self
            .inventories
            .get(&preview.inventory)
            .ok_or(Refusal::Expired)?;
        let names = preview
            .items
            .iter()
            .map(|item| item.slot.clone())
            .collect::<std::collections::BTreeSet<_>>();
        let mut acquired = Vec::new();
        // Acquire ALL slots before revalidation or deletion. Failure rolls back
        // exclusion without deleting anything, never falls through to pruning.
        let validation = (|| {
            for name in names {
                let idle = scheduler.cache_acquire(&name).ok_or(Refusal::Busy)?;
                let target = SlotDirectory::new(home, &idle.repository, idle.slot).target();
                acquired.push((name, idle, target));
            }
            let mut remaining = MAX_ENTRIES;
            let deadline = Instant::now() + SCAN_TIME;
            for (name, _, target) in &acquired {
                let current = snapshot(target, deadline, &mut remaining)?;
                if !same_snapshot(&measured.snapshots[name], &current) {
                    return Err(Refusal::Changed);
                }
            }
            if milliseconds() >= preview.expires_ms
                || Instant::now() >= self.receipts[&preview.token].expires
            {
                return Err(Refusal::Expired);
            }
            Ok(())
        })();
        if let Err(reason) = validation {
            for (_, idle, _) in acquired {
                effects.extend(scheduler.cache_release(idle.key, false));
            }
            return Err(reason);
        }
        // Persist invalidation before deletion: a daemon crash must not restore
        // successful-test or compilation claims for missing artifacts.
        let invalidated = acquired.iter().try_for_each(|(_, idle, _)| {
            let directory = SlotDirectory::new(home, &idle.repository, idle.slot);
            let mut record = directory.read_record().map_err(|_| Refusal::Io)?;
            record.units.clear();
            record.passed.clear();
            record.compilations.clear();
            directory.write_record(&record).map_err(|_| Refusal::Io)
        });
        if let Err(reason) = invalidated {
            for (_, idle, _) in acquired {
                effects.extend(scheduler.cache_release(idle.key, false));
            }
            return Err(reason);
        }
        // Record writes are external I/O too; approval may have expired while
        // they completed. Keep the conservative invalidation, but delete nothing.
        if milliseconds() >= preview.expires_ms
            || Instant::now() >= self.receipts[&preview.token].expires
        {
            for (_, idle, _) in acquired {
                effects.extend(scheduler.cache_release(idle.key, true));
            }
            return Err(Refusal::Expired);
        }
        let outcome = remove_selected(measured, &preview.items, Instant::now() + SCAN_TIME);
        for (_, idle, _) in acquired {
            effects.extend(scheduler.cache_release(idle.key, true));
        }
        self.receipts
            .get_mut(&preview.token)
            .expect("validated receipt")
            .result = Some(outcome);
        Ok(())
    }
}

fn remove_selected(measured: &Measured, items: &[Item], deadline: Instant) -> Outcome {
    let mut removed = Vec::new();
    for item in items {
        let roots = &measured.paths[&item.id];
        let snapshot = &measured.snapshots[&item.slot];
        let mut paths = snapshot
            .iter()
            .filter(|(path, _)| roots.iter().any(|root| path.starts_with(root)))
            .collect::<Vec<_>>();
        paths.sort_by_key(|(path, _)| std::cmp::Reverse(path.components().count()));
        for (path, metadata) in paths {
            let failure = if Instant::now() >= deadline {
                Some(Refusal::Limit)
            } else {
                let result = if metadata.is_dir() {
                    std::fs::remove_dir(path)
                } else {
                    std::fs::remove_file(path)
                };
                result.err().map(|_| Refusal::Io)
            };
            if let Some(reason) = failure {
                return Outcome::Partial {
                    removed,
                    failed: item.id.clone(),
                    reason,
                };
            }
        }
        removed.push(item.id.clone());
    }
    Outcome::Completed { removed }
}

fn milliseconds() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("clock before epoch")
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

        fn inventory(&mut self) -> Inventory {
            let (Response::Inventory(inventory), _) = self.operate(Operation::Inventory {
                owner: self.cache.owner.clone(),
            }) else {
                panic!("inventory");
            };
            assert!(inventory.complete, "{inventory:?}");
            inventory
        }

        fn preview(&mut self, inventory: &Inventory, items: Vec<String>) -> Preview {
            let (Response::Preview(preview), effects) = self.operate(Operation::Preview {
                owner: inventory.owner.clone(),
                inventory: inventory.revision.clone(),
                items,
            }) else {
                panic!("preview");
            };
            assert!(effects.is_empty());
            preview
        }
    }

    #[test]
    fn preview_is_inert_execute_removes_only_selected_and_invalidates_claims() {
        let mut fixture = Fixture::new();
        let inventory = fixture.inventory();
        assert_eq!(inventory.items.len(), 3);
        let unit = inventory
            .items
            .iter()
            .find(|item| item.unit.is_some())
            .unwrap();
        let preview = fixture.preview(&inventory, vec![unit.id.clone()]);
        assert!(
            fixture
                .target
                .join("debug/deps/pkg-aaaaaaaaaaaaaaaa")
                .exists()
        );
        assert!(fixture.scheduler.cache_slots().all(|(_, busy)| !busy));
        let (response, effects) = fixture.operate(Operation::Execute {
            preview: preview.clone(),
        });
        assert!(
            matches!(response, Response::Receipt { result: Some(Outcome::Completed { ref removed }), .. } if removed == std::slice::from_ref(&unit.id))
        );
        assert!(effects.iter().any(|effect| matches!(effect, Effect::Persist { record, .. }
            if record.units.is_empty() && record.compilations.is_empty() && record.passed.is_empty())));
        assert!(
            !fixture
                .target
                .join("debug/deps/pkg-aaaaaaaaaaaaaaaa")
                .exists()
        );
        assert!(
            !fixture
                .target
                .join("debug/.fingerprint/pkg-aaaaaaaaaaaaaaaa")
                .exists()
        );
        for path in [
            "debug/incremental/a/file",
            "debug/incremental/b/file",
            "debug/product",
        ] {
            assert!(fixture.target.join(path).exists());
        }
        assert!(fixture.source.join("keep").exists());
        assert!(fixture.home.0.join("keep.log").exists());
        assert_eq!(
            fixture
                .operate(Operation::Execute {
                    preview: preview.clone()
                })
                .0,
            response
        );
        assert_eq!(fixture.operate(Operation::Receipt { preview }).0, response);
    }

    #[test]
    fn changed_busy_foreign_and_mutated_payloads_delete_nothing() {
        let mut fixture = Fixture::new();
        let inventory = fixture.inventory();
        let preview = fixture.preview(&inventory, vec![inventory.items[0].id.clone()]);
        let idle = fixture
            .scheduler
            .cache_acquire(&inventory.slots[0].id)
            .unwrap();
        assert_eq!(
            fixture
                .operate(Operation::Execute {
                    preview: preview.clone()
                })
                .0,
            Response::Refused {
                reason: Refusal::Busy
            }
        );
        fixture.scheduler.cache_release(idle.key, false);
        let mut foreign = preview.clone();
        foreign.owner.host = "foreign".into();
        assert_eq!(
            fixture.operate(Operation::Execute { preview: foreign }).0,
            Response::Unknown {
                reason: Refusal::ForeignOwner
            }
        );
        let mut changed = preview.clone();
        changed.items[0].reclaimable_bytes += 1;
        assert_eq!(
            fixture.operate(Operation::Execute { preview: changed }).0,
            Response::Refused {
                reason: Refusal::PayloadChanged
            }
        );
        std::fs::write(fixture.target.join("debug/incremental/a/file"), "changed").unwrap();
        assert_eq!(
            fixture.operate(Operation::Execute { preview }).0,
            Response::Refused {
                reason: Refusal::Changed
            }
        );
        assert!(fixture.target.join("debug/incremental/b/file").exists());
        assert!(fixture.scheduler.cache_slots().all(|(_, busy)| !busy));
    }

    #[test]
    fn receipts_expire_restart_is_unknown_and_cancel_is_terminal() {
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
        assert_eq!(
            fixture
                .operate(Operation::Execute {
                    preview: preview.clone()
                })
                .0,
            cancelled
        );
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
            fixture.operate(Operation::Execute { preview }).0,
            Response::Unknown {
                reason: Refusal::ForeignOwner
            }
        );
        assert!(fixture.target.join("debug/incremental/a/file").exists());
    }

    #[test]
    fn snapshot_limits_and_symlinks_produce_partial_inventory_not_targets() {
        let mut fixture = Fixture::new();
        let mut remaining = 2;
        assert_eq!(
            snapshot(&fixture.target, Instant::now() + SCAN_TIME, &mut remaining).unwrap_err(),
            Refusal::Limit
        );
        let mut remaining = MAX_ENTRIES;
        assert_eq!(
            snapshot(&fixture.target, Instant::now(), &mut remaining).unwrap_err(),
            Refusal::Limit
        );
        std::os::unix::fs::symlink(&fixture.source, fixture.target.join("debug/foreign")).unwrap();
        let (Response::Inventory(inventory), _) = fixture.operate(Operation::Inventory {
            owner: fixture.cache.owner.clone(),
        }) else {
            panic!("inventory");
        };
        assert!(!inventory.complete);
        assert!(inventory.items.is_empty());
        assert_eq!(inventory.slots[0].incomplete, Some(Refusal::Protected));
        assert!(fixture.source.join("keep").exists());
    }

    #[test]
    fn hardlinks_and_measured_classification_match_existing_accounting() {
        let mut fixture = Fixture::new();
        let linked = fixture.target.join("debug/incremental/a/file");
        std::fs::hard_link(&linked, fixture.home.0.join("external-link")).unwrap();
        let inventory = fixture.inventory();
        let measured = &fixture.cache.inventories[&inventory.revision];
        let original = slot::evictables(&fixture.target, &HashMap::new()).unwrap();
        let sizes = slot::disk_usage(&fixture.target, &original).unwrap();
        for (item, size) in original.iter().zip(sizes.freeable) {
            let (id, _) = measured
                .paths
                .iter()
                .find(|(_, paths)| **paths == item.paths)
                .unwrap();
            assert_eq!(
                inventory
                    .items
                    .iter()
                    .find(|item| item.id == *id)
                    .unwrap()
                    .reclaimable_bytes,
                size
            );
        }
        let (id, _) = measured
            .paths
            .iter()
            .find(|(_, paths)| paths.iter().any(|path| linked.starts_with(path)))
            .unwrap();
        let linked_item = inventory.items.iter().find(|item| item.id == *id).unwrap();
        assert_eq!(
            linked_item.reclaimable_bytes,
            std::fs::metadata(linked.parent().unwrap())
                .unwrap()
                .blocks()
                * 512
        );
        let preview = fixture.preview(&inventory, vec![linked_item.id.clone()]);
        fixture.operate(Operation::Execute { preview });
        assert!(fixture.home.0.join("external-link").exists());
    }

    #[test]
    fn partial_removal_reports_completed_items_and_never_prunes_remainder() {
        let mut fixture = Fixture::new();
        let inventory = fixture.inventory();
        let measured = &fixture.cache.inventories[&inventory.revision];
        let second = &measured.paths[&inventory.items[1].id];
        // A real fixture I/O failure at the removal boundary, after a separate
        // successful validation. Execute's changed-target rejection is covered
        // above; this directly exercises the partial filesystem outcome.
        let missing = second[0].join("file");
        std::fs::remove_file(&missing).unwrap();
        let outcome = remove_selected(measured, &inventory.items, Instant::now() + SCAN_TIME);
        assert_eq!(
            outcome,
            Outcome::Partial {
                removed: vec![inventory.items[0].id.clone()],
                failed: inventory.items[1].id.clone(),
                reason: Refusal::Io,
            }
        );
        assert!(
            fixture
                .target
                .join("debug/deps/pkg-aaaaaaaaaaaaaaaa")
                .exists()
        );
        assert!(fixture.target.join("debug/product").exists());
    }

    #[test]
    fn maintenance_exclusion_defers_builds_and_marks_inventory_protected() {
        let mut fixture = Fixture::new();
        let inventory = fixture.inventory();
        let idle = fixture
            .scheduler
            .cache_acquire(&inventory.slots[0].id)
            .unwrap();
        let (Response::Inventory(protected), _) = fixture.operate(Operation::Inventory {
            owner: fixture.cache.owner.clone(),
        }) else {
            panic!("inventory");
        };
        assert!(protected.items.is_empty());
        assert!(protected.slots[0].protected);
        let effects = fixture.scheduler.submit(crate::scheduler::Submission {
            waiter: crate::scheduler::WaiterId(1),
            source: crate::snapshot::Source {
                repository: idle.repository.clone(),
                worktree: fixture.source.clone(),
                prefix: PathBuf::new(),
                index: PathBuf::new(),
            },
            revision: Revision::of_tree("1234"),
            operation: crate::cargo::Operation {
                command: crate::cargo::Command::Check,
                args: Vec::new(),
                rustflags: Vec::new(),
            },
            label: None,
            copy_to: None,
            rerun_all: false,
            optional: false,
        });
        assert!(
            !effects
                .iter()
                .any(|effect| matches!(effect, Effect::Start(_)))
        );
        let ready = fixture.scheduler.cache_release(idle.key, false);
        assert!(
            ready
                .iter()
                .any(|effect| matches!(effect, Effect::Start(_)))
        );
    }
}
