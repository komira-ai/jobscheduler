//! The apt jobs of release.yml as the text of the file shows them: which jobs there are, that each
//! waits for the plan and runs only on its release, that the promotion waits for the install
//! check, and which of them hold `id-token: write`. The shape of each job is `tests/apt_workflow`.

use super::{artifacts_workflow, holders, job, jobs, workflow};

/// The jobs of the apt repository in `text`, in file order: the read, index and write jobs of both
/// suites in every attempt, and `check-apt`.
pub fn apt_jobs(text: &str) -> Vec<String> {
    jobs(text)
        .into_iter()
        .map(|(name, _)| name)
        .filter(|name| name.contains("-apt-") || name == "check-apt")
        .collect()
}

/// Asserts that the apt repository has its eighteen jobs of the two suites and `check-apt`, each
/// after the plan and gated on the plan's release, and that `check-apt` and the promotion wait
/// for the install check: the first attempt's stable jobs need `check-apt` and run only when it
/// succeeded, and `check-apt` waits for every attempt of the testing write and nothing of the
/// testing suite waits for it.
#[test]
fn the_apt_jobs_are_gated_on_the_release_and_the_promotion_on_the_install_check() {
    let text = workflow();
    let apt = apt_jobs(&text);
    assert_eq!(apt.len(), 19, "{apt:?}");
    let all = jobs(&text);
    for name in &apt {
        let lines = job(&all, name);
        assert!(
            lines.iter().any(|l| l.starts_with("    needs: [plan")),
            "{name}"
        );
        let condition = lines
            .iter()
            .find(|l| l.starts_with("    if:"))
            .unwrap_or_else(|| panic!("{name} has no if"));
        assert!(
            condition.contains("needs.plan.outputs.release == 'true'"),
            "{name}: {condition}"
        );
    }
    for name in ["stable-apt-read", "stable-apt-index", "stable-apt-write"] {
        let lines = job(&all, name);
        assert!(
            lines
                .iter()
                .any(|l| l.starts_with("    needs: [plan, check-apt,")),
            "{name}"
        );
        assert!(
            lines
                .iter()
                .any(|l| l.contains("needs.check-apt.result == 'success'")),
            "{name}"
        );
    }
    let check = job(&all, "check-apt").join("\n");
    assert!(
        check
            .contains("needs: [plan, testing-apt-write, testing-apt-write-2, testing-apt-write-3]"),
        "{check}"
    );
    for name in apt.iter().filter(|n| n.starts_with("testing-")) {
        let code: Vec<&String> = job(&all, name)
            .iter()
            .filter(|l| !l.trim_start().starts_with('#'))
            .collect();
        assert!(!code.iter().any(|l| l.contains("check-apt")), "{name}");
    }
}

/// Asserts that in release.yml `id-token: write` is held by `artifacts` (the call, whose grant
/// is the most its jobs can hold), `gamma`, `cp-deploy` (the call of docker-swarm-updater's
/// deploy workflow) and the apt jobs that read and write (never the index jobs, which hold the
/// signing key, nor `check-apt`), and `attestations: write` by `artifacts` alone; and that in
/// artifacts.yml both are held by `attest` and by no other job.
#[test]
fn only_attest_gamma_cp_deploy_and_the_apt_read_and_write_jobs_hold_id_token() {
    let release = workflow();
    let mut want = vec![
        "artifacts".to_owned(),
        "gamma".to_owned(),
        "cp-deploy".to_owned(),
    ];
    want.extend(
        apt_jobs(&release)
            .into_iter()
            .filter(|name| name.contains("-apt-read") || name.contains("-apt-write")),
    );
    assert_eq!(want.len(), 15, "{want:?}");
    assert_eq!(holders(&release, "id-token: write"), want);
    assert_eq!(holders(&release, "attestations: write"), ["artifacts"]);
    let artifacts = artifacts_workflow();
    assert_eq!(holders(&artifacts, "id-token: write"), ["attest"]);
    assert_eq!(holders(&artifacts, "attestations: write"), ["attest"]);
    assert!(holders(&artifacts, "contents: write").is_empty());
}
