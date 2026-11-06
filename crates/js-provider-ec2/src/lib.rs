//! The read half of the AWS EC2 machine provider: its catalog. What EC2 says about a
//! region's instance types (their GPU information, architectures, sizes), the zones each
//! is offered in, its on-demand, spot and Capacity Block prices and the account's vCPU
//! quotas, translated in code into [`js_elastic::Offer`]s in the scheduler's vocabulary.
//! The scheduler core knows no EC2 instance-type name, and no configuration says what a
//! type provides.
//!
//! It holds
//!
//! - [`Ec2Api`], the port: narrow, blocking and read-only (instance types, zone offerings,
//!   on-demand prices, spot prices, a vCPU quota, Capacity Block offerings), each call one
//!   request or one hidden walk over every page of one list, a failure never an empty
//!   answer; and [`FakeEc2`], the same calls over memory with failures scripted per
//!   [`Action`];
//! - the typed answers ([`InstanceTypeInfo`], [`GpuInfo`], [`ZoneOffering`], [`TypePrice`],
//!   [`ZonePrice`], [`VcpuQuota`], [`BlockOffering`]) and the errors ([`ApiError`], matched
//!   on the cloud's [`ErrorCode`], never on its message text; [`CallError`] naming the
//!   action);
//! - [`Ec2Catalog`] with its [`Ec2CatalogSettings`], whose [`Ec2Catalog::catalog`] returns
//!   the offers and the [`CatalogNote`]s of the quota reads it did without.
//!
//! **No network code yet.** No implementation of [`Ec2Api`] here talks to a network, and
//! the crate does not implement `js_elastic::CapacityProvider`: launching, releasing,
//! listing and reading machines, the boot payload, the machine image, and the signed HTTPS
//! client of the port come with the launch side, which calls this catalog.
//!
//! **What the catalog holds.** One offer per (instance type, availability zone, purchase
//! option) for on-demand and spot, and one per Capacity Block offering, for the purchase
//! options the configuration lists and no other. With an `allowed_instance_types` list,
//! every type it names, GPU or not; without one, only the types with a GPU (a region has
//! hundreds of types and thousands of offers, and the list is how the configuration
//! selects CPU types). Every offer states its architecture (`x86_64` or `arm64`); a type
//! with neither (`i386`-only, a Mac type) has none. A type no zone offers has no on-demand
//! or spot offer; a Capacity Block offering is its own evidence of capacity. An offer with
//! no price is listed with none, so admission can still count it. Prices are whole US
//! cents per hour, rounded up from the cloud's decimal text. The quota is information
//! only: it never removes or marks an offer, and a failed quota read leaves the offers
//! without one and adds a note. The order is stable: instance type, zone, the configured
//! purchase order, then the Capacity Block offering.
//!
//! **Purchase options.** `on-demand`, `spot` and `capacity-block`. The newest GPU types are
//! not all sold on demand or as spot: AWS documents P6e-GB300 UltraServers as bought
//! through AWS sales and P6e UltraServers as reserved through EC2 Capacity Blocks for ML,
//! so `capacity-block` reads `DescribeCapacityBlockOfferings` for the length the
//! configuration sets (`capacity_block_hours`) and offers each block with its terms
//! ([`js_elastic::BlockTerms`]) at its upfront fee spread over its instance hours.
//!
//! **GPU names.** The translation table maps `GpuInfo` (manufacturer, name and memory per
//! GPU) to a variant spelling of the `js-caps` alias table, which gives the canonical model
//! and its variant facts. It has rows only for the spellings read from AWS's own sources:
//! the A100 of 40 and 80 GiB (p4d, p4de) and the H100 of 80 GiB (p5). Every other GPU (the
//! A10G, L4, L40S, T4, T4g, V100, M60, K80, H200, B200, B300, the GB200 and GB300 types,
//! and those AWS adds) passes through as its own spelling, `manufacturer-name` in lower
//! case with words joined by `-` (`nvidia-l40s`), which an exact-name request matches, and
//! is logged once per type at `warn`. The memory per GPU of a pass-through is what
//! `DescribeInstanceTypes` states.
//!
//! **Adding an instance family's GPU.** Add one row to the table in `gpu.rs` (the
//! manufacturer, name and memory per GPU as `DescribeInstanceTypes` gives them, and the
//! `js-caps` variant spelling) with a comment naming the AWS source it was read from, and
//! one test case: the table test drives every row and checks its spelling against
//! `js-caps`. A canonical name `js-caps` does not define is added there first.

mod api;
mod catalog;
mod error;
mod fake;
mod gpu;
mod model;
mod price;
mod quota;
mod translate;

pub use api::{Action, Ec2Api, PurchaseKind};
pub use catalog::{CatalogAnswer, CatalogNote, Ec2Catalog, Ec2CatalogSettings};
pub use error::{ApiError, CallError, ErrorCode, SettingsError};
pub use fake::{Call, FakeEc2};
pub use model::{
    ArchName, BlockOffering, GpuInfo, InstanceType, InstanceTypeInfo, Region, TypePrice, VcpuQuota,
    Zone, ZoneOffering, ZonePrice,
};
pub use quota::QuotaFamily;
