//! Which build runs where and who hears about it.
//!
//! The scheduler is pure state: the daemon feeds it requests, withdrawals,
//! output and exits, and carries out the [`Effect`]s it returns.
//!
//! - **Deduplication.** A request equal to a queued or running build (same
//!   repository, revision, directory and operation) waits for that build.
//! - **Supersession.** A request from a worktree replaces that worktree's
//!   queued requests for the same directory and operation at older
//!   revisions: their waiters wait for the new revision instead, at the
//!   oldest one's place in the queue. Waiters from other worktrees keep
//!   their revision.
//! - **Cancellation.** A running build nobody waits for any more is
//!   cancelled; a queued one is dropped.
//! - **Slots.** At most `capacity` slots are busy: running a build, or being
//!   kept within their disk limit right after one. A build prefers the idle
//!   slot that last built its worktree, then the least recently used idle
//!   slot of its repository. It gets a new slot only when every slot of its
//!   repository runs a build, so slot directories are created only when
//!   builds of a repository run concurrently.

use std::collections::{BTreeMap, HashMap};
use std::path::PathBuf;
use std::time::{Duration, Instant};

use crate::cargo::Operation;
use crate::protocol::{Message, Outcome, QueuedBuild, RunningBuild, SlotStatus};
use crate::slot::slot_name;
use crate::snapshot::{Revision, Source};

/// A build the scheduler tracks.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Ord, PartialOrd, Hash)]
pub(crate) struct JobId(u64);

/// A slot the scheduler tracks.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash)]
pub(crate) struct SlotKey(usize);

/// A client waiting for a build.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Ord, PartialOrd, Hash)]
pub(crate) struct WaiterId(pub(crate) u64);

/// A request to build `revision` of `source`.
pub(crate) struct Submission {
    pub(crate) waiter: WaiterId,
    pub(crate) source: Source,
    pub(crate) revision: Revision,
    pub(crate) operation: Operation,
}

/// What the daemon must do.
#[derive(Debug, PartialEq)]
pub(crate) enum Effect {
    Send { waiter: WaiterId, message: Message },
    Start(Start),
    Cancel { job: JobId },
}

/// A build to start in slot `slot` of `repository`.
#[derive(Debug, PartialEq)]
pub(crate) struct Start {
    pub(crate) job: JobId,
    pub(crate) key: SlotKey,
    pub(crate) repository: PathBuf,
    /// The slot's index among its repository's slots.
    pub(crate) slot: usize,
    pub(crate) prefix: PathBuf,
    pub(crate) revision: Revision,
    pub(crate) operation: Operation,
}

struct Waiter {
    id: WaiterId,
    worktree: PathBuf,
    since: Instant,
}

struct Job {
    repository: PathBuf,
    prefix: PathBuf,
    revision: Revision,
    operation: Operation,
    waiters: Vec<Waiter>,
    /// The job's place in the queue: the earliest submission it carries.
    order: u64,
    state: State,
    /// Output so far, replayed to waiters who join while it runs.
    output: Vec<Message>,
}

enum State {
    Queued,
    Running {
        /// Index into the scheduler's slots.
        slot: usize,
        started: Instant,
        cancelled: bool,
    },
}

struct Slot {
    repository: PathBuf,
    /// The slot's index among its repository's slots.
    index: usize,
    job: Option<JobId>,
    /// Its target is being kept within its disk limit after a build.
    maintaining: bool,
    /// Its target's disk usage after its last build, when measured.
    size: Option<u64>,
    worktree: Option<PathBuf>,
    /// When it last started a build, on the scheduler's clock.
    used: u64,
}

impl Slot {
    fn busy(&self) -> bool {
        self.job.is_some() || self.maintaining
    }
}

pub(crate) struct Scheduler {
    capacity: usize,
    clock: u64,
    next_job: u64,
    jobs: BTreeMap<JobId, Job>,
    waiting: HashMap<WaiterId, JobId>,
    slots: Vec<Slot>,
}

impl Job {
    fn same_build(&self, source: &Source, revision: &Revision, operation: &Operation) -> bool {
        self.repository == source.repository
            && self.prefix == source.prefix
            && self.operation == *operation
            && self.revision == *revision
    }

    fn queued(&self) -> bool {
        matches!(self.state, State::Queued)
    }

    fn cancelled(&self) -> bool {
        matches!(
            self.state,
            State::Running {
                cancelled: true,
                ..
            }
        )
    }

    fn send_all(&self, message: &Message) -> impl Iterator<Item = Effect> {
        self.waiters.iter().map(move |waiter| Effect::Send {
            waiter: waiter.id,
            message: message.clone(),
        })
    }
}

impl Scheduler {
    pub(crate) fn new(capacity: usize) -> Self {
        assert!(capacity > 0, "a scheduler runs at least one build");
        Self {
            capacity,
            clock: 0,
            next_job: 0,
            jobs: BTreeMap::new(),
            waiting: HashMap::new(),
            slots: Vec::new(),
        }
    }

    pub(crate) fn submit(&mut self, submission: Submission) -> Vec<Effect> {
        let Submission {
            waiter,
            source,
            revision,
            operation,
        } = submission;
        assert!(!self.waiting.contains_key(&waiter), "a waiter submits once");
        self.clock += 1;
        let mut order = self.clock;
        let mut joining = vec![Waiter {
            id: waiter,
            worktree: source.worktree.clone(),
            since: Instant::now(),
        }];

        let superseded = self
            .jobs
            .iter()
            .filter(|(_, job)| {
                job.queued()
                    && job.repository == source.repository
                    && job.prefix == source.prefix
                    && job.operation == operation
                    && job.revision != revision
            })
            .map(|(id, _)| *id)
            .collect::<Vec<_>>();
        for id in superseded {
            let job = self.jobs.get_mut(&id).expect("listed above");
            let (mine, others) = std::mem::take(&mut job.waiters)
                .into_iter()
                .partition::<Vec<_>, _>(|waiter| waiter.worktree == source.worktree);
            job.waiters = others;
            if !mine.is_empty() {
                order = order.min(job.order);
                joining.extend(mine);
            }
            if job.waiters.is_empty() {
                self.jobs.remove(&id);
            }
        }

        let existing = self
            .jobs
            .iter()
            .find(|(_, job)| job.same_build(&source, &revision, &operation) && !job.cancelled())
            .map(|(id, _)| *id);
        let id = existing.unwrap_or_else(|| {
            let id = JobId(self.next_job);
            self.next_job += 1;
            self.jobs.insert(
                id,
                Job {
                    repository: source.repository.clone(),
                    prefix: source.prefix.clone(),
                    revision,
                    operation,
                    waiters: Vec::new(),
                    order,
                    state: State::Queued,
                    output: Vec::new(),
                },
            );
            id
        });

        let mut effects = Vec::new();
        let job = self.jobs.get_mut(&id).expect("found or inserted above");
        job.order = job.order.min(order);
        for waiter in joining {
            self.waiting.insert(waiter.id, id);
            effects.extend(self.greeting(id, waiter.id));
            self.jobs
                .get_mut(&id)
                .expect("found or inserted above")
                .waiters
                .push(waiter);
        }
        effects.extend(self.start_ready());
        effects
    }

    /// What a waiter joining job `id` is told first.
    fn greeting(&self, id: JobId, waiter: WaiterId) -> Vec<Effect> {
        let job = &self.jobs[&id];
        match job.state {
            State::Queued => vec![Effect::Send {
                waiter,
                message: Message::Queued {
                    revision: job.revision.clone(),
                },
            }],
            State::Running { slot, .. } => {
                let started = Message::Started {
                    revision: job.revision.clone(),
                    slot: slot_name(&self.slots[slot].repository, self.slots[slot].index),
                };
                std::iter::once(started)
                    .chain(job.output.iter().cloned())
                    .map(|message| Effect::Send { waiter, message })
                    .collect()
            }
        }
    }

    /// The client of `waiter` went away.
    pub(crate) fn withdraw(&mut self, waiter: WaiterId) -> Vec<Effect> {
        // A waiter whose build already finished is no longer tracked.
        let Some(id) = self.waiting.remove(&waiter) else {
            return Vec::new();
        };
        let job = self.jobs.get_mut(&id).expect("a waiter's job is tracked");
        job.waiters.retain(|other| other.id != waiter);
        if !job.waiters.is_empty() {
            return Vec::new();
        }
        match &mut job.state {
            State::Queued => {
                self.jobs.remove(&id);
                Vec::new()
            }
            State::Running { cancelled, .. } => {
                *cancelled = true;
                vec![Effect::Cancel { job: id }]
            }
        }
    }

    /// A line of output of running job `id`.
    pub(crate) fn output(&mut self, id: JobId, message: Message) -> Vec<Effect> {
        // Output of a process that escaped its build's process group can
        // arrive after the build ended; nobody waits for it.
        let Some(job) = self.jobs.get_mut(&id) else {
            return Vec::new();
        };
        let effects = job.send_all(&message).collect();
        job.output.push(message);
        effects
    }

    /// Running job `id` ended with `outcome`.
    pub(crate) fn exited(&mut self, id: JobId, outcome: &Outcome) -> Vec<Effect> {
        let job = self.jobs.remove(&id).expect("only tracked jobs run");
        let State::Running { slot, started, .. } = job.state else {
            panic!("only running jobs exit");
        };
        self.slots[slot].job = None;
        self.slots[slot].maintaining = true;
        let build_ms = millis(started.elapsed());
        let mut effects = Vec::new();
        for waiter in job.waiters {
            self.waiting.remove(&waiter.id);
            effects.push(Effect::Send {
                waiter: waiter.id,
                message: Message::Finished {
                    revision: job.revision.clone(),
                    outcome: outcome.clone(),
                    queued_ms: millis(started.saturating_duration_since(waiter.since)),
                    build_ms,
                },
            });
        }
        effects.extend(self.start_ready());
        effects
    }

    /// Slot `key` is within its disk limit again, `size` bytes when
    /// measured.
    pub(crate) fn maintained(&mut self, key: SlotKey, size: Option<u64>) -> Vec<Effect> {
        let slot = &mut self.slots[key.0];
        assert!(
            slot.maintaining,
            "only a slot that ran a build is maintained"
        );
        slot.maintaining = false;
        slot.size = size;
        self.start_ready()
    }

    /// Starts queued builds, next first, while slots are free. A build that
    /// waits for its repository's slot lets later builds of other
    /// repositories go first.
    fn start_ready(&mut self) -> Vec<Effect> {
        let mut effects = Vec::new();
        let mut queued = self
            .jobs
            .iter()
            .filter(|(_, job)| job.queued())
            .map(|(id, job)| (job.order, *id))
            .collect::<Vec<_>>();
        queued.sort_unstable();
        for (_, id) in queued {
            if self.slots.iter().filter(|slot| slot.busy()).count() >= self.capacity {
                break;
            }
            let job = &self.jobs[&id];
            let worktree = job.waiters[0].worktree.clone();
            let Some(slot) = self.choose_slot(&job.repository.clone(), &worktree) else {
                continue;
            };
            self.clock += 1;
            let entry = &mut self.slots[slot];
            entry.job = Some(id);
            entry.worktree = Some(worktree);
            entry.used = self.clock;
            let job = self.jobs.get_mut(&id).expect("chosen above");
            job.state = State::Running {
                slot,
                started: Instant::now(),
                cancelled: false,
            };
            let started = Message::Started {
                revision: job.revision.clone(),
                slot: slot_name(&self.slots[slot].repository, self.slots[slot].index),
            };
            effects.extend(job.send_all(&started));
            effects.push(Effect::Start(Start {
                job: id,
                key: SlotKey(slot),
                repository: job.repository.clone(),
                slot: self.slots[slot].index,
                prefix: job.prefix.clone(),
                revision: job.revision.clone(),
                operation: job.operation.clone(),
            }));
        }
        effects
    }

    /// The slot a build of `repository` for `worktree` runs in now: an idle
    /// one, or a new one when every slot of the repository runs a build and
    /// it has fewer than `capacity`. None while the build must wait.
    fn choose_slot(&mut self, repository: &PathBuf, worktree: &PathBuf) -> Option<usize> {
        let idle = |slot: &&Slot| slot.repository == *repository && !slot.busy();
        let affine = self
            .slots
            .iter()
            .enumerate()
            .find(|(_, slot)| idle(slot) && slot.worktree.as_ref() == Some(worktree));
        let least_recent = || {
            self.slots
                .iter()
                .enumerate()
                .filter(|(_, slot)| idle(slot))
                .min_by_key(|(_, slot)| slot.used)
        };
        if let Some((index, _)) = affine.or_else(least_recent) {
            return Some(index);
        }
        let own = || {
            self.slots
                .iter()
                .filter(|slot| slot.repository == *repository)
        };
        // A slot being maintained is free again soon, and warm.
        if own().any(|slot| slot.maintaining) || own().count() >= self.capacity {
            return None;
        }
        let index = own().count();
        self.slots.push(Slot {
            repository: repository.clone(),
            index,
            job: None,
            maintaining: false,
            size: None,
            worktree: None,
            used: 0,
        });
        Some(self.slots.len() - 1)
    }

    /// The slots and the queue, next build first.
    pub(crate) fn status(&self) -> (Vec<SlotStatus>, Vec<QueuedBuild>) {
        let slots = self
            .slots
            .iter()
            .map(|slot| SlotStatus {
                name: slot_name(&slot.repository, slot.index),
                worktree: slot.worktree.clone(),
                size: slot.size,
                maintaining: slot.maintaining,
                build: slot.job.map(|id| {
                    let job = &self.jobs[&id];
                    let State::Running {
                        started, cancelled, ..
                    } = job.state
                    else {
                        panic!("a slot's job runs");
                    };
                    RunningBuild {
                        revision: job.revision.clone(),
                        operation: job.operation.clone(),
                        waiters: job.waiters.len(),
                        elapsed_ms: millis(started.elapsed()),
                        cancelled,
                    }
                }),
            })
            .collect();
        let mut queued = self
            .jobs
            .iter()
            .filter(|(_, job)| job.queued())
            .collect::<Vec<_>>();
        queued.sort_by_key(|(id, job)| (job.order, **id));
        let queue = queued
            .into_iter()
            .map(|(_, job)| QueuedBuild {
                revision: job.revision.clone(),
                operation: job.operation.clone(),
                worktrees: job
                    .waiters
                    .iter()
                    .map(|waiter| waiter.worktree.clone())
                    .collect(),
                waited_ms: job
                    .waiters
                    .iter()
                    .map(|waiter| millis(waiter.since.elapsed()))
                    .max()
                    .unwrap_or(0),
            })
            .collect();
        (slots, queue)
    }
}

fn millis(duration: Duration) -> u64 {
    u64::try_from(duration.as_millis()).unwrap_or(u64::MAX)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cargo::Command;

    fn source(worktree: &str) -> Source {
        Source {
            repository: "/repo/.git".into(),
            worktree: worktree.into(),
            prefix: PathBuf::new(),
            index: PathBuf::new(),
        }
    }

    fn check() -> Operation {
        Operation {
            command: Command::Check,
            args: Vec::new(),
        }
    }

    fn revision(name: &str) -> Revision {
        serde_json::from_value(serde_json::Value::String(name.into())).unwrap()
    }

    fn submit(scheduler: &mut Scheduler, waiter: u64, worktree: &str, tree: &str) -> Vec<Effect> {
        scheduler.submit(Submission {
            waiter: WaiterId(waiter),
            source: source(worktree),
            revision: revision(tree),
            operation: check(),
        })
    }

    fn starts(effects: &[Effect]) -> Vec<(JobId, usize, String)> {
        effects
            .iter()
            .filter_map(|effect| match effect {
                Effect::Start(start) => Some((start.job, start.slot, start.revision.to_string())),
                _ => None,
            })
            .collect()
    }

    fn sent(effects: &[Effect], waiter: u64) -> Vec<&Message> {
        effects
            .iter()
            .filter_map(|effect| match effect {
                Effect::Send {
                    waiter: to,
                    message,
                } if *to == WaiterId(waiter) => Some(message),
                _ => None,
            })
            .collect()
    }

    fn ok() -> Outcome {
        Outcome::Exited { code: 0 }
    }

    #[test]
    fn equal_requests_share_one_build_and_a_late_joiner_gets_the_output_so_far() {
        let mut scheduler = Scheduler::new(1);
        let first = submit(&mut scheduler, 1, "/a", "t1");
        let [(job, 0, _)] = starts(&first)[..] else {
            panic!("{first:?}");
        };
        let line = Message::Stdout { line: "x".into() };
        assert_eq!(scheduler.output(job, line.clone()).len(), 1);

        let second = submit(&mut scheduler, 2, "/b", "t1");
        assert!(starts(&second).is_empty());
        assert!(matches!(sent(&second, 2)[..], [Message::Started { .. }, m] if *m == line));

        let finished = scheduler.exited(job, &ok());
        assert!(matches!(sent(&finished, 1)[..], [Message::Finished { .. }]));
        assert!(matches!(sent(&finished, 2)[..], [Message::Finished { .. }]));
    }

    /// Ends running job `job` in slot `slot` and maintains the slot.
    fn finish(scheduler: &mut Scheduler, job: JobId, slot: usize) -> Vec<Effect> {
        let mut effects = scheduler.exited(job, &ok());
        effects.extend(scheduler.maintained(SlotKey(slot), Some(1)));
        effects
    }

    #[test]
    fn a_slot_being_maintained_is_waited_for_rather_than_replaced() {
        let mut scheduler = Scheduler::new(2);
        let first = submit(&mut scheduler, 1, "/a", "a1");
        let [(job, 0, _)] = starts(&first)[..] else {
            panic!("{first:?}");
        };
        assert!(starts(&scheduler.exited(job, &ok())).is_empty());
        let waiting = submit(&mut scheduler, 2, "/a", "a2");
        assert!(starts(&waiting).is_empty(), "no second, cold slot");
        // Another repository's build does not wait behind it.
        let other = scheduler.submit(Submission {
            waiter: WaiterId(3),
            source: Source {
                repository: "/other/.git".into(),
                ..source("/o")
            },
            revision: revision("o1"),
            operation: check(),
        });
        assert_eq!(starts(&other).len(), 1);
        let (slots, queue) = scheduler.status();
        assert!(slots[0].maintaining && slots[0].build.is_none());
        assert_eq!(queue.len(), 1);
        let ready = scheduler.maintained(SlotKey(0), Some(5));
        assert!(
            matches!(starts(&ready)[..], [(_, 0, ref tree)] if tree == "a2"),
            "the waiting build takes its warm slot"
        );
        assert_eq!(scheduler.status().0[0].size, Some(5));
    }

    #[test]
    fn a_newer_request_supersedes_its_worktrees_queued_ones_and_keeps_their_place() {
        let mut scheduler = Scheduler::new(1);
        let busy = submit(&mut scheduler, 1, "/busy", "t0");
        let [(running, ..)] = starts(&busy)[..] else {
            panic!("{busy:?}");
        };
        submit(&mut scheduler, 2, "/a", "t1");
        submit(&mut scheduler, 3, "/b", "t1");
        submit(&mut scheduler, 4, "/c", "u1");
        let newer = submit(&mut scheduler, 5, "/a", "t2");
        // /a's waiter moves to t2; /b's keeps t1, which is still its content.
        assert!(
            matches!(sent(&newer, 2)[..], [Message::Queued { revision }] if revision.to_string() == "t2")
        );
        let (_, queue) = scheduler.status();
        let order = queue
            .iter()
            .map(|build| (build.revision.to_string(), build.worktrees.len()))
            .collect::<Vec<_>>();
        assert_eq!(
            order,
            [("t1".into(), 1), ("t2".into(), 2), ("u1".into(), 1)]
        );

        let next = finish(&mut scheduler, running, 0);
        assert_eq!(
            starts(&next)
                .iter()
                .map(|s| s.2.clone())
                .collect::<Vec<_>>(),
            ["t1"]
        );
    }

    #[test]
    fn a_build_nobody_waits_for_is_dropped_or_cancelled_and_not_joined() {
        let mut scheduler = Scheduler::new(1);
        let first = submit(&mut scheduler, 1, "/a", "t1");
        let [(job, ..)] = starts(&first)[..] else {
            panic!("{first:?}");
        };
        submit(&mut scheduler, 2, "/b", "t2");
        assert!(scheduler.withdraw(WaiterId(2)).is_empty());
        assert!(scheduler.status().1.is_empty());

        assert_eq!(scheduler.withdraw(WaiterId(1)), [Effect::Cancel { job }]);
        // An equal request does not join the build being stopped.
        let again = submit(&mut scheduler, 3, "/a", "t1");
        assert!(matches!(sent(&again, 3)[..], [Message::Queued { .. }]));
        let mut next = scheduler.exited(job, &Outcome::Signaled { signal: 15 });
        next.extend(scheduler.maintained(SlotKey(0), None));
        assert!(sent(&next, 1).is_empty());
        assert_eq!(starts(&next).len(), 1);
        assert!(scheduler.withdraw(WaiterId(1)).is_empty());
    }

    #[test]
    fn builds_return_to_their_worktrees_slot_and_slots_grow_only_with_concurrency() {
        let mut scheduler = Scheduler::new(2);
        let a = submit(&mut scheduler, 1, "/a", "a1");
        let b = submit(&mut scheduler, 2, "/b", "b1");
        let [(job_a, 0, _)] = starts(&a)[..] else {
            panic!("{a:?}")
        };
        let [(job_b, 1, _)] = starts(&b)[..] else {
            panic!("{b:?}")
        };
        finish(&mut scheduler, job_a, 0);
        finish(&mut scheduler, job_b, 1);

        let b = submit(&mut scheduler, 3, "/b", "b2");
        let [(job_b, 1, _)] = starts(&b)[..] else {
            panic!("{b:?}")
        };
        finish(&mut scheduler, job_b, 1);
        // A new worktree takes the least recently used slot, not a third one.
        let c = submit(&mut scheduler, 4, "/c", "c1");
        let [(_, 0, _)] = starts(&c)[..] else {
            panic!("{c:?}")
        };
        let d = submit(&mut scheduler, 5, "/d", "d1");
        let [(_, 1, _)] = starts(&d)[..] else {
            panic!("{d:?}")
        };
        let e = submit(&mut scheduler, 6, "/e", "e1");
        assert!(starts(&e).is_empty());
        assert_eq!(scheduler.status().0.len(), 2);
    }
}
