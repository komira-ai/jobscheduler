//! The wire format: frames, descriptors, deadlines and the shape of each message.

use std::io::Write as _;
use std::os::fd::AsFd;
use std::os::unix::net::UnixStream;

use super::*;

fn run_request() -> Request {
    Request::Run {
        lease: "1.2".to_owned(),
        argv: vec!["/usr/bin/true".to_owned()],
        env: vec![("A".to_owned(), "b".to_owned())],
        session: Session::Background,
        limits: None,
        deadline_ms: None,
    }
}

/// Catches: descriptors lost or reordered between the daemon and the helper, or
/// a frame read short.
#[test]
fn a_frame_and_its_descriptors_arrive_together() {
    let (a, b) = UnixStream::pair().unwrap();
    let files: Vec<std::fs::File> = (0..RUN_FDS)
        .map(|_| std::fs::File::open("/dev/null").unwrap())
        .collect();
    let fds: Vec<BorrowedFd<'_>> = files.iter().map(AsFd::as_fd).collect();
    send(a.as_fd(), &run_request(), &fds).unwrap();
    send(a.as_fd(), &Reply::Killed, &[]).unwrap();
    drop(a);
    let (request, got) = recv::<Request>(b.as_fd(), RUN_FDS).unwrap().unwrap();
    assert_eq!(request, run_request());
    assert_eq!(got.len(), RUN_FDS);
    for fd in &got {
        let flags = rustix::io::fcntl_getfd(fd).unwrap();
        assert!(flags.contains(rustix::io::FdFlags::CLOEXEC));
    }
    assert_eq!(
        recv::<Reply>(b.as_fd(), 0).unwrap().unwrap().0,
        Reply::Killed
    );
    assert!(recv::<Reply>(b.as_fd(), 0).unwrap().is_none());
}

/// Catches: a request carrying more descriptors than `run` takes accepted (the
/// extra ones would stay open in the helper).
#[test]
fn too_many_descriptors_are_refused() {
    let (a, b) = UnixStream::pair().unwrap();
    let file = std::fs::File::open("/dev/null").unwrap();
    send(a.as_fd(), &Reply::Killed, &[file.as_fd(), file.as_fd()]).unwrap();
    let error = recv::<Reply>(b.as_fd(), 1).unwrap_err();
    assert!(error.to_string().contains("too many"), "{error}");

    let five = [file.as_fd(); RUN_FDS + 1];
    let error = send(a.as_fd(), &Reply::Killed, &five).unwrap_err();
    assert!(error.to_string().contains("too many"), "{error}");

    // Many more than a request carries, in one message: all received, then refused.
    let (a, b) = UnixStream::pair().unwrap();
    let many = [file.as_fd(); 8 * RUN_FDS];
    let mut space = [MaybeUninit::uninit(); rustix::cmsg_space!(ScmRights(8 * RUN_FDS))];
    let mut control = SendAncillaryBuffer::new(&mut space);
    assert!(control.push(SendAncillaryMessage::ScmRights(&many)));
    let frame = [0, 0, 0, 2, b'{', b'}'];
    rustix::net::sendmsg(
        &a,
        &[IoSlice::new(&frame)],
        &mut control,
        SendFlags::empty(),
    )
    .unwrap();
    let error = recv::<Reply>(b.as_fd(), RUN_FDS).unwrap_err();
    assert!(error.to_string().contains("too many"), "{error}");
}

/// The kernel cuts ancillary data that does not fit the receive buffer, and says
/// so; the cut is refused, not taken for the whole. (Linux: macOS's buffer is
/// larger than any control data its kernel passes.)
#[cfg(target_os = "linux")]
#[test]
fn descriptors_beyond_the_buffer_are_refused() {
    let (a, b) = UnixStream::pair().unwrap();
    let file = std::fs::File::open("/dev/null").unwrap();
    let many = [file.as_fd(); 4 * RUN_FDS];
    let body = b"{}";
    let mut frame = (body.len() as u32).to_be_bytes().to_vec();
    frame.extend_from_slice(body);
    let mut space = [MaybeUninit::uninit(); rustix::cmsg_space!(ScmRights(4 * RUN_FDS))];
    let mut control = SendAncillaryBuffer::new(&mut space);
    assert!(control.push(SendAncillaryMessage::ScmRights(&many)));
    rustix::net::sendmsg(
        &a,
        &[IoSlice::new(&frame)],
        &mut control,
        SendFlags::empty(),
    )
    .unwrap();
    let error = recv::<Reply>(b.as_fd(), 4 * RUN_FDS).unwrap_err();
    assert!(error.to_string().contains("cut them off"), "{error}");
}

/// Catches: an unbounded frame length (the helper would allocate what any caller
/// asks), a truncated frame taken whole, or unknown fields accepted.
#[test]
fn malformed_frames_are_refused() {
    let (mut a, b) = UnixStream::pair().unwrap();
    a.write_all(&(MAX_FRAME as u32 + 1).to_be_bytes()).unwrap();
    // Closed, so a reader that skipped the check fails at once rather than waiting.
    drop(a);
    assert!(
        recv::<Request>(b.as_fd(), 0)
            .unwrap_err()
            .to_string()
            .contains("too large")
    );

    let (mut a, b) = UnixStream::pair().unwrap();
    a.write_all(&[0, 0, 0, 9, b'{']).unwrap();
    drop(a);
    assert_eq!(
        recv::<Request>(b.as_fd(), 0).unwrap_err().kind(),
        io::ErrorKind::UnexpectedEof
    );

    let (mut a, b) = UnixStream::pair().unwrap();
    a.write_all(&[0, 0]).unwrap();
    drop(a);
    assert_eq!(
        recv::<Request>(b.as_fd(), 0).unwrap_err().kind(),
        io::ErrorKind::UnexpectedEof
    );

    let (mut a, b) = UnixStream::pair().unwrap();
    let body = br#"{"verb":"kill-uid","lease":"1.1","uid":0}"#;
    a.write_all(&(body.len() as u32).to_be_bytes()).unwrap();
    a.write_all(body).unwrap();
    assert!(recv::<Request>(b.as_fd(), 0).is_err());

    let (a, _b) = UnixStream::pair().unwrap();
    let huge = Reply::Refused {
        reason: "x".repeat(MAX_FRAME),
    };
    assert!(
        send(a.as_fd(), &huge, &[])
            .unwrap_err()
            .to_string()
            .contains("too large")
    );
}

/// A clock that reads the next of `readings` (in ms) at each call and the last one
/// after that, and counts the calls: each call is one look at what is left, so a
/// test says when each read of the frame happens without waiting for it.
struct Script {
    readings: Vec<u64>,
    calls: std::cell::Cell<usize>,
}

impl Script {
    fn new(readings: &[u64]) -> Self {
        Self {
            readings: readings.to_vec(),
            calls: std::cell::Cell::new(0),
        }
    }
    fn now(&self) -> std::time::Duration {
        let at = self.calls.get();
        self.calls.set(at + 1);
        let ms = self.readings[at.min(self.readings.len() - 1)];
        std::time::Duration::from_millis(ms)
    }
}

fn receive_timeout(fd: BorrowedFd<'_>) -> Option<std::time::Duration> {
    rustix::net::sockopt::socket_timeout(fd, rustix::net::sockopt::Timeout::Recv).unwrap()
}

/// Catches: `recv_by` checking the deadline only after reading, a deadline already
/// passed handed to the OS as a zero timeout (which means no timeout), a plain
/// `recv`'s own read timeout reported as a passed deadline, and a frame that
/// arrives in time not being read.
#[test]
fn recv_by_ends_at_its_deadline() {
    use std::time::Duration;
    let frame = [0, 0, 0, 8, b'"', b'k', b'i', b'l', b'l', b'e', b'd', b'"'];
    let (mut a, b) = UnixStream::pair().unwrap();
    a.write_all(&frame).unwrap();
    let late = recv_by::<Reply>(b.as_fd(), 0, Instant::now()).unwrap_err();
    assert_eq!(late.kind(), io::ErrorKind::TimedOut);
    let soon = Instant::now() + Duration::from_secs(10);
    let (reply, _) = recv_by::<Reply>(b.as_fd(), 0, soon).unwrap().unwrap();
    assert_eq!(reply, Reply::Killed);
    // Half a frame, the rest never sent (`a` stays open): refused at the deadline.
    a.write_all(&frame[..6]).unwrap();
    let deadline = Instant::now() + Duration::from_millis(50);
    let late = recv_by::<Reply>(b.as_fd(), 0, deadline).unwrap_err();
    assert_eq!(late.kind(), io::ErrorKind::TimedOut, "{late}");
    // Without a deadline, a read timeout set on the socket is its own error.
    let (_a, b) = UnixStream::pair().unwrap();
    b.set_read_timeout(Some(Duration::from_millis(20))).unwrap();
    let error = recv::<Reply>(b.as_fd(), 0).unwrap_err();
    assert_eq!(error.kind(), io::ErrorKind::WouldBlock, "{error}");
}

/// Catches: a receive timeout set once, before the first read, rather than before
/// every read from what is left (a short read would then restart the full wait and
/// a trickling sender could stretch the deadline). Two bytes of a header are
/// queued; the clock reads 0 ms at the start and 30 ms when the second read looks
/// at what is left of 50 ms, so that read may wait 20 ms and no more.
#[test]
fn each_read_waits_only_for_what_is_left_of_the_deadline() {
    use std::time::Duration;
    let (mut a, b) = UnixStream::pair().unwrap();
    a.write_all(&[0, 0]).unwrap();
    // Readings: the start (0), the first look (0), the second look (30).
    let clock = Script::new(&[0, 0, 30]);
    let now = || clock.now();
    let late = recv_within::<Reply>(b.as_fd(), 0, Duration::from_millis(50), &now).unwrap_err();
    assert_eq!(late.kind(), io::ErrorKind::TimedOut, "{late}");
    assert_eq!(clock.calls.get(), 3, "one look per read");
    assert_eq!(receive_timeout(b.as_fd()), Some(Duration::from_millis(20)));
    drop(a);
}

/// Catches a receive that gives up when a signal interrupts it. With a receive
/// timeout set, `recvmsg` is never restarted after a signal handler runs (Linux
/// returns EINTR even under `SA_RESTART`): the read must be retried, to the same
/// deadline, not reported as an error. The first look at what is left allows 30 s
/// (a safety bound: nothing arrives, so only a signal ends that read); a signaller
/// interrupts it until the read has looked again, and the second look finds the
/// deadline passed.
#[test]
fn a_signal_does_not_end_recv_by() {
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::time::Duration;
    extern "C" fn ignore(_: libc::c_int) {}
    // SAFETY: installs a handler that does nothing for a signal no other code of
    // this test binary sends or handles; the struct is zeroed, then filled.
    unsafe {
        let mut action: libc::sigaction = std::mem::zeroed();
        action.sa_sigaction = ignore as extern "C" fn(libc::c_int) as libc::sighandler_t;
        libc::sigemptyset(&raw mut action.sa_mask);
        assert_eq!(
            libc::sigaction(libc::SIGUSR2, &raw const action, std::ptr::null_mut()),
            0
        );
    }
    // SAFETY: the calling thread's own id; it outlives the signalling thread,
    // which is joined below.
    let me = unsafe { libc::pthread_self() } as usize;
    let (_a, b) = UnixStream::pair().unwrap();
    let done = std::sync::Arc::new(AtomicBool::new(false));
    let stop = done.clone();
    let signaller = std::thread::spawn(move || {
        while !stop.load(Ordering::SeqCst) {
            // SAFETY: the receiving thread is alive until this thread is joined.
            unsafe { libc::pthread_kill(me as libc::pthread_t, libc::SIGUSR2) };
            std::thread::yield_now();
        }
    });
    // Readings: the start (0), the first look (0: 30 s left), the second look
    // (after the interrupted read: past the 30 s deadline).
    let clock = Script::new(&[0, 0, 30_000]);
    let now = || clock.now();
    let late = recv_within::<Reply>(b.as_fd(), 0, Duration::from_secs(30), &now).unwrap_err();
    done.store(true, Ordering::SeqCst);
    signaller.join().unwrap();
    assert_eq!(late.kind(), io::ErrorKind::TimedOut, "{late}");
    assert_eq!(
        clock.calls.get(),
        3,
        "the read looked again after the signal"
    );
}

/// What a short `sendmsg` leaves is written after it, whole. (A blocking Unix
/// stream socket rarely sends short, so the remainder path is driven directly.)
#[test]
fn the_rest_of_a_short_send_is_written() {
    let (a, mut b) = UnixStream::pair().unwrap();
    write_all(a.as_fd(), b"rest of the frame").unwrap();
    drop(a);
    let mut got = String::new();
    std::io::Read::read_to_string(&mut b, &mut got).unwrap();
    assert_eq!(got, "rest of the frame");
    let big = Reply::Refused {
        reason: "y".repeat(MAX_FRAME - 64),
    };
    let (a, b) = UnixStream::pair().unwrap();
    let reader = std::thread::spawn(move || recv::<Reply>(b.as_fd(), 0).unwrap().unwrap().0);
    send(a.as_fd(), &big, &[]).unwrap();
    assert_eq!(reader.join().unwrap(), big);
}

/// Only descriptors are taken from ancillary data (credentials, which the socket
/// never asks for, are not descriptors).
#[cfg(target_os = "linux")]
#[test]
fn credentials_are_not_descriptors() {
    let credentials = rustix::net::UCred {
        pid: rustix::process::getpid(),
        uid: rustix::process::getuid(),
        gid: rustix::process::getgid(),
    };
    assert!(rights(RecvAncillaryMessage::ScmCredentials(credentials)).is_none());
}

#[test]
fn the_wire_names_are_kebab_case() {
    let text = serde_json::to_string(&Request::UserCreate {
        lease: "1.1".to_owned(),
        grant: None,
    })
    .unwrap();
    assert_eq!(text, r#"{"verb":"user-create","lease":"1.1"}"#);
    let text = serde_json::to_string(&Reply::Deleted { existed: true }).unwrap();
    assert_eq!(text, r#"{"deleted":{"existed":true}}"#);
    let text = serde_json::to_string(&Request::RebootDirty).unwrap();
    assert_eq!(text, r#"{"verb":"reboot-dirty"}"#);
    let classes = vec![("lease-users".to_owned(), 1)];
    let text = serde_json::to_string(&Reply::Baselined { classes }).unwrap();
    assert_eq!(text, r#"{"baselined":{"classes":[["lease-users",1]]}}"#);
}

/// Catches: role verbs that change the wire of the lease verbs, or whose names
/// differ from the ones the daemon sends; and a `role-run` that needs an `env` an
/// older sender leaves out.
#[test]
fn the_role_verbs_are_their_own_verbs() {
    let text = serde_json::to_string(&Request::RoleSessionReady).unwrap();
    assert_eq!(text, r#"{"verb":"role-session-ready"}"#);
    let text = serde_json::to_string(&Request::RoleKillUid).unwrap();
    assert_eq!(text, r#"{"verb":"role-kill-uid"}"#);
    let reap = Request::RoleReap {
        pgid: 4242,
        start: 1_700_000_000_123_456,
        boot: "B0".to_owned(),
    };
    let text = serde_json::to_string(&reap).unwrap();
    assert_eq!(
        text,
        r#"{"verb":"role-reap","pgid":4242,"start":1700000000123456,"boot":"B0"}"#
    );
    assert_eq!(serde_json::from_str::<Request>(&text).unwrap(), reap);
    let run = Request::RoleRun {
        argv: vec!["/usr/bin/true".to_owned()],
        env: Vec::new(),
    };
    let text = serde_json::to_string(&run).unwrap();
    assert_eq!(
        text,
        r#"{"verb":"role-run","argv":["/usr/bin/true"],"env":[]}"#
    );
    let bare = r#"{"verb":"role-run","argv":["/usr/bin/true"]}"#;
    assert_eq!(serde_json::from_str::<Request>(bare).unwrap(), run);
}

/// Catches: an image verb that can carry a program, an argument vector or a
/// guest binary path from the caller (the helper must run only its own installed
/// `js-vmm`, with argv it builds), or whose names differ from the daemon's.
#[test]
fn an_image_verb_carries_paths_and_nothing_to_run() {
    let setup = Request::ImageSetup {
        bundle: "/images/base".to_owned(),
    };
    let text = serde_json::to_string(&setup).unwrap();
    assert_eq!(text, r#"{"verb":"image-setup","bundle":"/images/base"}"#);
    assert_eq!(serde_json::from_str::<Request>(&text).unwrap(), setup);
    let harvest = Request::ImageHarvest {
        bundle: "/images/clone".to_owned(),
        out: "/images/out".to_owned(),
    };
    let text = serde_json::to_string(&harvest).unwrap();
    assert_eq!(
        text,
        r#"{"verb":"image-harvest","bundle":"/images/clone","out":"/images/out"}"#
    );
    assert_eq!(serde_json::from_str::<Request>(&text).unwrap(), harvest);
    for hostile in [
        r#"{"verb":"image-setup","bundle":"/images/b","program":"/bin/sh"}"#,
        r#"{"verb":"image-setup","bundle":"/images/b","argv":["/bin/sh"]}"#,
        r#"{"verb":"image-setup","bundle":"/images/b","guest-binary":"/tmp/g"}"#,
        r#"{"verb":"image-harvest","bundle":"/images/b","out":"/images/o","args":[]}"#,
    ] {
        assert!(
            serde_json::from_str::<Request>(hostile).is_err(),
            "{hostile}"
        );
    }
    let text = serde_json::to_string(&Reply::Imaged {
        report: "{}".to_owned(),
    })
    .unwrap();
    assert_eq!(text, r#"{"imaged":{"report":"{}"}}"#);
}

/// Catches a `run` that changes on the wire for the daemons that predate the
/// session target (a background run must still send, and read, no `session`), and a
/// GUI run whose target is lost or misnamed.
#[test]
fn a_run_names_its_session_only_when_it_is_the_gui() {
    let background = serde_json::to_string(&run_request()).unwrap();
    assert!(!background.contains("session"), "{background}");
    let gui = Request::Run {
        lease: "1.2".to_owned(),
        argv: vec!["/usr/bin/true".to_owned()],
        env: vec![("A".to_owned(), "b".to_owned())],
        session: Session::Gui,
        limits: None,
        deadline_ms: None,
    };
    let text = serde_json::to_string(&gui).unwrap();
    assert!(text.ends_with(r#","session":"gui"}"#), "{text}");
    assert_eq!(serde_json::from_str::<Request>(&text).unwrap(), gui);
    assert_eq!(
        serde_json::from_str::<Request>(&background).unwrap(),
        run_request()
    );
    let ready = serde_json::to_string(&Request::SessionReady {
        lease: "1.2".to_owned(),
    })
    .unwrap();
    assert_eq!(ready, r#"{"verb":"session-ready","lease":"1.2"}"#);
    let answer = serde_json::to_string(&Reply::SessionReady { ready: true }).unwrap();
    assert_eq!(answer, r#"{"session-ready":{"ready":true}}"#);
}

/// A helper and a daemon from before `watch`, `stop` and `run`'s `deadline_ms`: the request
/// as they read it, unknown fields refused, as this crate's own type does.
mod old {
    use serde::Deserialize;

    #[derive(Debug, PartialEq, Eq, Deserialize)]
    #[serde(tag = "verb", rename_all = "kebab-case", deny_unknown_fields)]
    pub(super) enum Request {
        Run {
            lease: String,
            argv: Vec<String>,
            #[serde(default)]
            env: Vec<(String, String)>,
            #[serde(default)]
            session: Option<String>,
            #[serde(default)]
            limits: Option<serde_json::Value>,
        },
        KillUid {
            lease: String,
        },
    }
}

/// The refusal a helper's server gives a request it cannot read, as a client sees it.
fn refusal_of(why: &serde_json::Error) -> crate::client::ClientError {
    crate::client::ClientError::Refused(format!("bad request: {why}"))
}

/// Catches: a `run` that changes on the wire for a helper or a daemon from before the
/// deadline. A request with no deadline is the old text exactly, reads in the old decoder,
/// and the old text reads in the new one as no deadline.
#[test]
fn a_run_without_a_deadline_is_the_old_request() {
    let text = serde_json::to_string(&run_request()).unwrap();
    assert!(!text.contains("deadline"), "{text}");
    let old: old::Request = serde_json::from_str(&text).unwrap();
    let want = old::Request::Run {
        lease: "1.2".to_owned(),
        argv: vec!["/usr/bin/true".to_owned()],
        env: vec![("A".to_owned(), "b".to_owned())],
        session: None,
        limits: None,
    };
    assert_eq!(old, want);
    let back: Request = serde_json::from_str(&text).unwrap();
    assert_eq!(back, run_request());
}

/// Catches: a deadline or a new verb that an old helper would take for something else, or
/// whose refusal the client cannot tell from any other (it must fall back, and only then).
/// The new shapes round-trip in the new decoder; the old decoder refuses each, and the
/// refusal the server would send reads as one the client may fall back from.
#[test]
fn the_new_shapes_are_refused_by_an_old_decoder_in_a_way_the_client_reads() {
    let bounded = Request::Run {
        lease: "1.2".to_owned(),
        argv: vec!["/usr/bin/true".to_owned()],
        env: Vec::new(),
        session: Session::Background,
        limits: None,
        deadline_ms: Some(65_000),
    };
    let watch = Request::Watch {
        lease: "1.2".to_owned(),
    };
    let stop = Request::Stop {
        lease: "1.2".to_owned(),
        grace_ms: 5_000,
    };
    for request in [bounded, watch, stop] {
        let text = serde_json::to_string(&request).unwrap();
        assert_eq!(serde_json::from_str::<Request>(&text).unwrap(), request);
        let why = serde_json::from_str::<old::Request>(&text).unwrap_err();
        assert!(refusal_of(&why).unknown_to_helper(), "{text}: {why}");
    }
}

/// Catches: a client that falls back from a refusal that is not about an unknown verb or
/// field (it would then kill where it should have stopped, or drop a deadline for nothing).
#[test]
fn only_an_unreadable_request_is_one_to_fall_back_from() {
    use crate::client::ClientError;
    let malformed = serde_json::from_str::<old::Request>("{").unwrap_err();
    assert!(!refusal_of(&malformed).unknown_to_helper());
    let refused = ClientError::Refused("lease 1.2 has no user".to_owned());
    assert!(!refused.unknown_to_helper());
    let named = ClientError::Refused("unknown variant `stop`".to_owned());
    assert!(!named.unknown_to_helper(), "not the server's bad request");
    let lost = ClientError::Io(io::Error::other("unknown field"));
    assert!(!lost.unknown_to_helper());
}
