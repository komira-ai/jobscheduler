//! The Lambda path from a queued job to a machine that is gone, through the real
//! server: the scaler, the registry, the Lambda provider over an account in memory, and the
//! Tailscale mode against an HTTP server on 127.0.0.1. No agent says hello: a launched
//! machine joins with a host certificate, which the test server has no way to give it, so
//! each machine here ends at `boot_timeout`. Time is the test's clock and the ticks are
//! stepped; nothing sleeps and nothing measures elapsed time.

use js_provider_lambdalabs::{Call, TerminateEffect};
use js_scaler::Phase;

use crate::lambda_e2e_support::{DELETE_DEVICE, KEYS, OAUTH, Rig};
use crate::support::request;
use crate::tailnet::AUTH_KEY;

/// Asserts the sequence of a launch through the real parts, ending at the boot timeout. A
/// job is queued and, once the scaler has read the pool's offers, is not refused. The next
/// tick writes the Launching record, has the Tailscale API mint an auth key for the host's
/// name, creates the no-inbound ruleset in the offer's region and launches with user data
/// that holds the key and a unit that names the host and no token or instance file; the
/// record file holds the Launching record and no secret. The next tick reads the machine
/// back and it is Booting. No agent says hello, so `boot_timeout` after the launch the
/// record goes Releasing and the machine is terminated; while the read back says
/// Terminating the record stays Releasing and the tailnet device stays; when the machine is
/// gone the device is deleted and the record is Gone. At every step neither status route
/// shows a secret. It catches a launch that does not carry the key, a unit that still names
/// a token file, a record that reads Terminating as gone, and a device left behind.
#[tokio::test]
async fn a_queued_job_gets_a_launched_machine_that_is_released_at_boot_timeout() {
    let rig = Rig::start("e2e_life").await;
    rig.account
        .set_terminate_effect(TerminateEffect::Terminating);
    rig.complete_view().await;
    rig.tick().await;
    rig.wait().await;
    assert_eq!(rig.account.launched(), 0);

    rig.tick().await;
    assert_eq!(rig.account.launched(), 1);
    assert_eq!(rig.phase(), Phase::Launching);
    let name = rig.name();
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
    for retired in ["--token-file", "--instance-file"] {
        assert!(!script.contains(retired), "{retired} is in the unit");
    }
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
    assert!(!raw.contains(AUTH_KEY));
    rig.assert_status_is_clean(&[]).await;

    rig.show_device(&name);
    rig.tick().await;
    assert_eq!(rig.phase(), Phase::Booting);
    assert!(rig.file().instances[0].cloud_id.is_some());
    assert!(rig.registry().hosts().is_empty());

    // No agent says hello: the machine is given up on at `boot_timeout`.
    rig.advance(14 * 60);
    rig.wait().await;
    rig.tick().await;
    assert_eq!(
        rig.phase(),
        Phase::Booting,
        "one minute before boot_timeout"
    );
    assert_eq!(rig.account.count(Call::Terminate), 0);
    rig.advance(60);
    rig.wait().await;
    rig.tick().await;
    assert_eq!(rig.phase(), Phase::Releasing);
    assert_eq!(rig.account.count(Call::Terminate), 1);
    assert_eq!(
        rig.asked(DELETE_DEVICE),
        0,
        "the device stays while the machine is up"
    );
    assert_eq!(rig.account.instances().len(), 1);

    rig.tick().await;
    assert_eq!(rig.phase(), Phase::Releasing, "terminating is not gone");
    assert_eq!(rig.asked(DELETE_DEVICE), 0);
    let id = rig.account.instances()[0].id.clone();
    rig.account.remove_instance(&id);
    rig.tick().await;
    rig.tick().await;
    assert_eq!(rig.phase(), Phase::Gone);
    assert_eq!(rig.asked(DELETE_DEVICE), 1);
    assert!(rig.registry().hosts().is_empty());
    assert_eq!(rig.file().instances[0].phase, Phase::Gone);
    assert_eq!(rig.account.count(Call::Launch), 1);
    assert_eq!(rig.asked(KEYS), 1);
    rig.assert_status_is_clean(&[]).await;
}

/// Asserts a pool in `default` mode (the operator's own route to the scheduler) when the
/// host is not there to say hello: the user data has no Tailscale step and no key, the
/// Tailscale API is not asked once, the record stays Booting until `boot_timeout` and then
/// the machine is released and the pool launches nothing for `boot_fail_cooldown`. It catches a
/// `default` pool whose launch still carries a Tailscale step or calls the API, and one
/// that bills past `boot_timeout`.
#[tokio::test]
async fn a_default_mode_pool_whose_host_has_no_route_is_released_at_boot_timeout() {
    let rig = Rig::start_default("e2e_default_silent").await;
    rig.complete_view().await;
    rig.launch().await;
    let script = rig.script(0);
    assert!(!script.to_lowercase().contains("tailscale"), "{script}");
    assert!(!script.contains(AUTH_KEY));
    assert!(rig.tailnet.seen().is_empty(), "{:?}", rig.tailnet.seen());

    rig.advance(14 * 60);
    rig.wait().await;
    rig.tick().await;
    assert_eq!(
        rig.phase(),
        Phase::Booting,
        "one minute before boot_timeout"
    );
    assert_eq!(rig.account.count(Call::Terminate), 0);
    rig.advance(60);
    rig.wait().await;
    rig.tick().await;
    assert_eq!(rig.phase(), Phase::Releasing);
    assert_eq!(rig.account.count(Call::Terminate), 1);
    rig.wait().await;
    rig.tick().await;
    assert_eq!(rig.phase(), Phase::Gone);
    assert_eq!(rig.state().blocked["burst"], js_scaler::Blocked::BootFailed);
    assert_eq!(rig.account.launched(), 1);
    assert!(rig.tailnet.seen().is_empty(), "no Tailscale call at all");
}

/// Asserts a refused OAuth client end to end. The Tailscale API answers 401 to the OAuth
/// exchange at the first check: the server starts, the pool is unavailable with the
/// network check failed and the cloud check passed, `/healthz` is 200, no secret is shown,
/// and a job submitted then is queued with nothing launched for it. When the client is
/// accepted again the next check makes the pool available and the job is launched for. It catches a pool that launches over a network it cannot use, a server that
/// stops, and a job left waiting on a pool that cannot serve it.
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
    let queued = crate::scaler_support::submit(&rig.h, "j1").await;
    assert_eq!(queued.status, 201, "{}", queued.body);
    assert_eq!(rig.account.launched(), 0);

    rig.tailnet
        .set(OAUTH, crate::lambda_e2e_support::token_reply());
    rig.advance(10);
    rig.h.checks.step().await;
    let reply = request(rig.h.running.addr(), "GET", "/v1/pools", None, None).await;
    assert_eq!(reply.json()["pools"][0]["state"], "available");
    rig.tick().await;
    rig.wait().await;
    rig.tick().await;
    assert_eq!(rig.account.launched(), 1);
    rig.assert_status_is_clean(&[]).await;
}

/// Asserts that the unit the machine boots with gives the agent the name the scaler's
/// record has (`--host-id <name>`), so the host registers under the name the scaler looks
/// for. It catches a unit whose agent registers under a name of its own, which the scaler
/// never sees: the machine is then Booting until `boot_timeout` however well it boots.
#[tokio::test]
async fn the_unit_names_the_host_the_scaler_looks_for() {
    let rig = Rig::start("e2e_host_id").await;
    let name = rig.launch().await;
    assert_eq!(rig.unit_host_id(0).as_deref(), Some(name.as_str()));
}
