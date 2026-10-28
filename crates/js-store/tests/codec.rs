//! The test codec: what it hands the tests of this crate and of its users, and the ways it
//! refuses a row.

use js_caps::NodeCaps;
use js_sched::{DaemonInstance, Request};
use js_store::test_support::TestCodec;
use js_store::{RowCodec, StateError, Value, WorkerUp};
use js_types::sched::usage::LeaseCost;
use js_types::sched::{
    LeaseGrant, LeaseId, LeaseKind, OperationId, Qos, Resources, WorkKey, WorkerId,
};

fn request() -> Request {
    Request {
        key: WorkKey::new("main", "ab/1"),
        qos: Qos::Ci,
        kind: LeaseKind::Action,
        resources: Resources::new(1_000, 1 << 30),
        needs: js_caps::Request::default(),
        hermetic: true,
        do_not_cache: false,
    }
}

fn grant() -> LeaseGrant {
    LeaseGrant {
        lease: LeaseId::new(5, 4),
        operation: OperationId(8),
        worker: WorkerId::new("node-a"),
        cost: LeaseCost {
            cpu_millis: 500,
            memory_bytes: 2 << 30,
            limit_bytes: 4 << 30,
            cold: true,
        },
    }
}

fn up() -> WorkerUp {
    WorkerUp {
        worker: WorkerId::new("node-a"),
        instance: DaemonInstance::new("daemon-1"),
        predecessor_stopped: true,
        capacity: Resources::new(2_000, 1 << 30),
        caps: NodeCaps::from_report([("arch", "x86_64"), ("os", "linux")]).expect("caps"),
    }
}

/// Asserts that each value the codec encodes reads back as the value, whichever other
/// values were encoded since, and that the bytes are text a test can print.
#[test]
fn each_value_reads_back_as_it_was_encoded() {
    let codec = TestCodec::new();

    let submit = codec.submit(&request()).expect("a request");
    let lease = codec.lease(&grant()).expect("a grant");
    let session = codec.session(&up()).expect("a registration");

    assert_eq!(
        codec.read("op/1", &submit).expect("read"),
        Value::Submit(request())
    );
    assert_eq!(
        codec.read("lease/1", &lease).expect("read"),
        Value::Lease(grant())
    );
    assert_eq!(
        codec.read("session/1", &session).expect("read"),
        Value::Session(up())
    );
    assert_eq!(session, b"session 2");
}

/// Asserts that a value that starts with `later` reads as one a later binary wrote, whatever
/// follows, and is not an error.
#[test]
fn a_value_of_a_later_binary_reads_as_unknown() {
    let codec = TestCodec::new();

    assert_eq!(codec.read("op/1", b"later").expect("read"), Value::Unknown);
    assert_eq!(
        codec.read("op/1", b"later 7 ?").expect("read"),
        Value::Unknown
    );
}

/// Asserts that bytes that are not a value of this codec are refused as corrupt, naming the
/// row: not text, text of another shape, a number that is no number, and a number of a value
/// another codec encoded.
#[test]
fn bytes_that_are_not_a_value_of_the_codec_are_refused_naming_the_row() {
    let codec = TestCodec::new();
    codec.submit(&request()).expect("a request");

    for (bytes, why) in [
        (&[0xff_u8][..], "not text"),
        (b"junk", "not a row of the test codec"),
        (b"submit x", "not a row of the test codec"),
        (b"submit 1", "a row of another codec"),
    ] {
        let refused = codec.read("op/9", bytes);
        let Err(StateError::Corrupt { key, why: said }) = refused else {
            panic!("{bytes:?}: {refused:?}");
        };
        assert_eq!((key.as_str(), said.as_str()), ("op/9", why));
    }
}
