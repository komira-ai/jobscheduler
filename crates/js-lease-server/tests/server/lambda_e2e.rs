//! The Lambda path from a waiting request to a machine that is gone, through the real
//! server: the scaler, the registry, the join tokens, the Lambda provider over an account
//! in memory, and the Tailscale mode against an HTTP server on 127.0.0.1. The agent is the
//! test, saying the messages a real one says. Time is the test's clock and the ticks are
//! stepped; nothing sleeps and nothing measures elapsed time.

use js_provider::{Observed, Provider, StartAck, StopAck};
use js_provider_lambdalabs::{Call, TerminateEffect};
use js_scaler::Phase;
use js_types::{HostId, LeaseId};

use crate::joins::code;
use crate::lambda_e2e_support::{DELETE_DEVICE, KEYS, OAUTH, Rig};
use crate::support::request;
use crate::tailnet::AUTH_KEY;

/// Asserts the whole sequence of the design, each step through the real parts. A request
/// waits and, once the scaler has read the pool's offers, is not refused. The next tick
/// writes the Launching record, mints the join token, has the Tailscale API mint an auth
/// key for the host's name, creates the no-inbound ruleset in the offer's region and
/// launches with user data that holds the token, the key and the instance line; the record
/// file holds the Launching record and no secret. The next tick reads the machine back and
/// it is Booting. The agent says hello with the token and gets a host credential; the
/// token is dead from then on and the credential carries the heartbeats. The record is
/// Registered and the consumer's next poll places its request on the host; the host is
/// not drained while the lease is held, however long that is. After the lease ends and the
/// host has been quiet for `idle`, the record goes Draining and then Releasing, the registry
/// marks the host, the credential is revoked and the machine is terminated; while the read
/// back says Terminating the record stays Releasing and the tailnet device stays; when the
/// machine is gone the host is removed, the device is deleted and the record is Gone. At
/// every step neither status route shows a secret. The order of the record and the launch
/// is pinned by `nothing_is_launched_while_the_record_cannot_be_written`, and the order of
/// the revoke and the terminate by `a_machine_is_not_terminated_before_its_token_is_revoked_and_a_restart_finishes_the_release`;
/// here the credential is already refused when the first terminate is seen. It catches a
/// launch that does not carry the token or the key, a token that outlives the
/// first hello, a host released while it holds a lease, a machine terminated while its
/// credential works, a record that reads Terminating as gone, and a device left behind.
#[tokio::test]
async fn a_waiting_request_is_served_by_a_launched_host_that_is_released_when_idle() {
    let rig = Rig::start("e2e_life").await;
    rig.account
        .set_terminate_effect(TerminateEffect::Terminating);
    rig.tick().await;
    rig.wait();
    assert_eq!(rig.account.launched(), 0);

    rig.tick().await;
    assert_eq!(rig.account.launched(), 1);
    assert_eq!(rig.phase(), Phase::Launching);
    let name = rig.name();
    let token = rig.token(0);
    let script = rig.script(0);
    let launch = rig.account.launches().remove(0);
    let rulesets = rig.account.firewall_rulesets();
    assert_eq!(rulesets.len(), 1);
    assert_eq!(
        (rulesets[0].rules, rulesets[0].region.as_str()),
        (0, "us-west-1")
    );
    assert_eq!(launch.firewall_ruleset_ids, [rulesets[0].id.clone()]);
    assert_eq!(launch.region, "us-west-1");
    assert_eq!(launch.instance_type, "gpu_1x_a100");
    assert_eq!(launch.name, name);
    assert!(script.contains(&format!("burst:{name}")));
    assert!(
        script.contains(AUTH_KEY),
        "the script holds the Tailscale key"
    );
    assert!(script.contains(&format!("--hostname={name}")));
    assert!(rig.asked(OAUTH) >= 1);
    assert_eq!(rig.asked(KEYS), 1);
    let minted = rig
        .tailnet
        .seen()
        .into_iter()
        .find(|s| s.target.ends_with("/keys"));
    assert_eq!(minted.unwrap().json()["description"], name.as_str());
    let written = rig.file();
    assert_eq!(written.instances.len(), 1);
    assert_eq!(written.instances[0].phase, Phase::Launching);
    assert_eq!(written.instances[0].cloud_id, None);
    let raw = std::fs::read_to_string(crate::scaler_support::records_path(&rig.h.scratch));
    let raw = raw.unwrap();
    assert!(!raw.contains(&token) && !raw.contains(AUTH_KEY));
    rig.assert_status_is_clean(&[token.as_str()]).await;

    rig.show_device(&name);
    rig.tick().await;
    assert_eq!(rig.phase(), Phase::Booting);
    assert!(rig.file().instances[0].cloud_id.is_some());
    assert!(rig.registry().hosts().is_empty());

    let (me, credential) = rig.join(&token, &name).await;
    assert_eq!(
        rig.beat(&token, &me).await.status,
        401,
        "the token is retired"
    );
    assert_eq!(rig.beat(&credential, &me).await.status, 200);
    rig.assert_status_is_clean(&[token.as_str(), credential.as_str()])
        .await;
    rig.tick().await;
    assert_eq!(rig.phase(), Phase::Registered);
    assert_eq!(rig.registry().hosts().len(), 1);

    // The consumer's poll commits the lease on the host; it stays pending until the
    // agent reports the host ready, and then the same poll is answered Started.
    assert!(matches!(rig.present(), StartAck::Unknown));
    assert!(rig.registry().waiting().is_empty());
    let lease = LeaseId::new(1, 1);
    assert_eq!(rig.ready(&credential, &me).await, lease.to_string());
    let ack = rig.present();
    assert!(matches!(&ack, StartAck::Started { host } if host.as_str() == name));
    let host = HostId::new(&name).unwrap();
    assert_eq!(
        rig.registry().provider().observe(&lease),
        Observed::Present { host }
    );

    for _ in 0..30 {
        rig.advance(10);
        assert_eq!(rig.beat(&credential, &me).await.status, 200);
        rig.tick().await;
    }
    assert_eq!(
        rig.phase(),
        Phase::Registered,
        "a leased host is never drained"
    );
    assert_eq!(rig.account.count(Call::Terminate), 0);

    let stopped = rig.registry().provider().stop(&lease);
    assert!(matches!(stopped, StopAck::Stopped { .. }));
    for _ in 0..2 {
        assert_eq!(rig.beat(&credential, &me).await.status, 200);
    }
    let mut phases = vec![rig.phase()];
    for _ in 0..40 {
        rig.advance(10);
        let beat = rig.beat(&credential, &me).await;
        if rig.account.count(Call::Terminate) == 0 {
            assert_eq!(beat.status, 200, "{}", beat.body);
        }
        rig.tick().await;
        if phases.last() != Some(&rig.phase()) {
            phases.push(rig.phase());
        }
        if rig.account.count(Call::Terminate) > 0 {
            break;
        }
    }
    assert_eq!(
        phases,
        [Phase::Registered, Phase::Draining, Phase::Releasing]
    );
    assert_eq!(rig.mark(&name).await, "releasing");
    assert_eq!(
        rig.beat(&credential, &me).await.status,
        401,
        "revoked with the release"
    );
    assert_eq!(
        rig.asked(DELETE_DEVICE),
        0,
        "the device stays while the machine is up"
    );
    assert_eq!(rig.account.instances().len(), 1);

    rig.tick().await;
    assert_eq!(rig.phase(), Phase::Releasing, "terminating is not gone");
    assert_eq!(rig.asked(DELETE_DEVICE), 0);
    assert_eq!(rig.registry().hosts().len(), 1);
    let id = rig.account.instances()[0].id.clone();
    rig.account.remove_instance(&id);
    rig.tick().await;
    rig.tick().await;
    assert_eq!(rig.phase(), Phase::Gone);
    assert_eq!(rig.asked(DELETE_DEVICE), 1);
    assert!(rig.registry().hosts().is_empty());
    assert_eq!(rig.file().instances[0].phase, Phase::Gone);
    assert_eq!(rig.account.launched(), 1);
    assert_eq!(rig.account.count(Call::Launch), 1);
    assert_eq!(rig.asked(KEYS), 1);
    rig.assert_status_is_clean(&[token.as_str(), credential.as_str()])
        .await;
}

/// Asserts what the join token's binding does and does not do. The right token with
/// another instance is refused `join-token-instance` and stays unused. The right instance
/// with the token succeeds, whoever sends it: the binding guards a mix-up between launches
/// and claims no more. After that hello the token alone is the 401 of an unknown token, on a
/// heartbeat, on a hello of the same host and on a hello of a second host id, and the host
/// credential carries the heartbeats. It catches a hello admitted for the wrong instance,
/// a refusal that spends the token, and a token that stays good after the credential.
#[tokio::test]
async fn the_binding_refuses_another_instance_and_the_token_dies_at_the_first_hello() {
    let rig = Rig::start("e2e_binding").await;
    let (name, token) = rig.launch().await;

    let wrong = rig.hello(&token, &name, "someone-elses-machine").await;
    assert_eq!(wrong.status, 403, "{}", wrong.body);
    assert_eq!(code(&wrong), "join-token-instance");
    assert!(rig.registry().hosts().is_empty());

    let (me, credential) = rig.join(&token, &name).await;
    assert_eq!(rig.beat(&token, &me).await.status, 401);
    for host in [name.as_str(), "burst-second-host"] {
        let again = rig.hello(&token, host, &name).await;
        assert_eq!(again.status, 401, "{}", again.body);
    }
    assert_eq!(rig.registry().hosts().len(), 1);
    assert_eq!(rig.beat(&credential, &me).await.status, 200);
}

/// Asserts a pool in `default` mode (the operator's own route to the scheduler) when the
/// host is not there to say hello: the user data has no Tailscale step and no key, the
/// Tailscale API is not asked once, the record stays Booting until `boot_timeout` and then
/// the machine is released with its token revoked (a hello with it is the 401 of an
/// unknown token) and the pool launches nothing for `boot_fail_cooldown`. It catches a
/// `default` pool whose launch still carries a Tailscale step or calls the API, and one
/// that bills past `boot_timeout`.
#[tokio::test]
async fn a_default_mode_pool_whose_host_has_no_route_is_released_at_boot_timeout() {
    let rig = Rig::start_default("e2e_default_silent").await;
    rig.complete_view(&[]).await;
    let (name, token) = rig.launch().await;
    let script = rig.script(0);
    assert!(!script.to_lowercase().contains("tailscale"), "{script}");
    assert!(!script.contains(AUTH_KEY));
    assert!(rig.tailnet.seen().is_empty(), "{:?}", rig.tailnet.seen());

    rig.advance(14 * 60);
    rig.wait();
    rig.tick().await;
    assert_eq!(
        rig.phase(),
        Phase::Booting,
        "one minute before boot_timeout"
    );
    assert_eq!(rig.account.count(Call::Terminate), 0);
    rig.advance(60);
    rig.wait();
    rig.tick().await;
    assert_eq!(rig.phase(), Phase::Releasing);
    assert_eq!(rig.account.count(Call::Terminate), 1);
    assert_eq!(rig.hello(&token, &name, &name).await.status, 401);
    rig.wait();
    rig.tick().await;
    assert_eq!(rig.phase(), Phase::Gone);
    assert_eq!(rig.state().blocked["burst"], js_scaler::Blocked::BootFailed);
    assert_eq!(rig.account.launched(), 1);
    assert!(rig.tailnet.seen().is_empty(), "no Tailscale call at all");
}

/// Asserts the same pool when the host is there (the operator's own route): the hello is
/// admitted with the token the user data holds, the record is Registered, the request is
/// placed on the host and the lease is ready, the same path as with Tailscale, with no
/// Tailscale call at all. It catches a `default` path that needs the tailnet to serve.
#[tokio::test]
async fn a_default_mode_pool_whose_host_says_hello_serves_a_request() {
    let rig = Rig::start_default("e2e_default_hello").await;
    let (name, token) = rig.launch().await;
    let (me, credential) = rig.join(&token, &name).await;
    rig.tick().await;
    assert_eq!(rig.phase(), Phase::Registered);
    assert!(matches!(rig.present(), StartAck::Unknown));
    rig.ready(&credential, &me).await;
    let ack = rig.present();
    assert!(matches!(&ack, StartAck::Started { host } if host.as_str() == name));
    let host = HostId::new(&name).unwrap();
    let observed = rig.registry().provider().observe(&LeaseId::new(1, 1));
    assert_eq!(observed, Observed::Present { host });
    assert!(rig.tailnet.seen().is_empty(), "no Tailscale call at all");
    assert_eq!(rig.account.launched(), 1);
}

/// Asserts a refused OAuth client end to end. The Tailscale API answers 401 to the OAuth
/// exchange at the first check: the server starts, the pool is unavailable with the
/// network check failed and the cloud check passed, `/healthz` is 200, no secret is shown,
/// and a request that only a launch could serve is refused at once (it does not wait on a
/// dead network) with nothing launched. When the client is accepted again the next check
/// makes the pool available and the request waits, is launched for, and the machine is
/// launched. It catches a pool that launches over a network it cannot use, a server that
/// stops, and a request left waiting on a pool that cannot serve it.
#[tokio::test]
async fn a_refused_oauth_client_leaves_the_pool_unavailable_until_it_is_accepted() {
    let tailnet = crate::lambda_e2e_support::tailnet();
    tailnet.set(
        OAUTH,
        crate::tailnet::Reply::json(401, r#"{"message":"no"}"#),
    );
    let rig = Rig::over("e2e_oauth", tailnet, true).await;

    let reply = request(rig.h.running.addr(), "GET", "/v1/pools", None, None).await;
    let pool = reply.json()["pools"][0].clone();
    assert_eq!(pool["state"], "unavailable", "{pool}");
    assert_eq!(pool["network_check"]["ok"], false);
    assert_eq!(pool["credentials"]["ok"], true);
    let health = request(rig.h.running.addr(), "GET", "/healthz", None, None).await;
    assert_eq!((health.status, health.body.as_str()), (200, "ok"));
    rig.assert_status_is_clean(&[]).await;
    rig.tick().await;
    assert!(!matches!(rig.present(), StartAck::Started { .. }));
    assert!(rig.registry().waiting().is_empty());
    assert_eq!(rig.account.launched(), 0);

    rig.tailnet
        .set(OAUTH, crate::lambda_e2e_support::token_reply());
    rig.advance(10);
    rig.h.checks.step().await;
    let reply = request(rig.h.running.addr(), "GET", "/v1/pools", None, None).await;
    assert_eq!(reply.json()["pools"][0]["state"], "available");
    rig.tick().await;
    rig.wait();
    rig.tick().await;
    assert_eq!(rig.account.launched(), 1);
    rig.assert_status_is_clean(&[]).await;
}

/// Asserts that the unit the machine boots with gives the agent the name the scaler's
/// record has (`--host-id <name>`), so the host registers under the name the scaler looks
/// for, and that the registry then lists the host under it. It catches a unit whose agent
/// registers under a name of its own, which the scaler never sees: the machine is then
/// Booting until `boot_timeout` however well it boots.
#[tokio::test]
async fn the_unit_names_the_host_the_scaler_looks_for() {
    let rig = Rig::start("e2e_host_id").await;
    let (name, token) = rig.launch().await;
    assert_eq!(rig.unit_host_id(0).as_deref(), Some(name.as_str()));
    rig.join(&token, &name).await;
    let listing = crate::agents_support::hosts(&rig.h).await;
    assert_eq!(crate::agents_support::listed(&listing), [name.as_str()]);
}

/// The unit of `script` with `flag` and the word after it removed from the `ExecStart` line.
fn without_flag(script: &str, flag: &str) -> String {
    let mut out = Vec::new();
    for line in script.lines() {
        let words: Vec<&str> = line.split(' ').collect();
        match words.iter().position(|word| *word == flag) {
            Some(at) if line.trim_start().starts_with("ExecStart=") => {
                let kept = words.iter().enumerate();
                let kept = kept.filter(|(i, _)| *i != at && *i != at + 1);
                out.push(kept.map(|(_, word)| *word).collect::<Vec<_>>().join(" "));
            }
            _ => out.push(line.to_owned()),
        }
    }
    out.join("\n")
}

/// Asserts that the unit the machine boots with gives the agent what a GPU pool in
/// `container` mode demands of a host: with `--container-bin` removed from the unit the
/// agent reports no runtime and its hello is refused `host-contract` naming `runtime`; with
/// `--gpu-toolkit-hook` removed it reports no toolkit and is refused `host-contract` naming
/// `gpu-toolkit`; the registry holds no host after either, and the unit as rendered is
/// admitted (the other tests). It catches a unit that drops either flag, which would boot
/// a machine the pool refuses and release it at `boot_timeout`.
#[tokio::test]
async fn a_unit_without_the_container_runtime_or_the_gpu_hook_is_refused_by_the_pool() {
    for (flag, field) in [
        ("--container-bin", "runtime"),
        ("--gpu-toolkit-hook", "gpu-toolkit"),
    ] {
        let rig = Rig::start("e2e_unit_contract").await;
        let (name, token) = rig.launch().await;
        let script = rig.script(0);
        assert!(script.contains(flag), "the rendered unit gives {flag}");
        let edited = without_flag(&script, flag);
        assert!(!edited.contains(flag));
        let bytes = rig.hello_from(&edited, &name).to_string().into_bytes();
        let addr = rig.h.running.agent_addr().unwrap();
        let path = "/v1/agent/hello";
        let reply = request(addr, "POST", path, Some(&token), Some(&bytes)).await;
        assert_eq!(reply.status, 403, "{flag}: {}", reply.body);
        assert_eq!(code(&reply), "host-contract", "{flag}");
        assert_eq!(reply.json()["field"], field, "{flag}");
        assert!(rig.registry().hosts().is_empty());
    }
}
