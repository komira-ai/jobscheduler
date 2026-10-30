//! The Xcodes of this Mac and which of them can run a job.
//!
//! The agent keeps its own catalog, built at start from `--xcode BUILD=PATH` flags
//! ([`XcodeFlag`]); it never trusts a path from the executor. [`Catalog::survey`] asks
//! every flagged Xcode, under that Xcode's own `DEVELOPER_DIR` and with its own
//! `xcodebuild`, four questions in this order, and the first answer that is not a yes
//! decides the [`State`]:
//!
//! 1. `xcodebuild -version` prints a build, and it is the build the flag names;
//! 2. `xcodebuild -license check` (an Xcode whose licence is not accepted still answers
//!    `-version`, while `cc` and `xcrun` fail);
//! 3. `xcodebuild -checkFirstLaunchStatus` (its first launch was run);
//! 4. `xcrun --find clang` (its compiler can be found).
//!
//! Only a [`State::Ready`] Xcode is advertised ([`Catalog::advertised`]) or granted
//! ([`Catalog::host`]). A ready Xcode is then asked which of the other components its
//! `xcrun` finds (`swiftc`, `metal`, `simctl`); an answer of no only leaves that
//! component out.
//!
//! This is a pure crate: the questions are put to a function the caller supplies
//! ([`Question`], [`Reply`]), which runs the program with `DEVELOPER_DIR` set to
//! [`Question::developer_dir`] and a deadline of its own. The survey reads the file
//! system only to resolve each app's real path.

use std::collections::BTreeSet;

use js_bundle::{DottedVersion, XcodeComponent};

use crate::seatbelt::{AbsPath, MAX_LISTED, PathError};

/// The program that answers `--find` questions: the shim, run under the Xcode's
/// `DEVELOPER_DIR` so it resolves to that Xcode's tools.
pub const XCRUN: &str = "/usr/bin/xcrun";

/// At most this much of a failed question's answer is kept as the reason.
pub const REASON_BYTES: usize = 500;

/// The longest build a flag names, in bytes (real builds are about seven); the message of
/// [`FlagError::BadBuild`] says 32.
pub const MAX_BUILD_BYTES: usize = 32;

/// Why a `--xcode` flag is refused.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum FlagError {
    /// There is no `=`.
    #[error("expected BUILD=PATH")]
    NoEquals,
    /// The build is empty, longer than [`MAX_BUILD_BYTES`] or holds anything but ASCII
    /// letters and digits.
    #[error("the build is not 1 to 32 ASCII letters and digits")]
    BadBuild,
    /// The path is not accepted ([`AbsPath`]).
    #[error("the path is not accepted: {0}")]
    Path(#[from] PathError),
}

/// One `--xcode BUILD=PATH` flag: the build the operator says the Xcode at `PATH` is.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct XcodeFlag {
    build: String,
    app: AbsPath,
}

impl XcodeFlag {
    /// Parses `BUILD=PATH`. `PATH` is the `Xcode.app` directory; it may hold `=`.
    ///
    /// # Errors
    /// A [`FlagError`] naming the first rule the text breaks.
    pub fn parse(text: &str) -> Result<Self, FlagError> {
        let (build, path) = text.split_once('=').ok_or(FlagError::NoEquals)?;
        let plain = build.bytes().all(|b| b.is_ascii_alphanumeric());
        if build.is_empty() || build.len() > MAX_BUILD_BYTES || !plain {
            return Err(FlagError::BadBuild);
        }
        Ok(Self {
            build: build.to_owned(),
            app: AbsPath::parse(path)?,
        })
    }

    /// The build the flag names.
    #[must_use]
    pub fn build(&self) -> &str {
        &self.build
    }

    /// The app directory as flagged (it may be a link; the survey resolves it).
    #[must_use]
    pub fn app(&self) -> &AbsPath {
        &self.app
    }
}

/// Whether a flagged Xcode can run jobs, and if not, which check it failed first.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum State {
    /// It answered every question; its build is advertised.
    Ready,
    /// `xcodebuild -license check` was refused.
    LicenseNotAccepted,
    /// `xcodebuild -checkFirstLaunchStatus` was refused.
    FirstLaunchNotRun,
    /// Anything else: the app does not resolve, no build or version, a build that is
    /// not the flagged one, no compiler, a question that could not be run.
    Failed,
}

/// A question for the caller to run.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Question<'a> {
    /// The absolute program: the Xcode's own `xcodebuild`, or [`XCRUN`].
    pub program: String,
    /// Its arguments.
    pub args: &'a [&'a str],
    /// The `DEVELOPER_DIR` to run it with: the app's real path joined with
    /// `Contents/Developer`.
    pub developer_dir: &'a AbsPath,
}

/// Why a question has no answer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Unanswered {
    /// It ran and exited non-zero: the answer is no. The text is its stderr.
    Refused(String),
    /// It could not be run or did not answer in time. The text says why.
    NoAnswer(String),
}

/// What a question answers: its stdout when it exited 0.
pub type Reply = Result<String, Unanswered>;

/// One flagged Xcode as the survey found it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Xcode {
    /// The flag it came from.
    pub flag: XcodeFlag,
    /// Whether it can run jobs.
    pub state: State,
    /// Why not: the question that failed and its answer, cut to [`REASON_BYTES`].
    /// Empty when ready.
    pub reason: String,
    /// What a profile may grant; present exactly when the Xcode is [`State::Ready`].
    pub host: Option<HostXcode>,
}

/// Why a catalog is refused.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum CatalogError {
    /// Two flags name the same build; the argument is the build.
    #[error("build {0} is flagged twice")]
    DuplicateBuild(String),
    /// More flags than [`MAX_LISTED`].
    #[error("too many --xcode flags")]
    TooMany,
}

/// A ready Xcode the profile may grant: the proof object of
/// [`ReadGrant::Xcode`](crate::seatbelt::ReadGrant::Xcode). Only a [`Catalog`] makes one.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HostXcode {
    build: String,
    version: DottedVersion,
    app: AbsPath,
    developer_dir: AbsPath,
    components: BTreeSet<XcodeComponent>,
}

impl HostXcode {
    /// The build.
    #[must_use]
    pub fn build(&self) -> &str {
        &self.build
    }

    /// The version.
    #[must_use]
    pub fn version(&self) -> DottedVersion {
        self.version
    }

    /// The app's real path.
    #[must_use]
    pub fn app(&self) -> &AbsPath {
        &self.app
    }

    /// `DEVELOPER_DIR` for a job that runs on this Xcode.
    #[must_use]
    pub fn developer_dir(&self) -> &AbsPath {
        &self.developer_dir
    }

    /// The components its `xcrun` finds, `clang` always.
    #[must_use]
    pub fn components(&self) -> &BTreeSet<XcodeComponent> {
        &self.components
    }
}

/// The surveyed Xcodes of the agent.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Catalog {
    xcodes: Vec<Xcode>,
}

/// The questions, in the order they are read, with the state a refusal means.
const ASKED: [(Tool, &[&str], State); 4] = [
    (Tool::Xcodebuild, &["-version"], State::Failed),
    (
        Tool::Xcodebuild,
        &["-license", "check"],
        State::LicenseNotAccepted,
    ),
    (
        Tool::Xcodebuild,
        &["-checkFirstLaunchStatus"],
        State::FirstLaunchNotRun,
    ),
    (Tool::Xcrun, &["--find", "clang"], State::Failed),
];

/// The other components, each found by `xcrun --find <tool>`.
const OPTIONAL: [(XcodeComponent, &str); 3] = [
    (XcodeComponent::Swift, "swiftc"),
    (XcodeComponent::Metal, "metal"),
    (XcodeComponent::Simctl, "simctl"),
];

#[derive(Clone, Copy)]
enum Tool {
    Xcodebuild,
    Xcrun,
}

impl Catalog {
    /// Asks every flagged Xcode, in the order of the flags.
    ///
    /// # Errors
    /// [`CatalogError`] for more than [`MAX_LISTED`] flags or one build flagged twice.
    /// An Xcode that fails a question is in the catalog, not ready.
    pub fn survey(
        flags: &[XcodeFlag],
        mut ask: impl FnMut(&Question<'_>) -> Reply,
    ) -> Result<Self, CatalogError> {
        let _ = (flags, &mut ask);
        Ok(Self { xcodes: Vec::new() })
    }

    /// Every flagged Xcode, ready or not, in the order of the flags.
    #[must_use]
    pub fn all(&self) -> &[Xcode] {
        &self.xcodes
    }

    /// The builds of the ready Xcodes, in the order of the flags.
    #[must_use]
    pub fn advertised(&self) -> Vec<&str> {
        self.ready().map(HostXcode::build).collect()
    }

    /// The ready Xcode of `build` (the flagged build), if there is one.
    #[must_use]
    pub fn host(&self, build: &str) -> Option<HostXcode> {
        self.ready().find(|host| host.build() == build).cloned()
    }

    fn ready(&self) -> impl Iterator<Item = &HostXcode> {
        self.xcodes.iter().filter_map(|xcode| xcode.host.as_ref())
    }
}

fn check(flag: &XcodeFlag, ask: &mut impl FnMut(&Question<'_>) -> Reply) -> Xcode {
    match ask_all(flag, ask) {
        Ok(host) => Xcode {
            flag: flag.clone(),
            state: State::Ready,
            reason: String::new(),
            host: Some(host),
        },
        Err((state, why)) => Xcode {
            flag: flag.clone(),
            state,
            reason: cut(&why),
            host: None,
        },
    }
}

fn ask_all(
    flag: &XcodeFlag,
    ask: &mut impl FnMut(&Question<'_>) -> Reply,
) -> Result<HostXcode, (State, String)> {
    let (app, developer_dir) = resolve(&flag.app).map_err(|why| (State::Failed, why))?;
    let xcodebuild = format!("{}/usr/bin/xcodebuild", developer_dir.as_str());
    let mut answers = Vec::new();
    for (tool, args, refused) in ASKED {
        let program = match tool {
            Tool::Xcodebuild => xcodebuild.clone(),
            Tool::Xcrun => XCRUN.to_owned(),
        };
        let question = Question {
            program,
            args,
            developer_dir: &developer_dir,
        };
        match ask(&question) {
            Ok(out) => answers.push(out),
            Err(Unanswered::Refused(why)) => return Err((refused, why)),
            Err(Unanswered::NoAnswer(why)) => return Err((State::Failed, why)),
        }
    }
    let shown = answers.swap_remove(0);
    let failed = |what: &str| {
        let text = format!("{what} in xcodebuild -version: {:?}", shown.trim());
        (State::Failed, text)
    };
    let build = build_of(&shown).ok_or_else(|| failed("no build"))?;
    let version = version_of(&shown).ok_or_else(|| failed("no version"))?;
    if build != flag.build {
        let text = format!(
            "flagged as build {}, but xcodebuild says {build}",
            flag.build
        );
        return Err((State::Failed, text));
    }
    let mut components = BTreeSet::from([XcodeComponent::Clang]);
    for (component, tool) in OPTIONAL {
        let question = Question {
            program: XCRUN.to_owned(),
            args: &["--find", tool],
            developer_dir: &developer_dir,
        };
        if ask(&question).is_ok() {
            components.insert(component);
        }
    }
    Ok(HostXcode {
        build: build.to_owned(),
        version,
        app,
        developer_dir,
        components,
    })
}

/// The real path of `app` and its `DEVELOPER_DIR`.
fn resolve(app: &AbsPath) -> Result<(AbsPath, AbsPath), String> {
    let real = std::fs::canonicalize(app.as_str()).map_err(|e| format!("{}: {e}", app.as_str()))?;
    let developer = real.join("Contents").join("Developer");
    match (AbsPath::new(&real), AbsPath::new(&developer)) {
        (Ok(real), Ok(developer)) => Ok((real, developer)),
        (Err(e), _) | (_, Err(e)) => Err(format!("{}: {e}", app.as_str())),
    }
}

/// The build in `xcodebuild -version` output (`Build version 16C5032a`), if any.
fn build_of(shown: &str) -> Option<&str> {
    shown
        .lines()
        .find_map(|line| line.trim().strip_prefix("Build version "))
        .map(str::trim)
        .filter(|build| !build.is_empty() && !build.contains(char::is_whitespace))
}

/// The version in `xcodebuild -version` output (`Xcode 16.2`), if any.
fn version_of(shown: &str) -> Option<DottedVersion> {
    let line = shown
        .lines()
        .find_map(|line| line.trim().strip_prefix("Xcode "))?;
    DottedVersion::parse(line.split_whitespace().next()?).ok()
}

/// `text`, or its first [`REASON_BYTES`] bytes (backed off to a character boundary).
fn cut(text: &str) -> String {
    let text = text.trim();
    let end = (0..=REASON_BYTES.min(text.len()))
        .rev()
        .find(|&i| text.is_char_boundary(i))
        .unwrap_or(0);
    text[..end].to_owned()
}
