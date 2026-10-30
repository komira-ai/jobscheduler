//! The host side of the spike: tools, `sudo`, sysctls, test addresses, listeners and an
//! AppArmor profile, each undone when its guard drops.

use std::io::{Read as _, Write as _};
use std::net::{SocketAddr, TcpListener};
use std::path::PathBuf;
use std::process::{Command, Output, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

static SERIAL: Mutex<()> = Mutex::new(());

/// Held by a test for its whole run: the tests change host-wide state (sysctls, addresses,
/// AppArmor profiles), so they run one at a time even without `--test-threads=1`.
pub fn serial() -> MutexGuard<'static, ()> {
    SERIAL
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// Whether a missing tool fails a test (the workflow sets this) instead of skipping it.
pub fn strict() -> bool {
    std::env::var_os("JS_EGRESS_SPIKE_STRICT").is_some()
}

/// The absolute path of `name` on `PATH` or in the sbin directories, if there is one.
pub fn find(name: &str) -> Option<PathBuf> {
    let mut dirs: Vec<PathBuf> = std::env::var_os("PATH")
        .map(|path| std::env::split_paths(&path).collect())
        .unwrap_or_default();
    for extra in ["/usr/sbin", "/sbin", "/usr/local/sbin", "/usr/bin"] {
        dirs.push(extra.into());
    }
    dirs.into_iter()
        .map(|dir| dir.join(name))
        .find(|path| path.is_file())
}

/// The absolute path of `name`, as a string; panics when it is missing (`require` first).
pub fn bin(name: &str) -> String {
    find(name)
        .unwrap_or_else(|| panic!("{name} is not installed"))
        .to_string_lossy()
        .into_owned()
}

/// Whether every tool is installed. A missing tool fails the test in strict mode and
/// skips it, with the reason printed, otherwise.
pub fn require(tools: &[&str]) -> bool {
    let missing: Vec<&str> = tools
        .iter()
        .copied()
        .filter(|name| find(name).is_none())
        .collect();
    if missing.is_empty() {
        return true;
    }
    assert!(!strict(), "missing tools: {missing:?}");
    eprintln!("SKIP: missing tools {missing:?}");
    false
}

/// Runs `cmd` for at most `limit` and returns what it printed. A command still running at
/// the limit is killed and its status is the kill's.
pub fn run_timeout(mut cmd: Command, limit: Duration) -> Output {
    cmd.stdout(Stdio::piped()).stderr(Stdio::piped());
    let mut child = cmd
        .spawn()
        .unwrap_or_else(|error| panic!("cannot start {cmd:?}: {error}"));
    let mut out = child.stdout.take().expect("piped");
    let mut err = child.stderr.take().expect("piped");
    let read_out = std::thread::spawn(move || {
        let mut bytes = Vec::new();
        let _ = out.read_to_end(&mut bytes);
        bytes
    });
    let read_err = std::thread::spawn(move || {
        let mut bytes = Vec::new();
        let _ = err.read_to_end(&mut bytes);
        bytes
    });
    let deadline = Instant::now() + limit;
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) if Instant::now() < deadline => std::thread::sleep(Duration::from_millis(20)),
            _ => {
                let _ = child.kill();
                break child.wait().expect("wait after kill");
            }
        }
    };
    Output {
        status,
        stdout: read_out.join().unwrap_or_default(),
        stderr: read_err.join().unwrap_or_default(),
    }
}

/// `stdout` then `stderr` of `output`, lossy.
pub fn text(output: &Output) -> String {
    format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    )
}

/// `program args...` with `stdin` written to it, as the current user.
pub fn run_with_stdin(program: &str, args: &[&str], stdin: &str) -> Output {
    let mut child = Command::new(program)
        .args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap_or_else(|error| panic!("cannot start {program}: {error}"));
    let mut pipe = child.stdin.take().expect("piped");
    let bytes = stdin.as_bytes().to_vec();
    let writer = std::thread::spawn(move || {
        let _ = pipe.write_all(&bytes);
    });
    let output = child.wait_with_output().expect("wait");
    let _ = writer.join();
    output
}

/// `sudo -n program args...`: whatever it did, without failing the test.
pub fn sudo(program: &str, args: &[&str]) -> Output {
    let mut cmd = Command::new("sudo");
    cmd.arg("-n").arg(bin(program)).args(args);
    run_timeout(cmd, Duration::from_secs(60))
}

/// `sudo -n program args...`, which must succeed; returns its output.
pub fn sudo_must(program: &str, args: &[&str]) -> String {
    let output = sudo(program, args);
    assert!(
        output.status.success(),
        "sudo {program} {args:?} failed: {}",
        text(&output)
    );
    text(&output)
}

/// A sysctl set for a test and put back when dropped.
pub struct Sysctl {
    key: String,
    old: String,
}

impl Sysctl {
    /// Sets `key` (dotted) to `value`, remembering the old value.
    pub fn set(key: &str, value: &str) -> Self {
        let path = format!("/proc/sys/{}", key.replace('.', "/"));
        let old = std::fs::read_to_string(&path)
            .unwrap_or_else(|error| panic!("cannot read sysctl {key} ({path}): {error}"))
            .trim()
            .to_owned();
        sudo_must("sysctl", &["-w", &format!("{key}={value}")]);
        Self {
            key: key.to_owned(),
            old,
        }
    }
}

impl Drop for Sysctl {
    fn drop(&mut self) {
        let _ = sudo("sysctl", &["-w", &format!("{}={}", self.key, self.old)]);
    }
}

/// The current value of a sysctl, or `absent`.
pub fn sysctl_value(key: &str) -> String {
    std::fs::read_to_string(format!("/proc/sys/{}", key.replace('.', "/")))
        .map_or_else(|_| "absent".to_owned(), |value| value.trim().to_owned())
}

/// Addresses added to the host's `lo`, so the endpoints a job may or may not reach exist
/// on a runner with no other network to use; removed when dropped.
pub struct HostAddrs(Vec<String>);

impl HostAddrs {
    /// Adds each `a.b.c.d` as a /32 on `lo`.
    pub fn add(addrs: &[&str]) -> Self {
        for addr in addrs {
            let _ = sudo("ip", &["addr", "del", &format!("{addr}/32"), "dev", "lo"]);
            sudo_must("ip", &["addr", "add", &format!("{addr}/32"), "dev", "lo"]);
        }
        Self(addrs.iter().map(|addr| (*addr).to_owned()).collect())
    }
}

impl Drop for HostAddrs {
    fn drop(&mut self) {
        for addr in &self.0 {
            let _ = sudo("ip", &["addr", "del", &format!("{addr}/32"), "dev", "lo"]);
        }
    }
}

/// TCP listeners that greet and close, one thread each, stopped when dropped.
pub struct Servers {
    stop: Arc<AtomicBool>,
    threads: Vec<JoinHandle<()>>,
}

impl Servers {
    /// Listens on every address.
    pub fn start(addrs: &[&str]) -> Self {
        let stop = Arc::new(AtomicBool::new(false));
        let mut threads = Vec::new();
        for addr in addrs {
            let addr: SocketAddr = addr.parse().expect("socket address");
            let listener = TcpListener::bind(addr)
                .unwrap_or_else(|error| panic!("cannot listen on {addr}: {error}"));
            listener.set_nonblocking(true).expect("nonblocking");
            let stop = Arc::clone(&stop);
            threads.push(std::thread::spawn(move || {
                while !stop.load(Ordering::Relaxed) {
                    match listener.accept() {
                        Ok((mut stream, _)) => {
                            let _ = stream.write_all(b"hi\n");
                        }
                        Err(_) => std::thread::sleep(Duration::from_millis(5)),
                    }
                }
            }));
        }
        Self { stop, threads }
    }
}

impl Drop for Servers {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        for thread in self.threads.drain(..) {
            let _ = thread.join();
        }
    }
}

/// An AppArmor profile loaded for a test and removed when dropped.
pub struct Profile {
    name: String,
    file: PathBuf,
}

impl Profile {
    /// Loads `text` (which defines the profile `name`).
    pub fn load(name: &str, text: &str) -> Self {
        let file =
            std::env::temp_dir().join(format!("js-egress-spike-{}-{name}.aa", std::process::id()));
        std::fs::write(&file, text).expect("profile file");
        sudo_must("apparmor_parser", &["-r", &file.to_string_lossy()]);
        Self {
            name: name.to_owned(),
            file,
        }
    }

    /// The profile's name.
    pub fn name(&self) -> &str {
        &self.name
    }
}

impl Drop for Profile {
    fn drop(&mut self) {
        let _ = sudo("apparmor_parser", &["-R", &self.file.to_string_lossy()]);
        let _ = std::fs::remove_file(&self.file);
    }
}

/// A profile that allows everything a container job does, plus `userns`: the grant for the
/// build command's own user namespace.
pub const JOB_PROFILE: &str = "abi <abi/4.0>,\n\
profile js_spike_job flags=(attach_disconnected,mediate_deleted) {\n\
  file,\n  capability,\n  network,\n  mount,\n  umount,\n  pivot_root,\n  ptrace,\n  signal,\n  unix,\n  userns,\n}\n";

/// The name of [`JOB_PROFILE`].
pub const JOB_PROFILE_NAME: &str = "js_spike_job";

/// The test endpoints, as `ip:port`, and the host addresses and listeners that serve them.
pub mod endpoints {
    /// Allowed: a tierstore front.
    pub const ALLOWED_A: &str = "198.51.100.10:9300";
    /// Allowed: the grant listener.
    pub const ALLOWED_B: &str = "198.51.100.20:9443";
    /// The allowed host, another port.
    pub const OTHER_PORT: &str = "198.51.100.10:9301";
    /// Another host in another network.
    pub const OTHER_HOST: &str = "198.51.100.99:9300";
    /// The cloud metadata address.
    pub const METADATA: &str = "169.254.169.254:80";
    /// Every endpoint above, comma-separated, as the probe takes them.
    pub const ALL: &str = "198.51.100.10:9300,198.51.100.20:9443,198.51.100.10:9301,198.51.100.99:9300,169.254.169.254:80";
    /// The host addresses to add to `lo`.
    pub const ADDRS: [&str; 4] = [
        "198.51.100.10",
        "198.51.100.20",
        "198.51.100.99",
        "169.254.169.254",
    ];
}

/// The capabilities the privileged helper is run with: `CAP_NET_ADMIN` (links, addresses,
/// routes, nft in a namespace), `CAP_SYS_ADMIN` (`setns` into a namespace owned by another
/// user namespace) and `CAP_SYS_PTRACE` (open the namespace files of another user's process).
pub const HELPER_CAPS: &str = "-all,+net_admin,+sys_admin,+sys_ptrace";

/// What `CapEff` reads for [`HELPER_CAPS`]: bits 12, 19 and 21.
pub const HELPER_CAPEFF: &str = "0000000000281000";

/// `program args...` as the privileged helper: root, but with a bounding and inheritable
/// set of exactly [`HELPER_CAPS`] (`setpriv`), so a call that needs more fails.
pub fn helper(program: &str, args: &[&str], stdin: &str) -> Output {
    let setpriv = bin("setpriv");
    let program = bin(program);
    let mut all = vec![
        "-n",
        setpriv.as_str(),
        "--bounding-set",
        HELPER_CAPS,
        "--inh-caps",
        "-all",
        "--",
        program.as_str(),
    ];
    all.extend_from_slice(args);
    run_with_stdin("sudo", &all, stdin)
}

/// [`helper`], which must succeed; returns its output.
pub fn helper_must(program: &str, args: &[&str]) -> String {
    let output = helper(program, args, "");
    assert!(
        output.status.success(),
        "helper {program} {args:?} failed: {}",
        text(&output)
    );
    text(&output)
}
