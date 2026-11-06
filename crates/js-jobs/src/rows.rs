//! The bytes of a stored job or allocation row, read and written outside the store.
//!
//! The fuzz targets of `fuzz/` feed arbitrary bytes to these functions; they are the codec of
//! [`crate::JobStore`] and nothing else. See the codec's layout notes for the row format.

use crate::codec::{self, DecodeError};
use crate::error::JobStoreError;
use crate::record::{AllocationRecord, JobRecord};

/// The job a stored row holds.
///
/// # Errors
/// [`DecodeError`] when the bytes are not a job row, or a later build wrote them.
pub fn decode_job_row(row: &[u8]) -> Result<JobRecord, DecodeError> {
    codec::decode_job(row)
}

/// The stored row of `job`.
///
/// # Errors
/// [`JobStoreError::TooLarge`] when a field is over 4 GiB.
pub fn encode_job_row(job: &JobRecord) -> Result<Vec<u8>, JobStoreError> {
    codec::encode_job(job)
}

/// The allocation a stored row holds.
///
/// # Errors
/// [`DecodeError`] when the bytes are not an allocation row, or a later build wrote them.
pub fn decode_allocation_row(row: &[u8]) -> Result<AllocationRecord, DecodeError> {
    codec::decode_allocation(row)
}

/// The stored row of `allocation`.
///
/// # Errors
/// [`JobStoreError::TooLarge`] when a field is over 4 GiB.
pub fn encode_allocation_row(allocation: &AllocationRecord) -> Result<Vec<u8>, JobStoreError> {
    codec::encode_allocation(allocation)
}
