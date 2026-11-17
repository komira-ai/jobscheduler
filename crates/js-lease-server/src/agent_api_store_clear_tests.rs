//! The operator's clear of a host's taint ([`Agents::clear_taint`]) over a server whose
//! bindings are kept in the state store: what it commits, before what it changes in memory,
//! and what a refused or failed commit leaves. The rig is the one of the parent module.

use super::*;

/// The taint `run` reports: a residue of the check `daemonised-child`.
fn residue(run: &str) -> Value {
    json!({ "reason": "residue", "check": "daemonised-child", "run_id": run })
}

/// Asserts that a clear commits the binding without the taint in one transaction before the
/// registry forgets it, answers the taint it cleared, and that a second clear of the same host
/// answers none and commits nothing; and that a clear whose commit fails is `Unsaved`, logged
/// as an error naming the failure, leaves the taint in the store and in the registry, and is
/// done by the same call once the store works. It catches a clear that forgets the taint in
/// memory before it is durable (a restart would bring it back), one that writes when there is
/// nothing to clear, and a failed commit that changes memory.
#[tokio::test]
async fn a_clear_commits_the_binding_without_the_taint_before_the_registry_forgets_it() {
    let rig = rig("clear_commit");
    let welcome = rig.pooled("host-a", "burst").await.unwrap();
    let reply = rig
        .beat((&rig.pool, None), "host-a", &welcome, residue("r1"))
        .await;
    assert!(reply.unwrap().taint_ack);
    assert_eq!(rig.binding("host-a")["taint_reason"], "residue");
    let host = HostId::new("host-a").unwrap();

    rig.store.fail_next();
    let lines = Lines::default();
    let capture = capture(&lines);
    let before = rig.commits();
    let failed = rig.agents.clear_taint(&host).await;
    drop(capture);
    assert_eq!(failed.err(), Some(CordonFault::Unsaved));
    let message = "cannot commit the clearing of a host's taint";
    logged(&lines, "ERROR", message);
    assert_eq!(rig.commits(), before);
    assert_eq!(rig.binding("host-a")["taint_reason"], "residue");
    assert!(rig.held("host-a").unwrap().taint().is_some());

    let cleared = rig.agents.clear_taint(&host).await.unwrap();
    let reason = cleared.map(|taint| taint.reason);
    assert_eq!(reason.as_deref(), Some("residue"));
    assert_eq!(rig.commits(), before + 1, "one transaction");
    let stored = rig.binding("host-a");
    for key in ["taint_reason", "taint_check", "taint_run_id"] {
        assert_eq!(stored[key], Value::Null, "{key}");
    }
    assert!(rig.held("host-a").unwrap().taint().is_none());

    let again = rig.agents.clear_taint(&host).await.unwrap();
    assert_eq!(again, None);
    assert_eq!(rig.commits(), before + 1, "nothing was written");
}

/// Asserts that a clear made on what a server saw before another server stored a different
/// taint is refused: both servers hold the host, the second is told taint `r1` and commits
/// it, the store then holds `r2` (as another server's newer report leaves it), and the
/// second's clear is `Unsaved` with nothing written, the store keeping `r2` and the second
/// still holding `r1`. It catches a clear that erases a newer taint another server stored,
/// as the heartbeat's clear does not.
#[tokio::test]
async fn a_clear_is_refused_when_the_store_holds_another_taint() {
    let first = rig("clear_two_first");
    first.pooled("host-a", "burst").await.unwrap();
    let second = Rig::second(&first, "clear_two_second");
    let welcome = second.pooled("host-a", "burst").await.unwrap();
    let reply = second
        .beat((&second.pool, None), "host-a", &welcome, residue("r1"))
        .await;
    assert!(reply.unwrap().taint_ack);

    let newer = first.binding("host-a");
    let mut newer = newer.as_object().unwrap().clone();
    newer.insert("taint_run_id".to_owned(), json!("r2"));
    newer.insert("taint_check".to_owned(), json!("process-as-job-user"));
    let put = Txn::new().put(
        "hosts/binding/host-a",
        Value::Object(newer).to_string(),
        Cond::Any,
    );
    first.store.inner().txn(put).unwrap().unwrap();

    let host = HostId::new("host-a").unwrap();
    let commits = first.commits();
    let refused = second.agents.clear_taint(&host).await;
    assert_eq!(refused.err(), Some(CordonFault::Unsaved));
    assert_eq!(first.commits(), commits, "nothing was written");
    assert_eq!(first.binding("host-a")["taint_run_id"], "r2");
    let held = second.held("host-a").unwrap();
    assert_eq!(held.taint().unwrap().run_id.as_deref(), Some("r1"));
}

/// Asserts that a clear of a host the registry does not hold is `NotHeld` and commits
/// nothing, and that a clear of a held host with no taint answers none and commits nothing.
/// It catches a clear that writes a binding for a host it does not know.
#[tokio::test]
async fn a_clear_of_an_unknown_or_clean_host_writes_nothing() {
    let rig = rig("clear_none");
    rig.pooled("host-a", "burst").await.unwrap();
    let before = rig.commits();
    let ghost = HostId::new("ghost").unwrap();
    let unknown = rig.agents.clear_taint(&ghost).await;
    assert_eq!(unknown.err(), Some(CordonFault::NotHeld));
    let clean = HostId::new("host-a").unwrap();
    assert_eq!(rig.agents.clear_taint(&clean).await.unwrap(), None);
    assert_eq!(rig.commits(), before);
}
