//! A fake `pfctl` for the tests of the provisioner and the verbs.

use std::cell::{Cell, RefCell};
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use crate::pfctl::{Output, Pfctl};
use crate::provision::RULES_FILE;

/// A `pfctl` that answers like a healthy one and records what it was asked.
pub(crate) struct Fake {
    calls: RefCell<Vec<Vec<String>>>,
    /// The token the next `-E` prints.
    pub token: RefCell<String>,
    /// What the main ruleset prints.
    pub main: RefCell<String>,
    /// What the anchor prints; `None` echoes the rules file it was loaded from.
    pub loaded: RefCell<Option<String>>,
    /// What `-a <anchor> -s rules -vv` prints.
    pub counters: RefCell<String>,
    /// Fails the call with this index, by not starting (`true`) or by exit code 1.
    pub fail: Option<(usize, bool)>,
    /// Fails every `-X`.
    pub fail_release: bool,
    /// Prints this instead of a token line.
    pub no_token: bool,
    /// Whether pf is on; `-E` turns it on unless `stay_off`.
    pub enabled: Cell<bool>,
    /// `-E` does not turn pf on.
    pub stay_off: bool,
    dir: PathBuf,
}

impl Fake {
    pub fn new(dir: &Path) -> Self {
        Self {
            calls: RefCell::default(),
            token: RefCell::new("123456".to_owned()),
            main: RefCell::new(
                "scrub-anchor \"com.apple/*\" all fragment\nanchor \"com.apple/*\" all\n"
                    .to_owned(),
            ),
            loaded: RefCell::default(),
            counters: RefCell::default(),
            fail: None,
            fail_release: false,
            no_token: false,
            enabled: Cell::new(false),
            stay_off: false,
            dir: dir.to_path_buf(),
        }
    }

    fn turn_on(&self) {
        if !self.stay_off {
            self.enabled.set(true);
        }
    }

    pub fn calls(&self) -> Vec<String> {
        self.calls.borrow().iter().map(|c| c.join(" ")).collect()
    }
}

fn ok(stdout: &str, stderr: &str) -> io::Result<Output> {
    Ok(Output {
        code: Some(0),
        stdout: stdout.to_owned(),
        stderr: stderr.to_owned(),
    })
}

impl Pfctl for Fake {
    fn run(&self, args: &[&str]) -> io::Result<Output> {
        let index = self.calls.borrow().len();
        self.calls
            .borrow_mut()
            .push(args.iter().map(ToString::to_string).collect());
        match self.fail {
            Some((n, true)) if n == index => {
                return Err(io::Error::new(io::ErrorKind::NotFound, "no pfctl"));
            }
            Some((n, false)) if n == index => {
                return Ok(Output {
                    code: Some(1),
                    stdout: String::new(),
                    stderr: "pfctl: nope\n".to_owned(),
                });
            }
            _ => {}
        }
        let echo = |path: &str| fs::read_to_string(path).unwrap_or_default();
        match args {
            ["-E"] if self.no_token => {
                self.turn_on();
                ok("", "pf enabled\n")
            }
            ["-E"] => {
                self.turn_on();
                ok(
                    "",
                    &format!("pf enabled\nToken : {}\n", self.token.borrow()),
                )
            }
            ["-X", _] if self.fail_release => Ok(Output {
                code: None,
                stdout: String::new(),
                stderr: "no such token".to_owned(),
            }),
            ["-s", "info"] if self.enabled.get() => ok("Status: Enabled for 0 days 00:01:02\n", ""),
            ["-s", "info"] => ok("Status: Disabled\n", ""),
            ["-sr"] => ok(&self.main.borrow(), ""),
            ["-a", _, "-s", "rules", "-vv"] => ok(&self.counters.borrow(), ""),
            ["-a", _, "-sr"] => ok(
                &self
                    .loaded
                    .borrow()
                    .clone()
                    .unwrap_or_else(|| echo(self.dir.join(RULES_FILE).to_str().unwrap())),
                "",
            ),
            _ => ok("", ""),
        }
    }
}
