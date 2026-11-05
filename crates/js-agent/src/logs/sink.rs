//! Where one job's logs go: the store and the job's prefix in it, checked once when the job
//! starts.

use js_agent_wire::{JobStartSpec, LogSink};
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
        let sink = spec.log_sink_checked().map_err(|why| why.to_string())?;
        sink.map(Self::of_wire).transpose()
    }

    /// The sink `sink` says, as a start or a progress record carries it.
    ///
    /// # Errors
    ///
    /// The store client cannot use it, named by field and rule (never a value).
    pub(super) fn of_wire(sink: &LogSink) -> Result<Self, String> {
        let endpoint = Endpoint::parse(&sink.endpoint, &sink.region, &sink.bucket, sink.path_style)
            .map_err(|why| format!("log_sink: {why}"))?;
        let prefix =
            JobPrefix::parse(&sink.prefix).map_err(|why| format!("log_sink.prefix: {why}"))?;
        Ok(Self::new(endpoint, prefix))
    }

    /// The sink as a start carries it, for the progress record: the store's URL (its scheme
    /// and authority, the bucket not in it), region, bucket, path style and the job's prefix.
    /// [`Sink::of_wire`] of it is this sink.
    pub(super) fn wire(&self) -> LogSink {
        let endpoint = &self.endpoint;
        let url = endpoint.url("");
        // A virtual-host-style endpoint's host is the bucket, a dot and the authority.
        let url = match endpoint.path_style() {
            true => url,
            false => url.replacen(&format!("://{}.", endpoint.bucket()), "://", 1),
        };
        LogSink {
            endpoint: url,
            region: endpoint.region().to_owned(),
            bucket: endpoint.bucket().to_owned(),
            prefix: self.prefix.as_str().to_owned(),
            path_style: endpoint.path_style(),
        }
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
