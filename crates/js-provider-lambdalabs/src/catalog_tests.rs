use js_types::{FAMILIES, FormFactor, Interconnect, Model, Vendor};

use super::*;

/// Asserts that every row's accelerator names a canonical model the alias table defines
/// (`Model::canonical`, the lookup `js-caps` re-exports, knows it as a family) with the
/// row's count, and that the family is the one the row states. Catches a row added with a
/// family the alias table does not define, which would offer a model no request and no
/// agent hello resolves to.
#[test]
fn every_emitted_canonical_name_is_in_the_alias_table() {
    for row in TYPES {
        let accelerator = row.accelerator().unwrap_or_else(|| panic!("{row:?}"));
        let resolved = accelerator.model.canonical();
        assert!(
            resolved.known,
            "{}: {}",
            row.instance_type, accelerator.model
        );
        assert_eq!(resolved.model, accelerator.model, "{}", row.instance_type);
        assert_eq!(
            accelerator.model.as_str(),
            row.family,
            "{}",
            row.instance_type
        );
        assert!(
            FAMILIES.iter().any(|family| family.name == row.family),
            "{}",
            row.instance_type
        );
        assert_eq!(accelerator.count, row.count, "{}", row.instance_type);
        assert_eq!(accelerator.vendor.as_str(), Vendor::NVIDIA);
    }
}

/// Asserts that the table is sorted by name with no name twice, that every row is found by
/// its name, and that a name it does not hold, a prefix of one it holds and the empty name
/// find nothing. Catches a row added out of order, which the binary search would miss.
#[test]
fn every_row_is_found_by_its_name_and_no_other_name_is() {
    assert!(
        TYPES
            .windows(2)
            .all(|pair| pair[0].instance_type < pair[1].instance_type)
    );
    for row in TYPES {
        assert_eq!(entry(row.instance_type), Some(row));
    }
    for name in ["gpu_1x_rtx6000", "gpu_1x_a1", "gpu_8x_h100_sxm5x", ""] {
        assert_eq!(entry(name), None, "{name}");
    }
}

/// Asserts the accelerator of a type whose variant the alias table knows: the 8x A100 80
/// GB SXM4 type carries the `nvidia-smi` spelling of that variant, the table's memory and
/// form factor, and an NVLink domain of 8. Catches a row whose memory or form factor
/// disagrees with the alias table (the variant would then be missed) and a lost
/// interconnect.
#[test]
fn a_known_variant_carries_its_spelling_and_the_nvlink_domain() {
    let accelerator = entry("gpu_8x_a100_80gb_sxm4")
        .and_then(Entry::accelerator)
        .unwrap();
    assert_eq!(accelerator.model.as_str(), NVIDIA_A100);
    assert_eq!(
        accelerator.variant.as_ref().map(Model::as_str),
        Some("nvidia-a100-sxm4-80gb")
    );
    assert_eq!(accelerator.count, 8);
    assert_eq!(accelerator.memory_per_device, Some(80 * GIB));
    assert_eq!(accelerator.form_factor, Some(FormFactor::Sxm));
    assert_eq!(
        accelerator.interconnect,
        Some(Interconnect::parse("nvlink-domain>=8").unwrap())
    );
}

/// Asserts the accelerator of a type whose family the alias table knows and no variant of
/// it: the 1x A10 type is the family, one device, the row's 24 GiB and PCIe, and no
/// interconnect. Catches memory or form factor dropped where the table has no variant.
#[test]
fn a_family_only_type_carries_the_rows_memory_and_form() {
    let accelerator = entry("gpu_1x_a10").and_then(Entry::accelerator).unwrap();
    assert_eq!(accelerator.model.as_str(), NVIDIA_A10);
    assert_eq!(accelerator.variant, None);
    assert_eq!(accelerator.count, 1);
    assert_eq!(accelerator.memory_per_device, Some(24 * GIB));
    assert_eq!(accelerator.form_factor, Some(FormFactor::Pcie));
    assert_eq!(accelerator.interconnect, None);
    let unknown_form = entry("gpu_2x_a100").and_then(Entry::accelerator).unwrap();
    assert_eq!(
        (unknown_form.variant, unknown_form.form_factor),
        (None, None)
    );
    assert_eq!(unknown_form.memory_per_device, Some(40 * GIB));
}

/// Asserts that each row the alias table knows a variant for keeps the table's memory and
/// form factor, and that the variant spelling is the one the table resolves to this row's
/// family: the PCIe and SXM forms of the 40 GB A100 and of the H100 are told apart. Catches
/// a row that picks the wrong variant of its family.
#[test]
fn each_variant_row_resolves_to_its_own_variant() {
    let spelt = |name: &str| entry(name).map(Entry::spelling);
    assert_eq!(spelt("gpu_1x_a100"), Some("nvidia-a100-pcie-40gb"));
    assert_eq!(spelt("gpu_1x_a100_sxm4"), Some("nvidia-a100-sxm4-40gb"));
    assert_eq!(spelt("gpu_8x_a100"), Some("nvidia-a100-sxm4-40gb"));
    assert_eq!(spelt("gpu_1x_h100_pcie"), Some("nvidia-h100-pcie"));
    assert_eq!(spelt("gpu_8x_h100_sxm5"), Some("nvidia-h100-80gb-hbm3"));
    assert_eq!(spelt("gpu_4x_a100"), Some(NVIDIA_A100));
    for row in TYPES {
        let accelerator = row.accelerator().unwrap();
        let variant = Model::parse(row.spelling()).unwrap().canonical().variant;
        if let Some(variant) = variant {
            assert_eq!(variant.memory_per_device, row.memory_gib * GIB);
            assert_eq!(Some(variant.form_factor), row.form_factor);
        }
        assert_eq!(accelerator.memory_per_device, Some(row.memory_gib * GIB));
        assert_eq!(accelerator.form_factor, row.form_factor);
        let fabric = accelerator.interconnect.map(|link| link.size());
        assert_eq!(fabric, row.nvlink_domain, "{}", row.instance_type);
    }
}
