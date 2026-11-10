//! The install check of `release.yml` ([`super::check_apt`]): the file passes, and each way of
//! weakening it, running it on a runner or in an image it should not, handing it something private
//! or letting another job run its verb is named.

use super::check_apt::check_apt_problems;
use super::support::{each_named, text_of};
use super::yaml;

const IMAGE: &str =
    "ubuntu@sha256:fe81560990fae86dc4743202b2cb4d3df98d0bcf949173cecef15ce1127ef852";

fn problems_of(text: &str) -> Vec<String> {
    match yaml::load(text) {
        Ok(doc) => check_apt_problems(&doc),
        Err(e) => vec![e],
    }
}

#[test]
fn the_install_check_of_release_yml_passes() {
    // Catches: the check job lost or renamed, which the defects below would not notice.
    assert_eq!(problems_of(&text_of("release.yml")), Vec::<String>::new());
}

/// `(job, text to replace once in it, its replacement, what the problem must say)`.
const DEFECTS: &[(&str, &str, &str, &str)] = &[
    // needs and the condition
    (
        "check-apt",
        "needs: [plan, testing-apt-write, testing-apt-write-2, testing-apt-write-3]",
        "needs: [plan, testing-apt-write]",
        "check-apt: needs are not",
    ),
    (
        "check-apt",
        "!cancelled() && !failure() && ",
        "",
        "check-apt: `if` is not",
    ),
    (
        "check-apt",
        "github.event_name == 'push' && github.ref == 'refs/heads/main' && ",
        "",
        "check-apt: `if` is not",
    ),
    (
        "check-apt",
        " || needs.testing-apt-write-3.outputs.conflict == 'false'",
        "",
        "check-apt: `if` is not",
    ),
    // nothing private
    (
        "check-apt",
        "    timeout-minutes: 30\n",
        "    timeout-minutes: 30\n    environment: apt-publish\n",
        "check-apt: names an environment",
    ),
    (
        "check-apt",
        "      contents: read\n",
        "      contents: write\n",
        "check-apt: permissions are [\"contents: write\"], not contents: read alone",
    ),
    (
        "check-apt",
        "      contents: read\n",
        "      contents: read\n      id-token: write\n",
        "check-apt: names id-token; this job holds no secret",
    ),
    (
        "check-apt",
        "          ARCH: ${{ matrix.arch }}\n",
        "          ARCH: ${{ matrix.arch }}\n          TOKEN: ${{ secrets.ANYTHING }}\n",
        "check-apt: names secrets.; this job holds no secret",
    ),
    (
        "check-apt",
        "          ARCH: ${{ matrix.arch }}\n",
        "          ARCH: ${{ matrix.arch }}\n          X: ${{ vars.APT_BUCKET }}\n",
        "check-apt: names vars.; this job holds no secret",
    ),
    (
        "check-apt",
        "          ARCH: ${{ matrix.arch }}\n",
        "          ARCH: ${{ matrix.arch }}\n          GH_TOKEN: x\n",
        "check-apt: names GH_TOKEN; this job holds no secret",
    ),
    // the container
    (
        "check-apt",
        IMAGE,
        "ubuntu:24.04",
        "check-apt: the container image is not ubuntu@sha256:<digest>, pinned by digest",
    ),
    (
        "check-apt",
        "/usr/local/bin/xtask:ro",
        "/usr/local/bin/xtask",
        "check-apt: the xtask binary is not mounted read-only",
    ),
    (
        "check-apt",
        "docker run --rm ",
        "docker run --rm --privileged ",
        "check-apt: the container is given more than a clean Ubuntu has",
    ),
    (
        "check-apt",
        "docker run",
        "echo",
        "check-apt: it needs one step that runs `docker run`",
    ),
    (
        "check-apt",
        "      - name: Compile xtask\n        run: cargo xtask --help\n",
        "",
        "check-apt: xtask is not compiled (`cargo xtask --help`) before the container runs",
    ),
    // the matrix
    (
        "check-apt",
        "            runner: ubuntu-24.04-arm\n",
        "            runner: ubuntu-24.04\n",
        "check-apt: the matrix does not run on ubuntu-24.04-arm: both architectures are checked",
    ),
    (
        "check-apt",
        "          - arch: arm64\n",
        "          - arch: s390x\n",
        "check-apt: the matrix has no arm64",
    ),
    (
        "check-apt",
        "runs-on: ${{ matrix.runner }}",
        "runs-on: ubuntu-24.04",
        "check-apt: runs-on is not ${{ matrix.runner }}",
    ),
    // what the check is given
    (
        "check-apt",
        "xtask channels check-apt --arch \"$ARCH\" ",
        "xtask channels check-apt ",
        "check-apt: the check is not given xtask channels check-apt --arch \"$ARCH\"",
    ),
    (
        "check-apt",
        "--version \"$VERSION\"",
        "--version 0.0.0",
        "check-apt: the check is not given --version \"$VERSION\"",
    ),
    (
        "check-apt",
        "--version \"$VERSION\"",
        "--version \"$VERSION\" --commit \"$COMMIT\"",
        "check-apt: the check is given --commit",
    ),
    (
        "check-apt",
        "          ARCH: ${{ matrix.arch }}\n",
        "          ARCH: amd64\n",
        "check-apt: ARCH is not the leg's ${{ matrix.arch }}",
    ),
    (
        "check-apt",
        "      VERSION: ${{ needs.plan.outputs.version }}\n",
        "      VERSION: 0.0.0\n",
        "check-apt: VERSION is not the plan's ${{ needs.plan.outputs.version }}",
    ),
    // the verb and docker are check-apt's alone
    (
        "plan",
        "      - run: rustup toolchain install\n",
        "      - run: docker run --rm ubuntu true\n      - run: rustup toolchain install\n",
        "plan: runs `channels check-apt` or docker; only check-apt may",
    ),
    (
        "publish",
        "      - name: Draft the release, add the files it lacks, publish it\n",
        "      - run: cargo xtask channels check-apt --arch amd64 --version 0.0.0\n      - name: Draft the release, add the files it lacks, publish it\n",
        "publish: runs `channels check-apt` or docker; only check-apt may",
    ),
    (
        "gamma",
        "    permissions:\n      id-token: write\n",
        "    container: ubuntu:24.04\n    permissions:\n      id-token: write\n",
        "gamma: its container image is not pinned by digest (image@sha256:<64 hex digits>)",
    ),
];

#[test]
fn each_defect_in_the_install_check_is_named_by_the_lint() {
    // Catches: a pin of the install check that holds nothing, because the file passes whether or
    // not the thing it pins is there.
    each_named("release.yml", problems_of, DEFECTS);
}

#[test]
fn a_missing_install_check_and_a_container_image_by_name_are_named() {
    // Catches: the job deleted (nothing then gates the promotion on an install), and an image
    // pinned in a job's `container:` by tag or by digest in either of the two spellings.
    assert_eq!(
        check_apt_problems(&yaml::load("jobs: {}\n").expect("parses")),
        ["check-apt: there is no such job"]
    );
    let by_digest = format!(
        "jobs:\n  a:\n    container: {IMAGE}\n  b:\n    container:\n      image: {IMAGE}\n"
    );
    assert_eq!(
        check_apt_problems(&yaml::load(&by_digest).expect("parses")),
        ["check-apt: there is no such job"]
    );
    let by_tag = "jobs:\n  a:\n    container:\n      image: ubuntu:24.04\n  b:\n    container: ubuntu@sha256:abc\n";
    let problems = check_apt_problems(&yaml::load(by_tag).expect("parses"));
    assert_eq!(
        problems
            .iter()
            .filter(|p| p.ends_with(
                "its container image is not pinned by digest (image@sha256:<64 hex digits>)"
            ))
            .count(),
        2,
        "{problems:?}"
    );
    // A job that is nothing like it: no steps, no matrix.
    let bare = check_apt_problems(&yaml::load("jobs:\n  check-apt: {}\n").expect("parses"));
    assert!(
        bare.contains(&"check-apt: it needs one step that runs `docker run`".to_owned()),
        "{bare:?}"
    );
}
