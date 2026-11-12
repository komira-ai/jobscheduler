//! Pass 2 of the placement walk (`js_sched::acquire::pick`): which machine provider and
//! which of its offers a job is acquired from, over offers built by one helper.

use js_elastic::{Demand, Offer, PriceUnit, Purchase};
use js_sched::acquire::{
    AcquireDecision, AcquireInput, ProviderView, Skip, SkipReason, Waiting, pick,
};
use js_types::{AcceleratorNeed, ExecMode, Model, ProviderId, Resources, accelerator_of};

const GIB: u64 = 1 << 30;

/// A demand for four cores and 16 GiB, with no accelerator.
fn demand() -> Demand {
    Demand {
        resources: Resources::new(4000, 16 * GIB, 0, None).unwrap(),
        disk_bytes: None,
        os: None,
        arch: None,
        isa_level: None,
        exec: None,
        accelerator: None,
        network: Default::default(),
        host_ports: Vec::new(),
        layered_rootfs: false,
    }
}

/// A demand for `count` accelerators of the model `name`.
fn asking(name: &str, count: u32) -> Demand {
    Demand {
        accelerator: Some(AcceleratorNeed {
            model: Some(Model::parse(name).unwrap()),
            count,
            ..AcceleratorNeed::default()
        }),
        ..demand()
    }
}

/// An available Linux offer `id` in `zone-a` of eight cores, 32 GiB and 100 GiB of disk,
/// with no accelerator and no purchase option, at `price` cents an hour.
fn offer(id: &str, price: Option<u64>) -> Offer {
    Offer {
        id: id.to_owned(),
        zone: Some("zone-a".to_owned()),
        cpu_millis: 8000,
        memory_bytes: 32 * GIB,
        disk_bytes: 100 * GIB,
        accelerator: None,
        os: "linux".to_owned(),
        arch: "x86_64".to_owned(),
        isa_level: None,
        exec: ExecMode::Container,
        price_cents_per_hour: price,
        price_unit: PriceUnit::UsCents,
        available: true,
        purchase: None,
        quota: None,
        block: None,
    }
}

/// [`offer`] with `count` accelerators spelt `spelling`.
fn with_accelerators(offer: Offer, spelling: &str, count: u32) -> Offer {
    Offer {
        accelerator: Some(accelerator_of(
            &Model::parse(spelling).unwrap(),
            count,
            None,
        )),
        ..offer
    }
}

fn id(name: &str) -> ProviderId {
    ProviderId::new(name).unwrap()
}

/// The provider `name` with `offers`: enabled, not cooling down, not braked, holding no
/// machine, with no share of its own and no purchase order.
fn provider<'a>(name: &str, offers: &'a [Offer]) -> ProviderView<'a> {
    ProviderView {
        id: id(name),
        enabled: true,
        cooling_down: false,
        braked: false,
        max_machines: None,
        machines: 0,
        purchase: &[],
        offers,
    }
}

/// The input for `demand` over `providers`, with no pool limit and no machine held.
fn input<'a>(demand: &'a Demand, providers: &'a [ProviderView<'a>]) -> AcquireInput<'a> {
    AcquireInput {
        demand,
        allowed_instance_types: None,
        max_price_cents_per_hour: None,
        max_machines: None,
        machines: 0,
        providers,
    }
}

/// The provider and the offer `decision` acquires from, by provider id, offer id and zone.
fn acquired(decision: &AcquireDecision) -> Option<(&str, &str, Option<&str>)> {
    match decision {
        AcquireDecision::Acquire { provider, offer } => {
            Some((provider.as_str(), offer.id.as_str(), offer.zone.as_deref()))
        }
        AcquireDecision::Wait(_) => None,
    }
}

/// The skip of the provider `name` for `why`.
fn skip(name: &str, why: SkipReason) -> Skip {
    Skip {
        provider: id(name),
        why,
    }
}

/// The id of the offer one provider with `offers` and `purchase` order is acquired from for
/// `demand`, the same whether the catalog lists the offers in this order or reversed, and
/// with no provider skipped.
fn winner(demand: &Demand, offers: &[Offer], purchase: &[Purchase]) -> String {
    let mut reversed = offers.to_vec();
    reversed.reverse();
    let ids: Vec<String> = [offers.to_vec(), reversed]
        .iter()
        .map(|offers| {
            let providers = [ProviderView {
                purchase,
                ..provider("cloud", offers)
            }];
            let (decision, skips) = pick(&input(demand, &providers));
            assert_eq!(skips, Vec::<Skip>::new());
            let (_, offer, _) = acquired(&decision).expect("an acquire");
            offer.to_owned()
        })
        .collect();
    assert_eq!(ids[0], ids[1], "the catalog's order changed the pick");
    ids[0].clone()
}

/// Asserts each key of the policy in turn, every case over one provider and two offers
/// that differ in exactly the key under test, the winner losing on every later key (and
/// listed both first and last): (1) the cheaper offer wins although it has more
/// accelerators and more spare CPU and memory; (2) at an equal price the offer with fewer
/// accelerators wins although it has more spare; (3) at an equal price and count the
/// smaller spare CPU wins although it has more spare memory and its id sorts later;
/// (4) at an equal spare CPU the smaller spare memory wins although its purchase option is
/// later in the order; (5) with `purchase = [on-demand, spot]` the on-demand offer wins
/// although its id sorts later, and an offer with a listed option beats one with none and
/// one with an unlisted option; (6) all else equal the lower id wins although its zone
/// sorts later, and with the id equal the lower zone wins. Then two providers: the first
/// has only an expensive offer, the second a cheap one, and the first provider's offer is
/// acquired. Catches a key dropped, two keys swapped, or a key applied before the order of
/// the providers.
#[test]
fn the_policy_order_is_provider_then_price_then_fewest_accelerators_then_spare_then_purchase_then_id()
 {
    let job = demand();
    let order = [Purchase::OnDemand, Purchase::Spot];
    let spot = |offer: Offer| Offer {
        purchase: Some(Purchase::Spot),
        ..offer
    };
    let on_demand = |offer: Offer| Offer {
        purchase: Some(Purchase::OnDemand),
        ..offer
    };
    let big = |offer: Offer| Offer {
        cpu_millis: 64_000,
        memory_bytes: 256 * GIB,
        ..offer
    };

    // (1) Price.
    let cheap = with_accelerators(big(offer("z-cheap", Some(100))), "nvidia-a10", 4);
    let dear = with_accelerators(offer("a-dear", Some(200)), "nvidia-a10", 1);
    assert_eq!(winner(&job, &[dear, cheap], &order), "z-cheap");

    // (2) Fewest accelerators.
    let few = with_accelerators(big(offer("z-few", Some(100))), "nvidia-a10", 1);
    let many = with_accelerators(offer("a-many", Some(100)), "nvidia-a10", 2);
    assert_eq!(winner(&job, &[many, few], &order), "z-few");
    let none = big(offer("z-none", Some(100)));
    let one = with_accelerators(offer("a-one", Some(100)), "nvidia-a10", 1);
    assert_eq!(winner(&job, &[one, none], &order), "z-none");

    // (3) Least spare CPU.
    let tight_cpu = Offer {
        memory_bytes: 256 * GIB,
        ..spot(offer("z-tight-cpu", Some(100)))
    };
    let loose_cpu = Offer {
        cpu_millis: 16_000,
        ..on_demand(offer("a-loose-cpu", Some(100)))
    };
    assert_eq!(winner(&job, &[loose_cpu, tight_cpu], &order), "z-tight-cpu");

    // (4) Least spare memory.
    let tight_memory = spot(offer("z-tight-memory", Some(100)));
    let loose_memory = Offer {
        memory_bytes: 64 * GIB,
        ..on_demand(offer("a-loose-memory", Some(100)))
    };
    assert_eq!(
        winner(&job, &[loose_memory, tight_memory], &order),
        "z-tight-memory"
    );

    // (5) The provider's purchase order.
    let first = on_demand(offer("z-on-demand", Some(100)));
    let second = spot(offer("a-spot", Some(100)));
    assert_eq!(winner(&job, &[second, first], &order), "z-on-demand");
    let listed = spot(offer("z-listed", Some(100)));
    let unstated = offer("a-unstated", Some(100));
    assert_eq!(
        winner(&job, &[unstated, listed.clone()], &order),
        "z-listed"
    );
    let unlisted = Offer {
        purchase: Some(Purchase::Other("savings-plan".to_owned())),
        ..offer("a-unlisted", Some(100))
    };
    assert_eq!(winner(&job, &[unlisted, listed], &order), "z-listed");
    // With no order, the purchase option decides nothing and the id does.
    let spot_b = spot(offer("b-spot", Some(100)));
    let on_demand_c = on_demand(offer("c-on-demand", Some(100)));
    assert_eq!(winner(&job, &[on_demand_c, spot_b], &[]), "b-spot");

    // (6) The offer id, then the zone.
    let low_id = Offer {
        zone: Some("zone-b".to_owned()),
        ..offer("a-id", Some(100))
    };
    let high_id = offer("b-id", Some(100));
    assert_eq!(winner(&job, &[high_id, low_id], &order), "a-id");
    let zones = [
        Offer {
            zone: Some("zone-b".to_owned()),
            ..offer("same", Some(100))
        },
        offer("same", Some(100)),
    ];
    for listed in [zones.to_vec(), zones.iter().rev().cloned().collect()] {
        let providers = [provider("cloud", &listed)];
        let (decision, _) = pick(&input(&job, &providers));
        assert_eq!(acquired(&decision), Some(("cloud", "same", Some("zone-a"))));
    }

    // Provider order comes before every key.
    let expensive = [offer("expensive", Some(900))];
    let inexpensive = [offer("inexpensive", Some(10))];
    let providers = [
        provider("first", &expensive),
        provider("second", &inexpensive),
    ];
    let (decision, skips) = pick(&input(&job, &providers));
    assert_eq!(
        acquired(&decision),
        Some(("first", "expensive", Some("zone-a")))
    );
    assert_eq!(skips, Vec::<Skip>::new());
}

/// Asserts that an offer with no price and one with a price of zero, both fitting and
/// available, are never acquired although they would be the cheapest; that when they are
/// a provider's only fitting offers the decision is `Wait(Unpriced)` (also when a later
/// provider has no offer at all), while a later provider's priced offer is still acquired;
/// and that an unpriced offer that would not serve anyway (it does not fit, or is
/// unavailable), or that belongs to a provider passed over before its offers are looked at,
/// leaves the decision `Wait(NoOffer)`. Catches a missing price sorted as zero, and a
/// `Wait(Unpriced)` reported for an offer that could not have served.
#[test]
fn an_unpriced_offer_is_dropped() {
    let job = demand();
    let unpriced = [offer("no-price", None), offer("zero-price", Some(0))];
    let mut catalog = unpriced.to_vec();
    catalog.push(offer("priced", Some(500)));
    let providers = [provider("cloud", &catalog)];
    let (decision, _) = pick(&input(&job, &providers));
    assert_eq!(
        acquired(&decision),
        Some(("cloud", "priced", Some("zone-a")))
    );

    let providers = [provider("cloud", &unpriced), provider("empty", &[])];
    assert_eq!(
        pick(&input(&job, &providers)),
        (
            AcquireDecision::Wait(Waiting::Unpriced),
            vec![
                skip("cloud", SkipReason::NoUsableOffer),
                skip("empty", SkipReason::NoUsableOffer),
            ],
        )
    );

    let later = [offer("later", Some(700))];
    let providers = [provider("cloud", &unpriced), provider("later", &later)];
    let (decision, skips) = pick(&input(&job, &providers));
    assert_eq!(
        acquired(&decision),
        Some(("later", "later", Some("zone-a")))
    );
    assert_eq!(skips, [skip("cloud", SkipReason::NoUsableOffer)]);

    let too_small = [Offer {
        cpu_millis: 1000,
        ..offer("too-small", None)
    }];
    let gone = [Offer {
        available: false,
        ..offer("gone", None)
    }];
    for catalog in [&too_small[..], &gone[..]] {
        let providers = [provider("cloud", catalog)];
        assert_eq!(
            pick(&input(&job, &providers)),
            (
                AcquireDecision::Wait(Waiting::NoOffer),
                vec![skip("cloud", SkipReason::NoUsableOffer)],
            )
        );
    }
    let providers = [ProviderView {
        braked: true,
        ..provider("cloud", &unpriced)
    }];
    assert_eq!(
        pick(&input(&job, &providers)).0,
        AcquireDecision::Wait(Waiting::NoOffer)
    );
}

/// Asserts that provider A at its share is passed over with `OverShare` and provider B is
/// acquired from, and A one below its share is acquired from; that A's share is held
/// against A's own machines, not the pool's (A holds none of the pool's five and is
/// acquired from); that at the pool's `max_machines` the decision is `Wait(PoolFull)`
/// although both providers have room in their shares, and one below it an acquire; and
/// that a provider passed over before the pool total is looked at keeps its skip. Catches
/// a share checked against the pool's total, a pool total ignored, and either limit
/// compared with the wrong operator.
#[test]
fn max_machines_share_is_respected() {
    let job = demand();
    let a_offers = [offer("a-offer", Some(10))];
    let b_offers = [offer("b-offer", Some(90))];
    let a = |machines, share| ProviderView {
        max_machines: Some(share),
        machines,
        ..provider("a", &a_offers)
    };
    let b = ProviderView {
        max_machines: Some(10),
        machines: 3,
        ..provider("b", &b_offers)
    };

    let providers = [a(2, 2), b.clone()];
    let (decision, skips) = pick(&AcquireInput {
        machines: 5,
        max_machines: Some(20),
        ..input(&job, &providers)
    });
    assert_eq!(acquired(&decision), Some(("b", "b-offer", Some("zone-a"))));
    assert_eq!(skips, [skip("a", SkipReason::OverShare)]);

    let providers = [a(2, 3), b.clone()];
    let (decision, skips) = pick(&input(&job, &providers));
    assert_eq!(acquired(&decision), Some(("a", "a-offer", Some("zone-a"))));
    assert_eq!(skips, Vec::<Skip>::new());

    let providers = [a(0, 2), b.clone()];
    let (decision, _) = pick(&AcquireInput {
        machines: 5,
        ..input(&job, &providers)
    });
    assert_eq!(acquired(&decision), Some(("a", "a-offer", Some("zone-a"))));

    let providers = [a(1, 4), b.clone()];
    let full = AcquireInput {
        machines: 5,
        max_machines: Some(5),
        ..input(&job, &providers)
    };
    assert_eq!(
        pick(&full),
        (AcquireDecision::Wait(Waiting::PoolFull), vec![])
    );
    let (decision, _) = pick(&AcquireInput {
        machines: 4,
        ..full
    });
    assert_eq!(acquired(&decision), Some(("a", "a-offer", Some("zone-a"))));

    let disabled = ProviderView {
        enabled: false,
        ..provider("off", &a_offers)
    };
    let providers = [disabled, b];
    assert_eq!(
        pick(&AcquireInput {
            providers: &providers,
            ..full
        }),
        (
            AcquireDecision::Wait(Waiting::PoolFull),
            vec![skip("off", SkipReason::Disabled)],
        )
    );
}

/// Asserts that three providers ahead of a usable fourth, each with a cheaper usable offer,
/// are passed over as not enabled, cooling down and braked in turn, the fourth is acquired
/// from, and the three skips name their reasons in provider order; and that a provider with
/// several of the flags is skipped once, for the first in that order. Catches a flag that
/// is ignored, a reason reported for the wrong flag, and skips out of provider order.
#[test]
fn a_disabled_cooling_or_braked_provider_is_passed_over() {
    let job = demand();
    let cheap = [offer("cheap", Some(10))];
    let usable = [offer("usable", Some(90))];
    let providers = [
        ProviderView {
            enabled: false,
            ..provider("disabled", &cheap)
        },
        ProviderView {
            cooling_down: true,
            ..provider("cooling", &cheap)
        },
        ProviderView {
            braked: true,
            ..provider("braked", &cheap)
        },
        provider("fourth", &usable),
    ];
    let (decision, skips) = pick(&input(&job, &providers));
    assert_eq!(
        acquired(&decision),
        Some(("fourth", "usable", Some("zone-a")))
    );
    assert_eq!(
        skips,
        [
            skip("disabled", SkipReason::Disabled),
            skip("cooling", SkipReason::CoolingDown),
            skip("braked", SkipReason::Braked),
        ]
    );

    let every = ProviderView {
        enabled: false,
        cooling_down: true,
        braked: true,
        ..provider("every", &cheap)
    };
    let cooling_and_braked = ProviderView {
        enabled: true,
        ..every.clone()
    };
    for (view, why) in [
        (every, SkipReason::Disabled),
        (cooling_and_braked, SkipReason::CoolingDown),
    ] {
        let providers = [view];
        assert_eq!(
            pick(&input(&job, &providers)),
            (
                AcquireDecision::Wait(Waiting::NoOffer),
                vec![skip("every", why)]
            )
        );
    }
}

/// Asserts that an offer outside `allowed_instance_types`, one over
/// `max_price_cents_per_hour` and one that is unavailable are not acquired, though each
/// fits and is cheaper than the offer exactly at the price limit, which is kept and
/// acquired; that with that offer gone the provider has no usable offer; that with the
/// limits unset the cheapest available offer is acquired; and that an unpriced offer of a
/// type not allowed leaves the decision `Wait(NoOffer)`. Catches a limit compared with the
/// wrong operator, a limit ignored, and availability ignored.
#[test]
fn pool_limits_filter_offers() {
    let job = demand();
    let gone = Offer {
        available: false,
        ..offer("gone", Some(5))
    };
    let catalog = [
        offer("not-allowed", Some(10)),
        gone,
        offer("over", Some(301)),
        offer("at-limit", Some(300)),
    ];
    let allowed = ["gone".to_owned(), "over".to_owned(), "at-limit".to_owned()];
    let providers = [provider("cloud", &catalog)];
    let limited = AcquireInput {
        allowed_instance_types: Some(&allowed),
        max_price_cents_per_hour: Some(300),
        ..input(&job, &providers)
    };
    let (decision, _) = pick(&limited);
    assert_eq!(
        acquired(&decision),
        Some(("cloud", "at-limit", Some("zone-a")))
    );

    let without = [catalog[0].clone(), catalog[1].clone(), catalog[2].clone()];
    let providers = [provider("cloud", &without)];
    assert_eq!(
        pick(&AcquireInput {
            providers: &providers,
            ..limited
        }),
        (
            AcquireDecision::Wait(Waiting::NoOffer),
            vec![skip("cloud", SkipReason::NoUsableOffer)],
        )
    );

    let providers = [provider("cloud", &catalog)];
    let (decision, _) = pick(&input(&job, &providers));
    assert_eq!(
        acquired(&decision),
        Some(("cloud", "not-allowed", Some("zone-a")))
    );

    let unpriced = [offer("not-allowed", None)];
    let providers = [provider("cloud", &unpriced)];
    assert_eq!(
        pick(&AcquireInput {
            providers: &providers,
            ..limited
        })
        .0,
        AcquireDecision::Wait(Waiting::NoOffer)
    );
}

/// Asserts that a job asking four `nvidia-gb300` is acquired from the offer whose
/// accelerators are four of that model, though an `nvidia-a100` offer and an offer of two
/// `nvidia-gb300` are cheaper; and that a job asking four of the family `nvidia-a100` is
/// acquired from an offer spelt `nvidia-a100-sxm4-80gb` (a variant of that family) rather
/// than a cheaper `nvidia-h100` one. Catches a filter that compares the spelling, or skips
/// the accelerator.
#[test]
fn an_offer_is_chosen_by_the_canonical_fit_not_by_spelling() {
    let catalog = [
        with_accelerators(offer("a100x8", Some(100)), "nvidia-a100", 8),
        with_accelerators(offer("gb300x2", Some(150)), "nvidia-gb300", 2),
        with_accelerators(offer("gb300x4", Some(400)), "nvidia-gb300", 4),
    ];
    let job = asking("nvidia-gb300", 4);
    let providers = [provider("cloud", &catalog)];
    let (decision, _) = pick(&input(&job, &providers));
    assert_eq!(
        acquired(&decision),
        Some(("cloud", "gb300x4", Some("zone-a")))
    );

    let catalog = [
        with_accelerators(offer("h100x8", Some(100)), "nvidia-h100", 8),
        with_accelerators(offer("sxm4x8", Some(500)), "nvidia-a100-sxm4-80gb", 8),
    ];
    let job = asking("nvidia-a100", 4);
    let providers = [provider("cloud", &catalog)];
    let (decision, _) = pick(&input(&job, &providers));
    assert_eq!(
        acquired(&decision),
        Some(("cloud", "sxm4x8", Some("zone-a")))
    );
}

/// Every order of `items`.
fn permutations(items: &[Offer]) -> Vec<Vec<Offer>> {
    match items.len() {
        0 => vec![Vec::new()],
        _ => (0..items.len())
            .flat_map(|index| {
                let mut rest = items.to_vec();
                let first = rest.remove(index);
                permutations(&rest).into_iter().map(move |mut tail| {
                    tail.insert(0, first.clone());
                    tail
                })
            })
            .collect(),
    }
}

/// Asserts that all 720 orders of a six-offer catalog give the same acquire: one offer,
/// and five that each lose to it on one key alone (price, accelerators, spare CPU, purchase
/// order, zone) and tie with it on every other. Catches any dependence on the order of the
/// catalog, such as a key left out so that a tie goes to the offer listed first.
#[test]
fn the_same_inputs_give_the_same_decision() {
    let job = demand();
    let order = [Purchase::OnDemand, Purchase::Spot];
    let winner = Offer {
        purchase: Some(Purchase::OnDemand),
        ..offer("m", Some(100))
    };
    let catalog = [
        Offer {
            price_cents_per_hour: Some(101),
            ..winner.clone()
        },
        with_accelerators(winner.clone(), "nvidia-a10", 1),
        Offer {
            cpu_millis: 8001,
            ..winner.clone()
        },
        Offer {
            purchase: Some(Purchase::Spot),
            ..winner.clone()
        },
        Offer {
            zone: Some("zone-b".to_owned()),
            ..winner.clone()
        },
        winner.clone(),
    ];
    let all = permutations(&catalog);
    assert_eq!(all.len(), 720);
    for listed in all {
        let providers = [ProviderView {
            purchase: &order,
            ..provider("cloud", &listed)
        }];
        assert_eq!(
            pick(&input(&job, &providers)),
            (
                AcquireDecision::Acquire {
                    provider: id("cloud"),
                    offer: Box::new(winner.clone()),
                },
                vec![],
            )
        );
    }
}

/// Asserts that a pool with no machine provider acquires nothing and skips nothing.
#[test]
fn a_pool_with_no_provider_waits_for_an_offer() {
    let job = demand();
    assert_eq!(
        pick(&input(&job, &[])),
        (AcquireDecision::Wait(Waiting::NoOffer), vec![])
    );
}
