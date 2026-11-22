//! The native runtime on a Mac with the real supervisor, as the user running the tests: the
//! account pool is the real one over a fake host (`fake_accounts`), whose slot is the test's
//! own uid, and the runs are started by `SystemLauncher` over the `js-agent` binary cargo
//! built, so the supervisor sets the limits, applies the profile the runtime built with
//! `sandbox_init_with_parameters` and starts the program, as it does in production. What this
//! cannot show is the change of user and the audit session, which need root
//! (`tests/native_privileged.rs`); the runtime is built without the session
//! (`NativeRuntime::without_new_session`).

use std::fs;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};

use js_agent::lifecycle::{Exit, RunHandle, RunSpec, Runtime, Status};
use js_agent::mac_accounts::Settings;
use js_agent::mac_supervisor::SystemLauncher;
use js_agent::native::{Ceiling, NativeConfig, NativeRuntime};
use js_agent_wire::NativeSpec;
use js_mac_pool::seatbelt::AbsPath;

use super::{
    FakeAccounts, Scratch, TERM, config, native, open_pool, pool_settings, settle, spec_in,
};

fn exited(code: i32) -> Status {
    Status::Exited(Exit {
        code,
        oom_killed: false,
    })
}

/// A native runtime of one slot (the test's own uid) over the real supervisor.
struct MacRig {
    rt: NativeRuntime,
    dir: Scratch,
    settings: Settings,
}

impl MacRig {
    fn new(name: &str) -> Self {
        Self::with(name, |_, _| {})
    }

    /// A rig whose configuration `adjust` has changed, knowing the rig's directory.
    fn with(name: &str, adjust: impl FnOnce(&mut NativeConfig, &Scratch)) -> Self {
        let dir = Scratch::new(&format!("native-mac-{name}"));
        let accounts = FakeAccounts::new();
        let settings = pool_settings(&dir, 1);
        let pool = open_pool(&dir, &settings, &accounts);
        let mut config = config(&dir, &settings);
        adjust(&mut config, &dir);
        let launcher = Arc::new(SystemLauncher::at(env!("CARGO_BIN_EXE_js-agent")));
        let rt = NativeRuntime::over(Some(pool), launcher, config, TERM)
            .unwrap()
            .without_new_session();
        Self { rt, dir, settings }
    }

    fn spec(&self, run_id: &str, body: NativeSpec) -> RunSpec {
        spec_in(&self.dir, run_id, body)
    }

    /// Starts `program args` and returns its handle.
    fn start(&mut self, run_id: &str, program: &str, args: &[&str]) -> RunHandle {
        let spec = self.spec(run_id, native(program, args));
        self.rt.create_and_start(&spec).unwrap()
    }

    fn settle(&mut self, id: &RunHandle) -> Status {
        settle(&mut self.rt, id)
    }

    /// The run directory of the account lent now: the only home there is.
    fn exec(&self) -> PathBuf {
        let mut homes = fs::read_dir(&self.settings.homes)
            .unwrap()
            .map(|entry| entry.unwrap().path());
        let home = homes.next().expect("an account is lent");
        assert_eq!(homes.next(), None, "more than one account is lent");
        home.join("exec")
    }
}

/// A directory `name` below `dir`, made, as a read deny of a profile.
fn denied(dir: &Scratch, name: &str) -> AbsPath {
    let path = dir.path(name);
    fs::create_dir_all(&path).unwrap();
    AbsPath::new(&fs::canonicalize(&path).unwrap()).unwrap()
}

/// The per-user folder of the user running the tests, with a trailing slash.
fn user_folder() -> String {
    let out = std::process::Command::new("/usr/bin/getconf")
        .arg("DARWIN_USER_DIR")
        .output()
        .unwrap();
    String::from_utf8(out.stdout).unwrap().trim().to_owned()
}

/// Asserts that a program runs under the profile the runtime builds, started by the real
/// supervisor, and that what it prints reaches the log. Catches a profile the system refuses
/// to read, a parameter spelled wrong, and a program that cannot start under it.
#[test]
fn a_program_runs_under_the_profile_and_its_output_reaches_the_log() {
    let mut rig = MacRig::new("echo");
    let id = rig.start("r1", "/bin/echo", &["hello", "native"]);
    assert_eq!(rig.settle(&id), exited(0));
    let tail = rig.rt.log_tail(&id).unwrap().unwrap();
    assert_eq!(tail.stdout, "hello native\n");
    assert_eq!(tail.stderr, "");
    rig.rt.remove(&id).unwrap();
}

/// Asserts that the profile grants writes in the run directory and nowhere else: a shell writes
/// a file in its working directory, and the same shell is refused a file in the account's home
/// (the parent of the run directory) and one in the user's per-user folder, which the user may
/// write otherwise. Catches a profile that does not confine the program, or that grants the
/// home or the per-user folder.
#[test]
fn the_program_writes_in_its_run_directory_and_nowhere_else() {
    static UNIQUE: AtomicU32 = AtomicU32::new(0);
    let mut rig = MacRig::new("write");
    let inside = rig.start("r1", "/bin/sh", &["-c", "echo data > made.txt"]);
    assert_eq!(rig.settle(&inside), exited(0));
    let made = rig.exec().join("made.txt");
    assert_eq!(fs::read_to_string(made).unwrap(), "data\n");
    rig.rt.remove(&inside).unwrap();

    let home_file = rig.start("r2", "/bin/sh", &["-c", "echo data > ../in-home.txt"]);
    let status = rig.settle(&home_file);
    let in_home = rig.exec().parent().unwrap().join("in-home.txt");
    let written = in_home.exists();
    assert!(!written, "the profile let the program write {in_home:?}");
    assert_eq!(status, exited(1));
    rig.rt.remove(&home_file).unwrap();

    let folder = PathBuf::from(format!(
        "{}js-native-{}-{}",
        user_folder(),
        std::process::id(),
        UNIQUE.fetch_add(1, Ordering::SeqCst)
    ));
    let script = format!("echo data > {}", folder.display());
    let in_folder = rig.start("r3", "/bin/sh", &["-c", &script]);
    let status = rig.settle(&in_folder);
    let written = folder.exists();
    // Removed before anything is asserted, so a profile that fails leaves nothing behind.
    let _ = fs::remove_file(&folder);
    assert!(!written, "the profile let the program write {folder:?}");
    assert_eq!(status, exited(1));
    rig.rt.remove(&in_folder).unwrap();
}

/// Asserts the production layout of the read denies: the scratch root is read-denied, the
/// job's standard-stream files are inside it, and what the job prints still reaches them (a
/// write through a descriptor opened before the profile was applied is not a path access).
/// Catches a profile that costs every native job its output.
#[test]
fn output_reaches_the_log_with_the_scratch_root_denied() {
    let mut rig = MacRig::with("scratch", |config, dir| {
        config.read_denies = vec![denied(dir, "scratch")];
    });
    let id = rig.start("r1", "/bin/sh", &["-c", "echo to-out; echo to-err >&2"]);
    assert_eq!(rig.settle(&id), exited(0));
    let tail = rig.rt.log_tail(&id).unwrap().unwrap();
    assert_eq!(tail.stdout, "to-out\n");
    assert_eq!(tail.stderr, "to-err\n");
    rig.rt.remove(&id).unwrap();
}

/// Asserts a read deny that file modes do not explain: the file is readable by the user the
/// test runs as (the control, a file beside it, is read), and the profile refuses the one in
/// the denied directory. Catches a read deny that is only the mode of the directory.
#[test]
fn a_read_deny_is_the_profiles_not_the_file_modes() {
    let mut rig = MacRig::with("deny", |config, dir| {
        config.read_denies = vec![denied(dir, "secret")];
    });
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
    rig.rt.remove(&id).unwrap();
    let id = rig.start("r2", "/bin/cat", &[&secret]);
    assert_eq!(rig.settle(&id), exited(1));
    let tail = rig.rt.log_tail(&id).unwrap().unwrap();
    assert_eq!(tail.stdout, "");
    assert!(
        tail.stderr.contains("Operation not permitted"),
        "{}",
        tail.stderr
    );
    rig.rt.remove(&id).unwrap();
}

const CONNECT: &str = r#"use IO::Socket::INET; exit(IO::Socket::INET->new(PeerAddr => "127.0.0.1", PeerPort => $ARGV[0]) ? 0 : 1)"#;

/// Asserts the network of the profile for real: a loopback connect succeeds when the start
/// asks for loopback under a loopback ceiling, is refused when it asks for none, and is refused
/// when the ceiling is none whatever the start asks. The runtime is driven directly, so this is
/// the profile's own bound: an agent refuses such a start before it reaches the runtime
/// (`check_native`), and the profile never grants more than the ceiling all the same. Catches a
/// level that is not applied to the program, and a profile that grants more than the ceiling.
#[test]
fn a_loopback_connect_follows_the_lower_of_request_and_cap() {
    use js_agent_wire::NetworkLevel;

    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port().to_string();
    let cases = [
        (Ceiling::Loopback, NetworkLevel::Loopback, 0),
        (Ceiling::Loopback, NetworkLevel::Off, 1),
        (Ceiling::Off, NetworkLevel::On, 1),
    ];
    for (n, (cap, asked, code)) in cases.into_iter().enumerate() {
        let mut rig = MacRig::with(&format!("net{n}"), |config, _| config.network = cap);
        let mut body = native("/usr/bin/perl", &["-e", CONNECT, &port]);
        body.network = asked;
        let spec = rig.spec("r1", body);
        let id = rig.rt.create_and_start(&spec).unwrap();
        assert_eq!(rig.settle(&id), exited(code), "case {n}");
        rig.rt.remove(&id).unwrap();
    }
    drop(listener);
}

/// Asserts the file size limit for real, as the supervisor sets it with `setrlimit` before it
/// starts the program: a program that writes four MiB to its standard output under a limit of
/// one MiB is ended by `SIGXFSZ` (status 128 and the signal, 153), and the log never grows past
/// the limit. Catches a run that can fill the agent's disk through its output, and a limit the
/// supervisor did not set or that was lost on the way to the program.
#[test]
fn a_run_that_writes_past_the_file_limit_is_ended_at_it() {
    let mut rig = MacRig::with("fsize", |config, _| config.max_file_bytes = 1 << 20);
    let spec = rig.spec(
        "r1",
        native("/bin/dd", &["if=/dev/zero", "bs=1048576", "count=4"]),
    );
    let id = rig.rt.create_and_start(&spec).unwrap();
    let status = rig.settle(&id);
    let tail = rig.rt.log_tail(&id).unwrap().unwrap();
    let by_signal = status == exited(128 + libc::SIGXFSZ);
    let by_efbig = status == exited(1) && tail.stderr.contains("File too large");
    assert!(
        by_signal || by_efbig,
        "not stopped by SIGXFSZ or EFBIG: {status:?} {tail:?}"
    );
    let written = fs::metadata(spec.scratch.join("stdout")).unwrap().len();
    assert!(written <= 1 << 20, "{written} bytes under a limit of 1 MiB");
    rig.rt.remove(&id).unwrap();
}

/// Asserts that the process limit reaches the supervisor before the program starts and
/// holds: the run's limit is `max_processes` and one, and it counts every process of the user
/// (here the user running the tests, who has more than two), so under `max_processes = 1` the
/// supervisor cannot start the program at all. The start fails as a setup the supervisor ended
/// before the job started, and the program's own effect (a file in the run directory) never
/// happens; the account is retired. Catches a run whose user can fill the process table, and a
/// limit that is not set before the program starts.
#[test]
fn a_run_cannot_fork_past_the_process_limit() {
    let mut rig = MacRig::with("nproc", |config, _| config.max_processes = 1);
    let spec = rig.spec("r1", native("/bin/sh", &["-c", "echo forked > forked.txt"]));
    let why = rig.rt.create_and_start(&spec).unwrap_err();
    assert!(
        why.0
            .starts_with("the supervisor: ended before the job started: SetupFailed"),
        "{}",
        why.0
    );
    let homes: Vec<_> = fs::read_dir(&rig.settings.homes)
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .collect();
    for home in &homes {
        assert!(
            !home.join("exec").join("forked.txt").exists(),
            "the program ran past the process limit"
        );
    }
}

/// Asserts that a limit the supervisor set cannot be raised by the program: a shell started as
/// the job raises its file size limit to unlimited, which the hard limit refuses, so it never
/// prints. Catches limits set soft only, which the run could lift itself.
#[test]
fn a_run_cannot_raise_the_limits_it_was_given() {
    let mut rig = MacRig::with("raise", |config, _| config.max_file_bytes = 1 << 20);
    let id = rig.start(
        "r1",
        "/bin/sh",
        &["-c", "ulimit -f unlimited && echo raised"],
    );
    let status = rig.settle(&id);
    let tail = rig.rt.log_tail(&id).unwrap().unwrap();
    assert_ne!(status, exited(0), "{tail:?}");
    assert_eq!(tail.stdout, "", "the limit was raised: {tail:?}");
    rig.rt.remove(&id).unwrap();
}

/// Asserts that the supervisor starts no shell in front of the program: the program is run with
/// an environment that would make a shell trace with a `PS4` that creates a file in the run
/// directory (which the profile lets the job write), and run an imported `ulimit` function that
/// does the same. The file must not exist afterwards, and the run still ends well. Catches a
/// shell put in front of the program, which would run with the job's environment.
#[test]
fn the_job_environment_steers_no_shell_because_there_is_none() {
    let mut rig = MacRig::new("shell-env");
    let touch = "/usr/bin/touch escaped";
    let mut body = native("/bin/echo", &["done"]);
    body.env = std::collections::BTreeMap::from([
        ("SHELLOPTS".to_owned(), "xtrace".to_owned()),
        ("PS4".to_owned(), format!("$({touch}) ")),
        (
            "BASH_FUNC_ulimit%%".to_owned(),
            format!("() {{ {touch}; return 0; }}"),
        ),
    ]);
    let spec = rig.spec("r1", body);
    let id = rig.rt.create_and_start(&spec).unwrap();
    let status = rig.settle(&id);
    let escaped = rig.exec().join("escaped").exists();
    let tail = rig.rt.log_tail(&id).unwrap().unwrap();
    // The job must have run first: a start that never ran it would leave no file either, and
    // that is not the absence of a shell.
    assert_eq!(status, exited(0), "{tail:?}");
    assert_eq!(tail.stdout, "done\n");
    assert!(!escaped, "a shell ran with the job's environment");
    rig.rt.remove(&id).unwrap();
}

/// Asserts that the start-up self-test passes with the real supervisor: `/usr/bin/true` runs
/// through every step of a start under the real profile, its end and its removal, with its
/// scratch directory below the state directory. Catches a supervisor, a profile or a pool that
/// cannot run a job, which would fail every job after the hello said it could.
#[test]
fn the_self_test_passes_with_the_real_supervisor() {
    let mut rig = MacRig::new("selftest");
    assert_eq!(rig.rt.self_test(), Ok(()));
    assert_eq!(
        fs::read_dir(&rig.settings.homes).unwrap().count(),
        0,
        "the self-test left an account"
    );
}

/// The kbf shape of a tree: a read-only, `no_exec` mount of the host directory `dir` at
/// `/work/.kbf-tree`, as the lifecycle hands it over once the provider has answered.
fn tree_mount(dir: &std::path::Path) -> js_agent::lifecycle::JobFs {
    use js_agent_wire::{JobMount, MountMode, MountSource, Propagation};
    js_agent::lifecycle::JobFs {
        layers: Vec::new(),
        mounts: vec![JobMount {
            source: MountSource::HostPath {
                path: dir.display().to_string(),
            },
            target: "/work/.kbf-tree".to_owned(),
            mode: MountMode::ReadOnly,
            sub_path: None,
            no_exec: true,
            propagation: Propagation::Private,
        }],
    }
}

/// What a job prints about the trees root `root`: its own tree read through the link, run
/// B's tree, a listing of the root, a write into its own tree, and a run of a script there.
const TREE_CHECKS: &str = r#"cat .kbf-tree/data.txt
cat "$1/data.txt" >/dev/null 2>&1 && echo other-read || echo other-refused
ls "$2" >/dev/null 2>&1 && echo root-listed || echo root-refused
(echo x > .kbf-tree/new) 2>/dev/null && echo wrote || echo write-refused
./.kbf-tree/run.sh 2>/dev/null || echo exec-refused"#;

/// Asserts under the real profile, with the trees root the one mount root that is no read
/// deny (as `native_fs::denied_roots` makes the denies), that a job reads its own tree
/// through its link `exec/.kbf-tree`, is refused run B's tree beside it and the listing of the
/// root, and can neither write its tree nor run a script in it, though the file modes allow
/// both; and that with the trees root among the read denies (what the agent did before), the
/// same job cannot read its own tree. Catches a profile that opens every run's trees, one that
/// lets a job write or run a tree, and the deny of the root after the trees' grants, which
/// hides every tree.
#[test]
fn a_job_reads_its_own_tree_and_nothing_else_below_the_trees_root() {
    use js_agent::lifecycle::FsPrepared;
    use js_agent::native::MountRoot;

    let run = |deny_root: bool| {
        let mut rig = MacRig::with(&format!("trees-{deny_root}"), |config, dir| {
            let root = denied(dir, "trees");
            let roots = vec![MountRoot {
                path: root.as_str().into(),
                read_only: true,
                trees: true,
            }];
            config.read_denies = js_agent::native_fs::denied_roots(&roots)
                .map(|root| AbsPath::new(&root.path).unwrap())
                .collect();
            if deny_root {
                config.read_denies.push(root);
            }
            config.mount_roots = roots;
        });
        let root = fs::canonicalize(rig.dir.path("trees")).unwrap();
        let (own, other) = (
            root.join("mount-provider/aa/6a732e7231/0"),
            root.join("mount-provider/aa/6a732e7232/0"),
        );
        for (tree, text) in [(&own, "tree one\n"), (&other, "tree two\n")] {
            fs::create_dir_all(tree).unwrap();
            fs::write(tree.join("data.txt"), text).unwrap();
            let script = tree.join("run.sh");
            fs::write(&script, "#!/bin/sh\necho ran\n").unwrap();
            fs::set_permissions(&script, std::os::unix::fs::PermissionsExt::from_mode(0o755))
                .unwrap();
        }
        let mounts = tree_mount(&own);
        assert_eq!(rig.rt.check_job_fs(&mounts, &[]), Ok(()));
        assert_eq!(rig.rt.resolve_fs("r1", "", &mounts, 0), FsPrepared::Ready);
        let (other, listed) = (other.display().to_string(), root.display().to_string());
        let id = rig.start(
            "r1",
            "/bin/sh",
            &["-c", TREE_CHECKS, "sh", other.as_str(), listed.as_str()],
        );
        let status = rig.settle(&id);
        let tail = rig.rt.log_tail(&id).unwrap().unwrap();
        let link = fs::read_link(rig.exec().join(".kbf-tree")).unwrap();
        let wrote = own.join("new").exists();
        rig.rt.remove(&id).unwrap();
        assert_eq!(link, own);
        assert!(!wrote, "the job wrote its tree");
        (status, tail.stdout)
    };
    assert_eq!(
        run(false),
        (
            exited(0),
            "tree one\nother-refused\nroot-refused\nwrite-refused\nexec-refused\n".to_owned()
        )
    );
    let (_, denied_root) = run(true);
    assert!(
        !denied_root.starts_with("tree one"),
        "a job read its tree with the trees root denied: {denied_root}"
    );
}
