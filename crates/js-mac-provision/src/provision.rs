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
//! goes. `install` takes a new reference, stores its token, then releases the one it
//! stored before, so an install that runs every few minutes (the LaunchDaemon) holds
//! exactly one. After a reboot the stored token is stale; releasing it fails and is
//! ignored. `remove` releases the stored token and leaves pf on if anything else holds it.

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

    /// Loads the anchor, enables pf, checks that the main ruleset evaluates the anchor and
    /// reads the rules back. Safe to run again: it replaces the anchor's rules whole.
    ///
    /// # Errors
    /// Any step failing; see [`ProvisionError`]. A failure after the load leaves the
    /// anchor loaded and the token held; run `remove` to undo.
    pub fn install(&self, uids: UidRange, endpoints: &[Endpoint]) -> Result<(), ProvisionError> {
        let rules = self.write(RULES_FILE, &render(uids, endpoints, self.anchor))?;
        let rules_arg = path_arg(&rules);
        self.pfctl(&["-a", self.anchor.as_str(), "-f", &rules_arg])?;
        let previous = self.stored_token();
        let printed = self.pfctl(&["-E"])?.combined();
        let token = parse_token(&printed)
            .ok_or_else(|| ProvisionError::NoToken(printed.trim().to_owned()))?;
        self.write(TOKEN_FILE, &format!("{token}\n"))?;
        if let Some(old) = previous {
            // A token from before a reboot is gone; there is nothing to give back.
            let _ = self.pfctl(&["-X", &old]);
        }
        let main = self.pfctl(&["-sr"])?;
        if !references(&main.stdout, self.anchor) {
            return Err(ProvisionError::NotReferenced(self.anchor.to_string()));
        }
        self.check(uids, endpoints)
    }

    /// Compares the rules loaded in the anchor with the ones the policy renders to. Both
    /// are text pfctl printed (`-sr` of the anchor, and `-n -v -f` of the rendered file,
    /// which parses without loading), so pf's own spelling of the rules cancels out.
    ///
    /// # Errors
    /// [`ProvisionError::Drift`] when they differ; or a failed step.
    pub fn check(&self, uids: UidRange, endpoints: &[Endpoint]) -> Result<(), ProvisionError> {
        let file = self.write(EXPECTED_FILE, &render(uids, endpoints, self.anchor))?;
        let file_arg = path_arg(&file);
        let loaded = self.pfctl(&["-a", self.anchor.as_str(), "-sr"])?;
        let expected = self.pfctl(&["-a", self.anchor.as_str(), "-n", "-v", "-f", &file_arg])?;
        let (expected, loaded) = (lines(&expected.stdout), lines(&loaded.stdout));
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
        for name in [RULES_FILE, EXPECTED_FILE, TOKEN_FILE] {
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

    #[test]
    fn install_loads_enables_checks_and_reads_back_in_this_order() {
        let dir = TestDir::under(std::env::temp_dir(), "js-mac-provision-install");
        let state = dir.join("state");
        let pf = Fake::new(&state);
        let a = anchor();
        Provisioner::new(&pf, &a, &state)
            .install(uids(), &eps())
            .unwrap();
        let rules = state.join(RULES_FILE);
        let expected = state.join(EXPECTED_FILE);
        assert_eq!(
            pf.calls(),
            [
                format!("-a com.apple/com.komira.js.pool -f {}", rules.display()),
                "-E".to_owned(),
                "-sr".to_owned(),
                "-a com.apple/com.komira.js.pool -sr".to_owned(),
                format!(
                    "-a com.apple/com.komira.js.pool -n -v -f {}",
                    expected.display()
                ),
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
        assert_eq!(mode(&state), 0o700);
        for f in [RULES_FILE, EXPECTED_FILE, TOKEN_FILE] {
            assert_eq!(mode(&state.join(f)), 0o600, "{f}");
        }
    }

    #[test]
    fn a_second_install_holds_one_reference_by_giving_the_first_back() {
        let dir = TestDir::under(std::env::temp_dir(), "js-mac-provision-twice");
        let pf = Fake::new(dir.path());
        let a = anchor();
        let p = Provisioner::new(&pf, &a, dir.path());
        p.install(uids(), &eps()).unwrap();
        *pf.token.borrow_mut() = "777".to_owned();
        p.install(uids(), &eps()).unwrap();
        let calls = pf.calls();
        let enables: Vec<usize> = (0..calls.len()).filter(|i| calls[*i] == "-E").collect();
        let release = calls.iter().position(|c| c == "-X 123456").unwrap();
        // The new reference is taken before the old one is given back.
        assert!(enables[1] < release);
        assert_eq!(fs::read_to_string(dir.join(TOKEN_FILE)).unwrap(), "777\n");
    }

    #[test]
    fn a_token_pf_no_longer_knows_does_not_fail_the_install() {
        let dir = TestDir::under(std::env::temp_dir(), "js-mac-provision-stale");
        fs::write(dir.join(TOKEN_FILE), "999\n").unwrap();
        let mut pf = Fake::new(dir.path());
        pf.fail_release = true;
        let a = anchor();
        Provisioner::new(&pf, &a, dir.path())
            .install(uids(), &eps())
            .unwrap();
        assert!(pf.calls().contains(&"-X 999".to_owned()));
        assert_eq!(
            fs::read_to_string(dir.join(TOKEN_FILE)).unwrap(),
            "123456\n"
        );
    }

    #[test]
    fn install_refuses_when_the_main_ruleset_never_evaluates_the_anchor() {
        let dir = TestDir::under(std::env::temp_dir(), "js-mac-provision-unref");
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
        assert_eq!(pf.calls().len(), 3);
    }

    #[test]
    fn install_refuses_an_enable_that_prints_no_token() {
        let dir = TestDir::under(std::env::temp_dir(), "js-mac-provision-notoken");
        let mut pf = Fake::new(dir.path());
        pf.no_token = true;
        let a = anchor();
        let err = Provisioner::new(&pf, &a, dir.path())
            .install(uids(), &eps())
            .unwrap_err();
        assert!(matches!(err, ProvisionError::NoToken(_)));
        assert!(err.to_string().ends_with("printed no token: pf enabled"));
        assert!(!dir.join(TOKEN_FILE).exists());
    }

    #[test]
    fn every_pfctl_call_of_install_that_fails_stops_it_and_says_which() {
        // The calls of a healthy install, by index: load, enable, main ruleset, read
        // back, parse.
        let want = ["-f", "-E", "-sr", "-sr", "-n"];
        for (n, arg) in want.iter().enumerate() {
            for not_started in [true, false] {
                let dir = TestDir::under(std::env::temp_dir(), "js-mac-provision-fail");
                let mut pf = Fake::new(dir.path());
                pf.fail = Some((n, not_started));
                let a = anchor();
                let err = Provisioner::new(&pf, &a, dir.path())
                    .install(uids(), &eps())
                    .unwrap_err();
                assert_eq!(pf.calls().len(), n + 1, "{arg} {not_started}");
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
        let dir = TestDir::under(std::env::temp_dir(), "js-mac-provision-signal");
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
        let dir = TestDir::under(std::env::temp_dir(), "js-mac-provision-nodir");
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
        let dir = TestDir::under(std::env::temp_dir(), "js-mac-provision-noopen");
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
    fn check_passes_when_the_anchor_holds_what_the_policy_renders_to() {
        let dir = TestDir::under(std::env::temp_dir(), "js-mac-provision-check");
        let pf = Fake::new(dir.path());
        let a = anchor();
        let p = Provisioner::new(&pf, &a, dir.path());
        p.install(uids(), &eps()).unwrap();
        let before = pf.calls().len();
        p.check(uids(), &eps()).unwrap();
        // Read only as far as pf goes: no load, no enable.
        let after = &pf.calls()[before..];
        assert_eq!(after.len(), 2);
        assert!(
            after
                .iter()
                .all(|c| !c.contains(" -f ") || c.contains(" -n "))
        );
    }

    #[test]
    fn check_reports_the_drift_when_the_loaded_rules_are_not_the_rendered_ones() {
        let dir = TestDir::under(std::env::temp_dir(), "js-mac-provision-drift");
        let mut pf = Fake::new(dir.path());
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
    }

    #[test]
    fn check_ignores_blank_lines_and_indentation_in_what_pfctl_prints() {
        let dir = TestDir::under(std::env::temp_dir(), "js-mac-provision-blank");
        let mut pf = Fake::new(dir.path());
        let a = anchor();
        let text = render(uids(), &eps(), &a);
        pf.loaded = Some(format!("\n  {}\n\n", text.replace('\n', "\n\n  ")));
        Provisioner::new(&pf, &a, dir.path())
            .check(uids(), &eps())
            .unwrap();
    }

    #[test]
    fn check_catches_a_changed_endpoint_list() {
        let dir = TestDir::under(std::env::temp_dir(), "js-mac-provision-changed");
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
    fn remove_flushes_only_this_anchor_gives_the_token_back_and_deletes_the_state() {
        let dir = TestDir::under(std::env::temp_dir(), "js-mac-provision-remove");
        let pf = Fake::new(dir.path());
        let a = anchor();
        let p = Provisioner::new(&pf, &a, dir.path());
        p.install(uids(), &eps()).unwrap();
        let before = pf.calls().len();
        p.remove().unwrap();
        assert_eq!(
            pf.calls()[before..],
            [
                "-a com.apple/com.komira.js.pool -F rules".to_owned(),
                "-X 123456".to_owned()
            ]
        );
        for f in [RULES_FILE, EXPECTED_FILE, TOKEN_FILE] {
            assert!(!dir.join(f).exists(), "{f}");
        }
        // Nothing the whole ruleset or other anchors hold was named.
        assert!(!pf.calls().iter().any(|c| c.contains("-F all")));
        assert!(!pf.calls().iter().any(|c| c.contains("-F state")));
    }

    #[test]
    fn remove_twice_is_fine_and_gives_back_no_token_it_does_not_hold() {
        let dir = TestDir::under(std::env::temp_dir(), "js-mac-provision-remove2");
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
        let dir = TestDir::under(std::env::temp_dir(), "js-mac-provision-remove3");
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
        let dir = TestDir::under(std::env::temp_dir(), "js-mac-provision-remove4");
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
        let dir = TestDir::under(std::env::temp_dir(), "js-mac-provision-remove5");
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
