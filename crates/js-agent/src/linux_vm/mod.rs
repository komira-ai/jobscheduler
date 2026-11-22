//! `js-agent __linux-vm`: a development verb of the prototype that runs Linux containers on a
//! Mac in one long-running Linux VM the agent launches itself, with nothing of the scheduler
//! in the guest. It is for measuring the unknowns of that shape and is removed before any
//! release.
//!
//! - `run` boots the VM through Apple's Virtualization framework and owns it in this
//!   process (or, with `--detach`, in a new process of this program, in a process group of
//!   its own, so that it outlives the process that started it). The owner serves a control
//!   socket in the VM's directory ([`control`]) and carries the guest's connections on the
//!   forwarded ports to TCP addresses on the Mac. macOS only (`vm`).
//! - `adopt` is the agent's side of a VM that outlives it: it uses the VM a live owner holds
//!   in the directory, or boots one detached, then stays until its process is ended.
//! - `ctl`, `call` and `time` are clients ([`client`]): a control request; timed requests to
//!   the guest program over the virtio socket ([`wire`]); a timed program (the stock channel,
//!   `ssh`).
//! - `place` places one input tree through a mount provider on the Mac (tierstore's) with the
//!   agent's own provider client ([`place`]), for the tree routes.
//!
//! The VM's directory holds `disk.raw` (the guest's disk), an optional `seed.iso` (the
//! cloud-init seed, attached read-only), and what the owner makes on the first boot and
//! keeps: `efi-vars` (the firmware's variables), `machine-id` and `mac` (the VM's identity
//! and its network address, so the guest keeps its address across boots), `console.log` (the
//! guest's serial console), `ready` (the guest's last ready line), `owner.pid` and
//! `control.sock`.

use std::ffi::OsString;
use std::io::Write;
use std::process::ExitCode;

pub mod client;
pub mod clock;
pub mod control;
pub mod flags;
pub mod place;
#[cfg(target_os = "macos")]
mod vm;
pub mod wire;

/// The verb.
pub const VERB: &str = "__linux-vm";

/// The status of a refused command line.
const USAGE: u8 = 2;

/// Runs the verb: `args` is the whole command line, `args[1]` the verb.
pub fn run(args: &[OsString], out: &mut dyn Write, err: &mut dyn Write) -> ExitCode {
    let words: Option<Vec<String>> = args
        .get(2..)
        .unwrap_or_default()
        .iter()
        .map(|a| a.to_str().map(str::to_owned))
        .collect();
    let Some(words) = words else {
        let _ = writeln!(err, "js-agent {VERB}: an argument is not UTF-8");
        return ExitCode::from(USAGE);
    };
    let command = match flags::parse(&words) {
        Ok(command) => command,
        Err(why) => {
            let _ = writeln!(err, "js-agent {VERB}: {why}");
            return ExitCode::from(USAGE);
        }
    };
    match command {
        flags::Command::Ctl { dir, line } => match client::ctl(&dir, &line) {
            Ok(answer) => {
                let _ = writeln!(out, "ok {answer}");
                ExitCode::SUCCESS
            }
            Err(why) => {
                let _ = writeln!(err, "js-agent {VERB} ctl: {why}");
                ExitCode::FAILURE
            }
        },
        flags::Command::Call {
            dir,
            port,
            request,
            samples,
        } => counted(client::call(&dir, port, &request, samples, out), err),
        flags::Command::Time {
            samples,
            program,
            args,
        } => counted(client::time(&program, &args, samples, out), err),
        flags::Command::Place(place) => match place::run(&place, out) {
            Ok(()) => ExitCode::SUCCESS,
            Err(why) => {
                let _ = writeln!(err, "js-agent {VERB} place: {why}");
                ExitCode::FAILURE
            }
        },
        flags::Command::Run(flags) => owner(&flags, false, out, err),
        flags::Command::Adopt(flags) => owner(&flags, true, out, err),
    }
}

/// The status of a client that counted failed samples.
fn counted(result: std::io::Result<u32>, err: &mut dyn Write) -> ExitCode {
    match result {
        Ok(0) => ExitCode::SUCCESS,
        Ok(_) => ExitCode::FAILURE,
        Err(why) => {
            let _ = writeln!(err, "js-agent {VERB}: write the output: {why}");
            ExitCode::FAILURE
        }
    }
}

/// `run` and `adopt`: the VM's owner, on a Mac.
#[cfg(target_os = "macos")]
fn owner(
    flags: &flags::RunFlags,
    adopt: bool,
    out: &mut dyn Write,
    err: &mut dyn Write,
) -> ExitCode {
    let result = if adopt {
        vm::adopt(flags, out)
    } else {
        vm::run(flags, out)
    };
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(why) => {
            let _ = writeln!(err, "js-agent {VERB}: {why}");
            ExitCode::FAILURE
        }
    }
}

/// `run` and `adopt` need Apple's Virtualization framework.
#[cfg(not(target_os = "macos"))]
fn owner(
    _flags: &flags::RunFlags,
    _adopt: bool,
    _out: &mut dyn Write,
    err: &mut dyn Write,
) -> ExitCode {
    let _ = writeln!(err, "js-agent {VERB}: run and adopt are macOS only");
    ExitCode::from(USAGE)
}

#[cfg(test)]
mod tests {
    use super::*;
    use js_testkit::dir::TestDir;

    fn line(words: &[&str]) -> Vec<OsString> {
        ["js-agent", VERB]
            .iter()
            .chain(words)
            .map(OsString::from)
            .collect()
    }

    fn verb(words: &[&str]) -> (ExitCode, String, String) {
        let (mut out, mut err) = (Vec::new(), Vec::new());
        let code = run(&line(words), &mut out, &mut err);
        (
            code,
            String::from_utf8(out).unwrap(),
            String::from_utf8(err).unwrap(),
        )
    }

    /// Asserts that a malformed line is refused with status 2 and the parser's reason, and
    /// that a `ctl` or `call` with no owner listening fails with status 1 and says so.
    /// Catches a verb that exits 0 when it did nothing.
    #[test]
    fn refusals_and_failures_have_their_status() {
        let (code, out, err) = verb(&["bogus"]);
        assert_eq!(code, ExitCode::from(USAGE));
        assert_eq!(
            (out.as_str(), err.as_str()),
            ("", "js-agent __linux-vm: unknown command \"bogus\"\n")
        );

        let dir = TestDir::under(std::env::temp_dir(), "linux-vm-verb");
        let d = dir.path().to_str().unwrap();
        let (code, _, err) = verb(&["ctl", "--dir", d, "state"]);
        assert_eq!(code, ExitCode::FAILURE);
        assert!(
            err.starts_with("js-agent __linux-vm ctl: connect "),
            "{err}"
        );

        let (code, out, _) = verb(&["call", "--dir", d, "--port", "7000", "--request", "{}"]);
        assert_eq!(code, ExitCode::FAILURE);
        assert!(out.ends_with("summary n=0 failed=1\n"), "{out}");
    }

    /// A writer that refuses every write.
    struct Refusing;

    impl Write for Refusing {
        fn write(&mut self, _: &[u8]) -> std::io::Result<usize> {
            Err(std::io::Error::other("refused"))
        }

        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    /// Asserts that an argument that is not UTF-8 is refused with status 2, and that a client
    /// whose output cannot be written fails and says so. Catches a verb that runs with a
    /// mangled argument or hides a lost report.
    #[test]
    fn a_bad_argument_and_a_lost_output_fail() {
        use std::os::unix::ffi::OsStringExt as _;
        let mut args = line(&["ctl"]);
        args.push(OsString::from_vec(vec![0xff]));
        let (mut out, mut err) = (Vec::new(), Vec::new());
        assert_eq!(run(&args, &mut out, &mut err), ExitCode::from(USAGE));
        assert_eq!(
            String::from_utf8(err).unwrap(),
            "js-agent __linux-vm: an argument is not UTF-8\n"
        );
        let me = std::env::current_exe().unwrap();
        let args = line(&[
            "time",
            "--",
            me.to_str().unwrap(),
            "--exact",
            "no-such-test",
        ]);
        let mut err = Vec::new();
        assert_eq!(run(&args, &mut Refusing, &mut err), ExitCode::FAILURE);
        assert_eq!(
            String::from_utf8(err).unwrap(),
            "js-agent __linux-vm: write the output: refused\n"
        );
    }

    /// Asserts that `place` prints the directory a provider answers and exits 0, and that a
    /// refused ref exits 1 saying why. Catches a place whose failure reads as success.
    #[test]
    fn place_prints_the_directory_or_fails() {
        use std::os::unix::fs::PermissionsExt as _;
        let dir = TestDir::under(std::env::temp_dir(), "linux-vm-verb-place");
        let fake = js_testkit::mount_provider::FakeProvider::start(dir.path()).unwrap();
        std::fs::set_permissions(fake.socket(), std::fs::Permissions::from_mode(0o600)).unwrap();
        let socket = fake.socket().to_str().unwrap().to_owned();
        let uid = rustix::process::geteuid().as_raw().to_string();
        let words = [
            "place", "--socket", &socket, "--uid", &uid, "--ref", "cas/l/ab", "--run", "v1",
        ];
        let (code, out, _) = verb(&words);
        assert_eq!(code, ExitCode::SUCCESS);
        assert_eq!(out, "/trees/linux-vm-poc/js.v1/0\n");
        fake.refuse("cas/l/no");
        let words = [
            "place", "--socket", &socket, "--uid", &uid, "--ref", "cas/l/no", "--run", "v2",
        ];
        let (code, _, err) = verb(&words);
        assert_eq!(code, ExitCode::FAILURE);
        assert!(
            err.starts_with("js-agent __linux-vm place: refused: "),
            "{err}"
        );
    }

    /// Asserts that `ctl` prints the owner's answer after `ok` and exits 0.
    #[test]
    fn ctl_prints_the_answer() {
        use std::io::{BufRead as _, BufReader};
        let dir = TestDir::under(std::env::temp_dir(), "linux-vm-verb-ctl");
        let listener =
            std::os::unix::net::UnixListener::bind(dir.path().join(control::SOCKET)).unwrap();
        std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut line = String::new();
            BufReader::new(stream.try_clone().unwrap())
                .read_line(&mut line)
                .unwrap();
            stream.write_all(b"ok running active=0\n").unwrap();
        });
        let (code, out, _) = verb(&["ctl", "--dir", dir.path().to_str().unwrap(), "state"]);
        assert_eq!(
            (code, out.as_str()),
            (ExitCode::SUCCESS, "ok running active=0\n")
        );
    }

    /// Asserts that `time` of a program that exits 0 succeeds and prints its summary.
    #[test]
    fn a_timed_program_that_succeeds_exits_zero() {
        let me = std::env::current_exe().unwrap();
        let (code, out, _) = verb(&[
            "time",
            "--",
            me.to_str().unwrap(),
            "--exact",
            "no-such-test",
        ]);
        assert_eq!(code, ExitCode::SUCCESS, "{out}");
        assert!(out.contains("summary n=1 "), "{out}");
    }

    /// Asserts that `run` and `adopt` refuse on a platform without the Virtualization
    /// framework, by name. Catches a verb that reports a VM it could not boot.
    #[cfg(not(target_os = "macos"))]
    #[test]
    fn the_owner_is_macos_only() {
        for command in ["run", "adopt"] {
            let (code, _, err) = verb(&[command, "--dir", "/nonexistent"]);
            assert_eq!(code, ExitCode::from(USAGE));
            assert_eq!(err, "js-agent __linux-vm: run and adopt are macOS only\n");
        }
    }
}
