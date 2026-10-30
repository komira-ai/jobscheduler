//! The flags: what they may lower, what they refuse, and what reaches the hello.

use js_agent::Dialer;
use serde_json::{Value, json};

use crate::support::{
    CountingSleeper, Highest, Scratch, code, flags, linux_host, mac_host, run, run_scripted,
    welcome,
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
/// detected `os` and `arch`, the operator's capabilities and labels, the pool, and the
/// GPUs of the host.
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
        json!({ "os": "macos", "arch": "aarch64", "toolchain": "xcode16" })
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

/// Asserts the egress flags. The default is no network and is accepted with and without
/// `--egress none`; every flag of an allow-list names the mode it needs; a mode that is not
/// one of the two is refused; and an allow-list is refused at start, whatever else it says
/// (only the `crun` runtime enforces it and this build cannot select that runtime), as is
/// the Podman network flag beside it, so an operator who asks for a network never gets a
/// different one. It catches an allow-list that starts and runs with no network at all, and
/// one that is accepted together with `--container-network`.
#[test]
fn egress_flags_default_to_none_and_an_allow_list_is_refused_until_crun_is_selectable() {
    let host = linux_host();
    let hello = hello_of(&["--egress", "none"], &host);
    assert_eq!(hello["phase"], "serving");
    let said = refused(&["--egress", "open"], &host);
    assert!(
        said.contains("--egress: expected none or allow-list"),
        "{said}"
    );
    for (flags, name) in [
        (&["--egress-allow", "192.0.2.10:443"][..], "--egress-allow"),
        (&["--egress-dns", "192.0.2.53"][..], "--egress-dns"),
        (&["--egress-pool", "198.18.0.0/16"][..], "--egress-pool"),
        (&["--egress-nft", "/bin/sh"][..], "--egress-nft"),
    ] {
        let said = refused(flags, &host);
        assert!(
            said.contains(&format!("{name}: needs --egress allow-list")),
            "{said}"
        );
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
        let said = refused(&allow, &host);
        assert!(
            said.contains("--egress allow-list: only the crun runtime enforces it"),
            "{said}"
        );
        let mut both = allow.to_vec();
        both.extend(["--container-network", "jsnet"]);
        let said = refused(&both, &host);
        assert!(said.contains("there is one network mode"), "{said}");
        let said = refused(&["--egress", "allow-list"], &host);
        assert!(said.contains("needs at least one --egress-allow"), "{said}");
    }
}
