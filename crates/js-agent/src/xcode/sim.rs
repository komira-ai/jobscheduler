//! The simulator runtimes a Mac's ready Xcodes offer, for the hello's `sim.<platform>`
//! capabilities: `sim.ios`, `sim.tvos`, `sim.watchos` and `sim.visionos`, each the versions
//! of the available runtimes of that platform, ascending and joined with `,` (`17.5,18.2`).
//!
//! Each ready Xcode whose `xcrun` finds `simctl` ([`crate::xcode::survey`]) is asked
//! `xcrun simctl list runtimes -j` under its own `DEVELOPER_DIR`, through the agent's
//! [`CommandRunner`] with that environment alone and a deadline of
//! [`QUESTION_LIMIT`](crate::xcode::QUESTION_LIMIT). The capabilities are the union of the
//! answers. A runtime counts when the listing says it `isAvailable`, its `identifier`
//! names one of the four platforms (`com.apple.CoreSimulator.SimRuntime.iOS-18-2`; visionOS
//! runtimes are named `xrOS`), and its `version` is `major.minor[.patch]`; any other entry
//! is passed over. A listing that cannot be run or read is left out with a warning that
//! names the Xcode's build and why. A platform with no available runtime has no key, and a
//! Mac with no ready Xcode that has `simctl` runs nothing here and reports none.

use std::collections::{BTreeMap, BTreeSet};

use js_bundle::DottedVersion;
use js_mac_pool::xcode::{HostXcode, XCRUN};
use serde_json::Value;

use crate::facts::CommandRunner;
use crate::xcode::{ANSWER_BYTES, QUESTION_LIMIT, fit};

/// The prefix of the capability key of each simulator platform: `sim.ios` and the like.
pub const SIM_PREFIX: &str = "sim.";

/// The arguments the runtimes are listed with.
pub const LIST_RUNTIMES: [&str; 4] = ["simctl", "list", "runtimes", "-j"];

/// The prefix of a runtime's `identifier`, before its platform.
const RUNTIME_PREFIX: &str = "com.apple.CoreSimulator.SimRuntime.";

/// The platform of a runtime identifier (what follows [`RUNTIME_PREFIX`], up to the first
/// `-`) and the capability key of its versions.
const PLATFORMS: [(&str, &str); 5] = [
    ("iOS", "sim.ios"),
    ("tvOS", "sim.tvos"),
    ("watchOS", "sim.watchos"),
    ("xrOS", "sim.visionos"),
    ("visionOS", "sim.visionos"),
];

/// The `sim.<platform>` capabilities of the Xcodes `with_simctl`, and the warnings to log;
/// see the module documentation.
pub fn runtimes<'a>(
    runner: &dyn CommandRunner,
    with_simctl: impl IntoIterator<Item = &'a HostXcode>,
) -> (BTreeMap<String, String>, Vec<String>) {
    let mut versions: BTreeMap<&'static str, BTreeSet<DottedVersion>> = BTreeMap::new();
    let mut warnings = Vec::new();
    for host in with_simctl {
        let listed = list(runner, host).and_then(|text| parse(&text));
        match listed {
            Ok(found) => {
                for (key, version) in found {
                    versions.entry(key).or_default().insert(version);
                }
            }
            Err(why) => warnings.push(format!(
                "Xcode {}: its simulator runtimes cannot be listed ({why}); none of its own \
                 is reported",
                host.build()
            )),
        }
    }
    let mut capabilities = BTreeMap::new();
    for (key, set) in versions {
        let shown: Vec<String> = set.into_iter().map(String::from).collect();
        let shown: Vec<&str> = shown.iter().map(String::as_str).collect();
        let (value, warning) = fit(key, "versions", &shown);
        warnings.extend(warning);
        capabilities.extend(value.map(|value| (key.to_owned(), value)));
    }
    (capabilities, warnings)
}

/// What `xcrun simctl list runtimes -j` prints under the `DEVELOPER_DIR` of `host`.
fn list(runner: &dyn CommandRunner, host: &HostXcode) -> Result<String, String> {
    let env = [("DEVELOPER_DIR", host.developer_dir().as_str())];
    let answer = runner.run_streams(XCRUN, &LIST_RUNTIMES, &env, ANSWER_BYTES, QUESTION_LIMIT);
    let streams = answer?;
    match streams.dropped {
        true => Err(format!("{XCRUN}: printed more than {ANSWER_BYTES} bytes")),
        false => Ok(String::from_utf8_lossy(&streams.stdout).into_owned()),
    }
}

/// The capability key and version of each available runtime the listing `text` names.
///
/// # Errors
///
/// The text is not JSON, or has no `runtimes` array.
fn parse(text: &str) -> Result<Vec<(&'static str, DottedVersion)>, String> {
    let value: Value = serde_json::from_str(text).map_err(|why| format!("not JSON: {why}"))?;
    let listed = value["runtimes"]
        .as_array()
        .ok_or_else(|| "no runtimes array".to_owned())?;
    Ok(listed.iter().filter_map(runtime).collect())
}

/// The capability key and version of one listed runtime, when it is available, of a known
/// platform, and of a readable version.
fn runtime(entry: &Value) -> Option<(&'static str, DottedVersion)> {
    if entry["isAvailable"].as_bool() != Some(true) {
        return None;
    }
    let identifier = entry["identifier"].as_str()?;
    let rest = identifier.strip_prefix(RUNTIME_PREFIX)?;
    let platform = rest.split('-').next().unwrap_or(rest);
    let key = PLATFORMS
        .iter()
        .find_map(|(name, key)| (*name == platform).then_some(*key))?;
    let version = DottedVersion::parse(entry["version"].as_str()?).ok()?;
    Some((key, version))
}
