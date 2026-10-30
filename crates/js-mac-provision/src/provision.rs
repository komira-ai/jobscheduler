//! install, check and remove: what the host step does with the rendered anchor.
//!
//! The anchor is loaded under its own anchor point with `pfctl -a <anchor> -f <file>`;
//! no other ruleset is touched, and `remove` flushes only the rules of this anchor.
//!
//! State lives in one directory the host step owns (root, mode 0700), every file written
//! through a temporary file and a rename so a reader sees the old file or the new one,
//! never half of one: the rendered rules the anchor was loaded from (`pool.pf`), what
//! `pfctl -a <anchor> -sr` printed right after the load (`anchor.sr`), the pf enable token
//! (`pf-token`) and two markers (below).
//!
//! # check is read-only
//! [`Provisioner::check`] and [`enforced`] change nothing, so any number may run at once
//! beside each other and beside `install`'s own check: they read `pfctl -s info`, the main
//! ruleset (`-sr`) and the anchor (`-a <anchor> -sr`), and two files. pf prints a rule in
//! its own spelling, so the rendered text is not compared with pf's; instead `install`
//! stores pf's own readback, and `check` compares the live anchor with that. The stored
//! rendered text is compared with what the policy renders now, so a changed policy is
//! `Drift` until `install` runs. The readback is written atomically by rename; no digest
//! is kept (a torn file cannot be read). Reading the rules of an anchor opens `/dev/pf`,
//! so `pfctl` needs root: the macOS agent runs as root and can call it; a caller that is
//! not root gets a `Failed` error, which is a refusal.
//!
//! # The enable token
//! `pfctl -E` turns pf on and takes a reference on it, printing a token; `pfctl -X
//! <token>` gives the reference back, and pf turns off only when the last reference
//! goes. `install` takes a reference only when pf is off, or when pf is on and the run
//! holds neither a stored token nor the record `pf-token.none` ("`-E` printed no token
//! and pf was on"), so an install that runs every half minute takes nothing. After a
//! reboot pf is off, so a stale token or record is replaced (releasing the old token
//! fails and is ignored). The marker `pf-token.pending` is written before `-E` and removed
//! once the token (or the record) is stored; if the next `install` finds it, a run died
//! between the two, a reference may be held that no file names and only a reboot clears
//! it: it says so on standard error and removes the marker. If the token cannot be
//! stored, it is released at once. `-E` printing no token line while pf is Enabled is
//! accepted: unverified on a Mac. `remove` releases the stored token and leaves pf on if
//! anything else holds it.
//!
//! # Enforcement
//! [`enforced`] (and [`Provisioner::check`], which it is) is the question a caller asks
//! before it lets a `Net::On` job run: is pf enabled, does the main ruleset evaluate the
//! anchor, does the anchor hold what `install` read back, and is that what the policy
//! renders now? Any error means refuse.

use std::fs;
use std::io::{self, Write as _};
use std::os::unix::fs::{DirBuilderExt as _, OpenOptionsExt as _};
use std::path::{Path, PathBuf};

use crate::pfctl::{Output, Pfctl};
use crate::policy::{AnchorName, Endpoint, UidRange};
use crate::render::render;

/// The file the anchor is loaded from.
pub const RULES_FILE: &str = "pool.pf";
/// The file holding what `pfctl -a <anchor> -sr` printed after the load.
pub const READBACK_FILE: &str = "anchor.sr";
/// The file holding the pf enable token.
pub const TOKEN_FILE: &str = "pf-token";
/// The marker written before `pfctl -E` and removed once its token is stored.
pub const PENDING_FILE: &str = "pf-token.pending";
/// The record that `-E` printed no token while pf was on: there is none to keep, and `-E`
/// is not run again while pf stays on.
pub const NO_TOKEN_FILE: &str = "pf-token.none";

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
    /// The anchor reads back with fewer rules than the policy renders (none, when its
    /// `-sr` printed nothing), so it cannot be what was loaded.
    #[error("the anchor reads back {got} rules, fewer than the {want} the policy renders")]
    BadReadback {
        /// Rule lines read back.
        got: usize,
        /// Rule lines rendered.
        want: usize,
    },
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

    /// Writes `text` to a state file (mode 0600) by a temporary file and a rename, making
    /// the directory (0700) first.
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
        let temp = self.state(&format!("{name}.{}.tmp", std::process::id()));
        let mut file = fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o600)
            .open(&temp)
            .map_err(state)?;
        file.write_all(text.as_bytes()).map_err(state)?;
        fs::rename(&temp, &path).map_err(state)?;
        Ok(path)
    }

    /// A state file's text.
    fn read(&self, name: &str) -> Result<String, ProvisionError> {
        let path = self.state(name);
        fs::read_to_string(&path).map_err(|source| ProvisionError::State { path, source })
    }

    /// Loads the anchor, takes a pf enable reference if it holds none, stores pf's
    /// readback of the anchor and runs [`Provisioner::check`]. Safe to run again: it
    /// replaces the anchor's rules whole. Connections already in pf's state table are not
    /// touched by a reload: one to an endpoint the new rules refuse stays open until it ends.
    ///
    /// # Errors
    /// Any step failing; see [`ProvisionError`]. A failure after the load leaves the
    /// anchor loaded; run `remove` to undo.
    pub fn install(&self, uids: UidRange, endpoints: &[Endpoint]) -> Result<(), ProvisionError> {
        let pending = self.state(PENDING_FILE);
        if pending.exists() {
            eprintln!(
                "js-mac-provision: an earlier run died while enabling pf; a pf reference may \
                 be held that no file names, and only a reboot clears it"
            );
            let _ = fs::remove_file(&pending);
        }
        // The readback goes first: until the load below succeeds and is read back, what
        // `check` would compare (the new rules file, the old readback) says nothing about
        // what is loaded, so a failed load must leave `check` unable to pass.
        self.discard(READBACK_FILE)?;
        let rendered = render(uids, endpoints, self.anchor);
        let rules = self.write(RULES_FILE, &rendered)?;
        self.pfctl(&["-a", self.anchor.as_str(), "-f", &path_arg(&rules)])?;
        let enabled = is_enabled(&self.pfctl(&["-s", "info"])?.stdout);
        let held = self.stored_token().is_some() || self.state(NO_TOKEN_FILE).exists();
        if !(enabled && held) {
            self.enable()?;
        }
        let readback = self.pfctl(&["-a", self.anchor.as_str(), "-sr"])?;
        enough_rules(&readback.stdout, &rendered)?;
        self.write(READBACK_FILE, &readback.stdout)?;
        self.check(uids, endpoints)
    }

    /// Takes a pf enable reference and stores its token.
    fn enable(&self) -> Result<(), ProvisionError> {
        let previous = self.stored_token();
        let pending = self.write(PENDING_FILE, "enabling\n")?;
        let printed = self.pfctl(&["-E"])?.combined();
        let none = self.state(NO_TOKEN_FILE);
        if let Some(token) = parse_token(&printed) {
            let _ = fs::remove_file(&none);
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
        } else if is_enabled(&self.pfctl(&["-s", "info"])?.stdout) {
            self.write(NO_TOKEN_FILE, "enabled without a token\n")?;
        } else {
            return Err(ProvisionError::NoToken(printed.trim().to_owned()));
        }
        let _ = fs::remove_file(&pending);
        Ok(())
    }

    /// Whether the policy is enforced now, reading only: pf is enabled, the main ruleset
    /// evaluates the anchor, the rules `install` stored are what the policy renders now,
    /// and the anchor still reads back as `install` stored it.
    ///
    /// # Errors
    /// [`ProvisionError::PfDisabled`], [`ProvisionError::NotReferenced`],
    /// [`ProvisionError::Drift`], or a failed step: pfctl not run (or not root), or a state
    /// file `install` writes missing, which is "could not tell".
    pub fn check(&self, uids: UidRange, endpoints: &[Endpoint]) -> Result<(), ProvisionError> {
        if !is_enabled(&self.pfctl(&["-s", "info"])?.stdout) {
            return Err(ProvisionError::PfDisabled);
        }
        let main = self.pfctl(&["-sr"])?;
        if !references(&main.stdout, self.anchor) {
            return Err(ProvisionError::NotReferenced(self.anchor.to_string()));
        }
        let text = render(uids, endpoints, self.anchor);
        let rendered = lines(&text);
        let installed = lines(&self.read(RULES_FILE)?);
        if installed != rendered {
            return Err(ProvisionError::Drift {
                expected: rendered,
                loaded: installed,
            });
        }
        let stored = self.read(READBACK_FILE)?;
        enough_rules(&stored, &text)?;
        let stored = lines(&stored);
        let live = lines(&self.pfctl(&["-a", self.anchor.as_str(), "-sr"])?.stdout);
        if stored == live {
            return Ok(());
        }
        Err(ProvisionError::Drift {
            expected: stored,
            loaded: live,
        })
    }

    /// Flushes the rules of this anchor and of a leftover `<anchor>/check` child (never another
    /// anchor, never the main ruleset),
    /// gives back the enable token and deletes the state files.
    ///
    /// # Errors
    /// The flush failing, or a state file that cannot be deleted. A token pf no longer
    /// knows is ignored.
    pub fn remove(&self) -> Result<(), ProvisionError> {
        self.pfctl(&["-a", self.anchor.as_str(), "-F", "rules"])?;
        // An earlier version loaded the rendered rules into a child to read them back; a
        // child left behind is flushed too, and its absence is no error.
        let child = format!("{}/check", self.anchor);
        let _ = self.pfctl(&["-a", &child, "-F", "rules"]);
        if let Some(token) = self.stored_token() {
            let _ = self.pfctl(&["-X", &token]);
        }
        for name in [
            RULES_FILE,
            READBACK_FILE,
            TOKEN_FILE,
            PENDING_FILE,
            NO_TOKEN_FILE,
        ] {
            self.discard(name)?;
        }
        Ok(())
    }

    /// Deletes a state file; one that is not there is fine.
    fn discard(&self, name: &str) -> Result<(), ProvisionError> {
        let path = self.state(name);
        match fs::remove_file(&path) {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(()),
            Err(source) => Err(ProvisionError::State { path, source }),
        }
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

/// The rule lines of `text`: non-blank lines that are not comments.
fn rule_count(text: &str) -> usize {
    text.lines()
        .map(str::trim)
        .filter(|l| !l.is_empty() && !l.starts_with('#'))
        .count()
}

/// Refuses a readback with fewer rules than the rendered text: an empty `-sr` would
/// otherwise equal an empty stored copy and pass for an enforced anchor.
fn enough_rules(readback: &str, rendered: &str) -> Result<(), ProvisionError> {
    let (got, want) = (rule_count(readback), rule_count(rendered));
    if got < want {
        return Err(ProvisionError::BadReadback { got, want });
    }
    Ok(())
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

    const NAME: &str = "com.apple/com.komira.js.pool";

    fn anchor() -> AnchorName {
        NAME.parse().unwrap()
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

    /// A fake and a directory with the policy installed.
    fn installed(name: &str) -> (TestDir, Fake) {
        let dir = dir(name);
        let pf = Fake::new(dir.path());
        let a = anchor();
        Provisioner::new(&pf, &a, dir.path())
            .install(uids(), &eps())
            .unwrap();
        (dir, pf)
    }

    #[test]
    fn install_loads_enables_reads_back_and_checks_in_this_order() {
        let dir = dir("install");
        let state = dir.join("state");
        let pf = Fake::new(&state);
        let a = anchor();
        Provisioner::new(&pf, &a, &state)
            .install(uids(), &eps())
            .unwrap();
        let rules = state.join(RULES_FILE);
        assert_eq!(
            pf.calls(),
            [
                format!("-a {NAME} -f {}", rules.display()),
                "-s info".to_owned(),
                "-E".to_owned(),
                format!("-a {NAME} -sr"),
                "-s info".to_owned(),
                "-sr".to_owned(),
                format!("-a {NAME} -sr"),
            ]
        );
        let text = render(uids(), &eps(), &a);
        assert_eq!(fs::read_to_string(&rules).unwrap(), text);
        // The fake prints the loaded rules as the readback.
        assert_eq!(fs::read_to_string(state.join(READBACK_FILE)).unwrap(), text);
        assert_eq!(
            fs::read_to_string(state.join(TOKEN_FILE)).unwrap(),
            "123456\n"
        );
        assert!(!state.join(PENDING_FILE).exists());
        assert_eq!(mode(&state), 0o700);
        for f in [RULES_FILE, READBACK_FILE, TOKEN_FILE] {
            assert_eq!(mode(&state.join(f)), 0o600, "{f}");
        }
        // No temporary file is left.
        assert!(
            !state
                .join(format!("pool.pf.{}.tmp", std::process::id()))
                .exists()
        );
    }

    #[test]
    fn a_second_install_takes_no_reference_while_pf_is_on_and_a_token_is_stored() {
        let (dir, pf) = installed("twice");
        *pf.token.borrow_mut() = "777".to_owned();
        let before = pf.calls().len();
        let a = anchor();
        Provisioner::new(&pf, &a, dir.path())
            .install(uids(), &eps())
            .unwrap();
        let second = &pf.calls()[before..];
        assert!(!second.iter().any(|c| c == "-E"));
        assert!(!second.iter().any(|c| c.starts_with("-X")));
        assert_eq!(
            fs::read_to_string(dir.join(TOKEN_FILE)).unwrap(),
            "123456\n"
        );
    }

    #[test]
    fn a_stale_token_and_record_are_replaced_when_pf_is_off() {
        let dir = dir("stale");
        fs::write(dir.join(TOKEN_FILE), "999\n").unwrap();
        fs::write(dir.join(NO_TOKEN_FILE), "x").unwrap();
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
        assert!(!dir.join(NO_TOKEN_FILE).exists());
    }

    #[test]
    fn pf_on_with_nothing_held_takes_a_reference() {
        let dir = dir("on-nothing");
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
        let pf = Fake::new(dir.path());
        *pf.main.borrow_mut() = "anchor \"com.other/*\" all\n".to_owned();
        let a = anchor();
        let err = Provisioner::new(&pf, &a, dir.path())
            .install(uids(), &eps())
            .unwrap_err();
        assert!(matches!(err, ProvisionError::NotReferenced(_)));
        assert!(
            err.to_string()
                .contains("no anchor rule that reaches com.apple/com.komira.js.pool")
        );
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
        assert!(!dir.join(NO_TOKEN_FILE).exists());
    }

    #[test]
    fn a_tokenless_enable_is_recorded_and_not_repeated_while_pf_stays_on() {
        let dir = dir("notoken-on");
        let mut pf = Fake::new(dir.path());
        pf.no_token = true;
        let a = anchor();
        let p = Provisioner::new(&pf, &a, dir.path());
        p.install(uids(), &eps()).unwrap();
        assert!(!dir.join(TOKEN_FILE).exists());
        assert!(dir.join(NO_TOKEN_FILE).exists());
        assert!(!dir.join(PENDING_FILE).exists());
        for _ in 0..3 {
            p.install(uids(), &eps()).unwrap();
        }
        let enables = pf.calls().iter().filter(|c| *c == "-E").count();
        assert_eq!(enables, 1);
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
    fn the_next_install_finds_a_pending_marker_and_clears_it() {
        let dir = dir("pending2");
        fs::write(dir.join(PENDING_FILE), "enabling\n").unwrap();
        let pf = Fake::new(dir.path());
        let a = anchor();
        Provisioner::new(&pf, &a, dir.path())
            .install(uids(), &eps())
            .unwrap();
        assert!(!dir.join(PENDING_FILE).exists());
    }

    #[test]
    fn every_pfctl_call_of_install_that_fails_stops_it_and_says_which() {
        // The calls of a healthy install, by index: load, info, enable, readback, info,
        // main ruleset, anchor.
        let want = ["-f", "info", "-E", "-sr", "info", "-sr", "-sr"];
        for (n, arg) in want.iter().enumerate() {
            for not_started in [true, false] {
                let dir = dir("fail");
                let mut pf = Fake::new(dir.path());
                pf.fail = Some((n, not_started));
                let a = anchor();
                let err = Provisioner::new(&pf, &a, dir.path())
                    .install(uids(), &eps())
                    .unwrap_err();
                assert_eq!(pf.calls().len(), n + 1, "{arg} {n} {not_started}");
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
        assert!(err.to_string().contains("file/state"));
        assert!(pf.calls().is_empty());
    }

    #[test]
    fn a_state_file_that_cannot_be_replaced_is_a_state_error() {
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
    fn a_temporary_file_that_cannot_be_opened_is_a_state_error() {
        let dir = dir("notemp");
        fs::create_dir(dir.join(format!("pool.pf.{}.tmp", std::process::id()))).unwrap();
        let pf = Fake::new(dir.path());
        let a = anchor();
        let err = Provisioner::new(&pf, &a, dir.path())
            .install(uids(), &eps())
            .unwrap_err();
        assert!(matches!(err, ProvisionError::State { .. }));
    }

    #[test]
    fn check_passes_and_changes_nothing_however_often_it_runs() {
        let (dir, pf) = installed("check");
        let a = anchor();
        let p = Provisioner::new(&pf, &a, dir.path());
        let snapshot = || {
            [RULES_FILE, READBACK_FILE, TOKEN_FILE]
                .map(|f| fs::read_to_string(dir.join(f)).unwrap())
        };
        let (files, before) = (snapshot(), pf.calls().len());
        p.check(uids(), &eps()).unwrap();
        p.check(uids(), &eps()).unwrap();
        let after = &pf.calls()[before..];
        // Three reads each: pf's status, the main ruleset, the anchor.
        assert_eq!(
            after,
            [
                "-s info".to_owned(),
                "-sr".to_owned(),
                format!("-a {NAME} -sr"),
                "-s info".to_owned(),
                "-sr".to_owned(),
                format!("-a {NAME} -sr"),
            ]
        );
        assert_eq!(snapshot(), files);
    }

    #[test]
    fn check_fails_when_pf_has_been_disabled() {
        let (dir, pf) = installed("disabled");
        pf.enabled.set(false);
        let a = anchor();
        let before = pf.calls().len();
        let err = Provisioner::new(&pf, &a, dir.path())
            .check(uids(), &eps())
            .unwrap_err();
        assert!(matches!(err, ProvisionError::PfDisabled));
        assert!(err.to_string().contains("not enabled"));
        assert_eq!(pf.calls()[before..], ["-s info"]);
    }

    #[test]
    fn check_fails_when_the_main_ruleset_dropped_the_anchor_reference() {
        let (dir, pf) = installed("dropped");
        *pf.main.borrow_mut() = "block all\n".to_owned();
        let a = anchor();
        let before = pf.calls().len();
        let err = Provisioner::new(&pf, &a, dir.path())
            .check(uids(), &eps())
            .unwrap_err();
        assert!(matches!(err, ProvisionError::NotReferenced(_)));
        assert_eq!(pf.calls()[before..], ["-s info", "-sr"]);
    }

    #[test]
    fn check_reports_the_drift_when_the_anchor_no_longer_reads_back_as_installed() {
        let (dir, pf) = installed("drift");
        *pf.loaded.borrow_mut() = Some("pass out quick all\n".to_owned());
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
    fn a_failed_load_leaves_check_unable_to_pass_for_either_policy() {
        let (dir, mut pf) = installed("failed-load");
        let a = anchor();
        let other: Vec<Endpoint> = vec!["192.0.2.11:443".parse().unwrap()];
        // The calls of the first install are 0..7; the next load is call 7.
        pf.fail = Some((7, false));
        let p = Provisioner::new(&pf, &a, dir.path());
        assert!(matches!(
            p.install(uids(), &other),
            Err(ProvisionError::Failed { .. })
        ));
        // The new rules file is there, the old readback is not: nothing says what is loaded.
        assert!(matches!(
            p.check(uids(), &other),
            Err(ProvisionError::State { .. })
        ));
        assert!(matches!(
            p.check(uids(), &eps()),
            Err(ProvisionError::Drift { .. })
        ));
    }

    #[test]
    fn install_refuses_an_empty_or_short_readback_and_stores_none() {
        for text in ["", "\n  \n", "# only a comment\n", "pass out quick all\n"] {
            let dir = dir("short");
            let pf = Fake::new(dir.path());
            *pf.loaded.borrow_mut() = Some(text.to_owned());
            let a = anchor();
            let err = Provisioner::new(&pf, &a, dir.path())
                .install(uids(), &eps())
                .unwrap_err();
            assert!(
                matches!(err, ProvisionError::BadReadback { .. }),
                "{text:?}"
            );
            assert!(err.to_string().contains("fewer than the 5"), "{err}");
            assert!(!dir.join(READBACK_FILE).exists());
        }
    }

    #[test]
    fn check_refuses_an_empty_stored_readback_even_when_the_anchor_reads_empty() {
        let (dir, pf) = installed("empty-stored");
        fs::write(dir.join(READBACK_FILE), "\n").unwrap();
        *pf.loaded.borrow_mut() = Some(String::new());
        let a = anchor();
        let err = Provisioner::new(&pf, &a, dir.path())
            .check(uids(), &eps())
            .unwrap_err();
        assert!(matches!(
            err,
            ProvisionError::BadReadback { got: 0, want: 5 }
        ));
    }

    #[test]
    fn a_readback_file_that_cannot_be_discarded_is_a_state_error() {
        let dir = dir("discard");
        let stuck = dir.join(READBACK_FILE);
        fs::create_dir(&stuck).unwrap();
        fs::write(stuck.join("inner"), "x").unwrap();
        let pf = Fake::new(dir.path());
        let a = anchor();
        let err = Provisioner::new(&pf, &a, dir.path())
            .install(uids(), &eps())
            .unwrap_err();
        assert!(matches!(err, ProvisionError::State { .. }));
        assert!(pf.calls().is_empty());
    }

    #[test]
    fn check_ignores_blank_lines_and_indentation_in_what_pfctl_prints() {
        let (dir, pf) = installed("blank");
        let a = anchor();
        let text = render(uids(), &eps(), &a);
        *pf.loaded.borrow_mut() = Some(format!("\n  {}\n\n", text.replace('\n', "\n\n  ")));
        Provisioner::new(&pf, &a, dir.path())
            .check(uids(), &eps())
            .unwrap();
    }

    #[test]
    fn a_changed_policy_is_drift_until_install_runs() {
        let (dir, pf) = installed("changed");
        let a = anchor();
        let p = Provisioner::new(&pf, &a, dir.path());
        let other: Vec<Endpoint> = vec!["192.0.2.11:443".parse().unwrap()];
        let err = p.check(uids(), &other).unwrap_err();
        assert!(matches!(err, ProvisionError::Drift { .. }));
        assert!(err.to_string().contains("192.0.2.11"));
        // The anchor was not even read: the stored rules already differ.
        assert_eq!(pf.calls().last().unwrap(), "-sr");
        p.install(uids(), &other).unwrap();
        p.check(uids(), &other).unwrap();
        assert!(p.check(uids(), &eps()).is_err());
    }

    #[test]
    fn a_missing_readback_or_rules_file_is_a_could_not_tell_error() {
        let (dir, pf) = installed("missing");
        let a = anchor();
        let p = Provisioner::new(&pf, &a, dir.path());
        fs::remove_file(dir.join(READBACK_FILE)).unwrap();
        let err = p.check(uids(), &eps()).unwrap_err();
        assert!(matches!(err, ProvisionError::State { .. }));
        assert!(err.to_string().contains(READBACK_FILE));
        fs::remove_file(dir.join(RULES_FILE)).unwrap();
        let err = p.check(uids(), &eps()).unwrap_err();
        assert!(err.to_string().contains(RULES_FILE));
    }

    #[test]
    fn enforced_is_ok_when_enabled_referenced_and_loaded_and_names_each_way_it_is_not() {
        let (dir, pf) = installed("enforced");
        let a = anchor();
        let e = eps();
        let cfg = Config {
            pf: &pf,
            anchor: &a,
            dir: dir.path(),
            uids: uids(),
            endpoints: &e,
        };
        assert!(enforced(&cfg).is_ok());
        let main = pf.main.replace(String::new());
        assert!(matches!(
            enforced(&cfg),
            Err(ProvisionError::NotReferenced(_))
        ));
        pf.main.replace(main);
        *pf.loaded.borrow_mut() = Some(String::new());
        assert!(matches!(enforced(&cfg), Err(ProvisionError::Drift { .. })));
        pf.loaded.replace(None);
        pf.enabled.set(false);
        assert!(matches!(enforced(&cfg), Err(ProvisionError::PfDisabled)));
        let mut broken = on(dir.path());
        broken.fail = Some((0, true));
        let cfg = Config { pf: &broken, ..cfg };
        assert!(matches!(enforced(&cfg), Err(ProvisionError::Run { .. })));
    }

    #[test]
    fn remove_flushes_only_this_anchor_gives_the_token_back_and_deletes_the_state() {
        let (dir, pf) = installed("remove");
        fs::write(dir.join(PENDING_FILE), "x").unwrap();
        fs::write(dir.join(NO_TOKEN_FILE), "x").unwrap();
        let a = anchor();
        let before = pf.calls().len();
        Provisioner::new(&pf, &a, dir.path()).remove().unwrap();
        assert_eq!(
            pf.calls()[before..],
            [
                format!("-a {NAME} -F rules"),
                format!("-a {NAME}/check -F rules"),
                "-X 123456".to_owned()
            ]
        );
        for f in [
            RULES_FILE,
            READBACK_FILE,
            TOKEN_FILE,
            PENDING_FILE,
            NO_TOKEN_FILE,
        ] {
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
        assert_eq!(pf.calls().len(), 4);
        assert!(!pf.calls().iter().any(|c| c.starts_with("-X")));
    }

    #[test]
    fn remove_ignores_a_failed_flush_of_a_leftover_child() {
        let dir = dir("remove-child");
        let mut pf = Fake::new(dir.path());
        pf.fail = Some((1, false));
        let a = anchor();
        Provisioner::new(&pf, &a, dir.path()).remove().unwrap();
        assert_eq!(pf.calls().len(), 2);
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
        assert_eq!(pf.calls().len(), before + 2);
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
