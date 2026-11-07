//! The whole-node reservation ledger across a restart under `--placement js-sched`, over
//! the rig of the mark tests set up as the server sets it up for that placement: js-sched's
//! engine keeps the queue, and the ledger the tick hands the lease table is written from it.
//! A server is stopped and another started over the same lease file with `--boot resume`,
//! on the same manual clock; the host beats every five seconds and the consumer polls as
//! it does in the program. The lease file is also read and written with the previous
//! release's own reader and writer (the lease crate's cross-release fixture), in both
//! directions. Nothing here sleeps and no test asserts how long anything takes on a wall
//! clock.

use std::fs;

use js_lease::{ExclusionEnd, ReservationRecord, Timestamp};
use js_types::RequestId;

use super::whole_view_tests::{GOLDEN, Rig, T0};
use crate::reservations::RESERVATION_BOOT_GRACE;

/// The previous release's reader and writer of the lease file: its `store.rs` types, as
/// the lease crate's cross-release test keeps them, parsed and written as that release's
/// `store::read` and `store::write` do.
mod previous {
    use std::collections::BTreeMap;

    use js_lease::LeaseState;
    use serde::{Deserialize, Serialize};

    include!("../../../js-lease/tests/lease/cross_release/previous_store.rs");

    /// Reads `text` as the previous release's `store::read` does.
    pub(super) fn read(text: &str) -> Document {
        toml::from_str(text).unwrap()
    }

    /// Writes `document` as the previous release's `store::write` does.
    pub(super) fn write(document: &Document) -> String {
        toml::to_string(document).unwrap()
    }

    /// A row as that release writes it, holding `host` from `at` until `deadline`, at
    /// `place` since `since`, with `give_ups` and one excluded host until `until`.
    pub(super) fn row(
        request_id: &str,
        held: Option<(&str, u64, u64)>,
        (place, since): (u32, u64),
        give_ups: u32,
        until: Option<(&str, u64)>,
    ) -> StoredReservation {
        StoredReservation {
            request_id: request_id.to_string(),
            host: held.map(|(host, _, _)| host.to_string()),
            reserved_at_ms: held.map(|(_, at, _)| at),
            deadline_ms: held.map(|(_, _, deadline)| deadline),
            assigned: false,
            tail_end_ms: None,
            queue_place: Some(place),
            queued_since_ms: Some(since),
            give_ups,
            prepare_failures: 0,
            exclusion: until
                .into_iter()
                .map(|(host, until_ms)| StoredExclusion {
                    host: host.to_string(),
                    until_ms: Some(until_ms),
                    forever: false,
                })
                .collect(),
            extra: toml::Table::new(),
        }
    }

    /// A file of no lease and `rows`, as that release writes it.
    pub(super) fn file(rows: Vec<StoredReservation>) -> Document {
        Document {
            version: 1,
            term: 3,
            next_sequence: 1,
            lease_count: 0,
            reservation_count: u64::try_from(rows.len()).unwrap(),
            lease: Vec::new(),
            reservation: rows,
            extra: toml::Table::new(),
        }
    }
}

fn id(name: &str) -> RequestId {
    RequestId::new(name).unwrap()
}

fn at(millis: u64) -> Timestamp {
    Timestamp::from_millis(millis)
}

const GRACE_MS: u64 = RESERVATION_BOOT_GRACE.as_secs() * 1000;

/// A lease file as an earlier server wrote it: request `r1` first presented at `T0`, at the
/// front of the wait list, holding no host, with one drain give-up.
const GAVE_UP_ONCE: &str = "version = 1\nterm = 3\nnext_sequence = 1\nlease_count = 0\n\
    reservation_count = 1\n\n\
    [[reservation]]\nrequest_id = \"r1\"\nqueue_place = 0\nqueued_since_ms = 1000000\n\
    give_ups = 1\n";

/// The row of request `name` in `rig`'s ledger.
fn row(rig: &Rig, name: &str) -> ReservationRecord {
    let ledger = rig.registry.reservation_ledger();
    let found = ledger.into_iter().find(|row| row.request_id == name);
    found.unwrap()
}

/// Asserts that the ledger the engine writes is read back by a second server over the same
/// file under js-sched, and that the engine then keeps the restored mark. A server
/// comes up over a file in which `r1` has one give-up and its place; the host is busy, and
/// `r1`, presented again, marks it with the thirty-second deadline of a request that has
/// given one up. The tick writes the ledger, and a second server over the same file with
/// `--boot resume` gets back the mark on the same host with the same instant and tail and
/// with its deadline at the boot floor (the stored one has passed it, so it is raised, not
/// restarted), the place and the give-up count. Presented again, the engine holds the same
/// mark and gives it up at the floor, not a deadline restarted from the presentation: the
/// count is then two and the host is backed off. It catches a ledger written without the
/// engine's mark or count, a restart that resets the count or the place, and a restored
/// mark the engine replaces by a new one.
#[tokio::test]
async fn reservation_ledger_rebinds_after_restart_on_the_sched_path() {
    let mut rig = Rig::over_file_in("sched_rebinds", GAVE_UP_ONCE, true).await;
    rig.lease().await;
    rig.ask("r1");
    let before = rig.registry.reservations();
    assert_eq!(before.len(), 1);
    let mark = &before[0];
    assert_eq!((&mark.request, mark.host.as_str()), (&id("r1"), "a"));
    assert_eq!((mark.at, mark.deadline), (at(T0), at(T0 + 30_000)));
    assert_eq!(row(&rig, "r1").give_ups, 1);
    rig.shared.tick().await;

    let mut back = rig.restart(true).await;
    let after = back.registry.reservations();
    assert_eq!(after.len(), 1);
    let kept = &after[0];
    assert_eq!((&kept.request, &kept.host), (&mark.request, &mark.host));
    assert_eq!((kept.at, kept.tail_end), (mark.at, mark.tail_end));
    assert_eq!(kept.deadline, at(T0 + GRACE_MS));
    assert_eq!(back.registry.queue_position(&id("r1")), Some(0));
    assert_eq!(row(&back, "r1").give_ups, 1);
    assert_eq!(back.registry.dropped_reservations(), 0);

    back.ask("r1");
    assert_eq!(back.registry.reservations(), after);
    back.pass(GRACE_MS / 1000 - 5, &["r1"]);
    assert_eq!(back.registry.reservations(), after);
    back.pass(5, &["r1"]);
    assert!(
        back.registry.reservations().is_empty(),
        "given up at the floor"
    );
    let gave_up = row(&back, "r1");
    assert_eq!(gave_up.give_ups, 2);
    assert_eq!(gave_up.exclusions.len(), 1);
    let backed_off = ExclusionEnd::At(at(T0 + GRACE_MS + 300_000));
    assert_eq!(gave_up.exclusions[0].until, backed_off);
    assert_eq!(back.registry.queue_position(&id("r1")), Some(0));
}

/// Asserts the host listing under js-sched on a host marked for twenty seconds: `mark`,
/// `reserved_for` and `drain_deadline_ms` are the three values the golden listing of the
/// registry's placement shows (the same pool, lease and request), and the deadline is the
/// engine's. A host nobody reserved shows none of the three keys. It catches a listing that
/// reads the registry's marks where the engine's moved, and a deadline that is not the
/// mark's.
#[tokio::test]
async fn hosts_api_and_d55_goldens_unchanged() {
    let mut rig = Rig::sched("sched_listing").await;
    let plain = rig.hosts().await;
    for key in ["mark", "reserved_for", "drain_deadline_ms"] {
        assert!(!plain.contains(key), "{plain}");
    }
    rig.lease().await;
    rig.ask("r1");
    rig.pass(20, &["r1"]);
    let body: serde_json::Value = serde_json::from_str(&rig.hosts().await).unwrap();
    let golden: serde_json::Value = serde_json::from_str(GOLDEN).unwrap();
    for key in ["mark", "reserved_for", "drain_deadline_ms"] {
        assert_eq!(body["hosts"][0][key], golden["hosts"][0][key], "{key}");
    }
    let deadline = rig.registry.reservations()[0].deadline.millis();
    assert_eq!(body["hosts"][0]["drain_deadline_ms"], deadline);
}

/// Asserts the lease file in both directions across releases under js-sched (rule B8).
/// This build's file, with a mark and a place the engine made, is read by the previous
/// release's reader with no field it does not know, in the file or in the row, and every
/// field it knows holds the mark; the file that release writes back resumes here with the
/// same mark and place. A file the previous release wrote itself, with a mark, a place, a
/// give-up and an exclusion, resumes here under js-sched with each of them. It catches a
/// field added, renamed or required by the ledger written from the engine.
#[tokio::test]
async fn the_previous_release_reads_the_new_ledger_and_the_reverse() {
    let mut rig = Rig::sched("sched_cross_out").await;
    rig.lease().await;
    rig.ask("r1");
    rig.shared.tick().await;
    let text = fs::read_to_string(rig.file()).unwrap();
    let document = previous::read(&text);
    assert!(document.extra.is_empty(), "{text}");
    assert_eq!(document.reservation_count, 1);
    assert_eq!(document.lease.len(), 1);
    let written = &document.reservation[0];
    assert!(written.extra.is_empty(), "{text}");
    assert_eq!(written.request_id, "r1");
    assert_eq!(written.host.as_deref(), Some("a"));
    let held = (
        written.reserved_at_ms,
        written.deadline_ms,
        written.assigned,
    );
    assert_eq!(held, (Some(T0), Some(T0 + 30_000), false));
    let queued = (written.queue_place, written.queued_since_ms);
    assert_eq!(queued, (Some(0), Some(T0)));
    let counts = (written.give_ups, written.prepare_failures);
    assert_eq!(counts, (0, 0));
    assert!(written.exclusion.is_empty());
    let marks = rig.registry.reservations();
    drop(rig);

    let rewritten = previous::write(&document);
    let back = Rig::over_file_in("sched_cross_back", &rewritten, true).await;
    let kept = back.registry.reservations();
    assert_eq!(kept.len(), 1);
    assert_eq!(
        (&kept[0].request, &kept[0].host),
        (&marks[0].request, &marks[0].host)
    );
    assert_eq!(kept[0].at, marks[0].at);
    assert_eq!(back.registry.queue_position(&id("r1")), Some(0));
    drop(back);

    let rows = vec![
        previous::row(
            "r9",
            Some(("a", T0, T0 + 30_000)),
            (0, T0),
            1,
            Some(("a", T0 + 400_000)),
        ),
        previous::row("r10", None, (1, T0), 0, None),
    ];
    let old = previous::write(&previous::file(rows));
    let resumed = Rig::over_file_in("sched_cross_in", &old, true).await;
    let marks = resumed.registry.reservations();
    assert_eq!(marks.len(), 1);
    assert_eq!(
        (&marks[0].request, marks[0].host.as_str()),
        (&id("r9"), "a")
    );
    assert_eq!(
        (marks[0].at, marks[0].deadline),
        (at(T0), at(T0 + GRACE_MS))
    );
    assert_eq!(resumed.registry.queue_position(&id("r9")), Some(0));
    assert_eq!(resumed.registry.queue_position(&id("r10")), Some(1));
    let r9 = row(&resumed, "r9");
    assert_eq!(r9.give_ups, 1);
    assert_eq!(r9.exclusions.len(), 1);
    assert_eq!(r9.exclusions[0].until, ExclusionEnd::At(at(T0 + 400_000)));
    assert_eq!(resumed.registry.dropped_reservations(), 0);
}
