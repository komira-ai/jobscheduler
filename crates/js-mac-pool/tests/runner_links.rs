//! A read-denied file cannot be read through another name, under the generated profile and
//! the real `sandbox-exec` on a macOS runner, as the runner's own user (no root, no grant).
//!
//! A read deny matches paths, so a run that could give the file a second name or a copy
//! inside its run directory, or reach it by a spelling the deny does not match, would read it
//! there. The scene holds a file of the runner's own (mode 0644, so Unix modes allow every
//! read and only the profile can refuse) in a directory the policy read-denies, and each test
//! tries one way round the deny: a direct read, a symbolic link, a hard link, an APFS clone,
//! the volume-id path (`/.vol/<device>/<inode>`), another spelling of the name's case, and a
//! rename into the run directory. A hard link and a clone are made by the sandboxed process
//! itself (this test binary, run again under the profile as a child role), because the call
//! is the thing the profile must refuse.
//!
//! Each refusal has a control: the same step without the sandbox succeeds and yields the
//! secret, or the refusal proves nothing. The one skip is the case spelling on a
//! case-sensitive volume, which states its reason. The file runs in `beta.yml`'s workspace
//! `cargo test` step on hosted macOS (js-mac-pool is not excluded there).

#![cfg(target_os = "macos")]

use std::ffi::{OsStr, OsString};
use std::os::fd::AsFd as _;
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use js_mac_pool::seatbelt::{AbsPath, Caps, Net, Policy, Profile, ReadModel, SlotLayout};
use js_treestore::mac::{Clonefile, Cloner as _};

/// The environment variable that names the role a child run of this binary plays.
const ROLE: &str = "MAC_POOL_LINK_ROLE";
/// The environment variable that names the file the child links or clones.
const SRC: &str = "MAC_POOL_LINK_SRC";
/// The environment variable that names the new name the child makes.
const DST: &str = "MAC_POOL_LINK_DST";

/// A scratch tree under the build's own target directory: `run/` (granted, the run
/// directory) and `outside/secret` (read-denied). Removed when dropped.
struct Scene {
    base: PathBuf,
    run: PathBuf,
    outside: PathBuf,
    secret: PathBuf,
    token: String,
}

impl Scene {
    fn new(tag: &str) -> Self {
        let root = Path::new(env!("CARGO_TARGET_TMPDIR"));
        let base = root.join(format!("mac-pool-links-{tag}-{}", std::process::id()));
        std::fs::create_dir_all(base.join("run")).expect("run directory");
        std::fs::create_dir_all(base.join("outside")).expect("outside directory");
        let base = base.canonicalize().expect("base");
        let token = format!("token-of-{tag}-{}", std::process::id());
        let secret = base.join("outside").join("secret");
        std::fs::write(&secret, &token).expect("the secret");
        std::fs::set_permissions(&secret, std::fs::Permissions::from_mode(0o644))
            .expect("the secret's mode");
        Self {
            run: base.join("run"),
            outside: base.join("outside"),
            secret,
            token,
            base,
        }
    }

    /// The policy: the run directory is the one writable place and `outside/` is read-denied.
    fn policy(&self) -> Policy {
        let layout = SlotLayout {
            trees: None,
            run: AbsPath::new(&self.run).expect("run path"),
            uid_entry: None,
            inputs: vec![],
            read_denies: vec![AbsPath::new(&self.outside).expect("read-denied path")],
            mounts: vec![],
        };
        Policy::base(
            layout,
            Caps {
                net: Net::Off,
                read_model: ReadModel::Open,
            },
        )
        .expect("policy")
    }

    /// Runs `program args` with `envs` under the generated profile.
    fn sandboxed(&self, program: &str, args: &[OsString], envs: &[(&str, &OsStr)]) -> Output {
        let profile = Profile::for_policy(&self.policy()).expect("profile");
        let argv = profile.argv(AbsPath::parse(program).expect("program"), args);
        Command::new(&argv[0])
            .args(&argv[1..])
            .envs(envs.iter().copied())
            .output()
            .expect("sandbox-exec starts")
    }

    /// Runs this test binary as the child `role`, which makes `dst` from the secret, with the
    /// sandbox when `sandboxed` and without it as the control.
    fn child(&self, role: &str, dst: &Path, sandboxed: bool) -> Output {
        let exe = std::env::current_exe().expect("this binary's path");
        let args = args(&[
            "--exact",
            "child_of_the_links",
            "--nocapture",
            "--test-threads=1",
        ]);
        let envs: [(&str, &OsStr); 3] = [
            (ROLE, OsStr::new(role)),
            (SRC, self.secret.as_os_str()),
            (DST, dst.as_os_str()),
        ];
        if sandboxed {
            self.sandboxed(exe.to_str().expect("a UTF-8 path"), &args, &envs)
        } else {
            Command::new(&exe)
                .args(&args)
                .envs(envs.iter().copied())
                .output()
                .expect("control starts")
        }
    }

    /// Asserts that `out` does not hold the secret anywhere.
    fn assert_not_leaked(&self, out: &Output, what: &str) {
        assert!(
            !text(out).contains(&self.token),
            "{what} leaked: {}",
            describe(out)
        );
    }
}

impl Drop for Scene {
    fn drop(&mut self) {
        // Best effort: the runner is discarded anyway.
        let _ = std::fs::remove_dir_all(&self.base);
    }
}

fn args(list: &[&str]) -> Vec<OsString> {
    list.iter().map(OsString::from).collect()
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

/// Everything `out` printed.
fn text(out: &Output) -> String {
    format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    )
}

fn describe(out: &Output) -> String {
    format!("{:?} {}", out.status, text(out))
}

/// The `LINK made=...` text a child printed, from wherever it starts on its line (libtest
/// prints `test <name> ... ` before the test's first output, on the same line); fails when
/// there is none, so a child that never ran cannot pass for a refusal.
fn verdict(out: &Output) -> String {
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .find_map(|line| line.find("LINK made=").map(|at| line[at..].to_owned()))
        .unwrap_or_else(|| panic!("the child printed no verdict: {}", describe(out)))
}

/// The errno of `error`, or -1.
fn errno(error: &std::io::Error) -> i32 {
    error.raw_os_error().unwrap_or(-1)
}

/// What a child role prints: whether it made the new name, and what reading it gave.
fn report(made: std::io::Result<()>, dst: &Path) {
    let made = match made {
        Ok(()) => "yes".to_owned(),
        Err(error) => format!("no:{}", errno(&error)),
    };
    let read = match std::fs::read(dst) {
        Ok(bytes) => String::from_utf8_lossy(&bytes).into_owned(),
        Err(error) => format!("refused:{}", errno(&error)),
    };
    println!("LINK made={made} read={read}");
}

/// A clone of `src` named `dst`, through the tree store's safe binding of `clonefileat(2)`
/// (`js_treestore::mac::Clonefile`, which clones a file as well as a directory). Its source
/// is a path, so the clone is tried without a read of the source first, as an attacker's own
/// call would be (a tool such as `cp -c` reads the source's metadata first and would be
/// refused before it tried).
fn clone_to(src: &Path, dst: &Path) -> std::io::Result<()> {
    let parent = dst
        .parent()
        .ok_or_else(|| std::io::Error::other("the destination has no directory"))?;
    let name = dst
        .file_name()
        .ok_or_else(|| std::io::Error::other("the destination has no name"))?;
    let dir = std::fs::File::open(parent)?;
    Clonefile.clone_dir(src, dir.as_fd(), name)
}

/// What a test does when it starts this binary as a child: nothing, unless the role says.
/// Not `#[ignore]`: it is a no-op without the role, and the runs name it exactly.
#[test]
fn child_of_the_links() {
    let Some(role) = std::env::var_os(ROLE) else {
        return;
    };
    let src = PathBuf::from(std::env::var_os(SRC).expect("the source"));
    let dst = PathBuf::from(std::env::var_os(DST).expect("the destination"));
    match role.to_str() {
        Some("link") => report(std::fs::hard_link(&src, &dst), &dst),
        Some("clone") => report(clone_to(&src, &dst), &dst),
        other => panic!("unknown role {other:?}"),
    }
}

/// Asserts that a read-denied file read by its own path prints nothing and fails under the
/// profile, and that the same read without the sandbox prints the secret.
///
/// Catches a read deny that does not name the directory, which every other test relies on.
#[test]
fn a_direct_read_of_a_denied_file_is_refused() {
    let scene = Scene::new("direct");
    let cat = [os(&scene.secret)];
    let open = control("/bin/cat", &cat);
    assert_eq!(
        String::from_utf8_lossy(&open.stdout),
        scene.token,
        "{}",
        describe(&open)
    );
    let out = scene.sandboxed("/bin/cat", &cat, &[]);
    scene.assert_not_leaked(&out, "a direct read");
    assert!(!out.status.success(), "{}", describe(&out));
}

/// Asserts that a symbolic link to the denied file, made in the run directory from outside
/// the sandbox, cannot be read through under the profile, and that it can be without it.
///
/// Catches a deny matched against the name given rather than the resolved path.
#[test]
fn a_symbolic_link_to_a_denied_file_cannot_be_read_through() {
    let scene = Scene::new("symlink");
    let link = scene.run.join("sym");
    std::os::unix::fs::symlink(&scene.secret, &link).expect("the symbolic link");
    let cat = [os(&link)];
    let open = control("/bin/cat", &cat);
    assert_eq!(
        String::from_utf8_lossy(&open.stdout),
        scene.token,
        "{}",
        describe(&open)
    );
    let out = scene.sandboxed("/bin/cat", &cat, &[]);
    scene.assert_not_leaked(&out, "a symbolic link");
    assert!(!out.status.success(), "{}", describe(&out));
}

/// Asserts that a sandboxed process which hard-links the denied file into the run directory
/// never reads the secret, either because the link is refused or because the read through it
/// is, and that the same process without the sandbox makes the link and reads the secret.
///
/// Catches a read deny without a link deny: the new name is a path no deny names.
#[test]
fn a_hard_link_to_a_denied_file_is_refused_or_cannot_be_read() {
    let scene = Scene::new("hard");
    let open_dst = scene.run.join("hard-control");
    let open = scene.child("link", &open_dst, false);
    assert!(
        verdict(&open).contains(&format!("made=yes read={}", scene.token)),
        "{}",
        describe(&open)
    );
    let dst = scene.run.join("hard");
    let out = scene.child("link", &dst, true);
    let line = verdict(&out);
    scene.assert_not_leaked(&out, "a hard link");
    assert!(
        !line.contains("made=yes") || line.contains("read=refused"),
        "{line}"
    );
    let _ = std::fs::remove_file(&dst);
    let _ = std::fs::remove_file(&open_dst);
}

/// Asserts that a sandboxed process which clones the denied file into the run directory
/// (`clonefileat`) never reads the secret, and that the same process without the sandbox makes
/// the clone and reads it. The control panics, with its reason, on a volume that is not APFS.
///
/// Catches a read deny without a clone deny: the copy is a path no deny names.
#[test]
fn a_clone_of_a_denied_file_is_refused_or_cannot_be_read() {
    let scene = Scene::new("clone");
    let open_dst = scene.run.join("clone-control");
    let open = scene.child("clone", &open_dst, false);
    let line = verdict(&open);
    assert!(
        line.contains(&format!("made=yes read={}", scene.token)),
        "the control cannot clone (the volume is not APFS, or clonefile is refused): {line}"
    );
    let dst = scene.run.join("clone");
    let out = scene.child("clone", &dst, true);
    let line = verdict(&out);
    scene.assert_not_leaked(&out, "a clone");
    assert!(
        !line.contains("made=yes") || line.contains("read=refused"),
        "{line}"
    );
    let _ = std::fs::remove_file(&dst);
    let _ = std::fs::remove_file(&open_dst);
}

/// Asserts that the volume-id path of the denied file (`/.vol/<device>/<inode>`) is not
/// readable under the profile, and is without it.
///
/// Catches a deny that the file-id path walks around.
#[test]
fn the_volume_id_path_of_a_denied_file_cannot_be_read() {
    let scene = Scene::new("volfs");
    let meta = std::fs::metadata(&scene.secret).expect("the secret's metadata");
    let by_id = PathBuf::from(format!("/.vol/{}/{}", meta.dev(), meta.ino()));
    let cat = [os(&by_id)];
    let open = control("/bin/cat", &cat);
    assert_eq!(
        String::from_utf8_lossy(&open.stdout),
        scene.token,
        "{}",
        describe(&open)
    );
    let out = scene.sandboxed("/bin/cat", &cat, &[]);
    scene.assert_not_leaked(&out, "the volume-id path");
    assert!(!out.status.success(), "{}", describe(&out));
}

/// Asserts that the denied file named with the case of its last component changed is not
/// readable under the profile, where the volume reads it without. On a case-sensitive volume
/// the other spelling names no file; the test says so and returns.
///
/// Catches a deny that matches the spelling it was given instead of the file.
#[test]
fn another_case_spelling_of_a_denied_file_cannot_be_read() {
    let scene = Scene::new("case");
    let other = scene.outside.join("SECRET");
    let cat = [os(&other)];
    let open = control("/bin/cat", &cat);
    if String::from_utf8_lossy(&open.stdout) != scene.token {
        println!("the volume is case-sensitive: the other spelling names no file");
        return;
    }
    let out = scene.sandboxed("/bin/cat", &cat, &[]);
    scene.assert_not_leaked(&out, "the other case spelling");
    assert!(!out.status.success(), "{}", describe(&out));
}

/// Asserts that moving the denied file into the run directory fails under the profile and
/// leaves the file in place with its content. There is no control: the file's presence after
/// the move is the check, since a move that worked would leave it elsewhere.
///
/// Catches a read-denied file that a run can take out of its place and read at the new one.
#[test]
fn a_denied_file_cannot_be_moved_into_the_run_directory() {
    let scene = Scene::new("rename");
    let moved = scene.run.join("moved");
    let out = scene.sandboxed("/bin/mv", &[os(&scene.secret), os(&moved)], &[]);
    assert!(!out.status.success(), "{}", describe(&out));
    assert!(!moved.exists(), "{}", describe(&out));
    assert_eq!(
        std::fs::read_to_string(&scene.secret).expect("the secret"),
        scene.token
    );
}
