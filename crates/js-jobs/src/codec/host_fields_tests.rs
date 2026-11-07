//! The host-isolation fields: the job row's isolation tag and the taint row, with round trips,
//! the bytes, the fields a later build adds and every way a taint row can fail to parse.

use super::taint_tag as t;
use super::{decode_taint, encode_taint};
use crate::codec::tests::{full_job, job_row, with, without};
use crate::codec::{DecodeError, FORMAT, decode_job, field_of, job_tag};
use crate::ids::JobId;
use crate::record::{HostTaint, Isolation, JobRecord};
use crate::suite::host_spec;

fn taint() -> HostTaint {
    HostTaint {
        job: JobId::parse("host-job-1").expect("an id"),
    }
}

fn host_job() -> JobRecord {
    JobRecord {
        spec: host_spec(),
        ..full_job()
    }
}

// Catches isolation written and not read back: a host job and a contained one read back as
// written, and a host job's row differs from the contained row only by tag 45.
#[test]
fn isolation_reads_back_as_written() {
    let host = host_job();
    let row = job_row(&host);
    assert_eq!(decode_job(&row), Ok(host));
    let contained = full_job();
    assert_eq!(
        decode_job(&job_row(&contained)).map(|job| job.spec.constraints.isolation),
        Ok(Isolation::Contained)
    );
    assert_eq!(without(&row, job_tag::ISOLATION), job_row(&contained));
}

// Catches a contained job whose row gains a field (an older build then reads a changed row)
// and a host job written with another value than 1.
#[test]
fn only_a_host_job_writes_the_isolation_tag() {
    assert_eq!(field_of(&job_row(&full_job()), job_tag::ISOLATION), None);
    assert_eq!(
        field_of(&job_row(&host_job()), job_tag::ISOLATION),
        Some(vec![1])
    );
}

// Catches a row an earlier build wrote (no isolation tag) read as anything but contained,
// and an explicit zero refused.
#[test]
fn a_row_without_the_isolation_tag_reads_contained() {
    let row = without(&job_row(&host_job()), job_tag::ISOLATION);
    let job = decode_job(&row).expect("reads");
    assert_eq!(job.spec.constraints.isolation, Isolation::Contained);
    let zero = with(&row, job_tag::ISOLATION, &[0]);
    assert_eq!(
        decode_job(&zero).map(|job| job.spec.constraints.isolation),
        Ok(Isolation::Contained)
    );
}

// Catches an isolation value of a later build taken for one this build knows, and a value of
// the wrong width taken for a byte.
#[test]
fn an_isolation_value_this_build_does_not_know_is_newer() {
    let row = job_row(&full_job());
    assert_eq!(
        decode_job(&with(&row, job_tag::ISOLATION, &[2])),
        Err(DecodeError::Newer)
    );
    assert_eq!(
        decode_job(&with(&row, job_tag::ISOLATION, &[1, 1])),
        Err(DecodeError::BadLength {
            tag: job_tag::ISOLATION,
        })
    );
}

// Catches a changed taint layout: the format byte, then tag 1 holding the job id as text.
#[test]
fn the_taint_layout_is_pinned() {
    let row = encode_taint(&taint()).expect("encodes");
    assert_eq!(row[0], FORMAT);
    assert_eq!(field_of(&row, t::JOB), Some(b"host-job-1".to_vec()));
    assert_eq!(row.len(), 1 + 2 + 4 + 10);
    assert_eq!(decode_taint(&row), Ok(taint()));
}

// Catches a taint row with a tag this build does not know refused or misread: it is skipped.
#[test]
fn a_taint_row_skips_a_tag_it_does_not_know() {
    let row = with(&encode_taint(&taint()).expect("encodes"), 77, &[1, 2, 3]);
    assert_eq!(decode_taint(&row), Ok(taint()));
}

// Catches a taint row that does not parse taken for a taint (or a panic).
#[test]
fn every_malformed_taint_row_is_an_error() {
    let row = encode_taint(&taint()).expect("encodes");
    assert_eq!(decode_taint(&[]), Err(DecodeError::Empty));
    assert_eq!(decode_taint(&[FORMAT + 1]), Err(DecodeError::Newer));
    assert_eq!(
        decode_taint(&without(&row, t::JOB)),
        Err(DecodeError::Missing { tag: t::JOB })
    );
    assert_eq!(
        decode_taint(&with(&row, t::JOB, b"job id")),
        Err(DecodeError::BadValue { tag: t::JOB })
    );
}
