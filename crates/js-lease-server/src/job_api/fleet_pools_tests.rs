//! `ListPools` and `ListProviders` over a fake status of the pools: what they carry, and what
//! they leave out.

use super::*;

/// A pool view with every field the routes write, its provider's checks failing with a
/// reason when `failing`, and `state` as its state.
fn pool_view(state: State, failing: bool) -> PoolView {
    let failed = failing.then(|| "no ssh key named js".to_owned());
    PoolView {
        name: "burst".to_owned(),
        kind: "lambdalabs".to_owned(),
        network: "default",
        exec: "container".to_owned(),
        launch: "plan",
        live_until_ms: None,
        memory: MemoryView {
            configured_bytes: 64 << 30,
            schedulable_percent: 90,
            bookable_bytes: 61_847_529_062,
        },
        state,
        reasons: failed.iter().cloned().collect(),
        credentials: Some(CheckView {
            ok: true,
            reason: None,
            checked_at_ms: T0,
        }),
        network_check: None,
        provider_checks: vec![
            NamedCheckView {
                name: "ssh-key",
                ok: !failing,
                reason: failed.clone(),
                note: Some("looked for js".to_owned()),
                checked_at_ms: T0 + 1,
            },
            NamedCheckView {
                name: "firewall",
                ok: true,
                reason: None,
                note: None,
                checked_at_ms: T0 + 2,
            },
        ],
        instances: Instances {
            n: 2,
            max: 4,
            spend_cents_per_hour: 150,
            max_spend_cents_per_hour: 1000,
        },
        blocked: None,
        no_offer: None,
        last_launch: None,
        machines: vec![
            MachineView {
                host: "burst-1".to_owned(),
                state: Phase::Registered,
                offer: "gpu_1x_a10".to_owned(),
                zone: Some("us-west-1".to_owned()),
                price_cents_per_hour: 75,
                provider_id: Some("i-1".to_owned()),
            },
            MachineView {
                host: "burst-2".to_owned(),
                state: Phase::Launching,
                offer: "gpu_1x_a10".to_owned(),
                zone: None,
                price_cents_per_hour: 75,
                provider_id: None,
            },
        ],
        orphans: Vec::new(),
        alerts: Vec::new(),
        last_tick_ms: Some(T0 + 3),
        scaler: true,
        next_launch: None,
    }
}

fn unavailable_pool() -> PoolsView {
    PoolsView {
        pools: vec![pool_view(State::Unavailable, true)],
        warnings: Vec::new(),
        logs: None,
        store: None,
    }
}

/// The pool and the provider that the body of `GET /v1/pools` writes as `pool`, as
/// `ListPools` and `ListProviders` should tell them, read from the JSON alone.
fn told_pool(pool: &Value) -> (wire::Pool, wire::Provider) {
    let text = |value: &Value| value.as_str().map(str::to_owned).unwrap_or_default();
    let number = |value: &Value| value.as_u64().unwrap();
    let state = |value: &Value| match value.as_str().unwrap() {
        "checking" => wire::Availability::Checking,
        "available" => wire::Availability::Available,
        _ => wire::Availability::Unavailable,
    };
    let check = |name: String, check: &Value| wire::ProviderCheck {
        name,
        ok: check["ok"].as_bool().unwrap(),
        reason: text(&check["reason"]),
        note: check.get("note").map(text).unwrap_or_default(),
        checked_at_ms: number(&check["checked_at_ms"]),
    };
    let named = pool["provider_checks"].as_array().unwrap().iter();
    let named = named.map(|named| check(text(&named["name"]), named));
    let checks: Vec<_> = std::iter::once(check(CREDENTIALS.to_owned(), &pool["credentials"]))
        .chain(named)
        .collect();
    let machines = pool["machines"].as_array().unwrap().iter();
    let machines = machines.map(|machine| wire::Machine {
        host: text(&machine["host"]),
        state: text(&machine["state"]),
        offer: text(&machine["offer"]),
        zone: text(&machine["zone"]),
        price_cents_per_hour: machine["price_cents_per_hour"].as_u64(),
        provider_id: text(&machine["provider_id"]),
    });
    let memory = &pool["memory"];
    let reasons = pool["reasons"].as_array().unwrap().iter().map(text);
    let told = wire::Pool {
        name: text(&pool["name"]),
        kind: text(&pool["kind"]),
        state: i32::from(state(&pool["state"])),
        reasons: reasons.collect(),
        memory: Some(wire::Memory {
            configured_bytes: number(&memory["configured_bytes"]),
            schedulable_percent: u32::try_from(number(&memory["schedulable_percent"])).unwrap(),
            schedulable_bytes: number(&memory["bookable_bytes"]),
        }),
        instances: u32::try_from(number(&pool["instances"]["n"])).unwrap(),
        last_tick_ms: pool["last_tick_ms"].as_u64(),
    };
    let provider = wire::Provider {
        name: text(&pool["name"]),
        kind: text(&pool["kind"]),
        state: i32::from(state(&pool["state"])),
        checks,
        machines: machines.collect(),
    };
    (told, provider)
}

async fn pools_of(api: &JobApi) -> (Vec<wire::Pool>, Vec<wire::Provider>) {
    let pools = list_pools(api, Request::new(ListPoolsRequest {})).await;
    let providers = list_providers(api, Request::new(ListProvidersRequest {})).await;
    (
        pools.unwrap().into_inner().pools,
        providers.unwrap().into_inner().providers,
    )
}

/// Asserts that for a pool whose provider check fails with a reason, `ListPools` and
/// `ListProviders` carry the names, states, reasons, checks and machines the body of
/// `GET /v1/pools` writes for the same status; that a provider whose checks all hold is
/// available and one before the first pass is checking; and that with no elastic pool both
/// lists are empty. It catches the views drifting apart, a provider's failure lost, and no
/// pool answered as an error.
#[tokio::test]
async fn pools_and_providers_agree_with_the_json_status() {
    let api = api_over(
        Arc::new(FakeHosts::new()),
        Arc::new(FakePools(unavailable_pool)),
        None,
    );
    let body = serde_json::to_value(unavailable_pool()).unwrap();
    let (pool, provider) = told_pool(&body["pools"][0]);
    let (pools, providers) = pools_of(&api).await;
    assert_eq!(pools, [pool]);
    assert_eq!(providers, [provider]);
    assert_eq!(providers[0].checks[1].reason, "no ssh key named js");

    let fine = || PoolsView {
        pools: vec![
            pool_view(State::Available, false),
            pool_view(State::Checking, false),
            pool_view(State::Unavailable, false),
        ],
        warnings: Vec::new(),
        logs: None,
        store: None,
    };
    let api = api_over(Arc::new(FakeHosts::new()), Arc::new(FakePools(fine)), None);
    let (pools, providers) = pools_of(&api).await;
    let (available, checking, unavailable) = (
        i32::from(wire::Availability::Available),
        i32::from(wire::Availability::Checking),
        i32::from(wire::Availability::Unavailable),
    );
    let body = serde_json::to_value(fine()).unwrap();
    let written = body["pools"].as_array().unwrap().iter();
    let written: Vec<wire::Pool> = written.map(|pool| told_pool(pool).0).collect();
    assert_eq!(pools, written);
    let pool_states: Vec<i32> = pools.iter().map(|pool| pool.state).collect();
    assert_eq!(pool_states, [available, checking, unavailable]);
    let provider_states: Vec<i32> = providers.iter().map(|provider| provider.state).collect();
    assert_eq!(provider_states, [available, checking, available]);
    let none = api_over(
        Arc::new(FakeHosts::new()),
        Arc::new(FakePools(no_pools)),
        None,
    );
    assert_eq!(pools_of(&none).await, (Vec::new(), Vec::new()));
}

/// The pool view of [`unavailable_pool`] with a recognisable text in every field the
/// messages do not carry: the plan of the next launch and its secret slots, the reason
/// there is no offer, the last launch, the orphans, the alerts, the warnings, the network,
/// the execution mode and the cap that blocks it.
fn marked_pool() -> PoolsView {
    let pool = PoolView {
        network: "MARK-NETWORK",
        exec: "MARK-EXEC".to_owned(),
        launch: "MARK-LAUNCH",
        blocked: Some("MARK-BLOCKED"),
        no_offer: Some(NoOffer {
            request: "MARK-REQUEST".to_owned(),
            dimension: "MARK-DIMENSION".to_owned(),
        }),
        last_launch: Some(LaunchView {
            at_ms: T0,
            outcome: "accepted",
            host: "MARK-LAUNCHED".to_owned(),
        }),
        orphans: vec![OrphanView {
            host: "MARK-ORPHAN".to_owned(),
            provider_id: Some("MARK-ORPHAN-ID".to_owned()),
        }],
        alerts: vec!["MARK-ALERT".to_owned()],
        next_launch: Some(NextLaunch {
            offer: "MARK-OFFER".to_owned(),
            zone: None,
            name: "MARK-NAME".to_owned(),
            plan: Some("token=MARK-TOKEN key=/MARK-KEY-PATH script=MARK-SCRIPT".to_owned()),
            plan_error: None,
            secret_slots: vec!["MARK-SLOT".to_owned()],
        }),
        ..pool_view(State::Unavailable, true)
    };
    PoolsView {
        pools: vec![pool],
        warnings: vec!["MARK-WARNING".to_owned()],
        logs: None,
        store: None,
    }
}

/// Asserts that the encoded answers of `ListPools` and `ListProviders` hold none of the
/// texts of the fields they do not carry (a plan with a token, a key path and a launch
/// script; secret slots; alerts; orphans; warnings), while they do hold a mapped field's
/// text. It catches a view or a config serialised whole into a message.
#[tokio::test]
async fn pools_and_providers_carry_no_secret() {
    let api = api_over(
        Arc::new(FakeHosts::new()),
        Arc::new(FakePools(marked_pool)),
        None,
    );
    let pools = list_pools(&api, Request::new(ListPoolsRequest {})).await;
    let providers = list_providers(&api, Request::new(ListProvidersRequest {})).await;
    let mut bytes = pools.unwrap().into_inner().encode_to_vec();
    bytes.extend(providers.unwrap().into_inner().encode_to_vec());
    let holds = |needle: &str| {
        bytes
            .windows(needle.len())
            .any(|window| window == needle.as_bytes())
    };
    assert!(holds("burst-1"));
    assert!(!holds("MARK"), "{:?}", String::from_utf8_lossy(&bytes));
}
