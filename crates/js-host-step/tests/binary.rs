//! Whether the running program may carry a secret (its image and every directory above it are
//! root's alone), and who owns the listener a secret would be sent to.

use std::collections::BTreeMap;
use std::io;
use std::net::{TcpListener, UdpSocket};
use std::os::unix::fs::MetadataExt;

use js_host_step::fake::FakeHost;
use js_host_step::{Host, RealHost, check_ancestry, check_owner_mode, parse_listeners};

/// Catches a binary that others could replace passing as trusted: only a file owned by root
/// and writable by neither its group nor others is accepted; the message names the path and
/// says which of the two is wrong.
#[test]
fn only_a_file_root_owns_alone_is_trusted() {
    for mode in [0o755, 0o700, 0o555, 0o4755, 0o000, 0o100755] {
        assert!(check_owner_mode("/p", 0, mode).is_ok(), "{mode:o}");
    }
    for mode in [0o775, 0o757, 0o777, 0o772, 0o720, 0o41777] {
        let err = check_owner_mode("/p", 0, mode).unwrap_err().to_string();
        assert!(
            err.starts_with("/p is writable by group or others (mode "),
            "{err}"
        );
    }
    let err = check_owner_mode("/p", 0, 0o100775).unwrap_err().to_string();
    assert_eq!(err, "/p is writable by group or others (mode 0775)");
    for (uid, mode) in [(1000, 0o755), (1000, 0o777), (1, 0o700)] {
        let err = check_owner_mode("/p", uid, mode).unwrap_err().to_string();
        assert_eq!(err, format!("/p is owned by uid {uid}, not root"));
    }
}

fn tree(entries: &[(&str, u32, u32)]) -> impl Fn(&str) -> io::Result<Option<(u32, u32)>> {
    let map: BTreeMap<String, (u32, u32)> = entries
        .iter()
        .map(|(p, uid, mode)| ((*p).to_owned(), (*uid, *mode)))
        .collect();
    move |p| Ok(map.get(p).copied())
}

/// Catches a check that looks at the binary alone (a user-owned or writable directory above it
/// lets that user swap the binary), one that stops before `/`, one that reads the owner but not
/// the group and other write bits, and a missing directory passing: every entry from the file
/// up to `/` is judged, and the error names the first one that is not root's alone.
#[test]
fn every_directory_up_to_the_root_is_checked() {
    let good = [
        ("/", 0, 0o40755),
        ("/usr", 0, 0o40755),
        ("/usr/local", 0, 0o40755),
        ("/usr/local/bin", 0, 0o40755),
        ("/usr/local/bin/js-mdm", 0, 0o100755),
    ];
    let exe = "/usr/local/bin/js-mdm";
    check_ancestry(exe, tree(&good)).expect("a tree root owns alone passes");
    let cases: [(usize, u32, u32, &str); 6] = [
        (
            4,
            501,
            0o100755,
            "/usr/local/bin/js-mdm is owned by uid 501",
        ),
        (4, 0, 0o100775, "/usr/local/bin/js-mdm is writable"),
        (
            3,
            0,
            0o40757,
            "/usr/local/bin is writable by group or others (mode 0757)",
        ),
        (2, 501, 0o40755, "/usr/local is owned by uid 501"),
        (
            1,
            0,
            0o41777,
            "/usr is writable by group or others (mode 1777)",
        ),
        (0, 0, 0o40775, "/ is writable"),
    ];
    for (at, uid, mode, want) in cases {
        let mut entries = good;
        entries[at] = (entries[at].0, uid, mode);
        let err = check_ancestry(exe, tree(&entries)).unwrap_err().to_string();
        assert!(err.starts_with(want), "{want}: {err}");
    }
    let err = check_ancestry(exe, tree(&good[1..]))
        .unwrap_err()
        .to_string();
    assert_eq!(err, "cannot check /: nothing is there");
    let err = check_ancestry(exe, |_| Err(io::Error::other("denied"))).unwrap_err();
    assert_eq!(err.to_string(), "denied");
}

fn root_tree(host: &FakeHost) {
    for dir in ["/", "/usr", "/usr/local", "/usr/local/bin"] {
        host.put_dir(dir, 0o755, (0, 0));
    }
    host.put_file("/usr/local/bin/js-mdm", 0o755, (0, 0), b"x");
}

/// Catches the fake host (and so a verb's test) letting a binary through whose directories a
/// user can write: with the binary placed in the tree, a user-owned or group-writable ancestor
/// is refused, a link component is resolved first and the directory it leads to is judged
/// (a link in root's directory to a user's directory is refused; to root's tree it passes),
/// and an all-root tree passes. Without a placed binary only the image is judged.
#[test]
fn the_fake_host_judges_the_binary_and_every_directory_above_it() {
    let host = FakeHost::new(0);
    assert!(host.check_binary().is_ok());
    host.set_binary_path("/usr/local/bin/js-mdm");
    assert!(host.check_binary().is_err(), "nothing is there yet");
    root_tree(&host);
    assert!(host.check_binary().is_ok());

    host.put_dir("/usr/local", 0o755, (1000, 1000));
    let err = host.check_binary().unwrap_err().to_string();
    assert_eq!(err, "/usr/local is owned by uid 1000, not root");
    host.put_dir("/usr/local", 0o775, (0, 0));
    let err = host.check_binary().unwrap_err().to_string();
    assert_eq!(err, "/usr/local is writable by group or others (mode 0775)");
    host.put_dir("/usr/local", 0o755, (0, 0));
    assert!(host.check_binary().is_ok());

    // /opt/js is a link in root's directory; where it leads decides.
    host.put_dir("/opt", 0o755, (0, 0));
    host.put_symlink("/opt/js", "/home/u/js");
    host.put_dir("/home", 0o755, (0, 0));
    host.put_dir("/home/u", 0o755, (1000, 1000));
    host.put_dir("/home/u/js", 0o755, (0, 0));
    host.put_file("/home/u/js/js-mdm", 0o755, (0, 0), b"x");
    host.set_binary_path("/opt/js/js-mdm");
    let err = host.check_binary().unwrap_err().to_string();
    assert_eq!(err, "/home/u is owned by uid 1000, not root");
    host.put_symlink("/opt/js", "/usr/local/bin");
    host.put_file("/usr/local/bin/js-mdm", 0o755, (0, 0), b"x");
    assert!(host.check_binary().is_ok());

    // The image itself.
    host.set_binary(1000, 0o755);
    assert!(host.check_binary().is_err());
}

/// Catches the real host judging some other file than the running program, or only the file
/// and not its directories: its verdict is the verdict an independent walk gives on the
/// canonical path of this test binary, owner and group/other write bits of every name from the
/// file up to `/` (a binary under a user's target directory is refused when the tests run as
/// that user).
#[test]
fn the_real_host_judges_the_running_binary_and_its_directories() {
    let exe = std::fs::canonicalize(std::env::current_exe().unwrap()).unwrap();
    let want = exe.ancestors().all(|p| {
        let m = std::fs::symlink_metadata(p).unwrap();
        m.uid() == 0 && m.mode() & 0o022 == 0
    });
    let got = RealHost::new().check_binary();
    assert_eq!(got.is_ok(), want, "{got:?}");
    if let Err(e) = got {
        assert!(e.to_string().contains(" is "), "{e}");
    }
}

const TABLE: &str = "  sl  local_address rem_address   st tx_queue rx_queue tr tm->when retrnsmt   uid  timeout inode
   0: 0100007F:2329 00000000:0000 0A 00000000:00000000 00:00000000 00000000  1000        0 11111 1 0000000000000000 100 0 0 10 0
   1: 00000000:2329 00000000:0000 0A 00000000:00000000 00:00000000 00000000     0        0 22222 1 0000000000000000 100 0 0 10 0
   2: 0100007F:2329 0100007F:C350 01 00000000:00000000 00:00000000 00000000  1001        0 33333 1 0000000000000000 100 0 0 10 0
   3: 0100007F:232A 00000000:0000 0A 00000000:00000000 00:00000000 00000000  1002        0 44444 1 0000000000000000 100 0 0 10 0
";

/// Catches a table reader that takes a connection for a listener (state 01), a listener on
/// another port, a port read in decimal, or a damaged line for a socket: only state `0A` lines
/// whose local port (hex) is the one asked for count, and each gives its owner uid.
#[test]
fn listeners_are_read_from_the_socket_table() {
    assert_eq!(parse_listeners(TABLE, 0x2329), [1000, 0]);
    assert_eq!(parse_listeners(TABLE, 0x232A), [1002]);
    assert!(parse_listeners(TABLE, 0x2328).is_empty());
    assert!(parse_listeners("", 1).is_empty());
    let damaged = "header\nnonsense\n0: nocolon 0 0A 0 0 0 1000\n0: 0100007F:2329 0 0A 0 0 0\n0: 0100007F:2329 0 0A 0 0 0 x\n0: 0100007F:zz 0 0A 0 0 0 1000\n";
    assert!(parse_listeners(damaged, 0x2329).is_empty());
}

/// Catches the fake's listeners leaking across ports.
#[test]
fn the_fake_host_lists_listeners_by_port() {
    let host = FakeHost::new(0);
    host.add_listener(9001, 900);
    host.add_listener(9001, 1000);
    host.add_listener(9002, 0);
    assert_eq!(host.listener_uids(9001).unwrap(), [900, 1000]);
    assert!(host.listener_uids(9003).unwrap().is_empty());
}

/// Catches the real host not finding a listener on the machine, or finding it under another
/// owner: a socket this test listens on (IPv4 loopback, and IPv6 loopback where the machine has
/// it) is reported with this process's uid, a UDP socket on a port is not a listener, and a
/// port nobody listens on has none.
#[test]
fn the_real_host_finds_who_listens_on_a_port() {
    let me = rustix::process::geteuid().as_raw();
    let v4 = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = v4.local_addr().unwrap().port();
    assert_eq!(RealHost::new().listener_uids(port).unwrap(), [me]);
    if let Ok(v6) = TcpListener::bind("[::1]:0") {
        let port = v6.local_addr().unwrap().port();
        assert_eq!(RealHost::new().listener_uids(port).unwrap(), [me]);
    }
    let udp = UdpSocket::bind("127.0.0.1:0").unwrap();
    let udp_port = udp.local_addr().unwrap().port();
    assert!(RealHost::new().listener_uids(udp_port).unwrap().is_empty());
    drop(v4);
}
