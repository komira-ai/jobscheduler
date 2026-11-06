//! The accelerators of an insert, end to end: the catalog's offers over the in-memory
//! project, an acquire of each, and the insert the project records. An attached accelerator
//! (an N1 type with a Tesla GPU) is on the insert; one a machine type carries is not.

use std::sync::Arc;

use js_elastic::{AcquireRequest, CapacityProvider, Offer};
use js_provider_gce::{
    AcceleratorTypeName, AttachedAccelerator, GceCatalogSettings, GceProvider, InsertRequest,
    ManualClock,
};

use crate::acquire::accepted;
use crate::rig::{Account, OTHER_ZONE, PROJECT, ZONE, ask, live, machine, zone};

/// The N1 type the attached accelerators go on: 8 vCPUs, within every row's limit.
const N1_TYPE: &str = "n1-standard-8";

/// The accelerator-optimized types of the project, each with the accelerator it carries:
/// and how it is sold (A4 is sold on Spot only): one per accelerator the crate translates by
/// name.
const BUILT_IN: [(&str, &str, &str); 5] = [
    ("a2-highgpu-1g", "nvidia-tesla-a100", "on-demand"),
    ("a2-ultragpu-1g", "nvidia-a100-80gb", "on-demand"),
    ("a3-highgpu-8g", "nvidia-h100-80gb", "on-demand"),
    ("a3-megagpu-8g", "nvidia-h100-mega-80gb", "on-demand"),
    ("a4-highgpu-8g", "nvidia-b200", "spot"),
];

/// A project with the N1 type, the accelerator-optimized types and the three attachable
/// accelerator types in the zone, with a provider over it that offers every one of the
/// types, on demand and on Spot.
fn world() -> (Arc<Account>, GceProvider) {
    let account = Arc::new(Account::new());
    account.add_machine_type(machine(N1_TYPE, 8, &[], "X86_64"));
    for (name, accelerator, _) in BUILT_IN {
        account.add_machine_type(machine(name, 24, &[(accelerator, 1)], "X86_64"));
    }
    for name in ["nvidia-tesla-t4", "nvidia-tesla-p4", "nvidia-tesla-v100"] {
        account.add_accelerator_type(name, &zone(ZONE), 8);
    }
    let mut allowed = vec![N1_TYPE.to_owned()];
    allowed.extend(BUILT_IN.iter().map(|(name, _, _)| (*name).to_owned()));
    let settings = GceCatalogSettings::new(
        PROJECT,
        &[ZONE, OTHER_ZONE],
        &["on-demand", "spot"],
        Some(allowed),
    )
    .unwrap();
    let provider = GceProvider::new(
        account.clone(),
        Arc::new(ManualClock::new()),
        settings,
        live(40),
    );
    (account, provider)
}

/// The offers of the provider's catalog, awaited on a runtime of its own.
fn offers(provider: &GceProvider) -> Vec<Offer> {
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(1)
        .build()
        .unwrap();
    runtime.block_on(provider.catalog()).unwrap()
}

/// Acquires the offer whose id is `id` and returns the insert the project recorded for it.
fn inserted_for(account: &Account, provider: &GceProvider, id: &str) -> InsertRequest {
    let offer = offers(provider)
        .into_iter()
        .find(|offer| offer.id == id && offer.zone.as_deref() == Some(ZONE))
        .unwrap_or_else(|| panic!("the catalog has no offer {id} in {ZONE}"));
    let before = account.inserts().len();
    accepted(provider.acquire(&AcquireRequest {
        offer,
        ..ask(&format!("acquire-{before}"))
    }));
    let mut sent = account.inserts();
    assert_eq!(sent.len(), before + 1, "{id}");
    sent.pop().unwrap()
}

/// Asserts for every `count` of the attachable accelerator `name` that the catalog offers
/// the N1 type with that many on demand, and that an acquire of the offer inserts the N1
/// type with exactly that accelerator type and number attached.
fn attached_family(name: &str, counts: &[u32]) {
    let (account, provider) = world();
    for count in counts {
        let id = format!("{N1_TYPE}+{name}x{count}/on-demand");
        let sent = inserted_for(&account, &provider, &id);
        assert_eq!(sent.machine_type.as_str(), N1_TYPE, "{id}");
        assert_eq!(
            sent.accelerators,
            Some(AttachedAccelerator {
                type_name: AcceleratorTypeName(name.to_owned()),
                count: *count,
            }),
            "{id}"
        );
    }
}

/// Asserts that every T4 offer of the catalog (1, 2 and 4 of them on the N1 type) inserts
/// with that many `nvidia-tesla-t4` attached. Catches a family whose catalog id does not
/// read back into the insert.
#[test]
fn every_t4_offer_of_the_catalog_attaches_its_count_of_t4() {
    attached_family("nvidia-tesla-t4", &[1, 2, 4]);
}

/// Asserts that every P4 offer of the catalog (1, 2 and 4 of them on the N1 type) inserts
/// with that many `nvidia-tesla-p4` attached. Catches a family whose catalog id does not
/// read back into the insert.
#[test]
fn every_p4_offer_of_the_catalog_attaches_its_count_of_p4() {
    attached_family("nvidia-tesla-p4", &[1, 2, 4]);
}

/// Asserts that every V100 offer of the catalog (1, 2, 4 and 8 of them on the N1 type)
/// inserts with that many `nvidia-tesla-v100` attached. Catches a family whose catalog id
/// does not read back into the insert.
#[test]
fn every_v100_offer_of_the_catalog_attaches_its_count_of_v100() {
    attached_family("nvidia-tesla-v100", &[1, 2, 4, 8]);
}

/// Asserts that the offer of each accelerator-optimized type, whose accelerators are part
/// of the machine type, inserts that machine type with no accelerator attached.
#[test]
fn offers_of_types_that_carry_their_accelerators_attach_none() {
    let (account, provider) = world();
    for (name, _, kind) in BUILT_IN {
        let sent = inserted_for(&account, &provider, &format!("{name}/{kind}"));
        assert_eq!(sent.machine_type.as_str(), name);
        assert_eq!(sent.accelerators, None, "{name}");
    }
}

/// Asserts that the offer of the N1 type with no accelerator inserts it with none attached.
#[test]
fn an_offer_with_no_accelerator_attaches_none() {
    let (account, provider) = world();
    let sent = inserted_for(&account, &provider, &format!("{N1_TYPE}/on-demand"));
    assert_eq!(sent.machine_type.as_str(), N1_TYPE);
    assert_eq!(sent.accelerators, None);
}
