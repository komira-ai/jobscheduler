//! A job's outputs in the store: the rules of an output path and of a declaration, the keys,
//! the manifest's encoding and its checks, and a presigned GET the fake store answers.

use std::time::Duration;

use js_logstore::outputs::{
    MAX_DECLARED, MAX_MANIFEST_BYTES, MAX_PATH_LEN, Manifest, ManifestError, OutputFailure,
    OutputFile, OutputPath, OutputPathError, check_declared, presigned_get,
};
use js_logstore::{HttpRequest, JobPrefix, Metadata, Method, Transport};

use crate::support::{ALL, Rig, policy};

const SHA: &str = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";

fn path(text: &str) -> OutputPath {
    OutputPath::parse(text).unwrap()
}

/// Asserts the paths that are outputs and each refusal: empty or past 512 bytes, an empty, `.`
/// or `..` segment, a segment past 255 bytes, a byte outside `[A-Za-z0-9._-]` (a space, a
/// colon, a non-ASCII byte), a leading or trailing `/`; and the refusals' messages. It catches a
/// path that would leave the job's directory or that a prefix grant could not put.
#[test]
fn an_output_path_is_segments_a_key_may_hold() {
    for good in ["a", "out/report.json", "A-b_c.d/e", &"x".repeat(255)] {
        assert_eq!(OutputPath::parse(good).unwrap().as_str(), good);
    }
    let long = format!("{}/{}", "a".repeat(255), "b".repeat(MAX_PATH_LEN - 256 + 1));
    assert_eq!(long.len(), MAX_PATH_LEN + 1);
    for (bad, why) in [
        ("", OutputPathError::Length),
        (long.as_str(), OutputPathError::Length),
        ("/a", OutputPathError::Segment),
        ("a/", OutputPathError::Segment),
        ("a//b", OutputPathError::Segment),
        (".", OutputPathError::Segment),
        ("a/../b", OutputPathError::Segment),
        ("a b", OutputPathError::Segment),
        ("a:b", OutputPathError::Segment),
        ("caf\u{e9}", OutputPathError::Segment),
    ] {
        assert_eq!(OutputPath::parse(bad), Err(why), "{bad:?}");
    }
    let segment = "x".repeat(256);
    assert_eq!(OutputPath::parse(&segment), Err(OutputPathError::Segment));
    let messages = [
        (OutputPathError::Length, "is empty or longer than 512 bytes"),
        (
            OutputPathError::Segment,
            "has a segment that is empty, . or .., longer than 255 bytes, or holds a byte outside [A-Za-z0-9._-]",
        ),
        (OutputPathError::Twice, "is declared twice"),
        (
            OutputPathError::Nested,
            "is inside another output, or another is inside it",
        ),
        (
            OutputPathError::TooMany,
            "is past the 64 outputs a job may declare",
        ),
    ];
    for (why, text) in messages {
        assert_eq!(why.to_string(), text);
    }
}

/// Asserts a path's join, its display, its segments and what is within it: a path is within
/// itself and its parents, not within a sibling that shares its first bytes. It catches
/// `out2` read as inside `out`.
#[test]
fn a_path_knows_what_is_within_it() {
    let out = path("out");
    let deep = out.join("report.json").unwrap();
    assert_eq!(deep.to_string(), "out/report.json");
    assert_eq!(deep.segments().collect::<Vec<_>>(), ["out", "report.json"]);
    assert!(deep.within(&out) && out.within(&out));
    assert!(!out.within(&deep));
    assert!(!path("out2").within(&out));
    assert_eq!(out.join("a b"), Err(OutputPathError::Segment));
}

/// Asserts a declaration: paths are checked in order, the first at fault is named with why (a
/// bad path, a repeat, one inside an earlier one, one an earlier one is inside), and more than 64
/// are refused at the 65th. It catches two declared paths that would upload one file twice.
#[test]
fn a_declaration_names_its_first_path_at_fault() {
    let list = |items: &[&str]| {
        items
            .iter()
            .map(|item| (*item).to_owned())
            .collect::<Vec<_>>()
    };
    let ok = check_declared(&list(&["out", "report.json", "outer/x"])).unwrap();
    assert_eq!(ok, [path("out"), path("report.json"), path("outer/x")]);
    assert_eq!(check_declared(&[]), Ok(Vec::new()));
    let cases = [
        (list(&["a", ""]), (1, OutputPathError::Length)),
        (list(&["a", "b", "a"]), (2, OutputPathError::Twice)),
        (list(&["a", "a/b"]), (1, OutputPathError::Nested)),
        (list(&["a/b", "a"]), (1, OutputPathError::Nested)),
    ];
    for (declared, want) in cases {
        assert_eq!(check_declared(&declared), Err(want), "{declared:?}");
    }
    let many: Vec<String> = (0..=MAX_DECLARED).map(|at| format!("f{at}")).collect();
    assert_eq!(
        check_declared(&many),
        Err((MAX_DECLARED, OutputPathError::TooMany))
    );
    assert!(check_declared(&many[..MAX_DECLARED]).is_ok());
}

/// Asserts the keys: a file at `P/outputs/<path>` and the manifest at `P/outputs.json`, beside
/// the log keys and apart from them. It catches an output key that a log key could equal.
#[test]
fn outputs_are_keyed_under_the_jobs_prefix() {
    let prefix = JobPrefix::new("logs/js-0011", "j-1").unwrap();
    assert_eq!(
        prefix.output_key(&path("out/report.json")),
        "logs/js-0011/j-1/outputs/out/report.json"
    );
    assert_eq!(
        prefix.outputs_manifest_key(),
        "logs/js-0011/j-1/outputs.json"
    );
}

/// A manifest of each kind of entry.
fn manifest() -> Manifest {
    Manifest {
        version: 1,
        files: vec![OutputFile {
            path: "out/report.json".to_owned(),
            bytes: 12,
            sha256: SHA.to_owned(),
        }],
        missing: vec!["coverage.lcov".to_owned()],
        failed: vec![OutputFailure {
            path: "out/core".to_owned(),
            reason: "too-large".to_owned(),
        }],
    }
}

/// Asserts that a manifest reads back as written, by its exact JSON; that fields a reader does
/// not know are ignored and the lists may be left out; and that files within a declared
/// directory and declared files are both accepted. It catches a manifest a newer agent writes
/// that an older server cannot read.
#[test]
fn a_manifest_reads_back_and_ignores_what_it_does_not_know() {
    let declared = [path("out"), path("coverage.lcov")];
    let written = manifest().encode();
    assert_eq!(
        String::from_utf8(written.clone()).unwrap(),
        format!(
            r#"{{"version":1,"files":[{{"path":"out/report.json","bytes":12,"sha256":"{SHA}"}}],"missing":["coverage.lcov"],"failed":[{{"path":"out/core","reason":"too-large"}}]}}"#
        )
    );
    assert_eq!(Manifest::decode(&written, &declared), Ok(manifest()));
    let newer = br#"{"version":1,"later":true,"files":[]}"#;
    assert_eq!(
        Manifest::decode(newer, &declared),
        Ok(Manifest {
            version: 1,
            ..Manifest::default()
        })
    );
}

/// Asserts each manifest a reader refuses, by its whole message: over 1 MiB, not JSON, another
/// version, a file outside every declared path, a file whose path is not an output path, and a
/// digest that is not 64 lower-case hex digits. It catches a manifest that would hand a client a
/// key outside the job's outputs.
#[test]
fn a_manifest_that_is_not_the_jobs_is_refused() {
    let declared = [path("out")];
    let long = vec![b' '; MAX_MANIFEST_BYTES + 1];
    assert_eq!(
        Manifest::decode(&long, &declared),
        Err(ManifestError::TooLong)
    );
    assert_eq!(
        ManifestError::TooLong.to_string(),
        "the manifest is longer than 1048576 bytes"
    );
    let Err(ManifestError::Malformed(why)) = Manifest::decode(b"[", &declared) else {
        panic!("not malformed");
    };
    assert!(why.starts_with("EOF while parsing"), "{why}");
    assert_eq!(
        Manifest::decode(br#"{"version":2}"#, &declared),
        Err(ManifestError::Version(2))
    );
    assert_eq!(
        ManifestError::Version(2).to_string(),
        "the manifest is of version 2, not 1"
    );
    let file = |path: &str, sha: &str| {
        let mut manifest = manifest();
        manifest.files[0].path = path.to_owned();
        manifest.files[0].sha256 = sha.to_owned();
        manifest.encode()
    };
    for (stray, sha) in [
        ("other/x", SHA),
        ("out/../etc", SHA),
        ("out/x", "ABC"),
        ("out/x", &SHA.to_uppercase()),
    ] {
        assert_eq!(
            Manifest::decode(&file(stray, sha), &declared),
            Err(ManifestError::Stray(stray.to_owned())),
            "{stray} {sha}"
        );
    }
    assert_eq!(
        ManifestError::Stray("x".to_owned()).to_string(),
        "the manifest lists a file that is not an output of the job: \"x\""
    );
}

/// Asserts that a presigned GET reads the object it names from the fake store with no further
/// signing, path-style and virtual-host style, and nothing else: another key with the same
/// signature, and the same URL past its expiry, are refused. It catches a URL that reads more
/// than one object or outlives what it was signed for.
#[test]
fn a_presigned_get_reads_one_object_until_it_expires() {
    for path_style in [true, false] {
        let rig = Rig::with_style(path_style);
        let admin = rig.admin();
        admin
            .put("p/outputs/a", b"bytes".to_vec(), &Metadata::new())
            .unwrap();
        admin
            .put("p/outputs/b", b"other".to_vec(), &Metadata::new())
            .unwrap();
        let reader = rig.fake.issue(policy(&ALL, "p/"));
        let now = js_logstore::Clock::now(&*rig.clock);
        let url = presigned_get(&rig.endpoint, &reader, "p/outputs/a", now, 60);
        let get = |url: &str| {
            let request = HttpRequest {
                method: Method::Get,
                url: url.to_owned(),
                headers: vec![("host".to_owned(), rig.endpoint.host())],
                body: Vec::new(),
            };
            rig.fake.send(request).unwrap()
        };
        let answer = get(&url);
        assert_eq!(
            (answer.status, answer.body.as_slice()),
            (200, &b"bytes"[..])
        );
        let other = get(&url.replace("outputs/a", "outputs/b"));
        assert_eq!(other.status, 403);
        rig.clock.advance(Duration::from_secs(61));
        assert_eq!(get(&url).status, 403);
    }
}
