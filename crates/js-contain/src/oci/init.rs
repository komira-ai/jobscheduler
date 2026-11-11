//! The first process of a job's container (`init/init.rs`), embedded in this
//! crate: how it is put on the node and where the container sees it.
//!
//! The binary is built statically by `build.rs`, so it runs in any image. The driver
//! writes it to `<scratch>/oci-bin/js-init` (the scratch directory is searchable by
//! others; an installed or built daemon's directory may not be) and binds it read-only
//! at [`super::OciConfig::init_dest`], in the container's own `/dev` (a `tmpfs` made fresh per container, so
//! the image's root is never written to).

use std::io::Write as _;
use std::path::{Path, PathBuf};

use super::install::{WhileOpen, nothing_while_open};

/// The program, empty on a target that runs no containers.
const BINARY: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/js-init"));

/// Where the container sees it unless the caller says otherwise
/// ([`super::OciConfig::init_dest`]).
pub const DEFAULT_DEST: &str = "/dev/js-init";

/// The file in a lease directory the init writes why the program could not be started
/// to (its stdin: the container's other processes cannot reach it).
pub(crate) fn report_path(lease: &Path) -> PathBuf {
    lease.join("init-report")
}

/// Makes the report file of the lease in `lease`, empty, for the init to write to.
pub(crate) fn create_report(lease: &Path) -> Result<std::fs::File, String> {
    let path = report_path(lease);
    std::fs::File::create(&path).map_err(|e| format!("{}: {e}", path.display()))
}

/// Writes the program into `<scratch>/oci-bin/js-init` (mode 0755, replaced
/// atomically) and returns that path. It is written on a thread with a descriptor table of
/// its own, so no forked child holds it busy (`super::install`).
pub(crate) fn install(scratch: &Path) -> std::io::Result<PathBuf> {
    install_with(scratch, &mut nothing_while_open)
}

/// [`install`], running `while_open` on the writing thread once the program is written and
/// before its file is closed: the seam a test forks through.
pub(crate) fn install_with(
    scratch: &Path,
    while_open: &mut WhileOpen<'_>,
) -> std::io::Result<PathBuf> {
    install_bytes(scratch, BINARY, while_open)
}

fn install_bytes(
    scratch: &Path,
    bytes: &[u8],
    while_open: &mut WhileOpen<'_>,
) -> std::io::Result<PathBuf> {
    if bytes.is_empty() {
        return Err(std::io::Error::other(
            "this build has no container init: containers run on Linux only",
        ));
    }
    super::install::publish(scratch, "js-init", &mut |file| {
        file.write_all(bytes)?;
        while_open();
        Ok(())
    })
}

#[cfg(test)]
mod tests {
    use std::os::unix::fs::PermissionsExt as _;

    use super::*;

    fn scratch(name: &str) -> PathBuf {
        let dir = std::env::current_exe()
            .expect("test binary")
            .parent()
            .expect("deps directory")
            .join("js-contain-unit")
            .join(format!("init-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("scratch");
        dir
    }

    /// Catches an init the container's user cannot execute (a mode without the others'
    /// bits: the job's ids are not the daemon's), one that is not the bytes built, a
    /// second install failing on the first's file, and a build without an init (a target
    /// that runs no containers) installing an empty file the container would fail to
    /// start on.
    #[test]
    fn the_init_is_installed_executable_by_others_and_replaced_atomically() {
        let scratch = scratch("install");
        let first = install(&scratch).expect("install");
        assert_eq!(first, scratch.join("oci-bin/js-init"));
        let mode = |p: &Path| std::fs::metadata(p).expect("meta").permissions().mode() & 0o777;
        assert_eq!(mode(&first), 0o755);
        assert_eq!(mode(first.parent().expect("dir")), 0o755);
        assert_eq!(std::fs::read(&first).expect("read"), BINARY);
        assert!(!BINARY.is_empty());
        assert_eq!(install(&scratch).expect("again"), first);
        let names: Vec<_> = std::fs::read_dir(first.parent().expect("dir"))
            .expect("dir")
            .map(|e| e.expect("entry").file_name())
            .collect();
        assert_eq!(names, ["js-init"], "no staged file left");
        // The report file: empty each time, and an error naming the file when the lease
        // directory is not there.
        let lease = scratch.join("lease");
        std::fs::create_dir(&lease).expect("lease");
        std::fs::write(report_path(&lease), b"left over").expect("old report");
        create_report(&lease).expect("report");
        assert_eq!(std::fs::read(report_path(&lease)).expect("read"), b"");
        let why = create_report(&scratch.join("gone")).expect_err("no lease directory");
        assert!(why.contains("gone/init-report: "), "{why}");
        let empty = install_bytes(&scratch, b"", &mut nothing_while_open).expect_err("empty");
        assert!(empty.to_string().contains("no container init"), "{empty}");
    }

    /// What `/proc/<pid>/stat` says of a process: its state, its session and when it started
    /// (clock ticks since boot). Exec changes none of what the test reads from it: a process
    /// is alive (not a zombie) and in its session from the fork on, before the shell's forked
    /// child has become `sleep` as after, and its start time is that of the fork.
    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    struct Stat {
        state: char,
        session: u64,
        start: u64,
    }

    /// The stat of `pid`, or `None` when there is no such process. The fields follow the
    /// last `)`, since a command name may hold one.
    fn stat(pid: i32) -> Option<Stat> {
        let text = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
        let (_, after) = text.rsplit_once(')')?;
        let fields: Vec<&str> = after.split_whitespace().collect();
        Some(Stat {
            state: fields.first()?.chars().next()?,
            session: fields.get(3)?.parse().ok()?,
            start: fields.get(19)?.parse().ok()?,
        })
    }

    /// A process the init's program left, known by its pid and by the init it ran under. A
    /// process in another session than the init's, or one that started before the init, is
    /// not it. A pid the kernel gave to a later process of the same session would still pass;
    /// the test does not guard against that.
    struct Leftover {
        pid: i32,
        init: Stat,
    }

    impl Leftover {
        /// Whether the process is still running: there, not a zombie, and not ruled out as
        /// another process by its session or start time. Whether it has exec'd yet does not
        /// matter.
        fn running(&self) -> bool {
            stat(self.pid).is_some_and(|now| {
                !matches!(now.state, 'Z' | 'X')
                    && now.session == self.init.session
                    && now.start >= self.init.start
            })
        }
    }

    /// The processes a run left, killed (SIGKILL) when this is dropped, on every path out of
    /// the test, a failed assertion's unwinding too, so none outlives it holding the test's
    /// output open.
    #[derive(Default)]
    struct Leftovers(Vec<Leftover>);

    impl Drop for Leftovers {
        fn drop(&mut self) {
            for left in &self.0 {
                let pid = rustix::process::Pid::from_raw(left.pid);
                if let Some(pid) = pid.filter(|_| left.running()) {
                    let _ = rustix::process::kill_process(pid, rustix::process::Signal::KILL);
                }
            }
        }
    }

    /// Proves what counts as a running leftover: a live process whose program is not
    /// `sleep` (this test's own) does; a process in another session than the init's, or one
    /// that started before the init, does not; nor does one that has ended, whether still a
    /// zombie or reaped. Catches a check that would call a gone process running, or a live
    /// one gone.
    #[test]
    fn a_leftover_runs_only_while_alive_in_the_inits_session_and_no_older_than_it() {
        let me = i32::try_from(std::process::id()).expect("a pid");
        let mine = stat(me).expect("this test's stat");
        let left = |pid, init| Leftover { pid, init };
        assert!(left(me, mine).running());
        let other_session = Stat {
            session: mine.session + 1,
            ..mine
        };
        assert!(!left(me, other_session).running());
        let started_later = Stat {
            start: mine.start + 1,
            ..mine
        };
        assert!(!left(me, started_later).running());

        let mut ended = std::process::Command::new("/bin/sh")
            .args(["-c", "exit 0"])
            .stdin(std::process::Stdio::null())
            .spawn()
            .expect("a child");
        let pid = i32::try_from(ended.id()).expect("a pid");
        let at_start = stat(pid).expect("the child's stat");
        // Waits for the child to exit without reaping it, so it is a zombie until the wait
        // below.
        let raw = rustix::process::Pid::from_raw(pid).expect("a positive pid");
        rustix::process::waitid(
            rustix::process::WaitId::Pid(raw),
            rustix::process::WaitIdOptions::EXITED | rustix::process::WaitIdOptions::NOWAIT,
        )
        .expect("the child exits");
        assert_eq!(stat(pid).map(|now| now.state), Some('Z'));
        assert!(!left(pid, at_start).running(), "a zombie is not running");
        ended.wait().expect("reaped");
        assert_eq!(stat(pid), None);
        assert!(
            !left(pid, at_start).running(),
            "a reaped process is not running"
        );
    }

    /// Runs the installed init with `options`, over a shell that leaves three processes,
    /// writing their pids to `dir`, and exits 3: a forked child that never execs (it blocks
    /// opening a FIFO no one writes to, before its `exec sleep`), a background `sleep` child,
    /// and a `sleep` grandchild whose parent has gone. Returns the init's exit code and the
    /// three processes, held to be killed.
    fn run_leaving_three(init: &Path, dir: &Path, options: &[&str]) -> (Option<i32>, Leftovers) {
        let script = format!(
            "rm -f {d}/hold && mkfifo {d}/hold || exit 99; \
             {{ read x < {d}/hold; exec sleep 100000; }} & echo $! > {d}/held; \
             sleep 100000 & echo $! > {d}/child; \
             (sleep 100000 & echo $! > {d}/grandchild); exit 3",
            d = dir.display()
        );
        let mut running = std::process::Command::new(init)
            .args(options)
            .args(["--", "/bin/sh", "-c", &script])
            .stdin(std::process::Stdio::null())
            .spawn()
            .expect("the init runs");
        // The init is this test's child and is not reaped until the wait below, so its stat
        // is there to read even if it has already exited.
        let pid = i32::try_from(running.id()).expect("a pid");
        let init_stat = stat(pid).expect("the init's stat");
        let status = running.wait().expect("the init ends");
        let mut left = Leftovers::default();
        for name in ["held", "child", "grandchild"] {
            let text = std::fs::read_to_string(dir.join(name)).expect("a pid");
            let pid = text.trim().parse().expect("a pid");
            left.0.push(Leftover {
                pid,
                init: init_stat,
            });
        }
        (status.code(), left)
    }

    /// Proves the init of a host container (`--end-descendants`) ends with its program's
    /// code only once everything the program left is gone: a child that has not exec'd, a
    /// background child and a grandchild whose parent had exited are all killed and reaped
    /// (no such process is left) by the time it exits. Without the option (a contained job's
    /// init, whose PID namespace the kernel ends) all three are left running. Catches a host
    /// job whose leftover processes keep `crun run`, and the job, from ever ending. A
    /// leftover counts as running from the fork on, whether or not it has exec'd, so neither
    /// half depends on how far the shell's children got.
    #[test]
    fn a_host_init_ends_what_its_program_left_before_it_exits() {
        let dir = scratch("end-descendants");
        let init = install(&dir).expect("install");
        let (code, left) = run_leaving_three(&init, &dir, &["--end-descendants"]);
        assert_eq!(code, Some(3));
        for process in &left.0 {
            assert_eq!(
                stat(process.pid),
                None,
                "process {} outlived the init",
                process.pid
            );
        }
        let (code, left) = run_leaving_three(&init, &dir, &[]);
        assert_eq!(code, Some(3));
        for process in &left.0 {
            assert!(
                process.running(),
                "process {} was ended without the option",
                process.pid
            );
        }
    }

    /// Runs the installed init with `options` over a shell that writes `ran` to a marker
    /// file, its stdin a file to report to. Returns the exit code, whether the program ran,
    /// and the report's text.
    fn run_marker(name: &str, options: &[&str]) -> (Option<i32>, bool, String) {
        let dir = scratch(name);
        let init = install(&dir).expect("install");
        let marker = dir.join("marker");
        let report = create_report(&dir).expect("report");
        let script = format!("echo ran > {}", marker.display());
        let status = std::process::Command::new(&init)
            .args(options)
            .args(["--", "/bin/sh", "-c", &script])
            .stdin(report)
            .status()
            .expect("the init runs");
        let said = std::fs::read_to_string(report_path(&dir)).expect("report text");
        (status.code(), marker.exists(), said)
    }

    /// Proves `--wait-listen` lets the program start, exit 0, once its port has a listener
    /// in the init's network namespace. Catches an init that holds a ready container for
    /// its whole wait, or reports a listening port as missing.
    #[test]
    fn a_listening_port_lets_the_program_start() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
        let port = listener.local_addr().expect("addr").port().to_string();
        let (code, ran, said) = run_marker("listen-ready", &["--wait-listen", "1", &port]);
        assert_eq!(code, Some(0));
        assert!(ran, "the program ran");
        assert_eq!(said, "");
    }

    /// Proves a port with no listener ends the init with 126 after its seconds, the
    /// program never started, and names that port, the first missing one, in the report. The
    /// port is one end of a connection the test holds (established, never listening, and
    /// not available to anything else), listed after a port that does listen. Catches an
    /// init that starts the program on a port nobody listens on, names a listening port as
    /// missing, or leaves the report empty.
    #[test]
    fn a_port_that_never_listens_is_reported_and_the_program_does_not_start() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
        let client =
            std::net::TcpStream::connect(listener.local_addr().expect("addr")).expect("connect");
        let open = listener.local_addr().expect("addr").port();
        let silent = client.local_addr().expect("client addr").port();
        let ports = format!("{open},{silent}");
        let (code, ran, said) = run_marker("listen-missing", &["--wait-listen", "1", &ports]);
        assert_eq!(code, Some(126));
        assert!(!ran, "the program must not run");
        assert_eq!(
            said,
            format!("js-init: the network is not ready: port {silent} not listening within 1 s\n")
        );
        drop(client);
    }

    /// Proves a malformed `--wait-listen` is the usage error (exit 2, the program not
    /// started): seconds of 0, beyond 3600 or not a number, a port of 0 or above 65535, an
    /// empty entry, an empty list, and no list at all. Catches an init that waits on, or
    /// starts the program under, a bound or list it cannot meet.
    #[test]
    fn a_malformed_listen_wait_is_a_usage_error() {
        let cases: [&[&str]; 8] = [
            &["--wait-listen", "0", "80"],
            &["--wait-listen", "1", "0"],
            &["--wait-listen", "1", "65536"],
            &["--wait-listen", "1", "80,"],
            &["--wait-listen", "1", ""],
            &["--wait-listen", "x", "80"],
            &["--wait-listen", "1"],
            &["--wait-listen", "3601", "80"],
        ];
        for (n, options) in cases.iter().enumerate() {
            let (code, ran, _) = run_marker(&format!("listen-usage-{n}"), options);
            assert_eq!(code, Some(2), "{options:?}");
            assert!(!ran, "{options:?}: the program must not run");
        }
    }

    /// Proves the options have a fixed order: `--wait-route` after `--wait-listen` is a
    /// usage error. Catches an init that accepts them in either order, so a bundle's
    /// arguments would mean two things.
    #[test]
    fn a_route_wait_after_a_listen_wait_is_a_usage_error() {
        let (code, ran, _) = run_marker(
            "listen-order",
            &["--wait-listen", "1", "80", "--wait-route", "1"],
        );
        assert_eq!(code, Some(2));
        assert!(!ran, "the program must not run");
    }
}
