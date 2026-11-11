//! `release desired`: the choice of the release, what it refuses and the targets it writes,
//! run against stand-ins for `gh` and `curl` (small scripts in a scratch directory, which log
//! their command lines and answer from the text the test gives). No network is used and the
//! real programs are never started.

use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;

use js_testkit::dir::TestDir;
use serde_json::Value;

use super::manifest::{PARAMS, Param};
use super::repo::{Listed, listed, sums_digest};
use super::*;

const AMD64: &str = "1111111111111111111111111111111111111111111111111111111111111111";
const ARM64: &str = "2222222222222222222222222222222222222222222222222222222222222222";
const OTHER: &str = "3333333333333333333333333333333333333333333333333333333333333333";

/// Held by each test from its first script to its last run: a script that is being written is
/// a file open for writing, and a program started meanwhile by another test of this process
/// would carry that descriptor into its child, which makes the script's own start fail with
/// "text file busy". Tests that write scripts run one at a time.
static SERIAL: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// Waits for the turn of one test (a failed test does not stop the others).
pub(crate) fn serial() -> std::sync::MutexGuard<'static, ()> {
    SERIAL
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// A scratch directory of its own for one test.
pub(crate) fn scratch(name: &str) -> TestDir {
    TestDir::under(std::env::temp_dir(), name)
}

/// Writes the executable `name` of `dir`, a shell script with `body`.
pub(crate) fn script(dir: &TestDir, name: &str, body: &str) -> String {
    let path = dir.join(name);
    std::fs::write(&path, format!("#!/bin/sh\n{body}")).expect("script");
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).expect("mode");
    path.display().to_string()
}

/// Shell that prints `text` as it is.
pub(crate) fn say(text: &str) -> String {
    format!("cat <<'EOF'\n{text}\nEOF\n")
}

/// A stand-in `gh` that logs its command line to `gh.log` and runs the shell of the case whose
/// arguments are exactly `$*`, and fails (status 9) on any other call.
pub(crate) fn gh(dir: &TestDir, cases: &[(String, String)]) -> String {
    let mut body = format!(
        "echo \"$*\" >> '{}'\ncase \"$*\" in\n",
        dir.join("gh.log").display()
    );
    for (args, shell) in cases {
        body.push_str(&format!("'{args}')\n{shell};;\n"));
    }
    body.push_str("*) echo \"unexpected: $*\" >&2; exit 9;;\nesac\n");
    script(dir, "gh", &body)
}

/// What a stand-in `curl` does for an architecture's URL.
enum Index<'a> {
    /// Writes this `Packages` text to the `--output` file.
    Text(&'a str),
    /// Fails as `curl --fail` does for a 404.
    Missing,
}

/// A stand-in `curl` that logs its command line to `curl.log` and answers each architecture's
/// `Packages` URL as the test says.
fn curl(dir: &TestDir, amd64: &Index<'_>, arm64: &Index<'_>) -> String {
    let case = |arch: &str, index: &Index<'_>| match index {
        Index::Text(text) => {
            format!("*/binary-{arch}/Packages)\ncat > \"$out\" <<'EOF'\n{text}\nEOF\n;;\n")
        }
        Index::Missing => {
            format!("*/binary-{arch}/Packages) echo 'curl: (22) 404' >&2; exit 22;;\n")
        }
    };
    let body = format!(
        "echo \"$*\" >> '{}'\nfor last; do :; done\nwhile [ $# -gt 0 ]; do\n\
         [ \"$1\" = --output ] && out=$2\nshift\ndone\ncase \"$last\" in\n{}{}esac\n",
        dir.join("curl.log").display(),
        case("amd64", amd64),
        case("arm64", arm64),
    );
    script(dir, "curl", &body)
}

/// The text of a log (empty when the program was never run).
pub(crate) fn log(dir: &TestDir, name: &str) -> String {
    std::fs::read_to_string(dir.join(name)).unwrap_or_default()
}

/// A `Packages` file with a stanza for `filename` at `sha256`, after another package's.
fn packages(filename: &str, sha256: &str) -> String {
    format!(
        "Package: other\nFilename: pool/jobscheduler/other_1.0.0_amd64.deb\nSHA256: {OTHER}\n\n\
         Package: js-agent\nVersion: 1.2.3\nFilename: {filename}\nSize: 10\nSHA256: {sha256}\n"
    )
}

/// The release JSON of a published release holding `assets`.
pub(crate) fn release(assets: &[&str]) -> String {
    let assets: Vec<String> = assets
        .iter()
        .map(|a| format!("{{\"name\":\"{a}\"}}"))
        .collect();
    format!(
        "{{\"draft\":false,\"prerelease\":false,\"assets\":[{}]}}",
        assets.join(",")
    )
}

/// The `SHA256SUMS` of a release of 1.2.3 that lists both `.deb`s at `AMD64` and `ARM64`.
fn sums() -> String {
    format!(
        "{AMD64}  js-agent_1.2.3_amd64.deb\n{ARM64} *js-agent_1.2.3_arm64.deb\n{OTHER}  js-agent-1.2.3-linux.tar.gz\n"
    )
}

/// The gh cases of a published release 1.2.3 that holds `SHA256SUMS`, which download writes
/// `sums` for.
fn release_cases(dir: &TestDir, sums: &str) -> Vec<(String, String)> {
    vec![
        (
            "api repos/o/r/releases/tags/v1.2.3".to_owned(),
            say(&release(&["SHA256SUMS", "js-agent_1.2.3_amd64.deb"])),
        ),
        (
            format!(
                "release download v1.2.3 --repo o/r --dir {} --pattern SHA256SUMS",
                dir.join("d").display()
            ),
            format!(
                "cat > '{}' <<'EOF'\n{sums}EOF\n",
                dir.join("d/SHA256SUMS").display()
            ),
        ),
    ]
}

/// The arguments of a run by version in `dir`, with the stand-ins.
fn args(dir: &TestDir, gh: String, curl: String) -> DesiredArgs {
    DesiredArgs {
        repo: "o/r".to_owned(),
        version: "1.2.3".to_owned(),
        after_run: String::new(),
        dir: dir.join("d"),
        output: Some(dir.join("out")),
        apt_url: "https://apt.example/apt".to_owned(),
        gh,
        curl,
    }
}

/// The two stand-ins for a release whose stable suite lists both `.deb`s as `SHA256SUMS` does.
fn consistent(dir: &TestDir) -> DesiredArgs {
    let gh = gh(dir, &release_cases(dir, &sums()));
    let amd64 = packages("pool/jobscheduler/js-agent_1.2.3_amd64.deb", AMD64);
    let arm64 = packages("pool/jobscheduler/js-agent_1.2.3_arm64.deb", ARM64);
    let curl = curl(dir, &Index::Text(&amd64), &Index::Text(&arm64));
    args(dir, gh, curl)
}

/// The refusal's lines.
fn refused(result: Result<String, Error>) -> Vec<String> {
    match result {
        Err(Error::Refused(lines)) => lines,
        other => panic!("expected a refusal, got {other:?}"),
    }
}

/// The `targets` value the verb writes for 1.2.3 with the digests of the two packages: the
/// whole JSON, written out. Hostkeeper's `Target` in proto3 canonical JSON (lowerCamel names,
/// 64-bit integers as strings), one `apt` entry of the stable repository per platform.
fn golden() -> String {
    let entry = |digest: &str| {
        format!(
            "{{\"entries\":[{{\"digest\":\"{digest}\",\"installer\":\"apt\",\"manifest\":{{\"hook\":{{\"cleanupRetries\":3,\"command\":[\"/usr/bin/js-agent\",\"update-hook\"],\"timeoutSecs\":{{\"cleanup\":\"120\",\"cutover\":\"4320\",\"prepare\":\"3900\",\"rollback\":\"8220\",\"verify\":\"615\"}},\"verifyDeadlineSecs\":\"9000\"}},\"installerBounds\":{{\"activateSecs\":\"300\",\"removeSecs\":\"300\",\"stageSecs\":\"600\"}},\"minHostkeeper\":\"0.1.225\",\"package\":\"js-agent\",\"params\":{{\"cleanup_timeout_secs\":{{\"integer\":\"120\"}},\"cordon_lease_secs\":{{\"integer\":\"300\"}},\"cordon_max_secs\":{{\"integer\":\"17355\"}},\"cutover_timeout_secs\":{{\"integer\":\"4320\"}},\"defer_retry_secs\":{{\"integer\":\"300\"}},\"drain_wait_secs\":{{\"integer\":\"3600\"}},\"flags_file\":{{\"text\":\"/etc/js-agent/flags\"}},\"prepare_timeout_secs\":{{\"integer\":\"3900\"}},\"release_link\":{{\"text\":\"/var/lib/hostkeeper/packages/js-agent/current\"}},\"rollback_timeout_secs\":{{\"integer\":\"8220\"}},\"runtime\":{{\"text\":\"crun\"}},\"state_dir\":{{\"text\":\"/var/lib/js-agent-update\"}},\"state_root\":{{\"text\":\"/var/lib/js-agent/state\"}},\"token_file\":{{\"text\":\"/etc/js-agent/token\"}},\"unit\":{{\"text\":\"js-agent-crun.service\"}},\"verify_deadline_secs\":{{\"integer\":\"9000\"}},\"verify_timeout_secs\":{{\"integer\":\"615\"}}}},\"run\":[\"js-agent-crun.service\"]}},\"packageVersion\":\"1.2.3\",\"repository\":\"komira-stable\"}}]}}"
        )
    };
    format!(
        "{{\"linux-arm64\":{},\"linux-x86_64\":{}}}",
        entry(ARM64),
        entry(AMD64)
    )
}

/// Asserts a run by version: the outputs are `set=true`, the version and the targets, exactly
/// the golden JSON; the printed text is the summary and the same lines; and `gh` and `curl`
/// ran exactly the calls the design says, each architecture's `Packages` read over HTTPS with
/// the pinned flags.
#[test]
fn a_release_the_stable_suite_lists_is_set_with_its_targets() {
    let _serial = serial();
    let dir = scratch("desired-version");
    let args = consistent(&dir);
    let printed = run(&args).expect("the run passes");
    let lines = format!("set=true\npackage-version=1.2.3\ntargets={}\n", golden());
    assert_eq!(
        std::fs::read_to_string(dir.join("out")).expect("output"),
        lines
    );
    assert_eq!(
        printed,
        format!(
            "release v1.2.3: the stable suite lists js-agent 1.2.3 for each of 2 architectures, each with the digest SHA256SUMS lists\n{}",
            lines.trim_end()
        )
    );
    assert_eq!(
        log(&dir, "gh.log"),
        format!(
            "api repos/o/r/releases/tags/v1.2.3\nrelease download v1.2.3 --repo o/r --dir {} --pattern SHA256SUMS\n",
            dir.join("d").display()
        )
    );
    let flags = "--proto =https --tlsv1.2 --fail --silent --show-error --retry 5 --retry-delay 20 --retry-all-errors --max-time 60 --output";
    let d = dir.join("d");
    assert_eq!(
        log(&dir, "curl.log"),
        format!(
            "{flags} {0}/Packages-arm64 https://apt.example/apt/dists/stable/main/binary-arm64/Packages\n\
             {flags} {0}/Packages-amd64 https://apt.example/apt/dists/stable/main/binary-amd64/Packages\n",
            d.display()
        )
    );
}

/// Asserts that the output is appended to what the file holds (Actions' `GITHUB_OUTPUT` has
/// other steps' lines) and that no `--output` still prints the lines.
#[test]
fn the_outputs_are_appended_and_optional() {
    let _serial = serial();
    let dir = scratch("desired-append");
    let mut args = consistent(&dir);
    std::fs::write(dir.join("out"), "earlier=1\n").expect("file");
    run(&args).expect("passes");
    assert!(
        std::fs::read_to_string(dir.join("out"))
            .expect("output")
            .starts_with("earlier=1\nset=true\n")
    );
    args.output = None;
    let printed = run(&args).expect("passes");
    assert!(
        printed.contains("\nset=true\npackage-version=1.2.3\ntargets={"),
        "{printed}"
    );
}

/// Asserts the choice after a run: the job `publish` succeeded, so the newest published release
/// is set: the highest version by number (1.2.10 over 1.2.9), skipping a draft, a pre-release
/// and a tag that is not a version, then checked like any release.
#[test]
fn after_a_run_that_published_the_newest_release_is_set() {
    let _serial = serial();
    let dir = scratch("desired-after-run");
    let mut args = consistent(&dir);
    args.version = String::new();
    args.after_run = "77".to_owned();
    // The version-1.2.3 cases serve the release `newest` picks, so it is 1.2.3 here.
    let mut cases = release_cases(&dir, &sums());
    cases.push((
        "api repos/o/r/actions/runs/77/jobs?per_page=100&filter=latest".to_owned(),
        say("{\"total_count\":3,\"jobs\":[{\"name\":\"plan\",\"conclusion\":\"success\"},{\"name\":\"publish\",\"conclusion\":\"failure\"},{\"name\":\"publish\",\"conclusion\":\"success\"}]}"),
    ));
    cases.push((
        "api repos/o/r/releases?per_page=100".to_owned(),
        say("[{\"tag_name\":\"v1.2.9\",\"draft\":false,\"prerelease\":false},{\"tag_name\":\"v1.2.3\",\"draft\":false,\"prerelease\":false},{\"tag_name\":\"v9.0.0\",\"draft\":true,\"prerelease\":false},{\"tag_name\":\"v8.0.0\",\"draft\":false,\"prerelease\":true},{\"tag_name\":\"nightly\",\"draft\":false,\"prerelease\":false}]"),
    ));
    args.gh = gh(&dir, &cases);
    let error = run(&args).expect_err("1.2.9 is the newest and has no cases here");
    assert!(
        matches!(error, Error::Release(super::super::Error::Failed { .. })),
        "{error:?}"
    );
    let calls = log(&dir, "gh.log");
    assert!(calls.contains("runs/77/jobs?per_page=100&filter=latest\napi repos/o/r/releases?per_page=100\napi repos/o/r/releases/tags/v1.2.9\n"), "{calls}");
    // With the newest listed as 1.2.3 the whole run passes.
    let mut cases = release_cases(&dir, &sums());
    cases.push((
        "api repos/o/r/actions/runs/77/jobs?per_page=100&filter=latest".to_owned(),
        say("{\"total_count\":1,\"jobs\":[{\"name\":\"publish\",\"conclusion\":\"success\"}]}"),
    ));
    cases.push((
        "api repos/o/r/releases?per_page=100".to_owned(),
        say("[{\"tag_name\":\"v1.2.3\",\"draft\":false,\"prerelease\":false},{\"tag_name\":\"v1.2.2\",\"draft\":false,\"prerelease\":false}]"),
    ));
    args.gh = gh(&dir, &cases);
    let printed = run(&args).expect("passes");
    assert!(
        printed.contains("set=true\npackage-version=1.2.3\n"),
        "{printed}"
    );
}

/// Asserts that when the job `publish` did not succeed (skipped, failed or absent) nothing is
/// set: `set=false` is appended and printed, the verb succeeds, and neither the releases nor
/// `curl` are asked.
#[test]
fn after_a_run_that_published_nothing_nothing_is_set() {
    let _serial = serial();
    for jobs in [
        "{\"total_count\":2,\"jobs\":[{\"name\":\"plan\",\"conclusion\":\"success\"},{\"name\":\"publish\",\"conclusion\":\"skipped\"}]}",
        "{\"total_count\":1,\"jobs\":[{\"name\":\"publish\",\"conclusion\":\"failure\"}]}",
        "{\"total_count\":0,\"jobs\":[]}",
    ] {
        let dir = scratch("desired-nothing");
        let mut args = consistent(&dir);
        args.version = String::new();
        args.after_run = "77".to_owned();
        args.gh = gh(
            &dir,
            &[(
                "api repos/o/r/actions/runs/77/jobs?per_page=100&filter=latest".to_owned(),
                say(jobs),
            )],
        );
        let printed = run(&args).expect("a run that published nothing is not a failure");
        assert_eq!(
            printed,
            "run 77 of o/r did not publish a release (publish did not succeed): nothing is set\nset=false"
        );
        assert_eq!(
            std::fs::read_to_string(dir.join("out")).expect("output"),
            "set=false\n"
        );
        assert_eq!(log(&dir, "gh.log").lines().count(), 1);
        assert_eq!(log(&dir, "curl.log"), "");
        // Without `--output` the same is printed and no file is made.
        args.output = None;
        let again = run(&args).expect("passes");
        assert_eq!(again, printed);
    }
}

/// Asserts that a run listing more jobs than one page (or fewer than it says) is refused, not
/// half read, and that a run or release list `gh` cannot answer is the program's failure.
#[test]
fn a_job_listing_of_more_than_one_page_is_refused() {
    let _serial = serial();
    let dir = scratch("desired-pages");
    let mut args = consistent(&dir);
    args.version = String::new();
    args.after_run = "77".to_owned();
    args.gh = gh(
        &dir,
        &[(
            "api repos/o/r/actions/runs/77/jobs?per_page=100&filter=latest".to_owned(),
            say("{\"total_count\":101,\"jobs\":[{\"name\":\"plan\"}]}"),
        )],
    );
    assert_eq!(
        run(&args),
        Err(Error::Failed(
            "run 77 of o/r has 101 jobs; one page of 1 was read".to_owned()
        ))
    );
}

/// Asserts that a repository with no published release version has nothing to set.
#[test]
fn a_repository_with_no_published_release_is_refused() {
    let _serial = serial();
    let dir = scratch("desired-none");
    let mut args = consistent(&dir);
    args.version = String::new();
    args.after_run = "5".to_owned();
    args.gh = gh(
        &dir,
        &[
            (
                "api repos/o/r/actions/runs/5/jobs?per_page=100&filter=latest".to_owned(),
                say(
                    "{\"total_count\":1,\"jobs\":[{\"name\":\"publish\",\"conclusion\":\"success\"}]}",
                ),
            ),
            (
                "api repos/o/r/releases?per_page=100".to_owned(),
                say(
                    "[{\"tag_name\":\"v1.0.0\",\"draft\":true,\"prerelease\":false},{\"tag_name\":\"v1.0.1\",\"draft\":false,\"prerelease\":true},{}]",
                ),
            ),
        ],
    );
    assert_eq!(
        run(&args),
        Err(Error::Failed(
            "o/r has no published release vX.Y.Z".to_owned()
        ))
    );
}

/// Asserts the usage errors: both or neither of `--version` and `--after-run`, a version that
/// is not three numbers, a run id that is not digits and an apt URL that is not https or ends
/// in a slash; none runs `gh`.
#[test]
fn flags_that_do_not_fit_together_are_usage_errors() {
    let _serial = serial();
    let dir = scratch("desired-usage");
    let ok = consistent(&dir);
    let with = |version: &str, after_run: &str, apt_url: &str| DesiredArgs {
        version: version.to_owned(),
        after_run: after_run.to_owned(),
        apt_url: apt_url.to_owned(),
        gh: ok.gh.clone(),
        curl: ok.curl.clone(),
        repo: ok.repo.clone(),
        dir: ok.dir.clone(),
        output: None,
    };
    let url = "https://apt.example/apt";
    let both = "give one of --version and --after-run, not both and not neither";
    assert_eq!(
        run(&with("1.2.3", "5", url)),
        Err(Error::Usage(both.to_owned()))
    );
    assert_eq!(run(&with("", "", url)), Err(Error::Usage(both.to_owned())));
    assert_eq!(
        run(&with("1.2", "", url)),
        Err(Error::Usage(
            "--version \"1.2\" is not <major>.<minor>.<patch>, a release tag without its v"
                .to_owned()
        ))
    );
    assert_eq!(
        run(&with("", "12a", url)),
        Err(Error::Usage(
            "--after-run \"12a\" is not the id of a workflow run".to_owned()
        ))
    );
    for bad in ["http://apt.example/apt", "https://apt.example/apt/"] {
        assert_eq!(
            run(&with("1.2.3", "", bad)),
            Err(Error::Usage(format!(
                "--apt-url {bad:?} is not an https URL without a trailing /"
            )))
        );
    }
    assert_eq!(log(&dir, "gh.log"), "");
}

/// Asserts that a draft and a pre-release are not a released version, even when named by
/// `--version`, and that a release without `SHA256SUMS` is refused naming it.
#[test]
fn a_draft_a_prerelease_and_a_release_without_sums_are_refused() {
    let _serial = serial();
    for flag in ["draft", "prerelease"] {
        let dir = scratch("desired-draft");
        let mut args = consistent(&dir);
        let body = format!("{{\"{flag}\":true,\"assets\":[]}}");
        args.gh = gh(
            &dir,
            &[("api repos/o/r/releases/tags/v1.2.3".to_owned(), say(&body))],
        );
        assert_eq!(
            run(&args),
            Err(Error::Failed(
                "release v1.2.3 of o/r is a draft or a pre-release, not a released version"
                    .to_owned()
            ))
        );
    }
    let dir = scratch("desired-nosums");
    let mut args = consistent(&dir);
    args.gh = gh(
        &dir,
        &[(
            "api repos/o/r/releases/tags/v1.2.3".to_owned(),
            say(&release(&["js-agent_1.2.3_amd64.deb"])),
        )],
    );
    assert_eq!(
        refused(run(&args)),
        ["release v1.2.3 has no SHA256SUMS: there are no digests to hold the stable suite to"]
    );
    assert_eq!(log(&dir, "curl.log"), "");
}

/// Asserts that `SHA256SUMS` that does not list a `.deb` is refused naming it, and that the
/// other architecture is still held to the suite (every problem is reported at once).
#[test]
fn sums_that_miss_a_deb_are_refused_by_name() {
    let _serial = serial();
    let dir = scratch("desired-missing-deb");
    let mut args = consistent(&dir);
    let only_amd64 = format!("{AMD64}  js-agent_1.2.3_amd64.deb\n");
    let mut cases = release_cases(&dir, &only_amd64);
    args.gh = gh(&dir, &cases);
    assert_eq!(
        refused(run(&args)),
        ["SHA256SUMS of release v1.2.3 lists no js-agent_1.2.3_arm64.deb"]
    );
    assert_eq!(log(&dir, "curl.log").lines().count(), 1);
    // Neither listed.
    cases = release_cases(&dir, "");
    args.gh = gh(&dir, &cases);
    assert_eq!(refused(run(&args)).len(), 2);
}

/// Asserts the refusals about the stable index, all in one run: an index with no stanza for the
/// file (the release was not promoted), a stanza with another SHA-256, a stanza with none and an
/// index that cannot be read; a stanza of the same name in another directory does not count.
#[test]
fn a_stable_index_that_does_not_list_the_deb_at_the_sums_digest_is_refused() {
    let _serial = serial();
    let dir = scratch("desired-index");
    let mut args = consistent(&dir);
    let amd64 = packages("pool/jobscheduler/js-agent_1.2.3_amd64.deb", OTHER);
    args.curl = curl(&dir, &Index::Text(&amd64), &Index::Missing);
    assert_eq!(
        refused(run(&args)),
        [
            "https://apt.example/apt/dists/stable/main/binary-arm64/Packages could not be read: curl: (22) 404".to_owned(),
            format!("the stable suite lists js-agent_1.2.3_amd64.deb with sha256 {OTHER}, but SHA256SUMS lists {AMD64}"),
        ]
    );
    let elsewhere = packages("pool/other/js-agent_1.2.3_amd64.deb", AMD64);
    let no_field = "Package: js-agent\nFilename: pool/jobscheduler/js-agent_1.2.3_arm64.deb\n";
    args.curl = curl(&dir, &Index::Text(&elsewhere), &Index::Text(no_field));
    assert_eq!(
        refused(run(&args)),
        [
            "the stable suite lists js-agent_1.2.3_arm64.deb with no SHA256 field",
            "the stable suite lists no js-agent_1.2.3_amd64.deb: the release is not promoted there",
        ]
    );
    assert_eq!(
        std::fs::read_to_string(dir.join("out")).unwrap_or_default(),
        ""
    );
}

/// Asserts the failures of the programs: a `gh` that is not there is a spawn failure naming it,
/// one that fails carries its stderr, one that prints no JSON is named, a `SHA256SUMS` line that
/// is not a digest and a name is refused by its number, and a `curl` that is not there is a
/// failure, not a problem of the index.
#[test]
fn programs_that_fail_are_reported_with_their_reason() {
    let _serial = serial();
    let dir = scratch("desired-programs");
    let mut args = consistent(&dir);
    let missing = dir.join("absent").display().to_string();
    args.gh = missing.clone();
    assert!(matches!(
        run(&args),
        Err(Error::Release(super::super::Error::Spawn { program, .. })) if program == missing
    ));
    args.gh = gh(&dir, &[]);
    let Err(Error::Release(super::super::Error::Failed { stderr, .. })) = run(&args) else {
        panic!("an unexpected call must fail");
    };
    assert_eq!(stderr, "unexpected: api repos/o/r/releases/tags/v1.2.3");
    args.gh = gh(
        &dir,
        &[(
            "api repos/o/r/releases/tags/v1.2.3".to_owned(),
            say("not json"),
        )],
    );
    let Err(Error::Failed(why)) = run(&args) else {
        panic!("text that is not JSON must fail");
    };
    assert!(
        why.starts_with("gh api repos/o/r/releases/tags/v1.2.3 did not print JSON: "),
        "{why}"
    );
    args.gh = gh(
        &dir,
        &[(
            "api repos/o/r/releases/tags/v1.2.3".to_owned(),
            "printf '\\377'".to_owned(),
        )],
    );
    let Err(Error::Release(super::super::Error::NotUtf8 { command })) = run(&args) else {
        panic!("output that is not UTF-8 must fail");
    };
    assert!(
        command.ends_with("api repos/o/r/releases/tags/v1.2.3"),
        "{command}"
    );

    let mut args = consistent(&dir);
    args.gh = gh(&dir, &release_cases(&dir, "not a sums file\n"));
    assert_eq!(
        run(&args),
        Err(Error::Sums {
            line: 1,
            text: "not a sums file".to_owned()
        })
    );
    args.gh = gh(&dir, &release_cases(&dir, &sums()));
    args.curl = missing.clone();
    assert!(matches!(
        run(&args),
        Err(Error::Release(super::super::Error::Spawn { program, .. })) if program == missing
    ));
}

/// Asserts the file failures: an output that cannot be appended to and a download directory
/// that cannot be made are I/O errors naming the path.
#[test]
fn files_that_cannot_be_written_are_io_errors() {
    let _serial = serial();
    let dir = scratch("desired-io");
    let mut args = consistent(&dir);
    args.output = Some(dir.join("no-such-directory/out"));
    let Err(Error::Release(super::super::Error::Io { path, .. })) = run(&args) else {
        panic!("an output that cannot be opened must fail");
    };
    assert!(path.ends_with("no-such-directory/out"), "{path}");
    let mut args = consistent(&dir);
    std::fs::remove_dir_all(dir.join("d")).expect("remove");
    std::fs::write(dir.join("d"), "a file").expect("file");
    let Err(Error::Release(super::super::Error::Io { path, .. })) = run(&args) else {
        panic!("a directory that cannot be made must fail");
    };
    assert_eq!(path, dir.join("d").display().to_string());
    // Downloaded SHA256SUMS that is missing after a download that did nothing.
    std::fs::remove_file(dir.join("d")).expect("remove");
    args.gh = gh(
        &dir,
        &[
            (
                "api repos/o/r/releases/tags/v1.2.3".to_owned(),
                say(&release(&["SHA256SUMS"])),
            ),
            (
                format!(
                    "release download v1.2.3 --repo o/r --dir {} --pattern SHA256SUMS",
                    dir.join("d").display()
                ),
                "true".to_owned(),
            ),
        ],
    );
    let Err(Error::Release(super::super::Error::Io { path, .. })) = run(&args) else {
        panic!("a missing SHA256SUMS must fail");
    };
    assert_eq!(path, dir.join("d/SHA256SUMS").display().to_string());
    // A `Packages` file `curl` said it wrote and did not.
    args.gh = gh(&dir, &release_cases(&dir, &sums()));
    args.curl = script(&dir, "curl-silent", "true\n");
    let Err(Error::Release(super::super::Error::Io { path, .. })) = run(&args) else {
        panic!("a missing Packages must fail");
    };
    assert_eq!(path, dir.join("d/Packages-arm64").display().to_string());
}

/// Asserts the reader of `SHA256SUMS`: text and binary lines, the last line for a name that is
/// listed twice, nothing for a name not listed, and a refusal naming the number of the first
/// line that is not 64 lowercase hex digits, a space, a space or `*` and a name.
#[test]
fn the_sums_reader_takes_what_sha256sum_writes() {
    let _serial = serial();
    let text = format!("{AMD64}  a\n{ARM64} *b\n{OTHER}  a\n");
    assert_eq!(sums_digest(&text, "a"), Ok(Some(OTHER.to_owned())));
    assert_eq!(sums_digest(&text, "b"), Ok(Some(ARM64.to_owned())));
    assert_eq!(sums_digest(&text, "c"), Ok(None));
    assert_eq!(sums_digest("", "c"), Ok(None));
    let upper = AMD64.replace('1', "A");
    for bad in [
        format!("{upper}  a"),
        format!("{}  a", &AMD64[1..]),
        format!("{AMD64} a"),
        AMD64.to_owned(),
        String::new(),
    ] {
        assert_eq!(
            sums_digest(&format!("{AMD64}  ok\n{bad}\n"), "ok"),
            Err(Error::Sums {
                line: 2,
                text: bad.clone()
            }),
            "{bad:?}"
        );
    }
}

/// Asserts the reader of a `Packages` index: the stanza is found by its exact `Filename`
/// (a field of another name that starts alike, or a value that only starts alike, is not it),
/// its `SHA256` is read from that stanza and not a neighbour's, and a stanza without the field
/// is listed with none.
#[test]
fn the_packages_reader_finds_a_stanza_by_its_filename() {
    let _serial = serial();
    let text = "Package: a\nFilename: pool/x/a.deb\nSHA256: aa\n\n\
                Package: b\nFilename-Extra: pool/x/b.deb\nFilename: pool/x/b.deb.sig\nSHA256-Extra: no\n\n\
                Package: c\nDescription: one\n  Filename: pool/x/c.deb\nFilename: pool/x/c.deb\n";
    assert_eq!(
        listed(text, "pool/x/a.deb"),
        Listed::Yes(Some("aa".to_owned()))
    );
    assert_eq!(listed(text, "pool/x/b.deb"), Listed::No);
    assert_eq!(listed(text, "pool/x/b.deb.sig"), Listed::Yes(None));
    assert_eq!(listed(text, "pool/x/c.deb"), Listed::Yes(None));
    assert_eq!(listed("", "pool/x/a.deb"), Listed::No);
}

/// The reserved keys the updater adds to the params file, as hostkeeper writes them: quoted
/// dotted keys beside the manifest's own.
fn params_file() -> String {
    let mut table = toml::Table::new();
    for (key, value) in manifest::params().as_object().expect("table") {
        let scalar = if let Some(text) = value.get("text") {
            toml::Value::String(text.as_str().expect("text").to_owned())
        } else {
            toml::Value::Integer(
                value["integer"]
                    .as_str()
                    .expect("integer")
                    .parse()
                    .expect("number"),
            )
        };
        table.insert(key.clone(), scalar);
    }
    for (key, value) in [
        ("updater.artifact_digest", format!("sha256:{AMD64}")),
        (
            "updater.staged_release",
            "/var/lib/hostkeeper/packages/js-agent/1.2.3".to_owned(),
        ),
        (
            "updater.previous_release",
            "/var/lib/hostkeeper/packages/js-agent/1.2.2".to_owned(),
        ),
        ("updater.target_version", "1.2.3".to_owned()),
        ("updater.previous_version", "1.2.2".to_owned()),
    ] {
        table.insert(key.to_owned(), toml::Value::String(value));
    }
    table.insert(
        "updater.promotion_ceiling_secs".to_owned(),
        toml::Value::Integer(36000),
    );
    toml::to_string(&table).expect("toml")
}

/// Asserts that the params the manifest carries are exactly the keys the update hook requires
/// (`js_agent::update_hook::params::KEYS`: none missing, none extra, none twice), and that the
/// hook's own reader accepts them written as hostkeeper writes its params file, with the
/// bounds it reads equal to the manifest's `hook.timeoutSecs` and `verifyDeadlineSecs`.
#[test]
fn the_params_are_the_keys_the_hook_requires_and_its_reader_accepts_them() {
    let _serial = serial();
    use js_agent::update_hook::params::{KEYS, Params};
    let mut ours: Vec<&str> = PARAMS.iter().map(|(key, _)| *key).collect();
    let mut theirs: Vec<&str> = KEYS.to_vec();
    ours.sort_unstable();
    theirs.sort_unstable();
    assert_eq!(ours, theirs);
    assert_eq!(
        PARAMS
            .iter()
            .filter(|(_, p)| matches!(p, Param::Integer(_)))
            .count(),
        10
    );

    let params = Params::parse(&params_file()).expect("the hook's reader accepts the table");
    assert_eq!(params.settings.unit, "js-agent-crun.service");
    assert_eq!(params.settings.runtime, "crun");
    assert_eq!(
        params.release_link,
        PathBuf::from("/var/lib/hostkeeper/packages/js-agent/current")
    );
    assert_eq!(params.target_version, "1.2.3");
    let hook = &manifest::manifest()["hook"];
    let secs = |key: &str| {
        hook["timeoutSecs"][key]
            .as_str()
            .expect(key)
            .parse::<u64>()
            .expect("n")
    };
    assert_eq!(params.settings.timeouts.prepare, secs("prepare"));
    assert_eq!(params.settings.timeouts.cutover, secs("cutover"));
    assert_eq!(params.settings.timeouts.verify, secs("verify"));
    assert_eq!(params.settings.timeouts.rollback, secs("rollback"));
    assert_eq!(params.settings.timeouts.cleanup, secs("cleanup"));
    assert_eq!(
        params.settings.verify_deadline_secs.to_string(),
        hook["verifyDeadlineSecs"].as_str().expect("deadline")
    );
}

/// Asserts the manifest's shape against the contract of hostkeeper's native entries: 64-bit
/// integers are strings, there is no `uninstall` bound and no health URL, the hook command is
/// the program the package installs, the unit it runs is a regular unit of the package, and the
/// verify deadline covers `verify` plus `rollback`.
#[test]
fn the_manifest_follows_the_native_entry_contract() {
    let _serial = serial();
    let manifest = manifest::manifest();
    assert_eq!(
        manifest["hook"]["command"],
        serde_json::json!(["/usr/bin/js-agent", "update-hook"])
    );
    assert!(manifest["hook"]["timeoutSecs"].get("uninstall").is_none());
    assert!(manifest.get("healthUrl").is_none());
    assert_eq!(
        manifest["run"],
        serde_json::json!(["js-agent-crun.service"])
    );
    assert!(
        super::super::deb::FILES
            .iter()
            .any(|f| f.path == "/usr/lib/systemd/system/js-agent-crun.service")
    );
    const {
        assert!(manifest::VERIFY_DEADLINE_SECS >= manifest::VERIFY_SECS + manifest::ROLLBACK_SECS);
    }
    for secs in manifest["hook"]["timeoutSecs"]
        .as_object()
        .expect("bounds")
        .values()
    {
        assert!(secs.is_string(), "{secs}");
    }
    assert_eq!(manifest["hook"]["cleanupRetries"], 3);
    let value: Value = serde_json::from_str(&golden()).expect("golden");
    assert_eq!(value["linux-x86_64"]["entries"][0]["manifest"], manifest);
}

/// Asserts the targets of one platform: only the platforms given are keyed (no `macos-arm64`).
#[test]
fn the_targets_hold_only_the_platforms_given() {
    let _serial = serial();
    let one = manifest::targets("1.2.3", &[(Platform::LinuxArm64, ARM64.to_owned())]);
    assert_eq!(
        one.as_object().expect("object").keys().collect::<Vec<_>>(),
        ["linux-arm64"]
    );
    assert_eq!(one["linux-arm64"]["entries"][0]["digest"], ARM64);
}
