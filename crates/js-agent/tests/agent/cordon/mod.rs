//! `js-agent cordon`: the window logic over a fake lease server and a clock moved by hand,
//! the verb's flags and exit statuses, the HTTP client and the verb against a real lease
//! server in the test process, and the program itself with a real SIGTERM. No test waits for
//! time: the fake pace moves its own clock, and the one wait on a child process is for a line
//! it prints, with a hang guard.

mod binary;
mod fakes;
mod hold;
mod http;
mod verb;
