//! The translation of EC2's GPU names into the accelerator vocabulary. A table in code maps
//! this cloud's source spelling (`GpuInfo` `Manufacturer` and `Name`, with the memory per
//! GPU to pick the variant where one family has several) to a variant spelling that
//! `js-caps`'s alias table defines, which resolves to the canonical model and its variant
//! facts. This crate defines no canonical name. A spelling the table does not know passes
//! through as its own spelling (`manufacturer-name`, lower case, words joined by `-`), so a
//! request that names exactly that spelling still matches it.

use js_caps::{Accelerator, Model, accelerator_of};

use crate::model::GpuInfo;

/// Bytes in a MiB.
const MIB: u64 = 1 << 20;

/// The table: `Manufacturer`, `Name`, `MemoryInfo.SizeInMiB` per GPU, and the `js-caps`
/// variant spelling it is. A row is here only when the spelling was read from AWS's own
/// sources; every other GPU passes through (the crate docs list the families).
pub(crate) const GPU_ROWS: [(&str, &str, u64, &str); 3] = [
    // p4d.24xlarge: "8 x NVIDIA A100 GPU", "8 x 40 GiB" (EC2 instance types guide,
    // accelerated computing specifications); `DescribeInstanceTypes` answers `NVIDIA`,
    // `A100`, 40960 MiB (the answer quoted in aws/aws-cli issue 8970). SXM4 modules.
    ("NVIDIA", "A100", 40 * 1024, "nvidia-a100-sxm4-40gb"),
    // p4de.24xlarge: "8 x NVIDIA A100 GPU", "8 x 80 GiB" (EC2 instance types guide,
    // accelerated computing specifications): the same GPU name with 80 GiB, in MiB.
    ("NVIDIA", "A100", 80 * 1024, "nvidia-a100-sxm4-80gb"),
    // p5.48xlarge and p5.4xlarge: "NVIDIA H100 GPU", "80 GiB" each (EC2 instance types
    // guide, accelerated computing specifications); `DescribeInstanceTypes` answers
    // `NVIDIA`, `H100`, 81920 MiB (aws/aws-cli issue 8970). SXM5 modules with HBM3.
    ("NVIDIA", "H100", 80 * 1024, "nvidia-h100-80gb-hbm3"),
];

/// What a type's GPU information comes to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Gpus {
    /// The type has no GPU.
    None,
    /// A spelling of the table: the canonical model of `js-caps`.
    Known(Accelerator),
    /// A spelling no row knows, passed through as its own spelling.
    Unknown(Accelerator),
    /// A GPU whose name has no letter or digit to spell a model with.
    Unnamed,
}

/// The lower-case ASCII words of `text`, split at every character that is not a letter or
/// a digit.
fn words(text: &str) -> Vec<String> {
    text.split(|c: char| !c.is_ascii_alphanumeric())
        .filter(|word| !word.is_empty())
        .map(str::to_ascii_lowercase)
        .collect()
}

/// The pass-through spelling of a GPU: the manufacturer's words, then the name's, joined
/// by `-`; a name that already begins with the manufacturer's words is not given them
/// twice.
pub(crate) fn pass_through(manufacturer: &str, name: &str) -> String {
    let vendor = words(manufacturer);
    let named = words(name);
    let spelled = match named.starts_with(&vendor) {
        true => named,
        false => vendor.into_iter().chain(named).collect(),
    };
    spelled.join("-")
}

/// The table's spelling for `gpu`, if a row matches it: the manufacturer and the name
/// (compared without regard to case) and the memory per GPU, exactly.
fn row_for(gpu: &GpuInfo) -> Option<&'static str> {
    GPU_ROWS
        .iter()
        .find(|(manufacturer, name, memory_mib, _)| {
            [
                gpu.manufacturer.eq_ignore_ascii_case(manufacturer),
                gpu.name.eq_ignore_ascii_case(name),
                gpu.memory_mib == *memory_mib,
            ]
            .into_iter()
            .all(|same| same)
        })
        .map(|(_, _, _, spelling)| *spelling)
}

/// The accelerators of a type from its `GpuInfo.Gpus`: the first entry's GPUs (an EC2 type
/// has one GPU model; a second entry would be another model, which one accelerator cannot
/// state), resolved through the `js-caps` alias table, with the memory per GPU the cloud
/// states as the measured memory.
pub(crate) fn translate(gpus: &[GpuInfo]) -> Gpus {
    let Some(gpu) = gpus.first() else {
        return Gpus::None;
    };
    let measured = Some(gpu.memory_mib.saturating_mul(MIB));
    // Every row is a spelling `Model::parse` takes (the table test pins it).
    let known = row_for(gpu).and_then(|spelling| Model::parse(spelling).ok());
    match (
        known,
        Model::parse(&pass_through(&gpu.manufacturer, &gpu.name)),
    ) {
        (Some(model), _) => Gpus::Known(accelerator_of(&model, gpu.count, measured)),
        (None, Ok(model)) => Gpus::Unknown(accelerator_of(&model, gpu.count, measured)),
        (None, Err(_)) => Gpus::Unnamed,
    }
}

#[cfg(test)]
mod tests {
    use js_caps::{FAMILIES, FormFactor};

    use super::*;
    use crate::model::{ArchName, InstanceType, InstanceTypeInfo};

    fn gpu(manufacturer: &str, name: &str, count: u32, memory_mib: u64) -> GpuInfo {
        GpuInfo {
            name: name.to_owned(),
            manufacturer: manufacturer.to_owned(),
            count,
            memory_mib,
        }
    }

    /// Asserts that every row of the table emits a canonical name `js-caps` defines: its
    /// spelling resolves, through `js-caps`'s own `Model::canonical`, to a variant the
    /// alias table knows (not an unknown name passed through) of a family listed in
    /// `js-caps`'s `FAMILIES`. Catches a name typed differently from `js-caps`, or a row
    /// added for a model `js-caps` does not define.
    #[test]
    fn every_emitted_canonical_name_is_in_the_alias_table() {
        for (_, _, _, spelling) in GPU_ROWS {
            let resolved = Model::parse(spelling).unwrap().canonical();
            assert!(resolved.known, "{spelling} is not in the alias table");
            assert!(resolved.variant.is_some(), "{spelling} names no variant");
            let family = resolved.model.as_str();
            assert!(
                FAMILIES.iter().any(|row| row.name == family),
                "{spelling} resolves to {family}, which js-caps does not define"
            );
        }
    }

    /// Asserts, for every row of the table, that the row's own source spelling translates
    /// to the canonical family `js-caps` gives its spelling, with that variant's facts
    /// (memory per device and form factor), the vendor `nvidia` and the count, and that an
    /// instance type built with that GPU has an offer stating the same accelerator. The
    /// two A100 rows give one family and two variants, 40 and 80 GiB. Catches a row that
    /// maps to another model, a lost count or memory, or a lost variant.
    #[test]
    fn gpu_names_translate_to_canonical_models() {
        for (manufacturer, name, memory_mib, spelling) in GPU_ROWS {
            let resolved = Model::parse(spelling).unwrap().canonical();
            let variant = resolved.variant.unwrap();
            assert_eq!(variant.memory_per_device, memory_mib * MIB, "{spelling}");
            let source = gpu(manufacturer, name, 4, memory_mib);
            let Gpus::Known(accelerator) = translate(std::slice::from_ref(&source)) else {
                panic!("{spelling} is not translated through its row");
            };
            assert_eq!(accelerator.model, resolved.model);
            assert_eq!(accelerator.variant.as_ref().unwrap().as_str(), spelling);
            assert_eq!(accelerator.vendor.as_str(), "nvidia");
            assert_eq!(accelerator.count, 4);
            assert_eq!(accelerator.memory_per_device, Some(memory_mib * MIB));
            assert_eq!(accelerator.form_factor, Some(variant.form_factor));
            let info = InstanceTypeInfo {
                name: InstanceType("p9.sample".to_owned()),
                vcpus: 96,
                memory_mib: 1 << 20,
                architectures: vec![ArchName("x86_64".to_owned())],
                gpus: vec![source],
                instance_storage_gb: None,
                bare_metal: false,
            };
            let machine = crate::translate::machine(&info).unwrap();
            assert_eq!(machine.offer.accelerator, Some(accelerator));
            assert_eq!(machine.unknown_gpu, None);
        }
        let a100 = |memory_mib| match translate(&[gpu("nvidia", "a100", 8, memory_mib)]) {
            Gpus::Known(accelerator) => accelerator,
            other => panic!("{other:?}"),
        };
        let (small, large) = (a100(40960), a100(81920));
        assert_eq!(small.model.as_str(), "nvidia-a100");
        assert_eq!(large.model, small.model);
        assert_eq!(small.memory_per_device, Some(40 << 30));
        assert_eq!(large.memory_per_device, Some(80 << 30));
        assert_ne!(small.variant, large.variant);
        assert_eq!(small.form_factor, Some(FormFactor::Sxm));
    }

    /// Asserts that a GPU a row does not match on any one of its three keys passes
    /// through as its own spelling with the memory the cloud states: another
    /// manufacturer, another name, and an A100 of a memory no row has (which spells the
    /// family name itself, and so resolves to the family with no variant).
    #[test]
    fn a_gpu_that_misses_a_row_on_any_key_passes_through() {
        let unknown = |gpu: GpuInfo| match translate(&[gpu]) {
            Gpus::Unknown(accelerator) => accelerator,
            other => panic!("{other:?}"),
        };
        let other_maker = unknown(gpu("Habana", "A100", 1, 40960));
        assert_eq!(other_maker.model.as_str(), "habana-a100");
        let other_name = unknown(gpu("NVIDIA", "L40S", 4, 45776));
        assert_eq!(other_name.model.as_str(), "nvidia-l40s");
        assert_eq!(other_name.memory_per_device, Some(45776 * MIB));
        let other_size = unknown(gpu("NVIDIA", "A100", 8, 40536));
        assert_eq!(other_size.model.as_str(), "nvidia-a100");
        assert_eq!(other_size.variant, None);
        assert_eq!(other_size.memory_per_device, Some(40536 * MIB));
    }

    /// Asserts the pass-through spelling: words split at anything that is not a letter or
    /// digit, lower case, joined by single `-`, the manufacturer first unless the name
    /// already begins with it, and no manufacturer when it has no word; a type with no GPU
    /// has none, and a GPU whose name and maker have no word is unnamed (not a panic).
    /// The first entry of the list is the one read.
    #[test]
    fn the_pass_through_spelling_is_the_lower_case_dash_form() {
        assert_eq!(pass_through("NVIDIA", "GB300"), "nvidia-gb300");
        assert_eq!(pass_through("NVIDIA", "NVIDIA GB300"), "nvidia-gb300");
        assert_eq!(
            pass_through("AMD", "Radeon Pro V520"),
            "amd-radeon-pro-v520"
        );
        assert_eq!(
            pass_through("Habana", "Gaudi HL-205"),
            "habana-gaudi-hl-205"
        );
        assert_eq!(pass_through("", "T4g"), "t4g");
        assert_eq!(
            pass_through("NVIDIA", "RTX  PRO--6000"),
            "nvidia-rtx-pro-6000"
        );
        assert_eq!(translate(&[]), Gpus::None);
        assert_eq!(translate(&[gpu("—", "", 1, 1)]), Gpus::Unnamed);
        let first = translate(&[gpu("NVIDIA", "T4", 1, 16384), gpu("NVIDIA", "M60", 2, 8192)]);
        let Gpus::Unknown(accelerator) = first else {
            panic!("{first:?}");
        };
        assert_eq!(accelerator.model.as_str(), "nvidia-t4");
        assert_eq!(accelerator.count, 1);
    }
}
