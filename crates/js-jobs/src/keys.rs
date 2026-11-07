//! The keys of the job store's rows. A job id is the key text as it is: its alphabet holds no
//! `/` and nothing that needs escaping.

use js_state::StoreTime;
use js_types::HostId;

use crate::ids::JobId;

/// `job/<id>`: a job's record.
pub const JOB: &str = "job/";

/// `alloc/<id>`: the allocation of the job with that id. It has the job's id, so a job's
/// allocation is one read, not a scan.
pub const ALLOCATION: &str = "alloc/";

/// `mark/<id>`: the mark of the queued whole-host job with that id, found from its job without
/// a scan. No `idx/` space names it.
pub const MARK: &str = "mark/";

/// `meta/jobs/schema`: the schema version of the job, allocation and mark rows, four bytes,
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

/// The key of the mark of the job `id`.
#[must_use]
pub fn mark(id: &JobId) -> String {
    format!("{MARK}{id}")
}

/// `idx/`: the secondary key spaces. Each entry is a key with an empty value, written in the
/// transaction that writes the row it points at, so it never disagrees with a row the job store
/// wrote. A key under `idx/` that none of the spaces below names (a later build's) is left as
/// it is.
pub(crate) const INDEX: &str = "idx/";

/// `idx/job/active/<id>`: a job that has not ended (queued, placed, starting or running).
pub(crate) const ACTIVE: &str = "idx/job/active/";

/// `idx/job/due/<end>/<id>`: an ended job, `<end>` the nanoseconds of its retention end on the
/// store's clock as twenty digits (zero for an ended row an earlier build wrote without one),
/// so the keys sort by retention end and then by job id.
pub(crate) const DUE: &str = "idx/job/due/";

/// `idx/job/unread/<id>`: a job row this build cannot read (a later build's, or one that does
/// not parse), found by the backfill. Every indexed read of the job table reads these rows too,
/// so it treats them as a scan of the table would.
pub(crate) const UNREAD_JOB: &str = "idx/job/unread/";

/// `idx/alloc/host/<host>/<id>`: a live (booked or releasing) allocation, by host and then job
/// id. A host id holds no `/`.
pub(crate) const ON_HOST: &str = "idx/alloc/host/";

/// `idx/alloc/freed/<at>/<id>`: a freed allocation, `<at>` the nanoseconds of its free as
/// twenty digits, so the keys sort by the free and then by job id.
pub(crate) const FREED: &str = "idx/alloc/freed/";

/// `idx/alloc/unread/<id>`: an allocation row this build cannot read, as [`UNREAD_JOB`].
pub(crate) const UNREAD_ALLOCATION: &str = "idx/alloc/unread/";

/// The width of a time in an index key: the digits of `u64::MAX`.
const STAMP_DIGITS: usize = 20;

/// `time` as an index key's stamp: its nanoseconds, saturating at `u64::MAX`, as
/// [`STAMP_DIGITS`] digits, so stamps sort as the times do.
pub(crate) fn stamp(time: StoreTime) -> String {
    let nanos = u64::try_from(time.as_duration().as_nanos()).unwrap_or(u64::MAX);
    format!("{nanos:0width$}", width = STAMP_DIGITS)
}

/// The active entry of the job `id`.
pub(crate) fn active(id: &JobId) -> String {
    format!("{ACTIVE}{id}")
}

/// The due entry of the job `id`, whose retention ends at `end`.
pub(crate) fn due(end: StoreTime, id: &JobId) -> String {
    format!("{DUE}{}/{id}", stamp(end))
}

/// The first due entry of a job whose retention ends at `at`: a scan of [`DUE`] up to it,
/// excluded, reads exactly the entries that end before `at`.
pub(crate) fn due_below(at: StoreTime) -> String {
    format!("{DUE}{}", stamp(at))
}

/// The on-host entry of the allocation of the job `id`, on `host`.
pub(crate) fn on_host(host: &HostId, id: &JobId) -> String {
    format!("{ON_HOST}{host}/{id}")
}

/// The prefix of every on-host entry of `host`.
pub(crate) fn on_host_prefix(host: &HostId) -> String {
    format!("{ON_HOST}{host}/")
}

/// The freed entry of the allocation of the job `id`, freed at `at`.
pub(crate) fn freed(at: StoreTime, id: &JobId) -> String {
    format!("{FREED}{}/{id}", stamp(at))
}

/// The first freed entry of an allocation freed at `at`, as [`due_below`].
pub(crate) fn freed_below(at: StoreTime) -> String {
    format!("{FREED}{}", stamp(at))
}

/// The unread entry of the row `key`, `None` for a key outside the job and allocation tables.
pub(crate) fn unread(key: &str) -> Option<String> {
    if let Some(rest) = key.strip_prefix(JOB) {
        Some(format!("{UNREAD_JOB}{rest}"))
    } else {
        key.strip_prefix(ALLOCATION)
            .map(|rest| format!("{UNREAD_ALLOCATION}{rest}"))
    }
}

/// The row an index entry points at: `job/<id>` or `alloc/<id>`. `None` for a key no space of
/// this build names, which is left as it is.
pub(crate) fn indexed_row(entry: &str) -> Option<String> {
    let stamped = |rest: &str| {
        let (stamp, id) = rest.split_at_checked(STAMP_DIGITS)?;
        let id = id.strip_prefix('/')?;
        stamp
            .bytes()
            .all(|byte| byte.is_ascii_digit())
            .then(|| id.to_owned())
    };
    let (table, id) = if let Some(id) = entry.strip_prefix(ACTIVE) {
        (JOB, Some(id.to_owned()))
    } else if let Some(rest) = entry.strip_prefix(DUE) {
        (JOB, stamped(rest))
    } else if let Some(id) = entry.strip_prefix(UNREAD_JOB) {
        (JOB, Some(id.to_owned()))
    } else if let Some(rest) = entry.strip_prefix(ON_HOST) {
        (
            ALLOCATION,
            rest.split_once('/').map(|(_, id)| id.to_owned()),
        )
    } else if let Some(rest) = entry.strip_prefix(FREED) {
        (ALLOCATION, stamped(rest))
    } else if let Some(id) = entry.strip_prefix(UNREAD_ALLOCATION) {
        (ALLOCATION, Some(id.to_owned()))
    } else {
        return None;
    };
    id.filter(|id| !id.is_empty())
        .map(|id| format!("{table}{id}"))
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use js_state::StoreTime;
    use js_types::HostId;

    use super::*;

    fn id(text: &str) -> JobId {
        JobId::parse(text).expect("a valid job id")
    }

    // Catches an entry mapped back to the wrong row, a stamp that is not fixed-width (which
    // would sort 10 before 9), and a key of an unnamed space or a malformed stamp taken as
    // one of this build's entries.
    #[test]
    fn every_entry_names_its_row_and_nothing_else_is_taken_for_one() {
        let at = StoreTime::from_duration(Duration::from_nanos(9));
        let host = HostId::new("host-a").expect("a host");
        let job = id("j-1");
        for (entry, row) in [
            (active(&job), "job/j-1"),
            (due(at, &job), "job/j-1"),
            (unread("job/j-1").expect("a job key"), "job/j-1"),
            (on_host(&host, &job), "alloc/j-1"),
            (freed(at, &job), "alloc/j-1"),
            (unread("alloc/j-1").expect("an allocation key"), "alloc/j-1"),
        ] {
            assert_eq!(indexed_row(&entry).as_deref(), Some(row), "{entry}");
        }
        assert_eq!(due(at, &job), "idx/job/due/00000000000000000009/j-1");
        assert!(stamp(at) < stamp(StoreTime::from_duration(Duration::from_nanos(10))));
        assert!(due(at, &job) < due_below(at.saturating_add(Duration::from_nanos(1))));
        assert!(due_below(at) < due(at, &job));
        assert!(freed_below(at) < freed(at, &job));
        assert_eq!(on_host_prefix(&host), "idx/alloc/host/host-a/");
        assert_eq!(
            stamp(StoreTime::from_duration(Duration::from_secs(u64::MAX))),
            u64::MAX.to_string()
        );
        for other in [
            "idx/later/x",
            "idx/job/active/",
            "idx/job/due/123/j-1",
            "idx/job/due/0000000000000000000x/j-1",
            "idx/job/due/00000000000000000009j-1",
            "idx/alloc/host/host-a",
            "job/j-1",
            "mark/j-1",
        ] {
            assert_eq!(indexed_row(other), None, "{other}");
        }
        assert_eq!(unread("meta/jobs/schema"), None);
        assert_eq!(mark(&job), "mark/j-1");
        assert_eq!(unread(&mark(&job)), None, "a mark row has no unread entry");
    }
}
