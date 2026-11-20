//! The Xcodes a native run may name. A job start's `xcode` is a build id; the run gets the
//! `Contents/Developer` of the installed Xcode of that build as `DEVELOPER_DIR`, in its
//! environment, and the program it runs is the job's own. Nothing here selects an Xcode for
//! the host: no `xcode-select` is run, so the global selection is never changed.
//!
//! A start that names a build the runtime does not hold is refused before a user is made
//! ([`check`], through [`Runtime::check_xcode`](crate::lifecycle::Runtime::check_xcode)).

use std::collections::BTreeMap;
use std::path::PathBuf;

use js_agent_wire::refusal;

use super::NativeRuntime;
use crate::lifecycle::{NativeRefusal, RuntimeError};
use crate::xcode::Xcode;

/// The installed Xcodes by build: the app of each.
pub(super) type Table = BTreeMap<String, PathBuf>;

/// The table of `xcodes`.
pub(super) fn table(xcodes: Vec<Xcode>) -> Table {
    xcodes
        .into_iter()
        .map(|xcode| (xcode.build, xcode.app))
        .collect()
}

/// Whether the runtime holds an Xcode of build `build`.
///
/// # Errors
///
/// [`refusal::XCODE_NOT_HELD`], naming the build.
pub(super) fn check(held: &Table, build: &str) -> Result<(), NativeRefusal> {
    match held.contains_key(build) {
        true => Ok(()),
        false => Err(NativeRefusal {
            code: refusal::XCODE_NOT_HELD,
            why: format!("this agent holds no Xcode of build {build}"),
        }),
    }
}

/// The `DEVELOPER_DIR` of a run that names `build`: the `Contents/Developer` of its app;
/// none for a run that names no build.
///
/// # Errors
///
/// The build is not one the runtime holds (admission refused such a start already).
pub(super) fn developer_dir(
    held: &Table,
    build: Option<&str>,
) -> Result<Option<PathBuf>, RuntimeError> {
    let Some(build) = build else {
        return Ok(None);
    };
    match held.get(build) {
        Some(app) => Ok(Some(app.join("Contents/Developer"))),
        None => Err(RuntimeError::new(format!(
            "the Xcode of build {build} is not one this runtime holds"
        ))),
    }
}

impl NativeRuntime {
    /// The same runtime, holding `xcodes`: a start that names the build of one runs with that
    /// Xcode's `Contents/Developer` as `DEVELOPER_DIR`, and a start that names another build
    /// is refused ([`refusal::XCODE_NOT_HELD`]).
    #[must_use]
    pub fn with_xcodes(self, xcodes: Vec<Xcode>) -> Self {
        let xcodes = table(xcodes);
        Self { xcodes, ..self }
    }
}
