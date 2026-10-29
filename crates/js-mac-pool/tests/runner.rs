//! The generated profile under the real `sandbox-exec` on a macOS runner, as the runner's
//! own user. Each refusal has a control: the same step without the sandbox must succeed,
//! or the refusal proves nothing.

#![cfg(target_os = "macos")]

use std::ffi::OsString;
use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use js_mac_pool::seatbelt::{
    AbsPath, Caps, Narrowing, Net, Policy, Ports, Profile, ReadModel, SlotLayout,
};

/// A scratch tree under the build's own target directory: `run/` (granted), `run/in/`
/// (protected input) and `outside/` (granted to nobody). Removed when dropped.
struct Scene {
    base: PathBuf,
    run: PathBuf,
    input: PathBuf,
    outside: PathBuf,
    uid_entry: PathBuf,
}

impl Scene {
    /// `run_name` is the name of the run directory, so a test can give it a hostile one.
    fn new(tag: &str, run_name: &str) -> Self {
        let root = Path::new(env!("CARGO_TARGET_TMPDIR"));
        let base = root.join(format!("mac-pool-{tag}-{}", std::process::id()));
        let run = base.join(run_name);
        let input = run.join("in");
        let outside = base.join("outside");
        std::fs::create_dir_all(&input).expect("run directory");
        std::fs::create_dir_all(&outside).expect("outside directory");
        let temp = Command::new("/usr/bin/getconf")
            .arg("DARWIN_USER_TEMP_DIR")
            .output()
            .expect("getconf");
        assert!(temp.status.success(), "getconf DARWIN_USER_TEMP_DIR");
        let temp = PathBuf::from(String::from_utf8(temp.stdout).expect("utf8").trim());
        let temp = temp.canonicalize().expect("the user temp directory exists");
        let uid_entry = temp.parent().expect("the user entry").to_owned();
        Self {
            base: base.canonicalize().expect("base"),
            run: run.canonicalize().expect("run"),
            input: input.canonicalize().expect("input"),
            outside: outside.canonicalize().expect("outside"),
            uid_entry,
        }
    }

    fn policy(&self, net: Net) -> Policy {
        let layout = SlotLayout {
            run: AbsPath::new(&self.run).expect("run path"),
            uid_entry: AbsPath::new(&self.uid_entry).expect("entry path"),
            inputs: vec![AbsPath::new(&self.input).expect("input path")],
            read_denies: vec![],
        };
        Policy::base(
            layout,
            Caps {
                net,
                read_model: ReadModel::Open,
            },
        )
        .expect("policy")
    }

    /// Runs `program args` under the generated profile of `net`.
    fn sandboxed(&self, net: Net, program: &str, args: &[OsString]) -> Output {
        self.sandboxed_with(&self.policy(net), program, args)
    }

    fn sandboxed_with(&self, policy: &Policy, program: &str, args: &[OsString]) -> Output {
        let profile = Profile::for_policy(policy).expect("profile");
        let argv = profile.argv(&AbsPath::parse(program).expect("program"), args);
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

fn args(list: &[&str]) -> Vec<OsString> {
    list.iter().map(OsString::from).collect()
}

fn control(program: &str, args: &[OsString]) -> Output {
    Command::new(program)
        .args(args)
        .output()
        .expect("control starts")
}

fn describe(out: &Output) -> String {
    format!("{:?} {}", out.status, String::from_utf8_lossy(&out.stderr))
}

fn touch(target: &Path) -> Vec<OsString> {
    vec![os(target)]
}

#[test]
fn writes_are_granted_only_where_the_policy_grants() {
    let scene = Scene::new("writes", "run");
    let pid = std::process::id();
    let granted = scene.run.join("granted");
    let entry = scene.uid_entry.join(format!("js-mac-pool-{pid}"));
    let in_input = scene.input.join("protected");
    let outside = scene.outside.join("elsewhere");
    let shared_tmp = Path::new("/private/tmp").join(format!("js-mac-pool-{pid}"));

    // The controls: every place below that the sandbox refuses can be written without it.
    for target in [&outside, &shared_tmp, &in_input] {
        let out = control("/usr/bin/touch", &touch(target));
        assert!(
            out.status.success(),
            "control {target:?}: {}",
            describe(&out)
        );
        std::fs::remove_file(target).expect("remove the control's file");
    }

    for target in [&granted, &entry] {
        let out = scene.sandboxed(Net::Off, "/usr/bin/touch", &touch(target));
        assert!(
            out.status.success(),
            "granted {target:?}: {}",
            describe(&out)
        );
        assert!(target.exists());
        std::fs::remove_file(target).expect("remove the granted file");
    }
    for target in [&in_input, &outside, &shared_tmp] {
        let out = scene.sandboxed(Net::Off, "/usr/bin/touch", &touch(target));
        assert!(!out.status.success(), "refused {target:?}");
        assert!(!target.exists(), "{target:?} was created");
    }
}

#[test]
fn a_policy_without_the_user_folder_grant_cannot_write_there() {
    let scene = Scene::new("dropped", "run");
    let entry = scene
        .uid_entry
        .join(format!("js-mac-pool-drop-{}", std::process::id()));
    let policy = scene
        .policy(Net::Off)
        .narrowed(Narrowing {
            drop_uid_entry: true,
            ..Narrowing::default()
        })
        .expect("narrowed");
    let out = scene.sandboxed_with(&policy, "/usr/bin/touch", &touch(&entry));
    assert!(!out.status.success(), "refused");
    assert!(!entry.exists());
}

#[test]
fn preference_writes_are_refused_with_a_control() {
    let scene = Scene::new("defaults", "run");
    let domain = format!("com.komira.js-mac-pool-test-{}", std::process::id());
    let write = args(&["write", &domain, "key", "1"]);
    let read = args(&["read", &domain]);
    let delete = args(&["delete", &domain]);

    let refused = scene.sandboxed(Net::Off, "/usr/bin/defaults", &write);
    let landed_sandboxed = control("/usr/bin/defaults", &read).status.success();
    // Clean up before asserting, so a failure leaves nothing behind.
    let _ = control("/usr/bin/defaults", &delete);

    let written = control("/usr/bin/defaults", &write);
    let landed_control = control("/usr/bin/defaults", &read).status.success();
    let _ = control("/usr/bin/defaults", &delete);

    assert!(
        written.status.success(),
        "control write: {}",
        describe(&written)
    );
    assert!(landed_control, "the control's write landed");
    assert!(
        !refused.status.success(),
        "sandboxed write: {}",
        describe(&refused)
    );
    assert!(!landed_sandboxed, "no preference reached the domain");
}

/// `nc -z 127.0.0.1 PORT` exits 0 when the connect succeeds.
fn connect_to(port: u16) -> Vec<OsString> {
    args(&["-z", "127.0.0.1", &port.to_string()])
}

#[test]
fn each_network_level_allows_what_it_names_and_refuses_the_rest() {
    let scene = Scene::new("net", "run");
    let one = TcpListener::bind("127.0.0.1:0").expect("listener");
    let two = TcpListener::bind("127.0.0.1:0").expect("listener");
    let port_one = one.local_addr().expect("addr").port();
    let port_two = two.local_addr().expect("addr").port();
    let to_one = connect_to(port_one);
    let to_two = connect_to(port_two);

    let control_out = control("/usr/bin/nc", &to_one);
    assert!(
        control_out.status.success(),
        "control connect: {}",
        describe(&control_out)
    );

    let connects =
        |net: Net, to: &[OsString]| scene.sandboxed(net, "/usr/bin/nc", to).status.success();
    let only_one = Net::Ports(Ports::new(&[port_one]).expect("ports"));
    assert!(!connects(Net::Off, &to_one), "Off refuses loopback");
    assert!(
        connects(only_one.clone(), &to_one),
        "a listed port connects"
    );
    assert!(!connects(only_one, &to_two), "an unlisted port is refused");
    assert!(connects(Net::Loopback, &to_one), "Loopback connects");
    assert!(
        connects(Net::Loopback, &to_two),
        "Loopback connects to any port"
    );
    assert!(connects(Net::On, &to_one), "On connects");
}

#[test]
fn a_hostile_run_directory_name_grants_exactly_that_directory() {
    let scene = Scene::new(
        "hostile",
        "r\"u)n (x) =;(allow file-write* (subpath \"x\"))",
    );
    let inside = scene.run.join("granted");
    let outside = scene.outside.join("elsewhere");
    let out = scene.sandboxed(Net::Off, "/usr/bin/touch", &touch(&inside));
    assert!(
        out.status.success(),
        "the named directory is granted: {}",
        describe(&out)
    );
    let out = scene.sandboxed(Net::Off, "/usr/bin/touch", &touch(&outside));
    assert!(!out.status.success(), "nothing else is granted");
    assert!(!outside.exists());
}

const HELLO_C: &str = "#include <stdio.h>\nint main(void) { puts(\"hello\"); return 0; }\n";

/// Builds and runs a C hello, in the sandbox or not.
fn hello(scene: &Scene, sandboxed: bool) {
    let source = scene.run.join("hello.c");
    std::fs::write(&source, HELLO_C).expect("source");
    let exe = scene.run.join(if sandboxed {
        "hello_sandboxed"
    } else {
        "hello_control"
    });
    let build = [os(&source), OsString::from("-o"), os(&exe)];
    let exe_text = exe.to_str().expect("utf8");
    let (built, ran) = if sandboxed {
        let built = scene.sandboxed(Net::Off, "/usr/bin/clang", &build);
        (built, scene.sandboxed(Net::Off, exe_text, &[]))
    } else {
        (control("/usr/bin/clang", &build), control(exe_text, &[]))
    };
    assert!(
        built.status.success(),
        "clang (sandboxed: {sandboxed}): {}",
        describe(&built)
    );
    assert!(
        ran.status.success(),
        "run (sandboxed: {sandboxed}): {}",
        describe(&ran)
    );
    assert_eq!(String::from_utf8_lossy(&ran.stdout), "hello\n");
}

/// The control first: if the toolchain does not work here without the sandbox, the
/// sandboxed run proves nothing.
#[test]
fn a_c_toolchain_smoke_runs_under_the_profile_and_in_the_control() {
    let scene = Scene::new("clang", "run");
    hello(&scene, false);
    hello(&scene, true);
}

/// Seatbelt takes only `*` and `localhost` as the host of a `remote ip` filter, so the
/// profile cannot deny a literal address; the builder offers no such rule.
#[test]
fn the_profile_compiler_refuses_a_literal_address_in_a_remote_ip_filter() {
    let out = control(
        "/usr/bin/sandbox-exec",
        &args(&[
            "-p",
            "(version 1)(allow default)(deny network-outbound (remote ip \"10.0.0.1:*\"))",
            "/usr/bin/true",
        ]),
    );
    assert!(!out.status.success(), "compiled: {}", describe(&out));
    let stderr = String::from_utf8_lossy(&out.stderr).into_owned();
    assert!(stderr.contains("host must be * or localhost"), "{stderr}");
}
