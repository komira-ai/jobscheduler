//! install, check and remove: what the host step does with the rendered anchor.
//!
//! The anchor is loaded under its own anchor point with `pfctl -a <anchor> -f <file>`;
//! no other ruleset is touched, and `remove` flushes only the rules of this anchor.
//!
//! State lives in one directory the host step owns (root, mode 0700): the rules file the
//! anchor was loaded from (`pool.pf`), the file `check` renders its expectation into
//! (`expected.pf`), and the pf enable token (`pf-token`).
//!
//! # The enable token
//! `pfctl -E` turns pf on and takes a reference on it, printing a token; `pfctl -X
//! <token>` gives the reference back, and pf turns off only when the last reference
//! goes. `install` takes a reference only when it holds none: when pf is not enabled, or
//! no token is stored (after a reboot pf is off, so a stale token is replaced and the
//! release of the old one is ignored). A run that finds pf enabled and a token stored
//! takes nothing, so an install that runs every half minute never churns references.
//! The marker `pf-token.pending` is written before `-E` and removed once the token is
//! stored; a marker left behind means a run died between the two, and a reference may be
//! held that no file names. If the token cannot be stored, the new token is released. If
//! pf was already on and `-E` printed no token line, `install` accepts that (nothing to
//! keep) when `-s info` says Enabled: unverified on a Mac. `remove` releases the stored
//! token and leaves pf on if anything else holds it.
//!
//! # Enforcement
//! [`enforced`] (and [`Provisioner::check`], which it is) is the question a caller asks
//! before it lets a `Net::On` job run: is pf enabled, does the main ruleset evaluate the
//! anchor, and does the anchor hold exactly the rendered rules? Any error means refuse.

use std::fs;
use std::io::{self, Write as _};
use std::os::unix::fs::{DirBuilderExt as _, OpenOptionsExt as _};
use std::path::{Path, PathBuf};

use crate::pfctl::{Output, Pfctl};
use crate::policy::{AnchorName, Endpoint, UidRange};
use crate::render::render;

/// The file the anchor is loaded from.
pub const RULES_FILE: &str = "pool.pf";
/// The file `check` writes the expected rules to.
pub const EXPECTED_FILE: &str = "expected.pf";
/// The file holding the pf enable token.
pub const TOKEN_FILE: &str = "pf-token";
/// The marker written before `pfctl -E` and removed once its token is stored.
pub const PENDING_FILE: &str = "pf-token.pending";
/// The child of the anchor the rendered rules are loaded into by `check`, so they are read
/// back in pf's own spelling. A child is not reached by the wildcard of the anchor's parent.
pub const SCRATCH_CHILD: &str = "check";

/// Why a step failed.
#[derive(Debug, thiserror::Error)]
pub enum ProvisionError {
    /// `pfctl` could not be started.
    #[error("could not run pfctl {args}: {source}")]
    Run {
        /// The arguments.
        args: String,
        /// Why.
        source: io::Error,
    },
    /// `pfctl` ran and refused.
    #[error("pfctl {args} exited with {code}: {stderr}")]
    Failed {
        /// The arguments.
        args: String,
        /// The exit code, or "a signal".
        code: String,
        /// What it printed on standard error.
        stderr: String,
    },
    /// A state file could not be read or written.
    #[error("{path}: {source}")]
    State {
        /// The file.
        path: PathBuf,
        /// Why.
        source: io::Error,
    },
    /// `pfctl -E` printed no token.
    #[error("pfctl -E printed no token: {0}")]
    NoToken(String),
    /// pf is not enabled, so no rule is enforced.
    #[error("pf is not enabled (pfctl -s info does not say Status: Enabled)")]
    PfDisabled,
    /// The main ruleset does not evaluate the anchor, so its rules would never run.
    #[error(
        "the main ruleset has no anchor rule that reaches {0}: add `anchor \"{0}\"` (or a \
         wildcard of its parent, as `anchor \"com.apple/*\"`) to /etc/pf.conf and load it"
    )]
    NotReferenced(String),
    /// The loaded rules are not the rendered ones.
    #[error(
        "the loaded rules differ from the rendered ones\nexpected:\n{expected}\nloaded:\n{loaded}"
    )]
    Drift {
        /// The rules the policy renders to, as pf prints them.
        expected: String,
        /// The rules pf holds in the anchor.
        loaded: String,
    },
}

/// The host step over one anchor.
pub struct Provisioner<'a> {
    pf: &'a dyn Pfctl,
    anchor: &'a AnchorName,
    dir: &'a Path,
}

impl<'a> Provisioner<'a> {
    /// A provisioner of `anchor` that keeps its state in `dir`.
    #[must_use]
    pub fn new(pf: &'a dyn Pfctl, anchor: &'a AnchorName, dir: &'a Path) -> Self {
        Self { pf, anchor, dir }
    }

    /// Runs `pfctl`; a failure to start and a non-zero exit are errors.
    fn pfctl(&self, args: &[&str]) -> Result<Output, ProvisionError> {
        let shown = args.join(" ");
        let out = self.pf.run(args).map_err(|source| ProvisionError::Run {
            args: shown.clone(),
            source,
        })?;
        if out.success() {
            return Ok(out);
        }
        Err(ProvisionError::Failed {
            args: shown,
            code: out
                .code
                .map_or_else(|| "a signal".to_owned(), |c| c.to_string()),
            stderr: out.stderr.trim().to_owned(),
        })
    }

    fn state(&self, name: &str) -> PathBuf {
        self.dir.join(name)
    }

    /// Writes `text` to a state file (mode 0600), making the directory (0700) first.
    fn write(&self, name: &str, text: &str) -> Result<PathBuf, ProvisionError> {
        let path = self.state(name);
        let state = |source: io::Error| ProvisionError::State {
            path: path.clone(),
            source,
        };
        fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(self.dir)
            .map_err(state)?;
        let mut file = fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o600)
            .open(&path)
            .map_err(state)?;
        file.write_all(text.as_bytes()).map_err(state)?;
        Ok(path)
    }

    /// Loads the anchor, takes a pf enable reference if it holds none, and runs
    /// [`Provisioner::check`]. Safe to run again: it replaces the anchor's rules whole.
    /// Connections already in pf's state table are not touched by a reload: one to an
    /// endpoint the new rules refuse stays open until it ends.
    ///
    /// # Errors
    /// Any step failing; see [`ProvisionError`]. A failure after the load leaves the
    /// anchor loaded; run `remove` to undo.
    pub fn install(&self, uids: UidRange, endpoints: &[Endpoint]) -> Result<(), ProvisionError> {
        let rules = self.write(RULES_FILE, &render(uids, endpoints, self.anchor))?;
        self.pfctl(&["-a", self.anchor.as_str(), "-f", &path_arg(&rules)])?;
        let enabled = is_enabled(&self.pfctl(&["-s", "info"])?.stdout);
        if !(enabled && self.stored_token().is_some()) {
            self.enable()?;
        }
        self.check(uids, endpoints)
    }

    /// Takes a pf enable reference and stores its token.
    fn enable(&self) -> Result<(), ProvisionError> {
        let previous = self.stored_token();
        let pending = self.write(PENDING_FILE, "enabling\n")?;
        let printed = self.pfctl(&["-E"])?.combined();
        if let Some(token) = parse_token(&printed) {
            if let Err(e) = self.write(TOKEN_FILE, &format!("{token}\n")) {
                // The reference would be held with no file naming it.
                let _ = self.pfctl(&["-X", &token]);
                let _ = fs::remove_file(&pending);
                return Err(e);
            }
            if let Some(old) = previous {
                // A token from before a reboot is gone; there is nothing to give back.
                let _ = self.pfctl(&["-X", &old]);
            }
        } else if !is_enabled(&self.pfctl(&["-s", "info"])?.stdout) {
            return Err(ProvisionError::NoToken(printed.trim().to_owned()));
        }
        let _ = fs::remove_file(&pending);
        Ok(())
    }

    /// Whether the policy is enforced now: pf is enabled, the main ruleset evaluates the
    /// anchor, and the anchor holds the rules the policy renders to. The rendered rules
    /// are loaded into a scratch child of the anchor (`<anchor>/check`) and both are read
    /// back with `-sr`, so pf's own spelling is on both sides; the scratch child is
    /// flushed again. Blank lines and indentation are ignored.
    ///
    /// # Errors
    /// [`ProvisionError::PfDisabled`], [`ProvisionError::NotReferenced`],
    /// [`ProvisionError::Drift`], or a failed step.
    pub fn check(&self, uids: UidRange, endpoints: &[Endpoint]) -> Result<(), ProvisionError> {
        if !is_enabled(&self.pfctl(&["-s", "info"])?.stdout) {
            return Err(ProvisionError::PfDisabled);
        }
        let main = self.pfctl(&["-sr"])?;
        if !references(&main.stdout, self.anchor) {
            return Err(ProvisionError::NotReferenced(self.anchor.to_string()));
        }
        let file = self.write(EXPECTED_FILE, &render(uids, endpoints, self.anchor))?;
        let scratch = format!("{}/{SCRATCH_CHILD}", self.anchor);
        self.pfctl(&["-a", &scratch, "-f", &path_arg(&file)])?;
        let read = |anchor: &str| self.pfctl(&["-a", anchor, "-sr"]).map(|o| lines(&o.stdout));
        let both = read(self.anchor.as_str()).and_then(|l| Ok((l, read(&scratch)?)));
        // Flushed whether or not the reads worked; a failed read is the error reported.
        let flushed = self.pfctl(&["-a", &scratch, "-F", "rules"]);
        let (loaded, expected) = both?;
        flushed?;
        if expected == loaded {
            return Ok(());
        }
        Err(ProvisionError::Drift { expected, loaded })
    }

    /// Flushes the rules of this anchor (never another anchor, never the main ruleset),
    /// gives back the enable token and deletes the state files.
    ///
    /// # Errors
    /// The flush failing, or a state file that cannot be deleted. A token pf no longer
    /// knows is ignored.
    pub fn remove(&self) -> Result<(), ProvisionError> {
        self.pfctl(&["-a", self.anchor.as_str(), "-F", "rules"])?;
        if let Some(token) = self.stored_token() {
            let _ = self.pfctl(&["-X", &token]);
        }
        for name in [RULES_FILE, EXPECTED_FILE, TOKEN_FILE, PENDING_FILE] {
            let path = self.state(name);
            match fs::remove_file(&path) {
                Ok(()) => {}
                Err(e) if e.kind() == io::ErrorKind::NotFound => {}
                Err(source) => return Err(ProvisionError::State { path, source }),
            }
        }
        Ok(())
    }

    /// The token the last install stored, if there is one.
    fn stored_token(&self) -> Option<String> {
        let text = fs::read_to_string(self.state(TOKEN_FILE)).ok()?;
        let token = text.trim();
        (!token.is_empty()).then(|| token.to_owned())
    }
}

/// A path as a `pfctl` argument (lossy: the state directory is ASCII in practice).
fn path_arg(path: &Path) -> String {
    path.to_string_lossy().into_owned()
}

/// Whether the output of `pfctl -s info` says pf is enabled (`Status: Enabled ...`).
#[must_use]
pub fn is_enabled(info: &str) -> bool {
    info.lines().any(|l| {
        l.trim()
            .strip_prefix("Status:")
            .is_some_and(|r| r.trim_start().starts_with("Enabled"))
    })
}

/// The config of one policy, for [`enforced`].
pub struct Config<'a> {
    /// The `pfctl` runner.
    pub pf: &'a dyn Pfctl,
    /// The anchor.
    pub anchor: &'a AnchorName,
    /// The state directory.
    pub dir: &'a Path,
    /// The pool's uids.
    pub uids: UidRange,
    /// The endpoints the pool may reach.
    pub endpoints: &'a [Endpoint],
}

/// Whether the policy of `config` is enforced right now. **Call it at agent start and
/// before every `Net::On` job, and refuse on any error**: the LaunchDaemon's interval is
/// not the guard, this call is (a reload of the main ruleset can drop the anchor between
/// two ticks). The errors that mean "not enforced" are [`ProvisionError::PfDisabled`],
/// [`ProvisionError::NotReferenced`] and [`ProvisionError::Drift`]; `Run`, `Failed` and
/// `State` mean it could not be told, which is a refusal too.
///
/// # Errors
/// See [`Provisioner::check`].
pub fn enforced(config: &Config<'_>) -> Result<(), ProvisionError> {
    Provisioner::new(config.pf, config.anchor, config.dir).check(config.uids, config.endpoints)
}

/// The token in the output of `pfctl -E`: the digits after `Token :`.
#[must_use]
pub fn parse_token(output: &str) -> Option<String> {
    output.lines().find_map(|line| {
        let (key, value) = line.split_once(':')?;
        let value = value.trim();
        let digits = !value.is_empty() && value.bytes().all(|b| b.is_ascii_digit());
        (key.trim().eq_ignore_ascii_case("token") && digits).then(|| value.to_owned())
    })
}

/// Whether a main ruleset (`pfctl -sr`) evaluates `anchor`: it has an `anchor "<name>"`
/// rule for the anchor itself, or `anchor "<parent>/*"` for the anchor's parent (the
/// wildcard reaches the children of `<parent>`, not their own children).
#[must_use]
pub fn references(main_rules: &str, anchor: &AnchorName) -> bool {
    let name = anchor.as_str();
    main_rules.lines().any(|line| {
        let Some(rest) = line.trim().strip_prefix("anchor \"") else {
            return false;
        };
        let pattern = rest.split('"').next().unwrap_or_default();
        pattern == name
            || name
                .rsplit_once('/')
                .is_some_and(|(parent, _)| pattern.strip_suffix("/*") == Some(parent))
    })
}

/// The non-blank lines of `text`, trimmed, one per line: pf's printing of a ruleset with
/// whitespace the comparison does not care about removed.
fn lines(text: &str) -> String {
    text.lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .collect::<Vec<_>>()
        .join("\n")
}

#[cfg(test)]
mod tests {
    use std::os::unix::fs::PermissionsExt as _;

    use js_testkit::dir::TestDir;

    use super::*;
    use crate::testing::Fake;

    fn anchor() -> AnchorName {
        "com.apple/com.komira.js.pool".parse().unwrap()
    }

    fn uids() -> UidRange {
        UidRange::new(600, 100).unwrap()
    }

    fn eps() -> Vec<Endpoint> {
        vec!["192.0.2.10:443".parse().unwrap()]
    }

    fn mode(path: &Path) -> u32 {
        fs::metadata(path).unwrap().permissions().mode() & 0o777
    }

    fn dir(name: &str) -> TestDir {
        TestDir::under(std::env::temp_dir(), &format!("js-mac-provision-{name}"))
    }

    /// A fake whose pf is already on.
    fn on(dir: &Path) -> Fake {
        let pf = Fake::new(dir);
        pf.enabled.set(true);
        pf
    }

    #[test]
    fn install_loads_enables_checks_and_reads_back_in_this_order() {
        let dir = dir("install");
        let state = dir.join("state");
        let pf = Fake::new(&state);
        let a = anchor();
        Provisioner::new(&pf, &a, &state)
            .install(uids(), &eps())
            .unwrap();
        let rules = state.join(RULES_FILE);
        let expected = state.join(EXPECTED_FILE);
        let name = "com.apple/com.komira.js.pool";
        assert_eq!(
            pf.calls(),
            [
                format!("-a {name} -f {}", rules.display()),
                "-s info".to_owned(),
                "-E".to_owned(),
                "-s info".to_owned(),
                "-sr".to_owned(),
                format!("-a {name}/check -f {}", expected.display()),
                format!("-a {name} -sr"),
                format!("-a {name}/check -sr"),
                format!("-a {name}/check -F rules"),
            ]
        );
        assert_eq!(
            fs::read_to_string(&rules).unwrap(),
            render(uids(), &eps(), &a)
        );
        assert_eq!(
            fs::read_to_string(state.join(TOKEN_FILE)).unwrap(),
            "123456\n"
        );
        assert!(!state.join(PENDING_FILE).exists());
        assert_eq!(mode(&state), 0o700);
        for f in [RULES_FILE, EXPECTED_FILE, TOKEN_FILE] {
            assert_eq!(mode(&state.join(f)), 0o600, "{f}");
        }
    }

    #[test]
    fn a_second_install_takes_no_reference_while_pf_is_on_and_a_token_is_stored() {
        let dir = dir("twice");
        let pf = Fake::new(dir.path());
        let a = anchor();
        let p = Provisioner::new(&pf, &a, dir.path());
        p.install(uids(), &eps()).unwrap();
        *pf.token.borrow_mut() = "777".to_owned();
        let before = pf.calls().len();
        p.install(uids(), &eps()).unwrap();
        let second = &pf.calls()[before..];
        assert!(!second.iter().any(|c| c == "-E"));
        assert!(!second.iter().any(|c| c.starts_with("-X")));
        assert_eq!(
            fs::read_to_string(dir.join(TOKEN_FILE)).unwrap(),
            "123456\n"
        );
    }

    #[test]
    fn a_stale_token_is_replaced_when_pf_is_off_and_its_release_may_fail() {
        let dir = dir("stale");
        fs::write(dir.join(TOKEN_FILE), "999\n").unwrap();
        let mut pf = Fake::new(dir.path());
        pf.fail_release = true;
        let a = anchor();
        Provisioner::new(&pf, &a, dir.path())
            .install(uids(), &eps())
            .unwrap();
        let calls = pf.calls();
        let enable = calls.iter().position(|c| c == "-E").unwrap();
        let release = calls.iter().position(|c| c == "-X 999").unwrap();
        // The new reference is taken before the old one is given back.
        assert!(enable < release);
        assert_eq!(
            fs::read_to_string(dir.join(TOKEN_FILE)).unwrap(),
            "123456\n"
        );
    }

    #[test]
    fn pf_on_with_no_stored_token_takes_a_reference() {
        let dir = dir("on-no-token");
        let pf = on(dir.path());
        let a = anchor();
        Provisioner::new(&pf, &a, dir.path())
            .install(uids(), &eps())
            .unwrap();
        assert!(pf.calls().contains(&"-E".to_owned()));
        assert!(dir.join(TOKEN_FILE).exists());
    }

    #[test]
    fn install_refuses_when_the_main_ruleset_never_evaluates_the_anchor() {
        let dir = dir("unref");
        let mut pf = Fake::new(dir.path());
        pf.main = "anchor \"com.other/*\" all\n".to_owned();
        let a = anchor();
        let err = Provisioner::new(&pf, &a, dir.path())
            .install(uids(), &eps())
            .unwrap_err();
        assert!(matches!(err, ProvisionError::NotReferenced(_)));
        assert!(
            err.to_string()
                .contains("no anchor rule that reaches com.apple/com.komira.js.pool")
        );
        // Nothing past the reference check ran.
        assert_eq!(pf.calls().last().unwrap(), "-sr");
    }

    #[test]
    fn install_refuses_an_enable_that_prints_no_token_and_leaves_pf_off() {
        let dir = dir("notoken");
        let mut pf = Fake::new(dir.path());
        pf.no_token = true;
        pf.stay_off = true;
        let a = anchor();
        let err = Provisioner::new(&pf, &a, dir.path())
            .install(uids(), &eps())
            .unwrap_err();
        assert!(matches!(err, ProvisionError::NoToken(_)));
        assert!(err.to_string().ends_with("printed no token: pf enabled"));
        assert!(!dir.join(TOKEN_FILE).exists());
    }

    #[test]
    fn no_token_line_is_accepted_when_pf_is_enabled() {
        let dir = dir("notoken-on");
        let mut pf = Fake::new(dir.path());
        pf.no_token = true;
        let a = anchor();
        Provisioner::new(&pf, &a, dir.path())
            .install(uids(), &eps())
            .unwrap();
        assert!(!dir.join(TOKEN_FILE).exists());
        assert!(!dir.join(PENDING_FILE).exists());
    }

    #[test]
    fn a_token_that_cannot_be_stored_is_released() {
        let dir = dir("token-write");
        fs::create_dir(dir.join(TOKEN_FILE)).unwrap();
        let pf = Fake::new(dir.path());
        let a = anchor();
        let err = Provisioner::new(&pf, &a, dir.path())
            .install(uids(), &eps())
            .unwrap_err();
        assert!(matches!(err, ProvisionError::State { .. }));
        assert!(pf.calls().contains(&"-X 123456".to_owned()));
        assert!(!dir.join(PENDING_FILE).exists());
    }

    #[test]
    fn the_pending_marker_stays_when_the_enable_itself_fails() {
        let dir = dir("pending");
        let mut pf = Fake::new(dir.path());
        pf.fail = Some((2, false));
        let a = anchor();
        let err = Provisioner::new(&pf, &a, dir.path())
            .install(uids(), &eps())
            .unwrap_err();
        assert!(err.to_string().contains("-E"));
        assert!(dir.join(PENDING_FILE).exists());
    }

    #[test]
    fn every_pfctl_call_of_install_that_fails_stops_it_and_says_which() {
        // The calls of a healthy install, by index: load, info, enable, info, main
        // ruleset, load of the scratch child, read of the anchor, read of the child, flush.
        let want = ["-f", "info", "-E", "info", "-sr", "-f", "-sr", "-sr", "-F"];
        for (n, arg) in want.iter().enumerate() {
            for not_started in [true, false] {
                let dir = dir("fail");
                let mut pf = Fake::new(dir.path());
                pf.fail = Some((n, not_started));
                let a = anchor();
                let err = Provisioner::new(&pf, &a, dir.path())
                    .install(uids(), &eps())
                    .unwrap_err();
                // A failed read still flushes the scratch child.
                let len = if n == 6 || n == 7 { n + 2 } else { n + 1 };
                assert_eq!(pf.calls().len(), len, "{arg} {n} {not_started}");
                let shown = err.to_string();
                assert!(shown.contains(arg), "{shown}");
                if not_started {
                    assert!(matches!(err, ProvisionError::Run { .. }), "{shown}");
                    assert!(shown.contains("no pfctl"));
                } else {
                    assert!(shown.contains("exited with 1: pfctl: nope"), "{shown}");
                }
            }
        }
    }

    #[test]
    fn a_pfctl_a_signal_ended_is_a_failure_naming_the_signal() {
        let dir = dir("signal");
        let mut pf = Fake::new(dir.path());
        pf.fail_release = true;
        let a = anchor();
        let err = Provisioner::new(&pf, &a, dir.path())
            .pfctl(&["-X", "1"])
            .unwrap_err();
        assert!(err.to_string().contains("exited with a signal"));
    }

    #[test]
    fn a_state_directory_that_cannot_be_made_is_a_state_error() {
        let dir = dir("nodir");
        let blocker = dir.join("file");
        fs::write(&blocker, "x").unwrap();
        let state = blocker.join("state");
        let pf = Fake::new(&state);
        let a = anchor();
        let err = Provisioner::new(&pf, &a, &state)
            .install(uids(), &eps())
            .unwrap_err();
        assert!(matches!(err, ProvisionError::State { .. }));
        assert!(err.to_string().contains(RULES_FILE));
        assert!(pf.calls().is_empty());
    }

    #[test]
    fn a_rules_file_that_cannot_be_opened_is_a_state_error() {
        let dir = dir("noopen");
        fs::create_dir(dir.join(RULES_FILE)).unwrap();
        let pf = Fake::new(dir.path());
        let a = anchor();
        let err = Provisioner::new(&pf, &a, dir.path())
            .install(uids(), &eps())
            .unwrap_err();
        assert!(matches!(err, ProvisionError::State { .. }));
        assert!(pf.calls().is_empty());
    }

    #[test]
    fn check_passes_and_loads_nothing_into_the_live_anchor() {
        let dir = dir("check");
        let pf = Fake::new(dir.path());
        let a = anchor();
        let p = Provisioner::new(&pf, &a, dir.path());
        p.install(uids(), &eps()).unwrap();
        let before = pf.calls().len();
        p.check(uids(), &eps()).unwrap();
        let after = &pf.calls()[before..];
        assert_eq!(after.len(), 6);
        // Only the scratch child is loaded, and it is flushed last.
        let loads: Vec<&String> = after.iter().filter(|c| c.contains(" -f ")).collect();
        assert_eq!(loads.len(), 1);
        assert!(loads[0].contains("/check -f "));
        assert_eq!(
            after.last().unwrap(),
            "-a com.apple/com.komira.js.pool/check -F rules"
        );
    }

    #[test]
    fn check_fails_when_pf_has_been_disabled() {
        let dir = dir("disabled");
        let pf = Fake::new(dir.path());
        let a = anchor();
        let err = Provisioner::new(&pf, &a, dir.path())
            .check(uids(), &eps())
            .unwrap_err();
        assert!(matches!(err, ProvisionError::PfDisabled));
        assert!(err.to_string().contains("not enabled"));
        assert_eq!(pf.calls(), ["-s info"]);
    }

    #[test]
    fn check_fails_when_the_main_ruleset_dropped_the_anchor_reference() {
        let dir = dir("dropped");
        let mut pf = on(dir.path());
        pf.main = "block all\n".to_owned();
        let a = anchor();
        let err = Provisioner::new(&pf, &a, dir.path())
            .check(uids(), &eps())
            .unwrap_err();
        assert!(matches!(err, ProvisionError::NotReferenced(_)));
        assert_eq!(pf.calls(), ["-s info", "-sr"]);
    }

    #[test]
    fn check_reports_the_drift_when_the_loaded_rules_are_not_the_rendered_ones() {
        let dir = dir("drift");
        let mut pf = on(dir.path());
        pf.loaded = Some("pass out quick all\n".to_owned());
        let a = anchor();
        let err = Provisioner::new(&pf, &a, dir.path())
            .check(uids(), &eps())
            .unwrap_err();
        assert!(matches!(err, ProvisionError::Drift { .. }));
        let shown = err.to_string();
        assert!(shown.contains("\nloaded:\npass out quick all"));
        assert!(shown.contains("\nexpected:\n# js-mac-provision"));
        assert!(shown.contains("block return out quick"));
        // The scratch child is flushed on a drift too.
        assert!(pf.calls().last().unwrap().ends_with("/check -F rules"));
    }

    #[test]
    fn check_ignores_blank_lines_and_indentation_in_what_pfctl_prints() {
        let dir = dir("blank");
        let mut pf = on(dir.path());
        let a = anchor();
        let text = render(uids(), &eps(), &a);
        pf.loaded = Some(format!("\n  {}\n\n", text.replace('\n', "\n\n  ")));
        Provisioner::new(&pf, &a, dir.path())
            .check(uids(), &eps())
            .unwrap();
    }

    #[test]
    fn check_catches_a_changed_endpoint_list() {
        let dir = dir("changed");
        let pf = Fake::new(dir.path());
        let a = anchor();
        let p = Provisioner::new(&pf, &a, dir.path());
        p.install(uids(), &eps()).unwrap();
        let other: Vec<Endpoint> = vec!["192.0.2.11:443".parse().unwrap()];
        assert!(matches!(
            p.check(uids(), &other),
            Err(ProvisionError::Drift { .. })
        ));
    }

    #[test]
    fn enforced_is_ok_when_enabled_referenced_and_loaded_and_names_each_way_it_is_not() {
        let dir = dir("enforced");
        let a = anchor();
        let e = eps();
        let installed = Fake::new(dir.path());
        Provisioner::new(&installed, &a, dir.path())
            .install(uids(), &e)
            .unwrap();
        let cfg = |pf| Config {
            pf,
            anchor: &a,
            dir: dir.path(),
            uids: uids(),
            endpoints: &e,
        };
        assert!(enforced(&cfg(&installed)).is_ok());
        installed.enabled.set(false);
        assert!(matches!(
            enforced(&cfg(&installed)),
            Err(ProvisionError::PfDisabled)
        ));
        let mut dropped = on(dir.path());
        dropped.main = String::new();
        assert!(matches!(
            enforced(&cfg(&dropped)),
            Err(ProvisionError::NotReferenced(_))
        ));
        let mut empty = on(dir.path());
        empty.loaded = Some(String::new());
        assert!(matches!(
            enforced(&cfg(&empty)),
            Err(ProvisionError::Drift { .. })
        ));
        let mut broken = on(dir.path());
        broken.fail = Some((0, true));
        assert!(matches!(
            enforced(&cfg(&broken)),
            Err(ProvisionError::Run { .. })
        ));
    }

    #[test]
    fn remove_flushes_only_this_anchor_gives_the_token_back_and_deletes_the_state() {
        let dir = dir("remove");
        let pf = Fake::new(dir.path());
        let a = anchor();
        let p = Provisioner::new(&pf, &a, dir.path());
        p.install(uids(), &eps()).unwrap();
        fs::write(dir.join(PENDING_FILE), "x").unwrap();
        let before = pf.calls().len();
        p.remove().unwrap();
        assert_eq!(
            pf.calls()[before..],
            [
                "-a com.apple/com.komira.js.pool -F rules".to_owned(),
                "-X 123456".to_owned()
            ]
        );
        for f in [RULES_FILE, EXPECTED_FILE, TOKEN_FILE, PENDING_FILE] {
            assert!(!dir.join(f).exists(), "{f}");
        }
        assert!(!pf.calls().iter().any(|c| c.contains("-F all")));
        assert!(!pf.calls().iter().any(|c| c.contains("-F state")));
    }

    #[test]
    fn remove_twice_is_fine_and_gives_back_no_token_it_does_not_hold() {
        let dir = dir("remove2");
        let pf = Fake::new(dir.path());
        let a = anchor();
        let p = Provisioner::new(&pf, &a, dir.path());
        p.remove().unwrap();
        p.remove().unwrap();
        assert_eq!(
            pf.calls(),
            [
                "-a com.apple/com.komira.js.pool -F rules",
                "-a com.apple/com.komira.js.pool -F rules"
            ]
        );
    }

    #[test]
    fn remove_ignores_a_stale_token_and_an_empty_token_file() {
        let dir = dir("remove3");
        fs::write(dir.join(TOKEN_FILE), "55\n").unwrap();
        let mut pf = Fake::new(dir.path());
        pf.fail_release = true;
        let a = anchor();
        let p = Provisioner::new(&pf, &a, dir.path());
        p.remove().unwrap();
        assert!(pf.calls().contains(&"-X 55".to_owned()));
        fs::write(dir.join(TOKEN_FILE), "\n").unwrap();
        let before = pf.calls().len();
        p.remove().unwrap();
        assert_eq!(pf.calls().len(), before + 1);
    }

    #[test]
    fn remove_stops_when_the_flush_fails_and_keeps_the_state() {
        let dir = dir("remove4");
        fs::write(dir.join(TOKEN_FILE), "55\n").unwrap();
        let mut pf = Fake::new(dir.path());
        pf.fail = Some((0, false));
        let a = anchor();
        let err = Provisioner::new(&pf, &a, dir.path()).remove().unwrap_err();
        assert!(matches!(err, ProvisionError::Failed { .. }));
        assert!(dir.join(TOKEN_FILE).exists());
        assert_eq!(pf.calls().len(), 1);
    }

    #[test]
    fn remove_reports_a_state_file_it_cannot_delete() {
        let dir = dir("remove5");
        let stuck = dir.join(RULES_FILE);
        fs::create_dir(&stuck).unwrap();
        fs::write(stuck.join("inner"), "x").unwrap();
        let pf = Fake::new(dir.path());
        let a = anchor();
        let err = Provisioner::new(&pf, &a, dir.path()).remove().unwrap_err();
        assert!(matches!(err, ProvisionError::State { .. }));
        assert!(err.to_string().starts_with(&stuck.display().to_string()));
    }

    #[test]
    fn the_token_is_the_digits_after_the_token_label_on_either_stream() {
        assert_eq!(
            parse_token("pf enabled\nToken : 8379817265\n").unwrap(),
            "8379817265"
        );
        assert_eq!(parse_token("\ntoken:42").unwrap(), "42");
        assert_eq!(
            parse_token("pfctl: pf already enabled\nToken : 7").unwrap(),
            "7"
        );
        for text in [
            "",
            "pf enabled",
            "Token : ",
            "Token : abc",
            "Other : 12",
            "Token 12",
        ] {
            assert_eq!(parse_token(text), None, "{text:?}");
        }
    }

    #[test]
    fn info_says_enabled_only_on_the_status_line() {
        assert!(is_enabled(
            "Status: Enabled for 0 days 00:00:05           Debug: Urgent\n"
        ));
        assert!(is_enabled("Foo\n  Status:   Enabled\n"));
        for text in [
            "",
            "Status: Disabled\n",
            "Status: Disabled  Enabled\n",
            "Debug: Enabled\n",
        ] {
            assert!(!is_enabled(text), "{text:?}");
        }
    }

    #[test]
    fn the_main_ruleset_reaches_the_anchor_by_name_or_by_its_parents_wildcard() {
        let a = anchor();
        assert!(references("anchor \"com.apple/*\" all\n", &a));
        assert!(references(
            "  anchor \"com.apple/com.komira.js.pool\" all",
            &a
        ));
        assert!(references(
            "scrub-anchor \"x/*\" all\nanchor \"com.apple/*\" all",
            &a
        ));
        for text in [
            "",
            "block drop all",
            "anchor \"com.other/*\" all",
            "anchor \"com.apple\" all",
            "anchor \"com.apple/com.komira.js/*\" all",
            "anchor \"*\" all",
        ] {
            assert!(!references(text, &a), "{text}");
        }
        // The wildcard reaches the children, not their own children.
        let deep: AnchorName = "com.apple/a/b".parse().unwrap();
        assert!(!references("anchor \"com.apple/*\" all", &deep));
        assert!(references("anchor \"com.apple/a/*\" all", &deep));
        // An anchor with no parent is reached only by its own name.
        let top: AnchorName = "com.komira.js".parse().unwrap();
        assert!(references("anchor \"com.komira.js\" all", &top));
        assert!(!references("anchor \"com.apple/*\" all", &top));
    }
}
