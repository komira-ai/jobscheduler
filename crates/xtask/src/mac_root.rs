//! `cargo xtask mac-root-tests --work <dir>`: the Mac root tests (every one `#[ignore]`d
//! under a plain `cargo test`, as each needs root and changes the machine) run on one Mac
//! in one command, with a table of every test's result.
//!
//! - the safety check comes first: the Mac's user list (`dscl . -list /Users UniqueID`,
//!   read as this user) must hold no lease user ([`check_users`]): no uid of the agent's
//!   pool range [`PRODUCTION_UIDS`] or of the tests' own [`TEST_UIDS`], and no name that
//!   starts `kbf-lease-`. The tests make and delete accounts named `kbf-acct-<n>` with uids of
//!   their own, so they run only on a Mac whose agent has not made its pool there;
//! - every target ([`TARGETS`]) and the `js-mac-provision` program are built as this user
//!   ([`test_build::build`], [`test_build::build_bin`]), before anything runs as root;
//! - through `sudo -n`: the work directory `--work` (which must not exist) and a directory
//!   of each target's are made root-owned, mode 0755, and the binaries are copied into
//!   `<work>/bin`, mode 0755. The tests' pool users run the test binary itself, and the
//!   build's directory below this user's home is closed to them;
//! - each target's binary runs as root ([`run_args`]): `/usr/bin/env` with the variables the
//!   target reads ([`env_for`]), `--ignored --test-threads=1`, from its package's directory,
//!   its output in `target/mac-root-tests/<target>.log`;
//! - after each `js-agent` target, pass or fail, the users it made are deleted
//!   ([`to_delete`]): only those named `kbf-acct-<n>` with a uid the tests use. A user
//!   still there afterwards is a failure;
//! - the output of every binary is read into one row per test ([`judge`]): `pass`, `FAIL`
//!   with the line its panic printed, or `not-run` with the reason. A binary that reports no
//!   test, or exits with a failure and names no failed test, is a failure (never a silent
//!   pass). The table ([`render`]) is printed and written to
//!   `target/mac-root-tests/table.tsv`, after the macOS version and build.
//!
//! `--only <key>` (repeatable) runs just the targets of those [`Target::key`]s ([`select`]),
//! in [`TARGETS`] order, and builds `js-mac-provision` only for its own target; a key that
//! is no target's is refused first, on any system. The release's beta stage runs
//! `--only native_isolation` on a hosted Mac of its own
//! (`.github/workflows/js-agent-native-isolation.yml`).
//!
//! Every selected target runs even when one before it failed; the verb fails if any row is
//! `FAIL`.

use std::ffi::{OsStr, OsString};
use std::fs::File;
use std::ops::RangeInclusive;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitStatus, Stdio};

use crate::test_build::{self, Built, Error};

/// The uids that an earlier release's lease users have (the root helper it ran lent these to
/// jobs): a user with one is a lease user of a farm Mac.
pub const PRODUCTION_UIDS: RangeInclusive<i64> = 600..=699;
/// Every uid the tests use: the account pool of `native_privileged` ([`NATIVE_PRIVILEGED_UIDS`]),
/// the accounts of `native_isolation`, `native_inbound_pf` and `native_fence`, and the unprivileged uid of
/// `js-mac-pool` and `js-treestore` ([`PLAIN_UID`]).
pub const TEST_UIDS: RangeInclusive<i64> = 4300..=4320;
/// The slots of `native_privileged`'s account pool.
pub const NATIVE_PRIVILEGED_UIDS: RangeInclusive<i64> = 4300..=4309;
/// The one slot of the account pool `native_isolation` opens.
pub const ISOLATION_UID: u32 = 4310;
/// The one slot of the account pool `native_inbound_pf` opens.
pub const INBOUND_UID: u32 = 4311;
/// The one slot of the account pool `native_fence` opens.
pub const FENCE_UID: u32 = 4312;
/// The uids of the accounts the `js-agent` targets make, which the verb deletes after them.
pub const MADE_UIDS: RangeInclusive<i64> = 4300..=4312;
/// The unprivileged uid and gid of `js-mac-pool` and `js-treestore`: no account is needed.
pub const PLAIN_UID: u32 = 4320;
/// The prefix of the accounts the `js-agent` targets' account pools make.
pub const POOL_USER_PREFIX: &str = "kbf-acct-";
/// The prefix of every lease user's name, of any release.
pub const LEASE_USER_PREFIX: &str = "kbf-lease-";
/// The directory service tool the user list is read with.
pub const DSCL: &str = "/usr/bin/dscl";
/// The tool the macOS version is read with.
pub const SW_VERS: &str = "/usr/bin/sw_vers";
/// The package of the pf root test, and of the program it loads its anchor with.
pub const PROVISION: &str = "js-mac-provision";
/// The test filtered out of `native_privileged`, and why: the table says so.
pub const VM_TEST: (&str, &str) = (
    "vm::a_command_runs_in_a_fresh_vm_and_the_vm_is_destroyed",
    "needs a signed js-vmm and a base image",
);
/// The most characters of a failure's reason the table keeps.
pub const REASON_CHARS: usize = 200;

/// One test target the verb runs.
#[derive(Debug, PartialEq, Eq)]
pub struct Target {
    /// Its name in the table, its log's and its binary's file name.
    pub key: &'static str,
    /// Its package.
    pub package: &'static str,
    /// Its integration test target.
    pub test: &'static str,
    /// Whether it is a `js-agent` target, whose account pool makes accounts the verb deletes after.
    pub makes_users: bool,
}

/// Every target, in the order they run.
pub const TARGETS: [Target; 8] = [
    Target {
        key: "pf_as_root",
        package: PROVISION,
        test: "pf_as_root",
        makes_users: false,
    },
    Target {
        key: "treestore",
        package: "js-treestore",
        test: "privileged",
        makes_users: false,
    },
    Target {
        key: "mac_pool",
        package: "js-mac-pool",
        test: "privileged",
        makes_users: false,
    },
    Target {
        key: "mac_pool_trace",
        package: "js-mac-pool",
        test: "trace_base_set",
        makes_users: false,
    },
    Target {
        key: "native_privileged",
        package: "js-agent",
        test: "native_privileged",
        makes_users: true,
    },
    Target {
        key: "native_isolation",
        package: "js-agent",
        test: "native_isolation",
        makes_users: true,
    },
    Target {
        key: "native_inbound_pf",
        package: "js-agent",
        test: "native_inbound_pf",
        makes_users: true,
    },
    Target {
        key: "native_fence",
        package: "js-agent",
        test: "native_fence",
        makes_users: true,
    },
];

/// What the verb asks of the machine: the seam the tests put a fake behind.
pub trait Machine {
    /// Builds the integration test targets `tests` of `package` as this user: cargo's JSON
    /// messages.
    ///
    /// # Errors
    /// Cargo could not be started, or the build failed.
    fn build(&self, package: &str, tests: &[&str]) -> Result<String, Error>;

    /// Builds the binary target `bin` of `package` as this user: cargo's JSON messages.
    ///
    /// # Errors
    /// Cargo could not be started, or the build failed.
    fn build_bin(&self, package: &str, bin: &str) -> Result<String, Error>;

    /// Runs `program args` as this user: what it printed on stdout.
    ///
    /// # Errors
    /// The program could not be started, or exited with a failure status.
    fn read(&self, program: &str, args: &[&str]) -> Result<String, Error>;

    /// Runs `args` as root: its exit status. Its output is this process's.
    ///
    /// # Errors
    /// The program could not be started.
    fn root(&self, args: &[OsString]) -> Result<ExitStatus, Error>;

    /// Runs `args` as root in `cwd`, its stdout and stderr written to `log`: its exit status
    /// and what the log holds.
    ///
    /// # Errors
    /// The log could not be made or read, or the program could not be started.
    fn root_logged(
        &self,
        args: &[OsString],
        cwd: &Path,
        log: &Path,
    ) -> Result<(ExitStatus, String), Error>;
}

/// The real machine: cargo, the user's own programs, and `sudo -n`.
pub struct RealMachine<'a> {
    /// The cargo program.
    pub cargo: &'a OsStr,
    /// The repository root, where cargo runs.
    pub root: &'a Path,
    /// The sudo program; it is run with `-n`, so it never prompts.
    pub sudo: &'a Path,
}

impl Machine for RealMachine<'_> {
    fn build(&self, package: &str, tests: &[&str]) -> Result<String, Error> {
        test_build::build(self.cargo, self.root, package, tests)
    }

    fn build_bin(&self, package: &str, bin: &str) -> Result<String, Error> {
        test_build::build_bin(self.cargo, self.root, package, bin)
    }

    fn read(&self, program: &str, args: &[&str]) -> Result<String, Error> {
        let out = Command::new(program)
            .args(args)
            .stderr(Stdio::inherit())
            .output()
            .map_err(Error::spawn(program.to_owned()))?;
        if !out.status.success() {
            return Err(Error::Failed(format!(
                "{program} {} failed: {}",
                args.join(" "),
                out.status
            )));
        }
        Ok(String::from_utf8_lossy(&out.stdout).into_owned())
    }

    fn root(&self, args: &[OsString]) -> Result<ExitStatus, Error> {
        Command::new(self.sudo)
            .arg("-n")
            .args(args)
            .status()
            .map_err(Error::spawn(self.sudo.display().to_string()))
    }

    fn root_logged(
        &self,
        args: &[OsString],
        cwd: &Path,
        log: &Path,
    ) -> Result<(ExitStatus, String), Error> {
        let file = |e: std::io::Error| Error::Failed(format!("{}: {e}", log.display()));
        let out = File::create(log).map_err(file)?;
        let err = out.try_clone().map_err(file)?;
        let status = Command::new(self.sudo)
            .arg("-n")
            .args(args)
            .current_dir(cwd)
            .stdout(out)
            .stderr(err)
            .status()
            .map_err(Error::spawn(self.sudo.display().to_string()))?;
        let text = std::fs::read(log).map_err(file)?;
        Ok((status, String::from_utf8_lossy(&text).into_owned()))
    }
}

/// The users of `listing`, the output of `dscl . -list /Users UniqueID`: name and uid.
///
/// # Errors
/// A line that is not a name and a whole number, or no user at all (a listing that is not
/// one is never read as "no lease user").
pub fn parse_users(listing: &str) -> Result<Vec<(String, i64)>, Error> {
    let mut users = Vec::new();
    for line in listing.lines().filter(|l| !l.trim().is_empty()) {
        let mut words = line.split_whitespace();
        let user = match (words.next(), words.next(), words.next()) {
            (Some(name), Some(uid), None) => uid.parse().ok().map(|uid| (name.to_owned(), uid)),
            _ => None,
        };
        let user = user.ok_or_else(|| {
            Error::Failed(format!(
                "dscl printed a line that is not a user and its uid: {line:?}"
            ))
        })?;
        users.push(user);
    }
    if users.is_empty() {
        return Err(Error::Failed("dscl listed no user".to_owned()));
    }
    Ok(users)
}

/// Refuses a Mac that has a lease user: a uid of [`PRODUCTION_UIDS`] or [`TEST_UIDS`], or a
/// name that starts with [`LEASE_USER_PREFIX`].
///
/// # Errors
/// The refusal, naming every such user.
pub fn check_users(users: &[(String, i64)]) -> Result<(), Error> {
    let found: Vec<String> = users
        .iter()
        .filter(|(name, uid)| {
            PRODUCTION_UIDS.contains(uid)
                | TEST_UIDS.contains(uid)
                | name.starts_with(LEASE_USER_PREFIX)
        })
        .map(|(name, uid)| format!("{name} ({uid})"))
        .collect();
    if found.is_empty() {
        return Ok(());
    }
    Err(Error::Failed(format!(
        "refused: the root tests run only on a Mac with no lease users, because they make and \
         delete users named {POOL_USER_PREFIX}<n> (uids {}-{}); this Mac has {}",
        TEST_UIDS.start(),
        TEST_UIDS.end(),
        found.join(", ")
    )))
}

/// The users a `js-agent` target left that the verb deletes: named
/// `kbf-acct-<n>`, with a uid of [`MADE_UIDS`]. No other user is ever deleted.
pub fn to_delete(users: &[(String, i64)]) -> Vec<String> {
    users
        .iter()
        .filter(|(name, uid)| name.starts_with(POOL_USER_PREFIX) & MADE_UIDS.contains(uid))
        .map(|(name, _)| name.clone())
        .collect()
}

/// `<work>/<name>`, as one argument.
fn under(work: &Path, name: &str) -> OsString {
    work.join(name).into_os_string()
}

/// `<name>=<value>`, for a value that need not be UTF-8.
fn var(name: &str, value: impl AsRef<OsStr>) -> OsString {
    let mut out = OsString::from(name);
    out.push("=");
    out.push(value);
    out
}

/// The directories of `target` under `work` the verb makes as root before it runs, in
/// order (a parent before its child).
pub fn dirs_for(key: &str, work: &Path) -> Vec<PathBuf> {
    let names: &[&str] = match key {
        "treestore" => &["treestore", "treestore/root"],
        "mac_pool" => &["mac-pool"],
        "mac_pool_trace" => &["mac-pool-trace"],
        "native_privileged" => &["native-privileged"],
        "native_isolation" => &["native-isolation"],
        "native_inbound_pf" => &["native-inbound-pf"],
        "native_fence" => &["native-fence"],
        _ => &[],
    };
    names.iter().map(|name| work.join(name)).collect()
}

/// The variables the target `key` reads, for a work directory `work` and the invoking
/// user `owner_uid`.
pub fn env_for(key: &str, work: &Path, owner_uid: u32) -> Vec<OsString> {
    let plain = |prefix: &str| {
        vec![
            var(&format!("{prefix}_UID"), PLAIN_UID.to_string()),
            var(&format!("{prefix}_GID"), PLAIN_UID.to_string()),
        ]
    };
    let pool = |dir: &str| {
        let mut env = vec![
            var("JS_MAC_POOL_PRIV_ROOT", under(work, dir)),
            var("JS_MAC_POOL_PRIV_OWNER_UID", owner_uid.to_string()),
        ];
        env.extend(plain("JS_MAC_POOL_PRIV"));
        env
    };
    match key {
        "pf_as_root" => vec![var(
            "JS_MAC_PROVISION_BIN",
            work.join("bin").join(PROVISION),
        )],
        "treestore" => {
            let mut env = vec![
                var("JS_TREESTORE_PRIV_PARENT", under(work, "treestore")),
                var("JS_TREESTORE_PRIV_ROOT", under(work, "treestore/root")),
            ];
            env.extend(plain("JS_TREESTORE_PRIV"));
            env
        }
        "mac_pool" => pool("mac-pool"),
        "mac_pool_trace" => pool("mac-pool-trace"),
        "native_privileged" => vec![var("JS_NATIVE_PRIV_ROOT", under(work, "native-privileged"))],
        "native_isolation" => vec![
            var("JS_NATIVE_ISO_ROOT", under(work, "native-isolation")),
            var("JS_NATIVE_ISO_UID", ISOLATION_UID.to_string()),
        ],
        "native_inbound_pf" => vec![
            var("JS_NATIVE_INBOUND_ROOT", under(work, "native-inbound-pf")),
            var("JS_NATIVE_INBOUND_UID", INBOUND_UID.to_string()),
        ],
        "native_fence" => vec![
            var("JS_NATIVE_FENCE_ROOT", under(work, "native-fence")),
            var("JS_NATIVE_FENCE_UID", FENCE_UID.to_string()),
            var("JS_NATIVE_FENCE_OWNER_UID", owner_uid.to_string()),
        ],
        _ => Vec::new(),
    }
}

/// The libtest arguments after `--ignored --test-threads=1` for the target `key`: a
/// filter, or the tests it skips.
pub fn extra_for(key: &str) -> Vec<OsString> {
    match key {
        "mac_pool_trace" => vec!["trace".into()],
        "native_privileged" => vec!["--skip".into(), "vm::".into()],
        _ => Vec::new(),
    }
}

/// The tests of the target `key` the verb does not run, each with the reason.
pub fn skipped_for(key: &str) -> Vec<(&'static str, &'static str)> {
    match key {
        "native_privileged" => vec![VM_TEST],
        _ => Vec::new(),
    }
}

/// The arguments for `sudo -n` that run the target `key`'s binary in `<work>/bin`: through
/// `/usr/bin/env` with the target's variables, its ignored tests only, one at a time.
pub fn run_args(key: &str, work: &Path, owner_uid: u32) -> Vec<OsString> {
    let mut args = vec![OsString::from("/usr/bin/env")];
    args.extend(env_for(key, work, owner_uid));
    args.push(work.join("bin").join(key).into_os_string());
    args.extend(["--ignored", "--test-threads=1"].map(OsString::from));
    args.extend(extra_for(key));
    args
}

/// A test's result.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Outcome {
    /// It passed.
    Pass,
    /// It failed.
    Fail,
    /// It did not run.
    NotRun,
}

impl Outcome {
    /// The word the table shows.
    pub fn word(self) -> &'static str {
        match self {
            Self::Pass => "pass",
            Self::Fail => "FAIL",
            Self::NotRun => "not-run",
        }
    }
}

/// One row of the table.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Row {
    /// The target ([`Target::key`]), or `cleanup`.
    pub target: String,
    /// The test, `(binary)` for the binary as a whole, or a user name for `cleanup`.
    pub test: String,
    /// Its result.
    pub outcome: Outcome,
    /// Why it failed or did not run; empty for a pass.
    pub reason: String,
}

impl Row {
    /// A row of `target`.
    pub fn new(target: &str, test: &str, outcome: Outcome, reason: &str) -> Row {
        Row {
            target: target.to_owned(),
            test: test.to_owned(),
            outcome,
            reason: reason.to_owned(),
        }
    }
}

/// The result lines of libtest's output `text`: `test <name> ... ok|FAILED|ignored[, ...]`,
/// in order, each with its test's name.
pub fn results(text: &str) -> Vec<(String, Outcome)> {
    text.lines()
        .filter_map(|line| {
            let (name, verdict) = line.strip_prefix("test ")?.split_once(" ... ")?;
            let outcome = match verdict.trim_end() {
                "ok" => Outcome::Pass,
                "FAILED" => Outcome::Fail,
                v if v.starts_with("ignored") => Outcome::NotRun,
                _ => return None,
            };
            Some((name.to_owned(), outcome))
        })
        .collect()
}

/// `text` cut to [`REASON_CHARS`] characters.
fn cut(text: &str) -> String {
    text.chars().take(REASON_CHARS).collect()
}

/// Why the test `name` failed, from the `---- <name> stdout ----` block of libtest's
/// output `text`: the line after the one that says it panicked.
pub fn reason_of(text: &str, name: &str) -> String {
    let header = format!("---- {name} stdout ----");
    let mut lines = text.lines().skip_while(|line| *line != header).skip(1);
    let block = lines.by_ref().take_while(|line| !line.starts_with("---- "));
    let mut after = block.skip_while(|line| !line.contains("panicked at"));
    match (after.next(), after.next()) {
        (Some(_), Some(message)) if !message.trim().is_empty() => cut(message.trim()),
        (Some(_), _) => "it panicked; no message (see the log)".to_owned(),
        (None, _) => "no panic in its output (see the log)".to_owned(),
    }
}

/// The rows of the target `key` from its binary's output `text`, which ended with
/// `status`, its log at `log`: one per test it reported, and a failure of the binary as a
/// whole when it reported none, or exited with a failure and named no failed test.
pub fn judge(key: &str, text: &str, status: &str, success: bool, log: &Path) -> Vec<Row> {
    let mut rows: Vec<Row> = results(text)
        .into_iter()
        .map(|(name, outcome)| {
            let reason = match outcome {
                Outcome::Pass => String::new(),
                Outcome::Fail => reason_of(text, &name),
                Outcome::NotRun => "ignored by the binary".to_owned(),
            };
            Row::new(key, &name, outcome, &reason)
        })
        .collect();
    let failed = rows.iter().any(|row| row.outcome == Outcome::Fail);
    if rows.is_empty() {
        let why = format!("reported no test; {status}; see {}", log.display());
        rows.push(Row::new(key, "(binary)", Outcome::Fail, &why));
    } else if !success & !failed {
        let why = format!("{status} with no failed test; see {}", log.display());
        rows.push(Row::new(key, "(binary)", Outcome::Fail, &why));
    }
    rows
}

/// The table: a header line with the macOS `version`, then one tab-separated line per row
/// (target, test, result, reason).
pub fn tsv(version: &str, rows: &[Row]) -> String {
    let mut out = format!("# macOS {version}\ntarget\ttest\tresult\treason\n");
    for row in rows {
        out.push_str(&format!(
            "{}\t{}\t{}\t{}\n",
            row.target,
            row.test,
            row.outcome.word(),
            row.reason
        ));
    }
    out
}

/// The table for a terminal: the macOS `version`, then the rows in aligned columns.
pub fn render(version: &str, rows: &[Row]) -> String {
    let width =
        |f: fn(&Row) -> usize, title: &str| rows.iter().map(f).fold(title.len(), usize::max);
    let target = width(|r| r.target.len(), "target");
    let test = width(|r| r.test.len(), "test");
    let mut out = format!("macOS {version}\n");
    let mut line = |a: &str, b: &str, c: &str, d: &str| {
        out.push_str(format!("{a:target$}  {b:test$}  {c:7}  {d}").trim_end());
        out.push('\n');
    };
    line("target", "test", "result", "reason");
    for row in rows {
        line(&row.target, &row.test, row.outcome.word(), &row.reason);
    }
    out
}

/// What `mac-root-tests` runs and where.
pub struct MacRootTests<'a> {
    /// The operating system this runs on (`std::env::consts::OS`); the verb refuses all
    /// but `macos`.
    pub os: &'a str,
    /// The machine.
    pub machine: &'a dyn Machine,
    /// The repository root: the logs and the table go to its `target/mac-root-tests/`.
    pub root: &'a Path,
    /// The work directory to make as root; it must not exist, and its parent must.
    pub work: &'a Path,
    /// The uid of the user running the verb.
    pub owner_uid: u32,
    /// The keys of the targets to run ([`select`]); empty runs every one.
    pub only: &'a [String],
}

/// Fails unless `work` is an absolute path that does not exist, in a directory that does.
fn check_work(work: &Path) -> Result<(), Error> {
    let shown = work.display();
    if !work.is_absolute() {
        return Err(Error::Failed(format!(
            "--work {shown} is not an absolute path"
        )));
    }
    if work.symlink_metadata().is_ok() {
        return Err(Error::Failed(format!(
            "--work {shown} exists; name a new directory"
        )));
    }
    if !work.parent().is_some_and(Path::is_dir) {
        return Err(Error::Failed(format!(
            "--work {shown}: its parent is not a directory"
        )));
    }
    Ok(())
}

/// Runs `args` as root, and fails unless they exit 0.
fn as_root(machine: &dyn Machine, args: Vec<OsString>) -> Result<(), Error> {
    let status = machine.root(&args)?;
    if status.success() {
        return Ok(());
    }
    let shown: Vec<_> = args.iter().map(|a| a.to_string_lossy()).collect();
    Err(Error::Failed(format!(
        "sudo -n {} failed: {status}",
        shown.join(" ")
    )))
}

/// The users of the Mac.
fn users(machine: &dyn Machine) -> Result<Vec<(String, i64)>, Error> {
    parse_users(&machine.read(DSCL, &[".", "-list", "/Users", "UniqueID"])?)
}

/// The macOS version and build: `<version> (<build>)`.
fn version(machine: &dyn Machine) -> Result<String, Error> {
    let product = machine.read(SW_VERS, &["-productVersion"])?;
    let build = machine.read(SW_VERS, &["-buildVersion"])?;
    Ok(format!("{} ({})", product.trim(), build.trim()))
}

/// The targets the verb runs, in [`TARGETS`] order: every one when `only` is empty, else
/// those whose [`Target::key`] `only` names.
///
/// # Errors
/// A name in `only` that is no target's key, naming the keys there are.
pub fn select(only: &[String]) -> Result<Vec<&'static Target>, Error> {
    let keys: Vec<&str> = TARGETS.iter().map(|t| t.key).collect();
    if let Some(unknown) = only.iter().find(|name| !keys.contains(&name.as_str())) {
        return Err(Error::Failed(format!(
            "--only {unknown}: no such target; the targets are {}",
            keys.join(", ")
        )));
    }
    Ok(TARGETS
        .iter()
        .filter(|t| only.is_empty() || only.iter().any(|name| name == t.key))
        .collect())
}

/// Whether `targets` hold the one that runs the `js-mac-provision` program.
fn needs_program(targets: &[&Target]) -> bool {
    targets.iter().any(|t| t.package == PROVISION)
}

/// Builds `targets` and, when they need it ([`needs_program`]), the `js-mac-provision`
/// program: each target's binary, in the order of `targets`, and the program.
fn build_all(
    machine: &dyn Machine,
    targets: &[&Target],
) -> Result<(Vec<Built>, Option<PathBuf>), Error> {
    let mut built = Vec::new();
    let mut packages: Vec<&str> = targets.iter().map(|t| t.package).collect();
    packages.dedup();
    for package in packages {
        let tests: Vec<&str> = targets
            .iter()
            .filter(|t| t.package == package)
            .map(|t| t.test)
            .collect();
        let messages = machine.build(package, &tests)?;
        for test in tests {
            built.push(test_build::find(&messages, test)?);
        }
    }
    let program = match needs_program(targets) {
        true => Some(test_build::find_bin(
            &machine.build_bin(PROVISION, PROVISION)?,
            PROVISION,
        )?),
        false => None,
    };
    Ok((built, program))
}

/// Makes the work directory, its `bin` and the directories of `targets` as root, and copies
/// their binaries (and the program, when there is one) into `bin` with mode 0755.
fn lay_out(
    machine: &dyn Machine,
    work: &Path,
    targets: &[&Target],
    built: &[Built],
    program: Option<&Path>,
) -> Result<(), Error> {
    let bin = work.join("bin");
    let mut dirs = vec![work.to_path_buf(), bin.clone()];
    for target in targets {
        dirs.extend(dirs_for(target.key, work));
    }
    for dir in dirs {
        as_root(
            machine,
            vec!["/bin/mkdir".into(), "-m".into(), "0755".into(), dir.into()],
        )?;
    }
    let mut copies: Vec<(&Path, PathBuf)> = targets
        .iter()
        .zip(built)
        .map(|(target, built)| (built.executable.as_path(), bin.join(target.key)))
        .collect();
    copies.extend(program.map(|program| (program, bin.join(PROVISION))));
    for (from, to) in &copies {
        as_root(machine, vec!["/bin/cp".into(), from.into(), to.into()])?;
    }
    let mut chmod: Vec<OsString> = vec!["/bin/chmod".into(), "0755".into()];
    chmod.extend(copies.into_iter().map(|(_, to)| to.into_os_string()));
    as_root(machine, chmod)
}

/// Deletes the users the `js-agent` target `key` left ([`to_delete`]) and lists the users
/// again: a failure row for each that is still there, or for a list that cannot be read.
fn clean_up(machine: &dyn Machine, key: &str) -> Vec<Row> {
    let before = users(machine).map(|u| to_delete(&u));
    for name in before.iter().flatten() {
        let _ = machine.root(&[
            DSCL.into(),
            ".".into(),
            "-delete".into(),
            format!("/Users/{name}").into(),
        ]);
    }
    match users(machine) {
        Ok(after) => to_delete(&after)
            .into_iter()
            .map(|name| {
                let why = format!("the user is left after {key}");
                Row::new("cleanup", &name, Outcome::Fail, &why)
            })
            .collect(),
        Err(e) => vec![Row::new(
            "cleanup",
            "(listing)",
            Outcome::Fail,
            &format!("after {key}: {e}"),
        )],
    }
}

/// Runs one target's binary as root: its rows, and the cleanup's after a `js-agent` one.
fn run_target(p: &MacRootTests<'_>, target: &Target, built: &Built, logs: &Path) -> Vec<Row> {
    let log = logs.join(format!("{}.log", target.key));
    let args = run_args(target.key, p.work, p.owner_uid);
    let mut rows = match p.machine.root_logged(&args, &built.dir, &log) {
        Ok((status, text)) => judge(
            target.key,
            &text,
            &status.to_string(),
            status.success(),
            &log,
        ),
        Err(e) => vec![Row::new(
            target.key,
            "(binary)",
            Outcome::Fail,
            &e.to_string(),
        )],
    };
    for (test, why) in skipped_for(target.key) {
        rows.push(Row::new(target.key, test, Outcome::NotRun, why));
    }
    if target.makes_users {
        rows.extend(clean_up(p.machine, target.key));
    }
    rows
}

/// Checks the Mac, builds the selected targets ([`MacRootTests::only`]), lays the work
/// directory out as root and runs each target there as root: the line to print when no test
/// failed. The table is printed and
/// written to `target/mac-root-tests/table.tsv` under the repository root.
///
/// # Errors
/// A name of `only` is no target's, a step before the runs failed (the first one's error),
/// or one or more rows are `FAIL` (one line each, after every selected target ran).
pub fn run(p: &MacRootTests<'_>) -> Result<String, Error> {
    let targets = select(p.only)?;
    if p.os != "macos" {
        return Err(Error::Failed(format!(
            "mac-root-tests runs on a Mac; this is {}",
            p.os
        )));
    }
    check_work(p.work)?;
    check_users(&users(p.machine)?)?;
    let version = version(p.machine)?;
    let (built, program) = build_all(p.machine, &targets)?;
    let logs = p.root.join("target/mac-root-tests");
    std::fs::create_dir_all(&logs)
        .map_err(|e| Error::Failed(format!("{}: {e}", logs.display())))?;
    lay_out(p.machine, p.work, &targets, &built, program.as_deref())?;
    let mut rows = Vec::new();
    for (target, built) in targets.iter().zip(&built) {
        rows.extend(run_target(p, target, built, &logs));
    }
    println!("{}", render(&version, &rows));
    let table = logs.join("table.tsv");
    std::fs::write(&table, tsv(&version, &rows))
        .map_err(|e| Error::Failed(format!("{}: {e}", table.display())))?;
    let failed: Vec<String> = rows
        .iter()
        .filter(|row| row.outcome == Outcome::Fail)
        .map(|row| format!("{} {}: {}", row.target, row.test, row.reason))
        .collect();
    if failed.is_empty() {
        let passed = rows.iter().filter(|r| r.outcome == Outcome::Pass).count();
        return Ok(format!(
            "ok: {passed} Mac root tests passed on macOS {version}; the table is {}",
            table.display()
        ));
    }
    Err(Error::Refused(failed))
}

#[cfg(test)]
mod tests;
