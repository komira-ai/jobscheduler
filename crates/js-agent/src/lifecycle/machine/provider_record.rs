//! What the agent writes down when it has asked its mount providers to prepare a run's
//! sources, so that a restart can release what may exist (`docs/design/mount-providers.md`,
//! section 4): `providers/<run>.json` in the state root, one file for each run that has
//! provider sources, holding the run's attempt and the provider and the index of each source.
//!
//! A source is named in every call by the agent, the run, the attempt and the index, so the
//! record needs no answer of a provider's to say what to release. A record written before
//! attempts were counted has none, and is read as attempt 0, which names its sources as they
//! were named then; an agent before attempts reads a record that has one and skips the field.
//!
//! It is written whole (a temporary file, synced, renamed over, the directory synced; the
//! directory is `0700`, the file `0600`) in the admission that sends the run's first
//! `Prepare`s, after them so that the syncs overlap the providers' work (a crash in between
//! is released by the sweep each provider is asked when the agent starts, which keeps only
//! the runs the agent holds: those it adopted and those whose lease the runtime left alone,
//! each from the attempt its record names), and removed once every source is released. An unfinished write (`<run>.json.tmp`) of a dead
//! process is removed without a finding; a file that cannot be read or understood is
//! removed and reported as a `provider-record-failed` finding.

use std::fs::{self, DirBuilder};
use std::io;
use std::os::unix::fs::DirBuilderExt;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use super::held::{TEMP_SUFFIX, read_capped, write_whole};
use crate::lifecycle::taint::StateRoot;
use crate::lifecycle::{Event, Finding, code};

/// The directory of the records, in the state root.
const DIR: &str = "providers";

/// The end of the name of a record.
const SUFFIX: &str = ".json";

/// The most bytes of a record that are read: a start names at most a few dozen sources, of
/// at most a few dozen bytes each.
const MAX: u64 = 1 << 20;

/// One source of a run, as the record names it: the provider that makes it and its place
/// among the run's layers, then its mounts, from 0.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(super) struct Source {
    /// The provider's name.
    pub(super) provider: String,
    /// The source's place.
    pub(super) index: u32,
}

/// A record as its file says it; fields it does not know are ignored.
#[derive(Serialize, Deserialize)]
struct Record {
    /// The run's attempt; 0 when the record has none.
    #[serde(default)]
    attempt: u64,
    sources: Vec<Source>,
}

/// One record read back: the run, its attempt and its sources.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct Recorded {
    /// The run.
    pub(super) run_id: String,
    /// The run's attempt; 0 when the record names none.
    pub(super) attempt: u64,
    /// The run's sources.
    pub(super) sources: Vec<Source>,
}

/// The directory of the records of the state root `state`.
fn dir_of(state: &StateRoot) -> PathBuf {
    state.path().join(DIR)
}

/// Writes the record of the attempt `attempt` of `run_id`: `sources`, durably.
///
/// # Errors
///
/// The directory could not be made or the file written.
pub(super) fn write(
    state: &StateRoot,
    (run_id, attempt): (&str, u64),
    sources: &[Source],
) -> io::Result<()> {
    let dir = dir_of(state);
    DirBuilder::new().recursive(true).mode(0o700).create(&dir)?;
    let record = Record {
        attempt,
        sources: sources.to_vec(),
    };
    serde_json::to_vec(&record)
        .map_err(io::Error::other)
        .and_then(|body| write_whole(&dir, &format!("{run_id}{SUFFIX}"), &body))
}

/// Removes the record of `run_id`; one that is not there is removed already. A record that
/// stays is read again by the next start of the agent, which releases its sources again.
pub(super) fn remove(state: &StateRoot, run_id: &str) {
    let _ = fs::remove_file(dir_of(state).join(format!("{run_id}{SUFFIX}")));
}

/// The records in the state root, in the order of their names. An unfinished write is
/// removed without a word; a record that cannot be used is removed and reported, and so is a
/// directory that cannot be listed.
pub(super) fn load(state: &StateRoot, events: &mut Vec<Event>) -> Vec<Recorded> {
    let dir = dir_of(state);
    let failed = |events: &mut Vec<Event>, what: &str, why: &str| {
        events.push(Event::Finding(Finding {
            run_id: None,
            code: code::PROVIDER_RECORD_FAILED,
            message: format!("{what}: {why}"),
        }));
    };
    let entries = match fs::read_dir(&dir) {
        Err(why) if why.kind() == io::ErrorKind::NotFound => return Vec::new(),
        Err(why) => {
            failed(events, "the provider records", &why.to_string());
            return Vec::new();
        }
        Ok(entries) => entries,
    };
    let mut names: Vec<String> = entries
        .filter_map(Result::ok)
        .map(|entry| entry.file_name().to_string_lossy().into_owned())
        .collect();
    names.sort();
    let mut found = Vec::new();
    for name in names {
        let path = dir.join(&name);
        if name.ends_with(TEMP_SUFFIX) {
            let _ = fs::remove_file(&path);
            continue;
        }
        let run = name.strip_suffix(SUFFIX).unwrap_or(&name).to_owned();
        let read = read_capped(&path, MAX)
            .map_err(|why| why.to_string())
            .and_then(|text| serde_json::from_str::<Record>(&text).map_err(|why| why.to_string()));
        match read {
            Ok(record) => found.push(Recorded {
                run_id: run,
                attempt: record.attempt,
                sources: record.sources,
            }),
            Err(why) => {
                let _ = fs::remove_file(&path);
                failed(events, &format!("the provider record {name}"), &why);
            }
        }
    }
    found
}
