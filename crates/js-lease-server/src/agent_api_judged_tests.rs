//! The hello judged before anything is written: a refused hello uses no token and writes no
//! binding, so there is nothing to undo, and what a known host's hello reads. Over a server whose
//! bindings and join tokens are kept in the state store.

use super::*;

/// Asserts that a hello the registry refuses writes nothing, with a one-time token: the host id
/// is listed by another source (`host-listed`), so the hello is refused 403 before its token is
/// used or its binding written: no commit at all, the token's row unused, no binding, the token
/// serving no host, and the token registers another host afterwards. Catches a hello that
/// takes the use of its token before the registry judged it (a refused hello then strands the
/// token's instance), and one that leaves the binding of a refused host in the store.
#[tokio::test]
async fn a_refused_hello_uses_no_token_and_writes_no_binding() {
    let rig = rig("judged_refused");
    let who = rig.mint();
    rig.agents
        .registry
        .reserve([HostId::new("host-a").unwrap()]);
    let before = rig.commits();
    let refused = rig.hello(&who, "host-a", "burst").await;
    let refused = refused.expect_err("the registry refuses the listed host");
    assert_eq!(
        (refused.status.as_u16(), refused.error),
        (403, "host-listed")
    );
    assert_eq!(rig.commits(), before, "nothing was committed");
    assert_eq!(rig.row(&who)["used_by"], Value::Null);
    assert_eq!(rig.binding("host-a"), Value::Null);
    let unused = rig.agents.joins.serves(&who, "host-a");
    assert_eq!(unused, Err(JoinRefusal::NotAdmitted));
    assert!(rig.held("host-a").is_none());

    rig.hello(&who, "host-b", "burst")
        .await
        .expect("the token is good for another host");
    assert_eq!(rig.row(&who)["used_by"], "host-b");
}

/// Asserts the same for the pool's token, which commits the binding alone: a listed host is
/// refused with no commit and no binding. Catches a binding written for a host the registry
/// refuses.
#[tokio::test]
async fn a_pool_token_refusal_writes_no_binding() {
    let rig = rig("judged_pool_refused");
    rig.agents
        .registry
        .reserve([HostId::new("host-a").unwrap()]);
    let before = rig.commits();
    let refused = rig.pooled("host-a", "burst").await;
    assert_eq!(refused.expect_err("listed").status.as_u16(), 403);
    assert_eq!(rig.commits(), before);
    assert_eq!(rig.binding("host-a"), Value::Null);
    assert!(rig.held("host-a").is_none());
}

/// Asserts that a hello whose write fails after it was judged admissible changes nothing in the
/// registry: with the store's next commit failing, the hello is refused 503 `join-token-unsaved`,
/// the token's row stays unused and the registry holds no host, so the retry is admitted with
/// the same token. Catches an admission recorded before the writes it depends on are made, which
/// would leave a session no binding backs.
#[tokio::test]
async fn a_failed_write_leaves_the_registry_and_the_token_as_they_were() {
    let rig = rig("judged_write_failed");
    let who = rig.mint();
    rig.store.fail_next();
    let failed = rig.hello(&who, "host-a", "burst").await;
    let failed = failed.expect_err("the commit fails");
    assert_eq!(
        (failed.status.as_u16(), failed.error),
        (503, "join-token-unsaved")
    );
    assert!(rig.held("host-a").is_none());
    assert_eq!(rig.row(&who)["used_by"], Value::Null);
    assert_eq!(rig.binding("host-a"), Value::Null);

    rig.hello(&who, "host-a", "burst").await.expect("admitted");
    assert_eq!(rig.row(&who)["used_by"], "host-a");
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
    rig.pooled("host-a", "burst").await.unwrap();
    rig.clock.advance(Duration::from_secs(600));
    let commits = rig.commits();
    rig.store.fail_read_after(0);
    let failed = rig.pooled("host-a", "burst").await;
    let failed = failed.expect_err("the binding row cannot be read");
    assert_eq!(
        (failed.status.as_u16(), failed.error),
        (503, "state-write-failed")
    );
    assert_eq!(rig.commits(), commits);

    rig.pooled("host-a", "burst").await.expect("admitted");
    assert_eq!(rig.commits(), commits, "the row holds the binding already");
}

/// Asserts that a host the registry does not hold and no row knows (a server without the job
/// tables, restarted: its registry is empty and it reads no row for the cordon) is answered
/// 409 `unknown-host` by the host's own cordon route, as before. It catches a route that makes up
/// a host.
#[tokio::test]
async fn a_host_a_restarted_server_does_not_hold_is_unknown_to_the_cordon_route() {
    let rig = rig("judged_restart_unknown");
    let who = rig.mint();
    rig.hello(&who, "host-a", "burst").await.unwrap();
    let Rig {
        store, clock, dir, ..
    } = rig;
    let rig = over(dir, store, clock);
    let presented = Some(Extension(OneTime(who)));
    let target = axum::extract::Path("host-a:cordon".to_owned());
    let state = State(Arc::clone(&rig.agents));
    let answer = own_host::act(state, (presented, None), target, Ok(Bytes::new())).await;
    let refused = answer.unwrap_err();
    assert_eq!(
        (refused.status.as_u16(), refused.error),
        (409, "unknown-host")
    );
}
