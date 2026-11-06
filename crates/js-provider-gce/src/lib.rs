//! The read half of the GCP Compute Engine machine provider: its catalog. What Compute
//! Engine says about a project's zones (their machine types with their built-in
//! accelerators and architectures, the accelerator types each zone offers to attach), the
//! Cloud Billing prices of the regions and the regions' quotas, translated in code into
//! [`js_elastic::Offer`]s in the scheduler's vocabulary. The scheduler core knows no
//! Compute Engine machine-type name, and no configuration says what a type provides.
//!
//! It holds
//!
//! - [`GceApi`], the port: narrow, blocking and read-only (machine types, accelerator
//!   types, SKU prices, region quotas), each call one request or one hidden walk over every
//!   page of one list, a failure never an empty answer; and [`FakeGce`], the same calls
//!   over memory with failures scripted per [`Method`];
//! - the typed answers ([`MachineTypeInfo`], [`AcceleratorTypeInfo`], [`SkuPrice`],
//!   [`QuotaMetric`]) and the errors ([`ApiError`], matched on the cloud's
//!   [`ErrorReason`], never on its message text; [`CallError`] naming the method);
//! - [`GceCatalog`] with its [`GceCatalogSettings`], whose [`GceCatalog::catalog`] returns
//!   the offers and the [`CatalogNote`]s of the quota reads it did without.
//!
//! **No network code yet.** No implementation of [`GceApi`] here talks to a network, and
//! the crate does not implement `js_elastic::CapacityProvider`: inserting, deleting,
//! listing and reading instances, the boot payload, the boot disk and its image, and the
//! HTTPS client of the port with its credentials come with the launch side, which calls
//! this catalog.
//!
//! **What the catalog holds.** One offer per (machine type, zone, provisioning model), for
//! the provisioning models the configuration lists and the type is sold on, and for no
//! other; an N1 type has one more per accelerator its zone offers to attach and number of
//! it the N1 table allows. With an `allowed_machine_types` list, every type it names, with
//! or without accelerators; without one, only the offers with an accelerator. Every offer
//! states its architecture (`x86_64` or `arm64`), from the type's own `architecture` or
//! else from the series table; a type with neither has no offer (the count is logged).
//! A type no configured zone lists has no offer. An offer with no price is listed with none, so admission can still
//! count it. Prices are whole US cents per hour, rounded up from the SKUs' integer units
//! and nanos. The quota (vCPUs and GPUs) is information only: it never removes or marks an
//! offer, and a failed quota read leaves the region's offers without one and adds a note.
//! The order is stable: machine type, zone, accelerators, then the configured order of
//! the provisioning models.
//!
//! **Provisioning models.** `on-demand` (the standard model) and `spot`. The newest
//! accelerator-optimized series are not all sold on both: A4X and A4X Max only through
//! future reservations for capacity blocks, A4 and A3 Ultra and A3 High with fewer than 8
//! GPUs on Spot (and on the reservation and flex-start models) but not on the standard
//! model. A table in `translate.rs` says so, and those series have no offer of a model
//! they are not sold on. The reservation and scheduled models are not offered.
//!
//! **Accelerator names.** The translation table maps the accelerator type name to a
//! spelling of the `js-caps` alias table, which gives the canonical model and, for a
//! variant spelling, its variant facts. It has rows only for the names read from Google's
//! own documentation whose model `js-caps` defines: `nvidia-tesla-a100` and
//! `nvidia-a100-80gb` (the A100 family with 40 and 80 GiB), `nvidia-h100-80gb` and
//! `nvidia-h100-mega-80gb` (the H100 SXM5 80 GB HBM3 variant) and `nvidia-b200` (the B200
//! family). Every other accelerator (the T4, P4 and V100 attached to N1, the L4, the RTX
//! PRO 6000, the H200, the GB200 and GB300, the virtual workstation forms, and those Google
//! adds) passes through as its own spelling, lower case with words joined by `-`
//! (`nvidia-l4`), which an exact-name request matches, and is logged once per catalog
//! read at `warn`; its memory per GPU is not stated, since the API states none.
//!
//! **Adding a machine family.** Add one row to the table it needs, with a comment naming
//! the Google documentation it was read from (`gpu.rs` for an accelerator name, the N1
//! attachments; `translate.rs` for the architecture and the provisioning models;
//! `price.rs` for the billing series; `quota.rs` for the quota metrics), and one test
//! case: each table's test drives every row, and the accelerator table's checks each
//! spelling against `js-caps`. A canonical name `js-caps` does not define is added there
//! first.

mod api;
mod catalog;
mod error;
mod fake;
mod gpu;
mod model;
mod price;
mod quota;
mod translate;

pub use api::{GceApi, Method, PurchaseKind};
pub use catalog::{CatalogAnswer, CatalogNote, GceCatalog, GceCatalogSettings};
pub use error::{ApiError, CallError, ErrorReason, SettingsError};
pub use fake::{Call, FakeGce};
pub use model::{
    AcceleratorTypeInfo, AcceleratorTypeName, ArchName, AttachedAccelerator, MachineTypeInfo,
    MachineTypeName, Money, PricedItem, Project, QuotaMetric, Region, SeriesName, SkuPrice,
    UsageType, Zone,
};
