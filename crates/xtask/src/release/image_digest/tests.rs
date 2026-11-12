//! `release image-digest`: the digest and the two addresses of a release whose files are as
//! published, and each refusal, by its full message.

use js_testkit::dir::TestDir;

use super::*;
use crate::release::server_image::build;

const VERSION: Version = Version {
    major: 1,
    minor: 2,
    patch: 3,
};

const IMAGE: &str = "js-lease-server-1.2.3-x86_64-unknown-linux-musl.oci.tar";
const BUNDLE: &str = "js-lease-server-1.2.3-x86_64-unknown-linux-musl.oci.tar.sigstore.json";
const BASE: &str = "https://github.com/komira-ai/jobscheduler/releases/download/v1.2.3";

/// A scratch directory of its own for one test.
fn scratch(name: &str) -> TestDir {
    TestDir::under(std::env::temp_dir(), name)
}

/// The linux-x86_64 image of a binary that holds [`VERSION`], built as `release image` builds it.
fn built() -> crate::release::server_image::Image {
    build(VERSION, Target::X86_64, b"\x7fELF....\x001.2.3\x00").expect("the image builds")
}

/// `dir` laid out as `publish` leaves `dist`: the image, its bundle, the arm64 image and a
/// `SHA256SUMS` over both images (the arm64 one in binary mode), as `sha256sum` writes it.
fn lay_out(dir: &Path, image: &[u8]) {
    let other = "js-lease-server-1.2.3-aarch64-unknown-linux-musl.oci.tar";
    std::fs::write(dir.join(IMAGE), image).expect("image");
    std::fs::write(dir.join(other), b"arm64").expect("other image");
    std::fs::write(dir.join(BUNDLE), b"{\"bundle\":true}").expect("bundle");
    let sums = format!(
        "{}  {other}\n{} *{IMAGE}\n",
        sha256_hex(b"arm64"),
        sha256_hex(image)
    );
    std::fs::write(dir.join(SUMS), sums).expect("sums");
}

fn args(dir: &Path, output: Option<PathBuf>) -> DigestArgs {
    DigestArgs {
        dist: dir.to_path_buf(),
        repo: "komira-ai/jobscheduler".to_owned(),
        version: VERSION,
        output,
    }
}

/// Asserts that a release laid out as published gives the image's manifest digest (the one
/// `release image` reports) and the release's addresses of the image and the bundle under the
/// tag `v1.2.3`, and appends exactly those three lines to `--output` after what it held.
#[test]
fn a_published_release_gives_the_digest_and_both_addresses() {
    let dir = scratch("image-digest-ok");
    let image = built();
    lay_out(&dir, &image.tar);
    let output = dir.join("github-output");
    std::fs::write(&output, "earlier=kept\n").expect("output");
    let lines = run(&args(&dir, Some(output.clone()))).expect("the digest");
    let want = format!(
        "digest=sha256:{}\nimage-url={BASE}/{IMAGE}\nbundle-url={BASE}/{BUNDLE}",
        image.manifest_digest
    );
    assert_eq!(lines, want);
    assert_eq!(
        std::fs::read_to_string(&output).expect("output"),
        format!("earlier=kept\n{want}\n")
    );
}

/// Asserts that without `--output` the lines are returned and no file is written.
#[test]
fn without_output_nothing_is_written() {
    let dir = scratch("image-digest-no-output");
    lay_out(&dir, &built().tar);
    let before = std::fs::read_dir(&*dir).expect("dir").count();
    assert!(run(&args(&dir, None)).is_ok());
    assert_eq!(std::fs::read_dir(&*dir).expect("dir").count(), before);
}

/// Asserts that a repository that is not `<owner>/<name>` of plain names is refused before
/// anything is read (the directory does not exist, and the error is not about it).
#[test]
fn a_repository_that_is_not_two_plain_names_is_refused_first() {
    let dir = scratch("image-digest-repo");
    let missing = dir.join("missing");
    for repo in [
        "jobscheduler",
        "komira-ai/job/scheduler",
        "komira-ai/",
        "/jobscheduler",
        ".hidden/jobscheduler",
        "komira-ai/job scheduler",
        "komira-ai/job?x=1",
    ] {
        let mut a = args(&missing, None);
        a.repo = repo.to_owned();
        let e = run(&a).expect_err(repo);
        assert_eq!(
            e.to_string(),
            format!(
                "--repo {repo:?} is not <owner>/<name> of ASCII letters, digits, `.`, `_` and `-`, each starting with a letter or digit"
            )
        );
    }
}

/// One change to a laid-out release directory.
type Change = Box<dyn Fn(&Path)>;

/// Asserts the refusals of the files around the image: no `SHA256SUMS`, a line of it that is not
/// `<sha256>  <name>`, the image listed no time or twice, an image that is not the listed file,
/// no bundle and an empty bundle; and that a refusal appends nothing to `--output`.
#[test]
fn the_sums_and_the_bundle_are_held_to_the_image() {
    let image = built().tar;
    let digest = sha256_hex(&image);
    let cases: Vec<(&str, Change, String)> = vec![
        (
            "no-sums",
            Box::new(|d: &Path| std::fs::remove_file(d.join(SUMS)).expect("rm")),
            "SHA256SUMS: No such file or directory (os error 2)".to_owned(),
        ),
        (
            "bad-line",
            Box::new(|d: &Path| {
                let sums = std::fs::read_to_string(d.join(SUMS)).expect("sums");
                std::fs::write(d.join(SUMS), format!("{sums}not a sum\n")).expect("sums");
            }),
            "SHA256SUMS line 3 is not `<sha256>  <name>`: \"not a sum\"".to_owned(),
        ),
        (
            "short-digest",
            Box::new(|d: &Path| {
                std::fs::write(d.join(SUMS), format!("abc  {IMAGE}\n")).expect("sums")
            }),
            format!("SHA256SUMS line 1 is not `<sha256>  <name>`: \"abc  {IMAGE}\""),
        ),
        (
            "one-space",
            Box::new(|d: &Path| {
                std::fs::write(d.join(SUMS), format!("{}\t{IMAGE}\n", "a".repeat(64)))
                    .expect("sums");
            }),
            format!(
                "SHA256SUMS line 1 is not `<sha256>  <name>`: \"{}\\t{IMAGE}\"",
                "a".repeat(64)
            ),
        ),
        (
            "unlisted",
            Box::new(|d: &Path| std::fs::write(d.join(SUMS), "").expect("sums")),
            format!("SHA256SUMS lists {IMAGE} 0 times, not once"),
        ),
        (
            "twice",
            Box::new(|d: &Path| {
                let sums = std::fs::read_to_string(d.join(SUMS)).expect("sums");
                std::fs::write(d.join(SUMS), format!("{sums}{sums}")).expect("sums");
            }),
            format!("SHA256SUMS lists {IMAGE} 2 times, not once"),
        ),
        (
            "other-bytes",
            Box::new(|d: &Path| std::fs::write(d.join(IMAGE), b"other").expect("image")),
            format!(
                "{IMAGE} has sha256 {}, not the {digest} SHA256SUMS lists",
                sha256_hex(b"other")
            ),
        ),
        (
            "no-image",
            Box::new(|d: &Path| std::fs::remove_file(d.join(IMAGE)).expect("rm")),
            format!("{IMAGE}: No such file or directory (os error 2)"),
        ),
        (
            "no-bundle",
            Box::new(|d: &Path| std::fs::remove_file(d.join(BUNDLE)).expect("rm")),
            format!("{BUNDLE}: No such file or directory (os error 2)"),
        ),
        (
            "empty-bundle",
            Box::new(|d: &Path| std::fs::write(d.join(BUNDLE), b"").expect("bundle")),
            format!("{BUNDLE} is empty"),
        ),
    ];
    for (name, change, says) in cases {
        let dir = scratch(&format!("image-digest-{name}"));
        lay_out(&dir, &image);
        change(&dir);
        let output = dir.join("github-output");
        let e = run(&args(&dir, Some(output.clone()))).expect_err(name);
        // An I/O error names the full path; the rest of the message is compared whole.
        let shown = e.to_string().replace(&format!("{}/", dir.display()), "");
        assert_eq!(shown, says, "{name}");
        assert!(!output.exists(), "{name}");
    }
}

/// Asserts that an output file that cannot be opened for appending is an I/O error naming it.
#[test]
fn an_output_that_cannot_be_written_is_an_error() {
    let dir = scratch("image-digest-output-dir");
    lay_out(&dir, &built().tar);
    let e = run(&args(&dir, Some(dir.to_path_buf()))).expect_err("a directory");
    assert!(
        matches!(&e, Error::Release(crate::release::Error::Io { path, .. }) if *path == dir.display().to_string()),
        "{e}"
    );
}

/// A tar of `files` (path and content), with a directory entry first.
fn tar_of(files: &[(&str, &[u8])]) -> Vec<u8> {
    let mut builder = tar::Builder::new(Vec::new());
    let mut dir = tar::Header::new_ustar();
    dir.set_entry_type(tar::EntryType::Directory);
    dir.set_mode(0o755);
    dir.set_size(0);
    builder
        .append_data(&mut dir, "blobs/", std::io::empty())
        .expect("dir");
    for (path, content) in files {
        let mut header = tar::Header::new_ustar();
        header.set_mode(0o644);
        header.set_size(content.len() as u64);
        builder
            .append_data(&mut header, path, *content)
            .expect("file");
    }
    builder.into_inner().expect("tar")
}

/// Asserts the digest of a layout written here: the one manifest's digest, its blob present and
/// hashing to it; and each way a layout fails that, by its full reason: not a tar, no
/// `index.json`, an `index.json` that is not JSON, no `manifests`, none or two manifests, a digest
/// that is not `sha256:<64 hex>`, no blob, and a blob that hashes to something else.
#[test]
fn the_manifest_digest_is_the_one_manifest_whose_blob_matches() {
    let manifest = br#"{"schemaVersion":2}"#;
    let hex = sha256_hex(manifest);
    let blob = format!("blobs/sha256/{hex}");
    let one = format!(r#"{{"manifests":[{{"digest":"sha256:{hex}"}}]}}"#);
    assert_eq!(
        manifest_digest(&tar_of(&[
            ("index.json", one.as_bytes()),
            (&blob, manifest)
        ])),
        Ok(format!("sha256:{hex}"))
    );
    let two =
        format!(r#"{{"manifests":[{{"digest":"sha256:{hex}"}},{{"digest":"sha256:{hex}"}}]}}"#);
    let upper = format!(
        r#"{{"manifests":[{{"digest":"sha256:{}"}}]}}"#,
        hex.to_uppercase()
    );
    let other_hash = format!("blobs/sha256/{hex} has sha256 {}", sha256_hex(b"other"));
    // A tar whose first header no longer matches its checksum.
    let mut torn = tar_of(&[("index.json", one.as_bytes())]);
    torn[0] = b'X';
    let cases: Vec<(Vec<u8>, String)> = vec![
        (
            torn,
            "not a tar: archive header checksum mismatch".to_owned(),
        ),
        (
            tar_of(&[(&blob, manifest)]),
            "the layout has no index.json".to_owned(),
        ),
        (
            tar_of(&[("index.json", b"{")]),
            "index.json is not JSON: EOF while parsing an object at line 1 column 1".to_owned(),
        ),
        (
            tar_of(&[("index.json", b"{}")]),
            "index.json names 0 manifests, not one".to_owned(),
        ),
        (
            tar_of(&[("index.json", br#"{"manifests":[]}"#)]),
            "index.json names 0 manifests, not one".to_owned(),
        ),
        (
            tar_of(&[("index.json", two.as_bytes()), (&blob, manifest)]),
            "index.json names 2 manifests, not one".to_owned(),
        ),
        (
            tar_of(&[("index.json", br#"{"manifests":[{}]}"#)]),
            "index.json names the manifest \"\", not sha256:<64 hex>".to_owned(),
        ),
        (
            tar_of(&[("index.json", upper.as_bytes())]),
            format!(
                "index.json names the manifest \"sha256:{}\", not sha256:<64 hex>",
                hex.to_uppercase()
            ),
        ),
        (
            tar_of(&[("index.json", one.as_bytes())]),
            format!("the layout has no {blob}"),
        ),
        (
            tar_of(&[("index.json", one.as_bytes()), (&blob, b"other")]),
            other_hash,
        ),
    ];
    for (bytes, says) in cases {
        assert_eq!(manifest_digest(&bytes), Err(says));
    }
}

/// Asserts that an image listed and present but not an OCI layout is refused naming the file.
#[test]
fn a_listed_file_that_is_not_an_image_is_refused_by_name() {
    let dir = scratch("image-digest-not-an-image");
    let bytes = tar_of(&[("index.json", b"{}")]);
    lay_out(&dir, &bytes);
    assert_eq!(
        run(&args(&dir, None)).expect_err("no manifest").to_string(),
        format!("{IMAGE}: index.json names 0 manifests, not one")
    );
}
