//! How a violation spells the cell's values: by hand, field by field, in the shapes of
//! [`js_sim::text`], never through `Debug`.
//!
//! | value | text |
//! |---|---|
//! | host | its id, as it is |
//! | lease | `<term>.<sequence>` |
//! | run | `run "<id>"`, the id quoted and escaped |
//! | item | `item<n>` |
//! | client event | `submitted`, `queued`, `placed <host>`, `started`, `terminal <how>`, `release-sent`, `released`; never the ids the client learned |
//! | resources | `res(cpu=<n> mem=<n> gpus=<n>)`, the three axes a host is booked on |

use js_types::{HostId, LeaseId, Resources};

use crate::observe::{ClientEvent, ItemId, RunId};

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

/// A client event, without the ids the server gave the item.
pub(crate) fn event(event: &ClientEvent) -> String {
    match event {
        ClientEvent::Submitted => "submitted".to_owned(),
        ClientEvent::Queued { .. } => "queued".to_owned(),
        ClientEvent::Placed { host: on } => format!("placed {}", host(on)),
        ClientEvent::Started => "started".to_owned(),
        ClientEvent::Terminal { how } => format!("terminal {}", how.name()),
        ClientEvent::ReleaseSent => "release-sent".to_owned(),
        ClientEvent::Released => "released".to_owned(),
    }
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
