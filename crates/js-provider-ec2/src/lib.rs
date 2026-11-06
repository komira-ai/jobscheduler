//! The AWS EC2 machine provider: its catalog, and the launch, release, list and read of the
//! instances it launches. [`Ec2Provider`] implements `js_elastic::CapacityProvider`: the
//! scheduler core knows no EC2 instance-type name, no zone and no purchase option, and no
//! configuration says what a type provides.
//!
//! It holds
//!
//! - [`Ec2Api`], the port: narrow and blocking, each call one request or one hidden walk
//!   over every page of one list, a failure never an empty answer. Its catalog side reads
//!   instance types, zone offerings, on-demand and spot prices, a vCPU quota and Capacity
//!   Block offerings; its launch side launches an instance, reads instances and
//!   terminates them, buys a Capacity Block and reads capacity reservations.
//!   [`HttpEc2`] is the implementation over the signed HTTPS client
//!   (SigV4, `js_logstore::Transport`), and [`FakeEc2`] the same calls over memory, with
//!   failures scripted per [`Action`];
//! - the typed answers ([`InstanceTypeInfo`], [`GpuInfo`], [`ZoneOffering`], [`TypePrice`],
//!   [`ZonePrice`], [`VcpuQuota`], [`BlockOffering`], [`Instance`],
//!   [`CapacityReservation`]) and the errors
//!   ([`ApiError`], matched on the cloud's [`ErrorCode`], never on its message text;
//!   [`CallError`] naming the action);
//! - [`Ec2Catalog`] with its [`Ec2CatalogSettings`], whose [`Ec2Catalog::catalog`] returns
//!   the offers and the [`CatalogNote`]s of the quota reads it did without;
//! - [`Ec2Provider`] with its [`Ec2Config`], and [`kind`], the `ec2` provider kind of the
//!   lease server's configuration.
//!
//! **Launching.** An acquire is refused, with nothing sent, unless the provider is enabled,
//! the offer is on demand, spot or a Capacity Block with its terms, in a zone of an allowed
//! type, the host name is this scheduler's, the boot payload fits [`PAYLOAD_LIMIT`], an
//! image is configured for the offer's architecture and the labels become tags; in dry-run
//! mode it stops there, and buys nothing. A launch is `RunInstances` for one instance,
//! tagged with the scheduler, the acquire and the host name ([`SCHEDULER_TAG`],
//! [`ACQUIRE_TAG`], [`NAME_TAG`]), carrying a client token derived from the scheduler and
//! the acquire, so a repeat of one acquire is the same launch. When the token's window has
//! passed the provider finds the instance again by its tags. The answer of a launch that
//! failed is read by the one table of [`launch_answer`]: a cloud code that says the launch
//! did not happen is a refusal, every other failure (a throttle, a timeout, a server
//! fault) is unknown, and an unknown acquire counts toward the machine cap until a list
//! shows it or [`Ec2Config::unknown_hold`] passes. The user data is the boot payload as it
//! was rendered; it is never logged.
//!
//! **Capacity Blocks.** An acquire of a block offer first buys the block, then launches
//! into it. `PurchaseCapacityBlock` has no client token, so before buying the provider
//! reads the capacity reservations that carry the scheduler's and the acquire's tags
//! ([`SCHEDULER_TAG`], [`ACQUIRE_TAG`]) and uses the one an earlier attempt bought; a read
//! that fails launches nothing and buys nothing, and a purchase that got no answer is
//! unknown, to be settled by that read. A reservation that is not active (`scheduled`,
//! `payment-pending`, `failed` and the others) is refused as
//! [`js_elastic::RefusalCode::Capacity`] with the state as the reason, and nothing is
//! launched; a retry finds the same reservation.
//! The launch names the reservation and the market type `capacity-block`, and the instance
//! carries the reservation's end in the [`BLOCK_END_TAG`] tag, which is the end the held
//! machine states ([`js_elastic::Held::ends_unix_seconds`]) when it is launched and when
//! it is listed.
//!
//! **Which instances are ours.** An instance is ours when it carries the scheduler's tag, an
//! acquire tag and a name with the scheduler's prefix; one with some of those marks is a
//! suspect, which counts toward the cap and is never terminated; any other is foreign and
//! untouched. A release reads the instance by id, terminates it only when it is ours, and
//! reads it back: only a not-found answer is "gone".
//!
//! **Identity.** A machine's first hello may carry the signed instance identity document
//! the instance metadata service serves. [`AwsIdentityVerifier`] checks its signature
//! against the key AWS publishes for the document; the provider then accepts the hello
//! only when the document names the instance, the configured region and the image that was
//! launched. A hello with no identity is matched by name and instance id.
//!
//! **GPU jobs.** An instance with a GPU is offered with its GPUs, but jobs on EC2 run on
//! the fallback runtime until GPU passthrough exists (design step 23); this crate has no
//! GPU runtime code, and the machine image carries whatever driver stack the operator
//! puts in it.
//!
//! **Credentials.** The client signs with the provider-standard variables
//! (`AWS_ACCESS_KEY_ID`, `AWS_SECRET_ACCESS_KEY`, `AWS_SESSION_TOKEN`) when they are set,
//! else with the credentials of the machine's instance role, read from the instance
//! metadata service (version 2) and refreshed before they expire. No credential is
//! configuration, and none appears in a `Debug` text or a log. The region of the
//! configuration is the one used; `AWS_REGION` is not read.
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
//! ([`js_elastic::BlockTerms`]) at its upfront fee spread over its instance hours. An
//! acquire of such an offer buys that block and launches into it (see Capacity Blocks
//! above).
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
mod clock;
mod config;
mod error;
mod fake;
mod gpu;
mod http;
mod identity;
pub mod kind;
mod launch;
mod model;
mod own;
mod price;
mod provider;
mod quota;
mod refusal;
mod translate;

pub use api::{Action, Ec2Api, PurchaseKind};
pub use catalog::{CatalogAnswer, CatalogNote, Ec2Catalog, Ec2CatalogSettings};
pub use clock::{Clock, ManualClock, SystemClock};
pub use config::{ConfigError, Ec2Config, Images, Mode};
pub use error::{ApiError, CallError, ErrorCode, SettingsError};
pub use fake::{Call, FakeEc2};
pub use http::{Credentials, EC2_API_VERSION, HttpEc2, QUOTAS_TARGET, StaticKeys};
pub use identity::{AwsIdentityVerifier, FakeIdentityVerifier, IdentityDocument, IdentityVerifier};
pub use launch::{
    ACQUIRE_TAG, BLOCK_END_TAG, BlockPurchase, CapacityReservation, CapacityReservationId,
    ClientToken, DescribeFilter, ImageId, Instance, InstanceId, InstanceState, LABEL_PREFIX,
    LaunchPurchase, NAME_TAG, ReservationState, RunRequest, SCHEDULER_TAG, Tag, UserData,
};
pub use model::{
    ArchName, BlockOffering, GpuInfo, InstanceType, InstanceTypeInfo, Region, TypePrice, VcpuQuota,
    Zone, ZoneOffering, ZonePrice,
};
pub use own::LabelError;
pub use provider::{Ec2Provider, PAYLOAD_LIMIT, PROVIDER_ID};
pub use quota::QuotaFamily;
pub use refusal::launch_answer;
