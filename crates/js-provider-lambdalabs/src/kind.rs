//! The `lambdalabs` kind of a service's table of provider kinds: a pool whose machines are
//! Lambda Cloud GPU instances.
//!
//! A pool of this kind is written in the service's configuration file as
//!
//! ```toml
//! provider = { kind = "lambdalabs", api_key_file = "/run/credentials/js/lambda-key",
//!              types = [{ name = "gpu_1x_a10", gpu = "nvidia-a10", gpus = 1 }] }
//! ```
//!
//! with `regions`, `ssh_key` and `api_url` as optional keys. [`build`] reads the table the
//! service hands it (everything but `kind`) and the facts the pool decides, checks every
//! rule that needs no network, and returns the [`LambdaProvider`] with the facts the
//! service needs of it offline ([`Built`]). It makes no network call: the client is
//! built, not used; what only the network can say (a real type name, a key that works)
//! is for the service's checks.
//!
//! The rules, each refusal naming the field relative to the provider table:
//!
//! - a type's `name` is 1 to 64 lower case letters, digits or `_`, and no two types have
//!   one name;
//! - a type's `gpu` is a class in the agent's spelling ([`GpuClass::parse`]), and a pool
//!   has one class: every type names the class of the first;
//! - a type's `gpus` is a whole number from 1;
//! - `api_key_file` is read through the service's reader of secret files, never directly,
//!   and holds one key.
//!
//! The pool's `launch` setting decides the second lock the provider has: `live` builds it
//! enabled and live, anything else builds it disabled and in dry-run mode, so a scaler
//! that ignored `launch` still could not launch from it.

use std::fmt::Display;
use std::num::NonZeroUsize;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use js_elastic::{GpuClass, SchedulerId};
use toml::{Table, Value};

use crate::clock::Clock;
use crate::config::ElasticConfig;
use crate::http::HttpApi;
use crate::key::ApiKey;
use crate::options::{OptionRow, OptionTable};
use crate::provider::LambdaProvider;

/// The address of Lambda's API when the table has no `api_url`. The client appends the
/// API's path.
pub const DEFAULT_API_URL: &str = "https://cloud.lambdalabs.com";

/// How long one call to the API may take, connect and TLS included.
pub const API_TIMEOUT: Duration = Duration::from_secs(30);

/// The fields of a `lambdalabs` provider table (`kind` is the service's and is not passed).
const KNOWN: &[&str] = &["api_key_file", "regions", "types", "ssh_key", "api_url"];

/// The fields of one row of `types`.
const ROW_KNOWN: &[&str] = &["name", "gpu", "gpus"];

/// A table that [`build`] refused: the field at fault, relative to the provider table, and
/// why. The reason holds nothing the table said except what it names, and no secret.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Refused {
    /// The field at fault, such as `api_key_file` or `types[2].gpu`.
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

/// How [`build`] reads a secret file: the service's one reader, which refuses a file that
/// anyone else can read. A reason names the path and never the content.
pub trait SecretFiles {
    /// The bytes of the file at `path`, or why it was not read.
    ///
    /// # Errors
    ///
    /// The reason the file was refused.
    fn read(&self, path: &Path) -> Result<Vec<u8>, String>;
}

/// What the pool decides and the service supplies besides the table.
pub struct Context<'a> {
    /// The scheduler's id, which marks everything the provider launches.
    pub scheduler: SchedulerId,
    /// The most machines the provider holds: the pool's `max_instances`, as its own
    /// backstop.
    pub max_machines: NonZeroUsize,
    /// Whether the pool's `launch` is `live`.
    pub live: bool,
    /// The provider's monotonic clock.
    pub clock: Arc<dyn Clock>,
    /// How the key file is read.
    pub files: &'a dyn SecretFiles,
}

/// A provider and what the service learns of it offline.
pub struct Built {
    /// The provider.
    pub provider: LambdaProvider,
    /// The fewest GPUs of any type of the pool.
    pub gpus: u64,
    /// The class of the pool's GPUs.
    pub gpu_class: GpuClass,
}

/// A string field, if the table has it.
fn string<'t>(table: &'t Table, field: &str) -> Result<Option<&'t str>, Refused> {
    match table.get(field) {
        None => Ok(None),
        Some(Value::String(text)) => Ok(Some(text.as_str())),
        Some(_) => Err(refused(field, "must be a string")),
    }
}

/// A field of a row of `types`, named `at.field` in a refusal, that must be a string.
fn row_string<'t>(at: &str, row: &'t Table, field: &str) -> Result<&'t str, Refused> {
    let named = format!("{at}.{field}");
    match row.get(field) {
        None => Err(refused(&named, "required")),
        Some(Value::String(text)) => Ok(text.as_str()),
        Some(_) => Err(refused(&named, "must be a string")),
    }
}

/// The row `value` of `types`, named `at`, after the `earlier` ones.
fn type_row(at: &str, value: &Value, earlier: &[OptionRow]) -> Result<OptionRow, Refused> {
    let Value::Table(row) = value else {
        return Err(refused(at, "must be a table with name, gpu and gpus"));
    };
    if let Some(unknown) = row.keys().find(|key| !ROW_KNOWN.contains(&key.as_str())) {
        let reason = format!("unknown field; known: {}", ROW_KNOWN.join(", "));
        return Err(refused(&format!("{at}.{unknown}"), reason));
    }
    let name = row_string(at, row, "name")?;
    let named = (1..=64).contains(&name.len())
        && name
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_');
    if !named {
        let reason = "must be 1 to 64 lower case letters, digits or '_'";
        return Err(refused(&format!("{at}.name"), reason));
    }
    if earlier.iter().any(|each| each.instance_type == name) {
        let reason = format!("{name:?} is listed twice");
        return Err(refused(&format!("{at}.name"), reason));
    }
    let class = GpuClass::parse(row_string(at, row, "gpu")?)
        .map_err(|why| refused(&format!("{at}.gpu"), why))?;
    if let Some(first) = earlier.first().filter(|first| first.gpu_class != class) {
        let reason = format!(
            "a pool has one gpu class; {:?} is the class of types[1], not {:?}",
            first.gpu_class.as_str(),
            class.as_str()
        );
        return Err(refused(&format!("{at}.gpu"), reason));
    }
    let gpus = match row.get("gpus") {
        None => return Err(refused(&format!("{at}.gpus"), "required")),
        Some(Value::Integer(count)) => u32::try_from(*count).ok().filter(|count| *count >= 1),
        Some(_) => None,
    };
    let Some(gpus) = gpus else {
        let reason = "must be a whole number from 1";
        return Err(refused(&format!("{at}.gpus"), reason));
    };
    Ok(OptionRow {
        instance_type: name.to_owned(),
        gpu_class: class,
        gpus,
    })
}

/// What the `types` rows say.
struct Types {
    rows: Vec<OptionRow>,
    gpus: u64,
    gpu_class: GpuClass,
}

/// The rows of `types`, the fewest GPUs of any of them and their class.
fn types(table: &Table) -> Result<Types, Refused> {
    let items = match table.get("types") {
        None => return Err(refused("types", "required")),
        Some(Value::Array(items)) => items,
        Some(_) => return Err(refused("types", "must be an array of tables")),
    };
    let mut rows: Vec<OptionRow> = Vec::new();
    for (index, value) in items.iter().enumerate() {
        let row = type_row(&format!("types[{}]", index + 1), value, &rows)?;
        rows.push(row);
    }
    let Some(first) = rows.first() else {
        return Err(refused("types", "must list at least one type"));
    };
    let gpu_class = first.gpu_class.clone();
    let gpus = rows.iter().map(|row| u64::from(row.gpus)).min();
    Ok(Types {
        gpus: gpus.unwrap_or_default(),
        gpu_class,
        rows,
    })
}

/// The regions the table limits the pool to, if it does.
fn regions(table: &Table) -> Result<Option<Vec<&str>>, Refused> {
    let items = match table.get("regions") {
        None => return Ok(None),
        Some(Value::Array(items)) => items,
        Some(_) => return Err(refused("regions", "must be an array of strings")),
    };
    let mut names = Vec::new();
    for item in items {
        match item {
            Value::String(name) => names.push(name.as_str()),
            _ => return Err(refused("regions", "must be an array of strings")),
        }
    }
    Ok(Some(names))
}

/// The provider's configuration: its scheduler and ssh key, regions, and the second
/// lock of the pool's `launch`.
fn config(table: &Table, ctx: &Context<'_>) -> Result<ElasticConfig, Refused> {
    let scheduler = ctx.scheduler.clone();
    let config = match string(table, "ssh_key")? {
        Some(name) => ElasticConfig::new(scheduler, name, ctx.max_machines)
            .map_err(|why| refused("ssh_key", why))?,
        None => ElasticConfig::with_account_key(scheduler, ctx.max_machines),
    };
    let config = match regions(table)? {
        Some(only) => config
            .regions(&only)
            .map_err(|why| refused("regions", why))?,
        None => config,
    };
    Ok(match ctx.live {
        true => config.enable().live(),
        false => config,
    })
}

/// The provider `table` describes, with the facts the service needs of it offline.
///
/// # Errors
///
/// [`Refused`] naming the field when the table has a field it does not know, lacks
/// `api_key_file` or `types`, has a field of the wrong type or a value that breaks a rule
/// of the module documentation, names a key file the reader refuses or one that holds no
/// key, or gives an `api_url` the client cannot use.
pub fn build(table: &Table, ctx: &Context<'_>) -> Result<Built, Refused> {
    if let Some(unknown) = table.keys().find(|key| !KNOWN.contains(&key.as_str())) {
        let reason = format!("unknown field; known: {}", KNOWN.join(", "));
        return Err(refused(unknown, reason));
    }
    let file = string(table, "api_key_file")?.ok_or_else(|| refused("api_key_file", "required"))?;
    let Types {
        rows,
        gpus,
        gpu_class,
    } = types(table)?;
    let config = config(table, ctx)?;
    let path = PathBuf::from(file);
    let bytes = ctx
        .files
        .read(&path)
        .map_err(|why| refused("api_key_file", why))?;
    let key = ApiKey::parse(&path, &bytes).map_err(|why| refused("api_key_file", why))?;
    let url = string(table, "api_url")?.unwrap_or(DEFAULT_API_URL);
    let api =
        HttpApi::connect(url, &key, None, API_TIMEOUT).map_err(|why| refused("api_url", why))?;
    let provider = LambdaProvider::new(
        Arc::new(api),
        Arc::clone(&ctx.clock),
        OptionTable::from_rows(rows),
        config,
    );
    Ok(Built {
        provider,
        gpus,
        gpu_class,
    })
}
