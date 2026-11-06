//! The bounds a hook's waits are built from, in seconds. A hook holds its declared step
//! timeouts to them, so that the updater never kills a step in the middle of a wait.

/// The time between two looks at the host while a step waits for it to hold no lease.
pub const POLL_SECS: u64 = 10;

/// How much longer than the drain wait the declared `prepare` timeout must be, in seconds: the
/// version check, the cordon calls and the record writes happen around the wait.
pub const PREPARE_MARGIN_SECS: u64 = 60;

/// How long a started unit has to become active, in seconds.
pub const START_WAIT_SECS: u64 = 120;

/// The margin `cutover` keeps between the end of its wait for the stop and the declared
/// timeout, besides the start wait, in seconds.
pub const CUTOVER_MARGIN_SECS: u64 = 60;

/// The margin `verify` keeps between the end of its wait and the declared timeout, in seconds.
pub const VERIFY_MARGIN_SECS: u64 = 15;

/// The margin `rollback` keeps between the end of its waits and the declared timeout, in
/// seconds.
pub const ROLLBACK_MARGIN_SECS: u64 = 60;
