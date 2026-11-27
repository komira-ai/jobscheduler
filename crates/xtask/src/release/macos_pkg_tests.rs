use std::collections::BTreeMap;
use std::path::Path;

use js_testkit::dir::TestDir;
use sha2::Digest as _;

use super::*;

const BASE: &str = "https://packages.example";

fn scratch(name: &str) -> TestDir {
    TestDir::under(std::env::temp_dir(), &format!("xtask-macos-pkg-{name}"))
}

/// The bytes of the package of `version` in the fake repository.
fn bytes_of(version: &str) -> Vec<u8> {
    format!("xar! the package of {version}").into_bytes()
}

/// The `SHA256SUMS` line of `bytes` under the package name of `version`.
fn sum_line(version: &str, bytes: &[u8]) -> String {
    format!(
        "{}  js-agent-{version}-macos-arm64.pkg\n",
        hex::encode(sha2::Sha256::digest(bytes))
    )
}

/// A package repository that holds the packages of `versions`, each with its `SHA256SUMS`.
fn repository(versions: &[&str]) -> BTreeMap<String, Vec<u8>> {
    let mut files = BTreeMap::new();
    for version in versions {
        let dir = format!("{BASE}/pkg/jobscheduler/{version}/");
        let bytes = bytes_of(version);
        // A line that is no sum and file comes first: it is passed over.
        let sums = format!("no-sum-here\n{}", sum_line(version, &bytes));
        files.insert(format!("{dir}SHA256SUMS"), sums.into_bytes());
        files.insert(format!("{dir}js-agent-{version}-macos-arm64.pkg"), bytes);
    }
    files
}

/// The download of `files`: a URL it does not hold answers 403, as the repository's CDN does.
fn get_of(files: BTreeMap<String, Vec<u8>>) -> impl Fn(&str) -> Result<Vec<u8>, String> {
    move |url| {
        files
            .get(url)
            .cloned()
            .ok_or_else(|| "status 403".to_owned())
    }
}

fn args(dir: &Path, version: Option<&str>, below: Option<&str>) -> MacosPkgArgs {
    let tags = dir.join("tags");
    std::fs::write(
        &tags,
        "v0.1.9\nv0.1.8\nnot-a-tag\nv0.1.7\nv0.1.10\nv0.1.6\nv0.1.8\n",
    )
    .unwrap();
    MacosPkgArgs {
        version: version.map(str::to_owned),
        below: below.map(str::to_owned),
        tags: Some(tags),
        out: dir.join("dist"),
        output: dir.join("output"),
        base_url: format!("{BASE}/"),
    }
}

fn output(dir: &Path) -> String {
    std::fs::read_to_string(dir.join("output")).unwrap()
}

/// Asserts that `--version` writes the version's package, as the repository serves it, to
/// `--out` under its file name, and appends `version=<it>` to `--output`; and that a trailing
/// `/` of `--base-url` is one directory, not two.
/// Catches a package written under another name, or a version left out of the output.
#[test]
fn a_version_s_package_is_written_and_its_version_said() {
    let dir = scratch("version");
    let a = args(dir.path(), Some("0.1.9"), None);
    let line = run(&a, &get_of(repository(&["0.1.9"]))).unwrap();
    assert_eq!(line, "version=0.1.9");
    assert_eq!(output(dir.path()), "version=0.1.9\n");
    let written = std::fs::read(dir.path().join("dist/js-agent-0.1.9-macos-arm64.pkg")).unwrap();
    assert_eq!(written, bytes_of("0.1.9"));
}

/// Asserts that `--below` takes the newest tag below the version whose package the repository
/// serves, passing over the version itself, those above it, a tag twice, a line that is no
/// version tag, and a release with no package (its `SHA256SUMS` answers 403).
/// Catches a version compared as text (`0.1.10` after `0.1.9`), the version itself or a newer
/// one taken, and a missing package taken as a failure.
#[test]
fn below_takes_the_newest_earlier_release_that_has_a_package() {
    let dir = scratch("below");
    let a = args(dir.path(), None, Some("0.1.9"));
    let files = repository(&["0.1.9", "0.1.10", "0.1.7", "0.1.6"]);
    let line = run(&a, &get_of(files)).unwrap();
    assert_eq!(line, "version=0.1.7");
    assert_eq!(output(dir.path()), "version=0.1.7\n");
    let written = std::fs::read(dir.path().join("dist/js-agent-0.1.7-macos-arm64.pkg")).unwrap();
    assert_eq!(written, bytes_of("0.1.7"));
    assert!(
        !dir.path()
            .join("dist/js-agent-0.1.8-macos-arm64.pkg")
            .exists()
    );
}

/// Asserts that a `--below` with no earlier release that has a package fails naming each
/// release it asked for and why, newest first, and that one with no earlier tag says so;
/// nothing is written either way.
/// Catches a test run against no earlier release at all.
#[test]
fn below_with_no_earlier_package_names_what_it_asked() {
    let dir = scratch("below-none");
    let a = args(dir.path(), None, Some("0.1.9"));
    let err = run(&a, &get_of(repository(&["0.1.9"]))).unwrap_err();
    assert_eq!(
        err.to_string(),
        "no release below 0.1.9 has a package in the package repository: \
         0.1.8 (status 403), 0.1.7 (status 403), 0.1.6 (status 403)"
    );
    let a = args(dir.path(), None, Some("0.1.6"));
    let err = run(&a, &get_of(repository(&[]))).unwrap_err();
    assert_eq!(
        err,
        Error::NoEarlierPackage {
            below: "0.1.6".to_owned(),
            tried: "no tag below it".to_owned(),
        }
    );
    assert!(!dir.path().join("output").exists());
    assert!(!dir.path().join("dist").exists());
}

/// Asserts that at most [`CANDIDATES`] earlier releases are asked for a package, newest first.
/// Catches a walk down every tag of the repository.
#[test]
fn below_asks_at_most_the_newest_candidates() {
    let dir = scratch("candidates");
    let a = args(dir.path(), None, Some("0.3.0"));
    let lines: String = (1..=30).map(|patch| format!("v0.2.{patch}\n")).collect();
    std::fs::write(a.tags.as_ref().unwrap(), lines).unwrap();
    let asked = std::cell::RefCell::new(Vec::new());
    let get = |url: &str| {
        asked.borrow_mut().push(url.to_owned());
        Err::<Vec<u8>, String>("status 403".to_owned())
    };
    assert!(matches!(run(&a, &get), Err(Error::NoEarlierPackage { .. })));
    let asked = asked.into_inner();
    assert_eq!(asked.len(), CANDIDATES);
    assert_eq!(
        asked.first().map(String::as_str),
        Some("https://packages.example/pkg/jobscheduler/0.2.30/SHA256SUMS")
    );
    assert_eq!(
        asked.last().map(String::as_str),
        Some("https://packages.example/pkg/jobscheduler/0.2.21/SHA256SUMS")
    );
}

/// Asserts that a package whose SHA-256 is not the one its `SHA256SUMS` lists, sums that list
/// no line for its file, sums that are not text, a package that cannot be downloaded and a
/// version whose sums cannot be downloaded each fail naming the file, with nothing written.
/// Catches a package taken without its sum, or written before it was checked.
#[test]
fn a_package_that_does_not_match_its_sums_or_cannot_be_read_is_refused() {
    let dir = scratch("refused");
    let a = args(dir.path(), Some("0.1.9"), None);
    let sums = "https://packages.example/pkg/jobscheduler/0.1.9/SHA256SUMS";
    let pkg = "https://packages.example/pkg/jobscheduler/0.1.9/js-agent-0.1.9-macos-arm64.pkg";

    let mut files = repository(&["0.1.9"]);
    files.insert(pkg.to_owned(), b"other bytes".to_vec());
    let err = run(&a, &get_of(files)).unwrap_err();
    assert_eq!(
        err,
        Error::PackageSum {
            url: pkg.to_owned(),
            found: hex::encode(sha2::Sha256::digest(b"other bytes")),
            listed: hex::encode(sha2::Sha256::digest(bytes_of("0.1.9"))),
        }
    );

    let mut files = repository(&["0.1.9"]);
    files.insert(
        sums.to_owned(),
        sum_line("0.1.8", &bytes_of("0.1.9")).into_bytes(),
    );
    let err = run(&a, &get_of(files)).unwrap_err();
    assert_eq!(
        err.to_string(),
        format!("{sums} lists no js-agent-0.1.9-macos-arm64.pkg")
    );

    let mut files = repository(&["0.1.9"]);
    files.insert(sums.to_owned(), vec![0xff, 0xfe]);
    let err = run(&a, &get_of(files)).unwrap_err();
    assert_eq!(err.to_string(), format!("{sums}: not UTF-8 text"));

    let mut files = repository(&["0.1.9"]);
    files.remove(pkg);
    let err = run(&a, &get_of(files)).unwrap_err();
    assert_eq!(err.to_string(), format!("{pkg}: status 403"));

    let err = run(&a, &get_of(repository(&[]))).unwrap_err();
    assert_eq!(err.to_string(), format!("{sums}: status 403"));

    assert!(!dir.path().join("dist").exists());
    assert!(!dir.path().join("output").exists());
}

/// Asserts that a version that is not `MAJOR.MINOR.PATCH` is refused before anything is
/// downloaded, for `--version` and `--below`, that neither flag is a usage error, that tags that
/// cannot be read and an output or a directory that cannot be written fail naming the path.
/// Catches a version put in a URL unchecked.
#[test]
fn a_bad_version_missing_flags_and_unwritable_paths_are_refused() {
    let dir = scratch("bad");
    let get = |url: &str| -> Result<Vec<u8>, String> { panic!("downloaded {url}") };
    for (version, below) in [(Some("0.1"), None), (None, Some("v0.1.9"))] {
        let err = run(&args(dir.path(), version, below), &get).unwrap_err();
        assert!(matches!(err, Error::NotAVersion(_)), "{err}");
    }
    let neither = MacosPkgArgs {
        tags: None,
        ..args(dir.path(), None, Some("0.1.9"))
    };
    assert_eq!(
        run(&neither, &get).unwrap_err(),
        Error::Usage("give --version, or --below with --tags".to_owned())
    );
    let no_tags = MacosPkgArgs {
        tags: Some(dir.path().join("absent")),
        ..args(dir.path(), None, Some("0.1.9"))
    };
    assert!(matches!(
        run(&no_tags, &get).unwrap_err(),
        Error::Io { path, .. } if path.ends_with("absent")
    ));

    let files = || get_of(repository(&["0.1.9"]));
    let blocked = dir.path().join("a-file");
    std::fs::write(&blocked, "").unwrap();
    let out_in_a_file = MacosPkgArgs {
        out: blocked.join("dist"),
        ..args(dir.path(), Some("0.1.9"), None)
    };
    assert!(matches!(
        run(&out_in_a_file, &files()).unwrap_err(),
        Error::Io { .. }
    ));
    let out_is_taken = MacosPkgArgs {
        out: dir.path().join("taken"),
        ..args(dir.path(), Some("0.1.9"), None)
    };
    std::fs::create_dir_all(dir.path().join("taken/js-agent-0.1.9-macos-arm64.pkg")).unwrap();
    assert!(matches!(
        run(&out_is_taken, &files()).unwrap_err(),
        Error::Io { .. }
    ));
    let output_in_a_file = MacosPkgArgs {
        output: blocked.join("output"),
        ..args(dir.path(), Some("0.1.9"), None)
    };
    assert!(matches!(
        run(&output_in_a_file, &files()).unwrap_err(),
        Error::Io { .. }
    ));
}
