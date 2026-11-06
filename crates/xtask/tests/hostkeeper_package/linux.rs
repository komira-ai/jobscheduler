//! The two Linux packages, read with hostkeeper's own code: the unpacker, the manifest loader and
//! the placed-path lint, and with the agent's own command-line parser.

use std::collections::{BTreeMap, BTreeSet};
use std::num::NonZeroU64;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;

use clap::Parser;
use hostkeeper_config::manifest::{Hook, Manifest, ManifestRefusal, Timeouts};
use hostkeeper_placed::lint::{check_unit_text, through_current, wanted_by};
use js_agent::{Cli, RuntimeKind};

use crate::support::{
    CURRENT_BINARY, FLAGS, LINUX, PACKAGES, SAMPLE_FLAGS, Scratch, TOKEN, VERSION, assignments,
    expand, hk, hook_params, load_edited, manifest_toml, package, running, text, unit_value,
    unpack,
};

const UNIT: &str = "units/js-agent-crun.service";
const GPU_UNIT: &str = "units/js-agent-crun-gpu.service";
const FARM_UNIT: &str = "packaging/js-agent/js-agent-crun.service";
const FARM_GPU_UNIT: &str = "packaging/js-agent/js-agent-crun-gpu.service";
const FARM_TMPFILES: &str = "packaging/js-agent/tmpfiles-crun.conf";

fn bound(seconds: u64) -> Option<NonZeroU64> {
    Some(NonZeroU64::new(seconds).expect("not zero"))
}

/// The text of a file of the repository.
pub fn repo_file(path: &str) -> String {
    let root = Path::new(concat!(env!("CARGO_MANIFEST_DIR"), "/../.."));
    std::fs::read_to_string(root.join(path)).unwrap_or_else(|e| panic!("{path}: {e}"))
}

/// The lines of `text` that are not comments or blank, trimmed.
pub fn code(text: &str) -> Vec<String> {
    text.lines()
        .map(str::trim)
        .filter(|l| !l.is_empty() && !l.starts_with('#'))
        .map(str::to_owned)
        .collect()
}

/// The `[params]` of `manifest` as a map of its values.
pub fn params(manifest: &Manifest) -> BTreeMap<String, toml::Value> {
    manifest
        .params
        .iter()
        .map(|(key, value)| (key.clone(), value.clone()))
        .collect()
}

/// The integer param `key`.
pub fn param_int(manifest: &Manifest, key: &str) -> u64 {
    let value = manifest.params[key].as_integer().expect("an integer");
    u64::try_from(value).expect("not negative")
}

/// The params are what the update hook reads, no more and no fewer, and the step bounds the
/// hook is given are the ones hostkeeper holds it to.
pub fn check_params_mirror_the_manifest(manifest: &Manifest) {
    let keys: Vec<&str> = manifest.params.keys().map(String::as_str).collect();
    assert_eq!(keys, hook_params());
    let timeouts = manifest.hook.timeout_secs;
    assert_eq!(
        param_int(manifest, "prepare_timeout_secs"),
        timeouts.prepare.expect("a bounded prepare").get()
    );
    for (key, bound) in [
        ("cutover_timeout_secs", timeouts.cutover),
        ("verify_timeout_secs", timeouts.verify),
        ("rollback_timeout_secs", timeouts.rollback),
        ("cleanup_timeout_secs", timeouts.cleanup),
    ] {
        assert_eq!(param_int(manifest, key), bound.get(), "{key}");
    }
    assert_eq!(
        param_int(manifest, "verify_deadline_secs"),
        manifest.hook.verify_deadline_secs
    );
}

/// Catches a package hostkeeper's unpacker refuses (its archive rules, its manifest rules), and
/// any key of the manifest other than what the update hook and the units are built on: the hook
/// command, its bounds, the run unit, the tmpfiles link, the params.
#[test]
fn the_linux_packages_unpack_with_hostkeeper_and_their_manifest_is_the_one_the_hook_expects() {
    for platform in LINUX {
        let scratch = Scratch::new("hostkeeper-package-linux-unpack");
        let unpacked = unpack(scratch.path(), &package(platform), platform).expect("unpacked");
        let manifest = &unpacked.manifest;
        assert_eq!(manifest.name.as_str(), "js-agent");
        assert_eq!(manifest.version.as_str(), VERSION);
        assert_eq!(manifest.platform, hk(platform));
        assert_eq!(manifest.min_hostkeeper, running());
        assert_eq!(
            manifest.hook,
            Hook {
                command: vec!["bin/js-agent".to_owned(), "update-hook".to_owned()],
                boot: None,
                verify_deadline_secs: 9000,
                promotion_ceiling_secs: None,
                cleanup_retries: 3,
                timeout_secs: Timeouts {
                    prepare: bound(3900),
                    cutover: bound(4320).expect("a bound"),
                    verify: bound(615).expect("a bound"),
                    rollback: bound(8220).expect("a bound"),
                    cleanup: bound(120).expect("a bound"),
                    uninstall: None,
                },
            }
        );
        assert_eq!(
            manifest.unit_files,
            BTreeSet::from([
                "js-agent-crun-gpu.service".to_owned(),
                "js-agent-crun.service".to_owned()
            ])
        );
        assert_eq!(manifest.run, ["js-agent-crun.service"]);
        assert_eq!(manifest.health_url, None);
        let files: Vec<(&str, &str)> = manifest
            .files
            .iter()
            .map(|f| (f.source.as_str(), f.path.as_str()))
            .collect();
        assert_eq!(
            files,
            [("files/tmpfiles.conf", "/etc/tmpfiles.d/js-agent-crun.conf")]
        );
        let expected: BTreeMap<String, toml::Value> = [
            ("unit", "js-agent-crun.service".into()),
            ("state_dir", "/var/lib/js-agent-update".into()),
            (
                "release_link",
                "/var/lib/hostkeeper/packages/js-agent/current".into(),
            ),
            ("flags_file", FLAGS.into()),
            ("token_file", TOKEN.into()),
            ("state_root", "/var/lib/js-agent/state".into()),
            ("runtime", "crun".into()),
            ("cordon_lease_secs", 300.into()),
            ("cordon_max_secs", 17355.into()),
            ("drain_wait_secs", 3600.into()),
            ("defer_retry_secs", 300.into()),
            ("prepare_timeout_secs", 3900.into()),
            ("cutover_timeout_secs", 4320.into()),
            ("verify_timeout_secs", 615.into()),
            ("rollback_timeout_secs", 8220.into()),
            ("cleanup_timeout_secs", 120.into()),
            ("verify_deadline_secs", 9000.into()),
        ]
        .into_iter()
        .map(|(key, value): (&str, toml::Value)| (key.to_owned(), value))
        .collect();
        assert_eq!(params(manifest), expected);
        let mode = |name: &str| {
            let meta = std::fs::metadata(unpacked.dir.join(name)).expect("unpacked file");
            meta.permissions().mode() & 0o7777
        };
        assert_eq!(mode("bin/js-agent"), 0o555, "the binary is executable");
        assert_eq!(mode(UNIT), 0o444);
        assert_eq!(mode("files/tmpfiles.conf"), 0o444);
    }
}

/// Catches a manifest with an `uninstall` bound (hostkeeper then runs an uninstall order the
/// hook refuses), an `install` bound or any key the loader does not know: the loader refuses
/// each, so the package must hold none; the unedited manifest loading is the control that the
/// edits are what is refused.
#[test]
fn the_loader_refuses_an_install_bound_or_an_unknown_key_and_the_package_has_neither() {
    let scratch = Scratch::new("hostkeeper-package-keys");
    for platform in LINUX {
        let dir = scratch.path().join(platform.as_str());
        let loaded = load_edited(&dir.join("as-built"), platform, |text| text);
        assert_eq!(loaded.expect("loads").version.as_str(), VERSION);
        let install = load_edited(&dir.join("install"), platform, |text| {
            text.replace(
                "[hook.timeout_secs]\n",
                "[hook.timeout_secs]\ninstall = 300\n",
            )
        });
        assert_eq!(
            install.unwrap_err(),
            ManifestRefusal::UnknownKey {
                table: "hook.timeout_secs".to_owned(),
                key: "install".to_owned(),
            }
        );
        let unknown = load_edited(&dir.join("unknown"), platform, |text| {
            format!("user = \"root\"\n{text}")
        });
        assert_eq!(
            unknown.unwrap_err(),
            ManifestRefusal::UnknownKey {
                table: String::new(),
                key: "user".to_owned(),
            }
        );
        let toml = manifest_toml(&package(platform));
        let steps: Vec<&str> = toml["hook"]["timeout_secs"]
            .as_table()
            .expect("a table")
            .keys()
            .map(String::as_str)
            .collect();
        assert_eq!(
            steps,
            ["cleanup", "cutover", "prepare", "rollback", "verify"]
        );
    }
}

/// The params are exactly the keys the update hook reads (`js_agent::update_hook::params::KEYS`,
/// the list the hook itself requires), the step bounds in them are the manifest's own, and
/// hostkeeper's rule that a verify and a rollback fit in the deadline holds. Catches a param
/// the hook does not read (hostkeeper passes it on regardless), a param the hook needs and the
/// manifest lacks (the hook refuses its params), and a bound the manifest changes while the
/// hook keeps the old one.
#[test]
fn the_params_are_exactly_the_keys_the_update_hook_reads() {
    for platform in LINUX {
        let scratch = Scratch::new("hostkeeper-package-params");
        let unpacked = unpack(scratch.path(), &package(platform), platform).expect("unpacked");
        check_params_mirror_the_manifest(&unpacked.manifest);
        let hook = &unpacked.manifest.hook;
        assert!(
            hook.verify_deadline_secs
                >= hook.timeout_secs.verify.get() + hook.timeout_secs.rollback.get()
        );
    }
}

/// The agent's own parser reads the unit's command line once the flags file is expanded, and
/// the unit alone carries neither the server nor the pool. Catches a unit with a flag the
/// agent does not take, a server or pool written into the package (a public package cannot
/// know them), and a flag that is no longer the agent's.
#[test]
fn the_unit_expands_with_a_flags_file_to_a_command_line_the_agent_parses() {
    for platform in LINUX {
        let unit = text(&package(platform), UNIT);
        let exec = unit_value(&unit, "[Service]", "ExecStart");
        let words: Vec<&str> = exec.split_whitespace().collect();
        assert_eq!(words[0], CURRENT_BINARY);
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
}

/// Catches an `Environment=` or `EnvironmentFile=` in the unit (configuration is flags), and
/// token text in any file of the package other than the join token's path: outside comments, the
/// path of the file, the flag that names it and the param that names it are the only mentions,
/// so a package never carries a token.
#[test]
fn no_file_of_any_package_sets_the_environment_or_holds_token_text() {
    for platform in crate::support::ALL {
        let archive = package(platform);
        for entry in crate::support::listing(&archive) {
            if entry.kind != tar::EntryType::Regular || entry.name == "bin/js-agent" {
                continue;
            }
            let body = String::from_utf8(entry.content).expect("text");
            for line in body.lines() {
                let line = line.trim();
                assert!(
                    !line.starts_with("Environment=") && !line.starts_with("EnvironmentFile="),
                    "{}: {line}",
                    entry.name
                );
            }
            let rest = code(&body)
                .join("\n")
                .replace(TOKEN, "")
                .replace("--token-file", "")
                .replace("token_file", "")
                .to_lowercase();
            assert!(
                !rest.contains("token"),
                "{platform}: {} mentions a token",
                entry.name
            );
        }
    }
}

/// The unit is the farm unit of the setup script with one change of substance: the program is
/// run through hostkeeper's `current` link and the server and pool are read from the flags file.
/// Catches a setting that drifted between the two (a Delegate, a KillMode or a stop bound that
/// one has and the other lacks), which the farm unit's own tests pin for the farm unit only.
#[test]
fn the_unit_equals_the_farm_unit_except_the_program_and_the_flags_file() {
    let packaged = text(&package(LINUX[0]), UNIT);
    let farm = repo_file(FARM_UNIT);
    let rest = |text: &str| -> Vec<(String, String, String)> {
        assignments(text)
            .into_iter()
            .filter(|(_, key, _)| key != "ExecStart")
            .collect()
    };
    assert_eq!(rest(&packaged), rest(&farm));
    let words = |text: &str| -> Vec<String> {
        unit_value(text, "[Service]", "ExecStart")
            .split_whitespace()
            .map(str::to_owned)
            .collect()
    };
    let (packaged_words, farm_words) = (words(&packaged), words(&farm));
    assert_eq!(packaged_words[0], CURRENT_BINARY);
    assert_eq!(farm_words[0], "/opt/js-agent/link/current/js-agent");
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
}

/// The GPU oneshot and the tmpfiles file are the farm's, moved to hostkeeper's `current` and
/// without the farm release layout of `/opt/js-agent`.
#[test]
fn the_gpu_unit_and_the_tmpfiles_file_equal_the_farms_except_the_release_layout() {
    let archive = package(LINUX[0]);
    let packaged = text(&archive, GPU_UNIT);
    let farm = repo_file(FARM_GPU_UNIT);
    let exec = |unit: &str| unit_value(unit, "[Service]", "ExecStart");
    assert_eq!(
        exec(&packaged).replace(CURRENT_BINARY, "PROGRAM"),
        exec(&farm).replace("/opt/js-agent/link/current/js-agent", "PROGRAM")
    );
    let rest = |text: &str| -> Vec<(String, String, String)> {
        assignments(text)
            .into_iter()
            .filter(|(_, key, _)| key != "ExecStart")
            .collect()
    };
    assert_eq!(rest(&packaged), rest(&farm));
    // The unit it extends is the one hostkeeper links.
    assert!(exec(&packaged).contains("--unit /etc/systemd/system/js-agent-crun.service "));
    let layout_free: Vec<String> = code(&repo_file(FARM_TMPFILES))
        .into_iter()
        .filter(|line| !line.contains("/opt/js-agent"))
        .collect();
    assert_eq!(code(&text(&archive, "files/tmpfiles.conf")), layout_free);
    // The unit's ExecStartPre reads the file at the path the manifest links it to.
    let unit = text(&archive, UNIT);
    let pre = unit_value(&unit, "[Service]", "ExecStartPre");
    assert!(pre.ends_with("/etc/tmpfiles.d/js-agent-crun.conf"), "{pre}");
}

/// Catches an ExecStart through a version directory or another path (the unit would not follow
/// the switch of `current`), a run unit hostkeeper cannot enable (no WantedBy=), a stop bound
/// shorter than the drain (systemd would kill a drain in progress), and a cutover bound that is
/// no longer than the stop (hostkeeper would give up on a stop that is still draining).
#[test]
fn the_units_run_the_binary_of_current_and_their_stop_covers_the_drain() {
    for platform in LINUX {
        let archive = package(platform);
        let unit = text(&archive, UNIT);
        let exec = unit_value(&unit, "[Service]", "ExecStart");
        let program = exec.split_whitespace().next().expect("a program");
        assert_eq!(program, CURRENT_BINARY);
        assert!(CURRENT_BINARY.ends_with("/current/bin/js-agent"));
        assert!(through_current(Path::new(PACKAGES), "js-agent", program));
        assert_eq!(wanted_by(&unit), ["multi-user.target"]);
        for (name, file) in [
            ("js-agent-crun.service", UNIT),
            ("js-agent-crun-gpu.service", GPU_UNIT),
        ] {
            assert_eq!(
                check_unit_text(Path::new(PACKAGES), "js-agent", name, &text(&archive, file)),
                Ok(()),
                "{name}"
            );
        }
        let stop: u64 = unit_value(&unit, "[Service]", "TimeoutStopSec")
            .parse()
            .expect("seconds");
        let words: Vec<&str> = exec.split_whitespace().collect();
        let flag = |name: &str| -> u64 {
            let at = words.iter().position(|w| *w == name).expect("a flag");
            words[at + 1].parse().expect("seconds")
        };
        assert!(stop > flag("--drain-wait") + flag("--stop-grace"));
        let toml = manifest_toml(&archive);
        let cutover = toml["hook"]["timeout_secs"]["cutover"]
            .as_integer()
            .expect("an integer");
        assert!(u64::try_from(cutover).expect("positive") > stop);
    }
}
