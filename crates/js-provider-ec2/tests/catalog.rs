//! The catalog against the in-memory account: what the cloud's answers become, and the
//! catalog semantics of the provider contract (a failure is an error, never an empty or
//! partial list). Every fixture is written by hand from the documented answer shapes; the
//! instance types, region and zones of the made-up fixtures are not EC2's.

use std::collections::BTreeSet;
use std::sync::Arc;

use js_caps::{AcceleratorNeed, Model};
use js_elastic::{BlockTerms, Demand, Dimension, Offer, Purchase, Quota, QuotaLimit};
use js_provider_ec2::{
    Action, ApiError, ArchName, BlockOffering, CatalogAnswer, CatalogNote, Ec2Catalog,
    Ec2CatalogSettings, ErrorCode, FakeEc2, GpuInfo, InstanceType, InstanceTypeInfo, PurchaseKind,
    QuotaFamily, Region, SettingsError, VcpuQuota, Zone,
};
use js_types::Resources;

const REGION: &str = "region-one";
const ZONES: [&str; 3] = ["region-one-a", "region-one-b", "region-one-c"];

/// Installs a subscriber that takes every event and writes none: a disabled event does
/// not evaluate its fields, and the fields are code that must run under test.
fn trace_everything() {
    static ONCE: std::sync::Once = std::sync::Once::new();
    ONCE.call_once(|| {
        let _ = tracing_subscriber::fmt()
            .with_max_level(tracing::Level::TRACE)
            .with_writer(std::io::sink)
            .try_init();
    });
}

fn nvidia(name: &str, count: u32, memory_mib: u64) -> GpuInfo {
    GpuInfo {
        name: name.to_owned(),
        manufacturer: "NVIDIA".to_owned(),
        count,
        memory_mib,
    }
}

/// An instance type of `vcpus` vCPUs and 4 GiB per vCPU on `arches`, with `gpus`.
fn kind(name: &str, vcpus: u32, arches: &[&str], gpus: Vec<GpuInfo>) -> InstanceTypeInfo {
    InstanceTypeInfo {
        name: InstanceType(name.to_owned()),
        vcpus,
        memory_mib: u64::from(vcpus) * 4096,
        architectures: arches.iter().map(|a| ArchName((*a).to_owned())).collect(),
        gpus,
        instance_storage_gb: None,
        bare_metal: false,
    }
}

/// A P-family type with eight 40 GiB A100s, the shape of the p4d row.
fn a100_type(name: &str) -> InstanceTypeInfo {
    InstanceTypeInfo {
        instance_storage_gb: Some(8000),
        ..kind(name, 96, &["x86_64"], vec![nvidia("A100", 8, 40960)])
    }
}

/// A Grace-class type as design section 5.6.1 describes it: `arm64`, four GPUs. The GPU
/// name is made up, so it passes through.
fn grace_type(name: &str) -> InstanceTypeInfo {
    kind(
        name,
        144,
        &["arm64"],
        vec![nvidia("Sample Grace", 4, 196608)],
    )
}

fn settings(purchase: &[&str]) -> Ec2CatalogSettings {
    Ec2CatalogSettings::new(REGION, purchase, None, Some(24)).unwrap()
}

fn allowing(purchase: &[&str], names: &[&str]) -> Ec2CatalogSettings {
    let names = names.iter().map(|name| (*name).to_owned()).collect();
    Ec2CatalogSettings::new(REGION, purchase, Some(names), Some(24)).unwrap()
}

fn catalog(fake: &Arc<FakeEc2>, settings: Ec2CatalogSettings) -> CatalogAnswer {
    trace_everything();
    Ec2Catalog::new(fake.clone(), settings).catalog().unwrap()
}

fn offers(fake: &Arc<FakeEc2>, purchase: &[&str]) -> Vec<Offer> {
    catalog(fake, settings(purchase)).offers
}

/// A fake with one A100 type offered in every zone, priced on demand and on spot in each.
fn priced() -> Arc<FakeEc2> {
    let fake = Arc::new(FakeEc2::new());
    fake.add_instance_type(a100_type("p9.24xlarge"));
    for zone in ZONES {
        fake.add_zone_offering("p9.24xlarge", zone)
            .add_spot_price("p9.24xlarge", zone, "9.8");
    }
    fake.add_on_demand_price("p9.24xlarge", "32.7726");
    fake
}

/// A block of `count` instances of `kind` in `zone` for a day at `fee` dollars.
fn block(id: &str, kind: &str, zone: &str, count: u32, fee: &str) -> BlockOffering {
    BlockOffering {
        id: id.to_owned(),
        instance_type: InstanceType(kind.to_owned()),
        zone: Zone(zone.to_owned()),
        instance_count: count,
        start_unix_seconds: 1_800_000_000,
        duration_minutes: 24 * 60,
        upfront_fee: fee.to_owned(),
        currency: "USD".to_owned(),
    }
}

/// Asserts that a failure of each read the catalog cannot do without (instance types,
/// zone offerings, the on-demand and spot prices, the Capacity Block offerings), made to
/// fail in turn with a throttle and then a denial, fails the whole catalog with an error
/// naming the action and the cloud's code; that the failure is not the last good catalog
/// (a good call before it does not stand in); that the other kinds of failure (transport,
/// unreadable, incomplete) are errors too; and that an account empty on purpose answers an
/// empty catalog. Catches a failure collapsed to an empty list or to a partial answer.
#[test]
fn catalog_failure_is_an_error_not_an_empty_list() {
    let fake = priced();
    fake.add_block_offering(block("cb-1", "p9.24xlarge", ZONES[0], 1, "100"));
    let all = settings(&["on-demand", "spot", "capacity-block"]);
    let provider = Ec2Catalog::new(fake.clone(), all);
    trace_everything();
    assert_eq!(provider.catalog().unwrap().offers.len(), 7);
    let actions = [
        Action::DescribeInstanceTypes,
        Action::DescribeInstanceTypeOfferings,
        Action::GetProducts,
        Action::DescribeSpotPriceHistory,
        Action::DescribeCapacityBlockOfferings,
    ];
    for action in actions {
        for (code, kind) in [
            ("RequestLimitExceeded", "throttled"),
            ("UnauthorizedOperation", "denied"),
        ] {
            fake.fail_next(action, ApiError::answered(code));
            let error = provider.catalog().unwrap_err();
            assert_eq!(
                error.to_string(),
                format!("{action} failed: {kind} ({code})")
            );
        }
    }
    for (error, text) in [
        (
            ApiError::Transport("timed out".to_owned()),
            "cannot talk to the cloud: timed out",
        ),
        (
            ApiError::Unreadable("no list".to_owned()),
            "the answer cannot be read: no list",
        ),
        (ApiError::Incomplete, "the answer is not complete"),
        (
            ApiError::answered("InternalError"),
            "refused (InternalError)",
        ),
    ] {
        fake.fail_next(Action::DescribeInstanceTypeOfferings, error);
        assert_eq!(
            provider.catalog().unwrap_err().to_string(),
            format!("DescribeInstanceTypeOfferings failed: {text}")
        );
    }
    // A block priced in another currency is an answer that cannot be read.
    let foreign = priced();
    foreign.add_block_offering(BlockOffering {
        currency: "EUR".to_owned(),
        ..block("cb-2", "p9.24xlarge", ZONES[0], 1, "100")
    });
    let error = Ec2Catalog::new(foreign, settings(&["capacity-block"]))
        .catalog()
        .unwrap_err();
    assert_eq!(
        error.to_string(),
        "DescribeCapacityBlockOfferings failed: the answer cannot be read: capacity block \
         cb-2 is priced in \"EUR\", not USD"
    );
    let empty = Arc::new(FakeEc2::new());
    let answer = catalog(&empty, settings(&["on-demand", "spot", "capacity-block"]));
    assert_eq!(
        answer,
        CatalogAnswer {
            offers: Vec::new(),
            notes: Vec::new()
        }
    );
}

/// Asserts that every offer states an architecture, `x86_64` or `arm64` and nothing else:
/// an `arm64` type's offers are `arm64` (never `aarch64`), the Grace-class type's are
/// `arm64`, a type that lists `i386` beside `x86_64` is `x86_64`, a type that lists both
/// `arm64` and `x86_64` states the first listed, and an `i386`-only and a Mac type have no
/// offer. Catches an architecture that is defaulted, copied as the cloud spells it, or
/// missing.
#[test]
fn arch_is_stated_on_every_offer() {
    let fake = Arc::new(FakeEc2::new());
    let gpu = || vec![nvidia("T4", 1, 16384)];
    for info in [
        kind("g9.x86", 4, &["x86_64"], gpu()),
        kind("g9g.arm", 4, &["arm64"], gpu()),
        kind("g9.old", 4, &["i386", "x86_64"], gpu()),
        kind("g9.both", 4, &["arm64", "x86_64"], gpu()),
        kind("g9.alias", 4, &["aarch64"], gpu()),
        kind("g9.i386", 4, &["i386"], gpu()),
        kind("mac9.metal", 4, &["x86_64_mac"], gpu()),
        kind("mac9m.metal", 4, &["arm64_mac"], gpu()),
        grace_type("p9e-sample.36xlarge"),
    ] {
        fake.add_zone_offering(info.name.as_str(), ZONES[0]);
        fake.add_instance_type(info);
    }
    let offers = offers(&fake, &["on-demand"]);
    let arch_of = |id: &str| -> Vec<&str> {
        offers
            .iter()
            .filter(|offer| offer.id == format!("{id}/on-demand"))
            .map(|offer| offer.arch.as_str())
            .collect()
    };
    assert!(
        offers
            .iter()
            .all(|o| ["x86_64", "arm64"].contains(&o.arch.as_str()))
    );
    assert_eq!(arch_of("g9.x86"), ["x86_64"]);
    assert_eq!(arch_of("g9g.arm"), ["arm64"]);
    assert_eq!(arch_of("g9.old"), ["x86_64"]);
    assert_eq!(arch_of("g9.both"), ["arm64"]);
    assert_eq!(arch_of("g9.alias"), ["arm64"]);
    assert_eq!(arch_of("p9e-sample.36xlarge"), ["arm64"]);
    assert!(arch_of("g9.i386").is_empty());
    assert!(arch_of("mac9.metal").is_empty());
    assert!(arch_of("mac9m.metal").is_empty());
    assert_eq!(offers.len(), 6);
}

/// Asserts that a type whose GPU has no letter or digit in its name or maker has no offer,
/// while the catalog still holds the others. Catches a panic or an offer with no model.
#[test]
fn a_gpu_with_no_name_has_no_offer() {
    let fake = priced();
    let mut nameless = kind("g9.nameless", 4, &["x86_64"], vec![nvidia("", 1, 1024)]);
    nameless.gpus[0].manufacturer = "--".to_owned();
    fake.add_instance_type(nameless)
        .add_zone_offering("g9.nameless", ZONES[0]);
    let offers = offers(&fake, &["on-demand"]);
    assert_eq!(offers.len(), 3);
    assert!(
        offers
            .iter()
            .all(|offer| offer.id == "p9.24xlarge/on-demand")
    );
}

/// Asserts what one offer states: the id of type and purchase option, the zone as its own
/// field, whole vCPUs as thousandths, memory and the instance store in bytes, the A100's
/// canonical accelerator, Linux, containers, no ISA level, available; and that a type with
/// no instance store states no disk.
#[test]
fn an_offer_states_the_type_in_the_vocabulary() {
    let fake = priced();
    fake.add_instance_type(kind("c9.large", 2, &["x86_64"], Vec::new()))
        .add_zone_offering("c9.large", ZONES[0]);
    let offers = catalog(
        &fake,
        allowing(&["on-demand"], &["p9.24xlarge", "c9.large"]),
    )
    .offers;
    let a100 = offers
        .iter()
        .find(|offer| offer.id == "p9.24xlarge/on-demand")
        .unwrap();
    assert_eq!(a100.zone.as_deref(), Some(ZONES[0]));
    assert_eq!(a100.cpu_millis, 96_000);
    assert_eq!(a100.memory_bytes, (96 * 4096) << 20);
    assert_eq!(a100.disk_bytes, 8_000_000_000_000);
    let accelerator = a100.accelerator.as_ref().unwrap();
    assert_eq!(accelerator.model.as_str(), "nvidia-a100");
    assert_eq!(accelerator.count, 8);
    assert_eq!(accelerator.memory_per_device, Some(40 << 30));
    assert_eq!(a100.os, "linux");
    assert_eq!(a100.exec, js_types::ExecMode::Container);
    assert_eq!(a100.isa_level, None);
    assert!(a100.available);
    assert_eq!(a100.block, None);
    let cpu = offers
        .iter()
        .find(|offer| offer.id == "c9.large/on-demand")
        .unwrap();
    assert_eq!(cpu.disk_bytes, 0);
    assert_eq!(cpu.accelerator, None);
}

/// A demand for `count` accelerators of `model`, as a request that books them states it.
fn wanting(model: &str, count: u32) -> Demand {
    Demand {
        resources: Resources::new(1000, 1 << 30, u64::from(count), Some(model.to_owned())).unwrap(),
        disk_bytes: None,
        os: Some("linux".to_owned()),
        arch: None,
        isa_level: None,
        exec: None,
        accelerator: Some(AcceleratorNeed {
            model: Some(Model::parse(model).unwrap()),
            count,
            ..AcceleratorNeed::default()
        }),
        network: Default::default(),
        host_ports: Vec::new(),
    }
}

/// Asserts that a type whose GPU name is in no row of the table has offers whose
/// accelerator model is its own spelling in the lower-case `-` form, with the memory the
/// cloud states, that a request for exactly that spelling fits the offer, and that a
/// request for a canonical model does not. Catches an unknown name dropped, mapped to the
/// nearest known model, or a panic.
#[test]
fn an_unknown_gpu_name_passes_through_unchanged() {
    let fake = Arc::new(FakeEc2::new());
    fake.add_instance_type(kind(
        "g9.sample",
        16,
        &["x86_64"],
        vec![nvidia("Sample-9 X", 2, 24576)],
    ))
    .add_zone_offering("g9.sample", ZONES[0]);
    let offers = offers(&fake, &["on-demand"]);
    let [offer] = offers.as_slice() else {
        panic!("{offers:?}");
    };
    let accelerator = offer.accelerator.as_ref().unwrap();
    assert_eq!(accelerator.model.as_str(), "nvidia-sample-9-x");
    assert_eq!(accelerator.vendor.as_str(), "nvidia");
    assert_eq!(accelerator.count, 2);
    assert_eq!(accelerator.memory_per_device, Some(24576 << 20));
    assert_eq!(offer.fit(&wanting("nvidia-sample-9-x", 2)), Ok(()));
    for canonical in ["nvidia-a100", "nvidia-h100", "nvidia-a10"] {
        assert_eq!(
            offer.fit(&wanting(canonical, 2)),
            Err(Dimension::AcceleratorModel)
        );
    }
}

/// Asserts that a type offered in three zones with `purchase = ["on-demand", "spot"]` has
/// six offers with distinct (id, zone) pairs, an on-demand and a spot one per zone, and
/// with `purchase = ["on-demand"]` three and no spot offer. Catches options emitted
/// unasked, collapsed, or zones merged.
#[test]
fn one_offer_per_type_zone_and_purchase_option() {
    let fake = priced();
    let both = offers(&fake, &["on-demand", "spot"]);
    let pairs: BTreeSet<(String, Option<String>)> = both
        .iter()
        .map(|o| (o.id.clone(), o.zone.clone()))
        .collect();
    assert_eq!(both.len(), 6);
    assert_eq!(pairs.len(), 6);
    for zone in ZONES {
        for (id, purchase) in [
            ("p9.24xlarge/on-demand", Purchase::OnDemand),
            ("p9.24xlarge/spot", Purchase::Spot),
        ] {
            let found: Vec<&Offer> = both
                .iter()
                .filter(|o| o.id == id && o.zone.as_deref() == Some(zone))
                .collect();
            assert_eq!(found.len(), 1, "{id} {zone}");
            assert_eq!(found[0].purchase, Some(purchase));
        }
    }
    let one = offers(&fake, &["on-demand"]);
    assert_eq!(one.len(), 3);
    assert!(one.iter().all(|o| o.purchase == Some(Purchase::OnDemand)));
    let calls: Vec<Action> = fake.calls().into_iter().map(|call| call.action).collect();
    assert_eq!(
        calls
            .iter()
            .filter(|a| **a == Action::DescribeSpotPriceHistory)
            .count(),
        1,
        "the spot prices are read only for the catalog that lists spot"
    );
}

/// Asserts that a type returned by `DescribeInstanceTypes` that no zone offers has no
/// on-demand or spot offer. Catches an unoffered type listed as
/// available.
#[test]
fn a_type_with_no_zone_offering_has_no_offer() {
    let fake = priced();
    fake.add_instance_type(a100_type("p9.unoffered"))
        .add_on_demand_price("p9.unoffered", "1");
    let offers = offers(&fake, &["on-demand", "spot"]);
    assert_eq!(offers.len(), 6);
    assert!(offers.iter().all(|o| !o.id.starts_with("p9.unoffered")));
}

/// Asserts that prices `1.5000`, `32.7726` and `0.0010` dollars an hour are 150, 3278 and 1
/// cents (a fraction of a cent rounded up), that a spot price comes from its own zone and
/// not from the on-demand price, and that an offer with no price is still listed, with
/// `price_cents_per_hour` `None`. Catches float rounding drift, the on-demand price used
/// for spot, and unpriced offers dropped.
#[test]
fn offers_carry_price_as_integer_cents() {
    let fake = Arc::new(FakeEc2::new());
    for (name, usd) in [
        ("g9.a", Some("1.5000")),
        ("g9.b", Some("32.7726")),
        ("g9.c", Some("0.0010")),
        ("g9.d", None),
    ] {
        fake.add_instance_type(kind(name, 4, &["x86_64"], vec![nvidia("T4", 1, 16384)]))
            .add_zone_offering(name, ZONES[0])
            .add_zone_offering(name, ZONES[1]);
        if let Some(usd) = usd {
            fake.add_on_demand_price(name, usd);
        }
    }
    fake.add_spot_price("g9.a", ZONES[0], "0.4512")
        .add_spot_price("g9.a", ZONES[1], "0.4400")
        .add_spot_price("g9.b", ZONES[2], "7");
    let offers = offers(&fake, &["on-demand", "spot"]);
    let price = |id: &str, zone: &str| {
        offers
            .iter()
            .find(|o| o.id == id && o.zone.as_deref() == Some(zone))
            .map(|o| o.price_cents_per_hour)
            .unwrap()
    };
    assert_eq!(price("g9.a/on-demand", ZONES[0]), Some(150));
    assert_eq!(price("g9.b/on-demand", ZONES[0]), Some(3278));
    assert_eq!(price("g9.c/on-demand", ZONES[1]), Some(1));
    assert_eq!(price("g9.d/on-demand", ZONES[0]), None);
    assert_eq!(price("g9.a/spot", ZONES[0]), Some(46));
    assert_eq!(price("g9.a/spot", ZONES[1]), Some(44));
    // `g9.b` has a spot price only in a zone that does not offer it.
    assert_eq!(price("g9.b/spot", ZONES[0]), None);
    assert_eq!(price("g9.d/spot", ZONES[1]), None);
    assert_eq!(offers.len(), 16);
    assert!(offers.iter().all(|o| o.available));
}

/// Asserts that a price that is not a decimal fails the catalog as unreadable, naming the
/// call it came from, on demand and on spot. Catches an unreadable price become a free
/// offer.
#[test]
fn an_unparseable_price_is_an_error_not_zero() {
    let on_demand = priced();
    on_demand.add_on_demand_price("p9.24xlarge", "free");
    let error = Ec2Catalog::new(on_demand, settings(&["on-demand"]))
        .catalog()
        .unwrap_err();
    assert_eq!(
        error.to_string(),
        "GetProducts failed: the answer cannot be read: \"free\" is not a price in US dollars"
    );
    let spot = priced();
    spot.add_spot_price("p9.24xlarge", ZONES[1], "1,25");
    let error = Ec2Catalog::new(spot, settings(&["spot"]))
        .catalog()
        .unwrap_err();
    assert_eq!(
        error.to_string(),
        "DescribeSpotPriceHistory failed: the answer cannot be read: \"1,25\" is not a price \
         in US dollars"
    );
}

/// Asserts that a vCPU limit smaller than the type's vCPUs leaves the offer listed and
/// available with that limit in its quota, that the on-demand and spot offers of one type
/// carry their own quotas, that two types of one quota family read it once, that a family
/// with no quota entry leaves the offers without one and no note, and that a type of no
/// quota family reads none. Catches a quota that removes or marks an offer, or one number
/// shared by the two purchase options.
#[test]
fn quota_is_read_into_the_offer_and_never_filters_it() {
    let fake = priced();
    fake.add_instance_type(a100_type("p9.48xlarge"))
        .add_zone_offering("p9.48xlarge", ZONES[0])
        .add_instance_type(kind(
            "g9.xlarge",
            4,
            &["x86_64"],
            vec![nvidia("T4", 1, 16384)],
        ))
        .add_zone_offering("g9.xlarge", ZONES[0])
        .add_instance_type(kind("q9.large", 2, &["x86_64"], Vec::new()))
        .add_zone_offering("q9.large", ZONES[0])
        .set_quota(
            QuotaFamily::P,
            PurchaseKind::OnDemand,
            VcpuQuota {
                limit: 48,
                in_use: Some(8),
            },
        )
        .set_quota(
            QuotaFamily::P,
            PurchaseKind::Spot,
            VcpuQuota {
                limit: 32,
                in_use: None,
            },
        );
    let names = ["p9.24xlarge", "p9.48xlarge", "g9.xlarge", "q9.large"];
    let answer = catalog(&fake, allowing(&["on-demand", "spot"], &names));
    assert!(answer.notes.is_empty());
    let quota_of = |id: &str| -> Vec<Option<Quota>> {
        answer
            .offers
            .iter()
            .filter(|o| o.id == id)
            .map(|o| o.quota)
            .collect()
    };
    let on_demand = Quota {
        vcpus: Some(QuotaLimit {
            limit: 48,
            in_use: Some(8),
        }),
        accelerators: None,
    };
    let spot = Quota {
        vcpus: Some(QuotaLimit {
            limit: 32,
            in_use: None,
        }),
        accelerators: None,
    };
    assert_eq!(quota_of("p9.24xlarge/on-demand"), [Some(on_demand); 3]);
    assert_eq!(quota_of("p9.24xlarge/spot"), [Some(spot); 3]);
    assert_eq!(quota_of("p9.48xlarge/on-demand"), [Some(on_demand)]);
    assert_eq!(quota_of("g9.xlarge/spot"), [None]);
    assert_eq!(quota_of("q9.large/on-demand"), [None]);
    assert_eq!(answer.offers.len(), 12);
    assert!(answer.offers.iter().all(|o| o.available));
    let quota_reads = fake
        .calls()
        .into_iter()
        .filter(|call| call.action == Action::GetServiceQuota)
        .count();
    assert_eq!(quota_reads, 4, "P and G, on demand and spot, each once");
}

/// Asserts that when only the quota reads fail the catalog is still whole: every offer is
/// listed with no quota, and each failed read is a note naming its family, purchase option
/// and error. Catches a quota failure that empties or fails the catalog.
#[test]
fn a_failed_quota_read_keeps_the_offers_without_a_quota() {
    let fake = priced();
    fake.set_quota(
        QuotaFamily::P,
        PurchaseKind::OnDemand,
        VcpuQuota {
            limit: 1,
            in_use: None,
        },
    );
    let throttled = ApiError::answered("TooManyRequestsException");
    fake.fail_always(Action::GetServiceQuota, throttled.clone());
    let answer = catalog(&fake, settings(&["on-demand", "spot"]));
    assert_eq!(answer.offers.len(), 6);
    assert!(answer.offers.iter().all(|o| o.quota.is_none()));
    assert_eq!(
        answer.notes,
        [
            CatalogNote {
                family: QuotaFamily::P,
                purchase: PurchaseKind::OnDemand,
                error: throttled.clone()
            },
            CatalogNote {
                family: QuotaFamily::P,
                purchase: PurchaseKind::Spot,
                error: throttled
            },
        ]
    );
}

/// Asserts that with an allow list of two types exactly those appear, a CPU type among
/// them, and a listed type the region lacks adds nothing; and that with no allow list only
/// the types with a GPU appear. Catches an allow list ignored, or a default that lists
/// every type.
#[test]
fn the_allow_list_selects_and_the_default_is_gpu_types_only() {
    let fake = Arc::new(FakeEc2::new());
    for info in [
        kind("g9.one", 4, &["x86_64"], vec![nvidia("T4", 1, 16384)]),
        kind("g9.two", 8, &["x86_64"], vec![nvidia("T4", 2, 16384)]),
        kind("c9.one", 2, &["x86_64"], Vec::new()),
        kind("c9.two", 4, &["arm64"], Vec::new()),
    ] {
        fake.add_zone_offering(info.name.as_str(), ZONES[0]);
        fake.add_instance_type(info);
    }
    let types = |answer: CatalogAnswer| -> Vec<String> {
        answer.offers.into_iter().map(|o| o.id).collect()
    };
    let listed = catalog(
        &fake,
        allowing(&["on-demand"], &["g9.one", "c9.one", "x9.absent"]),
    );
    assert_eq!(types(listed), ["c9.one/on-demand", "g9.one/on-demand"]);
    assert_eq!(
        types(catalog(&fake, settings(&["on-demand"]))),
        ["g9.one/on-demand", "g9.two/on-demand"]
    );
}

/// Asserts the purchase finding (the newest GPU types are sold as reserved blocks): a type
/// sold only as a Capacity Block (no zone offering, no on-demand price) has capacity-block
/// offers, one per offering, with the offering's terms and its upfront fee spread over its
/// instance hours, and no quota, when `purchase` lists `capacity-block`, and none (and no
/// offering read) when it does not; and that the settings refuse an option the provider
/// does not sell and `capacity-block` without a length. Catches an unlisted option offered
/// or an unsupported value accepted.
#[test]
fn the_purchase_finding_is_enforced() {
    let fake = Arc::new(FakeEc2::new());
    fake.add_instance_type(grace_type("p9e-sample.36xlarge"))
        .add_block_offering(block("cb-b", "p9e-sample.36xlarge", ZONES[1], 18, "86400"))
        .add_block_offering(block("cb-a", "p9e-sample.36xlarge", ZONES[1], 2, "100.005"))
        .add_block_offering(block("cb-x", "p9.unknown", ZONES[0], 1, "1"))
        .set_quota(
            QuotaFamily::P,
            PurchaseKind::OnDemand,
            VcpuQuota {
                limit: 1,
                in_use: None,
            },
        );
    let answer = catalog(&fake, settings(&["on-demand", "spot", "capacity-block"]));
    assert!(answer.notes.is_empty());
    let terms = |offering: &str, instances, upfront_cents| BlockTerms {
        offering: offering.to_owned(),
        start_unix_seconds: 1_800_000_000,
        duration_minutes: 24 * 60,
        upfront_cents,
        instances,
    };
    let seen: Vec<(String, Option<u64>, Option<BlockTerms>)> = answer
        .offers
        .iter()
        .map(|o| (o.id.clone(), o.price_cents_per_hour, o.block.clone()))
        .collect();
    assert_eq!(
        seen,
        [
            (
                "p9e-sample.36xlarge/capacity-block/cb-a".to_owned(),
                Some(209),
                Some(terms("cb-a", 2, 10001))
            ),
            (
                "p9e-sample.36xlarge/capacity-block/cb-b".to_owned(),
                Some(20000),
                Some(terms("cb-b", 18, 8_640_000))
            ),
        ]
    );
    assert!(answer.offers.iter().all(|o| {
        o.purchase == Some(Purchase::CapacityBlock) && o.quota.is_none() && o.arch == "arm64"
    }));
    assert!(
        fake.calls()
            .iter()
            .all(|call| call.action != Action::GetServiceQuota)
    );
    let before = fake.calls().len();
    assert!(offers(&fake, &["on-demand", "spot"]).is_empty());
    assert!(
        fake.calls()[before..]
            .iter()
            .all(|call| call.action != Action::DescribeCapacityBlockOfferings)
    );
    let refused = |purchase: &[&str], hours| Ec2CatalogSettings::new(REGION, purchase, None, hours);
    assert_eq!(
        refused(&["reserved"], None),
        Err(SettingsError::Purchase("reserved".to_owned()))
    );
    assert_eq!(
        refused(&["capacity-block"], None),
        Err(SettingsError::BlockHours)
    );
    assert_eq!(
        refused(&["capacity-block"], Some(0)),
        Err(SettingsError::BlockHours)
    );
}

/// Asserts that the block length the settings name (`capacity_block_hours`) is the length
/// the catalog asks `DescribeCapacityBlockOfferings` for: 48 hours, then 24 for a catalog
/// set to 24. Catches a length that is fixed, defaulted, or not passed to the port.
#[test]
fn the_configured_block_length_reaches_the_port() {
    let fake = priced();
    for hours in [48, 24] {
        let blocks = Ec2CatalogSettings::new(REGION, &["capacity-block"], None, Some(hours));
        catalog(&fake, blocks.unwrap());
    }
    assert_eq!(fake.block_hours_asked(), [48, 24]);
}

/// The fake of the ordering test, its answers added in the order `forward` says.
fn ordered(forward: bool) -> Arc<FakeEc2> {
    let fake = Arc::new(FakeEc2::new());
    let mut types = [a100_type("p9.24xlarge"), grace_type("p9e-sample.36xlarge")];
    let mut zones: Vec<(&str, &str)> = Vec::new();
    for name in ["p9.24xlarge", "p9e-sample.36xlarge"] {
        zones.extend(ZONES.iter().map(|zone| (name, *zone)));
    }
    let mut blocks = [
        block("cb-2", "p9e-sample.36xlarge", ZONES[1], 1, "10"),
        block("cb-1", "p9e-sample.36xlarge", ZONES[1], 1, "10"),
        block("cb-3", "p9e-sample.36xlarge", ZONES[0], 1, "10"),
    ];
    if !forward {
        types.reverse();
        zones.reverse();
        blocks.reverse();
    }
    for info in types {
        fake.add_instance_type(info);
    }
    for (name, zone) in &zones {
        fake.add_zone_offering(name, zone)
            .add_spot_price(name, zone, "1.25");
    }
    for offering in blocks {
        fake.add_block_offering(offering);
    }
    fake.add_on_demand_price("p9.24xlarge", "3")
        .add_on_demand_price("p9e-sample.36xlarge", "9");
    fake
}

/// Asserts that the same answers in another order give a byte-identical offer list, and
/// that the order is instance type, zone, the configured purchase order, then offering;
/// and that a type listed twice gives its offers once. Catches an order that depends on the
/// API's page order.
#[test]
fn offer_order_is_stable() {
    let purchase = ["spot", "capacity-block", "on-demand"];
    let one = offers(&ordered(true), &purchase);
    let other = offers(&ordered(false), &purchase);
    assert_eq!(format!("{one:?}"), format!("{other:?}"));
    let keys: Vec<(String, String)> = one
        .iter()
        .take(4)
        .map(|o| (o.id.clone(), o.zone.clone().unwrap()))
        .collect();
    assert_eq!(
        keys,
        [
            ("p9.24xlarge/spot".to_owned(), ZONES[0].to_owned()),
            ("p9.24xlarge/on-demand".to_owned(), ZONES[0].to_owned()),
            ("p9.24xlarge/spot".to_owned(), ZONES[1].to_owned()),
            ("p9.24xlarge/on-demand".to_owned(), ZONES[1].to_owned()),
        ]
    );
    let zone_b: Vec<&str> = one
        .iter()
        .filter(|o| o.id.starts_with("p9e") && o.zone.as_deref() == Some(ZONES[1]))
        .map(|o| o.id.as_str())
        .collect();
    assert_eq!(
        zone_b,
        [
            "p9e-sample.36xlarge/spot",
            "p9e-sample.36xlarge/capacity-block/cb-1",
            "p9e-sample.36xlarge/capacity-block/cb-2",
            "p9e-sample.36xlarge/on-demand",
        ]
    );
    let twice = ordered(true);
    twice.add_instance_type(a100_type("p9.24xlarge"));
    assert_eq!(offers(&twice, &purchase), one);
}

/// Whether `action` only reads. The match names every action, so a new one does not
/// compile until it is placed here.
fn reads_only(action: Action) -> bool {
    match action {
        Action::DescribeInstanceTypes
        | Action::DescribeInstanceTypeOfferings
        | Action::GetProducts
        | Action::DescribeSpotPriceHistory
        | Action::GetServiceQuota
        | Action::DescribeCapacityBlockOfferings
        | Action::DescribeInstances
        | Action::DescribeCapacityReservations => true,
        Action::RunInstances | Action::TerminateInstances | Action::PurchaseCapacityBlock => false,
    }
}

/// Asserts that a full catalog (every purchase option, quotas) makes only calls that
/// read, each of them once except the quota (one per family and option), all in the
/// configured region, and that every action's name is the cloud's. Catches a write call
/// added to the port without notice, or a call to another region.
#[test]
fn fake_and_port_are_read_only() {
    let fake = priced();
    fake.add_block_offering(block("cb-1", "p9.24xlarge", ZONES[0], 1, "1"));
    catalog(&fake, settings(&["on-demand", "spot", "capacity-block"]));
    let calls = fake.calls();
    assert!(calls.iter().all(|call| reads_only(call.action)));
    assert!(!reads_only(Action::RunInstances));
    assert!(!reads_only(Action::TerminateInstances));
    assert!(!reads_only(Action::PurchaseCapacityBlock));
    assert!(reads_only(Action::DescribeCapacityReservations));
    assert_eq!(Action::RunInstances.name(), "RunInstances");
    assert_eq!(Action::DescribeInstances.name(), "DescribeInstances");
    assert_eq!(Action::TerminateInstances.name(), "TerminateInstances");
    assert_eq!(Action::TerminateInstances.to_string(), "TerminateInstances");
    assert!(calls.iter().all(|call| call.region.as_str() == REGION));
    let actions: Vec<&str> = calls.iter().map(|call| call.action.name()).collect();
    assert_eq!(
        actions,
        [
            "DescribeInstanceTypes",
            "DescribeInstanceTypeOfferings",
            "GetProducts",
            "DescribeSpotPriceHistory",
            "DescribeCapacityBlockOfferings",
            "GetServiceQuota",
            "GetServiceQuota",
        ]
    );
}

/// Asserts that the settings are checked once, each refusal naming its key in words: a
/// region that is not a code (empty, upper case, too long), an empty purchase list, an
/// option listed twice, an unknown option, and a block length of zero; that a length is
/// kept only when `capacity-block` is listed (no offering is read otherwise); and the
/// spelling of each purchase option both ways.
#[test]
fn settings_are_checked_once_and_name_their_key() {
    let new =
        |region: &str, purchase: &[&str]| Ec2CatalogSettings::new(region, purchase, None, None);
    for region in ["", "Region-One", "region_one", &"a".repeat(33)] {
        assert_eq!(
            new(region, &["spot"]),
            Err(SettingsError::Region),
            "{region:?}"
        );
    }
    assert_eq!(
        new(&"a1-".repeat(10), &["spot"]).unwrap().region().as_str(),
        "a1-".repeat(10)
    );
    assert_eq!(new(REGION, &[]), Err(SettingsError::NoPurchase));
    assert_eq!(
        new(REGION, &["spot", "on-demand", "spot"]),
        Err(SettingsError::PurchaseTwice("spot".to_owned()))
    );
    let messages = [
        (
            SettingsError::Region,
            "region: not a region code (lower-case letters, digits and '-', 1 to 32)",
        ),
        (
            SettingsError::NoPurchase,
            "purchase: lists no purchase option",
        ),
        (
            SettingsError::Purchase("x".to_owned()),
            "purchase: \"x\" is not one of on-demand, spot, capacity-block",
        ),
        (
            SettingsError::PurchaseTwice("spot".to_owned()),
            "purchase: \"spot\" is listed twice",
        ),
        (
            SettingsError::BlockHours,
            "capacity_block_hours: capacity-block needs a length of at least one hour",
        ),
    ];
    for (error, text) in messages {
        assert_eq!(error.to_string(), text);
    }
    let kept = Ec2CatalogSettings::new(REGION, &["spot", "on-demand"], None, Some(24)).unwrap();
    assert_eq!(
        kept.purchase(),
        [PurchaseKind::Spot, PurchaseKind::OnDemand]
    );
    assert_eq!(kept.region().to_string(), REGION);
    let fake = priced();
    fake.add_block_offering(block("cb-1", "p9.24xlarge", ZONES[0], 1, "1"));
    assert!(
        catalog(&fake, kept)
            .offers
            .iter()
            .all(|o| o.block.is_none())
    );
    for (kind, text, purchase) in [
        (PurchaseKind::OnDemand, "on-demand", Purchase::OnDemand),
        (PurchaseKind::Spot, "spot", Purchase::Spot),
        (
            PurchaseKind::CapacityBlock,
            "capacity-block",
            Purchase::CapacityBlock,
        ),
    ] {
        assert_eq!(kind.name(), text);
        assert_eq!(PurchaseKind::parse(text), Ok(kind));
        assert_eq!(kind.purchase(), purchase);
    }
    assert_eq!(
        Region::new("region-one").unwrap(),
        Region::new(REGION).unwrap()
    );
}

/// Asserts that an error is classified by the cloud's code alone: each documented throttle
/// code is throttled, each denial code denied, any other refused, each carrying the code;
/// that an error with no code says so; and that a code keeps only printable ASCII, at most
/// 80 characters. Catches a classification that reads message text or drops the code.
#[test]
fn an_error_is_classified_by_its_code() {
    for code in [
        "RequestLimitExceeded",
        "ThrottlingException",
        "TooManyRequestsException",
    ] {
        assert!(
            matches!(ApiError::answered(code), ApiError::Throttled { .. }),
            "{code}"
        );
    }
    for code in [
        "AuthFailure",
        "UnauthorizedOperation",
        "OptInRequired",
        "Blocked",
        "AccessDeniedException",
    ] {
        assert!(
            matches!(ApiError::answered(code), ApiError::Denied { .. }),
            "{code}"
        );
    }
    let other = ApiError::answered("InsufficientInstanceCapacity");
    assert_eq!(
        other.code().map(ErrorCode::as_str),
        Some("InsufficientInstanceCapacity")
    );
    assert!(matches!(other, ApiError::Refused { .. }));
    for coded in ["RequestLimitExceeded", "AuthFailure"] {
        assert_eq!(ApiError::answered(coded).code().unwrap().to_string(), coded);
    }
    for codeless in [
        ApiError::Transport(String::new()),
        ApiError::Unreadable(String::new()),
        ApiError::Incomplete,
        ApiError::Timeout,
        ApiError::NoCredential,
    ] {
        assert_eq!(codeless.code(), None);
    }
    assert_eq!(ErrorCode::new("Bad Code\u{e9}\n").as_str(), "BadCode");
    assert_eq!(ErrorCode::new(&"x".repeat(100)).as_str().len(), 80);
    // The quota family a type's name gives, and the names of the model types.
    let p5 = InstanceType("p5.48xlarge".to_owned());
    assert_eq!((p5.as_str(), p5.family()), ("p5.48xlarge", "p5"));
    assert_eq!(Zone("z".to_owned()).as_str(), "z");
}
