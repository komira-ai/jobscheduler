//! Hostile input to a start: every rule has a test, and each says the defect it catches.

mod common;

use common::{broken, expect, job, long, map};
use js_agent_wire::limits::*;
use js_agent_wire::refusal;
use js_agent_wire::{Command, JobSpec, Mount, Rule, WireShape};

type Edit = fn(&mut JobSpec);

fn check(edit: impl FnOnce(&mut JobSpec)) -> js_agent_wire::ValidationError {
    broken(&job(), edit, JobSpec::validate)
}

/// A label under the scheduler's prefix on a start is refused with its own code, for
/// every spelling of the prefix a map can hold, through a command too. It catches a
/// caller forging a tenant, run or instance label that the scheduler writes itself.
#[test]
fn reserved_label_on_a_start_is_refused() {
    for key in [
        "jobscheduler.dev/tenant",
        "jobscheduler.dev/anything",
        "jobscheduler.dev/",
    ] {
        let err = check(|j| j.labels = map(&[(key, "x")]));
        expect(err, "labels", Rule::Reserved, refusal::RESERVED_LABEL);
    }
    let mut spec = job();
    spec.labels = map(&[("jobscheduler.dev/tenant", "x")]);
    let err = Command::Start(Box::new(spec)).validate().unwrap_err();
    assert_eq!(err.code(), refusal::RESERVED_LABEL);
    let upper = check(|j| j.labels = map(&[("JobScheduler.dev/tenant", "x")]));
    expect(upper, "labels", Rule::BadChars, refusal::INVALID_JOB);
}

/// Label count, key and value bounds: more than the cap, an empty, long, upper-case,
/// spaced or NUL key, and a long or out-of-alphabet value are refused; the caps
/// themselves pass. It catches unbounded label maps and keys that smuggle control bytes.
#[test]
fn labels_are_bounded() {
    let many = (0..=MAX_LABELS)
        .map(|i| (format!("k{i}"), "v".to_string()))
        .collect();
    expect(
        check(|j| j.labels = many),
        "labels",
        Rule::TooMany,
        refusal::INVALID_JOB,
    );
    let at_cap = (0..MAX_LABELS)
        .map(|i| (format!("k{i}"), "v".to_string()))
        .collect();
    let mut spec = job();
    spec.labels = at_cap;
    assert_eq!(spec.validate(), Ok(()));
    for key in ["", "A", "a b", "a\0b", "-a", "a=b"] {
        let err = check(|j| j.labels = map(&[(key, "v")]));
        assert_eq!(err.field(), "labels", "{key:?}");
        assert_eq!(err.code(), refusal::INVALID_JOB, "{key:?}");
    }
    let key = long(MAX_LABEL_KEY_LEN + 1);
    let err = check(|j| j.labels = map(&[(key.as_str(), "v")]));
    expect(err, "labels", Rule::TooLong, refusal::INVALID_JOB);
    let key = format!(
        "{}.{}/{}",
        long(MAX_DNS_LABEL_LEN),
        long(2),
        long(MAX_LABEL_KEY_LEN - MAX_DNS_LABEL_LEN - 4)
    );
    let mut spec = job();
    spec.labels = map(&[(key.as_str(), &long(MAX_LABEL_VALUE_LEN))]);
    assert_eq!(spec.validate(), Ok(()));
    for value in [
        long(MAX_LABEL_VALUE_LEN + 1),
        String::new(),
        "V".into(),
        "a b".into(),
        "a\0".into(),
    ] {
        let err = check(|j| j.labels = map(&[("k", value.as_str())]));
        expect(err, "labels", Rule::BadChars, refusal::INVALID_JOB);
    }
}

/// A run id or lease id that is empty, too long, has a path, a NUL, a space, a leading
/// dot or dash, or a non-ASCII letter is refused; the cap length passes. It catches an id
/// like `../../etc/cron.d/x` reaching a scratch path or a container name.
#[test]
fn run_and_lease_ids_are_bounded_and_path_free() {
    let bad = [
        "",
        "../../etc/cron.d/x",
        "a/b",
        "a\0",
        "a b",
        ".hidden",
        "-x",
        "_x",
        "é",
        "a\n",
    ];
    for id in bad {
        let err = check(|j| j.run_id = id.into());
        assert_eq!(
            (err.field(), err.code()),
            ("run_id", refusal::INVALID_JOB),
            "{id:?}"
        );
        let err = check(|j| j.lease_id = id.into());
        assert_eq!(
            (err.field(), err.code()),
            ("lease_id", refusal::INVALID_JOB),
            "{id:?}"
        );
    }
    let err = check(|j| j.run_id = long(MAX_ID_LEN + 1));
    expect(err, "run_id", Rule::TooLong, refusal::INVALID_JOB);
    let mut spec = job();
    spec.run_id = long(MAX_ID_LEN);
    spec.lease_id = format!("a.b_c-{}", long(MAX_ID_LEN - 6));
    assert_eq!(spec.validate(), Ok(()));
}

/// An image that starts with `-`, is empty, is too long, or holds a space, NUL, newline,
/// quote, `;` or `$` is refused; real references pass. It catches `--privileged` or
/// `-v/:/host` reaching a docker or podman command line as an option.
#[test]
fn image_cannot_be_an_option_or_carry_shell_bytes() {
    for image in [
        "--privileged",
        "-v/:/host",
        "-",
        "a b",
        "a\0",
        "a\n",
        "a;rm",
        "a'b",
        "$x",
        "é",
    ] {
        let err = check(|j| j.image = image.into());
        assert_eq!(
            (err.field(), err.code()),
            ("image", refusal::INVALID_IMAGE),
            "{image:?}"
        );
    }
    expect(
        check(|j| j.image.clear()),
        "image",
        Rule::Empty,
        refusal::INVALID_IMAGE,
    );
    let err = check(|j| j.image = long(MAX_IMAGE_LEN + 1));
    expect(err, "image", Rule::TooLong, refusal::INVALID_IMAGE);
    for image in [
        "ubuntu:24.04",
        "localhost:5000/team/img:1.2-rc_1",
        "ghcr.io/o/i@sha256:0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef",
        &long(MAX_IMAGE_LEN),
    ] {
        let mut spec = job();
        spec.image = image.into();
        assert_eq!(spec.validate(), Ok(()), "{image}");
    }
}

/// Argument count, each argument's length, NUL, and the total are bounded; the caps pass.
/// It catches a ten-million-element args array, a 100 MB argument, and a NUL that would
/// cut an exec argument short.
#[test]
fn args_are_bounded() {
    let err = check(|j| j.args = vec!["a".into(); MAX_ARGS + 1]);
    expect(err, "args", Rule::TooMany, refusal::INVALID_JOB);
    let err = check(|j| j.args = vec![long(MAX_ARG_LEN + 1)]);
    expect(err, "args", Rule::TooLong, refusal::INVALID_JOB);
    let err = check(|j| j.args = vec!["a\0b".into()]);
    expect(err, "args", Rule::BadChars, refusal::INVALID_JOB);
    let per = MAX_ARGS_TOTAL_BYTES / (MAX_ARG_LEN / 2);
    let err = check(|j| j.args = vec![long(MAX_ARG_LEN / 2); per + 1]);
    expect(err, "args", Rule::TooLong, refusal::INVALID_JOB);
    let mut spec = job();
    spec.args = vec![long(MAX_ARG_LEN / 2); per];
    assert_eq!(spec.validate(), Ok(()));
    spec.args = vec!["a".into(); MAX_ARGS];
    assert_eq!(spec.validate(), Ok(()));
    spec.args = vec![long(MAX_ARG_LEN)];
    assert_eq!(spec.validate(), Ok(()));
}

fn distinct_mounts(count: usize) -> Vec<Mount> {
    (0..count)
        .map(|i| mount("cache", &format!("m{i}")))
        .collect()
}

fn mount(root: &str, rel: &str) -> Mount {
    Mount {
        root_name: root.into(),
        rel: rel.into(),
        ro: false,
    }
}

/// A mount root name that is empty, long, holds `/`, `..`, NUL or a space is refused; so
/// is a rel that is empty (the root itself), absolute, climbing, holding NUL or too long;
/// plain, dotted and nested paths pass. It catches a mount reaching outside the agent's
/// roots or mounting a whole root, and a root name that is not a plain table key.
#[test]
fn mounts_stay_under_a_named_root() {
    for root in ["", "a/b", "..", "a\0", "a b", "/etc", ".x"] {
        let err = mount(root, "x").validate().unwrap_err();
        assert_eq!(
            (err.field(), err.code()),
            ("mounts.root_name", refusal::INVALID_MOUNT),
            "{root:?}"
        );
    }
    let err = mount(&long(MAX_ROOT_NAME_LEN + 1), "x")
        .validate()
        .unwrap_err();
    expect(
        err,
        "mounts.root_name",
        Rule::TooLong,
        refusal::INVALID_MOUNT,
    );
    for rel in ["/etc", "/", "../x", "a/../../b", "..", "a/..", "a/../b"] {
        let err = mount("cache", rel).validate().unwrap_err();
        expect(err, "mounts.rel", Rule::Escapes, refusal::INVALID_MOUNT);
    }
    for rel in [".", "./", "./.", "./a/b", "a/./b", "a//b", "a/", "a/."] {
        let err = mount("cache", rel).validate().unwrap_err();
        expect(err, "mounts.rel", Rule::Empty, refusal::INVALID_MOUNT);
    }
    for rel in [
        "a:b",
        "a,b",
        "x:/host:rw",
        "a\nb",
        "a\tb",
        "a\u{7f}",
        "a\\b",
        "\\",
    ] {
        let err = mount("cache", rel).validate().unwrap_err();
        expect(err, "mounts.rel", Rule::BadChars, refusal::INVALID_MOUNT);
    }
    let err = mount("cache", "a\0b").validate().unwrap_err();
    expect(err, "mounts.rel", Rule::BadChars, refusal::INVALID_MOUNT);
    let err = mount("cache", "").validate().unwrap_err();
    expect(err, "mounts.rel", Rule::Empty, refusal::INVALID_MOUNT);
    let err = mount("cache", &long(MAX_REL_LEN + 1))
        .validate()
        .unwrap_err();
    expect(err, "mounts.rel", Rule::TooLong, refusal::INVALID_MOUNT);
    for rel in [
        "a",
        "a/b",
        "a.b/c",
        "a..b",
        "..a",
        "a.",
        ".a/b",
        &long(MAX_REL_LEN),
    ] {
        assert_eq!(mount("cache", rel).validate(), Ok(()), "{rel:?}");
    }
    let err = check(|j| j.mounts.push(mount("cache", "../x")));
    expect(err, "mounts.rel", Rule::Escapes, refusal::INVALID_MOUNT);
    let err = check(|j| j.mounts = distinct_mounts(MAX_MOUNTS + 1));
    expect(err, "mounts", Rule::TooMany, refusal::INVALID_MOUNT);
    let mut spec = job();
    spec.mounts = distinct_mounts(MAX_MOUNTS);
    assert_eq!(spec.validate(), Ok(()));
}

/// Zero cpu or memory, `pids: 0`, and each value above its cap are refused, a zero wall
/// time with its own code and a too-long one with the generic code; the caps pass. It
/// catches a job that could never run, an unbounded job and a value that makes later sums
/// overflow.
#[test]
fn limits_have_ranges() {
    let cases: [(&str, Edit); 8] = [
        ("limits.cpu_millis", |j| j.limits.cpu_millis = 0),
        ("limits.cpu_millis", |j| {
            j.limits.cpu_millis = MAX_CPU_MILLIS + 1
        }),
        ("limits.memory_bytes", |j| j.limits.memory_bytes = 0),
        ("limits.memory_bytes", |j| {
            j.limits.memory_bytes = MAX_MEMORY_BYTES + 1
        }),
        ("limits.pids", |j| j.limits.pids = Some(0)),
        ("limits.pids", |j| j.limits.pids = Some(MAX_PIDS + 1)),
        ("limits.wall_ms", |j| j.limits.wall_ms = MAX_WALL_MS + 1),
        ("limits.cpu_millis", |j| j.limits.cpu_millis = u64::MAX),
    ];
    for (field, edit) in cases {
        expect(check(edit), field, Rule::OutOfRange, refusal::INVALID_JOB);
    }
    let err = check(|j| j.limits.wall_ms = 0);
    expect(
        err,
        "limits.wall_ms",
        Rule::OutOfRange,
        refusal::WALL_TIME_NOT_POSITIVE,
    );
    let mut spec = job();
    spec.limits.cpu_millis = MAX_CPU_MILLIS;
    spec.limits.memory_bytes = MAX_MEMORY_BYTES;
    spec.limits.pids = Some(MAX_PIDS);
    spec.limits.wall_ms = MAX_WALL_MS;
    assert_eq!(spec.validate(), Ok(()));
    spec.limits.wall_ms = 1;
    spec.limits.pids = Some(1);
    spec.limits.cpu_millis = 1;
    spec.limits.memory_bytes = 1;
    assert_eq!(spec.validate(), Ok(()));
    spec.limits.pids = None;
    assert_eq!(spec.validate(), Ok(()));
}

/// The caps leave room to sum 2^18 hosts' or jobs' values in a u64. It catches a cap
/// raised until a later sum can overflow.
#[test]
fn caps_leave_room_to_sum() {
    assert!(MAX_CPU_MILLIS.checked_mul(1 << 18).is_some());
    assert!(MAX_MEMORY_BYTES.checked_mul(1 << 18).is_some());
    assert!(MAX_WALL_MS.checked_mul(1 << 18).is_some());
}

/// Duplicate GPU indices, more indices than the cap, and `valid_for_ms` above its cap are
/// refused; the cap passes. It catches one device handed to a job twice and an unbounded
/// validity window.
#[test]
fn gpu_indices_and_validity_are_bounded() {
    let err = check(|j| j.gpu_indices = vec![1, 2, 1]);
    expect(err, "gpu_indices", Rule::Duplicate, refusal::INVALID_JOB);
    let err = check(|j| j.gpu_indices = (0..=MAX_GPUS as u32).collect());
    expect(err, "gpu_indices", Rule::TooMany, refusal::INVALID_JOB);
    for index in [MAX_GPUS as u32, 4_000_000_000, u32::MAX] {
        let err = check(|j| j.gpu_indices = vec![index]);
        expect(err, "gpu_indices", Rule::OutOfRange, refusal::INVALID_JOB);
    }
    let err = check(|j| j.valid_for_ms = MAX_VALID_FOR_MS + 1);
    expect(err, "valid_for_ms", Rule::OutOfRange, refusal::INVALID_JOB);
    let mut spec = job();
    spec.gpu_indices = (0..MAX_GPUS as u32).collect();
    assert_eq!(spec.gpu_indices.last(), Some(&(MAX_GPUS as u32 - 1)));
    spec.valid_for_ms = MAX_VALID_FOR_MS;
    assert_eq!(spec.validate(), Ok(()));
    spec.valid_for_ms = 0;
    spec.gpu_indices.clear();
    assert_eq!(spec.validate(), Ok(()));
}

/// A job whose cpu, memory or GPU count exceeds the shape it is checked against is
/// refused with the limits-exceed-shape code; equal passes. It catches the code existing
/// without anything computing it.
#[test]
fn limits_are_checked_against_a_shape() {
    let shape = |cpu, memory, gpus| WireShape {
        cpu_millis: cpu,
        memory_bytes: memory,
        gpus,
        disk_bytes: 0,
        gpu_class: None,
        max_leases: None,
    };
    let spec = job();
    let (cpu, memory) = (spec.limits.cpu_millis, spec.limits.memory_bytes);
    assert_eq!(spec.fits_shape(&shape(cpu, memory, 1)), Ok(()));
    for over in [
        shape(cpu - 1, memory, 1),
        shape(cpu, memory - 1, 1),
        shape(cpu, memory, 0),
    ] {
        let err = spec.fits_shape(&over).unwrap_err();
        expect(
            err,
            "limits",
            Rule::Inconsistent,
            refusal::LIMITS_EXCEED_SHAPE,
        );
    }
}

/// A stop or cancel with a bad run id is refused, an unknown command passes, and a start
/// is held to the job rules. It catches commands that skip validation.
#[test]
fn commands_are_validated() {
    for command in [
        Command::Stop {
            run_id: "../x".into(),
        },
        Command::Cancel {
            run_id: String::new(),
        },
        Command::Stop {
            run_id: long(MAX_ID_LEN + 1),
        },
    ] {
        let err = command.validate().unwrap_err();
        assert_eq!(
            (err.field(), err.code()),
            ("run_id", refusal::INVALID_REPLY)
        );
    }
    assert_eq!(Command::Unknown.validate(), Ok(()));
    assert_eq!(
        Command::Stop {
            run_id: "r1".into()
        }
        .validate(),
        Ok(())
    );
    assert_eq!(
        Command::Cancel {
            run_id: "r1".into()
        }
        .validate(),
        Ok(())
    );
    assert_eq!(Command::Start(Box::new(job())).validate(), Ok(()));
    let mut spec = job();
    spec.image = "--x".into();
    assert!(Command::Start(Box::new(spec)).validate().is_err());
}
