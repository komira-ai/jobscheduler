//! The files of the js-agent package (`packaging/js-agent`) and the workflow that builds and
//! attests it (`.github/workflows/js-agent-deb.yml`), read as they are in the repository:
//! the unit restarts the agent after every exit, its directories are closed to everyone but
//! root, the nfpm configuration lists the files the checker (`xtask::pkg`) accepts, and the
//! workflow publishes nothing.

use std::path::Path;

use xtask::pkg::FILES;

fn root() -> &'static Path {
    Path::new(concat!(env!("CARGO_MANIFEST_DIR"), "/../.."))
}

fn read(path: &str) -> String {
    std::fs::read_to_string(root().join(path)).unwrap_or_else(|e| panic!("{path}: {e}"))
}

/// The lines of `text` that are not comments or blank.
fn code(text: &str) -> Vec<&str> {
    text.lines()
        .map(str::trim)
        .filter(|l| !l.is_empty() && !l.starts_with('#'))
        .collect()
}

/// The value of the one `key=` line of the unit.
fn unit_value(key: &str) -> String {
    let unit = read("packaging/js-agent/js-agent.service");
    let found: Vec<&str> = code(&unit)
        .into_iter()
        .filter_map(|l| l.strip_prefix(&format!("{key}=")))
        .collect();
    assert_eq!(found.len(), 1, "{key}: {found:?}");
    found[0].to_owned()
}

/// The value after `--flag` on the unit's command line.
fn flag(flag: &str) -> String {
    let line = unit_value("ExecStart");
    let words: Vec<&str> = line.split_whitespace().collect();
    let at = words
        .iter()
        .position(|w| *w == flag)
        .unwrap_or_else(|| panic!("{flag} in {line}"));
    words[at + 1].to_owned()
}

/// The directories of the tmpfiles file: path, mode, owner, group.
fn tmpfiles() -> Vec<(String, u32, String, String)> {
    let text = read("packaging/js-agent/tmpfiles.conf");
    code(&text)
        .into_iter()
        .map(|line| {
            let f: Vec<&str> = line.split_whitespace().collect();
            assert_eq!(f[0], "d", "{line}");
            (
                f[1].to_owned(),
                u32::from_str_radix(f[2], 8).expect("an octal mode"),
                f[3].to_owned(),
                f[4].to_owned(),
            )
        })
        .collect()
}

/// The agent is started again after every exit, the status 0 of a stop request and the status
/// 2 of a refusal included (the reason is in the unit's comments and in the agent's docs).
#[test]
fn the_unit_restarts_the_agent_after_every_exit() {
    assert_eq!(unit_value("Restart"), "always");
    let unit = read("packaging/js-agent/js-agent.service");
    assert!(
        !code(&unit)
            .iter()
            .any(|l| l.starts_with("RestartPreventExitStatus")
                || l.starts_with("SuccessExitStatus")
                || l.starts_with("RestartForceExitStatus")),
        "{unit}"
    );
    assert!(unit.contains("Restart=always, and not on-failure"));
    assert_eq!(unit_value("RestartSec"), "5");
}

/// The unit runs the binary the package installs, with its settings as flags: the agent reads
/// no environment variable and no file of settings, and the join token is never on the command
/// line (argv is world-readable), only the path of its file.
#[test]
fn the_unit_runs_the_installed_binary_with_flags_only() {
    let line = unit_value("ExecStart");
    assert!(line.starts_with("/usr/bin/js-agent "), "{line}");
    assert!(FILES.iter().any(|(p, _, _)| *p == "usr/bin/js-agent"));
    let unit = read("packaging/js-agent/js-agent.service");
    for key in ["Environment", "EnvironmentFile", "PassEnvironment"] {
        assert!(
            !code(&unit).iter().any(|l| l.starts_with(key)),
            "{key} in the unit"
        );
    }
    assert!(!line.contains("--token "), "{line}");
    for required in ["--server", "--pool", "--token-file", "--state-root"] {
        flag(required);
    }
    assert_eq!(unit_value("WantedBy"), "multi-user.target");
}

/// The unit's paths are the ones the elastic boot payload gives its agent (the engine, the
/// state root and the scratch root), so a host started either way keeps its files in one place.
#[test]
fn the_unit_uses_the_paths_of_the_elastic_agent() {
    assert_eq!(flag("--container-bin"), js_elastic::CONTAINER_BIN);
    assert_eq!(flag("--state-root"), js_elastic::STATE_ROOT);
    assert_eq!(flag("--scratch-root"), js_elastic::SCRATCH_ROOT);
    assert_eq!(unit_value("RequiresMountsFor"), js_elastic::SCRATCH_ROOT);
}

/// The state root (the taint marker, and the host credential the agent writes at mode 0600 once
/// the server admits the host), the scratch root and the directory of the join token are made,
/// each root's with no permission for group or others: a credential in a directory a user
/// other than root can enter or list is exposed.
#[test]
fn the_directories_are_roots_and_closed_to_group_and_others() {
    let dirs = tmpfiles();
    let paths: Vec<&str> = dirs.iter().map(|d| d.0.as_str()).collect();
    let token = flag("--token-file");
    let token_dir = token.rsplit_once('/').expect("a path").0;
    for needed in [
        token_dir.to_owned(),
        flag("--state-root"),
        flag("--scratch-root"),
    ] {
        assert!(paths.contains(&needed.as_str()), "{needed} in {paths:?}");
    }
    // The state root and the scratch root lie in a directory that is also made.
    assert!(paths.contains(&"/var/lib/js-agent"), "{paths:?}");
    for (path, mode, owner, group) in &dirs {
        assert_eq!(mode & 0o077, 0, "{path}: mode {mode:o}");
        assert_eq!((owner.as_str(), group.as_str()), ("root", "root"), "{path}");
    }
    // The unit gives the agent no other account: it runs as root, the owner of all of them,
    // and the agent only starts with a token file of its own user.
    let unit = read("packaging/js-agent/js-agent.service");
    assert!(!code(&unit).iter().any(|l| l.starts_with("User=")));
}

/// The nfpm configuration installs exactly the files the checker accepts, with the modes it
/// accepts, and has no maintainer script or dependency of its own.
#[test]
fn the_nfpm_configuration_lists_the_files_of_the_checker() {
    let text = read("packaging/js-agent/nfpm.yaml");
    let lines = code(&text);
    let mut listed = Vec::new();
    let mut dst = String::new();
    for line in &lines {
        if let Some(d) = line.strip_prefix("dst: ") {
            dst = d.trim_start_matches('/').to_owned();
        }
        if let Some(m) = line.strip_prefix("mode: ") {
            listed.push((dst.clone(), u32::from_str_radix(m, 8).expect("a mode")));
        }
    }
    let mut want: Vec<(String, u32)> = FILES
        .iter()
        .map(|(p, m, _)| ((*p).to_owned(), *m))
        .collect();
    want.sort();
    listed.sort();
    assert_eq!(listed, want);
    for forbidden in [
        "scripts:",
        "depends:",
        "preinstall",
        "postinstall",
        "type: dir",
    ] {
        assert!(!text.contains(forbidden), "{forbidden}");
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

const WORKFLOW: &str = ".github/workflows/js-agent-deb.yml";

/// The workflow builds, checks, uploads as a workflow artifact and attests, and does nothing
/// else with the deb: no write scope on the repository, packages or releases, and no step that
/// runs a command or an action that publishes.
#[test]
fn the_workflow_publishes_nothing() {
    let text = read(WORKFLOW);
    let steps = code(&text);
    for line in &steps {
        for banned in [
            "contents: write",
            "packages: write",
            "gh release",
            "gh api",
            "cargo publish",
            "docker push",
            "docker login",
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
            "curl ",
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
    assert!(steps.contains(&"permissions: {}"));
    assert!(text.contains("name: js-agent-deb"));
}

/// Only the job that attests holds the token and the attestations write scope, it runs no
/// repository code, and it and the verification run only for a manual run on main: a pull
/// request builds and checks the deb and attests nothing.
#[test]
fn only_the_attest_job_can_attest_and_only_on_main() {
    let text = read(WORKFLOW);
    let jobs = jobs(&text);
    let names: Vec<&str> = jobs.iter().map(|(n, _)| n.as_str()).collect();
    assert_eq!(names, ["build", "attest", "verify"]);
    let holders: Vec<&str> = jobs
        .iter()
        .filter(|(_, l)| l.iter().any(|l| l.trim() == "id-token: write"))
        .map(|(n, _)| n.as_str())
        .collect();
    assert_eq!(holders, ["attest"]);
    let writers: Vec<&str> = jobs
        .iter()
        .filter(|(_, l)| l.iter().any(|l| l.trim() == "attestations: write"))
        .map(|(n, _)| n.as_str())
        .collect();
    assert_eq!(writers, ["attest"]);
    let gate =
        "if: ${{ github.event_name == 'workflow_dispatch' && github.ref == 'refs/heads/main' }}";
    for name in ["attest", "verify"] {
        let job = jobs
            .iter()
            .find(|(n, _)| n == name)
            .expect("a job")
            .1
            .join("\n");
        assert!(job.contains(gate), "{name}");
        assert!(
            !job.contains("actions/checkout") && !job.contains("cargo "),
            "{name} runs no repository code"
        );
    }
    let build = jobs[0].1.join("\n");
    assert!(build.contains("permissions:\n      contents: read\n    steps:"));
    assert!(build.contains("cargo xtask pkg check-deb"));
    assert!(build.contains("nfpm package"));
    assert!(build.contains("--sha256 \"$NFPM_SHA256_X86_64_LINUX\""));
    assert!(text.contains("subject-path: dist/js-agent_*_amd64.deb"));
}
