//! Scratch probes of the generated profile under the real `sandbox-exec`. Each test
//! asserts the outcome a build sandbox wants; a failure names a gap.

#![cfg(target_os = "macos")]

use std::ffi::OsString;
use std::os::unix::net::UnixListener;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use js_mac_pool::seatbelt::{AbsPath, Caps, Net, Policy, Ports, Profile, ReadModel, SlotLayout};

struct Scene {
    base: PathBuf,
    run: PathBuf,
    input: PathBuf,
    outside: PathBuf,
    uid_entry: PathBuf,
}

impl Scene {
    fn new(tag: &str) -> Self {
        let root = Path::new(env!("CARGO_TARGET_TMPDIR"));
        let raw = root.join(format!("probe-{tag}-{}", std::process::id()));
        std::fs::create_dir_all(raw.join("run/in")).expect("run");
        std::fs::create_dir_all(raw.join("outside")).expect("outside");
        let temp = Command::new("/usr/bin/getconf")
            .arg("DARWIN_USER_TEMP_DIR")
            .output()
            .expect("getconf");
        let temp = String::from_utf8(temp.stdout).expect("utf8");
        let temp = PathBuf::from(temp.trim()).canonicalize().expect("temp");
        let base = raw.canonicalize().expect("base");
        Self {
            run: base.join("run"),
            input: base.join("run/in"),
            outside: base.join("outside"),
            uid_entry: temp.parent().expect("entry").to_owned(),
            base,
        }
    }

    fn policy(&self, net: Net, inputs: Vec<AbsPath>, denies: Vec<AbsPath>) -> Policy {
        let layout = SlotLayout {
            run: AbsPath::new(&self.run).expect("run"),
            uid_entry: AbsPath::new(&self.uid_entry).expect("entry"),
            inputs,
            read_denies: denies,
        };
        let caps = Caps {
            net,
            read_model: ReadModel::Open,
        };
        Policy::base(layout, caps).expect("policy")
    }

    fn plain(&self, net: Net) -> Policy {
        self.policy(net, vec![AbsPath::new(&self.input).expect("input")], vec![])
    }

    fn sandboxed(&self, policy: &Policy, program: &str, args: &[&str]) -> Output {
        let profile = Profile::for_policy(policy).expect("profile");
        let args: Vec<OsString> = args.iter().map(OsString::from).collect();
        let argv = profile.argv(&AbsPath::parse(program).expect("program"), &args);
        Command::new(&argv[0])
            .args(&argv[1..])
            .output()
            .expect("sandbox-exec starts")
    }
}

impl Drop for Scene {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.base);
    }
}

fn control(program: &str, args: &[&str]) -> Output {
    Command::new(program)
        .args(args)
        .output()
        .expect("control starts")
}

fn describe(out: &Output) -> String {
    format!("{:?} {}", out.status, String::from_utf8_lossy(&out.stderr))
}

const LISTEN: &str = r#"use IO::Socket::INET; exit(IO::Socket::INET->new(Listen => 1, LocalAddr => "127.0.0.1", LocalPort => 0) ? 0 : 1)"#;
const RESOLVE: &str = r#"exit(gethostbyname("example.com") ? 0 : 1)"#;
const UNIX_CONNECT: &str =
    r#"use IO::Socket::UNIX; exit(IO::Socket::UNIX->new(Peer => $ARGV[0]) ? 0 : 1)"#;

fn listens(scene: &Scene, net: Net) -> bool {
    let c = control("/usr/bin/perl", &["-e", LISTEN]);
    assert!(c.status.success(), "control listen: {}", describe(&c));
    let policy = scene.plain(net);
    scene
        .sandboxed(&policy, "/usr/bin/perl", &["-e", LISTEN])
        .status
        .success()
}

fn resolves(scene: &Scene, net: Net) -> bool {
    let c = control("/usr/bin/perl", &["-e", RESOLVE]);
    assert!(c.status.success(), "control resolve: {}", describe(&c));
    let policy = scene.plain(net);
    scene
        .sandboxed(&policy, "/usr/bin/perl", &["-e", RESOLVE])
        .status
        .success()
}

#[test]
fn probe_hardlink_alias_does_not_make_an_input_writable() {
    let scene = Scene::new("link");
    let data = scene.input.join("data");
    std::fs::write(&data, "orig\n").expect("data");
    let alias = scene.run.join("alias");
    let alias_text = alias.to_str().expect("utf8");
    let data_text = data.to_str().expect("utf8");
    let policy = scene.plain(Net::Off);
    let linked = scene.sandboxed(&policy, "/bin/ln", &[data_text, alias_text]);
    let appended = scene.sandboxed(
        &policy,
        "/bin/sh",
        &["-c", r#"echo changed >> "$0""#, alias_text],
    );
    let now = std::fs::read_to_string(&data).expect("read");
    assert_eq!(
        now,
        "orig\n",
        "ln: {} append: {}",
        describe(&linked),
        describe(&appended)
    );
}

#[test]
fn probe_off_refuses_a_listener() {
    let scene = Scene::new("off-listen");
    assert!(!listens(&scene, Net::Off));
}

#[test]
fn probe_ports_refuses_a_listener() {
    let scene = Scene::new("ports-listen");
    let ports = Ports::new(&[9]).expect("ports");
    assert!(!listens(&scene, Net::Ports(ports)));
}

#[test]
fn probe_loopback_allows_a_listener() {
    let scene = Scene::new("lo-listen");
    assert!(listens(&scene, Net::Loopback));
}

#[test]
fn probe_off_refuses_name_resolution() {
    let scene = Scene::new("off-dns");
    assert!(!resolves(&scene, Net::Off));
}

#[test]
fn probe_loopback_refuses_name_resolution() {
    let scene = Scene::new("lo-dns");
    assert!(!resolves(&scene, Net::Loopback));
}

#[test]
fn probe_on_resolves_names() {
    let scene = Scene::new("on-dns");
    assert!(resolves(&scene, Net::On));
}

#[test]
fn probe_launchctl_submit_is_refused() {
    let scene = Scene::new("launchctl");
    let label = format!("js.probe.{}", std::process::id());
    let args = ["submit", "-l", label.as_str(), "--", "/usr/bin/true"];
    let policy = scene.plain(Net::Off);
    let sandboxed = scene.sandboxed(&policy, "/bin/launchctl", &args);
    let unsandboxed = control("/bin/launchctl", &args);
    let _ = control("/bin/launchctl", &["remove", label.as_str()]);
    assert!(
        !sandboxed.status.success(),
        "sandboxed: {} (control: {})",
        describe(&sandboxed),
        describe(&unsandboxed)
    );
}

#[test]
fn probe_an_input_spelled_in_another_case_is_still_protected() {
    let scene = Scene::new("case");
    let upper = scene.input.to_str().expect("utf8").to_ascii_uppercase();
    let inputs = vec![AbsPath::parse(&upper).expect("upper")];
    let policy = scene.policy(Net::Off, inputs, vec![]);
    let target = scene.input.join("written");
    let out = scene.sandboxed(&policy, "/usr/bin/touch", &[target.to_str().expect("utf8")]);
    assert!(!target.exists(), "input written: {}", describe(&out));
}

#[test]
fn probe_a_read_denied_directory_blocks_connecting_to_its_unix_socket_under_on() {
    let scene = Scene::new("unix");
    let socket = scene.outside.join("sock");
    let _listener = UnixListener::bind(&socket).expect("bind");
    let socket_text = socket.to_str().expect("utf8");
    let c = control("/usr/bin/perl", &["-e", UNIX_CONNECT, socket_text]);
    assert!(c.status.success(), "control connect: {}", describe(&c));
    let denies = vec![AbsPath::new(&scene.outside).expect("outside")];
    let policy = scene.policy(Net::On, vec![], denies);
    let out = scene.sandboxed(&policy, "/usr/bin/perl", &["-e", UNIX_CONNECT, socket_text]);
    assert!(!out.status.success(), "connected under On");
}

#[test]
fn probe_a_read_denied_path_inside_the_run_directory_is_not_writable() {
    let scene = Scene::new("rdwrite");
    let secret = scene.run.join("secret");
    std::fs::create_dir_all(&secret).expect("secret");
    let denies = vec![AbsPath::new(&secret).expect("secret")];
    let policy = scene.policy(Net::Off, vec![], denies);
    let target = secret.join("written");
    let out = scene.sandboxed(&policy, "/usr/bin/touch", &[target.to_str().expect("utf8")]);
    assert!(
        !target.exists(),
        "write into a read-denied path: {}",
        describe(&out)
    );
}
