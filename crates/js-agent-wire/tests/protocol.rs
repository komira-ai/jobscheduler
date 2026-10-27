//! The protocol version rule: servers upgrade before agents, a server refuses a hello
//! whose majors it does not speak, and within a major fields are additive.

mod common;

use common::{expect, hello};
use js_agent_wire::refusal;
use js_agent_wire::{PROTOCOL_MAJOR, PROTOCOL_NAME, ProtocolRange, Rule};

/// Asserts the negotiation matrix: the newest shared major is chosen, ranges that do not
/// meet choose nothing, and the chosen major is the same from either side. It catches
/// choosing the agent's newest when the server cannot speak it.
#[test]
fn negotiation_picks_the_newest_shared_major() {
    let range = |min, max| ProtocolRange { min, max };
    let cases = [
        (range(1, 1), range(1, 1), Some(1)),
        (range(1, 5), range(1, 1), Some(1)),
        (range(1, 3), range(2, 9), Some(3)),
        (range(2, 3), range(1, 1), None),
        (range(1, 1), range(2, 3), None),
        (range(3, 3), range(1, 2), None),
    ];
    for (agent, server, chosen) in cases {
        assert_eq!(agent.negotiate(&server), chosen, "{agent:?} {server:?}");
        assert_eq!(server.negotiate(&agent), chosen, "{server:?} {agent:?}");
    }
    assert_eq!(ProtocolRange::THIS, range(1, PROTOCOL_MAJOR));
    assert_eq!(PROTOCOL_NAME, "jobscheduler.agent.v1");
}

/// A hello from a newer agent that still speaks major 1 is answered with 1, one that
/// speaks only newer majors is refused with the protocol code, and one with a broken
/// range is a malformed hello. It catches a server admitting an agent it cannot speak
/// to, and the refusal being indistinguishable from a bad hello.
#[test]
fn a_server_refuses_a_hello_it_cannot_speak() {
    let mut newer = hello();
    newer.protocol = ProtocolRange { min: 1, max: 4 };
    assert_eq!(newer.negotiate(), Ok(1));
    assert_eq!(newer.validate(), Ok(()));
    newer.protocol = ProtocolRange { min: 2, max: 4 };
    let err = newer.negotiate().unwrap_err();
    expect(
        err,
        "protocol",
        Rule::UnsupportedProtocol,
        refusal::UNSUPPORTED_PROTOCOL,
    );
    assert_eq!(newer.validate(), Err(err));
    newer.protocol = ProtocolRange { min: 0, max: 0 };
    assert_eq!(
        newer.negotiate().unwrap_err().code(),
        refusal::INVALID_HELLO
    );
}
