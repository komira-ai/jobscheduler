//! The files of the js-agent .deb (`packaging/js-agent/deb`), read as they are in the repository:
//! the units equal the farm units of the setup script except where the package differs (the
//! program at `/usr/bin/js-agent`, the server and pool in the operator's flags file, the
//! package's own paths), the agent's command line parser accepts the unit's flags, the
//! package holds no secret and no environment, and the unit and the hook's params agree on the
//! unit's name and the stop bound.
#![cfg(target_os = "linux")]

use std::path::Path;

use clap::Parser;
use js_agent::{Cli, RuntimeKind};

const DEB: &str = "packaging/js-agent/deb";
const UNIT: &str = "packaging/js-agent/deb/js-agent-crun.service";
const GPU_UNIT: &str = "packaging/js-agent/deb/js-agent-crun-gpu.service";
const TMPFILES: &str = "packaging/js-agent/deb/tmpfiles.conf";
const FARM_UNIT: &str = "packaging/js-agent/js-agent-crun.service";
const FARM_GPU_UNIT: &str = "packaging/js-agent/js-agent-crun-gpu.service";
const FARM_TMPFILES: &str = "packaging/js-agent/tmpfiles-crun.conf";

/// The program the package installs.
const PROGRAM: &str = "/usr/bin/js-agent";
/// The farm unit's program, through the setup script's release link.
const FARM_PROGRAM: &str = "/opt/js-agent/link/current/js-agent";
/// The operator's flags file and token file.
const FLAGS: &str = "/etc/js-agent/flags";
const TOKEN: &str = "/etc/js-agent/token";
/// Where the package installs the units and the tmpfiles file.
const INSTALLED_UNIT: &str = "/usr/lib/systemd/system/js-agent-crun.service";
const INSTALLED_TMPFILES: &str = "/usr/lib/tmpfiles.d/js-agent-crun.conf";
/// The cutover bound of the hook manifest (`cargo xtask release desired`, `timeoutSecs.cutover`).
const CUTOVER_SECS: u64 = 4320;
/// The flags file of a host, as an operator writes it.
const SAMPLE_FLAGS: &str = "# this host's own flags\n--server=https://js.example.com:8443\n--pool=farm\n--instance-file=/etc/js-agent/instance\n--host-id=node-5\n";

fn read(path: &str) -> String {
    let root = Path::new(concat!(env!("CARGO_MANIFEST_DIR"), "/../.."));
    std::fs::read_to_string(root.join(path)).unwrap_or_else(|e| panic!("{path}: {e}"))
}

/// The lines of `text` that are not comments or blank, trimmed.
fn code(text: &str) -> Vec<String> {
    text.lines()
        .map(str::trim)
        .filter(|l| !l.is_empty() && !l.starts_with('#'))
        .map(str::to_owned)
        .collect()
}

/// The `(section, key, value)` of every assignment of a unit file, a backslash joining a line to
/// the next.
fn assignments(unit: &str) -> Vec<(String, String, String)> {
    let mut joined = Vec::new();
    let mut open = String::new();
    for line in code(unit) {
        match line.strip_suffix('\\') {
            Some(head) => {
                open.push_str(head);
                open.push(' ');
            }
            None => {
                open.push_str(&line);
                joined.push(std::mem::take(&mut open));
            }
        }
    }
    let mut section = String::new();
    let mut out = Vec::new();
    for line in joined {
        if line.starts_with('[') {
            section = line;
        } else if let Some((key, value)) = line.split_once('=') {
            out.push((section.clone(), key.to_owned(), value.to_owned()));
        }
    }
    out
}

/// The one value of `key` in `section` of `unit`.
fn unit_value(unit: &str, section: &str, key: &str) -> String {
    let found: Vec<String> = assignments(unit)
        .into_iter()
        .filter(|(s, k, _)| s == section && k == key)
        .map(|(_, _, value)| value)
        .collect();
    assert_eq!(found.len(), 1, "{section} {key}= in {unit}");
    found[0].clone()
}

/// The assignments of `unit` other than `ExecStart` and `ExecStartPre`.
fn rest(unit: &str) -> Vec<(String, String, String)> {
    assignments(unit)
        .into_iter()
        .filter(|(_, key, _)| key != "ExecStart" && key != "ExecStartPre")
        .collect()
}

/// A command line with each `--flags-file <path>` replaced by the words of `flags_text`.
fn expand(words: &[&str], flags_text: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut rest = words.iter();
    while let Some(word) = rest.next() {
        if *word == js_agent::flags_file::FLAG {
            let _path = rest.next().expect("a path after --flags-file");
            out.extend(js_agent::flags_file::words(flags_text).expect("the flags file's words"));
        } else {
            out.push((*word).to_owned());
        }
    }
    out
}

/// The package directory holds the four files the .deb is made of, and the README that describes
/// them, and no other: a file added to it is a file the deb builder does not carry.
///
/// Catches a packaging file that is added or renamed without the builder (and this test) knowing.
#[test]
fn the_deb_directory_holds_the_packaged_files_and_the_readme() {
    let root = Path::new(concat!(env!("CARGO_MANIFEST_DIR"), "/../.."));
    let mut names: Vec<String> = std::fs::read_dir(root.join(DEB))
        .unwrap()
        .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    names.sort();
    assert_eq!(
        names,
        [
            "README.md",
            "js-agent-crun-gpu.service",
            "js-agent-crun.service",
            "tmpfiles.conf"
        ]
    );
}

/// The agent's own parser reads the unit's command line once the flags file is expanded, and the
/// unit alone carries neither the server nor the pool.
///
/// Catches a unit with a flag the agent does not take, a server or pool written into the package
/// (a public package cannot know them), and a program that is not the package's.
#[test]
fn the_unit_expands_with_a_flags_file_to_a_command_line_the_agent_parses() {
    let unit = read(UNIT);
    let exec = unit_value(&unit, "[Service]", "ExecStart");
    let words: Vec<&str> = exec.split_whitespace().collect();
    assert_eq!(words[0], PROGRAM);
    assert_eq!(words[1..3], ["--flags-file", FLAGS]);
    let unexpanded = Cli::try_parse_from(&words).unwrap_err();
    assert_eq!(
        unexpanded.kind(),
        clap::error::ErrorKind::MissingRequiredArgument,
        "the unit alone has no server"
    );
    let cli = Cli::try_parse_from(expand(&words, SAMPLE_FLAGS)).expect("a command line");
    assert_eq!(cli.server, "https://js.example.com:8443");
    assert_eq!(cli.pool, "farm");
    assert_eq!(cli.flags_file, None);
    assert_eq!(cli.token_file.as_deref(), Some(Path::new(TOKEN)));
    assert_eq!(
        cli.state_root.as_deref(),
        Some(Path::new("/var/lib/js-agent/state"))
    );
    assert_eq!(
        cli.scratch_root.as_deref(),
        Some(Path::new("/var/lib/js-agent/scratch/runs"))
    );
    assert_eq!(cli.runtime, Some(RuntimeKind::Crun));
    assert_eq!(cli.crun_bin.as_deref(), Some(Path::new("/usr/bin/crun")));
    assert_eq!(cli.job_user.as_deref(), Some("jsjob"));
    assert_eq!(cli.drain_wait, Some(3600));
    assert_eq!(cli.stop_grace, Some(120));
    assert_eq!(
        cli.instance_file.as_deref(),
        Some(Path::new("/etc/js-agent/instance"))
    );
    assert_eq!(cli.host_id.as_deref(), Some("node-5"));
    assert!(js_agent::crun_host::flags(&cli).is_ok());
}

/// No file of the package sets the environment or holds token text: outside comments, the
/// token's path, the flag that names it and nothing else mention a token, so a package never
/// carries one.
///
/// Catches an `Environment=` or `EnvironmentFile=` in a unit (configuration is flags) and a
/// token written into the package.
#[test]
fn no_file_of_the_package_sets_the_environment_or_holds_token_text() {
    for path in [UNIT, GPU_UNIT, TMPFILES] {
        let body = read(path);
        for line in body.lines().map(str::trim) {
            assert!(
                !line.starts_with("Environment=") && !line.starts_with("EnvironmentFile="),
                "{path}: {line}"
            );
        }
        let left = code(&body)
            .join("\n")
            .replace(TOKEN, "")
            .replace("--token-file", "")
            .to_lowercase();
        assert!(!left.contains("token"), "{path} mentions a token");
    }
}

/// The unit is the farm unit of the setup script with the program at `/usr/bin/js-agent`, the
/// server and pool read from the flags file and the tmpfiles file at the package's path.
///
/// Catches a setting that drifted between the two (a Delegate, a KillMode or a stop bound that
/// one has and the other lacks), which the farm unit's own tests pin for the farm unit only.
#[test]
fn the_unit_equals_the_farm_unit_except_the_program_the_flags_file_and_the_tmpfiles_path() {
    let (packaged, farm) = (read(UNIT), read(FARM_UNIT));
    assert_eq!(rest(&packaged), rest(&farm));
    let words = |text: &str| -> Vec<String> {
        unit_value(text, "[Service]", "ExecStart")
            .split_whitespace()
            .map(str::to_owned)
            .collect()
    };
    let (packaged_words, farm_words) = (words(&packaged), words(&farm));
    assert_eq!(packaged_words[0], PROGRAM);
    assert_eq!(farm_words[0], FARM_PROGRAM);
    let strip = |words: &[String], drop: &[&[&str]]| -> Vec<String> {
        let mut out = words[1..].to_vec();
        for pair in drop {
            let at = out
                .windows(pair.len())
                .position(|w| w.iter().map(String::as_str).eq(pair.iter().copied()))
                .unwrap_or_else(|| panic!("{pair:?} in {words:?}"));
            out.drain(at..at + pair.len());
        }
        out
    };
    assert_eq!(
        strip(&packaged_words, &[&["--flags-file", FLAGS]]),
        strip(
            &farm_words,
            &[
                &["--server", "SET-THE-SERVER-WITH-SYSTEMCTL-EDIT"],
                &["--pool", "default"]
            ]
        )
    );
    let pre = |text: &str| unit_value(text, "[Service]", "ExecStartPre");
    assert_eq!(
        pre(&packaged),
        pre(&farm).replace("/etc/tmpfiles.d/", "/usr/lib/tmpfiles.d/")
    );
    assert!(pre(&packaged).ends_with(INSTALLED_TMPFILES));
}

/// The GPU oneshot and the tmpfiles file are the farm's, with the program at `/usr/bin/js-agent`,
/// the unit it extends at the package's path and the operator's drop-in directory named, and
/// without the farm's release layout of `/opt/js-agent`.
///
/// Catches a oneshot that extends a unit file the package does not install, and a tmpfiles file
/// that keeps the setup script's release directories.
#[test]
fn the_gpu_unit_and_the_tmpfiles_file_equal_the_farms_except_the_package_paths() {
    let (packaged, farm) = (read(GPU_UNIT), read(FARM_GPU_UNIT));
    assert_eq!(rest(&packaged), rest(&farm));
    let exec = |unit: &str| unit_value(unit, "[Service]", "ExecStart");
    let unit_flag = format!(
        "--unit {INSTALLED_UNIT} --dropin-dir /etc/systemd/system/js-agent-crun.service.d "
    );
    assert_eq!(
        exec(&packaged)
            .replace(PROGRAM, "PROGRAM")
            .replace(&unit_flag, ""),
        exec(&farm)
            .replace(FARM_PROGRAM, "PROGRAM")
            .replace("--unit /etc/systemd/system/js-agent-crun.service ", "")
    );
    assert!(exec(&packaged).contains(&unit_flag), "{}", exec(&packaged));
    let layout_free: Vec<String> = code(&read(FARM_TMPFILES))
        .into_iter()
        .filter(|line| !line.contains("/opt/js-agent"))
        .collect();
    assert_eq!(code(&read(TMPFILES)), layout_free);
}

/// The units are enabled by what hostkeeper starts (`multi-user.target`), the stop bound covers
/// the drain, and the cutover bound of the hook is longer than the stop.
///
/// Catches a run unit hostkeeper cannot enable (no `WantedBy=`), a stop bound shorter than the
/// drain (systemd would kill a drain in progress), and a cutover bound no longer than the stop
/// (hostkeeper would give up on a stop that is still draining).
#[test]
fn the_unit_is_enabled_by_multi_user_and_its_stop_covers_the_drain() {
    let unit = read(UNIT);
    assert_eq!(
        unit_value(&unit, "[Install]", "WantedBy"),
        "multi-user.target"
    );
    let exec = unit_value(&unit, "[Service]", "ExecStart");
    let words: Vec<&str> = exec.split_whitespace().collect();
    let flag = |name: &str| -> u64 {
        let at = words.iter().position(|w| *w == name).expect("a flag");
        words[at + 1].parse().expect("seconds")
    };
    let stop: u64 = unit_value(&unit, "[Service]", "TimeoutStopSec")
        .parse()
        .expect("seconds");
    assert!(stop > flag("--drain-wait") + flag("--stop-grace"));
    assert!(CUTOVER_SECS > stop);
}
