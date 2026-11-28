//! The resource limits of `docs/design/mac-runtime.md` section 3: what the native runtime
//! relies on (`RLIMIT_NPROC` counted per uid, `RLIMIT_FSIZE` per process) and what macOS does
//! not give it (a memory limit, a CPU rate). Each probe starts one of this binary's limit
//! modes ([`crate::child::Child`]), which sets the limit on itself and then acts past it.

use std::path::Path;
use std::process::Command;

use super::Scratch;
use super::exec::{self, Outcome};
use crate::parse::field;
use crate::report::{Probe, Report};

/// The process limit of `limits/nproc-per-uid`: the child alone is one process, so a count
/// per process tree would let it start one more; the runner's uid has more than this.
const PROCESSES: &str = "2";
/// The file size limit of `limits/fsize`, in bytes.
const FILE_LIMIT: u64 = 64 * 1024;
/// What `limits/fsize` tries to write, in bytes.
const FILE_WRITE: u64 = 1024 * 1024;
/// The address-space and data limit of `limits/as-not-enforced`, in bytes.
const MEMORY_LIMIT: u64 = 64 * 1024 * 1024;
/// What `limits/as-not-enforced` allocates and writes, in bytes.
const MEMORY_ALLOCATE: u64 = 512 * 1024 * 1024;
/// The soft CPU limit of `limits/cpu-seconds`, in seconds.
const CPU_SECONDS: u64 = 1;
/// The rounds of work `limits/cpu-seconds` gives the child: far more than one CPU second
/// on any Mac, so only the limit ends it, and few enough that an unenforced limit ends in
/// an exit within the job's time.
const CPU_ROUNDS: u64 = 60_000;

pub fn probes(scratch: &Scratch, report: &mut Report) {
    let exe = &scratch.places.exe;
    report.push(nproc_per_uid(exe));
    report.push(fsize(exe, &scratch.base.join("fsize")));
    report.push(as_not_enforced(exe));
    report.push(cpu_seconds(exe));
}

/// This binary in child mode `mode` with `args`.
fn child(exe: &Path, mode: &str, args: &[&str]) -> Outcome {
    let mut cmd = Command::new(exe);
    cmd.args(["child", mode]).args(args);
    exec::run(cmd)
}

/// Adds `prefix`signal, the signal that ended a command, when one did.
fn signal(probe: Probe, prefix: &str, outcome: &Outcome) -> Probe {
    match outcome.signal {
        Some(sig) => probe.fact(&format!("{prefix}signal"), sig),
        None => probe,
    }
}

/// Asserted: `RLIMIT_NPROC` counts every process of the uid, not the limited process's
/// own descendants. With the limit at 2, the child, one process with no children, cannot
/// start one (`EAGAIN`), because the uid already has more (this probe, at least); without
/// the limit the same start succeeds.
fn nproc_per_uid(exe: &Path) -> Probe {
    let exe_text = exe.to_string_lossy();
    let target = [exe_text.as_ref(), "child", "env", "HOME"];
    let control = child(exe, "spawn", &target);
    let limited_args: Vec<&str> = ["--max-processes", PROCESSES]
        .into_iter()
        .chain(target)
        .collect();
    let limited = child(exe, "spawn", &limited_args);
    let refused = limited.refused()
        && limited
            .stderr
            .starts_with(&format!("errno={} ", libc::EAGAIN));
    let probe = Probe::asserted("limits/nproc-per-uid", control.ok && refused)
        .fact("max_processes", PROCESSES);
    let probe = exec::describe(probe, "control_", &control);
    exec::describe(probe, "limited_", &limited)
}

/// Asserted: `RLIMIT_FSIZE` ends a process that writes past it (`SIGXFSZ`), and the file
/// holds no more than the limit.
fn fsize(exe: &Path, path: &Path) -> Probe {
    let path_text = path.to_string_lossy();
    let outcome = child(
        exe,
        "write-bytes",
        &[
            "--limit",
            &FILE_LIMIT.to_string(),
            "--bytes",
            &FILE_WRITE.to_string(),
            path_text.as_ref(),
        ],
    );
    let size = std::fs::metadata(path).map(|meta| meta.len());
    let _ = std::fs::remove_file(path);
    let held =
        outcome.signal == Some(libc::SIGXFSZ) && size.as_ref().is_ok_and(|&len| len <= FILE_LIMIT);
    let probe = Probe::asserted("limits/fsize", held)
        .fact("limit_bytes", FILE_LIMIT)
        .fact("write_bytes", FILE_WRITE)
        .fact(
            "file_bytes",
            size.map_or_else(|err| format!("error: {err}"), |len| len.to_string()),
        );
    signal(exec::describe(probe, "", &outcome), "", &outcome)
}

/// Asserted: `RLIMIT_AS` and `RLIMIT_DATA` give no memory cap on macOS (and `RLIMIT_RSS` is
/// `RLIMIT_AS` there). The child sets both to 64 MiB and then allocates and writes 512 MiB.
/// The claim holds either way macOS answers: it refuses the limit itself (`EINVAL`, as the
/// hosted macOS 15 runner does), or it takes the limit and the whole allocation succeeds.
/// It fails when the allocation is refused under the limit, the one answer that would change
/// what `mac-runtime.md` says a memory cap can be built from, and on any other failure.
fn as_not_enforced(exe: &Path) -> Probe {
    let outcome = child(
        exe,
        "allocate",
        &[
            "--limit",
            &MEMORY_LIMIT.to_string(),
            "--bytes",
            &MEMORY_ALLOCATE.to_string(),
        ],
    );
    let limit_refused = outcome.refused()
        && outcome
            .stderr
            .starts_with(&format!("errno={} ", libc::EINVAL));
    let allocated = outcome.ok && field(&outcome.stdout, "allocated") == Some(MEMORY_ALLOCATE);
    let probe = Probe::asserted("limits/as-not-enforced", limit_refused || allocated)
        .fact("limit_bytes", MEMORY_LIMIT)
        .fact("allocate_bytes", MEMORY_ALLOCATE)
        .fact("limit_refused", limit_refused);
    signal(exec::describe(probe, "", &outcome), "", &outcome)
}

/// Asserted: `RLIMIT_CPU` ends one process after a number of CPU seconds (`SIGXCPU` at the
/// soft limit). It is a total for the process, not a rate, so it gives no CPU quota.
fn cpu_seconds(exe: &Path) -> Probe {
    let outcome = child(
        exe,
        "burn",
        &[
            "--seconds",
            &CPU_SECONDS.to_string(),
            "--rounds",
            &CPU_ROUNDS.to_string(),
        ],
    );
    let probe = Probe::asserted("limits/cpu-seconds", outcome.signal == Some(libc::SIGXCPU))
        .fact("limit_seconds", CPU_SECONDS)
        .fact("rounds", CPU_ROUNDS);
    signal(exec::describe(probe, "", &outcome), "", &outcome)
}
