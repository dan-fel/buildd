//! Building new commits of chosen branches before anyone asks, so the
//! worktrees branching from them find warm slots.
//!
//! ```text
//! every minute, for each [[prewarm]] branch:
//!   commit moved? ──yes──▶ $BUILDD_HOME/prewarm/<project>, a worktree of the
//!                          repository, checked out at the new commit
//!                            └─▶ each configured build submitted to this
//!                                daemon as optional work
//! ```
//!
//! Optional work waits behind every other build and leaves a slot free; a
//! newer commit's builds replace queued older ones, as for any worktree.

use std::path::{Path, PathBuf};
use std::time::Duration;

use crate::config::{Prewarm, prewarm_operation};
use crate::git;
use crate::log::log;
use crate::protocol::{BuildRequest, Message};

/// How often branches are looked at.
const POLL: Duration = Duration::from_secs(60);

/// Follows `prewarms` on a thread of its own for as long as the daemon runs.
pub(crate) fn spawn(home: &Path, prewarms: Vec<Prewarm>) {
    if prewarms.is_empty() {
        return;
    }
    let home = home.to_owned();
    let spawned = std::thread::Builder::new()
        .name("buildd-prewarm".into())
        .spawn(move || {
            let mut seen = vec![None; prewarms.len()];
            loop {
                for (prewarm, seen) in prewarms.iter().zip(&mut seen) {
                    match follow(&home, prewarm, seen.as_deref()) {
                        Ok(commit) => *seen = Some(commit),
                        Err(error) => log!("prewarm of {}: {error}", prewarm.branch),
                    }
                }
                std::thread::sleep(POLL);
            }
        });
    if let Err(error) = spawned {
        log!("could not start prewarming: {error}");
    }
}

/// Builds `prewarm`'s branch if it moved from `seen`; returns its commit.
fn follow(home: &Path, prewarm: &Prewarm, seen: Option<&str>) -> Result<String, String> {
    let mut resolve = git::command(&prewarm.repository);
    resolve
        .args(["rev-parse", "--verify", "--quiet"])
        .arg(format!("refs/heads/{}^{{commit}}", prewarm.branch));
    let commit = git::run(resolve)?.trim().to_owned();
    if seen == Some(commit.as_str()) {
        return Ok(commit);
    }
    let source = crate::snapshot::resolve(&prewarm.repository)?;
    let worktree = home
        .join("prewarm")
        .join(crate::slot::project_name(&source.repository));
    check_out(&prewarm.repository, &worktree, &commit)?;
    log!(
        "prewarm: {} moved to {}; building it",
        prewarm.branch,
        &commit[..commit.len().min(10)]
    );
    for build in &prewarm.builds {
        submit(home, prewarm, &worktree, build)?;
    }
    Ok(commit)
}

/// Puts `worktree`, a worktree of `repository`, at `commit` with nothing
/// else in it.
fn check_out(repository: &Path, worktree: &Path, commit: &str) -> Result<(), String> {
    if worktree.join(".git").exists() {
        let mut checkout = git::command(worktree);
        checkout.args(["checkout", "--quiet", "--detach", "--force", commit]);
        git::run(checkout)?;
        let mut clean = git::command(worktree);
        clean.args(["clean", "--quiet", "-ffdx"]);
        git::run(clean)?;
    } else {
        let parent = worktree
            .parent()
            .expect("the prewarm worktree is under the home");
        std::fs::create_dir_all(parent)
            .map_err(|error| format!("could not create {}: {error}", parent.display()))?;
        let mut add = git::command(repository);
        add.args(["worktree", "add", "--quiet", "--detach", "--force"])
            .arg(worktree)
            .arg(commit);
        git::run(add)?;
    }
    Ok(())
}

/// Asks this daemon for `build` of `worktree` as optional work, on a thread
/// that waits for it and logs how it ended.
fn submit(home: &Path, prewarm: &Prewarm, worktree: &Path, build: &[String]) -> Result<(), String> {
    let operation = prewarm_operation(build)?;
    let request = BuildRequest {
        directory: worktree.to_owned(),
        operation,
        label: Some(format!("prewarm {}", prewarm.branch)),
        copy_to: None,
        rerun_all: false,
        os: prewarm.os.clone(),
        optional: true,
    };
    let home: PathBuf = home.to_owned();
    let described = build.join(" ");
    std::thread::Builder::new()
        .name("buildd-prewarm-build".into())
        .spawn(move || {
            let ended = crate::client::connect(&home)
                .map_err(|error| error.to_string())
                .and_then(|stream| crate::client::build(stream, request, |_| {}));
            match ended {
                Ok(Message::Finished { outcome, .. }) if outcome.success() => {}
                Ok(Message::Finished { outcome, .. }) => {
                    log!("prewarm build {described} ended: {outcome:?}");
                }
                Ok(other) => log!("prewarm build {described}: {other:?}"),
                Err(error) => log!("prewarm build {described}: {error}"),
            }
        })
        .map(drop)
        .map_err(|error| format!("could not start a prewarm build: {error}"))
}
