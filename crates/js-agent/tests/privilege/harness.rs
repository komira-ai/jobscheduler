//! Runs this test binary again as an ordinary user holding exactly a named set of
//! capabilities, and reads back what one probe said.

use std::collections::BTreeMap;
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use js_testkit::dir::TestDir;

/// One line a probe prints: a key and a one-line value.
pub type Kv = (String, String);

/// The uid and gid of the agent of shape A (no account has them).
pub const AGENT: u32 = 2000;
/// The uid and gid of the job's mapped user.
pub const JOB: u32 = 3000;
/// A user that is neither.
pub const STRANGER: u32 = 4000;

/// Capability numbers (`linux/capability.h`).
pub mod cap {
    pub const CHOWN: u32 = 0;
    pub const DAC_OVERRIDE: u32 = 1;
    pub const DAC_READ_SEARCH: u32 = 2;
    pub const FOWNER: u32 = 3;
    pub const NET_ADMIN: u32 = 12;
    pub const SYS_PTRACE: u32 = 19;
    pub const SYS_ADMIN: u32 = 21;
}

/// The variable that tells the `child` test which probe to run.
const PROBE_ENV: &str = "JS_PRIV_PROBE";

/// A key and value for a probe's report.
pub fn kv(key: &str, value: impl ToString) -> Kv {
    (key.to_owned(), value.to_string())
}

/// `ok`, or `err:` and the error's text, for a probe's report.
pub fn verdict<T, E: std::fmt::Display>(result: &Result<T, E>) -> String {
    match result {
        Ok(_) => "ok".to_owned(),
        Err(why) => format!("err:{why}"),
    }
}

/// The path a parent test passed to its child in `JS_PRIV_<name>`.
pub fn arg(name: &str) -> PathBuf {
    let var = format!("JS_PRIV_{name}");
    std::env::var_os(&var)
        .unwrap_or_else(|| panic!("the parent did not set {var}"))
        .into()
}

/// The path a CI step passed in `JS_PRIV_<name>`; a missing variable fails the test.
pub fn host_path(name: &str) -> PathBuf {
    let var = format!("JS_PRIV_{name}");
    let path = std::env::var_os(&var)
        .unwrap_or_else(|| panic!("set {var}: see the module docs of tests/privilege/main.rs"));
    fs::canonicalize(path).unwrap_or_else(|why| panic!("{var}: {why}"))
}

/// Stops the test unless the process is root.
pub fn require_root() {
    // SAFETY: geteuid has no preconditions.
    let euid = unsafe { libc::geteuid() };
    assert_eq!(
        euid, 0,
        "these tests run as root: use sudo (see the module docs)"
    );
}

/// A new directory below `/tmp` that every user can search, owned by root.
pub fn work_dir(name: &str) -> TestDir {
    let dir = TestDir::under("/tmp", name);
    fs::set_permissions(dir.path(), fs::Permissions::from_mode(0o755)).unwrap();
    dir
}

/// Makes `path` a directory with `mode`, owned by `uid`.
pub fn make_dir(path: &Path, mode: u32, uid: u32) {
    fs::create_dir_all(path).unwrap();
    fs::set_permissions(path, fs::Permissions::from_mode(mode)).unwrap();
    std::os::unix::fs::lchown(path, Some(uid), Some(uid)).unwrap();
}

/// What a child printed.
#[derive(Debug)]
pub struct Report {
    values: BTreeMap<String, String>,
    output: String,
}

impl Report {
    /// The value of `key`; a key the probe did not print fails the test with the child's
    /// whole output.
    pub fn get(&self, key: &str) -> &str {
        self.values
            .get(key)
            .unwrap_or_else(|| panic!("the probe did not report {key}; output:\n{}", self.output))
    }

    /// Fails the test unless the value of `key` is `want`.
    #[track_caller]
    pub fn expect(&self, key: &str, want: &str) {
        assert_eq!(self.get(key), want, "{key}; output:\n{}", self.output);
    }

    /// Fails the test unless the value of `key` starts with `prefix`.
    #[track_caller]
    pub fn expect_prefix(&self, key: &str, prefix: &str) {
        assert!(
            self.get(key).starts_with(prefix),
            "{key} should start with {prefix:?}; output:\n{}",
            self.output
        );
    }
}

/// The identity and capabilities of the running process, read from `/proc/self/status`.
fn identity() -> Vec<Kv> {
    let status = fs::read_to_string("/proc/self/status").unwrap();
    let field = |name: &str| {
        status
            .lines()
            .find_map(|line| line.strip_prefix(name))
            .map(|rest| rest.trim().to_owned())
            .unwrap_or_default()
    };
    vec![
        kv(
            "euid",
            field("Uid:").split_whitespace().nth(1).unwrap_or(""),
        ),
        kv("capeff", field("CapEff:")),
        kv("capamb", field("CapAmb:")),
        kv("capbnd", field("CapBnd:")),
    ]
}

/// The probe of the environment, printed for the parent. In the parent's own run (the
/// `--ignored` run of the whole binary) the variable is unset and this does nothing: the
/// parent tests fail when a child did not print `done`.
#[test]
#[ignore = "runs as the child of a privilege test; see the module docs of tests/privilege/main.rs"]
fn child() {
    let Ok(name) = std::env::var(PROBE_ENV) else {
        return;
    };
    let mut lines = identity();
    lines.extend(crate::probe(&name));
    for (key, value) in lines {
        println!("PROBE {key}={}", value.replace('\n', " "));
    }
    println!("PROBE done=1");
}

const PR_SET_KEEPCAPS: libc::c_int = 8;
const PR_CAPBSET_DROP: libc::c_int = 24;
const PR_CAP_AMBIENT: libc::c_int = 47;
const PR_CAP_AMBIENT_RAISE: libc::c_ulong = 2;
const LINUX_CAPABILITY_VERSION_3: u32 = 0x2008_0522;

#[repr(C)]
struct CapHeader {
    version: u32,
    pid: i32,
}

#[repr(C)]
#[derive(Clone, Copy)]
struct CapData {
    effective: u32,
    permitted: u32,
    inheritable: u32,
}

fn check(ret: i64) -> std::io::Result<()> {
    match ret {
        -1 => Err(std::io::Error::last_os_error()),
        _ => Ok(()),
    }
}

/// Becomes `uid` (and gid, with no supplementary groups) holding exactly the capabilities
/// in `mask`, in the permitted, effective, inheritable and ambient sets, with the bounding
/// set cut to them. Called between `fork` and `exec` in the child: system calls only.
fn become_user(uid: u32, mask: u64, last_cap: u32) -> std::io::Result<()> {
    // SAFETY: plain system calls with valid arguments.
    unsafe {
        // The bounding set can be cut only while the process still holds CAP_SETPCAP.
        for cap in 0..=last_cap {
            if (mask >> cap) & 1 == 0 {
                check(libc::prctl(PR_CAPBSET_DROP, libc::c_ulong::from(cap), 0, 0, 0).into())?;
            }
        }
        check(libc::prctl(PR_SET_KEEPCAPS, 1, 0, 0, 0).into())?;
        check(libc::setgroups(0, std::ptr::null()).into())?;
        check(libc::setresgid(uid, uid, uid).into())?;
        check(libc::setresuid(uid, uid, uid).into())?;
        let header = CapHeader {
            version: LINUX_CAPABILITY_VERSION_3,
            pid: 0,
        };
        let low = CapData {
            effective: mask as u32,
            permitted: mask as u32,
            inheritable: mask as u32,
        };
        let high = CapData {
            effective: (mask >> 32) as u32,
            permitted: (mask >> 32) as u32,
            inheritable: (mask >> 32) as u32,
        };
        let data = [low, high];
        check(libc::syscall(
            libc::SYS_capset,
            std::ptr::addr_of!(header),
            data.as_ptr(),
        ))?;
        for cap in 0..=last_cap {
            if (mask >> cap) & 1 == 1 {
                check(
                    libc::prctl(
                        PR_CAP_AMBIENT,
                        PR_CAP_AMBIENT_RAISE,
                        libc::c_ulong::from(cap),
                        0,
                        0,
                    )
                    .into(),
                )?;
            }
        }
    }
    Ok(())
}

/// Runs `probe` in a child that is `uid` holding exactly `caps` (uid 0 is left as it is:
/// root with every capability), with `env` set, and returns what it printed.
///
/// # Panics
///
/// If the child cannot be started, fails, does not print `done`, or reports an identity or
/// capability set other than the one asked for.
pub fn run_as(uid: u32, caps: &[u32], probe: &str, env: &[(&str, &Path)]) -> Report {
    require_root();
    let exe = std::env::current_exe().unwrap();
    // The test binary lives under a home directory that other users cannot search.
    let home = work_dir("priv-exe");
    let copy = home.path().join("child");
    fs::copy(&exe, &copy).unwrap();
    fs::set_permissions(&copy, fs::Permissions::from_mode(0o755)).unwrap();

    let last_cap: u32 = fs::read_to_string("/proc/sys/kernel/cap_last_cap")
        .unwrap()
        .trim()
        .parse()
        .unwrap();
    let mask: u64 = caps.iter().fold(0, |mask, cap| mask | (1u64 << *cap));
    let mut command = Command::new(&copy);
    command
        .args([
            "--exact",
            "harness::child",
            "--ignored",
            "--nocapture",
            "--test-threads=1",
        ])
        .env(PROBE_ENV, probe)
        .current_dir("/")
        .stdin(Stdio::null());
    for (name, value) in env {
        command.env(format!("JS_PRIV_{name}"), value);
    }
    if uid != 0 {
        // SAFETY: the closure makes system calls only (no allocation, no locks).
        unsafe {
            command.pre_exec(move || become_user(uid, mask, last_cap));
        }
    }
    let out = command
        .output()
        .unwrap_or_else(|why| panic!("cannot start the child of {probe}: {why}"));
    let text = String::from_utf8_lossy(&out.stdout).into_owned();
    let full = format!(
        "{text}\n--- stderr ---\n{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let values: BTreeMap<String, String> = text
        .lines()
        // The first line follows libtest's `test harness::child ... `.
        .filter_map(|line| line.find("PROBE ").map(|at| &line[at + "PROBE ".len()..]))
        .filter_map(|rest| rest.split_once('='))
        .map(|(key, value)| (key.to_owned(), value.to_owned()))
        .collect();
    assert!(
        out.status.success() && values.get("done").map(String::as_str) == Some("1"),
        "the child of {probe} did not finish ({:?}):\n{full}",
        out.status
    );
    let report = Report {
        values,
        output: full,
    };
    report.expect("euid", &uid.to_string());
    if uid != 0 {
        let hex = format!("{mask:016x}");
        report.expect("capeff", &hex);
        report.expect("capamb", &hex);
    }
    report
}

/// [`run_as`] the agent of shape A (uid 2000) with `caps`.
pub fn run_agent(caps: &[u32], probe: &str, env: &[(&str, &Path)]) -> Report {
    run_as(AGENT, caps, probe, env)
}
