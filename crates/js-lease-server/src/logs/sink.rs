//! The sink a job's start carries: where the agent uploads that one job's output, built from
//! the `[logs]` table and the host the job runs on. It holds no secret.

use js_agent_wire::LogSink;
use js_logmint::{LogPrefix, host_prefix};
use js_logstore::{Endpoint, JobPrefix};

/// What every job's sink shares: the store, its region, the bucket, the style and the log
/// prefix. A job's own sink adds the job's prefix `P` under its host's prefix
/// ([`SinkTemplate::sink_for`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct SinkTemplate {
    /// The store the agents put to.
    pub(crate) endpoint: Endpoint,
    /// The configured log prefix, with its trailing `/` (empty for none).
    pub(crate) prefix: LogPrefix,
}

impl SinkTemplate {
    /// The sink of the job `job_id` on the host `host`: the store's URL, region, bucket and
    /// style, and the job's prefix `prefix/js-<hash of host>/job_id/` under the host's own
    /// prefix ([`js_logmint::host_prefix`]), the one the host's credential covers. `None` for a job id
    /// the object layout builds no prefix for (one that does not begin with a letter or a
    /// digit, or is longer than 63 bytes), or a log prefix too long to hold a host's prefix:
    /// such a job runs without live logs.
    pub(crate) fn sink_for(&self, host: &str, job_id: &str) -> Option<LogSink> {
        let base = host_prefix(&self.prefix, host).ok()?;
        let prefix = JobPrefix::new(base.as_str().trim_end_matches('/'), job_id).ok()?;
        Some(LogSink {
            endpoint: self.endpoint.base_url(),
            region: self.endpoint.region().to_owned(),
            bucket: self.endpoint.bucket().to_owned(),
            prefix: prefix.as_str().to_owned(),
            path_style: self.endpoint.path_style(),
        })
    }
}
