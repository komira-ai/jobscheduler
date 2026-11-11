//! `release server-deploy`: the choice of the release, what it refuses and what it writes, run
//! against a stand-in `gh` (a small script in a scratch directory that logs its command lines and
//! answers from the text the test gives) and image tars the tests build. No network is used and
//! the real program is never started.

use js_testkit::dir::TestDir;

use std::str::FromStr;

use super::super::desired::tests::{gh, log, release, say, scratch, serial};
use super::*;

const DIGEST: &str = "sha256:0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";
const TAR: &str = "js-lease-server-1.2.3-x86_64-unknown-linux-musl.oci.tar";
const BUNDLE: &str = "js-lease-server-1.2.3-x86_64-unknown-linux-musl.oci.tar.sigstore.json";
const TAG_API: &str = "api repos/o/r/releases/tags/v1.2.3";
const BASE: &str = "https://github.com/o/r/releases/download/v1.2.3";

/// An `index.json` with the manifests `manifests` (each a JSON object).
fn index(manifests: &[String]) -> String {
    format!(
        "{{\"schemaVersion\":2,\"manifests\":[{}]}}",
        manifests.join(",")
    )
}

/// A manifest descriptor of `digest` for `os`/`architecture`.
fn manifest(digest: &str, os: &str, architecture: &str) -> String {
    format!(
        "{{\"mediaType\":\"application/vnd.oci.image.manifest.v1+json\",\"digest\":\"{digest}\",\"size\":7,\"platform\":{{\"os\":\"{os}\",\"architecture\":\"{architecture}\"}}}}"
    )
}

/// The index of an image of the right platform and digest.
fn good_index() -> String {
    index(&[manifest(DIGEST, "linux", "amd64")])
}

/// A tar holding the files `(name, bytes)`, in that order.
fn tar_of(files: &[(&str, &str)]) -> Vec<u8> {
    let mut builder = tar::Builder::new(Vec::new());
    for (name, text) in files {
        let mut header = tar::Header::new_gnu();
        header.set_size(text.len() as u64);
        header.set_mode(0o644);
        header.set_cksum();
        builder
            .append_data(&mut header, name, text.as_bytes())
            .expect("append");
    }
    builder.into_inner().expect("tar")
}

/// The gh cases of a published release 1.2.3 that holds the image and its bundle and whose
/// download writes `tar` as the image.
fn cases(dir: &TestDir, tar: &[u8]) -> Vec<(String, String)> {
    let fixture = dir.join("fixture.tar");
    std::fs::write(&fixture, tar).expect("fixture");
    vec![
        (
            TAG_API.to_owned(),
            say(&release(&[TAR, BUNDLE, "SHA256SUMS"])),
        ),
        (
            format!(
                "release download v1.2.3 --repo o/r --dir {} --pattern {TAR}",
                dir.join("d").display()
            ),
            format!(
                "cp '{}' '{}'\n",
                fixture.display(),
                dir.join("d").join(TAR).display()
            ),
        ),
    ]
}

/// The arguments of a run by version in `dir`, with the stand-in.
fn args(dir: &TestDir, gh: String) -> ServerDeployArgs {
    ServerDeployArgs {
        repo: "o/r".to_owned(),
        version: "1.2.3".to_owned(),
        after_run: String::new(),
        dir: dir.join("d"),
        output: Some(dir.join("out")),
        gh,
    }
}

/// The arguments for a release whose image tar is `tar`.
fn serving(dir: &TestDir, tar: &[u8]) -> ServerDeployArgs {
    let gh = gh(dir, &cases(dir, tar));
    args(dir, gh)
}

/// The refusal's lines.
fn refused(result: Result<String, Error>) -> Vec<String> {
    match result {
        Err(Error::Refused(lines)) => lines,
        other => panic!("expected a refusal, got {other:?}"),
    }
}

/// The lines a deploy of 1.2.3 hands on.
fn outputs() -> String {
    format!(
        "deploy=true\nversion=1.2.3\nimage-digest={DIGEST}\nimage-url={BASE}/{TAR}\nbundle-url={BASE}/{BUNDLE}\n"
    )
}

/// Asserts the whole path by version: only the image tar is downloaded, the one manifest's
/// digest and the two release URLs are the outputs, the output file holds exactly those five
/// lines, and what is printed says what was found. An index listed after other files is found.
#[test]
fn a_release_with_one_amd64_image_is_deployed_by_its_digest() {
    let _serial = serial();
    let dir = scratch("server-deploy-ok");
    let tar = tar_of(&[("blobs", "x"), ("index.json", &good_index())]);
    let args = serving(&dir, &tar);
    let printed = run(&args).expect("passes");
    assert_eq!(
        std::fs::read_to_string(dir.join("out")).expect("output"),
        outputs()
    );
    assert_eq!(
        printed,
        format!(
            "release v1.2.3: {TAR} holds one linux/amd64 image, {DIGEST}, and {BUNDLE} is attached\n{}",
            outputs().trim_end()
        )
    );
    assert_eq!(
        log(&dir, "gh.log"),
        format!(
            "api repos/o/r/releases/tags/v1.2.3\nrelease download v1.2.3 --repo o/r --dir {} --pattern {TAR}\n",
            dir.join("d").display()
        )
    );
    // Outputs are appended to a file that has lines, and optional.
    std::fs::write(dir.join("out"), "earlier=1\n").expect("seed");
    run(&args).expect("passes again");
    assert_eq!(
        std::fs::read_to_string(dir.join("out")).expect("output"),
        format!("earlier=1\n{}", outputs())
    );
    let mut args = args;
    args.output = None;
    std::fs::remove_file(dir.join("out")).expect("remove");
    assert_eq!(run(&args).expect("passes"), printed);
    assert!(!dir.join("out").exists());
}

/// Asserts that after a run whose `publish` succeeded the newest published release is the one
/// deployed (drafts, pre-releases and tags that are not versions are not candidates).
#[test]
fn after_a_run_that_published_the_newest_release_is_deployed() {
    let _serial = serial();
    let dir = scratch("server-deploy-after");
    let mut cases = cases(&dir, &tar_of(&[("index.json", &good_index())]));
    cases.push((
        "api repos/o/r/actions/runs/77/jobs?per_page=100&filter=latest".to_owned(),
        say("{\"total_count\":2,\"jobs\":[{\"name\":\"publish\",\"conclusion\":\"failure\"},{\"name\":\"publish\",\"conclusion\":\"success\"}]}"),
    ));
    cases.push((
        "api repos/o/r/releases?per_page=100".to_owned(),
        say("[{\"tag_name\":\"v1.2.3\",\"draft\":false,\"prerelease\":false},{\"tag_name\":\"v1.2.2\",\"draft\":false,\"prerelease\":false},{\"tag_name\":\"v9.0.0\",\"draft\":true,\"prerelease\":false},{\"tag_name\":\"v8.0.0\",\"draft\":false,\"prerelease\":true},{\"tag_name\":\"nightly\",\"draft\":false,\"prerelease\":false}]"),
    ));
    let mut args = args(&dir, gh(&dir, &cases));
    args.version = String::new();
    args.after_run = "77".to_owned();
    let printed = run(&args).expect("passes");
    assert!(printed.ends_with(&outputs().trim_end()), "{printed}");
    let calls = log(&dir, "gh.log");
    assert!(
        calls.starts_with("api repos/o/r/actions/runs/77/jobs?per_page=100&filter=latest\napi repos/o/r/releases?per_page=100\napi repos/o/r/releases/tags/v1.2.3\n"),
        "{calls}"
    );
}

/// Asserts that when the job `publish` did not succeed (skipped, failed or absent) nothing is
/// deployed: `deploy=false` is appended and printed, the verb succeeds, and neither the
/// releases nor an asset is asked for.
#[test]
fn after_a_run_that_published_nothing_nothing_is_deployed() {
    let _serial = serial();
    for jobs in [
        "{\"total_count\":2,\"jobs\":[{\"name\":\"plan\",\"conclusion\":\"success\"},{\"name\":\"publish\",\"conclusion\":\"skipped\"}]}",
        "{\"total_count\":1,\"jobs\":[{\"name\":\"publish\",\"conclusion\":\"failure\"}]}",
        "{\"total_count\":0,\"jobs\":[]}",
    ] {
        let dir = scratch("server-deploy-nothing");
        let mut args = serving(&dir, &[]);
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
            "run 77 of o/r did not publish a release (publish did not succeed): nothing is deployed\ndeploy=false"
        );
        assert_eq!(
            std::fs::read_to_string(dir.join("out")).expect("output"),
            "deploy=false\n"
        );
        assert_eq!(log(&dir, "gh.log").lines().count(), 1);
        args.output = None;
        assert_eq!(run(&args).expect("passes"), printed);
    }
}

/// Asserts the usage errors: both or neither of `--version` and `--after-run`, a version that
/// is not three numbers and a run id that is not digits; none runs `gh`.
#[test]
fn flags_that_do_not_fit_together_are_usage_errors() {
    let _serial = serial();
    let dir = scratch("server-deploy-usage");
    let ok = serving(&dir, &[]);
    let with = |version: &str, after_run: &str| ServerDeployArgs {
        version: version.to_owned(),
        after_run: after_run.to_owned(),
        gh: ok.gh.clone(),
        repo: ok.repo.clone(),
        dir: ok.dir.clone(),
        output: None,
    };
    let both = "give one of --version and --after-run, not both and not neither";
    assert_eq!(run(&with("1.2.3", "5")), Err(Error::Usage(both.to_owned())));
    assert_eq!(run(&with("", "")), Err(Error::Usage(both.to_owned())));
    assert_eq!(
        run(&with("1.2", "")),
        Err(Error::Usage(
            "--version \"1.2\" is not <major>.<minor>.<patch>, a release tag without its v"
                .to_owned()
        ))
    );
    assert_eq!(
        run(&with("", "12a")),
        Err(Error::Usage(
            "--after-run \"12a\" is not the id of a workflow run".to_owned()
        ))
    );
    assert_eq!(log(&dir, "gh.log"), "");
}

/// Asserts that a job listing of more than one page, a repository with no published release, and
/// a draft or a pre-release named by `--version` are failures, and nothing is downloaded.
#[test]
fn a_release_that_is_not_published_or_not_found_is_a_failure() {
    let _serial = serial();
    let dir = scratch("server-deploy-unpublished");
    let mut args = serving(&dir, &[]);
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
    args.gh = gh(
        &dir,
        &[
            (
                "api repos/o/r/actions/runs/77/jobs?per_page=100&filter=latest".to_owned(),
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
    for flag in ["draft", "prerelease"] {
        let mut args = serving(&dir, &[]);
        args.gh = gh(
            &dir,
            &[(
                TAG_API.to_owned(),
                say(&format!("{{\"{flag}\":true,\"assets\":[]}}")),
            )],
        );
        assert_eq!(
            run(&args),
            Err(Error::Failed(
                "release v1.2.3 of o/r is a draft or a pre-release, not a released version"
                    .to_owned()
            ))
        );
    }
    assert!(!log(&dir, "gh.log").contains("release download"));
}

/// Asserts that a release without the amd64 image tar, without its bundle or without both is
/// refused naming each missing asset, an image of the other architecture does not stand in, and
/// nothing is downloaded or made.
#[test]
fn a_release_without_the_image_or_its_bundle_is_refused_by_name() {
    let _serial = serial();
    let arm = "js-lease-server-1.2.3-aarch64-unknown-linux-musl.oci.tar";
    let bundle_of_arm = format!("{arm}.sigstore.json");
    for (assets, says) in [
        (
            vec![BUNDLE, arm, bundle_of_arm.as_str()],
            vec![format!("release v1.2.3 has no {TAR}")],
        ),
        (
            vec![TAR, arm, bundle_of_arm.as_str()],
            vec![format!("release v1.2.3 has no {BUNDLE}")],
        ),
        (
            vec![arm, bundle_of_arm.as_str()],
            vec![
                format!("release v1.2.3 has no {TAR}"),
                format!("release v1.2.3 has no {BUNDLE}"),
            ],
        ),
    ] {
        let dir = scratch("server-deploy-assets");
        let mut args = serving(&dir, &[]);
        args.gh = gh(&dir, &[(TAG_API.to_owned(), say(&release(&assets)))]);
        assert_eq!(refused(run(&args)), says);
        assert!(!dir.join("d").exists());
        assert!(!dir.join("out").exists());
    }
}

/// Asserts each way the tar is refused: not a tar, a tar without `index.json` (also one that
/// names it in a directory), an index that is not JSON, with no manifest and with two, a
/// platform that is not linux/amd64 (each half, and an index naming none), and a digest that is
/// missing, short, upper case, not sha256 or with another length; and that every problem of
/// one manifest is reported, and no output is written.
#[test]
fn an_image_that_is_not_one_linux_amd64_manifest_by_a_sha256_digest_is_refused() {
    let _serial = serial();
    let wrong_os = manifest(DIGEST, "darwin", "amd64");
    let wrong_arch = manifest(DIGEST, "linux", "arm64");
    let no_platform = format!("{{\"digest\":\"{DIGEST}\"}}");
    let upper = DIGEST.replace("abcdef", "ABCDEF");
    let cases: Vec<(Vec<u8>, Vec<String>)> = vec![
        (
            tar_of(&[("blobs", "x")]),
            vec![format!("{TAR} has no index.json")],
        ),
        (
            tar_of(&[("oci/index.json", &good_index())]),
            vec![format!("{TAR} has no index.json")],
        ),
        (
            vec![b'x'; 1024],
            vec![format!(
                "{TAR} cannot be read as a tar holding index.json: (the tar crate's reason)"
            )],
        ),
        (
            tar_of(&[("index.json", "not json")]),
            vec![format!(
                "index.json of {TAR} is not JSON: expected ident at line 1 column 2"
            )],
        ),
        (
            tar_of(&[("index.json", "{\"schemaVersion\":2}")]),
            vec![format!(
                "index.json of {TAR} lists 0 manifests, not exactly one"
            )],
        ),
        (
            tar_of(&[(
                "index.json",
                &index(&[
                    manifest(DIGEST, "linux", "amd64"),
                    manifest(DIGEST, "linux", "arm64"),
                ]),
            )]),
            vec![format!(
                "index.json of {TAR} lists 2 manifests, not exactly one"
            )],
        ),
        (
            tar_of(&[("index.json", &index(&[wrong_os]))]),
            vec![format!(
                "the manifest of {TAR} has platform darwin/amd64, not linux/amd64"
            )],
        ),
        (
            tar_of(&[("index.json", &index(&[wrong_arch]))]),
            vec![format!(
                "the manifest of {TAR} has platform linux/arm64, not linux/amd64"
            )],
        ),
        (
            tar_of(&[("index.json", &index(&[no_platform]))]),
            vec![format!(
                "the manifest of {TAR} has platform (none)/(none), not linux/amd64"
            )],
        ),
        (
            tar_of(&[("index.json", &index(&[manifest("", "linux", "amd64")]))]),
            vec![format!(
                "the manifest of {TAR} has digest \"\", not sha256: and 64 lowercase hex digits"
            )],
        ),
        (
            tar_of(&[(
                "index.json",
                &index(&[manifest("sha256:0123", "linux", "amd64")]),
            )]),
            vec![format!(
                "the manifest of {TAR} has digest \"sha256:0123\", not sha256: and 64 lowercase hex digits"
            )],
        ),
        (
            tar_of(&[("index.json", &index(&[manifest(&upper, "linux", "amd64")]))]),
            vec![format!(
                "the manifest of {TAR} has digest {upper:?}, not sha256: and 64 lowercase hex digits"
            )],
        ),
        (
            tar_of(&[(
                "index.json",
                &index(&[manifest(
                    &DIGEST.replace("sha256", "sha512"),
                    "linux",
                    "amd64",
                )]),
            )]),
            vec![format!(
                "the manifest of {TAR} has digest {:?}, not sha256: and 64 lowercase hex digits",
                DIGEST.replace("sha256", "sha512")
            )],
        ),
        (
            tar_of(&[(
                "index.json",
                &index(&[manifest(&format!("{DIGEST}0"), "linux", "amd64")]),
            )]),
            vec![format!(
                "the manifest of {TAR} has digest {:?}, not sha256: and 64 lowercase hex digits",
                format!("{DIGEST}0")
            )],
        ),
        (
            tar_of(&[(
                "index.json",
                "{\"manifests\":[{\"platform\":{\"os\":\"windows\",\"architecture\":\"arm\"}}]}",
            )]),
            vec![
                format!("the manifest of {TAR} has platform windows/arm, not linux/amd64"),
                format!(
                    "the manifest of {TAR} has digest \"\", not sha256: and 64 lowercase hex digits"
                ),
            ],
        ),
    ];
    for (tar, says) in cases {
        let dir = scratch("server-deploy-tar");
        let args = serving(&dir, &tar);
        let result = refused(run(&args));
        // The reason a non-tar gives is the tar crate's; the rest is exact.
        if says[0].contains("cannot be read as a tar") {
            assert_eq!(result.len(), 1);
            assert!(
                result[0].starts_with(&format!(
                    "{TAR} cannot be read as a tar holding index.json: "
                )),
                "{result:?}"
            );
        } else if says[0].contains("is not JSON") {
            assert_eq!(result.len(), 1);
            assert!(
                result[0].starts_with(&format!("index.json of {TAR} is not JSON: ")),
                "{result:?}"
            );
        } else {
            assert_eq!(result, says);
        }
        assert!(!dir.join("out").exists());
    }
}

/// Asserts the failures of programs and files: a `gh` that does not exist or fails, a release
/// JSON that is not JSON, a download that wrote nothing, an output that cannot be appended to
/// and a download directory that cannot be made, each an error naming its cause.
#[test]
fn programs_and_files_that_fail_are_reported_with_their_reason() {
    let _serial = serial();
    let dir = scratch("server-deploy-io");
    let mut args = serving(&dir, &tar_of(&[("index.json", &good_index())]));
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
    assert_eq!(stderr, format!("unexpected: {TAG_API}"));
    args.gh = gh(&dir, &[(TAG_API.to_owned(), say("not json"))]);
    let Err(Error::Failed(why)) = run(&args) else {
        panic!("text that is not JSON must fail");
    };
    assert!(
        why.starts_with("gh api repos/o/r/releases/tags/v1.2.3 did not print JSON: "),
        "{why}"
    );
    // A download that fails.
    let mut cases = cases(&dir, &[]);
    cases[1].1 = "echo denied >&2; exit 3\n".to_owned();
    args.gh = gh(&dir, &cases);
    let Err(Error::Release(super::super::Error::Failed { stderr, .. })) = run(&args) else {
        panic!("a failed download must fail");
    };
    assert_eq!(stderr, "denied");
    // A download that did nothing: the tar is missing.
    cases[1].1 = "true\n".to_owned();
    args.gh = gh(&dir, &cases);
    let Err(Error::Release(super::super::Error::Io { path, .. })) = run(&args) else {
        panic!("a missing tar must fail");
    };
    assert_eq!(path, dir.join("d").join(TAR).display().to_string());
    // An output that cannot be appended to.
    let mut args = serving(&dir, &tar_of(&[("index.json", &good_index())]));
    args.output = Some(dir.join("no-such-directory/out"));
    let Err(Error::Release(super::super::Error::Io { path, .. })) = run(&args) else {
        panic!("an output that cannot be opened must fail");
    };
    assert!(path.ends_with("no-such-directory/out"), "{path}");
    // The same for `deploy=false`.
    let mut args = serving(&dir, &[]);
    args.version = String::new();
    args.after_run = "5".to_owned();
    args.gh = gh(
        &dir,
        &[(
            "api repos/o/r/actions/runs/5/jobs?per_page=100&filter=latest".to_owned(),
            say("{\"total_count\":0,\"jobs\":[]}"),
        )],
    );
    args.output = Some(dir.join("no-such-directory/out"));
    assert!(matches!(
        run(&args),
        Err(Error::Release(super::super::Error::Io { .. }))
    ));
    // A download directory that cannot be made.
    let args = serving(&dir, &tar_of(&[("index.json", &good_index())]));
    std::fs::remove_dir_all(dir.join("d")).expect("remove");
    std::fs::write(dir.join("d"), "a file").expect("file");
    let Err(Error::Release(super::super::Error::Io { path, .. })) = run(&args) else {
        panic!("a directory that cannot be made must fail");
    };
    assert_eq!(path, dir.join("d").display().to_string());
}

/// Asserts the small parts: the asset name is the one `release image` writes for amd64, the
/// bundle's name is the asset's with `.sigstore.json`, and a digest is `sha256:` and exactly 64
/// lowercase hex digits.
#[test]
fn the_names_and_the_digest_check_are_exact() {
    let version = Version::from_str("1.2.3").expect("version");
    assert_eq!(tar_name(version), TAR);
    assert_eq!(bundle_name(TAR), BUNDLE);
    assert!(is_digest(DIGEST));
    for bad in [
        "",
        "sha256:",
        "sha256:0123",
        &DIGEST.to_uppercase(),
        &DIGEST.replace('0', "g"),
        &format!("{DIGEST}0"),
        &DIGEST["sha256:".len()..],
    ] {
        assert!(!is_digest(bad), "{bad}");
    }
}
