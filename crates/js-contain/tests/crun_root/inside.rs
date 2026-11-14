//! What the job sees of its own container, read by programs run inside it.

use crate::cell::Cell;

/// One line of `/proc/self/mountinfo`, as the job reads it.
struct MountInfo {
    point: String,
    options: Vec<String>,
    fstype: String,
    super_options: Vec<String>,
}

impl MountInfo {
    fn has(&self, option: &str) -> bool {
        self.options.iter().any(|o| o == option)
    }
}

/// The mounts in `text`, the content of a `/proc/<pid>/mountinfo`: the fields before the
/// ` - ` separator are the mount's (id, parent, device, root, mount point, options, optional
/// fields), the ones after it the filesystem's (type, source, options).
fn mounts(text: &str) -> Vec<MountInfo> {
    text.lines()
        .filter_map(|line| {
            let (mount, filesystem) = line.split_once(" - ")?;
            let mount: Vec<&str> = mount.split_whitespace().collect();
            let filesystem: Vec<&str> = filesystem.split_whitespace().collect();
            let (point, options) = (mount.get(4)?, mount.get(5)?);
            let (fstype, super_options) = (filesystem.first()?, filesystem.get(2)?);
            Some(MountInfo {
                point: (*point).to_owned(),
                options: options.split(',').map(str::to_owned).collect(),
                fstype: (*fstype).to_owned(),
                super_options: super_options.split(',').map(str::to_owned).collect(),
            })
        })
        .collect()
}

/// The mount at `point` in the mountinfo `text`, which must have one.
fn mount_at(text: &str, point: &str) -> MountInfo {
    mounts(text)
        .into_iter()
        .find(|m| m.point == point)
        .unwrap_or_else(|| panic!("no mount at {point} in:\n{text}"))
}

/// The lines of `out` from the line `marker` on, the marker excluded.
fn after<'a>(out: &'a str, marker: &str) -> Vec<&'a str> {
    out.lines().skip_while(|l| *l != marker).skip(1).collect()
}

/// Proves the container's ids are the job user's subordinate range and its root maps to no
/// real root (the maps are read from inside, where the kernel answers for the job's own
/// namespace): id 0 of the namespace is the first id of the job user's range for the full
/// 65536, the job runs as uid and gid 0 of it as in a Docker container, and a file the
/// host's root owns (the image's busybox) is owned by nobody, since no id of the host's root
/// is mapped.
#[tokio::test]
#[ignore = "real crun, root and a job user: run through cargo xtask crun-tests --job-user"]
async fn the_job_is_container_root_in_a_namespace_mapped_onto_the_job_users_subordinate_range() {
    let cell = Cell::new("ids");
    let run = cell
        .sh(
            "ids",
            "cat /proc/self/uid_map; echo --; cat /proc/self/gid_map; echo --; id -u; id -g; \
             echo --; ls -ln /bin/busybox",
        )
        .await;
    let out = run.out();
    let parts: Vec<Vec<&str>> = out.split("--\n").map(|p| p.lines().collect()).collect();
    let words = |part: &Vec<&str>| -> Vec<String> {
        part.first()
            .map(|l| l.split_whitespace().map(str::to_owned).collect())
            .unwrap_or_default()
    };
    let (sub_uid, sub_gid) = (cell.ids.sub_uid.to_string(), cell.ids.sub_gid.to_string());
    assert_eq!(
        words(&parts[0]),
        ["0", sub_uid.as_str(), "65536"],
        "{}",
        run.said
    );
    assert_eq!(
        words(&parts[1]),
        ["0", sub_gid.as_str(), "65536"],
        "{}",
        run.said
    );
    assert_eq!(parts[2], ["0", "0"], "{}", run.said);
    // `-rwxr-xr-x 1 <uid> <gid> ...`: the owner of a file of the host's root.
    let owner = words(&parts[3]);
    assert_eq!(owner[2..4], ["65534", "65534"], "{}", run.said);
}

/// The capabilities of Docker's default set as the kernel prints them: CHOWN, DAC_OVERRIDE,
/// FOWNER, FSETID, KILL, SETGID, SETUID, SETPCAP, NET_BIND_SERVICE, NET_RAW, SYS_CHROOT,
/// MKNOD, AUDIT_WRITE and SETFCAP.
const DOCKER_CAPS: &str = "00000000a80425fb";

/// Proves the job holds Docker's default capabilities and no more, can still not gain
/// privileges, and may not dump core: the bounding, permitted and effective sets are
/// [`DOCKER_CAPS`], the inheritable and ambient sets are empty, `NoNewPrivs` is 1 and the core
/// file size limit is 0.
#[tokio::test]
#[ignore = "real crun, root and a job user: run through cargo xtask crun-tests --job-user"]
async fn the_job_holds_dockers_default_capabilities_and_cannot_gain_privileges() {
    let cell = Cell::new("caps");
    let run = cell
        .sh(
            "caps",
            "grep -E '^(CapInh|CapPrm|CapEff|CapBnd|CapAmb|NoNewPrivs):' /proc/self/status; \
             echo core=$(ulimit -c)",
        )
        .await;
    let out = run.out();
    let caps: Vec<&str> = out.lines().filter(|l| l.starts_with("Cap")).collect();
    assert_eq!(caps.len(), 5, "{}", run.said);
    for line in caps {
        let (name, value) = line
            .split_once(':')
            .map_or(("", ""), |(n, v)| (n, v.trim()));
        let want = match name {
            "CapInh" | "CapAmb" => "0000000000000000",
            _ => DOCKER_CAPS,
        };
        assert_eq!(value, want, "{line}: {}", run.said);
    }
    let no_new_privs = out.lines().find(|l| l.starts_with("NoNewPrivs:"));
    assert_eq!(
        no_new_privs
            .and_then(|l| l.split_once(':'))
            .map(|(_, v)| v.trim()),
        Some("1"),
        "{}",
        run.said
    );
    assert!(out.lines().any(|l| l == "core=0"), "{}", run.said);
}

/// Proves root of the job's namespace is still no more than the namespace's: it cannot mount
/// (no `CAP_SYS_ADMIN`), make a real device node (`mknod` of a block device, though it holds
/// `CAP_MKNOD`), change the host's clock or reach a file whose owner the namespace does not
/// map (the image's busybox, owned by the host's root, reads as 65534: `chown` of it is
/// refused though the job holds `CAP_CHOWN`, and `chmod` too, though it holds `CAP_FOWNER`).
#[tokio::test]
#[ignore = "real crun, root and a job user: run through cargo xtask crun-tests --job-user"]
async fn root_of_the_namespace_still_cannot_do_what_the_isolation_forbids() {
    let cell = Cell::new("forbidden");
    let run = cell
        .sh(
            "forbidden",
            "exec 2>&1; mount -t tmpfs none /tmp; echo mount=$?; mknod /tmp/blk b 7 0; \
             echo mknod=$?; mknod /tmp/fifo p; echo mkfifo=$?; date -s '2001-01-01'; \
             echo clock=$?; chown 5 /bin/busybox; echo chown=$?; chmod 000 /bin/busybox; \
             echo chmod=$?; /bin/busybox true; echo still=$?",
        )
        .await;
    let out = run.out();
    for (step, refused) in [
        ("mount", true),
        ("mknod", true),
        ("mkfifo", false),
        ("clock", true),
        ("chown", true),
        ("chmod", true),
    ] {
        let line = out
            .lines()
            .find_map(|l| l.strip_prefix(step)?.strip_prefix('='))
            .unwrap_or("missing");
        assert_eq!(line != "0", refused, "{step}={line}: {}", run.said);
    }
    assert!(out.lines().any(|l| l == "still=0"), "{}", run.said);
}

/// Proves everything the job writes belongs to the container's root, as in Docker: the
/// exec root's overlay upper (`/work`), `/root`, and the tmpfs mounts (`/tmp`, `/var/tmp`,
/// `/run`) are uid and gid 0 inside, and the job makes a file in each and the file is its
/// own.
#[tokio::test]
#[ignore = "real crun, root and a job user: run through cargo xtask crun-tests --job-user"]
async fn what_the_job_writes_belongs_to_the_containers_root() {
    let cell = Cell::new("owners");
    let run = cell
        .sh(
            "owners",
            "stat -c '%n %u:%g' /work /root /tmp /var/tmp /run; \
             for d in /work /root /tmp /var/tmp /run; do echo x > $d/f && \
             stat -c '%n %u:%g' $d/f; done",
        )
        .await;
    let out = run.out();
    for line in [
        "/work 0:0",
        "/root 0:0",
        "/tmp 0:0",
        "/var/tmp 0:0",
        "/run 0:0",
        "/work/f 0:0",
        "/root/f 0:0",
        "/tmp/f 0:0",
        "/var/tmp/f 0:0",
        "/run/f 0:0",
    ] {
        assert!(out.lines().any(|l| l == line), "{line}: {}", run.said);
    }
}

/// Catches a job running without the seccomp filter: the kernel reports mode 2 (filter) for
/// the job's own process, with at least one filter attached (`Seccomp_filters`).
///
/// What the filter refuses is not read here. The pinned profile allows `unshare`, and a job
/// with no capability makes a user namespace of its own when the kernel and the profile of
/// `crun` allow it (a hosted runner does: `unshare -U` succeeds in the job, which an inner
/// namespace for a job's own network, made by the job itself, needs). Calls the profile
/// refuses and the kernel would allow have no busybox applet to make them.
#[tokio::test]
#[ignore = "real crun, root and a job user: run through cargo xtask crun-tests --job-user"]
async fn the_job_runs_under_a_seccomp_filter() {
    let cell = Cell::new("seccomp");
    let run = cell
        .sh("seccomp", "grep '^Seccomp' /proc/self/status")
        .await;
    let out = run.out();
    let field = |name: &str| {
        out.lines()
            .find_map(|l| l.strip_prefix(name)?.strip_prefix(':'))
            .map(|v| v.trim().to_owned())
    };
    assert_eq!(field("Seccomp").as_deref(), Some("2"), "{}", run.said);
    let filters = field("Seccomp_filters").and_then(|n| n.parse::<u32>().ok());
    assert!(filters.is_some_and(|n| n >= 1), "{filters:?}: {}", run.said);
}

/// Catches an image root a job can write: a file made at `/` fails with "Read-only file
/// system", and the root is mounted `ro` as the job's mount table lists it.
#[tokio::test]
#[ignore = "real crun, root and a job user: run through cargo xtask crun-tests --job-user"]
async fn the_root_is_read_only() {
    let cell = Cell::new("erofs");
    let run = cell
        .sh(
            "erofs",
            "touch /new-file 2>&1; echo touch=$?; mkdir /new-dir 2>&1; echo mkdir=$?; \
             echo --; cat /proc/self/mountinfo",
        )
        .await;
    let out = run.out();
    assert_eq!(
        out.matches("Read-only file system").count(),
        2,
        "{}",
        run.said
    );
    assert!(
        !out.contains("touch=0") && !out.contains("mkdir=0"),
        "{}",
        run.said
    );
    let root = mount_at(&after(out, "--").join("\n"), "/");
    assert!(root.has("ro"), "{:?}: {}", root.options, run.said);
}

/// Catches a `/tmp` a job cannot write, one that runs programs, and one with set-uid files
/// or devices: the job writes and reads back a file there, copies a program there and is
/// refused when it runs it ("Permission denied"; the same program runs from the image's
/// `/bin`), and the mount table lists `/tmp` as a tmpfs with `noexec`, `nosuid` and `nodev`.
#[tokio::test]
#[ignore = "real crun, root and a job user: run through cargo xtask crun-tests --job-user"]
async fn tmp_is_writable_and_runs_no_program() {
    let cell = Cell::new("tmp");
    let run = cell
        .sh(
            "tmp",
            "echo hello > /tmp/f; cat /tmp/f; cp /bin/busybox /tmp/bb; chmod 755 /tmp/bb; \
             /tmp/bb true 2>&1; echo exec=$?; /bin/busybox true; echo control=$?; \
             echo --; cat /proc/self/mountinfo",
        )
        .await;
    let out = run.out();
    assert!(out.lines().any(|l| l == "hello"), "{}", run.said);
    assert!(out.contains("Permission denied"), "{}", run.said);
    assert!(
        out.lines().any(|l| l.starts_with("exec=") && l != "exec=0"),
        "{}",
        run.said
    );
    assert!(out.lines().any(|l| l == "control=0"), "{}", run.said);
    let tmp = mount_at(&after(out, "--").join("\n"), "/tmp");
    assert_eq!(tmp.fstype, "tmpfs", "{}", run.said);
    for option in ["noexec", "nosuid", "nodev"] {
        assert!(
            tmp.has(option),
            "{option} in {:?}: {}",
            tmp.options,
            run.said
        );
    }
}

/// Catches a job given a network other than loopback: the interfaces the kernel lists for
/// the job's own network namespace, by `/proc/net/dev` and by the `/sys` of that namespace,
/// are `lo` and nothing else.
#[tokio::test]
#[ignore = "real crun, root and a job user: run through cargo xtask crun-tests --job-user"]
async fn the_network_namespace_has_only_loopback() {
    let cell = Cell::new("net");
    let run = cell
        .sh("net", "cat /proc/net/dev; echo --; ls /sys/class/net")
        .await;
    let out = run.out();
    // Two header lines, then one line per interface: `<name>: <counters>`.
    let dev: Vec<&str> = out
        .lines()
        .take_while(|l| *l != "--")
        .skip(2)
        .filter_map(|l| l.split_once(':').map(|(name, _)| name.trim()))
        .collect();
    assert_eq!(dev, ["lo"], "{}", run.said);
    assert_eq!(after(out, "--"), ["lo"], "{}", run.said);
}

/// Catches a container cgroup without `memory.oom.group`: the job reads `1` from the
/// cgroup file of its own cgroup (a cgroup namespace shows it as the root, `0::/`), so one
/// OOM kill takes the whole container, never a part of it.
#[tokio::test]
#[ignore = "real crun, root and a job user: run through cargo xtask crun-tests --job-user"]
async fn the_containers_cgroup_kills_as_a_group_on_out_of_memory() {
    let cell = Cell::new("oomgroup");
    let run = cell
        .sh(
            "oomgroup",
            "cat /proc/self/cgroup; cat /sys/fs/cgroup/memory.oom.group",
        )
        .await;
    let lines: Vec<&str> = run.out().lines().collect();
    assert_eq!(lines, ["0::/", "1"], "{}", run.said);
}

/// Catches an exec-root overlay mounted without the pinned options, and one that can run
/// a set-uid program: the job's mount table lists `/work` as an overlay of this lease's
/// own lower, upper and work directories, with `userxattr`, `nosuid` and `nodev`; each of
/// `redirect_dir`, `metacopy`, `index` and `xino` that the kernel lists has the pinned safe
/// value. The kernel lists those options only when they differ from its default, so this
/// catches a wrong value (for example `metacopy=on`) and not an option dropped from the
/// pinned string whose default is the safe value; that the pinned options are all present
/// is the resolved-bundle unit test's job (P4).
#[tokio::test]
#[ignore = "real crun, root and a job user: run through cargo xtask crun-tests --job-user"]
async fn the_exec_root_is_an_overlay_mounted_with_the_pinned_options() {
    let cell = Cell::new("overlay");
    let lease = cell.lease(
        "overlay",
        &["/bin/sh", "-c", "cat /proc/self/mountinfo"],
        crate::cell::Shape::default(),
    );
    let running = cell.start(&lease).await;
    let run = cell.finish(&lease, running).await;
    let work = mount_at(run.out(), "/work");
    assert_eq!(work.fstype, "overlay", "{}", run.said);
    for option in ["nosuid", "nodev"] {
        assert!(
            work.has(option),
            "{option} in {:?}: {}",
            work.options,
            run.said
        );
    }
    let value = |name: &str| {
        work.super_options
            .iter()
            .find_map(|o| o.strip_prefix(&format!("{name}=")))
            .map(str::to_owned)
    };
    let spec = &lease.spec;
    assert_eq!(
        value("lowerdir").as_deref(),
        spec.input_root.to_str(),
        "{}",
        run.said
    );
    assert_eq!(
        value("upperdir").as_deref(),
        spec.upper.to_str(),
        "{}",
        run.said
    );
    assert_eq!(
        value("workdir").as_deref(),
        spec.overlay_work.to_str(),
        "{}",
        run.said
    );
    assert!(
        work.super_options.iter().any(|o| o == "userxattr"),
        "{:?}: {}",
        work.super_options,
        run.said
    );
    for (name, pinned) in [
        ("redirect_dir", "nofollow"),
        ("metacopy", "off"),
        ("index", "off"),
        ("xino", "off"),
    ] {
        if let Some(value) = value(name) {
            assert_eq!(
                value, pinned,
                "{name}: {:?}: {}",
                work.super_options, run.said
            );
        }
    }
}
