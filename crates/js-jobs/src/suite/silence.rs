//! The reads and the upkeep of the presence table that a placement over rows relies on:
//! [`crate::JobStore::nodes`] (every readable row) and
//! [`crate::JobStore::forget_silent_nodes`] (the rows of hosts that went silent and hold
//! nothing).

use std::time::Duration;

use js_state::conformance::Backend;
use js_state::{Store, StoreError};
use js_types::HostId;

use super::pools::hello_in;
use super::{Rig, cancelled, host, rig, share};
use crate::codec::{FORMAT, LATER_FORMAT, encode_cordon, encode_taint};
use crate::interfere::INJECTED;
use crate::record::ScaleMark;
use crate::{EndReason, HostTaint, JobStoreError};

/// `secs` seconds.
fn secs(secs: u64) -> Duration {
    Duration::from_secs(secs)
}

/// Says the hello of `name` on `rig`.
fn hello_of<S: Store>(rig: &Rig<S>, name: &str) {
    rig.jobs
        .hello_node(&host(name), &hello_in("pool-a"))
        .expect("a hello");
}

/// Whether the row of `name` is there.
fn has_row<S: Store>(rig: &Rig<S>, name: &str) -> bool {
    rig.jobs.read_node(&host(name)).expect("reads").is_some()
}

/// [`crate::JobStore::nodes`] lists every row it can read in host order, those heard lately and
/// those long silent alike, with the store's time they were read at, leaves out a row it cannot
/// read, and fails with the store's clock. Catches a list that drops a silent row (as the live
/// list does), one that fails on a row it cannot read, and one that reads no clock.
pub fn nodes_lists_every_readable_row_heard_or_not<B: Backend>(backend: &B)
where
    B::Store: 'static,
{
    let name = "nodes_lists_every_readable_row_heard_or_not";
    let rig = rig(backend);
    let (now, rows) = rig.jobs.nodes().expect("reads");
    assert_eq!(
        (now, rows.len()),
        (rig.store.now().expect("clock"), 0),
        "{name}: an empty table"
    );
    hello_of(&rig, "host-b");
    rig.clock.advance(secs(100_000));
    hello_of(&rig, "host-a");
    rig.put_raw("node/host-later", vec![LATER_FORMAT]);
    rig.put_raw("node/host-junk", vec![FORMAT]);
    let (now, rows) = rig.jobs.nodes().expect("reads");
    let hosts: Vec<&HostId> = rows.iter().map(|row| &row.record.host).collect();
    assert_eq!(
        hosts,
        vec![&host("host-a"), &host("host-b")],
        "{name}: the rows listed"
    );
    assert_eq!(
        (now, rows[1].record.heard_at < now),
        (rig.store.now().expect("clock"), true),
        "{name}: the time and the silent row"
    );
    assert_eq!(
        rig.jobs.live_nodes(secs(60)).map(|live| live.len()),
        Ok(1),
        "{name}: the live list leaves the silent row out"
    );
    rig.store.fail_clock(true);
    assert_eq!(
        rig.jobs.nodes().map(drop),
        Err(JobStoreError::Store(StoreError::Failed(
            INJECTED.to_owned()
        ))),
        "{name}: a clock that cannot be read"
    );
}

/// [`crate::JobStore::forget_silent_nodes`] deletes the row of a host silent for longer than
/// the given time that holds nothing, and returns those hosts in host order; it keeps a row
/// heard within the time (the boundary included), and a silent row of a host that holds a live
/// allocation, a `taint/<host>` row, a binding with a taint, a cordon or a scale-down mark; a
/// host whose allocation was freed and whose binding holds no taint is forgotten. Catches a
/// forget that deletes a row the host still holds something under, one that keeps a row that
/// holds nothing, and one that counts a freed allocation or an untainted binding as held.
pub fn forget_silent_nodes_deletes_only_the_rows_that_hold_nothing<B: Backend>(backend: &B)
where
    B::Store: 'static,
{
    let name = "forget_silent_nodes_deletes_only_the_rows_that_hold_nothing";
    let rig = rig(backend);
    let silent_for = secs(50);

    hello_of(&rig, "host-edge");
    let heard = rig
        .jobs
        .read_node(&host("host-edge"))
        .expect("reads")
        .expect("a row")
        .record
        .heard_at;
    let now = rig.store.now().expect("the store's clock");
    rig.clock
        .advance(heard.saturating_add(silent_for).since(now));
    assert_eq!(
        rig.jobs.forget_silent_nodes(silent_for),
        Ok(Vec::new()),
        "{name}: a row heard exactly the time ago"
    );
    rig.clock.advance(secs(1));

    let held = [
        "host-alloc",
        "host-taint",
        "host-binding",
        "host-cordon",
        "host-mark",
    ];
    let released = ["host-bare", "host-freed", "host-plain-binding"];
    for text in held.iter().chain(released.iter()) {
        hello_of(&rig, text);
    }
    let live = rig.queue("sn-live");
    rig.jobs
        .place(&live, host("host-alloc"), share())
        .expect("placed");
    let ended = rig.queue("sn-ended");
    rig.jobs
        .place(&ended, host("host-freed"), share())
        .expect("placed");
    rig.jobs.end_job(&ended, cancelled()).expect("ends");
    rig.jobs.free(&ended, EndReason::Cancelled).expect("frees");
    rig.put_raw(
        "taint/host-taint",
        encode_taint(&HostTaint { job: live.clone() }).expect("encodes"),
    );
    rig.put_raw(
        "hosts/binding/host-binding",
        br#"{"version":1,"taint_reason":"residue"}"#.to_vec(),
    );
    rig.put_raw(
        "hosts/binding/host-plain-binding",
        br#"{"version":1}"#.to_vec(),
    );
    rig.put_raw("cordon/host-cordon", encode_cordon(None).expect("encodes"));
    assert_eq!(
        rig.jobs
            .mark_host(&host("host-mark"), ScaleMark::Draining, None),
        Ok(true)
    );

    rig.clock.advance(secs(100));
    hello_of(&rig, "host-fresh");
    let forgotten = rig.jobs.forget_silent_nodes(silent_for).expect("forgets");
    let names: Vec<&str> = forgotten.iter().map(HostId::as_str).collect();
    assert_eq!(
        names,
        vec!["host-bare", "host-edge", "host-freed", "host-plain-binding"],
        "{name}: the hosts forgotten"
    );
    for text in held.iter().chain(["host-fresh"].iter()) {
        assert!(has_row(&rig, text), "{name}: the row of {text} was deleted");
    }
    for text in released.iter().chain(["host-edge"].iter()) {
        assert!(!has_row(&rig, text), "{name}: the row of {text} was kept");
    }
    assert_eq!(
        rig.jobs.forget_silent_nodes(silent_for),
        Ok(Vec::new()),
        "{name}: nothing is left to forget"
    );
}

/// A row that a hello or beat moves between the scan and the delete is kept (the delete is at the
/// version the scan read), and so is a row that another server deleted first; the row of a host
/// whose binding cannot be read is kept while the next silent host's is forgotten; a failed
/// commit is the store's error and deletes nothing. Catches a forget that deletes a row a beat
/// has just refreshed, one that reports a row it did not delete, one that forgets a host whose
/// binding it cannot read, and one that one unreadable row stops for every other host.
pub fn forget_silent_nodes_keeps_a_row_that_moved_and_refuses_an_unreadable_binding<B: Backend>(
    backend: &B,
) where
    B::Store: 'static,
{
    let name = "forget_silent_nodes_keeps_a_row_that_moved_and_refuses_an_unreadable_binding";
    let rig = rig(backend);
    let silent_for = secs(50);
    let a = host("host-a");
    let epoch = rig
        .jobs
        .hello_node(&a, &hello_in("pool-a"))
        .expect("a hello")
        .record
        .epoch;
    rig.clock.advance(secs(100));

    let other = rig.other();
    let beater = a.clone();
    rig.store.before_next(move || {
        other
            .beat_node(&beater, epoch, None, |row| row.state.clone())
            .expect("a beat");
    });
    assert_eq!(
        rig.jobs.forget_silent_nodes(silent_for),
        Ok(Vec::new()),
        "{name}: a beat landed after the scan"
    );
    assert!(
        has_row(&rig, "host-a"),
        "{name}: the beaten row was deleted"
    );

    rig.clock.advance(secs(100));
    let other = rig.other();
    let gone = rig.jobs.read_node(&a).expect("reads").expect("a row");
    rig.store.before_next(move || {
        assert_eq!(other.forget_node(&a, gone.version), Ok(true));
    });
    assert_eq!(
        rig.jobs.forget_silent_nodes(silent_for),
        Ok(Vec::new()),
        "{name}: another server deleted the row first"
    );

    hello_of(&rig, "host-b");
    hello_of(&rig, "host-c");
    rig.clock.advance(secs(100));
    rig.store.fail_next();
    assert_eq!(
        rig.jobs.forget_silent_nodes(silent_for),
        Err(JobStoreError::Store(StoreError::Failed(
            INJECTED.to_owned()
        ))),
        "{name}: a failed commit"
    );
    assert!(has_row(&rig, "host-b"), "{name}: a failed commit deleted");

    rig.put_raw("hosts/binding/host-b", b"not json".to_vec());
    assert_eq!(
        rig.jobs.forget_silent_nodes(silent_for),
        Ok(vec![host("host-c")]),
        "{name}: a binding that cannot be read"
    );
    assert!(
        has_row(&rig, "host-b"),
        "{name}: the row of an unreadable binding"
    );
    rig.store.fail_clock(true);
    assert_eq!(
        rig.jobs.forget_silent_nodes(silent_for).map(drop),
        Err(JobStoreError::Store(StoreError::Failed(
            INJECTED.to_owned()
        ))),
        "{name}: a clock that cannot be read"
    );
}

/// Asserts that [`crate::JobStore::live_nodes_reusing`] takes a row from the rows it is given
/// only at the version the store still holds, and decodes the others: a given row at the
/// stored version comes back as given, one at another version comes back as stored, and a host
/// it was not given comes back as stored; whether a row is live is judged afresh, so a given
/// row whose host went silent past the grace is left out. Catches a reuse that hands back a
/// row written over since, and one that keeps a silent host live.
pub fn live_nodes_reusing_takes_only_rows_at_the_stored_version<B: Backend>(backend: &B)
where
    B::Store: 'static,
{
    let name = "live_nodes_reusing_takes_only_rows_at_the_stored_version";
    let rig = rig(backend);
    let grace = secs(30);
    hello_of(&rig, "host-a");
    hello_of(&rig, "host-b");
    hello_of(&rig, "host-c");
    let stored = |name: &str| {
        rig.jobs
            .read_node(&host(name))
            .expect("reads")
            .expect("a row")
    };
    let (a, b, c) = (stored("host-a"), stored("host-b"), stored("host-c"));
    // Rows given with labels the store does not hold, so that a row taken from them shows.
    let given = |row: &crate::NodeRow, version: js_state::Version| {
        let mut row = row.clone();
        row.record
            .labels
            .insert("given".to_owned(), "yes".to_owned());
        row.version = version;
        (row.record.host.clone(), row)
    };
    let known = [
        given(&a, a.version),
        given(&b, js_state::Version(b.version.0 + 1000)),
    ]
    .into_iter()
    .collect();
    let live = rig.jobs.live_nodes_reusing(grace, &known).expect("reads");
    assert_eq!(
        live,
        vec![known[&host("host-a")].clone(), b.clone(), c.clone()],
        "{name}: the given row at its version, the others as stored"
    );

    rig.clock.advance(grace + secs(1));
    assert_eq!(
        rig.jobs.live_nodes_reusing(grace, &known),
        Ok(Vec::new()),
        "{name}: silent past the grace, given or not"
    );
}
