//! The `gce` kind of a service's table of provider kinds: machines that are Compute Engine
//! instances.
//!
//! A machine provider of this kind is written in the service's configuration file as
//!
//! ```toml
//! [[machine_provider]]
//! name = "cloud"
//! kind = "gce"
//! project = "sample-project"
//! zones = ["us-central1-a", "us-central1-b"]
//! purchase = ["spot", "on-demand"]
//! network = "global/networks/default"
//! subnetwork = "regions/us-central1/subnetworks/default"
//! images = { x86_64 = "projects/sample-project/global/images/family/js-agent-x86" }
//! ```
//!
//! [`build`] reads the table the service hands it (everything but `name` and `kind`) and
//! the facts the pool decides, checks every rule that needs no network, and returns the
//! [`GceProvider`]. It makes no call: the client is built, not used. There is no
//! credential key: the client authenticates with the application-default credentials (the
//! key file `GOOGLE_APPLICATION_CREDENTIALS` names, else the service account of the machine
//! the service runs on), never with anything the table holds.
//!
//! The keys, each refusal naming the field relative to the provider table:
//!
//! - `project` (required): the project id the provider launches into;
//! - `zones` (required): the zones, in preference order, at least one, none twice;
//! - `purchase` (optional): the provisioning models in order, from `on-demand` and `spot`.
//!   When absent it is `["on-demand"]`;
//! - `network` (required): the VPC network every instance is attached to, as
//!   `global/networks/<name>` or `projects/<project>/global/networks/<name>`;
//! - `subnetwork` (required): its subnetwork, as `regions/<region>/subnetworks/<name>` or
//!   `projects/<project>/regions/<region>/subnetworks/<name>`. A subnetwork is regional, so
//!   every zone must be in its region. An instance gets no external address: the
//!   subnetwork gives it its way out (Cloud NAT, for one);
//! - `images` (required): a table from architecture to machine image. The keys are `arm64`
//!   (or its alias `aarch64`, which is read as `arm64`; naming both is refused) and
//!   `x86_64`, at least one of them, and each value is a global image
//!   (`projects/<project>/global/images/<image>`) or an image family
//!   (`projects/<project>/global/images/family/<family>`). An architecture the catalog
//!   offers and `images` lacks is not an error here: the provider refuses its acquires;
//! - `allowed_machine_types` (optional): the machine types the pool may launch and, in the
//!   catalog, offer. When absent the catalog lists the offers with an accelerator and the
//!   provider launches any type it is offered.
//!
//! The pool's `launch` setting decides the second lock the provider has: `live` builds it
//! enabled and live, anything else builds it disabled and in dry-run mode, so a scaler that
//! ignored `launch` still could not launch from it.

use std::fmt::Display;
use std::num::NonZeroUsize;
use std::sync::Arc;
use std::time::Duration;

use js_elastic::SchedulerId;
use js_logstore::{SystemClock as SystemWallClock, UreqTransport};
use toml::{Table, Value};

use crate::api::GceApi;
use crate::catalog::GceCatalogSettings;
use crate::clock::Clock;
use crate::config::{GceConfig, Images};
use crate::error::SettingsError;
use crate::http::{CredentialError, Credentials, HttpGce, KEY_FILE_VARIABLE};
use crate::launch::{ImagePath, NetworkInterface, is_resource_name};
use crate::model::{MachineTypeName, Project, Zone};
use crate::provider::GceProvider;

/// How long one call to the cloud may take, connect and TLS included.
pub const API_TIMEOUT: Duration = Duration::from_secs(30);

/// The fields of a `gce` provider table (`name` and `kind` are the service's and are not
/// passed).
const KNOWN: &[&str] = &[
    "project",
    "zones",
    "purchase",
    "network",
    "subnetwork",
    "images",
    "allowed_machine_types",
];

/// The provisioning models of a table that has no `purchase`.
const DEFAULT_PURCHASE: &str = "on-demand";

/// Why a network value is refused.
const NETWORK_FORM: &str =
    "must be global/networks/<name> or projects/<project>/global/networks/<name>";

/// Why a subnetwork value is refused.
const SUBNETWORK_FORM: &str = "must be regions/<region>/subnetworks/<name> or projects/<project>/regions/<region>/subnetworks/<name>";

/// Why an image value is refused.
const IMAGE_FORM: &str = "must be projects/<project>/global/images/<image> or projects/<project>/global/images/family/<family>";

/// A table that [`build`] refused: the field at fault, relative to the provider table, and
/// why. The reason holds nothing the table said except what it names.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Refused {
    /// The field at fault, such as `project` or `images.x86_64`.
    pub field: String,
    /// What is wrong with it.
    pub reason: String,
}

fn refused(field: &str, reason: impl Display) -> Refused {
    Refused {
        field: field.to_owned(),
        reason: reason.to_string(),
    }
}

/// What the pool decides and the service supplies besides the table.
pub struct Context {
    /// The scheduler's id, which marks everything the provider launches.
    pub scheduler: SchedulerId,
    /// The most machines the provider holds: the pool's `max_instances`, as its own
    /// backstop.
    pub max_machines: NonZeroUsize,
    /// Whether the pool's `launch` is `live`.
    pub live: bool,
    /// The provider's monotonic clock.
    pub clock: Arc<dyn Clock>,
}

/// A provider built from a table.
pub struct Built {
    /// The provider.
    pub provider: GceProvider,
}

/// What the table says, checked.
struct Parsed {
    settings: GceCatalogSettings,
    config: GceConfig,
}

/// A string field, if the table has it.
fn string<'t>(table: &'t Table, field: &str) -> Result<Option<&'t str>, Refused> {
    match table.get(field) {
        None => Ok(None),
        Some(Value::String(text)) => Ok(Some(text.as_str())),
        Some(_) => Err(refused(field, "must be a string")),
    }
}

/// A string field the table must have.
fn required<'t>(table: &'t Table, field: &str) -> Result<&'t str, Refused> {
    string(table, field)?.ok_or_else(|| refused(field, "required"))
}

/// An array of strings, if the table has the field.
fn strings(table: &Table, field: &str) -> Result<Option<Vec<String>>, Refused> {
    let Some(value) = table.get(field) else {
        return Ok(None);
    };
    value
        .as_array()
        .and_then(|items| {
            items
                .iter()
                .map(|item| item.as_str().map(str::to_owned))
                .collect::<Option<Vec<String>>>()
        })
        .map(Some)
        .ok_or_else(|| refused(field, "must be an array of strings"))
}

/// The image path `value`, named `field` in a refusal.
fn image(field: &str, value: &Value) -> Result<ImagePath, Refused> {
    let text = value
        .as_str()
        .ok_or_else(|| refused(field, "must be a string"))?;
    ImagePath::new(text).ok_or_else(|| refused(field, IMAGE_FORM))
}

/// The images of the `images` table: one per architecture, `aarch64` read as `arm64`.
fn images(table: &Table) -> Result<Images, Refused> {
    let Some(value) = table.get("images") else {
        return Err(refused("images", "required"));
    };
    let Value::Table(rows) = value else {
        return Err(refused(
            "images",
            "must be a table from architecture to image",
        ));
    };
    if rows.contains_key("aarch64") & rows.contains_key("arm64") {
        return Err(refused("images.aarch64", "arm64 is given twice"));
    }
    let mut arm64 = None;
    let mut x86_64 = None;
    for (key, value) in rows {
        let field = format!("images.{key}");
        let slot = match key.as_str() {
            "arm64" | "aarch64" => &mut arm64,
            "x86_64" => &mut x86_64,
            _ => {
                return Err(refused(
                    &field,
                    "unknown architecture; known: arm64, aarch64, x86_64",
                ));
            }
        };
        *slot = Some(image(&field, value)?);
    }
    Images::new(arm64, x86_64)
        .map_err(|_| refused("images", "must name an image for arm64 or x86_64"))
}

/// The text after an optional `projects/<project>/` at the start of `text`, when what
/// comes before it (if anything) is a project.
fn after_project(text: &str) -> Option<&str> {
    match text.strip_prefix("projects/") {
        None => Some(text),
        Some(rest) => {
            let (project, rest) = rest.split_once('/')?;
            Project::new(project).ok().map(|_| rest)
        }
    }
}

/// The network `text`, as the insert names it.
fn network(text: &str) -> Result<String, Refused> {
    let name = after_project(text).and_then(|rest| rest.strip_prefix("global/networks/"));
    match name.is_some_and(is_resource_name) {
        true => Ok(text.to_owned()),
        false => Err(refused("network", NETWORK_FORM)),
    }
}

/// The subnetwork `text`, as the insert names it; every zone of `zones` must be in its
/// region.
fn subnetwork(text: &str, zones: &[Zone]) -> Result<String, Refused> {
    let parts = after_project(text)
        .and_then(|rest| rest.strip_prefix("regions/"))
        .and_then(|rest| rest.split_once("/subnetworks/"));
    let Some((region, _)) =
        parts.filter(|(region, name)| is_resource_name(region) & is_resource_name(name))
    else {
        return Err(refused("subnetwork", SUBNETWORK_FORM));
    };
    match zones.iter().find(|zone| zone.region().as_str() != region) {
        Some(zone) => Err(refused(
            "subnetwork",
            format!(
                "is in the region {region:?}, and the zone {:?} is not",
                zone.as_str()
            ),
        )),
        None => Ok(text.to_owned()),
    }
}

/// The refusal of a catalog setting: the error's text starts with the key it names.
fn settings_refusal(error: SettingsError) -> Refused {
    let text = error.to_string();
    let (field, reason) = text.split_once(": ").unwrap_or_default();
    refused(field, reason)
}

/// Checks the table and the context, and makes the settings of the catalog and the
/// configuration of the provider from them.
fn parse(table: &Table, ctx: &Context) -> Result<Parsed, Refused> {
    if let Some(unknown) = table.keys().find(|key| !KNOWN.contains(&key.as_str())) {
        let reason = format!("unknown field; known: {}", KNOWN.join(", "));
        return Err(refused(unknown, reason));
    }
    let project = required(table, "project")?;
    let zones = strings(table, "zones")?.ok_or_else(|| refused("zones", "required"))?;
    let purchase = strings(table, "purchase")?.unwrap_or_else(|| vec![DEFAULT_PURCHASE.to_owned()]);
    let allowed = strings(table, "allowed_machine_types")?;
    let zones: Vec<&str> = zones.iter().map(String::as_str).collect();
    let purchase: Vec<&str> = purchase.iter().map(String::as_str).collect();
    let settings = GceCatalogSettings::new(project, &zones, &purchase, allowed.clone())
        .map_err(settings_refusal)?;
    let network = network(required(table, "network")?)?;
    let subnetwork = subnetwork(required(table, "subnetwork")?, settings.zones())?;
    let images = images(table)?;
    let config = GceConfig::new(
        ctx.scheduler.clone(),
        settings.project().clone(),
        images,
        NetworkInterface {
            network,
            subnetwork,
        },
        ctx.max_machines,
    );
    let config = match allowed {
        Some(names) => config.allowed(names.into_iter().map(MachineTypeName).collect()),
        None => config,
    };
    let config = match ctx.live {
        true => config.enable().live(),
        false => config,
    };
    Ok(Parsed { settings, config })
}

/// The provider of `parsed` over `api`.
fn assemble(parsed: Parsed, ctx: &Context, api: Arc<dyn GceApi>) -> Built {
    Built {
        provider: GceProvider::new(api, Arc::clone(&ctx.clock), parsed.settings, parsed.config),
    }
}

/// The provider `table` describes, over the HTTPS client of the cloud, which reads the
/// application-default credentials once here ([`Credentials::standard`]).
///
/// # Errors
///
/// [`Refused`] naming the field when the table has a field it does not know, lacks a
/// required one, has a field of the wrong type or a value that breaks a rule of the module
/// documentation; and naming `GOOGLE_APPLICATION_CREDENTIALS` when that variable names a
/// key file that cannot be read as a service-account key.
pub fn build(table: &Table, ctx: &Context) -> Result<Built, Refused> {
    build_with(table, ctx, Credentials::standard)
}

/// [`build`] with the credential read by `credentials`, which is called once, after the
/// table has been checked.
fn build_with(
    table: &Table,
    ctx: &Context,
    credentials: impl FnOnce() -> Result<Credentials, CredentialError>,
) -> Result<Built, Refused> {
    let parsed = parse(table, ctx)?;
    let credentials = credentials().map_err(|error| refused(KEY_FILE_VARIABLE, error))?;
    let api = HttpGce::new(
        credentials,
        Arc::new(UreqTransport::new(API_TIMEOUT)),
        Arc::new(SystemWallClock),
    );
    Ok(assemble(parsed, ctx, Arc::new(api)))
}

/// The provider `table` describes, over `api` instead of the cloud's client. It checks the
/// same rules as [`build`] and reads no credential; tests give it the in-memory project.
///
/// # Errors
///
/// The same as [`build`], but for the credential.
pub fn build_over(table: &Table, ctx: &Context, api: Arc<dyn GceApi>) -> Result<Built, Refused> {
    Ok(assemble(parse(table, ctx)?, ctx, api))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::clock::ManualClock;

    fn context() -> Context {
        Context {
            scheduler: SchedulerId::new("one").unwrap(),
            max_machines: NonZeroUsize::new(1).unwrap(),
            live: false,
            clock: Arc::new(ManualClock::new()),
        }
    }

    fn table() -> Table {
        r#"
            project = "sample-project"
            zones = ["region-one-a"]
            network = "global/networks/default"
            subnetwork = "regions/region-one/subnetworks/default"
            images = { x86_64 = "projects/image-project/global/images/js-agent-x86" }
        "#
        .parse()
        .unwrap()
    }

    /// Asserts that a credential that cannot be read refuses a good table under the name of
    /// the variable that names the key file, with the credential error's words, and that a
    /// table that is faulty is refused for its own field before the credential is read (with
    /// the same failing credential, the refusal names the table's field, not the variable).
    #[test]
    fn a_credential_that_cannot_be_read_refuses_the_table_under_its_variable() {
        let unreadable = || Err(CredentialError::NotJson);
        let refused = build_with(&table(), &context(), unreadable).err().unwrap();
        assert_eq!(refused.field, "GOOGLE_APPLICATION_CREDENTIALS");
        assert_eq!(
            refused.reason,
            "GOOGLE_APPLICATION_CREDENTIALS: the key file is not JSON"
        );
        let mut faulty = table();
        faulty.remove("project");
        let refused = build_with(&faulty, &context(), unreadable).err().unwrap();
        assert_eq!(refused.field, "project");
    }
}
