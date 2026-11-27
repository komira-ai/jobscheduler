//! The hello of a host with a host certificate over a host whose binding a release that admitted
//! hosts by token wrote, with the bindings kept in a store: what is committed, when, and what
//! goes back when the hello fails.

use super::*;

/// The hello `body` of `host` in the pool `burst`, as the registry takes it directly.
fn hello_of(host: &str) -> Hello {
    serde_json::from_slice(&body(host, "burst")).unwrap()
}

/// The credential a binding of `host` is stored with, as hex.
fn stored_credential(rig: &Rig, host: &str) -> String {
    rig.binding(host)["credential"].as_str().unwrap().to_owned()
}

/// Asserts that a hello with a certificate over a host whose stored binding is a token's and
/// that is not live commits the new binding in one transaction before the registry takes it:
/// when that commit fails (once, by the store's fault hook) the hello is refused 503
/// `state-write-failed` and the store still holds the token's binding; once the fault is gone
/// the same hello is admitted with one more commit, and the store and the registry hold the
/// certificate's credential.
#[tokio::test]
async fn a_rebinding_is_committed_before_the_registry_takes_it() {
    let rig = rig("rebind_commit");
    rig.bind_by_token("host-a");
    let before = rig.commits();
    rig.store.fail_next();
    let failed = rig.hello("host-a", "burst").await.unwrap_err();
    assert_eq!(
        (failed.status.as_u16(), failed.error),
        (503, STATE_WRITE_FAILED)
    );
    assert_eq!(rig.commits(), before);
    assert_eq!(stored_credential(&rig, "host-a"), legacy().to_hex());

    rig.hello("host-a", "burst").await.unwrap();
    let cert = who_of("host-a");
    assert_eq!(rig.commits(), before + 1);
    assert_eq!(stored_credential(&rig, "host-a"), cert.to_hex());
    assert_eq!(rig.held("host-a").unwrap().credential(), &cert);
}

/// Asserts that a host bound to a token that is live is refused a certificate hello 409
/// `host-live` with nothing written, and keeps the token's binding.
#[tokio::test]
async fn a_live_token_host_is_not_rebound_and_nothing_is_written() {
    let rig = rig("rebind_live");
    rig.bind_by_token("host-a");
    rig.agents
        .registry
        .hello(&hello_of("host-a"), &legacy())
        .unwrap();
    let before = rig.commits();
    let refused = rig.hello("host-a", "burst").await.unwrap_err();
    assert_eq!((refused.status.as_u16(), refused.error), (409, "host-live"));
    assert_eq!(rig.commits(), before);
    assert_eq!(stored_credential(&rig, "host-a"), legacy().to_hex());
    assert_eq!(rig.held("host-a").unwrap().credential(), &legacy());
}

/// Asserts that a certificate hello the registry refuses (it names a pool the server does not
/// serve) is judged before the new binding is written: the store takes no commit, and the store
/// holds the token's binding as before.
#[tokio::test]
async fn a_refused_rebinding_writes_nothing_and_keeps_the_token_binding() {
    let rig = rig("rebind_undo");
    rig.bind_by_token("host-a");
    let before = rig.commits();
    let refused = rig.hello("host-a", "elsewhere").await.unwrap_err();
    assert_eq!(
        (refused.status.as_u16(), refused.error),
        (403, "unknown-pool")
    );
    assert_eq!(rig.commits(), before);
    assert_eq!(stored_credential(&rig, "host-a"), legacy().to_hex());
}

/// Asserts that a certificate hello from the process of a live host bound to a token (the
/// registry takes it for the same agent coming back, so it is not `host-live`) moves the stored
/// binding onto the certificate's credential as the registry moves its own: the store and the
/// registry hold the certificate's credential. It catches a hello that decides in the store
/// whether the host is idle, which leaves the store bound to the token while the registry is
/// bound to the certificate (a restart would then refuse the host).
#[tokio::test]
async fn a_certificate_hello_of_the_live_hosts_own_process_moves_the_stored_binding() {
    let rig = rig("rebind_same_process");
    let with_process = |host: &str| {
        let mut hello: Value = serde_json::from_slice(&body(host, "burst")).unwrap();
        hello["agent_process"] = json!("process-1");
        Bytes::from(hello.to_string())
    };
    rig.bind_by_token("host-a");
    let first: Hello = serde_json::from_slice(&with_process("host-a")).unwrap();
    rig.agents.registry.hello(&first, &legacy()).unwrap();

    let again = Ok(with_process("host-a"));
    let welcome = serve_hello(&rig.agents, "host-a", again, &|| ());
    drop(welcome.await.expect("the same process is admitted"));
    let who = who_of("host-a");
    assert_eq!(stored_credential(&rig, "host-a"), who.to_hex());
    assert_eq!(rig.held("host-a").unwrap().credential(), &who);
}

/// Asserts that a hello that was judged is admitted though another hello of the host was
/// accepted by the registry in the seam between the judgement and the registry taking it: the
/// first is admitted all the same (the registry does not judge it again, so it takes the session
/// over, the epoch following the second's) and the host is held by its certificate's credential.
/// It catches a take that refuses a hello a race made inadmissible after its binding was
/// committed, which would leave a binding written for a hello that was not answered. The second
/// hello runs inside the seam of the first, so the order is fixed and nothing waits on timing.
#[tokio::test]
async fn a_judged_hello_is_admitted_though_another_of_the_host_registered_in_between() {
    let rig = rig("judged_again_accepted");
    let who = who_of("host-a");
    let second = || {
        rig.agents
            .registry
            .hello(&hello_of("host-a"), &who)
            .expect("the second hello is accepted");
    };
    let first = Ok(body("host-a", "burst"));
    let welcome = serve_hello(&rig.agents, "host-a", first, &second).await;
    let Json(welcome) = welcome.expect("the first hello was judged admissible, so it is admitted");
    assert_eq!(welcome.epoch, 2);
    assert_eq!(stored_credential(&rig, "host-a"), who.to_hex());
    assert_eq!(rig.held("host-a").unwrap().credential(), &who);
}
