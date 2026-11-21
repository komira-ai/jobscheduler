//! `release desired --pkg-url`: the Mac entry and manifest as written JSON, the release set for
//! Linux alone when the package is not published, and what is refused (a package that is not the
//! one the sums list, sums that list no package, a failing download), run against stand-ins for
//! `gh` and `curl` as the other tests of `release desired` are.

use js_testkit::dir::TestDir;
use serde_json::Value;

use super::super::manifest::{self, MAC_PARAMS, PARAMS};
use super::super::tests::{
    AMD64, ARM64, args, gh, golden, log, params_file_of, refused, release_cases, scratch, script,
    serial,
};
use super::super::{DesiredArgs, Error, run};
use super::*;
use crate::fetch::Sha256;

const BASE: &str = "https://pkg.example/pkg";
const PKG_NAME: &str = "js-agent-1.2.3-macos-arm64.pkg";
const PKG_BYTES: &str = "signed package bytes";
/// What the stand-in writes for it: the text and the newline its here-document ends with.
const PKG_FILE: &str = "signed package bytes\n";

/// What the stand-in `curl` does for one URL.
enum Serve<'a> {
    /// Writes this text to the `--output` file.
    Text(&'a str),
    /// Fails as `curl --fail` does for a 404.
    Missing,
    /// Fails as `curl --fail` does for a 403, which CloudFront over S3 answers for a missing
    /// object.
    Forbidden,
    /// Fails as `curl --fail` does for a 500.
    Broken,
    /// Succeeds and writes nothing.
    Nothing,
}

/// The shell of one URL's case.
fn serve(url: &str, what: &Serve<'_>) -> String {
    match what {
        Serve::Text(text) => format!("{url}) cat > \"$out\" <<'EOF'\n{text}\nEOF\n;;\n"),
        Serve::Missing => format!(
            "{url}) echo 'curl: (22) The requested URL returned error: 404' >&2; exit 22;;\n"
        ),
        Serve::Forbidden => format!(
            "{url}) echo 'curl: (22) The requested URL returned error: 403' >&2; exit 22;;\n"
        ),
        Serve::Broken => format!(
            "{url}) echo 'curl: (22) The requested URL returned error: 500' >&2; exit 22;;\n"
        ),
        Serve::Nothing => format!("{url}) ;;\n"),
    }
}

/// A stand-in `curl` that logs its command line to `curl.log` and answers the two `Packages`
/// URLs of a consistent stable suite, the Mac sums and the Mac package as the test says.
fn curl(dir: &TestDir, sums: &Serve<'_>, pkg: &Serve<'_>) -> String {
    let packages = |arch: &str, sha256: &str| {
        format!(
            "Package: js-agent\nFilename: pool/jobscheduler/js-agent_1.2.3_{arch}.deb\nSHA256: {sha256}\n"
        )
    };
    let body = format!(
        "echo \"$*\" >> '{}'\nfor last; do :; done\nwhile [ $# -gt 0 ]; do\n\
         [ \"$1\" = --output ] && out=$2\nshift\ndone\ncase \"$last\" in\n{}{}{}{}\
         *) echo \"unexpected: $last\" >&2; exit 9;;\nesac\n",
        dir.join("curl.log").display(),
        serve(
            "*/binary-amd64/Packages",
            &Serve::Text(&packages("amd64", AMD64))
        ),
        serve(
            "*/binary-arm64/Packages",
            &Serve::Text(&packages("arm64", ARM64))
        ),
        serve(&format!("{BASE}/1.2.3/SHA256SUMS"), sums),
        serve(&format!("{BASE}/1.2.3/{PKG_NAME}"), pkg),
    );
    script(dir, "curl", &body)
}

/// The sums file of the Mac directory: the package at `digest`, after another file.
fn pkg_sums(digest: &str) -> String {
    format!("{AMD64}  other-file\n{digest}  {PKG_NAME}")
}

/// The digest of the package the stand-in serves.
fn pkg_digest() -> String {
    Sha256::of(PKG_FILE.as_bytes()).to_string()
}

/// A run by version with the Mac looked at: the stand-in programs, `--pkg-url` set.
fn with_mac(dir: &TestDir, sums: &Serve<'_>, pkg: &Serve<'_>) -> DesiredArgs {
    let gh = gh(dir, &release_cases(dir, &super::super::tests::sums()));
    let mut args = args(dir, gh, curl(dir, sums, pkg));
    args.pkg_url = Some(BASE.to_owned());
    args
}

/// The `macos-arm64` entry of the release 1.2.3 for the package the stand-in serves, as the
/// proto3 canonical JSON hostkeeper reads: the whole text, written out.
fn mac_golden() -> String {
    format!(
        "{{\"bytes\":\"21\",\"digest\":\"{}\",\"installer\":\"macos-pkg\",\"manifest\":{{\"hook\":{{\"cleanupRetries\":3,\"command\":[\"/Library/Application Support/js-agent/bin/js-agent\",\"update-hook\"],\"drain\":\"drain\",\"resume\":\"resume\",\"timeoutSecs\":{{\"cleanup\":\"120\",\"cutover\":\"4320\",\"prepare\":\"3900\",\"rollback\":\"8220\",\"verify\":\"615\"}},\"verifyDeadlineSecs\":\"9000\"}},\"installerBounds\":{{\"activateSecs\":\"300\",\"removeSecs\":\"300\",\"stageSecs\":\"600\"}},\"minHostkeeper\":\"0.1.225\",\"package\":\"com.komira.js-agent\",\"params\":{{\"cleanup_timeout_secs\":{{\"integer\":\"120\"}},\"cordon_lease_secs\":{{\"integer\":\"300\"}},\"cordon_max_secs\":{{\"integer\":\"17355\"}},\"cutover_timeout_secs\":{{\"integer\":\"4320\"}},\"defer_retry_secs\":{{\"integer\":\"300\"}},\"drain_wait_secs\":{{\"integer\":\"3600\"}},\"flags_file\":{{\"text\":\"/etc/js-agent/flags\"}},\"helper_unit\":{{\"text\":\"js-agent.session\"}},\"pf_unit\":{{\"text\":\"js-agent.pf-pool\"}},\"prepare_timeout_secs\":{{\"integer\":\"3900\"}},\"release_link\":{{\"text\":\"/var/lib/hostkeeper/packages/js-agent/current\"}},\"rollback_timeout_secs\":{{\"integer\":\"8220\"}},\"runtime\":{{\"text\":\"native\"}},\"scratch_root\":{{\"text\":\"/var/db/js-agent/scratch/runs\"}},\"socket_group\":{{\"text\":\"_jsagent\"}},\"state_dir\":{{\"text\":\"/Library/Application Support/js-agent/update\"}},\"state_root\":{{\"text\":\"/var/db/js-agent/state\"}},\"token_file\":{{\"text\":\"/etc/js-agent/token\"}},\"unit\":{{\"text\":\"js-agent\"}},\"verify_deadline_secs\":{{\"integer\":\"9000\"}},\"verify_timeout_secs\":{{\"integer\":\"615\"}}}},\"run\":[\"js-agent\",\"js-agent.session\",\"js-agent.pf-pool\"]}},\"packageVersion\":\"1.2.3\",\"repository\":\"komira-stable\"}}",
        pkg_digest()
    )
}

/// The Linux targets' golden with the Mac platform added.
fn targets_with_mac() -> String {
    let linux = golden();
    format!(
        "{},\"macos-arm64\":{{\"entries\":[{}]}}}}",
        &linux[..linux.len() - 1],
        mac_golden()
    )
}

/// Asserts a release whose Mac package is published: `targets` is the Linux golden plus the
/// `macos-arm64` entry, written out, `set-mac=true` follows it, and `curl` read the Mac sums and
/// then the package over HTTPS, a `404` not being retried.
#[test]
fn a_published_mac_package_adds_the_macos_arm64_entry_and_sets_mac() {
    let _serial = serial();
    let dir = scratch("desired-mac");
    let sums_text = pkg_sums(&pkg_digest());
    let args = with_mac(&dir, &Serve::Text(&sums_text), &Serve::Text(PKG_BYTES));
    let printed = run(&args).expect("the run passes");
    let lines = format!(
        "set=true\npackage-version=1.2.3\ntargets={}\nset-mac=true\n",
        targets_with_mac()
    );
    assert_eq!(
        std::fs::read_to_string(dir.join("out")).expect("output"),
        lines
    );
    assert_eq!(
        printed,
        format!(
            "release v1.2.3: the stable suite lists js-agent 1.2.3 for each of 2 architectures, each with the digest SHA256SUMS lists\n\
             the Mac package {PKG_NAME} is published with the digest its SHA256SUMS lists\n{}",
            lines.trim_end()
        )
    );
    let flags = "--proto =https --tlsv1.2 --fail --silent --show-error --retry 5 --retry-delay 20 --max-time 600 --output";
    let pkg = dir.join("d/pkg");
    let calls = log(&dir, "curl.log");
    assert!(
        calls.ends_with(&format!(
            "{flags} {0}/SHA256SUMS {BASE}/1.2.3/SHA256SUMS\n{flags} {0}/{PKG_NAME} {BASE}/1.2.3/{PKG_NAME}\n",
            pkg.display()
        )),
        "{calls}"
    );
}

/// Asserts a release whose Mac directory has no `SHA256SUMS` (a `404`): the Linux targets are
/// exactly the ones written without `--pkg-url`, `set-mac=false` follows and the package is
/// never asked for.
#[test]
fn an_unpublished_mac_package_leaves_the_linux_targets_as_they_are() {
    let _serial = serial();
    let dir = scratch("desired-mac-absent");
    let args = with_mac(&dir, &Serve::Missing, &Serve::Missing);
    let printed = run(&args).expect("the run passes");
    let lines = format!(
        "set=true\npackage-version=1.2.3\ntargets={}\nset-mac=false\n",
        golden()
    );
    assert_eq!(
        std::fs::read_to_string(dir.join("out")).expect("output"),
        lines
    );
    assert!(!printed.contains("Mac package"), "{printed}");
    assert!(!log(&dir, "curl.log").contains(PKG_NAME));
}

/// Asserts a release whose Mac directory the host answers with a `403` for its `SHA256SUMS`, as
/// CloudFront over an S3 bucket it may not list answers every missing object: the same outputs as
/// a `404`, written out (the Linux targets, `set-mac=false`), the package never asked for, and the
/// sums asked for once (a `403` is an answer, not retried by hand).
#[test]
fn a_403_for_the_mac_sums_is_an_unpublished_mac_package() {
    let _serial = serial();
    let dir = scratch("desired-mac-forbidden");
    let args = with_mac(&dir, &Serve::Forbidden, &Serve::Forbidden);
    let printed = run(&args).expect("the run passes");
    let lines = format!(
        "set=true\npackage-version=1.2.3\ntargets={}\nset-mac=false\n",
        golden()
    );
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
    let calls = log(&dir, "curl.log");
    assert!(!calls.contains(PKG_NAME), "{calls}");
    assert_eq!(calls.matches("/1.2.3/SHA256SUMS").count(), 1, "{calls}");
}

/// Asserts that a `403` for the package, once its sums were read, is an error and nothing is
/// written: the sums say it is published, so the answer is not taken for an absent package.
#[test]
fn a_403_for_a_mac_package_its_sums_list_is_an_error() {
    let _serial = serial();
    let dir = scratch("desired-mac-pkg-forbidden");
    let sums_text = pkg_sums(&pkg_digest());
    let args = with_mac(&dir, &Serve::Text(&sums_text), &Serve::Forbidden);
    let Err(Error::Release(crate::release::Error::Failed { stderr, .. })) = run(&args) else {
        panic!("not a failed program");
    };
    assert!(stderr.contains("error: 403"), "{stderr}");
    assert!(!dir.join("out").exists());
}

/// Asserts that a failure other than a `404` or a `403` of the sums is an error and nothing is
/// written: a host group of Macs is not left unset for a host that merely could not be reached.
#[test]
fn a_failure_reading_the_mac_sums_other_than_a_404_or_403_is_an_error() {
    let _serial = serial();
    let dir = scratch("desired-mac-broken");
    let args = with_mac(&dir, &Serve::Broken, &Serve::Missing);
    let Err(Error::Release(crate::release::Error::Failed { stderr, .. })) = run(&args) else {
        panic!("not a failed program");
    };
    assert!(stderr.contains("error: 500"), "{stderr}");
    assert!(!dir.join("out").exists());
}

/// Asserts that a package that cannot be downloaded after its sums were read is an error.
#[test]
fn a_mac_package_that_cannot_be_downloaded_is_an_error() {
    let _serial = serial();
    let dir = scratch("desired-mac-nopkg");
    let sums_text = pkg_sums(&pkg_digest());
    let args = with_mac(&dir, &Serve::Text(&sums_text), &Serve::Missing);
    let Err(Error::Release(crate::release::Error::Failed { stderr, .. })) = run(&args) else {
        panic!("not a failed program");
    };
    assert!(stderr.contains("error: 404"), "{stderr}");
    assert!(!dir.join("out").exists());
}

/// Asserts that a package whose bytes are not the digest its sums list is refused, naming both
/// digests, and nothing is written.
#[test]
fn a_mac_package_with_another_digest_than_its_sums_is_refused() {
    let _serial = serial();
    let dir = scratch("desired-mac-digest");
    let sums_text = pkg_sums(AMD64);
    let args = with_mac(&dir, &Serve::Text(&sums_text), &Serve::Text(PKG_BYTES));
    assert_eq!(
        refused(run(&args)),
        [format!(
            "{PKG_NAME} at {BASE}/1.2.3 has sha256 {}, but its SHA256SUMS lists {AMD64}",
            pkg_digest()
        )]
    );
    assert!(!dir.join("out").exists());
}

/// Asserts that sums that do not list the package are refused, and the package is not downloaded.
#[test]
fn mac_sums_without_the_package_are_refused() {
    let _serial = serial();
    let dir = scratch("desired-mac-unlisted");
    let sums_text = format!("{AMD64}  js-agent-1.2.3-macos-arm64.pkg.sigstore.json");
    let args = with_mac(&dir, &Serve::Text(&sums_text), &Serve::Text(PKG_BYTES));
    assert_eq!(
        refused(run(&args)),
        [format!("{BASE}/1.2.3/SHA256SUMS lists no {PKG_NAME}")]
    );
    assert!(!log(&dir, "curl.log").contains(&format!("{BASE}/1.2.3/{PKG_NAME}")));
}

/// Asserts that sums that are not `sha256sum`'s lines are an error naming the line.
#[test]
fn malformed_mac_sums_are_an_error() {
    let _serial = serial();
    let dir = scratch("desired-mac-malformed");
    let args = with_mac(&dir, &Serve::Text("not a sum"), &Serve::Missing);
    assert_eq!(
        run(&args),
        Err(Error::Sums {
            line: 1,
            text: "not a sum".to_owned()
        })
    );
}

/// Asserts that files `curl` did not write, and a directory that cannot be made, are I/O errors
/// naming the path.
#[test]
fn files_the_mac_download_needs_are_io_errors() {
    let _serial = serial();
    let dir = scratch("desired-mac-io");
    let args = with_mac(&dir, &Serve::Nothing, &Serve::Nothing);
    let sums_file = dir.join("d/pkg/SHA256SUMS");
    let Err(Error::Release(crate::release::Error::Io { path, .. })) = run(&args) else {
        panic!("not an I/O error");
    };
    assert_eq!(path, sums_file.display().to_string());

    let sums_text = pkg_sums(&pkg_digest());
    let args = with_mac(&dir, &Serve::Text(&sums_text), &Serve::Nothing);
    let Err(Error::Release(crate::release::Error::Io { path, .. })) = run(&args) else {
        panic!("not an I/O error");
    };
    assert_eq!(path, dir.join("d/pkg").join(PKG_NAME).display().to_string());

    let blocked = scratch("desired-mac-blocked");
    std::fs::create_dir(blocked.join("d")).expect("dir");
    std::fs::write(blocked.join("d/pkg"), "a file").expect("file");
    let args = with_mac(&blocked, &Serve::Missing, &Serve::Missing);
    let Err(Error::Release(crate::release::Error::Io { path, .. })) = run(&args) else {
        panic!("not an I/O error");
    };
    assert_eq!(path, blocked.join("d/pkg").display().to_string());
}

/// Asserts that `--pkg-url` is held to the form of `--apt-url`: HTTPS and no trailing `/`.
#[test]
fn a_pkg_url_that_is_not_https_or_ends_in_a_slash_is_a_usage_error() {
    let _serial = serial();
    let dir = scratch("desired-mac-usage");
    for bad in ["http://pkg.example/pkg", "https://pkg.example/pkg/"] {
        let mut args = with_mac(&dir, &Serve::Missing, &Serve::Missing);
        args.pkg_url = Some(bad.to_owned());
        assert_eq!(
            run(&args),
            Err(Error::Usage(format!(
                "--pkg-url {bad:?} is not an https URL without a trailing /"
            )))
        );
    }
    assert!(!dir.join("curl.log").exists());
}

/// Asserts the package's name: the file the upload puts and the entry pins.
#[test]
fn the_package_is_named_by_its_version() {
    let version = "1.2.3".parse::<Version>().expect("version");
    assert_eq!(file_name(version), PKG_NAME);
}

/// Asserts the Mac manifest against the Linux one: the same keys and bounds plus `helper_unit`,
/// `pf_unit`, `socket_group` and `scratch_root`,
/// the package identifier, the launchd labels and the hook program the package installs, and
/// that the Linux manifest is the one it was.
#[test]
fn the_mac_manifest_is_the_linux_one_with_the_mac_units_paths_and_runtime() {
    let _serial = serial();
    let mac = manifest::mac_manifest();
    let linux = manifest::manifest();
    let keys = |table: &[(&str, manifest::Param)]| -> Vec<String> {
        let mut keys: Vec<String> = table.iter().map(|(k, _)| (*k).to_owned()).collect();
        keys.sort();
        keys
    };
    let mut linux_keys = keys(&PARAMS);
    linux_keys
        .extend(["helper_unit", "pf_unit", "socket_group", "scratch_root"].map(str::to_owned));
    linux_keys.sort();
    assert_eq!(keys(&MAC_PARAMS), linux_keys);
    assert_eq!(mac["installerBounds"], linux["installerBounds"]);
    assert_eq!(mac["hook"]["timeoutSecs"], linux["hook"]["timeoutSecs"]);
    assert_eq!(
        mac["hook"]["verifyDeadlineSecs"],
        linux["hook"]["verifyDeadlineSecs"]
    );
    assert_eq!(mac["package"], "com.komira.js-agent");
    assert_eq!(mac["minHostkeeper"], linux["minHostkeeper"]);
    assert_eq!(linux["package"], "js-agent");
    assert_eq!(linux["run"], serde_json::json!(["js-agent-crun.service"]));
    let value: Value = serde_json::from_str(&mac_golden()).expect("golden");
    assert_eq!(value["manifest"], mac);
    assert_eq!(value["bytes"], PKG_FILE.len().to_string());
}

/// Asserts that the Mac hook's host setup makes what the package's plists name: `socket_group` is
/// the helper's `--socket-group`, and `state_root` and `scratch_root` are the agent's
/// `--state-root` and `--scratch-root`, and that the hook's reader takes the table. Catches a
/// plist whose roots or group move while the hook keeps making the old ones.
#[test]
fn the_mac_host_setup_params_are_the_plists_own_values() {
    use js_mac_provision::daemon::{Daemon, SOCKET_GROUP};
    let _serial = serial();
    let text = |key: &str| {
        MAC_PARAMS.iter().find_map(|(name, value)| match value {
            manifest::Param::Text(text) if *name == key => Some((*text).to_owned()),
            _ => None,
        })
    };
    let flag = |daemon: &Daemon, name: &str| {
        let at = daemon.args.iter().position(|arg| arg == name);
        at.and_then(|at| daemon.args.get(at + 1)).cloned()
    };
    let agent = Daemon::agent();
    let helper = Daemon::session("cdhash H\"00\"");
    assert_eq!(text("socket_group").as_deref(), Some(SOCKET_GROUP));
    assert_eq!(text("socket_group"), flag(&helper, "--socket-group"));
    assert_eq!(text("state_root"), flag(&agent, "--state-root"));
    assert_eq!(text("scratch_root"), flag(&agent, "--scratch-root"));
    let file = params_file_of(&manifest::mac_manifest()["params"]);
    let params = js_agent::update_hook::Params::parse(&file).expect("the hook reads the table");
    assert_eq!(params.settings.socket_group.as_deref(), Some(SOCKET_GROUP));
    assert_eq!(
        params.scratch_root.map(|root| root.display().to_string()),
        flag(&agent, "--scratch-root")
    );
}
