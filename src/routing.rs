//! Host policy and the lifetime of in-flight placements. Resource admission
//! stays with each host's scheduler; observations only rank destinations.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Weak};

use crate::cargo::Operation;
use crate::config::Remote;
use crate::protocol::{BuildRequest, Capacity, Forecast, Host, HostPlan, Plan};
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
    exclusive: bool,
}

impl Placement {
    fn matches(&self, source: &Source, request: &BuildRequest) -> bool {
        self.source.repository == source.repository
            && self.source.prefix == source.prefix
            && self.operation == request.operation
            && self.copy_to == request.copy_to
            && self.rerun_all == request.rerun_all
            && self.exclusive == request.exclusive
    }
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
        let host = self.recommend(source, revision, request, observations)?;
        if let Some(placement) =
            self.placements
                .iter()
                .filter_map(Weak::upgrade)
                .find(|placement| {
                    placement.host == host
                        && placement.matches(source, request)
                        && placement.revision == *revision
                })
        {
            return Ok(placement);
        }
        let placement = Arc::new(Placement {
            host,
            source: source.clone(),
            revision: revision.clone(),
            operation: request.operation.clone(),
            copy_to: request.copy_to.clone(),
            rerun_all: request.rerun_all,
            exclusive: request.exclusive,
        });
        self.placements
            .retain(|placement| placement.strong_count() > 0);
        self.placements.push(Arc::downgrade(&placement));
        Ok(placement)
    }

    /// The exact host choice used by submission, without registering a placement.
    fn recommend(
        &self,
        source: &Source,
        revision: &Revision,
        request: &BuildRequest,
        observations: &[(Host, Result<Capacity, String>)],
    ) -> Result<String, String> {
        let constrained = |host: &Host| request.accepts_host(host) && self.enabled(&host.name);
        let eligible = observations
            .iter()
            .filter(|(host, _)| constrained(host))
            .collect::<Vec<_>>();
        let peak = eligible
            .iter()
            .filter_map(|(_, capacity)| capacity.as_ref().ok())
            .filter_map(|capacity| capacity.peak_memory)
            .max();
        let live = self
            .placements
            .iter()
            .filter_map(Weak::upgrade)
            .collect::<Vec<_>>();
        let usable = |host: &Host, capacity: &Capacity| {
            self.exclusion(host, request, Some(capacity), peak)
                .is_none()
        };
        let matching = |placement: &&Arc<Placement>| {
            placement.matches(source, request)
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
                return Ok(placement.host.clone());
            }
            placement.host.clone()
        } else {
            eligible
                .iter()
                .filter_map(|(host, result)| result.as_ref().ok().map(|capacity| (host, capacity)))
                .filter(|(host, capacity)| usable(host, capacity))
                .min_by_key(|(host, capacity)| {
                    let assigned = live
                        .iter()
                        .filter(|placement| placement.host == host.name)
                        .count();
                    let exclusive = live
                        .iter()
                        .any(|placement| placement.host == host.name && placement.exclusive);
                    rank(source, request, host, capacity, assigned, exclusive)
                })
                .map(|(host, _)| host.name.clone())
                .ok_or_else(|| {
                    let details = eligible
                        .iter()
                        .map(|(host, result)| {
                            let reason = match result {
                                Err(reason) => reason.clone(),
                                Ok(capacity) => self
                                    .exclusion(host, request, Some(capacity), peak)
                                    .expect("an unusable host has an exclusion"),
                            };
                            format!("{}: {reason}", host.name)
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
        Ok(host)
    }

    fn exclusion(
        &self,
        host: &Host,
        request: &BuildRequest,
        capacity: Option<&Capacity>,
        peak: Option<u64>,
    ) -> Option<String> {
        if !host.enabled || !self.enabled(&host.name) {
            return Some("host is disabled".into());
        }
        if !request.accepts_host(host) {
            return Some("excluded by the request's host, OS or artifact destination".into());
        }
        let capacity = capacity?;
        let status = &capacity.status;
        if status.draining {
            Some("host is draining".into())
        } else if status.free_disk.is_none() {
            Some("free disk unavailable".into())
        } else if status.free_disk.is_some_and(|free| free < status.min_free) {
            Some("below free disk floor".into())
        } else if peak.is_some_and(|peak| peak > status.memory) {
            Some(format!(
                "estimated peak {} bytes exceeds memory budget {}",
                peak.unwrap(),
                status.memory
            ))
        } else {
            None
        }
    }

    pub(crate) fn plan(
        &self,
        source: &Source,
        revision: Revision,
        request: &BuildRequest,
        observations: Vec<(Host, Result<Forecast, String>)>,
    ) -> Plan {
        let capacities = observations
            .iter()
            .map(|(host, forecast)| {
                (
                    host.clone(),
                    forecast
                        .as_ref()
                        .map(|forecast| forecast.capacity.clone())
                        .map_err(Clone::clone),
                )
            })
            .collect::<Vec<_>>();
        let peak = capacities
            .iter()
            .filter(|(host, _)| request.accepts_host(host) && self.enabled(&host.name))
            .filter_map(|(_, capacity)| capacity.as_ref().ok()?.peak_memory)
            .max();
        let recommendation = self.recommend(source, &revision, request, &capacities);
        let live = self
            .placements
            .iter()
            .filter_map(Weak::upgrade)
            .collect::<Vec<_>>();
        let mut hosts = observations
            .into_iter()
            .map(|(host, forecast)| {
                let exclusion = self
                    .exclusion(
                        &host,
                        request,
                        forecast.as_ref().ok().map(|forecast| &forecast.capacity),
                        peak,
                    )
                    .or_else(|| forecast.as_ref().err().cloned());
                HostPlan {
                    transfer_ms: (host.name == "local").then_some(0),
                    host,
                    forecast: forecast.ok(),
                    exclusion,
                }
            })
            .collect::<Vec<_>>();
        hosts.sort_by_cached_key(|plan| {
            let assigned = live
                .iter()
                .filter(|placement| placement.host == plan.host.name)
                .count();
            let rank = plan.forecast.as_ref().map(|forecast| {
                let exclusive = live
                    .iter()
                    .any(|placement| placement.host == plan.host.name && placement.exclusive);
                rank(
                    source,
                    request,
                    &plan.host,
                    &forecast.capacity,
                    assigned,
                    exclusive,
                )
            });
            (
                plan.exclusion.is_some(),
                recommendation
                    .as_ref()
                    .ok()
                    .is_none_or(|name| name != &plan.host.name),
                rank,
                plan.host.name.clone(),
            )
        });
        let (recommended_host, reason) = match recommendation {
            Ok(host) => {
                let existing = live.iter().any(|placement| {
                    placement.host == host
                        && placement.matches(source, request)
                        && (placement.revision == revision
                            || placement.source.worktree == source.worktree)
                });
                let reason = if existing {
                    "keeps equal or superseding work with its accepted placement"
                } else {
                    "current automatic routing order: immediate capacity, load, worktree affinity, memory budget"
                };
                (Some(host), reason.into())
            }
            Err(reason) => (None, reason),
        };
        Plan {
            revision,
            recommended_host,
            reason,
            hosts,
        }
    }
}

/// One ordering for host admission and its observational plan. These are
/// scheduling factors, not a prediction of end-to-end completion time.
fn rank(
    source: &Source,
    request: &BuildRequest,
    host: &Host,
    capacity: &Capacity,
    assigned: usize,
    exclusive: bool,
) -> (bool, usize, bool, std::cmp::Reverse<u64>, String) {
    let status = &capacity.status;
    let observed = status
        .slots
        .iter()
        .filter(|slot| slot.build.is_some() || slot.chore.is_some())
        .count()
        + status.queue.len();
    let load = observed.max(assigned);
    let reserved = capacity
        .peak_memory
        .or(status.unmeasured_memory)
        .unwrap_or(status.memory);
    let starts = load < status.capacity
        && !exclusive
        && !status
            .slots
            .iter()
            .any(|slot| slot.build.as_ref().is_some_and(|build| build.exclusive))
        && (!request.exclusive || load == 0)
        && status.idle_jobs > 0
        && status.queue.is_empty()
        && (capacity.peak_memory.is_some() || status.unmeasured_memory.is_some() || load == 0)
        && status.memory_in_use.saturating_add(reserved) <= status.memory
        && status.disk_wait.is_none()
        && status.free_disk.is_some_and(|free| {
            let running = status
                .slots
                .iter()
                .filter(|slot| slot.build.is_some())
                .count() as u64;
            free >= status
                .min_free
                .saturating_add(status.build_headroom.saturating_mul(running + 1))
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
            exclusive: false,
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
                peak_memory: Some(2),
                status: Status {
                    jobs: 2,
                    idle_jobs: 2,
                    capacity: 1,
                    memory,
                    memory_in_use: 0,
                    unmeasured_memory: None,
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

    fn forecasts(
        observations: &[(Host, Result<Capacity, String>)],
    ) -> Vec<(Host, Result<Forecast, String>)> {
        observations
            .iter()
            .map(|(host, capacity)| {
                (
                    host.clone(),
                    capacity.clone().map(|capacity| Forecast {
                        capacity,
                        recommended_slot: None,
                        shared_slot: None,
                        historical_build_ms: None,
                        queue_ms: Some(0),
                        slots: Vec::new(),
                        notes: Vec::new(),
                    }),
                )
            })
            .collect()
    }

    #[test]
    fn in_flight_exclusive_work_keeps_later_requests_on_other_hosts() {
        let home = TempDir::new();
        let mut router = Router::load(&home.0).unwrap();
        let mut hosts = vec![host("local", "macos", 100), host("pc", "linux", 100)];
        for (_, capacity) in &mut hosts {
            capacity.as_mut().unwrap().status.capacity = 4;
        }
        let mut exclusive = request();
        exclusive.exclusive = true;
        exclusive.host = Some("local".into());
        let first = router
            .place(&source("/a"), &revision("a"), &exclusive, &hosts)
            .unwrap();
        let ordinary = router
            .place(&source("/a"), &revision("a"), &request(), &hosts)
            .unwrap();
        assert_eq!(first.host, "local");
        assert_eq!(
            ordinary.host, "pc",
            "isolation applies before the host observes its build"
        );
        let shared = router
            .place(&source("/b"), &revision("a"), &exclusive, &hosts)
            .unwrap();
        assert!(Arc::ptr_eq(&first, &shared));
        let planned = router.plan(&source("/c"), revision("c"), &request(), forecasts(&hosts));
        assert_eq!(planned.recommended_host.as_deref(), Some("pc"));
    }

    #[test]
    fn exclusive_work_prefers_an_idle_host_and_explicit_constraints_still_hold() {
        let home = TempDir::new();
        let mut router = Router::load(&home.0).unwrap();
        let mut hosts = vec![host("local", "macos", 100), host("pc", "linux", 100)];
        for (_, capacity) in &mut hosts {
            capacity.as_mut().unwrap().status.capacity = 4;
        }
        let mut local = request();
        local.host = Some("local".into());
        let ordinary = router
            .place(&source("/a"), &revision("a"), &local, &hosts)
            .unwrap();
        let mut exclusive = request();
        exclusive.exclusive = true;
        let placed = router
            .place(&source("/a"), &revision("a"), &exclusive, &hosts)
            .unwrap();
        assert_eq!(placed.host, "pc");
        exclusive.host = Some("local".into());
        let pinned = router
            .place(&source("/c"), &revision("c"), &exclusive, &hosts)
            .unwrap();
        assert_eq!(pinned.host, ordinary.host);
    }

    #[test]
    fn plans_share_routing_order_without_registering_placements() {
        let home = TempDir::new();
        let mut router = Router::load(&home.0).unwrap();
        let hosts = [host("local", "linux", 12), host("pc", "linux", 24)];
        let plan = router.plan(&source("/a"), revision("a"), &request(), forecasts(&hosts));
        assert!(router.placements.is_empty());
        assert_eq!(plan.recommended_host.as_deref(), Some("pc"));
        assert_eq!(plan.hosts[0].host.name, "pc");
        assert_eq!(plan.hosts[0].transfer_ms, None);
        let placed = router
            .place(&source("/a"), &revision("a"), &request(), &hosts)
            .unwrap();
        assert_eq!(plan.recommended_host.as_ref(), Some(&placed.host));
        let same = router.plan(
            &source("/different"),
            revision("a"),
            &request(),
            forecasts(&hosts),
        );
        assert_eq!(same.recommended_host.as_ref(), Some(&placed.host));
        assert!(same.reason.contains("accepted placement"));
        assert_eq!(same.hosts[0].host.name, placed.host);
        assert_eq!(router.placements.len(), 1);
        let superseding = router.plan(
            &source("/a"),
            revision("new"),
            &request(),
            forecasts(&hosts),
        );
        assert_eq!(superseding.recommended_host.as_ref(), Some(&placed.host));
    }

    #[test]
    fn plans_explain_constraints_offline_hosts_and_resource_exclusions() {
        let home = TempDir::new();
        let router = Router::load(&home.0).unwrap();
        let mut hosts = [
            host("local", "macos", 1),
            host("pc", "linux", 24),
            host("offline", "linux", 12),
        ];
        hosts[2].1 = Err("connection timed out".into());
        let plan = router.plan(&source("/a"), revision("a"), &request(), forecasts(&hosts));
        assert_eq!(plan.recommended_host.as_deref(), Some("pc"));
        assert!(
            plan.hosts
                .iter()
                .find(|host| host.host.name == "local")
                .unwrap()
                .exclusion
                .as_ref()
                .unwrap()
                .contains("memory budget")
        );
        assert!(
            plan.hosts
                .iter()
                .find(|host| host.host.name == "offline")
                .unwrap()
                .exclusion
                .as_ref()
                .unwrap()
                .contains("timed out")
        );
        let mut constrained = request();
        constrained.host = Some("offline".into());
        let plan = router.plan(
            &source("/a"),
            revision("a"),
            &constrained,
            forecasts(&hosts),
        );
        assert!(plan.recommended_host.is_none());
        assert!(plan.hosts.iter().all(|host| host.exclusion.is_some()));
    }

    #[test]
    fn submission_reassesses_policy_after_a_plan() {
        let home = TempDir::new();
        let mut router = Router::load(&home.0).unwrap();
        let hosts = [host("local", "linux", 24), host("pc", "linux", 12)];
        let plan = router.plan(&source("/a"), revision("a"), &request(), forecasts(&hosts));
        assert_eq!(plan.recommended_host.as_deref(), Some("local"));
        router.set_enabled(&home.0, &[], "local", false).unwrap();
        let placed = router
            .place(&source("/a"), &revision("a"), &request(), &hosts)
            .unwrap();
        assert_eq!(placed.host, "pc");
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
    fn unknown_memory_can_learn_and_prefers_a_host_without_an_existing_assignment() {
        let home = TempDir::new();
        let mut router = Router::load(&home.0).unwrap();
        let mut hosts = [host("local", "linux", 12), host("pc", "linux", 12)];
        let existing = router
            .place(&source("/busy"), &revision("busy"), &request(), &hosts)
            .unwrap();
        assert_eq!(existing.host, "local");
        for (_, capacity) in &mut hosts {
            capacity.as_mut().unwrap().peak_memory = None;
        }
        let unknown = router
            .place(&source("/new"), &revision("new"), &request(), &hosts)
            .unwrap();
        assert_eq!(unknown.host, "pc");
    }

    #[test]
    fn routing_uses_each_hosts_unknown_reservation_even_when_another_host_measured_the_build() {
        let home = TempDir::new();
        let mut router = Router::load(&home.0).unwrap();
        let mut hosts = [host("local", "linux", 12), host("pc", "linux", 12)];
        for (_, capacity) in &mut hosts {
            capacity.as_mut().unwrap().status.capacity = 2;
        }
        let local = hosts[0].1.as_mut().unwrap();
        local.peak_memory = None;
        local.status.memory_in_use = 7;
        local.status.unmeasured_memory = Some(6);
        assert_eq!(
            router
                .place(&source("/a"), &revision("a"), &request(), &hosts)
                .unwrap()
                .host,
            "pc",
            "the other host's 2-byte peak cannot replace this host's reservation"
        );
        hosts[0].1.as_mut().unwrap().status.unmeasured_memory = Some(5);
        hosts[1].1.as_mut().unwrap().status.idle_jobs = 0;
        assert_eq!(
            router
                .place(&source("/b"), &revision("b"), &request(), &hosts)
                .unwrap()
                .host,
            "local",
            "an unmeasured build can share this host's remaining capacity"
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
        hosts[1].1.as_mut().unwrap().peak_memory = Some(15);
        hosts[1].1.as_mut().unwrap().status.idle_jobs = 0;
        assert_eq!(
            router
                .place(&source("/a"), &revision("a"), &request(), &hosts)
                .unwrap()
                .host,
            "pc"
        );
        hosts[1].1.as_mut().unwrap().peak_memory = Some(2);
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
