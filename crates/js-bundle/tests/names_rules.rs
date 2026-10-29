//! The naming rules both the packer and the extractor apply.

use js_bundle::{Defect, check_path, check_target};

fn long(count: usize) -> String {
    "a".repeat(count)
}

/// Paths a layer holds are admitted, up to the caps.
#[test]
fn ordinary_paths_and_paths_at_the_caps_are_admitted() {
    for path in ["a", "bin/tool", "a.b/c-d_e/+x~", ".hidden", "a/.fseventsd"] {
        assert_eq!(check_path(path.as_bytes()), Ok(path), "{path}");
    }
    let widest_component = long(255);
    assert_eq!(
        check_path(widest_component.as_bytes()),
        Ok(widest_component.as_str())
    );
    let longest = format!("{}/{}/a", long(255), long(254));
    assert_eq!(longest.len(), 512);
    assert_eq!(check_path(longest.as_bytes()), Ok(longest.as_str()));
    let deepest = vec!["a"; 64].join("/");
    assert_eq!(check_path(deepest.as_bytes()), Ok(deepest.as_str()));
}

/// Each way a path can break the rules has its own defect.
#[test]
fn each_broken_path_rule_is_named() {
    let too_long = format!("{}/{}/ab", long(255), long(254));
    let too_deep = vec!["a"; 65].join("/");
    let long_component = long(256);
    let cases: Vec<(&[u8], Defect)> = vec![
        (b"", Defect::Empty),
        (b"/etc/passwd", Defect::Absolute),
        (b"a/../b", Defect::DotComponent),
        (b"../a", Defect::DotComponent),
        (b"./a", Defect::DotComponent),
        (b".", Defect::DotComponent),
        (b"a//b", Defect::EmptyComponent),
        (b"a/", Defect::EmptyComponent),
        (b"a b", Defect::ForbiddenByte),
        (b"a\tb", Defect::ForbiddenByte),
        (b"a\x7fb", Defect::ForbiddenByte),
        (b"a\0b", Defect::ForbiddenByte),
        (b"\xff\xfe", Defect::ForbiddenByte),
        ("caf\u{e9}".as_bytes(), Defect::ForbiddenByte),
        (long_component.as_bytes(), Defect::ComponentTooLong),
        (too_long.as_bytes(), Defect::TooLong),
        (too_deep.as_bytes(), Defect::TooDeep),
        (b"._x", Defect::AppleDouble),
        (b"a/._x", Defect::AppleDouble),
    ];
    for (path, defect) in cases {
        assert_eq!(
            check_path(path),
            Err(defect),
            "{}",
            String::from_utf8_lossy(path)
        );
    }
}

/// The names APFS tools treat specially are refused at the root and nowhere else.
#[test]
fn volume_names_are_refused_at_the_tree_root_only() {
    for name in [
        ".fseventsd",
        ".Spotlight-V100",
        ".Trashes",
        ".DocumentRevisions-V100",
        ".TemporaryItems",
        ".metadata_never_index",
    ] {
        assert_eq!(check_path(name.as_bytes()), Err(Defect::RootReserved));
        let below = format!("{name}/x");
        assert_eq!(check_path(below.as_bytes()), Err(Defect::RootReserved));
        let nested = format!("dir/{name}");
        assert_eq!(check_path(nested.as_bytes()), Ok(nested.as_str()));
    }
}

/// Targets are relative and short; where `..` leads is the walker's question.
#[test]
fn targets_are_checked_for_shape_only() {
    let widest = long(255);
    let part = long(200);
    let longest = format!("{part}/{part}/{part}/{part}/{part}/{}", "b".repeat(19));
    assert_eq!(longest.len(), 1024);
    for target in [
        "tool",
        "../lib",
        "a/b/./c",
        "..",
        "a/..",
        widest.as_str(),
        longest.as_str(),
    ] {
        assert_eq!(check_target(target.as_bytes()), Ok(target));
    }
    let too_long = format!("{longest}b");
    let long_component = long(256);
    let cases: Vec<(&[u8], Defect)> = vec![
        (b"", Defect::Empty),
        (b"/etc", Defect::Absolute),
        (b"a//b", Defect::EmptyComponent),
        (b"a/", Defect::EmptyComponent),
        (b"a b", Defect::ForbiddenByte),
        (b"a\0b", Defect::ForbiddenByte),
        (b"\xff", Defect::ForbiddenByte),
        (long_component.as_bytes(), Defect::ComponentTooLong),
        (too_long.as_bytes(), Defect::TooLong),
    ];
    for (target, defect) in cases {
        assert_eq!(
            check_target(target),
            Err(defect),
            "{}",
            String::from_utf8_lossy(target)
        );
    }
}

/// The error text names the rule and shows the path escaped, so a path cannot split a log
/// line.
#[test]
fn a_refusal_shows_the_path_escaped() {
    use js_bundle::TreeRefusal;
    let refusal = TreeRefusal::Path {
        path: "a\nb".to_string(),
        defect: Defect::ForbiddenByte,
    };
    assert_eq!(
        refusal.to_string(),
        r#"path "a\nb" has a byte outside 0x21..=0x7e"#
    );
}
