//! The Lambda Cloud account of the live conformance suite: what the flags say, the table of
//! the `lambdalabs` kind they make, and the reader of the key file.
//!
//! It is included by `tests/live.rs`, which is only the `main` of the suite and is built with
//! the cargo feature `live`, and by `tests/live_cloud.rs`, an ordinary test target that
//! exercises everything here without a network: building a provider reads the key file and
//! sends nothing.

use std::num::NonZeroUsize;
use std::path::Path;
use std::sync::Arc;

use js_elastic::conformance::live::{LiveArgs, LiveCloud};
use js_elastic::{CapacityProvider, SchedulerId};
use js_provider_lambdalabs::kind::{self, Context, SecretFiles};
use js_provider_lambdalabs::{ApiKey, SystemClock};
use toml::{Table, Value};

/// The flags beyond the shared ones.
pub const EXTRA: &[&str] = &["key-file"];

/// Reads the key file the way the service reads its secret files: only a regular file that
/// its owner alone can read or write.
pub struct KeyFiles;

impl SecretFiles for KeyFiles {
    fn read(&self, path: &Path) -> Result<Vec<u8>, String> {
        // `ApiKey::from_file` applies the owner-only rule and its error names the path and the
        // mode, never the content; the bytes are then read for the kind, which parses them.
        ApiKey::from_file(path).map_err(|why| why.to_string())?;
        std::fs::read(path).map_err(|why| format!("{}: {why}", path.display()))
    }
}

/// The Lambda account the run launches in.
pub struct LambdaCloud {
    key_file: String,
    region: String,
    shape: String,
}

impl LambdaCloud {
    /// What the flags say. The instance type is also the offer id of the catalog.
    pub fn from_args(args: &LiveArgs, raw: &[String]) -> Result<Self, String> {
        if raw.iter().any(|arg| arg == "--zone") {
            return Err("--zone is for Compute Engine; Lambda takes --region".to_owned());
        }
        Ok(Self {
            key_file: args.required("key-file")?.to_owned(),
            region: args.location.clone(),
            shape: args.shape.clone(),
        })
    }

    /// The provider table of the `lambdalabs` kind.
    pub fn table(&self) -> Table {
        let text = |value: &str| Value::String(value.to_owned());
        let mut row = Table::new();
        row.insert("name".to_owned(), text(&self.shape));
        let mut table = Table::new();
        table.insert("api_key_file".to_owned(), text(&self.key_file));
        table.insert("types".to_owned(), Value::Array(vec![Value::Table(row)]));
        table.insert("regions".to_owned(), Value::Array(vec![text(&self.region)]));
        table
    }
}

impl LiveCloud for LambdaCloud {
    fn provider(
        &self,
        scheduler: &SchedulerId,
        machines: NonZeroUsize,
    ) -> Result<Box<dyn CapacityProvider>, String> {
        let files = KeyFiles;
        let ctx = Context {
            scheduler: scheduler.clone(),
            max_machines: machines,
            live: true,
            clock: Arc::new(SystemClock::new()),
            files: &files,
        };
        let built = kind::build(&self.table(), &ctx)
            .map_err(|refused| format!("{}: {}", refused.field, refused.reason))?;
        Ok(Box::new(built.provider))
    }
}
