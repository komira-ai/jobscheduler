//! The host-isolation fields: the job row's isolation tag and the taint row, with round trips,
//! the bytes, the fields a later build adds and every way a taint row can fail to parse.

use super::cordon_tag;
use super::taint_tag as t;
use super::{decode_cordon, decode_taint, encode_cordon, encode_taint};
use crate::codec::tests::{full_job, job_row, with, without};
use crate::codec::{DecodeError, FORMAT, LATER_FORMAT, decode_job, field_of, job_host_tag};
use crate::ids::JobId;
use crate::record::{HostTaint, Isolation, JobRecord};
use crate::suite::host_spec;
use js_types::CordonHolder;

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
// written, and a host job's row differs from the contained row only by tag 47.
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
    assert_eq!(without(&row, job_host_tag::ISOLATION), job_row(&contained));
}

// Catches a contained job whose row gains a field (an older build then reads a changed row)
// and a host job written with another value than 1.
#[test]
fn only_a_host_job_writes_the_isolation_tag() {
    assert_eq!(
        field_of(&job_row(&full_job()), job_host_tag::ISOLATION),
        None
    );
    assert_eq!(
        field_of(&job_row(&host_job()), job_host_tag::ISOLATION),
        Some(vec![1])
    );
}

// Catches a row an earlier build wrote (no isolation tag) read as anything but contained,
// and an explicit zero refused.
#[test]
fn a_row_without_the_isolation_tag_reads_contained() {
    let row = without(&job_row(&host_job()), job_host_tag::ISOLATION);
    let job = decode_job(&row).expect("reads");
    assert_eq!(job.spec.constraints.isolation, Isolation::Contained);
    let zero = with(&row, job_host_tag::ISOLATION, &[0]);
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
        decode_job(&with(&row, job_host_tag::ISOLATION, &[2])),
        Err(DecodeError::Newer)
    );
    assert_eq!(
        decode_job(&with(&row, job_host_tag::ISOLATION, &[1, 1])),
        Err(DecodeError::BadLength {
            tag: job_host_tag::ISOLATION,
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
    assert_eq!(decode_taint(&[LATER_FORMAT]), Err(DecodeError::Newer));
    assert_eq!(
        decode_taint(&without(&row, t::JOB)),
        Err(DecodeError::Missing { tag: t::JOB })
    );
    assert_eq!(
        decode_taint(&with(&row, t::JOB, b"job id")),
        Err(DecodeError::BadValue { tag: t::JOB })
    );
}

fn holder() -> CordonHolder {
    CordonHolder::new("req-1.a_b").expect("a holder")
}

/// Asserts that a cordon row round-trips with and without a holder, and that its layout is the
/// format byte and, for a holder, tag 1 holding its text, so a cordon no holder named is the
/// format byte alone. It catches a holder lost or invented on the way through the row.
#[test]
fn a_cordon_row_round_trips_and_its_layout_is_pinned() {
    let named = encode_cordon(Some(&holder())).expect("encodes");
    assert_eq!(named[0], FORMAT);
    assert_eq!(
        field_of(&named, cordon_tag::HOLDER),
        Some(b"req-1.a_b".to_vec())
    );
    assert_eq!(named.len(), 1 + 2 + 4 + 9);
    assert_eq!(decode_cordon(&named), Ok(Some(holder())));
    let unnamed = encode_cordon(None).expect("encodes");
    assert_eq!(unnamed, vec![FORMAT]);
    assert_eq!(decode_cordon(&unnamed), Ok(None));
}

/// Asserts that a cordon row with a tag this build does not know reads as the row without it.
/// It catches a later build's additive field refused as corrupt.
#[test]
fn a_cordon_row_skips_a_tag_it_does_not_know() {
    let row = with(
        &encode_cordon(Some(&holder())).expect("encodes"),
        77,
        &[1, 2],
    );
    assert_eq!(decode_cordon(&row), Ok(Some(holder())));
}

/// Asserts that every way a cordon row can fail to parse is the error that says so: empty, a
/// later build's format byte (a newer row, not a corrupt one), format byte 0, a holder that is
/// not UTF-8 and a holder that is not a valid id. It catches a bad row read as no holder.
#[test]
fn every_malformed_cordon_row_is_an_error() {
    assert_eq!(decode_cordon(&[]), Err(DecodeError::Empty));
    assert_eq!(decode_cordon(&[LATER_FORMAT]), Err(DecodeError::Newer));
    assert_eq!(decode_cordon(&[0]), Err(DecodeError::BadFormat));
    let row = encode_cordon(Some(&holder())).expect("encodes");
    assert_eq!(
        decode_cordon(&with(&row, cordon_tag::HOLDER, &[0xff])),
        Err(DecodeError::NotUtf8 {
            tag: cordon_tag::HOLDER
        })
    );
    assert_eq!(
        decode_cordon(&with(&row, cordon_tag::HOLDER, b"Not A Holder")),
        Err(DecodeError::BadValue {
            tag: cordon_tag::HOLDER
        })
    );
}
