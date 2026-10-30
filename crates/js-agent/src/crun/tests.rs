//! The pure parts of the crun runtime: image pins, the container a run becomes, the exit
//! a container's end becomes, and the names of the paths it keeps.

use std::collections::BTreeMap;
use std::path::PathBuf;

use js_contain::cgroup::OomOutcome;
use js_contain::container::{ContainerLimits, Ended, HostMount};
use js_contain::oci::LeaseDirs;
use js_contain::oci::image::PinnedImage;

use super::spec::{EXEC_ROOT, NOT_RUN, container_spec, exit_of, pinned};
use super::{config_dir, system_now_ms};
use crate::cli_runtime::DEFAULT_PIDS;
use crate::lifecycle::{Body, Exit, ResolvedMount, RunSpec};

fn digest() -> String {
    format!("sha256:{}", "ab".repeat(32))
}

/// Catches an image reference that reaches the store in a form it cannot fetch (a tag, or a
/// port taken for one) and one that is not pinned (no digest, the wrong algorithm, a short
/// or upper-case digest): the store is asked for `<repository>@<digest>` and nothing
/// else.
#[test]
fn an_image_is_pinned_by_its_digest_and_the_tag_is_dropped() {
    let d = digest();
    for (given, repository) in [
        ("registry.example/job@", "registry.example/job"),
        ("registry.example/job:1@", "registry.example/job"),
        ("registry.example:5000/job@", "registry.example:5000/job"),
        ("registry.example:5000/job:v2@", "registry.example:5000/job"),
        ("busybox@", "busybox"),
    ] {
        let image = pinned(&format!("{given}{d}")).expect("pinned");
        assert_eq!(image.reference, format!("{repository}@{d}"), "{given}");
        assert_eq!(image.digest, d);
    }
    let upper = format!("sha256:{}", "AB".repeat(32));
    let short = format!("sha256:{}", "ab".repeat(31));
    let other = format!("sha512:{}", "ab".repeat(32));
    for bad in [
        "registry.example/job:1".to_owned(),
        format!("registry.example/job@{upper}"),
        format!("registry.example/job@{short}"),
        format!("registry.example/job@{other}"),
        "registry.example/job@".to_owned(),
    ] {
        let why = pinned(&bad).expect_err(&bad);
        assert!(why.contains("not pinned"), "{bad}: {why}");
    }
}

fn lease() -> LeaseDirs {
    LeaseDirs {
        dir: PathBuf::from("/s/js-x-r1"),
        root: PathBuf::from("/s/js-x-r1/root"),
        upper: PathBuf::from("/s/js-x-r1/upper"),
        work: PathBuf::from("/s/js-x-r1/work"),
        home: PathBuf::from("/s/js-x-r1/rw/home"),
    }
}

fn run(pids: Option<u64>, files: Option<PathBuf>) -> RunSpec {
    RunSpec {
        run_id: "r1".to_owned(),
        body: Body::Container {
            image: "registry.example/job:1".to_owned(),
            args: vec!["--go".to_owned()],
        },
        cpu_millis: 1500,
        memory_bytes: 1 << 30,
        pids,
        gpu_indices: Vec::new(),
        mounts: vec![
            ResolvedMount {
                host: PathBuf::from("/data/in"),
                target: "/mnt/data/in".to_owned(),
                ro: true,
            },
            ResolvedMount {
                host: PathBuf::from("/data/out"),
                target: "/mnt/data/out".to_owned(),
                ro: false,
            },
        ],
        scratch: PathBuf::from("/scratch/r1"),
        files,
        slot: 3,
        isolation: js_agent_wire::Isolation::Contained,
        labels: BTreeMap::new(),
    }
}

/// Catches a run that becomes a different container than it asked for: the lease's name,
/// image, directories and cgroup; the run's scratch read-write at `/scratch` first, then
/// its mounts with the access they asked, then its files read-only; the job starting at the
/// exec root; no shared trees; the process limit the run named, or the default for one that
/// named none; no program or environment (the image config gives them).
#[test]
fn a_run_becomes_the_container_it_asked_for() {
    let image = PinnedImage {
        reference: format!("registry.example/job@{}", digest()),
        digest: digest(),
    };
    let spec = container_spec(
        "js-x-r1",
        &run(Some(77), Some(PathBuf::from("/run/files/r1"))),
        &image,
        &lease(),
        "/js/actions/js-x-r1",
    );
    assert_eq!(spec.name, "js-x-r1");
    assert_eq!(spec.image, image.reference);
    assert_eq!(spec.image_digest, digest());
    assert_eq!(spec.dir, PathBuf::from("/s/js-x-r1"));
    assert_eq!(spec.cgroup_parent, "/js/actions/js-x-r1");
    assert_eq!(spec.exec_root, EXEC_ROOT);
    assert_eq!(spec.input_root, PathBuf::from("/s/js-x-r1/root"));
    assert_eq!(spec.upper, PathBuf::from("/s/js-x-r1/upper"));
    assert_eq!(spec.overlay_work, PathBuf::from("/s/js-x-r1/work"));
    assert_eq!(spec.working_directory, "");
    assert!(spec.argv.is_empty() && spec.env.is_empty() && spec.shared.is_empty());
    assert_eq!(
        spec.limits,
        ContainerLimits {
            pids: 77,
            ..ContainerLimits::DEFAULT
        }
    );
    let bind = |target: &str, source: &str, read_only: bool| HostMount {
        target: target.to_owned(),
        source: PathBuf::from(source),
        read_only,
    };
    assert_eq!(
        spec.mounts,
        [
            bind("/scratch", "/scratch/r1", false),
            bind("/mnt/data/in", "/data/in", true),
            bind("/mnt/data/out", "/data/out", false),
            bind("/run/js-files", "/run/files/r1", true),
        ]
    );
    let plain = container_spec("js-x-r1", &run(None, None), &image, &lease(), "/c");
    assert_eq!(plain.limits.pids, DEFAULT_PIDS);
    assert_eq!(plain.mounts.len(), 3);
}

/// Catches a container's end that is read wrongly: an exit code kept as it is; a container
/// that did not run (crun's or the init's words, or crun not run at all) as the code 126;
/// a kill at the lease's own limit as 137 with the flag, whatever code crun gave; and a
/// kill the lease's limit did not cause (a busy node) without the flag.
#[test]
fn a_containers_end_is_an_exit_flagged_only_for_its_own_limit() {
    let plain = |code| Exit {
        code,
        oom_killed: false,
    };
    assert_eq!(exit_of(Ok(Ended::Exited(0)), None), plain(0));
    assert_eq!(exit_of(Ok(Ended::Exited(3)), None), plain(3));
    assert_eq!(
        exit_of(Ok(Ended::NotRun("exited 1".to_owned())), None),
        plain(NOT_RUN)
    );
    assert_eq!(exit_of(Err("no crun".to_owned()), None), plain(NOT_RUN));
    let own = OomOutcome::OwnLimit { used: 2, limit: 1 };
    assert_eq!(
        exit_of(Ok(Ended::Exited(1)), Some(&own)),
        Exit {
            code: 137,
            oom_killed: true
        }
    );
    let busy = OomOutcome::BusyNode("short".to_owned());
    assert_eq!(exit_of(Ok(Ended::Exited(137)), Some(&busy)), plain(137));
}

/// Catches the config directory moving away from where the image source is told to keep
/// the configs (both are named from the store's directory), and a clock that reads zero.
#[test]
fn the_configs_are_kept_beside_the_store_and_the_clock_reads_the_epoch() {
    assert_eq!(
        config_dir(&PathBuf::from("/s")),
        PathBuf::from("/s/oci-configs")
    );
    assert!(system_now_ms() > 1_700_000_000_000);
}
