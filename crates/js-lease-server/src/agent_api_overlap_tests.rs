//! Two servers over one store while one update overlaps them: the second starts while the first
//! serves every host, and the hosts move to it as the first stops. What the first stored after
//! the second started (a host's taint, a join token, a host credential) must hold on the
//! second, and the second must leave the bindings of the hosts the first serves as they are.

use super::*;

/// The taint the first server's host reports.
fn residue() -> Value {
    json!({ "reason": "residue", "check": "daemonised-child", "run_id": "r1" })
}

/// The hello of `host` in the pool `burst` with the one-time token or host credential whose
/// digest is `who`, asking for a host credential.
async fn asking(rig: &Rig, who: &Credential, host: &str) -> Result<Welcome, Refused> {
    let body = Ok(body(host, "burst", true));
    let welcome = serve_hello(&rig.agents, who.clone(), Some(who.clone()), body, &|| ());
    welcome.await.map(|Json(welcome)| welcome)
}

/// Asserts that a server started beside another leaves the bindings of the hosts the other
/// serves: its tick writes nothing for them and the store keeps both, while a host it held
/// itself and forgot loses its binding as before. It catches the save that made the store
/// equal this server's registry, which on every update deleted the binding of every host the
/// first server serves (and with it a taint the first stored).
#[tokio::test]
async fn a_second_server_leaves_the_bindings_of_hosts_it_never_held() {
    let first = rig("overlap_keep_first");
    first.pooled("host-a", "burst").await.unwrap();
    first.pooled("host-b", "burst").await.unwrap();
    let second = Rig::second(&first, "overlap_keep_second");
    second.pooled("host-c", "burst").await.unwrap();
    let commits = second.commits();
    second.agents.refresh().await;
    assert_eq!(second.commits(), commits, "nothing to write");
    for host in ["host-a", "host-b", "host-c"] {
        assert_eq!(first.binding(host)["version"], 1, "{host}");
    }

    second.clock.advance(Duration::from_secs(26 * 3600));
    second.agents.refresh().await;
    assert!(second.held("host-c").is_none());
    assert_eq!(
        first.binding("host-c"),
        Value::Null,
        "a forgotten host's goes"
    );
    assert_eq!(first.binding("host-a")["version"], 1);
    assert_eq!(first.binding("host-b")["version"], 1);
}

/// Asserts that a host that moves to a second server keeps the taint the first stored after
/// the second started: the second's registry holds it at the hello (so the host is offered no
/// job), and the stored binding is not overwritten with a clean one. A hello whose read of the
/// binding fails is refused 503 `state-write-failed` with nothing written or held. It catches
/// a hello that judged the host on the bindings read at start alone, which put a clean binding
/// over the stored taint and offered the host jobs its agent refuses.
#[tokio::test]
async fn a_host_that_moves_keeps_the_taint_its_first_server_stored() {
    let first = rig("overlap_taint_first");
    let welcome = first.pooled("host-a", "burst").await.unwrap();
    let second = Rig::second(&first, "overlap_taint_second");
    let third = Rig::second(&first, "overlap_taint_third");
    let reported = first.beat((&first.pool, None), "host-a", &welcome, residue());
    assert!(reported.await.unwrap().taint_ack);
    assert_eq!(first.binding("host-a")["taint_run_id"], "r1");

    let commits = third.commits();
    third.store.fail_read_after(0);
    let refused = third.pooled("host-a", "burst").await;
    let refused = refused.expect_err("the binding cannot be read");
    assert_eq!(
        (refused.status.as_u16(), refused.error),
        (503, "state-write-failed")
    );
    assert_eq!(third.commits(), commits, "nothing was written");
    assert!(third.held("host-a").is_none());

    second.pooled("host-a", "burst").await.unwrap();
    let held = second.held("host-a").unwrap();
    assert_eq!(held.taint().unwrap().run_id.as_deref(), Some("r1"));
    let host = HostId::new("host-a").unwrap();
    let views = second.agents.registry.hosts();
    let view = views.iter().find(|view| view.id == host).unwrap();
    assert_eq!(view.taint.as_ref().unwrap().reason, "residue");
    assert_eq!(first.binding("host-a")["taint_run_id"], "r1");
}

/// Asserts that a join token another server minted after this one started, and the host
/// credential that server then issued for it, are known here at their first use: each is read
/// from the store by its key and the host's hello with the credential is admitted. A token the
/// store holds no row of is unknown, and a read that fails is an error, never an unknown token
/// (the agent would take a 401 for good). It catches a server that knew only the tokens it read
/// at start, so a host admitted by the first server during the update was refused here, and
/// its agent ended with its runs, once the first stopped.
#[tokio::test]
async fn a_credential_another_server_issued_after_this_one_started_is_known() {
    let first = rig("overlap_join_first");
    let second = Rig::second(&first, "overlap_join_second");
    let minted = first
        .agents
        .joins
        .mint(instance(), Duration::from_secs(600))
        .unwrap();
    let token = Credential::of_token(&minted.token);
    assert_eq!(second.agents.joins.identify(&minted.token), None);
    let known = identify_single(&second.agents, &minted.token).await;
    assert_eq!(known, Ok(Some(token.clone())));
    assert_eq!(
        second.agents.joins.identify(&minted.token),
        Some(token.clone())
    );
    let held = identify_single(&second.agents, &minted.token).await;
    assert_eq!(held, Ok(Some(token.clone())), "held from here on");

    let welcome = asking(&first, &token, "host-a").await.unwrap();
    let issued = welcome.host_credential.unwrap();
    let credential = Credential::of_token(issued.expose());
    let found = identify_single(&second.agents, issued.expose()).await;
    assert_eq!(found, Ok(Some(credential.clone())));
    let moved = second.hello(&credential, "host-a", "burst").await;
    moved.expect("the credential the first server issued admits the host here");

    let unknown = identify_single(&second.agents, &"u".repeat(40)).await;
    assert_eq!(unknown, Ok(None));
    second.store.fail_read_after(0);
    let failed = identify_single(&second.agents, &"v".repeat(40)).await;
    assert!(failed.unwrap_err().contains(INJECTED), "the read's error");
}

/// Asserts that a stored row read at a host's hello or at a token's first use, which this
/// build cannot read as written, refuses the call rather than taking the row for absent: a
/// binding that is not JSON, and one whose credential is not a digest, each refuse the hello
/// 503 `state-write-failed` with nothing written or held; a token row that is not JSON is an
/// error naming its key. It catches a damaged row read as "no taint" or "no such token".
#[tokio::test]
async fn a_row_that_cannot_be_read_refuses_the_call() {
    let first = rig("overlap_bad_first");
    first.pooled("host-a", "burst").await.unwrap();
    let mut damaged = first.binding("host-a");
    damaged["credential"] = json!("zz");
    let second = Rig::second(&first, "overlap_bad_second");
    let rows = [
        ("hosts/binding/host-b", "not json".to_owned()),
        ("hosts/binding/host-c", damaged.to_string()),
    ];
    for (key, value) in rows {
        let put = Txn::new().put(key, value, Cond::Any);
        first.store.inner().txn(put).unwrap().unwrap();
    }
    for host in ["host-b", "host-c"] {
        let commits = second.commits();
        let refused = second
            .pooled(host, "burst")
            .await
            .expect_err("a damaged binding");
        assert_eq!(
            (refused.status.as_u16(), refused.error),
            (503, "state-write-failed"),
            "{host}"
        );
        assert_eq!(second.commits(), commits, "{host}: nothing was written");
        assert!(second.held(host).is_none(), "{host}");
    }

    let token = "w".repeat(40);
    let key = format!("hosts/join/{}", Credential::of_token(&token).to_hex());
    let put = Txn::new().put(&key, "not json", Cond::Any);
    first.store.inner().txn(put).unwrap().unwrap();
    let failed = identify_single(&second.agents, &token).await;
    assert!(
        failed.unwrap_err().contains(&key),
        "the error names the row"
    );
}

/// Asserts what the agent routes answer a bearer this server does not hold: 401 when the store
/// holds no row of it either, and 503 `{"error":"state-write-failed"}` when the store cannot be
/// read, so an agent retries instead of ending on a 401 while the store is down. It catches a
/// token read that fails open as unknown.
#[tokio::test]
async fn an_unknown_bearer_is_401_and_an_unread_one_is_503() {
    use tower::ServiceExt;

    let rig = rig("overlap_bearer");
    let app = super::router(Arc::clone(&rig.agents));
    let hello = |token: String| {
        axum::http::Request::post("/v1/agent/hello")
            .header("authorization", format!("Bearer {token}"))
            .header("content-type", "application/json")
            .body(axum::body::Body::from(body("host-a", "burst", false)))
            .unwrap()
    };
    let unknown = app.clone().oneshot(hello("u".repeat(40))).await.unwrap();
    assert_eq!(unknown.status(), StatusCode::UNAUTHORIZED);
    rig.store.fail_read_after(0);
    let failed = app.oneshot(hello("v".repeat(40))).await.unwrap();
    assert_eq!(failed.status(), StatusCode::SERVICE_UNAVAILABLE);
    let text = axum::body::to_bytes(failed.into_body(), 1024)
        .await
        .unwrap();
    assert_eq!(&text[..], br#"{"error":"state-write-failed"}"#);
}

/// Asserts that a binding another server dropped after this one read it is put again at the
/// host's hello here: the first server forgets the silent host and deletes its row, the host
/// then says hello to the second with the same credential, and the store holds its binding
/// again. It catches a hello that took the binding read at start for what the store holds,
/// wrote nothing, and left the host with no stored binding.
#[tokio::test]
async fn a_binding_another_server_dropped_is_put_again_at_the_hello() {
    let first = rig("overlap_dropped_first");
    first.pooled("host-a", "burst").await.unwrap();
    let second = Rig::second(&first, "overlap_dropped_second");
    first.clock.advance(Duration::from_secs(26 * 3600));
    first.agents.refresh().await;
    assert_eq!(first.binding("host-a"), Value::Null, "the first forgot it");

    second.pooled("host-a", "burst").await.unwrap();
    assert_eq!(first.binding("host-a")["version"], 1, "put again");
    assert!(second.held("host-a").is_some());
}
