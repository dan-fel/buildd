//! buildd coordinates the Cargo builds of many concurrent sessions on one
//! machine.
//!
//! Sessions (agents, editors, people) work in their own git worktrees and
//! never build there. They ask the daemon instead. The daemon snapshots the
//! worktree as a git tree, checks that tree out into one of a few **build
//! slots** (a checkout at a fixed path with the only target directory that
//! path ever uses), and runs Cargo there under one CPU budget shared by every
//! build. Equal requests share one build, a newer request from a worktree
//! replaces its queued older ones, and a build nobody waits for any more is
//! cancelled.
//!
//! Build disk is therefore a function of the slot count, not of the number of
//! sessions or worktrees, and a slot's Cargo fingerprints stay valid because
//! its path never changes.

mod activity;
mod budget;
mod build_log;
pub mod cache;
pub mod cargo;
pub mod client;
pub mod config;
pub mod daemon;
mod distance;
mod failures;
mod git;
pub mod log;
mod memory;
mod passed;
mod products;
pub mod protocol;
mod remote;
mod scheduler;
mod slot;
pub mod snapshot;
