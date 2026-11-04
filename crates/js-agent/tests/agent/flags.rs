//! The flags: what they may lower, what they refuse, and what reaches the hello.

use js_agent::Dialer;
use serde_json::{Value, json};

use crate::support::{
    CountingSleeper, Highest, Lowest, Ran, Scratch, Script, ScriptConnector, code, flags,
    linux_host, mac_host, run, run_scripted, welcome,
};

/// Runs the program to its first wait with a welcome and returns the hello it sent.
fn hello_of(extra: &[&str], host: &crate::support::Host) -> Value {
    let (ran, sent, _) = run_scripted(extra, host, vec![welcome(1)], 1);
    assert_eq!(ran.status(), code(0), "{}", ran.err);
    sent.borrow()[0].1.clone()
}

/// Runs the program with flags it must refuse before anything is sent; returns stderr.
fn refused(extra: &[&str], host: &crate::support::Host) -> String {
    let (ran, sent, waits) = run_scripted(extra, host, vec![welcome(1)], 1);
    assert_eq!(ran.status(), code(2), "{}", ran.err);
    assert!(sent.borrow().is_empty(), "something was sent: {:?}", sent);
    assert!(waits.is_empty());
    ran.err
}

/// Asserts that the hello offers what was detected when no flag lowers it, with the
/// detected `os` and `arch` (the architecture in the one spelling: a host whose platform
/// says `aarch64`, as Rust names it, reports `arm64`), the operator's capabilities and
/// labels, the pool, and the GPUs of the host. Catches an agent that sends `aarch64`.
#[test]
fn the_hello_offers_what_was_detected() {
    let hello = hello_of(
        &[
            "--host-id",
            "build-1",
            "--label",
            "rack=r1",
            "--capability",
            "toolchain=xcode16",
        ],
        &mac_host(),
    );
    assert_eq!(hello["host_id"], "build-1");
    assert_eq!(hello["pool"], "macs");
    assert_eq!(hello["phase"], "serving");
    assert_eq!(hello["protocol"], json!({ "min": 1, "max": 1 }));
    assert_eq!(hello["shape"]["cpu_millis"], 10_000);
    assert_eq!(hello["shape"]["memory_bytes"], 17_179_869_184u64);
    assert_eq!(hello["shape"]["gpus"], 1);
    assert_eq!(hello["shape"]["gpu_class"], "apple-m");
    assert_eq!(
        hello["capabilities"],
        json!({ "os": "macos", "arch": "arm64", "toolchain": "xcode16" })
    );
    assert_eq!(hello["labels"], json!({ "rack": "r1" }));
    assert_eq!(hello["gpu_devices"], json!([]));
}

/// Asserts that every flag lowers what was detected and none raises it: lowered values
/// reach the hello (and the GPU devices listed shrink to the count), a flag equal to the
/// detection is accepted, and a flag above it ends the program with status 2, naming the
/// flag, before anything is sent. It catches an agent that lets an operator advertise
/// more than the machine has.
#[test]
fn detected_shape_capped_by_flags_never_raised() {
    let hello = hello_of(
        &[
            "--cpu-millis",
            "2000",
            "--memory-bytes",
            "1073741824",
            "--gpus",
            "1",
        ],
        &linux_host(),
    );
    assert_eq!(hello["shape"]["cpu_millis"], 2000);
    assert_eq!(hello["shape"]["memory_bytes"], 1_073_741_824u64);
    assert_eq!(hello["shape"]["gpus"], 1);
    assert_eq!(hello["gpu_devices"].as_array().unwrap().len(), 1);
    assert_eq!(hello["gpu_devices"][0]["index"], 0);

    let equal = hello_of(&["--cpu-millis", "8000", "--gpus", "2"], &linux_host());
    assert_eq!(equal["shape"]["cpu_millis"], 8000);
    assert_eq!(equal["shape"]["gpus"], 2);

    let none = hello_of(&["--gpus", "0"], &mac_host());
    assert_eq!(none["shape"]["gpus"], 0);
    assert!(none["shape"].get("gpu_class").is_none());

    let host = linux_host();
    let said = refused(&["--cpu-millis", "8001"], &host);
    assert!(
        said.contains("--cpu-millis: 8001 is above the 8000 detected"),
        "{said}"
    );
    let said = refused(&["--memory-bytes", "34359738369"], &host);
    assert!(said.contains("--memory-bytes: "), "{said}");
    let said = refused(&["--gpus", "3"], &host);
    assert!(said.contains("--gpus: 3 is above the 2 detected"), "{said}");
    assert!(
        said.contains("a flag only lowers what the host has"),
        "{said}"
    );
}

/// Asserts that `--gpu-class` renames the offered GPUs' class, and is refused when no GPU
/// is offered (none detected, or `--gpus 0`).
#[test]
fn gpu_class_names_offered_gpus_only() {
    let hello = hello_of(&["--gpu-class", "nvidia-a100"], &linux_host());
    assert_eq!(hello["shape"]["gpu_class"], "nvidia-a100");
    let said = refused(
        &["--gpus", "0", "--gpu-class", "nvidia-a100"],
        &linux_host(),
    );
    assert!(
        said.contains("--gpu-class: the host offers no GPU to name"),
        "{said}"
    );
}

/// Asserts that `--instance-file` puts the instance the file names into the hello (a line
/// `PROVIDER:INSTANCE_ID`, surrounding white space dropped), that a hello without the flag
/// names none, and that a file that is missing or is not that form, or names what the
/// wire refuses, ends the program before it says hello and names the flag. It catches an
/// agent that cannot present the instance a one-time token is bound to, and one that
/// sends a made-up instance.
#[test]
fn the_instance_file_names_the_instance_in_the_hello() {
    let path = "/etc/js-agent/instance";
    let host = |text: &str| {
        let mut host = linux_host();
        host.files = host.files.with(path, text);
        host
    };
    let plain = hello_of(&["--host-id", "gpu-1"], &linux_host());
    assert!(plain.get("instance").is_none(), "{plain}");
    let flags = ["--host-id", "gpu-1", "--instance-file", path];
    let hello = hello_of(&flags, &host("lambdalabs:0920582c\n"));
    assert_eq!(
        hello["instance"],
        json!({ "provider": "lambdalabs", "instance_id": "0920582c" })
    );
    let said = refused(&flags, &linux_host());
    assert!(said.contains("--instance-file: "), "{said}");
    assert!(said.contains("No such file"), "{said}");
    let said = refused(&flags, &host("no-colon-here\n"));
    assert!(said.contains("must hold PROVIDER:INSTANCE_ID"), "{said}");
    let said = refused(&flags, &host("Not A Provider:i-1\n"));
    assert!(said.contains("instance.provider"), "{said}");
}

/// Asserts that `os` and `arch` cannot be set as capabilities, that an operator
/// capability of another name is passed on, and that a label the wire refuses (the
/// reserved prefix) ends the program before it says hello.
#[test]
fn detected_capabilities_and_bad_labels_are_refused() {
    let said = refused(&["--capability", "os=linux"], &mac_host());
    assert!(
        said.contains("--capability: os is detected and cannot be set"),
        "{said}"
    );
    let said = refused(&["--capability", "arch=arm"], &mac_host());
    assert!(said.contains("--capability: arch is detected"), "{said}");
    let said = refused(&["--label", "jobscheduler.dev/pool=other"], &mac_host());
    assert!(said.contains("the hello is not acceptable"), "{said}");
}

/// Asserts that an invalid host id is refused before anything is sent and that a machine
/// id that cannot be read points at `--host-id`.
#[test]
fn host_id_problems_are_refused_before_the_hello() {
    let said = refused(&["--host-id", "Not Valid"], &linux_host());
    assert!(
        said.contains("the hello is not acceptable: host_id"),
        "{said}"
    );
    let mut host = linux_host();
    host.files =
        crate::support::FakeFiles::default().with("/proc/meminfo", crate::support::MEMINFO);
    let said = refused(&[], &host);
    assert!(said.contains("cannot read the machine id"), "{said}");
    assert!(said.contains("pass --host-id"), "{said}");
}

/// Asserts that a probe whose figure the agent cannot guess ends the start with status 2:
/// a `getconf` that prints no number (an absent one is counted by the standard library).
#[test]
fn undetectable_cpu_is_refused() {
    let mut host = linux_host();
    host.commands = host.commands.with(crate::support::CPU_CMD, Ok("many\n"));
    let said = refused(&["--host-id", "h"], &host);
    assert!(said.contains("cannot count the CPUs"), "{said}");
}

/// Asserts how clap's own outcomes are shown: `--help` and `--version` on stdout with
/// status 0, a malformed `key=value`, an unknown flag and a missing required flag on
/// stderr with status 2. No transport is made for any of them.
#[test]
fn usage_errors_and_help() {
    let host = linux_host();
    let go = |args: &[&str]| {
        let mut sleeper = CountingSleeper::stopping_at(1);
        run(args, &host, &Dialer, &mut sleeper, &mut Highest)
    };
    let help = go(&["--help"]);
    assert_eq!(help.status(), code(0));
    assert!(help.out.contains("--token-file"), "{}", help.out);
    assert!(help.out.contains("--gpu-class"), "{}", help.out);
    assert!(help.err.is_empty());
    let version = go(&["--version"]);
    assert_eq!(version.status(), code(0));
    assert!(version.out.starts_with("js-agent "), "{}", version.out);
    // The line the node-setup script reads to know `--runtime crun` is wired.
    let runtimes = match cfg!(target_os = "linux") {
        true => "runtimes: docker crun",
        false => "runtimes: docker native",
    };
    let lines: Vec<&str> = version.out.lines().collect();
    assert_eq!(lines.len(), 2, "{}", version.out);
    assert_eq!(lines[1], runtimes);
    let bad = go(&flags(&["--label", "no-equals-sign"]));
    assert_eq!(bad.status(), code(2));
    assert!(bad.err.contains("is not key=value"), "{}", bad.err);
    assert!(bad.out.is_empty());
    let unknown = go(&flags(&["--join-token", "x"]));
    assert_eq!(unknown.status(), code(2));
    let missing = go(&["--server", "http://127.0.0.1:1"]);
    assert_eq!(missing.status(), code(2));
    assert!(missing.err.contains("--pool"), "{}", missing.err);
}

/// Asserts that plain http to a host off the loopback is refused naming `--server`, and
/// an https `--ca-cert` file that is missing naming `--ca-cert`, before any fact is probed
/// or anything is sent. (https without `--ca-cert` is no longer refused: it trusts the
/// bundled public roots.) It catches a token that would cross a network in clear.
#[test]
fn plain_http_off_loopback_refused() {
    let dir = Scratch::new("flags_http");
    let token = dir.write("token", "join-token-0123456789-abcdefghijklmnop\n", 0o600);
    let host = linux_host();
    let go = |server: &str| {
        let args = [
            "--server",
            server,
            "--token-file",
            token.to_str().unwrap(),
            "--pool",
            "macs",
        ];
        let mut sleeper = CountingSleeper::stopping_at(1);
        run(&args, &host, &Dialer, &mut sleeper, &mut Highest)
    };
    let plain = go("http://leases.example.com:7300");
    assert_eq!(plain.status(), code(2));
    assert!(
        plain.err.starts_with("js-agent: --server: "),
        "{}",
        plain.err
    );
    assert!(plain.err.contains("plain http"), "{}", plain.err);
    let missing = dir.path("absent.pem");
    let no_file = run(
        &[
            "--server",
            "https://leases.example.com:7300",
            "--token-file",
            token.to_str().unwrap(),
            "--pool",
            "macs",
            "--ca-cert",
            missing.to_str().unwrap(),
        ],
        &host,
        &Dialer,
        &mut CountingSleeper::stopping_at(1),
        &mut Highest,
    );
    assert_eq!(no_file.status(), code(2));
    assert!(
        no_file.err.starts_with("js-agent: --ca-cert: "),
        "{}",
        no_file.err
    );
    assert!(
        host.commands.ran.borrow().is_empty(),
        "a probe ran before the flags were checked"
    );
}

/// Asserts that a join token file other users can read (mode 0644) is refused with status
/// 2 and an error that names `--token-file` and the file, never the token, and that a
/// missing file is refused the same way. It catches an agent that sends a token it could
/// not keep private.
#[test]
fn token_file_0644_refused_naming_the_flag() {
    let dir = Scratch::new("flags_token_mode");
    let secret = "join-token-0123456789-abcdefghijklmnop";
    let open = dir.write("token", secret, 0o644);
    let go = |path: &std::path::Path| {
        let args = [
            "--server",
            "http://127.0.0.1:1",
            "--token-file",
            path.to_str().unwrap(),
            "--pool",
            "macs",
        ];
        let mut sleeper = CountingSleeper::stopping_at(1);
        run(&args, &linux_host(), &Dialer, &mut sleeper, &mut Highest)
    };
    let ran = go(&open);
    assert_eq!(ran.status(), code(2));
    assert!(
        ran.err.starts_with("js-agent: --token-file: "),
        "{}",
        ran.err
    );
    assert!(ran.err.contains("mode 0644"), "{}", ran.err);
    assert!(ran.err.contains(open.to_str().unwrap()), "{}", ran.err);
    assert!(!ran.err.contains(secret) && !ran.out.contains(secret));
    let missing = go(&dir.path("absent"));
    assert_eq!(missing.status(), code(2));
    assert!(missing.err.contains("--token-file: "), "{}", missing.err);
}

/// Asserts that a server that cannot be talked to is not a refusal: the real transport reports it
/// as a failed hello naming the address, the agent waits and tries again, and a stop
/// request ends the program with status 0. The token is not in the message.
#[test]
fn an_unreachable_server_is_retried_not_fatal() {
    let dir = Scratch::new("flags_unreachable");
    let secret = "join-token-0123456789-abcdefghijklmnop";
    let token = dir.write("token", secret, 0o600);
    // The listener stays bound and hangs up on every connection: a port that was bound
    // and then freed can be taken by another process before the agent dials it.
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            drop(stream);
        }
    });
    let args = [
        "--server",
        url.as_str(),
        "--token-file",
        token.to_str().unwrap(),
        "--pool",
        "macs",
        "--host-id",
        "mac-1",
    ];
    let mut sleeper = CountingSleeper::stopping_at(2);
    let waits = sleeper.waits.clone();
    let ran = run(&args, &linux_host(), &Dialer, &mut sleeper, &mut Highest);
    assert_eq!(ran.status(), code(0), "{}", ran.err);
    assert_eq!(waits.borrow().len(), 2);
    let said = format!("hello failed: cannot talk to {url}: ");
    assert_eq!(ran.err.matches(&said).count(), 2, "{}", ran.err);
    assert!(!ran.err.contains(secret));
}

/// Asserts the readiness-hook flags: with none the agent starts as before; a hook that is
/// an absolute path to an executable file (with arguments, hyphens included) is accepted;
/// and an empty program, a relative one, a file that is not executable, one that is not
/// there, and an argument without a hook each end the program with status 2 naming the
/// flag, before anything is sent. It catches a hook that is looked up on the `PATH`, one
/// that is checked only when a lease asks for it, and arguments that are silently dropped.
#[test]
fn the_readiness_hook_flags_are_checked_when_the_agent_starts() {
    let host = linux_host();
    let (ran, _, _) = run_scripted(&[], &host, vec![welcome(1)], 1);
    assert_eq!(ran.status(), code(0), "{}", ran.err);
    let ok = ["--readiness-hook", "/bin/sh", "--readiness-hook-arg", "-c"];
    let (ran, _, _) = run_scripted(&ok, &host, vec![welcome(1)], 1);
    assert_eq!(ran.status(), code(0), "{}", ran.err);

    let dir = Scratch::new("readiness-flags");
    let plain = dir.write("plain", "x", 0o600);
    let plain = plain.to_str().unwrap();
    let cases: [(&[&str], &str); 6] = [
        (
            &["--readiness-hook", ""],
            "--readiness-hook: not an absolute path",
        ),
        (
            &["--readiness-hook", "hook.sh"],
            "--readiness-hook: not an absolute path",
        ),
        (
            &["--readiness-hook", plain],
            "--readiness-hook: not an executable file",
        ),
        (
            &["--readiness-hook", "/"],
            "--readiness-hook: not an executable file",
        ),
        (
            &["--readiness-hook", "/nonexistent/hook"],
            "--readiness-hook: not an executable file",
        ),
        (
            &["--readiness-hook-arg", "x"],
            "--readiness-hook-arg: needs --readiness-hook",
        ),
    ];
    for (extra, said) in cases {
        let err = refused(extra, &host);
        assert!(err.contains(said), "{extra:?}: {err}");
    }
}

/// Asserts that `--scratch-quota` without `--scratch-root` is refused before anything is
/// sent and names the flag, since there is no scratch device to compare. It catches a
/// quota flag that silently does nothing.
#[test]
fn a_scratch_quota_without_a_scratch_root_is_refused() {
    let said = refused(&["--scratch-quota"], &linux_host());
    assert!(
        said.contains("--scratch-quota: needs --scratch-root"),
        "{said}"
    );
}

/// Asserts that `--scratch-low-water-gib` without `--scratch-root` is refused before
/// anything is sent and names the flag, since there is no volume to take the mark from.
/// It catches a mark that is silently ignored.
#[test]
fn a_low_water_mark_without_a_scratch_root_is_refused() {
    let said = refused(&["--scratch-low-water-gib", "1"], &linux_host());
    assert!(
        said.contains("--scratch-low-water-gib: needs --scratch-root"),
        "{said}"
    );
}

/// Asserts the egress flags of an agent whose runtime is not `crun`. The default is no
/// network and is accepted with and without `--egress none`; every flag of a job network
/// names the modes it needs (`--egress-forward-port` among them); a mode that is not one of
/// the three is refused; `allow-list` and `open` are refused at start, naming `--runtime
/// crun` (the only runtime that enforces them) and saying that a container-engine job has
/// network none, with no runtime given and with `--runtime docker`, so an operator who asks
/// for a network never gets a different one; and an operator cannot claim a ceiling with
/// `--capability`. It catches a ceiling accepted and silently ignored by a runtime that gives
/// jobs no such network, and a hello that says `egress=open` on an operator's word.
#[test]
fn egress_allow_list_is_refused_without_the_crun_runtime() {
    let host = linux_host();
    let hello = hello_of(&["--egress", "none"], &host);
    assert_eq!(hello["phase"], "serving");
    let said = refused(&["--egress", "wide"], &host);
    assert!(
        said.contains("--egress: expected none, allow-list or open"),
        "{said}"
    );
    for (flags, name) in [
        (&["--egress-allow", "192.0.2.10:443"][..], "--egress-allow"),
        (&["--egress-dns", "192.0.2.53"][..], "--egress-dns"),
        (
            &["--egress-forward-port", "80"][..],
            "--egress-forward-port",
        ),
        (&["--egress-pool", "198.18.0.0/16"][..], "--egress-pool"),
        (&["--egress-nft", "/bin/sh"][..], "--egress-nft"),
        (&["--egress-nsenter", "/bin/sh"][..], "--egress-nsenter"),
    ] {
        let said = refused(flags, &host);
        assert!(
            said.contains(&format!("{name}: needs --egress allow-list or open")),
            "{said}"
        );
    }
    for key in ["egress", "egress-ports"] {
        let claim = format!("{key}=open");
        let said = refused(&["--capability", &claim], &host);
        let want = format!("--capability: {key} is detected and cannot be set");
        assert!(said.contains(&want), "{said}");
    }
    #[cfg(target_os = "linux")]
    {
        let allow = [
            "--egress",
            "allow-list",
            "--egress-allow",
            "192.0.2.10:443",
            "--egress-ip",
            "/bin/sh",
            "--egress-nft",
            "/bin/sh",
        ];
        let mut docker = allow.to_vec();
        docker.extend(["--runtime", "docker", "--allow-container-engine"]);
        docker.extend(["--container-bin", "/bin/sh"]);
        for given in [&allow[..], &docker[..]] {
            let said = refused(given, &host);
            assert!(
                said.contains(
                    "--egress allow-list: needs --runtime crun, the only runtime that enforces \
                     it; a job run through --container-bin has network none"
                ),
                "{given:?}: {said}"
            );
        }
        let said = refused(&["--egress", "allow-list"], &host);
        assert!(said.contains("needs at least one --egress-allow"), "{said}");
        let programs = ["--egress-ip", "/bin/sh", "--egress-nft", "/bin/sh"];
        let mut open = vec!["--egress", "open", "--egress-nsenter", "/bin/sh"];
        open.extend(programs);
        let said = refused(&open, &host);
        assert!(
            said.contains("--egress open: needs --runtime crun"),
            "{said}"
        );
        open.extend(["--egress-forward-port", "70000"]);
        let said = refused(&open, &host);
        assert!(said.contains("--egress-forward-port: not <PORT>"), "{said}");
    }
}

/// What the agent prints for `--container-network`, which was removed.
const NO_NETWORK: &str = "--container-network: removed. A job run through the container \
    engine now always has network none. For networking give --runtime crun with --egress, and \
    ask for a network per job in the job's network field";

/// What the agent prints for `--container-egress-allow`, which was removed.
const NO_EGRESS_ALLOW: &str = "--container-egress-allow: removed. Use --egress allow-list with \
    --egress-allow on the crun runtime, or the job's network field";

/// What the agent prints for `--container-nft`, which was removed.
const NO_NFT: &str = "--container-nft: removed together with --container-network. The crun \
    runtime's nft program is --egress-nft";

/// Runs the program on Linux with `extra` beside the flags every run needs, until its first
/// wait; returns how it ended, whether it connected to the server (the scripted connector
/// hands its one transport out at the first connect) and how many messages it sent.
fn run_counting(extra: &[&str]) -> (Ran, bool, usize) {
    let (script, sent) = Script::new(vec![welcome(1)]);
    let connector = ScriptConnector::of(script);
    let mut sleeper = CountingSleeper::stopping_at(1);
    let ran = run(
        &flags(extra),
        &linux_host(),
        &connector,
        &mut sleeper,
        &mut Lowest,
    );
    let connected = connector.0.borrow().is_none();
    let sent = sent.borrow().len();
    (ran, connected, sent)
}

/// Asserts that each removed flag, in both spellings and beside an otherwise valid command
/// line, ends the program with status 2 and exactly one line that names the flag and what
/// replaces it, before the agent connects or sends anything; that the refusal comes before
/// clap's own (an unknown flag ahead of it does not turn it into clap's usage error); that a
/// removed name after a flag whose value cannot start with `-` is read as the flag, as clap
/// reads it; that a removed flag with no value, or an empty attached one, is refused the
/// same way; that the scan goes on after a value it skipped, a `--` taken as the value of
/// `--readiness-hook-arg` included; and that of two removed flags only the first given is
/// reported. Catches a removed flag silently accepted, clap's generic error printed in place
/// of the replacement, and a refusal that comes after the agent registered.
#[test]
fn the_removed_flags_are_refused_with_a_message() {
    let cases: [(&[&str], &str); 14] = [
        (&["--container-network", "jobs"], NO_NETWORK),
        (&["--container-network=jobs"], NO_NETWORK),
        (
            &["--container-egress-allow", "192.0.2.1:443"],
            NO_EGRESS_ALLOW,
        ),
        (&["--container-egress-allow=192.0.2.1:443"], NO_EGRESS_ALLOW),
        (&["--container-nft", "/usr/sbin/nft"], NO_NFT),
        (&["--container-nft=/usr/sbin/nft"], NO_NFT),
        (&["--container-network"], NO_NETWORK),
        (&["--container-egress-allow="], NO_EGRESS_ALLOW),
        (&["--container-nft="], NO_NFT),
        (
            &[
                "--readiness-hook",
                "/bin/sh",
                "--readiness-hook-arg",
                "--container-network",
                "--container-nft=/usr/sbin/nft",
            ],
            NO_NFT,
        ),
        (
            &[
                "--readiness-hook",
                "/bin/sh",
                "--readiness-hook-arg",
                "--",
                "--container-network",
                "jobs",
            ],
            NO_NETWORK,
        ),
        (
            &["--no-such-flag", "--container-network", "jobs"],
            NO_NETWORK,
        ),
        (&["--label", "--container-network", "jobs"], NO_NETWORK),
        (
            &[
                "--container-nft",
                "/usr/sbin/nft",
                "--container-network",
                "jobs",
            ],
            NO_NFT,
        ),
    ];
    for (given, want) in cases {
        let (ran, connected, sent) = run_counting(given);
        assert_eq!(ran.status(), code(2), "{given:?}: {}", ran.err);
        assert_eq!(ran.err, format!("js-agent: {want}\n"), "{given:?}");
        assert!(!connected, "{given:?}: the agent connected");
        assert_eq!(sent, 0, "{given:?}");
    }
}

/// Asserts that a removed flag's name given as the value of another flag is that flag's
/// value and not a refusal: as the value of `--readiness-hook-arg` (which takes values that
/// start with `-`) the agent starts and serves with a hook, and without the hook it goes on
/// to the next check, after it connected, which names `--readiness-hook`; attached with `=`
/// it is a value of `--label`, which reaches the hello. And a removed name after a bare `--`
/// is not read as a flag (clap refuses the stray word). Catches a scan that is a substring
/// or `contains` over the arguments.
#[test]
fn a_removed_flag_name_as_a_value_is_not_a_refusal() {
    let host = linux_host();
    let hook = [
        "--readiness-hook",
        "/bin/sh",
        "--readiness-hook-arg",
        "--container-network",
    ];
    assert_eq!(hello_of(&hook, &host)["phase"], "serving");
    let (ran, connected, sent) = run_counting(&["--readiness-hook-arg", "--container-network"]);
    assert_eq!(ran.status(), code(2), "{}", ran.err);
    let want = "js-agent: --readiness-hook-arg: needs --readiness-hook\n";
    assert!(ran.err.ends_with(want), "{}", ran.err);
    assert!(!ran.err.contains("removed"), "{}", ran.err);
    assert!(connected, "the run stopped before the hook check");
    assert_eq!(sent, 0);
    let hello = hello_of(&["--label=k=--container-nft"], &host);
    assert_eq!(hello["labels"], json!({ "k": "--container-nft" }));
    let (ran, connected, _) = run_counting(&["--", "--container-egress-allow", "192.0.2.1:443"]);
    assert_eq!(ran.status(), code(2), "{}", ran.err);
    assert!(!ran.err.contains("removed"), "{}", ran.err);
    assert!(!connected);
}

/// Asserts that a flag whose name only starts like a removed one is an ordinary unknown
/// argument: clap's own usage error, status 2, no removal message, and no connect. Catches a
/// scan that matches a prefix and tells an operator that a flag they never gave was removed.
#[test]
fn a_name_like_a_removed_flag_is_an_unknown_argument() {
    for given in ["--container-network-extra", "--container-nftx=/x"] {
        let (ran, connected, sent) = run_counting(&[given]);
        assert_eq!(ran.status(), code(2), "{given}: {}", ran.err);
        let name = given.split('=').next().unwrap_or(given);
        assert!(
            ran.err.contains("unexpected argument"),
            "{given}: {}",
            ran.err
        );
        assert!(ran.err.contains(name), "{given}: {}", ran.err);
        assert!(!ran.err.contains("removed"), "{given}: {}", ran.err);
        assert!(!connected, "{given}");
        assert_eq!(sent, 0, "{given}");
    }
}
