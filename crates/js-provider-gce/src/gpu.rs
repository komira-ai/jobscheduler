//! The translation of Compute Engine's accelerator type names into the accelerator
//! vocabulary, and the accelerators an N1 machine type may have attached.
//!
//! A table in code maps this cloud's source spelling (the accelerator type name the API
//! returns, which carries the memory where one GPU has several sizes: `nvidia-tesla-a100`
//! and `nvidia-a100-80gb`) to a spelling that `js-caps`'s alias table defines, which
//! resolves to the canonical model and, for a variant spelling, its variant facts. This
//! crate defines no canonical name. A spelling the table does not know passes through as
//! its own spelling (lower case, words joined by `-`), so a request that names exactly
//! that spelling still matches it.

use js_caps::{Accelerator, Model, accelerator_of};

use crate::model::{AcceleratorTypeName, MachineTypeName};

/// Bytes in a GiB.
const GIB: u64 = 1 << 30;

/// The table: the accelerator type name, the `js-caps` spelling it is, and the memory of
/// one GPU in GiB as Compute Engine's GPU machine types page states it (in "GB", the unit
/// the vendor's data sheet and `js-caps` read as GiB). A row is here only when the name and
/// the GPU were read from Google's documentation and `js-caps` defines the model; every
/// other accelerator passes through (the crate docs list them).
pub(crate) const ACCELERATOR_ROWS: [(&str, &str, u64); 5] = [
    // A2 Standard: "these machine types have A100 40GB GPUs (`nvidia-tesla-a100`)" (GPU
    // machine types). The form factor is not stated, so the family, with its memory.
    ("nvidia-tesla-a100", "nvidia-a100", 40),
    // A2 Ultra: "these machine types have A100 80GB GPUs (`nvidia-a100-80gb`)" (GPU machine
    // types). The form factor is not stated, so the family, with its memory.
    ("nvidia-a100-80gb", "nvidia-a100", 80),
    // A3 High and A3 Edge: "these machine types have H100 SXM GPUs (`nvidia-h100-80gb`)",
    // 80 GB of HBM3 (GPU machine types): the SXM5 module of 80 GB HBM3.
    ("nvidia-h100-80gb", "nvidia-h100-80gb-hbm3", 80),
    // A3 Mega: "these machine types have H100 SXM GPUs (`nvidia-h100-mega-80gb`)", 80 GB of
    // HBM3 (GPU machine types): the same GPU as A3 High, with more network bandwidth.
    ("nvidia-h100-mega-80gb", "nvidia-h100-80gb-hbm3", 80),
    // A4: "A4 (NVIDIA B200) (`nvidia-b200`)", 180 GB of HBM3e per GPU (GPU machine types).
    // The family, with its memory.
    ("nvidia-b200", "nvidia-b200", 180),
];

/// The machine types that take attached accelerators: the N1 general-purpose types ("N1
/// machine types don't come with a set number of attached GPUs", GPU machine types). The
/// shared-core `f1-micro` and `g1-small` take none ("except for the N1 shared-core",
/// creating a VM with attached GPUs) and do not start with it.
const ATTACHING: &str = "n1-";

/// The accelerators an N1 machine type may have attached: the accelerator type name, a
/// number of them, and the most vCPUs and memory (GB, which for N1 types is GiB: an
/// `n1-standard-96` has 360 GB, 368640 MiB) an N1 machine type may have with that number
/// (the N1 table of GPU machine types: "N1 instances with fewer GPUs limit the maximum
/// number of vCPUs"). The virtual workstation forms (`-vws`) are not attached by the
/// catalog.
pub(crate) const ATTACH_ROWS: [(&str, u32, u32, u64); 10] = [
    // NVIDIA T4 (`nvidia-tesla-t4`): 1 GPU, up to 48 vCPUs and 312 GB.
    ("nvidia-tesla-t4", 1, 48, 312),
    // NVIDIA T4: 2 GPUs, up to 48 vCPUs and 312 GB.
    ("nvidia-tesla-t4", 2, 48, 312),
    // NVIDIA T4: 4 GPUs, up to 96 vCPUs and 624 GB.
    ("nvidia-tesla-t4", 4, 96, 624),
    // NVIDIA P4 (`nvidia-tesla-p4`): 1 GPU, up to 24 vCPUs and 156 GB.
    ("nvidia-tesla-p4", 1, 24, 156),
    // NVIDIA P4: 2 GPUs, up to 48 vCPUs and 312 GB.
    ("nvidia-tesla-p4", 2, 48, 312),
    // NVIDIA P4: 4 GPUs, up to 96 vCPUs and 624 GB.
    ("nvidia-tesla-p4", 4, 96, 624),
    // NVIDIA V100 (`nvidia-tesla-v100`): 1 GPU, up to 12 vCPUs and 78 GB.
    ("nvidia-tesla-v100", 1, 12, 78),
    // NVIDIA V100: 2 GPUs, up to 24 vCPUs and 156 GB.
    ("nvidia-tesla-v100", 2, 24, 156),
    // NVIDIA V100: 4 GPUs, up to 48 vCPUs and 312 GB.
    ("nvidia-tesla-v100", 4, 48, 312),
    // NVIDIA V100: 8 GPUs, up to 96 vCPUs and 624 GB.
    ("nvidia-tesla-v100", 8, 96, 624),
];

/// What an accelerator type name comes to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Translated {
    /// A name of the table: the canonical model of `js-caps`.
    Known(Accelerator),
    /// A name no row knows, passed through as its own spelling.
    Unknown(Accelerator),
    /// A name with no letter or digit to spell a model with.
    Unnamed,
}

/// The pass-through spelling of an accelerator type name: its words, split at every
/// character that is not an ASCII letter or digit, in lower case, joined by `-`.
pub(crate) fn pass_through(name: &AcceleratorTypeName) -> String {
    name.as_str()
        .split(|c: char| !c.is_ascii_alphanumeric())
        .filter(|word| !word.is_empty())
        .map(str::to_ascii_lowercase)
        .collect::<Vec<_>>()
        .join("-")
}

/// The table's row for `name`, if one matches it exactly.
fn row_for(name: &AcceleratorTypeName) -> Option<(&'static str, u64)> {
    ACCELERATOR_ROWS
        .iter()
        .find(|(source, _, _)| *source == name.as_str())
        .map(|(_, spelling, memory_gib)| (*spelling, *memory_gib))
}

/// `count` accelerators of the type `name`, resolved through the `js-caps` alias table: a
/// row's spelling with the row's memory per GPU, or the pass-through spelling with no
/// memory (the API states none).
pub(crate) fn translate(name: &AcceleratorTypeName, count: u32) -> Translated {
    // Every row is a spelling `Model::parse` takes (the table test pins it).
    let known = row_for(name).and_then(|(spelling, memory_gib)| {
        Model::parse(spelling)
            .ok()
            .map(|model| (model, memory_gib * GIB))
    });
    match (known, Model::parse(&pass_through(name))) {
        (Some((model, memory)), _) => {
            Translated::Known(accelerator_of(&model, count, Some(memory)))
        }
        (None, Ok(model)) => Translated::Unknown(accelerator_of(&model, count, None)),
        (None, Err(_)) => Translated::Unnamed,
    }
}

/// The numbers of `accelerator` that a machine type `machine` of `vcpus` vCPUs and
/// `memory_mib` MiB may have attached, at most `max_per_instance` (the zone's
/// `maximumCardsPerInstance`), in rising order: none for a type that takes no attached
/// accelerator, or for an accelerator the attachment table does not list.
pub(crate) fn attachable_counts(
    machine: &MachineTypeName,
    accelerator: &AcceleratorTypeName,
    max_per_instance: u32,
    vcpus: u32,
    memory_mib: u64,
) -> Vec<u32> {
    let attaching = machine.as_str().starts_with(ATTACHING);
    ATTACH_ROWS
        .iter()
        .filter(|(name, count, max_vcpus, max_gb)| {
            [
                attaching,
                *name == accelerator.as_str(),
                *count <= max_per_instance,
                vcpus <= *max_vcpus,
                memory_mib <= max_gb * 1024,
            ]
            .into_iter()
            .all(|fits| fits)
        })
        .map(|(_, count, _, _)| *count)
        .collect()
}

#[cfg(test)]
mod tests {
    use js_caps::FAMILIES;

    use super::*;

    fn name(text: &str) -> AcceleratorTypeName {
        AcceleratorTypeName(text.to_owned())
    }

    /// Asserts that every row of the table emits a canonical name `js-caps` defines: its
    /// spelling resolves, through `js-caps`'s own `Model::canonical`, to a name the alias
    /// table knows (not an unknown name passed through) of a family listed in `js-caps`'s
    /// `FAMILIES`. Catches a name typed differently from `js-caps`, or a row added for a
    /// model `js-caps` does not define.
    #[test]
    fn every_emitted_canonical_name_is_in_the_alias_table() {
        for (source, spelling, _) in ACCELERATOR_ROWS {
            let resolved = Model::parse(spelling).unwrap().canonical();
            assert!(
                resolved.known,
                "{source}: {spelling} is not in the alias table"
            );
            let family = resolved.model.as_str();
            assert!(
                FAMILIES.iter().any(|row| row.name == family),
                "{source}: {spelling} resolves to {family}, which js-caps does not define"
            );
        }
    }

    /// Asserts, for every row of the table, driven from the table itself, that the row's
    /// source name translates to the canonical family `js-caps` gives the row's spelling,
    /// with that variant's facts where the spelling is a variant (memory per device and
    /// form factor) and the row's memory otherwise, the vendor `nvidia` and the count; and
    /// that a machine type built with that accelerator has an offer stating the same
    /// accelerator and no pass-through name. The two A100 rows give one family with 40
    /// and 80 GiB, and the two H100 rows one variant. Catches a row that maps to another
    /// model, a lost count or memory, or a lost variant.
    #[test]
    fn accelerator_names_translate_to_canonical_models() {
        for (source, spelling, memory_gib) in ACCELERATOR_ROWS {
            let model = Model::parse(spelling).unwrap();
            let resolved = model.canonical();
            let expected = accelerator_of(&model, 4, Some(memory_gib * GIB));
            assert_eq!(
                translate(&name(source), 4),
                Translated::Known(expected.clone()),
                "{source}"
            );
            assert_eq!(expected.model, resolved.model, "{source}");
            assert_eq!(expected.vendor.as_str(), "nvidia", "{source}");
            assert_eq!(expected.count, 4, "{source}");
            assert_eq!(
                expected.memory_per_device,
                Some(memory_gib * GIB),
                "{source}"
            );
            let form_factor = resolved.variant.map(|variant| variant.form_factor);
            assert_eq!(expected.form_factor, form_factor, "{source}");
            let variant_memory = resolved.variant.map(|variant| variant.memory_per_device);
            assert!(
                variant_memory.is_none_or(|bytes| bytes == memory_gib * GIB),
                "{source}"
            );
            let info = crate::model::MachineTypeInfo {
                name: MachineTypeName("q9-sample-4g".to_owned()),
                zone: crate::model::Zone::new("zone-one-a").unwrap(),
                vcpus: 48,
                memory_mib: 1 << 20,
                accelerators: Vec::new(),
                arch: Some(crate::model::ArchName("X86_64".to_owned())),
            };
            let attached = crate::model::AttachedAccelerator {
                type_name: name(source),
                count: 4,
            };
            let machine = crate::translate::machine(&info, Some(&attached)).unwrap();
            assert_eq!(machine.offer.accelerator, Some(expected), "{source}");
            assert_eq!(machine.unknown_accelerator, None, "{source}");
        }
        let accelerator = |source: &str| match translate(&name(source), 8) {
            Translated::Known(accelerator) => Some(accelerator),
            Translated::Unknown(_) | Translated::Unnamed => None,
        };
        let small = accelerator("nvidia-tesla-a100").unwrap();
        let large = accelerator("nvidia-a100-80gb").unwrap();
        assert_eq!(small.model.as_str(), "nvidia-a100");
        assert_eq!(large.model, small.model);
        assert_eq!(small.memory_per_device, Some(40 * GIB));
        assert_eq!(large.memory_per_device, Some(80 * GIB));
        let high = accelerator("nvidia-h100-80gb").unwrap();
        let mega = accelerator("nvidia-h100-mega-80gb").unwrap();
        assert_eq!(high, mega);
        assert_eq!(high.model.as_str(), "nvidia-h100");
        assert_eq!(high.variant.unwrap().as_str(), "nvidia-h100-80gb-hbm3");
        assert_eq!(accelerator("nvidia-l4"), None);
    }

    /// Asserts the pass-through spelling: words split at anything that is not a letter or
    /// digit, lower case, joined by single `-`; and that a name with no word is unnamed
    /// (not a panic).
    #[test]
    fn the_pass_through_spelling_is_the_lower_case_dash_form() {
        assert_eq!(pass_through(&name("nvidia-tesla-t4")), "nvidia-tesla-t4");
        assert_eq!(pass_through(&name("NVIDIA_L4  vws")), "nvidia-l4-vws");
        assert_eq!(pass_through(&name("-nvidia--gb300-")), "nvidia-gb300");
        assert_eq!(translate(&name("--"), 1), Translated::Unnamed);
        assert_eq!(translate(&name(""), 1), Translated::Unnamed);
    }

    /// Asserts which numbers of an accelerator an N1 machine type may have attached: the
    /// table's numbers for that accelerator, cut by the zone's maximum per instance, the
    /// machine's vCPUs and its memory, and none for a type that is not N1, for an
    /// accelerator the table does not list, or where the zone allows none. Catches a vCPU or memory
    /// bound read the wrong way, or attachments offered to a type that takes none.
    #[test]
    fn n1_types_take_the_documented_numbers_of_each_accelerator() {
        let counts = |machine: &str, accelerator: &str, max, vcpus, memory_mib| {
            attachable_counts(
                &MachineTypeName(machine.to_owned()),
                &name(accelerator),
                max,
                vcpus,
                memory_mib,
            )
        };
        let t4 = "nvidia-tesla-t4";
        let v100 = "nvidia-tesla-v100";
        assert_eq!(counts("n1-standard-8", t4, 4, 8, 30 * 1024), [1, 2, 4]);
        assert_eq!(counts("n1-standard-8", t4, 2, 8, 30 * 1024), [1, 2]);
        assert_eq!(counts("n1-standard-64", t4, 4, 64, 240 * 1024), [4]);
        assert_eq!(counts("n1-highmem-32", t4, 4, 32, 400 * 1024), [4]);
        assert_eq!(counts("n1-standard-16", v100, 8, 16, 60 * 1024), [2, 4, 8]);
        assert_eq!(counts("n1-standard-96", v100, 8, 96, 360 * 1024), [8]);
        assert_eq!(
            counts("n1-standard-4", "nvidia-tesla-p4", 4, 4, 15 * 1024),
            [1, 2, 4]
        );
        assert!(counts("n2-standard-8", t4, 4, 8, 32 * 1024).is_empty());
        assert!(counts("n1-standard-8", "nvidia-l4", 8, 8, 30 * 1024).is_empty());
        assert!(counts("n1-standard-8", t4, 0, 8, 30 * 1024).is_empty());
    }
}
