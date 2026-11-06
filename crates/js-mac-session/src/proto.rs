//! The helper's wire format on its Unix socket.
//!
//! One connection carries one request. Every message is a frame: a 4-byte big-endian
//! length, then that many bytes of JSON. The helper speaks first: once it has checked
//! the process that connected, it sends [`Reply::Hello`] with a fresh nonce (or
//! [`Reply::Refused`], and hangs up). The client's request is then one frame, a
//! [`Call`] that carries that nonce, so a request written before the helper's check
//! cannot pass for one written after it (S4.3). `run` attaches four descriptors to it
//! (`SCM_RIGHTS`): stdin, stdout, stderr and the lease directory, in that order, and so
//! does `role-run`. The
//! helper answers with one frame, except that a started `run` answers
//! [`Reply::Started`] and then, when the process exits, [`Reply::Exited`].

use std::io::{self, IoSlice, IoSliceMut};
use std::mem::MaybeUninit;
use std::os::fd::{BorrowedFd, OwnedFd};
use std::time::{Duration, Instant};

use rustix::net::sockopt::{Timeout, set_socket_timeout};
use rustix::net::{
    RecvAncillaryBuffer, RecvAncillaryMessage, RecvFlags, SendAncillaryBuffer,
    SendAncillaryMessage, SendFlags,
};
use serde::{Deserialize, Serialize};

pub use crate::limits::Limits;

/// The largest frame either side accepts.
pub const MAX_FRAME: usize = 64 * 1024;
/// The number of descriptors a `run` request carries.
pub const RUN_FDS: usize = 4;
/// The most entries each list of a [`Reply::Scanned`] names, each cut to
/// [`SCAN_LINE_BYTES`], so the reply fits a frame; the helper's log holds them all.
pub const SCAN_LINES: usize = 20;
/// The most bytes of one entry of a [`Reply::Scanned`] list; a longer one is cut.
pub const SCAN_LINE_BYTES: usize = 300;

/// The most bytes of `js-vmm`'s report an image verb's reply carries.
pub const IMAGE_REPORT_BYTES: usize = 16 * 1024;

/// The receive buffer for ancillary data. On Linux, room for one more descriptor than a
/// request may carry: the kernel cuts what does not fit, says so (`MSG_CTRUNC`), and
/// the cut is refused. On macOS the kernel takes at most `MCLBYTES` (2048) bytes of
/// control data with one message, so this buffer is never cut there; that matters,
/// because a cut message keeps its full length in its header on macOS, which the
/// parser would read past.
#[cfg(target_os = "linux")]
const CONTROL_BYTES: usize = rustix::cmsg_space!(ScmRights(RUN_FDS + 1));
#[cfg(not(target_os = "linux"))]
const CONTROL_BYTES: usize = 4096;

/// The client's one frame: the nonce of the helper's [`Reply::Hello`] on this
/// connection, and the request.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Call {
    /// The nonce the helper sent in its [`Reply::Hello`] on this connection.
    pub nonce: String,
    /// What the daemon asks.
    pub request: Request,
}

/// What the daemon asks.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "verb", rename_all = "kebab-case", deny_unknown_fields)]
pub enum Request {
    /// Create the lease's user; an administrator only with a gate grant.
    UserCreate {
        /// The lease, as `<term>.<seq>`.
        lease: String,
        /// The gate's signed grant, which makes the user an administrator.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        grant: Option<String>,
    },
    /// Start a process as the lease's user, with the attached descriptors; in its GUI
    /// session when `session` is `gui`.
    Run {
        /// The lease, as `<term>.<seq>`.
        lease: String,
        /// The program and its arguments.
        argv: Vec<String>,
        /// The environment the process gets, as (name, value) pairs.
        #[serde(default)]
        env: Vec<(String, String)>,
        /// Where the process starts.
        #[serde(default, skip_serializing_if = "Session::is_background")]
        session: Session,
        /// The file-size and process limits the helper sets on the process before it
        /// starts it; none for no limits. Refused in the GUI session.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        limits: Option<Limits>,
        /// The run's own deadline, in milliseconds after it starts: once it passes and the
        /// process still runs, the helper ends the lease's processes as `kill-uid` does,
        /// whether or not a caller is still connected. None for no deadline. Left out of a
        /// request that has none, so such a request is the text it was before the field. A
        /// helper from before the field refuses a request that carries it (`bad request:
        /// unknown field`), and starts nothing.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        deadline_ms: Option<u64>,
    },
    /// Find again the last process `run` started for the lease, for a caller that lost its
    /// connection: answered as `run` is, [`Reply::Started`] with its pid and then
    /// [`Reply::Exited`] when it ends (at once if it has). Refused for a lease with no run
    /// the helper has in memory (`has no run this helper started`).
    Watch {
        /// The lease, as `<term>.<seq>`.
        lease: String,
    },
    /// End the lease's run softly: `SIGTERM` to the process group of its last `run`, then,
    /// if that process has not ended once `grace_ms` milliseconds have passed, what
    /// `kill-uid` does. Answered [`Reply::Killed`].
    Stop {
        /// The lease, as `<term>.<seq>`.
        lease: String,
        /// How long the process has to end after the `SIGTERM`, in milliseconds.
        grace_ms: u64,
    },
    /// Whether the lease user's GUI session (`gui/<uid>`) is up, after `session-login`.
    SessionReady {
        /// The lease, as `<term>.<seq>`.
        lease: String,
    },
    /// End every process of the lease's uid.
    KillUid {
        /// The lease, as `<term>.<seq>`.
        lease: String,
    },
    /// Delete the lease's user and sweep what it leaves.
    UserDelete {
        /// The lease, as `<term>.<seq>`.
        lease: String,
    },
    /// Auto-login into the lease's user, then a userspace restart.
    SessionLogin {
        /// The lease, as `<term>.<seq>`.
        lease: String,
    },
    /// Rest the Mac, then a userspace restart: at the standing VM user when the helper
    /// has one (`--vm-user`), else at the login window (auto-login cleared).
    SessionIdle,
    /// Start `js-vmm vm-run --spec <file>` as the standing VM user in its GUI session,
    /// with the same four descriptors as `run`. Refused unless the argument vector is
    /// exactly the helper's configured `js-vmm`, `vm-run`, `--spec` and a regular file
    /// inside the images directory reached through no link, the VM user's `gui/<uid>`
    /// domain is up and no lease holds the session.
    RoleRun {
        /// The argument vector, which must be exactly the allowed one (see above).
        argv: Vec<String>,
        /// The environment, which must be empty.
        #[serde(default)]
        env: Vec<(String, String)>,
    },
    /// Whether the standing VM user's GUI session (`gui/<uid>`) is up. False while a
    /// lease holds the session.
    RoleSessionReady,
    /// End every process of the standing VM user (its session with them).
    RoleKillUid,
    /// End the process group a `role-run` started and nothing else: `pgid` is the pid
    /// `Started` reported, `start` that process's start time and `boot` the boot session
    /// it ran in. The helper kills the group only while that process still leads it, is
    /// the VM user's, and has this start time in this boot.
    RoleReap {
        /// The process group to end: the pid `Started` reported.
        pgid: i32,
        /// The start time of the process that leads the group.
        start: u64,
        /// The boot session the process ran in.
        boot: String,
    },
    /// As root, take the base image bundle `bundle` (a path directly inside the staging
    /// directory), move it into the images directory, write the guest's first-boot files
    /// into it (`js-vmm image setup`) and seal it: root's, mode 0750/0640 with the VM
    /// user's group, `uchg`. There is no program and no argument here: the helper runs its
    /// own installed `js-vmm`.
    ImageSetup {
        /// The base image bundle, a path directly inside the staging directory.
        bundle: String,
    },
    /// As root, copy the diagnostics allowlist out of the stopped guest bundle `bundle`
    /// (directly inside the staging or the images directory) into the new directory `out`
    /// (directly inside the images directory) (`js-vmm image harvest`); the helper makes
    /// `out`, root's and mode 0700.
    ImageHarvest {
        /// The stopped guest bundle, directly inside the staging or the images directory.
        bundle: String,
        /// The new directory to copy into, directly inside the images directory.
        out: String,
    },
    /// The boot reset, before the daemon's `Hello`: `held` are the leases the server
    /// still holds for this node.
    BootReset {
        /// The leases the server still holds for this node, as `<term>.<seq>`.
        #[serde(default)]
        held: Vec<String>,
    },
    /// The leak scan against the recorded baseline.
    Scan,
    /// Record the leak scan's baseline, in its window.
    Baseline,
    /// Reboot, after the helper's own scan came back dirty.
    RebootDirty,
}

/// Where `run` starts a process.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Session {
    /// Outside any login session: the system's launchd domain, as the helper runs.
    #[default]
    Background,
    /// In the lease user's GUI (Aqua) session, `gui/<uid>`, through `launchctl asuser`:
    /// the session `session-login` started. Refused while the session is not up.
    Gui,
}

impl Session {
    /// Whether this is [`Session::Background`], which a request leaves out.
    #[must_use]
    pub const fn is_background(&self) -> bool {
        matches!(self, Self::Background)
    }
}

/// What the helper answers.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case", deny_unknown_fields)]
pub enum Reply {
    /// The caller passed the check at connect; its request must carry this nonce.
    Hello {
        /// The nonce the request must carry.
        nonce: String,
    },
    /// The user exists with this uid.
    Created {
        /// The new user's uid.
        uid: u32,
    },
    /// The process started with this pid.
    Started {
        /// The new process's pid.
        pid: u32,
    },
    /// The process ended: its exit code, or the signal that ended it.
    Exited {
        /// The exit code, when the process exited.
        code: Option<i32>,
        /// The signal that ended the process, when one did.
        signal: Option<i32>,
    },
    /// No process of the uid remains.
    Killed,
    /// The user is gone and its state swept; `existed` is false when there was no
    /// user record left to delete (a repeated or resumed delete).
    Deleted {
        /// Whether there was a user record to delete.
        existed: bool,
    },
    /// The session switch is set and a userspace restart was asked for; the restart
    /// may end the connection before this arrives.
    Restarting,
    /// Whether the lease user's GUI session is up.
    SessionReady {
        /// Whether the GUI session is up.
        ready: bool,
    },
    /// The boot reset is done: the lease whose session it kept, and the leases whose
    /// users it deleted.
    Reset {
        /// The lease whose session was kept, as `<term>.<seq>`.
        kept: Option<String>,
        /// The leases whose users were deleted.
        deleted: Vec<String>,
    },
    /// The scan ran: `clean` when nothing was added to or removed from the baseline
    /// and every item could be read. Each list names at most
    /// [`SCAN_LINES`] entries, then how many more there are.
    Scanned {
        /// Whether nothing changed and every item could be read.
        clean: bool,
        /// Items that are not in the baseline.
        added: Vec<String>,
        /// Items of the baseline that are gone.
        removed: Vec<String>,
        /// Items that could not be read.
        errors: Vec<String>,
    },
    /// The baseline is recorded: how many items each class holds.
    Baselined {
        /// Each class of items and how many it holds.
        classes: Vec<(String, usize)>,
    },
    /// An image verb ran: what `js-vmm` printed (its JSON report), cut to
    /// [`IMAGE_REPORT_BYTES`].
    Imaged {
        /// The report, as the VMM printed it.
        report: String,
    },
    /// The request was refused or failed, and why.
    Refused {
        /// Why.
        reason: String,
    },
}

/// Sends `message` as one frame, with `fds` attached.
///
/// # Errors
/// The message is larger than [`MAX_FRAME`], or the socket write failed.
pub fn send<T: Serialize>(
    socket: BorrowedFd<'_>,
    message: &T,
    fds: &[BorrowedFd<'_>],
) -> io::Result<()> {
    send_frame(socket, &serde_json::to_vec(message)?, fds)
}

/// Receives one frame as a `T`, and the descriptors attached to it; `None` at a clean
/// end of stream before any byte. More than `max_fds` descriptors is an error (they
/// are closed).
///
/// # Errors
/// The stream ended inside a frame, the frame is too large or not a `T`, or too many
/// descriptors came.
pub fn recv<T: for<'de> Deserialize<'de>>(
    socket: BorrowedFd<'_>,
    max_fds: usize,
) -> io::Result<Option<(T, Vec<OwnedFd>)>> {
    recv_frame(socket, max_fds, None)?
        .map(|(body, fds)| Ok((serde_json::from_slice(&body)?, fds)))
        .transpose()
}

/// [`recv`], with the whole frame due by `deadline`: each read waits only for what is
/// left until then (it sets the socket's receive timeout), so a sender that trickles
/// bytes cannot stretch the wait.
///
/// # Errors
/// As [`recv`]; [`io::ErrorKind::TimedOut`] once the deadline has passed.
pub fn recv_by<T: for<'de> Deserialize<'de>>(
    socket: BorrowedFd<'_>,
    max_fds: usize,
    deadline: Instant,
) -> io::Result<Option<(T, Vec<OwnedFd>)>> {
    let origin = Instant::now();
    recv_within(
        socket,
        max_fds,
        deadline.saturating_duration_since(origin),
        &|| origin.elapsed(),
    )
}

/// A reading of a clock: time since an origin that does not change.
pub type Now<'a> = &'a dyn Fn() -> Duration;

/// [`recv_by`] with the deadline `within` after the call, on the clock `now`: the whole
/// frame is due when `now()` reads `within` more than it did at the call, and each read
/// waits only for what `now()` says is left. A test passes a clock it moves itself.
///
/// # Errors
/// As [`recv_by`].
pub fn recv_within<T: for<'de> Deserialize<'de>>(
    socket: BorrowedFd<'_>,
    max_fds: usize,
    within: Duration,
    now: Now<'_>,
) -> io::Result<Option<(T, Vec<OwnedFd>)>> {
    let due = Due {
        by: now().saturating_add(within),
        now,
    };
    recv_frame(socket, max_fds, Some(&due))?
        .map(|(body, fds)| Ok((serde_json::from_slice(&body)?, fds)))
        .transpose()
}

/// When a frame is due, and the clock that says what is left.
struct Due<'a> {
    by: Duration,
    now: Now<'a>,
}

fn send_frame(socket: BorrowedFd<'_>, body: &[u8], fds: &[BorrowedFd<'_>]) -> io::Result<()> {
    if body.len() > MAX_FRAME {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "frame too large",
        ));
    }
    let mut frame = u32::try_from(body.len())
        .map_err(io::Error::other)?
        .to_be_bytes()
        .to_vec();
    frame.extend_from_slice(body);
    let mut space = [MaybeUninit::uninit(); rustix::cmsg_space!(ScmRights(RUN_FDS))];
    let mut control = SendAncillaryBuffer::new(&mut space);
    if fds.len() > RUN_FDS {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "too many descriptors",
        ));
    }
    if !fds.is_empty() {
        // The buffer has room for RUN_FDS descriptors, so this cannot fail; were it
        // to, the request would arrive without them and be refused.
        control.push(SendAncillaryMessage::ScmRights(fds));
    }
    let sent = rustix::net::sendmsg(
        socket,
        &[IoSlice::new(&frame)],
        &mut control,
        SendFlags::empty(),
    )?;
    write_all(socket, &frame[sent..])
}

/// Writes what a short `sendmsg` left (the descriptors went with its first byte).
fn write_all(socket: BorrowedFd<'_>, mut rest: &[u8]) -> io::Result<()> {
    while !rest.is_empty() {
        let wrote = rustix::net::send(socket, rest, SendFlags::empty())?;
        rest = &rest[wrote..];
    }
    Ok(())
}

/// One frame's bytes and descriptors; see [`recv`].
fn recv_frame(
    socket: BorrowedFd<'_>,
    max_fds: usize,
    deadline: Option<&Due<'_>>,
) -> io::Result<Option<(Vec<u8>, Vec<OwnedFd>)>> {
    let mut fds = Vec::new();
    let mut header = [0u8; 4];
    let got = fill(socket, &mut header, &mut fds, deadline)?;
    if got == 0 {
        return Ok(None);
    }
    if got < header.len() {
        return Err(io::ErrorKind::UnexpectedEof.into());
    }
    let len = u32::from_be_bytes(header) as usize;
    if len > MAX_FRAME {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "frame too large",
        ));
    }
    let mut body = vec![0u8; len];
    if fill(socket, &mut body, &mut fds, deadline)? < len {
        return Err(io::ErrorKind::UnexpectedEof.into());
    }
    if fds.len() > max_fds {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "too many descriptors",
        ));
    }
    Ok(Some((body, fds)))
}

/// Fills `buf` from the socket, collecting attached descriptors, by `deadline` if
/// there is one. Returns how many bytes came: fewer than `buf` holds only if the
/// stream ended.
fn fill(
    socket: BorrowedFd<'_>,
    buf: &mut [u8],
    fds: &mut Vec<OwnedFd>,
    deadline: Option<&Due<'_>>,
) -> io::Result<usize> {
    let late = || io::Error::new(io::ErrorKind::TimedOut, "the deadline passed");
    let mut got = 0;
    while got < buf.len() {
        if let Some(deadline) = deadline {
            let left = deadline.by.saturating_sub((deadline.now)());
            if left.is_zero() {
                return Err(late());
            }
            set_socket_timeout(socket, Timeout::Recv, Some(left))?;
        }
        let mut space = [MaybeUninit::uninit(); CONTROL_BYTES];
        let mut control = RecvAncillaryBuffer::new(&mut space);
        let message = match rustix::net::recvmsg(
            socket,
            &mut [IoSliceMut::new(&mut buf[got..])],
            &mut control,
            recv_flags(),
        ) {
            // The receive timeout set above ran out: the deadline passed.
            Err(rustix::io::Errno::AGAIN) if deadline.is_some() => return Err(late()),
            // A signal handler ran; with a receive timeout set, the kernel does not
            // restart the call. Read again, to the time the deadline leaves.
            Err(rustix::io::Errno::INTR) => continue,
            other => other?,
        };
        for received in control.drain().filter_map(rights) {
            for fd in received {
                close_on_exec(&fd)?;
                fds.push(fd);
            }
        }
        if message.flags.contains(rustix::net::ReturnFlags::CTRUNC) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "too many descriptors: the kernel cut them off",
            ));
        }
        if message.bytes == 0 {
            break;
        }
        got += message.bytes;
    }
    Ok(got)
}

/// The descriptors of an `SCM_RIGHTS` message; no other kind is asked for.
fn rights(message: RecvAncillaryMessage<'_>) -> Option<impl Iterator<Item = OwnedFd> + '_> {
    match message {
        RecvAncillaryMessage::ScmRights(fds) => Some(fds),
        _ => None,
    }
}

/// Linux delivers received descriptors close-on-exec; macOS has no such flag, so
/// [`close_on_exec`] marks them at once (and the child's own pass covers the gap).
#[cfg(target_os = "linux")]
fn recv_flags() -> RecvFlags {
    RecvFlags::CMSG_CLOEXEC
}

#[cfg(not(target_os = "linux"))]
fn recv_flags() -> RecvFlags {
    RecvFlags::empty()
}

fn close_on_exec(fd: &OwnedFd) -> io::Result<()> {
    Ok(rustix::io::fcntl_setfd(fd, rustix::io::FdFlags::CLOEXEC)?)
}

#[cfg(test)]
mod tests;
