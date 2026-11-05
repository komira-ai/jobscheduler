//! The lengths of a job's live-log streams (`docs/design/live-logs.md`, section 2): written with
//! the result that reports them, read back unchanged (after a restart too), absent from a result
//! that reported none, kept in the row as a field an older build carries through a rewrite, and
//! deleted with the job by the retention sweep.

use std::sync::Arc;

use js_state::conformance::Backend;

use super::{output, rig};
use crate::codec::{field_of, job_tag};
use crate::keys;
use crate::{JobEnd, JobStore, LogLengths, Output, RandomIds, Retention};

/// What the agent reported of a job that printed 2.5 MiB to standard output and 100 bytes to
/// standard error, dropping nothing.
const LENGTHS: LogLengths = LogLengths {
    stdout_bytes: 2_621_440,
    stderr_bytes: 100,
    stdout_dropped: 0,
    stderr_dropped: 0,
};

/// An exit with code 0 whose output reported `logs`.
fn exit_with(logs: Option<LogLengths>) -> JobEnd {
    JobEnd::Exited {
        exit_code: 0,
        output: Output { logs, ..output() },
    }
}

/// A result that reports the lengths is read back with them, from the end's answer, from a
/// read, and after the store restarts; one that reports none reads with none. The stored row
/// holds them in the one field `RESULT_LOGS`, which a build from before the field keeps as a
/// field it does not know (`unknown_fields_survive_every_rewrite`), so it reads and rewrites
/// the row as it is. Past the retention end the sweep deletes the row, and the lengths with it.
pub fn the_log_lengths_are_kept_with_the_result_and_swept_with_it<B: Backend>(backend: &B) {
    let name = "the_log_lengths_are_kept_with_the_result_and_swept_with_it";
    let rig = rig(backend);
    let with = rig.running("with-logs");
    let without = rig.running("without-logs");
    let ended = rig
        .jobs
        .end_job(&with, exit_with(Some(LENGTHS)))
        .expect("ends");
    let lengths = |job: &crate::JobRecord| job.result.as_ref().and_then(|result| result.logs);
    assert_eq!(lengths(&ended), Some(LENGTHS), "{name}: the end lost them");
    let bare = rig.jobs.end_job(&without, exit_with(None)).expect("ends");
    assert_eq!(
        lengths(&bare),
        None,
        "{name}: a result without them has some"
    );
    let row = rig.raw(&keys::job(&with)).expect("written").value;
    let words: Vec<u8> = [2_621_440u64, 100, 0, 0]
        .iter()
        .flat_map(|word| word.to_be_bytes())
        .collect();
    assert_eq!(
        field_of(&row, job_tag::RESULT_LOGS),
        Some(words),
        "{name}: the row does not hold them in their field"
    );
    let bare_row = rig.raw(&keys::job(&without)).expect("written").value;
    assert_eq!(
        field_of(&bare_row, job_tag::RESULT_LOGS),
        None,
        "{name}: a result without them wrote the field"
    );

    let super::Rig { store, jobs, clock } = rig;
    drop(jobs);
    let store = Arc::into_inner(store)
        .and_then(|wrapped| wrapped.into_inner())
        .expect("nothing else holds the store");
    let jobs = JobStore::new(Arc::new(backend.restart(store)), RandomIds).expect("reopens");
    let read = |id| jobs.get_job(id).expect("reads").expect("present");
    assert_eq!(read(&with), ended, "{name}: the restart changed the record");
    assert_eq!(
        read(&without),
        bare,
        "{name}: the restart changed the bare one"
    );

    clock.advance(Retention::DEFAULT.window());
    let mut swept = jobs.tick().expect("sweeps");
    swept.sort();
    let mut want = vec![with.clone(), without.clone()];
    want.sort();
    assert_eq!(swept, want, "{name}: the sweep did not delete both");
    assert_eq!(
        jobs.get_job(&with),
        Ok(None),
        "{name}: the lengths outlived the result"
    );
}
