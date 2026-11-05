//! The `[logs]` table of the configuration file: where live logs go
//! (`docs/design/live-logs.md`, section 6).
//!
//! It is the only way to set a sink: no flag sets one, and a server run from flags has none.
//! Every rule is checked at start, offline, and a refusal names the key and never repeats its
//! value. The two key files are read here, owner-only like every key file of the server; their
//! secrets are held in [`AccessKey`], whose `Debug` shows the key id only. Nothing here goes to
//! the network: a wrong key, role or endpoint shows later, as a mint failure or a refused read.

use std::fmt;
use std::path::Path;
use std::time::Duration;

use js_logmint::{Bucket, LogPrefix, RoleArn, StsEndpoint};
use js_logstore::Endpoint;
use serde_json::Value;

use super::Section;
use crate::error::StartupError;
use crate::kinds::SecretFiles;

/// The keys of `[logs]`, in the order a refusal of a key beside `sink = "none"` looks for them.
const LOGS_KEYS: &[&str] = &[
    "sink",
    "url",
    "region",
    "endpoint",
    "path_style",
    "read_key_file",
    "credentials",
    "sts_endpoint",
    "role_arn",
    "mint_key_file",
    "credential_ttl",
    "max_follow",
];

/// The shortest `credential_ttl`: STS refuses a session shorter than 15 minutes.
const SHORTEST_TTL: Duration = Duration::from_secs(15 * 60);

/// The longest `credential_ttl`: STS refuses a session longer than 12 hours.
const LONGEST_TTL: Duration = Duration::from_secs(12 * 3600);

/// `credential_ttl` when the file does not give it.
const DEFAULT_TTL: Duration = Duration::from_secs(3600);

/// `max_follow` when the file does not give it.
const DEFAULT_MAX_FOLLOW: u64 = 256;

/// The most `max_follow` may be.
const MOST_FOLLOW: u64 = 4096;

/// The longest access key id a key file may hold, in bytes.
const MAX_KEY_ID_LEN: usize = 128;

/// The longest secret access key a key file may hold, in bytes.
const MAX_SECRET_LEN: usize = 1024;

/// What a key file must hold.
const KEY_FILE_MUST: &str = "must hold JSON {\"access_key_id\": ..., \"secret_access_key\": ...} and nothing else: an id of 1 to 128 letters and digits and a secret of 1 to 1024 printable ASCII bytes";

/// What `url` must be.
const URL_MUST: &str =
    "must be s3://BUCKET or s3://BUCKET/PREFIX, the prefix with no leading or trailing /";

/// Where live logs go: the `[logs]` table.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LogsConfig {
    /// `sink = "none"`, the default and the only sink of a server run from flags: logs stay on
    /// the host.
    None,
    /// `sink = "object_store"`: logs go to the object store the table names.
    ObjectStore(Box<ObjectStoreLogs>),
}

impl LogsConfig {
    /// The sink's name as the file spells it: `none` or `object_store`.
    pub fn sink(&self) -> &'static str {
        match self {
            Self::None => "none",
            Self::ObjectStore(_) => "object_store",
        }
    }
}

impl crate::config::Config {
    /// Where live logs go: the `[logs]` table of the file, checked; no sink from flags.
    pub fn logs(&self) -> &LogsConfig {
        &self.logs
    }
}

/// An object-store sink, as `[logs]` describes it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ObjectStoreLogs {
    /// The bucket of `url`.
    pub bucket: Bucket,
    /// The log prefix of `url`, with its trailing `/` (empty for none).
    pub prefix: LogPrefix,
    /// The store agents put to and the server reads from: `endpoint` (required), with
    /// `region`, the bucket and `path_style`.
    pub endpoint: Endpoint,
    /// `read_key_file`: the key that lists and gets under the prefix.
    pub read_key: AccessKey,
    /// How a host's credential is minted (`credentials`).
    pub minting: Minting,
    /// `credential_ttl`: how long a host credential is asked to live; 1 hour unless set.
    pub credential_ttl: Duration,
    /// `max_follow`: the most `GetLogs` calls with `follow` waiting at once; 256 unless set.
    pub max_follow: u64,
}

/// How a host's credential is minted.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Minting {
    /// `credentials = "sts"`: the store's STS `AssumeRole`.
    Sts {
        /// `sts_endpoint`: `endpoint` unless the file gives another.
        endpoint: StsEndpoint,
        /// `role_arn`: the role assumed.
        role_arn: RoleArn,
        /// `mint_key_file`: the key that calls `AssumeRole`.
        key: AccessKey,
    },
}

/// A long-lived key read from an owner-only key file. Its `Debug` shows the key id only.
#[derive(Clone, PartialEq, Eq)]
pub struct AccessKey {
    access_key_id: String,
    secret_access_key: String,
}

impl AccessKey {
    /// The key from the bytes of a key file: a JSON object with exactly `access_key_id` (1 to
    /// 128 ASCII letters and digits) and `secret_access_key` (1 to 1024 printable ASCII bytes).
    /// `None` for anything else; the reason never quotes the file.
    fn parse(bytes: &[u8]) -> Option<Self> {
        let value: Value = serde_json::from_slice(bytes).ok()?;
        let object = value.as_object().filter(|object| object.len() == 2)?;
        let id = object.get("access_key_id")?.as_str()?;
        let secret = object.get("secret_access_key")?.as_str()?;
        let valid = (1..=MAX_KEY_ID_LEN).contains(&id.len())
            && id.bytes().all(|b| b.is_ascii_alphanumeric())
            && (1..=MAX_SECRET_LEN).contains(&secret.len())
            && secret.bytes().all(|b| b.is_ascii_graphic());
        valid.then(|| Self {
            access_key_id: id.to_owned(),
            secret_access_key: secret.to_owned(),
        })
    }

    /// The access key id: not secret, it names the key.
    pub fn access_key_id(&self) -> &str {
        &self.access_key_id
    }

    /// The secret access key. Secret: never print it.
    pub fn secret_access_key(&self) -> &str {
        &self.secret_access_key
    }
}

impl fmt::Debug for AccessKey {
    fn fmt(&self, out: &mut fmt::Formatter<'_>) -> fmt::Result {
        out.debug_struct("AccessKey")
            .field("access_key_id", &self.access_key_id)
            .finish_non_exhaustive()
    }
}

/// The `[logs]` table, or [`LogsConfig::None`] when the file has none.
pub(super) fn read_logs(
    table: Option<toml::Table>,
    files: &dyn SecretFiles,
) -> Result<LogsConfig, StartupError> {
    let Some(table) = table else {
        return Ok(LogsConfig::None);
    };
    let mut s = Section::open(table, "logs.".to_owned());
    s.only(LOGS_KEYS)?;
    match s.string("sink")?.as_deref() {
        None | Some("none") => match LOGS_KEYS.iter().find(|key| s.map.contains_key(**key)) {
            Some(key) => Err(s.fail(key, "not allowed with logs.sink = \"none\"")),
            None => Ok(LogsConfig::None),
        },
        Some("object_store") => {
            let logs = read_object_store(&mut s, files)?;
            Ok(LogsConfig::ObjectStore(Box::new(logs)))
        }
        Some(_) => Err(s.fail("sink", "must be object_store or none")),
    }
}

/// The keys of an object-store sink, checked key by key in the table's order.
fn read_object_store(
    s: &mut Section,
    files: &dyn SecretFiles,
) -> Result<ObjectStoreLogs, StartupError> {
    let url = s.required_string("url")?;
    let (bucket, prefix) = parse_url(&url).map_err(|reason| s.fail("url", &reason))?;
    let region = s.required_string("region")?;
    if !region_ok(&region) {
        return Err(s.fail("region", "must be 1 to 64 bytes of [a-z0-9-]"));
    }
    // No default: the server names no cloud's host itself, so the store is the one the
    // operator wrote.
    let store_url = s.required_string("endpoint")?;
    let path_style = s.boolean("path_style")?.unwrap_or(false);
    let endpoint = Endpoint::parse(&store_url, &region, bucket.as_str(), path_style)
        .map_err(|err| s.fail("endpoint", &err.to_string()))?;
    let read = read_key(s, "read_key_file", files)?;
    let credentials = s.string("credentials")?;
    if credentials.is_some_and(|kind| kind != "sts") {
        let reason = "must be sts: no other way of minting a host credential is built";
        return Err(s.fail("credentials", reason));
    }
    let sts_url = s.string("sts_endpoint")?.unwrap_or(store_url);
    let sts_endpoint = StsEndpoint::parse(&sts_url, &region)
        .map_err(|err| s.fail("sts_endpoint", &err.to_string()))?;
    let role_arn = s.required_string("role_arn")?;
    let role_arn = RoleArn::new(&role_arn).map_err(|err| s.fail("role_arn", &err.to_string()))?;
    let mint_key = read_key(s, "mint_key_file", files)?;
    let credential_ttl = s.duration("credential_ttl")?.unwrap_or(DEFAULT_TTL);
    if !(SHORTEST_TTL..=LONGEST_TTL).contains(&credential_ttl) {
        return Err(s.fail("credential_ttl", "must be from 15m to 12h"));
    }
    let max_follow = s.bounded("max_follow", 1, MOST_FOLLOW, DEFAULT_MAX_FOLLOW)?;
    Ok(ObjectStoreLogs {
        bucket,
        prefix,
        endpoint,
        read_key: read,
        minting: Minting::Sts {
            endpoint: sts_endpoint,
            role_arn,
            key: mint_key,
        },
        credential_ttl,
        max_follow,
    })
}

/// The bucket and the log prefix of `url`, `s3://BUCKET` or `s3://BUCKET/PREFIX`.
fn parse_url(url: &str) -> Result<(Bucket, LogPrefix), String> {
    let rest = url.strip_prefix("s3://").ok_or(URL_MUST)?;
    let (bucket, prefix) = match rest.split_once('/') {
        Some((_, "")) => return Err(URL_MUST.to_owned()),
        Some(split) => split,
        None => (rest, ""),
    };
    let bucket = Bucket::new(bucket).map_err(|err| err.to_string())?;
    let prefix = LogPrefix::new(prefix).map_err(|err| err.to_string())?;
    Ok((bucket, prefix))
}

/// Whether `region` is 1 to 64 bytes of `[a-z0-9-]`.
fn region_ok(region: &str) -> bool {
    (1..=64).contains(&region.len())
        && region
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
}

/// The key of the owner-only file the required path `key` names.
fn read_key(
    s: &mut Section,
    key: &str,
    files: &dyn SecretFiles,
) -> Result<AccessKey, StartupError> {
    let path = s.required_path(key)?;
    let bytes = files.read(&path).map_err(|reason| s.fail(key, &reason))?;
    AccessKey::parse(&bytes).ok_or_else(|| s.fail(key, &must_hold(&path)))
}

/// Why the key file at `path` was refused for what it holds.
fn must_hold(path: &Path) -> String {
    format!("{} {KEY_FILE_MUST}", path.display())
}
