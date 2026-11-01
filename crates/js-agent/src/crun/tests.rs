//! The pure parts of the crun runtime: image pins, the container a run becomes, the exit
//! a container's end becomes, and the names of the paths it keeps.

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

use js_contain::cgroup::{LeaseCgroup, OomOutcome};
use js_contain::container::{ContainerLimits, Ended, HostMount};
use js_contain::oci::image::PinnedImage;
use js_contain::oci::{LeaseDirs, LogTail as Captured, StreamTail};

use super::spec::{
    EXEC_ROOT, NOT_RUN, container_spec, ctr_of, end_of, exit_of, log_tail_of, not_run_words, pinned,
};
use super::{config_dir, system_now_ms};
use crate::cli_runtime::DEFAULT_PIDS;
use crate::lifecycle::{Body, Exit, ResolvedMount, RunSpec};

fn digest() -> String {
    format!("sha256:{}", "ab".repeat(32))
}

/// Catches an image reference that reaches the store in a form it cannot fetch (a tag, or a
/// port taken for one) and one that is not pinned (no digest, the wrong algorithm, a short
/// or upper-case digest): the store is asked for `<repository>@<digest>` and nothing
/// else.
#[test]
fn an_image_is_pinned_by_its_digest_and_the_tag_is_dropped() {
    let d = digest();
    for (given, repository) in [
        ("registry.example/job@", "registry.example/job"),
        ("registry.example/job:1@", "registry.example/job"),
        ("registry.example:5000/job@", "registry.example:5000/job"),
        ("registry.example:5000/job:v2@", "registry.example:5000/job"),
        ("busybox@", "busybox"),
    ] {
        let image = pinned(&format!("{given}{d}")).expect("pinned");
        assert_eq!(image.reference, format!("{repository}@{d}"), "{given}");
        assert_eq!(image.digest, d);
    }
    let upper = format!("sha256:{}", "AB".repeat(32));
    let short = format!("sha256:{}", "ab".repeat(31));
    let other = format!("sha512:{}", "ab".repeat(32));
    for bad in [
        "registry.example/job:1".to_owned(),
        format!("registry.example/job@{upper}"),
        format!("registry.example/job@{short}"),
        format!("registry.example/job@{other}"),
        "registry.example/job@".to_owned(),
    ] {
        let why = pinned(&bad).expect_err(&bad);
        assert!(why.contains("not pinned"), "{bad}: {why}");
    }
}

fn lease() -> LeaseDirs {
    LeaseDirs {
        dir: PathBuf::from("/s/js-x-r1"),
        root: PathBuf::from("/s/js-x-r1/root"),
        upper: PathBuf::from("/s/js-x-r1/upper"),
        work: PathBuf::from("/s/js-x-r1/work"),
        home: PathBuf::from("/s/js-x-r1/rw/home"),
    }
}

fn run(pids: Option<u64>, files: Option<PathBuf>) -> RunSpec {
    RunSpec {
        run_id: "r1".to_owned(),
        body: Body::Container {
            image: "registry.example/job:1".to_owned(),
            args: vec!["--go".to_owned()],
        },
        cpu_millis: 1500,
        memory_bytes: 1 << 30,
        pids,
        gpu_indices: Vec::new(),
        mounts: vec![
            ResolvedMount {
                host: PathBuf::from("/data/in"),
                target: "/mnt/data/in".to_owned(),
                ro: true,
            },
            ResolvedMount {
                host: PathBuf::from("/data/out"),
                target: "/mnt/data/out".to_owned(),
                ro: false,
            },
        ],
        scratch: PathBuf::from("/scratch/r1"),
        files,
        slot: 3,
        isolation: js_agent_wire::Isolation::Contained,
        bundle: None,
        labels: BTreeMap::new(),
    }
}

/// Catches a run that becomes a different container than it asked for: the lease's name,
/// image, directories and cgroup; the run's scratch read-write at `/scratch` first, then
/// its mounts with the access they asked, then its files read-only; the job starting at the
/// exec root; no shared trees; the process limit the run named, or the default for one that
/// named none; no program or environment (the image config gives them).
#[test]
fn a_run_becomes_the_container_it_asked_for() {
    let image = PinnedImage {
        reference: format!("registry.example/job@{}", digest()),
        digest: digest(),
    };
    let spec = container_spec(
        "js-x-r1",
        &run(Some(77), Some(PathBuf::from("/run/files/r1"))),
        &image,
        &lease(),
        "/js/actions/js-x-r1",
    );
    assert_eq!(spec.name, "js-x-r1");
    assert_eq!(spec.image, image.reference);
    assert_eq!(spec.image_digest, digest());
    assert_eq!(spec.dir, PathBuf::from("/s/js-x-r1"));
    assert_eq!(spec.cgroup_parent, "/js/actions/js-x-r1");
    assert_eq!(spec.exec_root, EXEC_ROOT);
    assert_eq!(spec.input_root, PathBuf::from("/s/js-x-r1/root"));
    assert_eq!(spec.upper, PathBuf::from("/s/js-x-r1/upper"));
    assert_eq!(spec.overlay_work, PathBuf::from("/s/js-x-r1/work"));
    assert_eq!(spec.working_directory, "");
    assert!(spec.argv.is_empty() && spec.env.is_empty() && spec.shared.is_empty());
    assert_eq!(
        spec.limits,
        ContainerLimits {
            pids: 77,
            ..ContainerLimits::DEFAULT
        }
    );
    let bind = |target: &str, source: &str, read_only: bool| HostMount {
        target: target.to_owned(),
        source: PathBuf::from(source),
        read_only,
    };
    assert_eq!(
        spec.mounts,
        [
            bind("/scratch", "/scratch/r1", false),
            bind("/mnt/data/in", "/data/in", true),
            bind("/mnt/data/out", "/data/out", false),
            bind("/run/js-files", "/run/files/r1", true),
        ]
    );
    let plain = container_spec("js-x-r1", &run(None, None), &image, &lease(), "/c");
    assert_eq!(plain.limits.pids, DEFAULT_PIDS);
    assert_eq!(plain.mounts.len(), 3);
}

/// Catches a container's end that is read wrongly: an exit code kept as it is; a container
/// that did not run (crun's or the init's words, or crun not run at all) as the code 126;
/// a kill at the lease's own limit as 137 with the flag, whatever code crun gave; and a
/// kill the lease's limit did not cause (a busy node) without the flag.
#[test]
fn a_containers_end_is_an_exit_flagged_only_for_its_own_limit() {
    let plain = |code| Exit {
        code,
        oom_killed: false,
    };
    assert_eq!(exit_of(Ok(Ended::Exited(0)), None), plain(0));
    assert_eq!(exit_of(Ok(Ended::Exited(3)), None), plain(3));
    assert_eq!(
        exit_of(Ok(Ended::NotRun("exited 1".to_owned())), None),
        plain(NOT_RUN)
    );
    assert_eq!(exit_of(Err("no crun".to_owned()), None), plain(NOT_RUN));
    let own = OomOutcome::OwnLimit { used: 2, limit: 1 };
    assert_eq!(
        exit_of(Ok(Ended::Exited(1)), Some(&own)),
        Exit {
            code: 137,
            oom_killed: true
        }
    );
    let busy = OomOutcome::BusyNode("short".to_owned());
    assert_eq!(exit_of(Ok(Ended::Exited(137)), Some(&busy)), plain(137));
}

/// Catches the config directory moving away from where the image source is told to keep
/// the configs (both are named from the store's directory), and a clock that reads zero.
#[test]
fn the_configs_are_kept_beside_the_store_and_the_clock_reads_the_epoch() {
    assert_eq!(
        config_dir(&PathBuf::from("/s")),
        PathBuf::from("/s/oci-configs")
    );
    assert!(system_now_ms() > 1_700_000_000_000);
}

/// A cgroup mount of one test: `<mount>/actions/js-x-r1` is the lease's cgroup, with the
/// kernel's files written as the test says.
struct Fixture {
    mount: PathBuf,
}

impl Fixture {
    fn new(name: &str) -> Self {
        let mount = std::env::temp_dir().join(format!("js-crun-cg-{}-{name}", std::process::id()));
        let _ = fs::remove_dir_all(&mount);
        fs::create_dir_all(mount.join("actions/js-x-r1")).expect("make the lease's cgroup");
        Self { mount }
    }

    fn lease(&self) -> LeaseCgroup {
        LeaseCgroup::at(&self.mount, "/actions", "js-x-r1")
    }

    /// Writes the lease's `memory.events`, and the actions cgroup's `memory.events.local`
    /// when `actions_oom` is given.
    fn events(&self, max: u64, oom: u64, oom_kill: u64, actions_oom: Option<u64>) -> &Self {
        let events = format!("low 0\nhigh 0\nmax {max}\noom {oom}\noom_kill {oom_kill}\n");
        fs::write(self.mount.join("actions/js-x-r1/memory.events"), events).expect("events");
        if let Some(oom) = actions_oom {
            let local = format!("low 0\nhigh 0\nmax 0\noom {oom}\noom_kill {oom}\n");
            fs::write(self.mount.join("actions/memory.events.local"), local).expect("local");
        }
        self
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.mount);
    }
}

/// Proves, from the files the kernel writes and not from timing, that a kill is flagged
/// `oom_killed` only when the job's own memory limit made the OOM killer run: its
/// `memory.events` counts `oom` and `max` as well as `oom_kill`. Catches a runtime that
/// blames the job for a kill by the node: a parent cgroup's limit (the lease's `oom` is 0,
/// the actions cgroup's `memory.events.local` counts one) or the host (nothing counted
/// above), which would make the server retry with more memory for a job that did not
/// need it; and one that misses the own-limit kill, or flags a job whose `max` events
/// were met by reclaim (no process killed). Every case keeps the code crun gave, except
/// the own-limit kill, which is 137.
#[test]
fn only_the_jobs_own_memory_limit_flags_a_kill_from_the_cgroups_files() {
    let ok = |code: i32| Ok(Ended::Exited(code));
    let flagged = |code: i32, oom_killed: bool| Exit { code, oom_killed };

    let own = Fixture::new("own");
    own.events(4, 1, 2, Some(0));
    assert_eq!(
        end_of(ok(1), &own.lease(), Some(1 << 30), Some(0)),
        flagged(137, true)
    );

    let parent = Fixture::new("parent");
    parent.events(0, 0, 3, Some(1));
    assert_eq!(
        end_of(ok(137), &parent.lease(), Some(1 << 30), Some(0)),
        flagged(137, false)
    );

    let host = Fixture::new("host");
    host.events(0, 0, 3, Some(0));
    assert_eq!(
        end_of(ok(137), &host.lease(), Some(1 << 30), Some(0)),
        flagged(137, false)
    );

    let reclaimed = Fixture::new("reclaimed");
    reclaimed.events(9, 0, 0, Some(0));
    assert_eq!(
        end_of(ok(0), &reclaimed.lease(), Some(1 << 30), Some(0)),
        flagged(0, false)
    );

    let unread = Fixture::new("unread");
    assert_eq!(
        end_of(ok(5), &unread.lease(), Some(1 << 30), None),
        flagged(5, false)
    );
}

/// Log lines written by the test's subscriber on this thread.
#[derive(Clone, Default)]
struct Lines(std::sync::Arc<std::sync::Mutex<Vec<u8>>>);

impl std::io::Write for Lines {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        self.0.lock().expect("lock").extend_from_slice(bytes);
        Ok(bytes.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

/// Proves a lease whose `memory.events` cannot be read (here the path is a directory, so
/// the read fails) ends as crun gave it, never as an OOM kill, and that the failure is
/// logged as a warning naming the lease: a kill the runtime could not see must not pass
/// silently. Catches an `end_of` that drops the read error without a trace, and one that
/// flags the end when the file cannot say.
#[test]
fn a_lease_whose_memory_events_cannot_be_read_ends_unflagged_and_is_logged() {
    let lines = Lines::default();
    let writer = lines.clone();
    let subscriber = tracing_subscriber::fmt()
        .with_writer(move || writer.clone())
        .with_ansi(false)
        .with_max_level(tracing::Level::WARN)
        .finish();
    let _guard = tracing::subscriber::set_default(subscriber);
    tracing::callsite::rebuild_interest_cache();

    let broken = Fixture::new("unreadable");
    fs::create_dir(broken.mount.join("actions/js-x-r1/memory.events")).expect("unreadable file");
    assert_eq!(
        end_of(
            Ok(Ended::Exited(137)),
            &broken.lease(),
            Some(1 << 30),
            Some(0)
        ),
        Exit {
            code: 137,
            oom_killed: false
        }
    );
    let logged = String::from_utf8(lines.0.lock().expect("lock").clone()).expect("utf-8");
    assert!(logged.contains("WARN"), "{logged}");
    assert!(logged.contains("js-x-r1"), "{logged}");
    assert!(logged.contains("memory events"), "{logged}");
}

/// Proves the container's cgroup is `<lease>/ctr`: the path the usage sampler and the
/// integrity check accept for a direct runner's lease (their layout is `<actions>/<lease>/ctr`).
/// Catches a runtime that names the lease's own cgroup (usage then reads the booking's
/// parent, not the job) or any other child.
#[test]
fn the_containers_cgroup_is_ctr_below_the_lease() {
    let lease = Path::new("/sys/fs/cgroup/js/actions/js-x-r1");
    assert_eq!(
        ctr_of(lease),
        PathBuf::from("/sys/fs/cgroup/js/actions/js-x-r1/ctr")
    );
}

fn stream(bytes: &[u8], total: u64) -> StreamTail {
    StreamTail {
        bytes: bytes.to_vec(),
        total,
        file_cut: false,
    }
}

/// Proves the ring of js-contain becomes the result's tail: the two streams stay apart and
/// are text; a job that wrote nothing has an empty tail that is not truncated; one that
/// wrote more than the ring holds is truncated; and a ring that begins inside a character
/// loses that character's rest instead of showing replacement marks. Catches streams
/// merged or swapped, a tail that claims to be whole when bytes were dropped, and invalid
/// UTF-8 on the wire.
#[test]
fn the_ring_becomes_two_apart_texts_and_says_when_it_dropped_bytes() {
    let whole = Captured {
        stdout: stream(b"out\n", 4),
        stderr: stream(b"err\n", 4),
    };
    let tail = log_tail_of(&whole, None);
    assert_eq!(
        (tail.stdout.as_str(), tail.stderr.as_str()),
        ("out\n", "err\n")
    );
    assert!(!tail.truncated);
    assert_eq!(log_tail_of(&Captured::default(), None), Default::default());

    // "é" is 0xC3 0xA9: the ring begins at its second byte, and the job wrote 10 bytes.
    let begun = Captured {
        stdout: stream(b"\xA9tail", 10),
        stderr: stream(b"", 0),
    };
    let tail = log_tail_of(&begun, None);
    assert_eq!(tail.stdout, "tail");
    assert!(tail.truncated);

    let bad = Captured {
        stdout: stream(b"a\xFFb", 3),
        stderr: stream(b"", 0),
    };
    assert_eq!(log_tail_of(&bad, None).stdout, "a\u{FFFD}b");
}

/// Proves a tail is cut to the wire's limit from the front (the end is what is kept) at a
/// character boundary, and says it was cut, even when the ring held everything. Catches a
/// result over the wire's limit (the server refuses it) or one that keeps the start.
#[test]
fn a_tail_over_the_wire_limit_keeps_its_end() {
    use js_agent_wire::limits::MAX_LOG_STREAM_LEN;
    let mut bytes = "é".repeat(MAX_LOG_STREAM_LEN).into_bytes();
    bytes.extend_from_slice(b"end");
    let total = u64::try_from(bytes.len()).expect("a length");
    let captured = Captured {
        stdout: stream(&bytes, total),
        stderr: stream(b"", 0),
    };
    let tail = log_tail_of(&captured, None);
    assert!(tail.stdout.len() <= MAX_LOG_STREAM_LEN);
    assert!(tail.stdout.ends_with("end"));
    assert!(tail.truncated);
}

/// Proves a container that did not run says why in the result: the words of crun or the
/// init end the standard error, on a line of their own after what the job wrote, and the
/// standard output is untouched; a container that ran adds nothing. Catches a job that
/// fails with code 126 and nothing to read.
#[test]
fn a_container_that_did_not_run_gives_its_words_in_the_standard_error() {
    let written = Captured {
        stdout: stream(b"out", 3),
        stderr: stream(b"partial", 7),
    };
    let failed = Ok(Ended::NotRun("exited 1: no such file".to_owned()));
    let tail = log_tail_of(&written, not_run_words(&failed));
    assert_eq!(tail.stdout, "out");
    assert_eq!(tail.stderr, "partial\ncrun: exited 1: no such file\n");
    let unrun = Err("cannot start".to_owned());
    let tail = log_tail_of(&Captured::default(), not_run_words(&unrun));
    assert_eq!(tail.stderr, "crun: cannot start\n");
    assert_eq!(not_run_words(&Ok(Ended::Exited(1))), None);
    assert!(!log_tail_of(&written, None).stderr.contains("crun:"));
    // A stream that already ends in a newline gets no blank line before the words.
    let lined = Captured {
        stdout: stream(b"", 0),
        stderr: stream(b"partial\n", 8),
    };
    let tail = log_tail_of(&lined, Some("exited 1"));
    assert_eq!(tail.stderr, "partial\ncrun: exited 1\n");
}

/// A directory of one test, below the system's temporary directory.
fn scratch_dir(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("js-crun-dir-{}-{name}", std::process::id()));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).expect("make the test's directory");
    dir
}

/// Catches a directory made over one that is there (a stale lease), and one made without its
/// parent: `make_dir` makes a new directory of mode `0755` and refuses every other case with
/// the system's reason.
#[test]
fn a_directory_is_made_new_with_the_open_mode() {
    use std::os::unix::fs::PermissionsExt as _;
    let base = scratch_dir("make-dir");
    let fresh = base.join("fresh");
    super::make_dir(&fresh).expect("made new");
    let mode = fs::metadata(&fresh).expect("metadata").permissions().mode();
    assert_eq!(mode & 0o777, 0o755);
    let why = super::make_dir(&fresh).expect_err("there already");
    assert_eq!(why.kind(), std::io::ErrorKind::AlreadyExists);
    let why = super::make_dir(&base.join("no/such/parent")).expect_err("no parent");
    assert_eq!(why.kind(), std::io::ErrorKind::NotFound);
    let _ = fs::remove_dir_all(&base);
}

/// Catches a lease directory taken when it is not the lifecycle's bound one: it must be there,
/// be an empty directory (not a file, a link or a directory with something in it) and have the
/// project of the run's scratch directory, and its project is read for both, so an unreadable
/// one refuses it too. The one taken gets mode `0755`.
#[test]
fn a_lease_directory_is_taken_only_when_it_is_there_empty_and_in_the_runs_project() {
    use std::io::{Error, ErrorKind};
    use std::os::unix::fs::{PermissionsExt as _, symlink};
    let base = scratch_dir("take-dir");
    let run = base.join("run");
    let lease = base.join("lease");
    for dir in [&run, &lease] {
        fs::create_dir(dir).expect("a directory");
    }
    fs::set_permissions(&lease, fs::Permissions::from_mode(0o711)).expect("mode");
    let same = |_: &Path| Ok(7);
    super::take_dir(&lease, &run, &same).expect("taken");
    let mode = fs::metadata(&lease).expect("metadata").permissions().mode();
    assert_eq!(mode & 0o777, 0o755);

    let kind = |dir: &Path, project: &dyn Fn(&Path) -> std::io::Result<u32>| {
        super::take_dir(dir, &run, project)
            .expect_err("refused")
            .kind()
    };
    assert_eq!(kind(&base.join("missing"), &same), ErrorKind::NotFound);
    let full = base.join("full");
    fs::create_dir(&full).expect("full");
    fs::write(full.join("file"), b"x").expect("a file in it");
    let file = base.join("file");
    fs::write(&file, b"x").expect("a file");
    let link = base.join("link");
    symlink(&lease, &link).expect("a link");
    for refused in [&full, &file, &link] {
        assert_eq!(kind(refused, &same), ErrorKind::InvalidInput, "{refused:?}");
    }
    // The lease is outside the run's project.
    let apart = |dir: &Path| Ok(if dir == run { 7 } else { 0 });
    assert_eq!(kind(&lease, &apart), ErrorKind::PermissionDenied);
    // A project that cannot be read, of either directory.
    let broken_lease = |dir: &Path| {
        if dir == lease {
            Err(Error::other("lease"))
        } else {
            Ok(7)
        }
    };
    let broken_run = |dir: &Path| {
        if dir == run {
            Err(Error::other("run"))
        } else {
            Ok(7)
        }
    };
    assert_eq!(kind(&lease, &broken_lease), ErrorKind::Other);
    assert_eq!(kind(&lease, &broken_run), ErrorKind::Other);
    let _ = fs::remove_dir_all(&base);
}
