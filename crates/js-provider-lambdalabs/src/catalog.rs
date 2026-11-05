//! The GPU facts of Lambda's instance types, in code: which accelerator each type brings,
//! keyed by the instance-type name Lambda's API gives (`gpu_1x_a10`, `gpu_8x_h100_sxm5`).
//!
//! The table translates Lambda's spellings, the GPU part of each instance-type name and the
//! GPU its description names, to the accelerator vocabulary of `js-caps`: the canonical
//! family (a constant of the alias table, never a name spelt here), the number of devices,
//! the memory of one device, the form factor where Lambda's name or description states one,
//! and the NVLink domain of an eight-GPU SXM board. Where the alias table knows the variant
//! (the family, memory and form factor of an `nvidia-smi` spelling), the accelerator carries
//! that spelling and the table's facts. Lambda's free-text `gpu_description` is never
//! parsed.
//!
//! Configuration only selects among these types (the `[[option]]` allow list and the
//! `lambdalabs` kind's `types`); it no longer states a GPU. A type the table does not know
//! is never offered; it is added here, in a release. The API's GPU count is checked against
//! the table's on every read of the catalog, and a type on which they disagree is not
//! offered (the table is stale for it).
//!
//! Sources: Lambda's published instance-type list (names, GPU, count and memory per GPU)
//! and NVIDIA's data sheets (memory and form factor). Lambda's `gpu_1x_rtx6000` (a Quadro
//! RTX 6000) and `gpu_1x_gh200` (a Grace Hopper superchip, whose `nvidia-smi` spelling and
//! form are not in the alias table) are not rows yet.

use js_types::{
    ALIASES, Accelerator, FormFactor, Interconnect, Model, NVIDIA_A10, NVIDIA_A100, NVIDIA_B200,
    NVIDIA_H100, NVIDIA_RTX_A6000, accelerator_of,
};

/// Bytes in a GiB.
const GIB: u64 = 1 << 30;

/// The interconnect kind of devices that share one NVLink switch fabric.
const NVLINK_DOMAIN: &str = "nvlink-domain";

/// The GPU facts of one Lambda instance type.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Entry {
    /// Lambda's name for the type.
    pub(crate) instance_type: &'static str,
    /// The canonical family, a constant of the alias table.
    pub(crate) family: &'static str,
    /// How many devices.
    pub(crate) count: u32,
    /// The memory of one device, in whole GiB, as Lambda and the data sheet state it.
    pub(crate) memory_gib: u64,
    /// How the devices are built in; `None` where Lambda's name and description do not
    /// say.
    pub(crate) form_factor: Option<FormFactor>,
    /// The number of devices on one NVLink switch fabric; `None` where the type has none
    /// that Lambda documents.
    pub(crate) nvlink_domain: Option<u32>,
}

/// One row, as a struct literal: a macro rather than a `const fn`, because the table is
/// built at compile time and a function only called there would never run under test.
macro_rules! row {
    (
        $instance_type:literal,
        $family:expr,
        $count:literal,
        $memory_gib:literal,
        $form_factor:expr,
        $nvlink_domain:expr $(,)?
    ) => {
        Entry {
            instance_type: $instance_type,
            family: $family,
            count: $count,
            memory_gib: $memory_gib,
            form_factor: $form_factor,
            nvlink_domain: $nvlink_domain,
        }
    };
}

const PCIE: Option<FormFactor> = Some(FormFactor::Pcie);
const SXM: Option<FormFactor> = Some(FormFactor::Sxm);

/// Every Lambda instance type the provider can offer, sorted by name.
pub(crate) const TYPES: &[Entry] = &[
    // "1x A10 (24 GB PCIe)".
    row!("gpu_1x_a10", NVIDIA_A10, 1, 24, PCIE, None),
    // "1x A100 (40 GB PCIe)".
    row!("gpu_1x_a100", NVIDIA_A100, 1, 40, PCIE, None),
    // "1x A100 (40 GB SXM4)".
    row!("gpu_1x_a100_sxm4", NVIDIA_A100, 1, 40, SXM, None),
    // "1x A6000 (48 GB)": the RTX A6000, a PCIe card.
    row!("gpu_1x_a6000", NVIDIA_RTX_A6000, 1, 48, PCIE, None),
    // "1x B200 (180 GB SXM6)".
    row!("gpu_1x_b200_sxm6", NVIDIA_B200, 1, 180, SXM, None),
    // "1x H100 (80 GB PCIe)".
    row!("gpu_1x_h100_pcie", NVIDIA_H100, 1, 80, PCIE, None),
    // "1x H100 (80 GB SXM5)".
    row!("gpu_1x_h100_sxm5", NVIDIA_H100, 1, 80, SXM, None),
    // "2x A100 (40 GB)": the form is not one Lambda's name states, so it is left unknown.
    row!("gpu_2x_a100", NVIDIA_A100, 2, 40, None, None),
    // "2x B200 (180 GB SXM6)".
    row!("gpu_2x_b200_sxm6", NVIDIA_B200, 2, 180, SXM, None),
    // "2x H100 (80 GB SXM5)".
    row!("gpu_2x_h100_sxm5", NVIDIA_H100, 2, 80, SXM, None),
    // "4x A100 (40 GB)": as the two-GPU type, the form is left unknown.
    row!("gpu_4x_a100", NVIDIA_A100, 4, 40, None, None),
    // "4x H100 (80 GB SXM5)".
    row!("gpu_4x_h100_sxm5", NVIDIA_H100, 4, 80, SXM, None),
    // "8x A100 (40 GB SXM4)": a whole HGX board, eight GPUs on one NVSwitch fabric.
    row!("gpu_8x_a100", NVIDIA_A100, 8, 40, SXM, Some(8)),
    // "8x A100 (80 GB SXM4)".
    row!("gpu_8x_a100_80gb_sxm4", NVIDIA_A100, 8, 80, SXM, Some(8)),
    // "8x B200 (180 GB SXM6)".
    row!("gpu_8x_b200_sxm6", NVIDIA_B200, 8, 180, SXM, Some(8)),
    // "8x H100 (80 GB SXM5)".
    row!("gpu_8x_h100_sxm5", NVIDIA_H100, 8, 80, SXM, Some(8)),
];

/// The row of the instance type `name`, if the table has one.
pub(crate) fn entry(name: &str) -> Option<&'static Entry> {
    TYPES
        .binary_search_by(|row| row.instance_type.cmp(name))
        .ok()
        .map(|at| &TYPES[at])
}

impl Entry {
    /// The spelling the accelerator is built from: the alias table's spelling of the
    /// variant with this family, memory and form factor when it has one, else the family.
    pub(crate) fn spelling(&self) -> &'static str {
        ALIASES
            .iter()
            .find(|alias| {
                (alias.family, alias.memory_gib, Some(alias.form_factor))
                    == (self.family, self.memory_gib, self.form_factor)
            })
            .map_or(self.family, |alias| alias.spelling)
    }

    /// The accelerator of the type: resolved through the alias table from
    /// [`Entry::spelling`], with this row's memory, form factor and interconnect where the
    /// table knows no variant. `None` only if a row's spelling were not a model name, which
    /// the crate's tests rule out for every row.
    pub(crate) fn accelerator(&self) -> Option<Accelerator> {
        Model::parse(self.spelling()).ok().map(|model| {
            let mut accelerator = accelerator_of(&model, self.count, Some(self.memory_gib * GIB));
            accelerator.form_factor = accelerator.form_factor.or(self.form_factor);
            accelerator.interconnect = self
                .nvlink_domain
                .and_then(|size| Interconnect::new(NVLINK_DOMAIN, size).ok());
            accelerator
        })
    }
}

#[cfg(test)]
#[path = "catalog_tests.rs"]
mod tests;
