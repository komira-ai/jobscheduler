//! What the container's program does. `JS_EGRESS_PROBE` is `<command> [args]`; every
//! command prints `PROBE key=value` lines (one line per key, values on one line).
//!
//! - `id`: the job's ids, capabilities and `no_new_privs`.
//! - `userns-netns`: what the build command does in `kbf-task`
//!   (`crates/kbf-task/src/network.rs` there): a child that calls
//!   `unshare(CLONE_NEWUSER|CLONE_NEWNET)`, writes `setgroups`, `uid_map` and `gid_map`,
//!   raises `lo`, then runs the `after-netns` command in the new namespace. The parent
//!   keeps the container's network. A failed step is reported with its number and errno.
//! - `after-netns`: what the build command sees: interfaces, loopback, a connect outward.
//! - `suite`: the container's network as seen from inside: interfaces, routes, IPv6, a
//!   connect to each of `JS_EGRESS_TARGETS`, then attempts to change the firewall and the
//!   links, then the connects again; then waits for `JS_EGRESS_GATE` to exist.
//! - `hold`: does nothing until its standard input closes (keeps namespaces alive).

use std::ffi::CString;
use std::io::Read as _;
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::os::unix::process::CommandExt as _;
use std::process::Command;
use std::time::{Duration, Instant};

/// Prints one result line and flushes it.
fn emit(key: &str, value: impl std::fmt::Display) {
    let value = value.to_string().replace('\n', "\\n");
    println!("PROBE {key}={value}");
}

/// Runs the command named by `JS_EGRESS_PROBE`.
pub fn entry() {
    let Ok(line) = std::env::var("JS_EGRESS_PROBE") else {
        return;
    };
    let mut words = line.split_whitespace();
    match words.next().unwrap_or("") {
        "id" => id(),
        "userns-netns" => userns_netns(),
        "after-netns" => after_netns(),
        "suite" => {
            suite_body();
            wait_gate();
        }
        "composite" => {
            suite_body();
            userns_netns();
            wait_gate();
        }
        "v6" => v6(),
        "hold" => {
            let mut sink = Vec::new();
            let _ = std::io::stdin().read_to_end(&mut sink);
        }
        other => emit("error", format!("unknown command {other:?}")),
    }
}

/// One `Key:` line of `/proc/self/status`.
fn status_field(key: &str) -> String {
    std::fs::read_to_string("/proc/self/status")
        .unwrap_or_default()
        .lines()
        .find_map(|line| line.strip_prefix(key)?.strip_prefix(':'))
        .map_or_else(|| "n/a".to_owned(), |value| value.trim().to_owned())
}

fn id() {
    emit("uid", status_field("Uid"));
    emit("gid", status_field("Gid"));
    emit("cap_eff", status_field("CapEff"));
    emit("cap_bnd", status_field("CapBnd"));
    emit("no_new_privs", status_field("NoNewPrivs"));
    emit(
        "uid_map",
        std::fs::read_to_string("/proc/self/uid_map")
            .unwrap_or_default()
            .split_whitespace()
            .collect::<Vec<_>>()
            .join(" "),
    );
}

/// The step numbers of the child's setup, as `kbf-task` orders them.
const STEPS: [&str; 5] = ["unshare", "setgroups", "uid_map", "gid_map", "loopback"];

/// A raw error code the child reports through `spawn`: `step * 10000 + errno`.
const STEP_SCALE: i32 = 10_000;

/// The errno of the last failed call.
fn errno() -> i32 {
    std::io::Error::last_os_error()
        .raw_os_error()
        .unwrap_or(libc::EIO)
}

/// Writes `data` to `path` with one `write`, as the kernel's map files need.
fn write_file(path: &CString, data: &[u8]) -> Result<(), i32> {
    // SAFETY: `path` is NUL-terminated and `data` is readable for its length.
    unsafe {
        let fd = libc::open(path.as_ptr(), libc::O_WRONLY | libc::O_CLOEXEC);
        if fd < 0 {
            return Err(errno());
        }
        let done = libc::write(fd, data.as_ptr().cast(), data.len());
        let code = errno();
        libc::close(fd);
        if done < 0 { Err(code) } else { Ok(()) }
    }
}

/// Sets `IFF_UP` on `lo` in the current network namespace.
fn raise_loopback() -> Result<(), i32> {
    // SAFETY: integer arguments only; the zeroed `ifreq` is valid and lives through the calls.
    unsafe {
        let fd = libc::socket(libc::AF_INET, libc::SOCK_DGRAM | libc::SOCK_CLOEXEC, 0);
        if fd < 0 {
            return Err(errno());
        }
        let mut request: libc::ifreq = std::mem::zeroed();
        request.ifr_name[0] = b'l' as libc::c_char;
        request.ifr_name[1] = b'o' as libc::c_char;
        let mut result = Ok(());
        if libc::ioctl(fd, libc::SIOCGIFFLAGS as _, &raw mut request) < 0 {
            result = Err(errno());
        } else {
            request.ifr_ifru.ifru_flags |= (libc::IFF_UP | libc::IFF_RUNNING) as libc::c_short;
            if libc::ioctl(fd, libc::SIOCSIFFLAGS as _, &raw mut request) < 0 {
                result = Err(errno());
            }
        }
        libc::close(fd);
        result
    }
}

fn userns_netns() {
    // SAFETY: plain id queries.
    let (uid, gid) = unsafe { (libc::getuid(), libc::getgid()) };
    let uid_map = CString::new(format!("{uid} {uid} 1")).expect("no NUL");
    let gid_map = CString::new(format!("{gid} {gid} 1")).expect("no NUL");
    let setgroups = CString::new("/proc/self/setgroups").expect("no NUL");
    let uid_path = CString::new("/proc/self/uid_map").expect("no NUL");
    let gid_path = CString::new("/proc/self/gid_map").expect("no NUL");
    let mut command = Command::new("/proc/self/exe");
    command
        .args([
            "--exact",
            "probe_entry",
            "--ignored",
            "--nocapture",
            "--test-threads=1",
        ])
        .env("JS_EGRESS_PROBE", "after-netns");
    // SAFETY: the closure makes system calls only, on data made before the fork.
    unsafe {
        command.pre_exec(move || {
            let fail = |step: i32, code: i32| {
                Err(std::io::Error::from_raw_os_error(step * STEP_SCALE + code))
            };
            if libc::unshare(libc::CLONE_NEWUSER | libc::CLONE_NEWNET) != 0 {
                return fail(1, errno());
            }
            if let Err(code) = write_file(&setgroups, b"deny") {
                return fail(2, code);
            }
            if let Err(code) = write_file(&uid_path, uid_map.as_bytes()) {
                return fail(3, code);
            }
            if let Err(code) = write_file(&gid_path, gid_map.as_bytes()) {
                return fail(4, code);
            }
            if let Err(code) = raise_loopback() {
                return fail(5, code);
            }
            Ok(())
        });
    }
    match command.status() {
        Ok(status) => {
            emit("steps", "all ok");
            emit("inner_exit", status.code().unwrap_or(-1));
        }
        Err(error) => {
            let raw = error.raw_os_error().unwrap_or(0);
            let (step, code) = (raw / STEP_SCALE, raw % STEP_SCALE);
            let name = usize::try_from(step - 1)
                .ok()
                .and_then(|index| STEPS.get(index))
                .copied()
                .unwrap_or("spawn");
            emit(
                "steps",
                format!(
                    "failed at {name}: {}",
                    std::io::Error::from_raw_os_error(code)
                ),
            );
        }
    }
}

/// The interface names in `/proc/net/dev`.
fn interfaces() -> Vec<String> {
    std::fs::read_to_string("/proc/net/dev")
        .unwrap_or_default()
        .lines()
        .filter_map(|line| Some(line.split_once(':')?.0.trim().to_owned()))
        .collect()
}

/// `ok` when a connect to `target` reaches a listener that greets, else the error.
fn connect(target: &str) -> String {
    let Ok(addr) = target.parse::<SocketAddr>() else {
        return "bad address".to_owned();
    };
    match TcpStream::connect_timeout(&addr, Duration::from_secs(4)) {
        Ok(mut stream) => {
            let _ = stream.set_read_timeout(Some(Duration::from_secs(2)));
            let mut greeting = [0_u8; 3];
            match stream.read_exact(&mut greeting) {
                Ok(()) => "ok".to_owned(),
                Err(error) => format!("connected, no greeting: {:?}", error.kind()),
            }
        }
        Err(error) => format!(
            "err {:?} errno {}",
            error.kind(),
            error.raw_os_error().unwrap_or(0)
        ),
    }
}

fn after_netns() {
    emit("inner.uid", status_field("Uid"));
    emit("inner.ifaces", interfaces().join(","));
    let loopback = TcpListener::bind("127.0.0.1:0")
        .and_then(|listener| {
            let addr = listener.local_addr()?;
            let stream = TcpStream::connect_timeout(&addr, Duration::from_secs(2))?;
            drop(stream);
            Ok(())
        })
        .map_or_else(|error| format!("err {error}"), |()| "ok".to_owned());
    emit("inner.loopback", loopback);
    for target in std::env::var("JS_EGRESS_TARGETS")
        .unwrap_or_default()
        .split(',')
        .filter(|t| !t.is_empty())
    {
        emit(&format!("inner.connect.{target}"), connect(target));
    }
}

/// Whether the default route exists in `/proc/net/route`.
fn has_default_route() -> bool {
    std::fs::read_to_string("/proc/net/route")
        .unwrap_or_default()
        .lines()
        .skip(1)
        .any(|line| line.split_whitespace().nth(1) == Some("00000000"))
}

fn net_state() {
    emit("net.ifaces", interfaces().join(","));
    emit(
        "net.default_route",
        if has_default_route() { "yes" } else { "no" },
    );
    emit(
        "net.ipv6.disable_all",
        std::fs::read_to_string("/proc/sys/net/ipv6/conf/all/disable_ipv6")
            .map_or_else(|_| "absent".to_owned(), |v| v.trim().to_owned()),
    );
    let non_loopback_v6 = std::fs::read_to_string("/proc/net/if_inet6")
        .unwrap_or_default()
        .lines()
        .filter(|line| line.split_whitespace().last() != Some("lo"))
        .count();
    emit("net.ipv6.addrs", non_loopback_v6);
    emit("net.ipv6.connect", connect("[2001:db8::1]:9300"));
}

/// Runs `program args` and reports `exit:<code>` and the first error line.
fn attempt(label: &str, program: &str, args: &[&str]) -> bool {
    match Command::new(program).args(args).output() {
        Ok(output) => {
            let first = String::from_utf8_lossy(&output.stderr)
                .lines()
                .next()
                .unwrap_or("")
                .to_owned();
            emit(
                label,
                format!("exit {} {first}", output.status.code().unwrap_or(-1)),
            );
            output.status.success()
        }
        Err(error) => {
            emit(label, format!("cannot run: {error}"));
            false
        }
    }
}

fn tamper() {
    let nft = ["/usr/sbin/nft", "/sbin/nft"]
        .into_iter()
        .find(|p| std::path::Path::new(p).exists());
    let ip = ["/usr/sbin/ip", "/usr/bin/ip", "/sbin/ip"]
        .into_iter()
        .find(|p| std::path::Path::new(p).exists());
    let mut succeeded = 0;
    if let Some(nft) = nft {
        for (label, args) in [
            ("tamper.nft_flush", vec!["flush", "ruleset"]),
            (
                "tamper.nft_delete_table",
                vec!["delete", "table", "inet", "js_egress"],
            ),
            (
                "tamper.nft_add_accept",
                vec!["add", "rule", "inet", "js_egress", "output", "accept"],
            ),
        ] {
            succeeded += usize::from(attempt(label, nft, &args));
        }
    } else {
        emit("tamper.nft", "missing");
    }
    if let Some(ip) = ip {
        succeeded += usize::from(attempt(
            "tamper.ip_lo_down",
            ip,
            &["link", "set", "lo", "down"],
        ));
        succeeded += usize::from(attempt(
            "tamper.ip_route_flush",
            ip,
            &["route", "flush", "all"],
        ));
    } else {
        emit("tamper.ip", "missing");
    }
    emit("tamper.succeeded", succeeded);
}

fn suite_body() {
    let targets = std::env::var("JS_EGRESS_TARGETS").unwrap_or_default();
    let targets: Vec<&str> = targets.split(',').filter(|t| !t.is_empty()).collect();
    id();
    net_state();
    for target in &targets {
        emit(&format!("before.{target}"), connect(target));
    }
    tamper();
    for target in &targets {
        emit(&format!("after.{target}"), connect(target));
    }
    emit("suite.done", 1);
}

fn wait_gate() {
    if let Ok(gate) = std::env::var("JS_EGRESS_GATE") {
        let deadline = Instant::now() + Duration::from_secs(90);
        while !std::path::Path::new(&gate).exists() && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(100));
        }
    }
}

/// Whether the namespace has a usable IPv6 loopback: binds `[::1]` and connects to it.
fn v6() {
    let result = TcpListener::bind("[::1]:0")
        .and_then(|listener| {
            let addr = listener.local_addr()?;
            TcpStream::connect_timeout(&addr, Duration::from_secs(2)).map(drop)
        })
        .map_or_else(
            |error| {
                format!(
                    "err {:?} errno {}",
                    error.kind(),
                    error.raw_os_error().unwrap_or(0)
                )
            },
            |()| "ok".to_owned(),
        );
    emit("v6.loopback", result);
    emit(
        "v6.disable_all",
        std::fs::read_to_string("/proc/sys/net/ipv6/conf/all/disable_ipv6")
            .map_or_else(|_| "absent".to_owned(), |v| v.trim().to_owned()),
    );
}
