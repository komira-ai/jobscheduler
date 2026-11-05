use std::collections::BTreeSet;

use js_elastic::{Offer, PriceUnit, ProviderCaps};
use js_types::{
    AcceleratorNeed, Capabilities, ExecMode, HostId, IsaLevel, Model, Requirements, Resources,
    accelerator_of,
};

use super::*;

const GIB: u64 = 1 << 30;

/// Four cores, 16 GiB and one `nvidia-a10`, on Linux x86_64.
fn wanted() -> Requirements {
    let resources = Resources::new(4000, 16 * GIB, 1, Some("nvidia-a10".to_owned())).unwrap();
    Requirements {
        accelerator: AcceleratorNeed::booked(&resources),
        resources,
        disk_bytes: None,
        os: Some("linux".to_owned()),
        arch: Some("x86_64".to_owned()),
        isa_level: None,
        exec: None,
    }
}

fn facts() -> Capabilities {
    [
        ("os", "linux"),
        ("arch", "x86_64"),
        ("disk-gib", "100"),
        ("isa_level", "x86-64-v3"),
        ("exec", "container"),
    ]
    .into_iter()
    .map(|(key, value)| (key.to_owned(), value.to_owned()))
    .collect()
}

/// A serving host of eight cores, 32 GiB and one `nvidia-a10`.
fn host(name: &str) -> HostView {
    HostView {
        id: HostId::new(name).unwrap(),
        state: HostState::Serving,
        tainted: false,
        draining: false,
        total: Resources::new(8000, 32 * GIB, 1, Some("nvidia-a10".to_owned())).unwrap(),
        facts: facts(),
    }
}

fn in_state(state: HostState) -> HostView {
    HostView {
        state,
        ..host("h1")
    }
}

/// An offer of eight cores, 32 GiB, 200 GiB of disk and one `nvidia-a10`, available, at
/// 100 cents an hour.
fn offer(id: &str) -> Offer {
    Offer {
        id: id.to_owned(),
        zone: Some("zone-a".to_owned()),
        cpu_millis: 8000,
        memory_bytes: 32 * GIB,
        disk_bytes: 200 * GIB,
        accelerator: Some(accelerator_of(
            &Model::parse("nvidia-a10").unwrap(),
            1,
            None,
        )),
        os: "linux".to_owned(),
        arch: "x86_64".to_owned(),
        isa_level: None,
        exec: ExecMode::Container,
        price_cents_per_hour: Some(100),
        price_unit: PriceUnit::UsCents,
        available: true,
    }
}

fn caps() -> ProviderCaps {
    ProviderCaps {
        os: BTreeSet::from(["linux".to_owned()]),
        arch: BTreeSet::from(["x86_64".to_owned()]),
        accelerators: BTreeSet::from([Model::parse("nvidia-a10").unwrap()]),
        whole_host: true,
        host_ports: false,
    }
}

/// An enabled launching provider with a share of four, no limits, and `catalog`.
fn cloud(catalog: CatalogView) -> ProviderView {
    ProviderView {
        kind_is_static: false,
        enabled: true,
        share: 4,
        capabilities: caps(),
        catalog,
        limits: PoolLimits::default(),
    }
}

/// The `static` provider: it launches nothing. It is given a catalog that would admit,
/// so that a check that read it would be caught.
fn statics() -> ProviderView {
    ProviderView {
        kind_is_static: true,
        ..cloud(CatalogView::Offers(vec![offer("listed")]))
    }
}

const HOST: Admission = Admission::Accept(Path::CapableHost);
const OFFER: Admission = Admission::Accept(Path::MayOffer);

/// Asserts that a host that fits is capable whether it is serving, busy, lost or
/// cordoned: busy and lost hosts are unknown, not absent, and a cordon ends by intent.
/// Catches admission treating a busy or silent host as absent, which would refuse a request
/// that only has to wait.
#[test]
fn a_busy_capable_host_is_not_a_refusal() {
    for state in [
        HostState::Serving,
        HostState::Busy,
        HostState::Lost,
        HostState::Cordoned,
    ] {
        let hosts = [in_state(state)];
        assert_eq!(can_ever_serve(&wanted(), &hosts, &[]), HOST, "{state:?}");
    }
}

/// Asserts that a pool whose one fitting host is tainted, or draining, refuses, and that a
/// clean host beside it admits. Catches a check that ignores the taint or the drain, which
/// would queue a request for a host nobody will clear.
#[test]
fn a_tainted_only_pool_refuses() {
    let tainted = HostView {
        tainted: true,
        ..host("h1")
    };
    let draining = HostView {
        draining: true,
        ..host("h1")
    };
    for hosts in [[tainted.clone()], [draining.clone()]] {
        assert_eq!(can_ever_serve(&wanted(), &hosts, &[]), Admission::Refuse);
    }
    let beside = [tainted, draining, host("h2")];
    assert_eq!(can_ever_serve(&wanted(), &beside, &[]), HOST);
}

/// Asserts that a catalog that could not be read admits as may offer, and that a catalog
/// that was read and is empty refuses. Catches an unreadable catalog read as an empty one.
#[test]
fn an_unreadable_catalog_counts_as_may_offer() {
    let unread = [cloud(CatalogView::Unread)];
    assert_eq!(can_ever_serve(&wanted(), &[], &unread), OFFER);
    let empty = [cloud(CatalogView::Offers(Vec::new()))];
    assert_eq!(can_ever_serve(&wanted(), &[], &empty), Admission::Refuse);
}

/// Asserts that an offer that fits but is not available now admits. Catches admission
/// asking whether an offer is available, which is the acquire's question, not "ever".
#[test]
fn an_offer_unavailable_now_still_admits() {
    let gone = Offer {
        available: false,
        ..offer("a10")
    };
    let providers = [cloud(CatalogView::Offers(vec![gone]))];
    assert_eq!(can_ever_serve(&wanted(), &[], &providers), OFFER);
}

/// Asserts that a pool whose only provider is `static` refuses with no host, and with
/// only a host too small, and admits by its host when a capable one is busy. Catches a
/// static provider counted as may offer.
#[test]
fn a_static_only_pool_without_a_capable_host_refuses() {
    let only = [statics()];
    assert_eq!(can_ever_serve(&wanted(), &[], &only), Admission::Refuse);
    let small = HostView {
        total: Resources::new(2000, 32 * GIB, 1, Some("nvidia-a10".to_owned())).unwrap(),
        ..host("small")
    };
    assert_eq!(
        can_ever_serve(&wanted(), &[small], &only),
        Admission::Refuse
    );
    let busy = [in_state(HostState::Busy)];
    assert_eq!(can_ever_serve(&wanted(), &busy, &only), HOST);
    let unread = [ProviderView {
        catalog: CatalogView::Unread,
        ..statics()
    }];
    assert_eq!(can_ever_serve(&wanted(), &[], &unread), Admission::Refuse);
}

/// Asserts that a provider that is not enabled, and one with a share of zero, each with
/// a fitting offer, refuse, and that the same provider enabled with a share of one admits.
#[test]
fn a_disabled_or_zero_share_provider_does_not_count() {
    let fitting = || cloud(CatalogView::Offers(vec![offer("a10")]));
    let disabled = ProviderView {
        enabled: false,
        ..fitting()
    };
    let unshared = ProviderView {
        share: 0,
        ..fitting()
    };
    for provider in [disabled, unshared] {
        assert_eq!(
            can_ever_serve(&wanted(), &[], &[provider]),
            Admission::Refuse
        );
    }
    let one = ProviderView {
        share: 1,
        ..fitting()
    };
    assert_eq!(can_ever_serve(&wanted(), &[], &[one]), OFFER);
}

/// Asserts that a fitting offer whose instance type the pool does not allow refuses, as
/// does one above the maximum price; that one at the maximum price, and each with its limit
/// removed, admits.
#[test]
fn an_offer_outside_the_pool_limits_does_not_count() {
    let limited = |limits: PoolLimits| ProviderView {
        limits,
        ..cloud(CatalogView::Offers(vec![offer("a10")]))
    };
    let typed = PoolLimits {
        allowed_instance_types: Some(vec!["other".to_owned()]),
        max_price_cents_per_hour: None,
    };
    let cheap = PoolLimits {
        allowed_instance_types: None,
        max_price_cents_per_hour: Some(99),
    };
    for limits in [typed, cheap] {
        let refused = can_ever_serve(&wanted(), &[], &[limited(limits)]);
        assert_eq!(refused, Admission::Refuse);
    }
    let allowed = PoolLimits {
        allowed_instance_types: Some(vec!["other".to_owned(), "a10".to_owned()]),
        max_price_cents_per_hour: Some(100),
    };
    assert_eq!(can_ever_serve(&wanted(), &[], &[limited(allowed)]), OFFER);
    let none = limited(PoolLimits::default());
    assert_eq!(can_ever_serve(&wanted(), &[], &[none]), OFFER);
}

/// Asserts that a fitting offer with no price refuses under a price limit and admits
/// without one: the scaler never launches an unpriced offer under a limit.
#[test]
fn an_unpriced_offer_under_a_price_limit_does_not_count() {
    let unpriced = Offer {
        price_cents_per_hour: None,
        ..offer("a10")
    };
    let under = ProviderView {
        limits: PoolLimits {
            allowed_instance_types: None,
            max_price_cents_per_hour: Some(1_000_000),
        },
        ..cloud(CatalogView::Offers(vec![unpriced.clone()]))
    };
    assert_eq!(can_ever_serve(&wanted(), &[], &[under]), Admission::Refuse);
    let free = cloud(CatalogView::Offers(vec![unpriced]));
    assert_eq!(can_ever_serve(&wanted(), &[], &[free]), OFFER);
}

/// Asserts that a host is not capable when what the pool books of it is too small in any
/// dimension, or when its facts miss the request's `os`, `arch`, `disk-gib`, `isa_level` or
/// `exec`, and that it is when its facts meet each of them exactly.
#[test]
fn a_host_whose_shape_or_facts_miss_is_not_capable() {
    let exact = Requirements {
        disk_bytes: Some(99 * GIB + 1),
        isa_level: Some("x86-64-v3".parse::<IsaLevel>().unwrap()),
        exec: Some(ExecMode::Container),
        ..wanted()
    };
    assert_eq!(can_ever_serve(&exact, &[host("h1")], &[]), HOST);
    let misses = [
        Requirements {
            os: Some("macos".to_owned()),
            ..exact.clone()
        },
        Requirements {
            arch: Some("arm64".to_owned()),
            ..exact.clone()
        },
        Requirements {
            disk_bytes: Some(100 * GIB + 1),
            ..exact.clone()
        },
        Requirements {
            isa_level: Some("x86-64-v4".parse::<IsaLevel>().unwrap()),
            ..exact.clone()
        },
        Requirements {
            exec: Some(ExecMode::SandboxExec),
            ..exact.clone()
        },
        Requirements {
            resources: Resources::new(8001, GIB, 0, None).unwrap(),
            ..exact.clone()
        },
        Requirements {
            resources: Resources::new(1000, 32 * GIB + 1, 0, None).unwrap(),
            ..exact.clone()
        },
        Requirements {
            resources: Resources::new(1000, GIB, 2, Some("nvidia-a10".to_owned())).unwrap(),
            accelerator: AcceleratorNeed::booked(
                &Resources::new(1000, GIB, 2, Some("nvidia-a10".to_owned())).unwrap(),
            ),
            ..exact.clone()
        },
        Requirements {
            resources: Resources::new(1000, GIB, 1, Some("nvidia-h100".to_owned())).unwrap(),
            accelerator: AcceleratorNeed::booked(
                &Resources::new(1000, GIB, 1, Some("nvidia-h100".to_owned())).unwrap(),
            ),
            ..exact.clone()
        },
    ];
    for miss in misses {
        let refused = can_ever_serve(&miss, &[host("h1")], &[]);
        assert_eq!(refused, Admission::Refuse, "{miss:?}");
    }
}

/// Asserts that a provider never admits a request outside its capabilities, its catalog
/// read or not: another operating system, another architecture or another GPU class. A
/// request that names none of them is inside any provider's.
#[test]
fn a_request_outside_a_providers_capabilities_does_not_count() {
    let outside = [
        Requirements {
            os: Some("macos".to_owned()),
            ..wanted()
        },
        Requirements {
            arch: Some("arm64".to_owned()),
            ..wanted()
        },
        Requirements {
            resources: Resources::new(1000, GIB, 1, Some("nvidia-h100".to_owned())).unwrap(),
            accelerator: AcceleratorNeed::booked(
                &Resources::new(1000, GIB, 1, Some("nvidia-h100".to_owned())).unwrap(),
            ),
            ..wanted()
        },
    ];
    for req in outside {
        let unread = [cloud(CatalogView::Unread)];
        assert_eq!(
            can_ever_serve(&req, &[], &unread),
            Admission::Refuse,
            "{req:?}"
        );
    }
    let plain = Requirements {
        resources: Resources::new(1000, GIB, 0, None).unwrap(),
        os: None,
        arch: None,
        accelerator: None,
        ..wanted()
    };
    let bare = ProviderView {
        capabilities: ProviderCaps::default(),
        ..cloud(CatalogView::Unread)
    };
    assert_eq!(can_ever_serve(&plain, &[], &[bare]), OFFER);
}

/// Asserts that an offer too small for the request does not admit, and that one fitting
/// offer among several does.
#[test]
fn only_an_offer_that_fits_admits() {
    let small = Offer {
        cpu_millis: 2000,
        ..offer("small")
    };
    let alone = [cloud(CatalogView::Offers(vec![small.clone()]))];
    assert_eq!(can_ever_serve(&wanted(), &[], &alone), Admission::Refuse);
    let among = [cloud(CatalogView::Offers(vec![small, offer("a10")]))];
    assert_eq!(can_ever_serve(&wanted(), &[], &among), OFFER);
}

/// Asserts the order: a capable host admits as a capable host even when a provider may
/// offer too, and a later provider that may offer admits when an earlier one may not.
#[test]
fn a_capable_host_comes_before_a_provider() {
    let providers = [
        cloud(CatalogView::Offers(Vec::new())),
        cloud(CatalogView::Unread),
    ];
    assert_eq!(can_ever_serve(&wanted(), &[host("h1")], &providers), HOST);
    assert_eq!(can_ever_serve(&wanted(), &[], &providers), OFFER);
    assert_eq!(can_ever_serve(&wanted(), &[], &[]), Admission::Refuse);
}
