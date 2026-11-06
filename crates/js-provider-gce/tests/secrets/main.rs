//! No key, token, assertion or payload reaches a debug text, an error or a log line.
//!
//! A binary of its own: its one test captures log lines through a subscriber scoped to its
//! thread, and reads them back without the lines of any other test mixed in. The scripted
//! transport installs a global subscriber that writes nothing; the scoped one takes the
//! events of this thread.

// The scripted transport the `http` binary shares; this binary uses only part of it.
#[allow(dead_code)]
#[path = "../http/script.rs"]
mod script;
mod secrets;
