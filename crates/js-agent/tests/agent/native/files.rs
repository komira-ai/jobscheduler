//! Run files on a Mac: a native start's `files` written by the lifecycle into the agent's
//! files root, given to the run's user before its program starts, denied to its writes,
//! named by `JS_FILES_DIR`, kept out of every log line and error, refused with a code when
//! they cannot be staged, and claimed in the hello exactly when the agent has a root.

use std::fs;
use std::io;
use std::os::unix::fs::{MetadataExt as _, PermissionsExt as _};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, PoisonError};

use clap::Parser as _;
use js_agent::Cli;
use js_agent::lifecycle::{DirLayout, Local, Runtime as _};
use js_agent::native::HandOver;
use js_agent_wire::{JobSpec, Outcome, OutputKind, OutputSpec, RunFile};
use js_testkit::dir::TestDir;
use serde_json::Value;

use super::agent::{hello_on, native_flags, private};
use super::fake_helper::FakeHelper;
use super::lifecycle::{Native, job};
use super::{Rig, native, parse};
use crate::support::{Scratch, code, mac_host, welcome};

/// What the files of the tests hold: no log line, error or `Debug` may show it.
const MARKER: &str = "SECRET-MARKER-9f3a";

/// The paths a [`HandOver`] was asked to give away, with the user and the group.
type Handed = Arc<Mutex<Vec<(PathBuf, u32, u32)>>>;

/// A [`HandOver`] that records what it is asked and gives nothing away (only root can).
fn recording() -> (HandOver, Handed) {
    let handed: Handed = Arc::default();
    let log = Arc::clone(&handed);
    let seam: HandOver = Arc::new(move |path: &Path, uid, gid| {
        let mut log = log.lock().unwrap_or_else(PoisonError::into_inner);
        log.push((path.to_path_buf(), uid, gid));
        Ok(())
    });
    (seam, handed)
}

/// `spec` with the run files `files`, each a name and its contents.
fn carrying(mut spec: JobSpec, files: &[(&str, &str)]) -> JobSpec {
    spec.files = files
        .iter()
        .map(|(name, contents)| RunFile {
            name: (*name).to_owned(),
            contents: (*contents).to_owned(),
        })
        .collect();
    spec
}

fn mode(path: &Path) -> u32 {
    fs::symlink_metadata(path).unwrap().permissions().mode() & 0o7777
}

/// The files root of the lifecycle of `h` (the test directory's `files`), canonical.
fn root_of(h: &Native) -> PathBuf {
    fs::canonicalize(h.dir.path("files")).unwrap()
}

/// The lines of the lease journal of `h`, empty when it was never written.
fn journal_of(h: &Native) -> Vec<String> {
    let text = fs::read_to_string(h.dir.path("native-leases")).unwrap_or_default();
    text.lines().map(str::to_owned).collect()
}

/// The uid the fake helper answers `user-create` with.
fn helper_uid() -> u32 {
    rustix::process::getuid().as_raw()
}

/// Asserts that a native start's files are the run's user's alone before its program runs:
/// the run's directory is `0500` and each file `0400` (nothing for group or others), each
/// file and then the directory are given to the uid the helper made for the run (with the
/// directory's group), the contents are as sent, and a second run's directory is given to
/// its own user the same way. It catches secrets left world-readable (`0444`, `0555`), a
/// hand-over that is skipped or given to another uid, and a directory given away before its
/// files.
#[test]
fn files_are_readable_by_the_job_user_only() {
    let (seam, handed) = recording();
    let mut h = Native::with_files("files-modes", |_| {}, Some(seam));
    let two = [
        ("grant", "token-1\n"),
        ("put-url", "https://store.example/p"),
    ];
    assert_eq!(
        h.start(carrying(job("r1", "/bin/cat"), &two)),
        ["started r1"]
    );
    let dir = root_of(&h).join("r1");
    assert_eq!(mode(&dir), 0o500);
    for (name, contents) in two {
        assert_eq!(mode(&dir.join(name)), 0o400, "{name}");
        assert_eq!(fs::read_to_string(dir.join(name)).unwrap(), contents);
    }
    let gid = fs::metadata(&dir).unwrap().gid();
    let uid = helper_uid();
    let mut seen = handed.lock().unwrap().clone();
    assert_eq!(
        seen.pop(),
        Some((dir.clone(), uid, gid)),
        "the directory last"
    );
    seen.sort();
    let files: Vec<_> = two.map(|(name, _)| (dir.join(name), uid, gid)).into();
    assert_eq!(seen, files);
    assert_eq!(h.fake.verbs()[1..], ["user-create 7.0", "run 7.0"]);

    handed.lock().unwrap().clear();
    let one = [("grant", "token-2\n")];
    assert_eq!(
        h.start(carrying(job("r2", "/bin/cat"), &one)),
        ["started r2"]
    );
    let other = root_of(&h).join("r2");
    assert_eq!((mode(&other), mode(&other.join("grant"))), (0o500, 0o400));
    let want = [(other.join("grant"), uid, gid), (other.clone(), uid, gid)];
    assert_eq!(*handed.lock().unwrap(), want);
}

/// Asserts what the run is told of its files and what its profile lets it do there: the
/// environment the helper receives names the directory in `JS_FILES_DIR`; the profile
/// denies writes to exactly that directory (as an input, `IN_0`), no parameter of the
/// profile is that directory or a parent of it but `IN_0` (so no write grant and no read
/// deny covers it), no write allow names `IN_0`, and reads are open (`(allow default)`, the
/// open read model). A job's own `JS_FILES_DIR` is refused as a reserved name before a user
/// is made. It catches a run that cannot find its files, a grant that covers the files root
/// or a parent, a profile that lets the run rewrite its grant, and a job that points the
/// variable elsewhere.
#[test]
fn the_profile_grants_read_of_the_files_directory_and_no_write_there() {
    let mut h = Native::with_files("files-profile", |_| {}, None);
    let spec = carrying(job("r1", "/bin/cat"), &[("grant", "token")]);
    assert_eq!(h.start(spec), ["started r1"]);
    let dir = root_of(&h).join("r1").display().to_string();
    let run = h.fake.run_of("7.0");
    let named = ("JS_FILES_DIR".to_owned(), dir.clone());
    assert!(run.env.contains(&named), "{:?}", run.env);
    let parsed = parse(&run.argv);
    let covering: Vec<&String> = parsed
        .params
        .iter()
        .filter(|(_, value)| Path::new(&dir).starts_with(value))
        .map(|(name, _)| name)
        .collect();
    assert_eq!(covering, ["IN_0"], "{:?}", parsed.params);
    let deny = "(deny file-write* (subpath (param \"IN_0\")))";
    assert!(parsed.profile.contains(deny), "{}", parsed.profile);
    for line in parsed.profile.lines() {
        let grant = line.starts_with("(allow file-write") || line.starts_with("(allow file*");
        assert!(!(grant && line.contains("IN_0")), "{line}");
    }
    assert!(parsed.profile.contains("(allow default)"));
    assert_eq!(mode(Path::new(&dir)), 0o500, "lchown to the same uid");

    let mut rig = Rig::new("files-env");
    let mut body = native("/bin/cat", &[]);
    body.env.insert("JS_FILES_DIR".to_owned(), "/".to_owned());
    let spec = rig.spec("r1", body);
    let why = rig.rt.create_and_start(&spec).unwrap_err();
    assert_eq!(why.0, "native.env: JS_FILES_DIR is a reserved name");
    assert_eq!(rig.fake.verbs(), ["boot-reset"]);
}

/// A writer the tests' subscriber writes every log line into.
#[derive(Clone, Default)]
struct Captured(Arc<Mutex<Vec<u8>>>);

impl io::Write for Captured {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        let mut held = self.0.lock().unwrap_or_else(PoisonError::into_inner);
        held.extend_from_slice(bytes);
        Ok(bytes.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

/// Asserts that the contents of run files never reach a log line, an event, an error or
/// the `Debug` of the start and the run: a start whose file and output hold a marker runs,
/// has an output copied and one refused (a link) and is removed, under a subscriber that
/// captures every `tracing` event; a second runtime fails a start at the hand-over and a
/// removal at an output over its cap. The marker is in none of the captured lines, the
/// events, the `JobSpec`, the `RunSpec` and the errors, while the file name and its length
/// (`contents_len`) and the output names are. It catches a new log line or error that
/// prints a run file's contents or an output's bytes.
#[test]
fn file_contents_never_appear_in_debug_output() {
    let captured = Captured::default();
    let writer = captured.clone();
    let subscriber = tracing_subscriber::fmt()
        .with_max_level(tracing::Level::TRACE)
        .with_writer(move || writer.clone())
        .finish();
    let mut printed: Vec<String> = Vec::new();
    tracing::subscriber::with_default(subscriber, || {
        let mut h = Native::with_files("files-debug", |_| {}, None);
        let mut spec = carrying(job("r1", "/bin/cat"), &[("grant", MARKER)]);
        let declared = [("out.txt", OutputKind::File), ("link", OutputKind::File)];
        spec.native.as_mut().unwrap().outputs = declared
            .iter()
            .map(|(path, kind)| OutputSpec {
                path: (*path).to_owned(),
                kind: *kind,
            })
            .collect();
        printed.push(format!("{spec:?}"));
        let mut events = h.life.apply(&crate::lifecycle::reply(vec![
            js_agent_wire::Command::Start(Box::new(spec)),
        ]));
        let exec = h.fake.home_of("7.0").join("exec");
        fs::write(exec.join("out.txt"), MARKER).unwrap();
        let grant = root_of(&h).join("r1").join("grant");
        std::os::unix::fs::symlink(&grant, exec.join("link")).unwrap();
        h.fake.exit("7.0", 0);
        events.extend(h.until(|life| life.busy_slots().is_empty()));
        printed.push(format!("{events:?}"));
        printed.push(format!("{:?}", h.life.results()));

        let refusing: HandOver = Arc::new(|_: &Path, _, _| Err(io::Error::other("no")));
        let mut rig = Rig::new("files-debug-rt").handing_over(refusing);
        let mut spec = rig.spec("r1", native("/bin/cat", &[]));
        let dir = rig.dir.path("files-r1");
        fs::create_dir(&dir).unwrap();
        fs::write(dir.join("grant"), MARKER).unwrap();
        spec.files = Some(dir);
        printed.push(format!("{spec:?}"));
        printed.push(format!("{:?}", rig.rt.create_and_start(&spec).unwrap_err()));
        let mut rig = Rig::with("files-debug-cap", |config| config.max_file_bytes = 4);
        let mut body = native("/bin/cat", &[]);
        body.outputs = vec![OutputSpec {
            path: "out.txt".to_owned(),
            kind: OutputKind::File,
        }];
        let spec = rig.spec("r1", body);
        let id = rig.rt.create_and_start(&spec).unwrap();
        fs::write(rig.fake.home_of("7.0").join("exec/out.txt"), MARKER).unwrap();
        let removed = rig.rt.remove(&id);
        printed.push(format!("{:?}", removed.unwrap_err()));
    });
    let logged = String::from_utf8(captured.0.lock().unwrap().clone()).unwrap();
    assert!(
        logged.contains("output \"out.txt\" copied, 18 bytes"),
        "{logged}"
    );
    assert!(
        logged.contains("\"link: a symbolic link: not followed\""),
        "{logged}"
    );
    printed.push(logged);
    for text in &printed {
        assert!(!text.contains(MARKER), "the marker leaked: {text}");
    }
    let all = printed.join("\n");
    for told in [
        "\"grant\"",
        "contents_len: 18",
        "outputs-failed",
        "the files: no",
    ] {
        assert!(all.contains(told), "{told} is not told: {all}");
    }
}

/// Asserts the two ways the staging of a start's files fails. When the files cannot be
/// written (the files root is a file, not a directory) the start ends `failed_to_start`
/// with `scratch-failed`, the code of a failed write of a run's directories, before the
/// helper or the journal hears of it: no `user-create`, no journal line, and the booking is
/// given back once the cleanup ran. When the files cannot be given to the user (the
/// hand-over fails) the start ends `runtime-failed`, the user is killed and deleted, the
/// journal says the lease is done, and the files directory goes with the run. It catches a
/// lease user or a journal entry left by a start that never ran, and a start reported
/// running whose files its user cannot read.
#[test]
fn a_failed_staging_ends_the_start_with_a_code() {
    let mut h = Native::with_files("files-stage-fail", |_| {}, None);
    let root = root_of(&h);
    fs::remove_dir(&root).unwrap();
    fs::write(&root, "not a directory").unwrap();
    let spec = carrying(job("r1", "/bin/cat"), &[("grant", "token")]);
    let events = h.start(spec);
    assert_eq!(events, ["finding r1 scratch-failed", "result r1"]);
    let result = &h.life.results()[0];
    assert_eq!(result.outcome, Outcome::FailedToStart);
    assert_eq!(result.detail.as_deref(), Some("scratch-failed"));
    assert_eq!(h.fake.verbs(), ["boot-reset"]);
    assert!(journal_of(&h).is_empty(), "{:?}", journal_of(&h));
    fs::remove_file(&root).unwrap();
    fs::create_dir(&root).unwrap();
    fs::set_permissions(&root, fs::Permissions::from_mode(0o700)).unwrap();
    h.until(|life| life.busy_slots().is_empty());
    assert_eq!(h.life.free().cpu_millis, 4000);
    assert!(!h.life.is_tainted());

    let refusing: HandOver = Arc::new(|_: &Path, _, _| Err(io::Error::other("refused")));
    let mut h = Native::with_files("files-hand-fail", |_| {}, Some(refusing));
    let spec = carrying(job("r1", "/bin/cat"), &[("grant", "token")]);
    assert_eq!(h.start(spec), ["finding r1 start-failed", "result r1"]);
    let result = &h.life.results()[0];
    assert_eq!(result.outcome, Outcome::FailedToStart);
    assert_eq!(result.detail.as_deref(), Some("runtime-failed"));
    assert_eq!(
        h.fake.verbs(),
        [
            "boot-reset",
            "user-create 7.0",
            "kill-uid 7.0",
            "user-delete 7.0"
        ]
    );
    assert_eq!(journal_of(&h), ["+ 7.0", "- 7.0"]);
    h.until(|life| life.busy_slots().is_empty());
    assert!(!root_of(&h).join("r1").exists());
    assert_eq!(h.life.free().cpu_millis, 4000);
}

/// Asserts that a files directory the profile cannot name (spelled with `..`, which a
/// profile parameter may not hold) fails the start by name after the files were given to
/// the user, and that the user is removed. It catches a run started with its files outside
/// the profile's write deny.
#[test]
fn a_files_directory_the_profile_cannot_name_fails_the_start() {
    let (seam, handed) = recording();
    let mut rig = Rig::new("files-odd").handing_over(seam);
    let dir = rig.dir.path("files");
    fs::create_dir_all(dir.join("r1")).unwrap();
    let mut spec = rig.spec("r1", native("/bin/cat", &[]));
    spec.files = Some(dir.join("..").join("files").join("r1"));
    let why = rig.rt.create_and_start(&spec).unwrap_err();
    assert_eq!(why.0, "the files: path is not in normal form");
    assert_eq!(
        handed.lock().unwrap().len(),
        1,
        "the directory was handed over"
    );
    assert_eq!(rig.fake.verbs().last().unwrap(), "user-delete 7.0");
}

/// Asserts that a native agent without a files root refuses a start with files with
/// `files-unsupported`, the code the Linux agent gives, before anything is made: the helper
/// sees no user, no scratch directory is made and the booking is intact. It catches files
/// accepted by a host that has nowhere to keep them.
#[test]
fn a_native_start_with_files_and_no_files_root_is_refused_files_unsupported() {
    let mut h = Native::with_files("files-none", |config| config.files = None, None);
    let spec = carrying(job("r1", "/bin/cat"), &[("grant", "token")]);
    assert_eq!(h.start(spec), ["refused r1 files-unsupported"]);
    assert_eq!(h.fake.verbs(), ["boot-reset"]);
    assert!(!h.scratch_of("r1").exists());
    assert_eq!(h.life.free().cpu_millis, 4000);
    let result = &h.life.results()[0];
    assert_eq!(result.detail.as_deref(), Some("files-unsupported"));
}

/// A new directory below `/tmp`, searchable by everyone: a searchable root needs every
/// directory above it searchable.
fn world(name: &str) -> TestDir {
    let dir = TestDir::under("/tmp", name);
    fs::set_permissions(dir.path(), fs::Permissions::from_mode(0o755)).unwrap();
    dir
}

/// Asserts the hello of a native agent: given `--run-files-root` (on a disk, which a native
/// agent takes and a container agent refuses) it reports `run-files` beside `runtime` and
/// `exec` as `sandbox-exec`, and the root it made is `0711`; without the flag it reports no
/// `run-files`. Neither names the container runtime. It catches a Mac that claims the
/// capability with nowhere to keep the secrets (the server would send them), one that never
/// claims it, and a Mac refused for a root that is not a tmpfs, which it cannot have.
#[test]
fn a_native_agent_with_a_files_root_reports_run_files_and_without_one_does_not() {
    let world = world("native-hello-files");
    let files = world.path().join("files");
    for given in [true, false] {
        let dir = Scratch::new(&format!("native-hello-files-{given}"));
        let fake = FakeHelper::start(&dir);
        let mut extra = native_flags(&dir, &fake);
        if given {
            extra.extend(["--run-files-root".to_owned(), files.display().to_string()]);
        }
        let mut host = mac_host();
        host.files = host.files.clone().disk_under(world.path());
        let (ran, hello) = hello_on(&host, &extra, welcome(1));
        assert_eq!(ran.status(), code(0), "{given}: {}", ran.err);
        let caps = &hello["capabilities"];
        let want = if given { "true".into() } else { Value::Null };
        assert_eq!(caps["run-files"], want, "{given}");
        assert_eq!(caps["runtime"], "sandbox-exec", "{given}");
        assert_eq!(caps["exec"], "sandbox-exec", "{given}");
        assert_eq!(caps["per-job"], Value::Null, "{given}");
    }
    assert_eq!(mode(&files), 0o711);
}

/// Asserts how a native agent's flags open its files root: given, it is opened searchable
/// (`0711`, made when missing), so the run's user can walk down to its own directory;
/// without the flag there is no root and no warning (there is no default on a Mac); and a
/// root that is, holds or lies in the homes of the native users or a path no native run may
/// read (`--native-deny-read`) is refused naming it. It catches a root the run's user cannot
/// reach, a default the Mac does not have, a root the helper's sweep of the homes would
/// delete, and files the run's profile would deny it.
#[test]
fn a_native_files_root_is_searchable_and_apart_from_what_runs_may_not_read() {
    let world = world("native-files-flags");
    let (homes, denied) = (world.path().join("homes"), world.path().join("denied"));
    fs::create_dir(&homes).unwrap();
    fs::create_dir(&denied).unwrap();
    let dir = Scratch::new("native-files-flags");
    let (state, scratch) = (private(&dir, "state"), private(&dir, "scratch"));
    let cli = |files: Option<&Path>| {
        let mut argv: Vec<String> = [
            "js-agent",
            "--server",
            "http://127.0.0.1:1",
            "--pool",
            "macs",
            "--native-helper-socket",
            "/nonexistent/js-helper",
        ]
        .map(str::to_owned)
        .into();
        argv.extend(["--native-homes".to_owned(), homes.display().to_string()]);
        argv.extend([
            "--native-deny-read".to_owned(),
            denied.display().to_string(),
        ]);
        argv.extend(["--state-root".to_owned(), state.clone()]);
        argv.extend(["--scratch-root".to_owned(), scratch.clone()]);
        if let Some(files) = files {
            argv.extend(["--run-files-root".to_owned(), files.display().to_string()]);
        }
        Cli::try_parse_from(argv).unwrap()
    };
    let files = world.path().join("files");
    let local = Local::from_cli(&cli(Some(&files)), 0).unwrap();
    let root = local.files.as_ref().expect("a files root");
    assert_eq!(root.layout(), DirLayout::Searchable);
    assert_eq!(root.path(), fs::canonicalize(&files).unwrap());
    assert_eq!(mode(&files), 0o711);

    let local = Local::from_cli(&cli(None), 0).unwrap();
    assert_eq!((local.files, local.files_warning), (None, None));

    // The read denies are compared before the homes, so a root holding both names the deny.
    let (homes, denied) = (
        fs::canonicalize(&homes).unwrap(),
        fs::canonicalize(&denied).unwrap(),
    );
    let cases = [
        (homes.join("files"), "the --native-homes", &homes),
        (
            denied.join("files"),
            "the path no native run may read",
            &denied,
        ),
        (
            world.path().to_path_buf(),
            "the path no native run may read",
            &denied,
        ),
    ];
    for (files, named, other) in cases {
        fs::create_dir_all(&files).unwrap();
        fs::set_permissions(&files, fs::Permissions::from_mode(0o711)).unwrap();
        let why = Local::from_cli(&cli(Some(&files)), 0).unwrap_err();
        let said = format!(
            "--run-files-root: is, holds or lies in {named} {}: the roots must be separate \
             directories",
            other.display()
        );
        assert_eq!(why, said, "{}", files.display());
    }
}

/// Asserts that a native start whose files the lifecycle writes is given its files through
/// the agent's own hand-over (`lchown`, which a user may make to itself) and that the run's
/// directory goes with the run: after the run is removed nothing is left in the files root.
/// It catches a files directory that outlives its run.
#[test]
fn a_native_runs_files_directory_goes_with_the_run() {
    let mut h = Native::with_files("files-cleanup", |_| {}, None);
    let spec = carrying(job("r1", "/bin/cat"), &[("grant", "token")]);
    assert_eq!(h.start(spec), ["started r1"]);
    assert!(root_of(&h).join("r1").is_dir());
    h.fake.exit("7.0", 0);
    h.until(|life| life.busy_slots().is_empty());
    assert_eq!(fs::read_dir(root_of(&h)).unwrap().count(), 0);
}
