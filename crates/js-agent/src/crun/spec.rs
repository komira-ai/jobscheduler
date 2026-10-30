//! The pure part of the crun runtime: what a run's spec becomes for js-contain, and how a
//! container's end becomes the runtime's answer.

use std::path::Path;

use js_agent_wire::LogTail;
use js_contain::cgroup::{LeaseCgroup, OomOutcome};
use js_contain::container::{ContainerLimits, ContainerSpec, Ended, HostMount};
use js_contain::oci::image::PinnedImage;
use js_contain::oci::{LeaseDirs, LogTail as Captured, StreamTail};

use crate::cli_runtime::{DEFAULT_PIDS, FILES_TARGET, SCRATCH_TARGET, text_of};
use crate::lifecycle::{Exit, RunSpec};

/// Where the container sees the overlay of its (empty) input root, and where the job
/// starts: the image's own `WORKDIR` is not applied (see
/// [`ImageConfig::working_dir`](crate::image::ImageConfig::working_dir)).
pub(super) const EXEC_ROOT: &str = "/work";

/// The exit code of a container that did not run to an exit: `crun` or the init reported
/// an error, or `crun` could not be run. The code a shell gives for a command it found and
/// could not invoke.
pub(super) const NOT_RUN: i32 = 126;

/// The exit code of a process the kernel killed (`SIGKILL`).
const KILLED: i32 = 137;

/// The image of a run as the image store names it: `<repository>@sha256:<64 hex digits>`
/// with the tag, if the reference has one, left out (the digest pins the image; the store
/// asks the registry for the repository and the digest).
///
/// # Errors
/// The reference names no sha256 digest.
pub(super) fn pinned(image: &str) -> Result<PinnedImage, String> {
    let digest = image.rsplit_once('@').map(|(_, digest)| digest);
    let valid = |digest: &&str| {
        digest.strip_prefix("sha256:").is_some_and(|hex| {
            hex.len() == 64 && hex.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
        })
    };
    let digest = digest
        .filter(valid)
        .ok_or_else(|| format!("image {image} is not pinned by a sha256 digest"))?;
    let name = image.rsplit_once('@').map_or(image, |(name, _)| name);
    // A tag is a ':' after the last '/' (a ':' before it is a registry's port).
    let repository = match name.rsplit_once(':') {
        Some((repository, tag)) if !tag.contains('/') => repository,
        _ => name,
    };
    Ok(PinnedImage {
        reference: format!("{repository}@{digest}"),
        digest: digest.to_owned(),
    })
}

/// The container of `run` named `name` over `image`, with its directories and the cgroup
/// `cgroup_parent`. The program and environment are the image's to give
/// ([`ImageConfig::apply`](crate::image::ImageConfig::apply)); they are empty here.
///
/// The run's scratch directory, its mounts and its files are bound after the exec root, in
/// that order, as js-contain wants host mounts.
pub(super) fn container_spec(
    name: &str,
    run: &RunSpec,
    image: &PinnedImage,
    lease: &LeaseDirs,
    cgroup_parent: &str,
) -> ContainerSpec {
    let bind = |target: &str, source: &Path, read_only: bool| HostMount {
        target: target.to_owned(),
        source: source.to_owned(),
        read_only,
    };
    let mut mounts = vec![bind(SCRATCH_TARGET, &run.scratch, false)];
    mounts.extend(
        run.mounts
            .iter()
            .map(|mount| bind(&mount.target, &mount.host, mount.ro)),
    );
    mounts.extend(
        run.files
            .iter()
            .map(|files| bind(FILES_TARGET, files, true)),
    );
    ContainerSpec {
        name: name.to_owned(),
        image: image.reference.clone(),
        image_digest: image.digest.clone(),
        dir: lease.dir.clone(),
        cgroup_parent: cgroup_parent.to_owned(),
        exec_root: EXEC_ROOT.to_owned(),
        input_root: lease.root.clone(),
        upper: lease.upper.clone(),
        overlay_work: lease.work.clone(),
        working_directory: String::new(),
        env: Vec::new(),
        argv: Vec::new(),
        limits: ContainerLimits {
            pids: run.pids.unwrap_or(DEFAULT_PIDS),
            ..ContainerLimits::DEFAULT
        },
        shared: Vec::new(),
        mounts,
    }
}

/// What a container's end is to the lifecycle: the exit status of the job, or [`NOT_RUN`]
/// for one that did not run; flagged out of memory only when the kernel's killer ran for
/// the lease's own limit (a node short of memory is not the booking's fault).
pub(super) fn exit_of(ended: Result<Ended, String>, oom: Option<&OomOutcome>) -> Exit {
    let code = match ended {
        Ok(Ended::Exited(code)) => code,
        Ok(Ended::NotRun(_)) | Err(_) => NOT_RUN,
    };
    let own = matches!(oom, Some(OomOutcome::OwnLimit { .. }));
    Exit {
        code: if own { KILLED } else { code },
        oom_killed: own,
    }
}

/// The name of the container's cgroup below its lease's: where `crun` makes it from the
/// bundle's `cgroupsPath`.
const CTR: &str = "ctr";

/// The cgroup of the container of a lease whose cgroup is `lease`: `<lease>/ctr`, the
/// layout the usage sampler and the integrity check read.
pub(super) fn ctr_of(lease: &Path) -> std::path::PathBuf {
    lease.join(CTR)
}

/// What a container's end is, with the kernel's account of it read from the lease's cgroup
/// (`memory.events`, and the actions cgroup's `memory.events.local`): [`exit_of`] with the
/// OOM outcome js-contain classifies. A cgroup that cannot be read says nothing of an OOM
/// kill, so the end is the exit as crun gave it.
pub(super) fn end_of(
    ended: Result<Ended, String>,
    cgroup: &LeaseCgroup,
    cap: Option<u64>,
    backstop: Option<u64>,
) -> Exit {
    let oom = cgroup.oom_outcome(cap, backstop).ok().flatten();
    exit_of(ended, oom.as_ref())
}

/// The words `crun` or the init gave for a container that did not run to an exit.
pub(super) fn not_run_words(ended: &Result<Ended, String>) -> Option<&str> {
    match ended {
        Ok(Ended::NotRun(words)) | Err(words) => Some(words),
        Ok(Ended::Exited(_)) => None,
    }
}

/// The tail of what a run wrote, as the result carries it: each stream at most
/// [`MAX_LOG_STREAM_LEN`](js_agent_wire::limits::MAX_LOG_STREAM_LEN) bytes of UTF-8 (invalid bytes replaced, the cut at a character),
/// `truncated` when the job wrote more than a tail holds. A ring that dropped older bytes
/// may begin inside a character: its first bytes are skipped. `report`, the words of a
/// container that did not run, ends the standard error, so that they reach the result.
pub(super) fn log_tail_of(captured: &Captured, report: Option<&str>) -> LogTail {
    let (stdout, cut_out) = stream_text(&captured.stdout, &[]);
    let words: Vec<u8> = report
        .map(|words| format!("crun: {}\n", words.trim_end()).into_bytes())
        .unwrap_or_default();
    let (stderr, cut_err) = stream_text(&captured.stderr, &words);
    LogTail {
        stdout,
        stderr,
        truncated: cut_out || cut_err,
    }
}

/// One stream's text, with `after` appended to it (on a line of its own), and whether the
/// job wrote bytes that the text lacks.
fn stream_text(stream: &StreamTail, after: &[u8]) -> (String, bool) {
    let dropped = stream.total > u64::try_from(stream.bytes.len()).unwrap_or(u64::MAX);
    let mut bytes = stream.bytes.clone();
    if !after.is_empty() && !bytes.is_empty() && bytes.last() != Some(&b'\n') {
        bytes.push(b'\n');
    }
    bytes.extend_from_slice(after);
    let (text, cut) = text_of(&bytes, dropped);
    (text, dropped || cut)
}
