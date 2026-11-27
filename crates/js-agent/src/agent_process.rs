//! The id of this agent process: sent in every hello it says
//! ([`js_agent_wire::Hello::agent_process`]), so a server can tell the same process saying hello
//! again after it lost its session from another process claiming the host.
//!
//! A process draws its id when it starts ([`draw`]), unless the process before it on the same
//! state root stopped (a hand-over or a drain at a stop request) and left its id there, no longer
//! ago than the grace of its session (`lifecycle::handover`, the process file): then it takes
//! that id (`at_start`) and says hello as that process, so the server takes the host over at once
//! instead of refusing the hello `host-live` until the old session's grace runs out. A process
//! leaves its id only as it ends, so the session of that id has no process left to send for it
//! when the next one says hello; and past the grace the server holds that session live no more,
//! so an id left longer ago is not taken.

use std::io::Write;
use std::path::Path;
use std::time::{Duration, SystemTime};

use crate::lifecycle::handover::{self, Left};

/// The bytes of randomness behind an id: 32 lowercase hex characters, within
/// [`js_agent_wire::limits::MAX_ID_LEN`].
const BYTES: usize = 16;

/// A new id: 32 lowercase hex characters of random bytes, or `None` when the system's
/// randomness cannot be read. An agent without an id sends none and waits out the grace of a
/// live session like an agent from before the field: a fixed fallback id would let two
/// processes claim to be one and take each other's host over.
#[must_use]
pub fn draw() -> Option<String> {
    let mut bytes = [0_u8; BYTES];
    of(getrandom::getrandom(&mut bytes), &bytes)
}

/// The id of `bytes` when the read that filled them succeeded.
fn of<E>(read: Result<(), E>, bytes: &[u8; BYTES]) -> Option<String> {
    read.ok().map(|()| hex::encode(bytes))
}

/// The id of a process that starts with jobs over the state root `root`: the one the process
/// that stopped before it left there, taken (read and removed), or a new one ([`draw`]) when
/// there is none, none can be used, or there is no state root. `err` is told which, but for a
/// start with no id left.
pub(crate) fn at_start(root: Option<&Path>, err: &mut dyn Write) -> Option<String> {
    let owner = rustix::process::geteuid().as_raw();
    let left = root.map_or(Left::Absent, |root| handover::take_process(root, owner));
    of_left(left, SystemTime::now(), err, draw)
}

/// [`at_start`] of what the state root held, as of `now`, with the draw given.
fn of_left(
    left: Left,
    now: SystemTime,
    err: &mut dyn Write,
    draw: fn() -> Option<String>,
) -> Option<String> {
    let why = match left {
        Left::Absent => return draw(),
        Left::Kept { text, written } => match usable(&text, written, now) {
            Ok(id) => {
                let _ = writeln!(
                    err,
                    "js-agent: says hello as the agent process that stopped before it"
                );
                return Some(id);
            }
            Err(why) => why,
        },
        Left::Refused(why) => why,
    };
    let _ = writeln!(
        err,
        "js-agent: warning: the agent process id the last stop left is not used ({why}): the \
         hello waits out the server's grace for the last session"
    );
    draw()
}

/// The id of the process file's `text`, written at `written`, when it can be used at `now`:
/// an id of the drawn form on its first line, and on the second the grace in milliseconds of
/// the session that left it, which has not passed since. The error says why not.
fn usable(text: &str, written: Option<SystemTime>, now: SystemTime) -> Result<String, String> {
    let (id, grace) = text.split_once('\n').unwrap_or((text, ""));
    if !drawn(id) {
        return Err("its id is not 32 lowercase hex characters".to_owned());
    }
    let grace: u64 = grace
        .parse()
        .map_err(|_| "it names no grace in milliseconds".to_owned())?;
    let age = written.and_then(|at| now.duration_since(at).ok());
    if age.is_none_or(|age| age > Duration::from_millis(grace)) {
        return Err(format!(
            "it was not left within the session's grace of {grace} ms"
        ));
    }
    Ok(id.to_owned())
}

/// Whether `id` is of the form [`draw`] gives: 32 lowercase hex characters.
fn drawn(id: &str) -> bool {
    let hex = |b: u8| matches!(b, b'0'..=b'9' | b'a'..=b'f');
    id.len() == 2 * BYTES && id.bytes().all(hex)
}

#[cfg(test)]
mod tests {
    use std::time::{Duration, SystemTime};

    use super::{BYTES, at_start, draw, drawn, of, of_left};
    use crate::lifecycle::handover::{self, Left};
    use js_agent_wire::limits::MAX_ID_LEN;
    use js_testkit::dir::TestDir;

    const LEFT: &str = "0123456789abcdef0123456789abcdef";

    /// The time the tests take as now.
    fn now() -> SystemTime {
        SystemTime::UNIX_EPOCH + Duration::from_secs(1_800_000_000)
    }

    /// The draw of a test: an id no left one is.
    fn drew() -> Option<String> {
        Some("drawn".to_owned())
    }

    /// A process file of `text`, written `ago` before [`now`].
    fn kept(text: &str, ago: Duration) -> Left {
        Left::Kept {
            text: text.to_owned(),
            written: Some(now() - ago),
        }
    }

    fn of_left_said(left: Left) -> (Option<String>, String) {
        let mut err = Vec::new();
        let id = of_left(left, now(), &mut err, drew);
        (id, String::from_utf8(err).unwrap())
    }

    /// The warning of a left id that is not used, for `why`.
    fn warning(why: &str) -> String {
        format!(
            "js-agent: warning: the agent process id the last stop left is not used ({why}): \
             the hello waits out the server's grace for the last session\n"
        )
    }

    const SAID: &str = "js-agent: says hello as the agent process that stopped before it\n";

    /// Catches an id the wire refuses (so every hello would be unusable): it is 32 lowercase
    /// hex characters, inside the cap, with a leading alphanumeric.
    #[test]
    fn a_drawn_id_is_lowercase_hex_within_the_id_cap() {
        let id = draw().expect("the system's randomness reads");
        assert_eq!(id.len(), 32);
        assert!(id.len() <= MAX_ID_LEN);
        assert!(id.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f')));
    }

    /// Catches an id that is a constant: two draws of one process differ, so two agent
    /// processes do not share an id.
    #[test]
    fn two_draws_differ() {
        assert_ne!(draw(), draw());
    }

    /// Catches a fallback id drawn when the randomness could not be read: every agent that
    /// failed the read would share it and could take another's live host over.
    #[test]
    fn an_unread_randomness_gives_no_id() {
        assert_eq!(of(Err(()), &[0; BYTES]), None);
        assert_eq!(
            of(Ok::<(), ()>(()), &[0xab; BYTES]).as_deref(),
            Some("ab".repeat(16).as_str())
        );
    }

    /// Proves the id the last stop left within its grace is the one the next process says
    /// hello as, and that it says so, up to the grace itself; and that with none left the
    /// process draws its own and says nothing. Catches a next process that draws a new id
    /// after a stop, which the server refuses `host-live` until the last session's grace runs
    /// out.
    #[test]
    fn a_left_id_within_its_grace_is_taken_and_none_left_is_drawn() {
        let text = format!("{LEFT}\n15000");
        for ago in [
            Duration::ZERO,
            Duration::from_secs(9),
            Duration::from_secs(15),
        ] {
            assert_eq!(
                of_left_said(kept(&text, ago)),
                (Some(LEFT.to_owned()), SAID.to_owned()),
                "{ago:?}"
            );
        }
        assert_eq!(
            of_left_said(Left::Absent),
            (Some("drawn".to_owned()), String::new())
        );
    }

    /// Proves an id left longer ago than its grace, at a time that cannot be read, or later
    /// than now (a clock set back) is not used: the process draws and says why. Catches a state
    /// root kept or copied for longer than the grace whose id takes a live host over.
    #[test]
    fn a_left_id_past_its_grace_or_of_no_known_time_is_drawn_again() {
        let text = format!("{LEFT}\n15000");
        let stale = warning("it was not left within the session's grace of 15000 ms");
        let past = Duration::from_millis(15_001);
        assert_eq!(
            of_left_said(kept(&text, past)),
            (Some("drawn".to_owned()), stale.clone())
        );
        let unknown = Left::Kept {
            text: text.clone(),
            written: None,
        };
        assert_eq!(
            of_left_said(unknown),
            (Some("drawn".to_owned()), stale.clone())
        );
        let later = Left::Kept {
            text,
            written: Some(now() + Duration::from_secs(1)),
        };
        assert_eq!(of_left_said(later), (Some("drawn".to_owned()), stale));
    }

    /// Proves a left id that is not of the drawn form, a file that names no grace, and a
    /// process file that was not used give a drawn id and a warning that says why, in full.
    /// Catches a hello sent with an id the wire refuses (every hello would be unusable) and a
    /// refusal that is not told.
    #[test]
    fn a_left_id_that_cannot_be_used_is_drawn_again_with_a_warning() {
        let unfit = warning("its id is not 32 lowercase hex characters");
        let bads: [String; 5] = [
            String::new(),
            "0123456789abcdef".to_owned(),
            LEFT.to_uppercase(),
            format!("{LEFT}0"),
            LEFT.replace('a', "-"),
        ];
        for bad in &bads {
            assert!(!drawn(bad), "{bad:?}");
            let text = format!("{bad}\n15000");
            assert_eq!(
                of_left_said(kept(&text, Duration::ZERO)),
                (Some("drawn".to_owned()), unfit.clone()),
                "{bad:?}"
            );
        }
        assert!(drawn(LEFT));
        assert!(drawn(&draw().expect("the system's randomness reads")));
        let graceless = warning("it names no grace in milliseconds");
        for text in [LEFT.to_owned(), format!("{LEFT}\nsoon")] {
            assert_eq!(
                of_left_said(kept(&text, Duration::ZERO)),
                (Some("drawn".to_owned()), graceless.clone()),
                "{text:?}"
            );
        }
        assert_eq!(
            of_left_said(Left::Refused("it is owned by user 1".to_owned())),
            (Some("drawn".to_owned()), warning("it is owned by user 1"))
        );
    }

    /// Proves a start takes the id its state root holds, written just now, and removes the
    /// file; that the next start draws; and that a start with no state root (no jobs) draws.
    /// Catches a start that leaves the file for a later one, or reads it from somewhere else.
    #[test]
    fn a_start_takes_the_id_from_its_state_root() {
        let dir = TestDir::under(std::env::temp_dir(), "agent-process-start");
        handover::leave_process(dir.path(), LEFT, Duration::from_secs(3600)).unwrap();
        let mut err = Vec::new();
        assert_eq!(at_start(Some(dir.path()), &mut err).as_deref(), Some(LEFT));
        assert!(!dir.path().join(handover::PROCESS_FILE).exists());
        let drawn_now = at_start(Some(dir.path()), &mut err).expect("drawn");
        assert_ne!(drawn_now, LEFT);
        let without = at_start(None, &mut err).expect("drawn");
        assert!(drawn(&without));
        assert_eq!(String::from_utf8(err).unwrap(), SAID);
    }
}
