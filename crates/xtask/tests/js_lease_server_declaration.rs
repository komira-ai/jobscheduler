//! The host updater's declaration of js-lease-server
//! (`packaging/js-lease-server/updater.d/js-lease-server.toml`), read as the repository has it:
//! the updater's own validator accepts it (kbf's `update-rights`, at the revision the root
//! manifest pins), the hook reads it and the flags in its `hook_args`, its timeouts are the hook's
//! bounds exactly, its write paths are exactly the release directory, the release link's
//! directory and the hook record, and the unit it names is the one the flags name. Nothing here
//! runs the updater or the hook: these are checks of the file. (The release archive that carries
//! it is checked in `release_stage.rs`.)
#![cfg(target_os = "linux")]

use std::ffi::OsString;
use std::path::{Path, PathBuf};

use js_lease_server::update_hook::{
    CLEANUP_SECS, Declaration as HookDeclaration, Invocation, LOCK_WAIT_SECS, MIN_TIMEOUTS,
    PREPARE_WORK_SECS, Params, SERVE_WAIT_SECS, STOP_WAIT_SECS, VERIFY_WAIT_SECS,
};
use js_update_hook::{
    CUTOVER_MARGIN_SECS, PREPARE_MARGIN_SECS, ROLLBACK_MARGIN_SECS, START_WAIT_SECS,
    VERIFY_MARGIN_SECS,
};
use update_rights::{Declaration, Refusal, reserved};
use xtask::release::stage::SERVER_DECLARATION_SOURCE;

/// Where the updater stages the releases of js-lease-server, one directory each.
const RELEASES: &str = "/opt/js-lease-server/releases";
/// The keys the updater adds to the params file of an update.
const UPDATER_KEYS: &str = concat!(
    "\"updater.artifact_digest\" = \"sha256:0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef\"\n",
    "\"updater.staged_release\" = \"/opt/js-lease-server/releases/1.4.0\"\n",
    "\"updater.previous_release\" = \"/opt/js-lease-server/releases/1.3.2\"\n",
    "\"updater.target_version\" = \"1.4.0\"\n",
    "\"updater.previous_version\" = \"1.3.2\"\n",
);
const WRITE_PATHS: &str = r#"write_paths  = ["/opt/js-lease-server/releases", "/opt/js-lease-server/link", "/var/lib/js-lease-server-update"]"#;

fn shipped() -> String {
    let root = Path::new(concat!(env!("CARGO_MANIFEST_DIR"), "/../.."));
    std::fs::read_to_string(root.join(SERVER_DECLARATION_SOURCE))
        .unwrap_or_else(|e| panic!("{SERVER_DECLARATION_SOURCE}: {e}"))
}

/// What the hook makes of a declaration: the updater's view, the hook's own, the command line of
/// `hook_args` and the params of an update with its flags put in.
struct Read {
    updater: Declaration,
    hook: HookDeclaration,
    invocation: Invocation,
    params: Params,
}

/// Every rule a declaration of js-lease-server must keep, the reason for the first it breaks.
///
/// The updater's rules are its own code: `Declaration::parse` (the rules of the text) and, for a
/// hook that runs as root, the loader's rule that each write path is in a directory named for
/// the service (`reserved::check_service_write`). The loader's other rules read the host and are
/// the host's, not the file's.
fn check(text: &str) -> Result<Read, String> {
    let updater = Declaration::parse(text).map_err(|e| format!("the updater refuses it: {e}"))?;
    if updater.name != "js-lease-server" || updater.binary != "js-lease-server" {
        return Err(format!(
            "name {:?} and binary {:?} are not js-lease-server",
            updater.name, updater.binary
        ));
    }
    if updater.hook.user == "root" {
        for path in &updater.hook.write_paths {
            reserved::check_service_write(path, &updater.name).map_err(|why| {
                let refusal = Refusal::Reserved {
                    field: "hook.write_paths",
                    path: path.clone(),
                    why,
                };
                format!("the updater refuses it: {refusal}")
            })?;
        }
    }
    let hook = HookDeclaration::parse(text).map_err(|e| format!("the hook refuses it: {e}"))?;
    let (verb, flags) = updater
        .hook_args
        .split_first()
        .ok_or("hook_args is empty")?;
    if verb != "update-hook" {
        return Err(format!("hook_args starts with {verb:?}, not update-hook"));
    }
    let words = flags
        .iter()
        .map(String::as_str)
        .chain(["cutover", "--params", "/params"])
        .map(OsString::from);
    let invocation =
        Invocation::parse(words).map_err(|e| format!("the hook refuses hook_args: {e}"))?;
    let table = toml::to_string(&updater.hook.params).map_err(|e| e.to_string())?;
    let params = Params::parse(&format!("{table}{UPDATER_KEYS}"))
        .map_err(|e| format!("the hook refuses the params: {e}"))?
        .with_flags(&invocation.flags);
    let link = params
        .release_link
        .clone()
        .ok_or("no release link: cutover and rollback refuse to run")?;
    let link_dir = link.parent().map(Path::to_path_buf).unwrap_or_default();
    if !hook.write_paths.contains(&link_dir) {
        return Err(format!(
            "the release link's directory {} is not a write path",
            link_dir.display()
        ));
    }
    // The hook, as root, may read what it needs: the store file and the release link.
    let store = invocation.flags.store.clone().ok_or("no --store")?;
    for (what, path) in [("the store file", &store), ("the release link", &link)] {
        let readable = updater
            .hook
            .read_paths
            .iter()
            .any(|dir| path.starts_with(dir));
        if !readable {
            return Err(format!("{what} {} is under no read path", path.display()));
        }
    }
    // Exactly the release directory, the release link's directory and the hook record.
    let mut want = vec![PathBuf::from(RELEASES), link_dir, hook.state_dir.clone()];
    want.sort();
    let mut found = hook.write_paths.clone();
    found.sort();
    if found != want {
        return Err(format!(
            "write paths {found:?} are not the release directory, the release link's directory and the state directory {want:?}"
        ));
    }
    let theirs = &updater.hook.timeout_secs;
    let declared = [
        theirs.prepare,
        theirs.cutover,
        theirs.verify,
        theirs.rollback,
        theirs.cleanup,
    ];
    if declared != MIN_TIMEOUTS.by_step().map(|(_, secs)| secs) {
        return Err("the updater's timeouts are not the hook's bounds".to_owned());
    }
    if hook.timeouts != MIN_TIMEOUTS {
        return Err("the hook's timeouts are not its bounds".to_owned());
    }
    if updater.hook.units != [hook.unit.clone()].into() || params.unit != hook.unit {
        return Err(format!(
            "the unit {:?} is not the one --unit names ({:?})",
            hook.unit, params.unit
        ));
    }
    Ok(Read {
        updater,
        hook,
        invocation,
        params,
    })
}

fn refused(text: &str) -> String {
    match check(text) {
        Ok(_) => panic!("accepted:\n{text}"),
        Err(why) => why,
    }
}

/// The declaration as shipped, with `old` replaced by `new` (which must be there once).
fn shipped_with(old: &str, new: &str) -> String {
    let text = shipped();
    assert_eq!(text.matches(old).count(), 1, "{old}");
    text.replace(old, new)
}

/// The file passes the updater's validator and the hook's reading, and says what the hook needs:
/// root, the server's unit, a release link, the store and the address as flags, numbers only in
/// `[hook.params]`, and timeouts that are each the sum of the waits of the step. Catches a
/// declaration the updater would mark `unmanaged`, or one whose timeouts drifted from the hook's.
#[test]
fn js_lease_server_declaration_loads_with_the_updater_validator_and_the_hook_reader() {
    let read = check(&shipped()).unwrap_or_else(|why| panic!("{why}"));
    assert_eq!(read.updater.name, "js-lease-server");
    assert_eq!(read.updater.uid, "js-lease-server");
    assert_eq!(read.updater.hook.user, "root");
    assert_eq!(read.updater.hook.group, "root");
    assert!(read.updater.hook.capabilities.is_empty());
    assert!(read.updater.hook.environment.is_empty());
    assert_eq!(read.updater.signer.repository, "komira-ai/jobscheduler");
    assert_eq!(read.updater.signer.workflow, "release.yml");
    assert_eq!(read.updater.state_dir, "/var/lib/js-lease-server-update");
    assert_eq!(read.hook.state_dir, Path::new(&read.updater.state_dir));
    assert_eq!(
        read.updater.hook.units.iter().collect::<Vec<_>>(),
        ["js-lease-server.service"]
    );
    assert_eq!(read.hook.unit, "js-lease-server.service");
    assert_eq!(read.updater.hook_args[0], "update-hook");
    let flags = &read.invocation.flags;
    assert_eq!(
        flags.release_link,
        Some(PathBuf::from("/opt/js-lease-server/link/current"))
    );
    assert_eq!(flags.unit.as_deref(), Some("js-lease-server.service"));
    assert_eq!(
        flags.store,
        Some(PathBuf::from("/etc/js-lease-server/store"))
    );
    assert_eq!(flags.healthz, Some("127.0.0.1:7300".parse().unwrap()));
    assert_eq!(read.params.unit, "js-lease-server.service");
    assert!(
        read.updater
            .hook
            .params
            .values()
            .all(|value| value.is_integer())
    );
    assert_eq!(read.params.defer_retry_secs, 60);
    // The timeouts are the hook's bounds, and each is the sum of its waits.
    let t = read.hook.timeouts;
    assert_eq!(t, MIN_TIMEOUTS);
    assert_eq!(
        t.by_step().map(|(_, secs)| secs),
        [
            PREPARE_WORK_SECS + PREPARE_MARGIN_SECS,
            STOP_WAIT_SECS + LOCK_WAIT_SECS + START_WAIT_SECS + CUTOVER_MARGIN_SECS,
            VERIFY_WAIT_SECS + VERIFY_MARGIN_SECS,
            STOP_WAIT_SECS
                + LOCK_WAIT_SECS
                + START_WAIT_SECS
                + SERVE_WAIT_SECS
                + ROLLBACK_MARGIN_SECS,
            CLEANUP_SECS,
        ]
    );
    assert_eq!(t.by_step().map(|(_, secs)| secs), [90, 300, 300, 420, 60]);
    // The hook's timeouts are the updater's.
    let theirs = &read.updater.hook.timeout_secs;
    assert_eq!(
        t.by_step().map(|(_, secs)| secs),
        [
            theirs.prepare,
            theirs.cutover,
            theirs.verify,
            theirs.rollback,
            theirs.cleanup,
        ]
    );
}

/// A write path inside another is refused by the hook's reader, and a layout other than the
/// release directory, the release link's directory and the hook record is refused by the
/// checker, each with its exact reason. Catches a declaration that hands a release or the
/// server a way to reach the hook record or the link.
#[test]
fn js_lease_server_declaration_with_nested_write_paths_is_refused() {
    let widen = |extra: &str| {
        shipped_with(
            WRITE_PATHS,
            &format!(
                r#"write_paths  = ["/opt/js-lease-server/releases", "/opt/js-lease-server/link", "/var/lib/js-lease-server-update", "{extra}"]"#
            ),
        )
    };
    assert_eq!(
        refused(&widen("/opt/js-lease-server/link/inner")),
        "the hook refuses it: hook.write_paths /opt/js-lease-server/link/inner lies inside /opt/js-lease-server/link"
    );
    // The state directory moved under the releases.
    let moved = shipped().replace(
        "/var/lib/js-lease-server-update",
        "/opt/js-lease-server/releases/state",
    );
    assert_eq!(
        refused(&moved),
        "the hook refuses it: hook.write_paths /opt/js-lease-server/releases/state lies inside /opt/js-lease-server/releases"
    );
    // Not nested but not the layout either: the parent of the releases and the link.
    let found = [
        "/opt/js-lease-server/link",
        "/opt/js-lease-server/releases",
        "/var/lib/js-lease-server",
        "/var/lib/js-lease-server-update",
    ];
    assert_eq!(
        refused(&widen("/var/lib/js-lease-server")),
        format!(
            "write paths {found:?} are not the release directory, the release link's directory and the state directory [\"/opt/js-lease-server/link\", \"/opt/js-lease-server/releases\", \"/var/lib/js-lease-server-update\"]"
        )
    );
    assert_eq!(
        refused(&widen("/var/lib/other")),
        "the updater refuses it: hook.write_paths: path \"/var/lib/other\" is not at or below a directory named for the service, which a hook that runs as root needs"
    );
    assert_eq!(
        refused(&widen("/etc/js-lease-server")),
        "the updater refuses it: hook.write_paths: path \"/etc/js-lease-server\" is inside the reserved directory /etc, where nothing may be written"
    );
    // The release directory left out is refused too: the layout is exact.
    assert_eq!(
        refused(&shipped_with(
            WRITE_PATHS,
            r#"write_paths  = ["/opt/js-lease-server/link", "/var/lib/js-lease-server-update"]"#
        )),
        "write paths [\"/opt/js-lease-server/link\", \"/var/lib/js-lease-server-update\"] are not the release directory, the release link's directory and the state directory [\"/opt/js-lease-server/link\", \"/opt/js-lease-server/releases\", \"/var/lib/js-lease-server-update\"]"
    );
}

/// A timeout one second below its bound is refused by the hook's reader, naming the step and
/// both numbers, for each of the five steps. Catches a declaration whose step the updater would
/// kill in the middle of one of its waits.
#[test]
fn a_timeout_one_below_its_bound_is_refused_for_each_step() {
    let lines = [
        ("prepare", "prepare  = 90"),
        ("cutover", "cutover  = 300"),
        ("verify", "verify   = 300"),
        ("rollback", "rollback = 420"),
        ("cleanup", "cleanup  = 60"),
    ];
    for ((step, min), (name, line)) in MIN_TIMEOUTS.by_step().into_iter().zip(lines) {
        assert_eq!(step, name);
        let key = line.split('=').next().unwrap_or_default();
        let below = min - 1;
        assert_eq!(
            refused(&shipped_with(line, &format!("{key}= {below}"))),
            format!(
                "the hook refuses it: the declared {step} timeout {below} s is shorter than the hook's {min} s"
            ),
            "{step}"
        );
    }
}

/// A timeout above its bound still loads in the hook's reader (the bound is a floor), but the
/// checker holds the file to the bound exactly. Catches a bound that moved without the file.
#[test]
fn a_timeout_above_its_bound_loads_in_the_hook_but_not_in_the_checker() {
    let longer = shipped_with("cleanup  = 60", "cleanup  = 61");
    HookDeclaration::parse(&longer).expect("the hook accepts a longer timeout");
    assert_eq!(
        refused(&longer),
        "the updater's timeouts are not the hook's bounds"
    );
}

/// A verify deadline below `verify` plus `rollback` is refused by the updater, and a longer one
/// is accepted. Catches a rollback ordered while a verify is still allowed to run.
#[test]
fn a_verify_deadline_below_verify_plus_rollback_is_refused_by_the_updater() {
    assert_eq!(
        refused(&shipped_with(
            "verify_deadline_secs = 720",
            "verify_deadline_secs = 719"
        )),
        "the updater refuses it: verify_deadline_secs 719 is less than the verify and rollback bounds together (720)"
    );
    check(&shipped_with(
        "verify_deadline_secs = 720",
        "verify_deadline_secs = 721",
    ))
    .unwrap_or_else(|e| panic!("{e}"));
}

/// The text values are flags because the updater refuses a string in the params of a hook that
/// runs as root; a release link that is missing, relative or in a directory that is not a write
/// path, an unknown flag and a verb other than `update-hook` are refused. Catches a declaration
/// the hook cannot run cutover from.
#[test]
fn the_text_values_are_flags_and_a_bad_hook_args_is_refused() {
    let with_param = format!(
        "{}release_link = \"/opt/js-lease-server/link/current\"\n",
        shipped()
    );
    assert_eq!(
        refused(&with_param),
        "the updater refuses it: hook.params: a hook that runs as root takes numbers and booleans, not strings"
    );
    assert_eq!(
        refused(&shipped_with(
            r#""--release-link=/opt/js-lease-server/link/current", "#,
            ""
        )),
        "no release link: cutover and rollback refuse to run"
    );
    assert_eq!(
        refused(&shipped_with(
            "--release-link=/opt/js-lease-server/link/current",
            "--release-link=opt/js-lease-server/link/current"
        )),
        "the hook refuses hook_args: --release-link must be an absolute path"
    );
    assert_eq!(
        refused(&shipped_with(
            "--release-link=/opt/js-lease-server/link/current",
            "--release-link=/opt/js-lease-server/other/current"
        )),
        "the release link's directory /opt/js-lease-server/other is not a write path"
    );
    assert_eq!(
        refused(&shipped_with(
            "--store=/etc/js-lease-server/store",
            "--pool=default"
        )),
        "the hook refuses hook_args: unknown flag \"--pool=default\" before the step"
    );
    assert_eq!(
        refused(&shipped_with(r#"["update-hook", "#, r#"["update", "#)),
        "hook_args starts with \"update\", not update-hook"
    );
}

/// A `--healthz-url` that is not plain http to a loopback address and port is refused by the
/// hook's reading of `hook_args`, naming the value; another loopback address is accepted, and the
/// IPv6 one is refused by the updater's own limit on the characters of a word. Catches a
/// probe that would leave the host.
#[test]
fn a_healthz_url_that_is_not_loopback_is_refused() {
    for url in [
        "http://192.0.2.5:7300",
        "http://example.com:7300",
        "https://127.0.0.1:7300",
        "http://127.0.0.1",
        "http://127.0.0.1:0",
    ] {
        assert_eq!(
            refused(&shipped_with("http://127.0.0.1:7300", url)),
            format!(
                "the hook refuses hook_args: --healthz-url {url:?} must be http:// and a loopback address and port, such as http://127.0.0.1:7878"
            ),
            "{url}"
        );
    }
    // Another loopback port is fine; the IPv6 loopback cannot be written in `hook_args`, whose
    // words the updater limits to letters, digits and `._=:/@+-`.
    check(&shipped_with(
        "http://127.0.0.1:7300",
        "http://127.0.0.2:8080",
    ))
    .unwrap_or_else(|e| panic!("{e}"));
    assert_eq!(
        refused(&shipped_with("http://127.0.0.1:7300", "http://[::1]:7300")),
        "the updater refuses it: hook_args: \"--healthz-url=http://[::1]:7300\" is not 1 to 64 letters, digits and `._=:/@+-`"
    );
}

/// The unit and the binary are the server's own: a `--unit` that names another unit, a binary
/// that is a shell and a store given relative are refused, and a `hook_args` with no verb is
/// read as flags and refused. Catches a hook that restarts a unit the declaration does not allow.
#[test]
fn the_unit_and_the_binary_are_the_servers_own() {
    assert_eq!(
        refused(&shipped_with(
            "--unit=js-lease-server.service",
            "--unit=other.service"
        )),
        "the unit \"js-lease-server.service\" is not the one --unit names (\"other.service\")"
    );
    assert_eq!(
        refused(&shipped_with(
            "binary = \"js-lease-server\"",
            "binary = \"bin/sh\""
        )),
        "name \"js-lease-server\" and binary \"bin/sh\" are not js-lease-server"
    );
    assert_eq!(
        refused(&shipped_with(
            "--store=/etc/js-lease-server/store",
            "--store=store"
        )),
        "the hook refuses hook_args: --store must be an absolute path"
    );
    assert_eq!(
        refused(&shipped_with("\"update-hook\", ", "")),
        "hook_args starts with \"--release-link=/opt/js-lease-server/link/current\", not update-hook"
    );
}

/// Asserts that the store file and the release link must each lie under a declared read path:
/// a declaration that drops `/etc/js-lease-server` from `read_paths` is refused, by the store
/// file, and one that drops `/opt/js-lease-server` by the release link. Catches a declaration
/// whose hook could not read the store file it is told to probe.
#[test]
fn the_store_file_and_the_release_link_must_be_readable() {
    let without_etc = shipped_with(
        r#"read_paths   = ["/opt/js-lease-server", "/etc/js-lease-server"]"#,
        r#"read_paths   = ["/opt/js-lease-server"]"#,
    );
    assert_eq!(
        refused(&without_etc),
        "the store file /etc/js-lease-server/store is under no read path"
    );
    let without_opt = shipped_with(
        r#"read_paths   = ["/opt/js-lease-server", "/etc/js-lease-server"]"#,
        r#"read_paths   = ["/etc/js-lease-server"]"#,
    );
    assert_eq!(
        refused(&without_opt),
        "the release link /opt/js-lease-server/link/current is under no read path"
    );
}
