//! Behaviour of the host agent: what it detects, what it says to the server, how it
//! answers each refusal, and the whole path against a lease server started in the test
//! process. Time, randomness, signals, programs and files are all injected, so no test
//! waits for a clock or sends a signal to its own process. The two exceptions are real
//! children: the binary test, and the tests of the probe runner's bounds, which start a
//! process that never exits, let the runner give up on it and assert on the outcome (the
//! error, and that the process is gone), never on how long it took. Every wait of a test
//! on another thread or process has a hang guard that fails the test with a message.

mod backoff;
mod binary;
mod cli_runtime;
mod container;
mod drive;
mod e2e;
mod exclusive;
mod facts;
mod flags;
mod integrity;
mod jobs;
mod lifecycle;
mod readiness;
mod server;
mod session;
mod support;
mod usage;
