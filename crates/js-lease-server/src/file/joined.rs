//! The `joined` kind of `[[pool]]`: a fixed set of machines that run `js-agent` and join
//! by hand. The pool launches nothing, so it has no provider, no scaler, no spend caps and
//! no network mode; it books one shape for every host, and the hosts register with their
//! join tokens.
//!
//! ```toml
//! [[pool]]
//! kind  = "joined"
//! name  = "nodes"
//! class = "whole-node"
//! exec  = "container"                    # optional; absent or "none": no host contract
//! max_machines = 6                       # required; from 1 to 262144
//! join_token_file = "/etc/js/join-token" # optional shared pool token
//! heartbeat_interval = "10s"             # optional
//! grace              = "1m"              # optional
//! lost_retention     = "24h"             # optional
//! shape = { cpu_millis = 16000, memory_bytes = 68719476736 }
//! capabilities = { os = "linux", arch = "x86_64" }   # optional; what every host offers
//! ```
//!
//! A lease request is matched against the pool's shape, `capabilities` included (the
//! registry books every host as that shape), and a host is admitted only if its hello
//! reports every capability of the pool with the same value. A request that requires
//! `os=linux` therefore finds a place on a joined pool only if the pool names `os`.
//!
//! # Building one without the server's configuration file
//!
//! A program that embeds the server and keeps its own configuration builds the pool in
//! two steps, neither of which starts anything:
//!
//! 1. Fill a [`JoinedPool`] from its own settings (every field is public), or read one from
//!    a TOML table with [`JoinedPool::from_table`].
//! 2. Call [`JoinedPool::validate`], which checks every field and returns the registry's
//!    [`PoolConfig`] for the pool.
//!
//! The shared pool token, if the pool has one, is the caller's: `join_token_file` is a
//! field to carry, and the file is read when the server starts.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::time::Duration;

use js_agent_wire::{DEFAULT_GRACE_MS, DEFAULT_HEARTBEAT_INTERVAL_MS, limits};
use js_registry::{DEFAULT_LOST_RETENTION_MS, PoolConfig};
use js_types::{ClassId, ExecMode, ProviderId, Resources, Shape};

use super::{Pool, Read, Registering, Section, registry_pool};
use crate::agent_pool::{MAX_MACHINES, capabilities_fit, shipped_class};
use crate::agent_tokens::AgentTokens;
use crate::config::Token;
use crate::error::StartupError;

const KEYS: &[&str] = &[
    "kind",
    "name",
    "class",
    "shape",
    "capabilities",
    "exec",
    "max_machines",
    "heartbeat_interval",
    "grace",
    "lost_retention",
    "join_token_file",
];

const SHAPE_KEYS: &[&str] = &["cpu_millis", "memory_bytes", "gpus", "gpu_class"];

/// The keys of a pool that launches, which a joined pool refuses.
const LAUNCHING: &[&str] = &[
    "launch",
    "provider",
    "network",
    "max_instances",
    "max_spend_per_hour",
    "max_spend_per_day",
    "idle",
    "boot_timeout",
];

const LAUNCHES_NOTHING: &str =
    "a joined pool launches nothing: its machines are started by hand and join with js-agent";

const ID_RULE: &str = "must be 1 to 63 characters of [a-z0-9._-]";

/// A pool of hand-joined agents, as the configuration describes it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct JoinedPool {
    /// The pool's name: what a host names as `pool` in its hello, and the `provider` of
    /// its one-time join tokens.
    pub name: String,
    /// The pool's class, such as `whole-node`.
    pub class: String,
    /// The most machines the pool admits.
    pub max_machines: usize,
    /// The CPU every host books, in thousandths of a core.
    pub cpu_millis: u64,
    /// The memory every host books, in bytes.
    pub memory_bytes: u64,
    /// The GPUs every host books.
    pub gpus: u64,
    /// The class of those GPUs; present exactly when `gpus` is above zero.
    pub gpu_class: Option<String>,
    /// What every host offers besides its size: the capabilities a lease request is
    /// matched against, which each host's hello must report with the same values.
    pub capabilities: BTreeMap<String, String>,
    /// The execution mode the hosts must report, or `None` for no host contract.
    pub exec: Option<ExecMode>,
    /// How often a host heartbeats.
    pub heartbeat_interval: Duration,
    /// How long a silent host stays registered before it is lost.
    pub grace: Duration,
    /// How long a lost host that holds no lease is remembered.
    pub lost_retention: Duration,
    /// The file of the shared pool token, when the pool has one.
    pub join_token_file: Option<PathBuf>,
}

impl JoinedPool {
    /// Reads the pool from the table of one `[[pool]]`. It reads and checks the types of
    /// the keys and refuses the keys of a pool that launches, naming the key; the values
    /// themselves are checked by [`JoinedPool::validate`], which this calls.
    ///
    /// # Errors
    ///
    /// [`StartupError`] naming the pool and the field at fault.
    pub fn from_table(table: toml::Table) -> Result<Self, StartupError> {
        let head = match table.get("name").and_then(toml::Value::as_str) {
            Some(name) if ProviderId::new(name).is_ok() => format!("pool {name:?}: "),
            _ => "pool: ".to_owned(),
        };
        let mut s = Section::open(table, head);
        match s.string("kind")?.as_deref() {
            None | Some("joined") => {}
            Some(_) => {
                let reason =
                    "must be \"joined\"; a pool that launches names its kind under provider";
                return Err(s.fail("kind", reason));
            }
        }
        for key in LAUNCHING {
            if s.map.contains_key(*key) {
                return Err(s.fail(key, LAUNCHES_NOTHING));
            }
        }
        s.only(KEYS)?;
        let name = s.required_string("name")?;
        let class = s.required_string("class")?;
        let shape = match s.table("shape")? {
            Some(shape) => shape,
            None => return Err(s.fail("shape", "required")),
        };
        let mut sh = Section::open(shape, format!("{}shape.", s.head));
        sh.only(SHAPE_KEYS)?;
        let cpu_millis = needed(&mut sh, "cpu_millis")?;
        let memory_bytes = needed(&mut sh, "memory_bytes")?;
        let gpus = sh.integer("gpus")?.unwrap_or(0);
        let gpu_class = sh.string("gpu_class")?;
        let capabilities = capabilities_of(&mut s)?;
        let max_machines = match s.integer("max_machines")? {
            Some(most) => usize::try_from(most).unwrap_or(usize::MAX),
            None => return Err(s.fail("max_machines", "required")),
        };
        let exec = match s.string("exec")?.as_deref() {
            None | Some("none") => None,
            Some(other) => match other.parse::<ExecMode>() {
                Ok(mode) => Some(mode),
                Err(_) => return Err(s.fail("exec", "must be container, sandbox-exec or none")),
            },
        };
        let pool = Self {
            name,
            class,
            max_machines,
            cpu_millis,
            memory_bytes,
            gpus,
            gpu_class,
            capabilities,
            exec,
            heartbeat_interval: s.duration_or(
                "heartbeat_interval",
                Duration::from_millis(DEFAULT_HEARTBEAT_INTERVAL_MS),
            )?,
            grace: s.duration_or("grace", Duration::from_millis(DEFAULT_GRACE_MS))?,
            lost_retention: s.duration_or(
                "lost_retention",
                Duration::from_millis(DEFAULT_LOST_RETENTION_MS),
            )?,
            join_token_file: s.path("join_token_file")?,
        };
        pool.validate()?;
        Ok(pool)
    }

    /// Checks every field and builds the registry's pool: the booked shape, the host
    /// contract, the timing, and the dedicated settings of a whole-node class. Nothing is
    /// read from disk.
    ///
    /// # Errors
    ///
    /// [`StartupError`] naming the pool and the field at fault.
    pub fn validate(&self) -> Result<PoolConfig, StartupError> {
        let place = Section::open(toml::Table::new(), format!("pool {:?}: ", self.name));
        let id = match ProviderId::new(&self.name) {
            Ok(id) => id,
            Err(_) => return Err(place.fail("name", ID_RULE)),
        };
        let class = match ClassId::new(&self.class) {
            Ok(class) => shipped_class(class),
            Err(_) => return Err(place.fail("class", ID_RULE)),
        };
        if !(1..=MAX_MACHINES).contains(&self.max_machines) {
            let reason = format!("must be from 1 to {MAX_MACHINES}");
            return Err(place.fail("max_machines", &reason));
        }
        let caps = [
            ("cpu_millis", self.cpu_millis, limits::MAX_CPU_MILLIS),
            ("memory_bytes", self.memory_bytes, limits::MAX_MEMORY_BYTES),
            ("gpus", self.gpus, limits::MAX_GPUS),
        ];
        for (field, value, max) in caps {
            if value > max {
                let reason = format!("is above the wire's cap of {max}");
                return Err(place.fail(&format!("shape.{field}"), &reason));
            }
        }
        let booked = Resources::new(
            self.cpu_millis,
            self.memory_bytes,
            self.gpus,
            self.gpu_class.clone(),
        );
        let booked = match booked {
            Ok(booked) => booked,
            Err(err) => return Err(place.fail("shape", &err.to_string())),
        };
        if let Err(reason) = capabilities_fit(&self.capabilities) {
            return Err(place.fail("capabilities", &reason));
        }
        let shape = Shape::new(booked, self.capabilities.clone());
        let pool = registry_pool(&place, id, &class, shape, self.max_machines, self.exec)?;
        let pool = match pool.with_timing(millis(self.heartbeat_interval), millis(self.grace)) {
            Ok(pool) => pool,
            Err(err) => return Err(place.fail("grace", &format!("with heartbeat_interval: {err}"))),
        };
        match pool.with_lost_retention(millis(self.lost_retention)) {
            Ok(pool) => Ok(pool),
            Err(err) => Err(place.fail("lost_retention", &err.to_string())),
        }
    }
}

fn millis(span: Duration) -> u64 {
    u64::try_from(span.as_millis()).unwrap_or(u64::MAX)
}

/// The `capabilities` table: text values, as the agent pool file's `[capabilities]`.
fn capabilities_of(s: &mut Section) -> Result<BTreeMap<String, String>, StartupError> {
    let Some(table) = s.table("capabilities")? else {
        return Ok(BTreeMap::new());
    };
    let mut found = BTreeMap::new();
    for (key, value) in table {
        match value {
            toml::Value::String(text) => {
                found.insert(key, text);
            }
            _ => return Err(s.fail(&format!("capabilities.{key}"), "must be a string")),
        }
    }
    Ok(found)
}

/// The integer `key` of the shape, which the shape must give.
fn needed(sh: &mut Section, key: &str) -> Result<u64, StartupError> {
    match sh.integer(key)? {
        Some(value) => Ok(value),
        None => Err(sh.fail(key, "required")),
    }
}

/// Reads the `[[pool]]` table of a joined pool for the file: the pool, and the shared token
/// file when it names one. The pool registers agents and launches nothing.
pub(super) fn read(table: toml::Table, client: &Token) -> Result<Read, StartupError> {
    let joined = JoinedPool::from_table(table)?;
    let pool = joined.validate()?;
    let tokens = match &joined.join_token_file {
        Some(path) => match AgentTokens::open(path, client) {
            Ok(tokens) => tokens,
            Err(err) => {
                let place = format!("pool {:?}: join_token_file", joined.name);
                return Err(StartupError::at(place, err.reason()));
            }
        },
        None => AgentTokens::none(),
    };
    let registering = Registering {
        plan: None,
        pool,
        tokens,
        mint_provider: Some(joined.name.clone()),
    };
    Ok(Read {
        name: joined.name,
        pool: Pool::Elastic(Box::new(registering)),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn table(text: &str) -> toml::Table {
        text.parse().unwrap()
    }

    const MINIMAL: &str = "name = \"nodes\"\nclass = \"whole-node\"\n\
                           shape = { cpu_millis = 4000, memory_bytes = 8589934592 }\n";

    /// [`MINIMAL`] with `extra` before it, and a machine count unless `extra` gives one.
    fn with(extra: &str) -> toml::Table {
        let count = match extra.contains("max_machines") {
            true => "",
            false => "max_machines = 3\n",
        };
        table(&format!("{extra}\n{MINIMAL}{count}"))
    }

    fn refused(table: toml::Table) -> String {
        JoinedPool::from_table(table).unwrap_err().to_string()
    }

    /// The smallest joined pool parses with the wire's timing and no contract; a full one carries every field through to the registry's pool. It
    /// catches a key that is read and dropped and a default that drifts from the wire's.
    #[test]
    fn a_joined_pool_parses_with_defaults_and_with_every_field() {
        let small = JoinedPool::from_table(with("kind = \"joined\"")).unwrap();
        assert_eq!(small.max_machines, 3);
        assert_eq!(small.exec, None);
        assert!(small.capabilities.is_empty());
        assert_eq!(small.grace, Duration::from_millis(DEFAULT_GRACE_MS));
        assert_eq!(small.join_token_file, None);
        let full = JoinedPool::from_table(table(
            "name = \"gpus\"\nclass = \"gpu-host\"\nexec = \"container\"\nmax_machines = 6\n\
             heartbeat_interval = \"10s\"\ngrace = \"1m\"\nlost_retention = \"2h\"\n\
             join_token_file = \"/etc/js/token\"\n\
             capabilities = { os = \"linux\", arch = \"x86_64\" }\n\
             shape = { cpu_millis = 8000, memory_bytes = 1024, gpus = 2, gpu_class = \"nvidia-a100\" }\n",
        ))
        .unwrap();
        assert_eq!(full.max_machines, 6);
        assert_eq!(full.capabilities.len(), 2);
        assert_eq!(full.capabilities["os"], "linux");
        let mut bare = full.clone();
        bare.capabilities.clear();
        assert_ne!(full.validate().unwrap(), bare.validate().unwrap());
        assert_eq!(full.exec, Some(ExecMode::Container));
        assert_eq!(full.gpus, 2);
        assert_eq!(full.gpu_class.as_deref(), Some("nvidia-a100"));
        assert_eq!(full.heartbeat_interval, Duration::from_secs(10));
        assert_eq!(full.grace, Duration::from_secs(60));
        assert_eq!(full.lost_retention, Duration::from_secs(7200));
        assert_eq!(full.join_token_file, Some(PathBuf::from("/etc/js/token")));
        assert_ne!(full.validate().unwrap(), small.validate().unwrap());
        let none = JoinedPool::from_table(with("exec = \"none\"")).unwrap();
        assert_eq!(none.exec, None);
    }

    /// A caller that builds the struct from its own settings gets the same checks: the
    /// registry's pool for a good one, a refusal naming the field for a bad one. It
    /// catches a check that lives only in the table reader.
    #[test]
    fn a_pool_built_by_hand_is_validated() {
        let mut pool = JoinedPool::from_table(with("")).unwrap();
        assert!(pool.validate().is_ok());
        pool.max_machines = 0;
        let said = pool.validate().unwrap_err().to_string();
        assert_eq!(
            said,
            format!("pool \"nodes\": max_machines: must be from 1 to {MAX_MACHINES}")
        );
        pool.max_machines = 1;
        let long = "k".repeat(limits::MAX_CAPABILITY_KEY_LEN + 1);
        pool.capabilities.insert(long, "v".to_owned());
        let said = pool.validate().unwrap_err().to_string();
        assert!(said.starts_with("pool \"nodes\": capabilities: "), "{said}");
    }

    /// Each key of a pool that launches is refused by name with the reason that the pool
    /// launches nothing, and a kind other than joined is refused on `kind`. It catches a
    /// joined pool that accepts a spend cap or a network it would never use.
    #[test]
    fn the_keys_of_a_launching_pool_are_refused_by_name() {
        let launching = [
            ("launch", "\"plan\""),
            ("provider", "{ kind = \"static\" }"),
            ("network", "{ kind = \"default\" }"),
            ("max_instances", "4"),
            ("max_spend_per_hour", "\"1.00\""),
            ("max_spend_per_day", "\"2.00\""),
            ("idle", "\"10m\""),
            ("boot_timeout", "\"5m\""),
        ];
        for (key, value) in launching {
            let said = refused(with(&format!("{key} = {value}")));
            assert_eq!(said, format!("pool \"nodes\": {key}: {LAUNCHES_NOTHING}"));
        }
        let said = refused(with("kind = \"lambdalabs\""));
        assert!(
            said.starts_with("pool \"nodes\": kind: must be \"joined\""),
            "{said}"
        );
    }

    /// Every other mistake is refused naming the pool and the field, a missing
    /// `max_machines` among them. It catches a field that is checked by nobody, a count
    /// that defaults to the wire's cap, and a message that loses the pool.
    #[test]
    fn a_bad_field_is_refused_naming_it() {
        let cases = [
            ("bogus = 1\n", "pool \"nodes\": bogus: unknown field"),
            ("exec = \"vm\"\n", "pool \"nodes\": exec: must be"),
            (
                "max_machines = 0\n",
                "pool \"nodes\": max_machines: must be from 1",
            ),
            (
                "grace = \"15s\"\nheartbeat_interval = \"10s\"\n",
                "pool \"nodes\": grace: with heartbeat_interval",
            ),
            (
                "lost_retention = \"1s\"\n",
                "pool \"nodes\": lost_retention:",
            ),
            (
                "heartbeat_interval = \"soon\"\n",
                "pool \"nodes\": heartbeat_interval: must be a positive duration",
            ),
            ("kind = 3\n", "pool \"nodes\": kind: must be a string"),
            (
                "capabilities = { os = 1 }\n",
                "pool \"nodes\": capabilities.os: must be a string",
            ),
            (
                "capabilities = \"linux\"\n",
                "pool \"nodes\": capabilities: must be a table",
            ),
        ];
        for (extra, said) in cases {
            let got = refused(with(extra));
            assert!(got.starts_with(said), "{extra}: {got}");
        }
        let broken = [
            (
                "name = \"nodes\"\nclass = \"c\"\n",
                "shape = { cpu_millis = 1, memory_bytes = 1 }\n",
                "pool \"nodes\": max_machines: required",
            ),
            (
                "name = \"Nodes\"\nclass = \"c\"\nmax_machines = 3\n",
                "shape = { cpu_millis = 1, memory_bytes = 1 }\n",
                "pool \"Nodes\": name: must be 1 to 63",
            ),
            (
                "name = \"nodes\"\nclass = \"X\"\n",
                "shape = { cpu_millis = 1, memory_bytes = 1 }\n",
                "pool \"nodes\": class: must be 1 to 63",
            ),
            (
                "name = \"nodes\"\nclass = \"c\"\nmax_machines = 3\n",
                "",
                "pool \"nodes\": shape: required",
            ),
            (
                "name = \"nodes\"\nclass = \"c\"\nmax_machines = 3\n",
                "shape = { memory_bytes = 1 }\n",
                "pool \"nodes\": shape.cpu_millis: required",
            ),
            (
                "name = \"nodes\"\nclass = \"c\"\nmax_machines = 3\n",
                "shape = { cpu_millis = 1 }\n",
                "pool \"nodes\": shape.memory_bytes: required",
            ),
            (
                "name = \"nodes\"\nclass = \"c\"\nmax_machines = 3\n",
                "shape = { cpu_millis = 4294967297, memory_bytes = 1 }\n",
                "pool \"nodes\": shape.cpu_millis: is above",
            ),
            (
                "name = \"nodes\"\nclass = \"c\"\nmax_machines = 3\n",
                "shape = { cpu_millis = 1, memory_bytes = 35184372088833 }\n",
                "pool \"nodes\": shape.memory_bytes: is above",
            ),
            (
                "name = \"nodes\"\nclass = \"c\"\nmax_machines = 3\n",
                "shape = { cpu_millis = 1, memory_bytes = 1, gpus = 257, gpu_class = \"g\" }\n",
                "pool \"nodes\": shape.gpus: is above",
            ),
            (
                "name = \"nodes\"\nclass = \"c\"\nmax_machines = 3\n",
                "shape = { cpu_millis = 1, memory_bytes = 1, gpus = 1 }\n",
                "pool \"nodes\": shape:",
            ),
            (
                "name = \"nodes\"\nclass = \"c\"\nmax_machines = 3\n",
                "shape = { cpu_millis = 1, memory_bytes = 1, cores = 1 }\n",
                "pool \"nodes\": shape.cores: unknown field",
            ),
        ];
        for (head, tail, said) in broken {
            let got = refused(table(&format!("{head}{tail}")));
            assert!(got.starts_with(said), "{head}{tail}: {got}");
        }
    }
}
