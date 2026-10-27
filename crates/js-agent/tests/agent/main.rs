//! Behaviour of the host agent: what it detects, what it says to the server, how it
//! answers each refusal, and the whole path against a lease server started in the test
//! process. Time, randomness, signals, programs and files are all injected, so no test
//! waits for a clock or sends a signal to its own process.

mod backoff;
mod binary;
mod drive;
mod e2e;
mod facts;
mod flags;
mod server;
mod session;
mod support;
