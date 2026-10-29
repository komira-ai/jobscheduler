//! A server with a launching pool, started from a configuration file, over an account in
//! memory: a pool in `plan` launches nothing, a `live` pool launches once and not again
//! after a restart, a machine goes through its whole life, and a host whose release was
//! under way when the server stopped is marked when it says hello again. Time is the
//! test's clock and the ticks are stepped; nothing sleeps and no network is touched.

use std::collections::BTreeMap;
use std::os::unix::fs::PermissionsExt;
use std::time::Duration;

use js_elastic::AcquireId;
use js_lease_server::RecordsFile;
use js_provider::{Provider, StartAck};
use js_provider_lambdalabs::{Call, TerminateEffect};
use js_scaler::{Blocked, Millis, Phase, Record, Records, StoredOffer};
use serde_json::json;

use crate::agents_support::{entry, hosts};
use crate::scaler_support::{
    account, beat, hello, host_id_in, join, name_of, records_path, spec, start, start_refused,
    text, text_with, token_in,
};
use crate::support::{Harness, Scratch, T0};

fn secs(n: u64) -> Duration {
    Duration::from_secs(n)
}

/// The phase of the first record the scaler of `h` holds.
fn phase(h: &Harness) -> Phase {
    h.running.scaler().unwrap().state().records.instances[0].phase
}

/// Asserts that a pool in `plan` mode launches nothing however often it ticks: the account
/// sees reads and no launch, the scaler says it is in plan, no record is written, and a
/// request that only a launch could serve is refused at once and not queued, because a
/// plan pool answers "could a launch serve this" with no. It catches a launch from a pool
/// whose `launch` is not `live`.
#[tokio::test]
async fn a_plan_pool_launches_nothing() {
    let scratch = Scratch::new("scaler_plan");
    let account = account(&scratch);
    let text = text(&scratch, "plan", "initialise");
    let h = start(scratch, &text).await;
    for _ in 0..3 {
        h.ticks.step().await;
    }
    let registry = h.running.registry().unwrap();
    let ack = registry.provider().start(spec(1, "r1"));
    assert!(!matches!(ack, StartAck::Started { .. }));
    assert!(registry.waiting().is_empty());
    h.ticks.step().await;

    assert_eq!(account.launched(), 0);
    assert_eq!(account.count(Call::Launch), 0);
    assert!(account.count(Call::InstanceTypes) >= 1);
    assert!(!records_path(&h.scratch).exists());
    let state = h.running.scaler().unwrap().state();
    assert_eq!(state.blocked["burst"], Blocked::Plan);
    assert!(state.records.instances.is_empty());
}

/// Asserts that a pool in `plan` mode starts without the pin of the agent a live pool
/// needs, and ticks without launching: the scaler holds it in plan. It catches a plan pool
/// that needs what only a launch downloads.
#[tokio::test]
async fn a_plan_pool_starts_without_an_agent_pin() {
    let scratch = Scratch::new("scaler_plan_no_agent");
    account(&scratch);
    let source = text_with(&scratch, "plan", "initialise", &[("agent", "")]);
    let h = start(scratch, &source).await;
    h.ticks.step().await;
    let state = h.running.scaler().unwrap().state();
    assert_eq!(state.blocked["burst"], Blocked::Plan);
}

/// Asserts that a `live` pool launches once for a request and not again after a restart:
/// before the scaler has ticked a request is refused (no launch is known to be possible),
/// after it the request waits; one tick later the machine is launched with a join token in
/// its user data, the records are in a file that only the owner can use, and the account
/// holds one machine; the ticks that follow add none. The server is stopped and another
/// started over the same files, the request is presented again, and the account still holds
/// one machine, the scaler one record, and no second token is minted for it. It catches a
/// launch that is forgotten at a restart, and a request queued before a launch is known to
/// be possible.
#[tokio::test]
async fn a_live_pool_launches_once_and_not_again_after_a_restart() {
    let scratch = Scratch::new("scaler_live");
    let account = account(&scratch);
    let first = text(&scratch, "live", "initialise");
    let h = start(scratch, &first).await;
    let registry = h.running.registry().unwrap();
    let early = registry.provider().start(spec(1, "early"));
    assert!(!matches!(early, StartAck::Started { .. }));
    assert!(registry.waiting().is_empty());

    h.ticks.step().await;
    registry.provider().start(spec(2, "r1"));
    assert_eq!(registry.waiting().len(), 1);
    h.ticks.step().await;
    assert_eq!(account.launched(), 1);
    let path = records_path(&h.scratch);
    assert_eq!(
        std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
        0o600
    );
    let written = RecordsFile::new(path.clone()).load().unwrap();
    assert_eq!(written.instances.len(), 1);
    assert_eq!(written.instances[0].pool, "burst");
    // The acquire id carries 128 bits from the operating system's source; the
    // deterministic source of the tests ends in a long run of zero digits.
    let acquire = written.instances[0].acquire.as_ref().unwrap().to_string();
    assert!(acquire.starts_with("burst"), "{acquire}");
    assert!(!acquire.ends_with("0000000000000000"), "{acquire}");
    let script = account.launches()[0].user_data().unwrap().to_owned();
    let name = written.instances[0].name.clone();
    assert!(script.contains(&format!("burst:{name}")));
    assert!(!token_in(&script).is_empty());

    h.ticks.step().await;
    h.ticks.step().await;
    assert_eq!(account.launched(), 1);
    assert_eq!(phase(&h), Phase::Booting);

    let Harness {
        running, scratch, ..
    } = h;
    running.shutdown().await.unwrap();
    let again = text(&scratch, "live", "resume");
    let h = start(scratch, &again).await;
    h.ticks.step().await;
    let registry = h.running.registry().unwrap();
    registry.provider().start(spec(3, "r1"));
    assert_eq!(registry.waiting().len(), 1);
    for _ in 0..3 {
        h.ticks.step().await;
    }
    assert_eq!(account.launched(), 1);
    assert_eq!(account.count(Call::Launch), 1);
    let state = h.running.scaler().unwrap().state();
    assert_eq!(state.records.instances.len(), 1);
    assert_eq!(state.records.instances[0].name, name);
    assert_eq!(state.records.instances[0].phase, Phase::Booting);
}

/// Asserts a machine's whole life through the server: launched for a request, booted, its
/// agent says hello with the token from the user data and is registered; the request goes
/// away, and after `idle` of quiet the host is drained, marked releasing, its token revoked
/// and the machine terminated, in that order, with the host's own heartbeats accepted until
/// the tick that terminates it and refused after; the next tick removes the host. It
/// catches a token that outlives the machine and a host left in the registry.
#[tokio::test]
async fn a_machine_is_launched_registered_idle_released_and_removed() {
    let scratch = Scratch::new("scaler_life");
    let account = account(&scratch);
    account.set_terminate_effect(TerminateEffect::Removed);
    let source = text(&scratch, "live", "initialise");
    let h = start(scratch, &source).await;
    h.ticks.step().await;
    let registry = h.running.registry().unwrap();
    registry.provider().start(spec(1, "r1"));
    h.ticks.step().await;
    let name = h.running.scaler().unwrap().state().records.instances[0]
        .name
        .clone();
    let script = account.launches()[0].user_data().unwrap().to_owned();
    let token = token_in(&script);
    // The agent registers under the id its unit names, which is the record's name; the
    // scaler finds the host by that name, so the record moves past Booting.
    let registers_as = host_id_in(&script);
    assert_eq!(registers_as, name);
    h.ticks.step().await;
    assert_eq!(phase(&h), Phase::Booting);
    let me = join(&h, &token, &registers_as).await;
    h.ticks.step().await;
    assert_eq!(phase(&h), Phase::Registered);

    for _ in 0..40 {
        h.clock.advance(secs(10));
        let reply = beat(&h, &token, &me).await;
        if account.count(Call::Terminate) == 0 {
            assert_eq!(reply.status, 200, "{}", reply.body);
        }
        h.ticks.step().await;
        if account.count(Call::Terminate) > 0 {
            break;
        }
    }
    assert_eq!(account.count(Call::Terminate), 1);
    let after = beat(&h, &token, &me).await;
    assert_eq!(after.status, 401, "the token still works: {}", after.body);
    assert!(account.instances().is_empty());

    h.ticks.step().await;
    assert_eq!(phase(&h), Phase::Gone);
    assert!(registry.hosts().is_empty());
    assert_eq!(account.launched(), 1);
}

/// A record of the machine `name` of the pool `burst` that was being released.
fn releasing(name: &str) -> Records {
    Records {
        instances: vec![Record {
            pool: "burst".to_owned(),
            acquire: Some(AcquireId::new("burst-0001").unwrap()),
            name: name.to_owned(),
            offer: StoredOffer::nothing("gpu_1x_a100", Some("us-west-1".to_owned())),
            price_cents_per_hour: 129,
            phase: Phase::Releasing,
            since: Millis::new(T0),
            launched_at: Millis::new(T0),
            token_expires: None,
            cloud_id: Some("i-1".to_owned()),
            retry_at: None,
            attempt: 0,
            gone_at: None,
            registered: true,
            condemned: false,
            clean_lists: 0,
            last_clean: None,
            seen_live: false,
        }],
        cooldowns: BTreeMap::new(),
    }
}

/// Asserts the restart case of a release under way. The server stops after the scaler
/// wrote `Releasing` for a machine and before its token was revoked, so the token still
/// works. The server starts again; before any tick, and before any hello is admitted, the
/// scaler marks the machine ending; the agent says hello again with the token, is admitted
/// and is listed `releasing`, takes no lease and is reserved by no request; the first tick
/// revokes the token and releases the machine, the second removes the host, and a hello
/// with the token is then refused. It catches a host that rejoins unmarked and could be
/// booked before its machine is terminated.
#[tokio::test]
async fn a_host_whose_release_was_under_way_is_marked_when_it_says_hello_again() {
    let scratch = Scratch::new("scaler_restart_release");
    let account = account(&scratch);
    let name = name_of("js", "burst-0001");
    let first = text(&scratch, "live", "initialise");
    let h = start(scratch, &first).await;
    let body = json!({ "provider": "burst", "instance_id": name });
    let minted = h.call("POST", "/v1/agent/join-tokens", Some(&body)).await;
    assert_eq!(minted.status, 201, "{}", minted.body);
    let token = minted.json()["token"].as_str().unwrap().to_owned();
    let Harness {
        running, scratch, ..
    } = h;
    running.shutdown().await.unwrap();
    RecordsFile::new(records_path(&scratch))
        .save(&releasing(&name))
        .unwrap();

    let again = text(&scratch, "live", "resume");
    let h = start(scratch, &again).await;
    let me = join(&h, &token, &name).await;
    let listing = hosts(&h).await;
    assert_eq!(entry(&listing, &name)["mark"], "releasing");
    assert!(entry(&listing, &name).get("lease").is_none());
    let registry = h.running.registry().unwrap();
    let ack = registry.provider().start(spec(1, "r1"));
    assert!(!matches!(ack, StartAck::Started { .. }));
    let counts = registry.whole_counts();
    assert_eq!((counts.reserved, counts.held), (0, 0));
    assert_eq!(beat(&h, &token, &me).await.status, 200);

    h.ticks.step().await;
    assert_eq!(beat(&h, &token, &me).await.status, 401);
    h.ticks.step().await;
    assert_eq!(phase(&h), Phase::Gone);
    assert!(registry.hosts().is_empty());
    assert_eq!(hello(&h, &token, &name).await.status, 401);

    // The request that could not be placed on the releasing host is served by a machine of
    // its own, and the releasing machine is not launched again.
    assert_eq!(account.launched(), 1);
    let state = h.running.scaler().unwrap().state();
    assert_eq!(state.records.instances.len(), 2);
    assert_ne!(state.records.instances[1].name, name);
}

/// Asserts that a missing records file is a first start only when the operator says so
/// (`--boot initialise`). A live pool started with `initialise` and no file starts, and
/// writes no file until it has something to record; started again with `resume` and still
/// no file it is refused, and the refusal names the file and the remedy; a pool in `plan`
/// mode starts with `resume` and no file; and a live pool with the file present starts with
/// `resume`. It catches a deleted or moved records file read as an empty memory, which
/// would launch again for machines in flight and reset the day's spend.
#[tokio::test]
async fn a_live_pool_with_no_records_file_starts_only_at_a_first_start() {
    let scratch = Scratch::new("scaler_missing_records");
    account(&scratch);
    let first = text(&scratch, "live", "initialise");
    let h = start(scratch, &first).await;
    h.ticks.step().await;
    let Harness {
        running, scratch, ..
    } = h;
    running.shutdown().await.unwrap();
    let path = records_path(&scratch);
    assert!(!path.exists());

    let resumed = text(&scratch, "live", "resume");
    let said = start_refused(&scratch, &resumed).await;
    assert!(said.contains(&path.display().to_string()), "{said}");
    assert!(said.contains("is missing"), "{said}");
    assert!(said.contains("--boot initialise"), "{said}");

    let planned = text(&scratch, "plan", "resume");
    let h = start(scratch, &planned).await;
    let Harness {
        running, scratch, ..
    } = h;
    running.shutdown().await.unwrap();

    RecordsFile::new(path).save(&Records::default()).unwrap();
    let present = text(&scratch, "live", "resume");
    let h = start(scratch, &present).await;
    assert!(
        h.running
            .scaler()
            .unwrap()
            .state()
            .records
            .instances
            .is_empty()
    );
}
