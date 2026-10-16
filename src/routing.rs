//! Host policy and the lifetime of in-flight placements. Resource admission
//! stays with each host's scheduler; observations only rank destinations.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Weak};

use crate::cargo::Operation;
use crate::config::Remote;
use crate::protocol::{BuildRequest, Capacity, Host};
use crate::snapshot::{Revision, Source};

pub(crate) struct Router {
    disabled: BTreeSet<String>,
    placements: Vec<Weak<Placement>>,
}

/// Held until forwarding or local submission ends. Equal requests share its
/// destination even before the receiving scheduler can report the build.
#[derive(Debug)]
pub(crate) struct Placement {
    pub(crate) host: String,
    source: Source,
    revision: Revision,
    operation: Operation,
    copy_to: Option<PathBuf>,
    rerun_all: bool,
}

impl Router {
    pub(crate) fn load(home: &Path) -> Result<Self, String> {
        let path = home.join("host-policy.json");
        let disabled = match std::fs::read(&path) {
            Ok(bytes) => serde_json::from_slice(&bytes)
                .map_err(|error| format!("{}: {error}", path.display()))?,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => BTreeSet::new(),
            Err(error) => return Err(format!("{}: {error}", path.display())),
        };
        Ok(Self {
            disabled,
            placements: Vec::new(),
        })
    }

    pub(crate) fn hosts(&self, remotes: &[Remote]) -> Vec<Host> {
        std::iter::once(("local", std::env::consts::OS))
            .chain(
                remotes
                    .iter()
                    .map(|remote| (remote.name.as_str(), remote.os.as_str())),
            )
            .map(|(name, os)| Host {
                name: name.into(),
                os: os.into(),
                enabled: !self.disabled.contains(name),
            })
            .collect()
    }

    pub(crate) fn set_enabled(
        &mut self,
        home: &Path,
        remotes: &[Remote],
        name: &str,
        enabled: bool,
    ) -> Result<Vec<Host>, String> {
        if !self.hosts(remotes).iter().any(|host| host.name == name) {
            return Err(format!("unknown build host {name}"));
        }
        let mut disabled = self.disabled.clone();
        if enabled {
            disabled.remove(name);
        } else {
            disabled.insert(name.to_owned());
        }
        let bytes = serde_json::to_vec(&disabled).expect("host names serialize");
        let staged = home.join("host-policy.json.new");
        std::fs::write(&staged, bytes)
            .and_then(|()| std::fs::rename(&staged, home.join("host-policy.json")))
            .map_err(|error| format!("could not save host policy: {error}"))?;
        self.disabled = disabled;
        Ok(self.hosts(remotes))
    }

    pub(crate) fn enabled(&self, name: &str) -> bool {
        !self.disabled.contains(name)
    }

    pub(crate) fn place(
        &mut self,
        source: &Source,
        revision: &Revision,
        request: &BuildRequest,
        observations: &[(Host, Result<Capacity, String>)],
    ) -> Result<Arc<Placement>, String> {
        let constrained = |host: &Host| request.accepts_host(host) && self.enabled(&host.name);
        let eligible = observations
            .iter()
            .filter(|(host, _)| constrained(host))
            .collect::<Vec<_>>();
        let peak = eligible
            .iter()
            .filter_map(|(_, capacity)| capacity.as_ref().ok())
            .map(|capacity| capacity.peak_memory)
            .max()
            .unwrap_or(0);
        let live = self
            .placements
            .iter()
            .filter_map(Weak::upgrade)
            .collect::<Vec<_>>();
        let usable = |host: &Host, capacity: &Capacity| {
            constrained(host)
                && !capacity.status.draining
                && peak <= capacity.status.memory
                && capacity
                    .status
                    .free_disk
                    .is_some_and(|free| free >= capacity.status.min_free)
        };
        let matching = |placement: &&Arc<Placement>| {
            placement.source.repository == source.repository
                && placement.source.prefix == source.prefix
                && placement.operation == request.operation
                && placement.copy_to == request.copy_to
                && placement.rerun_all == request.rerun_all
                && (placement.revision == *revision || placement.source.worktree == source.worktree)
                && eligible.iter().any(|(host, capacity)| {
                    host.name == placement.host
                        && capacity
                            .as_ref()
                            .is_ok_and(|capacity| usable(host, capacity))
                })
        };
        let host = if let Some(placement) = live.iter().find(matching) {
            if placement.revision == *revision {
                return Ok(Arc::clone(placement));
            }
            placement.host.clone()
        } else {
            eligible
                .iter()
                .filter_map(|(host, result)| result.as_ref().ok().map(|capacity| (host, capacity)))
                .filter(|(host, capacity)| usable(host, capacity))
                .min_by_key(|(host, capacity)| {
                    let status = &capacity.status;
                    let observed = status
                        .slots
                        .iter()
                        .filter(|slot| slot.build.is_some() || slot.chore.is_some())
                        .count()
                        + status.queue.len();
                    let assigned = live
                        .iter()
                        .filter(|placement| placement.host == host.name)
                        .count();
                    let load = observed.max(assigned);
                    let starts = load < status.capacity
                        && status.idle_jobs > 0
                        && status.queue.is_empty()
                        && status.memory_in_use.saturating_add(peak) <= status.memory
                        && status.disk_wait.is_none()
                        && status.free_disk.is_some_and(|free| {
                            let running = status
                                .slots
                                .iter()
                                .filter(|slot| slot.build.is_some())
                                .count();
                            let builds =
                                u64::try_from(running + 1).expect("the slot count fits u64");
                            free >= status
                                .min_free
                                .saturating_add(status.build_headroom.saturating_mul(builds))
                        });
                    let warm = status
                        .slots
                        .iter()
                        .any(|slot| slot.worktree.as_ref() == Some(&source.worktree));
                    (
                        !starts,
                        load * 1024 / status.capacity,
                        !warm,
                        std::cmp::Reverse(status.memory),
                        host.name.clone(),
                    )
                })
                .map(|(host, _)| host.name.clone())
                .ok_or_else(|| {
                    let details = eligible
                        .iter()
                        .map(|(host, result)| match result {
                            Err(reason) => format!("{}: {reason}", host.name),
                            Ok(capacity) if capacity.status.draining => {
                                format!("{}: draining", host.name)
                            }
                            Ok(capacity) if capacity.status.free_disk.is_none() => {
                                format!("{}: free disk unavailable", host.name)
                            }
                            Ok(capacity)
                                if capacity
                                    .status
                                    .free_disk
                                    .is_some_and(|free| free < capacity.status.min_free) =>
                            {
                                format!("{}: below free disk floor", host.name)
                            }
                            Ok(capacity) => format!(
                                "{}: estimated peak {peak} bytes exceeds memory budget {}",
                                host.name, capacity.status.memory
                            ),
                        })
                        .collect::<Vec<_>>()
                        .join("; ");
                    format!(
                        "no enabled, reachable build host satisfies this request{}",
                        if details.is_empty() {
                            String::new()
                        } else {
                            format!(": {details}")
                        }
                    )
                })?
        };
        let placement = Arc::new(Placement {
            host,
            source: source.clone(),
            revision: revision.clone(),
            operation: request.operation.clone(),
            copy_to: request.copy_to.clone(),
            rerun_all: request.rerun_all,
        });
        self.placements
            .retain(|placement| placement.strong_count() > 0);
        self.placements.push(Arc::downgrade(&placement));
        Ok(placement)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cargo::Command;
    use crate::protocol::Status;
    use crate::snapshot::tests::TempDir;

    fn source(worktree: &str) -> Source {
        Source {
            repository: "/repo/.git".into(),
            worktree: worktree.into(),
            prefix: PathBuf::new(),
            index: PathBuf::new(),
        }
    }
    fn request() -> BuildRequest {
        BuildRequest {
            directory: "/a".into(),
            operation: Operation {
                command: Command::Check,
                args: Vec::new(),
                rustflags: Vec::new(),
            },
            label: None,
            copy_to: None,
            rerun_all: false,
            os: None,
            host: None,
            optional: false,
        }
    }
    fn revision(name: &str) -> Revision {
        serde_json::from_value(serde_json::json!(name)).unwrap()
    }
    fn host(name: &str, os: &str, memory: u64) -> (Host, Result<Capacity, String>) {
        (
            Host {
                name: name.into(),
                os: os.into(),
                enabled: true,
            },
            Ok(Capacity {
                peak_memory: 2,
                status: Status {
                    jobs: 2,
                    idle_jobs: 2,
                    capacity: 1,
                    memory,
                    memory_in_use: 0,
                    disk_budget: 20,
                    disk_used: 0,
                    free_disk: Some(30),
                    min_free: 15,
                    build_headroom: 0,
                    disk_wait: None,
                    undersized: false,
                    slots: Vec::new(),
                    queue: Vec::new(),
                    draining: false,
                },
            }),
        )
    }

    #[test]
    fn placement_prefers_ready_disk_capacity_and_refuses_unknown_free_space() {
        let home = TempDir::new();
        let mut router = Router::load(&home.0).unwrap();
        let mut hosts = [host("local", "linux", 12), host("pc", "linux", 12)];
        // Both hosts have the same CPU/memory load. The first cannot cover
        // the next build's output reservation, so the second can start sooner.
        hosts[0].1.as_mut().unwrap().status.build_headroom = 20;
        assert_eq!(
            router
                .place(&source("/a"), &revision("a"), &request(), &hosts)
                .unwrap()
                .host,
            "pc"
        );
        let local = &mut hosts[0].1.as_mut().unwrap().status;
        local.build_headroom = 0;
        local.disk_wait = Some(crate::protocol::DiskWait::Measuring);
        assert_eq!(
            router
                .place(&source("/a"), &revision("b"), &request(), &hosts)
                .unwrap()
                .host,
            "pc"
        );
        hosts[0].1.as_mut().unwrap().status.free_disk = None;
        assert_eq!(
            router
                .place(&source("/a"), &revision("c"), &request(), &hosts)
                .unwrap()
                .host,
            "pc"
        );
        hosts[1].1.as_mut().unwrap().status.free_disk = None;
        let error = router
            .place(&source("/a"), &revision("d"), &request(), &hosts)
            .unwrap_err();
        assert!(error.contains("free disk unavailable"), "{error}");
    }

    #[test]
    fn in_flight_placements_balance_fresh_work_and_share_equal_requests() {
        let home = TempDir::new();
        let mut router = Router::load(&home.0).unwrap();
        let hosts = [
            host("local", "macos", 12),
            host("pc", "linux", 24),
            host("air", "macos", 3),
        ];
        let a = router
            .place(&source("/a"), &revision("a"), &request(), &hosts)
            .unwrap();
        assert_eq!(a.host, "pc");
        let same = router
            .place(&source("/b"), &revision("a"), &request(), &hosts)
            .unwrap();
        assert!(Arc::ptr_eq(&a, &same));
        let b = router
            .place(&source("/b"), &revision("b"), &request(), &hosts)
            .unwrap();
        assert_eq!(b.host, "local");
        let c = router
            .place(&source("/c"), &revision("c"), &request(), &hosts)
            .unwrap();
        assert_eq!(c.host, "air");
        drop(a);
        drop(same);
        assert_eq!(
            router
                .place(&source("/d"), &revision("d"), &request(), &hosts)
                .unwrap()
                .host,
            "pc"
        );
    }

    #[test]
    fn explicit_constraints_never_relax_and_offline_or_disabled_hosts_do_not_win() {
        let home = TempDir::new();
        let mut router = Router::load(&home.0).unwrap();
        let mut hosts = [
            host("local", "macos", 12),
            host("pc", "linux", 24),
            host("air", "macos", 3),
        ];
        hosts[1].1 = Err("offline".into());
        let mut request = request();
        request.os = Some("linux".into());
        assert!(
            router
                .place(&source("/a"), &revision("a"), &request, &hosts)
                .unwrap_err()
                .contains("offline")
        );
        request.os = Some("macos".into());
        request.host = Some("pc".into());
        assert!(
            router
                .place(&source("/a"), &revision("a"), &request, &hosts)
                .is_err()
        );
        request.host = Some("air".into());
        assert_eq!(
            router
                .place(&source("/a"), &revision("a"), &request, &hosts)
                .unwrap()
                .host,
            "air"
        );
        hosts[2].0.enabled = false;
        assert!(
            router
                .place(&source("/a"), &revision("a"), &request, &hosts)
                .is_err()
        );
        request.host = None;
        request.os = None;
        assert_eq!(
            router
                .place(&source("/a"), &revision("a"), &request, &hosts)
                .unwrap()
                .host,
            "local"
        );
    }

    #[test]
    fn known_large_builds_avoid_small_hosts_and_exports_default_to_local() {
        let home = TempDir::new();
        let mut router = Router::load(&home.0).unwrap();
        let mut hosts = [
            host("local", "macos", 12),
            host("pc", "linux", 24),
            host("air", "macos", 3),
        ];
        hosts[1].1.as_mut().unwrap().peak_memory = 15;
        hosts[1].1.as_mut().unwrap().status.idle_jobs = 0;
        assert_eq!(
            router
                .place(&source("/a"), &revision("a"), &request(), &hosts)
                .unwrap()
                .host,
            "pc"
        );
        hosts[1].1.as_mut().unwrap().peak_memory = 2;
        let mut request = request();
        request.copy_to = Some("/out".into());
        assert_eq!(
            router
                .place(&source("/a"), &revision("a"), &request, &hosts)
                .unwrap()
                .host,
            "local"
        );
        hosts[0].1.as_mut().unwrap().status.draining = true;
        assert!(
            router
                .place(&source("/a"), &revision("a"), &request, &hosts)
                .is_err()
        );
    }

    #[test]
    fn policy_is_persisted_and_changes_only_new_placements() {
        let home = TempDir::new();
        let mut router = Router::load(&home.0).unwrap();
        let hosts = [host("local", std::env::consts::OS, 12)];
        let accepted = router
            .place(&source("/a"), &revision("a"), &request(), &hosts)
            .unwrap();
        router.set_enabled(&home.0, &[], "local", false).unwrap();
        assert_eq!(accepted.host, "local");
        assert!(
            router
                .place(&source("/b"), &revision("b"), &request(), &hosts)
                .is_err()
        );
        assert!(!Router::load(&home.0).unwrap().hosts(&[])[0].enabled);
        assert!(router.set_enabled(&home.0, &[], "missing", true).is_err());
        router.set_enabled(&home.0, &[], "local", true).unwrap();
        assert!(Router::load(&home.0).unwrap().hosts(&[])[0].enabled);
    }
}
