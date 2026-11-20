//! The writes of the bindings kept in a store that servers share, each conditional on the
//! version of its row that the server last read or wrote: a save that loses a version leaves the
//! row as the other server wrote it, and a start's restore never writes over a row changed since
//! its scan. Two binding stores over one store stand for two servers; nothing sleeps.

use std::collections::BTreeMap;
use std::sync::Arc;

use js_agent_wire::Taint;
use js_jobs::interfere::Interfere;
use js_registry::{Binding, Credential};
use js_state::fake::{Fake, Flaw};
use js_state::{Cond, ManualClock as StoreClock, Store, Txn};
use js_types::HostId;

use super::{BindingStore, epoch_clock};

/// A store shared by servers.
fn shared_store() -> Arc<Interfere<Fake>> {
    let clock = Arc::new(StoreClock::default());
    Arc::new(Interfere::new(Fake::with_flaw(clock, Flaw::None)))
}

/// A server over `store`, which reads the rows the store holds.
fn server(store: &Arc<Interfere<Fake>>) -> BindingStore {
    let dynamic: Arc<dyn Store> = store.clone();
    BindingStore::in_store(dynamic, epoch_clock()).unwrap().0
}

fn host() -> HostId {
    HostId::new("host-a").unwrap()
}

/// The binding of the host to `token`, tainted when `taint`.
fn bound(token: &str, taint: bool) -> Binding {
    let taint = taint.then(|| Taint {
        reason: "residue".to_owned(),
        run_id: None,
        check: None,
    });
    Binding::new(Credential::of_token(token), None).with_taint(taint)
}

/// The reason of the taint the store's row of the host holds.
fn stored_taint(store: &Interfere<Fake>) -> Option<String> {
    let row = store.get("hosts/binding/host-a").unwrap().unwrap();
    let value: serde_json::Value = serde_json::from_slice(&row.value).unwrap();
    value["taint_reason"].as_str().map(str::to_owned)
}

/// Asserts that a save that loses the version of a row (another server wrote it since this one
/// read it) writes nothing for that host, answers the binding the row holds, and writes the
/// host's next binding at the version it then read; and that the host's neighbour, saved with
/// it, is written all the same. It catches a save that puts at any version, which erases the
/// taint the other server stored, and one that gives up the whole save for one lost row.
#[test]
fn a_save_that_loses_a_version_keeps_the_row_and_writes_the_others() {
    let store = shared_store();
    let (a, b) = (server(&store), server(&store));
    let other = HostId::new("host-b").unwrap();
    let both = |token: &str, taint: bool| {
        BTreeMap::from([
            (host(), bound(token, taint)),
            (other.clone(), bound("b", false)),
        ])
    };
    assert!(a.save(|| both("a", false), &|_| false).unwrap().is_empty());

    // The other server stores a taint in the row this server wrote.
    b.stored(&host()).unwrap();
    let change = b.change(&host(), bound("a", true));
    let revision = b.commit(change.writes.clone()).unwrap();
    b.applied(&change, revision);
    assert_eq!(stored_taint(&store).as_deref(), Some("residue"));

    // This server's registry holds a cordon on `host-a` and a taint-free binding of `host-b`
    // that changed: the first put is refused, the second is written.
    let cordoned = bound("a", false).with_cordon(Some(9_000));
    let changed = BTreeMap::from([(host(), cordoned), (other.clone(), bound("c", false))]);
    let lost = a.save(|| changed.clone(), &|_| false).unwrap();
    assert_eq!(lost.len(), 1);
    assert_eq!((&lost[0].0, lost[0].1.taint().is_some()), (&host(), true));
    assert_eq!(stored_taint(&store).as_deref(), Some("residue"));
    let row = store.get("hosts/binding/host-b").unwrap().unwrap();
    assert!(
        String::from_utf8(row.value)
            .unwrap()
            .contains(&Credential::of_token("c").to_hex())
    );

    // Held as read, the host's binding is written at the version of the row now.
    let taken = bound("a", true).with_cordon(Some(9_000));
    let again = BTreeMap::from([(host(), taken), (other, bound("c", false))]);
    assert!(a.save(|| again.clone(), &|_| false).unwrap().is_empty());
    let row = store.get("hosts/binding/host-a").unwrap().unwrap();
    assert!(
        String::from_utf8(row.value)
            .unwrap()
            .contains("cordon_until_ms")
    );
    assert_eq!(stored_taint(&store).as_deref(), Some("residue"));
}

/// Asserts that a put that loses its version to another server's delete of the row reads it
/// absent and is written at `Cond::Absent` by the next save, and that a delete that loses its
/// version (another server wrote the row of a host this registry let go of) leaves that row and
/// stops holding the host: a later save writes nothing for it. It catches a delete at any
/// version, which takes a binding another server now serves.
#[test]
fn a_put_over_a_deleted_row_is_made_again_and_a_delete_over_a_rewritten_row_is_not_made() {
    let store = shared_store();
    let a = server(&store);
    let one = BTreeMap::from([(host(), bound("a", false))]);
    a.save(|| one.clone(), &|_| false).unwrap();
    store
        .inner()
        .txn(Txn::new().delete("hosts/binding/host-a", Cond::Any))
        .unwrap()
        .unwrap();
    let cordoned = BTreeMap::from([(host(), bound("a", false).with_cordon(Some(9_000)))]);
    assert!(a.save(|| cordoned.clone(), &|_| false).unwrap().is_empty());
    assert!(store.get("hosts/binding/host-a").unwrap().is_none());
    assert!(a.save(|| cordoned.clone(), &|_| false).unwrap().is_empty());
    assert!(store.get("hosts/binding/host-a").unwrap().is_some());

    // Another server rewrites the row; this registry lets the host go.
    let b = server(&store);
    b.stored(&host()).unwrap();
    let change = b.change(&host(), bound("a", true));
    let revision = b.commit(change.writes.clone()).unwrap();
    b.applied(&change, revision);
    let lost = a.save(BTreeMap::new, &|_| false).unwrap();
    assert_eq!(lost.len(), 1);
    assert_eq!(stored_taint(&store).as_deref(), Some("residue"));
    let commits = store.commit_stats().txns;
    assert!(a.save(BTreeMap::new, &|_| false).unwrap().is_empty());
    assert_eq!(
        store.commit_stats().txns,
        commits,
        "the host is no longer held"
    );
}

/// Asserts that a start's restore writes each binding at the version the start read and is
/// refused, writing nothing, when another server wrote one of the rows since: the start fails
/// and the next one reads the rows again. It catches a restore at `Cond::Any`, which puts the
/// job taints' bindings over a taint another server stored between the scan and the write.
#[test]
fn a_restore_is_refused_over_a_row_another_server_wrote_since_the_scan() {
    let store = shared_store();
    let first = server(&store);
    first
        .save(|| BTreeMap::from([(host(), bound("a", false))]), &|_| false)
        .unwrap();
    let starting = server(&store);
    let rival = server(&store);
    rival.stored(&host()).unwrap();
    let change = rival.change(&host(), bound("a", true));
    let revision = rival.commit(change.writes.clone()).unwrap();
    rival.applied(&change, revision);

    let restored = BTreeMap::from([(host(), bound("a", false).with_cordon(Some(9_000)))]);
    let refused = starting.restore(&restored).unwrap_err();
    assert!(refused.to_string().contains("refused"), "{refused}");
    assert_eq!(stored_taint(&store).as_deref(), Some("residue"));
}
