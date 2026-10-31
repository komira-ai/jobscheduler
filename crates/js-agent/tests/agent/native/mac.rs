//! The runtime's command line applied for real, on a Mac, as the user running the tests: the
//! fake helper starts the `run` it is asked for (`real_runs`), so `sandbox-exec` reads the
//! profile the runtime built. What this cannot show is the change of user, which needs root
//! (`tests/native_privileged.rs`).

use std::fs;
use std::sync::atomic::{AtomicU32, Ordering};

use js_agent::lifecycle::{Exit, Runtime, Status};

use super::Rig;

fn exited(code: i32) -> Status {
    Status::Exited(Exit {
        code,
        oom_killed: false,
    })
}

/// Asserts that a program runs under the profile the runtime builds, and that what it prints
/// reaches the log. It catches a profile `sandbox-exec` refuses to read, a parameter
/// spelled wrong, and a program that cannot start under it.
#[test]
fn a_program_runs_under_the_profile_and_its_output_reaches_the_log() {
    let mut rig = Rig::new("mac-echo");
    rig.fake.real_runs();
    let id = rig.start("r1", "/bin/echo", &["hello", "native"]);
    assert_eq!(rig.settle(&id), exited(0));
    let tail = rig.rt.log_tail(&id).unwrap().unwrap();
    assert_eq!(tail.stdout, "hello native\n");
    assert_eq!(tail.stderr, "");
}

/// Asserts that the profile grants writes in the run directory and nowhere else: a shell
/// writes a file in its working directory, and the same shell is refused a file in a
/// directory every user may write. It catches a profile that does not confine the program.
#[test]
fn the_program_writes_in_its_run_directory_and_nowhere_else() {
    static UNIQUE: AtomicU32 = AtomicU32::new(0);
    let mut rig = Rig::new("mac-write");
    rig.fake.real_runs();
    let inside = rig.start("r1", "/bin/sh", &["-c", "echo data > made.txt"]);
    assert_eq!(rig.settle(&inside), exited(0));
    let made = rig.fake.home_of("7.0").join("exec").join("made.txt");
    assert_eq!(fs::read_to_string(made).unwrap(), "data\n");

    let outside = std::path::PathBuf::from(format!(
        "/private/tmp/js-native-{}-{}",
        std::process::id(),
        UNIQUE.fetch_add(1, Ordering::SeqCst)
    ));
    let script = format!("echo data > {}", outside.display());
    let denied = rig.start("r2", "/bin/sh", &["-c", &script]);
    let status = rig.settle(&denied);
    let written = outside.exists();
    // Removed before anything is asserted, so a profile that fails leaves nothing behind.
    let _ = fs::remove_file(&outside);
    assert!(
        !written,
        "the profile let the program write {}",
        outside.display()
    );
    assert_eq!(status, exited(1));
}

/// A directory `name` below the rig's own, made, and its canonical spelling as a profile
/// parameter.
fn denied(dir: &crate::support::Scratch, name: &str) -> js_mac_pool::seatbelt::AbsPath {
    let path = dir.path(name);
    fs::create_dir_all(&path).unwrap();
    js_mac_pool::seatbelt::AbsPath::new(&fs::canonicalize(&path).unwrap()).unwrap()
}

/// Asserts the production layout of the read denies: the scratch root is read-denied, the
/// job's standard-stream files are inside it, and what the job prints still reaches them
/// (a write through a descriptor opened before the profile was applied is not a path
/// access). It catches a profile that costs every native job its output.
#[test]
fn output_reaches_the_log_with_the_scratch_root_denied() {
    let mut rig = Rig::in_dir("mac-scratch", |config, dir| {
        config.read_denies = vec![denied(dir, "scratch")];
    });
    rig.fake.real_runs();
    let id = rig.start("r1", "/bin/sh", &["-c", "echo to-out; echo to-err >&2"]);
    assert_eq!(rig.settle(&id), exited(0));
    let tail = rig.rt.log_tail(&id).unwrap().unwrap();
    assert_eq!(tail.stdout, "to-out\n");
    assert_eq!(tail.stderr, "to-err\n");
}

/// Asserts a read deny that file modes do not explain: the file is readable by the user the
/// test runs as (the control, a file beside it, is read), and the profile refuses the one in
/// the denied directory. It catches a read deny that is only the mode of the directory.
#[test]
fn a_read_deny_is_the_profiles_not_the_file_modes() {
    let mut rig = Rig::in_dir("mac-deny", |config, dir| {
        config.read_denies = vec![denied(dir, "secret")];
    });
    rig.fake.real_runs();
    for name in ["secret", "open"] {
        let folder = rig.dir.path(name);
        fs::create_dir_all(&folder).unwrap();
        fs::write(folder.join("f"), "x").unwrap();
    }
    let open = rig.dir.path("open").join("f").display().to_string();
    let secret = rig.dir.path("secret").join("f").display().to_string();
    let id = rig.start("r1", "/bin/cat", &[&open]);
    assert_eq!(rig.settle(&id), exited(0), "the control file is readable");
    assert_eq!(rig.rt.log_tail(&id).unwrap().unwrap().stdout, "x");
    let id = rig.start("r2", "/bin/cat", &[&secret]);
    assert_eq!(rig.settle(&id), exited(1));
    let tail = rig.rt.log_tail(&id).unwrap().unwrap();
    assert_eq!(tail.stdout, "");
    assert!(
        tail.stderr.contains("Operation not permitted"),
        "{}",
        tail.stderr
    );
}

const CONNECT: &str = r#"use IO::Socket::INET; exit(IO::Socket::INET->new(PeerAddr => "127.0.0.1", PeerPort => $ARGV[0]) ? 0 : 1)"#;

/// Asserts the network clamp for real: a loopback connect succeeds when the start asks for
/// loopback under a loopback cap, is refused when it asks for none, and is refused when the
/// cap is none whatever the start asks. It catches a level that is not applied to the
/// program, and a request that raises the cap.
#[test]
fn a_loopback_connect_follows_the_lower_of_request_and_cap() {
    use js_agent_wire::NetworkLevel;
    use js_mac_pool::seatbelt::Net;

    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port().to_string();
    let cases = [
        (Net::Loopback, NetworkLevel::Loopback, 0),
        (Net::Loopback, NetworkLevel::Off, 1),
        (Net::Off, NetworkLevel::On, 1),
    ];
    for (n, (cap, asked, code)) in cases.into_iter().enumerate() {
        let mut rig = Rig::with(&format!("mac-net{n}"), |config| config.net = cap);
        rig.fake.real_runs();
        let mut body = super::native("/usr/bin/perl", &["-e", CONNECT, &port]);
        body.network = asked;
        let spec = rig.spec("r1", body);
        let id = rig.rt.create_and_start(&spec).unwrap();
        assert_eq!(rig.settle(&id), exited(code), "case {n}");
    }
    drop(listener);
}

/// Asserts the file size limit for real: a program that writes four MiB to its standard
/// output under a limit of one MiB is ended, and the log never grows past the limit. It
/// catches a run that can fill the agent's disk through its output.
#[test]
fn a_run_that_writes_past_the_file_limit_is_ended_at_it() {
    let mut rig = Rig::with("mac-fsize", |config| config.max_file_bytes = 1 << 20);
    rig.fake.real_runs();
    let spec = rig.spec(
        "r1",
        super::native("/bin/dd", &["if=/dev/zero", "bs=1048576", "count=4"]),
    );
    let id = rig.rt.create_and_start(&spec).unwrap();
    let status = rig.settle(&id);
    assert_ne!(status, exited(0), "the program was not stopped");
    let written = fs::metadata(spec.scratch.join("stdout")).unwrap().len();
    assert!(written <= 1 << 20, "{written} bytes under a limit of 1 MiB");
}

/// Asserts the process limit for real: under a limit of one process the shell of a run
/// cannot start another program, so a fork bomb has no fork to make. It catches a run whose
/// user can fill the process table.
#[test]
fn a_run_cannot_fork_past_the_process_limit() {
    let mut rig = Rig::with("mac-nproc", |config| config.max_processes = 1);
    rig.fake.real_runs();
    let id = rig.start("r1", "/bin/sh", &["-c", "/bin/echo forked; :"]);
    rig.settle(&id);
    let tail = rig.rt.log_tail(&id).unwrap().unwrap();
    assert_eq!(tail.stdout, "", "the run forked");
    assert!(
        tail.stderr.contains("Resource temporarily unavailable"),
        "{}",
        tail.stderr
    );
}

/// Asserts that the command line the runtime builds keeps the shell that sets the limits
/// inside the profile: it is run for real with an environment that asks the shell to trace
/// with a `PS4` that touches a file in a directory every user may write, and to run an
/// imported `ulimit` function that does the same. The file must not exist afterwards, and
/// the run still ends well. The runtime refuses these names itself (see
/// `a_start_refuses_names_that_steer_the_shell_before_a_user_is_made`), so this applies the
/// command line without the runtime's checks, as a second line behind them. It catches a
/// limiting shell that runs before the profile is applied, as the lease user, with the
/// job's environment.
#[test]
fn the_job_environment_cannot_steer_the_shell_that_sets_the_limits() {
    static UNIQUE: AtomicU32 = AtomicU32::new(0);
    let rig = Rig::new("mac-shell-env");
    let marker = std::path::PathBuf::from(format!(
        "/private/tmp/js-native-escape-{}-{}",
        std::process::id(),
        UNIQUE.fetch_add(1, Ordering::SeqCst)
    ));
    let touch = format!("/usr/bin/touch {}", marker.display());
    let mut body = super::native("/bin/echo", &["done"]);
    body.env = std::collections::BTreeMap::from([
        ("SHELLOPTS".to_owned(), "xtrace".to_owned()),
        ("PS4".to_owned(), format!("$({touch}) ")),
        (
            "BASH_FUNC_ulimit%%".to_owned(),
            format!("() {{ {touch}; return 0; }}"),
        ),
    ]);
    let home = rig.fake.home_of("7.0");
    let exec = home.join("exec");
    fs::create_dir_all(&exec).unwrap();
    fs::create_dir_all(home.join("tmp")).unwrap();
    let config = super::config(&rig.fake);
    let argv = js_agent::native::profile_argv(&config, &body, &home).unwrap();
    let out = std::process::Command::new(&argv[0])
        .args(&argv[1..])
        .env_clear()
        .envs(&body.env)
        .current_dir(&exec)
        .output()
        .unwrap();
    let escaped = marker.exists();
    // Removed before anything is asserted, so a failure leaves nothing behind.
    let _ = fs::remove_file(&marker);
    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    // The job must have run first: a command line that never started would leave no marker
    // either, and that is not the sandbox holding. The imported `ulimit` function returns
    // success (its write is denied by the profile), so the shell goes on to exec the job.
    assert!(
        out.status.success() && stdout.contains("done"),
        "the job did not run: {:?}\nstdout: {stdout}\nstderr: {stderr}",
        out.status
    );
    assert!(
        !escaped,
        "a command ran outside the profile: {}\nstderr: {stderr}",
        marker.display()
    );
}
