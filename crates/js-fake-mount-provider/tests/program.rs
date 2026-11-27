//! The program as a test starts it: its usage error, and a start that serves until `quit`.

use std::io::{Read as _, Write as _};
use std::os::unix::net::UnixStream;
use std::process::{Command, Stdio};
use std::time::Duration;

use js_testkit::dir::TestDir;

/// The program under test.
const PROGRAM: &str = env!("CARGO_BIN_EXE_js-fake-mount-provider");

/// Asserts a usage error: status 2 and the reason on standard error, in full.
#[test]
fn a_usage_error_ends_with_status_2() {
    let output = Command::new(PROGRAM)
        .arg("--socket")
        .output()
        .expect("the program runs");
    assert_eq!(output.status.code(), Some(2));
    assert_eq!(
        String::from_utf8_lossy(&output.stderr),
        "js-fake-mount-provider: --socket needs a value\n"
    );
}

/// Asserts a start: the program prints its socket once it serves, answers its control socket,
/// and ends with status 0 after `quit`.
#[test]
fn the_program_serves_until_quit() {
    let dir = TestDir::under(std::env::temp_dir(), "jsfmp-program");
    let socket = dir.join("p.sock");
    let control = dir.join("c.sock");
    let mut child = Command::new(PROGRAM)
        .arg("--socket")
        .arg(&socket)
        .arg("--control")
        .arg(&control)
        .arg("--root")
        .arg(dir.join("trees"))
        .stdout(Stdio::piped())
        .spawn()
        .expect("the program starts");
    let mut line = [0u8; 8];
    child
        .stdout
        .take()
        .expect("its standard output")
        .read_exact(&mut line)
        .expect("its first line");
    assert_eq!(&line, b"serving ");
    let tell = |command: &str| {
        let mut stream = UnixStream::connect(&control).expect("the control socket");
        stream
            .set_read_timeout(Some(Duration::from_secs(30)))
            .expect("a read limit");
        stream
            .write_all(format!("{command}\n").as_bytes())
            .expect("the command");
        let mut reply = String::new();
        stream.read_to_string(&mut reply).expect("the reply");
        reply
    };
    assert_eq!(tell("calls"), "");
    assert_eq!(tell("quit"), "ok\n");
    let status = child.wait().expect("the program ends");
    assert_eq!(status.code(), Some(0));
    assert!(!socket.exists());
}
