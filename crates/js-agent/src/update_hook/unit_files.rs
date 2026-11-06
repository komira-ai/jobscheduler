//! The unit a release carries, and whether it differs from the one the previous release
//! carried: the comparison `prepare` makes so that `cutover` knows whether the agent restarts
//! on a new unit layout (systemd's `Slice`, `Delegate`, `DelegateSubgroup`, `ExecStart`...) and
//! waits for the unit's cgroup to empty before it starts.
//!
//! A release directory holds its unit under `units/`: `units/<unit>` for a systemd unit and
//! `units/<unit>.plist` for a launchd job ([`Manager::file_name`]). hostkeeper links the unit
//! of the release that runs into the system's directory, so the previous release's file is the
//! installed unit. A previous release that carries no unit (the agent was installed some other
//! way before) counts as a change, named [`INSTALLED_MISSING`].
//!
//! A systemd unit is compared as the list of its assignments: the `[Section]` and `Key=value`
//! lines, with comments, blank lines and the spacing around a key and a value taken away and a
//! line ending in a backslash joined to the next. The values of one key are compared in order.
//! A line before the first section or without `=` is ignored, as systemd ignores it. A plist is
//! compared as the bytes it is.

use std::collections::{BTreeMap, BTreeSet};
use std::io;
use std::path::{Path, PathBuf};

use super::record::UnitRestart;

/// The name the plan gives to the change when the previous release carries no unit file.
pub const INSTALLED_MISSING: &str = "(installed unit missing)";

/// The directory of a release that holds its units.
const UNITS_DIR: &str = "units";

/// Which unit manager runs the agent, and so what the unit file is.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Manager {
    /// systemd: the unit is a unit file.
    Systemd,
    /// launchd: the unit is a property list.
    Launchd,
}

impl Manager {
    /// The file of `unit` in a release's `units` directory.
    #[must_use]
    pub fn file_name(self, unit: &str) -> String {
        match self {
            Manager::Systemd => unit.to_owned(),
            Manager::Launchd => format!("{unit}.plist"),
        }
    }

    /// The path of the unit file in the release directory `release`.
    #[must_use]
    pub fn path_in(self, release: &Path, unit: &str) -> PathBuf {
        release.join(UNITS_DIR).join(self.file_name(unit))
    }
}

/// Why the units of two releases could not be compared.
#[derive(Debug, thiserror::Error)]
pub enum PlanError {
    /// The staged release carries no unit file, or it cannot be read.
    #[error("the staged release carries no readable unit {}: {source}", path.display())]
    Staged {
        /// The file the staged release should hold.
        path: PathBuf,
        /// What the system said.
        source: io::Error,
    },
    /// The previous release's unit file is there and cannot be read.
    #[error("cannot read the installed unit {}: {source}", path.display())]
    Installed {
        /// The file.
        path: PathBuf,
        /// What the system said.
        source: io::Error,
    },
}

/// The change to the unit between the previous release and the staged one: `None` when the
/// two are the same, otherwise what differs. A first install (`previous` is `None`) has no
/// installed unit to differ from, but the staged release must still carry its unit.
///
/// # Errors
/// [`PlanError::Staged`] when the staged release has no readable unit, and
/// [`PlanError::Installed`] when the previous release's unit is there and cannot be read.
pub fn plan(
    manager: Manager,
    unit: &str,
    staged: &Path,
    previous: Option<&Path>,
) -> Result<Option<UnitRestart>, PlanError> {
    let new_path = manager.path_in(staged, unit);
    let new = std::fs::read(&new_path).map_err(|source| PlanError::Staged {
        path: new_path,
        source,
    })?;
    let Some(previous) = previous else {
        return Ok(None);
    };
    let old_path = manager.path_in(previous, unit);
    let old = match std::fs::read(&old_path) {
        Ok(old) => old,
        Err(source) if source.kind() == io::ErrorKind::NotFound => {
            return Ok(Some(UnitRestart {
                changed: vec![INSTALLED_MISSING.to_owned()],
            }));
        }
        Err(source) => {
            return Err(PlanError::Installed {
                path: old_path,
                source,
            });
        }
    };
    let changed = match manager {
        Manager::Launchd if old == new => Vec::new(),
        Manager::Launchd => vec![manager.file_name(unit)],
        Manager::Systemd => differing_keys(&assignments(&old), &assignments(&new)),
    };
    Ok((!changed.is_empty()).then_some(UnitRestart { changed }))
}

/// The assignments of a unit file as `(section, key)` and the values given, in order.
type Assignments = BTreeMap<(String, String), Vec<String>>;

/// The assignments of the unit file `bytes` (see the module header for the reading).
fn assignments(bytes: &[u8]) -> Assignments {
    let text = String::from_utf8_lossy(bytes);
    let mut found = Assignments::new();
    let mut section: Option<String> = None;
    let mut joined = String::new();
    for raw in text.lines() {
        let line = raw.trim();
        // A comment line is skipped wherever it is, inside a continuation too, as systemd does.
        if line.starts_with('#') || line.starts_with(';') {
            continue;
        }
        if let Some(cut) = line.strip_suffix('\\') {
            joined.push_str(cut.trim_end());
            joined.push(' ');
            continue;
        }
        joined.push_str(line);
        let whole = std::mem::take(&mut joined);
        let whole = whole.trim();
        if let Some(name) = whole
            .strip_prefix('[')
            .and_then(|rest| rest.strip_suffix(']'))
        {
            section = Some(name.to_owned());
        } else if let (Some(section), Some((key, value))) = (&section, whole.split_once('=')) {
            found
                .entry((section.clone(), key.trim().to_owned()))
                .or_default()
                .push(value.trim().to_owned());
        }
    }
    found
}

/// `Section.Key` of every assignment the two lists do not give the same values for, sorted.
fn differing_keys(old: &Assignments, new: &Assignments) -> Vec<String> {
    let keys: BTreeSet<&(String, String)> = old.keys().chain(new.keys()).collect();
    keys.into_iter()
        .filter(|key| old.get(*key) != new.get(*key))
        .map(|(section, key)| format!("{section}.{key}"))
        .collect()
}
