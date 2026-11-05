//! Where one job's logs go: the store and the job's prefix in it, checked once when the job
//! starts.

use js_agent_wire::JobStartSpec;
use js_logstore::{Endpoint, JobPrefix};

/// The sink of a job start ([`js_agent_wire::LogSink`]) as the uploader uses it: the store's
/// endpoint (its URL, region, bucket and path style) and the job's prefix `P`. It holds no
/// secret.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Sink {
    endpoint: Endpoint,
    prefix: JobPrefix,
}

impl Sink {
    /// The sink of `spec`: `None` for a start that carries none, and an error, naming the
    /// field and the rule it broke (never a value), for one that fails
    /// [`JobStartSpec::log_sink_checked`] or that the store client cannot use (a region or a
    /// bucket it does not accept, or a prefix that is not the layout's).
    ///
    /// # Errors
    ///
    /// The text of the note the job gets (`log-sink-ignored`).
    pub fn of_start(spec: &JobStartSpec) -> Result<Option<Self>, String> {
        let Some(sink) = spec.log_sink_checked().map_err(|why| why.to_string())? else {
            return Ok(None);
        };
        let endpoint = Endpoint::parse(&sink.endpoint, &sink.region, &sink.bucket, sink.path_style)
            .map_err(|why| format!("log_sink: {why}"))?;
        let prefix =
            JobPrefix::parse(&sink.prefix).map_err(|why| format!("log_sink.prefix: {why}"))?;
        Ok(Some(Self::new(endpoint, prefix)))
    }

    /// The sink of the job whose prefix is `prefix` in the store at `endpoint`.
    #[must_use]
    pub fn new(endpoint: Endpoint, prefix: JobPrefix) -> Self {
        Self { endpoint, prefix }
    }

    /// The store's endpoint.
    #[must_use]
    pub fn endpoint(&self) -> &Endpoint {
        &self.endpoint
    }

    /// The job's prefix.
    #[must_use]
    pub fn prefix(&self) -> &JobPrefix {
        &self.prefix
    }
}
