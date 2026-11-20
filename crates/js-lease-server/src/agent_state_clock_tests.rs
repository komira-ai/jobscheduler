//! The cordon end of a binding kept in a store that servers share: stored on the store's
//! clock, held on each server's own, and never written or read without the store's clock.
//! The servers' and the store's clocks are manual and set apart; nothing sleeps.

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Duration;

use js_jobs::interfere::Interfere;
use js_registry::{Binding, Credential};
use js_state::fake::{Fake, Flaw};
use js_state::{ManualClock as StoreClock, Store, StoreError};
use js_types::{HostId, ManualClock, Timestamp};

use super::BindingStore;
use crate::clock::SharedClock;

/// Where the store's clock reads, in milliseconds since the Unix epoch.
const STORE_MS: u64 = 10_000_000;

/// The time a cordon has left when a test places it.
const LEFT_MS: u64 = 60_000;

/// A store shared by servers, its clock at [`STORE_MS`].
fn shared_store() -> Arc<Interfere<Fake>> {
    let clock = Arc::new(StoreClock::default());
    clock.advance(Duration::from_millis(STORE_MS));
    Arc::new(Interfere::new(Fake::with_flaw(clock, Flaw::None)))
}

/// A server's clock at `ms`, and the shared handle the bindings read it through.
fn server_clock(ms: u64) -> (ManualClock, SharedClock) {
    let clock = ManualClock::new(Timestamp::from_millis(ms));
    (clock.clone(), SharedClock::new(clock))
}

fn host() -> HostId {
    HostId::new("host-a").unwrap()
}

/// The binding of a host cordoned until `until_ms`.
fn cordoned(until_ms: u64) -> Binding {
    Binding::new(Credential::of_token("token-a"), None).with_cordon(Some(until_ms))
}

/// The store's clock now, in milliseconds since the Unix epoch.
fn store_ms(store: &Interfere<Fake>) -> u64 {
    u64::try_from(store.now().unwrap().as_duration().as_millis()).unwrap()
}

/// The `cordon_until_ms` the store's row of the host holds.
fn stored_end(store: &Interfere<Fake>) -> Option<u64> {
    let row = store.get("hosts/binding/host-a").unwrap().unwrap();
    let value: serde_json::Value = serde_json::from_slice(&row.value).unwrap();
    value["cordon_until_ms"].as_u64()
}

/// Asserts that a cordon end one server writes is the store's time and each server reads it on
/// its own clock: a server whose clock is 6 000 s behind the store's commits a binding with a
/// minute left, and the row holds the store's clock plus that minute; a second server, 3 000 s
/// behind, reads the minute left on its own clock at its start and by key at a hello; and the
/// tick's save of a renewed cordon, made after the first server's clock was stepped an hour on,
/// stores the store's time again. It catches an end stored as one server's clock reads (the
/// second server would hold the cordon 3 000 s too long), an end read back without moving it
/// onto the reader's clock, and a save that puts an end on the store's clock by a skew it did
/// not read again.
#[test]
fn a_cordon_end_is_stored_on_the_store_clock_and_read_on_each_servers_own() {
    let store = shared_store();
    let dynamic: Arc<dyn Store> = store.clone();
    let (first_clock, first) = server_clock(4_000_000);
    let (a, _) = BindingStore::in_store(Arc::clone(&dynamic), first).unwrap();
    let change = a.change(&host(), cordoned(4_000_000 + LEFT_MS));
    let revision = a.commit(change.writes.clone()).unwrap();
    a.applied(&change, revision);
    assert_eq!(stored_end(&store), Some(STORE_MS + LEFT_MS));

    // What the store's clock leaves of the cordon (the commit's sync moved that clock on) is
    // what the second server holds left on its own.
    let left = STORE_MS + LEFT_MS - store_ms(&store);
    let (_, second) = server_clock(7_000_000);
    let (b, read) = BindingStore::in_store(Arc::clone(&dynamic), second).unwrap();
    assert_eq!(read[&host()].cordon_until_ms(), Some(7_000_000 + left));
    let by_key = b.stored(&host()).unwrap().unwrap();
    assert_eq!(by_key.cordon_until_ms(), Some(7_000_000 + left));

    first_clock.set(Timestamp::from_millis(4_000_000 + 3_600_000));
    let renewed = cordoned(4_000_000 + 3_600_000 + 2 * LEFT_MS);
    let saved_at = store_ms(&store);
    a.save(|| BTreeMap::from([(host(), renewed)]), &|_| false)
        .unwrap();
    assert_eq!(stored_end(&store), Some(saved_at + 2 * LEFT_MS));
}

/// Asserts that a save whose bindings did not change writes nothing however the skew moved: the
/// bindings held are compared on the server's clock, so a skew read again at each save does not
/// rewrite a cordoned binding. It catches a save that compares what it would store with what it
/// read, which would rewrite every cordoned binding at every tick.
#[test]
fn a_save_of_unchanged_bindings_writes_nothing_when_the_skew_moves() {
    let store = shared_store();
    let dynamic: Arc<dyn Store> = store.clone();
    let (clock, shared) = server_clock(4_000_000);
    let (a, read) = BindingStore::in_store(Arc::clone(&dynamic), shared).unwrap();
    assert!(read.is_empty());
    let bindings = BTreeMap::from([(host(), cordoned(4_000_000 + LEFT_MS))]);
    a.save(|| bindings.clone(), &|_| false).unwrap();
    let txns = store.commit_stats().txns;
    clock.set(Timestamp::from_millis(4_000_250));
    a.save(|| bindings.clone(), &|_| false).unwrap();
    assert_eq!(store.commit_stats().txns, txns);
}

/// Asserts that a store that cannot read its clock (its database stopped) refuses every read
/// and write of the bindings that would move a time between the clocks, and writes nothing: the
/// start's read, a read by key, a save, and the reading a cordon's commit is made after. Once
/// the clock reads again, the save writes. It catches a cordon end put on the store's clock by
/// a reading that failed (the epoch, or a skew of zero), which another server would read as a
/// cordon that lapsed or one that holds for hours.
#[test]
fn a_store_that_cannot_read_its_clock_moves_no_binding_between_the_clocks() {
    let store = shared_store();
    let dynamic: Arc<dyn Store> = store.clone();
    let (_, shared) = server_clock(4_000_000);
    let (a, _) = BindingStore::in_store(Arc::clone(&dynamic), shared.clone()).unwrap();
    let stopped = StoreError::Disconnected("the database is stopped".to_owned());
    store.fail_with(stopped.clone());
    store.fail_clock(true);
    let before = store.scan_prefix("").unwrap();

    let started = BindingStore::in_store(Arc::clone(&dynamic), shared).err();
    assert_eq!(started, Some(stopped.to_string()));
    assert!(a.stored(&host()).is_err());
    assert!(a.measure().is_err());
    let bindings = BTreeMap::from([(host(), cordoned(4_000_000 + LEFT_MS))]);
    assert!(a.save(|| bindings.clone(), &|_| false).is_err());
    assert_eq!(store.scan_prefix("").unwrap(), before);

    store.fail_clock(false);
    let saved_at = store_ms(&store);
    a.save(|| bindings.clone(), &|_| false).unwrap();
    assert_eq!(stored_end(&store), Some(saved_at + LEFT_MS));
}
