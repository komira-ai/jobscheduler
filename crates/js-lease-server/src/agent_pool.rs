//! The pool file of `--agent-pool-config`: what an operator decides about the hosts that
//! register themselves.
//!
//! The whole-node keys are parsed, checked and carried on the loaded [`PoolConfig`], which
//! the agent registry is built from and reads the keys of (see `WholeNodeConfig`). The
//! lease table is given `prepare_timeout` as well (see `Config::from_cli`), so an
//! operator's `prepare_timeout = "20m"` is how long a whole-node lease waits, pending, on
//! both sides. The keys are accepted only for class
//! `whole-node`; `gpu-host` books a whole host (`BookAll`) and refuses them.

use std::collections::BTreeMap;
use std::path::Path;

use js_agent_wire::{DEFAULT_GRACE_MS, DEFAULT_HEARTBEAT_INTERVAL_MS, limits};
use js_registry::{DEFAULT_LOST_RETENTION_MS, PoolConfig, WholeNodeConfig};
use js_types::{Admission, ClassId, ExecMode, ProviderId, ResourceClass, Resources, Shape};
use serde::Deserialize;
use tracing::warn;

use crate::config::parse_duration;
use crate::error::StartupError;

/// The most machines a pool may allow. It is the count at which the sums of the wire's
/// CPU and memory caps over all hosts still fit a `u64` (see `js_agent_wire::limits`).
pub(crate) const MAX_MACHINES: usize = 1 << 18;

/// The file, as written. Unknown keys are refused, so a typo is not a default.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawPool {
    name: String,
    class: String,
    max_machines: usize,
    shape: RawShape,
    #[serde(default)]
    capabilities: BTreeMap<String, String>,
    heartbeat_interval: Option<String>,
    grace: Option<String>,
    lost_retention: Option<String>,
    // `container` or `sandbox-exec`: the mode the pool's hosts run jobs in. Absent, the
    // pool requires no host contract, as before.
    exec: Option<String>,
    // The whole-node class (design whole-node.md, section 6). Every key is optional.
    max_whole_node: Option<usize>,
    max_draining: Option<usize>,
    max_whole_node_per_consumer: Option<usize>,
    keep_for_ordinary: Option<usize>,
    allow_last_capable: Option<bool>,
    drain_after: Option<String>,
    drain_deadline: Option<String>,
    reserve_backoff: Option<String>,
    max_reserve_attempts: Option<u32>,
    prepare_timeout: Option<String>,
    max_replace: Option<u32>,
    regain_grace: Option<String>,
}

impl RawPool {
    /// Whether the file sets any whole-node key.
    fn sets_whole_node_keys(&self) -> bool {
        let set = [
            self.max_whole_node.is_some(),
            self.max_draining.is_some(),
            self.max_whole_node_per_consumer.is_some(),
            self.keep_for_ordinary.is_some(),
            self.allow_last_capable.is_some(),
            self.drain_after.is_some(),
            self.drain_deadline.is_some(),
            self.reserve_backoff.is_some(),
            self.max_reserve_attempts.is_some(),
            self.prepare_timeout.is_some(),
            self.max_replace.is_some(),
            self.regain_grace.is_some(),
        ];
        set.into_iter().any(|key| key)
    }
}

/// Why a mixed pool (a class admitted `Book`) is refused at load.
pub(crate) const MIXED_UNSERVED: &str =
    "a mixed pool (a class that books part of a host) is not served by this server yet";

/// The `[shape]` table: what the pool promises and books for every host.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawShape {
    cpu_millis: u64,
    memory_bytes: u64,
    #[serde(default)]
    gpus: u64,
    gpu_class: Option<String>,
}

/// Reads the pool file at `path`.
pub(crate) fn read(path: &Path) -> Result<PoolConfig, StartupError> {
    let flag = "--agent-pool-config";
    let text = std::fs::read_to_string(path)
        .map_err(|err| StartupError::new(flag, format!("{}: {err}", path.display())))?;
    let pool = parse(&text)
        .map_err(|reason| StartupError::new(flag, format!("{}: {reason}", path.display())))?;
    if let Some(note) = surplus_warning(&pool) {
        warn!("{}: {note}", path.display());
    }
    Ok(pool)
}

/// What to tell the operator when a dedicated pool allows fewer whole hosts than
/// machines: the surplus hosts would serve nothing.
fn surplus_warning(pool: &PoolConfig) -> Option<String> {
    let surplus = pool.whole_surplus();
    (surplus > 0).then(|| {
        format!("max_whole_node is below max_machines, so {surplus} host(s) of the pool will serve nothing")
    })
}

/// The pool a file's text describes, or what is wrong with it.
pub(crate) fn parse(text: &str) -> Result<PoolConfig, String> {
    parse_with(text, class)
}

/// [`parse`] with the class names read by `classes`, so a test can offer a class no
/// file can name today.
fn parse_with(
    text: &str,
    classes: fn(&str) -> Result<ResourceClass, String>,
) -> Result<PoolConfig, String> {
    let raw: RawPool = toml::from_str(text).map_err(|err| err.to_string())?;
    let name = ProviderId::new(&raw.name).map_err(|err| format!("name: {err}"))?;
    let class = classes(&raw.class)?;
    let admission = class.admission().clone();
    let whole = whole_node(&raw, &admission)?;
    let machines = 1..=MAX_MACHINES;
    if !machines.contains(&raw.max_machines) {
        return Err(format!("max_machines must be from 1 to {MAX_MACHINES}"));
    }
    let shape = &raw.shape;
    within("shape.cpu_millis", shape.cpu_millis, limits::MAX_CPU_MILLIS)?;
    within(
        "shape.memory_bytes",
        shape.memory_bytes,
        limits::MAX_MEMORY_BYTES,
    )?;
    within("shape.gpus", shape.gpus, limits::MAX_GPUS)?;
    let resources = Resources::new(
        shape.cpu_millis,
        shape.memory_bytes,
        shape.gpus,
        raw.shape.gpu_class,
    )
    .map_err(|err| format!("shape: {err}"))?;
    capabilities_fit(&raw.capabilities)?;
    let interval = millis(
        "heartbeat_interval",
        raw.heartbeat_interval,
        DEFAULT_HEARTBEAT_INTERVAL_MS,
    )?;
    let grace = millis("grace", raw.grace, DEFAULT_GRACE_MS)?;
    let shape = Shape::new(resources, raw.capabilities);
    let pool = PoolConfig::new(name, class, shape, raw.max_machines)
        .with_timing(interval, grace)
        .map_err(|err| format!("heartbeat_interval and grace: {err}"))?;
    let retention = millis(
        "lost_retention",
        raw.lost_retention,
        DEFAULT_LOST_RETENTION_MS,
    )?;
    let pool = pool
        .with_lost_retention(retention)
        .map_err(|err| format!("lost_retention: {err}"))?;
    let pool = match whole {
        Some(whole) => pool.with_whole_node(whole).map_err(|err| err.to_string())?,
        None => pool,
    };
    let exec = raw.exec.as_deref().map(str::parse::<ExecMode>);
    let pool = match exec.transpose().map_err(|err| format!("exec: {err}"))? {
        Some(exec) => pool.with_exec(exec),
        None => pool,
    };
    match admission {
        Admission::Book(_) => Err(MIXED_UNSERVED.to_string()),
        _ => Ok(pool),
    }
}

/// The whole-node settings a file asks for: a `Whole` class takes the dedicated
/// defaults with the file's keys over them; a `Book` class (a mixed pool) takes them
/// only when `max_whole_node` is set; any other class takes none and refuses the keys.
fn whole_node(raw: &RawPool, admission: &Admission) -> Result<Option<WholeNodeConfig>, String> {
    let base = match (admission, raw.max_whole_node) {
        (Admission::Whole, _) => WholeNodeConfig::dedicated(raw.max_machines),
        (Admission::Book(_), Some(most)) => WholeNodeConfig::mixed(most),
        (Admission::Book(_), None) => return Ok(None),
        (Admission::BookAll, _) => {
            return match raw.sets_whole_node_keys() {
                true => Err("whole-node keys need a whole-node class".to_string()),
                false => Ok(None),
            };
        }
    };
    let deadline = raw.drain_deadline.clone();
    Ok(Some(WholeNodeConfig {
        max_whole_node: raw.max_whole_node.unwrap_or(base.max_whole_node),
        max_draining: raw.max_draining.unwrap_or(base.max_draining),
        max_whole_node_per_consumer: raw.max_whole_node_per_consumer,
        keep_for_ordinary: raw.keep_for_ordinary.unwrap_or(base.keep_for_ordinary),
        allow_last_capable: raw.allow_last_capable.unwrap_or(base.allow_last_capable),
        drain_after_ms: millis_or_zero("drain_after", &raw.drain_after, base.drain_after_ms)?,
        drain_deadline_ms: deadline
            .map(|text| millis("drain_deadline", Some(text), 0))
            .transpose()?,
        reserve_backoff_ms: millis(
            "reserve_backoff",
            raw.reserve_backoff.clone(),
            base.reserve_backoff_ms,
        )?,
        max_reserve_attempts: raw
            .max_reserve_attempts
            .unwrap_or(base.max_reserve_attempts),
        prepare_timeout_ms: millis(
            "prepare_timeout",
            raw.prepare_timeout.clone(),
            base.prepare_timeout_ms,
        )?,
        max_replace: raw.max_replace.unwrap_or(base.max_replace),
        regain_grace_ms: millis(
            "regain_grace",
            raw.regain_grace.clone(),
            base.regain_grace_ms,
        )?,
    }))
}

/// Like [`millis`], but `0` is a value: the setting is "none".
fn millis_or_zero(field: &str, text: &Option<String>, default: u64) -> Result<u64, String> {
    let zero = |text: &String| matches!(text.as_str(), "0" | "0s" | "0m" | "0h");
    match text.as_ref().is_some_and(zero) {
        true => Ok(0),
        false => millis(field, text.clone(), default),
    }
}

/// The class a name stands for: a shipped class (`whole-node`, or `gpu-host`, kept as
/// an alias), or a class that books a whole host and asks nothing of the request
/// beyond what the pool's shape offers.
fn class(name: &str) -> Result<ResourceClass, String> {
    let id = ClassId::new(name).map_err(|err| format!("class: {err}"))?;
    Ok(shipped_class(id))
}

/// The class `id` names: a shipped class, else one that books a whole host (see [`class`]).
pub(crate) fn shipped_class(id: ClassId) -> ResourceClass {
    let shipped = [ResourceClass::whole_node(), ResourceClass::gpu_host()];
    let found = shipped.into_iter().find(|class| *class.id() == id);
    found.unwrap_or_else(|| ResourceClass::new(id, Admission::BookAll, 0))
}

/// Refuses a `value` above `max`: a floor no hello could meet (the wire refuses a hello
/// above its caps), so the pool would admit nobody.
fn within(field: &str, value: u64, max: u64) -> Result<(), String> {
    match value <= max {
        true => Ok(()),
        false => Err(format!("{field} is above the wire's cap of {max}")),
    }
}

/// Refuses capabilities no hello could report: more than the wire allows, or a key or
/// value longer than it allows.
fn capabilities_fit(capabilities: &BTreeMap<String, String>) -> Result<(), String> {
    let few = capabilities.len() <= limits::MAX_CAPABILITIES;
    let short = capabilities.iter().all(|(key, value)| {
        let key_fits = key.len() <= limits::MAX_CAPABILITY_KEY_LEN;
        let value_fits = value.len() <= limits::MAX_CAPABILITY_VALUE_LEN;
        key_fits & value_fits
    });
    match few & short {
        true => Ok(()),
        false => Err("capabilities hold more or longer entries than the wire allows".to_string()),
    }
}

/// A duration setting in milliseconds: `text` read as the flags read durations, else
/// `default`.
fn millis(field: &str, text: Option<String>, default: u64) -> Result<u64, String> {
    let Some(text) = text else {
        return Ok(default);
    };
    let parsed = parse_duration(&text).map_err(|err| format!("{field}: {err}"))?;
    // A parsed duration is a `u64` count of milliseconds, so the conversion cannot fail.
    Ok(u64::try_from(parsed.as_millis()).unwrap_or(u64::MAX))
}

#[cfg(test)]
mod tests {
    use super::*;

    const MINIMAL: &str = "name = \"mac\"\nclass = \"mac-host\"\nmax_machines = 4\n\
                           [shape]\ncpu_millis = 4000\nmemory_bytes = 8589934592\n";

    fn with(extra: &str) -> String {
        // Top-level keys go before the first table, so splice them in front.
        format!("{extra}\n{MINIMAL}")
    }

    /// The smallest pool file parses with the wire's default timing, and a full one
    /// carries each setting through: the class, the shape with its GPU class and
    /// capabilities, and the three durations. It catches a setting that is read and
    /// dropped (a pool that ignores the operator's grace) and a default that drifts from
    /// the wire's.
    #[test]
    fn a_pool_file_parses_with_defaults_and_with_every_setting() {
        let minimal = parse(MINIMAL).unwrap();
        let full = parse(
            "name = \"gpus\"\nclass = \"gpu-host\"\nmax_machines = 8\n\
             heartbeat_interval = \"10s\"\ngrace = \"1m\"\nlost_retention = \"2h\"\n\
             [shape]\ncpu_millis = 8000\nmemory_bytes = 1024\ngpus = 2\ngpu_class = \"nvidia-a100\"\n\
             [capabilities]\nisa = \"x86-64-v3\"\n",
        )
        .unwrap();
        let timing = minimal
            .clone()
            .with_timing(DEFAULT_HEARTBEAT_INTERVAL_MS, DEFAULT_GRACE_MS);
        assert_eq!(timing.unwrap(), minimal);
        let kept = minimal
            .clone()
            .with_lost_retention(DEFAULT_LOST_RETENTION_MS);
        assert_eq!(kept.unwrap(), minimal);
        assert_ne!(full, minimal);
        let retained = full.clone().with_lost_retention(7_200_000).unwrap();
        assert_eq!(retained, full);
        let timed = full.clone().with_timing(10_000, 60_000).unwrap();
        assert_eq!(timed, full);
    }

    /// The `exec` key sets the mode the pool's hosts run jobs in, and a file without it
    /// leaves the pool requiring nothing of the host contract. It catches an `exec` that is
    /// read and dropped, and a default that would refuse every agent that predates the
    /// capabilities.
    #[test]
    fn exec_sets_the_host_contract_and_its_absence_leaves_none() {
        assert_eq!(parse(MINIMAL).unwrap().exec(), None);
        let container = parse(&with("exec = \"container\"")).unwrap();
        assert_eq!(container.exec(), Some(ExecMode::Container));
        let mac = parse(&with("exec = \"sandbox-exec\"")).unwrap();
        assert_eq!(mac.exec(), Some(ExecMode::SandboxExec));
    }

    /// A file in the temp directory that is removed when the guard drops, so a failing
    /// assertion does not leave it behind.
    struct TempFile(std::path::PathBuf);

    impl TempFile {
        fn new(key: &str, text: &str) -> Self {
            let name = format!("js-pool-{key}-{}.toml", std::process::id());
            let path = std::env::temp_dir().join(name);
            std::fs::write(&path, text).unwrap();
            Self(path)
        }
    }

    impl Drop for TempFile {
        fn drop(&mut self) {
            let _ = std::fs::remove_file(&self.0);
        }
    }

    /// Asserts the class names: `whole-node` loads as the shipped whole-node class
    /// (`Whole`) and `gpu-host` as the shipped GPU class (`BookAll`, one GPU minimum),
    /// and any other id loads as `BookAll` with no minimum. It also asserts that a pool
    /// file naming each class parses to a pool that leases exactly that class. A defect
    /// that dropped a shipped name would load it as `BookAll` with no minimum, and one
    /// that swapped the two would change the admission.
    #[test]
    fn class_names_load_the_shipped_classes() {
        let whole = class("whole-node").unwrap();
        assert_eq!(whole, ResourceClass::whole_node());
        assert_eq!(whole.admission(), &Admission::Whole);
        let gpu = class("gpu-host").unwrap();
        assert_eq!(gpu, ResourceClass::gpu_host());
        assert_eq!(gpu.admission(), &Admission::BookAll);
        assert_eq!(gpu.min_gpus(), 1);
        let other = class("mac-host").unwrap();
        assert_eq!(other.admission(), &Admission::BookAll);
        assert_eq!(other.min_gpus(), 0);
        for (name, expected) in [("whole-node", whole), ("gpu-host", gpu)] {
            let shape = Shape::new(
                Resources::new(4000, 8_589_934_592, 0, None).unwrap(),
                BTreeMap::new(),
            );
            let pool = PoolConfig::new(ProviderId::new("mac").unwrap(), expected, shape, 4);
            // A `Whole` class takes the dedicated defaults; a `BookAll` one takes none.
            let pool = match name {
                "whole-node" => pool.with_whole_node(WholeNodeConfig::dedicated(4)).unwrap(),
                _ => pool,
            };
            assert_eq!(parse(&MINIMAL.replace("mac-host", name)).unwrap(), pool);
        }
    }

    /// Every mistake is refused with a reason that names the setting: an unknown key, a
    /// bad name or class, a machine count of zero or above the cap, a shape above the
    /// wire's caps or with a GPU class that does not match, capabilities the wire could
    /// not carry, and durations that do not parse or that break the wire's grace rule.
    /// It catches a file that starts a pool no host could ever join.
    #[test]
    fn mistakes_are_refused_by_name() {
        let long = "x".repeat(limits::MAX_CAPABILITY_KEY_LEN + 1);
        let many = (0..=limits::MAX_CAPABILITIES)
            .map(|n| format!("k{n} = \"v\"\n"))
            .collect::<String>();
        let cases = [
            (with("speed = 1"), "speed"),
            (MINIMAL.replace("\"mac\"", "\"Mac Pool\""), "name"),
            (MINIMAL.replace("\"mac-host\"", "\"No Good\""), "class"),
            (
                MINIMAL.replace("max_machines = 4", "max_machines = 0"),
                "max_machines",
            ),
            (
                MINIMAL.replace("max_machines = 4", "max_machines = 999999"),
                "max_machines",
            ),
            (MINIMAL.replace("4000", "99999999999"), "shape.cpu_millis"),
            (
                MINIMAL.replace("8589934592", "99999999999999999"),
                "shape.memory_bytes",
            ),
            (
                format!("{MINIMAL}gpus = 999\ngpu_class = \"a\"\n"),
                "shape.gpus",
            ),
            (format!("{MINIMAL}gpus = 1\n"), "shape:"),
            (
                format!("{MINIMAL}[capabilities]\n{long} = \"v\"\n"),
                "capabilities",
            ),
            (format!("{MINIMAL}[capabilities]\n{many}"), "capabilities"),
            (with("grace = \"soon\""), "grace"),
            (with("heartbeat_interval = \"0s\""), "heartbeat_interval"),
            (with("grace = \"5s\""), "heartbeat_interval and grace"),
            (with("lost_retention = \"1s\""), "lost_retention"),
            (with("lost_retention = \"later\""), "lost_retention"),
            (with("exec = \"vm\""), "exec: unknown execution mode"),
            (with("exec = \"\""), "known: container, sandbox-exec"),
            (String::from("name = ["), "TOML"),
        ];
        for (text, names) in cases {
            let reason = parse(&text).unwrap_err();
            let named = reason.contains(names) | (names == "TOML");
            assert!(named, "{names}: {reason}");
        }
        let value_long = MINIMAL.to_string()
            + "[capabilities]\nk = \""
            + &"v".repeat(limits::MAX_CAPABILITY_VALUE_LEN + 1)
            + "\"\n";
        assert!(parse(&value_long).unwrap_err().contains("capabilities"));
    }

    /// A duration too long to count in milliseconds is refused with the field's name
    /// rather than wrapped.
    #[test]
    fn a_duration_that_overflows_milliseconds_is_refused() {
        let text = Some("18446744073709551615s".to_string());
        assert!(millis("grace", text, 0).unwrap_err().starts_with("grace:"));
    }

    /// Asserts that the heartbeat interval takes milliseconds down to the wire's 250 ms
    /// minimum and no lower, naming the field below it, and that second-based values read
    /// as before. It catches an `ms` suffix that is refused, a minimum that drifts from
    /// the wire's, and a unit scaled wrongly (`1s` read as 1 ms, or `250ms` as 250 s).
    #[test]
    fn heartbeat_interval_takes_milliseconds_down_to_the_wire_minimum() {
        let base = parse(MINIMAL).unwrap();
        let cases = [
            ("250ms", 250),
            ("1500ms", 1500),
            ("10s", 10_000),
            ("10", 10_000),
        ];
        for (text, millis) in cases {
            let got = parse(&with(&format!("heartbeat_interval = \"{text}\""))).unwrap();
            let want = base.clone().with_timing(millis, DEFAULT_GRACE_MS).unwrap();
            assert_eq!(got, want, "{text}");
        }
        let refused = parse(&with("heartbeat_interval = \"249ms\"")).unwrap_err();
        assert!(refused.contains("heartbeat_interval"), "{refused}");
    }

    fn whole(extra: &str) -> String {
        with(extra).replace("mac-host", "whole-node")
    }

    fn book(_: &str) -> Result<ResourceClass, String> {
        let id = ClassId::new("mixed").unwrap();
        Ok(ResourceClass::new(
            id,
            Admission::Book(Default::default()),
            0,
        ))
    }

    /// A whole-node pool file with no whole-node key takes the dedicated defaults of the
    /// design's table, key by key (so a changed default fails), and a file with every key
    /// carries each one through. It catches a key read and dropped, a default that
    /// drifts, and a unit slip (minutes read as seconds).
    #[test]
    fn a_whole_node_pool_takes_the_tables_defaults_and_every_key() {
        let pool = parse(&whole("")).unwrap();
        let w = pool.whole_node().unwrap();
        assert_eq!(*w, WholeNodeConfig::dedicated(4));
        assert_eq!(
            (w.max_whole_node, w.max_draining, w.keep_for_ordinary),
            (4, 1, 0)
        );
        assert_eq!(
            (w.max_whole_node_per_consumer, w.allow_last_capable),
            (None, false)
        );
        assert_eq!((w.drain_after_ms, w.drain_deadline_ms), (0, None));
        assert_eq!((w.max_reserve_attempts, w.max_replace), (2, 3));
        assert_eq!(w.prepare_timeout_ms, 600_000);
        let all = parse(&whole(
            "max_whole_node = 3\nmax_draining = 2\nmax_whole_node_per_consumer = 1\n\
             keep_for_ordinary = 0\nallow_last_capable = true\ndrain_after = \"1m\"\n\
             drain_deadline = \"2h\"\nreserve_backoff = \"30s\"\nmax_reserve_attempts = 5\n\
             prepare_timeout = \"20m\"\nmax_replace = 7\nregain_grace = \"45\"",
        ))
        .unwrap();
        let expected = WholeNodeConfig {
            max_whole_node: 3,
            max_draining: 2,
            max_whole_node_per_consumer: Some(1),
            keep_for_ordinary: 0,
            allow_last_capable: true,
            drain_after_ms: 60_000,
            drain_deadline_ms: Some(7_200_000),
            reserve_backoff_ms: 30_000,
            max_reserve_attempts: 5,
            prepare_timeout_ms: 1_200_000,
            max_replace: 7,
            regain_grace_ms: 45_000,
        };
        assert_eq!(all.whole_node(), Some(&expected));
    }

    /// A pool file written before the whole-node keys loads unchanged in every old field:
    /// a `gpu-host` file (class `BookAll`) equals the pool built from the old settings
    /// alone, with no whole-node settings, exactly as merged; and it refuses whole-node
    /// keys, which need the `whole-node` class.
    #[test]
    fn old_pool_files_load_unchanged() {
        let old = "name = \"gpus\"\nclass = \"gpu-host\"\nmax_machines = 8\n\
                   [shape]\ncpu_millis = 8000\nmemory_bytes = 1024\ngpus = 1\ngpu_class = \"a\"\n";
        let loaded = parse(old).unwrap();
        let shape = Shape::new(
            Resources::new(8000, 1024, 1, Some("a".to_string())).unwrap(),
            BTreeMap::new(),
        );
        let name = ProviderId::new("gpus").unwrap();
        let built = PoolConfig::new(name, ResourceClass::gpu_host(), shape, 8);
        assert_eq!(loaded, built);
        assert_eq!(loaded.whole_node(), None);
        let keyed = parse(&format!("max_whole_node = 2\n{old}")).unwrap_err();
        assert!(keyed.contains("whole-node class"), "{keyed}");
        assert_eq!(parse(MINIMAL).unwrap().whole_node(), None);
    }

    /// A dedicated pool that allows fewer whole hosts than machines loads, and the
    /// surplus is reported as a warning text that counts the idle hosts; a pool with no
    /// surplus reports nothing. Through `read` the same file still loads and the warning
    /// is a log line, not an error.
    #[test]
    fn a_surplus_is_a_warning_not_an_error() {
        let pool = parse(&whole("max_whole_node = 1")).unwrap();
        let note = surplus_warning(&pool).unwrap();
        assert!(note.contains("3 host(s)"), "{note}");
        assert_eq!(surplus_warning(&parse(&whole("")).unwrap()), None);
        // A call with no subscriber skips evaluating its fields, so the warning would go
        // unrun; this one runs it and drops the line.
        let scope = tracing::subscriber::set_default(
            tracing_subscriber::fmt()
                .with_writer(std::io::sink)
                .finish(),
        );
        for (key, text) in [
            ("surplus", whole("max_whole_node = 1")),
            ("full", whole("")),
        ] {
            let file = TempFile::new(key, &text);
            assert_eq!(read(&file.0).unwrap(), parse(&text).unwrap());
        }
        drop(scope);
    }

    /// `read` names its flag and the path when the file is missing and when its text is
    /// not a pool, so the operator learns which file and why. It catches a start-up
    /// error that drops the path or the reason.
    #[test]
    fn a_file_that_cannot_be_read_or_parsed_is_refused_with_its_path() {
        let dir = std::env::temp_dir();
        let missing = dir.join(format!("js-pool-missing-{}.toml", std::process::id()));
        let err = read(&missing).unwrap_err();
        assert_eq!(err.flag(), "--agent-pool-config");
        assert!(
            err.to_string().contains(&*missing.to_string_lossy()),
            "{err}"
        );
        let bad = TempFile::new("bad", "speed = 1\n");
        let err = read(&bad.0).unwrap_err();
        let shown = err.to_string();
        assert!(shown.contains(&*bad.0.to_string_lossy()), "{shown}");
        assert!(shown.contains("speed"), "{shown}");
    }

    /// Every whole-node mistake is refused and names the key: a zero maximum or count, a
    /// zero duration where zero means nothing, a kept host in a dedicated pool, whole-node
    /// keys on a class that books all, the removed `max_draining_fraction`, and a bad
    /// duration. `drain_after` accepts zero in each spelling, because zero reserves at
    /// once.
    #[test]
    fn whole_node_mistakes_are_refused_by_name() {
        let cases = [
            (whole("max_whole_node = 0"), "max_whole_node"),
            (whole("max_draining = 0"), "max_draining"),
            (
                whole("max_whole_node_per_consumer = 0"),
                "max_whole_node_per_consumer",
            ),
            (whole("max_reserve_attempts = 0"), "max_reserve_attempts"),
            (whole("max_replace = 0"), "max_replace"),
            (whole("keep_for_ordinary = 1"), "keep_for_ordinary"),
            (whole("reserve_backoff = \"0s\""), "reserve_backoff"),
            (whole("prepare_timeout = \"0\""), "prepare_timeout"),
            (whole("regain_grace = \"0m\""), "regain_grace"),
            (whole("drain_deadline = \"0\""), "drain_deadline"),
            (whole("drain_deadline = \"soon\""), "drain_deadline"),
            (whole("drain_after = \"soon\""), "drain_after"),
            (whole("drain_after = \"0ms\""), "drain_after"),
            (whole("drain_after = \"0smh\""), "drain_after"),
            (
                whole("max_draining_fraction = 0.5"),
                "max_draining_fraction",
            ),
            (with("max_draining = 2"), "whole-node class"),
            (whole("max_whole_node = 5"), "max_whole_node"),
        ];
        for (text, names) in cases {
            let reason = parse(&text).unwrap_err();
            assert!(reason.contains(names), "{names}: {reason}");
        }
        let above = parse(&whole("max_whole_node = 5")).unwrap_err();
        assert!(above.contains("max_whole_node"), "{above}");
        assert!(above.contains("max_machines"), "{above}");
        assert!(parse(&whole("max_whole_node = 4")).is_ok());
        for zero in ["0", "0s", "0m", "0h"] {
            let pool = parse(&whole(&format!("drain_after = \"{zero}\""))).unwrap();
            assert_eq!(pool.whole_node().unwrap().drain_after_ms, 0);
        }
    }

    /// A mixed pool (class admitted `Book`) is validated and then refused with one exact
    /// message: a file that fits, a file with no `max_whole_node`, and a file over the
    /// ceiling (which is refused for the ceiling first). It catches a mixed pool that
    /// loads and is silently served as something else.
    #[test]
    fn a_mixed_pool_is_validated_then_refused_as_not_served() {
        let mixed = |extra: &str| parse_with(&with(extra), book).unwrap_err();
        assert_eq!(mixed("max_whole_node = 3"), MIXED_UNSERVED);
        assert_eq!(mixed(""), MIXED_UNSERVED);
        assert!(mixed("max_whole_node = 4").contains("max_machines"));
        assert!(mixed("max_whole_node = 0").contains("max_whole_node"));
        assert_eq!(
            MIXED_UNSERVED,
            "a mixed pool (a class that books part of a host) is not served by this server yet"
        );
    }
}
