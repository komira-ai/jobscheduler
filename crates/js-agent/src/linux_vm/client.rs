//! The Mac side's clients of a running VM: one request to the owner's control socket
//! (`ctl`), timed requests to the guest program through it (`call`), and a timed program
//! (`time`, for the stock channel: `ssh`).
//!
//! Each prints one line per sample and a last line with the summary of the round trips
//! ([`super::wire::Summary`]). A round trip is measured with the monotonic clock from the
//! connect to the control socket to the reply line read.

use std::io::{BufRead as _, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::path::Path;

use js_mac_session::disclaim::Program;

use super::clock;
use super::control::{self, Control};
use super::wire::{self, summarise};

/// Sends `line` to the control socket in `dir` and returns the owner's answer.
///
/// # Errors
///
/// The socket cannot be reached, or the owner refused.
pub fn ctl(dir: &Path, line: &str) -> Result<String, String> {
    let request = control::parse(line)?;
    let (_, answer) = ask(dir, request)?;
    control::answer(&answer)
}

/// Opens the control socket in `dir`, sends `request`, and returns the stream and the
/// answer line.
fn ask(dir: &Path, request: Control) -> Result<(UnixStream, String), String> {
    let path = dir.join(control::SOCKET);
    let shown = path.display();
    let mut stream = at(&format!("connect {shown}"), UnixStream::connect(&path))?;
    let line = control::render(request);
    at(&format!("write to {shown}"), writeln!(stream, "{line}"))?;
    let mut answer = String::new();
    let read = BufReader::new(&stream).read_line(&mut answer);
    at(&format!("read from {shown}"), read)?;
    Ok((stream, answer))
}

/// `result`, an I/O error turned into text that names `what`.
fn at<T>(what: &str, result: std::io::Result<T>) -> Result<T, String> {
    result.map_err(|why| format!("{what}: {why}"))
}

/// One request to the guest's `port`: the reply line, as the guest wrote it.
///
/// # Errors
///
/// The owner refused the connection, or the guest closed it without a reply.
pub fn request(dir: &Path, port: u32, line: &str) -> Result<String, String> {
    let (mut stream, answer) = ask(dir, Control::Connect(port))?;
    control::answer(&answer).map_err(|why| format!("connect {port}: {why}"))?;
    at(&format!("send to port {port}"), writeln!(stream, "{line}"))?;
    let mut reply = String::new();
    let read = BufReader::new(&stream).read_line(&mut reply);
    at(&format!("read from port {port}"), read)?;
    if reply.is_empty() {
        return Err(format!("port {port} closed without a reply"));
    }
    Ok(reply.trim_end().to_owned())
}

/// `call`: `samples` requests, each on a new connection, `{n}` in `template` replaced by the
/// sample's number. Prints each sample and the summary to `out`; returns how many failed.
///
/// # Errors
///
/// `out` cannot be written.
pub fn call(
    dir: &Path,
    port: u32,
    template: &str,
    samples: u32,
    out: &mut dyn Write,
) -> std::io::Result<u32> {
    let mut taken = Vec::new();
    let mut failed = 0;
    for n in 0..samples {
        let line = template.replace("{n}", &n.to_string());
        let began = clock::now();
        let result = request(dir, port, &line);
        let us = clock::micros_since(began);
        match result {
            Ok(line) => match wire::parse_reply(&line) {
                Ok(reply) if reply.ok => {
                    taken.push(us);
                    writeln!(out, "sample {n} us={us} reply={line}")?;
                }
                _ => {
                    failed += 1;
                    writeln!(out, "sample {n} FAILED reply={line}")?;
                }
            },
            Err(why) => {
                failed += 1;
                writeln!(out, "sample {n} FAILED {why}")?;
            }
        }
    }
    summary(&taken, failed, out)?;
    Ok(failed)
}

/// `time`: runs `program args` `samples` times and prints each wall time and the summary;
/// returns how many runs failed (did not start or exited non-zero).
///
/// # Errors
///
/// `out` cannot be written.
pub fn time(
    program: &str,
    args: &[String],
    samples: u32,
    out: &mut dyn Write,
) -> std::io::Result<u32> {
    let mut taken = Vec::new();
    let mut failed = 0;
    for n in 0..samples {
        let began = clock::now();
        let ran = Program::new(program, args).output();
        let us = clock::micros_since(began);
        match ran {
            Ok(output) if output.status.success() => {
                taken.push(us);
                let shown = wire::tail(&output.stdout, 200);
                writeln!(out, "sample {n} us={us} stdout={shown:?}")?;
            }
            Ok(output) => {
                failed += 1;
                let (status, shown) = (output.status, wire::tail(&output.stderr, 400));
                writeln!(out, "sample {n} FAILED {status} stderr={shown:?}")?;
            }
            Err(why) => {
                failed += 1;
                writeln!(out, "sample {n} FAILED cannot start {program}: {why}")?;
            }
        }
    }
    summary(&taken, failed, out)?;
    Ok(failed)
}

/// The last line: the summary of the samples that succeeded and the count that failed.
fn summary(taken: &[u64], failed: u32, out: &mut dyn Write) -> std::io::Result<()> {
    match summarise(taken) {
        Some(s) => writeln!(out, "summary {s} failed={failed}"),
        None => writeln!(out, "summary n=0 failed={failed}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use js_testkit::dir::TestDir;
    use std::os::unix::net::UnixListener;

    /// A stand-in owner on `dir`'s control socket that answers `connections` connections:
    /// for each it reads the request line and answers with `answer`; after an `ok` to a
    /// connect it reads one more line and writes `reply` (nothing when `reply` is empty).
    fn owner(dir: &Path, connections: usize, answer: &'static str, reply: &'static str) {
        let listener = UnixListener::bind(dir.join(control::SOCKET)).unwrap();
        std::thread::spawn(move || {
            for stream in listener.incoming().take(connections) {
                let mut stream = stream.unwrap();
                let mut reader = BufReader::new(stream.try_clone().unwrap());
                let mut line = String::new();
                reader.read_line(&mut line).unwrap();
                stream.write_all(answer.as_bytes()).unwrap();
                if answer.starts_with("ok") && line.starts_with("connect") {
                    line.clear();
                    reader.read_line(&mut line).unwrap();
                    stream.write_all(reply.as_bytes()).unwrap();
                }
            }
        });
    }

    /// Asserts that `ctl` returns the owner's `ok` rest, refuses a malformed line before it
    /// connects, says when no owner listens, and passes an `err` on. Catches a client that
    /// reports success when nothing answered.
    #[test]
    fn ctl_returns_the_answer_or_why_not() {
        let dir = TestDir::under(std::env::temp_dir(), "linux-vm-ctl");
        assert_eq!(
            ctl(dir.path(), "bogus"),
            Err("unknown request \"bogus\"".into())
        );
        let missing = ctl(dir.path(), "state").unwrap_err();
        assert!(missing.starts_with("connect "), "{missing}");
        owner(dir.path(), 1, "ok running 8192\n", "");
        assert_eq!(ctl(dir.path(), "state"), Ok("running 8192".into()));

        let refused = TestDir::under(std::env::temp_dir(), "linux-vm-ctl-err");
        owner(refused.path(), 1, "err no balloon\n", "");
        assert_eq!(ctl(refused.path(), "balloon 1"), Err("no balloon".into()));
    }

    /// Asserts that `call` counts good replies, numbers each sample's request, and counts a
    /// refused reply, a refused connect and a silent guest as failures, each on its line.
    /// Catches failures counted in the summary as samples.
    #[test]
    fn call_counts_replies_and_failures() {
        let good = TestDir::under(std::env::temp_dir(), "linux-vm-call-good");
        owner(good.path(), 3, "ok\n", "{\"ok\":true,\"guest_us\":5}\n");
        let mut out = Vec::new();
        let failed = call(good.path(), 7000, r#"{"op":"ping","n":{n}}"#, 3, &mut out).unwrap();
        let text = String::from_utf8(out).unwrap();
        assert_eq!(failed, 0, "{text}");
        assert_eq!(
            text.lines().filter(|l| l.contains(" us=")).count(),
            3,
            "{text}"
        );
        let last = text.lines().last().unwrap();
        assert!(last.starts_with("summary n=3 median="), "{last}");
        assert!(last.ends_with(" failed=0"), "{last}");

        let refused = TestDir::under(std::env::temp_dir(), "linux-vm-call-refused");
        owner(
            refused.path(),
            1,
            "ok\n",
            "{\"ok\":false,\"guest_us\":0,\"error\":\"x\"}\n",
        );
        let mut out = Vec::new();
        assert_eq!(call(refused.path(), 7000, "{}", 1, &mut out).unwrap(), 1);
        let text = String::from_utf8(out).unwrap();
        assert_eq!(
            text,
            "sample 0 FAILED reply={\"ok\":false,\"guest_us\":0,\"error\":\"x\"}\n\
             summary n=0 failed=1\n"
        );

        let no_port = TestDir::under(std::env::temp_dir(), "linux-vm-call-noport");
        owner(no_port.path(), 1, "err nobody listens\n", "");
        let mut out = Vec::new();
        assert_eq!(call(no_port.path(), 7000, "{}", 1, &mut out).unwrap(), 1);
        assert_eq!(
            String::from_utf8(out).unwrap(),
            "sample 0 FAILED connect 7000: nobody listens\nsummary n=0 failed=1\n"
        );

        let silent = TestDir::under(std::env::temp_dir(), "linux-vm-call-silent");
        owner(silent.path(), 1, "ok\n", "");
        assert_eq!(
            request(silent.path(), 7000, "{}"),
            Err("port 7000 closed without a reply".into())
        );
    }

    /// Asserts that `time` counts a program that exits 0, one that exits non-zero, and one
    /// that cannot start. Catches a failed run whose time lands in the summary.
    #[test]
    fn time_counts_runs_that_fail() {
        // This test program itself: with a filter that matches no test it exits 0, with an
        // unknown flag it exits non-zero.
        let me = std::env::current_exe().unwrap();
        let me = me.to_str().unwrap();
        let mut out = Vec::new();
        let none = ["--exact".to_owned(), "no-such-test".to_owned()];
        assert_eq!(time(me, &none, 2, &mut out).unwrap(), 0);
        let text = String::from_utf8(out).unwrap();
        assert!(
            text.lines().last().unwrap().starts_with("summary n=2 "),
            "{text}"
        );

        let mut out = Vec::new();
        assert_eq!(
            time(me, &["--no-such-flag".to_owned()], 1, &mut out).unwrap(),
            1
        );
        let text = String::from_utf8(out).unwrap();
        assert!(text.starts_with("sample 0 FAILED exit status: "), "{text}");
        assert!(text.ends_with("summary n=0 failed=1\n"), "{text}");

        let dir = TestDir::under(std::env::temp_dir(), "linux-vm-time-missing");
        let missing = dir.path().join("no-such-program");
        let missing = missing.to_str().unwrap();
        let mut out = Vec::new();
        assert_eq!(time(missing, &[], 1, &mut out).unwrap(), 1);
        let text = String::from_utf8(out).unwrap();
        assert!(
            text.starts_with(&format!("sample 0 FAILED cannot start {missing}: ")),
            "{text}"
        );
    }
}
