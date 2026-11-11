//! What [`clone_at`], [`mount_overlay`], [`unmount_all`] and [`make_private_dir`] ask of
//! the kernel, against a fake that records each call and fails the ones a test names. The
//! root suite (`tests/crun_root/job_fs.rs`) makes the same calls for real.

use std::cell::RefCell;
use std::os::fd::{AsFd as _, AsRawFd as _};

use rustix::fs::{CWD, Mode, OFlags};

use super::*;

/// One call the fake was asked to make.
#[derive(Clone, Debug, PartialEq, Eq)]
enum Call {
    Clone,
    Attributes(Attributes),
    Attach(PathBuf),
    Overlay(PathBuf, bool, String),
    Detach(PathBuf),
    Inspect(PathBuf),
    Bind(PathBuf),
    Private(PathBuf),
}

/// A fake kernel: records every call, answers `errno` for the first call that matches
/// `fails`, and says a directory is a mount point when it is in `mounted`.
#[derive(Default)]
struct Fake {
    calls: RefCell<Vec<Call>>,
    fails: Vec<(Call, i32)>,
    mounted: Vec<PathBuf>,
    /// The descriptors `attach` and `set_attributes` were given, which must be the one
    /// `clone_tree` returned.
    trees: RefCell<Vec<i32>>,
}

impl Fake {
    fn failing(fails: Vec<(Call, i32)>) -> Self {
        Self {
            fails,
            ..Self::default()
        }
    }

    fn answer(&self, call: Call) -> io::Result<()> {
        let errno = self.fails.iter().find(|(c, _)| *c == call).map(|(_, e)| *e);
        self.calls.borrow_mut().push(call);
        errno.map_or(Ok(()), |e| Err(io::Error::from_raw_os_error(e)))
    }

    fn calls(&self) -> Vec<Call> {
        self.calls.borrow().clone()
    }
}

impl MountCalls for Fake {
    fn clone_tree(&self, _: BorrowedFd<'_>) -> io::Result<OwnedFd> {
        self.answer(Call::Clone)?;
        let fd = rustix::fs::openat(CWD, ".", OFlags::PATH | OFlags::CLOEXEC, Mode::empty())?;
        self.trees.borrow_mut().push(fd.as_raw_fd());
        Ok(fd)
    }

    fn set_attributes(&self, tree: BorrowedFd<'_>, attributes: Attributes) -> io::Result<()> {
        self.trees.borrow_mut().push(tree.as_raw_fd());
        self.answer(Call::Attributes(attributes))
    }

    fn attach(&self, tree: BorrowedFd<'_>, point: &Path) -> io::Result<()> {
        self.trees.borrow_mut().push(tree.as_raw_fd());
        self.answer(Call::Attach(point.to_owned()))
    }

    fn mount_overlay(&self, point: &Path, no_exec: bool, options: &str) -> io::Result<()> {
        self.answer(Call::Overlay(point.to_owned(), no_exec, options.to_owned()))
    }

    fn detach(&self, point: &Path) -> io::Result<()> {
        self.answer(Call::Detach(point.to_owned()))
    }

    fn is_mount_point(&self, dir: &Path) -> io::Result<bool> {
        self.answer(Call::Inspect(dir.to_owned()))?;
        Ok(self.mounted.iter().any(|m| m == dir))
    }

    fn bind_onto_itself(&self, dir: &Path) -> io::Result<()> {
        self.answer(Call::Bind(dir.to_owned()))
    }

    fn make_private(&self, dir: &Path) -> io::Result<()> {
        self.answer(Call::Private(dir.to_owned()))
    }
}

/// A descriptor to clone from.
fn source() -> OwnedFd {
    rustix::fs::openat(CWD, ".", OFlags::PATH | OFlags::CLOEXEC, Mode::empty()).expect("open")
}

fn p(path: &str) -> PathBuf {
    PathBuf::from(path)
}

/// Catches a clone attached before it is set (a shared clone would be in the source's peer
/// group while attached, and a read-only one writable), set or attached through another
/// descriptor than the clone's, or given other attributes than asked: the three calls come
/// in order, on the clone, with the attributes unchanged.
#[test]
fn a_clone_is_set_while_detached_then_attached() {
    let fake = Fake::default();
    let attributes = Attributes {
        read_only: true,
        no_exec: true,
    };
    let point = p("/m/lease/src/0");
    clone_at(&fake, source().as_fd(), &point, attributes).expect("cloned");
    assert_eq!(
        fake.calls(),
        [
            Call::Clone,
            Call::Attributes(attributes),
            Call::Attach(point)
        ]
    );
    let trees = fake.trees.borrow();
    assert_eq!(trees.len(), 3);
    assert!(trees.iter().all(|fd| *fd == trees[0]), "{trees:?}");
}

/// Catches a failed step that is not reported as that step at that point, or a clone that
/// goes on to be attached after it could not be set (it would be shared, or writable): each
/// failure stops the clone there and names its step, its point and the kernel's answer.
#[test]
fn a_failed_step_stops_the_clone_and_is_named() {
    let point = p("/m/lease/src/3");
    let attributes = Attributes::default();
    let cases = [
        (Call::Clone, MountStep::Clone, vec![Call::Clone]),
        (
            Call::Attributes(attributes),
            MountStep::Attributes,
            vec![Call::Clone, Call::Attributes(attributes)],
        ),
        (
            Call::Attach(point.clone()),
            MountStep::Attach,
            vec![
                Call::Clone,
                Call::Attributes(attributes),
                Call::Attach(point.clone()),
            ],
        ),
    ];
    for (fails, step, made) in cases {
        let fake = Fake::failing(vec![(fails, libc::EPERM)]);
        let why = clone_at(&fake, source().as_fd(), &point, attributes).expect_err("failed");
        assert_eq!(why.step, step);
        assert_eq!(why.point, point);
        assert_eq!(why.source.raw_os_error(), Some(libc::EPERM));
        assert_eq!(fake.calls(), made, "{step:?}");
    }
}

/// Catches an error that does not say what failed where: the whole message of each step.
#[test]
fn each_step_is_named_in_its_error() {
    let cases = [
        (MountStep::Clone, "clone the tree for /p"),
        (MountStep::Attributes, "set the clone's attributes for /p"),
        (MountStep::Attach, "attach the clone at /p"),
        (MountStep::Options, "write the overlay's options for /p"),
        (MountStep::Overlay, "mount the overlay at /p"),
        (MountStep::Unmount, "unmount /p"),
        (MountStep::Inspect, "find whether this is a mount point: /p"),
        (MountStep::Bind, "bind onto itself /p"),
        (MountStep::Private, "make private /p"),
    ];
    for (step, said) in cases {
        let why = failed(step, Path::new("/p"))(io::Error::from_raw_os_error(libc::EPERM));
        assert_eq!(
            why.to_string(),
            format!("{said}: Operation not permitted (os error 1)")
        );
    }
}

fn overlay(lowers: &[&str]) -> Overlay {
    Overlay {
        lowers: lowers.iter().map(PathBuf::from).collect(),
        upper: p("/scratch/lease/rootfs-upper"),
        work: p("/scratch/lease/rootfs-work"),
        no_exec: false,
    }
}

/// Catches lowers in the spec's order instead of overlay's, a missing or changed fixed
/// option (an index, metacopy or redirects followed would let the upper change what a
/// lower shows), and `no_exec` not reaching the mount: the options, whole, as the call
/// receives them.
#[test]
fn an_overlay_is_mounted_with_its_lowers_top_first_and_fixed_options() {
    let fake = Fake::default();
    let mut root = overlay(&["/m/l/src/1", "/m/l/src/0", "/images/a/rootfs"]);
    mount_overlay(&fake, &root, Path::new("/m/l/rootfs")).expect("mounted");
    root.no_exec = true;
    mount_overlay(&fake, &root, Path::new("/m/l/cow/0")).expect("mounted");
    let options = "lowerdir=/m/l/src/1:/m/l/src/0:/images/a/rootfs,\
                   upperdir=/scratch/lease/rootfs-upper,workdir=/scratch/lease/rootfs-work,\
                   index=off,metacopy=off,redirect_dir=off,xino=off";
    assert_eq!(
        fake.calls(),
        [
            Call::Overlay(p("/m/l/rootfs"), false, options.to_owned()),
            Call::Overlay(p("/m/l/cow/0"), true, options.to_owned()),
        ]
    );
}

/// Catches a path that would change the overlay's options (a `,` starting an option of the
/// job's choosing, a `:` adding a lower, a `\` escaping either), one that is not UTF-8, an
/// overlay with no lower, and options longer than a mount takes (the kernel would cut them)
/// being mounted: each is refused before any call, naming the point; options of exactly the
/// most bytes are mounted.
#[test]
fn an_overlay_whose_options_cannot_be_written_is_not_mounted() {
    use std::os::unix::ffi::OsStrExt as _;
    let point = Path::new("/m/l/rootfs");
    let mut bad: Vec<Overlay> = ["/a,b", "/a:b", "/a\\b"]
        .iter()
        .map(|lower| overlay(&[lower]))
        .collect();
    let mut upper = overlay(&["/a"]);
    upper.upper = p("/up,per");
    let mut work = overlay(&["/a"]);
    work.work = p("/wo:rk");
    let mut not_utf8 = overlay(&["/a"]);
    not_utf8.lowers = vec![PathBuf::from(std::ffi::OsStr::from_bytes(b"/a\xff"))];
    bad.extend([upper, work, not_utf8, overlay(&[])]);
    for overlay in &bad {
        let fake = Fake::default();
        let why = mount_overlay(&fake, overlay, point).expect_err("refused");
        assert_eq!(why.step, MountStep::Options, "{overlay:?}");
        assert_eq!(why.point, point);
        assert_eq!(why.source.kind(), io::ErrorKind::InvalidInput);
        assert!(fake.calls().is_empty(), "{overlay:?}");
    }
    let fake = Fake::default();
    let why = mount_overlay(&fake, &overlay(&[]), point).expect_err("no lower");
    assert_eq!(
        why.to_string(),
        "write the overlay's options for /m/l/rootfs: an overlay needs a lower"
    );
    let why = mount_overlay(&fake, &overlay(&["/a:b"]), point).expect_err("a colon");
    assert_eq!(
        why.to_string(),
        "write the overlay's options for /m/l/rootfs: /a:b cannot be given to an overlay: \
         it is not UTF-8, or holds `,`, `:` or `\\`"
    );
    // The fixed part around one lower of `n` bytes.
    let fixed = overlay_options(&overlay(&["/"])).expect("options").len() - 1;
    let longest = format!("/{}", "a".repeat(MAX_OVERLAY_OPTIONS - fixed - 1));
    let options = overlay_options(&overlay(&[&longest])).expect("the most bytes");
    assert_eq!(options.len(), MAX_OVERLAY_OPTIONS);
    let one_more = format!("{longest}a");
    let why = mount_overlay(&fake, &overlay(&[&one_more]), point).expect_err("one more");
    assert_eq!(
        why.source.to_string(),
        format!("its options are 4096 bytes, more than the {MAX_OVERLAY_OPTIONS} a mount takes")
    );
    assert!(fake.calls().is_empty());
}

/// Catches a failed overlay mount not reported as one, at its point, with the kernel's
/// answer.
#[test]
fn a_failed_overlay_mount_is_named() {
    let point = p("/m/l/cow/2");
    let options = overlay_options(&overlay(&["/a"])).expect("options");
    let fake = Fake::failing(vec![(
        Call::Overlay(point.clone(), false, options),
        libc::EINVAL,
    )]);
    let why = mount_overlay(&fake, &overlay(&["/a"]), &point).expect_err("failed");
    assert_eq!(why.step, MountStep::Overlay);
    assert_eq!(why.point, point);
    assert_eq!(why.source.raw_os_error(), Some(libc::EINVAL));
}

/// Catches an unmount of a mount before the mounts below it, or, at one depth, of the
/// first made before the last (a mount made over another at the same point): the points
/// are detached deepest first, and the later listed first among equals.
#[test]
fn points_are_unmounted_deepest_first_and_last_made_first() {
    let fake = Fake::default();
    let points = [
        p("/m/l/src/0"),
        p("/m/l/src/1"),
        p("/m/l/rootfs"),
        p("/m/l/cow/1/deeper/still"),
        p("/m/l/cow/1"),
    ];
    unmount_all(&fake, &points).expect("unmounted");
    let order: Vec<Call> = [
        "/m/l/cow/1/deeper/still",
        "/m/l/cow/1",
        "/m/l/src/1",
        "/m/l/src/0",
        "/m/l/rootfs",
    ]
    .iter()
    .map(|point| Call::Detach(p(point)))
    .collect();
    assert_eq!(fake.calls(), order);
    let fake = Fake::default();
    unmount_all(&fake, &[]).expect("nothing to unmount");
    assert!(fake.calls().is_empty());
}

/// Catches a point listed but never mounted (`EINVAL`), or already removed (`ENOENT`),
/// failing the unmounts, and a real failure that stops the points after it from being
/// unmounted or is not reported: every point is tried, and the first failure is the answer.
#[test]
fn a_point_never_mounted_is_unmounted_and_a_failure_does_not_stop_the_rest() {
    let fake = Fake::failing(vec![
        (Call::Detach(p("/m/l/src/1")), libc::EINVAL),
        (Call::Detach(p("/m/l/src/0")), libc::ENOENT),
    ]);
    unmount_all(&fake, &[p("/m/l/src/0"), p("/m/l/src/1")]).expect("nothing was mounted");
    assert_eq!(fake.calls().len(), 2);
    let fake = Fake::failing(vec![
        (Call::Detach(p("/m/l/src/2")), libc::EBUSY),
        (Call::Detach(p("/m/l/src/1")), libc::EPERM),
    ]);
    let points = [p("/m/l/src/0"), p("/m/l/src/1"), p("/m/l/src/2")];
    let why = unmount_all(&fake, &points).expect_err("two failed");
    assert_eq!(why.step, MountStep::Unmount);
    assert_eq!(why.point, p("/m/l/src/2"));
    assert_eq!(why.source.raw_os_error(), Some(libc::EBUSY));
    assert_eq!(fake.calls().len(), 3, "every point is tried");
}

/// Catches a second bind over the directory a restarted daemon finds already bound (it
/// would hide the adopted runs' mounts below it), a directory left unbound at a first
/// start, and propagation left as found in either case: bound only when it is not a mount
/// point, and made private both times.
#[test]
fn the_mount_directory_is_bound_once_and_made_private_each_time() {
    let dir = p("/run/js-agent/mounts");
    let fake = Fake::default();
    make_private_dir(&fake, &dir).expect("first start");
    assert_eq!(
        fake.calls(),
        [
            Call::Inspect(dir.clone()),
            Call::Bind(dir.clone()),
            Call::Private(dir.clone())
        ]
    );
    let fake = Fake {
        mounted: vec![dir.clone()],
        ..Fake::default()
    };
    make_private_dir(&fake, &dir).expect("restart");
    assert_eq!(
        fake.calls(),
        [Call::Inspect(dir.clone()), Call::Private(dir.clone())]
    );
}

/// Catches a failed step of making the directory private not reported as that step, or
/// one that goes on after it: each failure is named and stops there.
#[test]
fn a_failed_step_of_the_mount_directory_is_named() {
    let dir = p("/run/js-agent/mounts");
    let cases = [
        (Call::Inspect(dir.clone()), MountStep::Inspect, 1),
        (Call::Bind(dir.clone()), MountStep::Bind, 2),
        (Call::Private(dir.clone()), MountStep::Private, 3),
    ];
    for (fails, step, made) in cases {
        let fake = Fake::failing(vec![(fails, libc::EACCES)]);
        let why = make_private_dir(&fake, &dir).expect_err("failed");
        assert_eq!((why.step, why.point.as_path()), (step, dir.as_path()));
        assert_eq!(why.source.raw_os_error(), Some(libc::EACCES));
        assert_eq!(fake.calls().len(), made, "{step:?}");
    }
}
