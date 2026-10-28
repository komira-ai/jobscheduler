//! The bounds that keep a test that goes wrong from taking the machine with it: a limit on
//! how many times a test sleeper may be woken, a cap on what a scripted transport and a
//! captured log may hold, and limits on the whole test binary (its address space and its
//! running time). A test that waits for the agent to end loops until the agent ends; when
//! the end is missed the loop has no other limit, and each pass adds to what the test
//! records, so the loop must be cut by the test.

use std::io::{self, Write};
use std::sync::Once;
use std::time::Duration;

/// How many times one test sleeper may be woken.
pub const WAKE_LIMIT: usize = 10_000;

/// How many messages one scripted transport may be sent.
pub const SENT_LIMIT: usize = 10_000;

/// How many bytes one captured log may hold.
pub const LOG_LIMIT: usize = 64 << 20;

/// The address space of the test process. Threads reserve address space they never touch,
/// so it is far above what a healthy run uses.
pub const ADDRESS_SPACE: u64 = 8 << 30;

/// How long the whole test binary may run before it is aborted.
pub const BINARY_BUDGET: Duration = Duration::from_secs(20 * 60);

/// Counts the wakes of a test sleeper and panics past a limit.
pub struct WakeBudget {
    limit: usize,
    wakes: usize,
}

impl WakeBudget {
    /// A budget of [`WAKE_LIMIT`] wakes. Also puts the process-wide limits in place.
    pub fn new() -> Self {
        Self::of(WAKE_LIMIT)
    }

    /// A budget of `limit` wakes.
    pub fn of(limit: usize) -> Self {
        install();
        Self { limit, wakes: 0 }
    }

    /// Counts one wake.
    pub fn spend(&mut self) {
        self.wakes += 1;
        assert!(
            self.wakes <= self.limit,
            "the sleeper was woken more than {} times: the loop it belongs to does not end",
            self.limit
        );
    }
}

/// A log buffer that panics when more than its limit is written to it.
pub struct Capped {
    limit: usize,
    bytes: Vec<u8>,
}

impl Capped {
    /// A buffer of at most [`LOG_LIMIT`] bytes.
    pub fn new() -> Self {
        Self::of(LOG_LIMIT)
    }

    /// A buffer of at most `limit` bytes.
    pub fn of(limit: usize) -> Self {
        Self {
            limit,
            bytes: Vec::new(),
        }
    }

    /// What was written, as text.
    pub fn text(self) -> String {
        String::from_utf8(self.bytes).unwrap()
    }
}

impl Write for Capped {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.bytes.extend_from_slice(buf);
        assert!(
            self.bytes.len() <= self.limit,
            "the log grew past {} bytes: the loop that writes it does not end",
            self.limit
        );
        Ok(buf.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

/// Puts the limits of the whole test binary in place, once: the address space, where the
/// platform has the limit, and a thread that aborts the process when the binary has run for
/// [`BINARY_BUDGET`]. The test harness has no hook that runs first, so the constructors every
/// test that can loop starts from call it: [`WakeBudget::of`], `Script::capped` and
/// `Scratch::new`.
pub fn install() {
    static ONCE: Once = Once::new();
    ONCE.call_once(|| {
        limit_address_space();
        std::thread::spawn(|| {
            std::thread::sleep(BINARY_BUDGET);
            eprintln!("the test binary ran for {BINARY_BUDGET:?}: aborting");
            std::process::abort();
        });
    });
}

#[cfg(target_os = "linux")]
fn limit_address_space() {
    use rustix::process::{Resource, Rlimit, getrlimit, setrlimit};
    let now = getrlimit(Resource::As);
    let ceiling = now
        .maximum
        .map_or(ADDRESS_SPACE, |max| max.min(ADDRESS_SPACE));
    let current = now.current.map_or(ceiling, |cur| cur.min(ceiling));
    let _ = setrlimit(
        Resource::As,
        Rlimit {
            current: Some(current),
            maximum: now.maximum,
        },
    );
}

#[cfg(not(target_os = "linux"))]
fn limit_address_space() {}

/// Asserts that a sleeper that is woken without end trips its budget with a message that
/// says so: the bound that cuts a loop that would otherwise spin until the machine is out
/// of memory. It does not depend on any fault being present.
#[test]
#[should_panic(expected = "the sleeper was woken more than 50 times")]
fn a_sleeper_that_is_woken_without_end_trips_its_budget() {
    let mut budget = WakeBudget::of(50);
    loop {
        budget.spend();
    }
}

/// Asserts that the budget allows exactly its limit of wakes and trips on the next.
#[test]
fn a_budget_allows_its_limit() {
    let mut budget = WakeBudget::of(3);
    (0..3).for_each(|_| budget.spend());
    let tripped = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| budget.spend()));
    assert!(tripped.is_err());
}

/// Asserts that a captured log holds up to its limit and panics past it, saying so.
#[test]
#[should_panic(expected = "the log grew past 8 bytes")]
fn a_log_that_grows_without_end_trips_its_cap() {
    let mut log = Capped::of(8);
    assert!(writeln!(log, "1234567").is_ok());
    assert_eq!(log.bytes.len(), 8);
    let _ = writeln!(log, "x");
}

/// Asserts that the scripted transport panics past its cap of messages sent, saying so.
#[test]
#[should_panic(expected = "was sent more than 3 messages")]
fn a_transport_that_is_sent_without_end_trips_its_cap() {
    use crate::support::Script;
    use js_agent::Transport;
    let (mut script, _) = Script::capped(vec![], 3);
    loop {
        let _ = script.post("/x", &serde_json::json!({}));
    }
}
