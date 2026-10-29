//! The placeholder grammar and its normalisation.

use std::collections::BTreeMap;

use std::collections::BTreeMap;

use js_bundle::{ExpandError, MAX_VALUE_BYTES, Segment, Template, TemplateError, TreeName};

fn tree(name: &str) -> TreeName {
    TreeName::parse(name).unwrap()
}

/// Values that are accepted, among them ones a careless parser would refuse. `$HOME`
/// without a brace is text: nothing but the two placeholders expands.
#[test]
fn accepted_values() {
    let long = "x".repeat(MAX_VALUE_BYTES);
    for ok in [
        "",
        "plain",
        "$HOME",
        "{tree:a}",
        "${tree:a}",
        "${tree:a}/bin",
        "${tree:a}/bin:${tree:b}/lib",
        "-I${tree:a}/include",
        "--sysroot=${tree:sdk}/usr",
        "${developer_dir}/usr/bin",
        "/usr/bin",
        "${tree:a}/.hidden/..x/x.",
        long.as_str(),
    ] {
        assert_eq!(Template::parse(ok).unwrap().as_str(), ok, "{ok:?}");
    }
}

/// Values that are refused, each for its own reason.
#[test]
fn refused_values() {
    let rows: Vec<(String, TemplateError)> = vec![
        ("${tree:a}/../x".into(), TemplateError::ParentDir),
        ("${tree:a}/x/..".into(), TemplateError::ParentDir),
        ("/a/../b".into(), TemplateError::ParentDir),
        ("${tree:a}/./x".into(), TemplateError::CurrentDir),
        ("${tree:a}//x".into(), TemplateError::EmptyComponent),
        ("${tree:a}/".into(), TemplateError::EmptyComponent),
        ("/".into(), TemplateError::EmptyComponent),
        ("//x".into(), TemplateError::EmptyComponent),
        ("${tree:a}x".into(), TemplateError::Attached),
        ("${tree:a},${tree:b}".into(), TemplateError::Attached),
        ("${tree:a}${tree:b}".into(), TemplateError::Attached),
        ("${tree:a}${developer_dir}".into(), TemplateError::Attached),
        ("${developer_dir}${tree:a}".into(), TemplateError::Attached),
        ("${tree:A}".into(), TemplateError::BadTreeName("A".into())),
        ("${tree:}".into(), TemplateError::BadTreeName("".into())),
        ("${tree:a".into(), TemplateError::Unterminated),
        ("${".into(), TemplateError::Unterminated),
        ("${HOME}".into(), TemplateError::Unknown("HOME".into())),
        ("${}".into(), TemplateError::Unknown("".into())),
        ("${tree}".into(), TemplateError::Unknown("tree".into())),
        (
            "${developer_dir }".into(),
            TemplateError::Unknown("developer_dir ".into()),
        ),
        (
            "${Developer_dir}".into(),
            TemplateError::Unknown("Developer_dir".into()),
        ),
        ("a\0b".into(), TemplateError::Nul),
        ("a\nb".into(), TemplateError::Newline),
        ("a\rb".into(), TemplateError::Newline),
        (
            "x".repeat(MAX_VALUE_BYTES + 1),
            TemplateError::TooLong {
                max: MAX_VALUE_BYTES,
            },
        ),
    ];
    for (text, want) in rows {
        assert_eq!(Template::parse(&text), Err(want), "{text:?}");
    }
}

/// The pieces, the trees named and the developer-dir flag are readable.
#[test]
fn accessors_describe_the_parse() {
    let t = Template::parse("-I${tree:a}/x:${tree:b}/y").unwrap();
    assert_eq!(
        t.segments(),
        [
            Segment::Lit("-I".into()),
            Segment::Tree(tree("a")),
            Segment::Lit("/x:".into()),
            Segment::Tree(tree("b")),
            Segment::Lit("/y".into()),
        ]
    );
    let names: Vec<&str> = t.trees().map(|n| n.as_str()).collect();
    assert_eq!(names, ["a", "b"]);
    assert!(!t.uses_developer_dir());
    assert!(
        Template::parse("${developer_dir}/bin")
            .unwrap()
            .uses_developer_dir()
    );
}

/// Expansion substitutes the directories and reports what is missing.
#[test]
fn expansion_substitutes_and_reports_missing_pieces() {
    let trees = BTreeMap::from([
        (tree("a"), "/s/a".to_string()),
        (tree("b"), "/s/b".to_string()),
    ]);
    let expand = |text: &str, dd: Option<&str>| Template::parse(text).unwrap().expand(&trees, dd);
    assert_eq!(
        expand("-I${tree:a}/i:${tree:b}/j", None),
        Ok("-I/s/a/i:/s/b/j".to_string())
    );
    assert_eq!(
        expand("${developer_dir}/bin", Some("/X")),
        Ok("/X/bin".to_string())
    );
    assert_eq!(expand("plain", None), Ok("plain".to_string()));
    assert_eq!(
        expand("${tree:c}/x", None),
        Err(ExpandError::UnknownTree(tree("c")))
    );
    assert_eq!(
        expand("${developer_dir}", None),
        Err(ExpandError::NoDeveloperDir)
    );
}
