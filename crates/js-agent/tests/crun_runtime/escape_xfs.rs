//! The escape walk ([`crate::escape`]) over [`Disk`] on the XFS volume, without a container:
//! the entries a job cannot make by itself (a descendant of another project below a bound
//! directory, a link that leads off the run) are made here by the test, as root, and the walk
//! must count exactly the bytes outside the run's project.

use std::fs;
use std::io::Write as _;
use std::os::unix::fs::{MetadataExt as _, symlink};
use std::path::{Path, PathBuf};

use js_agent::quota::{ProjectId, ProjectIds, ScratchQuota, XfsQuota};

use crate::escape::{self, Disk};
use crate::quota::volume;

/// The limit of a fixture's projects, far above what is written in them.
const ROOM: u64 = 64 << 20;

/// The inode limit of a fixture's projects.
const INODES: u64 = 1_000;

/// A directory of one test on the volume, and the projects bound in it; dropped, the
/// projects are cleared and the directory removed.
struct Fixture {
    base: PathBuf,
    bound: Vec<(XfsQuota, PathBuf, ProjectId)>,
}

impl Fixture {
    fn new(name: &str) -> Self {
        let base = volume().join(format!("js-escape-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&base);
        fs::create_dir(&base).expect("make the fixture's directory");
        Self {
            base,
            bound: Vec::new(),
        }
    }

    /// Makes the empty directory `name` in `parent` and binds it as the project of `slot`.
    fn bound_dir(&mut self, parent: &Path, name: &str, slot: u32) -> (PathBuf, ProjectId) {
        let dir = parent.join(name);
        fs::create_dir(&dir).expect("make a directory to bind");
        let quota = XfsQuota::new(parent.to_owned(), ProjectIds::default());
        let project = ProjectIds::default().of_slot(slot).expect("a project");
        quota
            .apply(&dir, project, ROOM, INODES)
            .unwrap_or_else(|e| panic!("bind {}: {e}", dir.display()));
        self.bound.push((quota, dir.clone(), project));
        (dir, project)
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        // The deepest bound directory first: it is inside the others.
        for (quota, dir, project) in self.bound.drain(..).rev() {
            let _ = fs::remove_dir_all(&dir);
            let _ = quota.clear(&dir, project);
        }
        let _ = fs::remove_dir_all(&self.base);
    }
}

/// Writes `bytes` of zeros to the new file `path` and syncs it.
fn write(path: &Path, bytes: u64) {
    let mut file = fs::File::create_new(path).expect("make a file");
    file.write_all(&vec![0_u8; bytes as usize])
        .expect("write the file");
    file.sync_all().expect("sync the file");
}

/// The bytes `path` itself holds on its device, a link not followed.
fn held(path: &Path) -> u64 {
    fs::symlink_metadata(path).expect("lstat").blocks() * 512
}

/// The walk of `dirs` on disk, for `project`.
fn walk(dirs: &[&Path], project: ProjectId) -> u64 {
    let dev = fs::metadata(volume()).expect("stat the volume").dev();
    escape::escaped(&Disk, dev, dirs, project.get()).unwrap_or_else(|e| panic!("walk: {e}"))
}

/// Asserts that a directory of another, nonzero project below a bound run directory is
/// counted to the byte, with the file in it, and that the run's own file beside it is not. It
/// catches a walk that reads the project of the run's directory only and takes everything
/// below it to share it.
#[test]
#[ignore = "XFS project quota as root: the js-agent-crun-runtime workflow"]
fn a_directory_of_another_project_inside_a_bound_run_is_counted() {
    let mut fx = Fixture::new("foreign");
    let base = fx.base.clone();
    let (run, project) = fx.bound_dir(&base, "run", 60);
    write(&run.join("mine"), 1 << 20);
    let (theirs, other) = fx.bound_dir(&run, "theirs", 61);
    assert_ne!(other, project);
    write(&theirs.join("fill"), 2 << 20);
    let expected = held(&theirs) + held(&theirs.join("fill"));
    assert!(
        expected >= 2 << 20,
        "the foreign file holds {expected} bytes"
    );
    assert_eq!(walk(&[&run], project), expected);
}

/// Asserts, on XFS, that a link, a dangling link and a pipe are counted in their directory's
/// project and that a link is not followed: in a bound run directory they leave nothing
/// escaped, although one link is long enough to hold a block of its own and leads to a
/// directory of project 0 holding 2 MiB; in a directory that is not bound, exactly the bytes
/// of the directory and of the three entries themselves are counted, and not the 2 MiB the
/// link leads to. It catches a walk that opens a link (it would follow it and count, or
/// enter, what it leads to) or a pipe (it would block), or that drops an entry it does not
/// open.
#[test]
#[ignore = "XFS project quota as root: the js-agent-crun-runtime workflow"]
fn links_and_pipes_take_their_directorys_project_and_are_not_followed() {
    let mut fx = Fixture::new("links");
    let base = fx.base.clone();
    let outside = base.join("outside");
    fs::create_dir(&outside).expect("make the directory a link leads to");
    write(&outside.join("big"), 2 << 20);
    // A target long enough that XFS keeps it in a block rather than in the inode.
    let far = format!("{}{}", outside.display(), "/.".repeat(1000));
    let (run, project) = fx.bound_dir(&base, "run", 62);
    let loose = base.join("loose");
    fs::create_dir(&loose).expect("make a directory that is not bound");
    for dir in [&run, &loose] {
        symlink(&far, dir.join("far")).expect("make a long link");
        symlink(base.join("gone"), dir.join("gone")).expect("make a dangling link");
        rustix::fs::mknodat(
            rustix::fs::CWD,
            dir.join("pipe"),
            rustix::fs::FileType::Fifo,
            rustix::fs::Mode::from_raw_mode(0o600),
            0,
        )
        .expect("make a pipe");
    }
    assert!(held(&run.join("far")) > 0, "the long link holds no block");

    assert_eq!(walk(&[&run], project), 0);
    let entries = ["far", "gone", "pipe"].map(|name| held(&loose.join(name)));
    assert_eq!(
        walk(&[&loose], project),
        held(&loose) + entries.iter().sum::<u64>()
    );
}
