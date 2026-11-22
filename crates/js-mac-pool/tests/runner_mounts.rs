//! A profile with mount grants under the real `sandbox-exec` on a macOS runner, as the
//! runner's own user, laid out as the agent lays out a job start's mounts
//! (`docs/design/job-rootfs.md` 5.2 and 5.4): two mount roots are read denies, a read-only
//! `no_exec` tree and a read-write `no_exec` output below them are granted, and the run
//! reaches each through a link in its own directory. Each refusal has a control: the same
//! step without the sandbox must succeed, or the refusal proves nothing.

#![cfg(target_os = "macos")]

use std::ffi::OsString;
use std::os::unix::fs::{PermissionsExt as _, symlink};
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use js_mac_pool::seatbelt::{
    AbsPath, Caps, MountGrant, Net, Policy, Profile, ReadModel, SlotLayout,
};

/// A scratch tree under the build's own target directory, removed when dropped:
///
/// - `run/` the run's directory (writable), with `run/tree` and `run/out` links to the
///   mounts, as the agent makes `exec/<rest>`;
/// - `roots/ro/tree/` the read-only mount, holding `input` and the script `tool`;
/// - `roots/ro/other/` a tree below the same root that this run was not given, holding
///   `secret`;
/// - `roots/rw/out/` the read-write mount, holding the script `tool`.
struct Scene {
    base: PathBuf,
    run: PathBuf,
    ro_root: PathBuf,
    rw_root: PathBuf,
    tree: PathBuf,
    other: PathBuf,
    out: PathBuf,
}

/// A script that prints `ran`: what a `no_exec` mount must not let a job start.
const TOOL: &str = "#!/bin/sh\necho ran\n";

impl Scene {
    fn new(tag: &str) -> Self {
        let root = Path::new(env!("CARGO_TARGET_TMPDIR"));
        let base = root.join(format!("mac-pool-mounts-{tag}-{}", std::process::id()));
        let made = |path: &Path| {
            std::fs::create_dir_all(path).expect("scratch directory");
            path.canonicalize().expect("canonical scratch directory")
        };
        let base = made(&base);
        let run = made(&base.join("run"));
        let ro_root = made(&base.join("roots/ro"));
        let rw_root = made(&base.join("roots/rw"));
        let tree = made(&ro_root.join("tree"));
        let other = made(&ro_root.join("other"));
        let out = made(&rw_root.join("out"));
        std::fs::write(tree.join("input"), "the input\n").expect("input");
        std::fs::write(other.join("secret"), "not this run's\n").expect("secret");
        for dir in [&tree, &out] {
            let tool = dir.join("tool");
            std::fs::write(&tool, TOOL).expect("tool");
            std::fs::set_permissions(&tool, std::fs::Permissions::from_mode(0o755)).expect("mode");
        }
        symlink(&tree, run.join("tree")).expect("tree link");
        symlink(&out, run.join("out")).expect("out link");
        Self {
            base,
            run,
            ro_root,
            rw_root,
            tree,
            other,
            out,
        }
    }

    /// The policy of a run given the two mounts, with both mount roots denied, as the agent
    /// builds it.
    fn policy(&self) -> Policy {
        let abs = |path: &Path| AbsPath::new(path).expect("absolute path");
        let layout = SlotLayout {
            trees: None,
            run: abs(&self.run),
            uid_entry: None,
            inputs: vec![abs(&self.run.join("tree"))],
            read_denies: vec![abs(&self.ro_root), abs(&self.rw_root)],
            mounts: vec![
                MountGrant {
                    path: abs(&self.tree),
                    write: false,
                    no_exec: true,
                },
                MountGrant {
                    path: abs(&self.out),
                    write: true,
                    no_exec: true,
                },
            ],
        };
        let caps = Caps {
            net: Net::Off,
            read_model: ReadModel::Open,
        };
        Policy::base(layout, caps).expect("policy")
    }

    /// Runs `program args` under the generated profile of [`Scene::policy`].
    fn sandboxed(&self, program: &str, args: &[OsString]) -> Output {
        let profile = Profile::for_policy(&self.policy()).expect("profile");
        let argv = profile.argv(AbsPath::parse(program).expect("program"), args);
        Command::new(&argv[0])
            .args(&argv[1..])
            .output()
            .expect("sandbox-exec starts")
    }
}

impl Drop for Scene {
    fn drop(&mut self) {
        // Best effort: the runner is discarded anyway.
        let _ = std::fs::remove_dir_all(&self.base);
    }
}

fn os(path: &Path) -> OsString {
    path.as_os_str().to_owned()
}

fn control(program: &str, args: &[OsString]) -> Output {
    Command::new(program)
        .args(args)
        .output()
        .expect("control starts")
}

fn describe(out: &Output) -> String {
    format!(
        "{:?} {} {}",
        out.status,
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    )
}

/// Asserts that a run reads its read-only mount through its link and directly, though the
/// mount's root is a read deny, and does not read a tree below the same root that it was not
/// given. Catches a grant that does not reopen the mount under its root's deny (the job could
/// not read its inputs) and one that reopens the whole root (a job could read another run's
/// mount).
#[test]
fn a_run_reads_its_mount_under_a_denied_root_and_nothing_else_there() {
    let scene = Scene::new("read");
    for path in [scene.run.join("tree/input"), scene.tree.join("input")] {
        let out = scene.sandboxed("/bin/cat", &[os(&path)]);
        assert!(out.status.success(), "{path:?}: {}", describe(&out));
        assert_eq!(String::from_utf8_lossy(&out.stdout), "the input\n");
    }
    let secret = scene.other.join("secret");
    let out = control("/bin/cat", &[os(&secret)]);
    assert!(out.status.success(), "control: {}", describe(&out));
    let out = scene.sandboxed("/bin/cat", &[os(&secret)]);
    assert!(!out.status.success(), "{}", describe(&out));
}

/// Asserts that a run writes into its read-write mount through its link, and cannot write
/// into its read-only mount by either path, nor replace the read-only mount's link. Catches
/// a write grant missing from the read-write mount (the job's output could not be
/// committed), and one given to the read-only mount (a job could change its inputs on the
/// host).
#[test]
fn a_run_writes_only_its_read_write_mount() {
    let scene = Scene::new("write");
    let made = scene.run.join("out/made");
    let out = scene.sandboxed("/usr/bin/touch", &[os(&made)]);
    assert!(out.status.success(), "{}", describe(&out));
    assert!(scene.out.join("made").exists());

    let probe = scene.tree.join("probe");
    let out = control("/usr/bin/touch", &[os(&probe)]);
    assert!(out.status.success(), "control: {}", describe(&out));
    std::fs::remove_file(&probe).expect("control's file");
    for path in [scene.run.join("tree/new"), scene.tree.join("new")] {
        let out = scene.sandboxed("/usr/bin/touch", &[os(&path)]);
        assert!(!out.status.success(), "{path:?}: {}", describe(&out));
    }
    assert!(!scene.tree.join("new").exists());
    let out = scene.sandboxed("/bin/rm", &[os(&scene.run.join("tree"))]);
    assert!(!out.status.success(), "{}", describe(&out));
    assert!(scene.run.join("tree").symlink_metadata().is_ok());
}

/// Asserts that a program in a `no_exec` mount, read-only or read-write, is not started from
/// inside the sandbox, while the same program starts without it. Catches a `no_exec` mount
/// whose files a job can run, such as a tool planted in the output directory.
#[test]
fn nothing_in_a_no_exec_mount_runs() {
    let scene = Scene::new("exec");
    for tool in [scene.tree.join("tool"), scene.out.join("tool")] {
        let out = control("/bin/sh", &[OsString::from("-c"), os(&tool)]);
        assert!(out.status.success(), "control {tool:?}: {}", describe(&out));
        assert_eq!(String::from_utf8_lossy(&out.stdout), "ran\n");
        let out = scene.sandboxed("/bin/sh", &[OsString::from("-c"), os(&tool)]);
        assert!(!out.status.success(), "{tool:?}: {}", describe(&out));
        assert!(!String::from_utf8_lossy(&out.stdout).contains("ran"));
    }
}
