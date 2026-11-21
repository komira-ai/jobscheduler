//! js-agent's first install on a Mac, end to end, as the Mac Studios get it: the package the
//! release builds, staged, sealed, installed and switched by hostkeeper's own code, then the
//! update hook's `prepare`, `cutover`, `verify` and `cleanup` (and `rollback`) run by hostkeeper's
//! step runner as transient launchd jobs, with launchd loading the agent, its root helper and
//! the pf anchor's job from the installed plists, and the agent joining a lease server by its
//! host certificate. What of hostkeeper's daemon is reproduced, and what of a Studio, is said in
//! `mac_first_install/hostkeeper.rs` and `mac_first_install/host.rs`.
//!
//! It changes the machine it runs on for good (a group, an APFS volume, the package installed,
//! launchd jobs, files under `/etc`, `/var/lib/hostkeeper`, `/var/db/js-agent` and
//! `/Library`), so it runs only on a disposable Mac, as root, and never on a host anyone uses:
//! in CI, a hosted macOS runner of the release's beta stage
//! (.github/workflows/js-agent-macos-first-install.yml). Every test is `#[ignore]`d; run them with
//! `--ignored --test-threads=1 --nocapture`, as root, with `JS_FIRST_INSTALL_PKG` naming the
//! package (`dist/js-agent-<version>-macos-arm64.pkg`) and `JS_FIRST_INSTALL_VERSION` its version
//! (the version the release stamped, which the package's `PackageInfo` and its agent's
//! `--version` carry). libtest takes no flags of a test's own, so these two inputs come from the
//! environment, as the native runtime's privileged tests take theirs. A test that is not root,
//! or lacks either, fails saying so: it never skips.
//!
//! The file builds on macOS only: hostkeeper's installer and launchd launcher are Mac code.

#![cfg(target_os = "macos")]

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
use js_testkit::dir::TestDir;

use host::{HOST_ID, studio};
use hostkeeper::{Daemon, PACKAGES, RUN, STATE_DIR, current_link, service_dir};
use look::{Diagnose, launchd_state, succeeds};

/// The package, as the release's package job leaves it.
const PKG_VAR: &str = "JS_FIRST_INSTALL_PKG";

/// The package's version.
const VERSION_VAR: &str = "JS_FIRST_INSTALL_VERSION";

/// The daemon's record directory of the service for the order of `n`: one per order, as each
/// order of the Studios ran on a host whose daemon had reported the last one and forgotten it.
fn store(n: u64) -> PathBuf {
    Path::new(STATE_DIR).join(format!("updates/js-agent-{n}"))
}

/// The package and its version, from the environment, on a host that is root and fresh.
fn inputs() -> (PathBuf, String) {
    assert!(
        rustix::process::geteuid().is_root(),
        "mac_first_install installs a package and loads launchd jobs: run it as root on a disposable Mac"
    );
    let pkg = std::env::var_os(PKG_VAR)
        .map(PathBuf::from)
        .unwrap_or_else(|| panic!("{PKG_VAR} names no package"));
    let version = std::env::var(VERSION_VAR).unwrap_or_else(|_| panic!("{VERSION_VAR} is unset"));
    assert!(pkg.is_file(), "{PKG_VAR}: {} is not a file", pkg.display());
    assert!(
        !service_dir().exists(),
        "{} exists: this is not a host js-agent was never installed on",
        service_dir().display()
    );
    (pkg, version)
}

/// Where `link` points, as its text; `None` when there is no link.
fn link_text(link: &Path) -> Option<PathBuf> {
    match fs::read_link(link) {
        Ok(target) => Some(target),
        Err(why) if why.kind() == io::ErrorKind::NotFound => None,
        Err(why) => panic!("read {}: {why}", link.display()),
    }
}

/// The first install as the Studios had it: hostkeeper stages and seals the package, the hook's
/// `prepare` sets the host up, and the `cutover` finds the release link not on the staged release
/// (on the Studios because hostkeeper names the staged release by its real path,
/// `/private/var/...`, and the hook read the link's `/var/...` as another place; here because
/// the switch leaves `current` unmade). The update fails `link_failed`, the hook's `rollback`
/// and the daemon's `back` take the package away, and the host is left as the four Studios are:
/// the group and the scratch volume `prepare` made, the staged version and its seal, no
/// `current`, no receipt, no job loaded.
///
/// Then the next order, from that host, with the daemon's real switch: the staged version found
/// on the host and sealed again under `/private/var`, the package installed, `current` moved to
/// it, and the hook's steps to `Succeeded`: the agent, its helper and the pf job running under
/// launchd, and the lease server showing the host live with the agent of this version on the
/// native runtime.
///
/// Catches a hook that holds `/var/lib/hostkeeper/packages/js-agent/current -> <version>` to be
/// off the staged release `/private/var/lib/hostkeeper/packages/js-agent/<version>`, and every
/// other step of a first install that fails on a real Mac (the host setup, a plist launchd
/// refuses, an agent that does not start or join), and a host the failed first install leaves
/// that the next one cannot install on.
#[test]
#[ignore = "needs root on a disposable Mac: installs the js-agent package and loads launchd jobs"]
fn a_first_install_whose_cutover_failed_as_on_the_studios_leaves_a_host_the_next_order_installs_and_starts()
 {
    let (pkg, version) = inputs();
    let dir = TestDir::under(env!("CARGO_TARGET_TMPDIR"), "mac-first-install");
    let server = studio(dir.path());
    let daemon = Arc::new(Daemon::new(&pkg, &version));
    let _diagnose = Diagnose::new(Arc::clone(&daemon.ran), vec![store(1), store(2)]);

    // The failed attempt.
    let (staged, fresh) = daemon.stage();
    assert!(fresh, "the first order stages the package");
    let real = Path::new("/private").join(PACKAGES.trim_start_matches('/'));
    assert_eq!(
        staged.dir(),
        real.join("js-agent").join(&version),
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
    let failed = daemon.run(&order, daemon.switch(false), &store(1));
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
    assert!(service_dir().join(&version).is_dir(), "the staged version");
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

    // The next order, from that host.
    let (again, fresh) = daemon.stage();
    assert!(
        !fresh,
        "the version staged by the failed attempt is the one used"
    );
    assert_eq!(again.dir(), staged.dir());
    let order = daemon.order(&again, 2);
    let done = daemon.run(&order, daemon.switch(true), &store(2));
    assert_eq!(
        done.verdict,
        Verdict::Succeeded {
            promotion_pending: false
        },
        "{done:?}"
    );
    assert_eq!(done.reason, None, "{done:?}");
    assert_eq!(
        link_text(&current_link()),
        Some(PathBuf::from(&version)),
        "current names the version, relative to its directory"
    );
    for label in ["js-agent", "js-agent.session"] {
        assert_eq!(
            launchd_state(label).as_deref(),
            Some("running"),
            "{label} runs under launchd"
        );
    }
    let seen = server.host(HOST_ID);
    assert_eq!(seen["state"], "live", "{seen}");
    assert_eq!(seen["agent_version"], version.as_str(), "{seen}");
    // The runtime the agent's hello named, which the host list does not show.
    let itself = server.host_as_itself(HOST_ID);
    assert_eq!(itself["runtime"], "sandbox-exec", "{itself}");
}
