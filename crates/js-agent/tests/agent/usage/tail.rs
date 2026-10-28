//! The binner's last minute index and the wire's are one number.

/// Asserts that the last minute index the binner folds into is the last one the heartbeat
/// accepts, and that `js_agent::usage` still names the binner's constant. The wire refuses
/// a minute index past its limit, so the two must not drift apart.
#[test]
fn usage_tail_minute_is_the_wires() {
    assert_eq!(
        js_agent_usage::TAIL_MINUTE,
        js_agent_wire::limits::TAIL_MINUTE
    );
    assert_eq!(js_agent::usage::TAIL_MINUTE, js_agent_usage::TAIL_MINUTE);
}
