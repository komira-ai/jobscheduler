//! The accelerator vocabulary a request, an offer and a hello share (design 3.8): what a
//! host or an offer has ([`Accelerator`]), what a request asks for ([`AcceleratorNeed`]),
//! and the one comparison between them ([`AcceleratorNeed::met_by`]).
//!
//! The alias table is here too ([`FAMILIES`], [`ALIASES`]): it maps every spelling
//! (`nvidia-a100-sxm4-80gb`) to its canonical family and its variant's facts
//! ([`Model::canonical`]), and it is the only place a canonical name is defined. `js-caps`
//! re-exports all of it unchanged; it lives in this crate because `Shape::satisfies` resolves
//! a host's class through it and this is the lowest layer. An [`Accelerator`] is built
//! resolved, by [`accelerator_of`]; [`AcceleratorNeed::met_by`] resolves the need's model
//! itself, so a family matches every variant and a full spelling pins its own.
//!
//! A request carries its need as capability keys ([`AcceleratorNeed::from_capabilities`],
//! [`AcceleratorNeed::to_capabilities`]), as it carries `disk-gib`, `exec` and `isa_level`.

use std::fmt;
use std::str::FromStr;

use crate::{Capabilities, Resources};

/// The capability key of the vendor a request asks for.
pub const ACCEL_VENDOR_KEY: &str = "accelerator.vendor";
/// The capability key of the model a request asks for: a family or a full spelling.
pub const ACCEL_MODEL_KEY: &str = "accelerator.model";
/// The capability key of the least number of devices a request asks for.
pub const ACCEL_COUNT_KEY: &str = "accelerator.count";
/// The capability key of the least memory per device a request asks for, in whole GiB.
pub const ACCEL_MEMORY_GIB_KEY: &str = "accelerator.memory-gib";
/// The capability key of the interconnect a request asks for, as `kind>=size`.
pub const ACCEL_INTERCONNECT_KEY: &str = "accelerator.interconnect";
/// The capability key of the form factor a request asks for.
pub const ACCEL_FORM_FACTOR_KEY: &str = "accelerator.form-factor";
/// Every accelerator capability key, in the order [`AcceleratorNeed::to_capabilities`]
/// writes them.
pub const ACCEL_KEYS: [&str; 6] = [
    ACCEL_VENDOR_KEY,
    ACCEL_MODEL_KEY,
    ACCEL_COUNT_KEY,
    ACCEL_MEMORY_GIB_KEY,
    ACCEL_INTERCONNECT_KEY,
    ACCEL_FORM_FACTOR_KEY,
];

/// Bytes in a GiB.
const GIB: u64 = 1 << 30;
/// The largest `accelerator.memory-gib`: the most whole GiB whose bytes fit 64 bits
/// (2^34 - 1).
pub const MAX_MEMORY_GIB: u64 = u64::MAX / GIB;
/// The longest vendor name.
const VENDOR_MAX: usize = 32;
/// The longest interconnect kind.
const KIND_MAX: usize = 32;

/// Why text is not an accelerator value.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum AccelError {
    /// A vendor is not 1 to 32 of `a-z`, `0-9` and `-`.
    #[error("vendor must be 1 to 32 of a-z, 0-9 and '-', not {0:?}")]
    BadVendor(String),
    /// A model is not lower-case letters and digits joined by single `-`.
    #[error("model must be lower case letters and digits joined by single '-', not {0:?}")]
    BadModel(String),
    /// An interconnect is not `kind>=size`.
    #[error("interconnect must be kind>=size, such as nvlink-domain>=72, not {0:?}")]
    BadInterconnect(String),
    /// A form factor is not `sxm` or `pcie`.
    #[error("form factor must be sxm or pcie, not {0:?}")]
    BadFormFactor(String),
    /// A request's `accelerator.model` contradicts the GPU class it books: another family,
    /// or another variant of the same family.
    #[error("accelerator.model {stated:?} contradicts the booked gpu_class {booked:?}")]
    ModelConflict {
        /// The model the key names.
        stated: String,
        /// The class the resources book.
        booked: String,
    },
    /// A count or a memory size is not a whole number in range.
    #[error("{key} must be a whole number, not {value:?}")]
    BadNumber {
        /// The capability key.
        key: &'static str,
        /// The text it held.
        value: String,
    },
}

/// Whether `text` is lower-case letters and digits joined by single `-`, with no `-` at
/// either end.
fn dashed_words(text: &str) -> bool {
    text.split('-').all(|word| {
        !word.is_empty()
            && word
                .bytes()
                .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit())
    })
}

/// Who makes an accelerator: a lower-case token (`[a-z0-9-]{1,32}`). The set is open: a
/// vendor no release knows is valid and matches only itself.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Vendor(String);

impl Vendor {
    /// NVIDIA.
    pub const NVIDIA: &'static str = "nvidia";
    /// Apple.
    pub const APPLE: &'static str = "apple";
    /// The vendor of a spelling that names none.
    pub const UNKNOWN: &'static str = "unknown";

    /// The vendor `text`.
    ///
    /// # Errors
    ///
    /// [`AccelError::BadVendor`] when `text` is not 1 to 32 of `a-z`, `0-9` and `-`.
    pub fn parse(text: &str) -> Result<Self, AccelError> {
        let token = |b: u8| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-';
        match (1..=VENDOR_MAX).contains(&text.len()) && text.bytes().all(token) {
            true => Ok(Self(text.to_owned())),
            false => Err(AccelError::BadVendor(text.to_owned())),
        }
    }

    /// The vendor as text.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for Vendor {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// An accelerator model name: lower-case letters and digits joined by single `-`, of any
/// length (a GPU class the previous release took has no length bound, so neither does a
/// model; a wire that carries one bounds its own text). It is a family (`nvidia-a100`) or a full spelling (`nvidia-a100-sxm4-80gb`)
/// until the alias table resolves it; a name the table does not know stays as it is.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Model(String);

impl Model {
    /// The model `text`.
    ///
    /// # Errors
    ///
    /// [`AccelError::BadModel`] when `text` is not lower-case words joined by single `-`.
    pub fn parse(text: &str) -> Result<Self, AccelError> {
        match dashed_words(text) {
            true => Ok(Self(text.to_owned())),
            false => Err(AccelError::BadModel(text.to_owned())),
        }
    }

    /// The model as text.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for Model {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// How a device is built into its host: the variant fact the alias table carries beside
/// memory.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum FormFactor {
    /// A socketed module (`sxm`), the form NVLink boards use.
    Sxm,
    /// A PCI Express card (`pcie`).
    Pcie,
}

impl FormFactor {
    /// The form factor's name in request syntax.
    #[must_use]
    pub fn name(self) -> &'static str {
        match self {
            Self::Sxm => "sxm",
            Self::Pcie => "pcie",
        }
    }
}

impl fmt::Display for FormFactor {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.name())
    }
}

impl FromStr for FormFactor {
    type Err = AccelError;

    fn from_str(text: &str) -> Result<Self, Self::Err> {
        match text {
            "sxm" => Ok(Self::Sxm),
            "pcie" => Ok(Self::Pcie),
            other => Err(AccelError::BadFormFactor(other.to_owned())),
        }
    }
}

/// How devices are linked: a kind and the size of the domain they share, written
/// `kind>=size` (`nvlink-domain>=72`). No kind is enumerated: a kind no release knows
/// matches only an identical kind.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Interconnect {
    kind: String,
    size: u32,
}

impl Interconnect {
    /// The interconnect of `kind` with a domain of `size` devices.
    ///
    /// # Errors
    ///
    /// [`AccelError::BadInterconnect`] when `kind` is not lower-case words joined by
    /// single `-`, at most 32 long.
    pub fn new(kind: &str, size: u32) -> Result<Self, AccelError> {
        match kind.len() <= KIND_MAX && dashed_words(kind) {
            true => Ok(Self {
                kind: kind.to_owned(),
                size,
            }),
            false => Err(AccelError::BadInterconnect(format!("{kind}>={size}"))),
        }
    }

    /// The interconnect written `kind>=size`.
    ///
    /// # Errors
    ///
    /// [`AccelError::BadInterconnect`] when `text` is not a kind, `>=` and a whole number.
    pub fn parse(text: &str) -> Result<Self, AccelError> {
        let bad = || AccelError::BadInterconnect(text.to_owned());
        let (kind, size) = text.split_once(">=").ok_or_else(bad)?;
        let size = size.parse::<u32>().map_err(|_| bad())?;
        Self::new(kind, size).map_err(|_| bad())
    }

    /// The kind of link.
    #[must_use]
    pub fn kind(&self) -> &str {
        &self.kind
    }

    /// How many devices share one domain.
    #[must_use]
    pub fn size(&self) -> u32 {
        self.size
    }

    /// Whether this interconnect serves `want`: the same kind, and a domain at least as
    /// large.
    #[must_use]
    pub fn satisfies(&self, want: &Interconnect) -> bool {
        self.kind == want.kind && self.size >= want.size
    }
}

impl fmt::Display for Interconnect {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}>={}", self.kind, self.size)
    }
}

/// The accelerators a host or an offer has, resolved: one model, a count of devices of it
/// and the facts of that model's variant.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Accelerator {
    /// Who makes it.
    pub vendor: Vendor,
    /// The canonical family (`nvidia-a100`), or the name as read when the alias table does
    /// not know it.
    pub model: Model,
    /// The full spelling the source gave (`nvidia-a100-sxm4-80gb`), when it is not the
    /// family name itself.
    pub variant: Option<Model>,
    /// How many devices.
    pub count: u32,
    /// The memory of one device, in bytes: the variant's nominal memory when the alias
    /// table knows the variant, else what was measured. `None`: not known.
    pub memory_per_device: Option<u64>,
    /// How the devices are built in, when the variant says. `None`: not known.
    pub form_factor: Option<FormFactor>,
    /// How the devices are linked. `None`: not known.
    pub interconnect: Option<Interconnect>,
}

/// The first part of an [`AcceleratorNeed`] that an [`Accelerator`] fails, in the order
/// [`AcceleratorNeed::met_by`] checks them.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum AcceleratorMiss {
    /// Another vendor, or no accelerator.
    Vendor,
    /// Another model, or no accelerator.
    Model,
    /// Fewer devices.
    Count,
    /// Less memory per device, or memory not known.
    Memory,
    /// Another interconnect kind, a smaller domain, or none known.
    Interconnect,
    /// Another form factor, or none known.
    FormFactor,
}

/// What a request asks of the accelerators of its host. A field that is `None` matches
/// anything; `count` is at least, and zero asks for none.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct AcceleratorNeed {
    /// The vendor. `None`: any.
    pub vendor: Option<Vendor>,
    /// The model: a family, or a full spelling that the alias table resolves to its family
    /// and pins to its variant. `None`: any.
    pub model: Option<Model>,
    /// The least number of devices.
    pub count: u32,
    /// The least memory per device, in bytes. `None`: any.
    pub memory_per_device: Option<u64>,
    /// The least interconnect. `None`: any.
    pub interconnect: Option<Interconnect>,
    /// The form factor. `None`: any.
    pub form_factor: Option<FormFactor>,
}

impl AcceleratorNeed {
    /// Whether `have` (already resolved, as [`accelerator_of`] builds it) serves this need,
    /// comparing in a fixed order (the order is part of the contract: the first miss is
    /// reported to clients): vendor (equal), model (the need's model resolved through the
    /// alias table, equal by canonical family), count (at least), memory per device (at
    /// least; an unknown memory fails a need that fixes one), interconnect (same kind,
    /// domain at least), form factor (equal). A field the need leaves `None` is not
    /// compared, so a need with nothing set and a count of zero is met by any host, with or
    /// without accelerators.
    ///
    /// A need whose model is a full spelling pins that variant: the memory per device must
    /// be exactly the variant's (and at least any memory the need also asks for), and the
    /// form factor the variant's (a form factor the need also names must match too, so a
    /// need naming another is met by nothing), so it is met only by that variant of the
    /// family, never by a larger or smaller one.
    ///
    /// # Errors
    ///
    /// The first [`AcceleratorMiss`].
    pub fn met_by(&self, have: Option<&Accelerator>) -> Result<(), AcceleratorMiss> {
        let resolved = self.model.as_ref().map(Model::canonical);
        let family = resolved.as_ref().map(|resolved| &resolved.model);
        let pin = resolved.as_ref().and_then(|resolved| resolved.variant);
        let pinned_form_factor = pin.map(|pin| pin.form_factor);
        let vendor = have.map(|have| &have.vendor);
        let model = have.map(|have| &have.model);
        let count = have.map_or(0, |have| have.count);
        let memory = have.and_then(|have| have.memory_per_device);
        let interconnect = have.and_then(|have| have.interconnect.as_ref());
        let form_factor = have.and_then(|have| have.form_factor);
        let checks = [
            (
                AcceleratorMiss::Vendor,
                self.vendor.as_ref().is_none_or(|want| vendor == Some(want)),
            ),
            (
                AcceleratorMiss::Model,
                family.is_none_or(|want| model == Some(want)),
            ),
            (AcceleratorMiss::Count, self.count <= count),
            (
                AcceleratorMiss::Memory,
                [
                    self.memory_per_device
                        .is_none_or(|want| memory.is_some_and(|have| have >= want)),
                    pin.is_none_or(|pin| memory == Some(pin.memory_per_device)),
                ]
                .into_iter()
                .all(|met| met),
            ),
            (
                AcceleratorMiss::Interconnect,
                self.interconnect
                    .as_ref()
                    .is_none_or(|want| interconnect.is_some_and(|have| have.satisfies(want))),
            ),
            (
                AcceleratorMiss::FormFactor,
                [pinned_form_factor, self.form_factor]
                    .into_iter()
                    .flatten()
                    .all(|want| form_factor == Some(want)),
            ),
        ];
        match checks.into_iter().find(|(_, met)| !met) {
            Some((miss, _)) => Err(miss),
            None => Ok(()),
        }
    }

    /// The need a booked GPU count and class state: that many devices of that model, the
    /// class resolved through the alias table when the need is compared. `None` when no GPU
    /// is booked or the class is not a model name (it is then matched by the class alone).
    #[must_use]
    pub fn booked(resources: &Resources) -> Option<Self> {
        let model = Model::parse(resources.gpu_class()?).ok()?;
        Some(Self {
            model: Some(model),
            count: u32::try_from(resources.gpus()).unwrap_or(u32::MAX),
            ..Self::default()
        })
    }

    /// The need a request states: its `accelerator.*` keys over the need its booked GPUs
    /// state ([`Self::booked`]). A key narrows the booked need and never widens it: a model
    /// and a count the keys leave out are the booking's (so a booked full spelling keeps its
    /// variant pin beside `accelerator.vendor` alone); when both name a model, the narrower
    /// is kept (a booked full spelling over the family named by the key, and the reverse);
    /// and the count is the larger of the two (an `accelerator.count` of zero asks for no
    /// fewer than the booking).
    ///
    /// # Errors
    ///
    /// The [`AccelError`] of the first key whose value is not valid, or
    /// [`AccelError::ModelConflict`] when the key's model and the booked class are of
    /// different families, or are two different variants of one.
    pub fn requested(
        capabilities: &Capabilities,
        resources: &Resources,
    ) -> Result<Option<Self>, AccelError> {
        let stated = Self::from_capabilities(capabilities)?;
        Ok(match (stated, Self::booked(resources)) {
            (None, booked) => booked,
            (Some(stated), None) => Some(stated),
            (Some(stated), Some(booked)) => Some(Self {
                model: narrower(stated.model.clone(), booked.model)?,
                count: stated.count.max(booked.count),
                ..stated
            }),
        })
    }

    /// The need the `accelerator.*` keys of `capabilities` state, or `None` when they name
    /// none. `accelerator.memory-gib` is whole GiB, at most [`MAX_MEMORY_GIB`].
    ///
    /// # Errors
    ///
    /// The [`AccelError`] of the first key whose value is not valid.
    pub fn from_capabilities(capabilities: &Capabilities) -> Result<Option<Self>, AccelError> {
        if !ACCEL_KEYS.iter().any(|key| capabilities.contains_key(*key)) {
            return Ok(None);
        }
        let get = |key: &'static str| capabilities.get(key).map(String::as_str);
        let number = |key: &'static str, text: &str| {
            text.parse::<u32>().map_err(|_| AccelError::BadNumber {
                key,
                value: text.to_owned(),
            })
        };
        let memory = get(ACCEL_MEMORY_GIB_KEY)
            .map(|text| {
                let bytes = text
                    .parse::<u64>()
                    .ok()
                    .and_then(|gib| gib.checked_mul(GIB));
                bytes.ok_or_else(|| AccelError::BadNumber {
                    key: ACCEL_MEMORY_GIB_KEY,
                    value: text.to_owned(),
                })
            })
            .transpose()?;
        Ok(Some(Self {
            vendor: get(ACCEL_VENDOR_KEY).map(Vendor::parse).transpose()?,
            model: get(ACCEL_MODEL_KEY).map(Model::parse).transpose()?,
            count: get(ACCEL_COUNT_KEY)
                .map(|text| number(ACCEL_COUNT_KEY, text))
                .transpose()?
                .unwrap_or(0),
            memory_per_device: memory,
            interconnect: get(ACCEL_INTERCONNECT_KEY)
                .map(Interconnect::parse)
                .transpose()?,
            form_factor: get(ACCEL_FORM_FACTOR_KEY)
                .map(str::parse::<FormFactor>)
                .transpose()?,
        }))
    }

    /// The `accelerator.*` capability entries that state this need, in [`ACCEL_KEYS`]
    /// order, leaving out every field that is `None` and a count of zero. Memory is written
    /// in whole GiB, rounded up, so it never asks for less than the need. A memory above
    /// [`MAX_MEMORY_GIB`] GiB, which no key can state, is written as the GiB it needs, which
    /// [`Self::from_capabilities`] refuses: such a need is refused where it is read, never
    /// read as a smaller one.
    #[must_use]
    pub fn to_capabilities(&self) -> Vec<(&'static str, String)> {
        let entries = [
            (
                ACCEL_VENDOR_KEY,
                self.vendor.as_ref().map(Vendor::to_string),
            ),
            (ACCEL_MODEL_KEY, self.model.as_ref().map(Model::to_string)),
            (
                ACCEL_COUNT_KEY,
                (self.count > 0).then(|| self.count.to_string()),
            ),
            (
                ACCEL_MEMORY_GIB_KEY,
                self.memory_per_device
                    .map(|bytes| bytes.div_ceil(GIB).to_string()),
            ),
            (
                ACCEL_INTERCONNECT_KEY,
                self.interconnect.as_ref().map(Interconnect::to_string),
            ),
            (
                ACCEL_FORM_FACTOR_KEY,
                self.form_factor.map(|ff| ff.name().to_owned()),
            ),
        ];
        entries
            .into_iter()
            .filter_map(|(key, value)| value.map(|value| (key, value)))
            .collect()
    }
}

impl Accelerator {
    /// The `accelerator.*` capability entries a host reports for these accelerators, in
    /// [`ACCEL_KEYS`] order: vendor, the canonical model, count, memory per device in whole
    /// GiB (rounded down, so a host never claims more than it has), interconnect and form
    /// factor, leaving out what is not known.
    #[must_use]
    pub fn to_capabilities(&self) -> Vec<(&'static str, String)> {
        let entries = [
            (ACCEL_VENDOR_KEY, Some(self.vendor.to_string())),
            (ACCEL_MODEL_KEY, Some(self.model.to_string())),
            (ACCEL_COUNT_KEY, Some(self.count.to_string())),
            (
                ACCEL_MEMORY_GIB_KEY,
                self.memory_per_device
                    .map(|bytes| (bytes / GIB).to_string()),
            ),
            (
                ACCEL_INTERCONNECT_KEY,
                self.interconnect.as_ref().map(Interconnect::to_string),
            ),
            (
                ACCEL_FORM_FACTOR_KEY,
                self.form_factor.map(|ff| ff.name().to_owned()),
            ),
        ];
        entries
            .into_iter()
            .filter_map(|(key, value)| value.map(|value| (key, value)))
            .collect()
    }

    /// The accelerators a host's report states through its `accelerator.*` entries, or
    /// `None` when it names no model. The model is resolved as [`accelerator_of`] resolves
    /// it: a known variant's nominal memory and form factor come first, the entries' own
    /// after. A vendor left out is derived from the model ([`vendor_of`]); a count left
    /// out is zero.
    ///
    /// # Errors
    ///
    /// The [`AccelError`] of the first entry whose value is not valid.
    pub fn from_capabilities(capabilities: &Capabilities) -> Result<Option<Self>, AccelError> {
        let Some(stated) = AcceleratorNeed::from_capabilities(capabilities)? else {
            return Ok(None);
        };
        let Some(model) = stated.model else {
            return Ok(None);
        };
        let resolved = model.canonical();
        Ok(Some(Self {
            vendor: stated.vendor.unwrap_or_else(|| vendor_of(&model)),
            variant: resolved.variant_spelling(&model),
            model: resolved.model,
            count: stated.count,
            memory_per_device: resolved
                .variant
                .map(|variant| variant.memory_per_device)
                .or(stated.memory_per_device),
            form_factor: resolved
                .variant
                .map(|variant| variant.form_factor)
                .or(stated.form_factor),
            interconnect: stated.interconnect,
        }))
    }
}

/// One row of the alias table: a spelling, the canonical family it names and the facts
/// of its variant.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Alias {
    /// The spelling as a source gives it (`nvidia-smi`'s model name, lower case, words
    /// joined by `-`).
    pub spelling: &'static str,
    /// The canonical family; one of [`FAMILIES`].
    pub family: &'static str,
    /// The memory of one device of this variant, in whole GiB (`nvidia-smi` and the
    /// vendor's data sheet state it in units of 2^30 bytes).
    pub memory_gib: u64,
    /// How this variant is built in.
    pub form_factor: FormFactor,
}

/// A canonical family and its vendor.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Family {
    /// The canonical family name.
    pub name: &'static str,
    /// Who makes it.
    pub vendor: &'static str,
}

/// The family of Apple silicon's integrated GPU, the class a Mac's agent reports.
pub const APPLE_M: &str = "apple-m";

/// Every canonical family, sorted by name. This and [`ALIASES`] are the only place a
/// canonical accelerator name is defined; a new accelerator is a new row in a release, and
/// no flag, configuration key or environment variable adds or overrides one.
pub const FAMILIES: &[Family] = &[
    // Apple silicon's integrated GPU, as the agent reports a Mac's (one device, memory
    // shared with the CPU, so none per device).
    Family {
        name: APPLE_M,
        vendor: Vendor::APPLE,
    },
    // NVIDIA A10: `nvidia-smi` prints `NVIDIA A10`, the family name itself.
    Family {
        name: "nvidia-a10",
        vendor: Vendor::NVIDIA,
    },
    // NVIDIA A100, in SXM4 and PCIe forms with 40 or 80 GB.
    Family {
        name: "nvidia-a100",
        vendor: Vendor::NVIDIA,
    },
    // NVIDIA H100, in SXM5 and PCIe forms.
    Family {
        name: "nvidia-h100",
        vendor: Vendor::NVIDIA,
    },
];

/// Every spelling of a variant, sorted by spelling and searched by binary search. Only
/// spellings read from a real source are rows; a spelling the table does not know passes
/// through as its own model and matches only an identical one.
pub const ALIASES: &[Alias] = &[
    // `NVIDIA A100-SXM4-40GB`: A100 SXM4 module with 40 GB HBM2 (NVIDIA A100 data sheet).
    // The spelling is the agent spelling the design's alias table names for this variant.
    Alias {
        spelling: "nvidia-a100-sxm4-40gb",
        family: "nvidia-a100",
        memory_gib: 40,
        form_factor: FormFactor::Sxm,
    },
    // `NVIDIA A100-SXM4-80GB`: A100 SXM4 module with 80 GB HBM2e (NVIDIA A100 data sheet;
    // `nvidia-smi` reports 81920 MiB). Read by the agent's fixtures.
    Alias {
        spelling: "nvidia-a100-sxm4-80gb",
        family: "nvidia-a100",
        memory_gib: 80,
        form_factor: FormFactor::Sxm,
    },
    // `NVIDIA H100 80GB HBM3`: H100 SXM5 module with 80 GB HBM3 (NVIDIA H100 data sheet;
    // `nvidia-smi` reports 81559 MiB, below 80 GiB, so the nominal size is what a request
    // is compared with). Read by the agent's fixtures.
    Alias {
        spelling: "nvidia-h100-80gb-hbm3",
        family: "nvidia-h100",
        memory_gib: 80,
        form_factor: FormFactor::Sxm,
    },
];

/// The facts of a variant the alias table knows.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Variant {
    /// The memory of one device, in bytes.
    pub memory_per_device: u64,
    /// How it is built in.
    pub form_factor: FormFactor,
}

/// A model name resolved through the alias table.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Resolved {
    /// The canonical family, or the name itself when the table does not know it.
    pub model: Model,
    /// The facts of the variant the name spelled; `None` for a family name or an unknown
    /// name.
    pub variant: Option<Variant>,
    /// Whether the table knows the name, as a family or as a variant spelling.
    pub known: bool,
}

impl Resolved {
    /// `spelling` when it named a variant rather than its family; `None` otherwise.
    fn variant_spelling(&self, spelling: &Model) -> Option<Model> {
        (*spelling != self.model).then(|| spelling.clone())
    }
}

/// The narrower of a model a request's key names and the model its booked class names:
/// either alone is kept; of one family, the one that spells a variant (the stated one when
/// neither does or both spell the same variant).
///
/// # Errors
///
/// [`AccelError::ModelConflict`] when they are of different families or name two different
/// variants.
fn narrower(stated: Option<Model>, booked: Option<Model>) -> Result<Option<Model>, AccelError> {
    let (stated, booked) = match (stated, booked) {
        (Some(stated), Some(booked)) => (stated, booked),
        (stated, booked) => return Ok(stated.or(booked)),
    };
    let (want, have) = (stated.canonical(), booked.canonical());
    let pins_differ = want.variant.zip(have.variant).is_some_and(|(a, b)| a != b);
    match want.model != have.model || pins_differ {
        true => Err(AccelError::ModelConflict {
            stated: stated.to_string(),
            booked: booked.to_string(),
        }),
        false if want.variant.is_none() => Ok(Some(booked)),
        false => Ok(Some(stated)),
    }
}

/// The family row named `name`, if any.
fn family(name: &str) -> Option<&'static Family> {
    FAMILIES
        .binary_search_by(|row| row.name.cmp(name))
        .ok()
        .map(|at| &FAMILIES[at])
}

/// The alias row spelled `spelling`, if any.
fn alias(spelling: &str) -> Option<&'static Alias> {
    ALIASES
        .binary_search_by(|row| row.spelling.cmp(spelling))
        .ok()
        .map(|at| &ALIASES[at])
}

impl Model {
    /// This name resolved through the alias table: a variant spelling becomes its family
    /// with that variant's facts, a family name stays itself with no variant, and a name
    /// the table does not know stays itself with `known: false`.
    #[must_use]
    pub fn canonical(&self) -> Resolved {
        if let Some(row) = alias(self.as_str()) {
            return Resolved {
                model: Model(row.family.to_owned()),
                variant: Some(Variant {
                    memory_per_device: row.memory_gib * GIB,
                    form_factor: row.form_factor,
                }),
                known: true,
            };
        }
        Resolved {
            model: self.clone(),
            variant: None,
            known: family(self.as_str()).is_some(),
        }
    }
}

/// The vendor of `model`: the table's vendor for a name it knows; for an unknown name with
/// a `-`, its first word when that is a vendor token (every agent spelling begins with its
/// vendor, so a new accelerator still meets `vendor=nvidia`); else [`Vendor::UNKNOWN`].
#[must_use]
pub fn vendor_of(model: &Model) -> Vendor {
    let family_name = model.canonical().model;
    if let Some(row) = family(family_name.as_str()) {
        return Vendor(row.vendor.to_owned());
    }
    match model.as_str().split_once('-') {
        Some((first, _)) => {
            Vendor::parse(first).unwrap_or_else(|_| Vendor(Vendor::UNKNOWN.to_owned()))
        }
        None => Vendor(Vendor::UNKNOWN.to_owned()),
    }
}

/// The accelerators of `count` devices that a source spells `spelling`: the one way a
/// host's, a hello's and an offer's accelerator is built. The model is the canonical
/// family, the vendor is [`vendor_of`], and the memory per device is the variant's nominal
/// memory when the table knows the variant, else `measured` (the least any device
/// reported; `None` when nothing was measured).
#[must_use]
pub fn accelerator_of(spelling: &Model, count: u32, measured: Option<u64>) -> Accelerator {
    let resolved = spelling.canonical();
    Accelerator {
        vendor: vendor_of(spelling),
        variant: resolved.variant_spelling(spelling),
        memory_per_device: resolved
            .variant
            .map(|variant| variant.memory_per_device)
            .or(measured),
        form_factor: resolved.variant.map(|variant| variant.form_factor),
        model: resolved.model,
        count,
        interconnect: None,
    }
}
