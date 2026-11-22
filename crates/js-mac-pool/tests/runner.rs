//! The generated profile applied by js-agent's `__job-start` on a macOS runner, as the
//! runner's own user. Each refusal has a control: the same step without the sandbox must succeed,
//! or the refusal proves nothing.

#![cfg(target_os = "macos")]

mod common;

use std::ffi::OsString;
use std::net::{TcpListener, UdpSocket};
use std::os::unix::net::UnixListener;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use common::start_program;
use js_mac_pool::seatbelt::{
    AbsPath, Caps, Narrowing, Net, Policy, Ports, Profile, ReadModel, START_VERB, SlotLayout,
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
        self.policy_denying(net, &[])
    }

    /// The policy of `net` that also denies reads of `denies`.
    fn policy_denying(&self, net: Net, denies: &[&Path]) -> Policy {
        let layout = SlotLayout {
            trees: None,
            run: AbsPath::new(&self.run).expect("run path"),
            uid_entry: Some(AbsPath::new(&self.uid_entry).expect("entry path")),
            inputs: vec![AbsPath::new(&self.input).expect("input path")],
            read_denies: denies
                .iter()
                .map(|path| AbsPath::new(path).expect("read-denied path"))
                .collect(),
            mounts: vec![],
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
        let argv = profile.start_argv(
            &start_program(),
            AbsPath::parse(program).expect("program"),
            args,
        );
        Command::new(&argv[0])
            .args(&argv[1..])
            .output()
            .expect("js-agent starts")
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

/// Connects to `127.0.0.1:PORT` and exits 0 when it connects. When it does not, it prints
/// the errno on stderr and exits 1: a sandbox refusal is `EPERM`, a closed port is
/// `ECONNREFUSED`, so a refusal is told apart from a listener that is not there.
const CONNECT: &str = r#"use IO::Socket::INET; my $s = IO::Socket::INET->new(PeerAddr => "127.0.0.1", PeerPort => $ARGV[0]); exit 0 if $s; print STDERR ($!+0); exit 1"#;
const EPERM: &str = "1";

/// A listener that accepts every connection on its own thread, so a connect completes
/// by the handshake and never waits in an accept queue.
fn accepting_listener() -> u16 {
    accepting_listener_on("127.0.0.1:0")
}

fn accepting_listener_on(address: &str) -> u16 {
    let listener = TcpListener::bind(address).expect("listener");
    let port = listener.local_addr().expect("addr").port();
    std::thread::spawn(move || for _connection in listener.incoming() {});
    port
}

fn connect_to(port: u16) -> Vec<OsString> {
    perl(CONNECT, &[OsString::from(port.to_string())])
}

/// How many times each connect is tried.
///
/// Measured on the hosted macOS runner (CI runs 41073976740, 41076930103 and 41081525386):
/// at `Ports` and `Loopback` the sandbox answered about one allowed connect in 400 with
/// `EPERM` (1 of 400 at each level in the second run, 1 of 300 in the first), at `On` none
/// in 400, and 600 processes of 10 connects each had no failure. The cause is not known.
/// An allow therefore needs at least `ALLOW_MIN` of `TRIES` connects to succeed and every
/// failure to be exactly `EPERM`; a profile that allowed fewer would fail, and so would any
/// other error. A refusal has no tolerance: every try is refused with `EPERM`.
const TRIES: usize = 8;
const ALLOW_MIN: usize = 7;

fn allowed(scene: &Scene, net: &Net, to: &[OsString]) -> bool {
    let mut connected = 0;
    for _ in 0..TRIES {
        let out = scene.sandboxed(net.clone(), "/usr/bin/perl", to);
        if out.status.success() {
            connected += 1;
        } else if String::from_utf8_lossy(&out.stderr).trim() != EPERM {
            return false;
        }
    }
    connected >= ALLOW_MIN
}

fn refused(scene: &Scene, net: &Net, to: &[OsString]) -> bool {
    (0..TRIES).all(|_| {
        let out = scene.sandboxed(net.clone(), "/usr/bin/perl", to);
        !out.status.success() && String::from_utf8_lossy(&out.stderr).trim() == EPERM
    })
}

#[test]
fn each_network_level_allows_what_it_names_and_refuses_the_rest() {
    let scene = Scene::new("net", "run");
    let port_one = accepting_listener();
    let port_two = accepting_listener();
    let to_one = connect_to(port_one);
    let to_two = connect_to(port_two);

    let control_out = control("/usr/bin/perl", &to_one);
    assert!(
        control_out.status.success(),
        "control connect: {}",
        describe(&control_out)
    );

    let only_one = Net::Ports(Ports::new(&[port_one]).expect("ports"));
    assert!(refused(&scene, &Net::Off, &to_one), "Off refuses loopback");
    assert!(
        allowed(&scene, &only_one, &to_one),
        "a listed port connects"
    );
    assert!(
        refused(&scene, &only_one, &to_two),
        "an unlisted port is refused"
    );
    assert!(
        allowed(&scene, &Net::Loopback, &to_one),
        "Loopback connects"
    );
    assert!(
        allowed(&scene, &Net::Loopback, &to_two),
        "Loopback connects to any port"
    );
    assert!(allowed(&scene, &Net::On, &to_one), "On connects");
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
/// profile cannot deny a literal address; the builder offers no such rule. The text goes
/// to the start mode, which refuses it with the compiler's words and exits 125, and does
/// not run the program.
#[test]
fn the_profile_compiler_refuses_a_literal_address_in_a_remote_ip_filter() {
    let start = start_program();
    let out = control(
        start.as_str(),
        &args(&[
            START_VERB,
            "--profile",
            "(version 1)(allow default)(deny network-outbound (remote ip \"10.0.0.1:*\"))",
            "--",
            "/usr/bin/true",
        ]),
    );
    assert_eq!(out.status.code(), Some(125), "{}", describe(&out));
    let stderr = String::from_utf8_lossy(&out.stderr).into_owned();
    assert!(
        stderr.starts_with("js-agent: the Seatbelt profile was refused: "),
        "{stderr}"
    );
    assert!(stderr.contains("host must be * or localhost"), "{stderr}");
}

/// Perl one-liners: the runner has `/usr/bin/perl`, and each prints nothing and reports
/// by exit status.
const LISTEN: &str = r#"use IO::Socket::INET; exit(IO::Socket::INET->new(Listen => 1, LocalAddr => "127.0.0.1", LocalPort => 0) ? 0 : 1)"#;
const UNIX_CONNECT: &str =
    r#"use IO::Socket::UNIX; exit(IO::Socket::UNIX->new(Peer => $ARGV[0]) ? 0 : 1)"#;

fn perl(script: &str, rest: &[OsString]) -> Vec<OsString> {
    let mut all = vec![OsString::from("-e"), OsString::from(script)];
    all.extend(rest.iter().cloned());
    all
}

fn listens_under(scene: &Scene, net: Net) -> bool {
    scene
        .sandboxed(net, "/usr/bin/perl", &perl(LISTEN, &[]))
        .status
        .success()
}

/// `dns-sd -V` asks the system resolver daemon for its version over its unix socket: a
/// local call, so no name is looked up and no network is needed. It exits 0 either way;
/// the answer is on stdout and the refusal is `DNSServiceGetProperty failed` on stderr.
const RESOLVER_VERSION: &str = "Currently running daemon";

fn resolver_answers(out: &Output) -> bool {
    String::from_utf8_lossy(&out.stdout).contains(RESOLVER_VERSION)
}

fn resolver_under(scene: &Scene, net: Net) -> Output {
    scene.sandboxed(net, "/usr/bin/dns-sd", &args(&["-V"]))
}

#[test]
fn listeners_are_allowed_on_loopback_only() {
    let scene = Scene::new("listen", "run");
    let free = control("/usr/bin/perl", &perl(LISTEN, &[]));
    assert!(free.status.success(), "control listen: {}", describe(&free));
    assert!(listens_under(&scene, Net::Loopback), "Loopback listens");
    assert!(!listens_under(&scene, Net::Off), "Off refuses a listener");
    let ports = Net::Ports(Ports::new(&[9]).expect("ports"));
    assert!(!listens_under(&scene, ports), "Ports refuses a listener");
}

#[test]
fn the_name_resolver_is_reachable_under_on_and_remote_only() {
    let scene = Scene::new("dns", "run");
    let free = control("/usr/bin/dns-sd", &args(&["-V"]));
    assert!(
        resolver_answers(&free),
        "control resolver: {}",
        describe(&free)
    );
    let on = resolver_under(&scene, Net::On);
    assert!(
        resolver_answers(&on),
        "On reaches the resolver: {}",
        describe(&on)
    );
    let remote = Net::Remote(Ports::new(&[443]).expect("ports"));
    let out = resolver_under(&scene, remote);
    assert!(
        resolver_answers(&out),
        "Remote reaches the resolver: {}",
        describe(&out)
    );
    for (name, net) in [("Off", Net::Off), ("Loopback", Net::Loopback)] {
        let out = resolver_under(&scene, net);
        assert!(!resolver_answers(&out), "{name} reached the resolver");
        let stderr = String::from_utf8_lossy(&out.stderr).into_owned();
        assert!(
            stderr.contains("DNSServiceGetProperty failed"),
            "{name} refuses the resolver: {stderr}"
        );
    }
}

/// Connects to `HOST:PORT` and reports as [`CONNECT`] does.
const CONNECT_HOST: &str = r#"use IO::Socket::INET; my $s = IO::Socket::INET->new(PeerAddr => $ARGV[0], PeerPort => $ARGV[1]); exit 0 if $s; print STDERR ($!+0); exit 1"#;

/// The address this Mac would use to reach another network. A UDP `connect` only picks
/// the route, so nothing is sent.
fn own_address() -> String {
    let probe = UdpSocket::bind("0.0.0.0:0").expect("a UDP socket");
    probe
        .connect("192.0.2.1:9")
        .expect("the Mac has a route out");
    probe.local_addr().expect("addr").ip().to_string()
}

fn connect_to_host(host: &str, port: u16) -> Vec<OsString> {
    perl(
        CONNECT_HOST,
        &[OsString::from(host), OsString::from(port.to_string())],
    )
}

/// Under `Remote(ports)` a run connects to a listed port of an address that is not
/// loopback, to any port on loopback, and to no other port beyond it; `Loopback` and `Off`
/// refuse the non-loopback address. The control connects without the sandbox.
///
/// Catches a `Remote` level that allows no outbound connect (the listed port refused), one
/// that allows every port beyond loopback, and one that drops the loopback rules.
#[test]
fn remote_connects_to_its_named_ports_beyond_loopback_and_to_loopback() {
    let scene = Scene::new("remote", "run");
    let named = accepting_listener_on("0.0.0.0:0");
    let other = accepting_listener_on("0.0.0.0:0");
    let host = own_address();
    let to_named = connect_to_host(&host, named);
    let to_other = connect_to_host(&host, other);
    let control_out = control("/usr/bin/perl", &to_named);
    assert!(
        control_out.status.success(),
        "control connect: {}",
        describe(&control_out)
    );
    let remote = Net::Remote(Ports::new(&[named]).expect("ports"));
    assert!(
        allowed(&scene, &remote, &to_named),
        "a named port connects beyond loopback"
    );
    assert!(
        refused(&scene, &remote, &to_other),
        "an unnamed port is refused beyond loopback"
    );
    assert!(
        allowed(&scene, &remote, &connect_to(other)),
        "Remote connects to any loopback port"
    );
    assert!(
        refused(&scene, &Net::Loopback, &to_named),
        "Loopback refuses a named port beyond loopback"
    );
    assert!(
        refused(&scene, &Net::Off, &to_named),
        "Off refuses beyond loopback"
    );
}

#[test]
fn a_unix_socket_in_a_read_denied_directory_is_refused_under_on() {
    let scene = Scene::new("unix", "run");
    let socket = scene.outside.join("sock");
    let _listener = UnixListener::bind(&socket).expect("bind");
    let connect = perl(UNIX_CONNECT, &[os(&socket)]);

    let free = control("/usr/bin/perl", &connect);
    assert!(
        free.status.success(),
        "control connect: {}",
        describe(&free)
    );
    // The same profile without the read deny connects: the refusal below is the deny's.
    let granted = scene.sandboxed(Net::On, "/usr/bin/perl", &connect);
    assert!(
        granted.status.success(),
        "Net::On without a deny: {}",
        describe(&granted)
    );

    let policy = scene.policy_denying(Net::On, &[&scene.outside]);
    let denied = scene.sandboxed_with(&policy, "/usr/bin/perl", &connect);
    assert!(!denied.status.success(), "connected under On");
}

#[test]
fn a_read_denied_path_inside_the_run_directory_is_not_writable() {
    let scene = Scene::new("rdwrite", "run");
    let secret = scene.run.join("secret");
    std::fs::create_dir_all(&secret).expect("secret");
    let target = secret.join("written");

    let granted = scene.sandboxed(Net::Off, "/usr/bin/touch", &touch(&target));
    assert!(
        granted.status.success(),
        "without the read deny the run directory is writable: {}",
        describe(&granted)
    );
    std::fs::remove_file(&target).expect("remove the granted file");

    let policy = scene.policy_denying(Net::Off, &[&secret]);
    let denied = scene.sandboxed_with(&policy, "/usr/bin/touch", &touch(&target));
    assert!(!denied.status.success(), "write into a read-denied path");
    assert!(!target.exists(), "{target:?} was created");
}

#[test]
fn launchctl_submit_is_refused_and_works_without_the_sandbox() {
    let scene = Scene::new("launchctl", "run");
    let label = format!("js.mac.pool.test.{}", std::process::id());
    let submit = args(&["submit", "-l", &label, "--", "/usr/bin/true"]);
    let remove = args(&["remove", &label]);

    let refused = scene.sandboxed(Net::Off, "/bin/launchctl", &submit);
    let leaked = control("/bin/launchctl", &args(&["list", &label]))
        .status
        .success();
    // Clean up before asserting, so a failure leaves no job behind.
    let _ = control("/bin/launchctl", &remove);

    let free = control("/bin/launchctl", &submit);
    let landed = control("/bin/launchctl", &args(&["list", &label]))
        .status
        .success();
    let _ = control("/bin/launchctl", &remove);

    assert!(free.status.success(), "control submit: {}", describe(&free));
    assert!(landed, "the control's job exists");
    assert!(
        !refused.status.success(),
        "sandboxed submit: {}",
        describe(&refused)
    );
    assert!(!leaked, "the sandboxed submit created no job");
}

/// The text of `profile` without its floor: the floor starts at `(deny mach-register)`.
fn without_floor(profile: &Profile) -> String {
    let text = profile.text();
    let cut = text.find("(deny mach-register)").expect("the floor");
    text[..cut].to_owned()
}

/// A floor rule beats an earlier grant: with a write grant over the protected store's
/// parent, the floor still refuses the write, while the same profile cut before the
/// floor lets the write land (the runner's user may write there). Without that second
/// half the test could not tell a floor that works from one that is not there.
#[test]
fn a_floor_deny_beats_an_earlier_grant() {
    let scene = Scene::new("floor", "run");
    let protected = Path::new("/private/var/protected");
    let target = protected
        .join("sfanalytics")
        .join(format!("js-mac-pool-{}", std::process::id()));
    let layout = SlotLayout {
        trees: None,
        run: AbsPath::new(protected).expect("run path"),
        uid_entry: Some(AbsPath::new(&scene.uid_entry).expect("entry path")),
        inputs: vec![],
        read_denies: vec![],
        mounts: vec![],
    };
    let caps = Caps {
        net: Net::Off,
        read_model: ReadModel::Open,
    };
    let profile =
        Profile::for_policy(&Policy::base(layout, caps).expect("policy")).expect("profile");
    let program = AbsPath::parse("/usr/bin/touch").expect("program");
    let run = |profile_text: &str, params: &Profile| {
        let mut argv = params.start_argv(&start_program(), &program, &touch(&target));
        let at = argv
            .iter()
            .position(|a| a == "--profile")
            .expect("--profile")
            + 1;
        argv[at] = OsString::from(profile_text);
        Command::new(&argv[0])
            .args(&argv[1..])
            .output()
            .expect("js-agent starts")
    };
    let with_floor = run(profile.text(), &profile);
    let created_with_floor = target.exists();
    let cut = without_floor(&profile);
    let os_only = run(&cut, &profile);
    let created_without_floor = target.exists();
    // Clean up before asserting, so a failure leaves nothing behind.
    let _ = std::fs::remove_file(&target);
    assert!(
        created_without_floor,
        "the grant alone writes there, so the floor is what refuses below: {}",
        describe(&os_only)
    );
    assert!(
        !with_floor.status.success(),
        "write refused: {}",
        describe(&with_floor)
    );
    assert!(
        !created_with_floor,
        "{target:?} was created under the floor"
    );
}

/// Checks in a name nobody else owns and prints the kernel return code: 0 when the name
/// is registered, otherwise the refusal.
const CHECK_IN_C: &str = r#"#include <mach/mach.h>
#include <servers/bootstrap.h>
#include <stdio.h>
int main(void) {
    mach_port_t port = MACH_PORT_NULL;
    kern_return_t kr = bootstrap_check_in(bootstrap_port, "com.komira.js-mac-pool.test", &port);
    printf("%d\n", (int)kr);
    return 0;
}
"#;

#[test]
fn registering_a_mach_service_is_refused() {
    let scene = Scene::new("mach", "run");
    let source = scene.run.join("checkin.c");
    let exe = scene.run.join("checkin");
    std::fs::write(&source, CHECK_IN_C).expect("source");
    let built = control("/usr/bin/clang", &[os(&source), "-o".into(), os(&exe)]);
    assert!(built.status.success(), "clang: {}", describe(&built));
    let exe_text = exe.to_str().expect("utf8");

    let free = control(exe_text, &[]);
    let refused = scene.sandboxed(Net::Off, exe_text, &[]);
    let free_code = String::from_utf8_lossy(&free.stdout).trim().to_owned();
    let refused_code = String::from_utf8_lossy(&refused.stdout).trim().to_owned();
    assert!(!free_code.is_empty(), "control ran: {}", describe(&free));
    assert!(
        !refused_code.is_empty(),
        "sandboxed ran: {}",
        describe(&refused)
    );
    assert_eq!(free_code, "0", "the control registers the name");
    assert_ne!(refused_code, "0", "the sandbox refuses the registration");
}

#[test]
fn mounting_is_refused() {
    let scene = Scene::new("mount", "run");
    let sudo = control("/usr/bin/sudo", &args(&["-n", "true"]));
    assert!(
        sudo.status.success(),
        "the runner has passwordless sudo: {}",
        describe(&sudo)
    );
    let mountpoint = scene.run.join("mountpoint");
    std::fs::create_dir_all(&mountpoint).expect("mountpoint");
    let mount = [
        OsString::from("-n"),
        OsString::from("/sbin/mount_devfs"),
        OsString::from("devfs"),
        os(&mountpoint),
    ];
    let unmount = [
        OsString::from("-n"),
        OsString::from("/sbin/umount"),
        os(&mountpoint),
    ];

    // Root starts js-agent, so the mount runs as root inside the profile: a setuid
    // `sudo` cannot be started under it, and the refusal must be the mount's.
    let profile = Profile::for_policy(&scene.policy(Net::Off)).expect("profile");
    let mut under_sudo = vec![OsString::from("-n")];
    under_sudo.extend(profile.start_argv(
        &start_program(),
        AbsPath::parse("/sbin/mount_devfs").expect("program"),
        &mount[2..],
    ));
    let refused = control("/usr/bin/sudo", &under_sudo);
    let mounted_sandboxed = control("/sbin/mount", &[]).stdout;
    let leaked =
        String::from_utf8_lossy(&mounted_sandboxed).contains(mountpoint.to_str().expect("utf8"));
    if leaked {
        let _ = control("/usr/bin/sudo", &unmount);
    }

    let free = control("/usr/bin/sudo", &mount);
    let mounted_free = control("/sbin/mount", &[]).stdout;
    let landed =
        String::from_utf8_lossy(&mounted_free).contains(mountpoint.to_str().expect("utf8"));
    let _ = control("/usr/bin/sudo", &unmount);

    assert!(free.status.success(), "control mount: {}", describe(&free));
    assert!(landed, "the control's mount is listed");
    assert!(
        !refused.status.success(),
        "sandboxed mount: {}",
        describe(&refused)
    );
    // The refusal is the sandbox's: `mount_devfs` ran and the kernel answered EPERM.
    let stderr = String::from_utf8_lossy(&refused.stderr).into_owned();
    assert!(
        stderr.contains("mount_devfs") && stderr.contains("Operation not permitted"),
        "the mount was refused by the sandbox: {stderr}"
    );
    assert!(!leaked, "the sandboxed mount is not listed");
}

/// Writing to `/dev/fd/1` opens the descriptor's file again, which needs the `/dev/fd`
/// grant; without it the redirect is refused. The control writes without the sandbox.
#[test]
fn the_dev_fd_grant_lets_a_shell_write_through_a_descriptor_path() {
    let scene = Scene::new("devfd", "run");
    let script = args(&["-c", "echo x > /dev/fd/1"]);
    let free = control("/bin/sh", &script);
    assert!(free.status.success(), "control: {}", describe(&free));
    assert_eq!(String::from_utf8_lossy(&free.stdout), "x\n");
    let granted = scene.sandboxed(Net::Off, "/bin/sh", &script);
    assert!(
        granted.status.success(),
        "sandboxed: {}",
        describe(&granted)
    );
    assert_eq!(String::from_utf8_lossy(&granted.stdout), "x\n");
}

/// `/dev/null` is written under the profile; `/dev/zero` is not, though the control
/// writes it. (`/dev/tty` is not granted either, `job_profile.rs` pins the text; a test
/// has no terminal, where the write fails without a profile too, so it is not run here.)
#[test]
fn a_run_writes_dev_null_and_not_dev_zero() {
    let scene = Scene::new("devices", "run");
    let script = args(&["-c", "echo x > /dev/null && echo x > /dev/zero"]);
    let free = control("/bin/sh", &script);
    assert!(free.status.success(), "control: {}", describe(&free));
    let refused = scene.sandboxed(Net::Off, "/bin/sh", &script);
    assert!(
        !refused.status.success(),
        "sandboxed: {}",
        describe(&refused)
    );
    let stderr = String::from_utf8_lossy(&refused.stderr).into_owned();
    assert!(
        stderr.contains("/dev/zero") && stderr.contains("Operation not permitted"),
        "the /dev/zero write was refused by the sandbox: {stderr}"
    );
}

/// `pidpath <pid>...`: for a child it starts, then each pid, prints `ok` or `errno N` of
/// `proc_pidpath`, one per line.
const PIDPATH_C: &str = r#"#include <errno.h>
#include <libproc.h>
#include <signal.h>
#include <stdio.h>
#include <stdlib.h>
#include <unistd.h>
static void ask(pid_t pid) {
    char path[PROC_PIDPATHINFO_MAXSIZE];
    if (proc_pidpath(pid, path, sizeof path) > 0) puts("ok"); else printf("errno %d\n", errno);
}
int main(int argc, char **argv) {
    pid_t child = fork();
    if (child == 0) { pause(); _exit(0); }
    ask(child);
    for (int i = 1; i < argc; i++) ask(atoi(argv[i]));
    kill(child, SIGKILL);
    return 0;
}
"#;

/// Under the profile a run reads the executable path of its own child and not of
/// another user's process (launchd, pid 1): `(deny process-info* (target others))`, with
/// its own processes allowed back. The control reads both. (The kernel's process list,
/// `sysctl` `KERN_PROC`, is not narrowed by it; the user boundary is what keeps others'
/// processes from a run.)
#[test]
fn a_run_reads_its_own_childs_path_and_not_another_users_process() {
    let scene = Scene::new("procs", "run");
    let source = scene.run.join("pidpath.c");
    let exe = scene.run.join("pidpath");
    std::fs::write(&source, PIDPATH_C).expect("source");
    let built = control("/usr/bin/clang", &[os(&source), "-o".into(), os(&exe)]);
    assert!(built.status.success(), "clang: {}", describe(&built));
    let exe = exe.to_str().expect("utf8");
    let free = control(exe, &args(&["1"]));
    assert_eq!(said(&free), "ok\nok", "control: {}", describe(&free));
    let held = scene.sandboxed(Net::Off, exe, &args(&["1"]));
    assert_eq!(said(&held), "ok\nerrno 1", "sandboxed: {}", describe(&held));
}

/// `posix <verb> <name>`: `make` creates a POSIX shared memory object and keeps it,
/// `open` opens one that exists, `unlink` removes one, and `sem` creates a named
/// semaphore and removes it again. Prints `ok`, or the `errno` of the call that failed.
const POSIX_C: &str = r#"#include <errno.h>
#include <fcntl.h>
#include <semaphore.h>
#include <stdio.h>
#include <string.h>
#include <sys/mman.h>
#include <unistd.h>
static int failed(void) { printf("errno %d\n", errno); return 1; }
int main(int argc, char **argv) {
    if (argc != 3) return 2;
    const char *verb = argv[1], *name = argv[2];
    if (strcmp(verb, "make") == 0) {
        int fd = shm_open(name, O_CREAT | O_EXCL | O_RDWR, 0600);
        if (fd < 0) return failed();
        close(fd);
    } else if (strcmp(verb, "open") == 0) {
        int fd = shm_open(name, O_RDWR, 0);
        if (fd < 0) return failed();
        close(fd);
    } else if (strcmp(verb, "unlink") == 0) {
        if (shm_unlink(name) != 0) return failed();
    } else if (strcmp(verb, "sem") == 0) {
        sem_t *sem = sem_open(name, O_CREAT | O_EXCL, 0600, 1);
        if (sem == SEM_FAILED) return failed();
        sem_close(sem);
        if (sem_unlink(name) != 0) return failed();
    } else {
        return 2;
    }
    puts("ok");
    return 0;
}
"#;

/// The POSIX probe, built without the sandbox in `scene`'s run directory.
fn posix_probe(scene: &Scene) -> String {
    let source = scene.run.join("posix.c");
    let exe = scene.run.join("posix");
    std::fs::write(&source, POSIX_C).expect("source");
    let built = control("/usr/bin/clang", &[os(&source), "-o".into(), os(&exe)]);
    assert!(built.status.success(), "clang: {}", describe(&built));
    exe.to_str().expect("utf8").to_owned()
}

/// What the probe printed.
fn said(out: &Output) -> String {
    String::from_utf8_lossy(&out.stdout).trim().to_owned()
}

/// Catches: a run that can create a POSIX shared memory object, which outlives it and
/// which the next run of the same reused uid could open by its name (no interface lists
/// the names, so the reset cannot remove it). The control creates one without the
/// sandbox, so the refusal is the profile's; a run can still open an object that exists
/// and create a named semaphore, which the profile leaves allowed.
#[test]
fn a_run_cannot_create_posix_shared_memory_and_can_open_it_and_make_a_semaphore() {
    let scene = Scene::new("posix", "run");
    let probe = posix_probe(&scene);
    let pid = std::process::id();
    let kept = format!("/jsmp-{pid}-kept");
    let refused = format!("/jsmp-{pid}-run");
    let semaphore = format!("/jsmp-{pid}-sem");

    let made = control(&probe, &args(&["make", kept.as_str()]));
    assert_eq!(said(&made), "ok", "control make: {}", describe(&made));
    let opened = scene.sandboxed(Net::Off, &probe, &args(&["open", kept.as_str()]));
    let unlinked = control(&probe, &args(&["unlink", kept.as_str()]));
    assert_eq!(said(&opened), "ok", "sandboxed open: {}", describe(&opened));
    assert_eq!(
        said(&unlinked),
        "ok",
        "control unlink: {}",
        describe(&unlinked)
    );

    let run = scene.sandboxed(Net::Off, &probe, &args(&["make", refused.as_str()]));
    if said(&run) == "ok" {
        // Made after all: unlinked before the assertion fails, so the Mac keeps nothing.
        control(&probe, &args(&["unlink", refused.as_str()]));
    }
    assert_eq!(said(&run), "errno 1", "sandboxed make: {}", describe(&run));
    let left = control(&probe, &args(&["open", refused.as_str()]));
    assert_eq!(
        said(&left),
        "errno 2",
        "nothing was left: {}",
        describe(&left)
    );

    let sem = scene.sandboxed(Net::Off, &probe, &args(&["sem", semaphore.as_str()]));
    assert_eq!(said(&sem), "ok", "sandboxed semaphore: {}", describe(&sem));
}
