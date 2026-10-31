//! Behaviour of the host agent: what it detects, what it says to the server, how it
//! answers each refusal, and the whole path against a lease server started in the test
//! process. Time, randomness, signals, programs and files are all injected, so no test
//! waits for a clock or sends a signal to its own process. The two exceptions are real
//! children: the binary test, and the tests of the probe runner's bounds, which start a
//! process that never exits, let the runner give up on it and assert on the outcome (the
//! error, and that the process is gone), never on how long it took. Every wait of a test
//! on another thread or process has a hang guard that fails the test with a message, and
//! every loop a test sleeper ends has a bound (`bounds`): a sleeper is woken at most so
//! many times, a scripted transport and a captured log hold at most so much, and the binary
//! has a limit on its address space and its running time.

mod backoff;
mod binary;
mod bounds;
mod bundle;
mod cli_runtime;
mod container;
mod credential;
mod credential_e2e;
#[cfg(target_os = "linux")]
mod crun_host;
mod drive;
mod e2e;
mod exclusive;
mod facts;
mod flags;
#[cfg(target_os = "linux")]
mod helper_modes;
mod integrity;
mod jobs;
mod lifecycle;
mod quota;
mod readiness;
mod runtime_conformance;
#[path = "../runtime_suite/mod.rs"]
mod runtime_suite;
mod server;
mod session;
mod support;
mod usage;
