//! One job through the real container create path of the agent, run by a real rootful
//! Podman, and what the job sees of its own sandbox. Every other container test of this
//! crate answers through a scripted engine, so it proves the arguments the agent builds and
//! never that an engine honours them; this one reads the properties from inside the
//! running container. The create path reads the mounts back before `start`, and on Podman
//! requires every bind's `.Mounts[].Options` to list `nosuid` and `nodev`: a job that starts
//! here shows that a real Podman reports them.
//!
//! The test is `#[ignore]`d, so `cargo test` is unchanged. It needs root, a `podman`, an
//! image already pulled (the agent pulls nothing) and a subordinate id range for a user,
//! all of which the `live-podman` job of `.github/workflows/ci.yml` prepares. The host
//! is described by two variables, so the workflow is the only place that names them:
//!
//! - `JS_LIVE_IMAGE`: the image, by digest, as it was pulled;
//! - `JS_LIVE_SUBUID_USER`: the user whose one line in `/etc/subuid` and `/etc/subgid` is
//!   the range the jobs are mapped onto.
//!
//! One job writes what it sees into its `/scratch`; the test waits for it through the
//! runtime's `poll`, reads the files back from the host directory and asserts on them.
//! A missing variable, a missing file or a job that failed is a failure, never a skip.

use std::collections::BTreeMap;
use std::fs;
use std::os::unix::fs::{DirBuilderExt, MetadataExt};
use std::path::{Path, PathBuf};
use std::thread::sleep;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use js_agent::Platform;
use js_agent::cli_runtime::{CliRuntime, Settings, system_runner};
use js_agent::lifecycle::{Body, Exit, RunSpec, Runtime, Status, label};
use js_agent_wire::Isolation;
use js_testkit::dir::TestDir;

const PODMAN: &str = "/usr/bin/podman";

/// The user and group the agent runs jobs as by default, in the job's namespace.
const JOB_ID: u32 = 65534;

/// The length of a mapped range.
const RANGE_LEN: u64 = 65536;

/// How long the job may take. It copies a small file and lists a few directories.
const JOB_LIMIT: Duration = Duration::from_secs(180);

/// What the job runs, under `sh -c`, in its `/scratch`. No step stops the script: each
/// writes its outcome (exit status, error text) to a file, and `done` is written last, so
/// a file that is missing is a job that did not get that far.
///
/// - `status`, `uid_map`, `gid_map`, `mountinfo`: the kernel's view of the process.
/// - `net`: the network interfaces.
/// - `usr_write.*`: a write under `/usr`, on the image's read-only root.
/// - `tmp_x_ls`, `tmp_exec.*`: a program copied into `/tmp` (that works: `/tmp` is
///   writable) and then executed from there (that must not: `/tmp` is `noexec`).
const SCRIPT: &str = r#"
cd /scratch
cat /proc/self/status > status
cat /proc/self/uid_map > uid_map
cat /proc/self/gid_map > gid_map
cat /proc/self/mountinfo > mountinfo
ls /sys/class/net > net
{ echo x > /usr/x; } 2> usr_write.err
echo $? > usr_write.rc
cp /bin/busybox /tmp/x 2> tmp_cp.err
echo $? > tmp_cp.rc
ls -l /tmp/x > tmp_x_ls 2>&1
{ /tmp/x true; } 2> tmp_exec.err
echo $? > tmp_exec.rc
echo done > done
"#;

fn need(name: &str) -> String {
    std::env::var(name).unwrap_or_else(|_| panic!("{name} must be set by the workflow step"))
}

/// The `start:count` of the one line of `file` (`/etc/subuid` or `/etc/subgid`) for `user`.
fn range_of(file: &str, user: &str) -> (u64, u64) {
    let text = fs::read_to_string(file).unwrap_or_else(|why| panic!("{file}: {why}"));
    let mut lines = text
        .lines()
        .filter_map(|line| line.strip_prefix(user)?.strip_prefix(':'));
    let line = lines
        .next()
        .unwrap_or_else(|| panic!("{file} has no line for {user}"));
    assert!(lines.next().is_none(), "{file} has two lines for {user}");
    let (start, count) = line.split_once(':').expect("start:count");
    (start.parse().expect("start"), count.parse().expect("count"))
}

fn now_ms() -> u64 {
    let since = SystemTime::now().duration_since(UNIX_EPOCH).unwrap();
    u64::try_from(since.as_millis()).unwrap()
}

/// The job: the pulled image running `SCRIPT`, with the limits and labels a real start has.
fn job(image: &str, scratch: &Path) -> RunSpec {
    let labels: BTreeMap<String, String> = [(label::RUN, "live1"), (label::LEASE, "lease-live")]
        .into_iter()
        .map(|(k, v)| (k.to_owned(), v.to_owned()))
        .collect();
    RunSpec {
        run_id: "live1".to_owned(),
        body: Body::Container {
            image: image.to_owned(),
            args: ["sh", "-c", SCRIPT].map(str::to_owned).to_vec(),
        },
        cpu_millis: 1000,
        memory_bytes: 256 << 20,
        pids: Some(64),
        gpu_indices: Vec::new(),
        mounts: Vec::new(),
        scratch: scratch.to_path_buf(),
        slot: 0,
        isolation: Isolation::Contained,
        labels,
    }
}

/// Polls until the container has exited, and returns how.
fn wait_for_exit(rt: &mut CliRuntime, id: &js_agent::lifecycle::RunHandle) -> Exit {
    let started = Instant::now();
    loop {
        match rt.poll(id) {
            Ok(Status::Exited(exit)) => return exit,
            Ok(Status::Gone) => panic!("the container is gone before it was seen to exit"),
            Ok(Status::Running) | Err(_) => {}
        }
        assert!(started.elapsed() < JOB_LIMIT, "the job did not end in time");
        sleep(Duration::from_millis(250));
    }
}

/// The fields of the `/proc/self/status` line `name`, after the colon.
fn status_field<'a>(status: &'a str, name: &str) -> &'a str {
    status
        .lines()
        .find_map(|line| line.strip_prefix(name)?.strip_prefix(':'))
        .unwrap_or_else(|| panic!("no {name} in /proc/self/status:\n{status}"))
        .trim()
}

/// The three numbers of the only line of a `uid_map` or `gid_map`.
fn single_map(text: &str) -> (u64, u64, u64) {
    let rows: Vec<Vec<u64>> = text
        .lines()
        .map(|line| {
            line.split_whitespace()
                .map(|n| n.parse().expect("a number"))
                .collect()
        })
        .collect();
    assert_eq!(rows.len(), 1, "one map line, got:\n{text}");
    assert_eq!(rows[0].len(), 3, "three fields, got:\n{text}");
    (rows[0][0], rows[0][1], rows[0][2])
}

/// The per-mount options and the super options of the mount at `point` in `mountinfo`.
fn mount_options(mountinfo: &str, point: &str) -> Vec<String> {
    let line = mountinfo
        .lines()
        .find(|line| line.split(' ').nth(4) == Some(point))
        .unwrap_or_else(|| panic!("no mount at {point} in:\n{mountinfo}"));
    let fields: Vec<&str> = line.split(' ').collect();
    let dash = fields.iter().position(|f| *f == "-").expect("separator");
    let mut options: Vec<String> = fields[5].split(',').map(str::to_owned).collect();
    options.extend(fields[dash + 3].split(',').map(str::to_owned));
    options
}

fn has(options: &[String], one: &str) -> bool {
    options.iter().any(|o| o == one)
}

/// Runs one job under rootful Podman, mapped onto the range of the subordinate-id user,
/// and asserts every property the agent's flags are meant to give it, as the job saw it.
///
/// Each assertion names the property it checks and the host fact it is compared with:
/// the range comes from the files the workflow wrote, not from the code under test.
#[test]
#[ignore = "needs root, a rootful podman, the pulled image and a subordinate id range: see the live-podman job"]
fn a_job_in_real_podman_sees_the_sandbox_the_flags_describe() {
    assert!(rustix::process::geteuid().is_root(), "run as root: sudo -E");
    let image = need("JS_LIVE_IMAGE");
    let user = need("JS_LIVE_SUBUID_USER");
    let (uid_start, uid_count) = range_of("/etc/subuid", &user);
    let (gid_start, gid_count) = range_of("/etc/subgid", &user);
    assert_eq!((uid_count, gid_count), (RANGE_LEN, RANGE_LEN));

    // The scratch root and the run's directory are private to the agent (0700), as the
    // agent makes them; the runtime hands the run's directory to the job's mapped ids.
    let root = TestDir::under(env!("CARGO_TARGET_TMPDIR"), "live-podman");
    fs::set_permissions(
        root.path(),
        std::os::unix::fs::PermissionsExt::from_mode(0o700),
    )
    .unwrap();
    let scratch: PathBuf = root.path().join("live1");
    fs::DirBuilder::new().mode(0o700).create(&scratch).unwrap();

    let settings = Settings::new(Path::new(PODMAN), None, None)
        .unwrap()
        .with_subuid(&user)
        .unwrap()
        .with_agent(0, 0)
        .unwrap();
    let platform = Platform {
        os: "linux".to_owned(),
        arch: std::env::consts::ARCH.to_owned(),
    };
    let runner = Box::new(system_runner(Duration::from_secs(10)));
    let mut rt = CliRuntime::open(settings, "h-live-podman", &platform, now_ms, runner)
        .unwrap_or_else(|why| panic!("opening the runtime: {why}"));

    let id = rt
        .create_and_start(&job(&image, &scratch))
        .unwrap_or_else(|why| panic!("create_and_start: {why:?}"));
    let exit = wait_for_exit(&mut rt, &id);
    // Read before the container goes, and remove it before asserting, so a failed
    // assertion leaves nothing behind.
    let files = [
        "status",
        "uid_map",
        "gid_map",
        "mountinfo",
        "net",
        "usr_write.err",
        "usr_write.rc",
        "tmp_cp.rc",
        "tmp_x_ls",
        "tmp_exec.err",
        "tmp_exec.rc",
        "done",
    ];
    let seen: BTreeMap<&str, String> = files
        .iter()
        .map(|name| {
            (
                *name,
                fs::read_to_string(scratch.join(name)).unwrap_or_default(),
            )
        })
        .collect();
    let owners: Vec<(String, u32, u32)> = fs::read_dir(&scratch)
        .unwrap()
        .map(|entry| {
            let entry = entry.unwrap();
            let meta = entry.metadata().unwrap();
            (
                entry.file_name().to_string_lossy().into_owned(),
                meta.uid(),
                meta.gid(),
            )
        })
        .collect();
    let dir_meta = fs::metadata(&scratch).unwrap();
    rt.remove(&id).unwrap();

    // The job ran to its last line and ended 0.
    assert_eq!(
        exit,
        Exit {
            code: 0,
            oom_killed: false
        }
    );
    assert_eq!(
        seen["done"], "done\n",
        "the script did not finish: {seen:#?}"
    );

    // Seccomp: the pinned profile is installed as a filter (mode 2), and no-new-privileges
    // is set, so a setuid program cannot gain what the filter and the caps withhold.
    let status = &seen["status"];
    assert_eq!(status_field(status, "Seccomp"), "2", "{status}");
    assert_ne!(status_field(status, "Seccomp_filters"), "0", "{status}");
    assert_eq!(status_field(status, "NoNewPrivs"), "1", "{status}");

    // `--cap-drop ALL`: no capability in any set, as hex digits all zero.
    for set in ["CapEff", "CapPrm", "CapBnd", "CapInh", "CapAmb"] {
        let value = u64::from_str_radix(status_field(status, set), 16).unwrap();
        assert_eq!(value, 0, "{set} is not empty:\n{status}");
    }

    // The job runs as the default job user, not as the container's root.
    let uids: Vec<&str> = status_field(status, "Uid").split_whitespace().collect();
    assert_eq!(uids, vec!["65534"; 4], "{status}");

    // `--uidmap`/`--gidmap`: container ids 0 to 65535 are the range of the subid files,
    // and not the identity map of the host's namespace (`0 0 4294967295`).
    assert_eq!(single_map(&seen["uid_map"]), (0, uid_start, RANGE_LEN));
    assert_eq!(single_map(&seen["gid_map"]), (0, gid_start, RANGE_LEN));

    // `--network none`: the job's `/sys` lists the loopback and nothing else.
    assert_eq!(seen["net"].lines().collect::<Vec<_>>(), vec!["lo"]);

    // `--read-only`: the root is mounted read-only, and a write under it was refused for
    // that reason (EROFS, not a permission error), after the writable paths worked.
    let mountinfo = &seen["mountinfo"];
    assert!(has(&mount_options(mountinfo, "/"), "ro"), "{mountinfo}");
    assert_eq!(seen["usr_write.rc"].trim(), "1");
    assert!(
        seen["usr_write.err"].contains("Read-only file system"),
        "{}",
        seen["usr_write.err"]
    );

    // `/tmp` is a `noexec` tmpfs: a copy of busybox is written there and is executable by
    // mode, and running it is refused.
    assert_eq!(seen["tmp_cp.rc"].trim(), "0");
    assert!(seen["tmp_x_ls"].starts_with("-rwx"), "{}", seen["tmp_x_ls"]);
    let tmp = mount_options(mountinfo, "/tmp");
    for flag in ["noexec", "nosuid", "nodev"] {
        assert!(has(&tmp, flag), "/tmp lacks {flag}: {tmp:?}");
    }
    assert_eq!(seen["tmp_exec.rc"].trim(), "126");
    assert!(
        seen["tmp_exec.err"].contains("Permission denied"),
        "{}",
        seen["tmp_exec.err"]
    );

    // Podman's `--read-only` also mounts writable tmpfs on `/var/tmp` and `/run`; the agent
    // names both, so they are `noexec,nosuid,nodev` like `/tmp` and not Podman's defaults.
    for point in ["/var/tmp", "/run"] {
        let options = mount_options(mountinfo, point);
        for flag in ["noexec", "nosuid", "nodev"] {
            assert!(has(&options, flag), "{point} lacks {flag}: {options:?}");
        }
    }

    // The scratch bind mount is `nosuid,nodev` (and writable: the job wrote it).
    let scratch_mount = mount_options(mountinfo, "/scratch");
    for flag in ["nosuid", "nodev", "rw"] {
        assert!(
            has(&scratch_mount, flag),
            "/scratch lacks {flag}: {scratch_mount:?}"
        );
    }

    // The files the job wrote, and the directory, belong on the host to the mapped ids of
    // the job's user: range start plus 65534. (They are not root's, and not the range
    // start itself, which is the container's root.)
    let (want_uid, want_gid) = (
        u32::try_from(uid_start).unwrap() + JOB_ID,
        u32::try_from(gid_start).unwrap() + JOB_ID,
    );
    assert_eq!((dir_meta.uid(), dir_meta.gid()), (want_uid, want_gid));
    assert!(owners.len() >= files.len(), "{owners:?}");
    for (name, uid, gid) in owners {
        assert_eq!((uid, gid), (want_uid, want_gid), "owner of {name}");
    }
}
