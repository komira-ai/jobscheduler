//! The file system of a job on the `crun` runtime, as an ordinary user: the pool, the calls
//! that read a host path and the list of a run's mounts here, the assembly in
//! `assemble_tests` and [`super::Fs`] in `fs_tests`, over
//! directories of the test's own, with a fake of the mounts' calls ([`Mounts`], which
//! records each call and binds a clone's directories by copying them) and of the reads of
//! host paths ([`Paths`], which can hold a lookup or a check until the test lets it go).
//! Every bound is passed on the caller's clock (`now`), never waited for.

use std::fs;
use std::io;
use std::os::fd::{AsRawFd as _, BorrowedFd, OwnedFd};
use std::os::unix::fs::{MetadataExt as _, PermissionsExt as _, symlink};
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::sync::mpsc::{Receiver, Sender, channel};

use js_agent_wire::{JobMount, Layer, MountMode, MountSource, Propagation};
use js_contain::job_fs::{Attributes, MountCalls, ResolveError};
use js_testkit::dir::TestDir;

use super::assemble::Owners;
use super::kernel::{
    Kernel, PathCalls, RESOLVE_ATTEMPTS, become_user, in_root_with, missing_call, reach,
    search_with, unreadable,
};
use super::list;
use super::pool::{Poll, Pool};

/// A directory of the test's own.
pub(super) fn scratch(name: &str) -> TestDir {
    TestDir::under(std::env::temp_dir(), &format!("js-crun-fs-{name}"))
}

/// This process's uid and gid.
pub(super) fn own() -> (u32, u32) {
    (
        rustix::process::getuid().as_raw(),
        rustix::process::getgid().as_raw(),
    )
}

/// The path a descriptor holds, by its link in `/proc`.
pub(super) fn path_of(fd: BorrowedFd<'_>) -> PathBuf {
    fs::read_link(format!("/proc/self/fd/{}", fd.as_raw_fd())).expect("the descriptor's path")
}

/// Makes the directories of `from` again in `to`: what a bind shows of them.
pub(super) fn copy_dirs(from: &Path, to: &Path) -> io::Result<()> {
    for entry in fs::read_dir(from)? {
        let entry = entry?;
        if entry.file_type()?.is_dir() {
            let into = to.join(entry.file_name());
            fs::create_dir_all(&into)?;
            copy_dirs(&entry.path(), &into)?;
        }
    }
    Ok(())
}

/// The mounts' calls, recorded with each path relative to `root`; the call named `fail`
/// is refused `EPERM`.
#[derive(Debug, Default)]
pub(super) struct Mounts {
    pub(super) root: PathBuf,
    pub(super) asked: Mutex<Vec<String>>,
    pub(super) fail: Option<&'static str>,
    /// Whether a directory is a mount point already.
    pub(super) mounted: bool,
    /// Whether every call panics, as a thread lost to a fault does.
    pub(super) panics: bool,
}

impl Mounts {
    pub(super) fn under(root: &Path) -> Self {
        Self {
            root: root.to_owned(),
            ..Self::default()
        }
    }

    fn ask(&self, call: &'static str, path: &Path, rest: &str) -> io::Result<()> {
        assert!(!self.panics, "a call that panics");
        let path = path.strip_prefix(&self.root).unwrap_or(path);
        let line = format!("{call} {}{rest}", path.display());
        self.asked.lock().expect("calls").push(line);
        match self.fail == Some(call) {
            true => Err(io::Error::from_raw_os_error(libc::EPERM)),
            false => Ok(()),
        }
    }

    pub(super) fn asked(&self) -> Vec<String> {
        self.asked.lock().expect("calls").clone()
    }
}

impl MountCalls for Mounts {
    fn clone_tree(&self, source: BorrowedFd<'_>) -> io::Result<OwnedFd> {
        self.ask("clone", &path_of(source), "")?;
        source.try_clone_to_owned()
    }

    fn set_attributes(&self, tree: BorrowedFd<'_>, set: Attributes) -> io::Result<()> {
        let rest = format!(" ro={} noexec={}", set.read_only, set.no_exec);
        self.ask("set", &path_of(tree), &rest)
    }

    fn attach(&self, tree: BorrowedFd<'_>, point: &Path) -> io::Result<()> {
        self.ask("attach", point, "")?;
        copy_dirs(&path_of(tree), point)
    }

    fn mount_overlay(&self, point: &Path, no_exec: bool, options: &str) -> io::Result<()> {
        let root = format!("{}/", self.root.display());
        let rest = format!(" noexec={no_exec} {}", options.replace(&root, ""));
        self.ask("overlay", point, &rest)
    }

    fn detach(&self, point: &Path) -> io::Result<()> {
        self.ask("detach", point, "")
    }

    fn is_mount_point(&self, dir: &Path) -> io::Result<bool> {
        self.ask("inspect", dir, "").map(|()| self.mounted)
    }

    fn bind_onto_itself(&self, dir: &Path) -> io::Result<()> {
        self.ask("bind", dir, "")
    }

    fn make_private(&self, dir: &Path) -> io::Result<()> {
        self.ask("private", dir, "")
    }
}

/// Where a held read says it began, and the word it waits for.
pub(super) type Gate = (Mutex<Sender<()>>, Mutex<Receiver<()>>);

/// The reads of host paths: the kernel's, but held when there is a gate, and answering as
/// `search` and `answer` say.
#[derive(Debug, Default)]
pub(super) struct Paths {
    /// Each lookup and check says it began, then waits for a word, when set.
    pub(super) gate: Option<Gate>,
    /// The job user's search: refused with this error, or allowed.
    pub(super) search: Option<i32>,
    /// A check of a source: this error, or the kernel's answer.
    pub(super) answer: Option<i32>,
}

impl Paths {
    /// Reads that each wait for a word on the second channel, having said so on the first.
    pub(super) fn held() -> (Self, Receiver<()>, Sender<()>) {
        let (began, beginning) = channel();
        let (go, going) = channel();
        let paths = Self {
            gate: Some((Mutex::new(began), Mutex::new(going))),
            ..Self::default()
        };
        (paths, beginning, go)
    }

    fn wait(&self) {
        if let Some((began, go)) = &self.gate {
            let _ = began.lock().expect("began").send(());
            let _ = go.lock().expect("go").recv();
        }
    }
}

impl PathCalls for Paths {
    fn open_beneath(&self, dir: BorrowedFd<'_>, rest: &Path) -> Result<OwnedFd, ResolveError> {
        self.wait();
        Kernel.open_beneath(dir, rest)
    }

    fn search_as(&self, _dir: &Path, _ids: (u32, u32)) -> io::Result<()> {
        match self.search {
            Some(errno) => Err(io::Error::from_raw_os_error(errno)),
            None => Ok(()),
        }
    }

    fn answers(&self, dir: &Path) -> io::Result<()> {
        self.wait();
        match self.answer {
            Some(errno) => Err(io::Error::from_raw_os_error(errno)),
            None => Kernel.answers(dir),
        }
    }

    fn give_at(
        &self,
        dir: BorrowedFd<'_>,
        name: &std::ffi::OsStr,
        ids: (u32, u32),
    ) -> io::Result<()> {
        Kernel.give_at(dir, name, ids)
    }
}

/// Polls `what` until it gives an answer: the pool's threads run apart from the test, which
/// only waits for them to have run (never for a bound, which is passed on the caller's clock).
pub(super) fn until<T>(mut what: impl FnMut() -> Option<T>) -> T {
    for _ in 0..30_000 {
        if let Some(answer) = what() {
            return answer;
        }
        std::thread::sleep(std::time::Duration::from_millis(1));
    }
    panic!("the pool did not answer");
}

/// A mount of the host path `path` at `target`.
pub(super) fn mount(path: &Path, target: &str, mode: MountMode) -> JobMount {
    JobMount {
        source: MountSource::HostPath {
            path: path.display().to_string(),
        },
        target: target.to_owned(),
        mode,
        sub_path: None,
        no_exec: false,
        propagation: Propagation::Private,
    }
}

/// A host path layer.
pub(super) fn host_layer(path: &Path) -> Layer {
    Layer::HostPath {
        path: path.display().to_string(),
    }
}

/// The owners of a test's assembly: its own ids, which it may give anything to.
pub(super) fn owners() -> Owners {
    Owners {
        root: own(),
        job: own(),
        user: own(),
    }
}

/// Catches a pool that loses an answer, makes a caller wait past its deadline, or loses a
/// thread to a task that panics: an answer is had once, a task with no answer yet is
/// waiting until the caller's deadline and unanswered at it, and a task that panics is
/// unanswered at once, after which the thread takes the next task.
#[test]
fn the_pool_answers_once_waits_to_the_deadline_and_survives_a_panic() {
    let pool = Pool::new(1).expect("a pool");
    let (go, gate) = channel::<()>();
    let held = pool.run(100, task(Some(gate), false));
    assert_eq!(held.poll(99), Poll::Waiting);
    assert_eq!(held.poll(100), Poll::Unanswered("did not answer in time"));
    go.send(()).expect("let it go");
    let answered = until(|| match held.poll(0) {
        Poll::Waiting => None,
        other => Some(other),
    });
    assert_eq!(answered, Poll::Answered(8));
    let panicked = pool.run(u64::MAX, task(None, true));
    let lost = until(|| match panicked.poll(0) {
        Poll::Waiting => None,
        other => Some(other),
    });
    assert_eq!(lost, Poll::Unanswered("ended without an answer"));
    let after = pool.run(u64::MAX, task(None, false));
    let seven = until(|| match after.poll(0) {
        Poll::Answered(answer) => Some(answer),
        _ => None,
    });
    assert_eq!(seven, 7);
}

/// A task of the pool's test, of one type whatever it does: it waits for `gate` when it
/// has one (one more, for a word), then panics when `panics` says so, and answers 7.
fn task(gate: Option<Receiver<()>>, panics: bool) -> impl FnOnce() -> u32 + Send + 'static {
    move || {
        let waited = gate.map_or(0, |gate| u32::from(gate.recv().is_ok()));
        assert!(!panics, "a task that panics");
        waited + 7
    }
}

/// Catches a target made out of its tree, one made where a link of the tree does not lead,
/// a part made with the umask's mode or the maker's owner, or a check that makes what it
/// should only find: a missing part is made `0755` and given to the owner, a link is read
/// inside the tree, a check finds what is there and refuses what is not, and a file where a
/// directory should be, or a dangling link where a part is to be made, refuses.
#[test]
fn a_target_is_made_inside_its_tree_as_the_tree_reads() {
    let dir = scratch("reach");
    let tree = dir.path().join("tree");
    fs::create_dir_all(tree.join("usr/lib")).expect("a tree");
    // An absolute link is read inside the tree, as the job reads it.
    symlink("/usr/lib", tree.join("lib")).expect("a link");
    reach(&tree, Path::new("lib/sdk/bin"), Some(own()), &Kernel).expect("made through the link");
    let made = tree.join("usr/lib/sdk/bin");
    let meta = fs::metadata(&made).expect("the target");
    assert_eq!(meta.permissions().mode() & 0o777, 0o755);
    assert_eq!((meta.uid(), meta.gid()), own());
    reach(&tree, Path::new("usr/lib"), None, &Kernel).expect("found");
    let missing = reach(&tree, Path::new("opt/x"), None, &Kernel).expect_err("not made");
    assert_eq!(missing.raw_os_error(), Some(libc::ENOENT));
    fs::write(tree.join("file"), b"x").expect("a file");
    let through = reach(&tree, Path::new("file/x"), Some(own()), &Kernel).expect_err("a file");
    assert_eq!(through.raw_os_error(), Some(libc::ENOTDIR));
    symlink("/nowhere/at/all", tree.join("dangling")).expect("a dangling link");
    let dangling = reach(&tree, Path::new("dangling"), Some(own()), &Kernel).expect_err("dangling");
    assert_eq!(dangling.raw_os_error(), Some(libc::EEXIST));
    let gone = reach(
        &dir.path().join("no-tree"),
        Path::new("x"),
        Some(own()),
        &Kernel,
    );
    assert_eq!(
        gone.expect_err("no tree").raw_os_error(),
        Some(libc::ENOENT)
    );
}

/// Catches a lookup in a tree a rename or a mount raced being given up at once, made
/// again without bound, or made again for another answer: `EAGAIN` on every attempt but
/// the last is followed by another call with the same path (`.` for the tree itself),
/// which opens the directory; `EAGAIN` on every attempt is the answer after a fixed number
/// of them; any other answer is the answer after one.
#[test]
fn a_lookup_raced_by_a_rename_is_made_again_a_bounded_number_of_times() {
    let dir = scratch("raced");
    let tree = rustix::fs::open(
        dir.path(),
        rustix::fs::OFlags::PATH,
        rustix::fs::Mode::empty(),
    )
    .expect("the tree");
    // `openat2` answering `EAGAIN` the first `races` times and then `answer`, as the
    // paths it was given.
    let lookup = |rel: &str, races: usize, answer: Option<rustix::io::Errno>| {
        let mut given = Vec::new();
        let found = in_root_with(Path::new(rel), &mut |path| {
            given.push(path.to_owned());
            match (given.len() <= races, answer) {
                (true, _) => Err(rustix::io::Errno::AGAIN),
                (false, None) => Ok(tree.try_clone().expect("a clone")),
                (false, Some(errno)) => Err(errno),
            }
        });
        (found.map(|_| ()).map_err(|why| why.raw_os_error()), given)
    };
    let last = RESOLVE_ATTEMPTS - 1;
    let path = |rel: &str, n: usize| vec![PathBuf::from(rel); n];
    assert_eq!(lookup("usr/lib", 1, None), (Ok(()), path("usr/lib", 2)));
    assert_eq!(
        lookup("", last, None),
        (Ok(()), path(".", RESOLVE_ATTEMPTS))
    );
    assert_eq!(
        lookup("usr/lib", usize::MAX, None),
        (Err(Some(libc::EAGAIN)), path("usr/lib", RESOLVE_ATTEMPTS))
    );
    let missing = Some(rustix::io::Errno::NOENT);
    assert_eq!(
        lookup("usr/lib", 0, missing),
        (Err(Some(libc::ENOENT)), path("usr/lib", 1))
    );
}

/// Catches a search made with the caller's ids instead of the job user's, or one that
/// passes when the ids could not be taken: the search is made by a thread that took the
/// ids first, its refusal is the answer, and an ordinary user cannot take another's.
#[test]
fn the_job_users_search_is_made_by_a_thread_that_took_its_ids() {
    let dir = scratch("search");
    let open = dir.path().join("open");
    fs::create_dir(&open).expect("a directory");
    fn took(_: (u32, u32)) -> io::Result<()> {
        Ok(())
    }
    fn refused(_: (u32, u32)) -> io::Result<()> {
        Err(io::Error::from_raw_os_error(libc::EPERM))
    }
    search_with(&open, own(), took).expect("searchable");
    let missing = search_with(&dir.path().join("missing"), own(), took).expect_err("missing");
    assert_eq!(missing.raw_os_error(), Some(libc::ENOENT));
    let why = search_with(&open, own(), refused).expect_err("ids not taken");
    assert_eq!(why.raw_os_error(), Some(libc::EPERM));
    // An ordinary user may not drop its groups: the node's call refuses, and so the search.
    let why = become_user(own()).expect_err("not root");
    assert_eq!(why.raw_os_error(), Some(libc::EPERM));
    assert!(Kernel.search_as(&open, own()).is_err());
}

/// Catches a source taken for gone when it answers, or for there when its file system has
/// gone: a directory answers; `ENOTCONN`, `EIO` and `ESTALE` are a file system that went
/// away, and anything else is not; `ENOSYS` alone says a call is missing.
#[test]
fn a_source_answers_or_its_file_system_has_gone() {
    let dir = scratch("answers");
    Kernel.answers(dir.path()).expect("a directory answers");
    let why = Kernel
        .answers(&dir.path().join("missing"))
        .expect_err("missing");
    assert!(!unreadable(&why));
    for errno in [libc::ENOTCONN, libc::EIO, libc::ESTALE] {
        assert!(unreadable(&io::Error::from_raw_os_error(errno)), "{errno}");
    }
    assert!(missing_call(&Err(io::Error::from_raw_os_error(
        libc::ENOSYS
    ))));
    assert!(!missing_call(&Err(io::Error::from_raw_os_error(
        libc::EPERM
    ))));
    assert!(!missing_call(&Ok(())));
}

/// Catches a mount made before it is listed, a list read in another order, an unmount that
/// stops at the first failure or leaves the directory behind, and a directory removed while
/// a mount may be under it: each point is listed and made before its mount, the release
/// detaches deepest first and removes the directory, and one whose unmount fails keeps it.
#[test]
fn the_list_names_each_point_before_its_mount_and_the_release_follows_it() {
    let dir = scratch("list");
    let lease = dir.path().join("mounts/lease");
    list::make_dir(&lease).expect("the lease's mounts");
    assert_eq!(
        list::points(&lease).expect("no list"),
        Vec::<PathBuf>::new()
    );
    let points = [lease.join("src/0"), lease.join("rootfs")];
    for point in &points {
        list::note(&lease, point).expect("listed");
        assert!(point.is_dir(), "{point:?} made");
    }
    assert_eq!(list::points(&lease).expect("the list"), points);
    let failing = Mounts {
        fail: Some("detach"),
        ..Mounts::under(dir.path())
    };
    let why = list::release(&failing, &lease).expect_err("an unmount fails");
    assert_eq!(
        why,
        "unmount mounts/lease/src/0: Operation not permitted (os error 1)"
            .replace("mounts/lease", &lease.display().to_string())
    );
    assert!(lease.is_dir(), "kept for the next try");
    let calls = Mounts::under(dir.path());
    list::release(&calls, &lease).expect("released");
    assert_eq!(
        calls.asked(),
        ["detach mounts/lease/src/0", "detach mounts/lease/rootfs"]
    );
    assert!(!lease.exists());
    list::release(&calls, &lease).expect("nothing to release");
    // A list that cannot be read, and a directory that cannot be removed, keep the lease.
    let unreadable = dir.path().join("mounts/unreadable");
    fs::create_dir_all(unreadable.join(list::LIST)).expect("a list that is a directory");
    let why = list::release(&calls, &unreadable).expect_err("unreadable");
    assert!(why.starts_with("read the list of "), "{why}");
    let stuck = dir.path().join("mounts/stuck");
    fs::create_dir_all(stuck.join("locked/in")).expect("a tree");
    fs::set_permissions(stuck.join("locked"), fs::Permissions::from_mode(0o000)).expect("chmod");
    let why = list::release(&calls, &stuck).expect_err("cannot be removed");
    fs::set_permissions(stuck.join("locked"), fs::Permissions::from_mode(0o755)).expect("chmod");
    assert!(
        why.starts_with(&format!("remove {}", stuck.display())),
        "{why}"
    );
}
