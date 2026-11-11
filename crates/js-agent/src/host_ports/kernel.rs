//! The kernel's side of the host ports: [`Kernel`], the [`Listen`] an agent runs. It opens
//! the network namespace of a container's first process (`/proc/<pid>/ns/net`), refuses the
//! agent's own, and on a thread of its own enters it (`setns`), binds each listener and
//! ends: the agent's other threads never leave the host's namespace, and a socket keeps the
//! namespace it was made in.
//!
//! Each listener is bound with `IP_FREEBIND`: `crun` brings the container's loopback up
//! while it makes the container, and the agent may bind before it has, when `127.0.0.1` is
//! not yet an address of the namespace. A freely bound listener answers once it is.
//!
//! Every call is a straight line, so a test that calls it where it can only fail (entering
//! a namespace needs `CAP_SYS_ADMIN`) still runs every line of it.

use std::fs::{self, File};
use std::io;
use std::net::{Ipv4Addr, SocketAddrV4, TcpListener};
use std::os::fd::AsFd;
use std::os::unix::fs::MetadataExt;

use rustix::net::{AddressFamily, SocketFlags, SocketType};
use rustix::thread::{LinkNameSpaceType, move_into_link_name_space};

use super::Listen;

/// How many connections a listener holds that the forwarder has not accepted yet.
const BACKLOG: i32 = 128;

/// The [`Listen`] of a node: the listeners are made in the job's network namespace by a
/// thread that enters it. The agent must be root.
#[derive(Debug, Default, Clone, Copy)]
pub struct Kernel;

impl Listen for Kernel {
    fn listen(&self, pid: u32, ports: &[u16]) -> io::Result<Vec<TcpListener>> {
        job_namespace(pid).and_then(|namespace| listen_in(namespace, ports.to_vec()))
    }
}

/// The network namespace of the process `pid`, opened.
///
/// # Errors
///
/// The process or its namespace cannot be read, or the namespace is the agent's own (a
/// listener there would be the host's, not the job's).
pub(super) fn job_namespace(pid: u32) -> io::Result<File> {
    let namespace = File::open(format!("/proc/{pid}/ns/net"))?;
    let (theirs, own) = (namespace.metadata()?, fs::metadata("/proc/self/ns/net")?);
    let own_namespace = (theirs.dev(), theirs.ino()) == (own.dev(), own.ino());
    let shared = || {
        io::Error::other(format!(
            "process {pid} shares the agent's network namespace"
        ))
    };
    (!own_namespace).then_some(namespace).ok_or_else(shared)
}

/// A listener at each of `ports` on `127.0.0.1` in `namespace`, made by a thread that enters
/// it and ends.
///
/// # Errors
///
/// The thread could not be made, the namespace could not be entered, or a port could not be
/// bound; the listeners already bound are closed.
pub(super) fn listen_in(namespace: File, ports: Vec<u16>) -> io::Result<Vec<TcpListener>> {
    let network = Some(LinkNameSpaceType::Network);
    let mut listened = Ok(Vec::new());
    // The scope joins the thread before it ends, and a panic of the thread is the caller's.
    std::thread::scope(|scope| {
        let thread = std::thread::Builder::new().name("js-ports-ns".to_owned());
        let entered = thread.spawn_scoped(scope, || {
            let moved = move_into_link_name_space(namespace.as_fd(), network).map_err(Into::into);
            listened = moved.and_then(|()| ports.into_iter().map(listen_on).collect());
        });
        entered.map(drop)
    })?;
    listened
}

/// A listener on `127.0.0.1` at `port`, in the calling thread's network namespace, bound
/// whether or not that address is up yet (`IP_FREEBIND`).
///
/// # Errors
///
/// The socket could not be made, or bound (the port is taken), or set listening.
pub(super) fn listen_on(port: u16) -> io::Result<TcpListener> {
    let address = SocketAddrV4::new(Ipv4Addr::LOCALHOST, port);
    let socket = rustix::net::socket_with(
        AddressFamily::INET,
        SocketType::STREAM,
        SocketFlags::CLOEXEC,
        None,
    )?;
    rustix::net::sockopt::set_ip_freebind(&socket, true)
        .and_then(|()| rustix::net::bind(&socket, &address))
        .and_then(|()| rustix::net::listen(&socket, BACKLOG))
        .map(|()| TcpListener::from(socket))
        .map_err(io::Error::from)
}
