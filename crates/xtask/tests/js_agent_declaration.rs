//! The host updater's declaration of js-agent (`packaging/js-agent/updater.d/js-agent.toml`),
//! read as the repository has it, and what ships it: the updater's own validator accepts it
//! (kbf's `update-rights`, at the revision the root manifest pins), the hook reads it and the
//! flags in its `hook_args`, its timeouts keep the hook's bounds, its write paths are exactly
//! the release directory, the release link's directory and the hook record, the crun unit runs
//! the agent through the release link it names, and both the deb and the Linux js-agent release
//! archive carry it. Nothing here runs the updater or the hook: these are checks of the files.
#![cfg(target_os = "linux")]

use std::ffi::OsString;
use std::path::{Path, PathBuf};

use js_agent::update_hook::{
    CUTOVER_MARGIN_SECS, Declaration as HookDeclaration, Invalid, Invocation, POLL_SECS,
    PREPARE_MARGIN_SECS, Params, ROLLBACK_HELLO_SECS, ROLLBACK_MARGIN_SECS, Record,
    START_WAIT_SECS, UpdateInfo, VERIFY_MARGIN_SECS,
};
use update_rights::{Declaration, Refusal, reserved};
use xtask::pkg::{self, FILES, Source};
use xtask::release::stage::{self, DECLARATION_MEMBER, DECLARATION_SOURCE, Member};

const UNIT: &str = "packaging/js-agent/js-agent-crun.service";
const TMPFILES: &str = "packaging/js-agent/tmpfiles-crun.conf";
/// Where the updater stages the releases of js-agent, one directory each.
const RELEASES: &str = "/opt/js-agent/releases";
/// The keys the updater adds to the params file of an update.
const UPDATER_KEYS: &str = concat!(
    "\"updater.artifact_digest\" = \"sha256:0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef\"\n",
    "\"updater.staged_release\" = \"/opt/js-agent/releases/1.4.0\"\n",
    "\"updater.previous_release\" = \"/opt/js-agent/releases/1.3.2\"\n",
    "\"updater.target_version\" = \"1.4.0\"\n",
    "\"updater.previous_version\" = \"1.3.2\"\n",
);

fn read(path: &str) -> String {
    let root = Path::new(concat!(env!("CARGO_MANIFEST_DIR"), "/../.."));
    std::fs::read_to_string(root.join(path)).unwrap_or_else(|e| panic!("{path}: {e}"))
}

fn shipped() -> String {
    read(DECLARATION_SOURCE)
}

/// What the hook makes of a declaration: the updater's view, the hook's own, and the params
/// of an update with the flags of `hook_args` put in.
struct Read {
    updater: Declaration,
    hook: HookDeclaration,
    params: Params,
}

/// Every rule a declaration of js-agent must keep, the reason for the first it breaks.
///
/// The updater's rules are its own code: `Declaration::parse` (the rules of the text) and, for a
/// hook that runs as root, the loader's rule that each write path is in a directory named for
/// the service (`reserved::check_service_write`). The loader's other rules read the host (the
/// file's owner, the accounts, what a path resolves to) and are the host's, not the file's.
fn check(text: &str) -> Result<Read, String> {
    let updater = Declaration::parse(text).map_err(|e| format!("the updater refuses it: {e}"))?;
    // The loader refuses a declaration whose name is not its file's.
    if updater.name != "js-agent" || updater.binary != "js-agent" {
        return Err(format!(
            "name {:?} and binary {:?} are not js-agent",
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
    // The updater runs `<binary> <hook_args> <step> ...`: the hook reads the words after the
    // verb as its own command line.
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
        .and_then(|params| params.with_flags(&invocation.flags))
        .map_err(|e| format!("the hook refuses the params: {e}"))?;
    let link = params
        .release_link
        .clone()
        .ok_or("no release link: cutover and rollback refuse to run")?;
    // The timeouts against the hook's own bounds, on the record of an update that begins.
    let record = Record::new(UpdateInfo {
        target: "1.4.0".to_owned(),
        previous: Some("1.3.2".to_owned()),
        staged: PathBuf::from(format!("{RELEASES}/1.4.0")),
        previous_target: Some(PathBuf::from(format!("{RELEASES}/1.3.2"))),
        unit: hook.units.first().cloned().unwrap_or_default(),
        artifact_digest: params.artifact_digest.clone(),
    });
    js_agent::update_hook::validate(&record, &hook, &params)
        .map_err(|e| format!("the hook's bounds refuse it: {e}"))?;
    // Exactly the release directory, the release link's directory and the hook record.
    let link_dir = link.parent().map(Path::to_path_buf).unwrap_or_default();
    let mut want = vec![PathBuf::from(RELEASES), link_dir, hook.state_dir.clone()];
    want.sort();
    let mut found = hook.write_paths.clone();
    found.sort();
    if found != want {
        return Err(format!(
            "write paths {found:?} are not the release directory, the release link's directory and the state directory {want:?}"
        ));
    }
    Ok(Read {
        updater,
        hook,
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

const WRITE_PATHS: &str = r#"write_paths  = ["/opt/js-agent/releases", "/opt/js-agent/link", "/var/lib/js-agent-update"]"#;

/// The file passes the updater's validator and the hook's reading, and says what the hook needs:
/// root, the crun unit, a release link, the lease server and the state root as flags, and
/// numbers only in `[hook.params]`. Catches a declaration the updater would mark `unmanaged`.
#[test]
fn the_shipped_declaration_passes_the_updaters_validator_and_the_hooks_reading() {
    let read = check(&shipped()).unwrap_or_else(|why| panic!("{why}"));
    assert_eq!(read.updater.uid, "root");
    assert_eq!(read.updater.hook.user, "root");
    assert_eq!(read.updater.hook.group, "root");
    assert!(read.updater.hook.capabilities.is_empty());
    assert!(read.updater.hook.environment.is_empty());
    assert!(read.updater.health_ports.is_empty());
    assert_eq!(read.updater.signer.repository, "komira-ai/jobscheduler");
    assert_eq!(read.updater.signer.workflow, "release.yml");
    assert_eq!(read.updater.state_dir, "/var/lib/js-agent-update");
    assert_eq!(read.hook.units, ["js-agent-crun.service"]);
    assert_eq!(read.hook.runtime, "crun");
    assert!(
        read.updater
            .hook
            .params
            .values()
            .all(|value| value.is_integer())
    );
    assert_eq!(
        read.params.release_link,
        Some(PathBuf::from("/opt/js-agent/link/current"))
    );
    assert_eq!(
        read.params.connect.state_root,
        Some(PathBuf::from("/var/lib/js-agent/state"))
    );
    assert_eq!(
        read.params.connect.token_file,
        Some(PathBuf::from("/etc/js-agent/token"))
    );
    assert_eq!(
        read.params.connect.server.as_deref(),
        Some("SET-THE-SERVER")
    );
    assert_eq!(read.params.drain_wait_secs, 3600);
    // The hook's timeouts are the updater's.
    let (ours, theirs) = (read.hook.timeouts, &read.updater.hook.timeout_secs);
    assert_eq!(
        [
            ours.prepare,
            ours.cutover,
            ours.verify,
            ours.rollback,
            ours.cleanup
        ],
        [
            theirs.prepare,
            theirs.cutover,
            theirs.verify,
            theirs.rollback,
            theirs.cleanup
        ]
    );
}

/// A write path outside the release directory, the release link's directory and the hook record
/// is refused: by the updater when it is not js-agent's at all, by the layout rule when it is
/// another js-agent path (the agent's own state root, the parent of the releases). Catches a
/// declaration that hands the hook more of the disk than the update needs.
#[test]
fn a_write_path_outside_the_release_link_and_state_directories_is_refused() {
    let widen = |extra: &str| {
        shipped_with(
            WRITE_PATHS,
            &format!(
                r#"write_paths  = ["/opt/js-agent/releases", "/opt/js-agent/link", "/var/lib/js-agent-update", "{extra}"]"#
            ),
        )
    };
    let layout = |extra: &str| {
        let mut found = vec![
            extra.to_owned(),
            "/opt/js-agent/link".to_owned(),
            "/opt/js-agent/releases".to_owned(),
            "/var/lib/js-agent-update".to_owned(),
        ];
        found.sort();
        format!(
            "write paths {found:?} are not the release directory, the release link's directory and the state directory [\"/opt/js-agent/link\", \"/opt/js-agent/releases\", \"/var/lib/js-agent-update\"]"
        )
    };
    for extra in ["/var/lib/js-agent", "/opt/js-agent", "/var/cache/js-agent"] {
        assert_eq!(refused(&widen(extra)), layout(extra), "{extra}");
    }
    assert_eq!(
        refused(&widen("/var/lib/other")),
        "the updater refuses it: hook.write_paths: path \"/var/lib/other\" is not at or below a directory named for the service, which a hook that runs as root needs"
    );
    assert_eq!(
        refused(&widen("/etc/js-agent")),
        "the updater refuses it: hook.write_paths: path \"/etc/js-agent\" is inside the reserved directory /etc, where nothing may be written"
    );
    // The release directory left out is refused too: the layout is exact.
    assert_eq!(
        refused(&shipped_with(
            WRITE_PATHS,
            r#"write_paths  = ["/opt/js-agent/link", "/var/lib/js-agent-update"]"#
        )),
        "write paths [\"/opt/js-agent/link\", \"/var/lib/js-agent-update\"] are not the release directory, the release link's directory and the state directory [\"/opt/js-agent/link\", \"/opt/js-agent/releases\", \"/var/lib/js-agent-update\"]"
    );
}

/// The reason the text values are flags: the updater refuses a string in the params of a hook
/// that runs as root, and the hook refuses a value given both ways. A release link given as a
/// path that is not one of the hook's flags, or not at all, is refused.
#[test]
fn the_text_values_are_flags_because_a_root_hook_takes_no_string_param() {
    let with_param = format!(
        "{}release_link = \"/opt/js-agent/link/current\"\n",
        shipped()
    );
    assert_eq!(
        refused(&with_param),
        "the updater refuses it: hook.params: a hook that runs as root takes numbers and booleans, not strings"
    );
    assert_eq!(
        refused(&shipped_with(
            r#""--release-link=/opt/js-agent/link/current", "#,
            ""
        )),
        "no release link: cutover and rollback refuse to run"
    );
    assert_eq!(
        refused(&shipped_with(
            "--release-link=/opt/js-agent/link/current",
            "--release-link=opt/js-agent/link/current"
        )),
        "the hook refuses hook_args: --release-link must be an absolute path"
    );
    assert_eq!(
        refused(&shipped_with("--server=SET-THE-SERVER", "--pool=default")),
        "the hook refuses hook_args: unknown flag \"--pool=default\" before the step"
    );
    assert_eq!(
        refused(&shipped_with(r#"["update-hook", "#, r#"["update", "#)),
        "hook_args starts with \"update\", not update-hook"
    );
}

/// The timeouts keep the hook's bounds and the updater's: a `prepare` shorter than the drain
/// wait and its margin, and a verify deadline below `verify` plus `rollback`, are refused, each
/// by the one that holds the rule.
#[test]
fn timeouts_below_the_hooks_or_the_updaters_bounds_are_refused() {
    assert_eq!(
        refused(&shipped_with("prepare  = 3900", "prepare  = 3600")),
        "the hook's bounds refuse it: the declared prepare timeout 3600 s is shorter than drain_wait_secs 3600 s plus 60 s"
    );
    assert_eq!(
        refused(&shipped_with(
            "verify_deadline_secs = 9000",
            "verify_deadline_secs = 8834"
        )),
        "the updater refuses it: verify_deadline_secs 8834 is less than the verify and rollback bounds together (8835)"
    );
}

/// Each timeout at the edge of its rule: the smallest value the hook's bound (or the updater's,
/// for the deadline) accepts, and the one below it refused, each by the rule that holds it.
/// With `drain_wait_secs` 3600: `prepare` >= 3600 + 60, `cutover` > 120 + 60, `verify` > 15 +
/// 10, `rollback` > 3600 + 120 + 300 + 60, and `verify_deadline_secs` >= `verify` + `rollback`.
#[test]
fn each_timeout_is_accepted_at_its_bound_and_refused_just_below_it() {
    let wait = 3600;
    let hook = |e: Invalid| format!("the hook's bounds refuse it: {e}");
    let cases: [(&str, u64, u64, String); 4] = [
        (
            "prepare  = 3900",
            wait + PREPARE_MARGIN_SECS,
            wait + PREPARE_MARGIN_SECS - 1,
            hook(Invalid::PrepareTimeout {
                prepare: wait + PREPARE_MARGIN_SECS - 1,
                wait,
            }),
        ),
        (
            "cutover  = 4320",
            START_WAIT_SECS + CUTOVER_MARGIN_SECS + 1,
            START_WAIT_SECS + CUTOVER_MARGIN_SECS,
            hook(Invalid::CutoverTimeout {
                cutover: START_WAIT_SECS + CUTOVER_MARGIN_SECS,
            }),
        ),
        (
            "verify   = 615",
            VERIFY_MARGIN_SECS + POLL_SECS + 1,
            VERIFY_MARGIN_SECS + POLL_SECS,
            hook(Invalid::VerifyTimeout {
                verify: VERIFY_MARGIN_SECS + POLL_SECS,
            }),
        ),
        (
            "rollback = 8220",
            wait + START_WAIT_SECS + ROLLBACK_HELLO_SECS + ROLLBACK_MARGIN_SECS + 1,
            wait + START_WAIT_SECS + ROLLBACK_HELLO_SECS + ROLLBACK_MARGIN_SECS,
            hook(Invalid::RollbackTimeout {
                rollback: wait + START_WAIT_SECS + ROLLBACK_HELLO_SECS + ROLLBACK_MARGIN_SECS,
                wait,
            }),
        ),
    ];
    for (line, accepted, below, why) in cases {
        let key = line.split('=').next().unwrap_or_default();
        let at = |secs: u64| shipped_with(line, &format!("{key}= {secs}"));
        check(&at(accepted)).unwrap_or_else(|e| panic!("{line} at {accepted}: {e}"));
        assert_eq!(refused(&at(below)), why, "{line}");
    }
    // The deadline: equal to verify + rollback is enough.
    check(&shipped_with(
        "verify_deadline_secs = 9000",
        "verify_deadline_secs = 8835",
    ))
    .unwrap_or_else(|e| panic!("{e}"));
}

/// The hook is the agent's own binary, run with no shell: a declaration whose binary is a shell,
/// or whose name is not its file's, is refused.
#[test]
fn the_hook_is_the_agents_binary_and_never_a_shell() {
    assert_eq!(
        refused(&shipped_with(
            "binary = \"js-agent\"",
            "binary = \"bin/sh\""
        )),
        "name \"js-agent\" and binary \"bin/sh\" are not js-agent"
    );
    let renamed = shipped_with("name   = \"js-agent\"", "name   = \"js-agent2\"")
        .replace("\"js-agent-crun.service\"", "\"js-agent2-crun.service\"");
    assert_eq!(
        refused(&renamed),
        "name \"js-agent2\" and binary \"js-agent\" are not js-agent"
    );
}

/// The settings of the crun unit's `[Service]` lines: key and value.
fn unit_value(key: &str) -> String {
    let text = read(UNIT);
    let found: Vec<&str> = text
        .lines()
        .filter_map(|line| line.strip_prefix(&format!("{key}=")))
        .collect();
    assert_eq!(found.len(), 1, "{key}: {found:?}");
    found[0].to_owned()
}

/// The lines of the crun tmpfiles file that are not comments.
fn tmpfiles_lines() -> Vec<Vec<String>> {
    read(TMPFILES)
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty() && !line.starts_with('#'))
        .map(|line| line.split_whitespace().map(str::to_owned).collect())
        .collect()
}

/// The unit the declaration names runs the agent through the release link the declaration's
/// flag names, so `cutover` moving the link is what changes the program the next start runs;
/// the tmpfiles file the unit reads before every start makes the link (once, never replacing
/// it) and the directories of the releases and of the link, root's and closed to others.
#[test]
fn the_units_exec_start_resolves_through_the_release_link() {
    let read = check(&shipped()).unwrap_or_else(|why| panic!("{why}"));
    let link = read.params.release_link.unwrap();
    assert_eq!(
        Path::new(UNIT).file_name().unwrap().to_str(),
        read.hook.units.first().map(String::as_str)
    );
    let exec = unit_value("ExecStart");
    let program = exec.split_whitespace().next().unwrap();
    assert_eq!(Path::new(program), link.join("js-agent"));
    let lines = tmpfiles_lines();
    let link_line = lines
        .iter()
        .find(|f| f.get(1).map(String::as_str) == link.to_str())
        .expect("a tmpfiles line for the link");
    // `L`, not `L+`: an existing link is the hook's and stays.
    assert_eq!(
        link_line.iter().map(String::as_str).collect::<Vec<_>>(),
        [
            "L",
            "/opt/js-agent/link/current",
            "-",
            "-",
            "-",
            "-",
            "/usr/bin"
        ]
    );
    for dir in ["/opt/js-agent", RELEASES, "/opt/js-agent/link"] {
        assert!(
            lines.iter().any(|f| f
                .iter()
                .map(String::as_str)
                .eq(["d", dir, "0755", "root", "root", "-"])),
            "{dir}"
        );
    }
    // The first target holds the program the unit runs: the package's agent.
    assert!(
        FILES.iter().any(
            |(path, _, source)| *path == "usr/bin/js-agent" && matches!(source, Source::Binary)
        )
    );
}

/// What the deb and the Linux js-agent archive carry of the declaration: the reasons for each
/// one that lacks it, or that carries another file.
fn carried(deb: &[(&str, u32, Source)], archive: &[Member]) -> Vec<String> {
    let mut missing = Vec::new();
    let in_deb = deb.iter().any(|(path, mode, source)| {
        matches!(source, Source::Declaration)
            && *path == "usr/share/js-agent/updater.d/js-agent.toml"
            && *mode == 0o644
    });
    if !in_deb {
        missing.push("the deb does not carry the declaration".to_owned());
    }
    let in_archive = archive.iter().any(|member| {
        member.source == DECLARATION_SOURCE && member.path == "updater.d/js-agent.toml"
    });
    if !in_archive {
        missing.push("the js-agent release archive does not carry the declaration".to_owned());
    }
    missing
}

/// The deb and the Linux js-agent release archive both carry the declaration, from the same file
/// of the repository, the archive at the name kbf's release unpacker takes it by
/// (`updater.d/<binary>.toml`); the Mac archive and the other binaries' archives do not. The
/// check of this fails when either list omits it.
#[test]
fn the_deb_and_the_linux_agent_archive_carry_the_declaration() {
    let linux = stage::members("js-agent", "x86_64-unknown-linux-musl");
    assert_eq!(carried(&FILES, &linux), Vec::<String>::new());
    assert_eq!(
        format!("packaging/js-agent/{}", pkg::DECLARATION),
        DECLARATION_SOURCE
    );
    assert_eq!(DECLARATION_MEMBER, "updater.d/js-agent.toml");
    // Either list without it is refused, by name.
    let deb_without: Vec<(&str, u32, Source)> = FILES
        .iter()
        .copied()
        .filter(|(_, _, source)| !matches!(source, Source::Declaration))
        .collect();
    assert_eq!(
        carried(&deb_without, &linux),
        ["the deb does not carry the declaration"]
    );
    let mac = stage::members("js-agent", "aarch64-apple-darwin");
    assert_eq!(
        carried(&FILES, &mac),
        ["the js-agent release archive does not carry the declaration"]
    );
    assert_eq!(carried(&deb_without, &mac).len(), 2);
    for (bin, target) in [
        ("js-agent", "aarch64-apple-darwin"),
        ("js", "x86_64-unknown-linux-musl"),
        ("js-lease-server", "x86_64-unknown-linux-musl"),
    ] {
        assert!(
            !stage::members(bin, target)
                .iter()
                .any(|member| member.source == DECLARATION_SOURCE),
            "{bin} {target}"
        );
    }
}
