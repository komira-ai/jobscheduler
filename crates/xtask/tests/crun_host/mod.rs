//! The fake host of the `crun_host_setup.sh` tests: a root, recording stand-ins for the host's
//! programs, and the answers they give. Shared by the test files that run the script.
#![allow(dead_code)]

use std::collections::BTreeMap;
use std::os::unix::fs::{MetadataExt as _, PermissionsExt as _, symlink};
use std::path::{Path, PathBuf};
use std::process::Command;

use sha2::{Digest, Sha256};

pub const SHIM: &str = env!("CARGO_BIN_EXE_host-shim");
pub const SCRIPT: &str = "packaging/js-agent/crun_host_setup.sh";
pub const UNIT: &str = "packaging/js-agent/js-agent-crun.service";
pub const TMPFILES: &str = "packaging/js-agent/tmpfiles-crun.conf";

/// The programs the script runs that read or change the host. Each has a stand-in.
pub const GUARDED: [&str; 15] = [
    "apparmor_parser",
    "apt-get",
    "dpkg-query",
    "findmnt",
    "fuser",
    "getent",
    "id",
    "mount",
    "sha256sum",
    "systemctl",
    "systemd-analyze",
    "umount",
    "uname",
    "useradd",
    "usermod",
];

/// The real programs the script may run: plain file tools.
pub const TOOLS: [&str; 9] = [
    "cat", "chmod", "dirname", "install", "mkdir", "mv", "rm", "sed", "stat",
];

/// The mount unit of the scratch filesystem, as a host names it.
pub const MOUNT: &str = r"var-lib-js\x2dagent-scratch.mount";
pub const SCRATCH: &str = "/var/lib/js-agent/scratch";
pub const SERVER: &str = "https://sched.example.org:7300";

pub fn repo(path: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .join(path)
}

pub fn read(path: &str) -> String {
    std::fs::read_to_string(repo(path)).unwrap_or_else(|e| panic!("{path}: {e}"))
}

pub fn sha(text: &str) -> String {
    hex::encode(Sha256::digest(text.as_bytes()))
}

/// A stand-in's text with the escapes `host-shim` reads.
pub fn esc(text: &str) -> String {
    text.replace('\\', "\\\\")
        .replace('\n', "\\n")
        .replace('\t', "\\t")
}

/// One rule of the stand-ins' answers file.
pub fn rule(name: &str, args: &str, code: u8, stdout: &str, effects: &[String]) -> String {
    let mut line = format!("{name}\t{args}\t{code}\t{stdout}");
    for effect in effects {
        line.push('\t');
        line.push_str(effect);
    }
    line
}

/// The path of the first program called `name` in `dirs`.
pub fn resolve(name: &str, dirs: &[&Path]) -> Option<PathBuf> {
    dirs.iter().map(|d| d.join(name)).find(|p| p.exists())
}

/// A real program on the box, found where the distribution puts it.
pub fn real(name: &str) -> PathBuf {
    ["/usr/bin", "/bin", "/usr/sbin", "/sbin"]
        .iter()
        .map(|d| Path::new(d).join(name))
        .find(|p| p.is_file())
        .unwrap_or_else(|| panic!("no {name} on this box"))
}

/// The variable the coverage build gives the programs it runs, so the stand-ins' coverage is
/// counted: it names a file to write, and nothing the script could read as a setting.
pub fn profile_env() -> Vec<(&'static str, String)> {
    std::env::var("LLVM_PROFILE_FILE")
        .map(|v| vec![("LLVM_PROFILE_FILE", v)])
        .unwrap_or_default()
}

/// What a run printed and how it ended.
pub struct Out {
    pub code: i32,
    pub text: String,
}

/// One fake host: a root, the stand-ins and the answers they give.
pub struct Rig {
    pub root: PathBuf,
    pub shim: PathBuf,
    pub tools: PathBuf,
    /// Rules a test puts before the defaults.
    pub front: Vec<String>,
}

impl Rig {
    /// A healthy host that has nothing of the setup yet: a loop XFS image mounted by a unit with
    /// `Options=loop`, no `crun`, no job user.
    pub fn new(name: &str) -> Self {
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
    pub fn guard(&self) {
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

    pub fn write(&self, rel: &str, text: &str) {
        let path = self.root.join(rel);
        std::fs::create_dir_all(path.parent().expect("parent")).expect("directory");
        std::fs::write(path, text).expect("write");
    }

    pub fn path(&self, rel: &str) -> PathBuf {
        self.root.join(rel)
    }

    pub fn text(&self, rel: &str) -> String {
        std::fs::read_to_string(self.path(rel)).unwrap_or_else(|e| panic!("{rel}: {e}"))
    }

    /// Puts `rule` before the defaults.
    pub fn rule(&mut self, rule: String) {
        self.front.push(rule);
    }

    /// The answers of a healthy host.
    pub fn defaults(&self) -> Vec<String> {
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
            ok("id", "-ur", "1000\n"),
            rule(
                "sha256sum",
                "*",
                0,
                &format!("!{}", real("sha256sum").display()),
                &[],
            ),
            ok("apparmor_parser", "-r *", ""),
            ok(
                "systemctl",
                &format!("list-dependencies --reverse --plain --no-pager {MOUNT}"),
                &format!("{MOUNT}\n  local-fs.target\n"),
            ),
            ok(
                "systemctl",
                &format!("show -p Options --value -- {MOUNT}"),
                "loop,prjquota\n",
            ),
            ok(
                "findmnt",
                &format!("-n -o SOURCE --mountpoint {SCRATCH}"),
                "/dev/loop3\n",
            ),
            ok(
                "findmnt",
                "-n -o TARGET -S /dev/loop3",
                &format!("{SCRATCH}\n"),
            ),
            ok(
                "findmnt",
                &format!("-n -R -o TARGET {SCRATCH}"),
                &format!("{SCRATCH}\n"),
            ),
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
    pub fn run(&self, args: &[&str]) -> Out {
        self.run_with(args, false)
    }

    /// Like `run`; with `small_files` the script may write files of 2 KiB at most (a full disk
    /// for anything bigger: the write fails, and the signal that would kill the writer is ignored).
    pub fn run_with(&self, args: &[&str], small_files: bool) -> Out {
        let mut answers = self.front.clone();
        answers.extend(self.defaults());
        std::fs::write(self.shim.join("answers"), answers.join("\n") + "\n").expect("answers");
        // The program the script runs by an absolute path under the root is a stand-in as well.
        let path = format!("{}:{}", self.shim.display(), self.tools.display());
        let mut command = Command::new(real("bash"));
        if small_files {
            command.args(["-c", "ulimit -S -f 2; trap '' XFSZ; exec \"$0\" \"$@\""]);
            command.arg(real("bash"));
        }
        let out = command
            .arg(repo(SCRIPT))
            .args(["--root", &self.root.display().to_string()])
            .args(args)
            .env_clear()
            .env("PATH", path)
            .env("HOST_SHIM_DIR", &self.shim)
            .env("LC_ALL", "C")
            .envs(profile_env())
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
    pub fn apply_args(&self) -> Vec<String> {
        let mut args: Vec<String> = ["--apply", "--server", SERVER, "--pool", "farm"]
            .map(str::to_owned)
            .to_vec();
        args.extend(self.file_args());
        args
    }

    pub fn file_args(&self) -> Vec<String> {
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

    pub fn apply(&self) -> Out {
        let args = self.apply_args();
        self.run(&args.iter().map(String::as_str).collect::<Vec<_>>())
    }

    /// Every call the stand-ins logged so far.
    pub fn calls(&self) -> Vec<String> {
        std::fs::read_to_string(self.shim.join("calls"))
            .unwrap_or_default()
            .lines()
            .map(str::to_owned)
            .collect()
    }

    /// Everything under the root: mode and bytes (a link as its target).
    pub fn snapshot(&self) -> BTreeMap<String, (u32, Vec<u8>)> {
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
pub fn mount_unit(options: &str) -> String {
    format!(
        "[Unit]\nDescription=js-agent scratch\n\n[Mount]\nWhat=/var/lib/js-agent-scratch.img\nWhere={SCRATCH}\nType=xfs\nOptions={options}\n\n[Install]\nWantedBy=local-fs.target\n"
    )
}

/// Whether a logged call changes the host.
pub fn mutating(call: &str) -> bool {
    let mut words = call.split(' ');
    let name = words.next().unwrap_or_default();
    match name {
        "apt-get" | "useradd" | "usermod" | "userdel" | "apparmor_parser" | "mount" | "umount" => {
            true
        }
        "systemctl" => !matches!(
            words.next(),
            Some("--version" | "show" | "list-dependencies")
        ),
        _ => false,
    }
}

pub fn mutations(calls: &[String]) -> Vec<&String> {
    calls.iter().filter(|c| mutating(c)).collect()
}

pub fn index(calls: &[String], call: &str) -> usize {
    calls
        .iter()
        .position(|c| c == call)
        .unwrap_or_else(|| panic!("no call '{call}' in {calls:?}"))
}

pub fn mode(rig: &Rig, rel: &str) -> u32 {
    std::fs::metadata(rig.path(rel))
        .expect("metadata")
        .permissions()
        .mode()
        & 0o777
}

/// What the script must make of the repository's unit for the flags of `apply_args`.
pub fn rendered_unit() -> String {
    read(UNIT)
        .replace("SET-THE-SERVER-WITH-SYSTEMCTL-EDIT", SERVER)
        .replace("--pool default", "--pool farm")
}

/// A refused or failed run changed nothing: no write under the root and no call that changes
/// the host.
pub fn assert_untouched(rig: &Rig, before: &BTreeMap<String, (u32, Vec<u8>)>) {
    assert!(&rig.snapshot() == before, "the root changed");
    let calls = rig.calls();
    assert!(mutations(&calls).is_empty(), "{:?}", mutations(&calls));
    assert!(
        !calls.iter().any(|c| c.starts_with("UNCONFIGURED")),
        "{calls:?}"
    );
}

/// The host with `crun` and `uidmap` installed (the stand-in is the program at `/usr/bin/crun`).
impl Rig {
    pub fn install_crun(&self) {
        self.write("dpkg/crun", "install ok installed");
        self.write("dpkg/uidmap", "install ok installed");
        std::fs::copy(SHIM, self.path("usr/bin/crun")).expect("the crun stand-in");
    }

    /// Runs an apply (with the files of this repository) and expects it refused: exit 2, the
    /// text in the output, and neither a change of the root nor a call that changes the host.
    pub fn expect_refused(&self, want: &str) {
        let before = self.snapshot();
        let out = self.apply();
        assert_eq!(out.code, 2, "{}", out.text);
        assert!(out.text.contains(want), "{want}: {}", out.text);
        assert_untouched(self, &before);
    }
}
