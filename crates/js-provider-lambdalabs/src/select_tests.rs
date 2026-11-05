use js_types::{FormFactor, Model, Resources};

use super::*;
use crate::model::Region;

fn kind(name: &str, gpus: u32, regions: &[&str]) -> InstanceTypeOffer {
    InstanceTypeOffer {
        name: name.to_owned(),
        description: String::new(),
        gpu_description: String::new(),
        price_cents_per_hour: 50,
        vcpus: 8,
        memory_gib: 64,
        storage_gib: 100,
        gpus,
        architecture: Architecture::X86_64,
        regions: regions
            .iter()
            .map(|region| Region {
                name: (*region).to_owned(),
                description: String::new(),
            })
            .collect(),
    }
}

fn allow(names: &[&str]) -> OptionTable {
    let text: String = names
        .iter()
        .map(|name| format!("[[option]]\ninstance_type = {name:?}\n"))
        .collect();
    OptionTable::parse(&text).unwrap()
}

fn anything() -> Demand {
    Demand {
        resources: Resources::default(),
        disk_bytes: None,
        os: None,
        arch: None,
        isa_level: None,
        exec: None,
        accelerator: None,
        network: Default::default(),
        host_ports: Vec::new(),
    }
}

/// The ids of `offers`, in order.
fn ids(offers: &[Offer]) -> Vec<&str> {
    offers.iter().map(|offer| offer.id.as_str()).collect()
}

/// Asserts that an offer the catalog marks unavailable is never chosen even though it
/// fits, and that the same offer is chosen when it is available: the choice reads
/// availability and the fit separately.
#[test]
fn an_unavailable_offer_is_never_chosen() {
    let table = allow(&["gpu_1x_a10"]);
    let mut all = offers(&table, &[kind("gpu_1x_a10", 1, &["r1", "r2"])], None);
    assert_eq!(candidates(&all, &anything()).len(), 2);
    all[0].available = false;
    let chosen = candidates(&all, &anything());
    assert_eq!(chosen.len(), 1);
    assert_eq!(chosen[0].zone.as_deref(), Some("r2"));
}

/// Asserts that an offer with no price is never chosen ahead of a priced one, even
/// when its id and zone sort first and its price would read as the smallest.
#[test]
fn an_unpriced_offer_is_never_chosen_ahead_of_a_priced_one() {
    let table = allow(&["gpu_1x_a10", "gpu_1x_a100"]);
    let mut all = offers(
        &table,
        &[
            kind("gpu_1x_a10", 1, &["r1"]),
            kind("gpu_1x_a100", 1, &["r1"]),
        ],
        None,
    );
    all[0].price_cents_per_hour = None;
    let chosen = candidates(&all, &anything());
    assert_eq!(ids(&chosen), ["gpu_1x_a100", "gpu_1x_a10"]);
}

/// Asserts that an offer's accelerator is the catalog table's for its type (the A10
/// family, one device, 24 GiB, PCIe), whatever Lambda's free-text GPU description says:
/// the same offer comes of a description that names the A10 and of one that is nonsense.
/// Catches GPU facts read from a configuration row or parsed from the description.
#[test]
fn gpu_facts_come_from_code_not_rows() {
    let table = allow(&["gpu_1x_a10"]);
    let mut api = kind("gpu_1x_a10", 1, &["r1"]);
    api.gpu_description = "A10 (24 GB PCIe)".to_owned();
    let first = offers(&table, std::slice::from_ref(&api), None);
    let accelerator = first[0].accelerator.clone().unwrap();
    assert_eq!(accelerator.model.as_str(), js_types::NVIDIA_A10);
    assert_eq!(accelerator.count, 1);
    assert_eq!(accelerator.memory_per_device, Some(24 << 30));
    assert_eq!(accelerator.form_factor, Some(FormFactor::Pcie));
    api.gpu_description = "Quantum Banana 9000 (3 TB)".to_owned();
    api.description = "nonsense".to_owned();
    assert_eq!(offers(&table, &[api], None), first);
}

/// Asserts that a type on the allow list is skipped, and the others still offered, when
/// Lambda offers it and the catalog table does not know it, when the table knows it and
/// Lambda does not offer it, and when Lambda's GPU count for it is not the table's; and
/// that each is named in the row faults with its reason. Catches a type offered with
/// guessed facts and one bad row dropping its neighbours.
#[test]
fn a_type_not_in_the_catalog_is_skipped() {
    let table = allow(&["gpu_1x_rtx6000", "gpu_1x_a10", "gpu_8x_a100", "gpu_4x_a100"]);
    let api = [
        kind("gpu_1x_rtx6000", 1, &["r1"]),
        kind("gpu_1x_a10", 1, &["r1"]),
        kind("gpu_4x_a100", 2, &["r1"]),
    ];
    assert_eq!(ids(&offers(&table, &api, None)), ["gpu_1x_a10"]);
    assert_eq!(
        row_faults(&table, &api),
        [
            "gpu_1x_rtx6000 is not in the catalog table",
            "gpu_8x_a100 is not offered",
            "gpu_4x_a100 has 2 gpus in the catalog, not 4",
        ]
    );
    assert_eq!(
        row_faults(&allow(&["gpu_1x_a10"]), &api),
        Vec::<String>::new()
    );
}

/// Asserts that a type the catalog table knows and Lambda offers is not offered when the
/// allow list does not name it, and is once the list does. Catches offers decided by the
/// table alone.
#[test]
fn a_known_type_not_on_the_allow_list_is_not_offered() {
    let api = [
        kind("gpu_1x_a10", 1, &["r1"]),
        kind("gpu_8x_h100_sxm5", 8, &["r1"]),
    ];
    let listed = offers(&allow(&["gpu_1x_a10"]), &api, None);
    assert_eq!(ids(&listed), ["gpu_1x_a10"]);
    let both = offers(&allow(&["gpu_1x_a10", "gpu_8x_h100_sxm5"]), &api, None);
    assert_eq!(ids(&both), ["gpu_1x_a10", "gpu_8x_h100_sxm5"]);
}

/// Asserts that the capabilities name the canonical model of each listed type the catalog
/// table knows, once per model, and leave out a listed type the table does not know and a
/// type the table knows that is not listed. Catches capabilities read from another list
/// than the offers.
#[test]
fn the_capabilities_name_the_models_of_the_listed_types() {
    let caps = capabilities(&allow(&["gpu_1x_a100", "gpu_8x_a100", "gpu_1x_rtx6000"]));
    let models: Vec<&str> = caps.accelerators.iter().map(Model::as_str).collect();
    assert_eq!(models, [js_types::NVIDIA_A100]);
    assert!(capabilities(&OptionTable::empty()).accelerators.is_empty());
}
