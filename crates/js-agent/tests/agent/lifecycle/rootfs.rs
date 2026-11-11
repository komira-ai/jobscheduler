//! A job's layers and mounts through the lifecycle (`docs/design/job-rootfs.md`, 4.7 and
//! 8): what the admission refuses from the start's text and the agent's roots, the preparing
//! stage that waits for the start's images, the lookups of its host paths and its assembly,
//! each off the worker (here: answers the fake runtime gives when the test says), the
//! cleanup that discards what was mounted before the run's directories go, and the result of
//! a job whose sources no longer answered. The runtime's own side is `crun/fs`'s.

use std::fs;

use js_agent::lifecycle::{Config, FsPrepared, Lifecycle, MountRoots, NativeRefusal, RuntimeError};
use js_agent_wire::{
    HeldState, JobMount, JobStartSpec, Layer, Mount, MountMode, MountSource, Outcome, Propagation,
    refusal,
};

use super::fake::{FakeRuntime, Log, ManualClock};
use super::per_job::{job_start, listing, start};
use super::{Harness, config_in, hook, job, names};
use crate::support::Scratch;

/// The image of an image layer.
const LAYER: &str = "registry.example/layer:1@sha256:abcdefabcdefabcdefabcdefabcdefabcdefabcdefabcdefabcdefabcdefabcd";

/// The image of another image layer.
const LAYER2: &str = "registry.example/layer:2@sha256:abcdefabcdefabcdefabcdefabcdefabcdefabcdefabcdefabcdefabcdef0000";

/// The image of the job starts of `per_job::start`.
const IMAGE: &str = "registry.example/job:1@sha256:0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";

/// A harness whose agent has the roots `data` (read-write) and `ro` (read-only), and whose
/// runtime gives layers, keeping its lease directories beside the runs'.
fn harness(name: &str) -> Harness {
    let dir = Scratch::new(&format!("life-{name}"));
    let mut config: Config = config_in(&dir, false);
    fs::create_dir_all(dir.path("ro")).unwrap();
    config.mounts = MountRoots::with_read_only(
        &[("data".to_owned(), dir.path("data"))],
        &[("ro".to_owned(), dir.path("ro"))],
    )
    .unwrap();
    let (clock, log) = (ManualClock::default(), Log::default());
    let fake = FakeRuntime::new(&log, &clock);
    fake.give_layers();
    fake.keep_lease_dirs_in(&dir.path("scratch"));
    let checks = hook(&log, &fake, &dir.path("scratch"), &[]);
    let (life, _) = Lifecycle::adopt(
        config.clone(),
        Box::new(fake.clone()),
        checks,
        Box::new(clock.clone()),
    )
    .unwrap();
    Harness {
        life,
        fake,
        clock,
        log,
        config,
        dir,
    }
}

/// A mount of the host path `path` at `target`.
fn mount(path: &str, target: &str, mode: MountMode) -> JobMount {
    JobMount {
        source: MountSource::HostPath {
            path: path.to_owned(),
        },
        target: target.to_owned(),
        mode,
        sub_path: None,
        no_exec: false,
        propagation: Propagation::Private,
    }
}

/// A job start of `job_id` with a host path layer of the root `data` and a mount of it, of a
/// tenth of a core, so that many fit the harness's four.
fn layered(h: &Harness, job_id: &str) -> JobStartSpec {
    let data = h.dir.path("data").display().to_string();
    let mut small = start(job_id);
    small.booked.cpu_millis = 100;
    JobStartSpec {
        layers: vec![Layer::HostPath {
            path: format!("{data}/layer"),
        }],
        mounts: vec![mount(
            &format!("{data}/sdk"),
            "/opt/sdk",
            MountMode::ReadOnly,
        )],
        ..small
    }
}

/// The refusal of `job_id` among `events`.
fn refused(events: &[js_agent::lifecycle::Event], job_id: &str) -> Vec<String> {
    let prefix = format!("refused {job_id} ");
    names(events)
        .into_iter()
        .filter(|name| name.starts_with(&prefix))
        .collect()
}

/// The lines the fake runtime logged that begin with `what`.
fn logged(h: &Harness, what: &str) -> Vec<String> {
    let log = h.log.borrow();
    log.iter()
        .filter(|line| line.starts_with(what))
        .cloned()
        .collect()
}

/// Asserts that a start with a layer, or a mount, through a runtime that gives none (the
/// container engine's, a Mac's) is refused `rootfs-unsupported-here` and creates nothing.
/// Catches a job run on its image alone, without the layers and mounts it named.
#[test]
fn layers_and_mounts_are_refused_by_a_runtime_that_gives_none() {
    let mut h = Harness::new("rootfs-none");
    let data = h.dir.path("data").display().to_string();
    let with_layer = JobStartSpec {
        layers: vec![Layer::HostPath { path: data.clone() }],
        ..start("j1")
    };
    let with_mount = JobStartSpec {
        mounts: vec![mount(&data, "/work/data", MountMode::ReadOnly)],
        ..start("j2")
    };
    let events = h.apply(vec![job_start(with_layer), job_start(with_mount)]);
    assert_eq!(
        refused(&events, "j1"),
        ["refused j1 rootfs-unsupported-here"]
    );
    assert_eq!(
        refused(&events, "j2"),
        ["refused j2 rootfs-unsupported-here"]
    );
    let result = listing(&h, "j1").result.expect("a result");
    assert_eq!(result.outcome, Outcome::FailedToStart);
    assert_eq!(
        result.detail.as_deref(),
        Some(refusal::ROOTFS_UNSUPPORTED_HERE)
    );
    assert!(h.fake.created().is_empty());
}

/// Asserts that the admission holds each host path to the agent's roots, from the start's
/// text and nothing on the host: a path beneath no root, and a read-write mount beneath a
/// read-only root, are refused `input-not-allowed`; a read-only or copy-on-write mount
/// beneath a read-only root, and a read-write one beneath a read-write root, are admitted;
/// an image layer the runtime may not pull is refused `image-not-allowed`. Catches a job
/// that writes through a root the operator gave read-only, and a path no root holds looked
/// up at all.
#[test]
fn a_host_path_must_lie_beneath_a_root_that_allows_its_mode() {
    let mut h = harness("rootfs-roots");
    let (data, ro) = (h.dir.path("data"), h.dir.path("ro"));
    let at = |root: &std::path::Path, rel: &str| format!("{}/{rel}", root.display());
    h.fake.refuse_layer_image(LAYER);
    let cases = [
        (
            "nowhere",
            JobStartSpec {
                layers: vec![Layer::HostPath {
                    path: "/elsewhere".to_owned(),
                }],
                ..start("nowhere")
            },
            Some(refusal::INPUT_NOT_ALLOWED),
        ),
        (
            "rw-ro",
            JobStartSpec {
                mounts: vec![mount(&at(&ro, "x"), "/x", MountMode::ReadWrite)],
                ..start("rw-ro")
            },
            Some(refusal::INPUT_NOT_ALLOWED),
        ),
        (
            "image",
            JobStartSpec {
                layers: vec![Layer::Image {
                    image: LAYER.to_owned(),
                }],
                ..start("image")
            },
            Some(refusal::IMAGE_NOT_ALLOWED),
        ),
        (
            "cow-ro",
            JobStartSpec {
                mounts: vec![
                    mount(&at(&ro, "x"), "/work", MountMode::CopyOnWrite),
                    mount(&at(&ro, "y"), "/y", MountMode::ReadOnly),
                    mount(&at(&data, "z"), "/z", MountMode::ReadWrite),
                ],
                ..start("cow-ro")
            },
            None,
        ),
    ];
    for (job_id, spec, code) in cases {
        let events = h.apply(vec![job_start(spec)]);
        let want: Vec<String> = code
            .map(|code| format!("refused {job_id} {code}"))
            .into_iter()
            .collect();
        assert_eq!(refused(&events, job_id), want, "{job_id}");
    }
    assert_eq!(listing(&h, "cow-ro").state, HeldState::Starting);
    assert!(h.fake.created().is_empty());
}

/// Asserts that a run start that mounts a named root (the legacy form, which the lifecycle
/// resolves by path) is refused `invalid-mount` by a runtime that binds none, and creates
/// nothing. Catches a host path bound by its text, between whose check and bind a job that
/// writes beneath the root can swap it for a link.
#[test]
fn a_named_mount_is_refused_by_a_runtime_that_binds_none() {
    let mut h = harness("rootfs-named");
    fs::create_dir_all(h.dir.path("data/x")).unwrap();
    let mut spec = job("r1");
    spec.mounts = vec![Mount {
        root_name: "data".to_owned(),
        rel: "x".to_owned(),
        ro: true,
    }];
    let events = h.start(spec);
    assert_eq!(names(&events), ["refused r1 invalid-mount"]);
    assert!(h.fake.created().is_empty());
}

/// Asserts that a start with layers and mounts waits as `starting`, booking its slot, until
/// its host paths are found, its images (its own and its layers') are here and its file
/// system is assembled, in that order and each off the worker: its directories are made
/// only once the first two are, and its container only once the assembly is; another job is
/// admitted, started and ended meanwhile. Catches a container made before its root, a
/// lookup or a pull that holds up the agent's other jobs, and an image of a layer not
/// waited for.
#[test]
fn a_start_waits_for_its_lookups_its_images_and_its_assembly() {
    let mut h = harness("rootfs-wait");
    let image = h.fake.pull_pending(IMAGE);
    let layer = h.fake.pull_pending(LAYER);
    h.fake.resolve_answers("j1", FsPrepared::Pending);
    let mut spec = layered(&h, "j1");
    spec.layers.push(Layer::Image {
        image: LAYER.to_owned(),
    });
    h.apply(vec![job_start(spec)]);
    h.life.step();
    assert_eq!(listing(&h, "j1").state, HeldState::Starting);
    assert_eq!(
        logged(&h, "resolve j1"),
        [format!("resolve j1 (2 layers, 1 mounts, image {IMAGE:?})")]
    );
    assert_eq!(h.fake.asks(IMAGE), [None, Some(image)]);
    assert_eq!(h.fake.asks(LAYER), [None, Some(layer)]);
    // Another job, of another image, runs and ends while this one waits.
    let other = JobStartSpec {
        image: LAYER2.to_owned(),
        ..start("j2")
    };
    h.apply(vec![job_start(other)]);
    assert_eq!(listing(&h, "j2").state, HeldState::Running);
    h.fake.exit("j2", 0);
    h.life.step();
    assert_eq!(listing(&h, "j2").state, HeldState::ExitedUnacked);
    // Its paths are found; its images are still being fetched.
    h.fake.resolve_answers("j1", FsPrepared::Ready);
    h.life.step();
    assert!(
        !h.scratch_of("j1").exists(),
        "no directory before the images"
    );
    h.fake.pull_done(IMAGE);
    h.life.step();
    assert!(!h.scratch_of("j1").exists());
    h.fake.pull_done(LAYER);
    h.fake.assembly_answers("j1", FsPrepared::Pending);
    h.life.step();
    assert!(h.scratch_of("j1").exists(), "made for the assembly");
    assert!(
        h.dir.path("scratch/.lease-fake-j1").is_dir(),
        "the lease's too"
    );
    assert!(
        logged(&h, "assemble j1").is_empty(),
        "asked at the next step"
    );
    h.life.step();
    assert_eq!(logged(&h, "assemble j1"), ["assemble j1"]);
    assert_eq!(listing(&h, "j1").state, HeldState::Starting);
    assert_eq!(h.fake.created().len(), 1, "j2 alone");
    h.fake.assembly_answers("j1", FsPrepared::Ready);
    assert_eq!(names(&h.life.step()), ["started j1"]);
    let created = h.fake.created();
    assert_eq!(created[1].run_id, "j1");
    assert_eq!(created[1].scratch, h.scratch_of("j1"));
}

/// Asserts that a preparation that fails ends the start `failed_to_start`, with the code the
/// runtime refused it with, or `runtime-failed` for a failed call: a lookup refused
/// `input-unavailable`, an assembly refused `invalid-mount`, a lookup and an assembly that
/// failed, an image of a layer that could not be had (at the start and while waiting); and
/// that directories that cannot be made are `scratch-failed`. Catches a job started over
/// half a file system, and a refusal reported as the host's fault.
#[test]
fn a_preparation_that_fails_ends_the_start_with_its_code() {
    let unavailable = || {
        FsPrepared::Refused(NativeRefusal {
            code: refusal::INPUT_UNAVAILABLE,
            why: "missing".to_owned(),
        })
    };
    let invalid = || {
        FsPrepared::Refused(NativeRefusal {
            code: refusal::INVALID_MOUNT,
            why: "not in the mount".to_owned(),
        })
    };
    let failed = || FsPrepared::Failed(RuntimeError::new("a mount failed"));
    let mut h = harness("rootfs-fail");
    h.fake.resolve_answers("lookup", unavailable());
    h.fake.assembly_answers("assembly", invalid());
    h.fake.resolve_answers("lookup-call", failed());
    h.fake.assembly_answers("assembly-call", failed());
    h.fake.pull_fails(LAYER, "no such image");
    let ids = ["lookup", "assembly", "lookup-call", "assembly-call"];
    for job_id in ids {
        h.apply(vec![job_start(layered(&h, job_id))]);
    }
    let mut early = layered(&h, "layer-at-once");
    early.layers.push(Layer::Image {
        image: LAYER.to_owned(),
    });
    h.apply(vec![job_start(early)]);
    // A root of layers alone, whose layer's image fails while the start waits for it.
    let mut late = layered(&h, "layer-later");
    late.image = String::new();
    late.command = vec!["/bin/tool".to_owned()];
    late.layers.push(Layer::Image {
        image: LAYER2.to_owned(),
    });
    h.fake.pull_pending(LAYER2);
    h.apply(vec![job_start(late)]);
    h.fake.pull_fails(LAYER2, "gone");
    fs::write(h.scratch_of("dirs"), "in the way").unwrap();
    h.apply(vec![job_start(layered(&h, "dirs"))]);
    for _ in 0..4 {
        h.life.step();
    }
    let detail = |job_id: &str| {
        let result = listing(&h, job_id).result.expect("a result");
        assert_eq!(result.outcome, Outcome::FailedToStart, "{job_id}");
        result.detail
    };
    assert_eq!(
        detail("lookup").as_deref(),
        Some(refusal::INPUT_UNAVAILABLE)
    );
    assert_eq!(detail("assembly").as_deref(), Some(refusal::INVALID_MOUNT));
    for job_id in [
        "lookup-call",
        "assembly-call",
        "layer-at-once",
        "layer-later",
    ] {
        assert_eq!(
            detail(job_id).as_deref(),
            Some("runtime-failed"),
            "{job_id}"
        );
    }
    assert_eq!(detail("dirs").as_deref(), Some("scratch-failed"));
    assert!(h.fake.created().is_empty());
}

/// Asserts that a stop while the start waits for its lookups ends it at once, and one while
/// its assembly is under way waits for the assembly's answer before it ends it (what the
/// assembly mounts is removed only once it has stopped mounting); that a cancel ends it
/// with no result; and that the cleanup of each discards what the runtime prepared before
/// the run's directories go. Catches a mount made after its run's cleanup, and a cleanup
/// that removes the writable layers under a mounted root.
#[test]
fn a_stop_while_preparing_waits_for_the_assembly_and_the_cleanup_discards_it() {
    let mut h = harness("rootfs-stop");
    h.fake.resolve_answers("looking", FsPrepared::Pending);
    h.fake.assembly_answers("assembling", FsPrepared::Pending);
    for job_id in ["looking", "assembling"] {
        h.apply(vec![job_start(layered(&h, job_id))]);
    }
    h.life.step();
    h.life.step();
    assert!(
        h.scratch_of("assembling").exists(),
        "its assembly was asked for"
    );
    h.apply(vec![
        js_agent_wire::Command::JobStop {
            job_id: "looking".to_owned(),
        },
        js_agent_wire::Command::JobStop {
            job_id: "assembling".to_owned(),
        },
    ]);
    h.life.step();
    let looking = listing(&h, "looking")
        .result
        .expect("ended at the next step");
    assert_eq!(looking.outcome, Outcome::Killed);
    assert_eq!(listing(&h, "assembling").state, HeldState::Starting);
    h.life.step();
    assert_eq!(
        listing(&h, "assembling").state,
        HeldState::Starting,
        "waits"
    );
    h.fake.assembly_answers("assembling", FsPrepared::Ready);
    h.life.step();
    let assembling = listing(&h, "assembling").result.expect("ended");
    assert_eq!(assembling.outcome, Outcome::Killed);
    assert!(!h.scratch_of("assembling").exists());
    let log = h.log.borrow().clone();
    let discard = log.iter().position(|line| line == "discard assembling");
    let check = log
        .iter()
        .position(|line| line.starts_with("check assembling"));
    assert!(discard < check, "{log:?}");
    assert!(log.contains(&"discard looking".to_owned()));
    assert!(h.fake.created().is_empty());
}

/// Asserts that a discard that fails is a failure of the run's cleanup: the host is tainted
/// and the slot stays booked, and the next step tries again and goes on. Catches a mount left
/// behind on a host that takes the next job.
#[test]
fn a_discard_that_fails_taints_the_host() {
    let mut h = harness("rootfs-discard");
    h.fake.resolve_answers(
        "j1",
        FsPrepared::Refused(NativeRefusal {
            code: refusal::INPUT_UNAVAILABLE,
            why: "missing".to_owned(),
        }),
    );
    h.fake.fail("discard", 1);
    h.apply(vec![job_start(layered(&h, "j1"))]);
    let events = names(&h.life.step());
    assert!(
        events.contains(&"finding j1 scratch-failed".to_owned()),
        "{events:?}"
    );
    assert!(
        events.iter().any(|event| event.starts_with("tainted j1")),
        "{events:?}"
    );
    assert!(names(&h.life.step()).contains(&"freed j1".to_owned()));
}

/// Asserts that a job that did not succeed and whose sources no longer answered gets
/// `input-unreadable` in its result, its exit code kept, and that a job that succeeded gets
/// none whatever its sources do. Catches a provider's lost file system reported as the
/// user's failure, and a successful job blamed on its inputs.
#[test]
fn a_failed_job_whose_sources_are_gone_reports_them_unreadable() {
    let mut h = harness("rootfs-unreadable");
    for job_id in ["failed", "succeeded"] {
        h.fake.sources_gone(job_id);
        h.apply(vec![job_start(layered(&h, job_id))]);
    }
    h.life.step();
    h.life.step();
    h.life.step();
    h.fake.exit("failed", 2);
    h.fake.exit("succeeded", 0);
    h.life.step();
    let failed = listing(&h, "failed").result.expect("a result");
    assert_eq!(failed.outcome, Outcome::Failed);
    assert_eq!(failed.exit_code, Some(2));
    assert_eq!(
        failed.input_failure.as_deref(),
        Some(refusal::INPUT_UNREADABLE)
    );
    let succeeded = listing(&h, "succeeded").result.expect("a result");
    assert_eq!(succeeded.outcome, Outcome::Succeeded);
    assert_eq!(succeeded.input_failure, None);
}

/// Asserts that the post-job checks of a job are told where the runtime mounts the file
/// systems of the other jobs the agent holds (one that still waits for its assembly among
/// them), and not of the finished job itself or of one whose cleanup has passed. Catches the
/// mounts of a job that has no container yet counted against its neighbour.
#[test]
fn the_checks_know_the_mounts_of_the_other_held_jobs() {
    let mut h = harness("rootfs-held");
    h.fake.mount_in(&h.dir.path("mounts"));
    h.apply(vec![job_start(start("first"))]);
    h.fake.exit("first", 0);
    h.life.step();
    assert_eq!(listing(&h, "first").state, HeldState::ExitedUnacked);
    h.fake.resolve_answers("waits", FsPrepared::Pending);
    h.apply(vec![job_start(layered(&h, "waits"))]);
    h.apply(vec![job_start(start("second"))]);
    h.fake.exit("second", 0);
    h.life.step();
    let mounts = h.dir.path("mounts");
    assert_eq!(
        logged(&h, "mounts held beside second"),
        [format!(
            "mounts held beside second: {}",
            mounts.join("waits").display()
        )]
    );
}
