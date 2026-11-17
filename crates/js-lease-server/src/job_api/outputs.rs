//! `GetOutputs` (`docs/design/job-outputs.md`): the outputs a job declared, as its host put them
//! in the object store when the job ended. The call reads the manifest the host wrote last,
//! `P/outputs.json` under the job's prefix `P`, with the server's read key, and answers each
//! file it lists with a presigned GET of the file, signed with the same key, which reads that
//! one object for [`URL_TTL`]; no file's bytes pass through the server.
//!
//! The prefix is built from the configured log prefix, the host the job's record holds and the
//! job's id, as `GetLogs` builds it; nothing of the request but the job id is read. Answers:
//!
//! - `FAILED_PRECONDITION` `no-log-sink` on a server without an object store, and `no-outputs`
//!   for a job that declared none (or whose id gives no prefix, so it had no sink);
//! - not ready, with the job's state and no list, while the job was never placed, has not
//!   ended, or its host has not written the manifest (yet, or ever);
//! - `UNAVAILABLE` `log-store-unavailable` when the store cannot be read, and
//!   `outputs-unreadable` for a manifest that is not the job's (longer than 1 MiB, not JSON, of
//!   another version, or listing a file outside the job's outputs); the server logs why and
//!   the answer holds nothing of the store's;
//! - otherwise ready: the files with their URLs, the missing paths and the failures, and the
//!   time the URLs stop reading.

use std::sync::Arc;
use std::time::{Duration, UNIX_EPOCH};

use js_jobs::JobRecord;
use js_logstore::outputs::{
    MAX_MANIFEST_BYTES, Manifest, ManifestError, OutputPath, check_declared, presigned_get,
};
use js_logstore::{MAX_GET_BYTES, Object, StoreError};
use js_proto::v1::{self, GetOutputsResponse};
use js_types::Clock as _;
use tonic::Status;

use super::JobApi;
use super::logs::{Source, job_prefix, no_log_sink, store_failed};
use super::read::view;
use crate::util::blocking;

/// How long a file's URL reads it.
pub(crate) const URL_TTL: Duration = Duration::from_secs(3600);

/// The outputs of `job`, whose row the caller may read, as `GetOutputs` answers them.
///
/// # Errors
/// The refusals of the module docs.
pub(super) async fn get_outputs(
    api: &JobApi,
    job: JobRecord,
) -> Result<GetOutputsResponse, Status> {
    let source = api.logs.source().ok_or_else(no_log_sink)?;
    // The service checked the declaration at submit, so a stored one always passes.
    let declared = check_declared(&job.spec.outputs).unwrap_or_default();
    if declared.is_empty() {
        return Err(no_outputs());
    }
    let not_ready = GetOutputsResponse {
        state: view::state(job.state).into(),
        ..GetOutputsResponse::default()
    };
    let host = match (job.state.is_ended(), job.host.as_ref()) {
        (true, Some(host)) => host,
        _ => return Ok(not_ready),
    };
    let prefix =
        job_prefix(&source.prefix, host.as_str(), job.id.as_str()).ok_or_else(no_outputs)?;
    let key = prefix.outputs_manifest_key();
    let reader = Arc::clone(&source);
    let read = blocking(move || manifest_of(&reader, &key)).await;
    let Some(object) = read.map_err(|error| store_failed(&error))? else {
        return Ok(not_ready);
    };
    let unreadable = |why: ManifestError| {
        tracing::warn!(job = %job.id, %why, "the outputs manifest cannot be read");
        Status::unavailable("outputs-unreadable")
    };
    if object.total_len > MAX_MANIFEST_BYTES as u64 {
        return Err(unreadable(ManifestError::TooLong));
    }
    let manifest = Manifest::decode(&object.bytes, &declared).map_err(unreadable)?;
    let now_ms = api.clock.now().millis();
    let signed_at = UNIX_EPOCH + Duration::from_millis(now_ms);
    let files: Vec<v1::OutputFile> = manifest
        .files
        .into_iter()
        .filter_map(|file| {
            let path = OutputPath::parse(&file.path).ok()?;
            let key = prefix.output_key(&path);
            let url = presigned_get(
                &source.endpoint,
                &source.credential,
                &key,
                signed_at,
                URL_TTL.as_secs(),
            );
            Some(v1::OutputFile {
                path: file.path,
                bytes: file.bytes,
                sha256: file.sha256,
                url,
            })
        })
        .collect();
    let ttl_ms = u64::try_from(URL_TTL.as_millis()).unwrap_or(u64::MAX);
    let urls_expire_at_ms = (!files.is_empty()).then_some(now_ms.saturating_add(ttl_ms));
    Ok(GetOutputsResponse {
        ready: true,
        files,
        missing: manifest.missing,
        failed: manifest
            .failed
            .into_iter()
            .map(|failure| v1::OutputFailure {
                path: failure.path,
                reason: failure.reason,
            })
            .collect(),
        urls_expire_at_ms,
        ..not_ready
    })
}

/// The manifest at `key`: its first [`MAX_GET_BYTES`] (a manifest is at most
/// [`MAX_MANIFEST_BYTES`], the same) and its whole length, or `None` when there is none.
fn manifest_of(source: &Source, key: &str) -> Result<Option<Object>, StoreError> {
    source.store.get_range(key, 0, MAX_GET_BYTES)
}

/// The answer of a job that declared no outputs: `no-outputs`.
fn no_outputs() -> Status {
    Status::failed_precondition("no-outputs")
}
