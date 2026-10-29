//! The container flags: `--container-bin`, `--container-network`, `--container-user`,
//! `--container-host`, `--container-pull`, `--container-tmp-size` and
//! `--container-subuid`.

use std::ffi::OsStr;
use std::os::unix::ffi::OsStrExt;
use std::path::Path;

use js_agent::cli_runtime::{
    DEFAULT_NETWORK, DEFAULT_TMP_SIZE, DEFAULT_USER, Flags, JobUsers, Pull, Settings, SettingsError,
};
use js_agent::lifecycle::SLOTS;

use super::BIN;
use crate::support::{Scratch, code, linux_host, run_scripted, welcome};

fn bin() -> &'static Path {
    Path::new(BIN)
}

/// The ids of the agent a test pretends to be, not root.
const AGENT: (u32, u32) = (1000, 1000);

fn own_ids() -> (u32, u32) {
    (
        rustix::process::geteuid().as_raw(),
        rustix::process::getegid().as_raw(),
    )
}

/// Asserts that the runtime is a path and only a path: relative and non-UTF-8 paths are
/// refused (nothing is looked up on the `PATH`), and so are a file that is absent, a
/// directory and a file that cannot be executed. It catches a bare `docker` that would
/// be found on whatever `PATH` the agent inherited.
#[test]
fn the_runtime_is_an_absolute_path_to_an_executable_file() {
    let dir = Scratch::new("cli-settings-bin");
    let plain = dir.write("plain", "#!/bin/sh\n", 0o644);
    let exec = dir.write("exec", "#!/bin/sh\n", 0o755);
    let non_utf8 = Path::new(OsStr::from_bytes(b"/bin/\xff"));
    let folder = dir.path("");
    let refused = [
        (Path::new("docker"), SettingsError::BinPath),
        (Path::new("bin/docker"), SettingsError::BinPath),
        (non_utf8, SettingsError::BinPath),
        (Path::new("/nonexistent/docker"), SettingsError::BinMissing),
        (folder.as_path(), SettingsError::BinMissing),
        (plain.as_path(), SettingsError::BinMissing),
    ];
    for (path, why) in refused {
        let said = Settings::new(path, None, None);
        assert_eq!(said, Err(why), "{path:?}");
    }
    let ok = Settings::new(&exec, None, None).unwrap();
    assert_eq!(ok.bin(), exec.to_str().unwrap());
    assert_eq!(ok.network(), DEFAULT_NETWORK);
    assert_eq!(ok.network(), "none");
    assert_eq!(ok.user(), DEFAULT_USER);
    assert_eq!(ok.host(), None);
    assert_eq!(ok.pull(), Pull::Never);
    assert_eq!(ok.tmp_size(), DEFAULT_TMP_SIZE);
    assert_eq!(ok.tmp_size(), "64m");
    assert_eq!(ok.chown_scratch(), None);
}

/// Asserts that `host` is refused as a network in every spelling, that text which is
/// not a name is refused (`container:<id>` would join another container's network, a
/// leading `-` would read as an option), and that ordinary user-defined network names
/// are taken. It catches a job placed on the machine's own network.
#[test]
fn the_host_network_and_non_names_are_refused() {
    let long = "n".repeat(65);
    for name in [
        "host",
        "Host",
        "HOST",
        "",
        "container:abc",
        "ns:/proc/1/ns/net",
        "-x",
        "a b",
        "a,b",
        "a=b",
        long.as_str(),
    ] {
        let said = Settings::new(bin(), Some(name), None);
        assert_eq!(said, Err(SettingsError::Network), "{name:?}");
    }
    for name in ["jobs", "job-net_0.1", "none", "0net", &long[1..]] {
        let ok = Settings::new(bin(), Some(name), None).unwrap();
        assert_eq!(ok.network(), name);
    }
}

/// Asserts that the user is `uid:gid` of two numbers above zero, written back
/// canonically, and that root, a lone number, a name and text around the numbers are
/// refused. It catches a container that runs as root.
#[test]
fn the_container_user_is_not_root() {
    for user in [
        "0:0",
        "0:1",
        "1:0",
        "1000",
        "1000:",
        ":1000",
        "root:root",
        "a:1",
        "1:b",
        "-1:1",
        "1:2:3",
        "",
        "1000:1000 ",
    ] {
        let said = Settings::new(bin(), None, Some(user));
        assert_eq!(said, Err(SettingsError::User), "{user:?}");
    }
    let ok = Settings::new(bin(), None, Some("1000:2000")).unwrap();
    assert_eq!(ok.user(), "1000:2000");
    let plus = Settings::new(bin(), None, Some("+7:8")).unwrap();
    assert_eq!(plus.user(), "7:8", "written canonically");
}

/// Asserts the rule that makes the scratch directory usable: it is `0700` and the
/// agent's, so an agent that runs as root hands it to the container's user (whatever
/// that is), and any other agent can start only when the container's user is its own
/// `uid:gid`, because it cannot give a file away. The default user is not the agent's
/// unless the agent is that user. It catches a default configuration that mounts
/// `/scratch` read-write for a process that cannot write it.
#[test]
fn the_scratch_must_be_writable_by_the_container_user() {
    let user = |text: Option<&str>| Settings::new(bin(), None, text).unwrap();

    let root = user(Some("2000:3000")).with_agent(0, 0).unwrap();
    assert_eq!(root.chown_scratch(), Some((2000, 3000)));
    let root_default = user(None).with_agent(0, 0).unwrap();
    assert_eq!(root_default.chown_scratch(), Some((1000, 1000)));

    let own = user(Some("1000:1000")).with_agent(1000, 1000).unwrap();
    assert_eq!(own.chown_scratch(), None, "nothing to give away");
    let default_own = user(None).with_agent(1000, 1000).unwrap();
    assert_eq!(default_own.chown_scratch(), None);

    for (flag, uid, gid) in [
        (Some("1000:2000"), 1000, 1000),
        (Some("2000:1000"), 1000, 1000),
        (Some("1000:1000"), 1001, 1000),
        (None, 1001, 1000),
    ] {
        let said = user(flag).with_agent(uid, gid);
        let agent = format!("{uid}:{gid}");
        assert_eq!(said, Err(SettingsError::UserNotAgent { agent }), "{flag:?}");
    }
    let text = SettingsError::UserNotAgent {
        agent: "1000:1000".to_owned(),
    }
    .to_string();
    assert_eq!(
        text,
        "--container-user: must be 1000:1000 (the agent's own uid:gid) unless the agent runs as root"
    );
}

/// Asserts the user of each slot: the container ids of slot `k` are the base ids plus `k`
/// (the default base is `1000:1000`; a base with a different gid keeps the difference),
/// the last slot is still a container id, and a base that leaves no room for all the slots
/// below 65536 is refused when the jobs are mapped. It catches two slots with one user and a
/// slot whose id a job's user namespace does not have.
#[test]
fn each_slot_has_its_own_ids_and_all_of_them_fit() {
    assert_eq!(DEFAULT_USER, "1000:1000");
    let set = Settings::new(bin(), None, None).unwrap();
    assert_eq!(set.slot_ids(0), (1000, 1000));
    assert_eq!(set.slot_user(0), "1000:1000");
    assert_eq!(set.slot_ids(1), (1001, 1001));
    assert_eq!(set.slot_user(2), "1002:1002");
    let last = SLOTS - 1;
    assert_eq!(set.slot_ids(last), (1000 + last, 1000 + last));
    assert!(set.slot_ids(last).0 < 65_536);

    let other = Settings::new(bin(), None, Some("2000:3000")).unwrap();
    assert_eq!(other.slot_ids(5), (2005, 3005));
    assert_eq!(other.slot_user(5), "2005:3005");

    let top = 65_536 - SLOTS;
    for (base, fits) in [
        (format!("{top}:1"), true),
        (format!("1:{top}"), true),
        (format!("{}:1", top + 1), false),
        (format!("1:{}", top + 1), false),
    ] {
        let set = Settings::new(bin(), None, Some(&base)).unwrap();
        let said = set.with_subuid("jobmap").map(|_| ());
        let want = if fits {
            Ok(())
        } else {
            Err(SettingsError::UserOutsideMap)
        };
        assert_eq!(said, want, "{base}");
    }
}

/// Asserts the engine host: a socket on this host (`unix:///<path>`) is taken and kept as
/// written; every address of an engine that runs somewhere else (`tcp://`, `ssh://`, even
/// to the loopback), an empty or bare scheme, another scheme, a space, a control
/// character, a non-ASCII character and a text over 256 bytes are refused. It catches an
/// engine on another machine, where the agent's own `/proc`, mounts and users show
/// nothing of the jobs and every post-job check would pass, and an address that would be
/// split into more environment text than one value.
#[test]
fn the_engine_host_is_a_socket_on_this_host() {
    let long = format!("unix:///{}", "h".repeat(250));
    for host in [
        "",
        "unix://",
        "unix:///",
        "tcp://",
        "ssh://",
        "tcp://127.0.0.1:2375",
        "tcp://localhost:2375",
        "ssh://user@host",
        "http://h",
        "unix://relative",
        "unix://remote-host/run/engine.sock",
        "/run/engine.sock",
        "unix:///run/a b",
        "unix:///run/a\nb",
        "unix:///run/h\u{e9}",
        &format!("{long}x"),
    ] {
        let said = Settings::new(bin(), None, None)
            .unwrap()
            .with_host(Some(host));
        assert_eq!(said, Err(SettingsError::Host), "{host:?}");
    }
    for host in ["unix:///run/engine.sock", &long[..256]] {
        let ok = Settings::new(bin(), None, None)
            .unwrap()
            .with_host(Some(host))
            .unwrap();
        assert_eq!(ok.host(), Some(host));
    }
}

/// Asserts the pull policy and the size of `/tmp`: `never` and `missing`, and sizes of
/// one to nine digits (the first not zero) and `k`, `m` or `g`; anything else is refused.
#[test]
fn the_pull_policy_and_the_tmp_size() {
    let set = || Settings::new(bin(), None, None).unwrap();
    for (text, pull) in [("never", Pull::Never), ("missing", Pull::Missing)] {
        let ok = set().with_pull(Some(text)).unwrap();
        assert_eq!((ok.pull(), ok.pull().as_str()), (pull, text));
    }
    for text in ["", "always", "Never", "never ", "if-not-present"] {
        let said = set().with_pull(Some(text));
        assert_eq!(said, Err(SettingsError::Pull), "{text:?}");
    }
    for text in ["1k", "64m", "2g", "999999999g"] {
        let ok = set().with_tmp_size(Some(text)).unwrap();
        assert_eq!(ok.tmp_size(), text);
    }
    for text in [
        "",
        "m",
        "0m",
        "64",
        "64M",
        "1.5g",
        "-1m",
        "64mb",
        "1000000000g",
        " 1m",
    ] {
        let said = set().with_tmp_size(Some(text));
        assert_eq!(said, Err(SettingsError::TmpSize), "{text:?}");
    }
    assert_eq!(set().with_pull(None).unwrap().pull(), Pull::Never);
    assert_eq!(set().with_tmp_size(None).unwrap().tmp_size(), "64m");
}

/// Asserts the warning an operator gets for a network: none for the default, one naming
/// the flag and the egress for any network. It catches a network configured without the
/// operator being told it gives the job a way out.
#[test]
fn a_network_is_warned_about() {
    let none = Settings::new(bin(), None, None).unwrap();
    assert!(none.warnings().is_empty());
    let named = Settings::new(bin(), Some("jobs"), None).unwrap();
    let warnings = named.warnings();
    assert_eq!(warnings.len(), 1);
    assert!(warnings[0].starts_with("--container-network jobs: a job can reach"));
    assert!(warnings[0].contains("metadata endpoint"));
}

/// Asserts that without `--container-bin` there are no container settings, and that any
/// container flag given alone is an error and not silently ignored; and that all the
/// flags given with it make the settings. It catches a hardening flag that is accepted
/// and does nothing.
#[test]
fn network_and_user_need_the_runtime() {
    assert_eq!(Settings::from_flags(&Flags::default(), AGENT), Ok(None));
    let alone = [
        Flags {
            network: Some("jobs"),
            ..Flags::default()
        },
        Flags {
            user: Some("1:1"),
            ..Flags::default()
        },
        Flags {
            host: Some("unix:///run/engine.sock"),
            ..Flags::default()
        },
        Flags {
            pull: Some("never"),
            ..Flags::default()
        },
        Flags {
            tmp_size: Some("1g"),
            ..Flags::default()
        },
        Flags {
            subuid: Some("jobmap"),
            ..Flags::default()
        },
    ];
    for flags in alone {
        let said = Settings::from_flags(&flags, AGENT);
        assert_eq!(said, Err(SettingsError::NoBin), "{flags:?}");
    }
    let on = Settings::from_flags(
        &Flags {
            bin: Some(bin()),
            network: Some("jobs"),
            user: Some("1000:1000"),
            host: Some("unix:///run/engine.sock"),
            pull: Some("missing"),
            tmp_size: Some("128m"),
            subuid: Some("jobmap"),
        },
        AGENT,
    )
    .unwrap()
    .unwrap();
    assert_eq!((on.network(), on.user()), ("jobs", "1000:1000"));
    assert_eq!(on.host(), Some("unix:///run/engine.sock"));
    assert_eq!((on.pull(), on.tmp_size()), (Pull::Missing, "128m"));
    assert_eq!(on.chown_scratch(), None);
    assert_eq!(on.subuid(), Some("jobmap"));
    assert!(matches!(on.uid(), JobUsers::Mapped(_)));
    let root = Settings::from_flags(
        &Flags {
            bin: Some(bin()),
            user: Some("5:6"),
            subuid: Some("jobmap"),
            ..Flags::default()
        },
        (0, 0),
    )
    .unwrap()
    .unwrap();
    assert_eq!(root.chown_scratch(), Some((5, 6)));
    for flags in [
        Flags {
            bin: Some(bin()),
            host: Some("nope"),
            ..Flags::default()
        },
        Flags {
            bin: Some(bin()),
            pull: Some("nope"),
            ..Flags::default()
        },
        Flags {
            bin: Some(bin()),
            tmp_size: Some("nope"),
            ..Flags::default()
        },
        Flags {
            bin: Some(bin()),
            ..Flags::default()
        },
    ] {
        assert!(Settings::from_flags(&flags, AGENT).is_err(), "{flags:?}");
    }
}

/// Asserts that a container runtime without `--container-subuid` is refused (every job
/// runs in a user namespace mapped onto that user's range), that the user must be a name
/// the id files can hold, that the container user must be an id of the namespace (below
/// 65536), and that settings built without the mapping name the one host uid. It catches
/// a runtime that starts jobs in the host's user namespace and a user that could not be
/// found in `/etc/subuid`.
#[test]
fn the_containers_need_a_subordinate_user() {
    let flags = |subuid, user| Flags {
        bin: Some(bin()),
        user,
        subuid,
        ..Flags::default()
    };
    assert_eq!(
        Settings::from_flags(&flags(None, None), (0, 0)),
        Err(SettingsError::NoSubuid)
    );
    for name in ["", "a b", "a:b", "a\nb", "ü"] {
        let said = Settings::from_flags(&flags(Some(name), None), (0, 0));
        assert_eq!(said, Err(SettingsError::Subuid), "{name:?}");
    }
    for user in ["65536:1", "1:65536", "61441:1", "1:61441", "65535:65535"] {
        let said = Settings::from_flags(&flags(Some("jobmap"), Some(user)), (0, 0));
        assert_eq!(said, Err(SettingsError::UserOutsideMap), "{user}");
    }
    let edge = Settings::from_flags(&flags(Some("jobmap"), Some("61440:61440")), (0, 0));
    assert_eq!(edge.unwrap().unwrap().chown_scratch(), Some((61440, 61440)));
    let plain = Settings::new(bin(), None, None).unwrap();
    assert_eq!((plain.subuid(), plain.uid()), (None, JobUsers::Uid(1000)));
    for (why, flag) in [
        (SettingsError::NoSubuid, "--container-subuid"),
        (SettingsError::Subuid, "--container-subuid"),
        (SettingsError::UserOutsideMap, "--container-user"),
    ] {
        assert!(why.to_string().starts_with(flag), "{why}");
    }
}

/// Asserts that each error names its flag, since the text is what the operator reads.
#[test]
fn errors_name_their_flag() {
    let starts = |why: SettingsError, flag: &str| {
        assert!(why.to_string().starts_with(flag), "{why}");
    };
    starts(SettingsError::BinPath, "--container-bin");
    starts(SettingsError::BinMissing, "--container-bin");
    starts(SettingsError::Network, "--container-network");
    starts(SettingsError::User, "--container-user");
    starts(
        SettingsError::UserNotAgent {
            agent: "1:1".to_owned(),
        },
        "--container-user",
    );
    starts(SettingsError::Host, "--container-host");
    starts(SettingsError::NoSubuid, "--container-subuid");
    starts(SettingsError::Pull, "--container-pull");
    starts(SettingsError::TmpSize, "--container-tmp-size");
    assert!(
        SettingsError::NoBin
            .to_string()
            .contains("need --container-bin")
    );
}

/// Runs the agent with `extra` and returns (status, stderr, messages sent).
fn start(extra: &[&str]) -> (String, String, usize) {
    let (ran, sent, _) = run_scripted(extra, &linux_host(), vec![welcome(1)], 1);
    let sent = sent.borrow().len();
    (ran.status(), ran.err, sent)
}

/// Asserts that good container flags start the agent as before, and that a wrong one
/// ends it with status 2 before it registers, naming the flag, among them a container
/// user other than the agent's own when the agent is not root, and that a network
/// configured prints its warning on stderr and still starts. It catches a container flag
/// that is only found wrong when the first job arrives.
#[test]
fn container_flags_are_checked_when_the_agent_starts() {
    let dir = Scratch::new("cli-settings-flags");
    let exec = dir.write("docker", "#!/bin/sh\n", 0o755);
    let exec = exec.to_str().unwrap();
    let (uid, gid) = own_ids();
    let own = format!("{uid}:{gid}");
    let state = dir.path("state");
    std::fs::create_dir(&state).unwrap();
    std::fs::set_permissions(&state, std::os::unix::fs::PermissionsExt::from_mode(0o700)).unwrap();
    let state = state.to_str().unwrap();
    let scratch = dir.path("scratch");
    std::fs::create_dir(&scratch).unwrap();
    std::fs::set_permissions(
        &scratch,
        std::os::unix::fs::PermissionsExt::from_mode(0o700),
    )
    .unwrap();
    let scratch = scratch.to_str().unwrap();
    if uid > 0 {
        let (status, err, sent) = start(&[
            "--state-root",
            state,
            "--scratch-root",
            scratch,
            "--container-bin",
            exec,
            "--container-network",
            "jobs",
            "--container-user",
            &own,
            "--container-subuid",
            "jobmap",
            "--container-host",
            "unix:///run/engine.sock",
            "--container-pull",
            "missing",
            "--container-tmp-size",
            "1g",
        ]);
        assert_eq!(status, code(0), "{err}");
        assert_eq!(sent, 2, "the hello, and the draining heartbeat of the stop");
        assert!(
            err.contains("js-agent: warning: --container-network jobs: a job can reach"),
            "{err}"
        );

        // The default base is 1000:1000: an agent that is that user takes it.
        if (uid, gid) != (1000, 1000) {
            let (status, err, sent) =
                start(&["--container-bin", exec, "--container-subuid", "jobmap"]);
            assert_eq!(
                status,
                code(2),
                "the default user is not the agent's: {err}"
            );
            assert!(err.contains("--container-user: must be"), "{err}");
            assert_eq!(sent, 0);
        }
    }

    let cases: [(&[&str], &str); 13] = [
        (
            &["--container-bin", "docker"],
            "--container-bin: not an absolute",
        ),
        (
            &["--container-bin", "/nonexistent/docker"],
            "--container-bin: not an executable file",
        ),
        (
            &["--container-bin", exec, "--container-network", "host"],
            "--container-network: not a network name",
        ),
        (
            &["--container-bin", exec, "--container-user", "0:0"],
            "--container-user: not <uid>:<gid>",
        ),
        (
            &["--container-bin", exec, "--container-host", "nope"],
            "--container-host: only a unix:///<path> socket on this host",
        ),
        (
            &[
                "--container-bin",
                exec,
                "--container-host",
                "tcp://127.0.0.1:2375",
            ],
            "--container-host: only a unix:///<path> socket on this host",
        ),
        (
            &[
                "--container-bin",
                exec,
                "--container-host",
                "ssh://user@host",
            ],
            "--container-host: only a unix:///<path> socket on this host",
        ),
        (
            &["--container-bin", exec, "--container-pull", "always"],
            "--container-pull: not never or missing",
        ),
        (
            &["--container-bin", exec, "--container-tmp-size", "big"],
            "--container-tmp-size: not a size",
        ),
        (&["--container-network", "jobs"], "need --container-bin"),
        (
            &["--container-bin", exec],
            "--container-subuid: required with --container-bin",
        ),
        (
            &["--container-bin", exec, "--container-subuid", "a:b"],
            "--container-subuid: not a user name",
        ),
        (&["--container-subuid", "jobmap"], "need --container-bin"),
    ];
    for (extra, text) in cases {
        let (status, err, sent) = start(extra);
        assert_eq!(status, code(2), "{extra:?}");
        assert!(err.contains(text), "{extra:?}: {err}");
        assert_eq!(sent, 0, "{extra:?}");
    }
}
