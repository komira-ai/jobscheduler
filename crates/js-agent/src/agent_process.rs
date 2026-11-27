//! The id of this agent process: sent in every hello it says
//! ([`js_agent_wire::Hello::agent_process`]), so a server can tell the same process saying hello
//! again after it lost its session from another process claiming the host.
//!
//! A process draws its id when it starts ([`draw`]), unless the process before it handed its
//! runs over to it and left its id in the state root (`lifecycle::handover`, the process file):
//! then it takes that id (`at_start`) and says hello as that process, so the server takes the
//! host over at once instead of refusing the hello `host-live` until the old session's grace
//! runs out. A process leaves its id only as it ends, having handed its runs over, so the
//! session of that id has no process left to send for it when the next one says hello.

use std::io::Write;
use std::path::Path;

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
/// that handed its runs over left there, taken (read and removed), or a new one ([`draw`]) when
/// there is none, none can be used, or there is no state root. `err` is told which, but for a
/// start with no id left.
pub(crate) fn at_start(root: Option<&Path>, err: &mut dyn Write) -> Option<String> {
    let owner = rustix::process::geteuid().as_raw();
    let left = root.map_or(Left::Absent, |root| handover::take_process(root, owner));
    of_left(left, err, draw)
}

/// [`at_start`] of what the state root held, with the draw given.
fn of_left(left: Left, err: &mut dyn Write, draw: fn() -> Option<String>) -> Option<String> {
    let why = match left {
        Left::Absent => return draw(),
        Left::Kept(id) if drawn(&id) => {
            let _ = writeln!(
                err,
                "js-agent: says hello as the agent process that handed its runs over"
            );
            return Some(id);
        }
        Left::Kept(_) => "it is not 32 lowercase hex characters".to_owned(),
        Left::Refused(why) => why,
    };
    let _ = writeln!(
        err,
        "js-agent: warning: the agent process id the last hand-over left is not used ({why}): \
         the hello waits out the server's grace for the last session"
    );
    draw()
}

/// Whether `id` is of the form [`draw`] gives: 32 lowercase hex characters.
fn drawn(id: &str) -> bool {
    let hex = |b: u8| matches!(b, b'0'..=b'9' | b'a'..=b'f');
    id.len() == 2 * BYTES && id.bytes().all(hex)
}

#[cfg(test)]
mod tests {
    use super::{BYTES, at_start, draw, drawn, of, of_left};
    use crate::lifecycle::handover::{self, Left};
    use js_agent_wire::limits::MAX_ID_LEN;
    use js_testkit::dir::TestDir;

    const LEFT: &str = "0123456789abcdef0123456789abcdef";

    /// The draw of a test: an id no left one is.
    fn drew() -> Option<String> {
        Some("drawn".to_owned())
    }

    fn of_left_said(left: Left) -> (Option<String>, String) {
        let mut err = Vec::new();
        let id = of_left(left, &mut err, drew);
        (id, String::from_utf8(err).unwrap())
    }

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

    /// Proves the id a hand-over left is the one the next process says hello as, and that it
    /// says so; and that with none left the process draws its own and says nothing. Catches a
    /// next process that draws a new id after a hand-over, which the server refuses `host-live`
    /// until the last session's grace runs out.
    #[test]
    fn a_left_id_is_taken_and_none_left_is_drawn() {
        assert_eq!(
            of_left_said(Left::Kept(LEFT.to_owned())),
            (
                Some(LEFT.to_owned()),
                "js-agent: says hello as the agent process that handed its runs over\n".to_owned()
            )
        );
        assert_eq!(
            of_left_said(Left::Absent),
            (Some("drawn".to_owned()), String::new())
        );
    }

    /// Proves a left id that is not of the drawn form, and a process file that was not used,
    /// give a drawn id and a warning that says why, in full. Catches a hello sent with an id the
    /// wire refuses (every hello would be unusable) and a refusal that is not told.
    #[test]
    fn a_left_id_that_cannot_be_used_is_drawn_again_with_a_warning() {
        let warning = |why: &str| {
            format!(
                "js-agent: warning: the agent process id the last hand-over left is not used \
                 ({why}): the hello waits out the server's grace for the last session\n"
            )
        };
        let unfit = "it is not 32 lowercase hex characters";
        let bads: [String; 5] = [
            String::new(),
            "0123456789abcdef".to_owned(),
            LEFT.to_uppercase(),
            format!("{LEFT}0"),
            LEFT.replace('a', "-"),
        ];
        for bad in &bads {
            assert!(!drawn(bad), "{bad:?}");
            assert_eq!(
                of_left_said(Left::Kept(bad.to_owned())),
                (Some("drawn".to_owned()), warning(unfit)),
                "{bad:?}"
            );
        }
        assert!(drawn(LEFT));
        assert!(drawn(&draw().expect("the system's randomness reads")));
        assert_eq!(
            of_left_said(Left::Refused("it is owned by user 1".to_owned())),
            (Some("drawn".to_owned()), warning("it is owned by user 1"))
        );
    }

    /// Proves a start takes the id from its state root and removes the file, and that a start
    /// with no state root (no jobs) draws one. Catches a start that leaves the file for a later
    /// one, or reads it from somewhere else.
    #[test]
    fn a_start_takes_the_id_from_its_state_root() {
        let dir = TestDir::under(std::env::temp_dir(), "agent-process-start");
        handover::leave_process(dir.path(), LEFT).unwrap();
        let mut err = Vec::new();
        assert_eq!(at_start(Some(dir.path()), &mut err).as_deref(), Some(LEFT));
        assert!(!dir.path().join(handover::PROCESS_FILE).exists());
        let drawn_now = at_start(Some(dir.path()), &mut err).expect("drawn");
        assert_ne!(drawn_now, LEFT);
        let without = at_start(None, &mut err).expect("drawn");
        assert!(drawn(&without));
        assert_eq!(
            String::from_utf8(err).unwrap(),
            "js-agent: says hello as the agent process that handed its runs over\n"
        );
    }
}
