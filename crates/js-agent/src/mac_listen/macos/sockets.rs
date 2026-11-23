//! Every TCP listener of the Mac through `libproc`: for each process (launchd, pid 1,
//! included, since it holds the sockets of the services it starts on demand), its file
//! descriptors (`PROC_PIDLISTFDS`), and for each socket among them its state, address and
//! port (`PROC_PIDFDSOCKETINFO`), with the process's uids from its short BSD info.
//!
//! The `libc` crate does not carry the socket structs of `<sys/proc_info.h>`, so the part
//! this module reads is written out below, `#[repr(C)]` and field for field, and its offsets
//! are checked when the module is compiled. The union at the end of `socket_info` is larger
//! in the kernel than the one member read here (`tcp_sockinfo`), so the answer's buffer has
//! room beyond the struct, and an answer shorter than the struct is refused.
//!
//! # FFI-BOUNDARY
//!
//! `proc_pidinfo(PROC_PIDLISTFDS)` and `proc_pidfdinfo(PROC_PIDFDSOCKETINFO)` write into
//! buffers this module owns (a `Vec` of `proc_fdinfo`, a zeroed [`Answer`]), sized by the byte
//! counts passed with them, and keep no pointer after the call; nothing is allocated by the C
//! side, so nothing is freed. No pointer leaves this module.

use std::io;
use std::mem::{offset_of, size_of};
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};

use crate::mac_listen::Listener;
use crate::mac_sweep::macos::procs::{all_pids, c_name, short_info};

/// `<sys/proc_info.h>`: the flavor of `proc_pidfdinfo` that answers a `socket_fdinfo`.
const PROC_PIDFDSOCKETINFO: libc::c_int = 3;
/// `socket_info.soi_kind` of a TCP socket.
const SOCKINFO_TCP: libc::c_int = 2;
/// `tcp_sockinfo.tcpsi_state` of a listening socket.
const TSI_S_LISTEN: libc::c_int = 1;
/// `in_sockinfo.insi_vflag` bits.
const INI_IPV4: u8 = 0x1;
const INI_IPV6: u8 = 0x2;

/// `struct in_sockinfo`: the local port, the address family flags and the local address are
/// read; the rest stands as words of its size and alignment (the foreign port, the
/// generation count, the flags and flow, the TTL, the foreign address, and the IPv4 and IPv6
/// extras). An address is a union of `in4in6_addr` and `in6_addr`, both 16 bytes.
#[repr(C)]
struct InSockInfo {
    _fport: libc::c_int,
    insi_lport: libc::c_int,
    _gencnt: u64,
    _flags_flow: [u32; 2],
    insi_vflag: u8,
    _ip_ttl: u8,
    _rfu_1: u32,
    _faddr: [u8; 16],
    insi_laddr: [u8; 16],
    _v4: u8,
    _v6: [u32; 3],
}

/// `struct tcp_sockinfo`: the state is read; the timers, the MSS, the flags and the control
/// block's address are not.
#[repr(C)]
struct TcpSockInfo {
    tcpsi_ini: InSockInfo,
    tcpsi_state: libc::c_int,
    _timer_mss_flags: [u32; 7],
    _tp: u64,
}

/// `struct socket_info`, up to the `pri_tcp` member of its union. Not read: `soi_stat` (a
/// `vinfo_stat`, 136 bytes) with `soi_so` and `soi_pcb`, then the type, protocol, family,
/// the eight shorts, the out-of-band mark and the two `sockbuf_info` (80 bytes).
#[repr(C)]
struct SocketInfo {
    _stat_so_pcb: [u64; 19],
    _type_to_snd: [u32; 20],
    soi_kind: libc::c_int,
    _rfu_1: u32,
    soi_proto: TcpSockInfo,
}

/// `struct socket_fdinfo`, as far as [`SocketInfo`] goes; its `proc_fileinfo` (24 bytes) is
/// not read.
#[repr(C)]
struct SocketFdInfo {
    _pfi: [u64; 3],
    psi: SocketInfo,
}

/// The buffer of one answer: the struct, and room for the larger members of the kernel's
/// union (`un_sockinfo` is the largest, about 530 bytes against `tcp_sockinfo`'s 120).
#[repr(C)]
struct Answer {
    info: SocketFdInfo,
    _rest: [u64; 128],
}

// The offsets of `<sys/proc_info.h>` (the same on every 64-bit Apple target), checked when the
// module is built.
const _: () = assert!(offset_of!(InSockInfo, insi_lport) == 4);
const _: () = assert!(offset_of!(InSockInfo, insi_vflag) == 24);
const _: () = assert!(offset_of!(InSockInfo, insi_laddr) == 48);
const _: () = assert!(size_of::<InSockInfo>() == 80);
const _: () = assert!(offset_of!(TcpSockInfo, tcpsi_state) == 80);
const _: () = assert!(size_of::<TcpSockInfo>() == 120);
const _: () = assert!(offset_of!(SocketInfo, soi_kind) == 232);
const _: () = assert!(offset_of!(SocketInfo, soi_proto) == 240);
const _: () = assert!(offset_of!(SocketFdInfo, psi) == 24);

/// Every TCP listener of the Mac. A process gone between the listing and the reading of its
/// sockets, and a descriptor closed meanwhile, are left out; any other failure fails the whole
/// listing, since a listener of that process could be missed.
pub(super) fn list() -> io::Result<Vec<Listener>> {
    let mut found = Vec::new();
    for pid in all_pids()? {
        // The kernel (pid 0) holds no descriptor table.
        if pid < 1 {
            continue;
        }
        // A zombie has exited and holds no socket.
        let Some(info) = short_info(pid)?.filter(|info| info.pbsi_status != libc::SZOMB) else {
            continue;
        };
        let Some(fds) = descriptors(pid)? else {
            continue;
        };
        let name = c_name(&info.pbsi_comm);
        let sockets = fds
            .iter()
            .filter(|fd| fd.proc_fdtype == libc::PROX_FDTYPE_SOCKET as u32);
        for fd in sockets {
            if let Some((addr, port)) = listening(pid, fd.proc_fd)? {
                found.push(Listener {
                    addr,
                    port,
                    pid,
                    uid: info.pbsi_uid,
                    ruid: info.pbsi_ruid,
                    name: name.clone(),
                });
            }
        }
    }
    Ok(found)
}

/// Whether an errno says the process or the descriptor is gone.
fn gone(why: &io::Error) -> bool {
    matches!(why.raw_os_error(), Some(libc::ESRCH | libc::EBADF))
}

/// The descriptors of `pid`, or `None` when it is gone.
fn descriptors(pid: libc::pid_t) -> io::Result<Option<Vec<libc::proc_fdinfo>>> {
    // SAFETY: a null buffer of size 0 asks only for the room the descriptors need; nothing
    // is written.
    let room =
        unsafe { libc::proc_pidinfo(pid, libc::PROC_PIDLISTFDS, 0, std::ptr::null_mut(), 0) };
    if room <= 0 {
        let why = io::Error::last_os_error();
        return match gone(&why) {
            true => Ok(None),
            false => Err(io::Error::new(why.kind(), format!("process {pid}: {why}"))),
        };
    }
    let entry = size_of::<libc::proc_fdinfo>();
    // Room for some more descriptors than the kernel counted, should some open meanwhile.
    let count = room.unsigned_abs() as usize / entry + 64;
    let mut fds: Vec<libc::proc_fdinfo> = Vec::with_capacity(count);
    let bytes = i32::try_from(count * entry).map_err(io::Error::other)?;
    // SAFETY: `fds` owns room for `count` entries, `bytes` bytes, and outlives the call,
    // which writes at most `bytes` bytes of `proc_fdinfo` records and returns how many.
    let wrote = unsafe {
        libc::proc_pidinfo(
            pid,
            libc::PROC_PIDLISTFDS,
            0,
            fds.as_mut_ptr().cast(),
            bytes,
        )
    };
    if wrote <= 0 {
        let why = io::Error::last_os_error();
        return match gone(&why) {
            true => Ok(None),
            false => Err(io::Error::new(why.kind(), format!("process {pid}: {why}"))),
        };
    }
    let got = wrote.unsigned_abs() as usize / entry;
    // SAFETY: the call wrote `got` whole records (at most `count`) into the start of the
    // buffer, so its first `got` entries are initialised plain C structs.
    unsafe { fds.set_len(got.min(count)) };
    Ok(Some(fds))
}

/// The address and port of the descriptor `fd` of `pid` when it is a TCP socket in the
/// listening state, `None` for any other socket and for one gone meanwhile.
fn listening(pid: libc::pid_t, fd: i32) -> io::Result<Option<(IpAddr, u16)>> {
    let mut answer = std::mem::MaybeUninit::<Answer>::zeroed();
    let size = i32::try_from(size_of::<Answer>()).map_err(io::Error::other)?;
    // SAFETY: `answer` is a writable buffer of `size` bytes, ours for the call, which writes
    // at most that many bytes.
    let wrote = unsafe {
        libc::proc_pidfdinfo(
            pid,
            fd,
            PROC_PIDFDSOCKETINFO,
            answer.as_mut_ptr().cast(),
            size,
        )
    };
    if wrote <= 0 {
        let why = io::Error::last_os_error();
        return match gone(&why) {
            true => Ok(None),
            false => Err(io::Error::new(
                why.kind(),
                format!("process {pid}, descriptor {fd}: {why}"),
            )),
        };
    }
    if (wrote.unsigned_abs() as usize) < size_of::<SocketFdInfo>() {
        return Err(io::Error::other(format!(
            "process {pid}, descriptor {fd}: a short answer of {wrote} bytes"
        )));
    }
    // SAFETY: the buffer was zeroed (a valid value of these plain C structs, every field an
    // integer or an array of them) and the call filled at least the whole `SocketFdInfo`.
    let answer = unsafe { answer.assume_init() };
    let socket = &answer.info.psi;
    if socket.soi_kind != SOCKINFO_TCP || socket.soi_proto.tcpsi_state != TSI_S_LISTEN {
        return Ok(None);
    }
    let inet = &socket.soi_proto.tcpsi_ini;
    Ok(Some((address(inet), port(inet.insi_lport))))
}

/// The local address of a socket: IPv6 when its flags say so, else IPv4 (the last four bytes
/// of an `in4in6_addr`); a socket that says neither is read as the IPv4 wildcard, which the
/// check counts as reachable, so an unreadable address is denied rather than missed.
fn address(inet: &InSockInfo) -> IpAddr {
    let bytes = inet.insi_laddr;
    if inet.insi_vflag & INI_IPV6 != 0 {
        return IpAddr::V6(Ipv6Addr::from(bytes));
    }
    if inet.insi_vflag & INI_IPV4 != 0 {
        return IpAddr::V4(Ipv4Addr::new(bytes[12], bytes[13], bytes[14], bytes[15]));
    }
    IpAddr::V4(Ipv4Addr::UNSPECIFIED)
}

/// The port of `insi_lport`: in network byte order in the low 16 bits of the int.
fn port(lport: libc::c_int) -> u16 {
    u16::from_be(lport as u16)
}
