//! The Xcodes installed on a Mac, by build id, for the hello's capability `xcode`
//! ([`js_agent_wire::limits::XCODE_CAPABILITY`]) and for the jobs that name one.
//!
//! The probe lists [`APPLICATIONS`] and takes each entry whose name starts with `Xcode` and
//! ends with `.app`: `Xcode-16.2.app` (the name hostkeeper installs under), `Xcode.app`,
//! `Xcode_beta.app` and the like. The name is not relied on for the version: every such app
//! is read for its build, from the `ProductBuildVersion` of its `Contents/version.plist`,
//! which must be a build id ([`js_agent_wire::is_xcode_build`]). An app whose plist cannot be
//! read or parsed, or whose build is missing or malformed, is left out with a warning that
//! names the app and why. Two apps with the same build (a link `Xcode.app` to
//! `Xcode-16.2.app`) are one Xcode: the first by name, ascending. A host without
//! [`APPLICATIONS`] has no Xcode and no warning; any other failure to list it is no Xcode
//! and a warning. The result is ascending by build.
//!
//! The probe only reads: it never writes, renames or removes anything, and it runs no
//! program (never `xcode-select`).
//!
//! The hello's value is the builds ascending joined with `,`, as many as fit
//! [`MAX_CAPABILITY_VALUE_LEN`] bytes ([`hello_value`]).

use std::io;
use std::path::{Path, PathBuf};

use js_agent_wire::is_xcode_build;
use js_agent_wire::limits::MAX_CAPABILITY_VALUE_LEN;

use crate::facts::FileReader;

/// The directory the Xcodes are looked for in.
pub const APPLICATIONS: &str = "/Applications";

/// The file of an app that names its build, below the app.
const VERSION_PLIST: &str = "Contents/version.plist";

/// An installed Xcode.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Xcode {
    /// Its build id (`16C5032a`), the `ProductBuildVersion` of its `version.plist`.
    pub build: String,
    /// Its app directory (`/Applications/Xcode-16.2.app`).
    pub app: PathBuf,
}

/// The Xcodes in [`APPLICATIONS`], ascending by build, and the warnings to log.
pub fn probe(files: &dyn FileReader) -> (Vec<Xcode>, Vec<String>) {
    probe_in(files, Path::new(APPLICATIONS))
}

/// The Xcodes in the directory `dir`, ascending by build, and the warnings to log; see the
/// module documentation.
pub fn probe_in(files: &dyn FileReader, dir: &Path) -> (Vec<Xcode>, Vec<String>) {
    let mut names = match files.entries(dir) {
        Ok(names) => names,
        Err(err) if err.kind() == io::ErrorKind::NotFound => return (Vec::new(), Vec::new()),
        Err(err) => {
            let warning = format!(
                "{}: cannot be listed ({err}); reporting no Xcode",
                dir.display()
            );
            return (Vec::new(), vec![warning]);
        }
    };
    names.sort();
    let mut found: Vec<Xcode> = Vec::new();
    let mut warnings = Vec::new();
    for name in &names {
        if !(name.starts_with("Xcode") && name.ends_with(".app")) {
            continue;
        }
        let app = dir.join(name);
        match build_of(files, &app) {
            Ok(build) => {
                if !found.iter().any(|xcode| xcode.build == build) {
                    found.push(Xcode { build, app });
                }
            }
            Err(why) => warnings.push(format!(
                "{}: {why}; not reported as an Xcode",
                app.display()
            )),
        }
    }
    found.sort_by(|a, b| a.build.cmp(&b.build));
    (found, warnings)
}

/// The build of the Xcode at `app`.
///
/// # Errors
///
/// The `version.plist` cannot be read or parsed, or names no build id.
fn build_of(files: &dyn FileReader, app: &Path) -> Result<String, String> {
    let path = app.join(VERSION_PLIST);
    let text = files
        .read(&path.to_string_lossy())
        .map_err(|why| format!("version.plist cannot be read ({why})"))?;
    let value = plist::Value::from_reader_xml(text.as_bytes())
        .map_err(|why| format!("version.plist cannot be parsed ({why})"))?;
    let build = value
        .as_dictionary()
        .and_then(|dictionary| dictionary.get("ProductBuildVersion"))
        .and_then(plist::Value::as_string)
        .ok_or_else(|| "version.plist has no ProductBuildVersion".to_owned())?;
    match is_xcode_build(build) {
        true => Ok(build.to_owned()),
        false => Err(format!("ProductBuildVersion {build:?} is not a build id")),
    }
}

/// The builds of `xcodes` joined with `,` (the order given, which [`probe`] makes
/// ascending); empty when there are none.
pub fn joined(xcodes: &[Xcode]) -> String {
    let builds: Vec<&str> = xcodes.iter().map(|xcode| xcode.build.as_str()).collect();
    builds.join(",")
}

/// The value of the hello's `xcode` capability for `xcodes`, and the warning to log: the
/// builds joined with `,`, as many from the first as fit [`MAX_CAPABILITY_VALUE_LEN`] bytes,
/// with a warning that says how many were left out when not all fit. No value when there
/// are no Xcodes.
pub fn hello_value(xcodes: &[Xcode]) -> (Option<String>, Option<String>) {
    let mut value = String::new();
    let mut taken = 0;
    for xcode in xcodes {
        let length = value.len() + usize::from(taken > 0) + xcode.build.len();
        if length > MAX_CAPABILITY_VALUE_LEN {
            break;
        }
        if taken > 0 {
            value.push(',');
        }
        value.push_str(&xcode.build);
        taken += 1;
    }
    let left_out = xcodes.len() - taken;
    let warning = (left_out > 0).then(|| {
        format!(
            "xcode: {left_out} of {} builds do not fit in {MAX_CAPABILITY_VALUE_LEN} bytes and \
             are left out of the hello",
            xcodes.len()
        )
    });
    (Some(value).filter(|text| !text.is_empty()), warning)
}
