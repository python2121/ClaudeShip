//! The `claudeship` binary: the hub, the command that runs Claude through
//! it, and the hub's internal entry points. See docs/hub.md and
//! docs/rust-core-plan.md.
//!
//! No argument parser: every argument that isn't `hub …` belongs to claude
//! and is passed on untouched, non-UTF-8 included.

mod approvals;
mod cli;
mod config;
mod frame;
mod hook;
mod hub;
mod local;
mod net;
mod paths;
mod procs;
mod pty;
mod service;
mod session;
mod supervisor;
mod swarm;
mod term;
mod token;
mod web;

fn main() {
    let args: Vec<std::ffi::OsString> = std::env::args_os().skip(1).collect();
    if args.first().is_some_and(|a| a == "permission-hook") {
        // Claude Code's PermissionRequest hook: never a decision it wasn't
        // given (see hook.rs).
        hook::run_helper();
    }
    cli::run(args);
}
