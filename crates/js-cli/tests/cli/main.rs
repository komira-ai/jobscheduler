//! Behaviour of `js` against a real lease server started in the test process on
//! 127.0.0.1: every subcommand and exit status, the pending poll and the renewal loop
//! driven through the injected waiter and clock, TLS, the refusals before any request,
//! lost replies, the processes a command leaves behind, and the program itself run as a
//! child process.

mod acquire;
mod binary;
mod failures;
mod flags;
mod lost_reply;
mod ops;
mod procs;
mod release;
mod run;
// These tests watch a process end through a pidfd, which only Linux has.
#[cfg(target_os = "linux")]
mod run_group;
mod run_renew;
mod servers;
mod support;
mod tls;
