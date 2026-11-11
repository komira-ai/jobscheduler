//! A job's layers and mounts on the crun runtime over the stand-in: the runtime looks up the
//! host paths beneath the node's root and assembles the file system through fakes of the
//! mounts' calls (which record each call) and of the reads of host paths, the container is
//! made over what was assembled, and every end of the run removes what was mounted and the
//! pins of the images, from the run's list of mounts. Each bound is the runtime's clock, which
//! reads 5000 here and never moves.

use std::io;
use std::os::fd::{BorrowedFd, OwnedFd};
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use js_agent::crun::fs::{FsSettings, Kernel, PathCalls};
use js_agent::crun::{CrunRuntime, CrunSettings};
use js_agent::lifecycle::{
    Body, FsPrepared, JobFs, MountRoots, RunHandle, RunSpec, Runtime, Status,
};
use js_agent_wire::{JobMount, Layer, MountMode, MountSource, Propagation, refusal};
use js_contain::job_fs::{Attributes, MountCalls, ResolveError};
use serde_json::{Value, json};

use crate::node::{LONG, NoImages, Node};

/// The mounts' calls, recorded; nothing is mounted, and an unmount fails while
/// `stuck` says so.
#[derive(Debug, Default)]
struct Calls(Mutex<Vec<String>>, AtomicBool);

impl Calls {
    fn note(&self, line: String) -> io::Result<()> {
        self.0.lock().expect("calls").push(line);
        Ok(())
    }

    fn of(&self, call: &str) -> Vec<String> {
        let calls = self.0.lock().expect("calls");
        calls
            .iter()
            .filter(|line| line.starts_with(call))
            .cloned()
            .collect()
    }

    fn stuck(&self, on: bool) {
        self.1.store(on, Ordering::SeqCst);
    }
}

impl MountCalls for Calls {
    fn clone_tree(&self, source: BorrowedFd<'_>) -> io::Result<OwnedFd> {
        self.note("clone".to_owned())?;
        source.try_clone_to_owned()
    }
    fn set_attributes(&self, _: BorrowedFd<'_>, set: Attributes) -> io::Result<()> {
        self.note(format!("set ro={}", set.read_only))
    }
    fn attach(&self, _: BorrowedFd<'_>, point: &Path) -> io::Result<()> {
        self.note(format!("attach {}", point.display()))
    }
    fn mount_overlay(&self, point: &Path, _: bool, _: &str) -> io::Result<()> {
        self.note(format!("overlay {}", point.display()))
    }
    fn detach(&self, point: &Path) -> io::Result<()> {
        self.note(format!("detach {}", point.display()))?;
        match self.1.load(Ordering::SeqCst) {
            true => Err(io::Error::from_raw_os_error(libc::EBUSY)),
            false => Ok(()),
        }
    }
    fn is_mount_point(&self, _: &Path) -> io::Result<bool> {
        Ok(true)
    }
    fn bind_onto_itself(&self, dir: &Path) -> io::Result<()> {
        self.note(format!("bind {}", dir.display()))
    }
    fn make_private(&self, _: &Path) -> io::Result<()> {
        Ok(())
    }
}

/// The reads of host paths: the kernel's lookups, a search that passes, and a check whose
/// answer is `gone` (the kernel's when none), which waits for a word when `hold` is set and
/// panics when `panics` is.
#[derive(Debug, Default)]
struct Paths {
    gone: Option<i32>,
    hold: Option<Mutex<std::sync::mpsc::Receiver<()>>>,
    panics: bool,
}

impl PathCalls for Paths {
    fn open_beneath(&self, dir: BorrowedFd<'_>, rest: &Path) -> Result<OwnedFd, ResolveError> {
        Kernel.open_beneath(dir, rest)
    }
    fn search_as(&self, _: &Path, _: (u32, u32)) -> io::Result<()> {
        Ok(())
    }
    fn answers(&self, dir: &Path) -> io::Result<()> {
        assert!(!self.panics, "a check that panics");
        if let Some(go) = &self.hold {
            let _ = go.lock().expect("go").recv();
        }
        match self.gone {
            Some(errno) => Err(io::Error::from_raw_os_error(errno)),
            None => Kernel.answers(dir),
        }
    }
    /// Nothing is given away: the node's ids are not the test's to give.
    fn give_at(&self, _: BorrowedFd<'_>, _: &std::ffi::OsStr, _: (u32, u32)) -> io::Result<()> {
        Ok(())
    }
}

/// The settings of a runtime on `node` that gives layers: its leases beside the runs, its
/// mounts in `mounts`, the root `data`, and the registry of the node's image allowed.
fn settings(node: &Node, calls: &Arc<Calls>, paths: Paths) -> CrunSettings {
    let data = node.dir.join("data");
    std::fs::create_dir_all(data.join("layer")).expect("a layer");
    std::fs::create_dir_all(data.join("sdk/bin")).expect("a mount's source");
    let mut settings = node.settings("/root");
    settings.oci.leases_in = Some(node.dir.join("runs"));
    settings.fs = Some(FsSettings {
        dir: node.dir.join("mounts"),
        roots: MountRoots::new(&[("data".to_owned(), data)]).expect("roots"),
        calls: calls.clone(),
        paths: Arc::new(paths),
        image_allow: vec![js_oci::Allow::parse("registry.example/job").expect("an entry")],
    });
    settings
}

/// A runtime of `settings`.
fn open(settings: CrunSettings) -> CrunRuntime {
    CrunRuntime::open_with(settings, Arc::new(NoImages), Node::hooks(LONG, LONG)).expect("opens")
}

/// The layers and mounts of a start: the host path `data/layer` and the node's image as
/// layers, and `data/sdk` read-only at `/opt/sdk`.
fn job_fs(node: &Node) -> JobFs {
    let data = node.dir.join("data");
    JobFs {
        layers: vec![
            Layer::HostPath {
                path: data.join("layer").display().to_string(),
            },
            Layer::Image {
                image: node.image(),
            },
        ],
        mounts: vec![JobMount {
            source: MountSource::HostPath {
                path: data.join("sdk").display().to_string(),
            },
            target: "/opt/sdk".to_owned(),
            mode: MountMode::ReadOnly,
            sub_path: None,
            no_exec: false,
            propagation: Propagation::Private,
        }],
    }
}

/// Asks `what` until it is not pending: the lookups and the assembly run on the runtime's
/// threads, which the test only waits for.
fn settled(mut what: impl FnMut() -> FsPrepared) -> FsPrepared {
    for _ in 0..30_000 {
        match what() {
            FsPrepared::Pending => std::thread::sleep(std::time::Duration::from_millis(1)),
            other => return other,
        }
    }
    panic!("the runtime's threads did not answer");
}

/// Prepares the file system `fs` of `spec` as the lifecycle does: its host paths looked up,
/// its lease directory made beside its scratch directory, then its assembly.
fn prepare(runtime: &mut CrunRuntime, spec: &RunSpec, fs: &JobFs) -> FsPrepared {
    let Body::Container { image, .. } = &spec.body else {
        panic!("a container");
    };
    let image = image.clone();
    let run = spec.run_id.clone();
    assert_eq!(
        runtime.resolve_fs(&run, &image, fs, 5000),
        FsPrepared::Pending
    );
    let found = settled(|| runtime.resolve_fs(&run, &image, fs, 5000));
    assert_eq!(found, FsPrepared::Ready);
    let lease = runtime.lease_dir(&run).expect("beside the runs");
    std::fs::create_dir_all(&lease).expect("the lease directory");
    settled(|| runtime.assemble_fs(&run, 5000))
}

/// The pins of the lease `name` in the node's store.
fn pins(node: &Node, name: &str) -> Vec<String> {
    let leases = node.scratch.join("oci-image-pins/leases");
    let mut pins: Vec<String> = std::fs::read_dir(leases)
        .into_iter()
        .flatten()
        .flatten()
        .map(|entry| entry.file_name().to_string_lossy().into_owned())
        .filter(|pin| pin.starts_with(name))
        .collect();
    pins.sort();
    pins
}

/// The bundle's config of the container `id`.
fn bundle(node: &Node, id: &RunHandle) -> Value {
    let dir = node.dir.join("runs").join(format!(".lease-{}", id.0));
    let text = std::fs::read_to_string(dir.join("bundle/config.json")).expect("the bundle");
    serde_json::from_str(&text).expect("a config")
}

/// Proves a layered run made over what its assembly mounted: the runtime gives layers and
/// binds no named mount, the images are pinned for the lease (its image under its own name,
/// its image layer under `<lease>.l1`), the container's root is the assembled root, given
/// writable, its mount is the clone of its source; removing the run unmounts every point of
/// its list and removes its directory of mounts and every pin. Catches a container made over
/// the image's tree instead of its layers, a clone bound by the job's path, and a removal
/// that leaves a mount or a pin behind.
#[test]
fn a_layered_run_is_made_over_its_assembled_root_and_removed_with_its_mounts() {
    let node = Node::new("layered-run");
    let calls = Arc::new(Calls::default());
    let mut runtime = open(settings(&node, &calls, Paths::default()));
    assert!(runtime.layered_rootfs());
    assert!(!runtime.binds_named_mounts());
    let spec = node.spec("l1-exit");
    assert_eq!(
        prepare(&mut runtime, &spec, &job_fs(&node)),
        FsPrepared::Ready
    );
    let id = runtime.create_and_start(&spec).expect("started");
    let mounts = node.dir.join("mounts").join(&id.0);
    assert_eq!(runtime.mounts_of("l1-exit"), Some(mounts.clone()));
    assert_eq!(pins(&node, &id.0), [id.0.clone(), format!("{}.l1", id.0)]);
    let config = bundle(&node, &id);
    assert_eq!(
        config["root"]["path"],
        json!(mounts.join("rootfs").display().to_string())
    );
    assert_eq!(config["root"]["readonly"], json!(false));
    let bound = config["mounts"].as_array().expect("mounts");
    let sdk = bound
        .iter()
        .find(|mount| mount["destination"] == "/opt/sdk")
        .expect("bound");
    assert_eq!(
        sdk["source"],
        json!(mounts.join("src/1").display().to_string())
    );
    assert!(
        mounts.join("rootfs/opt/sdk").is_dir(),
        "its target, made in the root"
    );
    runtime.wait_ended(&id);
    runtime.remove(&id).expect("removed");
    // Deepest first, and of two as deep the later listed first.
    let detached: Vec<String> = ["src/1", "src/0", "rootfs"]
        .iter()
        .map(|point| format!("detach {}", mounts.join(point).display()))
        .collect();
    assert_eq!(calls.of("detach"), detached);
    assert!(!mounts.exists());
    assert!(pins(&node, &id.0).is_empty());
}

/// Proves a root of layers alone (no image) runs the job's command as given, with the
/// default `PATH` under the job's environment, and pins no image of its own. Catches a
/// layers-only start refused for its empty image, or run with an image's command.
#[test]
fn a_root_of_layers_alone_runs_its_command_with_the_default_path() {
    let node = Node::new("layered-alone");
    let calls = Arc::new(Calls::default());
    let mut runtime = open(settings(&node, &calls, Paths::default()));
    let mut spec = node.spec("l2-0");
    spec.body = Body::Container {
        image: String::new(),
        args: vec!["/bin/tool".to_owned(), "--go".to_owned()],
    };
    spec.env = vec![("A".to_owned(), "2".to_owned())];
    let fs = JobFs {
        mounts: Vec::new(),
        layers: vec![job_fs(&node).layers.remove(0)],
    };
    assert_eq!(prepare(&mut runtime, &spec, &fs), FsPrepared::Ready);
    let id = runtime.create_and_start(&spec).expect("started");
    let config = bundle(&node, &id);
    assert_eq!(
        config["process"]["args"],
        json!(["/dev/js-init", "--", "/bin/tool", "--go"])
    );
    // The default `PATH`, then the job's own; the runtime's own variables follow.
    let env = config["process"]["env"].as_array().expect("env");
    assert_eq!(
        env[..2],
        [
            json!(format!("PATH={}", js_agent::image::DEFAULT_PATH)),
            json!("A=2")
        ]
    );
    assert!(pins(&node, &id.0).is_empty());
    runtime.wait_ended(&id);
    runtime.remove(&id).expect("removed");
}

/// Proves a run whose job did not succeed is held as running until each of its sources has
/// answered the check after it ended, and that one that no longer answers (`ENOTCONN`) makes
/// its result's input failure `input-unreadable`, for a run made by this process and for one
/// adopted from an earlier one (its sources counted from its list of mounts); and that a run
/// that succeeded is not checked. Catches a lost provider's file system reported as the job's
/// own failure, and a successful job blamed on its inputs.
#[test]
fn a_failed_runs_sources_are_checked_before_its_end_is_read() {
    let node = Node::new("layered-check");
    let calls = Arc::new(Calls::default());
    let gone = Paths {
        gone: Some(libc::ENOTCONN),
        ..Paths::default()
    };
    let mut runtime = open(settings(&node, &calls, gone));
    let mut ids = Vec::new();
    for run in ["l3-exit", "l4-0"] {
        let spec = node.job(run, 60_000);
        assert_eq!(
            prepare(&mut runtime, &spec, &job_fs(&node)),
            FsPrepared::Ready
        );
        ids.push(runtime.create_and_start(&spec).expect("started"));
        runtime.wait_ended(&ids[ids.len() - 1]);
    }
    let ended = |runtime: &mut CrunRuntime, id: &RunHandle| {
        for _ in 0..30_000 {
            match runtime.poll(id).expect("polled") {
                Status::Running => std::thread::sleep(std::time::Duration::from_millis(1)),
                other => return other,
            }
        }
        panic!("the check did not answer");
    };
    assert!(matches!(ended(&mut runtime, &ids[0]), Status::Exited(exit) if exit.code == 3));
    assert_eq!(
        runtime.input_failure(&ids[0]).as_deref(),
        Some(refusal::INPUT_UNREADABLE)
    );
    assert!(matches!(ended(&mut runtime, &ids[1]), Status::Exited(exit) if exit.code == 0));
    assert_eq!(runtime.input_failure(&ids[1]), None);
    // A run made and ended while no agent ran is adopted with its sources.
    let spec = node.job("l5-exit", 60_000);
    assert_eq!(
        prepare(&mut runtime, &spec, &job_fs(&node)),
        FsPrepared::Ready
    );
    let id = runtime.create_and_start(&spec).expect("started");
    runtime.wait_ended(&id);
    drop(runtime);
    let gone = Paths {
        gone: Some(libc::ENOTCONN),
        ..Paths::default()
    };
    let mut adopted = open(settings(&node, &calls, gone));
    assert!(matches!(ended(&mut adopted, &id), Status::Exited(exit) if exit.code == 3));
    assert_eq!(
        adopted.input_failure(&id).as_deref(),
        Some(refusal::INPUT_UNREADABLE)
    );
    adopted.remove(&id).expect("removed");
    assert!(!node.dir.join("mounts").join(&id.0).exists());
}

/// Proves a run whose file system was assembled and that is never made (stopped while it
/// waited) is discarded: its mounts unmounted, its directory of mounts and its pins removed;
/// and that what an earlier process assembled for a run it never made is removed when the
/// runtime next opens, and reported, while another agent's directory is left alone. Catches
/// a mount or a pin that outlives a run that never ran.
#[test]
fn an_assembly_whose_run_is_never_made_is_discarded_or_swept() {
    let node = Node::new("layered-discard");
    let calls = Arc::new(Calls::default());
    let mut runtime = open(settings(&node, &calls, Paths::default()));
    let spec = node.spec("l6-0");
    assert_eq!(
        prepare(&mut runtime, &spec, &job_fs(&node)),
        FsPrepared::Ready
    );
    let lease = runtime.mounts_of("l6-0").expect("its mounts");
    let leased = lease
        .file_name()
        .expect("a name")
        .to_string_lossy()
        .into_owned();
    assert_eq!(pins(&node, &leased).len(), 2);
    runtime.discard_fs("l6-0").expect("discarded");
    assert!(!lease.exists());
    assert!(pins(&node, &leased).is_empty());
    assert_eq!(calls.of("detach").len(), 3);
    // An assembly a process that ended left, and another agent's directory.
    let spec = node.spec("l7-0");
    assert_eq!(
        prepare(&mut runtime, &spec, &job_fs(&node)),
        FsPrepared::Ready
    );
    let left = runtime.mounts_of("l7-0").expect("its mounts");
    // Its lease directory is gone, as an agent of an earlier release's start removes it;
    // only its directory of mounts tells of it.
    let lease = runtime.lease_dir("l7-0").expect("beside the runs");
    std::fs::remove_dir_all(lease).expect("removed");
    let foreign = node.dir.join("mounts/js-0000000000000000-other");
    std::fs::create_dir_all(&foreign).expect("another agent's");
    drop(runtime);
    let reopened = open(settings(&node, &calls, Paths::default()));
    assert!(!left.exists());
    assert!(foreign.is_dir());
    let removed = &reopened.swept().removed;
    assert!(
        removed.iter().any(|lease| lease.run_id == "l7-0"),
        "{removed:?}"
    );
}

/// Proves a runtime opened with no file system settings (a kernel without the system calls)
/// gives no layers, refuses every call of a job's file system, discards and checks nothing,
/// and mounts nothing; and that a layer's image outside the allowed registries, or not a
/// pinned reference, is refused at admission, and one evicted from the store that cannot be
/// fetched again fails the assembly; a run id no lease is named by has nothing to discard. Catches a job given half a root where the kernel cannot make one, and an image
/// pulled from a registry the operator did not allow.
#[test]
fn a_runtime_refuses_what_it_cannot_give() {
    let node = Node::new("layered-without");
    let mut plain = node.open();
    assert!(!plain.layered_rootfs());
    let fs = job_fs(&node);
    let refused = |answer: FsPrepared| matches!(answer, FsPrepared::Failed(_));
    assert!(refused(plain.resolve_fs("r1", "", &fs, 5000)));
    assert!(!plain.ends_held(&RunHandle("r1".to_owned())));
    assert!(refused(plain.assemble_fs("r1", 5000)));
    assert_eq!(plain.discard_fs("r1"), Ok(()));
    assert_eq!(plain.mounts_of("r1"), None);
    assert!(plain.check_layer_image(&node.image()).is_err());
    let node = Node::new("layered-images");
    let calls = Arc::new(Calls::default());
    let mut runtime = open(settings(&node, &calls, Paths::default()));
    runtime.check_layer_image(&node.image()).expect("allowed");
    let elsewhere = format!("other.example/job@{}", node.digest);
    assert_eq!(
        runtime.check_layer_image(&elsewhere),
        Err(format!(
            "the image {elsewhere} is on no --image-allow or --image-allow-loopback entry"
        ))
    );
    assert!(
        runtime.check_layer_image("registry.example/job:1").is_err(),
        "not pinned"
    );
    assert!(
        runtime
            .check_layer_image(&format!("Bad Name@{}", node.digest))
            .is_err()
    );
    assert!(
        refused(runtime.resolve_fs("../r", "", &fs, 5000)),
        "not a run id"
    );
    assert_eq!(runtime.discard_fs("../r"), Ok(()), "no lease is named so");
    let evicted = format!("registry.example/job@sha256:{}", "cd".repeat(32));
    let mut fs = job_fs(&node);
    fs.layers[1] = Layer::Image { image: evicted };
    let spec = node.spec("l8-0");
    let answer = prepare(&mut runtime, &spec, &fs);
    // Pinned, and fetched again from a source that has nothing: the assembly fails.
    assert!(refused(answer), "evicted and not fetched");
    let mut bad = job_fs(&node);
    bad.layers[1] = Layer::Image {
        image: "registry.example/job:1".to_owned(),
    };
    let answer = prepare(&mut runtime, &node.spec("l9-0"), &bad);
    assert!(refused(answer));
}

/// Proves a layered run whose image has no kept config is refused, not fetched again (its
/// tree is a layer of the mounted root), and that the failed make takes its mounts apart
/// with the rest of its lease. Catches an image removed under a mounted overlay.
#[test]
fn a_layered_run_whose_image_has_no_config_is_taken_apart_with_its_mounts() {
    let node = Node::new("layered-noconfig");
    let calls = Arc::new(Calls::default());
    let mut runtime = open(settings(&node, &calls, Paths::default()));
    let hex = &node.digest[7..];
    std::fs::remove_file(js_agent::crun::config_dir(&node.scratch).join(format!("{hex}.json")))
        .expect("the config");
    let spec = node.spec("l10-0");
    assert_eq!(
        prepare(&mut runtime, &spec, &job_fs(&node)),
        FsPrepared::Ready
    );
    let mounts = runtime.mounts_of("l10-0").expect("its mounts");
    let why = runtime.create_and_start(&spec).expect_err("no config");
    assert!(
        why.to_string()
            .contains("its tree is a layer of the run's root"),
        "{why}"
    );
    assert!(!mounts.exists());
    assert_eq!(runtime.list_labelled().expect("listed"), Vec::new());
}

/// Polls the run `id` once.
fn status(runtime: &mut CrunRuntime, id: &RunHandle) -> Status {
    runtime.poll(id).expect("polled")
}

/// Proves the check after a failed job holds the run as running while a source has not
/// answered (here, until the test lets it), and that a check whose thread ends without an
/// answer counts as a source that did not answer; and that a run without sources, or one this
/// runtime does not hold, has no input failure. Catches a result reported before its check,
/// and a lost check taken for sources that answered.
#[test]
fn a_check_held_or_lost_is_waited_for_or_counted_as_unanswered() {
    let node = Node::new("layered-held");
    let calls = Arc::new(Calls::default());
    let (go, held) = std::sync::mpsc::channel();
    let paths = Paths {
        hold: Some(Mutex::new(held)),
        ..Paths::default()
    };
    let mut runtime = open(settings(&node, &calls, paths));
    let spec = node.spec("l12-exit");
    assert_eq!(
        prepare(&mut runtime, &spec, &job_fs(&node)),
        FsPrepared::Ready
    );
    let id = runtime.create_and_start(&spec).expect("started");
    runtime.wait_ended(&id);
    assert_eq!(
        status(&mut runtime, &id),
        Status::Running,
        "the check is asked"
    );
    assert_eq!(status(&mut runtime, &id), Status::Running, "and waited for");
    assert!(runtime.ends_held(&id), "its end is held");
    // Each source waits for a word: let them all go.
    drop(go);
    let ended = settled_status(&mut runtime, &id);
    assert!(matches!(ended, Status::Exited(exit) if exit.code == 3));
    assert_eq!(runtime.input_failure(&id), None, "every source answered");
    let plain = node.spec("l13-exit");
    let lease = runtime.lease_dir("l13-exit").expect("beside the runs");
    std::fs::create_dir_all(lease).expect("its lease directory, as the lifecycle makes it");
    let bare = runtime
        .create_and_start(&plain)
        .expect("started without sources");
    runtime.wait_ended(&bare);
    assert!(matches!(status(&mut runtime, &bare), Status::Exited(_)));
    assert_eq!(runtime.input_failure(&bare), None);
    assert_eq!(runtime.input_failure(&RunHandle("none".to_owned())), None);
    assert_eq!(runtime.mounts_of("../x"), None);

    let node = Node::new("layered-lost");
    let paths = Paths {
        panics: true,
        ..Paths::default()
    };
    let mut runtime = open(settings(&node, &calls, paths));
    let spec = node.spec("l14-exit");
    assert_eq!(
        prepare(&mut runtime, &spec, &job_fs(&node)),
        FsPrepared::Ready
    );
    let id = runtime.create_and_start(&spec).expect("started");
    runtime.wait_ended(&id);
    assert!(matches!(
        settled_status(&mut runtime, &id),
        Status::Exited(_)
    ));
    assert_eq!(
        runtime.input_failure(&id).as_deref(),
        Some(refusal::INPUT_UNREADABLE)
    );
}

/// Polls the run `id` until it is not running.
fn settled_status(runtime: &mut CrunRuntime, id: &RunHandle) -> Status {
    for _ in 0..30_000 {
        match status(runtime, id) {
            Status::Running => std::thread::sleep(std::time::Duration::from_millis(1)),
            other => return other,
        }
    }
    panic!("the check did not answer");
}

/// Proves a mount that cannot be removed keeps its run (and its lease and pins) until a later
/// removal can, and keeps an assembly an earlier process left until a later start can, which
/// says so; that an image pin the store will not write (its directory open to others) fails
/// the assembly; and that a directory of mounts that cannot be made stops the runtime from
/// opening. Catches a lease directory removed under a mounted root, and a pin lost.
#[test]
fn what_cannot_be_unmounted_or_pinned_is_kept_and_said() {
    use std::os::unix::fs::PermissionsExt as _;
    let node = Node::new("layered-stuck");
    let calls = Arc::new(Calls::default());
    let mut runtime = open(settings(&node, &calls, Paths::default()));
    let spec = node.spec("l15-exit");
    assert_eq!(
        prepare(&mut runtime, &spec, &job_fs(&node)),
        FsPrepared::Ready
    );
    let id = runtime.create_and_start(&spec).expect("started");
    runtime.wait_ended(&id);
    calls.stuck(true);
    let why = runtime
        .remove(&id)
        .expect_err("an unmount fails")
        .to_string();
    assert!(why.starts_with("unmount the run's file system: "), "{why}");
    assert_eq!(runtime.list_labelled().expect("listed").len(), 1);
    calls.stuck(false);
    runtime.remove(&id).expect("removed");
    // An assembly left by a process that ended, kept while it cannot be unmounted.
    let spec = node.spec("l16-0");
    assert_eq!(
        prepare(&mut runtime, &spec, &job_fs(&node)),
        FsPrepared::Ready
    );
    let left = runtime.mounts_of("l16-0").expect("its mounts");
    drop(runtime);
    calls.stuck(true);
    let kept = open(settings(&node, &calls, Paths::default()));
    assert!(left.exists());
    assert!(
        kept.swept()
            .failed
            .iter()
            .any(|lease| lease.run_id == "l16-0")
    );
    drop(kept);
    calls.stuck(false);
    let mut runtime = open(settings(&node, &calls, Paths::default()));
    assert!(!left.exists());
    // A pin the store will not write.
    let pins = node.scratch.join("oci-image-pins");
    std::fs::set_permissions(&pins, std::fs::Permissions::from_mode(0o755)).expect("open");
    let answer = prepare(&mut runtime, &node.spec("l17-0"), &job_fs(&node));
    std::fs::set_permissions(&pins, std::fs::Permissions::from_mode(0o700)).expect("closed");
    assert!(matches!(answer, FsPrepared::Failed(_)), "{answer:?}");
    // A directory of mounts below a file.
    std::fs::write(node.dir.join("file"), b"x").expect("a file");
    let mut bad = settings(&node, &calls, Paths::default());
    if let Some(fs) = bad.fs.as_mut() {
        fs.dir = node.dir.join("file/mounts");
    }
    let refused = CrunRuntime::open_with(bad, Arc::new(NoImages), Node::hooks(LONG, LONG));
    let why = refused.expect_err("no directory of mounts").to_string();
    assert!(
        why.starts_with("the directory of the runs' mounts "),
        "{why}"
    );
    assert!(format!("{:?}", js_agent::crun::MountSeam::default()).contains("/run/js-agent/mounts"));
}
