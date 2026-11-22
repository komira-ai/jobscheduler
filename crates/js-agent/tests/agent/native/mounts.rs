//! The mounts of a job-keyed start on a Mac (`docs/design/job-rootfs.md`, 5.1, 5.2, 5.4):
//! what the runtime refuses at admission, what it resolves and refuses when it looks up the
//! host paths, the links it makes in `exec/`, and the grants in the profile the supervisor is
//! asked to apply.

use std::fs;
use std::os::unix::fs::{MetadataExt as _, symlink};
use std::path::{Path, PathBuf};

use js_agent::lifecycle::{FsPrepared, JobFs, NativeRefusal, ResolvedMount, Runtime};
use js_agent::native::{MountRoot, NativeFlags, NativeSettings, PfFlags};
use js_agent_wire::{
    HostPort, JobMount, MountMode, MountSource, PortProtocol, Propagation, refusal,
};
use js_mac_pool::seatbelt::AbsPath;

use super::{Rig, native};
use crate::support::Scratch;

/// A rig whose runtime holds the roots `roots/ro` (read-only) and `roots/rw`, both made, and
/// every root a read deny of every run, as the agent's settings make them.
fn rig(name: &str) -> Rig {
    Rig::in_dir(name, 1, |config, dir| {
        for root in ["ro", "rw"] {
            fs::create_dir_all(dir.path("roots").join(root)).unwrap();
            let path = fs::canonicalize(dir.path("roots").join(root)).unwrap();
            config.read_denies.push(AbsPath::new(&path).unwrap());
            config.mount_roots.push(MountRoot {
                path,
                read_only: root == "ro",
                trees: false,
            });
        }
    })
}

/// A directory `rel` below the root `root` of the rig's directory, made, canonical.
fn dir_in(rig: &Rig, root: &str, rel: &str) -> PathBuf {
    let path = rig.dir.path("roots").join(root).join(rel);
    fs::create_dir_all(&path).unwrap();
    fs::canonicalize(path).unwrap()
}

fn mount(source: &Path, target: &str, mode: MountMode, no_exec: bool) -> JobMount {
    JobMount {
        source: MountSource::HostPath {
            path: source.display().to_string(),
        },
        target: target.to_owned(),
        mode,
        sub_path: None,
        no_exec,
        propagation: Propagation::Private,
    }
}

fn fs_of(mounts: Vec<JobMount>) -> JobFs {
    JobFs {
        layers: Vec::new(),
        mounts,
    }
}

fn refusal_of(code: &'static str, why: &str) -> NativeRefusal {
    NativeRefusal {
        code,
        why: why.to_owned(),
    }
}

/// The run directory `exec` of the account the `n`th launch (from 0) of `rig` ran as.
fn exec_of(rig: &Rig, n: usize) -> PathBuf {
    let launched = &rig.launcher.launched()[n];
    rig.home_of(launched.var("USER").expect("the account"))
        .join("exec")
}

/// The profile parameter whose value is `value`.
fn param_for(params: &[(String, String)], value: &Path) -> String {
    let wanted = value.display().to_string();
    let found = params.iter().find(|(_, path)| *path == wanted);
    found
        .unwrap_or_else(|| panic!("no parameter for {wanted}"))
        .0
        .clone()
}

/// The position of the line `line` in `profile`.
fn at(profile: &str, line: &str) -> usize {
    profile
        .lines()
        .position(|one| one == line)
        .unwrap_or_else(|| panic!("no line {line:?} in\n{profile}"))
}

/// Asserts a start of the kbf shape: a read-only no-exec mount at `/work/.kbf-tree` from the
/// read-only root and a read-write no-exec mount at `/work/.kbf-out` from the writable one.
/// Both are admitted and resolved at once, the links `exec/.kbf-tree` and `exec/.kbf-out`
/// point at the canonical host directories once the start is done, and the profile the
/// supervisor was asked to apply carries a `GR_` parameter for each: a read grant for both, a write grant for the read-write
/// one only, an exec deny for both, the grants after the read denies of the two roots and the
/// exec denies after the grants, and the read-only link is write-denied (an input) while the
/// read-write one is not. Catches a mount that is linked but not granted (the root's read deny
/// would hide it), a read-only mount that can be written, and an exec deny that a grant
/// reopens.
#[test]
fn a_kbf_shaped_start_links_both_mounts_and_grants_them_in_its_profile() {
    let mut rig = rig("mounts-kbf");
    let tree = dir_in(&rig, "ro", "tree");
    let out = dir_in(&rig, "rw", "out");
    fs::write(tree.join("input.txt"), "in").unwrap();
    let mounts = fs_of(vec![
        mount(&tree, "/work/.kbf-tree", MountMode::ReadOnly, true),
        mount(&out, "/work/.kbf-out", MountMode::ReadWrite, true),
    ]);
    assert_eq!(rig.rt.check_job_fs(&mounts, &[]), Ok(()));
    assert_eq!(rig.rt.resolve_fs("r1", "", &mounts, 0), FsPrepared::Ready);
    rig.rt
        .create_and_start(&rig.spec("r1", native("/bin/true", &[])))
        .unwrap();

    let exec = exec_of(&rig, 0);
    assert_eq!(fs::read_link(exec.join(".kbf-tree")).unwrap(), tree);
    assert_eq!(fs::read_link(exec.join(".kbf-out")).unwrap(), out);
    assert_eq!(
        fs::read_to_string(exec.join(".kbf-tree/input.txt")).unwrap(),
        "in"
    );

    let parsed = rig.launcher.last();
    let (tree_gr, out_gr) = (
        param_for(&parsed.params, &tree),
        param_for(&parsed.params, &out),
    );
    assert!(tree_gr.starts_with("GR_") && out_gr.starts_with("GR_"));
    let profile = parsed.profile.clone();
    let line = |rule: &str, name: &str| format!("({rule} (subpath (param \"{name}\")))");
    for name in [&tree_gr, &out_gr] {
        let read = line("allow file-read*", name);
        at(&profile, &read);
        at(&profile, &line("deny process-exec*", name));
        at(&profile, &line("deny file-map-executable", name));
    }
    at(
        &profile,
        &format!("(allow file-read-metadata (path-ancestors (param \"{tree_gr}\")))"),
    );
    at(&profile, &line("allow file-write*", &out_gr));
    assert!(
        !profile.contains(&line("allow file-write*", &tree_gr)),
        "the read-only mount is writable:\n{profile}"
    );

    // Each root is a read deny, before the grants; the exec denies follow the grants.
    let ro_root = tree.parent().unwrap();
    let rw_root = out.parent().unwrap();
    let denies: Vec<usize> = [ro_root, rw_root]
        .iter()
        .map(|root| {
            let name = param_for(&parsed.params, root);
            assert!(name.starts_with("RD_"), "{name}");
            at(&profile, &line("deny file-read*", &name))
        })
        .collect();
    let grants = [
        at(&profile, &line("allow file-read*", &tree_gr)),
        at(&profile, &line("allow file-read*", &out_gr)),
        at(&profile, &line("allow file-write*", &out_gr)),
    ];
    let execs = [
        at(&profile, &line("deny process-exec*", &tree_gr)),
        at(&profile, &line("deny file-map-executable", &out_gr)),
    ];
    assert!(denies.iter().max() < grants.iter().min());
    assert!(grants.iter().max() < execs.iter().min());

    // The read-only link may not be replaced; the read-write one is not write-denied.
    let protected = param_for(&parsed.params, &exec.join(".kbf-tree"));
    assert!(protected.starts_with("IN_"), "{protected}");
    assert!(
        parsed
            .params
            .iter()
            .all(|(_, value)| *value != exec.join(".kbf-out").display().to_string())
    );
}

/// Asserts the refusals made from the text of a start, with no I/O, by code and by exact
/// detail: a layer, a copy-on-write mount, a remapped host port, a target that is `/work`,
/// outside it or below another mount's (in any case), a provider's tree mounted read-write,
/// and more mounts than the profile lists; and that what a Mac gives is admitted: a
/// provider's tree mounted read-only, a host port at its own number (equal, or its job
/// port left out) is not refused here (the lifecycle's host-port admission refuses it, as this
/// runtime offers none), and mounts at siblings and a nested directory. Catches a mount that
/// would be linked over the working directory or inside another mount's host directory, a
/// remap that is silently run at the host's number, and a layer that is ignored.
#[test]
fn what_a_mac_cannot_give_is_refused_with_its_own_code() {
    let rig = rig("mounts-check");
    let dir = dir_in(&rig, "rw", "d");
    let ro = |target: &str| mount(&dir, target, MountMode::ReadOnly, false);
    let port = |host_port, container_port| HostPort {
        host_port,
        container_port,
        protocol: PortProtocol::Tcp,
    };
    let layered = JobFs {
        layers: vec![js_agent_wire::Layer::HostPath {
            path: dir.display().to_string(),
        }],
        mounts: Vec::new(),
    };
    let target = |bad: &str| {
        refusal_of(
            refusal::MOUNT_TARGET_UNSUPPORTED_HERE,
            &format!(
                "the mount target {bad} is not below /work: a native job's mounts are links in \
                 its working directory"
            ),
        )
    };
    let many: Vec<JobMount> = (0..65).map(|i| ro(&format!("/work/m{i}"))).collect();
    let tree = |mode| JobMount {
        source: MountSource::Provider {
            provider: "tierstore".to_owned(),
            reference: "cas/l/ab".to_owned(),
        },
        ..mount(&dir, "/work/.kbf-tree", mode, true)
    };
    let cases: Vec<(JobFs, Vec<HostPort>, Result<(), NativeRefusal>)> = vec![
        (fs_of(vec![tree(MountMode::ReadOnly)]), vec![], Ok(())),
        (
            fs_of(vec![tree(MountMode::ReadWrite)]),
            vec![],
            Err(refusal_of(
                refusal::INPUT_NOT_ALLOWED,
                "/work/.kbf-tree: a provider's tree is shared by every native job that names \
                 it, so a native job mounts it read-only only",
            )),
        ),
        (
            layered,
            vec![],
            Err(refusal_of(
                refusal::ROOTFS_UNSUPPORTED_HERE,
                "a native job has no root for layers to replace",
            )),
        ),
        (
            fs_of(vec![mount(&dir, "/work/x", MountMode::CopyOnWrite, false)]),
            vec![],
            Err(refusal_of(
                refusal::ROOTFS_UNSUPPORTED_HERE,
                "/work/x: a copy-on-write mount needs a mechanism this agent does not have",
            )),
        ),
        (
            fs_of(vec![]),
            vec![port(8080, Some(80))],
            Err(refusal_of(
                refusal::HOST_PORT_REMAP_UNSUPPORTED_HERE,
                "host port 8080 is asked at job port 80: a native job shares the host's \
                 loopback and reaches a host port only at its own number",
            )),
        ),
        (fs_of(vec![ro("/work")]), vec![], Err(target("/work"))),
        (
            fs_of(vec![ro("/elsewhere")]),
            vec![],
            Err(target("/elsewhere")),
        ),
        (
            fs_of(vec![ro("/work/a"), ro("/work/A/b")]),
            vec![],
            Err(refusal_of(
                refusal::MOUNT_TARGET_UNSUPPORTED_HERE,
                "/work/A/b lies below the mount at /work/a: a native job's mounts are links \
                 and do not nest",
            )),
        ),
        (
            fs_of(many),
            vec![],
            Err(refusal_of(
                refusal::TOO_MANY_INPUTS_HERE,
                "65 mounts: a native job's profile lists at most 64",
            )),
        ),
        (
            fs_of(vec![ro("/work/a"), ro("/work/ab"), ro("/work/b/c")]),
            vec![],
            Ok(()),
        ),
        (
            fs_of(vec![]),
            vec![port(8080, Some(8080)), port(9090, None)],
            Ok(()),
        ),
    ];
    for (fs, ports, want) in cases {
        assert_eq!(rig.rt.check_job_fs(&fs, &ports), want, "{fs:?} {ports:?}");
    }
    assert!(rig.rt.layered_rootfs());
}

/// Asserts the refusals of the lookup of the host paths, by code and exact detail: a path that
/// is not there and one that is a file (`input-unavailable`), a link inside a root that leads
/// outside every root and a read-write mount that resolves beneath the read-only root
/// (`input-not-allowed`: the lifecycle's own check reads the text of the path only), a mount
/// of a provider that has not answered, a path the profile cannot name (`invalid-mount`) and
/// a bad target. The first refusal wins and no plan is kept: a start after it makes no link.
/// Catches a link that gets a job out of the roots, and a read-write grant over a read-only
/// root.
#[test]
fn a_host_path_that_cannot_be_had_beneath_a_root_is_refused_by_code() {
    let mut rig = rig("mounts-resolve");
    let rw = fs::canonicalize(rig.dir.path("roots").join("rw")).unwrap();
    let ro = fs::canonicalize(rig.dir.path("roots").join("ro")).unwrap();
    let outside = rig.dir.path("outside");
    fs::create_dir(&outside).unwrap();
    let outside = fs::canonicalize(outside).unwrap();
    symlink(&outside, rw.join("leads-out")).unwrap();
    symlink(&ro, rw.join("leads-to-ro")).unwrap();
    fs::write(rw.join("file"), "x").unwrap();
    let odd = dir_in(&rig, "rw", "odd\nname");

    let missing = rw.join("missing");
    let missing_why = fs::canonicalize(&missing).unwrap_err();
    let provider = JobMount {
        source: MountSource::Provider {
            provider: "tier".to_owned(),
            reference: "r".to_owned(),
        },
        ..mount(&rw, "/work/p", MountMode::ReadOnly, false)
    };
    let cases: Vec<(JobMount, NativeRefusal)> = vec![
        (
            mount(&missing, "/work/a", MountMode::ReadOnly, false),
            refusal_of(
                refusal::INPUT_UNAVAILABLE,
                &format!("{}: {missing_why}", missing.display()),
            ),
        ),
        (
            mount(&rw.join("file"), "/work/a", MountMode::ReadOnly, false),
            refusal_of(
                refusal::INPUT_UNAVAILABLE,
                &format!("{}: not a directory", rw.join("file").display()),
            ),
        ),
        (
            mount(&rw.join("leads-out"), "/work/a", MountMode::ReadOnly, false),
            refusal_of(
                refusal::INPUT_NOT_ALLOWED,
                &format!(
                    "{} lies beneath no --mount-root or --mount-root-ro once its links are followed",
                    outside.display()
                ),
            ),
        ),
        (
            mount(
                &rw.join("leads-to-ro"),
                "/work/a",
                MountMode::ReadWrite,
                false,
            ),
            refusal_of(
                refusal::INPUT_NOT_ALLOWED,
                &format!(
                    "{} lies beneath the root {}, which jobs may mount read-only only",
                    ro.display(),
                    ro.display()
                ),
            ),
        ),
        (
            provider,
            refusal_of(
                refusal::INPUT_NOT_ALLOWED,
                "/work/p: a provider's tree is looked up once the provider has answered its \
                 directory",
            ),
        ),
        (
            mount(&odd, "/work/a", MountMode::ReadOnly, false),
            refusal_of(
                refusal::INVALID_MOUNT,
                &format!("{}: path contains a control character", odd.display()),
            ),
        ),
        (
            mount(&rw, "/work", MountMode::ReadOnly, false),
            refusal_of(
                refusal::MOUNT_TARGET_UNSUPPORTED_HERE,
                "the mount target /work is not below /work: a native job's mounts are links in \
                 its working directory",
            ),
        ),
    ];
    for (mount, want) in cases {
        let found = rig.rt.resolve_fs("r1", "", &fs_of(vec![mount.clone()]), 0);
        assert_eq!(found, FsPrepared::Refused(want), "{mount:?}");
    }
    // A refusal keeps no plan: a start of the run links nothing.
    rig.rt
        .create_and_start(&rig.spec("r1", native("/bin/true", &[])))
        .unwrap();
    let exec = exec_of(&rig, 0);
    assert_eq!(visible(&exec), 0);
}

/// Asserts that a read-only mount may come from either kind of root, a read-write one only
/// from a writable root, that a `sub_path` is resolved beneath the source and the link points
/// at the result, and that a nested target `/work/a/b` makes the directory `exec/a`, the run's
/// user's `0700`, and the link inside it. Catches a `sub_path` that is ignored (the whole
/// source linked) and a nested link whose parent is not made.
#[test]
fn a_sub_path_is_linked_and_a_nested_target_gets_its_parent_directory() {
    let mut rig = rig("mounts-nested");
    let rw = dir_in(&rig, "rw", "tree/sub");
    let source = rw.parent().unwrap().to_path_buf();
    let mut sub = mount(&source, "/work/a/b", MountMode::ReadWrite, false);
    sub.sub_path = Some("sub".to_owned());
    let ro = dir_in(&rig, "ro", "x");
    let mounts = fs_of(vec![
        sub,
        mount(&ro, "/work/c/d/e", MountMode::ReadOnly, true),
    ]);
    assert_eq!(rig.rt.resolve_fs("r1", "", &mounts, 0), FsPrepared::Ready);
    rig.rt
        .create_and_start(&rig.spec("r1", native("/bin/true", &[])))
        .unwrap();
    let exec = exec_of(&rig, 0);
    assert_eq!(fs::read_link(exec.join("a/b")).unwrap(), rw);
    assert_eq!(fs::read_link(exec.join("c/d/e")).unwrap(), ro);
    for parent in ["a", "c", "c/d"] {
        let meta = fs::symlink_metadata(exec.join(parent)).unwrap();
        assert!(meta.is_dir(), "{parent}");
        assert_eq!(meta.mode() & 0o7777, 0o700, "{parent}");
    }
}

/// Asserts that `discard_fs` drops the plan of a run, is the same when called again or for a
/// run it never saw, and answers `Ok` each time: a run started after its discard has no link
/// and no grant. Catches a plan that outlives its run's cleanup, which a later run of the
/// same id would be started with.
#[test]
fn discarding_the_file_system_of_a_run_drops_its_plan_and_is_idempotent() {
    let mut rig = rig("mounts-discard");
    let out = dir_in(&rig, "rw", "out");
    let mounts = fs_of(vec![mount(&out, "/work/out", MountMode::ReadWrite, true)]);
    assert_eq!(rig.rt.resolve_fs("r1", "", &mounts, 0), FsPrepared::Ready);
    assert!(rig.rt.discard_fs("r1").is_ok());
    assert!(rig.rt.discard_fs("r1").is_ok());
    assert!(rig.rt.discard_fs("never-seen").is_ok());
    rig.rt
        .create_and_start(&rig.spec("r1", native("/bin/true", &[])))
        .unwrap();
    let exec = exec_of(&rig, 0);
    assert_eq!(visible(&exec), 0);
    let parsed = rig.launcher.last();
    assert!(
        parsed
            .params
            .iter()
            .all(|(name, _)| !name.starts_with("GR_"))
    );
}

/// A start whose link has a name taken (a staged input of the legacy form at the same `dest`)
/// fails, naming the mount, and the account the start was lent is retired: its lease is
/// done, nothing is held and the supervisor is never asked to run anything. Catches a link
/// made through whatever was at the name, and a failed start that leaves its account.
#[test]
fn a_link_whose_name_is_taken_fails_the_start_and_removes_its_user() {
    let mut rig = rig("mounts-taken");
    let out = dir_in(&rig, "rw", "out");
    let taken = dir_in(&rig, "rw", "taken");
    let mounts = fs_of(vec![mount(&out, "/work/out", MountMode::ReadWrite, false)]);
    assert_eq!(rig.rt.resolve_fs("r1", "", &mounts, 0), FsPrepared::Ready);
    let mut spec = rig.spec("r1", native("/bin/true", &[]));
    spec.mounts = vec![ResolvedMount {
        host: taken,
        target: "out".to_owned(),
        ro: true,
    }];
    let error = rig.rt.create_and_start(&spec).unwrap_err();
    assert!(error.0.starts_with("a mount: /work/out: "), "{}", error.0);
    assert!(rig.launcher.launched().is_empty(), "no supervisor");
    assert!(rig.pool.held().is_empty(), "{:?}", rig.pool.held());
    let text = fs::read_to_string(rig.dir.path("native-leases")).unwrap();
    assert_eq!(text.lines().collect::<Vec<_>>(), ["+ 7.0", "- 7.0"]);
}

/// Asserts that the mount roots of the agent's flags become read denies of every run and the
/// roots a job's mounts must lie beneath, once and in addition to the agent's own denies, and
/// that a root the profile cannot name is refused naming the flag. Catches a mount root that
/// a run can read whole, one listed twice, and a root of a path that would break every start.
#[test]
fn the_mount_roots_are_read_denies_of_every_run() {
    let dir = Scratch::new("native-mount-roots");
    let root = dir.path("root");
    fs::create_dir(&root).unwrap();
    let root = fs::canonicalize(root).unwrap();
    let flags = NativeFlags {
        accounts: Some(Path::new("/var/db/js-accounts")),
        account_range: Some("501-510"),
        account_ready: None,
        homes: None,
        network: None,
        tool_roots: &[],
        deny_read: &[],
        max_file_mib: None,
        max_processes: None,
        pf: PfFlags::default(),
    };
    let settings = NativeSettings::from_flags(&flags, &[root.as_path()], Path::new("/srv/journal"))
        .unwrap()
        .unwrap();
    let roots = vec![MountRoot {
        path: root.clone(),
        read_only: true,
        trees: false,
    }];
    let settings = settings.with_mount_roots(roots.clone()).unwrap();
    let config = settings.config();
    let denied: Vec<&str> = config.read_denies.iter().map(AbsPath::as_str).collect();
    let once = denied
        .iter()
        .filter(|path| **path == root.to_str().unwrap())
        .count();
    assert_eq!(once, 1, "{denied:?}");
    assert_eq!(config.mount_roots, roots);

    let bad = vec![MountRoot {
        path: PathBuf::from("/tmp/root"),
        read_only: false,
        trees: false,
    }];
    let why = settings.with_mount_roots(bad).unwrap_err();
    assert_eq!(
        why,
        "--mount-root: /tmp/root: path starts with /tmp, a symbolic link; use the canonical \
         /private spelling"
    );
}

/// How many entries of the run directory `exec` the job did not get from the runtime: all
/// but the hidden directories the runtime makes there for the job's home and temporary files.
fn visible(exec: &Path) -> usize {
    fs::read_dir(exec)
        .unwrap()
        .filter(|entry| {
            !entry
                .as_ref()
                .unwrap()
                .file_name()
                .to_string_lossy()
                .starts_with('.')
        })
        .count()
}
