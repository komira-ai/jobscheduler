//! No key, token or signature reaches a debug text, an error or a log line.
//!
//! A binary of its own: its one test captures log lines through a subscriber scoped to its
//! thread, and in a process where no other test runs, no other thread can hit the client's
//! log callsites first and leave them cached as of interest to no subscriber.

// The scripted transport the `http` binary shares; this binary uses only part of it.
#[allow(dead_code)]
#[path = "../http/script.rs"]
mod script;
mod secrets;
