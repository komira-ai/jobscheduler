//! Why a job was lost (tag 75 of a job row): its round trip, what a job without a cause writes,
//! and a value a later build might write, which reads as no cause and is kept.

use super::job_lost_tag::LOST_CAUSE;
use crate::codec::tests::{bare_job, full_job, job_row, with, without};
use crate::codec::{decode_job, field_of};
use crate::record::{JobRecord, JobState};
use crate::result::LostCause;

/// [`full_job`] ended lost because its host was silent.
fn silent_job() -> JobRecord {
    JobRecord {
        state: JobState::Lost,
        lost_cause: Some(LostCause::HostSilent),
        ..full_job()
    }
}

// Catches a cause written under another tag or byte, and one written but not read back.
#[test]
fn a_silent_hosts_cause_is_one_byte_under_tag_75_and_reads_back() {
    let row = job_row(&silent_job());
    assert_eq!(field_of(&row, LOST_CAUSE), Some(vec![0]));
    assert_eq!(decode_job(&row), Ok(silent_job()));
}

// Catches a job without a cause writing the tag, so that its row would not be the bytes a build
// before the tag wrote, and a row without the tag read as having a cause.
#[test]
fn a_job_without_a_cause_writes_the_row_it_wrote_before() {
    for job in [full_job(), bare_job()] {
        assert_eq!(field_of(&job_row(&job), LOST_CAUSE), None);
    }
    let older = without(&job_row(&silent_job()), LOST_CAUSE);
    assert_eq!(
        older,
        job_row(&JobRecord {
            lost_cause: None,
            ..silent_job()
        })
    );
    assert_eq!(decode_job(&older).map(|job| job.lost_cause), Ok(None));
}

// Catches a cause a later build wrote refused (hiding the job), read as a cause this build
// knows, or dropped when the row is written again, for a code this build does not know and for
// a value of another length.
#[test]
fn a_cause_this_build_does_not_know_reads_as_none_and_is_written_back() {
    for value in [&[9u8][..], &[0, 0][..], &[][..]] {
        let later = with(&job_row(&full_job()), LOST_CAUSE, value);
        let read = decode_job(&later).expect("a later build's cause reads");
        assert_eq!(read.lost_cause, None, "{value:?}");
        assert_eq!(read.state, full_job().state, "{value:?}");
        assert_eq!(job_row(&read), later, "{value:?}: the field was not kept");
    }
}
