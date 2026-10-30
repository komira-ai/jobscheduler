//! The files of the js-agent package (`packaging/js-agent`) and the workflow that builds and
//! attests it (`.github/workflows/js-agent-deb.yml`), read as they are in the repository:
//! the unit restarts the agent after every exit with the flags the agent accepts, waits for the
//! container engine, makes its directories and waits for its scratch disk, and sets no sandboxing
//! the agent refuses; its directories are closed to everyone but root; the nfpm configuration
//! lists the files the checker (`xtask::pkg`) accepts; and the workflow publishes nothing.
//! Nothing here starts the unit: these are static checks of the files.

use std::path::{Path, PathBuf};

use clap::Parser;
use js_agent::Cli;
use js_agent::lifecycle::DEFAULT_STOP_GRACE;
use xtask::pkg::{FILES, Source};

const UNIT: &str = "packaging/js-agent/js-agent.service";
const WORKFLOW: &str = ".github/workflows/js-agent-deb.yml";

fn root() -> &'static Path {
    Path::new(concat!(env!("CARGO_MANIFEST_DIR"), "/../.."))
}

fn read(path: &str) -> String {
    std::fs::read_to_string(root().join(path)).unwrap_or_else(|e| panic!("{path}: {e}"))
}

/// The lines of `text` that are not comments or blank, without their indentation.
fn code(text: &str) -> Vec<&str> {
    text.lines()
        .map(str::trim)
        .filter(|l| !l.is_empty() && !l.starts_with('#'))
        .collect()
}

/// The lines of `text` that are not comments or blank, as they are.
fn indented_code(text: &str) -> Vec<&str> {
    text.lines()
        .filter(|l| !l.trim().is_empty() && !l.trim_start().starts_with('#'))
        .collect()
}

/// The settings of the unit: section, key and value.
fn settings() -> Vec<(String, String, String)> {
    let text = read(UNIT);
    let mut section = String::new();
    let mut found = Vec::new();
    for line in code(&text) {
        if let Some(name) = line
            .strip_prefix('[')
            .and_then(|rest| rest.strip_suffix(']'))
        {
            section = name.to_owned();
        } else {
            let (key, value) = line.split_once('=').unwrap_or_else(|| panic!("{line}"));
            found.push((section.clone(), key.to_owned(), value.to_owned()));
        }
    }
    found
}

/// Whether the unit sets `key`, in any section.
fn has_key(key: &str) -> bool {
    settings().iter().any(|(_, k, _)| k == key)
}

/// The value of the one `key=` line of the unit.
fn unit_value(key: &str) -> String {
    let found: Vec<String> = settings()
        .into_iter()
        .filter(|(_, k, _)| k == key)
        .map(|(_, _, v)| v)
        .collect();
    assert_eq!(found.len(), 1, "{key}: {found:?}");
    found[0].clone()
}

/// The words of the unit's `ExecStart`.
fn exec_words() -> Vec<String> {
    unit_value("ExecStart")
        .split_whitespace()
        .map(str::to_owned)
        .collect()
}

/// The unit's `ExecStart` as the agent's own command line parser reads it: a flag the agent
/// does not have, or a value it refuses, is a failure here and a restart loop on a host.
fn agent_cli() -> Cli {
    Cli::try_parse_from(exec_words())
        .unwrap_or_else(|e| panic!("ExecStart is not a command line of js-agent: {e}"))
}

fn seconds(text: &str) -> u64 {
    text.parse().unwrap_or_else(|e| panic!("{text}: {e}"))
}

/// The lines of the tmpfiles file, each split into its fields: type, path, mode, owner,
/// group, age.
fn tmpfiles() -> Vec<Vec<String>> {
    let text = read("packaging/js-agent/tmpfiles.conf");
    code(&text)
        .into_iter()
        .map(|line| line.split_whitespace().map(str::to_owned).collect())
        .collect()
}

/// The agent is started again after every exit, the status 0 of a stop request and the status
/// 2 of a refusal included (the reason is in the unit's comments and in the agent's docs), and
/// not faster than every 30 seconds, so that a refusal that cannot change (a retired token, an
/// unknown pool) does not send the server a hello every few seconds. `RestartSteps=` and
/// `RestartMaxDelaySec=` need systemd 254, which Debian 12 (252) does not have.
#[test]
fn the_unit_restarts_the_agent_after_every_exit_and_not_in_a_tight_loop() {
    assert_eq!(unit_value("Restart"), "always");
    for key in [
        "RestartPreventExitStatus",
        "SuccessExitStatus",
        "RestartForceExitStatus",
        "RestartSteps",
        "RestartMaxDelaySec",
    ] {
        assert!(!has_key(key), "{key} in the unit");
    }
    assert_eq!(unit_value("RestartSec"), "30");
}

/// The unit runs the binary the package installs, with its settings as flags that the agent's
/// own command line parser accepts: the agent reads no environment variable and no file of
/// settings, and the join token is never on the command line (argv is world-readable; the agent
/// has no flag for it), only the path of its file.
#[test]
fn the_unit_runs_the_installed_binary_with_flags_the_agent_accepts() {
    let words = exec_words();
    assert!(
        FILES
            .iter()
            .any(|(path, _, _)| format!("/{path}") == words[0]),
        "{words:?}"
    );
    let cli = agent_cli();
    assert_eq!(cli.pool, "default");
    assert_eq!(
        cli.token_file.as_deref(),
        Some(Path::new("/etc/js-agent/token"))
    );
    // The bundled roots serve https, and a host of the package joins with the pool's token.
    assert!(cli.ca_cert.is_none() && cli.instance_file.is_none());
    assert_eq!(unit_value("WantedBy"), "multi-user.target");
}

/// An operator who enables the unit without editing it gets a failure that names the flag, and
/// not an agent that sits running and retries a server that is not there: the shipped `--server`
/// is not an address the agent's client accepts.
#[test]
fn the_shipped_server_is_a_placeholder_the_agent_refuses_at_start() {
    let cli = agent_cli();
    assert!(
        js_client::ServerUrl::parse(&cli.server).is_err(),
        "{} is a usable server",
        cli.server
    );
}

/// `TimeoutStopSec` covers the drain a SIGTERM starts: `--drain-wait` for the jobs to finish,
/// `--stop-grace` to stop them, `DRAIN_SETTLE` for the server to acknowledge the results, and
/// 30 seconds for the engine's calls. A flag a host raises raises the timeout with it, or the
/// agent is killed with results unacknowledged.
#[test]
fn the_stop_timeout_covers_the_drain_the_flags_ask_for() {
    let cli = agent_cli();
    let grace = cli.stop_grace.unwrap_or(DEFAULT_STOP_GRACE.as_secs());
    let wait = cli.drain_wait.unwrap_or(grace);
    let drain = wait + grace + js_agent::DRAIN_SETTLE.as_secs();
    let timeout = seconds(&unit_value("TimeoutStopSec"));
    assert!(
        timeout >= drain + 30,
        "TimeoutStopSec={timeout} for a drain of {drain} s"
    );
    // The signal and the kill mode are systemd's defaults (SIGTERM to the control group): the
    // agent's containers live in the engine's cgroup, not the unit's.
    assert!(!has_key("KillMode") && !has_key("KillSignal"));
}

/// The agent is stopped before the engine at a shutdown (it stops and lists containers through
/// the engine while it drains), which `After=` alone gives: a `Wants=` or `Requires=` on the
/// engine would also start docker for a host whose engine is podman.
#[test]
fn the_unit_is_ordered_after_the_engine_and_does_not_pull_it_in() {
    let after = unit_value("After");
    let after: Vec<&str> = after.split_whitespace().collect();
    for needed in [
        "network-online.target",
        "docker.service",
        "containerd.service",
    ] {
        assert!(after.contains(&needed), "{needed} in {after:?}");
    }
    assert_eq!(unit_value("Wants"), "network-online.target");
    for key in ["Requires", "Requisite", "BindsTo", "PartOf", "Upholds"] {
        assert!(!has_key(key), "{key} in the unit");
    }
}

/// The unit's paths are the ones the elastic boot payload gives its agent for the engine, the
/// state root and the scratch root, and the unit waits for the scratch disk by the same path.
/// (The token is not among them: the payload keeps a one-time token and an instance file in
/// `/etc/jobscheduler`, a host of the package an operator's pool token in `/etc/js-agent`.)
#[test]
fn the_unit_uses_the_paths_of_the_elastic_agent() {
    let cli = agent_cli();
    let path = |p: &Option<PathBuf>| p.as_deref().map(Path::to_path_buf);
    assert_eq!(
        path(&cli.container_bin),
        Some(js_elastic::CONTAINER_BIN.into())
    );
    assert_eq!(path(&cli.state_root), Some(js_elastic::STATE_ROOT.into()));
    assert_eq!(
        path(&cli.scratch_root),
        Some(js_elastic::SCRATCH_ROOT.into())
    );
    assert_eq!(unit_value("RequiresMountsFor"), js_elastic::SCRATCH_ROOT);
}

/// A host without a disk of its own at the scratch root (the agent refuses a state root on the
/// scratch root's disk) does not start the agent and loop: the unit's condition names the mount
/// point and `systemctl status` says so.
#[test]
fn the_unit_starts_only_with_a_mount_at_the_scratch_root() {
    assert_eq!(
        unit_value("ConditionPathIsMountPoint"),
        agent_cli()
            .scratch_root
            .expect("a scratch root")
            .to_str()
            .expect("utf-8")
    );
}

/// The directories exist at every start, with the modes of the tmpfiles file, also on a first
/// install without a reboot (dpkg does not run systemd-tmpfiles) and after a disk was mounted at
/// the scratch root: the unit runs the installed tmpfiles file before it starts the agent.
#[test]
fn the_unit_makes_its_directories_before_every_start() {
    let (installed, _, _) = FILES
        .iter()
        .find(|(_, _, source)| matches!(source, Source::Tmpfiles))
        .expect("the tmpfiles file");
    assert_eq!(
        unit_value("ExecStartPre"),
        format!("+systemd-tmpfiles --create /{installed}")
    );
}

/// The agent must see the host (it reads `/proc/1/ns`, shares init's pid and mount namespaces
/// and owns the state and scratch roots) and refuses to start under `ProtectSystem=`,
/// `PrivateMounts=` or `PrivatePIDs=`, so the unit runs as root with no sandboxing and none of
/// the keys that would change that or the flags-only configuration.
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

/// Every setting of the unit is one of these: a new one is added here with its reason, so that
/// no key that changes what the agent can see or what starts it arrives unnoticed.
#[test]
fn the_unit_sets_only_the_settings_listed_here() {
    let mut found: Vec<(String, String)> = settings().into_iter().map(|(s, k, _)| (s, k)).collect();
    found.sort();
    let mut want: Vec<(String, String)> = [
        ("Unit", "After"),
        ("Unit", "ConditionPathIsMountPoint"),
        ("Unit", "Description"),
        ("Unit", "Documentation"),
        ("Unit", "RequiresMountsFor"),
        ("Unit", "Wants"),
        ("Service", "ExecStart"),
        ("Service", "ExecStartPre"),
        ("Service", "Restart"),
        ("Service", "RestartSec"),
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

/// The state root (the taint marker, and the host credential the agent writes at mode 0600 once
/// the server admits the host), the scratch root and the directory of the join token are made,
/// each root's with mode 0700 and no cleaning age: a credential in a directory a user other
/// than root can enter or list is exposed, and an age on a `d` line would let
/// `systemd-tmpfiles --clean` delete the credential. No other directory is made.
#[test]
fn the_directories_are_roots_mode_0700_and_never_cleaned() {
    let cli = agent_cli();
    let token = cli.token_file.expect("a token file");
    let token_dir = token.parent().expect("a directory");
    assert_eq!(token_dir, Path::new("/etc/js-agent"));
    let state = cli.state_root.expect("a state root");
    let scratch = cli.scratch_root.expect("a scratch root");
    // The state root and the scratch root lie in a directory that is also made.
    let mut want: Vec<&Path> = vec![
        token_dir,
        state.parent().expect("a directory"),
        &state,
        &scratch,
    ];
    want.sort();
    let dirs = tmpfiles();
    let mut paths: Vec<&Path> = dirs.iter().map(|f| Path::new(f[1].as_str())).collect();
    paths.sort();
    assert_eq!(paths, want);
    for fields in &dirs {
        let line = fields.join(" ");
        assert_eq!(fields.len(), 6, "{line}");
        assert_eq!(fields[0], "d", "{line}");
        assert_eq!(fields[2], "0700", "{line}");
        assert_eq!((fields[3].as_str(), fields[4].as_str()), ("root", "root"));
        assert_eq!(fields[5], "-", "{line}");
    }
}

/// The nfpm configuration installs exactly the files the checker accepts, with the modes it
/// accepts, from the files the build makes or the repository has, and has no maintainer script
/// or dependency of its own.
#[test]
fn the_nfpm_configuration_lists_the_files_of_the_checker() {
    let text = read("packaging/js-agent/nfpm.yaml");
    let lines = code(&text);
    let target = workflow_target();
    let mut listed = Vec::new();
    let (mut src, mut dst) = (String::new(), String::new());
    // An entry without its file_info adds no row to `listed`, so count the entries too.
    let entries = lines.iter().filter(|l| l.starts_with("- src: ")).count();
    assert_eq!(
        entries,
        FILES.len(),
        "entries of nfpm.yaml, files of the checker"
    );
    for line in &lines {
        if let Some(s) = line.strip_prefix("- src: ") {
            src = s.to_owned();
        }
        if let Some(d) = line.strip_prefix("dst: ") {
            dst = d.trim_start_matches('/').to_owned();
        }
        if let Some(m) = line.strip_prefix("mode: ") {
            let mode = u32::from_str_radix(m, 8).expect("a mode");
            listed.push((src.clone(), dst.clone(), mode));
        }
    }
    let mut want: Vec<(String, String, u32)> = FILES
        .iter()
        .map(|(path, mode, source)| {
            let src = match source {
                Source::Binary => format!("target/{target}/release/js-agent"),
                Source::Service => "packaging/js-agent/js-agent.service".to_owned(),
                Source::Tmpfiles => "packaging/js-agent/tmpfiles.conf".to_owned(),
                Source::License => "LICENSE".to_owned(),
            };
            (src, (*path).to_owned(), *mode)
        })
        .collect();
    want.sort();
    listed.sort();
    assert_eq!(listed, want);
    for (src, _, _) in &listed {
        assert!(src.contains(&target) || root().join(src).is_file(), "{src}");
    }
    for forbidden in [
        "scripts:",
        "depends:",
        "preinstall",
        "postinstall",
        "type: dir",
    ] {
        assert!(!lines.iter().any(|l| l.contains(forbidden)), "{forbidden}");
    }
    assert!(lines.contains(&"version: ${VERSION}"));
    assert!(lines.contains(&"compression: gzip"));
}

/// The jobs of a workflow: name and the lines of its block.
fn jobs(text: &str) -> Vec<(String, Vec<String>)> {
    let mut jobs: Vec<(String, Vec<String>)> = Vec::new();
    let mut in_jobs = false;
    for line in text.lines() {
        if line == "jobs:" {
            in_jobs = true;
        } else if in_jobs {
            let key = line
                .strip_prefix("  ")
                .filter(|rest| !rest.starts_with([' ', '#']))
                .and_then(|rest| rest.strip_suffix(':'));
            if let Some(name) = key {
                jobs.push((name.to_owned(), Vec::new()));
            } else if let Some((_, lines)) = jobs.last_mut() {
                lines.push(line.to_owned());
            }
        }
    }
    jobs
}

/// The lines of the top-level block `key` of a workflow, which are indented.
fn block<'a>(lines: &[&'a str], key: &str) -> Vec<&'a str> {
    let head = format!("{key}:");
    lines
        .iter()
        .copied()
        .skip_while(|l| *l != head)
        .skip(1)
        .take_while(|l| l.starts_with(' '))
        .collect()
}

/// The job `name` of the workflow `text`.
fn job(text: &str, name: &str) -> Vec<String> {
    jobs(text)
        .into_iter()
        .find(|(n, _)| n == name)
        .unwrap_or_else(|| panic!("job {name}"))
        .1
}

/// The `TARGET` the workflow builds for.
fn workflow_target() -> String {
    let text = read(WORKFLOW);
    let env = block(&indented_code(&text), "env");
    let target: Vec<&str> = env
        .iter()
        .filter_map(|l| l.trim().strip_prefix("TARGET: "))
        .collect();
    assert_eq!(target.len(), 1, "{target:?}");
    target[0].to_owned()
}

/// A line of a workflow with its quotes dropped and its white space made single, so that
/// `id-token:  'write'` and `id-token: write` compare equal.
fn normal(line: &str) -> String {
    let plain = line.replace(['\'', '"'], "");
    plain.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// The lines of `lines` (comments left out) that grant a write scope, in any spelling: a
/// `: write` line, `write-all`, or a flow-style mapping.
fn write_scopes(lines: &[String]) -> Vec<String> {
    let mut found: Vec<String> = lines
        .iter()
        .filter(|l| !l.trim_start().starts_with('#'))
        .map(|l| normal(l))
        .filter(|l| l.contains("write"))
        .collect();
    found.sort();
    found
}

/// The commands of the `run:` steps of a workflow, one per line of a block.
fn run_lines(text: &str) -> Vec<String> {
    let lines: Vec<&str> = text.lines().collect();
    let mut found = Vec::new();
    for (at, line) in lines.iter().enumerate() {
        let body = line.trim_start();
        let key = body.strip_prefix("- ").unwrap_or(body).trim_start();
        let Some(rest) = key.strip_prefix("run:") else {
            continue;
        };
        let rest = rest.trim();
        if rest == "|" {
            let key_indent = line.len() - key.len();
            for next in &lines[at + 1..] {
                let blank = next.trim().is_empty();
                if !blank && next.len() - next.trim_start().len() <= key_indent {
                    break;
                }
                if !blank {
                    found.push(next.trim().to_owned());
                }
            }
        } else {
            found.push(rest.to_owned());
        }
    }
    found
}

/// The workflow builds, checks, uploads as a workflow artifact and attests, and does nothing
/// else with the deb: no write scope on the repository, packages or releases, no action but
/// four pinned by commit, and only these commands. The list of banned words is a tripwire, not
/// the proof: the proof is the absence of a write scope (below) and the list of commands.
#[test]
fn the_workflow_publishes_nothing() {
    let text = read(WORKFLOW);
    let steps = code(&text);
    for line in &steps {
        for banned in [
            "contents: write",
            "packages: write",
            "write-all",
            "gh release",
            "gh api",
            "cargo publish",
            "docker push",
            "docker login",
            "git push",
            "git tag",
            "twine",
            "aws ",
            "gsutil",
            "gcloud",
            "az ",
            "apt-repo",
            "reprepro",
            "aptly",
            "action-gh-release",
            "upload-release-asset",
            "upload-pages",
            "ghcr.io",
            "curl",
            "wget",
        ] {
            assert!(!line.contains(banned), "{banned} in {line}");
        }
        if let Some(action) = line.trim_start_matches("- ").strip_prefix("uses: ") {
            let name = action.split('@').next().expect("a name");
            assert!(
                [
                    "actions/checkout",
                    "actions/upload-artifact",
                    "actions/download-artifact",
                    "actions/attest-build-provenance",
                ]
                .contains(&name),
                "{name}"
            );
            let sha = action.split('@').nth(1).expect("a version");
            let sha = sha.split(' ').next().expect("a sha");
            assert!(
                sha.len() == 40 && sha.bytes().all(|b| b.is_ascii_hexdigit()),
                "{action}"
            );
        }
    }
    assert!(text.contains("name: js-agent-deb"));
    // The commands are an allowlist, and none chains another or takes text from an expression.
    let allowed = [
        "sudo apt-get update",
        "sudo apt-get install --yes --no-install-recommends musl-tools",
        "rustup toolchain install",
        "rustup target add \"$TARGET\"",
        "cargo xtask release stamp --to \"$VERSION\"",
        "cargo build --locked --release --target \"$TARGET\" -p js-agent --bin js-agent",
        "cargo xtask fetch --url ",
        "$RUNNER_TEMP/tools/nfpm package --config packaging/js-agent/nfpm.yaml --packager deb ",
        "cargo xtask pkg check-deb --deb ",
        "gh attestation verify ",
    ];
    let runs = run_lines(&text);
    assert_eq!(runs.len(), 10, "{runs:?}");
    for run in &runs {
        assert!(allowed.iter().any(|a| run.starts_with(a)), "{run}");
        for chain in ["&&", "||", ";", "|", "`", "$(", ">", "${{"] {
            assert!(!run.contains(chain), "{chain} in {run}");
        }
    }
}

/// No job and no top level but the attest job's holds a write scope, whatever its spelling, and
/// the attest job's are exactly the two that attestation needs.
#[test]
fn only_the_attest_job_holds_a_write_scope_and_it_is_attestation() {
    let text = read(WORKFLOW);
    let lines = indented_code(&text);
    assert_eq!(
        lines
            .iter()
            .copied()
            .filter(|l| l.starts_with("permissions:"))
            .collect::<Vec<_>>(),
        ["permissions: {}"]
    );
    let top: Vec<String> = lines
        .iter()
        .copied()
        .take_while(|l| *l != "jobs:")
        .map(str::to_owned)
        .collect();
    assert_eq!(write_scopes(&top), Vec::<String>::new());
    for name in ["build", "verify"] {
        assert_eq!(
            write_scopes(&job(&text, name)),
            Vec::<String>::new(),
            "{name}"
        );
    }
    assert_eq!(
        write_scopes(&job(&text, "attest")),
        ["attestations: write", "id-token: write"]
    );
}

/// The attestation is made and verified only by a manual run on main, by jobs that run no
/// repository code: a pull request builds and checks the deb and attests nothing. The gate is
/// the job's own `if:`, not a step's or a comment's.
#[test]
fn the_attest_and_verify_jobs_run_only_for_a_manual_run_on_main() {
    let text = read(WORKFLOW);
    let names: Vec<String> = jobs(&text).into_iter().map(|(n, _)| n).collect();
    assert_eq!(names, ["build", "attest", "verify"]);
    let gate =
        "if: ${{ github.event_name == 'workflow_dispatch' && github.ref == 'refs/heads/main' }}";
    let job_ifs = |name: &str| -> Vec<String> {
        let lines = job(&text, name);
        lines
            .into_iter()
            .take_while(|l| l != "    steps:")
            .filter(|l| l.starts_with("    if:"))
            .collect()
    };
    for name in ["attest", "verify"] {
        assert_eq!(job_ifs(name), [format!("    {gate}")], "{name}");
        let body = job(&text, name).join("\n");
        assert!(
            !body.contains("actions/checkout") && !body.contains("cargo "),
            "{name} runs no repository code"
        );
    }
    assert_eq!(job_ifs("build"), Vec::<String>::new());
    let build = job(&text, "build").join("\n");
    assert!(build.contains("permissions:\n      contents: read\n    steps:"));
    assert!(build.contains("cargo xtask pkg check-deb"));
    assert!(build.contains("nfpm package"));
    // The musl release binary is built and checked static on a pull request too.
    assert!(build.contains("-p js-agent --bin js-agent"));
    assert!(build.contains("grep -E 'static-pie linked|statically linked'"));
    assert!(build.contains("--sha256 \"$NFPM_SHA256_X86_64_LINUX\""));
}

/// What starts the workflow: a pull request that touches what the package, its checker or the
/// agent are made of (any crate, the manifests, the toolchain), and a manual run. Not `pull_request_target`, `workflow_run`, a push or a schedule,
/// which would give code from outside the repository the workflow's scopes.
#[test]
fn the_workflow_starts_only_on_these_pull_requests_and_by_hand() {
    let text = read(WORKFLOW);
    let lines = indented_code(&text);
    let on = block(&lines, "on");
    let events: Vec<&str> = on
        .iter()
        .filter(|l| !l.starts_with("   "))
        .map(|l| l.trim())
        .collect();
    assert_eq!(events, ["pull_request:", "workflow_dispatch:"]);
    let paths: Vec<&str> = on
        .iter()
        .filter_map(|l| l.trim().strip_prefix("- "))
        .collect();
    assert_eq!(
        paths,
        [
            "'.github/workflows/js-agent-deb.yml'",
            "'packaging/js-agent/**'",
            "'crates/**'",
            "'Cargo.toml'",
            "'Cargo.lock'",
            "'rust-toolchain.toml'",
            "'.cargo/**'",
            "'LICENSE'",
        ]
    );
    // Two manual runs of different versions are not one group, which would drop a pending one.
    let concurrency = block(&lines, "concurrency");
    assert!(
        concurrency
            .iter()
            .any(|l| l.trim() == "group: js-agent-deb-${{ github.ref }}-${{ inputs.version }}"),
        "{concurrency:?}"
    );
    assert!(
        concurrency
            .iter()
            .any(|l| l.trim() == "cancel-in-progress: false")
    );
}

/// Every checkout drops the token from the clone, and the deb that is uploaded and attested is
/// the one `check-deb` read, by its exact name: no glob that a file of the same shape in the
/// checkout or the download could match.
#[test]
fn the_deb_is_named_exactly_and_the_checkout_keeps_no_credentials() {
    let text = read(WORKFLOW);
    let steps = code(&text);
    let count = |line: &str| steps.iter().filter(|l| l.contains(line)).count();
    assert!(count("uses: actions/checkout") >= 1);
    assert_eq!(
        count("uses: actions/checkout"),
        count("persist-credentials: false")
    );
    assert!(steps.contains(&"path: js-agent_${{ env.VERSION }}_amd64.deb"));
    assert!(steps.contains(&"subject-path: dist/js-agent_${{ env.VERSION }}_amd64.deb"));
    assert!(!steps.iter().any(|l| l.contains('*') && l.contains(".deb")));
    let check = run_lines(&text)
        .into_iter()
        .find(|l| l.starts_with("cargo xtask pkg check-deb"))
        .expect("the check");
    assert!(
        check.contains("--deb \"js-agent_${VERSION}_amd64.deb\""),
        "{check}"
    );
    assert!(check.contains("--license LICENSE"), "{check}");
    assert!(check.contains("--packaging packaging/js-agent"), "{check}");
}

/// The verify step is the one piece a pull request cannot run (the job is skipped there), so
/// its constraints are pinned as text: this repository, this workflow, main, this commit, and
/// no self-hosted runner.
#[test]
fn the_verification_is_of_this_repository_workflow_ref_and_commit() {
    let text = read(WORKFLOW);
    let verify = run_lines(&text)
        .into_iter()
        .find(|l| l.starts_with("gh attestation verify"))
        .expect("the verification");
    for pinned in [
        "--repo \"$REPO\"",
        "--signer-workflow \"$REPO/.github/workflows/js-agent-deb.yml\"",
        "--source-ref refs/heads/main",
        "--source-digest \"$GITHUB_SHA\"",
        "--deny-self-hosted-runners",
    ] {
        assert!(verify.contains(pinned), "{pinned} in {verify}");
    }
}

/// What the attestation covers is what the verify job checks: both jobs use the artifact the
/// build job uploaded, the attested subject is the file downloaded to `dist`, and `REPO` of the
/// verify step is this repository.
#[test]
fn the_attested_file_is_the_one_downloaded_and_verified() {
    let text = read(WORKFLOW);
    let lines: Vec<String> = text.lines().map(normal).collect();
    let values = |key: &str| -> Vec<String> {
        lines
            .iter()
            .filter_map(|l| l.strip_prefix(key).map(str::to_owned))
            .collect()
    };
    // The `name:` and `path:` of the `with:` block that follows each artifact step.
    let with_of = |action: &str| -> Vec<(String, String)> {
        let uses = format!("- uses: actions/{action}-artifact@");
        let mut found = Vec::new();
        for (at, line) in lines.iter().enumerate() {
            if line.starts_with(&uses) {
                let block = &lines[at + 1..(at + 5).min(lines.len())];
                let get = |key: &str| {
                    block
                        .iter()
                        .find_map(|l| l.strip_prefix(key))
                        .unwrap_or_default()
                        .to_owned()
                };
                found.push((get("name: "), get("path: ")));
            }
        }
        found
    };
    let uploads = with_of("upload");
    let downloads = with_of("download");
    // One upload and two downloads (attest, verify) share the one artifact name, and both
    // downloads put the deb in dist, where the attested subject and the verified file are.
    assert_eq!(uploads.len(), 1, "{uploads:?}");
    assert_eq!(downloads.len(), 2, "{downloads:?}");
    for (name, _) in uploads.iter().chain(&downloads) {
        assert_eq!(name, "js-agent-deb");
    }
    for (_, path) in &downloads {
        assert_eq!(path, "dist");
    }
    assert_eq!(
        values("subject-path: "),
        ["dist/js-agent_${{ env.VERSION }}_amd64.deb"]
    );
    assert_eq!(
        values("REPO: "),
        ["${{ github.repository }}"],
        "REPO of the verify step"
    );
    let verify = run_lines(&text)
        .into_iter()
        .find(|l| l.starts_with("gh attestation verify"))
        .expect("the verification");
    assert!(
        verify.contains("\"dist/js-agent_${VERSION}_amd64.deb\""),
        "{verify}"
    );
}
