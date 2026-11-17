//! What [`bind_into`] asks of the kernel, against a fake that records each call in order
//! and fails (or panics at) the call a test names. [`Kernel`]'s own calls are tested in
//! `kernel_tests.rs`, and made into a running container by the root suite.

use std::sync::Mutex;

use super::inject::*;
use super::*;
use crate::container::HostMount;

/// A fake kernel: every call appends a line to `log`, and the call named in `fail` (its
/// name and which of its calls, from 0) answers an error `boom`.
#[derive(Default)]
struct Fake {
    log: Mutex<Vec<String>>,
    /// The call to fail: its name, and how many calls of that name pass first.
    fail: Option<(&'static str, usize)>,
    /// The call to panic at.
    panic_at: Option<&'static str>,
    /// The source `open_source` was last given, which `is_dir` answers for: a source whose
    /// name holds `file` is not a directory.
    last_source: Mutex<String>,
}

impl Fake {
    fn failing(name: &'static str, after: usize) -> Self {
        Self {
            fail: Some((name, after)),
            ..Self::default()
        }
    }

    fn panicking(name: &'static str) -> Self {
        Self {
            panic_at: Some(name),
            ..Self::default()
        }
    }

    /// Records `name` with what it was given, then answers.
    fn call(&self, name: &str, given: &str) -> io::Result<()> {
        let line = if given.is_empty() {
            name.to_owned()
        } else {
            format!("{name} {given}")
        };
        let mut log = self.log.lock().expect("log");
        let before = log
            .iter()
            .filter(|l| l.split(' ').next() == Some(name))
            .count();
        log.push(line);
        drop(log);
        assert_ne!(self.panic_at, Some(name), "the fake was told to panic");
        match self.fail {
            Some((fails, after)) if fails == name && before == after => {
                Err(io::Error::other("boom"))
            }
            _ => Ok(()),
        }
    }

    fn log(&self) -> Vec<String> {
        self.log.lock().expect("log").clone()
    }

    /// A descriptor to hand back.
    fn fd() -> OwnedFd {
        OwnedFd::from(io::pipe().expect("pipe").0)
    }
}

impl MountCalls for Fake {
    fn clone_tree(&self, _: BorrowedFd<'_>) -> io::Result<OwnedFd> {
        self.call("clone_tree", "").map(|()| Self::fd())
    }

    fn set_attributes(&self, _: BorrowedFd<'_>, attributes: Attributes) -> io::Result<()> {
        let given = format!(
            "read_only={} no_exec={}",
            attributes.read_only, attributes.no_exec
        );
        self.call("set_attributes", &given)
    }

    fn attach(&self, _: BorrowedFd<'_>, point: &Path) -> io::Result<()> {
        self.call("attach", &point.display().to_string())
    }

    fn mount_overlay(&self, _: &Path, _: bool, _: &str) -> io::Result<()> {
        self.call("mount_overlay", "")
    }

    fn detach(&self, _: &Path) -> io::Result<()> {
        self.call("detach", "")
    }

    fn is_mount_point(&self, _: &Path) -> io::Result<bool> {
        self.call("is_mount_point", "").map(|()| false)
    }

    fn bind_onto_itself(&self, _: &Path) -> io::Result<()> {
        self.call("bind_onto_itself", "")
    }

    fn make_private(&self, _: &Path) -> io::Result<()> {
        self.call("make_private", "")
    }
}

impl EnterCalls for Fake {
    fn open_source(&self, path: &Path) -> io::Result<OwnedFd> {
        *self.last_source.lock().expect("source") = path.display().to_string();
        self.call("open_source", &path.display().to_string())
            .map(|()| Self::fd())
    }

    fn is_dir(&self, _: BorrowedFd<'_>) -> io::Result<bool> {
        let dir = !self.last_source.lock().expect("source").contains("file");
        self.call("is_dir", "").map(|()| dir)
    }

    fn open_namespace(&self, pid: u32) -> io::Result<OwnedFd> {
        self.call("open_namespace", &pid.to_string())
            .map(|()| Self::fd())
    }

    fn open_root(&self, pid: u32) -> io::Result<OwnedFd> {
        self.call("open_root", &pid.to_string())
            .map(|()| Self::fd())
    }

    fn unshare_fs(&self) -> io::Result<()> {
        self.call("unshare_fs", "")
    }

    fn join(&self, _: BorrowedFd<'_>) -> io::Result<()> {
        self.call("join", "")
    }

    fn change_dir(&self, _: BorrowedFd<'_>) -> io::Result<()> {
        self.call("change_dir", "")
    }

    fn change_root(&self) -> io::Result<()> {
        self.call("change_root", "")
    }

    fn set_fs_ids(&self, ids: (u32, u32)) -> io::Result<()> {
        self.call("set_fs_ids", &format!("{}:{}", ids.0, ids.1))
    }

    fn make_dir(&self, path: &Path) -> io::Result<()> {
        self.call("make_dir", &path.display().to_string())
    }

    fn make_file(&self, path: &Path) -> io::Result<()> {
        self.call("make_file", &path.display().to_string())
    }
}

fn bind(target: &str, source: &str, read_only: bool, no_exec: bool) -> HostMount {
    HostMount {
        target: target.to_owned(),
        source: PathBuf::from(source),
        read_only,
        recursive: !read_only,
        no_exec,
    }
}

/// A directory bound writable at `/run/work/scratch`, then a file bound read-only and
/// `noexec` at `/run/cfg/file.txt`.
fn two_binds() -> Vec<HostMount> {
    vec![
        bind("/run/work/scratch", "/host/dir-a", false, false),
        bind("/run/cfg/file.txt", "/host/file-b", true, true),
    ]
}

const IDS: (u32, u32) = (1000, 1001);

/// The calls of one bind's preparation in the daemon's namespace.
fn prepare_lines(source: &str, read_only: bool, no_exec: bool) -> Vec<String> {
    vec![
        format!("open_source {source}"),
        "is_dir".to_owned(),
        "clone_tree".to_owned(),
        format!("set_attributes read_only={read_only} no_exec={no_exec}"),
    ]
}

fn lines(text: &[&str]) -> Vec<String> {
    text.iter().map(|l| (*l).to_owned()).collect()
}

/// Catches a bind made in the wrong order or namespace, a mount point made as root or left
/// after the ids were reset, an ancestor made after its child (or `/` made), a file source
/// given a directory, a clone attached before it was set, or the container entered before
/// every source was cloned: the exact calls for two binds, a directory and a file, are the
/// sources first, then the entering, then for each bind the job's ids, the mount point
/// from the first part down, the ids reset to the daemon's, and the attach.
#[test]
fn two_binds_are_cloned_first_then_made_and_attached_one_by_one_in_the_container() {
    let fake = Fake::default();
    bind_into(&fake, 77, &two_binds(), IDS).expect("bound");
    let mut want = prepare_lines("/host/dir-a", false, false);
    want.extend(prepare_lines("/host/file-b", true, true));
    want.extend(lines(&[
        "open_namespace 77",
        "open_root 77",
        "unshare_fs",
        "join",
        "change_dir",
        "change_root",
        "set_fs_ids 1000:1001",
        "make_dir /run",
        "make_dir /run/work",
        "make_dir /run/work/scratch",
        "set_fs_ids 0:0",
        "attach /run/work/scratch",
        "set_fs_ids 1000:1001",
        "make_dir /run",
        "make_dir /run/cfg",
        "make_file /run/cfg/file.txt",
        "set_fs_ids 0:0",
        "attach /run/cfg/file.txt",
    ]));
    assert_eq!(fake.log(), want);
}

/// Catches a non-recursive bind refused, or one that changes what is asked of the clone:
/// it is cloned and set like any other.
#[test]
fn a_bind_that_is_not_recursive_is_cloned_like_the_others() {
    let fake = Fake::default();
    let mut one = bind("/run/x", "/host/dir-x", true, false);
    one.recursive = false;
    bind_into(&fake, 5, &[one], IDS).expect("bound");
    let log = fake.log();
    assert_eq!(log[..4], prepare_lines("/host/dir-x", true, false));
    assert_eq!(log.last().map(String::as_str), Some("attach /run/x"));
}

/// Catches a container entered for nothing: with no binds no call is made and the answer
/// is `Ok`.
#[test]
fn no_binds_enter_nothing() {
    let fake = Fake::default();
    bind_into(&fake, 77, &[], IDS).expect("nothing to do");
    assert_eq!(fake.log(), Vec::<String>::new());
}

/// Catches a target that is not absolute and normalized getting through to a call: each is
/// refused with its text, before any call, even when a good bind comes first.
#[test]
fn a_target_that_is_not_absolute_and_normalized_is_refused_before_any_call() {
    for target in [
        "run/work",
        "",
        "/",
        "//run",
        "/run//work",
        "/run/",
        "/run/./work",
        "/run/../etc",
        "/..",
        ".",
    ] {
        let fake = Fake::default();
        let binds = [
            bind("/run/good", "/host/dir-a", false, false),
            bind(target, "/host/dir-b", false, false),
        ];
        let why = bind_into(&fake, 77, &binds, IDS).expect_err(target);
        assert!(matches!(why, InjectError::BadTarget { .. }), "{target}");
        assert_eq!(
            why.to_string(),
            format!("the target {target:?} is not an absolute, normalized path below /")
        );
        assert_eq!(fake.log(), Vec::<String>::new(), "{target}");
    }
}

/// Catches a failure of the daemon-side steps that goes on to enter the container, or is
/// not named: each of the four calls, failing for the second bind, stops everything before
/// the container is entered, and the error says the step, the bind's target and the answer.
#[test]
fn a_source_that_cannot_be_prepared_fails_before_anything_is_entered() {
    let cases = [
        (
            "open_source",
            "open the source of /run/cfg/file.txt: boom",
            1,
        ),
        (
            "is_dir",
            "find whether the source is a directory, for /run/cfg/file.txt: boom",
            2,
        ),
        (
            "clone_tree",
            "clone the source of /run/cfg/file.txt: boom",
            3,
        ),
        (
            "set_attributes",
            "set the clone's attributes for /run/cfg/file.txt: boom",
            4,
        ),
    ];
    for (name, text, made) in cases {
        let fake = Fake::failing(name, 1);
        let why = bind_into(&fake, 77, &two_binds(), IDS).expect_err(name);
        assert_eq!(why.to_string(), text);
        let log = fake.log();
        assert_eq!(log.len(), 4 + made, "{name}: {log:?}");
        assert!(
            log.iter().all(|l| !l.starts_with("open_namespace")),
            "{name}: {log:?}"
        );
    }
}

/// Catches an entering call whose failure is passed over, or goes on to make a mount
/// point: each of the six calls that enter the container, failing, ends the work at that
/// call with its name and the process in the message, and nothing is made or attached.
#[test]
fn a_call_that_enters_the_container_stops_the_work_there() {
    let steps = [
        "open_namespace",
        "open_root",
        "unshare_fs",
        "join",
        "change_dir",
        "change_root",
    ];
    for (at, name) in steps.into_iter().enumerate() {
        let fake = Fake::failing(name, 0);
        let why = bind_into(&fake, 77, &two_binds(), IDS).expect_err(name);
        let label = name.replace('_', " ");
        assert_eq!(
            why.to_string(),
            format!("enter the namespace of process 77 ({label}): boom")
        );
        let log = fake.log();
        assert_eq!(log.len(), 8 + at + 1, "{name}: {log:?}");
        assert_eq!(log.last().map(|l| l.split(' ').next()), Some(Some(name)));
    }
}

/// Catches a failure at the mount point, the ids or the attach that is not named with the
/// bind it was for, or that lets the next step run: each stops there, with the binds
/// attached before it left as they are.
#[test]
fn a_step_that_fails_in_the_container_stops_there_and_names_its_target() {
    // (call, which of its calls fails, the text, the calls made in the container).
    let cases: [(&str, usize, &str, &[&str]); 7] = [
        (
            "set_fs_ids",
            0,
            "become the job's user to make /run/work/scratch: boom",
            &["set_fs_ids 1000:1001"],
        ),
        (
            "make_dir",
            1,
            "make the mount point /run/work/scratch: boom",
            &[
                "set_fs_ids 1000:1001",
                "make_dir /run",
                "make_dir /run/work",
            ],
        ),
        (
            "set_fs_ids",
            1,
            "become the daemon's user again after making /run/work/scratch: boom",
            &[
                "set_fs_ids 1000:1001",
                "make_dir /run",
                "make_dir /run/work",
                "make_dir /run/work/scratch",
                "set_fs_ids 0:0",
            ],
        ),
        (
            "attach",
            0,
            "attach the clone at /run/work/scratch: boom",
            &[
                "set_fs_ids 1000:1001",
                "make_dir /run",
                "make_dir /run/work",
                "make_dir /run/work/scratch",
                "set_fs_ids 0:0",
                "attach /run/work/scratch",
            ],
        ),
        (
            "make_file",
            0,
            "make the mount point /run/cfg/file.txt: boom",
            &[
                "set_fs_ids 1000:1001",
                "make_dir /run",
                "make_dir /run/work",
                "make_dir /run/work/scratch",
                "set_fs_ids 0:0",
                "attach /run/work/scratch",
                "set_fs_ids 1000:1001",
                "make_dir /run",
                "make_dir /run/cfg",
                "make_file /run/cfg/file.txt",
            ],
        ),
        (
            "attach",
            1,
            "attach the clone at /run/cfg/file.txt: boom",
            &[
                "set_fs_ids 1000:1001",
                "make_dir /run",
                "make_dir /run/work",
                "make_dir /run/work/scratch",
                "set_fs_ids 0:0",
                "attach /run/work/scratch",
                "set_fs_ids 1000:1001",
                "make_dir /run",
                "make_dir /run/cfg",
                "make_file /run/cfg/file.txt",
                "set_fs_ids 0:0",
                "attach /run/cfg/file.txt",
            ],
        ),
        (
            "set_fs_ids",
            2,
            "become the job's user to make /run/cfg/file.txt: boom",
            &[
                "set_fs_ids 1000:1001",
                "make_dir /run",
                "make_dir /run/work",
                "make_dir /run/work/scratch",
                "set_fs_ids 0:0",
                "attach /run/work/scratch",
                "set_fs_ids 1000:1001",
            ],
        ),
    ];
    let entered = [
        "open_namespace 77",
        "open_root 77",
        "unshare_fs",
        "join",
        "change_dir",
        "change_root",
    ];
    for (name, after, text, made) in cases {
        let fake = Fake::failing(name, after);
        let why = bind_into(&fake, 77, &two_binds(), IDS).expect_err(text);
        assert_eq!(why.to_string(), text);
        assert!(matches!(why, InjectError::Step { .. }));
        let log = fake.log();
        let mut want = prepare_lines("/host/dir-a", false, false);
        want.extend(prepare_lines("/host/file-b", true, true));
        want.extend(lines(&entered));
        want.extend(lines(made));
        assert_eq!(log, want, "{text}");
    }
}

/// Catches a panic in the entering thread that unwinds into the caller: it is an error
/// naming the process.
#[test]
fn a_panic_of_the_thread_is_an_error() {
    let fake = Fake::panicking("change_root");
    let why = bind_into(&fake, 77, &two_binds(), IDS).expect_err("panic");
    assert!(matches!(why, InjectError::Panicked { pid: 77 }));
    assert_eq!(
        why.to_string(),
        "the thread that binds into the namespace of process 77 panicked"
    );
}

/// Catches the error keeping the kernel's answer from its caller: the source of a step and
/// of an entering failure is the `io::Error`.
#[test]
fn an_error_carries_the_kernels_answer_as_its_source() {
    use std::error::Error as _;
    let fake = Fake::failing("clone_tree", 0);
    let why = bind_into(&fake, 77, &two_binds(), IDS).expect_err("clone");
    assert_eq!(why.source().expect("source").to_string(), "boom");
    let fake = Fake::failing("join", 0);
    let why = bind_into(&fake, 77, &two_binds(), IDS).expect_err("join");
    assert_eq!(why.source().expect("source").to_string(), "boom");
}

/// The fake's answers for the calls `bind_into` never makes, so that the whole fake is
/// exercised: each records itself and says yes.
#[test]
fn the_fake_records_the_mount_calls_bind_into_never_makes() {
    let fake = Fake::default();
    let nowhere = Path::new("/nowhere");
    fake.mount_overlay(nowhere, false, "").expect("overlay");
    fake.detach(nowhere).expect("detach");
    assert!(!fake.is_mount_point(nowhere).expect("mount point"));
    fake.bind_onto_itself(nowhere).expect("bind");
    fake.make_private(nowhere).expect("private");
    assert_eq!(fake.log().len(), 5);
}
