//! The heartbeat's `seq` and the hello's `agent_process`: two optional fields. Absent they
//! decode as `None` and are not written; present they round trip, a reader that does not know
//! them skips them, and a bad process id is refused.

mod common;

use common::{broken, expect, heartbeat, hello, long};
use js_agent_wire::limits::MAX_ID_LEN;
use js_agent_wire::refusal;
use js_agent_wire::{Heartbeat, Hello, Phase, Rule};
use serde::Deserialize;
use serde_json::json;

/// The previous release's heartbeat: no `seq`. A fixture for servers already deployed; it must
/// not follow later changes to `Heartbeat`.
#[derive(Debug, Deserialize)]
struct PreviousHeartbeat {
    host_id: String,
    epoch: u64,
    phase: Phase,
}

/// The previous release's hello: no `agent_process`.
#[derive(Debug, Deserialize)]
struct PreviousHello {
    host_id: String,
    pool: String,
}

/// Asserts a hello and a heartbeat without the fields decode with `None` and encode without
/// the keys. Catches a new field that is required, or written as `null`, which a deployed
/// server or agent would refuse or misread.
#[test]
fn a_message_without_the_fields_decodes_with_none_and_writes_no_key() {
    let beat = serde_json::to_value(heartbeat()).unwrap();
    assert!(beat.get("seq").is_none());
    let back: Heartbeat = serde_json::from_value(beat).unwrap();
    assert_eq!(back.seq, None);
    let hello_json = serde_json::to_value(hello()).unwrap();
    assert!(hello_json.get("agent_process").is_none());
    let back: Hello = serde_json::from_value(hello_json).unwrap();
    assert_eq!(back.agent_process, None);
}

/// Asserts the exact keys of a heartbeat with `seq` and of a hello with `agent_process`, and
/// that both round trip. Catches a renamed field, which an encode-decode round trip cannot see.
#[test]
fn a_message_with_the_fields_has_their_keys_and_round_trips() {
    let beat = Heartbeat {
        seq: Some(7),
        ..heartbeat()
    };
    let json = serde_json::to_value(&beat).unwrap();
    assert_eq!(json["seq"], json!(7));
    assert_eq!(serde_json::from_value::<Heartbeat>(json).unwrap(), beat);
    let said = Hello {
        agent_process: Some("0a1b2c3d".into()),
        ..hello()
    };
    let json = serde_json::to_value(&said).unwrap();
    assert_eq!(json["agent_process"], json!("0a1b2c3d"));
    assert_eq!(serde_json::from_value::<Hello>(json).unwrap(), said);
}

/// Asserts a reader from before the fields reads a message that carries them and keeps what it
/// knew. Catches a field that makes a server already deployed refuse the session.
#[test]
fn a_previous_reader_skips_the_fields() {
    let beat = Heartbeat {
        seq: Some(2),
        ..heartbeat()
    };
    let old: PreviousHeartbeat = serde_json::from_value(serde_json::to_value(&beat).unwrap())
        .expect("the previous heartbeat decodes a new one");
    assert_eq!((old.host_id.as_str(), old.epoch), ("mac-01", 3));
    assert_eq!(old.phase, Phase::Draining);
    let said = Hello {
        agent_process: Some("abc".into()),
        ..hello()
    };
    let old: PreviousHello = serde_json::from_value(serde_json::to_value(&said).unwrap())
        .expect("the previous hello decodes a new one");
    assert_eq!(
        (old.host_id.as_str(), old.pool.as_str()),
        ("mac-01", "builders")
    );
}

/// Asserts that a heartbeat from this release with `seq` 0 or a large number validates, and
/// that a literal body from an agent before the field decodes. Catches a range check on a
/// field that carries a plain counter.
#[test]
fn seq_is_a_plain_counter() {
    for seq in [0, 1, u64::MAX] {
        let beat = Heartbeat {
            seq: Some(seq),
            ..heartbeat()
        };
        assert_eq!(beat.validate(), Ok(()));
    }
    let text = r#"{"host_id":"mac-01","epoch":3,"phase":"serving","seq":9}"#;
    let beat: Heartbeat = serde_json::from_str(text).unwrap();
    assert_eq!(beat.seq, Some(9));
}

/// Asserts a process id of the largest allowed size and an ordinary one validate. Catches a
/// check that refuses the 32 hex characters the agent sends.
#[test]
fn a_good_agent_process_validates() {
    for id in [
        "0123456789abcdef0123456789abcdef",
        "a",
        "a.b_c-d",
        &long(MAX_ID_LEN),
    ] {
        let said = Hello {
            agent_process: Some(id.to_owned()),
            ..hello()
        };
        assert_eq!(said.validate(), Ok(()), "{id:?}");
    }
}

/// Asserts an empty, too long or badly spelled process id is refused as `invalid-hello` on the
/// field `agent_process`. Catches an unbounded or unchecked id that a server would store as a
/// key.
#[test]
fn a_bad_agent_process_is_refused() {
    let check = |id: String| broken(&hello(), |h| h.agent_process = Some(id), Hello::validate);
    expect(
        check(String::new()),
        "agent_process",
        Rule::Empty,
        refusal::INVALID_HELLO,
    );
    expect(
        check(long(MAX_ID_LEN + 1)),
        "agent_process",
        Rule::TooLong,
        refusal::INVALID_HELLO,
    );
    for id in ["../x", "a b", ".x", "a\0"] {
        let err = check(id.to_owned());
        assert_eq!(
            (err.field(), err.code()),
            ("agent_process", refusal::INVALID_HELLO),
            "{id:?}"
        );
    }
}
