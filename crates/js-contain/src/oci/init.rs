//! The first process of a job's container (`init/init.rs`), embedded in this
//! crate: how it is put on the node and where the container sees it.
//!
//! The binary is built statically by `build.rs`, so it runs in any image. The driver
//! writes it to `<scratch>/oci-bin/js-init` (the scratch directory is searchable by
//! others; an installed or built daemon's directory may not be) and binds it read-only
//! at [`super::OciConfig::init_dest`], in the container's own `/dev` (a `tmpfs` made fresh per container, so
//! the image's root is never written to).

use std::io::Write as _;
use std::path::{Path, PathBuf};

use super::install::{WhileOpen, nothing_while_open};

/// The program, empty on a target that runs no containers.
const BINARY: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/js-init"));

/// Where the container sees it unless the caller says otherwise
/// ([`super::OciConfig::init_dest`]).
pub const DEFAULT_DEST: &str = "/dev/js-init";

/// The file in a lease directory the init writes why the program could not be started
/// to (its stdin: the container's other processes cannot reach it).
pub(crate) fn report_path(lease: &Path) -> PathBuf {
    lease.join("init-report")
}

/// Makes the report file of the lease in `lease`, empty, for the init to write to.
pub(crate) fn create_report(lease: &Path) -> Result<std::fs::File, String> {
    let path = report_path(lease);
    std::fs::File::create(&path).map_err(|e| format!("{}: {e}", path.display()))
}

/// Writes the program into `<scratch>/oci-bin/js-init` (mode 0755, replaced
/// atomically) and returns that path. It is written on a thread with a descriptor table of
/// its own, so no forked child holds it busy (`super::install`).
pub(crate) fn install(scratch: &Path) -> std::io::Result<PathBuf> {
    install_with(scratch, &mut nothing_while_open)
}

/// [`install`], running `while_open` on the writing thread once the program is written and
/// before its file is closed: the seam a test forks through.
pub(crate) fn install_with(
    scratch: &Path,
    while_open: &mut WhileOpen<'_>,
) -> std::io::Result<PathBuf> {
    install_bytes(scratch, BINARY, while_open)
}

fn install_bytes(
    scratch: &Path,
    bytes: &[u8],
    while_open: &mut WhileOpen<'_>,
) -> std::io::Result<PathBuf> {
    if bytes.is_empty() {
        return Err(std::io::Error::other(
            "this build has no container init: containers run on Linux only",
        ));
    }
    super::install::publish(scratch, "js-init", &mut |file| {
        file.write_all(bytes)?;
        while_open();
        Ok(())
    })
}

#[cfg(test)]
mod tests {
    use std::os::unix::fs::PermissionsExt as _;

    use super::*;

    fn scratch(name: &str) -> PathBuf {
        let dir = std::env::current_exe()
            .expect("test binary")
            .parent()
            .expect("deps directory")
            .join("js-contain-unit")
            .join(format!("init-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("scratch");
        dir
    }

    /// Catches an init the container's user cannot execute (a mode without the others'
    /// bits: the job's ids are not the daemon's), one that is not the bytes built, a
    /// second install failing on the first's file, and a build without an init (a target
    /// that runs no containers) installing an empty file the container would fail to
    /// start on.
    #[test]
    fn the_init_is_installed_executable_by_others_and_replaced_atomically() {
        let scratch = scratch("install");
        let first = install(&scratch).expect("install");
        assert_eq!(first, scratch.join("oci-bin/js-init"));
        let mode = |p: &Path| std::fs::metadata(p).expect("meta").permissions().mode() & 0o777;
        assert_eq!(mode(&first), 0o755);
        assert_eq!(mode(first.parent().expect("dir")), 0o755);
        assert_eq!(std::fs::read(&first).expect("read"), BINARY);
        assert!(!BINARY.is_empty());
        assert_eq!(install(&scratch).expect("again"), first);
        let names: Vec<_> = std::fs::read_dir(first.parent().expect("dir"))
            .expect("dir")
            .map(|e| e.expect("entry").file_name())
            .collect();
        assert_eq!(names, ["js-init"], "no staged file left");
        // The report file: empty each time, and an error naming the file when the lease
        // directory is not there.
        let lease = scratch.join("lease");
        std::fs::create_dir(&lease).expect("lease");
        std::fs::write(report_path(&lease), b"left over").expect("old report");
        create_report(&lease).expect("report");
        assert_eq!(std::fs::read(report_path(&lease)).expect("read"), b"");
        let why = create_report(&scratch.join("gone")).expect_err("no lease directory");
        assert!(why.contains("gone/init-report: "), "{why}");
        let empty = install_bytes(&scratch, b"", &mut nothing_while_open).expect_err("empty");
        assert!(empty.to_string().contains("no container init"), "{empty}");
    }

    /// Whether the process `pid` is still the `sleep 100000` the script below started (a pid
    /// that is gone, or reused by another program, is not).
    fn sleeping(pid: &str) -> bool {
        std::fs::read(format!("/proc/{pid}/cmdline"))
            .is_ok_and(|line| line == b"sleep\x00100000\x00")
    }

    /// Runs the installed init with `options`, over a shell that leaves a child and a
    /// grandchild whose parent has gone, writing their pids to `dir`, and exits 3. Returns the
    /// init's exit code and the two pids.
    fn run_leaving_two(init: &Path, dir: &Path, options: &[&str]) -> (Option<i32>, [String; 2]) {
        let script = format!(
            "sleep 100000 & echo $! > {d}/child; (sleep 100000 & echo $! > {d}/grandchild); exit 3",
            d = dir.display()
        );
        let status = std::process::Command::new(init)
            .args(options)
            .args(["--", "/bin/sh", "-c", &script])
            .stdin(std::process::Stdio::null())
            .status()
            .expect("the init runs");
        let read = |name: &str| {
            let text = std::fs::read_to_string(dir.join(name)).expect("a pid");
            text.trim().to_owned()
        };
        (status.code(), [read("child"), read("grandchild")])
    }

    /// Proves the init of a host container (`--end-descendants`) ends with its program's
    /// code only once everything the program left is gone: a background child and a
    /// grandchild whose parent had exited are both killed and reaped by the time it exits.
    /// Without the option (a contained job's init, whose PID namespace the kernel ends) they
    /// are left running. Catches a host job whose leftover processes keep `crun run`, and the
    /// job, from ever ending.
    #[test]
    fn a_host_init_ends_what_its_program_left_before_it_exits() {
        let dir = scratch("end-descendants");
        let init = install(&dir).expect("install");
        let (code, left) = run_leaving_two(&init, &dir, &["--end-descendants"]);
        assert_eq!(code, Some(3));
        for pid in &left {
            assert!(!sleeping(pid), "process {pid} outlived the init");
        }
        let (code, left) = run_leaving_two(&init, &dir, &[]);
        assert_eq!(code, Some(3));
        for pid in &left {
            assert!(sleeping(pid), "process {pid} was ended without the option");
            let _ = std::process::Command::new("kill")
                .args(["-9", pid])
                .status();
        }
    }
}
