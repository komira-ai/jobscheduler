//! The accelerator vocabulary of design 3.8, as `js-caps` serves it to the agent, the
//! provider crates and the server: the structured [`Accelerator`] and [`AcceleratorNeed`],
//! their comparison ([`AcceleratorNeed::met_by`]), and the alias table ([`ALIASES`],
//! [`FAMILIES`], [`Model::canonical`], [`vendor_of`], [`accelerator_of`]) that maps every
//! spelling to its canonical family and variant facts.
//!
//! The table's data and those functions are defined in `js-types` and re-exported here
//! unchanged: `js-types` is the lowest layer (`layers.toml`), and its `Shape` matcher, which
//! the registry and the pools call, resolves a host's class through the same table. No
//! flag, configuration key or environment variable defines or overrides an alias.
//!
//! [`request_need`] is the one conversion of a request's accelerator, the optional object
//! of a lease body or a job, beside the `gpus` and `gpu_class` pair it may also carry, into
//! the booked GPU count and class and the `accelerator.*` capability keys.

use js_types::Capabilities;

pub use js_types::{
    ACCEL_COUNT_KEY, ACCEL_FORM_FACTOR_KEY, ACCEL_INTERCONNECT_KEY, ACCEL_KEYS,
    ACCEL_MEMORY_GIB_KEY, ACCEL_MODEL_KEY, ACCEL_VENDOR_KEY, ALIASES, APPLE_M, AccelError,
    Accelerator, AcceleratorMiss, AcceleratorNeed, Alias, FAMILIES, Family, FormFactor,
    Interconnect, MAX_MEMORY_GIB, Model, Resolved, Variant, Vendor, accelerator_of, vendor_of,
};

/// Bytes in a GiB.
const GIB: u64 = 1 << 30;

/// A request's accelerator as its sender wrote it: the optional object of a lease body or
/// a job. Every field is optional; a field left out matches anything.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct AcceleratorRequest {
    /// The vendor (`nvidia`).
    pub vendor: Option<String>,
    /// The model: a family (`nvidia-a100`) or a full spelling (`nvidia-a100-sxm4-80gb`).
    pub model: Option<String>,
    /// The least number of devices.
    pub count: Option<u32>,
    /// The least memory per device, in whole GiB.
    pub memory_per_device_gib: Option<u32>,
    /// The least interconnect, `kind>=size`.
    pub interconnect: Option<String>,
    /// The form factor, `sxm` or `pcie`.
    pub form_factor: Option<String>,
}

/// What a request books and requires once its accelerator is converted.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RequestNeed {
    /// The GPUs it books.
    pub gpus: u64,
    /// The class of those GPUs, set exactly when `gpus` is above zero.
    pub gpu_class: Option<String>,
    /// The `accelerator.*` capability entries that state its need; empty when it carries
    /// no accelerator object.
    pub capabilities: Vec<(&'static str, String)>,
}

/// Why a request's accelerator cannot be converted. Each names `accelerator`, the field
/// a client sends.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum RequestNeedError {
    /// A field of the object is not valid.
    #[error("accelerator: {0}")]
    Field(#[from] AccelError),
    /// The object asks for devices and names no model, so the GPUs it books would have no
    /// class.
    #[error("accelerator.model is required when accelerator.count is above zero")]
    CountWithoutModel,
    /// The object names another model than `gpu_class`.
    #[error("accelerator.model {accelerator:?} names another model than gpu_class {gpu_class:?}")]
    ModelDisagrees {
        /// The object's model.
        accelerator: String,
        /// The pair's class.
        gpu_class: String,
    },
    /// The object asks for another count than `gpus`.
    #[error("accelerator.count {accelerator} differs from gpus {gpus}")]
    CountDisagrees {
        /// The object's count.
        accelerator: u32,
        /// The pair's count.
        gpus: u64,
    },
}

/// The need a request states, from the `gpus` and `gpu_class` pair it books and the
/// accelerator object it may also carry:
///
/// - Neither (no GPU, no object): nothing booked, no capability entry, as before.
/// - The pair only: booked as it is, with no capability entry, so the request is stored
///   and compared exactly as the previous release stores it (a repeated acquire with the
///   same request id matches across an upgrade or a downgrade). Matching derives the need
///   from the pair ([`AcceleratorNeed::booked`]): the class is the model and `gpus` the
///   count, so a full spelling pins its variant and a family matches every variant.
/// - The object only: it books its count of its model (the class is the model as spelt);
///   a count above zero needs a model.
/// - Both: they must agree, the model by family and variant after resolution and the count
///   exactly; the pair stays what is booked.
///
/// # Errors
///
/// [`RequestNeedError`] naming the field that is not valid or the disagreement.
pub fn request_need(
    gpus: u64,
    gpu_class: Option<&str>,
    object: Option<&AcceleratorRequest>,
) -> Result<RequestNeed, RequestNeedError> {
    let booked = || RequestNeed {
        gpus,
        gpu_class: gpu_class.map(str::to_owned),
        capabilities: Vec::new(),
    };
    let Some(object) = object else {
        return Ok(booked());
    };
    let mut need = parse_object(object)?;
    let pair = gpus > 0 || gpu_class.is_some();
    if !pair {
        if need.count > 0 && need.model.is_none() {
            return Err(RequestNeedError::CountWithoutModel);
        }
        let class = (need.count > 0)
            .then(|| need.model.as_ref().map(Model::to_string))
            .flatten();
        return Ok(RequestNeed {
            gpus: u64::from(need.count),
            gpu_class: class,
            capabilities: need.to_capabilities(),
        });
    }
    let class = gpu_class.unwrap_or_default();
    match (need.model.clone(), Model::parse(class)) {
        (Some(model), Ok(booked_model)) if model.canonical() != booked_model.canonical() => {
            return Err(RequestNeedError::ModelDisagrees {
                accelerator: model.to_string(),
                gpu_class: class.to_owned(),
            });
        }
        (Some(model), Err(_)) => {
            return Err(RequestNeedError::ModelDisagrees {
                accelerator: model.to_string(),
                gpu_class: class.to_owned(),
            });
        }
        (None, Ok(booked_model)) => need.model = Some(booked_model),
        _ => {}
    }
    match object.count {
        Some(count) if u64::from(count) != gpus => {
            return Err(RequestNeedError::CountDisagrees {
                accelerator: count,
                gpus,
            });
        }
        _ => need.count = u32::try_from(gpus).unwrap_or(u32::MAX),
    }
    Ok(RequestNeed {
        capabilities: need.to_capabilities(),
        ..booked()
    })
}

/// Whether the `accelerator.*` capabilities a request names itself agree with the GPUs it
/// books as `gpus` and `gpu_class`: by the rules of [`request_need`] for an object beside
/// the pair, the stated model by family and variant and the stated count exactly. A request
/// that books no GPU, or names no such capability, agrees.
///
/// # Errors
///
/// [`RequestNeedError`] naming a capability that is not valid or the disagreement.
pub fn check_stated(
    capabilities: &Capabilities,
    gpus: u64,
    gpu_class: Option<&str>,
) -> Result<(), RequestNeedError> {
    let Some(stated) = AcceleratorNeed::from_capabilities(capabilities)? else {
        return Ok(());
    };
    let object = AcceleratorRequest {
        model: stated.model.as_ref().map(Model::to_string),
        count: (stated.count > 0).then_some(stated.count),
        ..AcceleratorRequest::default()
    };
    match gpus > 0 || gpu_class.is_some() {
        true => request_need(gpus, gpu_class, Some(&object)).map(|_| ()),
        false => Ok(()),
    }
}

/// The need the object states, each field validated.
fn parse_object(object: &AcceleratorRequest) -> Result<AcceleratorNeed, AccelError> {
    Ok(AcceleratorNeed {
        vendor: object.vendor.as_deref().map(Vendor::parse).transpose()?,
        model: object.model.as_deref().map(Model::parse).transpose()?,
        count: object.count.unwrap_or(0),
        memory_per_device: object.memory_per_device_gib.map(|gib| u64::from(gib) * GIB),
        interconnect: object
            .interconnect
            .as_deref()
            .map(Interconnect::parse)
            .transpose()?,
        form_factor: object
            .form_factor
            .as_deref()
            .map(str::parse::<FormFactor>)
            .transpose()?,
    })
}
