//! Hostile input: paths and port lists that must be refused, and paths that look like
//! profile syntax and must not change the profile.

mod common;

use std::collections::BTreeMap;
use std::ffi::OsStr;
use std::io::ErrorKind;
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};

use common::{Scratch, path, tree};
use js_mac_pool::seatbelt::{
    AbsPath, BUNDLE_PREFIX, Caps, MAX_PATH_BYTES, MAX_PORTS, Net, PathError, PinnedTree, Policy,
    Ports, PortsError, Profile, ReadModel, SlotLayout, ToolError, ToolPath, check_path_dirs,
};

#[test]
fn paths_that_are_not_plain_absolute_normal_text_are_refused() {
    let long = format!("/{}", "a".repeat(MAX_PATH_BYTES));
    let table: [(&str, PathError); 19] = [
        ("", PathError::NotAbsolute),
        ("relative/path", PathError::NotAbsolute),
        ("./run", PathError::NotAbsolute),
        ("/", PathError::Root),
        ("/a/../b", PathError::NotNormal),
        ("/a/..", PathError::NotNormal),
        ("/a/./b", PathError::NotNormal),
        ("/a//b", PathError::NotNormal),
        ("//a", PathError::NotNormal),
        ("/a/b/", PathError::NotNormal),
        ("/a\nb", PathError::Control),
        ("/a\rb", PathError::Control),
        ("/a\0b", PathError::Control),
        ("/a\tb", PathError::Control),
        ("/a\u{1b}[0mb", PathError::Control),
        ("/a\u{7f}b", PathError::Control),
        ("/var/folders/ab", PathError::Linked("var".to_owned())),
        ("/tmp/work", PathError::Linked("tmp".to_owned())),
        ("/etc/hosts", PathError::Linked("etc".to_owned())),
    ];
    for (text, expected) in table {
        assert_eq!(AbsPath::parse(text), Err(expected), "{text:?}");
    }
    assert_eq!(AbsPath::parse(&long), Err(PathError::TooLong));
}

#[test]
fn a_path_that_is_not_utf8_is_refused() {
    let bytes = OsStr::from_bytes(b"/Volumes/a\xffb");
    assert_eq!(AbsPath::new(Path::new(bytes)), Err(PathError::NotUtf8));
}

#[test]
fn the_longest_path_and_non_ascii_text_are_accepted() {
    let longest = format!("/{}", "a".repeat(MAX_PATH_BYTES - 1));
    assert_eq!(
        AbsPath::parse(&longest).expect("at the limit").as_str(),
        longest
    );
    let accented = AbsPath::parse("/Volumes/w\u{f6}rk/\u{fc}ber").expect("text");
    assert_eq!(accented.as_str(), "/Volumes/w\u{f6}rk/\u{fc}ber");
    assert_eq!(
        AbsPath::new(Path::new("/private/var/folders/ab"))
            .expect("canonical")
            .as_str(),
        "/private/var/folders/ab"
    );
}

#[test]
fn every_error_says_what_is_wrong() {
    let table = [
        (PathError::NotUtf8, "path is not valid UTF-8"),
        (PathError::TooLong, "path is too long"),
        (PathError::NotAbsolute, "path is not absolute"),
        (PathError::Control, "path contains a control character"),
        (PathError::Root, "path is the root directory"),
        (PathError::NotNormal, "path is not in normal form"),
        (
            PathError::Linked("var".to_owned()),
            "path starts with /var, a symbolic link; use the canonical /private spelling",
        ),
    ];
    for (error, text) in table {
        assert_eq!(error.to_string(), text);
    }
    assert_eq!(PortsError::Empty.to_string(), "no ports listed");
    assert_eq!(PortsError::Zero.to_string(), "port 0 listed");
    assert_eq!(PortsError::TooMany.to_string(), "too many ports listed");
}

/// Names that read like profile syntax. They are valid paths (a parameter value is never
/// part of the profile text), so the proof is that the profile is byte for byte the
/// profile of a plain path, and the hostile text is only a parameter value.
#[test]
fn paths_that_look_like_profile_syntax_never_enter_the_profile_text() {
    let hostile = [
        "/Volumes/a\"b",
        "/Volumes/a)b",
        "/Volumes/a(b",
        "/Volumes/a b",
        "/Volumes/a=b",
        "/Volumes/a;b",
        "/Volumes/a\\b",
        "/Volumes/x\"))(allow file-write* (subpath \"/\"))((\"",
        "/Volumes/;(allow default)",
        "/Volumes/(param \"RUN\")",
    ];
    let caps = || Caps {
        net: Net::Loopback,
        read_model: ReadModel::Open,
    };
    let layout = |run: &str, entry: &str, input: &str, deny: &str| SlotLayout {
        run: AbsPath::parse(run).expect("run"),
        uid_entry: Some(AbsPath::parse(entry).expect("entry")),
        inputs: vec![AbsPath::parse(input).expect("input")],
        read_denies: vec![AbsPath::parse(deny).expect("deny")],
    };
    let plain = Policy::base(
        layout("/Volumes/r", "/Volumes/e", "/Volumes/i", "/Volumes/d"),
        caps(),
    )
    .expect("plain");
    let plain_profile = Profile::for_policy(&plain).expect("plain profile");
    for text in hostile {
        let policy = Policy::base(layout(text, text, text, text), caps()).expect("hostile");
        let profile = Profile::for_policy(&policy).expect("hostile profile");
        assert_eq!(profile.text(), plain_profile.text(), "{text:?}");
        assert!(!profile.text().contains(text), "{text:?}");
        let values: Vec<&str> = profile
            .params()
            .iter()
            .map(|(_, value)| value.as_str())
            .collect();
        assert_eq!(values, [text, text, text, text], "{text:?}");
    }
}

#[test]
fn port_lists_that_are_empty_zero_or_too_long_are_refused() {
    assert_eq!(Ports::new(&[]), Err(PortsError::Empty));
    assert_eq!(Ports::new(&[80, 0]), Err(PortsError::Zero));
    let many = u16::try_from(MAX_PORTS).expect("small") + 1;
    assert_eq!(
        Ports::new(&(1..=many).collect::<Vec<u16>>()),
        Err(PortsError::TooMany)
    );
    let at_bound = Ports::new(&(1..many).collect::<Vec<u16>>()).expect("at the bound");
    assert_eq!(at_bound.iter().count(), MAX_PORTS);
}

#[test]
fn a_port_list_keeps_each_port_once_lowest_first() {
    let ports = Ports::new(&[9000, 80, 9000, 443]).expect("ports");
    assert_eq!(ports.iter().collect::<Vec<u16>>(), [80, 443, 9000]);
}

#[test]
fn a_child_is_one_more_normal_component() {
    let base = path("/Volumes/work/run");
    assert_eq!(
        base.child("stub.c").expect("child").as_str(),
        "/Volumes/work/run/stub.c"
    );
    let table = [
        ("", PathError::NotNormal),
        (".", PathError::NotNormal),
        ("..", PathError::NotNormal),
        ("a/b", PathError::NotNormal),
        ("/etc", PathError::NotNormal),
        ("a\nb", PathError::Control),
    ];
    for (name, expected) in table {
        assert_eq!(base.child(name), Err(expected), "{name:?}");
    }
}

#[test]
fn under_is_by_whole_components() {
    let dir = path("/Volumes/work/store");
    assert!(dir.starts_with(&dir));
    assert!(path("/Volumes/work/store/v1/x").starts_with(&dir));
    assert!(!path("/Volumes/work/store2/x").starts_with(&dir));
    assert!(!path("/Volumes/work").starts_with(&dir));
}

/// A store with a pinned tree and an unpinned one, and the entrypoints of a bundle that
/// holds a good one and the hostile shapes of 5.4.
struct Bundle {
    _scratch: Scratch,
    pinned: Vec<PinnedTree>,
    entrypoints: BTreeMap<String, PathBuf>,
}

fn bundle(tag: &str) -> Bundle {
    let scratch = Scratch::new(tag);
    let store = scratch.abs("store");
    let pinned = PinnedTree::new(&store, &common::diff_id('a')).expect("pinned");
    let other = PinnedTree::new(&store, &common::diff_id('b')).expect("other");
    let mine = PathBuf::from(pinned.path().as_str());
    let theirs = PathBuf::from(other.path().as_str());
    for dir in [&mine, &theirs] {
        std::fs::create_dir_all(dir.join("bin")).expect("tree");
        std::fs::write(dir.join("bin/tool"), "#!/bin/sh\n").expect("tool");
    }
    std::os::unix::fs::symlink(mine.join("bin/tool"), mine.join("bin/link")).expect("link");
    std::os::unix::fs::symlink(theirs.join("bin"), mine.join("escape")).expect("escape");
    let mut entrypoints = BTreeMap::new();
    for (name, target) in [
        ("tool", mine.join("bin/tool")),
        ("other", theirs.join("bin/tool")),
        ("link", mine.join("bin/link")),
        ("dir", mine.join("bin")),
        ("gone", mine.join("bin/gone")),
        ("out", mine.join("escape/tool")),
        ("relative", PathBuf::from("bin/tool")),
    ] {
        entrypoints.insert(name.to_owned(), target);
    }
    Bundle {
        _scratch: scratch,
        pinned: vec![pinned],
        entrypoints,
    }
}

#[test]
fn a_program_is_an_entrypoint_that_is_a_regular_file_of_a_pinned_tree() {
    let b = bundle("tool-good");
    let tool = ToolPath::from_entrypoint("@bundle/tool", &b.entrypoints, &b.pinned).expect("tool");
    assert_eq!(
        tool.path().as_str(),
        b.entrypoints["tool"].to_str().unwrap()
    );
    assert_eq!(BUNDLE_PREFIX, "@bundle/");
}

#[test]
fn programs_that_are_not_pinned_regular_entrypoints_are_refused() {
    let b = bundle("tool-refused");
    let other = b.entrypoints["other"].to_str().unwrap().to_owned();
    let table: [(&str, ToolError); 12] = [
        ("tool", ToolError::NotBundleRef("tool".to_owned())),
        (
            "/usr/bin/cc",
            ToolError::NotBundleRef("/usr/bin/cc".to_owned()),
        ),
        ("@bundle", ToolError::NotBundleRef("@bundle".to_owned())),
        ("@bundle/", ToolError::NoSuchEntrypoint(String::new())),
        (
            "@bundle/missing",
            ToolError::NoSuchEntrypoint("missing".to_owned()),
        ),
        (
            "@bundle/../tool",
            ToolError::NoSuchEntrypoint("../tool".to_owned()),
        ),
        (
            "@bundle/relative",
            ToolError::BadPath(PathError::NotAbsolute),
        ),
        ("@bundle/other", ToolError::UnpinnedTree(other)),
        ("@bundle/link", ToolError::Symlink),
        ("@bundle/dir", ToolError::NotRegular),
        ("@bundle/gone", ToolError::Unreadable(ErrorKind::NotFound)),
        ("@bundle/out", ToolError::LeavesTree),
    ];
    for (program, expected) in table {
        assert_eq!(
            ToolPath::from_entrypoint(program, &b.entrypoints, &b.pinned),
            Err(expected),
            "{program:?}"
        );
    }
}

#[test]
fn with_no_tree_pinned_no_entrypoint_is_a_program() {
    let b = bundle("tool-none");
    let none: [PinnedTree; 0] = [];
    assert!(matches!(
        ToolPath::from_entrypoint("@bundle/tool", &b.entrypoints, &none),
        Err(ToolError::UnpinnedTree(_))
    ));
    let elsewhere = [tree("/Volumes/work/store", 'a')];
    assert!(matches!(
        ToolPath::from_entrypoint("@bundle/tool", &b.entrypoints, &elsewhere),
        Err(ToolError::UnpinnedTree(_))
    ));
}

#[test]
fn a_path_directory_must_be_an_absolute_real_directory() {
    let scratch = Scratch::new("path-dirs");
    let real = scratch.dir("real");
    let file = scratch.file("file", "");
    let link = scratch.root.join("link");
    std::os::unix::fs::symlink(&real, &link).expect("link");
    let env = |value: &str| vec![("PATH".to_owned(), value.to_owned())];
    let text = |p: &Path| p.to_str().unwrap().to_owned();
    assert_eq!(check_path_dirs(&env(&text(&real))), Ok(()));
    assert_eq!(
        check_path_dirs(&env(&format!("{}:{}", text(&real), text(&real)))),
        Ok(())
    );
    assert_eq!(check_path_dirs(&[]), Ok(()));
    assert_eq!(
        check_path_dirs(&[("HOME".to_owned(), "relative".to_owned())]),
        Ok(())
    );
    let bad = [
        (text(&link), ToolError::PathEntrySymlink(text(&link))),
        (text(&file), ToolError::PathEntryNotDir(text(&file))),
        (
            format!("{}:{}", text(&real), text(&link)),
            ToolError::PathEntrySymlink(text(&link)),
        ),
        (
            text(&scratch.root.join("missing")),
            ToolError::Unreadable(ErrorKind::NotFound),
        ),
        ("bin".to_owned(), ToolError::BadPath(PathError::NotAbsolute)),
        (String::new(), ToolError::BadPath(PathError::NotAbsolute)),
        (
            format!("{}:", text(&real)),
            ToolError::BadPath(PathError::NotAbsolute),
        ),
    ];
    for (value, expected) in bad {
        assert_eq!(check_path_dirs(&env(&value)), Err(expected), "{value:?}");
    }
}

#[test]
fn every_tool_error_says_what_is_wrong() {
    let table = [
        (
            ToolError::NotBundleRef("x".to_owned()),
            "program \"x\" is not an @bundle/ entrypoint",
        ),
        (
            ToolError::NoSuchEntrypoint("x".to_owned()),
            "the bundle has no entrypoint \"x\"",
        ),
        (
            ToolError::BadPath(PathError::Root),
            "path is not accepted: path is the root directory",
        ),
        (
            ToolError::UnpinnedTree("/a".to_owned()),
            "/a is not under a pinned tree",
        ),
        (
            ToolError::Unreadable(ErrorKind::NotFound),
            "cannot examine the path: NotFound",
        ),
        (ToolError::Symlink, "the entrypoint is a symbolic link"),
        (
            ToolError::NotRegular,
            "the entrypoint is not a regular file",
        ),
        (
            ToolError::LeavesTree,
            "the entrypoint resolves outside its tree",
        ),
        (
            ToolError::PathEntrySymlink("/p".to_owned()),
            "PATH entry \"/p\" is a symbolic link",
        ),
        (
            ToolError::PathEntryNotDir("/p".to_owned()),
            "PATH entry \"/p\" is not a directory",
        ),
    ];
    for (error, text) in table {
        assert_eq!(error.to_string(), text);
    }
}
