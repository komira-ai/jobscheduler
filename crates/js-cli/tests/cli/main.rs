//! Behaviour of `js` against a real lease server started in the test process on
//! 127.0.0.1: every subcommand and exit status, the pending poll and the renewal loop
//! driven through the injected waiter, TLS, the refusals before any request, and the
//! program itself run as a child process.

mod acquire;
mod binary;
mod failures;
mod flags;
mod ops;
mod run;
mod support;
mod tls;
