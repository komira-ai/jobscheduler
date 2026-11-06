//! The GCP Compute Engine machine provider: its catalog, and the insert, delete, list and
//! read of the instances it launches. [`GceProvider`] implements
//! `js_elastic::CapacityProvider`: the scheduler core knows no Compute Engine
//! machine-type name, no zone and no provisioning model, and no configuration says what a
//! type provides. Its catalog is what Compute Engine says about a project's zones (their
//! machine types with their built-in accelerators and architectures, the accelerator
//! types each zone offers to attach), the Cloud Billing prices of the regions and the
//! regions' quotas, translated in code into [`js_elastic::Offer`]s in the scheduler's
//! vocabulary.
//!
//! It holds
//!
//! - [`GceApi`], the port: narrow and blocking. Its catalog side reads machine types,
//!   accelerator types, SKU prices and region quotas, each call one request or one hidden
//!   walk over every page of one list, a failure never an empty answer; its launch side is
//!   one call of the cloud per method (`instances.insert`, `instances.get`,
//!   `instances.delete`, one page of `instances.aggregatedList`, `zoneOperations.get`).
//!   [`HttpGce`] is the implementation over HTTPS, and [`FakeGce`] the same calls over
//!   memory, with failures scripted per [`Method`];
//! - the typed answers ([`MachineTypeInfo`], [`AcceleratorTypeInfo`], [`SkuPrice`],
//!   [`QuotaMetric`], [`Instance`], [`Operation`]) and the errors ([`ApiError`], matched on
//!   the cloud's [`ErrorReason`], never on its message text; [`CallError`] naming the
//!   method);
//! - [`GceCatalog`] with its [`GceCatalogSettings`], whose [`GceCatalog::catalog`] returns
//!   the offers and the [`CatalogNote`]s of the quota reads it did without;
//! - [`GceProvider`] with its [`GceConfig`], and [`kind`], the `gce` machine provider kind
//!   of the service's configuration.
//!
//! **Launching.** An acquire is refused, with nothing sent, unless the provider is enabled,
//! the offer is on demand or Spot in a configured zone of an allowed type, the host name is
//! this scheduler's (and a name of the cloud's grammar), the boot payload fits
//! [`PAYLOAD_LIMIT`], an image is configured for the offer's architecture and the labels
//! become labels of the cloud's grammar; in dry-run mode it stops there. A launch is
//! `instances.insert` for one instance named by the host name, labelled with the scheduler
//! and the acquire ([`SCHEDULER_LABEL`], [`ACQUIRE_LABEL`], values made by
//! [`label_value`]), its boot disk made from the image of its architecture, the boot
//! payload as the one metadata item [`PAYLOAD_KEY`], and a `requestId` derived from the
//! scheduler and the acquire ([`RequestId`]), so a repeat of one acquire is the same
//! insert. Before an insert, the provider lists the instances of the acquire by its labels;
//! an instance found is the answer, and nothing is inserted, so an acquire is still the
//! same machine once the cloud has forgotten the request id. A name taken already
//! (`alreadyExists`) is answered with the instance of that name. The insert answers a zone
//! operation, which is read once: `Accepted` means it was accepted and has not failed, not
//! that the machine is ready. The answer of an insert or operation that failed is read by
//! the one table of [`launch_answer`]: a code that says the launch did not happen is a
//! refusal, every other failure (a throttle, a timeout, a server fault, an operation that
//! cannot be read) is unknown, and an unknown acquire counts toward the machine cap until a
//! list shows it or [`GceConfig::unknown_hold`] passes. The payload is never read, edited or
//! logged.
//!
//! **Scheduling.** Every instance is created with `onHostMaintenance` `TERMINATE` and
//! `automaticRestart` false: a restarted machine would boot the agent again with a spent
//! join token. A Spot instance is deleted when the cloud preempts it
//! (`instanceTerminationAction` `DELETE`), so a reclaimed machine does not linger as
//! `TERMINATED`; a pool that bounds a machine's lifetime has it set as the instance's
//! `maxRunDuration`, with the same deletion, so billing stops even if the scheduler is
//! dead. The provider takes the provisioning model from the offer and never reorders the
//! configured ones. There is no quota check before a launch: a quota refusal is
//! `Refused(Quota)`, which the engine handles.
//!
//! **Which instances are ours.** An instance is ours when it carries the scheduler's label,
//! an acquire label and a name with the scheduler's prefix; one with some of those marks is
//! a suspect, which counts toward the cap and is never deleted; any other is foreign and
//! untouched. The scheduler's reference to an instance is `<zone>/<id>` ([`host_ref`]); a
//! release reads the instance by id, deletes it only when it is ours, and reads it back:
//! only a `notFound` answer is "gone", and a `TERMINATED` instance is still there.
//!
//! **Identity.** A machine's first hello may carry the identity token the metadata server
//! signs. [`GoogleIdentityVerifier`] checks its signature against the keys the crate pins;
//! the provider then accepts the hello only when the token names the configured project and
//! the zone, id and name of the instance launched under that name. A hello with no identity
//! is matched by name and instance id, and the one-time join token stays the credential.
//!
//! **GPU jobs.** A machine type with accelerators is offered with them, but jobs on these
//! machines run on the fallback runtime until GPU passthrough for `crun` exists; this crate
//! has no GPU runtime code, and the machine image carries whatever driver stack the
//! operator puts in it.
//!
//! **Credentials.** The client authenticates with the application-default credentials: the
//! service-account key file that `GOOGLE_APPLICATION_CREDENTIALS` names, when it is set,
//! else the service account of the machine the service runs on, whose access token the
//! metadata server hands out. They are read once, when the client is built. No credential
//! is configuration, and none appears in a `Debug` text, an error or a log.
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

pub use api::{GceApi, Method, PurchaseKind};
pub use catalog::{CatalogAnswer, CatalogNote, GceCatalog, GceCatalogSettings};
pub use clock::{Clock, ManualClock, SystemClock};
pub use config::{ConfigError, GceConfig, Images, Mode};
pub use error::{ApiError, CallError, ErrorReason, SettingsError};
pub use fake::{Call, FakeGce};
pub use http::{
    BILLING_BASE, COMPUTE_BASE, COMPUTE_ENGINE_SERVICE, CredentialError, Credentials, HttpGce,
    KEY_FILE_VARIABLE, ServiceAccountKey,
};
pub use identity::{
    FakeIdentityVerifier, GOOGLE_KEYS, GoogleIdentityVerifier, IdentityClaims, IdentityVerifier,
};
pub use launch::{
    ACQUIRE_LABEL, HostMaintenance, ImagePath, InsertRequest, Instance, InstanceId, InstanceKey,
    InstanceName, InstanceRef, InstanceStatus, LABEL_PREFIX, Label, ListFilter, NetworkInterface,
    Operation, OperationRef, OperationStatus, PAYLOAD_KEY, Page, Payload, ProvisioningModel,
    RequestId, SCHEDULER_LABEL, Scheduling, TerminationAction, host_ref, is_label_key,
    is_label_value, is_resource_name, label_value, parse_host_ref,
};
pub use model::{
    AcceleratorTypeInfo, AcceleratorTypeName, ArchName, AttachedAccelerator, MachineTypeInfo,
    MachineTypeName, Money, PricedItem, Project, QuotaMetric, Region, SeriesName, SkuPrice,
    UsageType, Zone,
};
pub use own::LabelError;
pub use provider::{GceProvider, PAYLOAD_LIMIT, PROVIDER_ID};
pub use refusal::launch_answer;
