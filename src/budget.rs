//! The machine's job budget, shared by every running build.
//!
//! Each build gets a jobserver of its own: a FIFO its Cargo and compilers
//! take tokens from and give them back to, as GNU make's protocol says. The
//! daemon moves tokens between these jobservers from one budget, so it knows
//! at every moment what each build holds: the tokens its processes took, the
//! ones waiting in its FIFO, and Cargo's implicit token. A build that is
//! killed returns everything when it ends, whatever its processes held.
//! (With one jobserver for all builds, a compiler killed while holding
//! tokens took them with it for the daemon's lifetime.)
//!
//! A build whose tests run is charged at least the tests' share of the
//! machine: test threads and the processes they start use it without
//! taking tokens, so compilation elsewhere shrinks to what is left.
//!
//! Every [`TICK`] the daemon takes back the tokens waiting in each FIFO and
//! deals what is free again: one to each compiling build, then more to the
//! builds that took tokens since the last tick, a round at a time, starting
//! with a different build each tick.

use std::collections::BTreeMap;
use std::fs::File;
use std::io::{Read as _, Write as _};
use std::os::fd::AsRawFd as _;
use std::os::unix::fs::OpenOptionsExt as _;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Condvar, Mutex};
use std::time::Duration;

/// How often tokens are dealt again while builds run.
const TICK: Duration = Duration::from_millis(5);

/// The budget: how many jobs the machine runs at once, of which the tests
/// of one build take `test_jobs` while they run.
#[derive(Clone)]
pub(crate) struct Budget {
    shared: Arc<Shared>,
}

struct Shared {
    ledger: Mutex<Ledger>,
    /// Signalled when a build joins, so the dealer wakes from idleness.
    joined: Condvar,
}

struct Ledger {
    jobs: usize,
    test_jobs: usize,
    /// FIFOs are created here and removed once open.
    scratch: PathBuf,
    next: u64,
    accounts: BTreeMap<u64, Account>,
    /// The account dealt to first next tick.
    turn: u64,
}

/// A build's jobserver and what it is charged.
struct Account {
    /// The daemon's own end: reads never block.
    reader: File,
    /// Where tokens are written; the build's processes write theirs back
    /// through it too.
    writer: File,
    /// The read end the build's processes inherit: reads block, as every
    /// jobserver client expects.
    child_reader: File,
    /// Tokens written into the FIFO and not taken back: held by the build's
    /// processes or waiting in the FIFO.
    deposited: usize,
    /// Tokens its processes held at the latest tick.
    held: usize,
    testing: bool,
}

impl Account {
    /// What the build is charged: Cargo's implicit token and every token
    /// deposited, and while its tests run, at least the tests' share.
    fn charge(&self, test_jobs: usize) -> usize {
        let charge = 1 + self.deposited;
        if self.testing {
            charge.max(test_jobs)
        } else {
            charge
        }
    }

    /// Tokens waiting in the FIFO.
    fn waiting(&self) -> usize {
        rustix::io::ioctl_fionread(&self.reader).map_or_else(
            |error| panic!("a FIFO the daemon holds open can be queried: {error}"),
            |waiting| usize::try_from(waiting).expect("a FIFO holds few bytes"),
        )
    }

    /// Takes back every token waiting in the FIFO.
    fn withdraw_waiting(&mut self) {
        let mut buffer = [0; 64];
        loop {
            match self.reader.read(&mut buffer) {
                Ok(0) => panic!("the daemon holds the FIFO's writer open"),
                Ok(read) => {
                    // A process wrote back more than it took: nothing to
                    // take back beyond what was deposited.
                    self.deposited = self.deposited.saturating_sub(read);
                }
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => return,
                Err(error) if error.kind() == std::io::ErrorKind::Interrupted => {}
                Err(error) => panic!("reading the daemon's own FIFO fails only by a bug: {error}"),
            }
        }
    }

    fn deposit(&mut self) {
        self.writer
            .write_all(b"|")
            .expect("a FIFO with room for every token takes one more");
        self.deposited += 1;
    }
}

impl Budget {
    /// A budget of `jobs`, `test_jobs` of them for a build's tests, whose
    /// FIFOs are made in `scratch`. Starts the thread that deals tokens.
    ///
    /// # Errors
    /// When the dealer thread cannot start.
    pub(crate) fn new(jobs: usize, test_jobs: usize, scratch: &Path) -> Result<Self, String> {
        assert!(
            (1..=jobs).contains(&test_jobs),
            "tests take between one job and all of them"
        );
        let budget = Self {
            shared: Arc::new(Shared {
                ledger: Mutex::new(Ledger {
                    jobs,
                    test_jobs,
                    scratch: scratch.to_owned(),
                    next: 0,
                    accounts: BTreeMap::new(),
                    turn: 0,
                }),
                joined: Condvar::new(),
            }),
        };
        let dealer = budget.clone();
        std::thread::Builder::new()
            .name("buildd-budget".into())
            .spawn(move || dealer.deal_forever())
            .map_err(|error| format!("could not start dealing jobs: {error}"))?;
        Ok(budget)
    }

    fn ledger(&self) -> std::sync::MutexGuard<'_, Ledger> {
        self.shared
            .ledger
            .lock()
            .expect("no thread panics holding the budget")
    }

    /// Opens an account for a build: a jobserver its Cargo is configured
    /// with. Closing the lease closes the account and returns its tokens.
    ///
    /// # Errors
    /// When its FIFO cannot be made.
    pub(crate) fn open(&self) -> Result<Lease, String> {
        let mut ledger = self.ledger();
        let id = ledger.next;
        ledger.next += 1;
        let account = fifo(
            &ledger
                .scratch
                .join(format!("jobs-{}-{id}", std::process::id())),
        )?;
        ledger.accounts.insert(id, account);
        drop(ledger);
        self.shared.joined.notify_one();
        Ok(Lease {
            budget: self.clone(),
            id,
        })
    }

    /// Jobs the budget has, and of those, the ones no build is charged for.
    pub(crate) fn jobs(&self) -> (usize, usize) {
        let ledger = self.ledger();
        (ledger.jobs, ledger.free())
    }

    fn deal_forever(&self) -> ! {
        let mut ledger = self.ledger();
        loop {
            while ledger.accounts.is_empty() {
                ledger = self
                    .shared
                    .joined
                    .wait(ledger)
                    .expect("no thread panics holding the budget");
            }
            ledger.deal();
            drop(ledger);
            std::thread::sleep(TICK);
            ledger = self.ledger();
        }
    }
}

impl Ledger {
    /// Jobs no build is charged for.
    fn free(&self) -> usize {
        let charged = self
            .accounts
            .values()
            .map(|account| account.charge(self.test_jobs))
            .sum::<usize>();
        self.jobs.saturating_sub(charged)
    }

    /// Takes back the tokens waiting in every FIFO and deals what is free.
    fn deal(&mut self) {
        let mut took = BTreeMap::new();
        for (id, account) in &mut self.accounts {
            let held = account.deposited.saturating_sub(account.waiting());
            took.insert(*id, held.saturating_sub(account.held));
            account.held = held;
            account.withdraw_waiting();
        }
        let mut free = self.free();
        // Compiling builds in this tick's order, starting at `turn`.
        let order = self
            .accounts
            .range(self.turn..)
            .chain(self.accounts.range(..self.turn))
            .filter(|(_, account)| !account.testing)
            .map(|(id, _)| *id)
            .collect::<Vec<_>>();
        let mut round = 0;
        while free > 0 {
            let mut dealt = false;
            for id in &order {
                if free == 0 {
                    break;
                }
                // Everyone gets one; more only to builds that took as many.
                if round == 0 || took[id] > round {
                    self.accounts.get_mut(id).expect("listed above").deposit();
                    free -= 1;
                    dealt = true;
                }
            }
            if !dealt {
                break;
            }
            round += 1;
        }
        // Next tick starts after the build that went first in this one.
        self.turn = order.first().map_or(0, |id| id + 1);
    }
}

/// A build's account in the budget, closed when dropped.
pub(crate) struct Lease {
    budget: Budget,
    id: u64,
}

impl Lease {
    /// Makes `command` a client of the build's jobserver, as Cargo reads it
    /// from `CARGO_MAKEFLAGS`.
    pub(crate) fn configure(&self, command: &mut std::process::Command) {
        use std::os::unix::process::CommandExt as _;

        let ledger = self.budget.ledger();
        let account = &ledger.accounts[&self.id];
        let read = account.child_reader.as_raw_fd();
        let write = account.writer.as_raw_fd();
        command.env(
            "CARGO_MAKEFLAGS",
            format!("-j --jobserver-fds={read},{write} --jobserver-auth={read},{write}"),
        );
        // SAFETY: the closure only calls fcntl, which is async-signal-safe,
        // on descriptors the account keeps open until the lease is dropped,
        // after the child started.
        unsafe {
            command.pre_exec(move || {
                for fd in [read, write] {
                    let fd = std::os::fd::BorrowedFd::borrow_raw(fd);
                    rustix::io::fcntl_setfd(fd, rustix::io::FdFlags::empty())?;
                }
                Ok(())
            });
        }
    }

    /// The build's tests run: it is charged at least their share from now on.
    pub(crate) fn testing(&self) {
        self.budget
            .ledger()
            .accounts
            .get_mut(&self.id)
            .expect("a lease's account stays open")
            .testing = true;
    }

    /// What the build is charged now.
    pub(crate) fn charge(&self) -> usize {
        let ledger = self.budget.ledger();
        ledger.accounts[&self.id].charge(ledger.test_jobs)
    }
}

impl Drop for Lease {
    fn drop(&mut self) {
        self.budget.ledger().accounts.remove(&self.id);
    }
}

/// Makes a FIFO at `path`, opens its three ends and removes the path: the
/// open descriptors keep it alive.
fn fifo(path: &Path) -> Result<Account, String> {
    let failed = |error: std::io::Error| format!("could not make a jobserver: {error}");
    std::fs::create_dir_all(path.parent().expect("a FIFO lives in a directory")).map_err(failed)?;
    let name = std::ffi::CString::new(path.as_os_str().as_encoded_bytes())
        .expect("scratch paths hold no NUL");
    // SAFETY: `name` is a NUL-terminated path.
    if unsafe { libc::mkfifo(name.as_ptr(), 0o600) } != 0 {
        return Err(failed(std::io::Error::last_os_error()));
    }
    let opened = (|| {
        // Opening for reading and writing never waits for a peer, and keeps
        // a writer open so reads never see the end of the file.
        let writer = File::options().read(true).write(true).open(path)?;
        let child_reader = File::open(path)?;
        let reader = File::options()
            .read(true)
            .custom_flags(libc::O_NONBLOCK)
            .open(path)?;
        Ok(Account {
            reader,
            writer,
            child_reader,
            deposited: 0,
            held: 0,
            testing: false,
        })
    })();
    let removed = std::fs::remove_file(path);
    let account = opened.map_err(failed)?;
    removed.map_err(failed)?;
    Ok(account)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::snapshot::tests::TempDir;

    /// A ledger without its dealer thread, dealt by hand.
    fn ledger(jobs: usize, test_jobs: usize, scratch: &Path) -> Ledger {
        Ledger {
            jobs,
            test_jobs,
            scratch: scratch.to_owned(),
            next: 0,
            accounts: BTreeMap::new(),
            turn: 0,
        }
    }

    fn open(ledger: &mut Ledger) -> u64 {
        let id = ledger.next;
        ledger.next += 1;
        let account = fifo(&ledger.scratch.join(format!("jobs-{id}"))).unwrap();
        ledger.accounts.insert(id, account);
        id
    }

    /// A process of build `id` takes a token, as a jobserver client does.
    fn take(ledger: &Ledger, id: u64) {
        assert!(ledger.accounts[&id].waiting() > 0, "a token waits for {id}");
        let mut byte = [0];
        (&ledger.accounts[&id].child_reader)
            .read_exact(&mut byte)
            .unwrap();
    }

    /// A process of build `id` gives a token back.
    fn give_back(ledger: &Ledger, id: u64) {
        (&ledger.accounts[&id].writer).write_all(b"|").unwrap();
    }

    #[test]
    fn every_build_gets_a_token_and_builds_that_take_more_get_more() {
        let scratch = TempDir::new();
        let mut ledger = ledger(6, 3, &scratch.0);
        let (a, b) = (open(&mut ledger), open(&mut ledger));
        ledger.deal();
        // Each build is charged its implicit token and one waiting token.
        assert_eq!(ledger.free(), 2);
        assert_eq!(ledger.accounts[&a].waiting(), 1);
        // A takes its token and gets another; B's waiting one is dealt again.
        take(&ledger, a);
        ledger.deal();
        assert_eq!(ledger.accounts[&a].held, 1);
        assert_eq!(ledger.accounts[&a].waiting(), 1);
        assert_eq!(ledger.accounts[&b].waiting(), 1);
        assert_eq!(ledger.free(), 1);
        assert_eq!(
            ledger.accounts[&a].charge(3) + ledger.accounts[&b].charge(3),
            5
        );
        // No scratch file stays behind.
        assert_eq!(std::fs::read_dir(&scratch.0).unwrap().count(), 0);
    }

    #[test]
    fn a_closed_account_returns_whatever_its_processes_held() {
        let scratch = TempDir::new();
        let mut ledger = ledger(4, 2, &scratch.0);
        let a = open(&mut ledger);
        for _ in 0..3 {
            ledger.deal();
            take(&ledger, a);
        }
        ledger.deal();
        assert_eq!(
            ledger.free(),
            0,
            "a holds every job: {}",
            ledger.accounts[&a].held
        );
        // The build is killed without giving anything back.
        ledger.accounts.remove(&a);
        let b = open(&mut ledger);
        ledger.deal();
        assert_eq!(ledger.free(), 2);
        assert_eq!(ledger.accounts[&b].waiting(), 1);
    }

    #[test]
    fn tokens_given_back_go_to_whoever_needs_them_and_tests_take_their_share() {
        let scratch = TempDir::new();
        let mut ledger = ledger(5, 3, &scratch.0);
        let (a, b) = (open(&mut ledger), open(&mut ledger));
        ledger.deal();
        take(&ledger, a);
        ledger.deal();
        take(&ledger, a);
        give_back(&ledger, a);
        ledger.deal();
        assert_eq!(ledger.accounts[&a].held, 1);
        // B's tests run: it is charged three, A keeps only what it holds.
        ledger.accounts.get_mut(&b).unwrap().testing = true;
        ledger.deal();
        assert_eq!(ledger.accounts[&b].charge(3), 3);
        assert_eq!(ledger.accounts[&b].waiting(), 0, "tests take no tokens");
        assert_eq!(ledger.accounts[&a].charge(3), 2);
        assert_eq!(ledger.accounts[&a].waiting(), 0, "the budget is spent");
        assert_eq!(ledger.free(), 0);
    }
}
