//! Why a job was lost: tag 75 of a job row, one byte, 0 for a host silent past the server's
//! `silence_limit` ([`LostCause::HostSilent`]).
//!
//! The tag is written only for a job whose end recorded a cause, so the row of every other job
//! is the bytes a build before the tag wrote, and a row without it reads with no cause. A build
//! before the tag keeps it as a field it does not know and writes it back unchanged.
//!
//! **Additive (B8).** A code this build does not know, or a value that is not one byte, is
//! neither an error nor dropped: the job reads with no cause, and the field stays among the
//! fields the record carries, so the next write of the row keeps it for the build that wrote
//! it. Unlike an unknown state, which makes the row a later build's, a cause changes nothing
//! this build decides, so it does not hide the job.

use super::{Fields, Writer};
use crate::result::LostCause;

/// The tag this field adds to a job row, after the swap peak's 74.
pub(crate) mod job_lost_tag {
    /// Why the job was lost (one byte: 0 a host silent past the server's limit).
    pub const LOST_CAUSE: u16 = 75;
}

use job_lost_tag::LOST_CAUSE;

/// The byte of `cause`.
fn code(cause: LostCause) -> u8 {
    match cause {
        LostCause::HostSilent => 0,
    }
}

/// The cause a value names, when this build knows it.
fn cause_of(value: &[u8]) -> Option<LostCause> {
    match value {
        [0] => Some(LostCause::HostSilent),
        _ => None,
    }
}

/// Puts why the job was lost, when its end recorded a cause, in `row`.
pub(super) fn put_job(row: &mut Writer, cause: Option<LostCause>) {
    row.put_opt(LOST_CAUSE, cause.map(|cause| vec![code(cause)]));
}

/// Why the job of `fields` was lost, when the row holds a cause this build knows, which is then
/// taken from the fields; a value this build does not know is left among them to be carried.
pub(super) fn take_job(fields: &mut Fields) -> Option<LostCause> {
    let cause = fields.0.get(&LOST_CAUSE).and_then(|value| cause_of(value));
    if cause.is_some() {
        fields.0.remove(&LOST_CAUSE);
    }
    cause
}

#[cfg(test)]
#[path = "lost_fields_tests.rs"]
mod tests;
