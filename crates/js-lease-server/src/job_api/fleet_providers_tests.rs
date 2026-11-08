//! What `ListProviders` tells of a provider besides its checks and machines, over a fake
//! status of the pools: the catalog, the instance types passed over for quota, the offers
//! declined for capacity, the brake, the cool-down and the launches out, each on its own
//! provider only.

use js_elastic::{Instant, OfferKey};
use js_scaler::{Brake, Cooldown, CooldownReason, Millis};
use js_sched::failover::PassedOver;
use js_types::RequestId;

use super::pools::pool_view;
use super::*;
use crate::scaler::InFlight;

/// The status's time in these tests.
const NOW: u64 = T0 + 600_000;

fn at(millis: u64) -> Instant {
    Instant::from_millis(millis)
}

/// The provider id of the pools of these tests.
fn provider_id() -> ProviderId {
    ProviderId::known("lambdalabs")
}

/// What the status reads of the provider of `burst`: a catalog of 3 offers fetched at
/// T0 + 1 s; one instance type passed over in a region and one with no zone; one offer
/// declined for request `job-1`; a brake engaged after 3 failures; a cool-down until
/// NOW + 5 min for a failed boot; and one launch out since NOW - 42 s.
fn held_state() -> ProviderState {
    let key = |offer: &str, zone: Option<&str>| OfferKey {
        provider: provider_id(),
        offer: offer.to_owned(),
        zone: zone.map(str::to_owned),
    };
    let passed = |instance_type: &str, region: Option<&str>| PassedOver {
        provider: provider_id(),
        instance_type: instance_type.to_owned(),
        region: region.map(str::to_owned),
    };
    ProviderState {
        now_ms: NOW,
        catalog: Some((at(T0 + 1000), 3)),
        passed_over: vec![
            (
                passed("gpu_8x_h100", Some("us-east-1")),
                at(NOW + 3_600_000),
            ),
            (passed("gpu_1x_a10", None), at(NOW + 60_000)),
        ],
        declined: vec![(
            RequestId::new("job-1").unwrap(),
            key("gpu_1x_a100", Some("us-west-1")),
            at(NOW - 7000),
        )],
        brake: Some(Brake {
            failures: 3,
            tripped_at: Some(Millis::new(NOW - 9000)),
        }),
        cooldown: Some(Cooldown {
            until: Millis::new(NOW + 300_000),
            reason: CooldownReason::BootFailed,
        }),
        in_flight: vec![InFlight {
            pool: "burst".to_owned(),
            host: "js-burst-1".to_owned(),
            offer: "gpu_1x_a100".to_owned(),
            zone: Some("us-west-1".to_owned()),
            since: Millis::new(NOW - 42_000),
        }],
    }
}

/// Two pools: `burst`, whose provider holds [`held_state`], and `calm`, a healthy one whose
/// provider holds nothing.
fn two_pools() -> PoolsView {
    let burst = PoolView {
        provider_state: held_state(),
        ..pool_view(State::Available, false)
    };
    let calm = PoolView {
        name: "calm".to_owned(),
        provider_state: ProviderState {
            now_ms: NOW,
            ..ProviderState::default()
        },
        ..pool_view(State::Available, false)
    };
    PoolsView {
        pools: vec![burst, calm],
        warnings: Vec::new(),
        logs: None,
        store: None,
    }
}

async fn providers_of(view: fn() -> PoolsView) -> Vec<wire::Provider> {
    let api = api_over(Arc::new(FakeHosts::new()), Arc::new(FakePools(view)), None);
    let answer = list_providers(&api, Request::new(ListProvidersRequest {})).await;
    answer.unwrap().into_inner().providers
}

/// The fields of a provider this file is about, with every other field left at its default.
fn state_of(provider: &wire::Provider) -> wire::Provider {
    wire::Provider {
        catalog: provider.catalog,
        passed_over: provider.passed_over.clone(),
        declined: provider.declined.clone(),
        brake: provider.brake,
        cooldown_until_ms: provider.cooldown_until_ms,
        cooldown_reason: provider.cooldown_reason.clone(),
        launching: provider.launching.clone(),
        ..wire::Provider::default()
    }
}

/// Asserts, by exact equality, every field `ListProviders` adds for a provider whose pool
/// holds a catalog, two pass-overs (one with no region), a declined offer, an engaged brake,
/// a cool-down and a launch out: the catalog's fetch time and count, each pass-over with the
/// reason `quota` and its end, the decline with its request, the reason `capacity` and its
/// time, the brake engaged since its trip with its count, the cool-down's end and reason as
/// the records spell it, and the launch with its age at the status's time (42 s). The second,
/// healthy provider tells none of them. Catches a field dropped or mapped from the wrong
/// value, an age taken from the wrong clock, an empty region told as something, and one
/// provider's state told on another.
#[tokio::test]
async fn a_provider_tells_what_holds_it_and_a_healthy_one_tells_nothing() {
    let providers = providers_of(two_pools).await;
    let names: Vec<&str> = providers.iter().map(|p| p.name.as_str()).collect();
    assert_eq!(names, ["burst", "calm"]);
    let told = wire::Provider {
        catalog: Some(wire::ProviderCatalog {
            fetched_at_ms: T0 + 1000,
            offer_count: 3,
        }),
        passed_over: vec![
            wire::PassedOver {
                instance_type: "gpu_8x_h100".to_owned(),
                region: "us-east-1".to_owned(),
                reason: "quota".to_owned(),
                until_ms: NOW + 3_600_000,
            },
            wire::PassedOver {
                instance_type: "gpu_1x_a10".to_owned(),
                region: String::new(),
                reason: "quota".to_owned(),
                until_ms: NOW + 60_000,
            },
        ],
        declined: vec![wire::DeclinedOffer {
            request: "job-1".to_owned(),
            offer: "gpu_1x_a100".to_owned(),
            zone: "us-west-1".to_owned(),
            reason: "capacity".to_owned(),
            declined_at_ms: NOW - 7000,
        }],
        brake: Some(wire::ProviderBrake {
            engaged: true,
            engaged_at_ms: Some(NOW - 9000),
            failures: 3,
        }),
        cooldown_until_ms: Some(NOW + 300_000),
        cooldown_reason: "boot_failed".to_owned(),
        launching: vec![wire::PendingLaunch {
            host: "js-burst-1".to_owned(),
            offer: "gpu_1x_a100".to_owned(),
            zone: "us-west-1".to_owned(),
            started_at_ms: NOW - 42_000,
            age_ms: 42_000,
        }],
        ..wire::Provider::default()
    };
    assert_eq!(state_of(&providers[0]), told);
    assert_eq!(state_of(&providers[1]), wire::Provider::default());
}

/// A pool whose brake counts failures without being engaged, cooling down after a denied
/// launch, with a catalog fetched empty and a launch whose start is after the status's time.
fn counting_pool() -> PoolsView {
    let state = ProviderState {
        now_ms: NOW,
        catalog: Some((at(NOW), 0)),
        brake: Some(Brake {
            failures: 1,
            tripped_at: None,
        }),
        cooldown: Some(Cooldown {
            until: Millis::new(NOW + 1),
            reason: CooldownReason::Denied,
        }),
        in_flight: vec![InFlight {
            pool: "burst".to_owned(),
            host: "js-burst-2".to_owned(),
            offer: "gpu_1x_a10".to_owned(),
            zone: None,
            since: Millis::new(NOW + 5),
        }],
        ..ProviderState::default()
    };
    PoolsView {
        pools: vec![PoolView {
            provider_state: state,
            ..pool_view(State::Available, false)
        }],
        warnings: Vec::new(),
        logs: None,
        store: None,
    }
}

/// Asserts the boundaries: a brake that counts one failure and is not engaged is told with
/// its count, not engaged and with no time; a cool-down for a denied launch is told
/// `denied`; a catalog fetched empty is told with 0 offers, not left out; and a launch whose
/// start is after the status's time (a clock read in another order) has age 0, not a
/// wrapped one. Catches a brake told engaged for a count alone, an empty catalog read as
/// none, and an age that underflows.
#[tokio::test]
async fn a_counting_brake_an_empty_catalog_and_a_new_launch_are_told_as_they_are() {
    let providers = providers_of(counting_pool).await;
    let provider = &providers[0];
    assert_eq!(
        provider.brake,
        Some(wire::ProviderBrake {
            engaged: false,
            engaged_at_ms: None,
            failures: 1,
        })
    );
    assert_eq!(provider.cooldown_reason, "denied");
    assert_eq!(provider.cooldown_until_ms, Some(NOW + 1));
    assert_eq!(
        provider.catalog,
        Some(wire::ProviderCatalog {
            fetched_at_ms: NOW,
            offer_count: 0,
        })
    );
    assert_eq!(provider.launching.len(), 1);
    assert_eq!(provider.launching[0].age_ms, 0);
    assert_eq!(provider.launching[0].zone, "");
}
