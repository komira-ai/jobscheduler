//! The memory a reply decode holds is bounded by a multiple of the body, not by the
//! size of a generic JSON tree per entry.
//!
//! This file holds one test, so no other test of the process allocates while the limit is
//! set. The allocator is `cap::Cap` over the system allocator: during the decode its limit
//! is the bytes live before it plus [`PEAK_PER_BODY_BYTE`] per byte of body, and a decode
//! that would hold more fails an allocation, which the default allocation-error handler
//! turns into an abort of the test binary.

use js_agent_wire::limits::REPLY_BODY_BUDGET;
use js_agent_wire::{Command, HeartbeatReply};

#[global_allocator]
static ALLOC: cap::Cap<std::alloc::System> = cap::Cap::new(std::alloc::System, usize::MAX);

/// The most live bytes above the body that a decode may hold, per byte of body.
const PEAK_PER_BODY_BYTE: usize = 16;

/// Asserts that a reply of the largest size a transport reads, made of commands of the
/// smallest text each kind has, decodes holding at most [`PEAK_PER_BODY_BYTE`] bytes per
/// byte of body (past that the process aborts), and that the decoded commands are the ones
/// the body lists. It catches a
/// decode that keeps a generic JSON tree for every command until the whole list is read,
/// which holds some fifty bytes per byte of an unknown command's text.
#[test]
fn reply_decode_holds_a_bounded_multiple_of_the_body() {
    let unit = r#"{"type":"x"}"#;
    let mut body = String::from(r#"{"commands":["#);
    while body.len() + 2 * unit.len() + 3 <= REPLY_BODY_BUDGET {
        body.push_str(unit);
        body.push(',');
    }
    body.push_str(unit);
    body.push_str("]}");
    assert!(body.len() <= REPLY_BODY_BUDGET);

    ALLOC
        .set_limit(ALLOC.allocated() + PEAK_PER_BODY_BYTE * body.len())
        .expect("the limit is above what is live");
    let reply: HeartbeatReply = serde_json::from_slice(body.as_bytes()).expect("the reply decodes");
    ALLOC
        .set_limit(usize::MAX)
        .expect("the limit is above what is live");

    assert!(reply.commands.iter().all(|c| *c == Command::Unknown));
    assert_eq!(reply.commands.len(), body.matches(unit).count());
}
