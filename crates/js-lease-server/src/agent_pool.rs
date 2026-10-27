//! The pool file of `--agent-pool-config`: what an operator decides about the hosts that
//! register themselves.

use std::collections::BTreeMap;
use std::path::Path;

use js_agent_wire::{DEFAULT_GRACE_MS, DEFAULT_HEARTBEAT_INTERVAL_MS, limits};
use js_registry::{DEFAULT_LOST_RETENTION_MS, PoolConfig};
use js_types::{Admission, ClassId, ProviderId, ResourceClass, Resources, Shape};
use serde::Deserialize;

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
}

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
    parse(&text).map_err(|reason| StartupError::new(flag, format!("{}: {reason}", path.display())))
}

/// The pool a file's text describes, or what is wrong with it.
pub(crate) fn parse(text: &str) -> Result<PoolConfig, String> {
    let raw: RawPool = toml::from_str(text).map_err(|err| err.to_string())?;
    let name = ProviderId::new(&raw.name).map_err(|err| format!("name: {err}"))?;
    let class = class(&raw.class)?;
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
    pool.with_lost_retention(retention)
        .map_err(|err| format!("lost_retention: {err}"))
}

/// The class a name stands for: a shipped class (`whole-node`, or `gpu-host`, kept as
/// an alias), or a class that books a whole host and asks nothing of the request
/// beyond what the pool's shape offers.
fn class(name: &str) -> Result<ResourceClass, String> {
    let id = ClassId::new(name).map_err(|err| format!("class: {err}"))?;
    let shipped = [ResourceClass::whole_node(), ResourceClass::gpu_host()];
    let found = shipped.into_iter().find(|class| *class.id() == id);
    Ok(found.unwrap_or_else(|| ResourceClass::new(id, Admission::BookAll, 0)))
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
    u64::try_from(parsed.as_millis()).map_err(|_| format!("{field}: is too long"))
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

    /// Asserts the class names: `whole-node` and `gpu-host` load as the shipped classes
    /// (both `Whole`, with their GPU minimums), and any other id loads as `BookAll`
    /// with no minimum. A defect that dropped a shipped name would load it as BookAll.
    #[test]
    fn class_names_load_the_shipped_classes() {
        assert_eq!(class("whole-node").unwrap(), ResourceClass::whole_node());
        assert_eq!(class("gpu-host").unwrap(), ResourceClass::gpu_host());
        let other = class("mac-host").unwrap();
        assert_eq!(other.admission(), &Admission::BookAll);
        assert_eq!(other.min_gpus(), 0);
        let loaded = parse(&MINIMAL.replace("mac-host", "whole-node")).unwrap();
        let named = parse(&MINIMAL.replace("mac-host", "gpu-host")).unwrap();
        assert_ne!(loaded, named);
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

    /// A duration too long to count in milliseconds is refused rather than wrapped; the
    /// flags' durations are whole seconds in a `u64`, so only the conversion can fail.
    #[test]
    fn a_duration_that_overflows_milliseconds_is_refused() {
        let text = Some("18446744073709551615s".to_string());
        assert!(millis("grace", text, 0).is_err());
    }
}
