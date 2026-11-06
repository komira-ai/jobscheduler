//! The `lambdalabs` kind of a service's table of provider kinds: a pool whose machines are
//! Lambda Cloud GPU instances.
//!
//! A pool of this kind is written in the service's configuration file as
//!
//! ```toml
//! provider = { kind = "lambdalabs", api_key_file = "/run/credentials/js/lambda-key",
//!              types = [{ name = "gpu_1x_a10" }] }
//! ```
//!
//! with `regions`, `ssh_key` and `api_url` as optional keys. [`build`] reads the table the
//! service hands it (everything but `kind`) and the facts the pool decides, checks every
//! rule that needs no network, and returns the [`LambdaProvider`] with the facts the
//! service needs of it offline ([`Built`]). It makes no network call: the client is
//! built, not used; what only the network can say (a type Lambda offers today, a key that
//! works) is for the service's checks.
//!
//! `types` is the allow list of instance types the pool may launch. The GPU facts of each
//! type come from the crate's catalog table (`catalog.rs`), and the pool floor is derived
//! from them: the fewest GPUs of any listed type, and the one canonical model they share.
//! A row may still carry `gpu` and `gpus`, as rows did before the facts moved into the
//! crate: they are accepted, not checked and ignored, and each such row logs one warning
//! naming the row and its type.
//!
//! The rules, each refusal naming the field relative to the provider table:
//!
//! - a type's `name` is 1 to 64 lower case letters, digits or `_`, and no two types have
//!   one name;
//! - a type's `name` is a type the catalog table knows. Here, unlike in the catalog the
//!   provider serves (where a listed type the table does not know is skipped with a
//!   warning), an unknown type is refused: without its facts the pool has no floor;
//! - a pool has one GPU model: every type has the canonical model of the first, and a
//!   refusal names both models;
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

use js_elastic::SchedulerId;
use js_types::Model;
use toml::{Table, Value};

use crate::catalog;

use crate::clock::Clock;
use crate::config::ElasticConfig;
use crate::http::HttpApi;
use crate::key::ApiKey;
use crate::options::{OptionRow, OptionTable, instance_type_name};
use crate::provider::LambdaProvider;

/// The address of Lambda's API when the table has no `api_url`. The client appends the
/// API's path.
pub const DEFAULT_API_URL: &str = "https://cloud.lambdalabs.com";

/// How long one call to the API may take, connect and TLS included.
pub const API_TIMEOUT: Duration = Duration::from_secs(30);

/// The fields of a `lambdalabs` provider table (`kind` is the service's and is not passed).
const KNOWN: &[&str] = &["api_key_file", "regions", "types", "ssh_key", "api_url"];

/// The fields of one row of `types`. `gpu` and `gpus` are the GPU facts rows carried
/// before the facts moved into the crate: accepted and ignored.
const ROW_KNOWN: &[&str] = &["name", "gpu", "gpus"];

/// The fields of [`ROW_KNOWN`] that are accepted and ignored.
const ROW_IGNORED: &[&str] = &["gpu", "gpus"];

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
    /// The fewest GPUs of any type of the pool, from the catalog table.
    pub gpus: u64,
    /// The canonical model of the pool's GPUs, from the catalog table.
    pub gpu_class: Model,
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

/// One row of `types` with what the catalog table says of its type.
struct TypeRow {
    row: OptionRow,
    gpus: u32,
    model: Model,
}

/// The row `value` of `types`, named `at`, after the `earlier` ones.
fn type_row(at: &str, value: &Value, earlier: &[TypeRow]) -> Result<TypeRow, Refused> {
    let Value::Table(row) = value else {
        return Err(refused(at, "must be a table with a name"));
    };
    if let Some(unknown) = row.keys().find(|key| !ROW_KNOWN.contains(&key.as_str())) {
        let reason = format!("unknown field; known: {}", ROW_KNOWN.join(", "));
        return Err(refused(&format!("{at}.{unknown}"), reason));
    }
    let name = row_string(at, row, "name")?;
    if !instance_type_name(name) {
        let reason = "must be 1 to 64 lower case letters, digits or '_'";
        return Err(refused(&format!("{at}.name"), reason));
    }
    if earlier.iter().any(|each| each.row.instance_type == name) {
        let reason = format!("{name:?} is listed twice");
        return Err(refused(&format!("{at}.name"), reason));
    }
    let Some((gpus, model)) = catalog::entry(name)
        .and_then(|entry| entry.accelerator())
        .map(|accelerator| (accelerator.count, accelerator.model))
    else {
        let reason = format!(
            "{name:?} is not an instance type this release knows the GPUs of; \
             the known types are listed in js-provider-lambdalabs"
        );
        return Err(refused(&format!("{at}.name"), reason));
    };
    if let Some(first) = earlier.first().filter(|first| first.model != model) {
        let reason = format!(
            "a pool has one gpu model; types[1] ({:?}) has {:?}, {name:?} has {:?}",
            first.row.instance_type,
            first.model.as_str(),
            model.as_str()
        );
        return Err(refused(&format!("{at}.name"), reason));
    }
    Ok(TypeRow {
        row: OptionRow {
            instance_type: name.to_owned(),
        },
        gpus,
        model,
    })
}

/// What the `types` rows say.
struct Types {
    rows: Vec<OptionRow>,
    gpus: u64,
    gpu_class: Model,
}

/// The rows of `types`, the fewest GPUs of any of them and their model, both from the
/// catalog table. A row that carries `gpu` or `gpus` logs one warning, once every row is
/// accepted.
fn types(table: &Table) -> Result<Types, Refused> {
    let items = match table.get("types") {
        None => return Err(refused("types", "required")),
        Some(Value::Array(items)) => items,
        Some(_) => return Err(refused("types", "must be an array of tables")),
    };
    let mut rows: Vec<TypeRow> = Vec::new();
    for (index, value) in items.iter().enumerate() {
        let row = type_row(&format!("types[{}]", index + 1), value, &rows)?;
        rows.push(row);
    }
    let Some(first) = rows.first() else {
        return Err(refused("types", "must list at least one type"));
    };
    let gpu_class = first.model.clone();
    let gpus = rows.iter().map(|row| u64::from(row.gpus)).min();
    for (index, (value, row)) in items.iter().zip(&rows).enumerate() {
        let carries = value
            .as_table()
            .is_some_and(|table| ROW_IGNORED.iter().any(|key| table.contains_key(*key)));
        let name = &row.row.instance_type;
        if carries {
            tracing::warn!(
                "types[{}] ({name}): gpu and gpus are ignored; the GPU facts of an instance \
                 type come from js-provider-lambdalabs, so remove them from the row",
                index + 1
            );
        }
    }
    Ok(Types {
        gpus: gpus.unwrap_or_default(),
        gpu_class,
        rows: rows.into_iter().map(|row| row.row).collect(),
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
