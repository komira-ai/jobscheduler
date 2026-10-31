//! The native-job flags, checked: what they default to, what they refuse, and what they
//! hand the runtime.

use std::fs;
use std::path::{Path, PathBuf};

use js_agent::native::{NativeFlags, NativeSettings, PfFlags, ProvisionError};
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
        max_file_mib: None,
        max_processes: None,
        pf: PfFlags::default(),
    }
}

/// The pf flags of a pool of ten uids from 600.
fn pool() -> PfFlags<'static> {
    PfFlags {
        uid_start: Some(600),
        uid_count: Some(10),
        ..PfFlags::default()
    }
}

/// Asserts that no native flag at all means no native jobs, and that a native flag without
/// the helper's socket is refused by name. It catches a flag that is ignored because the
/// socket is missing, and an agent that turns native on without being asked.
#[test]
fn native_jobs_need_the_helper_socket() {
    assert!(
        NativeSettings::from_flags(
            &flags(None, None, None, &[]),
            &[],
            Path::new("/srv/journal")
        )
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
        (
            NativeFlags {
                pf: PfFlags {
                    uid_start: Some(600),
                    ..PfFlags::default()
                },
                ..flags(None, None, None, &[])
            },
            "--native-pf-uid-start",
        ),
    ];
    for (given, flag) in cases {
        let why = NativeSettings::from_flags(&given, &[], Path::new("/srv/journal")).unwrap_err();
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
    let settings = NativeSettings::from_flags(&plain, &[state, scratch], Path::new("/srv/journal"))
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
    let settings = NativeSettings::from_flags(&given, &[], Path::new("/srv/journal"))
        .unwrap()
        .unwrap();
    assert_eq!(settings.config().net, Net::Loopback);
    assert_eq!(settings.config().tool_roots, tools);
    assert!(settings.config().egress.is_none(), "no check below on");
    let mut given = flags(Some(socket), Some(&homes), Some("on"), &[]);
    given.pf = pool();
    let settings = NativeSettings::from_flags(&given, &[], Path::new("/srv/journal"))
        .unwrap()
        .unwrap();
    assert_eq!(settings.config().net, Net::On);
    assert!(
        settings.config().egress.is_some(),
        "a cap of on has its check"
    );
}

/// Asserts that without `--native-homes` the home directory is `/Users`: the flags are
/// accepted exactly where that directory exists. It catches a default that is not the
/// helper's.
#[test]
fn the_default_home_directory_is_users() {
    let socket = Path::new("/run/js/helper");
    let said = NativeSettings::from_flags(
        &flags(Some(socket), None, None, &[]),
        &[],
        Path::new("/srv/journal"),
    );
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
        let why = NativeSettings::from_flags(&given, &[], Path::new("/srv/journal")).unwrap_err();
        assert!(why.starts_with(text), "{text}: {why}");
    }
    let good = flags(Some(socket), Some(&homes), None, &[]);
    let why =
        NativeSettings::from_flags(&good, &[Path::new("/tmp/state")], Path::new("/srv/journal"))
            .unwrap_err();
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
    let settings = NativeSettings::from_flags(&with, &[], Path::new("/srv/journal"))
        .unwrap()
        .unwrap();
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
    let why = NativeSettings::from_flags(&with, &[], Path::new("/srv/journal")).unwrap_err();
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
        let why = NativeSettings::from_flags(&given, &[], Path::new("/srv/journal")).unwrap_err();
        assert!(why.starts_with("--native-tool-root: "), "{bad}: {why}");
        assert!(why.contains("is not a directory below /"), "{bad}: {why}");
    }
}

/// Asserts the resource flags: defaults of 4096 MiB and 512 processes, the values given
/// reach the configuration (the file size in bytes), zero is refused, a size that does not
/// fit in bytes is refused, and each needs the helper's socket. It catches a limit that is
/// not applied or that overflows into a small one.
#[test]
fn the_resource_limits_are_checked_and_handed_on() {
    let dir = Scratch::new("native-set-limits");
    let socket = Path::new("/run/js/helper");
    let journal = Path::new("/srv/journal");
    let root = dir.path("");
    let mut given = flags(Some(socket), Some(&root), None, &[]);
    let settings = NativeSettings::from_flags(&given, &[], journal)
        .unwrap()
        .unwrap();
    assert_eq!(settings.config().max_file_bytes, 4096 << 20);
    assert_eq!(settings.config().max_processes, 512);
    assert_eq!(settings.config().journal, journal);
    given.max_file_mib = Some(8);
    given.max_processes = Some(64);
    let settings = NativeSettings::from_flags(&given, &[], journal)
        .unwrap()
        .unwrap();
    assert_eq!(settings.config().max_file_bytes, 8 << 20);
    assert_eq!(settings.config().max_processes, 64);
    for (mib, procs) in [(Some(0), Some(64)), (Some(8), Some(0))] {
        given.max_file_mib = mib;
        given.max_processes = procs;
        let why = NativeSettings::from_flags(&given, &[], journal).unwrap_err();
        assert!(why.contains("above 0"), "{why}");
    }
    given.max_file_mib = Some(u64::MAX);
    given.max_processes = None;
    let why = NativeSettings::from_flags(&given, &[], journal).unwrap_err();
    assert!(why.starts_with("--native-max-file-mib: "), "{why}");
    // Above the helper's caps (1 TiB, and 2^20 processes): refused at start-up, since the
    // helper would refuse every run.
    for (mib, procs) in [((1 << 20) + 1, 64), (8, (1 << 20) + 1)] {
        given.max_file_mib = Some(mib);
        given.max_processes = Some(procs);
        let why = NativeSettings::from_flags(&given, &[], journal).unwrap_err();
        assert!(why.contains("not in 1..="), "{mib} {procs}: {why}");
    }
    given.max_file_mib = Some(1 << 20);
    given.max_processes = Some(1 << 20);
    NativeSettings::from_flags(&given, &[], journal).unwrap();
    let mut alone = flags(None, None, None, &[]);
    alone.max_file_mib = Some(8);
    let why = NativeSettings::from_flags(&alone, &[], journal).unwrap_err();
    assert_eq!(why, "--native-max-file-mib: needs --native-helper-socket");
    alone.max_file_mib = None;
    alone.max_processes = Some(8);
    let why = NativeSettings::from_flags(&alone, &[], journal).unwrap_err();
    assert_eq!(why, "--native-max-processes: needs --native-helper-socket");
}

/// Asserts that a cap of `on` needs the pool's uids, each flag named when it is missing, and
/// that a uid range, an anchor name or an endpoint the provisioner would refuse is refused
/// here by flag. It catches an agent that starts with a cap of `on` and nothing to check, and
/// a policy that is only found wrong when the first job asks for the network.
#[test]
fn a_cap_of_on_needs_the_pool_and_a_policy_that_is_valid() {
    let socket = Path::new("/run/js/helper");
    let dir = Scratch::new("native-set-pf");
    let homes = dir.path("homes");
    fs::create_dir(&homes).unwrap();
    let on = |pf: PfFlags<'_>| {
        let mut given = flags(Some(socket), Some(&homes), Some("on"), &[]);
        given.pf = pf;
        NativeSettings::from_flags(&given, &[], Path::new("/srv/journal")).map(|_| ())
    };
    let bad = |pf: PfFlags<'_>| on(pf).unwrap_err();
    assert!(
        bad(PfFlags::default())
            .starts_with("--native-pf-uid-start: required with --native-network on"),
    );
    let only_start = PfFlags {
        uid_start: Some(600),
        ..PfFlags::default()
    };
    assert!(
        bad(only_start).starts_with("--native-pf-uid-count: required with --native-network on")
    );
    let system = PfFlags {
        uid_start: Some(100),
        uid_count: Some(10),
        ..PfFlags::default()
    };
    assert!(bad(system).starts_with("--native-pf-uid-start and --native-pf-uid-count: "));
    let anchor = PfFlags {
        anchor: Some("com.apple/with space"),
        ..pool()
    };
    assert!(bad(anchor).starts_with("--native-pf-anchor: "));
    let endpoints = ["169.254.169.254:80".to_owned()];
    let metadata = PfFlags {
        endpoints: &endpoints,
        ..pool()
    };
    assert!(bad(metadata).starts_with("--native-pf-endpoint: "));
    let endpoints = ["192.0.2.10:443".to_owned()];
    let all = PfFlags {
        pfctl: Some(Path::new("/sbin/pfctl")),
        anchor: Some("com.apple/pool"),
        state_dir: Some(Path::new("/var/db/pool")),
        endpoints: &endpoints,
        ..pool()
    };
    assert_eq!(on(all), Ok(()));
}

/// Asserts that a pf flag with a cap below `on` is refused, named, for each of the six, and
/// that the same flags with `on` are accepted. It catches a flag that is ignored because the
/// cap makes it moot, where the operator believes a limit is in force.
#[test]
fn a_pf_flag_needs_a_cap_of_on() {
    let socket = Path::new("/run/js/helper");
    let dir = Scratch::new("native-set-pf-off");
    let homes = dir.path("homes");
    fs::create_dir(&homes).unwrap();
    let endpoints = ["192.0.2.10:443".to_owned()];
    let each = [
        (
            "--native-pfctl",
            PfFlags {
                pfctl: Some(Path::new("/sbin/pfctl")),
                ..PfFlags::default()
            },
        ),
        (
            "--native-pf-anchor",
            PfFlags {
                anchor: Some("com.apple/pool"),
                ..PfFlags::default()
            },
        ),
        (
            "--native-pf-state-dir",
            PfFlags {
                state_dir: Some(Path::new("/var/db/pool")),
                ..PfFlags::default()
            },
        ),
        (
            "--native-pf-uid-start",
            PfFlags {
                uid_start: Some(600),
                ..PfFlags::default()
            },
        ),
        (
            "--native-pf-uid-count",
            PfFlags {
                uid_count: Some(10),
                ..PfFlags::default()
            },
        ),
        (
            "--native-pf-endpoint",
            PfFlags {
                endpoints: &endpoints,
                ..PfFlags::default()
            },
        ),
    ];
    for network in [None, Some("off"), Some("loopback")] {
        for (flag, pf) in each {
            let mut given = flags(Some(socket), Some(&homes), network, &[]);
            given.pf = pf;
            let why =
                NativeSettings::from_flags(&given, &[], Path::new("/srv/journal")).unwrap_err();
            assert_eq!(
                why,
                format!("{flag}: needs --native-network on"),
                "{network:?}"
            );
        }
    }
}

/// Asserts that the check the flags make runs the `pfctl` they name, with the verdicts of the
/// provisioner: a `pfctl` that prints nothing says pf is not enabled, one that exits non-zero
/// is a failed call, and one that is not there is a call that could not be made. It catches a
/// check that ignores `--native-pfctl`, or one that reads any of these as enforced.
#[test]
fn the_check_the_flags_make_asks_the_pfctl_they_name() {
    let socket = Path::new("/run/js/helper");
    let dir = Scratch::new("native-set-pf-real");
    let homes = dir.path("homes");
    let state = dir.path("pf-state");
    fs::create_dir(&homes).unwrap();
    let verdict = |pfctl: &str| {
        let mut given = flags(Some(socket), Some(&homes), Some("on"), &[]);
        given.pf = PfFlags {
            pfctl: Some(Path::new(pfctl)),
            state_dir: Some(&state),
            ..pool()
        };
        let settings = NativeSettings::from_flags(&given, &[], Path::new("/srv/journal"))
            .unwrap()
            .unwrap();
        settings.config().egress.as_ref().unwrap().enforced()
    };
    assert!(matches!(
        verdict("/usr/bin/true"),
        Err(ProvisionError::PfDisabled)
    ));
    assert!(matches!(
        verdict("/usr/bin/false"),
        Err(ProvisionError::Failed { .. })
    ));
    assert!(matches!(
        verdict("/nonexistent/pfctl"),
        Err(ProvisionError::Run { .. })
    ));
}
