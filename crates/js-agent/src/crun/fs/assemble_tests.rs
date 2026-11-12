//! The assembly of a job's file system, call by call, over directories of the test's own
//! and the fake of the mounts' calls of [`super::tests`].

use std::fs;
use std::os::unix::fs::PermissionsExt as _;
use std::path::{Path, PathBuf};

use js_agent_wire::{JobMount, MountMode, refusal};
use js_contain::container::HostMount;
use js_testkit::dir::TestDir;

use super::assemble::{Lower, Owners, Plan, Unassembled, assemble};
use super::list;
use super::tests::{Mounts, Paths, mount, own, owners, scratch};
use crate::lifecycle::NativeRefusal;

/// A host directory with a file and a directory in it, and the lease directories of an
/// assembly.
struct Bench {
    dir: TestDir,
    mounts: PathBuf,
    lease: PathBuf,
}

impl Bench {
    fn new(name: &str) -> Self {
        let dir = scratch(name);
        let mounts = dir.path().join("mounts/lease");
        let lease = dir.path().join(".lease-lease");
        list::make_dir(&mounts).expect("mounts");
        fs::create_dir_all(&lease).expect("lease");
        Self { dir, mounts, lease }
    }

    /// A host directory `name` holding the directories `dirs` and a file.
    fn host(&self, name: &str, dirs: &[&str]) -> PathBuf {
        let host = self.dir.path().join("host").join(name);
        fs::create_dir_all(&host).expect("a host directory");
        fs::write(host.join("file"), b"x").expect("a file");
        for dir in dirs {
            fs::create_dir_all(host.join(dir)).expect("a directory of it");
        }
        host
    }

    /// The plan of `layers` (base first, with the opened host paths of `sources`) and
    /// `mounts` over `image`.
    fn plan(
        &self,
        image: Option<PathBuf>,
        (layers, sources): (Vec<Lower>, Vec<PathBuf>),
        mounts: Vec<JobMount>,
    ) -> Plan {
        let sources = sources
            .iter()
            .map(|path| js_contain::job_fs::open_root(path).expect("open"))
            .collect();
        Plan {
            dir: self.mounts.clone(),
            lease: self.lease.clone(),
            image,
            layers,
            mounts,
            sources,
            owners: owners(),
        }
    }
}

/// Catches a root stacked in the wrong order, a clone not read-only where it must be or
/// read-only where the job writes, a mount listed after it is made, a copy-on-write layer
/// in the wrong place, a target made outside its tree, and the mounts given to `crun` in an
/// order where a child comes before its parent: the whole assembly of a layered root with a
/// read-only, a read-write and a copy-on-write mount, call by call.
#[test]
fn a_layered_root_and_its_mounts_are_assembled_in_order() {
    let bench = Bench::new("assemble-all");
    let image = bench.dir.path().join("store/image");
    fs::create_dir_all(&image).expect("an image tree");
    let base = bench.host("base", &[]);
    let top = bench.host("top", &[]);
    let sdk = bench.host("sdk", &["bin/x"]);
    let out = bench.host("out", &[]);
    let work = bench.host("work", &[]);
    let lower = bench.dir.path().join("store/layer");
    let mut cow = mount(&work, "/work", MountMode::CopyOnWrite);
    cow.no_exec = true;
    let plan = bench.plan(
        Some(image),
        (
            vec![Lower::Source, Lower::Image(lower), Lower::Source],
            vec![base, top, sdk, out, work],
        ),
        vec![
            mount(
                &bench.dir.path().join("host/sdk"),
                "/opt/sdk",
                MountMode::ReadOnly,
            ),
            mount(
                &bench.dir.path().join("host/out"),
                "/work/out",
                MountMode::ReadWrite,
            ),
            cow,
            {
                let mut tool = mount(
                    &bench.dir.path().join("host/sdk"),
                    "/opt/sdk/bin/x",
                    MountMode::ReadOnly,
                );
                tool.no_exec = true;
                tool
            },
            mount(
                &bench.dir.path().join("host/out"),
                "/work/out/cache",
                MountMode::ReadOnly,
            ),
        ],
    );
    // The sources of the fourth and fifth mounts.
    let mut plan = plan;
    for again in ["sdk", "out"] {
        let path = bench.dir.path().join("host").join(again);
        plan.sources
            .push(js_contain::job_fs::open_root(&path).expect("open"));
    }
    let calls = Mounts::under(bench.dir.path());
    let made = assemble(&calls, &Paths::default(), &plan).expect("assembled");
    let m = "mounts/lease";
    let host = |name: &str| format!("host/{name}");
    assert_eq!(
        calls.asked(),
        [
            format!("clone {}", host("base")),
            format!("set {} ro=true noexec=false", host("base")),
            format!("attach {m}/src/0"),
            format!("clone {}", host("top")),
            format!("set {} ro=true noexec=false", host("top")),
            format!("attach {m}/src/1"),
            format!("clone {}", host("sdk")),
            format!("set {} ro=true noexec=false", host("sdk")),
            format!("attach {m}/src/2"),
            format!("clone {}", host("out")),
            format!("set {} ro=false noexec=false", host("out")),
            format!("attach {m}/src/3"),
            format!("clone {}", host("work")),
            format!("set {} ro=false noexec=true", host("work")),
            format!("attach {m}/src/4"),
            format!("clone {}", host("sdk")),
            format!("set {} ro=true noexec=true", host("sdk")),
            format!("attach {m}/src/5"),
            format!("clone {}", host("out")),
            format!("set {} ro=true noexec=false", host("out")),
            format!("attach {m}/src/6"),
            format!(
                "overlay {m}/rootfs noexec=false lowerdir={m}/src/1:store/layer:{m}/src/0:store/image,\
                 upperdir=.lease-lease/rootfs-upper,workdir=.lease-lease/rootfs-work,\
                 index=off,metacopy=off,redirect_dir=off,xino=off"
            ),
            format!(
                "overlay {m}/cow/4 noexec=true lowerdir={m}/src/4,\
                 upperdir=.lease-lease/cow/4/upper,workdir=.lease-lease/cow/4/work,\
                 index=off,metacopy=off,redirect_dir=off,xino=off"
            ),
        ]
    );
    let listed: Vec<PathBuf> = [
        "src/0", "src/1", "src/2", "src/3", "src/4", "src/5", "src/6", "rootfs", "cow/4",
    ]
    .iter()
    .map(|point| bench.mounts.join(point))
    .collect();
    assert_eq!(list::points(&bench.mounts).expect("the list"), listed);
    let mode = |path: &Path| fs::metadata(path).expect("made").permissions().mode() & 0o777;
    assert_eq!(mode(&bench.lease.join("rootfs-upper")), 0o755);
    assert_eq!(mode(&bench.lease.join("rootfs-work")), 0o700);
    assert_eq!(mode(&bench.lease.join("cow/4/upper")), 0o755);
    // Below no mount: made in the root; below the copy-on-write mount: made in it; below a
    // read-only mount, found in its clone; below a read-write mount, `crun`'s to make.
    assert!(bench.mounts.join("rootfs/opt/sdk").is_dir());
    assert!(bench.mounts.join("rootfs/work").is_dir());
    assert!(bench.mounts.join("cow/4/out").is_dir());
    assert!(
        !bench.mounts.join("src/3/cache").exists(),
        "below the read-write mount"
    );
    let bind = |target: &str, source: &str, read_only: bool, no_exec: bool| HostMount {
        target: target.to_owned(),
        source: bench.mounts.join(source),
        read_only,
        recursive: true,
        no_exec,
    };
    assert_eq!(made.root, Some(bench.mounts.join("rootfs")));
    assert_eq!(made.sources, 7);
    assert_eq!(
        made.mounts,
        [
            bind("/work", "cow/4", false, true),
            bind("/opt/sdk", "src/2", true, false),
            bind("/work/out", "src/3", false, false),
            bind("/work/out/cache", "src/6", true, false),
            bind("/opt/sdk/bin/x", "src/5", true, true),
        ]
    );
}

/// Catches a root of layers alone given the image's tree, one with no layers given a root
/// at all, and targets made where they are not the agent's: with no image the root is the
/// layers alone; with no layers there is no root, a target below no mount is `crun`'s, and
/// one below a read-only mount must be in it (`invalid-mount`).
#[test]
fn a_root_of_layers_alone_and_mounts_with_no_root() {
    let bench = Bench::new("assemble-alone");
    let only = bench.host("only", &[]);
    let plan = bench.plan(None, (vec![Lower::Source], vec![only]), Vec::new());
    let calls = Mounts::under(bench.dir.path());
    let made = assemble(&calls, &Paths::default(), &plan).expect("assembled");
    assert_eq!(made.root, Some(bench.mounts.join("rootfs")));
    let overlay = calls.asked().pop().expect("the overlay");
    assert!(
        overlay.contains("lowerdir=mounts/lease/src/0,upperdir="),
        "{overlay}"
    );

    let bench = Bench::new("assemble-no-root");
    let tools = bench.host("tools", &["share"]);
    let found = mount(&tools, "/opt/tools", MountMode::ReadOnly);
    let inner = mount(&tools, "/opt/tools/share", MountMode::ReadOnly);
    let free = mount(&tools, "/data", MountMode::ReadOnly);
    let plan = bench.plan(
        Some(bench.dir.path().join("image")),
        (
            Vec::new(),
            vec![tools.clone(), tools.clone(), tools.clone()],
        ),
        vec![found, inner, free],
    );
    let calls = Mounts::under(bench.dir.path());
    let made = assemble(&calls, &Paths::default(), &plan).expect("assembled");
    assert_eq!(made.root, None);
    assert!(!calls.asked().iter().any(|call| call.starts_with("overlay")));
    let missing = mount(&tools, "/opt/tools/lib", MountMode::ReadOnly);
    let parent = mount(&tools, "/opt/tools", MountMode::ReadOnly);
    let bench = Bench::new("assemble-missing");
    let plan = bench.plan(
        None,
        (Vec::new(), vec![tools.clone(), tools]),
        vec![parent, missing],
    );
    let refused = assemble(&Mounts::under(bench.dir.path()), &Paths::default(), &plan)
        .expect_err("not in the read-only mount");
    let Unassembled::Refused(NativeRefusal { code, why }) = refused else {
        panic!("refused: {refused:?}");
    };
    assert_eq!(code, refusal::INVALID_MOUNT);
    assert!(
        why.starts_with("the target /opt/tools/lib cannot be had below /opt/tools: "),
        "{why}"
    );
}

/// Catches a source the job cannot reach left to fail in `crun`, and a failed mount or
/// listing taken for success: a search refused is `input-unavailable` naming the target, and
/// a clone, an overlay or the list that fails is the host's failure, named.
#[test]
fn an_assembly_that_cannot_be_made_says_why() {
    let bench = Bench::new("assemble-fail");
    let data = bench.host("data", &[]);
    let plan = bench.plan(
        None,
        (Vec::new(), vec![data.clone()]),
        vec![mount(&data, "/data", MountMode::ReadOnly)],
    );
    let paths = Paths {
        search: Some(libc::EACCES),
        ..Paths::default()
    };
    let refused = assemble(&Mounts::under(bench.dir.path()), &paths, &plan).expect_err("refused");
    assert_eq!(
        refused,
        Unassembled::Refused(NativeRefusal {
            code: refusal::INPUT_UNAVAILABLE,
            why: "the job cannot reach the source of /data: Permission denied (os error 13)"
                .to_owned(),
        })
    );
    for (call, why) in [
        ("clone", "clone the tree for"),
        ("overlay", "mount the overlay at"),
    ] {
        let bench = Bench::new(&format!("assemble-fail-{call}"));
        let base = bench.host("base", &[]);
        let plan = bench.plan(None, (vec![Lower::Source], vec![base]), Vec::new());
        let calls = Mounts {
            fail: Some(call),
            ..Mounts::under(bench.dir.path())
        };
        let failed = assemble(&calls, &Paths::default(), &plan).expect_err(call);
        let Unassembled::Failed(text) = failed else {
            panic!("failed: {failed:?}");
        };
        assert!(text.starts_with(why), "{text}");
    }
    let bench = Bench::new("assemble-fail-list");
    let base = bench.host("base", &[]);
    let plan = bench.plan(None, (vec![Lower::Source], vec![base]), Vec::new());
    fs::create_dir_all(bench.mounts.join(list::LIST)).expect("a list that is a directory");
    let failed = assemble(&Mounts::under(bench.dir.path()), &Paths::default(), &plan);
    let Err(Unassembled::Failed(text)) = failed else {
        panic!("failed: {failed:?}");
    };
    assert!(text.starts_with("list a clone: "), "{text}");
}

/// Catches a mount's source checked with the ids of the job user, which runs `crun` and is
/// not mapped in the job's namespace, instead of the job's own: a source of mode `0700`
/// owned by the job's identity, as a provider presents a writable tree (decision LR5 of
/// `docs/design/job-rootfs.md`), is one the job can use and the job user cannot search.
/// With the three identities apart, the read-only and the read-write mount's sources are
/// each searched once, in spec order, as the job's identity and never as another.
#[test]
fn a_mounts_source_is_searched_as_the_job_not_the_job_user() {
    let bench = Bench::new("assemble-search-ids");
    let data = bench.host("data", &[]);
    let out = bench.host("out", &[]);
    let mut plan = bench.plan(
        None,
        (Vec::new(), vec![data.clone(), out.clone()]),
        vec![
            mount(&data, "/data", MountMode::ReadOnly),
            mount(&out, "/out", MountMode::ReadWrite),
        ],
    );
    let (uid, gid) = own();
    let job = (uid.wrapping_add(1000), gid.wrapping_add(1000));
    plan.owners = Owners {
        root: own(),
        job,
        user: (uid.wrapping_add(2000), gid.wrapping_add(2000)),
    };
    let paths = Paths::default();
    assemble(&Mounts::under(bench.dir.path()), &paths, &plan).expect("assembled");
    assert_eq!(*paths.searched.lock().expect("searched"), [job, job]);
}
