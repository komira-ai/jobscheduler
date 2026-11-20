//! The hello judged before anything is written: what the compensation of a hello the registry
//! refuses after all (a race inside the process, which the seam of the route stands in for)
//! gives back, and what a known host's hello reads. Over a server whose bindings and join tokens
//! are kept in the state store.

use super::*;

/// The seam that has the registry refuse the hello of `host` after it was judged admissible: the
/// host id is listed by another source in between (`host-listed`).
fn listed_in_between(rig: &Rig, host: &str) -> impl Fn() + Sync {
    let registry = rig.agents.registry.clone();
    let host = HostId::new(host).unwrap();
    move || registry.reserve([host.clone()])
}

/// Asserts the compensation of a hello the registry refuses after it passed the check and the
/// token's use and the binding were committed (the host id was listed by another source in
/// between, which the route's seam arranges): two commits in all, the token's row unused, no
/// binding, the token serving no host, and the token registers another host afterwards. It
/// catches a compensation that is gone now that hellos are judged first (a race that then leaves
/// the token consumed for a host that was never admitted, stranding its instance), and one that
/// leaves the binding of a refused host in the store.
#[tokio::test]
async fn a_refusal_after_the_check_gives_the_token_and_the_binding_back() {
    let rig = rig("judged_give_back");
    let who = rig.mint();
    let before = rig.commits();
    let body = Ok(body("host-a", "burst", false));
    let seam = listed_in_between(&rig, "host-a");
    let refused = serve_hello(&rig.agents, who.clone(), Some(who.clone()), body, &seam).await;
    let refused = refused.expect_err("the registry refuses the listed host");
    assert_eq!(
        (refused.status.as_u16(), refused.error),
        (403, "host-listed")
    );
    assert_eq!(rig.commits(), before + 2, "the use and its compensation");
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

/// Asserts the same for the pool's token, which commits the binding alone: the binding goes back
/// in a second commit. It catches a binding left in the store for a host the registry refused
/// after the check.
#[tokio::test]
async fn a_pool_token_refusal_after_the_check_takes_the_binding_back() {
    let rig = rig("judged_pool_back");
    let before = rig.commits();
    let body = Ok(body("host-a", "burst", false));
    let seam = listed_in_between(&rig, "host-a");
    let refused = serve_hello(&rig.agents, rig.pool.clone(), None, body, &seam).await;
    assert_eq!(refused.expect_err("listed").status.as_u16(), 403);
    assert_eq!(rig.commits(), before + 2);
    assert_eq!(rig.binding("host-a"), Value::Null);
    assert!(rig.held("host-a").is_none());
}

/// Asserts what a failed compensation leaves (its commit fails after the use was committed): the
/// token consumed by the host and the host's binding, both in the store and in memory (the token
/// serves that host and refuses another as used). Both or neither holds. It catches a failed
/// compensation that gives the token back in memory alone (a restart would take it away again)
/// or drops one of the two.
#[tokio::test]
async fn a_failed_compensation_leaves_the_binding_and_the_consumed_token() {
    let rig = rig("judged_compensation_failed");
    let who = rig.mint();
    rig.store.fail_after(1);
    let body = Ok(body("host-a", "burst", false));
    let seam = listed_in_between(&rig, "host-a");
    let refused = serve_hello(&rig.agents, who.clone(), Some(who.clone()), body, &seam).await;
    assert_eq!(refused.expect_err("refused").status.as_u16(), 403);
    assert_eq!(rig.row(&who)["used_by"], "host-a");
    assert_eq!(rig.binding("host-a")["credential"], who.to_hex().as_str());
    assert_eq!(rig.agents.joins.serves(&who, "host-a"), Ok(()));
    let other = rig.hello(&who, "host-b", "burst").await;
    let other = other.expect_err("the token is the host's");
    assert_eq!(
        (other.status.as_u16(), other.error),
        (403, "join-token-used")
    );
}

/// Asserts that a compensation that finds the binding written by another server since the hello
/// committed it leaves that server's row alone: the undo is conditional on the version the
/// hello's commit wrote, so it is refused and both the use and the binding stay as the store
/// holds them. It catches an undo at any version, which would delete or put over a row another
/// server wrote in the seam.
#[tokio::test]
async fn a_compensation_never_undoes_a_row_another_server_wrote() {
    let rig = rig("judged_undo_conditional");
    let who = rig.mint();
    let (agents, store) = (rig.agents.registry.clone(), rig.store.inner());
    let host = HostId::new("host-a").unwrap();
    let seam = move || {
        agents.reserve([host.clone()]);
        let row = json!({ "version": 1, "credential": "0".repeat(64), "taint_reason": "residue" });
        let put = Txn::new().put("hosts/binding/host-a", row.to_string(), Cond::Any);
        store.txn(put).unwrap().unwrap();
    };
    let body = Ok(body("host-a", "burst", false));
    let refused = serve_hello(&rig.agents, who.clone(), Some(who.clone()), body, &seam).await;
    assert_eq!(refused.expect_err("refused").status.as_u16(), 403);
    assert_eq!(rig.binding("host-a")["taint_reason"], "residue");
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
