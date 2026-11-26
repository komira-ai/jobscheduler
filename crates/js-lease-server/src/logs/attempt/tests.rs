//! The prefix of each attempt and its length rule: attempt 1's prefix is the one a job has
//! always had, a later attempt's adds its segment, no attempt of one job shares a key with an
//! attempt of another, and a log prefix too long for the segment gives later attempts no sink
//! and is said once at start. The reader's side of the length rule is in
//! `job_api/logs_attempt_tests.rs`.

use js_jobs::JobId;
use js_logmint::{Bucket, LogPrefix, RoleArn, StsEndpoint, host_prefix};
use js_logstore::{Endpoint, JobPrefix};
use js_types::{ManualClock, Timestamp};

use super::*;
use crate::agent_api::job_rig::{LOG_BUCKET, LOG_ENDPOINT, LOG_REGION, LOG_TTL, START_MS};
use crate::api::rig::Lines;
use crate::clock::SharedClock;
use crate::file::{AccessKey, LogsConfig, Minting, ObjectStoreLogs};
use crate::logs::{SinkTemplate, from_config};

/// The host segment of `host-a`: `js-` and the first 32 hex digits of SHA-256 of `host-a`.
const HOST_A: &str = "js-c151e392ca52d573f45b9de84af4e5a1";

/// The segment of attempt 2 of `build-42`: SHA-256 of `build-42/2`, in hex.
const BUILD_42_SECOND: &str = "dce52b72a9a8a2b28569b8c66437a18639f0accebc7406445480938d39c62131";

/// The log prefix `text`.
fn log_prefix(text: &str) -> LogPrefix {
    LogPrefix::new(text).unwrap()
}

/// The sink template of the log prefix `text`.
fn template(text: &str) -> SinkTemplate {
    SinkTemplate {
        endpoint: Endpoint::parse(LOG_ENDPOINT, LOG_REGION, LOG_BUCKET, true).unwrap(),
        prefix: log_prefix(text),
    }
}

/// Asserts that attempt 1 (and 0, an older record's) of a job gets the prefix it always had,
/// `prefix/H/job_id/`, built as before attempts were; that attempt 2 gets
/// `prefix/H/S/job_id/` with `S` the hex of SHA-256 of `job_id/2` (the value the agent's
/// check is tested with, `js-agent-wire` `an_attempt_prefix_passes_the_start_check`); that
/// the start's sink carries those prefixes; and that over four job ids, two hosts and 40
/// attempts no attempt's prefix is the prefix of another's, among them a job whose id is the
/// first 63 hex digits of another job's segment and one whose id ends as an attempt marker
/// (`x_a2`), while the 64-digit segment itself is no job id. Catches a later attempt that
/// reuses attempt 1's keys, a change to the keys of jobs without retries, and an attempt's
/// keys that another job's could hold.
#[test]
fn each_attempt_gets_its_own_log_prefix() {
    let js = log_prefix("js");
    let first = format!("js/{HOST_A}/build-42/");
    let before = host_prefix(&js, "host-a").unwrap();
    let before = JobPrefix::new(before.as_str().trim_end_matches('/'), "build-42").unwrap();
    assert_eq!(before.as_str(), first);
    for attempt in [0, 1] {
        let prefix = attempt_prefix(&js, "host-a", "build-42", attempt).unwrap();
        assert_eq!(prefix, before, "{attempt}");
    }
    assert_eq!(attempt_segment("build-42", 2), BUILD_42_SECOND);
    let second = format!("js/{HOST_A}/{BUILD_42_SECOND}/build-42/");
    let prefix = attempt_prefix(&js, "host-a", "build-42", 2).unwrap();
    assert_eq!(prefix.as_str(), second);
    let bare = attempt_prefix(&log_prefix(""), "host-a", "build-42", 2).unwrap();
    assert_eq!(
        bare.as_str(),
        format!("{HOST_A}/{BUILD_42_SECOND}/build-42/")
    );
    let sinks = template("js");
    assert_eq!(
        sinks.sink_for("host-a", "build-42", 1).unwrap().prefix,
        first
    );
    assert_eq!(
        sinks.sink_for("host-a", "build-42", 2).unwrap().prefix,
        second
    );

    let lookalike = &BUILD_42_SECOND[..63];
    assert!(JobId::parse(BUILD_42_SECOND).is_err());
    assert_eq!(attempt_prefix(&js, "host-a", BUILD_42_SECOND, 1), None);
    let mut prefixes = Vec::new();
    for job in ["build-42", "x_a2", lookalike, "a"] {
        for host in ["host-a", "host-b"] {
            for attempt in 1..=40 {
                let prefix = attempt_prefix(&js, host, job, attempt).unwrap();
                assert_eq!(attempt_segment(job, attempt).len(), ATTEMPT_SEGMENT_LEN);
                prefixes.push(prefix.as_str().to_owned());
            }
        }
    }
    for (i, one) in prefixes.iter().enumerate() {
        for (j, other) in prefixes.iter().enumerate() {
            assert!(i == j || !other.starts_with(one.as_str()), "{one} {other}");
        }
    }
}

/// The `[logs]` table of an STS sink under the log prefix `text`.
fn table(text: &str) -> LogsConfig {
    let key = br#"{"access_key_id": "MINTKEY", "secret_access_key": "mint-secret"}"#;
    let key = AccessKey::parse(key).unwrap();
    LogsConfig::ObjectStore(Box::new(ObjectStoreLogs {
        bucket: Bucket::new(LOG_BUCKET).unwrap(),
        prefix: log_prefix(text),
        endpoint: Endpoint::parse(LOG_ENDPOINT, LOG_REGION, LOG_BUCKET, false).unwrap(),
        read_key: key.clone(),
        minting: Minting::Sts {
            endpoint: StsEndpoint::parse(LOG_ENDPOINT, LOG_REGION).unwrap(),
            role_arn: RoleArn::new("arn:aws:iam::example:role/js-logs").unwrap(),
            key,
        },
        credential_ttl: LOG_TTL,
        max_follow: 256,
    }))
}

/// Asserts, for configured prefixes of 0, 411, 412 and 476 bytes (476 is the longest `[logs]`
/// takes), that the grants built at start give attempt 1 of a 63-byte job id a sink under
/// each, and attempt 2 one only up to 411 bytes; that a prefix of 412 or 476 bytes logs
/// exactly one warning, word for word, and one of 0 or 411 bytes logs nothing; and that
/// [`later_attempts_fit`] says the same. Catches a long prefix refused at start (a
/// configuration that worked before would stop working), a later attempt's sink built past
/// the layout's length, and a warning that is missing or logged for a prefix that fits.
#[test]
fn an_attempt_prefix_that_does_not_fit_runs_without_live_logs() {
    let lines = Lines::default();
    let subscriber = tracing_subscriber::fmt()
        .with_writer(lines.clone())
        .with_ansi(false)
        .without_time()
        .with_target(false)
        .finish();
    let _guard = tracing::subscriber::set_default(subscriber);
    let clock = SharedClock::new(ManualClock::new(Timestamp::from_millis(START_MS)));
    let job = "j".repeat(63);
    let warning = " WARN the log prefix is longer than 411 bytes: a job's attempts after the \
                   first run without live logs\n";
    for (length, fits) in [(0, true), (411, true), (412, false), (476, false)] {
        let text = "p".repeat(length);
        let grants = from_config(&table(&text), &clock).unwrap();
        assert!(grants.sink_for("host-a", &job, 1).is_some(), "{length}");
        let second = grants.sink_for("host-a", &job, 2);
        assert_eq!(second.is_some(), fits, "{length}");
        assert_eq!(later_attempts_fit(&log_prefix(&text)), fits, "{length}");
        let said = lines.take();
        match fits {
            true => assert_eq!(said, "", "{length}"),
            false => assert_eq!(said, warning, "{length}"),
        }
    }
    assert_eq!(LONGEST_PREFIX_FOR_EVERY_ATTEMPT, 411);
}
