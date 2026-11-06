//! The Mac package: its manifest and launchd job, read with hostkeeper's loader and the agent's
//! own parser, and against the job the installer package's provisioning tool renders.

use std::collections::{BTreeMap, BTreeSet};
use std::num::NonZeroU64;
use std::path::Path;

use clap::Parser;
use hostkeeper_config::manifest::{Hook, ManifestRefusal, Timeouts};
use hostkeeper_placed::lint::{plist_first_argument, through_current};
use js_agent::Cli;
use js_mac_provision::daemon::{
    AGENT_EXIT_TIMEOUT_SECS, AGENT_LABEL, Daemon, GATE, SESSION_SOCKET,
};
use xtask::release::hostkeeper_package::Platform;

use crate::linux::{check_params_mirror_the_manifest, params};
use crate::support::{
    CURRENT_BINARY, FLAGS, PACKAGES, SAMPLE_FLAGS, Scratch, TOKEN, VERSION, Value, expand, hk,
    listing, load_edited, manifest_toml, package, plist, running, string, strings, text, unpack,
};

const PLIST: &str = "units/js-agent.plist";
const MAC: Platform = Platform::MacosArm64;

fn bound(seconds: u64) -> NonZeroU64 {
    NonZeroU64::new(seconds).expect("not zero")
}

/// Catches a Mac package hostkeeper's unpacker refuses, and a manifest whose bounds, run label,
/// files or params are not the ones the hook and the launchd job are built on.
#[test]
fn the_macos_package_unpacks_with_hostkeeper_and_its_manifest_is_the_one_the_hook_expects() {
    let scratch = Scratch::new("hostkeeper-package-macos-unpack");
    let unpacked = unpack(scratch.path(), &package(MAC), MAC).expect("unpacked");
    let manifest = &unpacked.manifest;
    assert_eq!(manifest.name.as_str(), "js-agent");
    assert_eq!(manifest.version.as_str(), VERSION);
    assert_eq!(manifest.platform, hk(MAC));
    assert_eq!(manifest.min_hostkeeper, running());
    assert_eq!(
        manifest.hook,
        Hook {
            command: vec!["bin/js-agent".to_owned(), "update-hook".to_owned()],
            boot: None,
            verify_deadline_secs: 1200,
            promotion_ceiling_secs: None,
            cleanup_retries: 3,
            timeout_secs: Timeouts {
                prepare: Some(bound(3900)),
                cutover: bound(300),
                verify: bound(600),
                rollback: bound(600),
                cleanup: bound(300),
                uninstall: None,
            },
        }
    );
    assert_eq!(
        manifest.unit_files,
        BTreeSet::from(["js-agent.plist".to_owned()])
    );
    assert_eq!(manifest.run, ["js-agent"]);
    assert!(manifest.files.is_empty(), "{:?}", manifest.files);
    let expected: BTreeMap<String, toml::Value> = [
        ("unit", "js-agent".into()),
        ("state_dir", "/var/db/js-agent-update".into()),
        (
            "release_link",
            "/var/lib/hostkeeper/packages/js-agent/current".into(),
        ),
        ("flags_file", FLAGS.into()),
        ("token_file", TOKEN.into()),
        ("state_root", "/var/db/js-agent/state".into()),
        ("runtime", "native".into()),
        ("cordon_lease_secs", 300.into()),
        ("cordon_max_secs", 14400.into()),
        ("drain_wait_secs", 3600.into()),
        ("defer_retry_secs", 300.into()),
        ("prepare_timeout_secs", 3900.into()),
        ("cutover_timeout_secs", 300.into()),
        ("verify_timeout_secs", 600.into()),
        ("rollback_timeout_secs", 600.into()),
        ("cleanup_timeout_secs", 300.into()),
        ("verify_deadline_secs", 1200.into()),
    ]
    .into_iter()
    .map(|(key, value): (&str, toml::Value)| (key.to_owned(), value))
    .collect();
    assert_eq!(params(manifest), expected);
    check_params_mirror_the_manifest(manifest);
}

/// Catches a manifest of the Mac that grew or lost a table or a bound against the Linux one
/// without a reason: they share every key of `[hook]` and `[hook.timeout_secs]` and the params,
/// and differ in having no `[[files]]`.
#[test]
fn the_macos_manifest_has_the_keys_of_the_linux_one_but_no_files() {
    let keys = |table: &toml::Table| -> Vec<String> { table.keys().cloned().collect() };
    let mac = manifest_toml(&package(MAC));
    let linux = manifest_toml(&package(Platform::LinuxX86_64));
    let section = |m: &toml::Table, name: &str| -> toml::Table {
        m[name].as_table().expect("a table").clone()
    };
    assert_eq!(keys(&section(&mac, "hook")), keys(&section(&linux, "hook")));
    assert_eq!(
        keys(&section(&section(&mac, "hook"), "timeout_secs")),
        keys(&section(&section(&linux, "hook"), "timeout_secs"))
    );
    assert_eq!(
        keys(&section(&mac, "params")),
        keys(&section(&linux, "params"))
    );
    assert!(linux.contains_key("files"));
    assert!(!mac.contains_key("files"));
}

/// Catches a package with a second job, a service file in the Mac package or a plist in a Linux
/// one: hostkeeper refuses a package that mixes the two kinds.
#[test]
fn the_macos_package_has_one_job_and_no_service_file_and_the_linux_ones_no_plist() {
    let names = |platform| -> Vec<String> {
        listing(&package(platform))
            .into_iter()
            .map(|entry| entry.name)
            .filter(|name| name.starts_with("units/"))
            .collect()
    };
    assert_eq!(names(MAC), [PLIST]);
    for platform in [Platform::LinuxX86_64, Platform::LinuxArm64] {
        assert!(
            names(platform)
                .iter()
                .all(|name| name.ends_with(".service"))
        );
    }
}

/// Catches a label other than the service's: hostkeeper takes the job from `units/<label>.plist`
/// and refuses a label that is neither the service's name nor starts with it, so the installer
/// package's `com.komira.js-agent` cannot be the label. The unedited manifest loading, and the
/// plist's own label being the run entry, are the controls.
#[test]
fn the_label_of_the_job_is_the_run_entry_and_hostkeeper_refuses_the_installers_label() {
    let scratch = Scratch::new("hostkeeper-package-macos-label");
    let archive = package(MAC);
    let job = plist(&text(&archive, PLIST));
    assert_eq!(string(&job["Label"]), "js-agent");
    let loaded = load_edited(&scratch.path().join("as-built"), MAC, |text| text).expect("loads");
    assert_eq!(loaded.run, [string(&job["Label"])]);
    assert_ne!(AGENT_LABEL, "js-agent");
    let relabelled = load_edited(&scratch.path().join("relabelled"), MAC, |text| {
        text.replace(
            "run = [\"js-agent\"]",
            &format!("run = [\"{AGENT_LABEL}\"]"),
        )
    });
    assert!(
        matches!(&relabelled, Err(ManifestRefusal::RunUnit { unit, .. }) if unit == AGENT_LABEL),
        "{relabelled:?}"
    );
}

/// The job runs the binary of `current` and the agent's own parser reads its arguments once the
/// flags file is expanded; the job alone has no server or pool. Catches a flag the agent does not
/// take, a server or pool written into the package, a program through a version directory.
#[test]
fn the_plist_runs_the_binary_of_current_and_its_arguments_parse_as_the_agents() {
    let archive = package(MAC);
    let body = text(&archive, PLIST);
    let job = plist(&body);
    let words = strings(&job["ProgramArguments"]);
    assert_eq!(words[0], CURRENT_BINARY);
    assert_eq!(words[1..3], ["--flags-file", FLAGS]);
    assert!(through_current(Path::new(PACKAGES), "js-agent", words[0]));
    assert_eq!(plist_first_argument(&body).as_deref(), Some(CURRENT_BINARY));
    let unexpanded = Cli::try_parse_from(&words).unwrap_err();
    assert_eq!(
        unexpanded.kind(),
        clap::error::ErrorKind::MissingRequiredArgument
    );
    let cli = Cli::try_parse_from(expand(&words, SAMPLE_FLAGS)).expect("a command line");
    assert_eq!(cli.server, "https://js.example.com:8443");
    assert_eq!(cli.pool, "farm");
    assert_eq!(cli.token_file.as_deref(), Some(Path::new(TOKEN)));
    assert_eq!(
        cli.state_root.as_deref(),
        Some(Path::new("/var/db/js-agent/state"))
    );
    assert_eq!(
        cli.scratch_root.as_deref(),
        Some(Path::new("/var/db/js-agent/scratch"))
    );
    assert_eq!(
        cli.native_helper_socket.as_deref(),
        Some(Path::new(SESSION_SOCKET))
    );
}

/// The plist is the installer package's agent job with the label and the program's arguments of
/// hostkeeper's layout: every other key is equal, and the arguments differ by the flags file in
/// place of the server and the pool. Catches a key that drifted between the two (the exit
/// timeout, the throttle, the user, the gate on the token file, the logs).
#[test]
fn the_plist_equals_the_provisioning_tools_agent_plist_except_the_label_and_the_arguments() {
    let packaged_text = text(&package(MAC), PLIST);
    let packaged = plist(&packaged_text);
    let rendered = Daemon::agent("https://js.example.com:8443", "farm").render();
    let farm = plist(&rendered);
    let without = |mut job: BTreeMap<String, Value>| {
        job.remove("Label");
        job.remove("ProgramArguments");
        job
    };
    assert_eq!(without(packaged.clone()), without(farm.clone()));
    assert_eq!(string(&farm["Label"]), AGENT_LABEL);
    assert_eq!(
        packaged["ExitTimeOut"],
        Value::Integer(i64::from(AGENT_EXIT_TIMEOUT_SECS))
    );
    // The job starts only while the token file exists.
    let Value::Dict(keep) = &packaged["KeepAlive"] else {
        panic!("KeepAlive is a dictionary");
    };
    let Value::Dict(paths) = &keep["PathState"] else {
        panic!("PathState is a dictionary");
    };
    assert_eq!(paths.keys().map(String::as_str).collect::<Vec<_>>(), [GATE]);
    assert_eq!(GATE, TOKEN);
    // The arguments: the flags file in place of the server and the pool.
    let packaged_args = strings(&packaged["ProgramArguments"]);
    let farm_args = strings(&farm["ProgramArguments"]);
    let kept = |args: &[&str], drop: &[&str]| -> Vec<String> {
        let mut out = Vec::new();
        let mut words = args[1..].iter();
        while let Some(word) = words.next() {
            if drop.contains(word) {
                words.next();
            } else {
                out.push((*word).to_owned());
            }
        }
        out
    };
    assert_eq!(
        kept(&packaged_args, &["--flags-file"]),
        kept(&farm_args, &["--server", "--pool"])
    );
    // And the whole text is what the provisioning tool renders for that job.
    let mut daemon = Daemon::agent("unused", "unused");
    daemon.label = "js-agent".to_owned();
    daemon.program = packaged_args[0].to_owned();
    daemon.args = packaged_args[1..].iter().map(|w| (*w).to_owned()).collect();
    assert_eq!(daemon.render(), packaged_text);
}
