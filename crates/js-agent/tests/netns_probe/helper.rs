//! Probe helper, compiled statically by the live_podman test and run inside a job
//! container. Prints `NETNS-PROBE <key>=<value>` lines and never fails on an errno.
use std::io::{Error, Read};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::process::Command;
use std::time::Duration;

extern "C" {
    fn unshare(flags: i32) -> i32;
    fn socket(domain: i32, ty: i32, proto: i32) -> i32;
    fn ioctl(fd: i32, req: u64, ...) -> i32;
}

const NEWUSER: i32 = 0x1000_0000;
const NEWNET: i32 = 0x4000_0000;

fn errno_name(n: i32) -> String {
    let name = match n {
        1 => "EPERM",
        2 => "ENOENT",
        12 => "ENOMEM",
        13 => "EACCES",
        22 => "EINVAL",
        24 => "EMFILE",
        28 => "ENOSPC",
        38 => "ENOSYS",
        95 => "EOPNOTSUPP",
        _ => "E?",
    };
    format!("{name}({n})")
}

fn last() -> String {
    errno_name(Error::last_os_error().raw_os_error().unwrap_or(0))
}

fn line(tag: &str, key: &str, value: &str) {
    println!("NETNS-PROBE {tag} {key}={value}");
}

fn read(path: &str) -> String {
    let mut text = String::new();
    match std::fs::File::open(path).and_then(|mut f| f.read_to_string(&mut text)) {
        Ok(_) => text.trim().replace('\n', ";"),
        Err(why) => format!(
            "unreadable:{}",
            why.raw_os_error().map_or("?".into(), errno_name)
        ),
    }
}

fn status_field(name: &str) -> String {
    let status = read("/proc/self/status");
    status
        .split(';')
        .find_map(|l| l.strip_prefix(name)?.strip_prefix(':'))
        .map_or("absent".to_owned(), |v| v.trim().replace('\t', " "))
}

fn lo_up() -> String {
    // struct ifreq: 16 bytes of name, then the 24-byte union; ifr_flags is its first short.
    let mut req = [0u8; 40];
    req[..2].copy_from_slice(b"lo");
    let fd = unsafe { socket(2, 2, 0) };
    if fd < 0 {
        return format!("socket:{}", last());
    }
    if unsafe { ioctl(fd, 0x8913, req.as_mut_ptr()) } < 0 {
        return format!("SIOCGIFFLAGS:{}", last());
    }
    let flags = u16::from_ne_bytes([req[16], req[17]]) | 0x1 | 0x40; // IFF_UP | IFF_RUNNING
    req[16..18].copy_from_slice(&flags.to_ne_bytes());
    if unsafe { ioctl(fd, 0x8914, req.as_mut_ptr()) } < 0 {
        return format!("SIOCSIFFLAGS:{}", last());
    }
    "ok".to_owned()
}

fn connect(addr: SocketAddr) -> String {
    match TcpStream::connect_timeout(&addr, Duration::from_secs(2)) {
        Ok(_) => "connected".to_owned(),
        Err(why) => why
            .raw_os_error()
            .map_or(format!("{:?}", why.kind()), errno_name),
    }
}

/// Runs in the child after the unshare of `flags`.
fn after_unshare(tag: &str, flags: i32) {
    let rc = unsafe { unshare(flags) };
    let outcome = if rc == 0 { "ok".to_owned() } else { last() };
    let label = match flags {
        NEWNET => "unshare_net",
        NEWUSER => "unshare_user",
        _ => "unshare_user_net",
    };
    line(tag, label, &outcome);
    if rc != 0 {
        return;
    }
    line(tag, &format!("{label}.CapEff"), &status_field("CapEff"));
    line(
        tag,
        &format!("{label}.uid_map"),
        &read("/proc/self/uid_map"),
    );
    if flags & NEWNET != 0 {
        line(
            tag,
            &format!("{label}.proc_net_dev_before_lo"),
            &read("/proc/net/dev"),
        );
        line(tag, &format!("{label}.lo_up"), &lo_up());
        let flags = read("/sys/class/net/lo/flags");
        line(tag, &format!("{label}.lo_flags"), &flags);
        match TcpListener::bind("127.0.0.1:0") {
            Ok(listener) => {
                let port = listener.local_addr().map(|a| a.port()).unwrap_or(0);
                let addr: SocketAddr = format!("127.0.0.1:{port}").parse().unwrap();
                line(tag, &format!("{label}.tcp_127.0.0.1"), &connect(addr));
            }
            Err(why) => line(
                tag,
                &format!("{label}.tcp_127.0.0.1"),
                &format!("bind:{}", why.raw_os_error().map_or("?".into(), errno_name)),
            ),
        }
        let outside: SocketAddr = "192.0.2.1:80".parse().unwrap();
        line(tag, &format!("{label}.tcp_outside"), &connect(outside));
    }
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let tag = args.get(1).cloned().unwrap_or_else(|| "?".to_owned());
    match args.get(2).map(String::as_str) {
        Some("net") => return after_unshare(&tag, NEWNET),
        Some("user") => return after_unshare(&tag, NEWUSER),
        Some("usernet") => return after_unshare(&tag, NEWUSER | NEWNET),
        _ => {}
    }
    for key in [
        "Uid",
        "Seccomp",
        "Seccomp_filters",
        "NoNewPrivs",
        "CapEff",
        "CapBnd",
    ] {
        line(&tag, key, &status_field(key));
    }
    line(&tag, "apparmor_profile", &read("/proc/self/attr/current"));
    line(&tag, "uid_map_container", &read("/proc/self/uid_map"));
    for path in [
        "/proc/sys/kernel/apparmor_restrict_unprivileged_userns",
        "/proc/sys/user/max_user_namespaces",
        "/proc/sys/kernel/unprivileged_userns_clone",
    ] {
        line(&tag, &format!("container_view:{path}"), &read(path));
    }
    // Each unshare in a child process of its own: a failed or successful one leaves the
    // others untouched.
    let me = std::env::current_exe().unwrap();
    for mode in ["net", "user", "usernet"] {
        let out = Command::new(&me).arg(&tag).arg(mode).output();
        match out {
            Ok(out) => print!("{}", String::from_utf8_lossy(&out.stdout)),
            Err(why) => line(&tag, mode, &format!("spawn:{why}")),
        }
    }
}
