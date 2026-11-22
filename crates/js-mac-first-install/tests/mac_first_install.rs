//! js-agent on a Mac, installed and then kept up, end to end, as the Mac Studios get it: the
//! package the release builds, staged, sealed, installed and switched by hostkeeper's own code,
//! then the update hook's `prepare`, `cutover`, `verify` and `cleanup` (and `rollback`) run by
//! hostkeeper's step runner as transient launchd jobs, with launchd loading the agent, its root
//! helper and the pf anchor's job from the installed plists, and the agent joining a lease server
//! by its host certificate. On the host that install leaves, the host group's config package is
//! rolled over the running agent (the config hook restarts it onto the new flags) and then the
//! agent itself is updated to the next version. The host the lease server lists carries the
//! capability `full-disk-access` as `not-granted`: the agent the hosted runner installs is not
//! signed by the release's identity and the runner has no privacy profile, so it holds no Full
//! Disk Access, and the agent reports what its own listing shows. (That such an agent refuses every native job with
//! `no-full-disk-access` is the agent's own tests; submitting a job here would need a lease
//! request, a native job body and its outcome read back, which is more machinery than this
//! install test carries.) What of hostkeeper's daemon is reproduced, and
//! what of a Studio, is said in `mac_first_install/hostkeeper.rs` and `mac_first_install/host.rs`.
//!
//! It changes the machine it runs on for good (a group, an APFS volume, packages installed,
//! launchd jobs, files under `/etc`, `/var/lib/hostkeeper`, `/var/db` and `/Library`), so it runs
//! only on a disposable Mac, as root, and never on a host anyone uses: in CI, a hosted macOS
//! runner of the release's beta stage (.github/workflows/js-agent-macos-first-install.yml). Every
//! test is `#[ignore]`d; run them with `--ignored --test-threads=1 --nocapture`, as root, with
//! `JS_FIRST_INSTALL_PKG` naming the package (`dist/js-agent-<version>-macos-arm64.pkg`) and
//! `JS_FIRST_INSTALL_VERSION` its version (the version the release stamped, which the package's
//! `PackageInfo` and its agent's `--version` carry), and `JS_FIRST_INSTALL_UPDATE_PKG` and
//! `JS_FIRST_INSTALL_UPDATE_VERSION` naming a second package of the same programs and its version,
//! which the agent is updated to. libtest takes no flags of a test's own, so these inputs come from
//! the environment, as the native runtime's privileged tests take theirs. A test that is not root,
//! or lacks any of them, fails saying so: it never skips.
//!
//! The file builds on macOS only: hostkeeper's installer and launchd launcher are Mac code.

#![cfg(target_os = "macos")]

#[path = "mac_first_install/config_pkg.rs"]
mod config_pkg;
#[path = "mac_first_install/host.rs"]
mod host;
#[path = "mac_first_install/hostkeeper.rs"]
mod hostkeeper;
#[path = "mac_first_install/look.rs"]
mod look;

use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use hostkeeper_hook::Verdict;
use hostkeeper_steps::Staged;
use js_testkit::dir::TestDir;

use host::{FLAGS_FILE, HOST_ID, LeaseServer, studio};
use hostkeeper::{
    AGENT, CONFIG, CONFIG_SERVICE, Daemon, PACKAGES, RUN, SERVICE, STATE_DIR, current_link,
    service_dir,
};
use look::{Diagnose, launchd_pid, launchd_state, succeeds};

/// The package, as the release's package job leaves it.
const PKG_VAR: &str = "JS_FIRST_INSTALL_PKG";

/// The package's version.
const VERSION_VAR: &str = "JS_FIRST_INSTALL_VERSION";

/// The package of the version the agent is updated to.
const UPDATE_PKG_VAR: &str = "JS_FIRST_INSTALL_UPDATE_PKG";

/// The version the agent is updated to.
const UPDATE_VERSION_VAR: &str = "JS_FIRST_INSTALL_UPDATE_VERSION";

/// The version of the config package the test builds.
const CONFIG_VERSION: &str = "1.0.0";

/// The config hook's state directory by its real path: its params name it `/var/db/js-agent-config`,
/// and `/var` is a link to `/private/var`.
const CONFIG_STATE_DIR: &str = "/private/var/db/js-agent-config";

/// The packages and versions the test installs.
struct Inputs {
    pkg: PathBuf,
    version: String,
    update_pkg: PathBuf,
    update_version: String,
}

/// The daemon's record directory of the service for the order of `n`: one per order, as each
/// order of the Studios ran on a host whose daemon had reported the last one and forgotten it.
fn store(service: &str, n: u64) -> PathBuf {
    Path::new(STATE_DIR).join(format!("updates/{service}-{n}"))
}

/// The packages and their versions, from the environment, on a host that is root and fresh.
fn inputs() -> Inputs {
    assert!(
        rustix::process::geteuid().is_root(),
        "mac_first_install installs packages and loads launchd jobs: run it as root on a disposable Mac"
    );
    let package = |var: &str| {
        let pkg = std::env::var_os(var)
            .map(PathBuf::from)
            .unwrap_or_else(|| panic!("{var} names no package"));
        assert!(pkg.is_file(), "{var}: {} is not a file", pkg.display());
        pkg
    };
    let version = |var: &str| std::env::var(var).unwrap_or_else(|_| panic!("{var} is unset"));
    let found = Inputs {
        pkg: package(PKG_VAR),
        version: version(VERSION_VAR),
        update_pkg: package(UPDATE_PKG_VAR),
        update_version: version(UPDATE_VERSION_VAR),
    };
    assert_ne!(
        found.version, found.update_version,
        "{UPDATE_VERSION_VAR} is the version of {VERSION_VAR}: there is nothing to update to"
    );
    assert!(
        !service_dir().exists(),
        "{} exists: this is not a host js-agent was never installed on",
        service_dir().display()
    );
    found
}

/// Where `link` points, as its text; `None` when there is no link.
fn link_text(link: &Path) -> Option<PathBuf> {
    match fs::read_link(link) {
        Ok(target) => Some(target),
        Err(why) if why.kind() == io::ErrorKind::NotFound => None,
        Err(why) => panic!("read {}: {why}", link.display()),
    }
}

/// The verdict of a step run that succeeded with nothing pending and no reason.
fn assert_succeeded(done: &hostkeeper_steps::Finished) {
    assert_eq!(
        done.verdict,
        Verdict::Succeeded {
            promotion_pending: false
        },
        "{done:?}"
    );
    assert_eq!(done.reason, None, "{done:?}");
}

/// The agent and its helper run under launchd.
fn assert_agent_runs() {
    for label in ["js-agent", "js-agent.session"] {
        assert_eq!(
            launchd_state(label).as_deref(),
            Some("running"),
            "{label} runs under launchd"
        );
    }
}

/// The first install as the Studios had it: hostkeeper stages and seals the package, the hook's
/// `prepare` sets the host up, and the `cutover` finds the release link not on the staged release
/// (on the Studios because hostkeeper names the staged release by its real path,
/// `/private/var/...`, and the hook read the link's `/var/...` as another place; here because
/// the switch leaves `current` unmade). The update fails `link_failed`, the hook's `rollback`
/// and the daemon's `back` take the package away, and the host is left as the four Studios are:
/// the group and the scratch volume `prepare` made, the staged version and its seal, no
/// `current`, no receipt, no job loaded. Returns the release staged then.
fn failed_first_install(daemon: &Arc<Daemon>, version: &str) -> Staged {
    let (staged, fresh) = daemon.stage();
    assert!(fresh, "the first order stages the package");
    let real = Path::new("/private").join(PACKAGES.trim_start_matches('/'));
    assert_eq!(
        staged.dir(),
        real.join(SERVICE).join(version),
        "hostkeeper names the staged release by its real path"
    );
    // The runner checks this before every step; a refusal here would halt the update with
    // `artifact_changed` before the hook runs.
    if let Err(refused) = staged.check(Path::new(
        "tree/Library/Application Support/js-agent/bin/js-agent",
    )) {
        panic!("hostkeeper's check refuses the staged release: {refused}");
    }
    let order = daemon.order(&staged, 1);
    let failed = daemon.run(&order, daemon.switch(false), &store(SERVICE, 1));
    assert!(
        matches!(failed.verdict, Verdict::Failed { .. }),
        "{failed:?}"
    );
    assert_eq!(failed.reason.as_deref(), Some("link_failed"), "{failed:?}");

    // What it leaves: the four Studios' state.
    assert!(
        succeeds("/usr/bin/dscl", &[".", "-read", "/Groups/_jsagent"]),
        "the group prepare made is there"
    );
    let volumes = look::output("/usr/sbin/diskutil", &["apfs", "list"]);
    assert!(
        volumes.contains("js-agent-scratch"),
        "the scratch volume prepare made is there: {volumes}"
    );
    assert!(service_dir().join(version).is_dir(), "the staged version");
    assert!(
        service_dir().join(format!("{version}.seal")).is_file(),
        "its seal"
    );
    assert_eq!(link_text(&current_link()), None, "no current");
    assert!(
        !succeeds("/usr/sbin/pkgutil", &["--pkg-info", "com.komira.js-agent"]),
        "no receipt: the package was taken away"
    );
    for label in RUN {
        assert_eq!(launchd_state(label), None, "{label} is not loaded");
    }
    staged
}

/// The next order, from the host the failed first install left, with the daemon's real switch:
/// the staged version found on the host and sealed again under `/private/var`, the package
/// installed, `current` moved to it, and the hook's steps to `Succeeded`.
fn good_first_install(daemon: &Arc<Daemon>, failed: &Staged, version: &str) -> Staged {
    let (again, fresh) = daemon.stage();
    assert!(
        !fresh,
        "the version staged by the failed attempt is the one used"
    );
    assert_eq!(again.dir(), failed.dir());
    let order = daemon.order(&again, 2);
    let done = daemon.run(&order, daemon.switch(true), &store(SERVICE, 2));
    assert_succeeded(&done);
    assert_eq!(
        link_text(&current_link()),
        Some(PathBuf::from(version)),
        "current names the version, relative to its directory"
    );
    again
}

/// The host group's config package rolled over the running agent: the flags file replaced by the
/// installer, the config hook (`prepare` to `cleanup`, as root, from a launchd job) keeping its
/// state under `/var`, which is a link on a Mac, and restarting the agent onto the new flags.
fn config_update(daemon: &Arc<Daemon>, server: &LeaseServer, scratch: &Path) {
    let before = launchd_pid("js-agent").expect("the agent has a process before the config update");
    let session = server.session_started(HOST_ID);
    assert!(session.is_some(), "the agent is in a session: {session:?}");

    let current = fs::read_to_string(FLAGS_FILE).expect("the flags file");
    let flags = format!("{current}# A change the config package makes.\n");
    let pkg = config_pkg::build(scratch, &flags, CONFIG_VERSION);
    let config = Arc::new(daemon.another(&CONFIG, &pkg, CONFIG_VERSION));
    let (staged, fresh) = config.stage();
    assert!(fresh, "the config package is staged");
    let order = config.order(&staged, 1);
    let done = config.run(&order, config.switch(true), &store(CONFIG_SERVICE, 1));
    assert_succeeded(&done);

    assert_eq!(
        fs::read_to_string(FLAGS_FILE).expect("the flags file"),
        flags,
        "the config package replaced the flags file"
    );
    assert!(
        Path::new(CONFIG_STATE_DIR).is_dir(),
        "the config hook's state directory is at its real path {CONFIG_STATE_DIR}"
    );
    assert!(
        succeeds("/usr/sbin/pkgutil", &["--pkg-info", CONFIG_SERVICE]),
        "the config package has a receipt"
    );
    assert_agent_runs();
    let after = launchd_pid("js-agent").expect("the agent has a process after the config update");
    assert_ne!(after, before, "the agent was restarted onto the new flags");
    let seen = server.host(HOST_ID);
    assert_eq!(seen["state"], "live", "{seen}");
    assert_ne!(
        server.session_started(HOST_ID),
        session,
        "the agent said hello in a session of its own"
    );
}

/// The host entry carries the agent's own answer as the capability `full-disk-access`:
/// `not-granted`, as the agent of this runner is unsigned and holds no grant.
fn assert_full_disk_access_not_granted(server: &LeaseServer) {
    let seen = server.host(HOST_ID);
    assert_eq!(
        seen["capabilities"]["full-disk-access"], "not-granted",
        "the host list shows the agent without Full Disk Access: {seen}"
    );
}

/// The running agent updated to the next version, over the release it is installed from.
fn agent_update(daemon: &Arc<Daemon>, server: &LeaseServer, installed: &Staged, versions: &Inputs) {
    let next = Arc::new(daemon.another(&AGENT, &versions.update_pkg, &versions.update_version));
    let (staged, fresh) = next.stage();
    assert!(fresh, "the next version is staged");
    assert_eq!(
        staged.dir(),
        Path::new("/private")
            .join(PACKAGES.trim_start_matches('/'))
            .join(SERVICE)
            .join(&versions.update_version),
        "hostkeeper names the staged release by its real path"
    );
    let order = next.order_over(&staged, 3, (&versions.version, installed));
    let done = next.run(&order, next.switch(true), &store(SERVICE, 3));
    assert_succeeded(&done);

    assert_eq!(
        link_text(&current_link()),
        Some(PathBuf::from(&versions.update_version)),
        "current names the next version, relative to its directory"
    );
    assert_eq!(
        link_text(&service_dir().join("previous")),
        Some(PathBuf::from(&versions.version)),
        "previous names the version the update replaced"
    );
    assert_agent_runs();
    let receipt = look::output("/usr/sbin/pkgutil", &["--pkg-info", "com.komira.js-agent"]);
    assert!(
        receipt.contains(&format!("version: {}", versions.update_version)),
        "the receipt shows the next version: {receipt}"
    );
    let seen = server.host(HOST_ID);
    assert_eq!(seen["state"], "live", "{seen}");
    assert_eq!(
        seen["agent_version"],
        versions.update_version.as_str(),
        "{seen}"
    );
    assert_full_disk_access_not_granted(server);
}

/// js-agent on a Mac from its first install to an update: a first install whose cutover failed
/// as on the Studios, the next order that installs and starts it, the host group's config
/// package rolled over it, and the agent updated to the next version. The phases are one test
/// because each runs on the host the one before leaves, and libtest runs tests in no order.
///
/// The first install is as the Studios had it: the update fails `link_failed` and is taken away,
/// leaving the group, the scratch volume, the staged version and its seal, and no `current`, no
/// receipt, no job loaded. The next order installs from that host with the daemon's real switch,
/// to `Succeeded`: the agent, its helper and the pf job running under launchd, and the lease
/// server showing the host live with the agent of this version on the native runtime.
///
/// The config package (`/etc/js-agent/flags`, built with `pkgbuild` and `productbuild`) is staged,
/// sealed and installed over the running agent, and its hook's steps run as root from launchd
/// jobs: `Succeeded` with no reason, the new flags in place, the hook's state directory made at its
/// real path `/private/var/db/js-agent-config`, the agent running under another process id, and
/// the host live in a new session.
///
/// The update orders the second package over the first: `Succeeded` with no reason, `current`
/// naming the next version and `previous` the first, the agent and its helper running, and the
/// lease server showing the host live with the agent of the next version.
///
/// Catches a hook that holds `/var/lib/hostkeeper/packages/js-agent/current -> <version>` to be
/// off the staged release `/private/var/lib/hostkeeper/packages/js-agent/<version>`, and every
/// other step of a first install that fails on a real Mac (the host setup, a plist launchd
/// refuses, an agent that does not start or join), a host the failed first install leaves that
/// the next one cannot install on, a config hook that halts on the links above its state
/// directory (`/var` is one) or leaves the agent on the old flags, and an update of a running
/// agent that does not switch `current` and `previous`, or does not bring the agent of the next
/// version up and joined, and an agent that does not report whether it holds Full Disk
/// Access (the host list then lacks the capability or shows another state).
#[test]
#[ignore = "needs root on a disposable Mac: installs the js-agent package, updates it and loads launchd jobs"]
fn js_agent_installs_on_a_mac_after_a_failed_first_install_takes_a_config_package_and_updates_to_the_next_version()
 {
    let versions = inputs();
    let dir = TestDir::under(env!("CARGO_TARGET_TMPDIR"), "mac-first-install");
    let server = studio(dir.path());
    let daemon = Arc::new(Daemon::new(&AGENT, &versions.pkg, &versions.version));
    let stores = vec![
        store(SERVICE, 1),
        store(SERVICE, 2),
        store(CONFIG_SERVICE, 1),
        store(SERVICE, 3),
    ];
    let _diagnose = Diagnose::new(Arc::clone(&daemon.ran), stores);

    let failed = failed_first_install(&daemon, &versions.version);
    let installed = good_first_install(&daemon, &failed, &versions.version);
    assert_agent_runs();
    let seen = server.host(HOST_ID);
    assert_eq!(seen["state"], "live", "{seen}");
    assert_eq!(seen["agent_version"], versions.version.as_str(), "{seen}");
    // The runtime the agent's hello named, which the host list does not show.
    let itself = server.host_as_itself(HOST_ID);
    assert_eq!(itself["runtime"], "sandbox-exec", "{itself}");
    assert_full_disk_access_not_granted(&server);

    config_update(&daemon, &server, &dir.path().join("config-pkg"));
    agent_update(&daemon, &server, &installed, &versions);
}
