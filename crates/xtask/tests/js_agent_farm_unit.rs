//! The unit of a Linux farm node that runs jobs with crun (`packaging/js-agent/js-agent-crun.service`)
//! and its tmpfiles file, read as they are in the repository: the agent's own command line parser
//! accepts the unit's flags and the crun start check accepts their combination, the unit delegates
//! the cgroup, is stopped by the agent's drain, waits for no container engine, and the directories
//! it makes have the modes the agent's scratch rule and the job's ids need. Nothing here starts the
//! unit: these are static checks of the files. (Linux only: only Linux runs containers.)
#![cfg(target_os = "linux")]

use std::path::{Path, PathBuf};

use clap::Parser;
use js_agent::lifecycle::DirLayout;
use js_agent::{Cli, RuntimeKind};

const UNIT: &str = "packaging/js-agent/js-agent-crun.service";
const TMPFILES: &str = "packaging/js-agent/tmpfiles-crun.conf";
/// Where the setup script installs the tmpfiles file, which the unit's `ExecStartPre` reads.
const INSTALLED_TMPFILES: &str = "/etc/tmpfiles.d/js-agent-crun.conf";

fn read(path: &str) -> String {
    let root = Path::new(concat!(env!("CARGO_MANIFEST_DIR"), "/../.."));
    std::fs::read_to_string(root.join(path)).unwrap_or_else(|e| panic!("{path}: {e}"))
}

/// The lines that are not comments or blank, without their indentation.
fn code(text: &str) -> Vec<String> {
    text.lines()
        .map(str::trim)
        .filter(|l| !l.is_empty() && !l.starts_with('#'))
        .map(str::to_owned)
        .collect()
}

/// The settings of the unit: section, key and value.
fn settings() -> Vec<(String, String, String)> {
    let mut section = String::new();
    let mut found = Vec::new();
    for line in code(&read(UNIT)) {
        if let Some(name) = line.strip_prefix('[').and_then(|r| r.strip_suffix(']')) {
            section = name.to_owned();
        } else {
            let (key, value) = line.split_once('=').unwrap_or_else(|| panic!("{line}"));
            found.push((section.clone(), key.to_owned(), value.to_owned()));
        }
    }
    found
}

/// The value of the one `key=` line of the unit.
fn value(key: &str) -> String {
    let found: Vec<String> = settings()
        .into_iter()
        .filter(|(_, k, _)| k == key)
        .map(|(_, _, v)| v)
        .collect();
    assert_eq!(found.len(), 1, "{key}: {found:?}");
    found[0].clone()
}

/// The unit's `ExecStart` as the agent's own parser reads it.
fn agent_cli() -> Cli {
    let words: Vec<String> = value("ExecStart")
        .split_whitespace()
        .map(str::to_owned)
        .collect();
    Cli::try_parse_from(words)
        .unwrap_or_else(|e| panic!("ExecStart is not a command line of js-agent: {e}"))
}

/// The directories the tmpfiles file makes: path and mode.
fn directories() -> Vec<(PathBuf, u32)> {
    code(&read(TMPFILES))
        .iter()
        .map(|line| {
            let f: Vec<&str> = line.split_whitespace().collect();
            assert_eq!(f.len(), 6, "{line}");
            assert_eq!(
                (f[0], f[3], f[4], f[5]),
                ("d", "root", "root", "-"),
                "{line}"
            );
            (
                PathBuf::from(f[1]),
                u32::from_str_radix(f[2], 8).expect("a mode"),
            )
        })
        .collect()
}

/// The unit runs the agent with `--runtime crun`, the distribution's crun by its path, a job
/// user and the roots the packaged unit and the elastic payload use, and none of the engine's
/// flags; the combination passes the check the agent applies before it registers, so a unit that
/// lacks a flag fails here and not as a restart loop on a node.
#[test]
fn the_unit_runs_the_agent_with_the_crun_runtime_and_flags_the_agent_accepts() {
    let cli = agent_cli();
    assert_eq!(cli.runtime, Some(RuntimeKind::Crun));
    assert_eq!(cli.crun_bin.as_deref(), Some(Path::new("/usr/bin/crun")));
    assert_eq!(cli.job_user.as_deref(), Some("jsjob"));
    assert_eq!(
        cli.state_root.as_deref(),
        Some(Path::new(js_elastic::STATE_ROOT))
    );
    assert_eq!(
        cli.scratch_root.as_deref(),
        Some(Path::new(&format!("{}/runs", js_elastic::SCRATCH_ROOT)))
    );
    assert_eq!(
        cli.token_file.as_deref(),
        Some(Path::new("/etc/js-agent/token"))
    );
    assert!(js_agent::crun_host::flags(&cli).is_ok());
    assert_eq!(js_agent::crun_host::DISTRO_CRUN, "/usr/bin/crun");
}

/// The cgroup is delegated, the unit is in a slice of its own, the state directory is made `0711`,
/// a stop signals the agent alone and waits for its drain within a finite bound, and the agent is
/// restarted after every exit. Each is a setting a node needs and the engine unit does not set
/// (`KillMode` and a finite `TimeoutStopSec` are the engine unit's).
#[test]
fn the_unit_delegates_the_cgroup_and_leaves_the_stop_to_the_agent() {
    assert_eq!(value("Delegate"), "yes");
    // The agent runs in a leaf, so the unit's cgroup holds no process and can enable the
    // controllers for `<unit>/actions`; `supervisor` is the leaf name js-contain's delegate step
    // recognizes, which makes the unit's cgroup (not the leaf) its root.
    assert_eq!(value("DelegateSubgroup"), "supervisor");
    assert!(value("Slice").ends_with(".slice"));
    assert_eq!(value("StateDirectoryMode"), "0711");
    assert_eq!(value("KillMode"), "mixed");
    // The bound is finite and covers the drain the unit's own flags ask for (a hung agent is
    // killed and restarted), whatever those flags are set to.
    let cli = agent_cli();
    let grace = cli.stop_grace.expect("the unit sets --stop-grace");
    let wait = cli.drain_wait.expect("the unit sets --drain-wait");
    let drain = wait + grace + js_agent::DRAIN_SETTLE.as_secs();
    let timeout: u64 = value("TimeoutStopSec")
        .parse()
        .expect("TimeoutStopSec is a number of seconds, not infinity");
    assert!(
        timeout >= drain + 30,
        "TimeoutStopSec={timeout} for a drain of {drain} s"
    );
    assert!(
        timeout <= 2 * drain,
        "TimeoutStopSec={timeout} is no bound for {drain} s"
    );
    assert_eq!(value("Restart"), "always");
    assert_eq!(value("RestartSec"), "30");
    assert_eq!(value("Conflicts"), "js-agent.service");
}

/// No container engine is waited for or pulled in: the runtime is crun and the agent starts
/// without Docker or Podman.
#[test]
fn the_unit_names_no_container_engine() {
    let after = value("After");
    assert_eq!(after, "network-online.target");
    for (_, key, value) in settings() {
        if key == "ExecStart" {
            continue;
        }
        for engine in ["docker", "containerd", "podman"] {
            assert!(!value.contains(engine), "{key}={value}");
        }
    }
    for key in ["Requires", "Requisite", "BindsTo", "PartOf", "Upholds"] {
        assert!(!settings().iter().any(|(_, k, _)| k == key), "{key}");
    }
}

/// The agent must see the host and own its roots, so the unit runs as root with no sandboxing and
/// sets none of the keys that would change that or the flags-only configuration.
#[test]
fn the_unit_runs_as_root_without_sandboxing_or_other_configuration() {
    for (section, key, _) in settings() {
        for prefix in [
            "Protect",
            "Private",
            "Restrict",
            "Capability",
            "SystemCall",
            "NoNewPrivileges",
            "ReadOnly",
            "ReadWrite",
            "Inaccessible",
            "Mount",
            "Root",
            "User",
            "Group",
            "DynamicUser",
            "Environment",
            "PassEnvironment",
            "UnsetEnvironment",
            "LoadCredential",
            "SetCredential",
            "ImportCredential",
        ] {
            assert!(!key.starts_with(prefix), "[{section}] {key} in the unit");
        }
    }
}

/// Every setting is one of these: a new one is added here with its reason.
#[test]
fn the_unit_sets_only_the_settings_listed_here() {
    let mut found: Vec<(String, String)> = settings().into_iter().map(|(s, k, _)| (s, k)).collect();
    found.sort();
    let mut want: Vec<(String, String)> = [
        ("Unit", "After"),
        ("Unit", "Conflicts"),
        ("Unit", "ConditionPathIsMountPoint"),
        ("Unit", "Description"),
        ("Unit", "Documentation"),
        ("Unit", "RequiresMountsFor"),
        ("Unit", "Wants"),
        ("Service", "Delegate"),
        ("Service", "DelegateSubgroup"),
        ("Service", "ExecStart"),
        ("Service", "ExecStartPre"),
        ("Service", "KillMode"),
        ("Service", "Restart"),
        ("Service", "RestartSec"),
        ("Service", "Slice"),
        ("Service", "StateDirectory"),
        ("Service", "StateDirectoryMode"),
        ("Service", "TimeoutStopSec"),
        ("Service", "Type"),
        ("Install", "WantedBy"),
    ]
    .into_iter()
    .map(|(s, k)| (s.to_owned(), k.to_owned()))
    .collect();
    want.sort();
    assert_eq!(found, want);
}

/// The unit makes its directories from the file the setup script installs, before every start, and
/// waits for the scratch disk by its mount, `/var/lib/js-agent/scratch`, the parent of the
/// `--scratch-root` flag's path (`.../scratch/runs`).
#[test]
fn the_unit_makes_its_directories_and_waits_for_the_scratch_disk() {
    assert_eq!(
        value("ExecStartPre"),
        format!("+systemd-tmpfiles --create {INSTALLED_TMPFILES}")
    );
    assert_eq!(value("RequiresMountsFor"), js_elastic::SCRATCH_ROOT);
    assert_eq!(value("ConditionPathIsMountPoint"), js_elastic::SCRATCH_ROOT);
}

/// The modes of the directories are the ones the agent and the job's ids need: the scratch root is
/// what the agent's `Searchable` layout accepts (`0711`), the directory above it is the unit's
/// `StateDirectoryMode`, so the two files agree and a start does not change the other's work, and
/// the state root and the token directory stay private. No other directory is made.
#[test]
fn the_directories_have_the_modes_the_agent_and_the_jobs_ids_need() {
    let cli = agent_cli();
    let state = cli.state_root.unwrap();
    let scratch = cli.scratch_root.unwrap();
    // The scratch root is the runs directory inside the scratch mount, so the image store the agent
    // places beside it (`<mount>/oci`) is on the mount's disk.
    let mount = Path::new(js_elastic::SCRATCH_ROOT);
    assert_eq!(scratch.parent().unwrap(), mount);
    let parent = mount.parent().unwrap().to_owned();
    assert_eq!(state.parent().unwrap(), parent);
    let dirs = directories();
    let mode_of = |path: &Path| dirs.iter().find(|(p, _)| p == path).map(|(_, m)| *m);
    assert_eq!(mode_of(&scratch), Some(DirLayout::Searchable.dir_mode()));
    assert_eq!(mode_of(mount), Some(DirLayout::Searchable.dir_mode()));
    assert_eq!(mode_of(&parent), Some(0o711));
    assert_eq!(
        u32::from_str_radix(value("StateDirectoryMode").trim_start_matches('0'), 8).ok(),
        mode_of(&parent)
    );
    assert_eq!(mode_of(&state), Some(DirLayout::Private.dir_mode()));
    assert_eq!(mode_of(Path::new("/etc/js-agent")), Some(0o700));
    assert_eq!(dirs.len(), 5, "{dirs:?}");
    assert_eq!(
        value("StateDirectory"),
        parent.file_name().unwrap().to_str().unwrap()
    );
}

/// The held host setup script writes a node's server, pool and job user by replacing these three
/// placeholders of the unit, so each is in the unit once, exactly as the script spells it: a unit
/// that renamed one would be refused by the script, and an extra copy (in a comment, say) would be
/// rewritten with the rest.
#[test]
fn the_unit_holds_each_placeholder_the_setup_script_replaces_once() {
    let text = read(UNIT);
    for placeholder in [
        "--server SET-THE-SERVER-WITH-SYSTEMCTL-EDIT",
        "--pool default",
        "--job-user jsjob",
    ] {
        assert_eq!(text.matches(placeholder).count(), 1, "{placeholder}");
    }
}
