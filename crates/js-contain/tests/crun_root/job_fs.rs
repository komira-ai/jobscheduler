//! A job's own file system, made on the host by this (root) process with
//! `js_contain::job_fs`, as the daemon makes it, and read from inside the job: a root of
//! layers, read-only, read-write and copy-on-write mounts with their options, and what the
//! unmounts and the removal leave.
//!
//! Every host path is a directory of the cell's scratch directory, below a directory that
//! stands for an operator's mount root (`host`); the mount directory is the cell's
//! `mounts`, made private as the daemon makes its own. Each test unmounts what it mounted
//! before the cell is removed ([`Mounts`]' drop), whether it passed or not.

use std::fs;
use std::os::fd::AsFd as _;
use std::os::unix::fs::{FileTypeExt as _, MetadataExt as _, lchown};
use std::path::{Path, PathBuf};

use js_contain::container::HostMount;
use js_contain::job_fs::{self, Attributes, Kernel, MountCalls as _, Overlay};
use js_contain::remove::remove_tree;
use tokio::io::AsyncReadExt as _;

use crate::cell::{Cell, Lease, Outcome, Shape, mkdir};

/// The mounts of one lease, under the cell's mount directory, and the mounts the test made
/// on the host for its sources: all are unmounted when this is dropped.
struct Mounts {
    /// The cell's mount directory (`<mounts>`).
    dir: PathBuf,
    /// The lease's directory in it (`<mounts>/<lease>`).
    here: PathBuf,
    /// The lease's scratch directory, which holds the overlays' upper and work directories.
    lease_dir: PathBuf,
    /// Every point mounted under `here`, in the order made.
    points: Vec<PathBuf>,
    /// The mounts the test made on the host, in the order made.
    host: Vec<PathBuf>,
}

impl Mounts {
    /// The mount directory of `cell`, bound onto itself and private, and `lease`'s own
    /// directory in it, each mode 0711 as the daemon makes them.
    fn new(cell: &Cell, lease: &Lease) -> Self {
        let dir = cell.scratch.join("mounts");
        mkdir(&dir, 0o711);
        job_fs::make_private_dir(&Kernel, &dir).expect("make the mount directory private");
        let here = dir.join(&lease.spec.name);
        for part in ["", "src", "cow"] {
            mkdir(&here.join(part), 0o711);
        }
        Self {
            dir,
            here,
            lease_dir: lease.spec.dir.clone(),
            points: Vec::new(),
            host: Vec::new(),
        }
    }

    /// Clones the host path `rest` beneath `root`, and its sub-path `sub` beneath that
    /// when not empty, as source `i`: its point, `<mounts>/<lease>/src/<i>`.
    fn clone(&mut self, i: usize, root: &Path, rest: &str, sub: &str, set: Attributes) -> PathBuf {
        let root = job_fs::open_root(root).expect("open the root");
        let path = job_fs::open_beneath(root.as_fd(), Path::new(rest)).expect("the host path");
        let opened = job_fs::open_beneath(path.as_fd(), Path::new(sub)).expect("the sub-path");
        let point = self.here.join("src").join(i.to_string());
        mkdir(&point, 0o711);
        self.points.push(point.clone());
        job_fs::clone_at(&Kernel, opened.as_fd(), &point, set).expect("clone the host path");
        point
    }

    /// An overlay of `lowers` (top first) with its upper and work directories at
    /// `<lease>/<name>/upper` and `.../work`, the upper's root owned by `owner` and mode
    /// 0755, mounted at `point`.
    fn overlay(
        &mut self,
        name: &str,
        lowers: Vec<PathBuf>,
        owner: (u32, u32),
        no_exec: bool,
        point: PathBuf,
    ) -> PathBuf {
        let upper = self.lease_dir.join(name).join("upper");
        let work = self.lease_dir.join(name).join("work");
        mkdir(&upper, 0o755);
        mkdir(&work, 0o700);
        lchown(&upper, Some(owner.0), Some(owner.1)).expect("give the upper its owner");
        mkdir(&point, 0o711);
        self.points.push(point.clone());
        let overlay = Overlay {
            lowers,
            upper,
            work,
            no_exec,
        };
        job_fs::mount_overlay(&Kernel, &overlay, &point).expect("mount the overlay");
        point
    }

    /// The root of the lease: an overlay of `lowers` whose upper's root is owned by the
    /// container's root, as an image's root is.
    fn root(&mut self, cell: &Cell, lowers: Vec<PathBuf>) -> PathBuf {
        let owner = (cell.ids.sub_uid, cell.ids.sub_gid);
        let point = self.here.join("rootfs");
        self.overlay("rootfs", lowers, owner, false, point)
    }

    /// The copy-on-write mount of source `i`: an overlay of its clone whose upper's root
    /// is the job's, at `<mounts>/<lease>/cow/<i>`.
    fn cow(&mut self, cell: &Cell, i: usize, no_exec: bool) -> PathBuf {
        let lower = self.here.join("src").join(i.to_string());
        let point = self.here.join("cow").join(i.to_string());
        self.overlay(
            &format!("cow/{i}"),
            vec![lower],
            cell.job_ids(),
            no_exec,
            point,
        )
    }

    /// Binds `source` at `target` on the host, as a provider mounts something below a
    /// source; unmounted when this is dropped.
    fn host_bind(&mut self, source: &Path, target: &Path) {
        rustix::mount::mount_bind(source, target).expect("bind on the host");
        self.host.push(target.to_owned());
    }

    /// Unmounts the lease's points, as the daemon does after `crun delete`.
    fn unmount_lease(&mut self) {
        job_fs::unmount_all(&Kernel, &self.points).expect("unmount the lease's points");
        self.points.clear();
    }
}

impl Drop for Mounts {
    fn drop(&mut self) {
        let _ = job_fs::unmount_all(&Kernel, &self.points);
        let _ = job_fs::unmount_all(&Kernel, &self.host);
        let _ = Kernel.detach(&self.dir);
    }
}

/// A host mount of `source` at `target`.
fn bind(target: &str, source: &Path, read_only: bool, no_exec: bool) -> HostMount {
    HostMount {
        target: target.to_owned(),
        source: source.to_owned(),
        read_only,
        recursive: true,
        no_exec,
    }
}

/// Makes `path` a directory with mode 0755 owned by `ids`.
fn owned_dir(path: &Path, ids: (u32, u32)) {
    mkdir(path, 0o755);
    lchown(path, Some(ids.0), Some(ids.1)).expect("give a directory");
}

/// Writes `text` to `path`, owned by `ids`.
fn owned_file(path: &Path, text: &str, ids: (u32, u32)) {
    fs::write(path, text).expect("write a file");
    lchown(path, Some(ids.0), Some(ids.1)).expect("give a file");
}

/// The cell's operator root (`host`), which the host paths are below.
fn host_root(cell: &Cell) -> PathBuf {
    let root = cell.scratch.join("host");
    mkdir(&root, 0o755);
    root
}

/// One mount of `/proc/<pid>/mountinfo`: its point, its per-mount options and its optional
/// fields (the propagation tags).
struct MountLine {
    point: String,
    options: Vec<String>,
    tags: Vec<String>,
}

/// The mounts of a `mountinfo` text.
fn mount_lines(text: &str) -> Vec<MountLine> {
    text.lines()
        .filter_map(|line| {
            let (mount, _) = line.split_once(" - ")?;
            let fields: Vec<&str> = mount.split_whitespace().collect();
            Some(MountLine {
                point: (*fields.get(4)?).to_owned(),
                options: fields.get(5)?.split(',').map(str::to_owned).collect(),
                tags: fields.get(6..)?.iter().map(|t| (*t).to_owned()).collect(),
            })
        })
        .collect()
}

/// The topmost mount at `point` in `text`.
fn line_at<'a>(lines: &'a [MountLine], point: &str) -> &'a MountLine {
    lines
        .iter()
        .rev()
        .find(|m| m.point == point)
        .unwrap_or_else(|| panic!("no mount at {point}"))
}

/// This process's mounts.
fn host_lines() -> Vec<MountLine> {
    mount_lines(&fs::read_to_string("/proc/self/mountinfo").expect("read mountinfo"))
}

/// Runs `lease` to its end.
async fn run(cell: &Cell, lease: &Lease) -> Outcome {
    let running = cell.start(lease).await;
    cell.finish(lease, running).await
}

/// Catches a root of layers that is not their stack (a layer missing, the image missing, or
/// the layers in the spec's order, so the base's file hides the top's), a root that is not
/// writable, a write or a delete that reaches a layer, and a lease directory the removal
/// leaves or cannot remove: the job reads a file of the image and of each layer, sees the
/// top layer's file where two share a name, writes a file and deletes a file and a
/// directory of the base layer; the layers are unchanged after, the upper holds the write
/// and a whiteout for each delete (a character device 0:0), and after the unmounts the lease
/// directory and the lease's mount directory are removed whole by `remove_tree`.
#[tokio::test]
#[ignore = "real crun, root and a job user: run through cargo xtask crun-tests --job-user"]
async fn a_root_of_layers_reads_through_and_keeps_its_writes() {
    let cell = Cell::new("layers");
    let job = cell.job_ids();
    let root = host_root(&cell);
    let image = cell
        .store()
        .join(cell.image.digest.trim_start_matches("sha256:"))
        .join("rootfs");
    fs::write(image.join("image-file"), "from the image\n").expect("write the image's file");
    for (layer, own) in [("layer-a", "a"), ("layer-b", "b")] {
        owned_dir(&root.join(layer).join("data"), job);
        owned_file(
            &root.join(layer).join(format!("data/{own}")),
            &format!("from {own}\n"),
            job,
        );
        owned_file(
            &root.join(layer).join("data/same"),
            &format!("{own}\n"),
            job,
        );
    }
    owned_dir(&root.join("layer-a/data/dir-a"), job);
    owned_file(&root.join("layer-a/data/dir-a/f"), "f\n", job);
    let script = "cat /image-file /data/a /data/b /data/same && echo new > /data/new && \
                  rm /data/a && rm -r /data/dir-a && ls /data";
    let mut lease = cell.lease("layers", &["/bin/sh", "-c", script], Shape::default());
    let mut mounts = Mounts::new(&cell, &lease);
    let read_only = Attributes {
        read_only: true,
        no_exec: false,
    };
    let base = mounts.clone(0, &root, "layer-a", "", read_only);
    let top = mounts.clone(1, &root, "layer-b", "", read_only);
    lease.spec.mounted_root = Some(mounts.root(&cell, vec![top, base, image]));
    let ran = run(&cell, &lease).await;
    assert_eq!(
        ran.out(),
        "from the image\nfrom a\nfrom b\nb\nb\nnew\nsame\n",
        "{}",
        ran.said
    );
    assert_eq!(
        fs::read_to_string(root.join("layer-a/data/a")).expect("a"),
        "from a\n"
    );
    assert!(
        root.join("layer-a/data/dir-a/f").is_file(),
        "the delete reached the layer"
    );
    assert!(
        !root.join("layer-b/data/new").exists(),
        "the write reached a layer"
    );
    let upper = lease.spec.dir.join("rootfs/upper/data");
    assert_eq!(fs::read_to_string(upper.join("new")).expect("new"), "new\n");
    for deleted in ["a", "dir-a"] {
        let whiteout = fs::symlink_metadata(upper.join(deleted)).expect("a whiteout");
        assert!(whiteout.file_type().is_char_device(), "{deleted}");
        assert_eq!(whiteout.rdev(), 0, "{deleted}");
    }
    mounts.unmount_lease();
    remove_tree(&lease.spec.dir).expect("remove the lease directory");
    remove_tree(&mounts.here).expect("remove the lease's mount directory");
    assert!(!lease.spec.dir.exists() && !mounts.here.exists());
    assert!(root.join("layer-a/data/dir-a/f").is_file() && root.join("layer-b/data/b").is_file());
}

/// Catches a read-only mount the job can write, a read-write mount whose writes do not
/// reach the host, and a copy-on-write mount at `/work` whose writes do, or that leaves the
/// exec root's own overlay over it: the write to the read-only mount fails "Read-only file
/// system"; the read-write mount's file is on the host; the copy-on-write mount's change and
/// new file are in its upper, and the host's file is unchanged with nothing new beside it.
#[tokio::test]
#[ignore = "real crun, root and a job user: run through cargo xtask crun-tests --job-user"]
async fn each_mode_writes_where_it_says() {
    let cell = Cell::new("modes");
    let job = cell.job_ids();
    let root = host_root(&cell);
    for source in ["ro-src", "rw-src", "cow-src"] {
        owned_dir(&root.join(source), job);
        owned_file(&root.join(source).join("f"), "orig\n", job);
    }
    let script = "exec 2>&1; echo x > /ro/new; echo ro=$?; echo w > /rw/new; echo rw=$?; \
                  echo changed > /work/f; echo n > /work/new; echo cow=$?; cat /work/f";
    let mut lease = cell.lease("modes", &["/bin/sh", "-c", script], Shape::default());
    let mut mounts = Mounts::new(&cell, &lease);
    let ro = mounts.clone(
        0,
        &root,
        "ro-src",
        "",
        Attributes {
            read_only: true,
            no_exec: false,
        },
    );
    let rw = mounts.clone(1, &root, "rw-src", "", Attributes::default());
    mounts.clone(2, &root, "cow-src", "", Attributes::default());
    let cow = mounts.cow(&cell, 2, false);
    let image = cell
        .store()
        .join(cell.image.digest.trim_start_matches("sha256:"))
        .join("rootfs");
    lease.spec.mounted_root = Some(mounts.root(&cell, vec![image]));
    lease.spec.mounts = vec![
        bind("/work", &cow, false, false),
        bind("/ro", &ro, true, false),
        bind("/rw", &rw, false, false),
    ];
    let ran = run(&cell, &lease).await;
    let out = ran.out();
    assert!(out.contains("Read-only file system"), "{}", ran.said);
    assert!(!out.contains("ro=0"), "{}", ran.said);
    assert!(
        out.contains("rw=0\n") && out.contains("cow=0\n"),
        "{}",
        ran.said
    );
    assert!(out.ends_with("changed\n"), "{}", ran.said);
    assert!(!root.join("ro-src/new").exists());
    assert_eq!(
        fs::read_to_string(root.join("rw-src/new")).expect("rw"),
        "w\n"
    );
    assert_eq!(
        fs::read_to_string(root.join("cow-src/f")).expect("cow"),
        "orig\n"
    );
    assert!(
        !root.join("cow-src/new").exists(),
        "the copy-on-write write reached the host"
    );
    let upper = lease.spec.dir.join("cow/2/upper");
    assert_eq!(
        fs::read_to_string(upper.join("f")).expect("copied up"),
        "changed\n"
    );
    assert_eq!(fs::read_to_string(upper.join("new")).expect("new"), "n\n");
}

/// Catches `no_exec` not reaching the mount (a program in it runs) or reaching a mount that
/// did not ask for it, in a read-only and a copy-on-write mount; and a set-id bit or a
/// device node honoured in any kind of mount: the same program runs from the mount without
/// `no_exec` and fails "Permission denied" (126) from each with it; a device node in a
/// read-only, a read-write and a copy-on-write mount cannot be opened ("Permission
/// denied", which `nodev` gives, where the device controller alone gives "Operation not
/// permitted"); and each of the three is `nosuid` and `nodev` in the job's own mount table
/// (the job runs with no new privileges, so a set-id program shows nothing more).
#[tokio::test]
#[ignore = "real crun, root and a job user: run through cargo xtask crun-tests --job-user"]
async fn no_exec_nosuid_and_nodev_hold_in_the_mounts() {
    let cell = Cell::new("options");
    let root = host_root(&cell);
    let tools = root.join("tools");
    mkdir(&tools, 0o755);
    let busybox = cell
        .store()
        .join(cell.image.digest.trim_start_matches("sha256:"))
        .join("rootfs/bin/busybox");
    fs::copy(&busybox, tools.join("busybox")).expect("copy busybox");
    let null = rustix::fs::Mode::from_raw_mode(0o666);
    let device = rustix::fs::FileType::CharacterDevice;
    rustix::fs::mknodat(
        rustix::fs::CWD,
        tools.join("null"),
        device,
        null,
        rustix::fs::makedev(1, 3),
    )
    .expect("make a device node");
    rustix::fs::chmod(tools.join("null"), null).expect("its mode");
    let script = "exec 2>&1; /ex/busybox true; echo ex=$?; /ne/busybox true; echo ne=$?; \
                  /work/busybox true; echo cow=$?; \
                  for m in ro rw work; do cat /$m/null; echo dev-$m=$?; done; \
                  echo --; cat /proc/self/mountinfo";
    let mut lease = cell.lease("options", &["/bin/sh", "-c", script], Shape::default());
    let mut mounts = Mounts::new(&cell, &lease);
    let read_only = Attributes {
        read_only: true,
        no_exec: false,
    };
    let ex = mounts.clone(0, &root, "tools", "", read_only);
    let ne = mounts.clone(
        1,
        &root,
        "tools",
        "",
        Attributes {
            read_only: true,
            no_exec: true,
        },
    );
    let rw = mounts.clone(2, &root, "tools", "", Attributes::default());
    mounts.clone(
        3,
        &root,
        "tools",
        "",
        Attributes {
            read_only: false,
            no_exec: true,
        },
    );
    let cow = mounts.cow(&cell, 3, true);
    lease.spec.mounts = vec![
        bind("/work", &cow, false, true),
        bind("/ex", &ex, true, false),
        bind("/ne", &ne, true, true),
        bind("/ro", &ex, true, false),
        bind("/rw", &rw, false, false),
    ];
    let image = busybox
        .parent()
        .and_then(Path::parent)
        .expect("the image root")
        .to_owned();
    lease.spec.mounted_root = Some(mounts.root(&cell, vec![image]));
    let ran = run(&cell, &lease).await;
    let out = ran.out();
    let (said, table) = out.split_once("--\n").expect("two parts");
    for line in [
        "ex=0",
        "ne=126",
        "cow=126",
        "dev-ro=1",
        "dev-rw=1",
        "dev-work=1",
    ] {
        assert!(said.lines().any(|l| l == line), "{line}: {}", ran.said);
    }
    assert_eq!(said.matches("Permission denied").count(), 5, "{}", ran.said);
    assert!(!said.contains("Operation not permitted"), "{}", ran.said);
    let lines = mount_lines(table);
    for point in ["/ro", "/rw", "/work", "/ne"] {
        let mount = line_at(&lines, point);
        for flag in ["nosuid", "nodev"] {
            assert!(
                mount.options.iter().any(|o| o == flag),
                "{point} {flag}: {table}"
            );
        }
    }
    assert!(
        line_at(&lines, "/ne").options.iter().any(|o| o == "noexec"),
        "{table}"
    );
    assert!(
        !line_at(&lines, "/ex").options.iter().any(|o| o == "noexec"),
        "{table}"
    );
}

/// Catches a mount whose source's submounts are not in it, a host mount made after the
/// start that reaches the job (the clone left in its source's peer group), a clone that is
/// not private, read-only and `nosuid`/`nodev` on the host, and an unmount of the clone
/// that propagates to the source's own submount: over a shared source with a mount below
/// it, the job reads the file below; a mount the test makes on the host after the job has
/// said it is up is not in the job; on the host the clone has no propagation tag and its
/// flags; and after the clone is unmounted the source's own mount below it is still there.
#[tokio::test]
#[ignore = "real crun, root and a job user: run through cargo xtask crun-tests --job-user"]
async fn a_mount_is_private_with_what_its_source_had_below_it() {
    let cell = Cell::new("private");
    let job = cell.job_ids();
    let root = host_root(&cell);
    let source = root.join("shared");
    for dir in [
        source.join("below"),
        source.join("later"),
        root.join("below-src"),
        root.join("later-src"),
    ] {
        owned_dir(&dir, job);
    }
    owned_file(&root.join("below-src/file"), "below\n", job);
    owned_file(&root.join("later-src/file"), "later\n", job);
    let sync = cell.sync_dir("private-sync", &["ready", "go"]);
    let ready = sync.source.join("ready");
    let go = sync.source.join("go");
    let script = "cat /m/below/file; echo ready > /sync/ready; read word < /sync/go; \
                  ls -A /m/later; echo listed";
    let mut lease = cell.lease("private", &["/bin/sh", "-c", script], Shape::default());
    let mut mounts = Mounts::new(&cell, &lease);
    // The source is a shared mount with a mount below it, as a provider's tree may be.
    mounts.host_bind(&source, &source);
    rustix::mount::mount_change(&source, rustix::mount::MountPropagationFlags::SHARED)
        .expect("make the source shared");
    mounts.host_bind(&root.join("below-src"), &source.join("below"));
    let clone = mounts.clone(
        0,
        &root,
        "shared",
        "",
        Attributes {
            read_only: true,
            no_exec: false,
        },
    );
    let on_host = host_lines();
    let point = clone.to_str().expect("utf-8");
    let cloned = line_at(&on_host, point);
    for flag in ["ro", "nosuid", "nodev"] {
        assert!(
            cloned.options.iter().any(|o| o == flag),
            "{flag}: {:?}",
            cloned.options
        );
    }
    assert!(
        cloned.tags.is_empty(),
        "the clone propagates: {:?}",
        cloned.tags
    );
    let below = format!("{point}/below");
    assert!(
        line_at(&on_host, &below).tags.is_empty(),
        "{below} propagates"
    );
    let image = cell
        .store()
        .join(cell.image.digest.trim_start_matches("sha256:"))
        .join("rootfs");
    lease.spec.mounted_root = Some(mounts.root(&cell, vec![image]));
    lease.spec.mounts = vec![bind("/m", &clone, true, false), sync];
    let mut running = cell.start(&lease).await;
    let mut pipe = tokio::net::unix::pipe::OpenOptions::new()
        .read_write(true)
        .open_receiver(&ready)
        .expect("open the ready pipe");
    let mut word = [0u8; 16];
    tokio::select! {
        n = pipe.read(&mut word) => assert_eq!(&word[..n.expect("read")], b"ready\n"),
        () = running.wait() => panic!("the job ended before it was up"),
    }
    mounts.host_bind(&root.join("later-src"), &source.join("later"));
    assert!(
        source.join("later/file").is_file(),
        "the host sees its own mount"
    );
    let mut release = tokio::net::unix::pipe::OpenOptions::new()
        .read_write(true)
        .open_sender(&go)
        .expect("open the go pipe");
    tokio::io::AsyncWriteExt::write_all(&mut release, b"go\n")
        .await
        .expect("release the job");
    let ran = cell.finish(&lease, running).await;
    assert_eq!(ran.out(), "below\nlisted\n", "{}", ran.said);
    mounts.unmount_lease();
    let after = host_lines();
    let source_below = source.join("below");
    let source_below = source_below.to_str().expect("utf-8");
    line_at(&after, source_below);
    assert!(
        !after.iter().any(|m| m.point.starts_with(point)),
        "the clone is still mounted"
    );
    assert_eq!(
        fs::read_to_string(source.join("below/file")).expect("below"),
        "below\n"
    );
}

/// Catches a read-only mount whose source's submounts stay writable (read-only set on the
/// clone's top mount alone): a write below a writable mount of the source fails
/// "Read-only file system" in the job, and nothing reaches the host.
#[tokio::test]
#[ignore = "real crun, root and a job user: run through cargo xtask crun-tests --job-user"]
async fn a_read_only_mount_is_read_only_all_the_way_down() {
    let cell = Cell::new("recursive");
    let job = cell.job_ids();
    let root = host_root(&cell);
    owned_dir(&root.join("src/w"), job);
    owned_dir(&root.join("w-src"), job);
    let script = "exec 2>&1; echo x > /ro/w/new; echo w=$?";
    let mut lease = cell.lease("recursive", &["/bin/sh", "-c", script], Shape::default());
    let mut mounts = Mounts::new(&cell, &lease);
    mounts.host_bind(&root.join("w-src"), &root.join("src/w"));
    let ro = mounts.clone(
        0,
        &root,
        "src",
        "",
        Attributes {
            read_only: true,
            no_exec: false,
        },
    );
    let image = cell
        .store()
        .join(cell.image.digest.trim_start_matches("sha256:"))
        .join("rootfs");
    lease.spec.mounted_root = Some(mounts.root(&cell, vec![image]));
    lease.spec.mounts = vec![bind("/ro", &ro, true, false)];
    let ran = run(&cell, &lease).await;
    let out = ran.out();
    assert!(out.contains("Read-only file system"), "{}", ran.said);
    assert!(!out.contains("w=0"), "{}", ran.said);
    assert!(
        !root.join("w-src/new").exists(),
        "the write reached the host"
    );
}

/// Catches a mount with a sub-path that shows the host path's root, or the sub-path's
/// parent: the job lists the sub-path's directory alone.
#[tokio::test]
#[ignore = "real crun, root and a job user: run through cargo xtask crun-tests --job-user"]
async fn a_sub_path_mount_shows_that_directory_alone() {
    let cell = Cell::new("sub-path");
    let job = cell.job_ids();
    let root = host_root(&cell);
    owned_dir(&root.join("tree/keep"), job);
    owned_file(&root.join("tree/keep/inside"), "in\n", job);
    owned_file(&root.join("tree/beside"), "beside\n", job);
    let mut lease = cell.lease("sub-path", &["/bin/ls", "-A", "/sub"], Shape::default());
    let mut mounts = Mounts::new(&cell, &lease);
    let sub = mounts.clone(
        0,
        &root,
        "tree",
        "keep",
        Attributes {
            read_only: true,
            no_exec: false,
        },
    );
    let image = cell
        .store()
        .join(cell.image.digest.trim_start_matches("sha256:"))
        .join("rootfs");
    lease.spec.mounted_root = Some(mounts.root(&cell, vec![image]));
    lease.spec.mounts = vec![bind("/sub", &sub, true, false)];
    let ran = run(&cell, &lease).await;
    assert_eq!(ran.out(), "inside\n", "{}", ran.said);
}

/// Catches a removal that walks into a mount left in the tree when that mount is a bind of
/// a directory of the same file system (same device number): the removal fails and the
/// bound directory's file stays; once unmounted, the tree is removed.
#[tokio::test]
#[ignore = "real crun, root and a job user: run through cargo xtask crun-tests --job-user"]
async fn remove_tree_does_not_cross_a_bind_of_the_same_file_system() {
    let cell = Cell::new("cross");
    let top = cell.scratch.join("top");
    let other = cell.scratch.join("other");
    mkdir(&top.join("sub"), 0o755);
    mkdir(&other, 0o755);
    fs::write(other.join("file"), "kept\n").expect("write");
    rustix::mount::mount_bind(&other, top.join("sub")).expect("bind");
    let crossed = remove_tree(&top);
    let still = other.join("file").is_file();
    Kernel.detach(&top.join("sub")).expect("unmount");
    let why = crossed.expect_err("the removal crossed the bind");
    assert_eq!(why.kind(), std::io::ErrorKind::ResourceBusy, "{why}");
    assert!(still, "the bound directory's file was removed");
    remove_tree(&top).expect("removed once unmounted");
    assert!(!top.exists());
}

/// Catches a mount directory bound a second time at a restart (it would hide the mounts
/// below it), left shared (a mount below it would propagate to the host's other
/// namespaces), or not made a mount point: after two calls, as at a first start and a
/// restart with a mount below it, the directory is one mount, with no propagation tag, and
/// the mount below it is still seen through it.
#[tokio::test]
#[ignore = "real crun, root and a job user: run through cargo xtask crun-tests --job-user"]
async fn the_mount_directory_is_bound_once_and_private() {
    let cell = Cell::new("mount-dir");
    let dir = cell.scratch.join("mounts");
    let source = cell.scratch.join("source");
    mkdir(&dir.join("lease/src/0"), 0o711);
    mkdir(&source, 0o755);
    fs::write(source.join("file"), "kept\n").expect("write");
    job_fs::make_private_dir(&Kernel, &dir).expect("first start");
    let point = dir.join("lease/src/0");
    rustix::mount::mount_bind(&source, &point).expect("a mount below it");
    let made = job_fs::make_private_dir(&Kernel, &dir);
    let lines = host_lines();
    let text = dir.to_str().expect("utf-8");
    let at_dir: Vec<&MountLine> = lines.iter().filter(|m| m.point == text).collect();
    let below = fs::read_to_string(point.join("file"));
    let _ = Kernel.detach(&point);
    let _ = Kernel.detach(&dir);
    made.expect("restart");
    assert_eq!(at_dir.len(), 1, "bound more than once");
    assert!(at_dir[0].tags.is_empty(), "{:?}", at_dir[0].tags);
    assert_eq!(below.expect("the mount below is seen"), "kept\n");
}
