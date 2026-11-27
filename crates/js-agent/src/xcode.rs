//! The Xcodes of a Mac, by build id, for the hello's capability `xcode`
//! ([`js_agent_wire::limits::XCODE_CAPABILITY`]), the simulator runtimes they offer (the
//! `sim.<platform>` capabilities, [`sim`]), and the jobs that name one.
//!
//! # Found
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
//! # Ready
//!
//! A found Xcode is offered only once it can run a job: [`survey`] puts each one to
//! [`js_mac_pool::xcode::Catalog::survey`], as the flag `BUILD=APP` of its build and app.
//! The catalog resolves the app (it and every directory above it must be root's, or the
//! owner of [`Site::owner`], and writable by no one else but the `wheel` and `admin`
//! groups) and asks the Xcode, under its own `DEVELOPER_DIR`, `xcodebuild -version` (which
//! must print the build the plist named), `xcodebuild -license check`, `xcodebuild
//! -checkFirstLaunchStatus` and `xcrun --find clang`; then which of `swiftc`, `metal` and
//! `simctl` its `xcrun` finds. A Mac with more Xcodes than the catalog takes (64) offers
//! none, and says so. Every question runs through the agent's [`CommandRunner`] with the
//! environment `DEVELOPER_DIR` alone and a deadline of [`QUESTION_LIMIT`]. An Xcode that
//! fails a question is left out with a warning that names its app, its build and why; the
//! others are the ready ones, ascending by build, each with its app's real path, which is
//! the app a job that names its build runs with. A host whose probe found no Xcode runs no
//! program.
//!
//! The hello's value is the builds of the ready Xcodes, ascending, joined with `,`, as many
//! as fit [`MAX_CAPABILITY_VALUE_LEN`] bytes ([`hello_value`]).

use std::collections::BTreeMap;
use std::io;
use std::path::{Path, PathBuf};
use std::time::Duration;

use js_agent_wire::is_xcode_build;
use js_agent_wire::limits::MAX_CAPABILITY_VALUE_LEN;
use js_bundle::XcodeComponent;
use js_mac_pool::xcode::{Catalog, HostXcode, Question, Reply, State, Unanswered, XcodeFlag};

use crate::facts::{CommandRunner, FileReader};

pub mod sim;

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
    let builds: Vec<&str> = xcodes.iter().map(|xcode| xcode.build.as_str()).collect();
    fit("xcode", "builds", &builds)
}

/// `items` joined with `,`, as many from the first as fit [`MAX_CAPABILITY_VALUE_LEN`]
/// bytes, and a warning naming the capability `key` and how many of its `what` were left
/// out when not all fit. No value when there are no items.
pub(crate) fn fit(key: &str, what: &str, items: &[&str]) -> (Option<String>, Option<String>) {
    let mut value = String::new();
    let mut taken = 0;
    for item in items {
        let length = value.len() + usize::from(taken > 0) + item.len();
        if length > MAX_CAPABILITY_VALUE_LEN {
            break;
        }
        if taken > 0 {
            value.push(',');
        }
        value.push_str(item);
        taken += 1;
    }
    let left_out = items.len() - taken;
    let warning = (left_out > 0).then(|| {
        format!(
            "{key}: {left_out} of {} {what} do not fit in {MAX_CAPABILITY_VALUE_LEN} bytes and \
             are left out of the hello",
            items.len()
        )
    });
    (Some(value).filter(|text| !text.is_empty()), warning)
}

/// How long one question of the survey (and one `simctl` listing) may take before it is
/// killed and counted as no answer. `xcodebuild` answers in about a second; a first
/// `simctl` call of a boot starts the simulator service and takes longer.
pub const QUESTION_LIMIT: Duration = Duration::from_secs(60);

/// The most bytes of a question's standard output that are read: far above what
/// `xcodebuild` prints, and above a `simctl list runtimes -j` of many runtimes, each with
/// its device types. An answer that prints more is no answer.
pub(crate) const ANSWER_BYTES: usize = 8 << 20;

/// Where a Mac's Xcodes are looked for, and whose they must be.
#[derive(Debug, Clone, Copy)]
pub struct Site<'a> {
    /// The directory the Xcodes are looked for in: [`APPLICATIONS`] outside a test.
    pub applications: &'a Path,
    /// The user that may own an app and the directories above it besides root: root (0)
    /// outside a test, which cannot make root-owned directories.
    pub owner: u32,
}

impl Site<'static> {
    /// The machine's own: [`APPLICATIONS`], owned by root.
    #[must_use]
    pub fn system() -> Self {
        Self {
            applications: Path::new(APPLICATIONS),
            owner: 0,
        }
    }
}

/// What the survey of a Mac found.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Surveyed {
    /// The ready Xcodes, ascending by build, each with its app's real path.
    pub ready: Vec<Xcode>,
    /// The hello's `sim.<platform>` capabilities: the key and its value ([`sim`]).
    pub sims: BTreeMap<String, String>,
    /// What to log: an app that was not read, an Xcode that is not ready and why, a
    /// simulator listing that failed.
    pub warnings: Vec<String>,
}

/// The Xcodes of `site` that can run a job, and the simulator runtimes they offer; see the
/// module documentation. The questions run through `runner`.
pub fn survey(files: &dyn FileReader, runner: &dyn CommandRunner, site: Site<'_>) -> Surveyed {
    let (found, mut warnings) = probe_in(files, site.applications);
    let mut flags = Vec::new();
    for xcode in &found {
        match XcodeFlag::parse(&format!("{}={}", xcode.build, xcode.app.display())) {
            Ok(flag) => flags.push(flag),
            Err(why) => warnings.push(format!(
                "{}: {why}; Xcode {} is not offered",
                xcode.app.display(),
                xcode.build
            )),
        }
    }
    let catalog = Catalog::survey(&flags, site.owner, |question| ask(runner, question));
    // The probe's builds are distinct, so the catalog refuses only more than its
    // `MAX_LISTED` (64) Xcodes; then nothing is offered, and the warning says why.
    let catalog = match catalog {
        Ok(catalog) => catalog,
        Err(why) => {
            warnings.push(format!(
                "the Xcodes cannot be surveyed ({why}); none is offered"
            ));
            return Surveyed {
                warnings,
                ..Surveyed::default()
            };
        }
    };
    let mut ready = Vec::new();
    let mut hosts: Vec<&HostXcode> = Vec::new();
    for xcode in catalog.all() {
        match &xcode.host {
            Some(host) => {
                ready.push(Xcode {
                    build: host.build().to_owned(),
                    app: PathBuf::from(host.app().as_str()),
                });
                hosts.push(host);
            }
            None => warnings.push(format!(
                "{}: Xcode {} is not offered: {}: {}",
                xcode.flag.app().as_str(),
                xcode.flag.build(),
                not_ready(xcode.state),
                xcode.reason
            )),
        }
    }
    let with_simctl = hosts
        .into_iter()
        .filter(|host| host.components().contains(&XcodeComponent::Simctl));
    let (sims, sim_warnings) = sim::runtimes(runner, with_simctl);
    warnings.extend(sim_warnings);
    Surveyed {
        ready,
        sims,
        warnings,
    }
}

/// The proof objects of `xcodes` (the ready ones a [`survey`] found) that a profile of the allow
/// read model grants ([`js_mac_pool::seatbelt::ReadGrant::Xcode`]): each is put to the catalog
/// again, as [`survey`] does, through `runner`, and one that is no longer ready is left out.
/// `owner` is the user that may own an app besides root ([`Site::owner`]). More Xcodes than
/// the catalog takes, or a build given twice, give none.
pub fn hosts(runner: &dyn CommandRunner, xcodes: &[Xcode], owner: u32) -> Vec<HostXcode> {
    let flags: Vec<XcodeFlag> = xcodes
        .iter()
        .filter_map(|xcode| {
            XcodeFlag::parse(&format!("{}={}", xcode.build, xcode.app.display())).ok()
        })
        .collect();
    let catalog = Catalog::survey(&flags, owner, |question| ask(runner, question));
    catalog
        .map(|catalog| {
            catalog
                .all()
                .iter()
                .filter_map(|xcode| xcode.host.clone())
                .collect()
        })
        .unwrap_or_default()
}

/// What a state that is not ready means, for the warning.
fn not_ready(state: State) -> &'static str {
    match state {
        State::LicenseNotAccepted => "its licence is not accepted",
        State::FirstLaunchNotRun => "its first launch has not run",
        State::Ready | State::Failed => "it did not answer the survey",
    }
}

/// Runs the survey's `question`: its standard output when it exits 0; [`Unanswered::Refused`]
/// when it exits with another status; [`Unanswered::NoAnswer`] when it cannot be started,
/// is killed, passes [`QUESTION_LIMIT`] or prints more than [`ANSWER_BYTES`].
fn ask(runner: &dyn CommandRunner, question: &Question<'_>) -> Reply {
    let env = question.env();
    let program = question.program.as_str();
    let answer = runner.run_streams(program, question.args, &env, ANSWER_BYTES, QUESTION_LIMIT);
    match answer {
        Ok(streams) if streams.dropped => Err(Unanswered::NoAnswer(format!(
            "{program}: printed more than {ANSWER_BYTES} bytes"
        ))),
        Ok(streams) => Ok(String::from_utf8_lossy(&streams.stdout).into_owned()),
        Err(why) => Err(unanswered(why)),
    }
}

/// A runner's error as the survey reads it: the runner says a program that ran and exited
/// with a status other than 0 as `<program>: exit status: <n>`, which is a refusal; any
/// other text (not started, killed by a signal, past its deadline) is no answer.
pub(crate) fn unanswered(why: String) -> Unanswered {
    match why.contains(": exit status: ") {
        true => Unanswered::Refused(why),
        false => Unanswered::NoAnswer(why),
    }
}
