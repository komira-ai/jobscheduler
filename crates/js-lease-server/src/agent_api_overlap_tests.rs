//! Two servers over one store while one update overlaps them: the second starts while the first
//! serves every host, and the hosts move to it as the first stops. What the first stored after
//! the second started (a host's taint) must hold on the second, and the second must leave the
//! bindings of the hosts the first serves as they are.

use super::*;

/// The taint the first server's host reports.
fn residue() -> Value {
    json!({ "reason": "residue", "check": "daemonised-child", "run_id": "r1" })
}

/// Asserts that a server started beside another leaves the bindings of the hosts the other
/// serves: its tick writes nothing for them and the store keeps both, while a host it held
/// itself and forgot loses its binding as before. It catches the save that made the store
/// equal this server's registry, which on every update deleted the binding of every host the
/// first server serves (and with it a taint the first stored).
#[tokio::test]
async fn a_second_server_leaves_the_bindings_of_hosts_it_never_held() {
    let first = rig("overlap_keep_first");
    first.hello("host-a", "burst").await.unwrap();
    first.hello("host-b", "burst").await.unwrap();
    let second = Rig::second(&first, "overlap_keep_second");
    second.hello("host-c", "burst").await.unwrap();
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
    let welcome = first.hello("host-a", "burst").await.unwrap();
    let second = Rig::second(&first, "overlap_taint_second");
    let third = Rig::second(&first, "overlap_taint_third");
    let reported = first.beat("host-a", &welcome, residue());
    assert!(reported.await.unwrap().taint_ack);
    assert_eq!(first.binding("host-a")["taint_run_id"], "r1");

    let commits = third.commits();
    third.store.fail_read_after(0);
    let refused = third.hello("host-a", "burst").await;
    let refused = refused.expect_err("the binding cannot be read");
    assert_eq!(
        (refused.status.as_u16(), refused.error),
        (503, "state-write-failed")
    );
    assert_eq!(third.commits(), commits, "nothing was written");
    assert!(third.held("host-a").is_none());

    second.hello("host-a", "burst").await.unwrap();
    let held = second.held("host-a").unwrap();
    assert_eq!(held.taint().unwrap().run_id.as_deref(), Some("r1"));
    let host = HostId::new("host-a").unwrap();
    let views = second.agents.registry.hosts();
    let view = views.iter().find(|view| view.id == host).unwrap();
    assert_eq!(view.taint.as_ref().unwrap().reason, "residue");
    assert_eq!(first.binding("host-a")["taint_run_id"], "r1");
}

/// Asserts that a stored row read at a host's hello, which this build cannot read as written,
/// refuses the call rather than taking the row for absent: a binding that is not JSON, and one
/// whose credential is not a digest, each refuse the hello 503 `state-write-failed` with nothing
/// written or held. It catches a damaged row read as "no taint".
#[tokio::test]
async fn a_row_that_cannot_be_read_refuses_the_call() {
    let first = rig("overlap_bad_first");
    first.hello("host-a", "burst").await.unwrap();
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
            .hello(host, "burst")
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
}

/// Asserts that every agent route is answered 401 `{"error":"host-certificate-required"}` when
/// the connection carries no verified host certificate, whether the request has no credential or
/// only a bearer (the client token of the lease routes, a pool's old token): the hello, the
/// heartbeat, the dispatch poll and the host's read of itself, with nothing written to the store
/// and no host held; the same hello and read with the certificate's host on the request pass
/// the layer and are answered. It catches a route outside the layer, and a bearer that admits.
#[tokio::test]
async fn a_request_without_a_host_certificate_is_refused_401_host_certificate_required() {
    use tower::ServiceExt;

    let rig = rig("overlap_certificate");
    let app = super::router(Arc::clone(&rig.agents));
    let request = |method: &str, path: &str, bearer: bool, certified: bool| {
        let mut request = axum::http::Request::builder()
            .method(method)
            .uri(path)
            .header("content-type", "application/json");
        if bearer {
            request = request.header("authorization", format!("Bearer {}", "j".repeat(40)));
        }
        let mut request = request
            .body(axum::body::Body::from(body("host-a", "burst")))
            .unwrap();
        if certified {
            let host = HostId::new("host-a").unwrap();
            request
                .extensions_mut()
                .insert(crate::host_cert::CertHost(host));
        }
        request
    };
    let commits = rig.commits();
    let routes = [
        ("POST", "/v1/agent/hello"),
        ("POST", "/v1/agent/heartbeat"),
        ("POST", js_agent_wire::DISPATCH_PATH),
        ("GET", "/v1/hosts/host-a"),
    ];
    for (method, path) in routes {
        for bearer in [false, true] {
            let refused = app
                .clone()
                .oneshot(request(method, path, bearer, false))
                .await
                .unwrap();
            assert_eq!(refused.status(), StatusCode::UNAUTHORIZED, "{path}");
            let text = axum::body::to_bytes(refused.into_body(), 1024)
                .await
                .unwrap();
            assert_eq!(
                &text[..],
                br#"{"error":"host-certificate-required"}"#,
                "{path}"
            );
        }
    }
    assert_eq!(rig.commits(), commits, "nothing was written");
    assert!(rig.held("host-a").is_none());

    let hello = request("POST", "/v1/agent/hello", true, true);
    let admitted = app.clone().oneshot(hello).await.unwrap();
    assert_eq!(admitted.status(), StatusCode::OK);
    assert!(rig.held("host-a").is_some());
    let read = app
        .oneshot(request("GET", "/v1/hosts/host-a", false, true))
        .await;
    assert_eq!(read.unwrap().status(), StatusCode::OK);
}
/// Asserts that a binding another server dropped after this one read it is put again at the
/// host's hello here: the first server forgets the silent host and deletes its row, the host
/// then says hello to the second with the same credential, and the store holds its binding
/// again. It catches a hello that took the binding read at start for what the store holds,
/// wrote nothing, and left the host with no stored binding.
#[tokio::test]
async fn a_binding_another_server_dropped_is_put_again_at_the_hello() {
    let first = rig("overlap_dropped_first");
    first.hello("host-a", "burst").await.unwrap();
    let second = Rig::second(&first, "overlap_dropped_second");
    first.clock.advance(Duration::from_secs(26 * 3600));
    first.agents.refresh().await;
    assert_eq!(first.binding("host-a"), Value::Null, "the first forgot it");

    second.hello("host-a", "burst").await.unwrap();
    assert_eq!(first.binding("host-a")["version"], 1, "put again");
    assert!(second.held("host-a").is_some());
}
