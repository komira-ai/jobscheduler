//! Short-lived helper containers: the "setup namespace" work, done by `crun` itself.
//!
//! A directory the daemon user owns can be given to the job's user (a subordinate
//! id), read back, or deleted once it holds files owned by subordinate ids, only from
//! inside a user namespace that maps both the daemon user and the subordinate range.
//! A helper is that namespace: container id 0 is the daemon user, container ids
//! `1..=65536` are the subordinate range ([`DaemonIds::setup_mappings`]), so a job's
//! user (uid 1000, host subordinate id 1000) is helper id 1001, the `1001:1001` it is given directories as.
//! `crun` creates it; AppArmor's grant for user namespaces is `crun`'s own profile,
//! so the daemon needs no profile of its own.
//!
//! A helper runs one program, never a shell: this daemon's own binary in its
//! give mode (descriptor-relative, never following a symlink) or its unpack mode (the
//! arguments that select them are [`HelperModes`]; the modes are [`crate::give`] and
//! [`crate::unpack_layers`], reached by [`crate::modes::dispatch`]), or the node's `find`, from a
//! read-only bind of the host's `/usr`. Its
//! root is a small directory of mount points (`prepare_rootfs`) shared by all helpers.
//! The directories it works on are bound at `/t/0`, `/t/1`, ... so no per-lease path
//! is ever created in the shared root.

use std::collections::HashSet;
use std::path::{Path, PathBuf};

use oci_spec::runtime::{
    LinuxBuilder, LinuxCapabilitiesBuilder, LinuxNamespaceBuilder, LinuxNamespaceType,
    ProcessBuilder, RootBuilder, Spec, SpecBuilder, UserBuilder,
};

use super::Mapping;
use super::hardening::held_capabilities;
use super::spec::{bind, built, filesystem};
use crate::subids::DaemonIds;

/// The arguments by which the daemon's own binary is told to run as a helper.
///
/// The give mode is run as `<binary> <give> <uid>:<gid> <dir>|- <whole> <dir>...`: it
/// gives the directories (and only the directories) below the first tree, `-` for none,
/// and every entry below each tree after `<whole>`, to the owner, by descriptor and
/// never through a symlink. The unpack mode is run as
/// `<binary> <unpack> <stage> <blobs> <layer>...`: it applies the layer files of `blobs`,
/// in order, into `stage/`[`image::ROOTFS`](super::image::ROOTFS). Both are implemented
/// here ([`crate::modes`]); the embedding process's `main` calls [`crate::modes::dispatch`]
/// with the same value first.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HelperModes {
    /// The argument that selects the give mode.
    pub give: String,
    /// The argument, in the give mode, that separates the directories-only tree from the
    /// trees given whole.
    pub whole: String,
    /// The argument that selects the unpack mode.
    pub unpack: String,
    /// Where the host binary is bound read-only inside a helper: an absolute path whose
    /// parent directory the helper root gets a mount point for.
    pub binary: String,
}

impl Default for HelperModes {
    fn default() -> Self {
        Self {
            give: "__give-dirs".to_owned(),
            whole: "--whole".to_owned(),
            unpack: "__unpack-layers".to_owned(),
            binary: "/js/daemon".to_owned(),
        }
    }
}

/// How many directories the helper root has mount points for (`/t/0` to `/t/7`); more
/// work too, with the mount point made inside the shared root by `crun`.
const BIND_POINTS: usize = 8;

/// The job that gives `whole` trees (every entry) and `dirs` (directories only, if any)
/// to `owner` (`uid:gid` in the helper's namespace), with this daemon's binary `daemon`.
pub(crate) fn give_job(
    modes: &HelperModes,
    daemon: &Path,
    owner: &str,
    dirs: Option<&Path>,
    whole: &[&Path],
) -> Job {
    let mut binds = Vec::new();
    let mut args = vec![modes.binary.clone(), modes.give.clone(), owner.to_owned()];
    args.push(match dirs {
        Some(dir) => {
            binds.push(Bind {
                source: dir.to_owned(),
                writable: true,
            });
            bound_at(0)
        }
        None => "-".to_owned(),
    });
    args.push(modes.whole.clone());
    for tree in whole {
        args.push(bound_at(binds.len()));
        binds.push(Bind {
            source: (*tree).to_owned(),
            writable: true,
        });
    }
    Job {
        binds,
        daemon: Some(daemon.to_owned()),
        bind: modes.binary.clone(),
        args,
    }
}

/// The job that deletes what is below `dir` (not `dir`).
pub(crate) fn remove_job(dir: &Path) -> Job {
    Job {
        binds: vec![Bind {
            source: dir.to_owned(),
            writable: true,
        }],
        daemon: None,
        bind: String::new(),
        args: ["/usr/bin/find", "/t/0", "-mindepth", "1", "-delete"]
            .map(str::to_owned)
            .to_vec(),
    }
}

/// The job that unpacks the layer files `layers` of the directory `blobs` into
/// `stage/rootfs`, with this daemon's binary `daemon`, in a
/// container with a job's user namespace (so the files are owned by the ids the
/// job sees). `stage` is open to the container's root, which makes the root
/// directory in it; `blobs` is read-only.
pub(crate) fn unpack_job(
    modes: &HelperModes,
    daemon: &Path,
    stage: &Path,
    blobs: &Path,
    layers: &[String],
) -> Job {
    let mut args = vec![
        modes.binary.clone(),
        modes.unpack.clone(),
        bound_at(0),
        bound_at(1),
    ];
    args.extend(layers.iter().cloned());
    Job {
        binds: vec![
            Bind {
                source: stage.to_owned(),
                writable: true,
            },
            Bind {
                source: blobs.to_owned(),
                writable: false,
            },
        ],
        daemon: Some(daemon.to_owned()),
        bind: modes.binary.clone(),
        args,
    }
}

/// A directory bound into a helper.
pub(crate) struct Bind {
    /// The host directory.
    pub source: PathBuf,
    /// Whether the helper may write it.
    pub writable: bool,
}

/// What a helper runs: the directories it works on and the command.
pub(crate) struct Job {
    pub binds: Vec<Bind>,
    /// The daemon's binary, bound read-only at [`Job::bind`], if the job runs it.
    pub daemon: Option<PathBuf>,
    /// Where it is bound ([`HelperModes::binary`]); empty if the job runs none.
    pub bind: String,
    pub args: Vec<String>,
}

/// The path `binds[i]` appears at inside a helper.
pub(crate) fn bound_at(index: usize) -> String {
    format!("/t/{index}")
}

/// A read-only or read-write bind of `source` at `destination`.
fn bind_at(source: &Path, destination: &str, writable: bool) -> oci_spec::runtime::Mount {
    let access = if writable { "rw" } else { "ro" };
    bind(source, destination, &["rbind", access, "nosuid"])
}

/// The `config.json` of a helper running `job` over `rootfs` (see [`prepare_rootfs`]),
/// in the user namespace `mapping` names.
pub(crate) fn config(
    rootfs: &Path,
    ids: &DaemonIds,
    mapping: Mapping,
    cgroup: &str,
    job: &Job,
) -> Spec {
    let mut mounts = vec![
        filesystem(
            "/dev",
            "tmpfs",
            &["nosuid", "strictatime", "mode=755", "size=1m"],
        ),
        bind_at(Path::new("/usr"), "/usr", false),
        bind_at(Path::new("/etc/ld.so.cache"), "/etc/ld.so.cache", false),
    ];
    if let Some(daemon) = &job.daemon {
        mounts.push(bind_at(daemon, &job.bind, false));
    }
    for (i, bind) in job.binds.iter().enumerate() {
        mounts.push(bind_at(&bind.source, &bound_at(i), bind.writable));
    }
    let held = held_capabilities();
    let capabilities = built(
        LinuxCapabilitiesBuilder::default()
            .bounding(held.clone())
            .effective(held.clone())
            .permitted(held)
            // Left out, a set is `oci-spec`'s default (AUDIT_WRITE, KILL, NET_BIND_SERVICE):
            // an ambient capability the permitted set lacks, which `capset` refuses.
            .inheritable(HashSet::new())
            .ambient(HashSet::new())
            .build(),
    );
    let process = built(
        ProcessBuilder::default()
            .terminal(false)
            .user(built(UserBuilder::default().uid(0u32).gid(0u32).build()))
            .args(job.args.clone())
            .env(vec!["PATH=/usr/bin:/usr/sbin:/bin:/sbin".to_owned()])
            .cwd(PathBuf::from("/"))
            .capabilities(capabilities)
            .no_new_privileges(true)
            .build(),
    );
    let (uids, gids) = ids.mappings(mapping);
    let namespaces = [LinuxNamespaceType::User, LinuxNamespaceType::Mount]
        .map(|typ| built(LinuxNamespaceBuilder::default().typ(typ).build()));
    let mut linux = built(
        LinuxBuilder::default()
            .uid_mappings(uids)
            .gid_mappings(gids)
            .namespaces(namespaces.to_vec())
            .cgroups_path(PathBuf::from(cgroup))
            .masked_paths(Vec::<String>::new())
            .readonly_paths(Vec::<String>::new())
            .build(),
    );
    // `oci-spec` fills what a builder leaves out with its own defaults (a resources
    // section, a hostname, annotations); a helper has no cgroup, no UTS namespace to
    // set a hostname in, and nothing to mask.
    linux.set_resources(None);
    let root = built(RootBuilder::default().path(rootfs).readonly(false).build());
    let mut spec = built(
        SpecBuilder::default()
            .version("1.0.0")
            .root(root)
            .process(process)
            .mounts(mounts)
            .linux(linux)
            .build(),
    );
    spec.set_hostname(None);
    spec.set_annotations(None);
    spec
}

/// The links the helper root holds so the programs and libraries of a merged-`/usr`
/// host are where their paths say (`/bin/sh`-style paths and the loader's): each into
/// the read-only bind of the host's `/usr`. A host whose `/bin` or `/lib` is a real
/// directory is not supported.
const USR_LINKS: [(&str, &str); 4] = [
    ("bin", "usr/bin"),
    ("sbin", "usr/sbin"),
    ("lib", "usr/lib"),
    ("lib64", "usr/lib64"),
];

/// Makes the helper root `dir`: the mount points of [`config`] and the links of
/// [`USR_LINKS`]. Safe to call again and from several leases at once (a mount point
/// or link that exists is left as it is).
///
/// # Errors
/// A directory or link cannot be made.
pub(crate) fn prepare_rootfs(dir: &Path, binary: &str) -> std::io::Result<()> {
    let binary = binary.trim_start_matches('/');
    let binary_dir = Path::new(binary).parent().unwrap_or(Path::new(""));
    for point in [
        Path::new("dev"),
        Path::new("usr"),
        Path::new("etc"),
        binary_dir,
    ] {
        std::fs::create_dir_all(dir.join(point))?;
    }
    for i in 0..BIND_POINTS {
        std::fs::create_dir_all(dir.join(bound_at(i).trim_start_matches('/')))?;
    }
    for file in ["etc/ld.so.cache", binary] {
        std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(dir.join(file))?;
    }
    for (name, target) in USR_LINKS {
        match std::os::unix::fs::symlink(target, dir.join(name)) {
            Err(e) if e.kind() != std::io::ErrorKind::AlreadyExists => return Err(e),
            _ => {}
        }
    }
    Ok(())
}

/// Copies the daemon's binary to `<scratch>/oci-bin/daemon` (mode 0755, replaced
/// atomically) and returns that path. A helper with a job's ids cannot search the
/// directories the binary was installed or built under (a home directory is 0750), but the
/// scratch directory was checked to be searchable by others.
pub(crate) fn install_daemon(scratch: &Path, daemon: &Path) -> std::io::Result<PathBuf> {
    install_daemon_settling(scratch, daemon, &super::install::settle)
}

/// [`install_daemon`], with `settle` the wait for the staged file to be startable.
fn install_daemon_settling(
    scratch: &Path,
    daemon: &Path,
    settle: &dyn Fn(&Path) -> std::io::Result<()>,
) -> std::io::Result<PathBuf> {
    let _ = settle;
    use std::os::unix::fs::PermissionsExt as _;
    let dir = scratch.join("oci-bin");
    std::fs::create_dir_all(&dir)?;
    std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o755))?;
    let staged = dir.join(format!(".daemon-{}", std::process::id()));
    std::fs::copy(daemon, &staged)?;
    std::fs::set_permissions(&staged, std::fs::Permissions::from_mode(0o755))?;
    let target = dir.join("daemon");
    std::fs::rename(&staged, &target)?;
    Ok(target)
}

#[cfg(test)]
mod install_tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt as _;

    /// Catches a daemon published while something still has it open for writing, one
    /// asked about before it is a whole copy with its mode, a failed wait replacing the
    /// daemon already installed, and a staged file left behind; and the copy a start
    /// that waits returns.
    #[test]
    fn a_daemon_that_stays_busy_is_not_published() {
        let dir = crate::unit_dir("oci-daemon-install");
        let source = dir.join("source");
        std::fs::write(&source, b"daemon").expect("source");
        let scratch = dir.join("s");
        let target = install_daemon_settling(&scratch, &source, &|_| Ok(())).expect("install");
        assert_eq!(target, scratch.join("oci-bin/daemon"));
        std::fs::write(&source, b"newer").expect("source");
        let asked = std::cell::RefCell::new(Vec::new());
        let error = install_daemon_settling(&scratch, &source, &|staged| {
            asked.borrow_mut().push((
                staged.to_owned(),
                std::fs::read(staged).expect("staged"),
                staged.metadata().expect("meta").permissions().mode() & 0o777,
            ));
            Err(std::io::Error::from(std::io::ErrorKind::ExecutableFileBusy))
        })
        .expect_err("busy");
        assert_eq!(error.kind(), std::io::ErrorKind::ExecutableFileBusy);
        let staged = scratch.join(format!("oci-bin/.daemon-{}", std::process::id()));
        assert_eq!(*asked.borrow(), [(staged, b"newer".to_vec(), 0o755)]);
        assert_eq!(std::fs::read(&target).expect("read"), b"daemon");
        let names: Vec<_> = std::fs::read_dir(scratch.join("oci-bin"))
            .expect("dir")
            .map(|e| e.expect("entry").file_name())
            .collect();
        assert_eq!(names, ["daemon"], "no staged file left");
        // The real wait, on a file nothing holds open.
        install_daemon(&scratch, &source).expect("settles");
        assert_eq!(std::fs::read(&target).expect("read"), b"newer");
    }
}
