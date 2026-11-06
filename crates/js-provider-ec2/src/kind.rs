//! The `ec2` kind of a service's table of provider kinds: a pool whose machines are AWS
//! EC2 instances.
//!
//! A pool of this kind is written in the service's configuration file as
//!
//! ```toml
//! provider = { kind = "ec2", region = "us-east-2",
//!              images = { x86_64 = "ami-0123456789abcdef0" },
//!              allowed_instance_types = ["m7i.large"] }
//! ```
//!
//! [`build`] reads the table the service hands it (everything but `kind`) and the facts
//! the pool decides, checks every rule that needs no network, and returns the
//! [`Ec2Provider`]. It makes no call: the client is built, not used. There is no credential
//! key: the client signs with the provider-standard variables or the machine's instance
//! role, never with anything the table holds.
//!
//! The keys, each refusal naming the field relative to the provider table:
//!
//! - `region` (required): the region code, the one the provider launches into;
//! - `images` (required): a table from architecture to machine image id. The keys are
//!   `arm64` (or its alias `aarch64`, which is read as `arm64`; naming both is refused) and
//!   `x86_64`, at least one of them, and each value is `ami-` and 8 to 17 lower-case
//!   hexadecimal digits. An architecture with no image is one the provider does not launch;
//! - `purchase` (optional): the purchase options in order, from `on-demand`, `spot` and
//!   `capacity-block`. When absent it is `["on-demand"]`;
//! - `allowed_instance_types` (optional): the instance types the pool may launch and, in
//!   the catalog, offer. When absent the catalog lists the types with a GPU and the
//!   provider launches any type it is offered;
//! - `capacity_block_hours` (optional): the length of the Capacity Blocks the catalog
//!   offers, required when `purchase` lists `capacity-block`.
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

use crate::api::Ec2Api;
use crate::catalog::Ec2CatalogSettings;
use crate::clock::Clock;
use crate::config::{Ec2Config, Images};
use crate::error::SettingsError;
use crate::http::{Credentials, HttpEc2};
use crate::launch::ImageId;
use crate::model::InstanceType;
use crate::provider::Ec2Provider;

/// How long one call to the cloud may take, connect and TLS included.
pub const API_TIMEOUT: Duration = Duration::from_secs(30);

/// The fields of an `ec2` provider table (`kind` is the service's and is not passed).
const KNOWN: &[&str] = &[
    "region",
    "purchase",
    "images",
    "allowed_instance_types",
    "capacity_block_hours",
];

/// The purchase options of a table that has no `purchase`.
const DEFAULT_PURCHASE: &str = "on-demand";

/// A table that [`build`] refused: the field at fault, relative to the provider table, and
/// why. The reason holds nothing the table said except what it names.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Refused {
    /// The field at fault, such as `region` or `images.x86_64`.
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
    pub provider: Ec2Provider,
}

/// What the table says, checked.
struct Parsed {
    settings: Ec2CatalogSettings,
    config: Ec2Config,
}

/// A string field, if the table has it.
fn string<'t>(table: &'t Table, field: &str) -> Result<Option<&'t str>, Refused> {
    match table.get(field) {
        None => Ok(None),
        Some(Value::String(text)) => Ok(Some(text.as_str())),
        Some(_) => Err(refused(field, "must be a string")),
    }
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

/// The length of the Capacity Blocks, if the table gives one.
fn block_hours(table: &Table) -> Result<Option<u32>, Refused> {
    let Some(value) = table.get("capacity_block_hours") else {
        return Ok(None);
    };
    value
        .as_integer()
        .and_then(|hours| u32::try_from(hours).ok())
        .map(Some)
        .ok_or_else(|| refused("capacity_block_hours", "must be a whole number of hours"))
}

/// The image id `value`, named `field` in a refusal.
fn image(field: &str, value: &Value) -> Result<ImageId, Refused> {
    let text = value
        .as_str()
        .ok_or_else(|| refused(field, "must be a string"))?;
    ImageId::new(text).ok_or_else(|| {
        refused(
            field,
            "must be ami- followed by 8 to 17 lower-case hexadecimal digits",
        )
    })
}

/// The images of the `images` table: one per architecture, `aarch64` read as `arm64`.
fn images(table: &Table) -> Result<Images, Refused> {
    let Some(value) = table.get("images") else {
        return Err(refused("images", "required"));
    };
    let Value::Table(rows) = value else {
        return Err(refused(
            "images",
            "must be a table from architecture to image id",
        ));
    };
    if rows.contains_key("aarch64") && rows.contains_key("arm64") {
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
    let region = string(table, "region")?.ok_or_else(|| refused("region", "required"))?;
    let images = images(table)?;
    let purchase = strings(table, "purchase")?.unwrap_or_else(|| vec![DEFAULT_PURCHASE.to_owned()]);
    let allowed = strings(table, "allowed_instance_types")?;
    let hours = block_hours(table)?;
    let purchase: Vec<&str> = purchase.iter().map(String::as_str).collect();
    let settings = Ec2CatalogSettings::new(region, &purchase, allowed.clone(), hours)
        .map_err(settings_refusal)?;
    let config = Ec2Config::new(
        ctx.scheduler.clone(),
        settings.region().clone(),
        images,
        ctx.max_machines,
    );
    let config = match allowed {
        Some(names) => config.allowed(names.into_iter().map(InstanceType).collect()),
        None => config,
    };
    let config = match ctx.live {
        true => config.enable().live(),
        false => config,
    };
    Ok(Parsed { settings, config })
}

/// The provider of `parsed` over `api`.
fn assemble(parsed: Parsed, ctx: &Context, api: Arc<dyn Ec2Api>) -> Built {
    Built {
        provider: Ec2Provider::new(api, Arc::clone(&ctx.clock), parsed.settings, parsed.config),
    }
}

/// The provider `table` describes, over the signed HTTPS client of the cloud.
///
/// # Errors
///
/// [`Refused`] naming the field when the table has a field it does not know, lacks
/// `region` or `images`, has a field of the wrong type or a value that breaks a rule of the
/// module documentation.
pub fn build(table: &Table, ctx: &Context) -> Result<Built, Refused> {
    let parsed = parse(table, ctx)?;
    let api = HttpEc2::new(
        parsed.settings.region().clone(),
        Credentials::standard(),
        Arc::new(UreqTransport::new(API_TIMEOUT)),
        Arc::new(SystemWallClock),
    );
    Ok(assemble(parsed, ctx, Arc::new(api)))
}

/// The provider `table` describes, over `api` instead of the cloud's client. It checks the
/// same rules as [`build`]; tests give it the in-memory account.
///
/// # Errors
///
/// The same as [`build`].
pub fn build_over(table: &Table, ctx: &Context, api: Arc<dyn Ec2Api>) -> Result<Built, Refused> {
    Ok(assemble(parse(table, ctx)?, ctx, api))
}
