//! A daemon on Windows builds for other machines' daemons, which reach it
//! through `buildd serve`, but it does not send builds on to other machines:
//! each exchange here would need pipes it can wait on alongside its client.

use std::path::{Path, PathBuf};
use std::process::Child;
use std::sync::{Arc, Mutex};

use crate::client::CacheError;
use crate::config::Remote;
use crate::protocol::{Activity, Capacity, Failures, Forecast, Message, RevisionRequest};
use crate::snapshot::{Revision, Source};
use crate::socket::UnixStream;

fn refused(remote: &Remote) -> String {
    format!(
        "{}: a buildd daemon on Windows does not send builds to other machines",
        remote.name
    )
}

pub(crate) fn kill_session(child: &Child) {
    let _ = child;
    unreachable!("a daemon on Windows starts no SSH session");
}

pub(crate) fn cache(
    _home: &Path,
    remote: &Remote,
    _operation: crate::cache::Operation,
    _requester: &UnixStream,
) -> Result<crate::cache::Response, CacheError> {
    Err(CacheError::Transport(refused(remote)))
}

pub(crate) fn activity(_home: &Path, remote: &Remote) -> Result<Activity, String> {
    Err(refused(remote))
}

pub(crate) fn capacity(
    remote: &Remote,
    _project: String,
    _prefix: PathBuf,
    _operation: crate::cargo::Operation,
) -> Result<Capacity, String> {
    Err(refused(remote))
}

pub(crate) fn forecast(
    remote: &Remote,
    _request: RevisionRequest,
    _requester: &UnixStream,
) -> Result<Forecast, String> {
    Err(refused(remote))
}

pub(crate) fn failures(_home: &Path, remote: &Remote, _hours: u64) -> Result<Failures, String> {
    Err(refused(remote))
}

pub(crate) fn build(
    remote: &Remote,
    _source: &Source,
    _revision: &Revision,
    _request: RevisionRequest,
    _requester: &UnixStream,
    _started: impl Fn(&Arc<Mutex<Child>>),
    _pass: impl FnMut(&Message) -> bool,
) -> Result<(), String> {
    Err(refused(remote))
}
