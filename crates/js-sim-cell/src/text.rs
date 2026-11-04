//! How a violation spells the cell's values: by hand, field by field, in the shapes of
//! [`js_sim::text`], never through `Debug`.
//!
//! | value | text |
//! |---|---|
//! | host | its id, as it is |
//! | lease | `<term>.<sequence>` |
//! | run | `run "<id>"`, the id quoted and escaped |
//! | item | `item<n>` |
//! | resources | `res(cpu=<n> mem=<n> gpus=<n>)`, the three axes a host is booked on |

use js_types::{HostId, LeaseId, Resources};

use crate::observe::{ItemId, RunId};

/// A host: its id.
pub(crate) fn host(host: &HostId) -> String {
    host.as_str().to_owned()
}

/// A lease: `<term>.<sequence>`, as [`js_sim::text::lease`] writes the scheduler's.
pub(crate) fn lease(lease: LeaseId) -> String {
    format!("{}.{}", lease.term(), lease.sequence())
}

/// A run: `run "<id>"`.
pub(crate) fn run(run: &RunId) -> String {
    format!("run {}", js_sim::text::quoted(run.as_str()))
}

/// An item: `item<n>`.
pub(crate) fn item(item: ItemId) -> String {
    format!("item{}", item.0)
}

/// Three booked amounts, as [`axes`] reads them from a vector.
pub(crate) fn load(load: [u128; 3]) -> String {
    let [cpu, mem, gpus] = load;
    format!("res(cpu={cpu} mem={mem} gpus={gpus})")
}

/// The booked axes of `r`, in the order [`AXES`] names them, widened so that sums of
/// them cannot overflow.
pub(crate) fn axes(r: &Resources) -> [u128; 3] {
    [r.cpu_millis(), r.memory_bytes(), r.gpus()].map(u128::from)
}

/// The names of the booked axes, in the order of [`axes`].
pub(crate) static AXES: [&str; 3] = ["cpu", "mem", "gpus"];
