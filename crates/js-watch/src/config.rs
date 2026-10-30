//! The watcher's configuration file. Everything the watcher needs is here, and a secret
//! is the path of a file, never the secret.
//!
//! ```toml
//! scheduler_id = "js"
//! cloud = "lambda"
//! status_url = "http://10.0.0.5:7300/v1/pools"
//! interval_secs = 60        # optional: time between ticks
//! stale_after_secs = 300    # optional: heartbeat alarm after this long without a tick
//! confirm_ticks = 2         # optional: ticks an instance must be unexplained
//! sweep_every_secs = 1800   # optional: time between sweeps for leftovers
//! repeat_after_secs = 3600  # optional: a holding alert is sent again this often
//!
//! [lambda]
//! api_key_file = "/run/credentials/js-watch/lambda-key"
//! api_url = "https://..."   # optional: Lambda's address by default
//!
//! [alerts]
//! webhook_url = "https://hooks.example.org/..."
//! webhook_token_file = "/run/credentials/js-watch/webhook-token"   # optional
//! ```

use std::path::{Path, PathBuf};
use std::time::Duration;

use serde::Deserialize;

use crate::watcher::Settings;

/// Why a configuration was refused. No variant repeats a value of the file.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ConfigError {
    /// The file cannot be read.
    #[error("{path}: cannot be read: {why}")]
    Read {
        /// The file.
        path: PathBuf,
        /// The operating system's reason.
        why: String,
    },
    /// The file is not TOML of this shape.
    #[error("{path}: {why}")]
    Syntax {
        /// The file.
        path: PathBuf,
        /// What is wrong, by line, key name and expected type; never the value.
        why: String,
    },
    /// A number that must be at least 1 is 0.
    #[error("{0} must be at least 1")]
    Zero(&'static str),
    /// The `[lambda]` table is missing and the cloud is Lambda.
    #[error("[lambda] is required")]
    NoLambda,
}

/// The clouds the watcher knows.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Cloud {
    /// Lambda Cloud.
    Lambda,
}

/// The `[lambda]` table.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LambdaTable {
    /// The file that holds the API key.
    pub api_key_file: PathBuf,
    /// The API's address, when it is not Lambda's own.
    pub api_url: Option<String>,
}

/// The `[alerts]` table.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AlertsTable {
    /// Where alerts are POSTed.
    pub webhook_url: String,
    /// The file that holds the bearer token, when the receiver wants one.
    pub webhook_token_file: Option<PathBuf>,
}

/// The configuration.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    /// The scheduler whose machines are watched.
    pub scheduler_id: String,
    /// The cloud.
    pub cloud: Cloud,
    /// The lease server's `GET /v1/pools`.
    pub status_url: String,
    interval_secs: Option<u64>,
    stale_after_secs: Option<u64>,
    confirm_ticks: Option<u32>,
    sweep_every_secs: Option<u64>,
    repeat_after_secs: Option<u64>,
    /// Lambda's table.
    pub lambda: Option<LambdaTable>,
    /// Where alerts go.
    pub alerts: AlertsTable,
}

impl Config {
    /// Reads and checks the file at `path`.
    ///
    /// # Errors
    ///
    /// [`ConfigError`] naming the file and what is wrong.
    pub fn load(path: &Path) -> Result<Self, ConfigError> {
        let text = std::fs::read_to_string(path).map_err(|why| ConfigError::Read {
            path: path.to_owned(),
            why: why.to_string(),
        })?;
        let config: Self = toml::from_str(&text).map_err(|why| ConfigError::Syntax {
            path: path.to_owned(),
            why: scrub(&text, &why),
        })?;
        let numbers = [
            ("interval_secs", config.interval_secs.unwrap_or(1)),
            ("stale_after_secs", config.stale_after_secs.unwrap_or(1)),
            (
                "confirm_ticks",
                u64::from(config.confirm_ticks.unwrap_or(1)),
            ),
            ("sweep_every_secs", config.sweep_every_secs.unwrap_or(1)),
            ("repeat_after_secs", config.repeat_after_secs.unwrap_or(1)),
        ];
        if let Some((name, _)) = numbers.iter().find(|(_, value)| *value == 0) {
            return Err(ConfigError::Zero(name));
        }
        Ok(config)
    }

    /// The time between ticks.
    pub fn interval(&self) -> Duration {
        Duration::from_secs(self.interval_secs.unwrap_or(60))
    }

    /// The watcher's settings.
    pub fn settings(&self) -> Settings {
        Settings {
            stale_after: Duration::from_secs(self.stale_after_secs.unwrap_or(300)),
            confirm_ticks: self.confirm_ticks.unwrap_or(2),
            sweep_every: Duration::from_secs(self.sweep_every_secs.unwrap_or(1800)),
            repeat_after: Duration::from_secs(self.repeat_after_secs.unwrap_or(3600)),
        }
    }

    /// The `[lambda]` table.
    ///
    /// # Errors
    ///
    /// [`ConfigError::NoLambda`] when the file has none.
    pub fn lambda(&self) -> Result<&LambdaTable, ConfigError> {
        self.lambda.as_ref().ok_or(ConfigError::NoLambda)
    }
}

/// What the parser said, without any value of the file: the line, and the key named by an
/// unknown or missing field, or the key on that line and the type the value was expected
/// to have. A parser message otherwise quotes the value it met, and the file may hold a URL
/// or a path there.
fn scrub(text: &str, why: &toml::de::Error) -> String {
    let before = why.span().map_or(0, |span| span.start);
    let line = text
        .get(..before)
        .map_or(0, |head| head.matches('\n').count());
    let key = text
        .lines()
        .nth(line)
        .and_then(|each| each.split_once('='))
        .map_or("", |(key, _)| key.trim());
    let message = why.message();
    let said = if message.starts_with("unknown field") || message.starts_with("missing field") {
        message.to_owned()
    } else {
        let expected = message
            .rsplit_once(", expected ")
            .map_or_else(String::new, |(_, expected)| {
                format!(", expected {expected}")
            });
        format!("{key:?} has a wrong type or form{expected}")
    };
    format!("line {}: {said}", line + 1)
}
