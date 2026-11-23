//! The native-job flags, checked: what they default to, what they refuse, and what they
//! hand the runtime.

use std::fs;
use std::path::{Path, PathBuf};

use js_agent::mac_accounts::Slots;
use js_agent::native::fence::PRIVACY_ONLY_PLACES;
use js_agent::native::{Ceiling, NativeFlags, NativeSettings, PfFlags, ProvisionError};
use js_mac_pool::seatbelt::ReadModel;
use js_mac_session::full_disk_access::FullDiskAccess;

use crate::support::Scratch;

/// The pool's state directory of the flags made here: it need not exist.
const ACCOUNTS: &str = "/srv/accounts";

/// The range of the pool's slots of the flags made here, ten of them.
const RANGE: &str = "501-510";

/// The flags of an agent with the account pool `ACCOUNTS`, its ten slots, `network` and
/// `tool_roots`, and everything else as the command line leaves it.
fn flags<'a>(network: Option<&'a str>, tool_roots: &'a [PathBuf]) -> NativeFlags<'a> {
    NativeFlags {
        accounts: Some(Path::new(ACCOUNTS)),
        account_range: Some(RANGE),
        account_ready: None,
        homes: None,
        network,
        tool_roots,
        deny_read: &[],
        max_file_mib: None,
        max_processes: None,
        pf: PfFlags::default(),
    }
}

/// The flags of an agent given no `--native-accounts` and none of the flags that need it.
fn bare() -> NativeFlags<'static> {
    NativeFlags {
        accounts: None,
        account_range: None,
        ..flags(None, &[])
    }
}

/// What every start denies reads of beyond the agent's own paths and the operator's: the
/// auto-login password, then the places only the privacy controls guard.
fn always_denied() -> Vec<String> {
    let mut denied = vec!["/private/etc/kcpassword".to_owned()];
    denied.extend(PRIVACY_ONLY_PLACES.iter().map(|place| (*place).to_owned()));
    denied
}

/// The pf flags of a pool of ten uids from 600.
fn pool() -> PfFlags<'static> {
    PfFlags {
        uid_start: Some(600),
        uid_count: Some(10),
        ..PfFlags::default()
    }
}

/// Asserts that no native flag at all means no native jobs, and that each native flag without
/// `--native-accounts` is refused by name (`<flag>: needs --native-accounts`): the range, the
/// ready count, the homes, the network level, a tool root, a read deny, each limit and each pf
/// flag. It catches a flag that is ignored because the pool is missing, and an agent that
/// turns native on without being asked.
#[test]
fn native_jobs_need_the_accounts_directory() {
    assert!(
        NativeSettings::from_flags(&bare(), &[], Path::new("/srv/journal"))
            .unwrap()
            .is_none()
    );
    let homes = Path::new("/Users");
    let roots = [PathBuf::from("/usr/bin")];
    let denied = [PathBuf::from("/usr")];
    let endpoints = ["192.0.2.10:443".to_owned()];
    let cases = [
        (
            NativeFlags {
                account_range: Some(RANGE),
                ..bare()
            },
            "--native-account-range",
        ),
        (
            NativeFlags {
                account_ready: Some(1),
                ..bare()
            },
            "--native-account-ready",
        ),
        (
            NativeFlags {
                homes: Some(homes),
                ..bare()
            },
            "--native-homes",
        ),
        (
            NativeFlags {
                network: Some("on"),
                ..bare()
            },
            "--native-network",
        ),
        (
            NativeFlags {
                tool_roots: &roots,
                ..bare()
            },
            "--native-tool-root",
        ),
        (
            NativeFlags {
                deny_read: &denied,
                ..bare()
            },
            "--native-deny-read",
        ),
        (
            NativeFlags {
                max_file_mib: Some(8),
                ..bare()
            },
            "--native-max-file-mib",
        ),
        (
            NativeFlags {
                max_processes: Some(8),
                ..bare()
            },
            "--native-max-processes",
        ),
        (
            NativeFlags {
                pf: PfFlags {
                    pfctl: Some(Path::new("/sbin/pfctl")),
                    ..PfFlags::default()
                },
                ..bare()
            },
            "--native-pfctl",
        ),
        (
            NativeFlags {
                pf: PfFlags {
                    anchor: Some("com.apple/pool"),
                    ..PfFlags::default()
                },
                ..bare()
            },
            "--native-pf-anchor",
        ),
        (
            NativeFlags {
                pf: PfFlags {
                    state_dir: Some(Path::new("/var/db/pool")),
                    ..PfFlags::default()
                },
                ..bare()
            },
            "--native-pf-state-dir",
        ),
        (
            NativeFlags {
                pf: PfFlags {
                    uid_start: Some(600),
                    ..PfFlags::default()
                },
                ..bare()
            },
            "--native-pf-uid-start",
        ),
        (
            NativeFlags {
                pf: PfFlags {
                    uid_count: Some(10),
                    ..PfFlags::default()
                },
                ..bare()
            },
            "--native-pf-uid-count",
        ),
        (
            NativeFlags {
                pf: PfFlags {
                    endpoints: &endpoints,
                    ..PfFlags::default()
                },
                ..bare()
            },
            "--native-pf-endpoint",
        ),
    ];
    for (given, flag) in cases {
        let why = NativeSettings::from_flags(&given, &[], Path::new("/srv/journal")).unwrap_err();
        assert_eq!(why, format!("{flag}: needs --native-accounts"));
    }
}

/// Asserts the pool's flags: `--native-accounts` without `--native-account-range` is refused
/// by name; a range that is not two uids, one that starts below 501, one that ends above
/// 60000 and one that runs backwards are each refused naming `--native-account-range`; more
/// ready accounts than slots is refused naming `--native-account-ready` and both numbers; and
/// a good range, with by default one ready account or the number given (at most the slots),
/// reaches the configuration as the pool. It catches a pool the agent could not open that
/// is found only when the first job asks for an account.
#[test]
fn the_pool_flags_are_checked_and_handed_on() {
    let journal = Path::new("/srv/journal");
    let mut given = flags(None, &[]);
    given.account_range = None;
    let why = NativeSettings::from_flags(&given, &[], journal).unwrap_err();
    assert_eq!(why, "--native-accounts: needs --native-account-range");
    for range in ["500-510", "501-60001", "510-501", "501", "a-b", ""] {
        given.account_range = Some(range);
        let why = NativeSettings::from_flags(&given, &[], journal).unwrap_err();
        assert!(
            why.starts_with("--native-account-range: "),
            "{range}: {why}"
        );
    }
    given.account_range = Some(RANGE);
    given.account_ready = Some(11);
    let why = NativeSettings::from_flags(&given, &[], journal).unwrap_err();
    assert_eq!(
        why,
        "--native-account-ready: 11 is more than the 10 slots of --native-account-range"
    );

    given.account_ready = None;
    let settings = NativeSettings::from_flags(&given, &[], journal)
        .unwrap()
        .unwrap();
    let accounts = &settings.config().accounts;
    assert_eq!(accounts.state, Path::new(ACCOUNTS));
    assert_eq!(accounts.slots, RANGE.parse::<Slots>().unwrap());
    assert_eq!(accounts.ready, 1, "one ready account by default");
    for ready in [1, 3, 10] {
        given.account_ready = Some(ready);
        let settings = NativeSettings::from_flags(&given, &[], journal)
            .unwrap()
            .unwrap();
        assert_eq!(settings.config().accounts.ready, ready);
    }
    given.account_range = Some("501-60000");
    given.account_ready = None;
    let widest = NativeSettings::from_flags(&given, &[], journal);
    assert_eq!(
        widest.unwrap().unwrap().config().accounts.slots,
        "501-60000".parse::<Slots>().unwrap()
    );
    given.accounts = Some(Path::new("accounts"));
    let why = NativeSettings::from_flags(&given, &[], journal).unwrap_err();
    assert_eq!(why, "--native-accounts: accounts is not an absolute path");
}

/// Asserts what good flags hand the runtime: the pool (its directory, its slots, one ready
/// account), the home directory (`/Users`, canonical, given or by default), the network level,
/// the tool roots (the system's four by default), the agent's paths, the pool's directory, the
/// auto-login password and the privacy-only places as read denials, in that order, and the open
/// read model. It catches a runtime configured with a path the profile cannot name, and a
/// read deny left out.
#[test]
fn good_flags_configure_the_runtime() {
    let (state, scratch) = (Path::new("/srv/state"), Path::new("/srv/scratch"));
    let users = fs::canonicalize("/Users").unwrap();

    let plain = flags(None, &[]);
    let settings = NativeSettings::from_flags(&plain, &[state, scratch], Path::new("/srv/journal"))
        .unwrap()
        .unwrap();
    let config = settings.config();
    assert_eq!(config.accounts.state, Path::new(ACCOUNTS));
    assert_eq!(config.accounts.slots, RANGE.parse::<Slots>().unwrap());
    assert_eq!(config.accounts.ready, 1);
    assert_eq!(config.homes, users);
    assert_eq!(config.network, Ceiling::Off);
    let roots: Vec<&str> = config
        .tool_roots
        .iter()
        .map(|r| r.to_str().unwrap())
        .collect();
    assert_eq!(roots, ["/usr/bin", "/bin", "/usr/sbin", "/sbin"]);
    let denied: Vec<&str> = config.read_denies.iter().map(|d| d.as_str()).collect();
    let mut want = vec![
        "/srv/state".to_owned(),
        "/srv/scratch".to_owned(),
        ACCOUNTS.to_owned(),
    ];
    want.extend(always_denied());
    assert_eq!(
        denied, want,
        "the agent's roots, the pool's directory, the auto-login password and the places only \
         the privacy controls guard"
    );
    assert_eq!(config.read_model, ReadModel::Open);
    assert_eq!(config.full_disk_access, FullDiskAccess::Undetermined);

    let tools = [PathBuf::from("/usr/lib")];
    let mut given = flags(Some("loopback"), &tools);
    given.homes = Some(Path::new("/Users"));
    let settings = NativeSettings::from_flags(&given, &[], Path::new("/srv/journal"))
        .unwrap()
        .unwrap();
    assert_eq!(settings.config().homes, users);
    assert_eq!(settings.config().network, Ceiling::Loopback);
    assert_eq!(settings.config().tool_roots, tools);
    assert!(settings.config().egress.is_none(), "no check below on");
    let mut given = flags(Some("on"), &[]);
    given.pf = pool();
    let settings = NativeSettings::from_flags(&given, &[], Path::new("/srv/journal"))
        .unwrap()
        .unwrap();
    assert_eq!(settings.config().network, Ceiling::On);
    assert!(
        settings.config().egress.is_some(),
        "a cap of on has its check"
    );
}

/// Asserts that every path of the agent's own is read-denied by the name the kernel resolves,
/// whether it exists or not: a file that is not there (the token file of a Mac that joins by its
/// host certificate) under a directory reached through a symbolic link (`/etc` is
/// `/private/etc`) is denied by the real path of its deepest ancestor that is there, with the
/// rest after it; and a path with no ancestor at all is refused by name.
///
/// Catches a missing file kept as given, which named the link: the profile refused
/// `/etc/js-agent/token` and the agent ended at every start of a first install on a Mac.
#[test]
fn an_agent_path_that_is_not_there_is_denied_by_the_real_path_it_would_have() {
    let dir = Scratch::new("native-set-planned");
    let real = dir.path("real");
    fs::create_dir(&real).unwrap();
    let alias = dir.path("alias");
    std::os::unix::fs::symlink(&real, &alias).unwrap();
    let real = fs::canonicalize(&real).unwrap();
    let accounts = alias.join("pool/accounts");
    let (token, deeper) = (alias.join("token"), alias.join("none/token"));
    let given = NativeFlags {
        accounts: Some(&accounts),
        ..flags(None, &[])
    };
    let settings = NativeSettings::from_flags(
        &given,
        &[&token, &deeper, &alias],
        Path::new("/srv/journal"),
    )
    .unwrap()
    .unwrap();
    let denied: Vec<&str> = settings
        .config()
        .read_denies
        .iter()
        .map(|d| d.as_str())
        .collect();
    let spelt = |path: PathBuf| path.to_str().unwrap().to_owned();
    let mut want = vec![
        spelt(real.join("token")),
        spelt(real.join("none/token")),
        spelt(real.clone()),
        spelt(real.join("pool/accounts")),
    ];
    want.extend(always_denied());
    assert_eq!(denied, want);

    let relative = Path::new("relative-absent/token");
    let refused = NativeSettings::from_flags(&given, &[relative], Path::new("/srv/journal"));
    assert_eq!(
        refused.err(),
        Some("relative-absent/token: path is not absolute".to_owned())
    );
}

/// Asserts that without `--native-homes` the home directory is `/Users`, and that it is the only
/// value: the flag given as `/Users` (or a path that resolves to it) is accepted, and any
/// other directory is refused naming the flag and the one the pool makes its homes in. It
/// catches a default that is not the pool's, and homes the pool would not make its accounts in.
#[test]
fn the_default_home_directory_is_users_and_the_only_one() {
    let journal = Path::new("/srv/journal");
    let users = fs::canonicalize("/Users").unwrap();
    let made = NativeSettings::from_flags(&flags(None, &[]), &[], journal)
        .unwrap()
        .unwrap();
    assert_eq!(made.config().homes, users);
    let odd = users.join("..").join(users.file_name().unwrap());
    for given in [Path::new("/Users"), odd.as_path()] {
        let mut flags = flags(None, &[]);
        flags.homes = Some(given);
        let made = NativeSettings::from_flags(&flags, &[], journal)
            .unwrap()
            .unwrap();
        assert_eq!(made.config().homes, users, "{}", given.display());
    }

    let dir = Scratch::new("native-set-homes");
    let elsewhere = dir.path("homes");
    fs::create_dir(&elsewhere).unwrap();
    let mut other = flags(None, &[]);
    other.homes = Some(&elsewhere);
    let why = NativeSettings::from_flags(&other, &[], journal).unwrap_err();
    assert!(why.starts_with("--native-homes: "), "{why}");
    assert!(
        why.ends_with(": the account pool makes its homes in /Users"),
        "{why}"
    );
}

/// Asserts the refusals, each naming its flag: a relative pool directory, home directory or
/// tool root, a home directory that does not exist, a network level that is none of the four,
/// and an agent path a profile cannot name (a relative one: an absolute path that is not
/// there is denied by the real path it would have, see
/// `an_agent_path_that_is_not_there_is_denied_by_the_real_path_it_would_have`). It catches a flag that is accepted and fails
/// when the first job starts.
#[test]
fn bad_flags_are_refused_by_name() {
    let dir = Scratch::new("native-set-bad");
    let relative = [PathBuf::from("bin")];
    let missing = dir.path("missing");
    let cases = [
        (
            NativeFlags {
                accounts: Some(Path::new("accounts.d")),
                ..flags(None, &[])
            },
            "--native-accounts: accounts.d is not an absolute path",
        ),
        (
            NativeFlags {
                homes: Some(Path::new("homes")),
                ..flags(None, &[])
            },
            "--native-homes: homes is not an absolute path",
        ),
        (
            NativeFlags {
                homes: Some(&missing),
                ..flags(None, &[])
            },
            "--native-homes: ",
        ),
        (
            flags(Some("wide"), &[]),
            "--native-network: \"wide\" is not off, loopback, on or open",
        ),
        (
            flags(None, &relative),
            "--native-tool-root: bin is not an absolute path",
        ),
    ];
    for (given, text) in cases {
        let why = NativeSettings::from_flags(&given, &[], Path::new("/srv/journal")).unwrap_err();
        assert!(why.starts_with(text), "{text}: {why}");
    }
    let good = flags(None, &[]);
    let why = NativeSettings::from_flags(
        &good,
        &[Path::new("relative/state")],
        Path::new("/srv/journal"),
    )
    .unwrap_err();
    assert!(why.starts_with("relative/state: "), "{why}");
}

/// Asserts the read denies a start adds beyond the agent's own: `--native-deny-read`, spelled
/// as the kernel resolves it, after the pool's directory and before the auto-login password and
/// the places only the privacy controls guard. A path that is not there yet (a directory made
/// after the agent starts, one level or two below a link) is denied by the real path it will
/// have, never refused; a relative path is refused naming the flag. It catches a pool ledger or
/// a privacy database that a run can read, a denied directory named through a link (which a
/// profile cannot name), and a fresh host's agent that never starts because a denied directory
/// is made only after the start.
#[test]
fn explicit_read_denies_are_resolved_and_need_not_exist() {
    let dir = Scratch::new("native-set-deny");
    let state = dir.path("extra-state");
    fs::create_dir(&state).unwrap();
    let alias = dir.path("alias");
    std::os::unix::fs::symlink(&state, &alias).unwrap();
    let odd = state.join("..").join("extra-state");
    let given = [odd, alias.join("ledger"), alias.join("made/later")];
    let mut with = flags(None, &[]);
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
    let spelt = |path: PathBuf| path.to_str().unwrap().to_owned();
    let mut want = vec![
        ACCOUNTS.to_owned(),
        spelt(resolved.clone()),
        spelt(resolved.join("ledger")),
        spelt(resolved.join("made/later")),
    ];
    want.extend(always_denied());
    assert_eq!(denied, want);
    assert!(!resolved.join("ledger").exists(), "the check made the path");

    let relative = [PathBuf::from("relative/ledger")];
    with.deny_read = &relative;
    let why = NativeSettings::from_flags(&with, &[], Path::new("/srv/journal")).unwrap_err();
    assert_eq!(
        why,
        "--native-deny-read: relative/ledger is not an absolute path"
    );
}

/// Asserts that every start read-denies the pool's directory, the auto-login password and each
/// of the places only the privacy controls guard (`PRIVACY_ONLY_PLACES`), with no
/// `--native-deny-read` and no path of the agent's own given. It catches a run that reads the
/// pool's ledger, or the privacy database an agent with Full Disk Access could read for it.
#[test]
fn every_start_denies_the_pool_and_the_privacy_only_places() {
    let journal = Path::new("/srv/journal");
    let settings = NativeSettings::from_flags(&flags(None, &[]), &[], journal)
        .unwrap()
        .unwrap();
    let denied: Vec<&str> = settings
        .config()
        .read_denies
        .iter()
        .map(|d| d.as_str())
        .collect();
    assert!(denied.contains(&ACCOUNTS), "{denied:?}");
    for place in PRIVACY_ONLY_PLACES {
        assert!(denied.contains(&place), "{place}: {denied:?}");
    }
    assert!(denied.contains(&"/private/etc/kcpassword"), "{denied:?}");
}

/// Asserts that a tool root that is `/` or not a directory is refused: a root of `/` lets
/// any program through, and a missing one is a typo that every start would meet. It catches
/// a tool root that is no restriction.
#[test]
fn a_tool_root_must_be_a_directory_below_the_root() {
    for bad in ["/", "/nonexistent-tool-root"] {
        let roots = [PathBuf::from(bad)];
        let given = flags(None, &roots);
        let why = NativeSettings::from_flags(&given, &[], Path::new("/srv/journal")).unwrap_err();
        assert!(why.starts_with("--native-tool-root: "), "{bad}: {why}");
        assert!(why.contains("is not a directory below /"), "{bad}: {why}");
    }
}

/// Asserts the resource flags: defaults of 4096 MiB and 512 processes, the values given
/// reach the configuration (the file size in bytes), zero is refused, a size below 128 MiB
/// (a build's largest output) is refused, a size that does not
/// fit in bytes is refused, a process count of `u32::MAX` (which the supervisor's own process
/// would overflow) is refused, and each needs `--native-accounts`. It catches a limit that is
/// not applied, that overflows into a small one, or that wraps to none.
#[test]
fn the_resource_limits_are_checked_and_handed_on() {
    let journal = Path::new("/srv/journal");
    let mut given = flags(None, &[]);
    let settings = NativeSettings::from_flags(&given, &[], journal)
        .unwrap()
        .unwrap();
    assert_eq!(settings.config().max_file_bytes, 4096 << 20);
    assert_eq!(settings.config().max_processes, 512);
    assert_eq!(settings.config().journal, journal);
    given.max_file_mib = Some(128);
    given.max_processes = Some(64);
    let settings = NativeSettings::from_flags(&given, &[], journal)
        .unwrap()
        .unwrap();
    assert_eq!(settings.config().max_file_bytes, 128 << 20);
    assert_eq!(settings.config().max_processes, 64);
    given.max_file_mib = Some(127);
    let why = NativeSettings::from_flags(&given, &[], journal).unwrap_err();
    assert_eq!(
        why,
        "--native-max-file-mib: 127 is below 128, the largest output a build writes by default"
    );
    for (mib, procs) in [(Some(0), Some(64)), (Some(128), Some(0))] {
        given.max_file_mib = mib;
        given.max_processes = procs;
        let why = NativeSettings::from_flags(&given, &[], journal).unwrap_err();
        assert!(why.contains("above 0"), "{why}");
    }
    given.max_file_mib = Some(u64::MAX);
    given.max_processes = None;
    let why = NativeSettings::from_flags(&given, &[], journal).unwrap_err();
    assert!(why.starts_with("--native-max-file-mib: "), "{why}");
    given.max_file_mib = None;
    given.max_processes = Some(u32::MAX);
    let why = NativeSettings::from_flags(&given, &[], journal).unwrap_err();
    assert_eq!(why, "--native-max-processes: 4294967295 is too large");
    // Only a limit of 0 or one that overflows is refused: the old caps are gone.
    given.max_file_mib = Some((1 << 20) + 1);
    given.max_processes = Some((1 << 20) + 1);
    let settings = NativeSettings::from_flags(&given, &[], journal)
        .unwrap()
        .unwrap();
    assert_eq!(settings.config().max_file_bytes, ((1 << 20) + 1) << 20);
    assert_eq!(settings.config().max_processes, (1 << 20) + 1);
    given.max_processes = Some(u32::MAX - 1);
    NativeSettings::from_flags(&given, &[], journal).unwrap();
    let mut alone = bare();
    alone.max_file_mib = Some(8);
    let why = NativeSettings::from_flags(&alone, &[], journal).unwrap_err();
    assert_eq!(why, "--native-max-file-mib: needs --native-accounts");
    alone.max_file_mib = None;
    alone.max_processes = Some(8);
    let why = NativeSettings::from_flags(&alone, &[], journal).unwrap_err();
    assert_eq!(why, "--native-max-processes: needs --native-accounts");
}

/// Asserts that a cap of `on` needs the pool's uids, each flag named when it is missing, and
/// that a uid range, an anchor name or an endpoint the provisioner would refuse is refused
/// here by flag. It catches an agent that starts with a cap of `on` and nothing to check, and
/// a policy that is only found wrong when the first job asks for the network.
#[test]
fn a_cap_of_on_needs_the_pool_and_a_policy_that_is_valid() {
    let on = |pf: PfFlags<'_>| {
        let mut given = flags(Some("on"), &[]);
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
    let relative = PfFlags {
        pfctl: Some(Path::new("pfctl")),
        ..pool()
    };
    assert_eq!(
        bad(relative),
        "--native-pfctl: pfctl is not an absolute path"
    );
    let relative = PfFlags {
        state_dir: Some(Path::new("state")),
        ..pool()
    };
    assert_eq!(
        bad(relative),
        "--native-pf-state-dir: state is not an absolute path"
    );
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

/// Each pf flag alone, by its name: `endpoints` is the one endpoint the last one gives.
fn each_pf_flag(endpoints: &[String]) -> [(&'static str, PfFlags<'_>); 6] {
    [
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
                endpoints,
                ..PfFlags::default()
            },
        ),
    ]
}

/// Asserts that a pf flag with a cap below `on` is refused, named, for each of the six, and
/// that the same flags with `on` are accepted. It catches a flag that is ignored because the
/// cap makes it moot, where the operator believes a limit is in force.
#[test]
fn a_pf_flag_needs_a_cap_of_on() {
    let endpoints = ["192.0.2.10:443".to_owned()];
    let each = each_pf_flag(&endpoints);
    for network in [None, Some("off"), Some("loopback")] {
        for (flag, pf) in each {
            let mut given = flags(network, &[]);
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
    let dir = Scratch::new("native-set-pf-real");
    let state = dir.path("pf-state");
    let verdict = |pfctl: &str| {
        let mut given = flags(Some("on"), &[]);
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

/// The settings of `network` with the pf flags `pf`.
fn with_network(network: &str, pf: PfFlags<'_>) -> Result<NativeSettings, String> {
    let mut given = flags(Some(network), &[]);
    given.pf = pf;
    NativeSettings::from_flags(&given, &[], Path::new("/srv/journal")).map(|found| found.unwrap())
}

/// Asserts that `open` is a ceiling of its own: accepted without any pf flag, it hands the
/// runtime the ceiling `open` and no pf check (nothing filters its jobs). Catches `open`
/// refused as an unknown value, read as `on` (which would demand the anchor), or given a
/// check it would then ask before jobs.
#[test]
fn native_network_accepts_open() {
    let settings = with_network("open", PfFlags::default()).unwrap();
    assert_eq!(settings.config().network, Ceiling::Open);
    assert!(settings.config().egress.is_none(), "open has no check");
}

/// Asserts that a value that is none of the four is refused naming the flag, the value and
/// all four values, by exact text, for a word close to one of them and for an empty one.
/// Catches an unknown word read as a known level (a typo of `off` that opens the network).
#[test]
fn native_network_rejects_unknown() {
    for word in ["opened", "", "ON", "allow-list"] {
        let Err(why) = with_network(word, PfFlags::default()) else {
            panic!("{word:?} is not a ceiling");
        };
        assert_eq!(
            why,
            format!("--native-network: {word:?} is not off, loopback, on or open"),
        );
    }
}

/// Asserts that each pf flag beside `--native-network open` stops the agent at start, naming
/// the flag and `--native-network open` by exact text, and that the same flag beside `on`
/// (with the pool's uids) is accepted. Catches an operator who sets a filter with `open` and
/// believes it is in force, while the agent never checks it.
#[test]
fn open_with_pf_flags_is_a_startup_error() {
    let endpoints = ["192.0.2.10:443".to_owned()];
    for (flag, pf) in each_pf_flag(&endpoints) {
        let Err(why) = with_network("open", pf) else {
            panic!("{flag} with open");
        };
        assert_eq!(
            why,
            format!(
                "{flag}: not valid with --native-network open, which sets no packet filter \
                 (the pf flags limit the network of --native-network on)"
            )
        );
        let on = PfFlags {
            uid_start: Some(600),
            uid_count: Some(10),
            ..pf
        };
        assert!(with_network("on", on).is_ok(), "{flag} with on");
    }
}

/// Asserts the gate of `on` is unchanged by `open`: `on` with no pf flag is refused at start
/// for the pool's uids, as before. Catches an `on` that now starts with nothing to check
/// because `open` needs none.
#[test]
fn on_without_pf_flags_still_fails_to_open() {
    let Err(why) = with_network("on", PfFlags::default()) else {
        panic!("on needs the pool's uids");
    };
    assert_eq!(
        why,
        "--native-pf-uid-start: required with --native-network on: the uids of the pool whose \
         egress the pf anchor limits"
    );
}

/// Asserts that settings made from the flags know nothing of Full Disk Access (it is
/// `undetermined`, which refuses every native job) until the agent has listed the privacy
/// database's directory and says so with `with_full_disk_access`, which changes that state and nothing else. Catches
/// settings that start out granted (a Mac that runs jobs before it asked) and a builder that
/// disturbs another setting.
#[test]
fn settings_are_undetermined_until_the_agent_has_looked() {
    let dir = Scratch::new("native-set-grant");
    let flags = flags(None, &[]);
    let made = NativeSettings::from_flags(&flags, &[], &dir.path("journal"))
        .unwrap()
        .unwrap();
    assert_eq!(made.config().full_disk_access, FullDiskAccess::Undetermined);
    let max_processes = made.config().max_processes;
    for state in [
        FullDiskAccess::Granted,
        FullDiskAccess::NotGranted,
        FullDiskAccess::Undetermined,
    ] {
        let told = made.clone().with_full_disk_access(state);
        assert_eq!(told.config().full_disk_access, state);
        assert_eq!(told.config().max_processes, max_processes);
        assert_eq!(told.config().accounts, made.config().accounts);
    }
}
