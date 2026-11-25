//! The operator's retirement of a host id ([`Agents::retire`]) over a server whose bindings are
//! kept in the state store and that holds no job tables: the registry's record and the stored
//! binding are what count toward the pool, and both go. The rig is the one of the parent
//! module.

use super::*;
use crate::agents::retire::RetireFault;

/// Asserts that a pool full of four hosts refuses a fifth id `pool-full`; that a live host is
/// kept (`Kept::Live`) with its binding; that once the hosts are lost, the retirement of one
/// deletes its stored binding and the registry's record of it, so the fifth id is admitted;
/// that a repeat answers `false` and commits nothing; and that a second server over the same
/// store, whose registry never held the hosts, deletes the stored binding of another of them,
/// while the first server keeps its own record of that host until the same call is made there,
/// which forgets the record and deletes no binding. Catches a retirement that leaves the
/// binding (a restart would put the host back in the pool's count), one that deletes only the
/// bindings its own registry held, one that leaves the record, and a repeat that writes.
#[tokio::test]
async fn a_retire_deletes_the_stored_binding_and_the_record_on_each_server() {
    let first = rig("retire_binding");
    for name in ["host-1", "host-2", "host-3", "host-4"] {
        first.pooled(name, "burst").await.unwrap();
    }
    let full = first.pooled("host-5", "burst").await;
    assert_eq!(full.err().map(|refused| refused.error), Some("pool-full"));
    let old = HostId::new("host-1").unwrap();
    let live = first.agents.retire(&old).await;
    assert_eq!(live, Err(RetireFault::Kept(js_jobs::Kept::Live)));
    assert_eq!(first.binding("host-1")["version"], 1);

    first
        .clock
        .advance(Duration::from_millis(js_agent_wire::DEFAULT_GRACE_MS + 1));
    let second = Rig::second(&first, "retire_binding_second");
    assert_eq!(first.agents.retire(&old).await, Ok(true));
    assert_eq!(first.binding("host-1"), Value::Null, "the stored binding");
    assert_eq!(first.held("host-1"), None, "the registry's binding");
    assert!(first.agents.registry.own_host(&old).is_none(), "the record");
    first.pooled("host-5", "burst").await.unwrap();
    let commits = first.commits();
    assert_eq!(first.agents.retire(&old).await, Ok(false), "a repeat");
    assert_eq!(first.commits(), commits, "a repeat committed");

    let other = HostId::new("host-2").unwrap();
    assert_eq!(second.agents.retire(&other).await, Ok(true));
    assert_eq!(first.binding("host-2"), Value::Null);
    assert!(first.agents.registry.own_host(&other).is_some());
    let commits = first.commits();
    assert_eq!(first.agents.retire(&other).await, Ok(true), "the record");
    assert_eq!(first.commits(), commits, "no binding was left to delete");
    assert!(first.agents.registry.own_host(&other).is_none());
}

/// Asserts that a retirement whose delete of the stored binding fails is
/// `RetireFault::Unsaved`, logged as an error naming the failure, and leaves the binding
/// stored; and that the same call again deletes it and answers `true`, although the registry
/// forgot the host the first time. Catches a failed delete answered as done, and a retirement
/// whose repeat cannot finish what the first left.
#[tokio::test]
async fn a_retire_whose_binding_cannot_be_deleted_is_unsaved_and_the_repeat_deletes_it() {
    let rig = rig("retire_binding_fails");
    rig.pooled("host-1", "burst").await.unwrap();
    rig.clock
        .advance(Duration::from_millis(js_agent_wire::DEFAULT_GRACE_MS + 1));
    let old = HostId::new("host-1").unwrap();
    rig.store.fail_next();
    let lines = Lines::default();
    let capture = capture(&lines);
    let failed = rig.agents.retire(&old).await;
    drop(capture);
    assert_eq!(failed, Err(RetireFault::Unsaved));
    logged(&lines, "ERROR", "cannot retire a host");
    assert_eq!(rig.binding("host-1")["version"], 1, "the binding stays");
    assert_eq!(rig.agents.retire(&old).await, Ok(true));
    assert_eq!(rig.binding("host-1"), Value::Null);
}

/// Asserts that when the host's stored binding holds a taint another server stored, the
/// retirement is refused `Kept::Tainted` with nothing committed, both on a server whose
/// registry never held the host (a second server over the same store) and on the server that
/// holds it with no taint of its own, whose next save then keeps the stored taint; and that a
/// binding that cannot be read is `RetireFault::Unsaved`, logged as an error, with the host's
/// binding and the registry's record of it kept. Catches a retirement that ends a taint another
/// server stored (the machine would come back clean under its new id), a refused retirement
/// after which a save writes the binding without the taint over it, and one that forgets a host
/// on a binding it could not read.
#[tokio::test]
async fn a_retire_is_refused_for_a_stored_taint_and_unsaved_for_an_unreadable_binding() {
    let first = rig("retire_taint");
    first.pooled("host-1", "burst").await.unwrap();
    first.pooled("host-2", "burst").await.unwrap();
    first
        .clock
        .advance(Duration::from_millis(js_agent_wire::DEFAULT_GRACE_MS + 1));
    let mut tainted = first.binding("host-1").as_object().unwrap().clone();
    tainted.insert("taint_reason".to_owned(), json!("residue"));
    let put = Txn::new().put(
        "hosts/binding/host-1",
        Value::Object(tainted).to_string(),
        Cond::Any,
    );
    first.store.inner().txn(put).unwrap().unwrap();
    let second = Rig::second(&first, "retire_taint_second");
    let commits = first.commits();
    let held = second.agents.retire(&HostId::new("host-1").unwrap()).await;
    assert_eq!(held, Err(RetireFault::Kept(js_jobs::Kept::Tainted)));
    assert_eq!(first.commits(), commits, "a refusal committed");
    assert_eq!(first.binding("host-1")["taint_reason"], "residue");
    let held = first.agents.retire(&HostId::new("host-1").unwrap()).await;
    assert_eq!(held, Err(RetireFault::Kept(js_jobs::Kept::Tainted)));
    assert!(first.agents.save_bindings().await);
    assert_eq!(first.binding("host-1")["taint_reason"], "residue", "a save");

    let put = Txn::new().put("hosts/binding/host-2", "not a binding", Cond::Any);
    first.store.inner().txn(put).unwrap().unwrap();
    let unreadable = HostId::new("host-2").unwrap();
    let lines = Lines::default();
    let capture = capture(&lines);
    let failed = first.agents.retire(&unreadable).await;
    drop(capture);
    assert_eq!(failed, Err(RetireFault::Unsaved));
    let text = lines.take();
    assert!(text.contains("cannot retire a host"), "{text}");
    assert!(first.agents.registry.own_host(&unreadable).is_some());
    assert_eq!(first.keys("hosts/binding/host-2"), ["hosts/binding/host-2"]);
}
