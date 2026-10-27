//! The container flags: `--container-bin`, `--container-network` and `--container-user`.

use std::ffi::OsStr;
use std::os::unix::ffi::OsStrExt;
use std::path::Path;

use js_agent::cli_runtime::{DEFAULT_NETWORK, DEFAULT_USER, Settings, SettingsError};

use super::BIN;
use crate::support::{Scratch, code, linux_host, run_scripted, welcome};

fn bin() -> &'static Path {
    Path::new(BIN)
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

/// Asserts that without `--container-bin` there are no container settings, and that a
/// network or user given alone is an error and not silently ignored. It catches a
/// hardening flag that is accepted and does nothing.
#[test]
fn network_and_user_need_the_runtime() {
    assert_eq!(Settings::from_flags(None, None, None), Ok(None));
    for (network, user) in [
        (Some("jobs"), None),
        (None, Some("1:1")),
        (Some("a"), Some("1:1")),
    ] {
        let said = Settings::from_flags(None, network, user);
        assert_eq!(said, Err(SettingsError::NoBin));
    }
    let on = Settings::from_flags(Some(bin()), Some("jobs"), Some("5:6"))
        .unwrap()
        .unwrap();
    assert_eq!((on.network(), on.user()), ("jobs", "5:6"));
}

/// Asserts that each error names its flag, since the text is what the operator reads.
#[test]
fn errors_name_their_flag() {
    assert!(
        SettingsError::BinPath
            .to_string()
            .starts_with("--container-bin")
    );
    assert!(
        SettingsError::BinMissing
            .to_string()
            .starts_with("--container-bin")
    );
    assert!(
        SettingsError::Network
            .to_string()
            .starts_with("--container-network")
    );
    assert!(
        SettingsError::User
            .to_string()
            .starts_with("--container-user")
    );
    assert!(SettingsError::NoBin.to_string().contains("--container-bin"));
}

/// Runs the agent with `extra` and returns (status, stderr, messages sent).
fn start(extra: &[&str]) -> (String, String, usize) {
    let (ran, sent, _) = run_scripted(extra, &linux_host(), vec![welcome(1)], 1);
    let sent = sent.borrow().len();
    (ran.status(), ran.err, sent)
}

/// Asserts that good container flags start the agent as before, and that a wrong one
/// ends it with status 2 before it registers, naming the flag. It catches a container
/// flag that is only found wrong when the first job arrives.
#[test]
fn container_flags_are_checked_when_the_agent_starts() {
    let dir = Scratch::new("cli-settings-flags");
    let exec = dir.write("docker", "#!/bin/sh\n", 0o755);
    let exec = exec.to_str().unwrap();
    let (status, err, sent) = start(&[
        "--container-bin",
        exec,
        "--container-network",
        "jobs",
        "--container-user",
        "1000:1000",
    ]);
    assert_eq!(status, code(0), "{err}");
    assert_eq!(sent, 2, "the hello, and the draining heartbeat of the stop");

    let cases: [(&[&str], &str); 5] = [
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
        (&["--container-network", "jobs"], "need --container-bin"),
    ];
    for (extra, text) in cases {
        let (status, err, sent) = start(extra);
        assert_eq!(status, code(2), "{extra:?}");
        assert!(err.contains(text), "{extra:?}: {err}");
        assert_eq!(sent, 0, "{extra:?}");
    }
}
