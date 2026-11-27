//! What a submit's answer says the server dropped (`docs/design/job-retry-and-completion-callback.md`,
//! section 7, "New client, old server"). A server released before `JobSpec.retry` (field 70)
//! and `JobSpec.callback` (field 71) skips them as unknown fields: it takes the job, runs it
//! once and calls nothing back, and the spec its answer echoes carries neither. A server that
//! knows them echoes each it took, the callback with its signing key empty, so the callback is
//! judged by the message's presence alone.

use js_proto::v1::{JobSpec, SubmitJobRequest, SubmitJobResponse};

/// A field of a submitted job's spec that the request set and the answer's spec does not
/// carry: the server dropped it, and the job runs without it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DroppedPolicy {
    /// `spec.retry`: the job runs once, whatever its policy said.
    Retry,
    /// `spec.callback`: the job's end is never called back.
    Callback,
}

impl DroppedPolicy {
    /// The spec field's name, as the gateway's JSON names it: `retry` or `callback`.
    #[must_use]
    pub fn field(self) -> &'static str {
        match self {
            DroppedPolicy::Retry => "retry",
            DroppedPolicy::Callback => "callback",
        }
    }
}

/// The policies `sent` set that the job spec of `answered` does not carry, the retry policy
/// before the callback; empty when the server kept every one it was sent, or none was sent.
/// An answer without a job or a spec carries none, so every policy sent is reported.
#[must_use]
pub fn dropped_policies(
    sent: &SubmitJobRequest,
    answered: &SubmitJobResponse,
) -> Vec<DroppedPolicy> {
    let asked = sent.spec.as_ref();
    let echoed = answered.job.as_ref().and_then(|job| job.spec.as_ref());
    let has = |spec: Option<&JobSpec>, field: DroppedPolicy| {
        spec.is_some_and(|spec| match field {
            DroppedPolicy::Retry => spec.retry.is_some(),
            DroppedPolicy::Callback => spec.callback.is_some(),
        })
    };
    [DroppedPolicy::Retry, DroppedPolicy::Callback]
        .into_iter()
        .filter(|&field| has(asked, field) && !has(echoed, field))
        .collect()
}
