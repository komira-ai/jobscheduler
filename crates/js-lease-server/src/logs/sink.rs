//! The sink a job's start carries: where the agent uploads that one job's output, built from
//! the `[logs]` table alone. It holds no secret.

use js_agent_wire::LogSink;
use js_logmint::LogPrefix;
use js_logstore::{Endpoint, JobPrefix};

/// What every job's sink shares: the store, its region, the bucket, the style and the log
/// prefix. A job's own sink adds the job's prefix `P` under it ([`SinkTemplate::sink_for`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct SinkTemplate {
    /// The store the agents put to.
    pub(crate) endpoint: Endpoint,
    /// The configured log prefix, with its trailing `/` (empty for none).
    pub(crate) prefix: LogPrefix,
}

impl SinkTemplate {
    /// The sink of the job `job_id`: the store's URL, region, bucket and style, and the job's
    /// prefix `prefix/job_id/` (`job_id/` under an empty prefix). `None` for a job id the
    /// object layout builds no prefix for (one that does not begin with a letter or a digit,
    /// or is longer than 63 bytes): such a job runs without live logs.
    pub(crate) fn sink_for(&self, job_id: &str) -> Option<LogSink> {
        let base = self.prefix.as_str().trim_end_matches('/');
        let prefix = JobPrefix::new(base, job_id).ok()?;
        Some(LogSink {
            endpoint: self.endpoint.base_url(),
            region: self.endpoint.region().to_owned(),
            bucket: self.endpoint.bucket().to_owned(),
            prefix: prefix.as_str().to_owned(),
            path_style: self.endpoint.path_style(),
        })
    }
}
