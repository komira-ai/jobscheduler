//! The files of a job: their contents are kept while it is live, and replaced by their
//! SHA-256 digests in the write that ends it, by every way a job ends; a resubmit of an ended
//! job is matched by those digests; and a boot replaces the contents an ended job's row still
//! holds from a build that kept them.

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Duration;

use js_state::Store;
use js_state::conformance::Backend;

use super::{Rig, cancelled, exited, full_spec, host, output, rig, share};
use crate::codec::encode_job;
use crate::{
    EndReason, FileDigest, JobEnd, JobFiles, JobId, JobRecord, JobSpec, JobState, JobStore,
    JobStoreError, MAX_PAGE, Needs, NewJob, RandomIds, StartFailure,
};

/// A read token, as a job's client hands it to the job.
const TOKEN: &[u8] = b"read-token-QX7vR2";

/// The SHA-256 of [`TOKEN`], as `sha256sum` gives it.
const TOKEN_SHA256: &str = "d6605b8b4ab3ba209c17ae2d4a51e8b2382230ea884976d0fd042196cba73491";

/// A presigned upload URL, as a job's client hands it to the job.
const PUT_URL: &[u8] = b"presigned-put-Zq81";

/// The SHA-256 of [`PUT_URL`], as `sha256sum` gives it.
const PUT_URL_SHA256: &str = "a9a420d9bd0041091cb2dbb6bcd5308bf5b20b7f3cbc21b81ca8aa363e5bae4e";

/// The 32 bytes 64 hex digits spell.
fn unhex(text: &str) -> [u8; 32] {
    let mut bytes = [0u8; 32];
    for (at, byte) in bytes.iter_mut().enumerate() {
        *byte = u8::from_str_radix(&text[2 * at..2 * at + 2], 16).expect("hex digits");
    }
    bytes
}

/// The files every job here is handed: [`TOKEN`] and [`PUT_URL`].
fn secret_files() -> BTreeMap<String, Vec<u8>> {
    BTreeMap::from([
        ("inputs/token".to_owned(), TOKEN.to_vec()),
        ("outputs/upload".to_owned(), PUT_URL.to_vec()),
    ])
}

/// What an ended job keeps of [`secret_files`]: each name with the SHA-256 `sha256sum` gives.
fn secret_digests() -> JobFiles {
    JobFiles::Digests(BTreeMap::from([
        (
            "inputs/token".to_owned(),
            FileDigest::from_bytes(unhex(TOKEN_SHA256)),
        ),
        (
            "outputs/upload".to_owned(),
            FileDigest::from_bytes(unhex(PUT_URL_SHA256)),
        ),
    ]))
}

/// The full spec, handed [`secret_files`].
fn secret_spec() -> JobSpec {
    JobSpec {
        files: JobFiles::Contents(secret_files()),
        ..full_spec()
    }
}

/// Whether `row` holds `needle` anywhere.
fn holds(row: &[u8], needle: &[u8]) -> bool {
    row.windows(needle.len()).any(|window| window == needle)
}

impl<S: Store> Rig<S> {
    /// Records the job `text` with [`secret_spec`] and brings it to `state` (queued, placed,
    /// starting or running).
    fn secret_in(&self, text: &str, state: JobState) -> JobId {
        let id = self.queue_spec(text, secret_spec());
        if state == JobState::Queued {
            return id;
        }
        self.jobs
            .place(&id, host("host-a"), share())
            .expect("a queued job is placed");
        for (from, to) in [
            (JobState::Placed, JobState::Starting),
            (JobState::Starting, JobState::Running),
        ] {
            if self.jobs.get_job(&id).expect("reads").map(|job| job.state) == Some(state) {
                break;
            }
            self.jobs.transition(&id, from, to).expect("moves");
        }
        id
    }

    /// Asserts, for the check `name`, that the ended job `id` holds the names of its files and
    /// their digests and no byte of their contents: in the record `returned` by the call that
    /// ended it, in its read, in a listing, and in its stored row.
    fn holds_digests_only(&self, name: &str, id: &JobId, returned: &JobRecord, how: &str) {
        assert!(returned.state.is_ended(), "{name}: {how} did not end {id}");
        assert_eq!(
            returned.spec.files,
            secret_digests(),
            "{name}: {how} returned {id} with its files' contents, or not their SHA-256"
        );
        let read = self.jobs.get_job(id).expect("reads").expect("present");
        assert_eq!(
            &read, returned,
            "{name}: {how} returned a record that differs from the one it stored"
        );
        let listed = self
            .jobs
            .list_jobs(Some(returned.state), None, MAX_PAGE)
            .expect("lists");
        assert!(
            listed.jobs.contains(returned),
            "{name}: the listing does not show {id} as stored after {how}"
        );
        let row = self
            .raw(&crate::keys::job(id))
            .expect("the row is kept")
            .value;
        for secret in [TOKEN, PUT_URL] {
            assert!(
                !holds(&row, secret),
                "{name}: the row of {id} still holds a file's contents after {how}"
            );
        }
        for name_of_file in returned.spec.files.names() {
            assert!(
                holds(&row, name_of_file.as_bytes()),
                "{name}: the row of {id} lost the file name {name_of_file} after {how}"
            );
        }
    }
}

/// Every way a job ends replaces its files' contents with their SHA-256 in the write that
/// ends it: each end of [`JobStore::end_job`] from each state it may end from, the cancel of a
/// queued job ([`JobStore::request_cancel`]) and [`JobStore::end_and_free`]. The names are
/// kept, and no record, listing or row holds a byte of the contents.
///
/// Catches an end that keeps the contents (a read token, an upload URL) in the history the
/// store holds for the retention window, by any of the paths that end a job.
pub fn ended_job_record_holds_no_file_bytes<B: Backend>(backend: &B) {
    use JobState::{Placed, Queued, Running, Starting};
    use StartFailure::{AgentRefused, ExclusiveNotReady, NoCapacity, StartTimeout};
    let name = "ended_job_record_holds_no_file_bytes";
    let rig = rig(backend);
    let wall_limit = JobEnd::WallLimit {
        exit_code: Some(137),
        output: output(),
    };
    let memory = JobEnd::Memory {
        exit_code: None,
        output: output(),
    };
    let ends = [
        (Queued, JobEnd::FailedToStart(NoCapacity)),
        (Queued, cancelled()),
        (Queued, JobEnd::Lost),
        (Placed, JobEnd::FailedToStart(StartTimeout)),
        (Placed, JobEnd::FailedToStart(AgentRefused)),
        (Placed, JobEnd::FailedToStart(ExclusiveNotReady)),
        (Placed, cancelled()),
        (Placed, JobEnd::Lost),
        (Starting, JobEnd::FailedToStart(StartTimeout)),
        (Starting, cancelled()),
        (Starting, JobEnd::Lost),
        (Running, exited(0)),
        (Running, exited(1)),
        (Running, wall_limit),
        (Running, memory),
        (Running, cancelled()),
        (Running, JobEnd::Lost),
    ];
    for (n, (from, end)) in ends.into_iter().enumerate() {
        let id = rig.secret_in(&format!("end-{n:02}"), from);
        let how = format!("end_job from {from:?} to {:?}", end.state());
        let ended = rig.jobs.end_job(&id, end).expect("the job ends");
        rig.holds_digests_only(name, &id, &ended, &how);
    }

    let queued = rig.secret_in("cancel-queued", Queued);
    let cancelled = rig.jobs.request_cancel(&queued).expect("cancels");
    rig.holds_digests_only(name, &queued, &cancelled, "the cancel of a queued job");

    for (n, from) in [Placed, Running].into_iter().enumerate() {
        let id = rig.secret_in(&format!("free-{n}"), from);
        let (ended, _) = rig
            .jobs
            .end_and_free(&id, JobEnd::Lost, EndReason::HostAbsent)
            .expect("ends and frees");
        rig.holds_digests_only(name, &id, &ended, &format!("end_and_free from {from:?}"));
    }
}

/// A job that has not ended keeps its files' contents byte for byte, so its host can still be
/// sent them: queued, placed, starting, running, running with a cancel asked for, and queued
/// again after a whole-host prepare failed.
///
/// Catches a scrub done too early (at placement, at a cancel intent, at a requeue), which
/// would leave the host nothing to mount for a job that still runs.
pub fn live_job_keeps_file_bytes<B: Backend>(backend: &B) {
    let name = "live_job_keeps_file_bytes";
    let rig = rig(backend);
    let contents = JobFiles::Contents(secret_files());
    let mut live = Vec::new();
    for state in [
        JobState::Queued,
        JobState::Placed,
        JobState::Starting,
        JobState::Running,
    ] {
        live.push((rig.secret_in(&format!("live-{state:?}"), state), state));
    }
    let asked = rig.secret_in("live-cancel-asked", JobState::Running);
    rig.jobs.request_cancel(&asked).expect("an intent");
    live.push((asked, JobState::Running));
    let whole = rig.queue_spec(
        "live-requeued",
        JobSpec {
            needs: Needs::WholeHost,
            ..secret_spec()
        },
    );
    rig.jobs
        .place(&whole, host("host-a"), crate::Booked::WholeHost)
        .expect("placed");
    rig.jobs.requeue_failed_prepare(&whole).expect("requeued");
    live.push((whole, JobState::Queued));
    for (id, state) in live {
        let job = rig.jobs.get_job(&id).expect("reads").expect("present");
        assert_eq!(job.state, state, "{name}: {id} is not {state:?}");
        assert_eq!(
            job.spec.files, contents,
            "{name}: {id}, {state:?}, lost its files' contents"
        );
        let row = rig.raw(&crate::keys::job(&id)).expect("present").value;
        assert!(
            holds(&row, TOKEN) & holds(&row, PUT_URL),
            "{name}: the row of {id}, {state:?}, lost its files' contents"
        );
    }
}

/// A resubmit of an ended job, whose files are digests, is matched by the SHA-256 of each
/// submitted file: the same files answer the stored job and write nothing; one file's contents
/// changed, a file added, a file left out or a file renamed is a conflict, as a resubmit with
/// another field changed is, and writes nothing.
///
/// Catches a resubmit compared byte for byte against the digests, which refuses every retry
/// of an ended job with files, and one compared by names alone, which answers a different job.
pub fn an_ended_job_is_matched_by_the_digests_of_its_files<B: Backend>(backend: &B) {
    let name = "an_ended_job_is_matched_by_the_digests_of_its_files";
    let rig = rig(backend);
    let id = rig.secret_in("ended", JobState::Queued);
    let ended = rig.jobs.request_cancel(&id).expect("cancels");
    assert_eq!(ended.spec.files, secret_digests(), "{name}: not digested");
    let row = rig.raw("job/ended");
    let resubmit = |spec: JobSpec| {
        rig.jobs.insert_queued(NewJob {
            id: Some(id.clone()),
            spec,
            priority: 5,
            client: None,
        })
    };

    let again = resubmit(secret_spec()).expect("the same job is no conflict");
    assert_eq!(
        (again.created, &again.record),
        (false, &ended),
        "{name}: the same files did not answer the stored job"
    );
    assert_eq!(
        rig.raw("job/ended"),
        row,
        "{name}: the same job wrote the row"
    );

    let file_change = |change: fn(&mut BTreeMap<String, Vec<u8>>)| {
        let mut files = secret_files();
        change(&mut files);
        JobSpec {
            files: JobFiles::Contents(files),
            ..secret_spec()
        }
    };
    let changes = [
        (
            "one file's contents",
            file_change(|files| {
                files.insert("inputs/token".to_owned(), b"read-token-other".to_vec());
            }),
        ),
        (
            "a file added",
            file_change(|files| {
                files.insert("inputs/more".to_owned(), Vec::new());
            }),
        ),
        (
            "a file left out",
            file_change(|files| {
                files.remove("outputs/upload");
            }),
        ),
        (
            "a file renamed",
            file_change(|files| {
                let contents = files.remove("outputs/upload").expect("present");
                files.insert("outputs/put".to_owned(), contents);
            }),
        ),
        (
            "the wall limit",
            JobSpec {
                wall_limit_secs: 601,
                ..secret_spec()
            },
        ),
    ];
    for (what, spec) in changes {
        assert_eq!(
            resubmit(spec),
            Err(JobStoreError::JobIdConflict { id: id.clone() }),
            "{name}: a resubmit with {what} changed was not a conflict"
        );
        assert_eq!(
            rig.raw("job/ended"),
            row,
            "{name}: the conflict over {what} wrote the row"
        );
    }
    assert_eq!(rig.job_rows(), 1, "{name}: a resubmit made a second job");
}

/// A row of an ended job that still holds its files' contents (one ended by a build that kept
/// them) has them replaced by their digests at the first boot after a restart, in the same put
/// as its raised retention end and in the boot's one commit; a later boot writes nothing, and a
/// live job's contents and an ended job without files are left as they are. Rows with a
/// retention end later than the boot, or with none, are digested with it left as it is.
///
/// Catches a store upgraded in place that keeps, for the rest of their retention, the contents
/// of every job that ended before the upgrade.
pub fn a_boot_digests_the_files_an_ended_row_still_holds<B: Backend>(backend: &B)
where
    B::Store: 'static,
{
    let name = "a_boot_digests_the_files_an_ended_row_still_holds";
    let rig = rig(backend);
    let old = rig.secret_in("old-ended", JobState::Running);
    let ended = rig.jobs.end_job(&old, exited(0)).expect("ends");
    // The row as a build that kept the contents after the end wrote it.
    let kept = JobRecord {
        spec: secret_spec(),
        ..ended.clone()
    };
    rig.put_raw("job/old-ended", encode_job(&kept).expect("encodes"));
    let bare = rig.queue_spec(
        "old-bare",
        JobSpec {
            files: JobFiles::default(),
            ..full_spec()
        },
    );
    let bare = rig.jobs.request_cancel(&bare).expect("cancels");
    let bare_kept = JobRecord {
        spec: JobSpec {
            files: JobFiles::default(),
            ..bare.spec.clone()
        },
        ..bare.clone()
    };
    rig.put_raw("job/old-bare", encode_job(&bare_kept).expect("encodes"));
    let live = rig.secret_in("old-running", JobState::Running);
    // Ended rows holding contents whose retention end the boot does not raise: one far past
    // the boot, one with none (as a build before retention ends wrote it).
    let mut untouched_ends = Vec::new();
    for (text, retention_ends) in [
        (
            "old-late",
            Some(js_state::StoreTime::from_duration(Duration::from_secs(
                u64::MAX / 4,
            ))),
        ),
        ("old-no-retention", None),
    ] {
        let id = rig.secret_in(text, JobState::Running);
        let ended = rig.jobs.end_job(&id, exited(0)).expect("ends");
        let kept = JobRecord {
            spec: secret_spec(),
            retention_ends,
            ..ended
        };
        rig.put_raw(&crate::keys::job(&id), encode_job(&kept).expect("encodes"));
        untouched_ends.push(kept);
    }

    let Rig { store, jobs, clock } = rig;
    drop(jobs);
    let store = Arc::into_inner(store)
        .and_then(|wrapped| wrapped.into_inner())
        .expect("nothing else holds the store");
    let store = Arc::new(backend.restart(store));
    let jobs = JobStore::new(Arc::clone(&store), RandomIds).expect("reopens");
    assert_eq!(
        jobs.get_job(&old).expect("reads").map(|job| job.spec.files),
        Some(JobFiles::Contents(secret_files())),
        "{name}: the row did not hold the contents before the boot"
    );
    clock.advance(Duration::from_secs(8 * 24 * 3600));
    let boot = store.now();
    let bare_row = store.get("job/old-bare").expect("reads");
    let live_row = store.get("job/old-running").expect("reads");
    let commits = store.commit_stats().txns;

    assert_eq!(
        jobs.raise_at_boot(boot),
        Ok(4),
        "{name}: the boot did not write the three ended rows with contents and the bare row's \
         retention end alone"
    );
    assert_eq!(
        store.commit_stats().txns,
        commits + 1,
        "{name}: the boot was not one commit"
    );
    let now = jobs.get_job(&old).expect("reads").expect("present");
    assert_eq!(
        now,
        JobRecord {
            spec: JobSpec {
                files: secret_digests(),
                ..secret_spec()
            },
            retention_ends: Some(boot.saturating_add(crate::BOOT_GRACE)),
            ..kept
        },
        "{name}: the ended row's contents were not replaced by their digests beside its \
         raised retention end, or something else changed"
    );
    for kept in untouched_ends {
        assert_eq!(
            jobs.get_job(&kept.id).expect("reads"),
            Some(JobRecord {
                spec: JobSpec {
                    files: secret_digests(),
                    ..secret_spec()
                },
                ..kept.clone()
            }),
            "{name}: {} was not digested with its retention end left as it was",
            kept.id
        );
    }
    for key in ["job/old-ended", "job/old-late", "job/old-no-retention"] {
        let row = store.get(key).expect("reads").expect("kept").value;
        assert!(
            !holds(&row, TOKEN) & !holds(&row, PUT_URL),
            "{name}: {key} still holds a file's contents after the boot"
        );
    }
    assert_ne!(
        store.get("job/old-bare").expect("reads"),
        bare_row,
        "{name}: the bare row's retention end was not raised"
    );
    assert_eq!(
        jobs.get_job(&bare.id)
            .expect("reads")
            .map(|job| job.spec.files),
        Some(JobFiles::default()),
        "{name}: an ended job without files had its files changed"
    );
    assert_eq!(
        store.get("job/old-running").expect("reads"),
        live_row,
        "{name}: the running job's row was written"
    );
    assert_eq!(
        jobs.get_job(&live)
            .expect("reads")
            .map(|job| job.spec.files),
        Some(JobFiles::Contents(secret_files())),
        "{name}: the running job lost its files' contents"
    );
    assert_eq!(
        jobs.raise_at_boot(boot),
        Ok(0),
        "{name}: a second boot wrote a row again"
    );
}
