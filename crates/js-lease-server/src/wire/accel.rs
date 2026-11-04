//! The `resources.accelerator` object of a lease body, as read, and its conversion to the
//! request `js_caps::request_need` takes.

use js_caps::AcceleratorRequest;
use serde::Deserialize;

/// The `resources.accelerator` object of an [`super::AcquireBody`]; every field is optional and a
/// field left out matches anything.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct AcceleratorBody {
    #[serde(default)]
    pub(super) vendor: Option<String>,
    #[serde(default)]
    pub(super) model: Option<String>,
    #[serde(default)]
    pub(super) count: Option<u32>,
    #[serde(default)]
    pub(super) memory_per_device_gib: Option<u32>,
    #[serde(default)]
    pub(super) interconnect: Option<String>,
    #[serde(default)]
    pub(super) form_factor: Option<String>,
}

impl From<AcceleratorBody> for AcceleratorRequest {
    fn from(body: AcceleratorBody) -> Self {
        Self {
            vendor: body.vendor,
            model: body.model,
            count: body.count,
            memory_per_device_gib: body.memory_per_device_gib,
            interconnect: body.interconnect,
            form_factor: body.form_factor,
        }
    }
}
