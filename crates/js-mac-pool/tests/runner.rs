//! The generated profile under the real `sandbox-exec` on a macOS runner, as the runner's
//! own user. Each refusal has a control: the same step without the sandbox must succeed,
//! or the refusal proves nothing.

#![cfg(target_os = "macos")]

use std::ffi::OsString;
use std::net::TcpListener;
use std::os::unix::net::UnixListener;
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
        self.policy_denying(net, &[])
    }

    /// The policy of `net` that also denies reads of `denies`.
    fn policy_denying(&self, net: Net, denies: &[&Path]) -> Policy {
        let layout = SlotLayout {
            run: AbsPath::new(&self.run).expect("run path"),
            uid_entry: AbsPath::new(&self.uid_entry).expect("entry path"),
            inputs: vec![AbsPath::new(&self.input).expect("input path")],
            read_denies: denies
                .iter()
                .map(|path| AbsPath::new(path).expect("read-denied path"))
                .collect(),
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

/// Perl one-liners: the runner has `/usr/bin/perl`, and each prints nothing and reports
/// by exit status.
const LISTEN: &str = r#"use IO::Socket::INET; exit(IO::Socket::INET->new(Listen => 1, LocalAddr => "127.0.0.1", LocalPort => 0) ? 0 : 1)"#;
const RESOLVE: &str = r#"exit(gethostbyname("example.com") ? 0 : 1)"#;
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

fn resolves_under(scene: &Scene, net: Net) -> bool {
    scene
        .sandboxed(net, "/usr/bin/perl", &perl(RESOLVE, &[]))
        .status
        .success()
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
fn names_resolve_under_on_only() {
    let scene = Scene::new("dns", "run");
    let free = control("/usr/bin/perl", &perl(RESOLVE, &[]));
    assert!(
        free.status.success(),
        "control resolve: {}",
        describe(&free)
    );
    assert!(resolves_under(&scene, Net::On), "On resolves");
    assert!(!resolves_under(&scene, Net::Off), "Off refuses DNS");
    assert!(
        !resolves_under(&scene, Net::Loopback),
        "Loopback refuses DNS"
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
        run: AbsPath::new(protected).expect("run path"),
        uid_entry: AbsPath::new(&scene.uid_entry).expect("entry path"),
        inputs: vec![],
        read_denies: vec![],
    };
    let caps = Caps {
        net: Net::Off,
        read_model: ReadModel::Open,
    };
    let profile =
        Profile::for_policy(&Policy::base(layout, caps).expect("policy")).expect("profile");
    let program = AbsPath::parse("/usr/bin/touch").expect("program");
    let run = |profile_text: &str, params: &Profile| {
        let mut argv = params.argv(&program, &touch(&target));
        let at = argv.iter().position(|a| a == "-p").expect("-p") + 1;
        argv[at] = OsString::from(profile_text);
        Command::new(&argv[0])
            .args(&argv[1..])
            .output()
            .expect("sandbox-exec starts")
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

    // Root starts `sandbox-exec`, so the mount runs as root inside the profile: a setuid
    // `sudo` cannot be started under it, and the refusal must be the mount's.
    let profile = Profile::for_policy(&scene.policy(Net::Off)).expect("profile");
    let mut under_sudo = vec![OsString::from("-n")];
    under_sudo.extend(profile.argv(
        &AbsPath::parse("/sbin/mount_devfs").expect("program"),
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

#[test]
fn probe_per_process() {
    let scene = Scene::new("probe4", "run");
    let one = TcpListener::bind("127.0.0.1:0").expect("listener");
    let port = one.local_addr().expect("addr").port();
    std::thread::spawn(move || for _ in one.incoming() {});
    let script = r#"use IO::Socket::INET; my $ok = 0; my @e; for my $i (1..10) { my $s = IO::Socket::INET->new(PeerAddr => "127.0.0.1", PeerPort => $ARGV[0]); if ($s) { $ok++; push @e, "ok" } else { push @e, ($!+0) } } print STDERR join(",", @e); exit($ok == 10 ? 0 : 1)"#;
    let to = perl(script, &[OsString::from(port.to_string())]);
    let net = Net::Ports(Ports::new(&[port]).expect("ports"));
    let mut bad = Vec::new();
    for i in 0..600 {
        let out = scene.sandboxed(net.clone(), "/usr/bin/perl", &to);
        if !out.status.success() {
            bad.push(format!("#{i}: {}", describe(&out)));
        }
    }
    assert!(
        bad.len() > 1000,
        "PROBE4 {} of 600 processes had a failure: {:?}",
        bad.len(),
        bad
    );
}
