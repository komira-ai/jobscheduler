//! The catalog against the in-memory project: what the cloud's answers become, and the
//! catalog semantics of the provider contract (a failure is an error, never an empty or
//! partial list). Every fixture is written by hand from the documented response shapes;
//! the project, regions and zones of the fixtures are made up, and the made-up machine
//! types (`q9-*`) are not Compute Engine's.

use std::collections::BTreeSet;
use std::sync::Arc;

use js_caps::{AcceleratorNeed, Model};
use js_elastic::{Demand, Dimension, Offer, Purchase, Quota, QuotaLimit};
use js_provider_gce::{
    ApiError, ArchName, AttachedAccelerator, CatalogAnswer, CatalogNote, FakeGce, GceCatalog,
    GceCatalogSettings, MachineTypeInfo, MachineTypeName, Method, Money, PricedItem, Project,
    QuotaMetric, Region, SeriesName, SettingsError, SkuPrice, UsageType, Zone,
};
use js_types::Resources;

const PROJECT: &str = "sample-project";
const ZONES: [&str; 3] = ["region-one-a", "region-one-b", "region-one-c"];
const OTHER_ZONE: &str = "region-two-a";

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

fn zone(name: &str) -> Zone {
    Zone::new(name).unwrap()
}

fn region() -> Region {
    zone(ZONES[0]).region()
}

/// A machine type in `zone` with `vcpus` vCPUs and 4 GiB per vCPU, the built-in
/// `accelerators` and the stated `arch`.
fn machine(
    name: &str,
    in_zone: &str,
    vcpus: u32,
    accelerators: &[(&str, u32)],
    arch: Option<&str>,
) -> MachineTypeInfo {
    MachineTypeInfo {
        name: MachineTypeName(name.to_owned()),
        zone: zone(in_zone),
        vcpus,
        memory_mib: u64::from(vcpus) * 4096,
        accelerators: accelerators
            .iter()
            .map(|(type_name, count)| AttachedAccelerator {
                type_name: js_provider_gce::AcceleratorTypeName((*type_name).to_owned()),
                count: *count,
            })
            .collect(),
        arch: arch.map(|name| ArchName(name.to_owned())),
    }
}

/// An A2 Standard type with two built-in A100 40GB in `in_zone`.
fn a2(in_zone: &str) -> MachineTypeInfo {
    machine(
        "a2-highgpu-2g",
        in_zone,
        24,
        &[("nvidia-tesla-a100", 2)],
        Some("X86_64"),
    )
}

/// An N1 type of 8 vCPUs and 30 GiB in `in_zone`, which takes attached accelerators.
fn n1(in_zone: &str) -> MachineTypeInfo {
    MachineTypeInfo {
        memory_mib: 30 * 1024,
        ..machine("n1-standard-8", in_zone, 8, &[], None)
    }
}

fn sku(item: PricedItem, usage: UsageType, units: &str, nanos: i32) -> SkuPrice {
    SkuPrice {
        item,
        usage,
        unit_price: Money {
            currency: "USD".to_owned(),
            units: units.to_owned(),
            nanos,
        },
    }
}

fn cores(series: &str) -> PricedItem {
    PricedItem::Cores(SeriesName(series.to_owned()))
}

fn memory(series: &str) -> PricedItem {
    PricedItem::Memory(SeriesName(series.to_owned()))
}

fn gpu(name: &str) -> PricedItem {
    PricedItem::Accelerator(js_provider_gce::AcceleratorTypeName(name.to_owned()))
}

fn settings(purchase: &[&str]) -> GceCatalogSettings {
    GceCatalogSettings::new(PROJECT, &ZONES, purchase, None).unwrap()
}

fn allowing(purchase: &[&str], names: &[&str]) -> GceCatalogSettings {
    let names = names.iter().map(|name| (*name).to_owned()).collect();
    GceCatalogSettings::new(PROJECT, &ZONES, purchase, Some(names)).unwrap()
}

fn catalog(fake: &Arc<FakeGce>, settings: GceCatalogSettings) -> CatalogAnswer {
    trace_everything();
    GceCatalog::new(fake.clone(), settings).catalog().unwrap()
}

fn offers(fake: &Arc<FakeGce>, purchase: &[&str]) -> Vec<Offer> {
    catalog(fake, settings(purchase)).offers
}

/// The A2 prices of the region: $0.50 a vCPU-hour, $0.05 a GiB-hour and $0.90 an A100 on
/// demand, a tenth of each on Spot.
fn add_a2_prices(fake: &FakeGce) {
    let region = region();
    for (usage, share) in [(UsageType::OnDemand, 1), (UsageType::Preemptible, 10)] {
        fake.add_price(&region, sku(cores("a2"), usage, "0", 500_000_000 / share))
            .add_price(&region, sku(memory("a2"), usage, "0", 50_000_000 / share))
            .add_price(
                &region,
                sku(gpu("nvidia-tesla-a100"), usage, "0", 900_000_000 / share),
            );
    }
}

/// A fake with one A2 type offered in every zone, priced on demand and on Spot.
fn priced() -> Arc<FakeGce> {
    let fake = Arc::new(FakeGce::new());
    for in_zone in ZONES {
        fake.add_machine_type(a2(in_zone));
    }
    add_a2_prices(&fake);
    fake
}

/// Asserts that a failure of each read the catalog cannot do without (machine types,
/// accelerator types, prices), made to fail in turn with a throttle and then a denial,
/// fails the whole catalog with an error naming the method and the cloud's reason; that
/// the failure is not the last good catalog (a good call before it does not stand in);
/// that the other kinds of failure (transport, unreadable, incomplete, another reason) are
/// errors too; and that a project empty on purpose answers an empty catalog. Catches a
/// failure collapsed to an empty list or to a partial answer.
#[test]
fn catalog_failure_is_an_error_not_an_empty_list() {
    let fake = priced();
    let provider = GceCatalog::new(fake.clone(), settings(&["on-demand", "spot"]));
    trace_everything();
    assert_eq!(provider.catalog().unwrap().offers.len(), 6);
    for method in [
        Method::MachineTypes,
        Method::AcceleratorTypes,
        Method::Prices,
    ] {
        for (reason, kind) in [("rateLimitExceeded", "throttled"), ("forbidden", "denied")] {
            fake.fail_next(method, ApiError::answered(reason));
            let error = provider.catalog().unwrap_err();
            assert_eq!(
                error.to_string(),
                format!("{method} failed: {kind} ({reason})")
            );
        }
    }
    for (error, text) in [
        (
            ApiError::Transport("timed out".to_owned()),
            "cannot talk to the cloud: timed out",
        ),
        (
            ApiError::Unreadable("no items".to_owned()),
            "the answer cannot be read: no items",
        ),
        (ApiError::Incomplete, "the answer is not complete"),
        (ApiError::answered("backendError"), "refused (backendError)"),
    ] {
        fake.fail_next(Method::AcceleratorTypes, error);
        assert_eq!(
            provider.catalog().unwrap_err().to_string(),
            format!("acceleratorTypes.aggregatedList failed: {text}")
        );
    }
    assert_eq!(provider.catalog().unwrap().offers.len(), 6);
    let empty = Arc::new(FakeGce::new());
    let answer = catalog(&empty, settings(&["on-demand", "spot"]));
    assert_eq!(
        answer,
        CatalogAnswer {
            offers: Vec::new(),
            notes: Vec::new()
        }
    );
}

/// Asserts that every offer states an architecture, `x86_64` or `arm64` and nothing else:
/// a stated `X86_64` and `ARM64` are read in lower case, a stated `AARCH64` is `arm64`, a
/// type that states none takes its series' row (C4A and T2A are `arm64`, N2 `x86_64`), and
/// a type whose architecture is unspecified or unstated with no row has no offer. Catches
/// an architecture that is defaulted, copied as the cloud spells it, or missing.
#[test]
fn arch_is_stated_on_every_offer() {
    let fake = Arc::new(FakeGce::new());
    let names = [
        ("q9-x86", Some("X86_64")),
        ("q9-arm", Some("ARM64")),
        ("q9-alias", Some("AARCH64")),
        ("c4a-standard-8", None),
        ("t2a-standard-4", None),
        ("n2-standard-4", None),
        ("q9-unstated", None),
        ("q9-unspecified", Some("ARCHITECTURE_UNSPECIFIED")),
    ];
    for (name, arch) in names {
        fake.add_machine_type(machine(name, ZONES[0], 4, &[], arch));
    }
    let all: Vec<&str> = names.iter().map(|(name, _)| *name).collect();
    let offers = catalog(&fake, allowing(&["on-demand"], &all)).offers;
    let arch_of = |name: &str| -> Vec<&str> {
        offers
            .iter()
            .filter(|offer| offer.id == format!("{name}/on-demand"))
            .map(|offer| offer.arch.as_str())
            .collect()
    };
    assert!(
        offers
            .iter()
            .all(|o| ["x86_64", "arm64"].contains(&o.arch.as_str()))
    );
    assert_eq!(arch_of("q9-x86"), ["x86_64"]);
    assert_eq!(arch_of("q9-arm"), ["arm64"]);
    assert_eq!(arch_of("q9-alias"), ["arm64"]);
    assert_eq!(arch_of("c4a-standard-8"), ["arm64"]);
    assert_eq!(arch_of("t2a-standard-4"), ["arm64"]);
    assert_eq!(arch_of("n2-standard-4"), ["x86_64"]);
    assert!(arch_of("q9-unstated").is_empty());
    assert!(arch_of("q9-unspecified").is_empty());
    assert_eq!(offers.len(), 6);
}

/// Asserts what one offer states: the id of type and provisioning model, the zone as its
/// own field, whole vCPUs as thousandths, memory in bytes, no disk, the A100's canonical
/// accelerator with its count and memory, Linux, containers, no ISA level, available, no
/// block; and that a type with no accelerator states none.
#[test]
fn an_offer_states_the_type_in_the_vocabulary() {
    let fake = priced();
    fake.add_machine_type(machine("n2-standard-2", ZONES[0], 2, &[], None));
    let offers = catalog(
        &fake,
        allowing(&["on-demand"], &["a2-highgpu-2g", "n2-standard-2"]),
    )
    .offers;
    let a100 = offers
        .iter()
        .find(|offer| offer.id == "a2-highgpu-2g/on-demand")
        .unwrap();
    assert_eq!(a100.zone.as_deref(), Some(ZONES[0]));
    assert_eq!(a100.cpu_millis, 24_000);
    assert_eq!(a100.memory_bytes, (24 * 4096) << 20);
    assert_eq!(a100.disk_bytes, 0);
    let accelerator = a100.accelerator.as_ref().unwrap();
    assert_eq!(accelerator.model.as_str(), "nvidia-a100");
    assert_eq!(accelerator.vendor.as_str(), "nvidia");
    assert_eq!(accelerator.count, 2);
    assert_eq!(accelerator.memory_per_device, Some(40 << 30));
    assert_eq!(a100.os, "linux");
    assert_eq!(a100.exec, js_types::ExecMode::Container);
    assert_eq!(a100.isa_level, None);
    assert!(a100.available);
    assert_eq!(a100.block, None);
    assert_eq!(a100.purchase, Some(Purchase::OnDemand));
    let cpu = offers
        .iter()
        .find(|offer| offer.id == "n2-standard-2/on-demand")
        .unwrap();
    assert_eq!(cpu.accelerator, None);
    assert_eq!(cpu.disk_bytes, 0);
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

/// Asserts that a type whose accelerator name is in no row of the table has offers whose
/// accelerator model is its own spelling in the lower-case `-` form, with no memory (the
/// API states none) and the vendor its first word, in each zone that lists it; that a
/// request for exactly that spelling fits the offer, and that a request for a canonical
/// model does not. Catches an unknown name dropped, mapped to the nearest known model, or
/// a panic.
#[test]
fn an_unknown_accelerator_name_passes_through_unchanged() {
    let fake = Arc::new(FakeGce::new());
    for in_zone in [ZONES[0], ZONES[1]] {
        fake.add_machine_type(machine(
            "g2-standard-8",
            in_zone,
            8,
            &[("nvidia-l4", 1)],
            None,
        ));
    }
    let offers = offers(&fake, &["on-demand"]);
    assert_eq!(offers.len(), 2);
    for offer in &offers {
        let accelerator = offer.accelerator.as_ref().unwrap();
        assert_eq!(accelerator.model.as_str(), "nvidia-l4");
        assert_eq!(accelerator.variant, None);
        assert_eq!(accelerator.vendor.as_str(), "nvidia");
        assert_eq!(accelerator.count, 1);
        assert_eq!(accelerator.memory_per_device, None);
        assert_eq!(offer.fit(&wanting("nvidia-l4", 1)), Ok(()));
        for canonical in ["nvidia-a100", "nvidia-h100", "nvidia-b200", "nvidia-a10"] {
            assert_eq!(
                offer.fit(&wanting(canonical, 1)),
                Err(Dimension::AcceleratorModel)
            );
        }
    }
}

/// Asserts that a type whose built-in accelerator name has no letter or digit has no
/// offer, while the catalog still holds the others. Catches a panic or an offer with no
/// model.
#[test]
fn an_accelerator_with_no_name_has_no_offer() {
    let fake = priced();
    fake.add_machine_type(machine(
        "q9-nameless",
        ZONES[0],
        4,
        &[("--", 1)],
        Some("X86_64"),
    ));
    let offers = offers(&fake, &["on-demand"]);
    assert_eq!(offers.len(), 3);
    assert!(
        offers
            .iter()
            .all(|offer| offer.id == "a2-highgpu-2g/on-demand")
    );
}

/// Asserts that a type offered in three zones with `purchase = ["on-demand", "spot"]` has
/// six offers with distinct (id, zone) pairs, a standard and a Spot one per zone, with
/// `purchase = ["on-demand"]` three and no Spot offer, and with `purchase = ["spot"]` three
/// Spot offers. Catches models emitted unasked, collapsed, or zones merged.
#[test]
fn one_offer_per_type_zone_and_provisioning_model() {
    let fake = priced();
    let both = offers(&fake, &["on-demand", "spot"]);
    let pairs: BTreeSet<(String, Option<String>)> = both
        .iter()
        .map(|o| (o.id.clone(), o.zone.clone()))
        .collect();
    assert_eq!(both.len(), 6);
    assert_eq!(pairs.len(), 6);
    for in_zone in ZONES {
        for (id, purchase) in [
            ("a2-highgpu-2g/on-demand", Purchase::OnDemand),
            ("a2-highgpu-2g/spot", Purchase::Spot),
        ] {
            let found: Vec<&Offer> = both
                .iter()
                .filter(|o| o.id == id && o.zone.as_deref() == Some(in_zone))
                .collect();
            assert_eq!(found.len(), 1, "{id} {in_zone}");
            assert_eq!(found[0].purchase, Some(purchase));
        }
    }
    let standard = offers(&fake, &["on-demand"]);
    assert_eq!(standard.len(), 3);
    assert!(
        standard
            .iter()
            .all(|o| o.purchase == Some(Purchase::OnDemand))
    );
    let spot = offers(&fake, &["spot"]);
    assert_eq!(spot.len(), 3);
    assert!(spot.iter().all(|o| o.purchase == Some(Purchase::Spot)));
}

/// Asserts that a type the answer lists only in a zone the configuration does not name,
/// and an accelerator type offered only there, give no offer, while the type of a named
/// zone keeps its offers. Catches a type with no configured zone listed as available.
#[test]
fn a_type_with_no_zone_has_no_offer() {
    let fake = priced();
    fake.add_machine_type(a2("region-one-z"))
        .add_machine_type(machine(
            "a3-megagpu-8g",
            "region-one-z",
            208,
            &[("nvidia-h100-mega-80gb", 8)],
            None,
        ))
        .add_machine_type(n1(ZONES[2]))
        .add_accelerator_type("nvidia-tesla-t4", &zone("region-one-z"), 4);
    let offers = offers(&fake, &["on-demand", "spot"]);
    assert_eq!(offers.len(), 6);
    assert!(
        offers.iter().all(
            |o| o.id.starts_with("a2-highgpu-2g/") && o.zone.as_deref() != Some("region-one-z")
        )
    );
}

/// Asserts that an N1 type in two zones, with the T4 offered only in the first and the
/// V100 (at most two per machine) only in the second, has T4 offers of 1, 2 and 4 in the
/// first zone only and V100 offers of 1 and 2 in the second only, each stating the
/// attached accelerator; that without an allow list the N1 type has no offer of its own,
/// and with one it has one in each zone. Catches an attachable accelerator offered where
/// the zone does not list it, or past the zone's maximum.
#[test]
fn an_attachable_accelerator_offered_in_one_zone_only_offers_there() {
    let fake = Arc::new(FakeGce::new());
    fake.add_machine_type(n1(ZONES[0]))
        .add_machine_type(n1(ZONES[1]))
        .add_accelerator_type("nvidia-tesla-t4", &zone(ZONES[0]), 4)
        .add_accelerator_type("nvidia-tesla-v100", &zone(ZONES[1]), 2);
    let keys = |offers: &[Offer]| -> Vec<(String, String)> {
        offers
            .iter()
            .map(|o| (o.id.clone(), o.zone.clone().unwrap()))
            .collect()
    };
    let key = |id: &str, in_zone: &str| (id.to_owned(), in_zone.to_owned());
    let attached = offers(&fake, &["on-demand"]);
    assert_eq!(
        keys(&attached),
        [
            key("n1-standard-8+nvidia-tesla-t4x1/on-demand", ZONES[0]),
            key("n1-standard-8+nvidia-tesla-t4x2/on-demand", ZONES[0]),
            key("n1-standard-8+nvidia-tesla-t4x4/on-demand", ZONES[0]),
            key("n1-standard-8+nvidia-tesla-v100x1/on-demand", ZONES[1]),
            key("n1-standard-8+nvidia-tesla-v100x2/on-demand", ZONES[1]),
        ]
    );
    let t4x2 = attached[1].accelerator.as_ref().unwrap();
    assert_eq!((t4x2.model.as_str(), t4x2.count), ("nvidia-tesla-t4", 2));
    let listed = catalog(&fake, allowing(&["on-demand"], &["n1-standard-8"])).offers;
    assert_eq!(
        keys(&listed),
        [
            key("n1-standard-8/on-demand", ZONES[0]),
            key("n1-standard-8+nvidia-tesla-t4x1/on-demand", ZONES[0]),
            key("n1-standard-8+nvidia-tesla-t4x2/on-demand", ZONES[0]),
            key("n1-standard-8+nvidia-tesla-t4x4/on-demand", ZONES[0]),
            key("n1-standard-8/on-demand", ZONES[1]),
            key("n1-standard-8+nvidia-tesla-v100x1/on-demand", ZONES[1]),
            key("n1-standard-8+nvidia-tesla-v100x2/on-demand", ZONES[1]),
        ]
    );
    assert_eq!(listed[0].accelerator, None);
}

/// Asserts the prices in cents per hour, a fraction of a cent rounded up, from unit prices
/// of several magnitudes: an N1 of 4 vCPUs at $0.031611 and 15 GiB at $0.004237 is 19 on
/// demand ($0.189999) and 4 on Spot ($0.04 from the preemptible SKUs), with one T4 at
/// $0.35 it is 54 and with two 89, and Spot with one T4 at $0.11 is 15; a G4 of 48 vCPUs at
/// $0.041, 180 GiB at $0.005 and one GPU at 3 units and 999999999 nanos is 687; and a type
/// with no SKU (A3) is listed with no price. Catches float drift, the standard price used
/// for Spot, and unpriced offers dropped.
#[test]
fn offers_carry_price_as_integer_cents() {
    let fake = Arc::new(FakeGce::new());
    let region = region();
    fake.add_machine_type(MachineTypeInfo {
        memory_mib: 15 * 1024,
        ..machine("n1-standard-4", ZONES[0], 4, &[], None)
    })
    .add_machine_type(MachineTypeInfo {
        memory_mib: 180 * 1024,
        ..machine(
            "g4-standard-48",
            ZONES[0],
            48,
            &[("nvidia-rtx-pro-6000", 1)],
            None,
        )
    })
    .add_machine_type(machine(
        "a3-highgpu-8g",
        ZONES[0],
        208,
        &[("nvidia-h100-80gb", 8)],
        None,
    ))
    .add_accelerator_type("nvidia-tesla-t4", &zone(ZONES[0]), 2);
    for price in [
        sku(cores("n1"), UsageType::OnDemand, "0", 31_611_000),
        sku(memory("n1"), UsageType::OnDemand, "0", 4_237_000),
        sku(cores("n1"), UsageType::Preemptible, "0", 6_655_000),
        sku(memory("n1"), UsageType::Preemptible, "0", 892_000),
        sku(
            gpu("nvidia-tesla-t4"),
            UsageType::OnDemand,
            "0",
            350_000_000,
        ),
        sku(
            gpu("nvidia-tesla-t4"),
            UsageType::Preemptible,
            "0",
            110_000_000,
        ),
        sku(cores("g4"), UsageType::OnDemand, "0", 41_000_000),
        sku(memory("g4"), UsageType::OnDemand, "0", 5_000_000),
        sku(
            gpu("nvidia-rtx-pro-6000"),
            UsageType::OnDemand,
            "3",
            999_999_999,
        ),
    ] {
        fake.add_price(&region, price);
    }
    let names = ["n1-standard-4", "g4-standard-48", "a3-highgpu-8g"];
    let offers = catalog(&fake, allowing(&["on-demand", "spot"], &names)).offers;
    let price = |id: &str| {
        offers
            .iter()
            .find(|o| o.id == id)
            .map(|o| o.price_cents_per_hour)
            .unwrap()
    };
    assert_eq!(price("n1-standard-4/on-demand"), Some(19));
    assert_eq!(price("n1-standard-4/spot"), Some(4));
    assert_eq!(price("n1-standard-4+nvidia-tesla-t4x1/on-demand"), Some(54));
    assert_eq!(price("n1-standard-4+nvidia-tesla-t4x2/on-demand"), Some(89));
    assert_eq!(price("n1-standard-4+nvidia-tesla-t4x1/spot"), Some(15));
    assert_eq!(price("g4-standard-48/on-demand"), Some(687));
    assert_eq!(price("g4-standard-48/spot"), None);
    assert_eq!(price("a3-highgpu-8g/on-demand"), None);
    assert_eq!(price("a3-highgpu-8g/spot"), None);
    assert_eq!(offers.len(), 10);
    assert!(offers.iter().all(|o| o.available));
}

/// Asserts that a unit price that cannot be read (units that are not a number, a negative
/// one, nanos beyond a unit, another currency) fails the catalog as unreadable, naming the
/// method and the whole value. Catches an unreadable price become a free offer.
#[test]
fn an_unparseable_price_is_an_error_not_zero() {
    let cases = [
        ("free", 0, "USD"),
        ("-1", 0, "USD"),
        ("0", 1_000_000_000, "USD"),
        ("1", 0, "EUR"),
    ];
    for (units, nanos, currency) in cases {
        let fake = priced();
        fake.add_price(
            &region(),
            SkuPrice {
                item: cores("a2"),
                usage: UsageType::OnDemand,
                unit_price: Money {
                    currency: currency.to_owned(),
                    units: units.to_owned(),
                    nanos,
                },
            },
        );
        let error = GceCatalog::new(fake, settings(&["on-demand"]))
            .catalog()
            .unwrap_err();
        assert_eq!(
            error.to_string(),
            format!(
                "services.skus.list failed: the answer cannot be read: {units} units and \
                 {nanos} nanos of {currency:?} is not a price in US dollars"
            )
        );
    }
}

fn quota(name: &str, limit: u64, usage: Option<u64>) -> QuotaMetric {
    QuotaMetric {
        metric: name.to_owned(),
        limit,
        usage,
    }
}

fn limit(limit: u32, in_use: Option<u32>) -> Option<QuotaLimit> {
    Some(QuotaLimit { limit, in_use })
}

/// Asserts that the region's quotas are stated on each offer and never remove or mark one:
/// an A100 limit of 1, below the type's 2 GPUs, leaves its offers listed and available
/// with that limit; standard and Spot offers carry their own metrics (the A100's standard
/// and preemptible GPUs); an N1 with a T4 states the `CPUS` vCPUs and the T4 GPUs, and on
/// Spot, with a preemptible vCPU limit of zero, the standard vCPUs; each region's quotas
/// are read once and state that region's numbers. Catches a quota that removes or marks an
/// offer, or one number shared by the two models.
#[test]
fn quota_is_read_into_the_offer_and_never_filters_it() {
    let zones = [ZONES[0], ZONES[1], OTHER_ZONE];
    let fake = priced();
    let other = zone(OTHER_ZONE).region();
    fake.add_machine_type(n1(ZONES[0]))
        .add_machine_type(a2(OTHER_ZONE))
        .add_accelerator_type("nvidia-tesla-t4", &zone(ZONES[0]), 1)
        .add_quota(&region(), quota("NVIDIA_A100_GPUS", 1, Some(0)))
        .add_quota(
            &region(),
            quota("PREEMPTIBLE_NVIDIA_A100_GPUS", 16, Some(4)),
        )
        .add_quota(&region(), quota("CPUS", 24, Some(8)))
        .add_quota(&region(), quota("PREEMPTIBLE_CPUS", 0, Some(0)))
        .add_quota(&region(), quota("NVIDIA_T4_GPUS", 4, None))
        .add_quota(&other, quota("NVIDIA_A100_GPUS", 64, None));
    let settings = GceCatalogSettings::new(PROJECT, &zones, &["on-demand", "spot"], None);
    let answer = catalog(&fake, settings.unwrap());
    assert!(answer.notes.is_empty());
    let quota_of = |id: &str, in_zone: &str| {
        answer
            .offers
            .iter()
            .find(|o| o.id == id && o.zone.as_deref() == Some(in_zone))
            .map(|o| o.quota)
            .unwrap()
    };
    let gpus = |limit| Quota {
        vcpus: None,
        accelerators: limit,
    };
    for in_zone in [ZONES[0], ZONES[1]] {
        assert_eq!(
            quota_of("a2-highgpu-2g/on-demand", in_zone),
            Some(gpus(limit(1, Some(0))))
        );
        assert_eq!(
            quota_of("a2-highgpu-2g/spot", in_zone),
            Some(gpus(limit(16, Some(4))))
        );
    }
    assert_eq!(
        quota_of("a2-highgpu-2g/on-demand", OTHER_ZONE),
        Some(gpus(limit(64, None)))
    );
    assert_eq!(
        quota_of("a2-highgpu-2g/spot", OTHER_ZONE),
        Some(gpus(limit(64, None)))
    );
    let n1 = Quota {
        vcpus: limit(24, Some(8)),
        accelerators: limit(4, None),
    };
    assert_eq!(
        quota_of("n1-standard-8+nvidia-tesla-t4x1/on-demand", ZONES[0]),
        Some(n1)
    );
    assert_eq!(
        quota_of("n1-standard-8+nvidia-tesla-t4x1/spot", ZONES[0]),
        Some(n1)
    );
    assert_eq!(answer.offers.len(), 8);
    assert!(answer.offers.iter().all(|o| o.available));
    let reads: Vec<Option<Region>> = fake
        .calls()
        .into_iter()
        .filter(|call| call.method == Method::RegionQuotas)
        .map(|call| call.region)
        .collect();
    assert_eq!(reads, [Some(region()), Some(other)]);
}

/// Asserts that when only the quota reads fail the catalog is still whole: every offer is
/// listed with no quota, and the failed read of each region is one note naming the region
/// and the error. Catches a quota failure that empties or fails the catalog.
#[test]
fn a_failed_quota_read_keeps_the_offers_without_a_quota() {
    let fake = priced();
    fake.add_quota(&region(), quota("NVIDIA_A100_GPUS", 8, None));
    let throttled = ApiError::answered("rateLimitExceeded");
    fake.fail_always(Method::RegionQuotas, throttled.clone());
    let answer = catalog(&fake, settings(&["on-demand", "spot"]));
    assert_eq!(answer.offers.len(), 6);
    assert!(answer.offers.iter().all(|o| o.quota.is_none()));
    assert_eq!(
        answer.notes,
        [CatalogNote {
            region: region(),
            error: throttled
        }]
    );
}

/// Asserts that with an allow list exactly the types it names appear, a CPU type among
/// them, and a listed type the zones lack adds nothing; and that with no allow list only
/// the types with an accelerator appear. Catches an allow list ignored, or a default that
/// lists every type.
#[test]
fn the_allow_list_selects_and_the_default_is_accelerator_types_only() {
    let fake = Arc::new(FakeGce::new());
    fake.add_machine_type(a2(ZONES[0]))
        .add_machine_type(machine(
            "g2-standard-8",
            ZONES[0],
            8,
            &[("nvidia-l4", 1)],
            None,
        ))
        .add_machine_type(machine("n2-standard-4", ZONES[0], 4, &[], None))
        .add_machine_type(machine("c4a-standard-4", ZONES[0], 4, &[], None));
    let ids = |answer: CatalogAnswer| -> Vec<String> {
        answer.offers.into_iter().map(|o| o.id).collect()
    };
    let listed = catalog(
        &fake,
        allowing(
            &["on-demand"],
            &["a2-highgpu-2g", "n2-standard-4", "q9-absent"],
        ),
    );
    assert_eq!(
        ids(listed),
        ["a2-highgpu-2g/on-demand", "n2-standard-4/on-demand"]
    );
    assert_eq!(
        ids(catalog(&fake, settings(&["on-demand"]))),
        ["a2-highgpu-2g/on-demand", "g2-standard-8/on-demand"]
    );
}

/// Asserts the purchase finding: the newest accelerator-optimized series are not all sold
/// on the standard model. With both models listed, A4X (sold only through capacity-block
/// reservations) has no offer, A4, A3 Ultra and A3 High with one GPU have Spot offers only,
/// and A3 High with eight GPUs and A3 Mega have both; with only `on-demand` listed, the
/// Spot-only series have none. The settings refuse every option this provider does not
/// offer (`capacity-block`, `reserved`, `flex-start`). Catches an offer for a model the
/// series is not sold on, an unlisted model offered, or an unsupported value accepted.
#[test]
fn the_purchase_finding_is_enforced() {
    let fake = Arc::new(FakeGce::new());
    for (name, accelerator, count) in [
        ("a4x-highgpu-4g", "nvidia-gb200", 4),
        ("a4-highgpu-8g", "nvidia-b200", 8),
        ("a3-ultragpu-8g", "nvidia-h200-141gb", 8),
        ("a3-highgpu-1g", "nvidia-h100-80gb", 1),
        ("a3-highgpu-8g", "nvidia-h100-80gb", 8),
        ("a3-megagpu-8g", "nvidia-h100-mega-80gb", 8),
    ] {
        fake.add_machine_type(machine(
            name,
            ZONES[0],
            26 * count,
            &[(accelerator, count)],
            None,
        ));
    }
    let ids = |purchase: &[&str]| -> Vec<String> {
        offers(&fake, purchase).into_iter().map(|o| o.id).collect()
    };
    assert_eq!(
        ids(&["on-demand", "spot"]),
        [
            "a3-highgpu-1g/spot",
            "a3-highgpu-8g/on-demand",
            "a3-highgpu-8g/spot",
            "a3-megagpu-8g/on-demand",
            "a3-megagpu-8g/spot",
            "a3-ultragpu-8g/spot",
            "a4-highgpu-8g/spot",
        ]
    );
    assert_eq!(
        ids(&["on-demand"]),
        ["a3-highgpu-8g/on-demand", "a3-megagpu-8g/on-demand"]
    );
    for refused in ["capacity-block", "reserved", "flex-start", "Spot"] {
        assert_eq!(
            GceCatalogSettings::new(PROJECT, &ZONES, &[refused], None),
            Err(SettingsError::Purchase(refused.to_owned()))
        );
    }
}

/// The fake of the ordering test, its answers added in the order `forward` says.
fn ordered(forward: bool) -> Arc<FakeGce> {
    let fake = Arc::new(FakeGce::new());
    let mut types = Vec::new();
    let mut accelerators = Vec::new();
    for in_zone in ZONES {
        types.push(a2(in_zone));
        types.push(n1(in_zone));
        accelerators.push(("nvidia-tesla-v100", in_zone));
        accelerators.push(("nvidia-tesla-t4", in_zone));
    }
    let mut prices = vec![
        sku(cores("n1"), UsageType::OnDemand, "0", 30_000_000),
        sku(memory("n1"), UsageType::OnDemand, "0", 4_000_000),
        sku(
            gpu("nvidia-tesla-t4"),
            UsageType::OnDemand,
            "0",
            350_000_000,
        ),
    ];
    let mut quotas = vec![
        quota("CPUS", 24, Some(1)),
        quota("NVIDIA_T4_GPUS", 4, Some(2)),
    ];
    if !forward {
        types.reverse();
        accelerators.reverse();
        prices.reverse();
        quotas.reverse();
    }
    for info in types {
        fake.add_machine_type(info);
    }
    for (name, in_zone) in accelerators {
        fake.add_accelerator_type(name, &zone(in_zone), 4);
    }
    for price in prices {
        fake.add_price(&region(), price);
    }
    for metric in quotas {
        fake.add_quota(&region(), metric);
    }
    add_a2_prices(&fake);
    fake
}

/// Asserts that the same answers in another order give a byte-identical offer list; that
/// the order is machine type, zone, accelerators (built in or none, then attached ones by
/// name and number), then the configured order of the models; and that a type listed
/// twice in a zone gives its offers once. Catches an order that depends on the API's page
/// order.
#[test]
fn offer_order_is_stable() {
    let purchase = ["spot", "on-demand"];
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
            ("a2-highgpu-2g/spot".to_owned(), ZONES[0].to_owned()),
            ("a2-highgpu-2g/on-demand".to_owned(), ZONES[0].to_owned()),
            ("a2-highgpu-2g/spot".to_owned(), ZONES[1].to_owned()),
            ("a2-highgpu-2g/on-demand".to_owned(), ZONES[1].to_owned()),
        ]
    );
    let n1_zone_a: Vec<&str> = one
        .iter()
        .filter(|o| o.id.starts_with("n1-") && o.zone.as_deref() == Some(ZONES[0]))
        .map(|o| o.id.as_str())
        .collect();
    assert_eq!(
        n1_zone_a,
        [
            "n1-standard-8+nvidia-tesla-t4x1/spot",
            "n1-standard-8+nvidia-tesla-t4x1/on-demand",
            "n1-standard-8+nvidia-tesla-t4x2/spot",
            "n1-standard-8+nvidia-tesla-t4x2/on-demand",
            "n1-standard-8+nvidia-tesla-t4x4/spot",
            "n1-standard-8+nvidia-tesla-t4x4/on-demand",
            "n1-standard-8+nvidia-tesla-v100x1/spot",
            "n1-standard-8+nvidia-tesla-v100x1/on-demand",
            "n1-standard-8+nvidia-tesla-v100x2/spot",
            "n1-standard-8+nvidia-tesla-v100x2/on-demand",
            "n1-standard-8+nvidia-tesla-v100x4/spot",
            "n1-standard-8+nvidia-tesla-v100x4/on-demand",
        ]
    );
    let twice = ordered(true);
    twice.add_machine_type(a2(ZONES[0])).add_accelerator_type(
        "nvidia-tesla-t4",
        &zone(ZONES[0]),
        4,
    );
    assert_eq!(offers(&twice, &purchase), one);
}

/// Whether `method` only reads. The match names every method, so a new one does not
/// compile until it is placed here.
fn reads_only(method: Method) -> bool {
    match method {
        Method::MachineTypes | Method::AcceleratorTypes | Method::Prices | Method::RegionQuotas => {
            true
        }
    }
}

/// Asserts that a full catalog (both models, two regions, quotas) makes only calls that
/// read, the machine and accelerator types once for the configured project and zones, the
/// prices and the quotas once per region, and that every method's name is the cloud's.
/// Catches a write call added to the port without notice, or a call for another project
/// or zone.
#[test]
fn fake_and_port_are_read_only() {
    let zones = [ZONES[0], OTHER_ZONE];
    let fake = priced();
    fake.add_machine_type(a2(OTHER_ZONE));
    let settings = GceCatalogSettings::new(PROJECT, &zones, &["on-demand", "spot"], None);
    catalog(&fake, settings.unwrap());
    let calls = fake.calls();
    assert!(calls.iter().all(|call| reads_only(call.method)));
    let methods: Vec<&str> = calls.iter().map(|call| call.method.name()).collect();
    assert_eq!(
        methods,
        [
            "machineTypes.aggregatedList",
            "acceleratorTypes.aggregatedList",
            "services.skus.list",
            "services.skus.list",
            "regions.get",
            "regions.get",
        ]
    );
    let project = Project::new(PROJECT).unwrap();
    let configured: Vec<Zone> = zones.iter().map(|name| zone(name)).collect();
    for call in &calls[..2] {
        assert_eq!(call.project.as_ref(), Some(&project));
        assert_eq!(call.zones, configured);
        assert_eq!(call.region, None);
    }
    let regions: Vec<Option<String>> = calls[2..]
        .iter()
        .map(|call| call.region.as_ref().map(Region::to_string))
        .collect();
    assert_eq!(
        regions,
        ["region-one", "region-two", "region-one", "region-two"].map(|name| Some(name.to_owned()))
    );
    assert_eq!(calls[2].project, None);
    assert_eq!(calls[4].project.as_ref(), Some(&project));
    assert!(calls[2..].iter().all(|call| call.zones.is_empty()));
    let display: Vec<String> = [
        Method::MachineTypes,
        Method::AcceleratorTypes,
        Method::Prices,
        Method::RegionQuotas,
    ]
    .iter()
    .map(Method::to_string)
    .collect();
    assert_eq!(
        display,
        [
            "machineTypes.aggregatedList",
            "acceleratorTypes.aggregatedList",
            "services.skus.list",
            "regions.get"
        ]
    );
}
