//! Any bytes as a frame body of each type the two sides read. A transport refuses a body
//! over its budget before it decodes, so an input longer than the budget of a type is not
//! decoded as that type: the agent's own frames (`Hello`, `Heartbeat`, `Caps`) are held to
//! `HEARTBEAT_BODY_BUDGET`, the server's (`Welcome`, `HelloReply`, `HeartbeatReply`,
//! `Command`, `JobSpec`, `JobStartSpec`) to `REPLY_BODY_BUDGET`.
//!
//! For each type the decode must not panic, `validate` of a decoded value must not
//! panic, and a decoded value must encode and decode to an equal value (a lenient field
//! drops what it cannot read, so the decoded value holds only entries that decode again).
//!
//! The decode of one body must not hold more than `PEAK_FACTOR` times the budget in live
//! bytes above what was live before it; the allocator (`cap::Cap` over the system allocator)
//! has that bound as its limit during the decode, and an allocation past it fails, which the
//! default allocation-error handler turns into an abort that the fuzzer reports as a crash.
//! The factor is 128. A local run found the worst
//! ratio of peak live bytes to body length, 55.2, on a body of the reply budget that is `{"type":"welcome","a":[`
//! then `[[[[0]]]],` repeated, decoded as a `HelloReply`: the tag of an
//! internally tagged enum makes serde buffer the whole body as a generic tree, and each
//! small nested value costs a node. The other shapes tried (a long command list of unknown
//! commands, of starts, of stops, long acknowledgement lists, many labels, many held runs)
//! gave 7.4 to 23.7; the Rust types themselves keep one tree per command at most. 128 leaves
//! more than twice the worst for a deeper shape; a decode that kept more than that, or one
//! copy of the body per entry, would break it.

#![no_main]

use std::fmt::Debug;

use js_agent_wire::limits::{HEARTBEAT_BODY_BUDGET, REPLY_BODY_BUDGET};
use js_agent_wire::{
    Caps, Command, Heartbeat, HeartbeatReply, Hello, HelloReply, JobSpec, JobStartSpec, Welcome,
};
use libfuzzer_sys::fuzz_target;
use serde::Serialize;
use serde::de::DeserializeOwned;

/// The most live bytes one decode may add, as a multiple of the budget it is held to.
const PEAK_FACTOR: usize = 128;

#[global_allocator]
static ALLOC: cap::Cap<std::alloc::System> = cap::Cap::new(std::alloc::System, usize::MAX);

/// Decodes `data` as a `T` if it is within `budget`, holds the decode to the live-bytes bound,
/// runs `validate` on a value and checks that it survives an encode and a decode.
fn frame<T>(data: &[u8], budget: usize, validate: impl Fn(&T))
where
    T: Serialize + DeserializeOwned + PartialEq + Debug,
{
    if data.len() > budget {
        return;
    }
    ALLOC
        .set_limit(ALLOC.allocated() + PEAK_FACTOR * budget)
        .expect("the limit is above what is live");
    let decoded = serde_json::from_slice::<T>(data);
    ALLOC
        .set_limit(usize::MAX)
        .expect("the limit is above what is live");
    let Ok(value) = decoded else { return };
    validate(&value);
    let encoded = serde_json::to_vec(&value).expect("a decoded value encodes");
    let again = serde_json::from_slice::<T>(&encoded).expect("an encoded value decodes");
    assert_eq!(again, value);
}

fuzz_target!(|data: &[u8]| {
    frame::<Hello>(data, HEARTBEAT_BODY_BUDGET, |v| {
        let _ = v.validate();
    });
    frame::<Heartbeat>(data, HEARTBEAT_BODY_BUDGET, |v| {
        let _ = v.validate();
    });
    frame::<Caps>(data, HEARTBEAT_BODY_BUDGET, |v| {
        let _ = v.validate();
    });
    frame::<Welcome>(data, REPLY_BODY_BUDGET, |v| {
        let _ = v.validate();
    });
    frame::<HelloReply>(data, REPLY_BODY_BUDGET, |v| {
        let _ = v.validate();
    });
    frame::<HeartbeatReply>(data, REPLY_BODY_BUDGET, |v| {
        let _ = v.validate();
    });
    frame::<Command>(data, REPLY_BODY_BUDGET, |v| {
        let _ = v.validate();
    });
    frame::<JobSpec>(data, REPLY_BODY_BUDGET, |v| {
        let _ = v.validate();
    });
    frame::<JobStartSpec>(data, REPLY_BODY_BUDGET, |v| {
        let _ = v.validate();
    });
});
