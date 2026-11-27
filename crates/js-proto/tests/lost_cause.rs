//! Why a job ended lost (`Job.lost_cause`), held to the evolution rule of the package: the field
//! and its enum are additions numbered after the shipped ones, a reader of the previous release
//! meets them as an unknown field, and this reader reads the previous release's bytes, and a
//! value a later release may add, as unspecified.

mod common;

use js_proto::v1::{Job, JobState, LostCause};
use prost::Message;

/// The previous release's `Job`, the fields this test reads: a reader that knows every field up
/// to the dropped attempts and nothing after them reads the same. A fixture standing for
/// readers already deployed; it must not follow later changes.
#[derive(Clone, PartialEq, Message)]
struct PreviousJob {
    #[prost(string, tag = "1")]
    id: String,
    #[prost(enumeration = "JobState", tag = "3")]
    state: i32,
    #[prost(string, tag = "10")]
    host_id: String,
    #[prost(uint32, tag = "41")]
    dropped_attempts: u32,
}

/// The three golden rows that name the lost cause, as the golden writes them.
const ADDED_ROWS: [&str; 3] = [
    "Job.lost_cause = 42 : LostCause",
    "LostCause.LOST_CAUSE_UNSPECIFIED = 0",
    "LostCause.LOST_CAUSE_HOST_SILENT = 1",
];

/// A job that ended lost because its host was silent past the server's limit.
fn silent_job() -> Job {
    Job {
        state: JobState::Lost.into(),
        lost_cause: LostCause::HostSilent.into(),
        dropped_attempts: 2,
        ..common::job()
    }
}

// Catches a renumbered or re-meant field or value, a cause that is written when it is
// unspecified (every job's bytes would change), a previous reader that cannot decode a job lost
// to silence, and a reader that refuses or misreads the previous release's bytes or a value a
// later release adds.
#[test]
fn the_lost_cause_is_additive_on_the_wire() {
    let golden = include_str!("golden/fields.txt");
    for row in ADDED_ROWS {
        let held = golden.lines().filter(|line| *line == row).count();
        assert_eq!(held, 1, "{row}");
    }

    let bytes = silent_job().encode_to_vec();
    // Field 42 as a varint (tag 42 << 3 | 0, two bytes) holding 1.
    assert!(
        bytes.windows(3).any(|w| w == [0xd0, 0x02, 0x01]),
        "{bytes:02x?}"
    );
    let previous = PreviousJob::decode(bytes.as_slice()).expect("the previous reader decodes");
    let expected = PreviousJob {
        id: "job-1".to_owned(),
        state: JobState::Lost.into(),
        host_id: "host-a".to_owned(),
        dropped_attempts: 2,
    };
    assert_eq!(previous, expected);

    let unspecified = Job {
        lost_cause: LostCause::Unspecified.into(),
        ..silent_job()
    };
    // An unspecified cause is not written: the cause costs its three bytes and nothing else.
    assert_eq!(unspecified.encoded_len() + 3, bytes.len());
    let old = expected.encode_to_vec();
    let read = Job::decode(old.as_slice()).expect("this reader decodes the previous bytes");
    assert_eq!(read.lost_cause(), LostCause::Unspecified);

    let later = Job {
        lost_cause: 7,
        ..silent_job()
    };
    let read = Job::decode(later.encode_to_vec().as_slice()).expect("a later value decodes");
    assert_eq!(read.lost_cause(), LostCause::Unspecified);
    assert_eq!(read.state(), JobState::Lost);
}
