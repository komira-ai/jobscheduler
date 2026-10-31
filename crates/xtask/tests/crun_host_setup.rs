//! `packaging/js-agent/crun_host_setup.sh`, the setup of a Linux host that runs `js-agent --runtime
//! crun`, run against a fake root with a `PATH` of recording stand-ins (`host-shim`) for every
//! program that reads or changes the host. Nothing here runs a real `systemctl`, `mount`,
//! `useradd` or `apt-get`: the harness refuses to start if one could be reached, or if it runs as
//! root, and a stand-in nobody configured fails the call (exit 97) and is logged.
//! (Linux only: the script is.)
#![cfg(target_os = "linux")]

use std::collections::BTreeMap;
use std::os::unix::fs::{MetadataExt as _, PermissionsExt as _, symlink};
use std::path::{Path, PathBuf};
use std::process::Command;

use sha2::{Digest, Sha256};

const SHIM: &str = env!("CARGO_BIN_EXE_host-shim");
const SCRIPT: &str = "packaging/js-agent/crun_host_setup.sh";
const UNIT: &str = "packaging/js-agent/js-agent-crun.service";
const TMPFILES: &str = "packaging/js-agent/tmpfiles-crun.conf";

/// The programs the script runs that read or change the host. Each has a stand-in.
const GUARDED: [&str; 14] = [
    "apparmor_parser",
    "apt-get",
    "dpkg-query",
    "findmnt",
    "fuser",
    "getent",
    "id",
    "mount",
    "systemctl",
    "systemd-analyze",
    "umount",
    "uname",
    "useradd",
    "usermod",
];

/// The real programs the script may run: plain file tools.
const TOOLS: [&str; 9] = [
    "cat",
    "chmod",
    "dirname",
    "install",
    "mkdir",
    "mv",
    "sed",
    "sha256sum",
    "stat",
];

/// The mount unit of the scratch filesystem, as a host names it.
const MOUNT: &str = r"var-lib-js\x2dagent-scratch.mount";
const SCRATCH: &str = "/var/lib/js-agent/scratch";
const SERVER: &str = "https://sched.example.org:7300";

fn repo(path: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .join(path)
}

fn read(path: &str) -> String {
    std::fs::read_to_string(repo(path)).unwrap_or_else(|e| panic!("{path}: {e}"))
}

fn sha(text: &str) -> String {
    hex::encode(Sha256::digest(text.as_bytes()))
}

/// A stand-in's text with the escapes `host-shim` reads.
fn esc(text: &str) -> String {
    text.replace('\\', "\\\\")
        .replace('\n', "\\n")
        .replace('\t', "\\t")
}

/// One rule of the stand-ins' answers file.
fn rule(name: &str, args: &str, code: u8, stdout: &str, effects: &[String]) -> String {
    let mut line = format!("{name}\t{args}\t{code}\t{stdout}");
    for effect in effects {
        line.push('\t');
        line.push_str(effect);
    }
    line
}

/// The path of the first program called `name` in `dirs`.
fn resolve(name: &str, dirs: &[&Path]) -> Option<PathBuf> {
    dirs.iter().map(|d| d.join(name)).find(|p| p.exists())
}

/// A real program on the box, found where the distribution puts it.
fn real(name: &str) -> PathBuf {
    ["/usr/bin", "/bin", "/usr/sbin", "/sbin"]
        .iter()
        .map(|d| Path::new(d).join(name))
        .find(|p| p.is_file())
        .unwrap_or_else(|| panic!("no {name} on this box"))
}

/// What a run printed and how it ended.
struct Out {
    code: i32,
    text: String,
}

/// One fake host: a root, the stand-ins and the answers they give.
struct Rig {
    root: PathBuf,
    shim: PathBuf,
    tools: PathBuf,
    /// Rules a test puts before the defaults.
    front: Vec<String>,
}

impl Rig {
    /// A healthy host that has nothing of the setup yet: a loop XFS image mounted by a unit with
    /// `Options=loop`, no `crun`, no job user.
    fn new(name: &str) -> Self {
        assert!(
            !rustix::process::geteuid().is_root(),
            "these tests must not run as root"
        );
        let dir = Path::new(env!("CARGO_TARGET_TMPDIR"))
            .join("crun-host-setup")
            .join(name);
        if dir.exists() {
            std::fs::remove_dir_all(&dir).expect("clear scratch");
        }
        let (root, shim, tools) = (dir.join("root"), dir.join("shim"), dir.join("tools"));
        for sub in [&shim, &tools] {
            std::fs::create_dir_all(sub).expect("directory");
        }
        for name in GUARDED {
            std::fs::copy(SHIM, shim.join(name)).expect("a stand-in");
        }
        for name in TOOLS {
            symlink(real(name), tools.join(name)).expect("a tool");
        }
        let rig = Self {
            root,
            shim,
            tools,
            front: Vec::new(),
        };
        rig.guard();
        rig.write("etc/passwd", "root:x:0:0:root:/root:/bin/bash\n");
        rig.write("etc/group", "root:x:0:\n");
        rig.write("etc/subuid", "");
        rig.write("etc/subgid", "");
        rig.write(
            "sys/fs/cgroup/cgroup.controllers",
            "cpuset cpu io memory pids\n",
        );
        std::fs::create_dir_all(rig.path("var/lib")).expect("directory");
        rig.write("etc/tmpfiles.d/.keep", "");
        std::fs::create_dir_all(rig.path("usr/bin")).expect("directory");
        std::fs::copy(SHIM, rig.path("usr/bin/js-agent")).expect("the agent's stand-in");
        rig.write(&format!("etc/systemd/system/{MOUNT}"), &mount_unit("loop"));
        rig.write(
            "mount-options",
            "rw,relatime,attr2,inode64,logbufs=8,logbsize=32k,noquota\n",
        );
        rig
    }

    /// Refuses to go on when a program of the host could be reached from the script's `PATH`.
    fn guard(&self) {
        let dirs = [self.shim.as_path(), self.tools.as_path()];
        for name in GUARDED {
            let found = resolve(name, &dirs).expect("resolves");
            assert_eq!(found, self.shim.join(name), "{name} is not a stand-in");
        }
        for name in TOOLS {
            assert!(
                !GUARDED.contains(&name),
                "{name} is a tool and a guarded program"
            );
        }
        assert!(
            !self.shim.starts_with(&self.root),
            "the shims are in the root"
        );
    }

    fn write(&self, rel: &str, text: &str) {
        let path = self.root.join(rel);
        std::fs::create_dir_all(path.parent().expect("parent")).expect("directory");
        std::fs::write(path, text).expect("write");
    }

    fn path(&self, rel: &str) -> PathBuf {
        self.root.join(rel)
    }

    fn text(&self, rel: &str) -> String {
        std::fs::read_to_string(self.path(rel)).unwrap_or_else(|e| panic!("{rel}: {e}"))
    }

    /// Puts `rule` before the defaults.
    fn rule(&mut self, rule: String) {
        self.front.push(rule);
    }

    /// The answers of a healthy host.
    fn defaults(&self) -> Vec<String> {
        let r = |rel: &str| self.path(rel).display().to_string();
        let write = |rel: &str, text: &str| format!("write={}={}", r(rel), esc(text));
        let append = |rel: &str, text: &str| format!("append={}={}", r(rel), esc(text));
        let ok = |name: &str, args: &str, out: &str| rule(name, args, 0, &esc(out), &[]);
        let file =
            |name: &str, args: &str, rel: &str| rule(name, args, 0, &format!("@{}", r(rel)), &[]);
        vec![
            ok("uname", "-s", "Linux\n"),
            ok("uname", "-r", "6.8.0-142-generic\n"),
            ok("id", "-u", "0\n"),
            ok(
                "systemctl",
                "--version",
                "systemd 255 (255.4-1ubuntu8)\n+PAM +AUDIT\n",
            ),
            ok(
                "systemctl",
                "show -p ActiveState --value -- js-agent.service",
                "inactive\n",
            ),
            ok(
                "systemctl",
                "show -p ActiveState --value -- js-agent-crun.service",
                "inactive\n",
            ),
            file("dpkg-query", "-W -f=${Status} crun", "dpkg/crun"),
            file("dpkg-query", "-W -f=${Status} uidmap", "dpkg/uidmap"),
            ok("dpkg-query", "-S /usr/bin/crun", "crun: /usr/bin/crun\n"),
            ok("crun", "--version", "crun version 1.14.1\ncommit: abc\n"),
            ok(
                "js-agent",
                "--version",
                "js-agent 0.2.0\nruntimes: docker crun\n",
            ),
            file("getent", "passwd", "etc/passwd"),
            file("getent", "group", "etc/group"),
            rule(
                "apt-get",
                "install -y --no-install-recommends crun uidmap",
                0,
                "",
                &[
                    write("dpkg/crun", "install ok installed"),
                    write("dpkg/uidmap", "install ok installed"),
                    format!("copy-self={}", r("usr/bin/crun")),
                ],
            ),
            rule(
                "useradd",
                "--system --user-group --no-create-home --home-dir /nonexistent --shell /usr/sbin/nologin jsjob",
                0,
                "",
                &[
                    append(
                        "etc/passwd",
                        "jsjob:x:998:998::/nonexistent:/usr/sbin/nologin\n",
                    ),
                    append("etc/group", "jsjob:x:998:\n"),
                ],
            ),
            rule(
                "usermod",
                "--add-subuids 200000-265535 jsjob",
                0,
                "",
                &[append("etc/subuid", "jsjob:200000:65536\n")],
            ),
            rule(
                "usermod",
                "--add-subgids 200000-265535 jsjob",
                0,
                "",
                &[append("etc/subgid", "jsjob:200000:65536\n")],
            ),
            file(
                "findmnt",
                &format!("-n -o OPTIONS --mountpoint {SCRATCH}"),
                "mount-options",
            ),
            rule("fuser", &format!("-m -- {SCRATCH}"), 1, "", &[]),
            ok("systemctl", "daemon-reload", ""),
            rule(
                "systemctl",
                &format!("stop {MOUNT}"),
                0,
                "",
                &[write("mount-options", "")],
            ),
            rule(
                "systemctl",
                &format!("start {MOUNT}"),
                0,
                "",
                &[write(
                    "mount-options",
                    "rw,relatime,attr2,inode64,logbufs=8,prjquota\n",
                )],
            ),
            ok("systemd-analyze", "verify *", ""),
        ]
    }

    /// Runs the script with `args` against the root; the calls it made are in `calls`.
    fn run(&self, args: &[&str]) -> Out {
        let mut answers = self.front.clone();
        answers.extend(self.defaults());
        std::fs::write(self.shim.join("answers"), answers.join("\n") + "\n").expect("answers");
        // The program the script runs by an absolute path under the root is a stand-in as well.
        let path = format!("{}:{}", self.shim.display(), self.tools.display());
        let out = Command::new(real("bash"))
            .arg(repo(SCRIPT))
            .args(["--root", &self.root.display().to_string()])
            .args(args)
            .env_clear()
            .env("PATH", path)
            .env("HOST_SHIM_DIR", &self.shim)
            .env("LC_ALL", "C")
            .output()
            .expect("run the script");
        let mut text = String::from_utf8_lossy(&out.stdout).into_owned();
        text.push_str(&String::from_utf8_lossy(&out.stderr));
        Out {
            code: out.status.code().expect("an exit code"),
            text,
        }
    }

    /// The flags of an apply with the unit and tmpfiles file of this repository.
    fn apply_args(&self) -> Vec<String> {
        let mut args: Vec<String> = ["--apply", "--server", SERVER, "--pool", "farm"]
            .map(str::to_owned)
            .to_vec();
        args.extend(self.file_args());
        args
    }

    fn file_args(&self) -> Vec<String> {
        vec![
            "--unit".into(),
            repo(UNIT).display().to_string(),
            "--unit-sha256".into(),
            sha(&read(UNIT)),
            "--tmpfiles".into(),
            repo(TMPFILES).display().to_string(),
            "--tmpfiles-sha256".into(),
            sha(&read(TMPFILES)),
        ]
    }

    fn apply(&self) -> Out {
        let args = self.apply_args();
        self.run(&args.iter().map(String::as_str).collect::<Vec<_>>())
    }

    /// Every call the stand-ins logged so far.
    fn calls(&self) -> Vec<String> {
        std::fs::read_to_string(self.shim.join("calls"))
            .unwrap_or_default()
            .lines()
            .map(str::to_owned)
            .collect()
    }

    /// Everything under the root: mode and bytes (a link as its target).
    fn snapshot(&self) -> BTreeMap<String, (u32, Vec<u8>)> {
        fn walk(base: &Path, dir: &Path, into: &mut BTreeMap<String, (u32, Vec<u8>)>) {
            for entry in std::fs::read_dir(dir).expect("read_dir") {
                let path = entry.expect("entry").path();
                let meta = std::fs::symlink_metadata(&path).expect("metadata");
                let rel = path
                    .strip_prefix(base)
                    .expect("under base")
                    .display()
                    .to_string();
                let body = if meta.is_dir() {
                    walk(base, &path, into);
                    Vec::new()
                } else if meta.is_symlink() {
                    std::fs::read_link(&path)
                        .expect("link")
                        .display()
                        .to_string()
                        .into_bytes()
                } else {
                    std::fs::read(&path).expect("file")
                };
                into.insert(rel, (meta.mode(), body));
            }
        }
        let mut into = BTreeMap::new();
        walk(&self.root, &self.root, &mut into);
        into
    }
}

/// The scratch mount unit with the given `Options=`.
fn mount_unit(options: &str) -> String {
    format!(
        "[Unit]\nDescription=js-agent scratch\n\n[Mount]\nWhat=/var/lib/js-agent-scratch.img\nWhere={SCRATCH}\nType=xfs\nOptions={options}\n\n[Install]\nWantedBy=local-fs.target\n"
    )
}

/// Whether a logged call changes the host.
fn mutating(call: &str) -> bool {
    let mut words = call.split(' ');
    let name = words.next().unwrap_or_default();
    match name {
        "apt-get" | "useradd" | "usermod" | "userdel" | "apparmor_parser" | "mount" | "umount" => {
            true
        }
        "systemctl" => !matches!(words.next(), Some("--version" | "show")),
        _ => false,
    }
}

fn mutations(calls: &[String]) -> Vec<&String> {
    calls.iter().filter(|c| mutating(c)).collect()
}

fn index(calls: &[String], call: &str) -> usize {
    calls
        .iter()
        .position(|c| c == call)
        .unwrap_or_else(|| panic!("no call '{call}' in {calls:?}"))
}

fn mode(rig: &Rig, rel: &str) -> u32 {
    std::fs::metadata(rig.path(rel))
        .expect("metadata")
        .permissions()
        .mode()
        & 0o777
}

/// What the script must make of the repository's unit for the flags of `apply_args`.
fn rendered_unit() -> String {
    read(UNIT)
        .replace("SET-THE-SERVER-WITH-SYSTEMCTL-EDIT", SERVER)
        .replace("--pool default", "--pool farm")
}

/// A refused or failed run changed nothing: no write under the root and no call that changes
/// the host.
fn assert_untouched(rig: &Rig, before: &BTreeMap<String, (u32, Vec<u8>)>) {
    assert!(&rig.snapshot() == before, "the root changed");
    let calls = rig.calls();
    assert!(mutations(&calls).is_empty(), "{:?}", mutations(&calls));
    assert!(
        !calls.iter().any(|c| c.starts_with("UNCONFIGURED")),
        "{calls:?}"
    );
}

/// The stand-ins really are the programs the script finds, and the harness knows its own mutating
/// calls: a defence against a test that passes because nothing was looked at.
#[test]
fn the_harness_reaches_only_stand_ins_and_names_the_calls_that_change_the_host() {
    let rig = Rig::new("harness");
    let dirs = [rig.shim.as_path(), rig.tools.as_path()];
    for name in GUARDED {
        assert_eq!(resolve(name, &dirs), Some(rig.shim.join(name)), "{name}");
    }
    for call in [
        "apt-get install x",
        "systemctl daemon-reload",
        "systemctl stop a",
        "systemctl start a",
        "usermod x",
        "mount a",
        "umount a",
    ] {
        assert!(mutating(call), "{call}");
    }
    for call in [
        "systemctl show -p ActiveState",
        "systemctl --version",
        "findmnt -n",
        "getent passwd",
        "fuser -m",
    ] {
        assert!(!mutating(call), "{call}");
    }
    // A stand-in nobody configured fails closed and says so.
    std::fs::write(rig.shim.join("answers"), "").expect("answers");
    let out = Command::new(rig.shim.join("systemctl"))
        .arg("poweroff")
        .env_clear()
        .env("HOST_SHIM_DIR", &rig.shim)
        .output()
        .expect("run");
    assert_eq!(out.status.code(), Some(97));
    assert_eq!(rig.calls(), ["UNCONFIGURED systemctl poweroff"]);
    // So does one run with no directory at all.
    let out = Command::new(rig.shim.join("systemctl"))
        .env_clear()
        .output()
        .expect("run");
    assert_eq!(out.status.code(), Some(97));
}

/// The default is a dry run: it prints the plan, including the scratch filesystem's quota, reads
/// the host, and changes neither the root nor the host.
#[test]
fn a_dry_run_prints_the_plan_and_changes_nothing() {
    let rig = Rig::new("dry-run");
    let before = rig.snapshot();
    let mut args = vec![
        "--server".to_owned(),
        SERVER.to_owned(),
        "--pool".into(),
        "farm".into(),
    ];
    args.extend(rig.file_args());
    let out = rig.run(&args.iter().map(String::as_str).collect::<Vec<_>>());
    assert_eq!(out.code, 0, "{}", out.text);
    for want in [
        "would: apt-get install crun uidmap",
        "would: useradd",
        "would: usermod --add-subuids 200000-265535 jsjob",
        "prjquota",
        "dry run: nothing changed",
    ] {
        assert!(out.text.contains(want), "{want}: {}", out.text);
    }
    assert_untouched(&rig, &before);
    assert!(!rig.calls().is_empty(), "the host was not read");
}

/// An apply on a host with nothing of the setup makes every piece, in an order that works: the
/// packages before the user, the unit's edit and a reload before the unmount and mount, the mount
/// check after it.
#[test]
fn an_apply_on_a_fresh_root_sets_the_host_up() {
    let rig = Rig::new("apply-fresh");
    let out = rig.apply();
    assert_eq!(out.code, 0, "{}", out.text);
    assert_eq!(rig.text("etc/subuid"), "jsjob:200000:65536\n");
    assert_eq!(rig.text("etc/subgid"), "jsjob:200000:65536\n");
    assert!(rig.text("etc/passwd").contains("jsjob:"));
    for (dir, want) in [
        ("etc/js-agent", 0o700),
        ("var/lib/js-agent", 0o711),
        ("var/lib/js-agent/state", 0o700),
        ("var/lib/js-agent/scratch", 0o711),
        ("var/lib/js-agent/scratch/runs", 0o711),
    ] {
        assert_eq!(mode(&rig, dir), want, "{dir}");
    }
    assert_eq!(
        rig.text("etc/systemd/system/js-agent-crun.service"),
        rendered_unit()
    );
    assert_eq!(
        rig.text("etc/tmpfiles.d/js-agent-crun.conf"),
        read(TMPFILES)
    );
    assert_eq!(
        rig.text(&format!("etc/systemd/system/{MOUNT}")),
        mount_unit("loop,prjquota"),
        "the existing option is kept and prjquota added"
    );
    let calls = rig.calls();
    assert!(
        !calls.iter().any(|c| c.starts_with("UNCONFIGURED")),
        "{calls:?}"
    );
    assert!(
        !calls
            .iter()
            .any(|c| c.starts_with("mount ") || c.starts_with("umount ")),
        "{calls:?}"
    );
    let stop = index(&calls, &format!("systemctl stop {MOUNT}"));
    let start = index(&calls, &format!("systemctl start {MOUNT}"));
    let reload = index(&calls, "systemctl daemon-reload");
    let check = calls
        .iter()
        .rposition(|c| c == &format!("findmnt -n -o OPTIONS --mountpoint {SCRATCH}"))
        .expect("the mount options were read");
    assert!(reload < stop && stop < start && start < check, "{calls:?}");
    assert!(
        index(
            &calls,
            "apt-get install -y --no-install-recommends crun uidmap"
        ) < index(&calls, "usermod --add-subuids 200000-265535 jsjob")
    );
    let manifest = rig.text("var/lib/js-crun-host-setup/manifest");
    for line in [
        "pkg\tcrun",
        "pkg\tuidmap",
        "user\tjsjob\t998",
        "subuid\tjsjob\t200000\t65536",
        "mount-options\t",
    ] {
        assert!(manifest.contains(line), "{line}: {manifest}");
    }
    assert!(out.text.contains("runtimes: docker crun"), "{}", out.text);
}

/// A second run on a host that is set up changes nothing and says so (exit 3), whether it is a
/// dry run or an apply: the root is the same bytes and no call changes the host.
#[test]
fn a_rerun_on_a_host_that_is_set_up_changes_nothing() {
    let rig = Rig::new("rerun");
    let first = rig.apply();
    assert_eq!(first.code, 0, "{}", first.text);
    let before = rig.snapshot();
    let mark = rig.calls().len();
    let second = rig.apply();
    assert_eq!(second.code, 3, "{}", second.text);
    assert!(second.text.contains("nothing to do"), "{}", second.text);
    let all = rig.calls();
    assert!(!all[mark..].is_empty(), "the second run read nothing");
    assert!(
        mutations(&all[mark..]).is_empty(),
        "{:?}",
        mutations(&all[mark..])
    );
    assert!(rig.snapshot() == before, "the root changed");
    let mut args = vec![
        "--server".to_owned(),
        SERVER.to_owned(),
        "--pool".into(),
        "farm".into(),
    ];
    args.extend(rig.file_args());
    let dry = rig.run(&args.iter().map(String::as_str).collect::<Vec<_>>());
    assert_eq!(dry.code, 3, "{}", dry.text);
    assert!(rig.snapshot() == before, "the dry run changed the root");
    assert!(mutations(&rig.calls()[mark..]).is_empty());
}

/// Where the mount unit already has prjquota and the mount shows it, the unit is not edited and
/// the filesystem is not unmounted: the rest of the setup goes on.
#[test]
fn prjquota_already_present_is_left_alone() {
    let rig = Rig::new("quota-present");
    rig.write(
        &format!("etc/systemd/system/{MOUNT}"),
        &mount_unit("loop,prjquota"),
    );
    rig.write("mount-options", "rw,relatime,attr2,inode64,prjquota\n");
    let out = rig.apply();
    assert_eq!(out.code, 0, "{}", out.text);
    assert_eq!(
        rig.text(&format!("etc/systemd/system/{MOUNT}")),
        mount_unit("loop,prjquota")
    );
    let calls = rig.calls();
    assert!(
        !calls.contains(&format!("systemctl stop {MOUNT}")),
        "{calls:?}"
    );
    assert!(
        !calls.contains(&format!("systemctl start {MOUNT}")),
        "{calls:?}"
    );
    assert!(
        !calls.contains(&"fuser -m -- /var/lib/js-agent/scratch".to_owned()),
        "{calls:?}"
    );
    assert!(out.text.contains("prjquota"), "{}", out.text);
}

/// Where the unit has prjquota but the running mount does not (an interrupted earlier run), the
/// unit is not edited again and the mount is made again.
#[test]
fn a_unit_with_prjquota_and_a_mount_without_it_is_mounted_again() {
    let rig = Rig::new("quota-unit-only");
    rig.write(
        &format!("etc/systemd/system/{MOUNT}"),
        &mount_unit("loop,prjquota"),
    );
    let out = rig.apply();
    assert_eq!(out.code, 0, "{}", out.text);
    assert_eq!(
        rig.text(&format!("etc/systemd/system/{MOUNT}")),
        mount_unit("loop,prjquota")
    );
    let calls = rig.calls();
    assert!(
        index(&calls, &format!("systemctl stop {MOUNT}"))
            < index(&calls, &format!("systemctl start {MOUNT}"))
    );
}

/// Something holds the scratch mount open: the run is refused before any change, in a dry run
/// and an apply.
#[test]
fn a_mount_that_something_holds_open_is_refused() {
    let mut rig = Rig::new("held");
    rig.rule(rule(
        "fuser",
        &format!("-m -- {SCRATCH}"),
        0,
        " 4242\\n",
        &[],
    ));
    let before = rig.snapshot();
    let out = rig.apply();
    assert_eq!(out.code, 2, "{}", out.text);
    assert!(
        out.text.contains("REFUSE") && out.text.contains("holds"),
        "{}",
        out.text
    );
    assert_untouched(&rig, &before);
    let dry = rig.run(&[]);
    assert_eq!(dry.code, 2, "{}", dry.text);
}

/// The agent runs (either unit, in any state but inactive or failed): refused before any change.
#[test]
fn a_running_agent_is_refused() {
    for (name, unit, state) in [
        ("running-engine-unit", "js-agent.service", "active"),
        ("running-crun-unit", "js-agent-crun.service", "activating"),
    ] {
        let mut rig = Rig::new(name);
        rig.rule(rule(
            "systemctl",
            &format!("show -p ActiveState --value -- {unit}"),
            0,
            &format!("{state}\\n"),
            &[],
        ));
        let before = rig.snapshot();
        let out = rig.apply();
        assert_eq!(out.code, 2, "{}", out.text);
        assert!(
            out.text.contains(unit) && out.text.contains(state),
            "{}",
            out.text
        );
        assert_untouched(&rig, &before);
    }
}

/// The mount comes back without prjquota: the run fails (exit 1), says why, and does not report
/// the host as set up.
#[test]
fn a_remount_that_does_not_show_prjquota_fails() {
    let mut rig = Rig::new("quota-not-active");
    rig.rule(rule(
        "systemctl",
        &format!("start {MOUNT}"),
        0,
        "",
        &[format!(
            "write={}=rw,noquota\\n",
            rig.path("mount-options").display()
        )],
    ));
    let out = rig.apply();
    assert_eq!(out.code, 1, "{}", out.text);
    assert!(out.text.contains("prjquota is not active"), "{}", out.text);
    assert!(!out.text.contains("done."), "{}", out.text);
}

/// A mount unit with no `Options=` line cannot have its options kept, and a missing unit file
/// cannot be edited: both are refused before any change, and `--skip-scratch-quota` leaves the
/// scratch filesystem out.
#[test]
fn a_mount_unit_that_cannot_be_edited_is_refused() {
    let rig = Rig::new("quota-no-options");
    rig.write(
        &format!("etc/systemd/system/{MOUNT}"),
        &mount_unit("loop").replace("Options=loop\n", ""),
    );
    let before = rig.snapshot();
    let out = rig.apply();
    assert_eq!(out.code, 2, "{}", out.text);
    assert!(out.text.contains("Options="), "{}", out.text);
    assert_untouched(&rig, &before);

    let missing = Rig::new("quota-no-unit");
    std::fs::remove_file(missing.path(&format!("etc/systemd/system/{MOUNT}"))).expect("remove");
    let before = missing.snapshot();
    let out = missing.apply();
    assert_eq!(out.code, 2, "{}", out.text);
    assert_untouched(&missing, &before);

    let mut args = missing.apply_args();
    args.push("--skip-scratch-quota".into());
    let out = missing.run(&args.iter().map(String::as_str).collect::<Vec<_>>());
    assert_eq!(out.code, 0, "{}", out.text);
    assert!(!missing.calls().iter().any(|c| c.contains(".mount")));
}

/// An apply needs root, the server and pool for the unit, and a unit that is the pinned file; each
/// missing piece is a refusal before any change.
#[test]
fn an_apply_without_what_it_needs_is_refused_before_any_change() {
    let mut rig = Rig::new("refusals");
    let before = rig.snapshot();
    // Not root.
    rig.rule(rule("id", "-u", 0, "1000\\n", &[]));
    let out = rig.apply();
    assert_eq!(out.code, 2, "{}", out.text);
    assert!(out.text.contains("not root"), "{}", out.text);
    assert_untouched(&rig, &before);
    rig.front.clear();
    // No server and pool.
    let mut args: Vec<String> = vec!["--apply".into()];
    args.extend(rig.file_args());
    let out = rig.run(&args.iter().map(String::as_str).collect::<Vec<_>>());
    assert_eq!(out.code, 2, "{}", out.text);
    assert!(out.text.contains("--server and --pool"), "{}", out.text);
    assert_untouched(&rig, &before);
    // A unit that is not the pinned file.
    let mut args = rig.apply_args();
    let at = args
        .iter()
        .position(|a| a == "--unit-sha256")
        .expect("flag")
        + 1;
    args[at] = "0".repeat(64);
    let out = rig.run(&args.iter().map(String::as_str).collect::<Vec<_>>());
    assert_eq!(out.code, 2, "{}", out.text);
    assert!(out.text.contains("sha256"), "{}", out.text);
    assert_untouched(&rig, &before);
    // A flag the script does not know.
    let out = rig.run(&["--bogus"]);
    assert_eq!(out.code, 2, "{}", out.text);
    assert!(out.text.contains("--bogus"), "{}", out.text);
    assert_untouched(&rig, &before);
}

/// `--help` prints every flag the script takes and changes nothing; the tests above use them all.
#[test]
fn help_lists_the_flags() {
    let rig = Rig::new("help");
    let before = rig.snapshot();
    let out = rig.run(&["--help"]);
    assert_eq!(out.code, 0, "{}", out.text);
    for flag in [
        "--apply",
        "--root",
        "--job-user",
        "--subid-start",
        "--unit",
        "--unit-sha256",
        "--tmpfiles",
        "--tmpfiles-sha256",
        "--server",
        "--pool",
        "--scratch-mount-unit",
        "--skip-scratch-quota",
    ] {
        assert!(out.text.contains(flag), "{flag}: {}", out.text);
    }
    assert_untouched(&rig, &before);
    assert!(rig.calls().is_empty(), "--help ran a program");
}
