//! A held container (`super::held`): its bundle, its pipe and the job written to it.

use std::io::Read as _;
use std::os::unix::fs::{FileTypeExt as _, PermissionsExt as _};

use rustix::fs::{Mode, OFlags};
use serde_json::json;

use super::tests::{config, scratch, start};
use super::*;
use crate::container::ContainerSpec;
use crate::container::tests::spec;

/// The job of the fixture's spec, as its fields: the form, the working directory (the exec
/// root and the spec's own), the arguments and the environment with the two defaults.
fn fixture_fields() -> Vec<&'static str> {
    vec![
        "js-job/1",
        "/job/root/pkg",
        "3",
        "tool",
        "--flag",
        "a b",
        "3",
        "PATH=/bin",
        "HOSTNAME=localhost",
        "HOME=/root",
    ]
}

/// The bytes of `fields`, each ended by a NUL byte.
fn framed(fields: &[&str]) -> Vec<u8> {
    fields
        .iter()
        .flat_map(|f| [f.as_bytes(), b"\0"].concat())
        .collect()
}

/// Opens the read end of the pipe in `lease` without waiting for a writer, then waiting
/// as a reader of a pipe does: a held init's open, already made.
fn reader(lease: &Path) -> std::fs::File {
    let flags = OFlags::RDONLY | OFlags::NONBLOCK | OFlags::CLOEXEC;
    let fd = rustix::fs::open(job_pipe(lease), flags, Mode::empty()).expect("the read end");
    rustix::fs::fcntl_setfl(&fd, OFlags::empty()).expect("blocking");
    std::fs::File::from(fd)
}

/// Proves a held bundle's init waits for its job on the lease's pipe, with no program of
/// its own, and that the pipe is bound read-only into the container's `/dev` last, while a
/// loopback bundle of the same spec runs its argv and binds no pipe. Catches a held
/// container that runs the spec's placeholder program, or whose init cannot reach the pipe.
#[tokio::test]
async fn a_held_bundle_waits_for_its_job_on_the_leases_pipe() {
    let dir = scratch("held-bundle");
    let oci = start(&config(&dir, true), &dir.join("scratch")).expect("started");
    let mut held = spec();
    held.dir = dir.join("held");
    oci.create_held(&held).await.expect("bundle");
    let config: serde_json::Value =
        serde_json::from_slice(&std::fs::read(held.dir.join("bundle/config.json")).expect("read"))
            .expect("json");
    assert_eq!(
        config["process"]["args"],
        json!(["/dev/js-init", "--wait-job", "/dev/js-job", "--"])
    );
    let mounts = config["mounts"].as_array().expect("mounts");
    assert_eq!(
        mounts.last(),
        Some(&json!({
            "destination": "/dev/js-job",
            "type": "bind",
            "source": held.dir.join("job"),
            "options": ["bind", "ro", "nosuid", "noexec", "nodev"],
        }))
    );
    let meta = std::fs::symlink_metadata(job_pipe(&held.dir)).expect("the pipe");
    assert!(meta.file_type().is_fifo());
    assert_eq!(meta.permissions().mode() & 0o777, 0o600);
    assert_eq!(
        std::fs::read_to_string(held.dir.join("etc/resolv.conf")).expect("resolv.conf"),
        RESOLV_CONF
    );

    let mut cold = spec();
    cold.dir = dir.join("cold");
    oci.create_loopback(&cold).await.expect("bundle");
    let config: serde_json::Value =
        serde_json::from_slice(&std::fs::read(cold.dir.join("bundle/config.json")).expect("read"))
            .expect("json");
    assert_eq!(
        config["process"]["args"],
        json!(["/dev/js-init", "--", "tool", "--flag", "a b"])
    );
    let pipes = config["mounts"].as_array().expect("mounts").iter();
    assert!(!pipes.into_iter().any(|m| m["destination"] == "/dev/js-job"));
    assert!(!job_pipe(&cold.dir).exists());
}

/// Proves a held bundle keeps the init's other waits after `--wait-job` (its listen ports),
/// a pipe a failed make left is replaced, and a host container is never held. Catches a held
/// init that skips a wait a cold one makes, or a second make failing on the first's pipe.
#[tokio::test]
async fn a_held_bundle_keeps_its_other_waits_and_a_host_container_is_never_held() {
    let dir = scratch("held-waits");
    let oci = start(&config(&dir, true), &dir.join("scratch")).expect("started");
    let mut held = spec();
    held.dir = dir.join("held");
    held.listen_ports = vec![8080];
    std::fs::create_dir_all(&held.dir).expect("lease");
    std::fs::write(job_pipe(&held.dir), b"left over").expect("an old file");
    oci.create_held(&held).await.expect("bundle");
    let config: serde_json::Value =
        serde_json::from_slice(&std::fs::read(held.dir.join("bundle/config.json")).expect("read"))
            .expect("json");
    assert_eq!(
        config["process"]["args"],
        json!([
            "/dev/js-init",
            "--wait-job",
            "/dev/js-job",
            "--wait-listen",
            "30",
            "8080",
            "--"
        ])
    );
    assert!(
        std::fs::symlink_metadata(job_pipe(&held.dir))
            .expect("the pipe")
            .file_type()
            .is_fifo()
    );

    let mut host = spec();
    host.dir = dir.join("host");
    host.isolation = crate::container::Isolation::Host;
    let why = oci.create_held(&host).await.expect_err("a host container");
    assert_eq!(why, "a host container is never made ahead of its job");
    assert!(!host.dir.exists(), "nothing was made");
}

/// Proves the pipe cannot be made where a directory takes its name, nor in a lease that is
/// not there, each failure naming the pipe. Catches a make that removes a directory, or
/// reports a failure without the path.
#[test]
fn a_pipe_that_cannot_be_made_names_its_path() {
    let dir = scratch("held-pipe-fails");
    std::fs::create_dir_all(dir.join("taken/job")).expect("a directory in the way");
    let why = held::make_pipe(&dir.join("taken")).expect_err("a directory");
    assert!(
        why.to_string()
            .starts_with(&format!("{}: ", dir.join("taken/job").display())),
        "{why}"
    );
    let why = held::make_pipe(&dir.join("gone")).expect_err("no lease");
    assert!(
        why.to_string()
            .starts_with(&format!("{}: ", dir.join("gone/job").display())),
        "{why}"
    );
}

/// Proves the bytes of a job: each field ended by a NUL byte, the working directory the exec
/// root alone when the spec names none, `HOSTNAME` and `HOME` defaulted unless the job sets
/// them, in the order `process.env` would have them. Catches a job whose environment or
/// directory differ from what the same spec's cold container gets.
#[test]
fn a_jobs_bytes_are_its_fields_each_ended_by_a_nul() {
    assert_eq!(
        job_bytes(&spec()).expect("bytes"),
        framed(&fixture_fields())
    );
    let mut own = spec();
    own.working_directory = String::new();
    own.env = vec![
        ("HOME".to_owned(), "/home/job".to_owned()),
        ("A".to_owned(), "1=2".to_owned()),
    ];
    own.argv = vec!["/bin/true".to_owned()];
    assert_eq!(
        job_bytes(&own).expect("bytes"),
        framed(&[
            "js-job/1",
            "/job/root",
            "1",
            "/bin/true",
            "3",
            "HOME=/home/job",
            "A=1=2",
            "HOSTNAME=localhost",
        ])
    );
}

/// Proves a job that cannot be written in its form is refused before the pipe is touched:
/// no program, or a NUL byte in the directory, an argument or a variable, each named. Catches
/// a NUL that would split a field and hand the init another job than the spec's.
#[test]
fn a_job_that_cannot_be_framed_is_refused_by_name() {
    let mut none = spec();
    none.argv.clear();
    assert_eq!(
        job_bytes(&none).expect_err("none").to_string(),
        "the job names no program"
    );
    let mut cases: Vec<(ContainerSpec, &str)> = Vec::new();
    let mut dir = spec();
    dir.working_directory = "a\0b".to_owned();
    cases.push((dir, "the job's working directory holds a NUL byte"));
    let mut arg = spec();
    arg.argv.push("x\0y".to_owned());
    cases.push((arg, "the job's arguments holds a NUL byte"));
    let mut var = spec();
    var.env.push(("X".to_owned(), "\0".to_owned()));
    cases.push((var, "the job's environment holds a NUL byte"));
    for (spec, said) in cases {
        let why = hand_job(&spec).expect_err("refused");
        assert_eq!(why.to_string(), said);
    }
}

/// Proves the hand-over: with no init waiting it is `NotWaiting` and writes nothing; with a
/// reader it writes the whole job, one longer than a pipe holds included, and the reader sees
/// the end; a lease with no pipe names the pipe. Catches a hand-over that blocks on a
/// container that is not up, or cuts a long job short.
#[test]
fn a_job_is_handed_whole_to_a_waiting_init_and_never_to_none() {
    let dir = scratch("held-hand");
    let mut job = spec();
    job.dir = dir.join("lease");
    std::fs::create_dir_all(&job.dir).expect("lease");
    held::make_pipe(&job.dir).expect("pipe");
    let why = hand_job(&job).expect_err("nobody waits");
    assert_eq!(
        why.to_string(),
        format!(
            "no init waits for a job on {}",
            job_pipe(&job.dir).display()
        )
    );

    let mut waiting = reader(&job.dir);
    hand_job(&job).expect("handed");
    let mut read = Vec::new();
    waiting.read_to_end(&mut read).expect("read");
    assert_eq!(read, framed(&fixture_fields()));

    // Longer than the pipe holds: the writer waits for the reader as it reads.
    let mut long = job.clone();
    long.argv.push("x".repeat(256 << 10));
    let mut waiting = reader(&long.dir);
    // A writer of the test's own, held until the hand-over is done: a reader of a pipe with
    // no writer reads its end at once.
    let flags = OFlags::WRONLY | OFlags::NONBLOCK | OFlags::CLOEXEC;
    let held_open = rustix::fs::open(job_pipe(&long.dir), flags, Mode::empty()).expect("writer");
    let read = std::thread::spawn(move || {
        let mut read = Vec::new();
        waiting.read_to_end(&mut read).map(|_| read)
    });
    hand_job(&long).expect("handed");
    drop(held_open);
    let read = read.join().expect("the reader").expect("read");
    assert_eq!(read, job_bytes(&long).expect("bytes"));

    let mut gone = spec();
    gone.dir = dir.join("no-pipe");
    let why = hand_job(&gone).expect_err("no pipe");
    assert!(
        why.to_string()
            .starts_with(&format!("{}: ", job_pipe(&gone.dir).display())),
        "{why}"
    );
}
