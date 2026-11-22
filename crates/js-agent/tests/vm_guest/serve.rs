//! The guest program's flags, its virtio socket listener and the dispatch of each request.
//!
//! One thread per connection: it reads one request line, carries it out and writes one
//! reply line. A run whose connection drops runs on; its exit is kept for a `wait`.

use std::collections::BTreeMap;
use std::io::{BufRead as _, BufReader, Read as _, Write as _};
use std::os::fd::{AsRawFd as _, FromRawFd as _, OwnedFd};
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::{Arc, Mutex};
use std::time::Instant;

use js_agent::linux_vm::wire::{self, Reply, Request};
use js_contain::oci::helper::HelperModes;

use crate::{hardened, plain};

/// The flags.
pub struct Flags {
    /// The virtio socket port to listen on.
    pub port: u32,
    /// The `crun` program.
    pub crun: PathBuf,
    /// The image root every plain run starts from (read-only).
    pub rootfs: PathBuf,
    /// Where each plain run's bundle is written.
    pub runs: PathBuf,
    /// The static busybox a hardened run's image is made of.
    pub busybox: Option<PathBuf>,
    /// The user a hardened run's `crun` runs as.
    pub job_user: (u32, u32),
    /// The directory of the hardened runtime.
    pub hardened_work: Option<PathBuf>,
    /// The host's virtio socket port to send the ready line to once listening; 0 for none.
    pub ready_port: u32,
}

fn parse(args: &[String]) -> Result<Flags, String> {
    let mut flags = Flags {
        port: 7000,
        crun: PathBuf::from("/usr/bin/crun"),
        rootfs: PathBuf::new(),
        runs: PathBuf::new(),
        busybox: None,
        job_user: (1000, 1000),
        hardened_work: None,
        ready_port: 0,
    };
    let mut at = 0;
    while at < args.len() {
        let value = args
            .get(at + 1)
            .ok_or_else(|| format!("{} takes a value", args[at]))?;
        match args[at].as_str() {
            "--port" => flags.port = value.parse().map_err(|_| "--port: not a number")?,
            "--ready-port" => {
                flags.ready_port = value.parse().map_err(|_| "--ready-port: not a number")?;
            }
            "--crun" => flags.crun = PathBuf::from(value),
            "--rootfs" => flags.rootfs = PathBuf::from(value),
            "--runs" => flags.runs = PathBuf::from(value),
            "--busybox" => flags.busybox = Some(PathBuf::from(value)),
            "--hardened-work" => flags.hardened_work = Some(PathBuf::from(value)),
            "--job-user" => {
                let (u, g) = value.split_once(':').ok_or("--job-user: not uid:gid")?;
                flags.job_user = (
                    u.parse().map_err(|_| "--job-user: not uid:gid")?,
                    g.parse().map_err(|_| "--job-user: not uid:gid")?,
                );
            }
            other => return Err(format!("unknown flag {other:?}")),
        }
        at += 2;
    }
    if flags.rootfs.as_os_str().is_empty() || flags.runs.as_os_str().is_empty() {
        return Err("--rootfs and --runs are required".to_owned());
    }
    Ok(flags)
}

/// The kept exits of the runs, by id, and the ids still running.
#[derive(Default)]
pub struct Book {
    /// Ended runs.
    pub exits: BTreeMap<String, Reply>,
    /// Runs not ended yet.
    pub running: std::collections::BTreeSet<String>,
}

/// What every connection's thread shares.
pub struct Shared {
    /// The flags.
    pub flags: Flags,
    /// The runs.
    pub book: Mutex<Book>,
    /// One hardened run at a time: the agent's runtime holds the firewall.
    pub hardened: Mutex<()>,
}

/// A listening virtio socket on `port`, any context id.
fn listen(port: u32) -> std::io::Result<OwnedFd> {
    // SAFETY: plain system calls on a descriptor this function owns; `addr` is a zeroed
    // `sockaddr_vm` with its family, port and context id set, passed with its own size.
    unsafe {
        let fd = libc::socket(libc::AF_VSOCK, libc::SOCK_STREAM | libc::SOCK_CLOEXEC, 0);
        if fd < 0 {
            return Err(std::io::Error::last_os_error());
        }
        let owned = OwnedFd::from_raw_fd(fd);
        let mut addr: libc::sockaddr_vm = std::mem::zeroed();
        addr.svm_family = libc::AF_VSOCK as libc::sa_family_t;
        addr.svm_port = port;
        addr.svm_cid = libc::VMADDR_CID_ANY;
        let len = std::mem::size_of::<libc::sockaddr_vm>() as libc::socklen_t;
        if libc::bind(fd, std::ptr::addr_of!(addr).cast(), len) != 0 {
            return Err(std::io::Error::last_os_error());
        }
        if libc::listen(fd, 64) != 0 {
            return Err(std::io::Error::last_os_error());
        }
        Ok(owned)
    }
}

/// A connection to the host's virtio socket `port` (context id 2).
fn connect_host(port: u32) -> std::io::Result<UnixStream> {
    // SAFETY: plain system calls on a descriptor this function owns; `addr` is a zeroed
    // `sockaddr_vm` with its family, port and context id set, passed with its own size.
    unsafe {
        let fd = libc::socket(libc::AF_VSOCK, libc::SOCK_STREAM | libc::SOCK_CLOEXEC, 0);
        if fd < 0 {
            return Err(std::io::Error::last_os_error());
        }
        let owned = OwnedFd::from_raw_fd(fd);
        let mut addr: libc::sockaddr_vm = std::mem::zeroed();
        addr.svm_family = libc::AF_VSOCK as libc::sa_family_t;
        addr.svm_port = port;
        addr.svm_cid = libc::VMADDR_CID_HOST;
        let len = std::mem::size_of::<libc::sockaddr_vm>() as libc::socklen_t;
        if libc::connect(fd, std::ptr::addr_of!(addr).cast(), len) != 0 {
            return Err(std::io::Error::last_os_error());
        }
        Ok(UnixStream::from(owned))
    }
}

/// Tells the VM's owner that this program listens: `ready uptime=<s> ip=<first address>`.
fn say_ready(port: u32) {
    let uptime = std::fs::read_to_string("/proc/uptime").unwrap_or_default();
    let uptime = uptime.split_whitespace().next().unwrap_or("?").to_owned();
    let ip = js_mac_session::disclaim::Program::new("hostname", ["-I"])
        .output()
        .map(|o| {
            String::from_utf8_lossy(&o.stdout)
                .split_whitespace()
                .next()
                .unwrap_or("")
                .to_owned()
        })
        .unwrap_or_default();
    match connect_host(port) {
        Ok(mut stream) => {
            let _ = writeln!(stream, "ready uptime={uptime} ip={ip}");
        }
        Err(why) => eprintln!("js-vm-guest: tell the owner on port {port}: {why}"),
    }
}

/// The next connection on `listener`, as a stream.
fn accept(listener: &OwnedFd) -> std::io::Result<UnixStream> {
    // SAFETY: `accept4` on a listening descriptor this program owns, with no address
    // wanted; the returned descriptor is new and owned by the stream made from it.
    unsafe {
        let fd = libc::accept4(
            listener.as_raw_fd(),
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            libc::SOCK_CLOEXEC,
        );
        if fd < 0 {
            return Err(std::io::Error::last_os_error());
        }
        // A virtio socket is a stream socket: read, write and shutdown behave as on a Unix one.
        Ok(UnixStream::from(OwnedFd::from_raw_fd(fd)))
    }
}

/// The program.
pub fn main() -> ExitCode {
    if let Some(code) = js_contain::modes::dispatch(&HelperModes::default()) {
        return ExitCode::from(code);
    }
    let args: Vec<String> = std::env::args().skip(1).collect();
    let flags = match parse(&args) {
        Ok(flags) => flags,
        Err(why) => {
            eprintln!("js-vm-guest: {why}");
            return ExitCode::from(2);
        }
    };
    let listener = match listen(flags.port) {
        Ok(listener) => listener,
        Err(why) => {
            eprintln!(
                "js-vm-guest: listen on virtio socket port {}: {why}",
                flags.port
            );
            return ExitCode::FAILURE;
        }
    };
    eprintln!(
        "js-vm-guest: listening on virtio socket port {}",
        flags.port
    );
    let shared = Arc::new(Shared {
        flags,
        book: Mutex::new(Book::default()),
        hardened: Mutex::new(()),
    });
    loop {
        match accept(&listener) {
            Ok(stream) => {
                let shared = Arc::clone(&shared);
                std::thread::spawn(move || serve(stream, &shared));
            }
            Err(why) => eprintln!("js-vm-guest: accept: {why}"),
        }
    }
}

/// One connection: one request, one reply.
fn serve(stream: UnixStream, shared: &Shared) {
    let began = Instant::now();
    let mut line = String::new();
    let Ok(reader) = stream.try_clone() else {
        return;
    };
    if BufReader::new(reader.take(1 << 20))
        .read_line(&mut line)
        .is_err()
    {
        return;
    }
    let mut reply = match wire::parse_request(&line) {
        Err(why) => Reply::refused(format!("not a request: {why}")),
        Ok(Request::Ping) => Reply {
            ok: true,
            ..Reply::default()
        },
        Ok(Request::Poweroff) => {
            // Answered first; the shutdown follows on its own thread.
            std::thread::spawn(|| {
                std::thread::sleep(std::time::Duration::from_millis(100));
                let _ = js_mac_session::disclaim::Program::new("systemctl", ["poweroff"]).output();
            });
            Reply {
                ok: true,
                ..Reply::default()
            }
        }
        Ok(Request::Run { id, args, wall_ms }) => plain::run(shared, &id, &args, wall_ms),
        Ok(Request::Kill { id, signal }) => plain::kill(shared, &id, &signal),
        Ok(Request::Wait { id }) => plain::wait(shared, &id),
        Ok(Request::Hardened { id, script, allow }) => hardened::run(shared, &id, &script, &allow),
    };
    reply.guest_us = u64::try_from(began.elapsed().as_micros()).unwrap_or(u64::MAX);
    if let Ok(text) = wire::reply_line(&reply) {
        let mut out = stream;
        let _ = writeln!(out, "{text}");
    }
}
