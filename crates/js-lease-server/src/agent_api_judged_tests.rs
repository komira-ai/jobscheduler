//! The hello judged before anything is written: a refused hello writes no binding, so there is
//! nothing to undo, and what a known host's hello reads. Over a server whose bindings are kept
//! in the state store.

use super::*;

/// Asserts that a hello the registry refuses writes nothing: the host id is listed by another
/// source (`host-listed`), so the hello is refused 403 before its binding is written: no commit
/// at all, no binding, and another host's hello is admitted afterwards. Catches a hello that
/// leaves the binding of a refused host in the store.
#[tokio::test]
async fn a_refused_hello_writes_no_binding() {
    let rig = rig("judged_refused");
    rig.agents
        .registry
        .reserve([HostId::new("host-a").unwrap()]);
    let before = rig.commits();
    let refused = rig.hello("host-a", "burst").await;
    let refused = refused.expect_err("the registry refuses the listed host");
    assert_eq!(
        (refused.status.as_u16(), refused.error),
        (403, "host-listed")
    );
    assert_eq!(rig.commits(), before, "nothing was committed");
    assert_eq!(rig.binding("host-a"), Value::Null);
    assert!(rig.held("host-a").is_none());

    rig.hello("host-b", "burst")
        .await
        .expect("another host is admitted");
    assert_eq!(rig.binding("host-b")["version"], 1);
}

/// Asserts that a hello whose write fails after it was judged admissible changes nothing in the
/// registry: with the store's next commit failing, the hello is refused 503 `state-write-failed`
/// and the registry holds no host, so the retry is admitted. Catches an admission recorded before
/// the writes it depends on are made, which would leave a session no binding backs.
#[tokio::test]
async fn a_failed_write_leaves_the_registry_as_it_was() {
    let rig = rig("judged_write_failed");
    rig.store.fail_next();
    let failed = rig.hello("host-a", "burst").await;
    let failed = failed.expect_err("the commit fails");
    assert_eq!(
        (failed.status.as_u16(), failed.error),
        (503, "state-write-failed")
    );
    assert!(rig.held("host-a").is_none());
    assert_eq!(rig.binding("host-a"), Value::Null);

    rig.hello("host-a", "burst").await.expect("admitted");
    assert_ne!(rig.binding("host-a"), Value::Null);
}

/// Asserts that a known host's hello reads the host's binding row (it is judged on that row and
/// its write is conditional on that row's version) and writes nothing when the row holds the
/// binding already: with the store's next read set to fail the hello is refused 503
/// `state-write-failed` with no commit, and without the fault it is admitted with no commit. It
/// catches a hello that judges a known host on the binding it read at start alone, which then
/// writes at a version it never read.
#[tokio::test]
async fn a_known_hosts_hello_reads_its_binding_row_and_writes_nothing() {
    let rig = rig("judged_known_read");
    rig.hello("host-a", "burst").await.unwrap();
    rig.clock.advance(Duration::from_secs(600));
    let commits = rig.commits();
    rig.store.fail_read_after(0);
    let failed = rig.hello("host-a", "burst").await;
    let failed = failed.expect_err("the binding row cannot be read");
    assert_eq!(
        (failed.status.as_u16(), failed.error),
        (503, "state-write-failed")
    );
    assert_eq!(rig.commits(), commits);

    rig.hello("host-a", "burst").await.expect("admitted");
    assert_eq!(rig.commits(), commits, "the row holds the binding already");
}

/// Asserts that a host the registry does not hold and no row knows (a server without the job
/// tables, restarted: its registry is empty and it reads no row for the cordon) is answered
/// 409 `unknown-host` by the host's own cordon route, as before. It catches a route that makes up
/// a host.
#[tokio::test]
async fn a_host_a_restarted_server_does_not_hold_is_unknown_to_the_cordon_route() {
    let rig = rig("judged_restart_unknown");
    rig.hello("host-a", "burst").await.unwrap();
    let Rig {
        store, clock, dir, ..
    } = rig;
    let rig = over(dir, store, clock);
    let certified = Extension(CertifiedHost(HostId::new("host-a").unwrap()));
    let target = axum::extract::Path("host-a:cordon".to_owned());
    let state = State(Arc::clone(&rig.agents));
    let answer = own_host::act(state, certified, target, Ok(Bytes::new())).await;
    let refused = answer.unwrap_err();
    assert_eq!(
        (refused.status.as_u16(), refused.error),
        (409, "unknown-host")
    );
}
