//! The native-job flags, checked: what they default to, what they refuse, and what they
//! hand the runtime.

use std::fs;
use std::path::{Path, PathBuf};

use js_agent::native::{NativeFlags, NativeSettings};
use js_mac_pool::seatbelt::{Net, ReadModel};

use crate::support::Scratch;

fn flags<'a>(
    socket: Option<&'a Path>,
    homes: Option<&'a Path>,
    network: Option<&'a str>,
    tool_roots: &'a [PathBuf],
) -> NativeFlags<'a> {
    NativeFlags {
        socket,
        homes,
        network,
        tool_roots,
        deny_read: &[],
    }
}

/// Asserts that no native flag at all means no native jobs, and that a native flag without
/// the helper's socket is refused by name. It catches a flag that is ignored because the
/// socket is missing, and an agent that turns native on without being asked.
#[test]
fn native_jobs_need_the_helper_socket() {
    assert!(
        NativeSettings::from_flags(&flags(None, None, None, &[]), &[])
            .unwrap()
            .is_none()
    );
    let dir = Scratch::new("native-set-socket");
    let homes = dir.path("homes");
    let roots = [PathBuf::from("/usr/bin")];
    let cases = [
        (flags(None, Some(&homes), None, &[]), "--native-homes"),
        (flags(None, None, Some("on"), &[]), "--native-network"),
        (flags(None, None, None, &roots), "--native-tool-root"),
    ];
    for (given, flag) in cases {
        let why = NativeSettings::from_flags(&given, &[]).unwrap_err();
        assert_eq!(why, format!("{flag}: needs --native-helper-socket"));
    }
}

/// Asserts what good flags hand the runtime: the helper's socket, the home directory
/// canonicalised (a path with `..` is spelled plainly), the network level, the tool roots
/// (the system's four by default), the agent's paths as read denials, and the open read
/// model. It catches a runtime configured with a path the profile cannot name.
#[test]
fn good_flags_configure_the_runtime() {
    let dir = Scratch::new("native-set-good");
    let homes = dir.path("homes");
    fs::create_dir(&homes).unwrap();
    let odd = homes.join("..").join("homes");
    let socket = Path::new("/run/js/helper");
    let (state, scratch) = (Path::new("/srv/state"), Path::new("/srv/scratch"));

    let plain = flags(Some(socket), Some(&odd), None, &[]);
    let settings = NativeSettings::from_flags(&plain, &[state, scratch])
        .unwrap()
        .unwrap();
    assert_eq!(settings.socket(), socket);
    let config = settings.config();
    assert_eq!(config.homes, fs::canonicalize(&homes).unwrap());
    assert_eq!(config.net, Net::Off);
    let roots: Vec<&str> = config
        .tool_roots
        .iter()
        .map(|r| r.to_str().unwrap())
        .collect();
    assert_eq!(roots, ["/usr/bin", "/bin", "/usr/sbin", "/sbin"]);
    let denied: Vec<&str> = config.read_denies.iter().map(|d| d.as_str()).collect();
    assert_eq!(
        denied,
        [
            "/srv/state",
            "/srv/scratch",
            "/run/js",
            "/private/etc/kcpassword"
        ],
        "the agent's roots, the helper's socket directory and the auto-login password"
    );
    assert_eq!(config.read_model, ReadModel::Open);

    let tools = [PathBuf::from("/usr/lib")];
    let given = flags(Some(socket), Some(&homes), Some("loopback"), &tools);
    let settings = NativeSettings::from_flags(&given, &[]).unwrap().unwrap();
    assert_eq!(settings.config().net, Net::Loopback);
    assert_eq!(settings.config().tool_roots, tools);
    let given = flags(Some(socket), Some(&homes), Some("on"), &[]);
    let settings = NativeSettings::from_flags(&given, &[]).unwrap().unwrap();
    assert_eq!(settings.config().net, Net::On);
}

/// Asserts that without `--native-homes` the home directory is `/Users`: the flags are
/// accepted exactly where that directory exists. It catches a default that is not the
/// helper's.
#[test]
fn the_default_home_directory_is_users() {
    let socket = Path::new("/run/js/helper");
    let said = NativeSettings::from_flags(&flags(Some(socket), None, None, &[]), &[]);
    match said {
        Ok(settings) => {
            let settings = settings.unwrap();
            assert_eq!(settings.config().homes, fs::canonicalize("/Users").unwrap());
        }
        Err(why) => {
            assert!(!Path::new("/Users").exists(), "{why}");
            assert!(why.starts_with("--native-homes: /Users: "), "{why}");
        }
    }
}

/// Asserts the refusals, each naming its flag: a relative socket, home directory or tool
/// root, a home directory that does not exist, a network level that is none of the three,
/// and an agent path a profile cannot name. It catches a flag that is accepted and fails
/// when the first job starts.
#[test]
fn bad_flags_are_refused_by_name() {
    let dir = Scratch::new("native-set-bad");
    let homes = dir.path("homes");
    fs::create_dir(&homes).unwrap();
    let socket = Path::new("/run/js/helper");
    let relative = [PathBuf::from("bin")];
    let missing = dir.path("missing");
    let cases = [
        (
            flags(Some(Path::new("helper.sock")), Some(&homes), None, &[]),
            "--native-helper-socket: helper.sock is not an absolute path",
        ),
        (
            flags(Some(socket), Some(Path::new("homes")), None, &[]),
            "--native-homes: homes is not an absolute path",
        ),
        (
            flags(Some(socket), Some(&missing), None, &[]),
            "--native-homes: ",
        ),
        (
            flags(Some(socket), Some(&homes), Some("wide"), &[]),
            "--native-network: \"wide\" is not off, loopback or on",
        ),
        (
            flags(Some(socket), Some(&homes), None, &relative),
            "--native-tool-root: bin is not an absolute path",
        ),
    ];
    for (given, text) in cases {
        let why = NativeSettings::from_flags(&given, &[]).unwrap_err();
        assert!(why.starts_with(text), "{text}: {why}");
    }
    let good = flags(Some(socket), Some(&homes), None, &[]);
    let why = NativeSettings::from_flags(&good, &[Path::new("/tmp/state")]).unwrap_err();
    assert!(why.starts_with("/tmp/state: "), "{why}");
}

/// Asserts the read denies a start adds beyond the agent's own: `--native-deny-read` (spelled
/// as the kernel resolves it, and refused when it does not exist), after the helper's socket
/// directory and before the auto-login password. It catches a helper state directory that a
/// run can read.
#[test]
fn explicit_read_denies_are_resolved_and_must_exist() {
    let dir = Scratch::new("native-set-deny");
    let state = dir.path("helper-state");
    fs::create_dir(&state).unwrap();
    let odd = state.join("..").join("helper-state");
    let socket = Path::new("/run/js/helper");
    let given = [odd];
    let root = dir.path("");
    let mut with = flags(Some(socket), Some(&root), None, &[]);
    with.deny_read = &given;
    let settings = NativeSettings::from_flags(&with, &[]).unwrap().unwrap();
    let denied: Vec<&str> = settings
        .config()
        .read_denies
        .iter()
        .map(|d| d.as_str())
        .collect();
    let resolved = fs::canonicalize(&state).unwrap();
    assert_eq!(
        denied,
        [
            "/run/js",
            resolved.to_str().unwrap(),
            "/private/etc/kcpassword"
        ]
    );
    let missing = [dir.path("missing")];
    with.deny_read = &missing;
    let why = NativeSettings::from_flags(&with, &[]).unwrap_err();
    assert!(why.starts_with("--native-deny-read: "), "{why}");
}

/// Asserts that a tool root that is `/` or not a directory is refused: a root of `/` lets
/// any program through, and a missing one is a typo that every start would meet. It catches
/// a tool root that is no restriction.
#[test]
fn a_tool_root_must_be_a_directory_below_the_root() {
    let dir = Scratch::new("native-set-roots");
    let socket = Path::new("/run/js/helper");
    let root = dir.path("");
    for bad in ["/", "/nonexistent-tool-root"] {
        let roots = [PathBuf::from(bad)];
        let given = flags(Some(socket), Some(&root), None, &roots);
        let why = NativeSettings::from_flags(&given, &[]).unwrap_err();
        assert!(why.starts_with("--native-tool-root: "), "{bad}: {why}");
        assert!(why.contains("is not a directory below /"), "{bad}: {why}");
    }
}
