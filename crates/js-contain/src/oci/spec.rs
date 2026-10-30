//! `config.json` for one job's container, built with `oci-spec`'s typed builders.
//!
//! Each isolation setting of a job's container has its line here (the table is in
//! [`action_config`]'s documentation); the capability set, masked paths and seccomp
//! filter are [`hardening`](super::hardening).

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

use oci_spec::OciSpecError;
use oci_spec::runtime::{
    LinuxBuilder, LinuxCapabilitiesBuilder, LinuxNamespace, LinuxNamespaceBuilder,
    LinuxNamespaceType, LinuxPidsBuilder, LinuxResourcesBuilder, Mount, MountBuilder, PosixRlimit,
    PosixRlimitBuilder, PosixRlimitType, ProcessBuilder, RootBuilder, Spec, SpecBuilder,
    UserBuilder,
};

use super::hardening::{MASKED_PATHS, READONLY_PATHS, SeccompError, seccomp};
use crate::container::{ACTION_ID, ContainerSpec};
use crate::subids::DaemonIds;

/// The OCI version the file declares (what `crun spec` writes).
const OCI_VERSION: &str = "1.0.0";

/// The hostname of every job's container, the same on every node.
pub(crate) const HOSTNAME: &str = "localhost";

/// The lease directories a job's container writes to, and the destination of each:
/// disk-backed, owned by the container's root, discarded with the lease (the home
/// directory). The image's root is read-only; these and the tmpfs mounts of [`TMPFS`] are
/// the places a job writes besides the exec root.
pub(crate) const WRITABLE: [(&str, &str); 1] = [("home", "/root")];

/// The places a job gets a size-limited, `noexec` tmpfs, as podman made them.
const TMPFS: [&str; 3] = ["/tmp", "/var/tmp", "/run"];

/// The directory of the lease holding [`WRITABLE`]'s directories.
pub(crate) const WRITABLE_DIR: &str = "rw";

/// The mount options of the exec-root overlay, pinned: no metacopy, redirects only
/// recorded and never followed (`nofollow`), no index or xino (a stub or redirect placed
/// in the upper layer cannot change what collection reads), and `userxattr` because the
/// overlay is mounted inside a user namespace. The kernel refuses `userxattr` together
/// with `redirect_dir=off` or `on` ("conflicting options"): `nofollow` is the one
/// redirect mode `userxattr` takes, and what it implies.
pub(crate) const OVERLAY_OPTIONS: &str =
    "userxattr,metacopy=off,redirect_dir=nofollow,index=off,xino=off";

/// Why a configuration could not be built.
#[derive(Debug, thiserror::Error)]
pub enum SpecError {
    /// `exec_root` or the init's path is not an absolute, normalized path (or the init's
    /// is not inside `/dev`).
    #[error("{what} {path:?} is not an absolute path without empty, `.` or `..` parts")]
    Path {
        /// Which setting.
        what: &'static str,
        /// Its value.
        path: String,
    },
    /// The seccomp profile could not be built.
    #[error(transparent)]
    Seccomp(#[from] SeccompError),
    /// The configuration could not be written as JSON.
    #[error("config.json: {0}")]
    Json(#[from] serde_json::Error),
}

/// Checks that `path` starts with `under` and continues with parts that are not empty,
/// `.` or `..` (so it is not `under` alone, and has no trailing slash): a mount destination
/// or working directory built from it cannot leave the place it names.
fn normalized(what: &'static str, path: &str, under: &str) -> Result<(), SpecError> {
    let ok = path
        .strip_prefix(under)
        .is_some_and(|rest| rest.split('/').all(|p| !matches!(p, "" | "." | "..")));
    if ok {
        Ok(())
    } else {
        Err(SpecError::Path {
            what,
            path: path.to_owned(),
        })
    }
}

/// What a builder of `oci-spec` makes. Every type here has a default for each field, so
/// `build()` cannot fail; the default is only what a failure would give.
pub(crate) fn built<T: Default>(made: Result<T, OciSpecError>) -> T {
    made.unwrap_or_default()
}

pub(crate) fn strings(options: &[&str]) -> Vec<String> {
    options.iter().map(|s| (*s).to_owned()).collect()
}

/// A bind mount of `source` at `destination`.
pub(crate) fn bind(source: &Path, destination: &str, options: &[&str]) -> Mount {
    built(
        MountBuilder::default()
            .destination(destination)
            .typ("bind")
            .source(source)
            .options(strings(options))
            .build(),
    )
}

/// A mount of the filesystem `typ` at `destination`.
pub(crate) fn filesystem(destination: &str, typ: &str, options: &[&str]) -> Mount {
    built(
        MountBuilder::default()
            .destination(destination)
            .typ(typ)
            .source(typ)
            .options(strings(options))
            .build(),
    )
}

/// Where a lease's container is made and what it sees of the node.
pub(crate) struct Placement<'a> {
    /// The unpacked image the container's root is.
    pub rootfs: &'a Path,
    /// The lease's cgroup, as the cgroup filesystem names it (`/.../lease-<n>`);
    /// the container's own cgroup is a child of it.
    pub cgroup: &'a str,
    /// The daemon user's ids and the subordinate ranges the container maps to.
    pub ids: &'a DaemonIds,
    /// Go's name for the node's architecture, for the seccomp profile.
    pub arch: &'a str,
    /// The container's first process (`init`), on the node.
    pub init: &'a Path,
    /// Where the container sees the init: an absolute path inside its `/dev`.
    pub init_dest: &'a str,
}

/// The job's tmpfs at `destination` (`/tmp`, `/var/tmp`, `/run`), of `size_mib`, that holds no program (`noexec`), no set-uid
/// file and no device node, and that anyone may write to (`mode=1777`). It counts against
/// the lease's memory, as the engine this backend replaces made it
/// (`--tmpfs <dir>:rw,noexec,nosuid,nodev,size=64m`).
fn tmp(destination: &str, size_mib: u64) -> Mount {
    let size = format!("size={size_mib}m");
    filesystem(
        destination,
        "tmpfs",
        &["nosuid", "nodev", "mode=1777", &size],
    )
}

/// The mounts of a job: the standard ones of `crun spec --rootless`, the `hosts`, `hostname` and `resolv.conf` files,
/// the exec-root overlay, the shared trees read-only inside it, `/tmp`, [`WRITABLE`], and
/// the host mounts last (each a recursive bind, `nosuid` and `nodev`, read-only or
/// read-write as asked).
fn mounts(spec: &ContainerSpec, init: &Path, init_dest: &str) -> Vec<Mount> {
    let shm = format!("size={}m", spec.limits.shm_mib);
    let open = ["rbind", "ro", "nosuid", "noexec", "nodev"];
    let mut mounts = vec![
        filesystem("/proc", "proc", &["nosuid", "noexec", "nodev"]),
        filesystem(
            "/dev",
            "tmpfs",
            &["nosuid", "strictatime", "mode=755", "size=65536k"],
        ),
        filesystem(
            "/dev/pts",
            "devpts",
            &[
                "nosuid",
                "noexec",
                "newinstance",
                "ptmxmode=0666",
                "mode=0620",
            ],
        ),
        built(
            MountBuilder::default()
                .destination("/dev/shm")
                .typ("tmpfs")
                .source("shm")
                .options(strings(&["nosuid", "noexec", "nodev", "mode=1777", &shm]))
                .build(),
        ),
        filesystem("/dev/mqueue", "mqueue", &["nosuid", "noexec", "nodev"]),
        filesystem("/sys", "sysfs", &["nosuid", "noexec", "nodev", "ro"]),
        filesystem(
            "/sys/fs/cgroup",
            "cgroup",
            &["nosuid", "noexec", "nodev", "relatime", "ro"],
        ),
        // Into the `/dev` above, a `tmpfs` made for this container: no mount point in the
        // image. Executable (no `noexec`): it is the container's first process.
        bind(init, init_dest, &["rbind", "ro", "nosuid", "nodev"]),
        bind(&spec.dir.join("etc/hosts"), "/etc/hosts", &open),
        bind(&spec.dir.join("etc/hostname"), "/etc/hostname", &open),
        bind(&spec.dir.join("etc/resolv.conf"), "/etc/resolv.conf", &open),
    ];
    let overlay = format!(
        "lowerdir={},upperdir={},workdir={},{OVERLAY_OPTIONS}",
        spec.input_root.display(),
        spec.upper.display(),
        spec.overlay_work.display(),
    );
    mounts.push(built(
        MountBuilder::default()
            .destination(&spec.exec_root)
            .typ("overlay")
            .source("overlay")
            .options(vec![overlay, "nodev".to_owned()])
            .build(),
    ));
    // After the overlay, which they are mounted inside: each read-only, with the flags
    // a child user namespace cannot relax.
    for shared in &spec.shared {
        let destination = format!("{}/{}", spec.exec_root, shared.path);
        mounts.push(bind(
            &shared.source,
            &destination,
            &["rbind", "ro", "nosuid", "nodev"],
        ));
    }
    mounts.extend(TMPFS.map(|dest| tmp(dest, spec.limits.tmp_mib)));
    for (name, destination) in WRITABLE {
        let source = spec.dir.join(WRITABLE_DIR).join(name);
        mounts.push(bind(
            &source,
            destination,
            &["rbind", "rw", "nosuid", "nodev"],
        ));
    }
    mounts.push(bind(Path::new("/tmp"), "/tmp", &["rbind", "ro", "nosuid", "nodev"]));
    for host in &spec.mounts {
        let access = if host.read_only { "ro" } else { "rw" };
        mounts.push(bind(
            &host.source,
            &host.target,
            &["rbind", access, "nosuid", "nodev"],
        ));
    }
    mounts
}

/// The container's `process.args`: the init, then `--` and the job's own argv. The
/// init runs the job as its child and reaps what the job leaves orphaned.
fn init_argv(init_dest: &str, argv: &[String]) -> Vec<String> {
    [init_dest, "--"]
        .into_iter()
        .map(str::to_owned)
        .chain(argv.iter().cloned())
        .collect()
}

fn rlimit(typ: PosixRlimitType, value: u64) -> PosixRlimit {
    built(
        PosixRlimitBuilder::default()
            .typ(typ)
            .hard(value)
            .soft(value)
            .build(),
    )
}

/// The namespaces a container gets, each new (the network namespace has loopback only).
pub(crate) fn namespaces() -> Vec<LinuxNamespace> {
    use LinuxNamespaceType::{Cgroup, Ipc, Mount, Pid, User, Uts};
    [Pid, Ipc, Uts, User, Cgroup, Mount]
        .into_iter()
        .map(|typ| built(LinuxNamespaceBuilder::default().typ(typ).build()))
        .collect()
}

/// The process environment: the Command's variables, then `HOSTNAME` and `HOME` unless
/// it sets them: `HOSTNAME=localhost` and `HOME=/root`, the writable home directory (owned by
/// the job's uid, which the image's user database need not name), the same on every node.
pub(crate) fn environment(env: &[(String, String)]) -> Vec<String> {
    let mut out: Vec<String> = env.iter().map(|(n, v)| format!("{n}={v}")).collect();
    for (name, default) in [("HOSTNAME", HOSTNAME), ("HOME", "/root")] {
        if !env.iter().any(|(n, _)| n == name) {
            out.push(format!("{name}={default}"));
        }
    }
    out
}

/// The `config.json` of the container for `spec`.
///
/// | Isolation                                | `config.json`                                     |
/// |------------------------------------------|---------------------------------------------------|
/// | no network                               | a new network namespace, nothing configured (crun brings `lo` up) |
/// | no id of the daemon user, not root       | `uidMappings`/`gidMappings` container `0..65536` to the daemon user's first subordinate range; `process.user` 1000:1000 |
/// | only the Command's environment           | `process.env` is the Command's variables only      |
/// | the Command's argv and directory         | `process.args`, `process.cwd`                      |
/// | the lease cgroup, `memory.oom.group=1`   | `cgroupsPath` a child of the lease cgroup, `unified` `memory.oom.group`       |
/// | a pids limit                             | `resources.pids.limit`                             |
/// | `nofile`, `nproc` ulimits, no core dump  | `process.rlimits` (`RLIMIT_CORE` 0)                |
/// | no capability                            | the five `process.capabilities` sets, empty; the seccomp filter resolved for none |
/// | no new privileges                        | `process.noNewPrivileges` true                     |
/// | `/dev/shm` size, `/tmp` size, no programs in `/tmp` | the tmpfs `size=`; `/tmp` also `noexec`  |
/// | the run's scratch, `--mount-root`, files | `spec.mounts`: recursive binds, `nosuid`, `nodev`  |
/// | the kernel's default OOM score           | `process.oomScoreAdj`                              |
/// | hostname `localhost`                     | `hostname`                                         |
/// | the input root, writes in `upper`        | an `overlay` mount at `exec_root`                  |
///
/// No memory or CPU limit is set here: the lease cgroup carries them
/// ([`LeasePolicy::Hard`](crate::cgroup::LeasePolicy::Hard)).
///
/// # Errors
/// `spec.exec_root` or the init's path is not a normalized absolute path, or the seccomp
/// profile cannot be resolved for the node's architecture.
pub(crate) fn action_config(
    spec: &ContainerSpec,
    placement: &Placement<'_>,
) -> Result<Spec, SpecError> {
    normalized("exec_root", &spec.exec_root, "/")?;
    normalized("init path", placement.init_dest, "/dev/")?;
    for host in &spec.mounts {
        normalized("mount target", &host.target, "/")?;
    }
    // All five sets empty, written out: `crun` gives a process with no `capabilities`
    // object its own default set.
    let capabilities = built(
        LinuxCapabilitiesBuilder::default()
            .bounding(HashSet::from([oci_spec::runtime::Capability::Chown]))
            .effective(HashSet::from([oci_spec::runtime::Capability::Chown]))
            .permitted(HashSet::from([oci_spec::runtime::Capability::Chown]))
            .inheritable(HashSet::new())
            .ambient(HashSet::new())
            .build(),
    );
    let cwd = if spec.working_directory.is_empty() {
        spec.exec_root.clone()
    } else {
        format!("{}/{}", spec.exec_root, spec.working_directory)
    };
    let process = built(
        ProcessBuilder::default()
            .terminal(false)
            .user(built(
                UserBuilder::default().uid(0u32).gid(0u32).build(),
            ))
            .args(init_argv(placement.init_dest, &spec.argv))
            .env(environment(&spec.env))
            .cwd(PathBuf::from(cwd))
            .capabilities(capabilities)
            .rlimits(vec![
                rlimit(PosixRlimitType::RlimitNofile, spec.limits.nofile),
                rlimit(PosixRlimitType::RlimitNproc, spec.limits.nproc),
                rlimit(PosixRlimitType::RlimitCore, 0),
            ])
            .no_new_privileges(false)
            .oom_score_adj(0)
            .build(),
    );
    let (uids, gids) = placement.ids.action_mappings();
    let pids = built(
        LinuxPidsBuilder::default()
            .limit(i64::try_from(spec.limits.pids).unwrap_or(i64::MAX))
            .build(),
    );
    let unified = HashMap::from([("memory.oom.group".to_owned(), "0".to_owned())]);
    let resources = built(
        LinuxResourcesBuilder::default()
            .unified(unified)
            .build(),
    );
    let linux = built(
        LinuxBuilder::default()
            .uid_mappings(uids)
            .gid_mappings(gids)
            .namespaces(namespaces())
            .cgroups_path(PathBuf::from(format!("{}/ctr", placement.cgroup)))
            .resources(resources)
            .masked_paths(strings(&MASKED_PATHS))
            .readonly_paths(strings(&READONLY_PATHS))
            .build(),
    );
    let root = built(
        RootBuilder::default()
            .path(placement.rootfs)
            .readonly(false)
            .build(),
    );
    Ok(built(
        SpecBuilder::default()
            .version(OCI_VERSION)
            .root(root)
            .hostname(HOSTNAME)
            .process(process)
            .mounts(mounts(spec, placement.init, placement.init_dest))
            .linux(linux)
            .build(),
    ))
}

/// The bytes of `config.json`: the spec as pretty JSON with keys in sorted order and
/// each capability set sorted (`oci-spec` holds them in hash sets, whose order changes
/// from run to run), and a newline: the same bytes for the same spec, so a golden file
/// can pin them.
///
/// # Errors
/// The spec cannot be written as JSON (it can always).
pub(crate) fn render(config: &Spec) -> Result<Vec<u8>, SpecError> {
    let mut value = serde_json::to_value(config)?;
    value["process"]["capabilities"]
        .as_object_mut()
        .into_iter()
        .flat_map(|sets| sets.values_mut())
        .filter_map(|set| set.as_array_mut())
        .for_each(|set| set.sort_by(|a, b| a.as_str().cmp(&b.as_str())));
    let mut bytes = serde_json::to_vec_pretty(&value)?;
    bytes.push(b'\n');
    Ok(bytes)
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use serde_json::{Value, json};

    use super::*;
    use crate::container::SharedMount;
    use crate::container::tests::spec;

    fn ids() -> DaemonIds {
        DaemonIds {
            uid: 990,
            gid: 991,
            sub_uid: 100_000,
            sub_gid: 200_000,
        }
    }

    fn config_of(spec: &ContainerSpec) -> Value {
        let rootfs = PathBuf::from("/scratch/oci-images/aaaa/rootfs");
        let placement = Placement {
            rootfs: &rootfs,
            cgroup: "/js.slice/actions/lease-1-2",
            ids: &ids(),
            arch: "amd64",
            init: Path::new("/scratch/oci-bin/js-init"),
            init_dest: "/dev/js-init",
        };
        let config = action_config(spec, &placement).expect("config");
        serde_json::from_slice(&render(&config).expect("render")).expect("json")
    }

    /// The whole `config.json` of the reference job, as a file: any field added,
    /// dropped or changed (a limit, a mount option, a namespace, the seccomp profile)
    /// is a diff to read (the seccomp entries are counted, not listed). Regenerate by
    /// replacing the file with the actual text the failure prints.
    #[test]
    fn the_config_matches_its_golden_file() {
        let mut config = config_of(&spec());
        // The filter's several hundred entries are pinned by `hardening`'s own tests;
        // the file shows how many there are and everything around them.
        let seccomp = &mut config["linux"]["seccomp"];
        let entries = seccomp["syscalls"].as_array().expect("syscalls").len();
        seccomp["syscalls"] = json!(format!("{entries} entries"));
        let actual = serde_json::to_string_pretty(&config).expect("json") + "\n";
        let golden = include_str!("../../tests/fixtures/oci-action-config.golden.json");
        assert!(
            actual == golden,
            "config.json differs from tests/fixtures/oci-action-config.golden.json; actual:\n{actual}"
        );
    }

    /// Catches `config.json` bytes that change between runs for the same spec: the
    /// capability sets are hash sets in `oci-spec`, so each build orders them
    /// differently unless `render` sorts them.
    #[test]
    fn the_bytes_are_the_same_every_time() {
        let rootfs = PathBuf::from("/scratch/oci-images/aaaa/rootfs");
        let build = || {
            let placement = Placement {
                rootfs: &rootfs,
                cgroup: "/js.slice/actions/lease-1-2",
                ids: &ids(),
                arch: "amd64",
                init: Path::new("/scratch/oci-bin/js-init"),
                init_dest: "/dev/js-init",
            };
            render(&action_config(&spec(), &placement).expect("config")).expect("render")
        };
        let first = build();
        for _ in 0..30 {
            assert_eq!(build(), first);
        }
    }

    /// Catches the job being the container's first process again (nothing then reaps
    /// what it orphans), the init's mount missing, writable or in the image's root (a
    /// mount point written into the image shared by every lease), or one the job
    /// cannot execute: `process.args` is the init, `--`, then the job's argv unchanged;
    /// the init is a read-only bind into `/dev`, after `/dev` is mounted, without `noexec`.
    #[test]
    fn the_action_runs_as_the_child_of_a_reaping_init() {
        let mut spec = spec();
        spec.argv = vec!["/bin/tool".to_owned(), "-c".to_owned(), "true".to_owned()];
        let config = config_of(&spec);
        assert_eq!(
            config["process"]["args"],
            json!(["/dev/js-init", "--", "/bin/tool", "-c", "true"])
        );
        let mounts = config["mounts"].as_array().expect("mounts");
        let at = |dest: &str| mounts.iter().position(|m| m["destination"] == dest);
        let init = at("/dev/js-init").expect("the init is mounted");
        assert!(at("/dev").expect("/dev") < init, "into the /dev tmpfs");
        assert_eq!(mounts[init]["type"], "bind");
        assert_eq!(mounts[init]["source"], "/scratch/oci-bin/js-init");
        let options: Vec<&str> = mounts[init]["options"]
            .as_array()
            .expect("options")
            .iter()
            .filter_map(Value::as_str)
            .collect();
        assert!(options.contains(&"ro"), "{options:?}");
        assert!(!options.contains(&"noexec"), "{options:?}");
    }

    /// The config of `spec` with the init at `init_dest`.
    fn config_with(spec: &ContainerSpec, init_dest: &str) -> Result<Value, SpecError> {
        let rootfs = PathBuf::from("/scratch/oci-images/aaaa/rootfs");
        let placement = Placement {
            rootfs: &rootfs,
            cgroup: "/js.slice/actions/lease-1-2",
            ids: &ids(),
            arch: "amd64",
            init: Path::new("/scratch/oci-bin/js-init"),
            init_dest,
        };
        let config = action_config(spec, &placement)?;
        Ok(serde_json::from_slice(&render(&config).expect("render")).expect("json"))
    }

    /// Catches the names a caller configures not reaching the config, or the defaults
    /// leaking in beside them: with another exec root and init path, the overlay, the
    /// shared mounts, the working directory and the init's mount and `process.args` carry
    /// exactly those names and none of the defaults.
    #[test]
    fn configured_names_are_the_only_names_in_the_config() {
        let mut spec = spec();
        spec.exec_root = "/other/root".to_owned();
        spec.shared = vec![SharedMount {
            path: "lib".to_owned(),
            source: PathBuf::from("/scratch/trees/bb"),
        }];
        let config = config_with(&spec, "/dev/other-init").expect("config");
        let text = config.to_string();
        assert!(!text.contains("/job/root"), "{text}");
        assert!(!text.contains("/dev/js-init"), "{text}");
        assert_eq!(config["process"]["cwd"], "/other/root/pkg");
        assert_eq!(config["process"]["args"][0], "/dev/other-init");
        let mounts = config["mounts"].as_array().expect("mounts");
        let dests: Vec<&str> = mounts
            .iter()
            .filter_map(|m| m["destination"].as_str())
            .collect();
        for want in ["/other/root", "/other/root/lib", "/dev/other-init"] {
            assert!(dests.contains(&want), "{want} not in {dests:?}");
        }
    }

    /// Catches an exec root or init path that could name another place than it says
    /// (relative, `/`, empty, `.` or `..` parts, a trailing slash), or an init outside
    /// `/dev`: each is refused with the setting's name, and a good one is accepted.
    #[test]
    fn exec_root_and_init_path_must_be_normalized_absolute_paths() {
        for bad in ["", "/", "root", "/a/../b", "/a/./b", "/a//b", "/a/", "/.."] {
            let mut spec = spec();
            spec.exec_root = bad.to_owned();
            let why = config_with(&spec, "/dev/js-init")
                .expect_err(bad)
                .to_string();
            assert!(why.starts_with("exec_root "), "{bad}: {why}");
        }
        for bad in ["/dev/../x", "js-init", "/tmp/init", "/dev/", "/dev"] {
            let why = config_with(&spec(), bad).expect_err(bad).to_string();
            assert!(why.starts_with("init path "), "{bad}: {why}");
        }
        config_with(&spec(), "/dev/a/b-init").expect("nested under /dev");
    }

    /// Catches a working directory that is not the exec root when the job names none (a
    /// trailing slash, or a directory the input root does not have), and one that is not
    /// under the exec root when it names one.
    #[test]
    fn the_working_directory_is_the_exec_root_or_below_it() {
        let mut spec = spec();
        spec.working_directory = String::new();
        assert_eq!(config_of(&spec)["process"]["cwd"], "/job/root");
        spec.working_directory = "pkg/sub".to_owned();
        assert_eq!(config_of(&spec)["process"]["cwd"], "/job/root/pkg/sub");
    }

    /// Catches the job running as root (a mode-000 file would be readable, a
    /// directory it does not own writable), and as an id its namespace does not map
    /// (the container would not start): the process is uid and gid 1000, and the
    /// mappings that hold id 1000 are the only ones, from container id 0.
    #[test]
    fn the_action_runs_as_an_unprivileged_id_its_namespace_maps() {
        let config = config_of(&spec());
        assert_eq!(config["process"]["user"], json!({"uid": 1000, "gid": 1000}));
        for (key, host) in [("uidMappings", 100_000), ("gidMappings", 200_000)] {
            let maps = config["linux"][key].as_array().expect(key);
            assert_eq!(maps.len(), 1, "{key}");
            assert_eq!(maps[0]["containerID"], 0);
            assert_eq!(maps[0]["hostID"], host);
            assert!(maps[0]["size"].as_u64().expect("size") > 1000, "{key}");
        }
    }

    /// Catches a job container without its own `hosts`, `hostname` or `resolv.conf`
    /// (the image's, or none, would answer a lookup), or one of them writable: each is a
    /// read-only bind of the file the lease wrote under its `etc/`.
    #[test]
    fn the_name_files_are_read_only_binds_of_the_leases_own() {
        let config = config_of(&spec());
        let mounts = config["mounts"].as_array().expect("mounts");
        for name in ["hosts", "hostname", "resolv.conf"] {
            let dest = format!("/etc/{name}");
            let found = mounts.iter().find(|m| m["destination"] == dest);
            assert!(found.is_some(), "no mount of {dest}");
            let mount = found.expect("checked");
            assert_eq!(mount["type"], "bind");
            assert_eq!(mount["source"], format!("/scratch/lease-1-2/etc/{name}"));
            let options = mount["options"].as_array().expect("options");
            assert!(options.iter().any(|o| o == "ro"), "{dest}: {options:?}");
        }
    }

    /// Catches a shared tree mounted writable, mounted outside the exec root, or mounted
    /// before the overlay it sits in (the overlay would hide it).
    #[test]
    fn shared_mounts_are_read_only_inside_the_overlay() {
        let mut spec = spec();
        spec.shared = vec![
            SharedMount {
                source: PathBuf::from("/scratch/trees/aa"),
                path: "external/toolchain".to_owned(),
            },
            SharedMount {
                source: PathBuf::from("/scratch/trees/bb"),
                path: "lib".to_owned(),
            },
        ];
        let config = config_of(&spec);
        let mounts = config["mounts"].as_array().expect("mounts");
        let at = |dest: &str| mounts.iter().position(|m| m["destination"] == dest);
        let overlay = at("/job/root").expect("overlay");
        for (dest, source) in [
            ("/job/root/external/toolchain", "/scratch/trees/aa"),
            ("/job/root/lib", "/scratch/trees/bb"),
        ] {
            let position = at(dest).expect("a mount of the shared tree");
            assert!(overlay < position, "{dest} before the overlay");
            let mount = &mounts[position];
            assert_eq!(mount["source"], source);
            assert_eq!(mount["type"], "bind");
            let options: Vec<_> = mount["options"]
                .as_array()
                .expect("options")
                .iter()
                .map(|o| o.as_str().expect("str"))
                .collect();
            assert!(options.contains(&"ro"), "{dest}: {options:?}");
            assert!(options.contains(&"nosuid"));
            assert!(options.contains(&"nodev"));
            assert!(!options.contains(&"rw"), "{dest}: {options:?}");
        }
        // No shared mount, no extra mount: the reference config has none.
        assert!(
            config_of(&self::spec())["mounts"]
                .as_array()
                .expect("mounts")
                .iter()
                .all(|m| !m["destination"].as_str().unwrap().starts_with("/job/root/"))
        );
    }

    /// Catches the Command's variables being reordered or dropped, and the two defaults
    /// being added when the Command sets them (or not added when it does not).
    #[test]
    fn the_environment_is_the_commands_then_the_two_defaults() {
        let var = |n: &str, v: &str| (n.to_owned(), v.to_owned());
        assert_eq!(environment(&[]), ["HOSTNAME=localhost", "HOME=/root"]);
        assert_eq!(
            environment(&[var("B", "2"), var("A", "1")]),
            ["B=2", "A=1", "HOSTNAME=localhost", "HOME=/root"]
        );
        assert_eq!(
            environment(&[var("HOME", "/w")]),
            ["HOME=/w", "HOSTNAME=localhost"]
        );
        assert_eq!(
            environment(&[var("HOSTNAME", "h"), var("HOME", "/w")]),
            ["HOSTNAME=h", "HOME=/w"]
        );
    }

    /// Catches the daemon user's id mapped into the container (any host id of a mapping
    /// equal to or spanning it), the mapping shrinking below a full 16-bit range, and
    /// the group mapping using the user range.
    #[test]
    fn the_container_maps_the_subordinate_range_only() {
        let ids = ids();
        let (uids, gids) = ids.action_mappings();
        let spans = |maps: &[oci_spec::runtime::LinuxIdMapping]| {
            maps.iter()
                .map(|m| (m.container_id(), m.host_id(), m.size()))
                .collect::<Vec<_>>()
        };
        assert_eq!(spans(&uids), [(0, 100_000, 65_536)]);
        assert_eq!(spans(&gids), [(0, 200_000, 65_536)]);
        // A helper's namespace maps the daemon user as its root and the range after it.
        let (uids, gids) = ids.setup_mappings();
        assert_eq!(spans(&uids), [(0, 990, 1), (1, 100_000, 65_536)]);
        assert_eq!(spans(&gids), [(0, 991, 1), (1, 200_000, 65_536)]);
    }

    /// Catches the container's cgroup not being below the lease's (its limits would not
    /// apply) and the memory-or-CPU limits being set on the container instead of the lease.
    #[test]
    fn the_cgroup_is_a_child_of_the_lease_with_no_memory_or_cpu_limit() {
        let config = config_of(&spec());
        assert_eq!(
            config["linux"]["cgroupsPath"],
            "/js.slice/actions/lease-1-2/ctr"
        );
        let resources = &config["linux"]["resources"];
        assert_eq!(resources["pids"], json!({"limit": 101}));
        assert!(resources.get("memory").is_none());
        assert!(resources.get("cpu").is_none());
    }
}
