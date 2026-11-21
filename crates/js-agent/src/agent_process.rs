//! The id of this agent process: drawn once when the process starts and sent in every hello it
//! says ([`js_agent_wire::Hello::agent_process`]), so a server can tell the same process
//! saying hello again after it lost its session from another process claiming the host.

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

#[cfg(test)]
mod tests {
    use super::{BYTES, draw, of};
    use js_agent_wire::limits::MAX_ID_LEN;

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
}
