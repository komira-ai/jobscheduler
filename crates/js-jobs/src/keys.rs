//! The keys of the job store's rows. A job id is the key text as it is: its alphabet holds no
//! `/` and nothing that needs escaping.

use crate::ids::JobId;

/// `job/<id>`: a job's record.
pub const JOB: &str = "job/";

/// `alloc/<id>`: the allocation of the job with that id. It has the job's id, so a job's
/// allocation is one read, not a scan.
pub const ALLOCATION: &str = "alloc/";

/// `meta/jobs/schema`: the schema version of the job and allocation rows, four bytes,
/// big-endian, written once by the first store to open.
pub const SCHEMA: &str = "meta/jobs/schema";

/// The key of the job `id`.
#[must_use]
pub fn job(id: &JobId) -> String {
    format!("{JOB}{id}")
}

/// The key of the allocation of the job `id`.
#[must_use]
pub fn allocation(id: &JobId) -> String {
    format!("{ALLOCATION}{id}")
}
