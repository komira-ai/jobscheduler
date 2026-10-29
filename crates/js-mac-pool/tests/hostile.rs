//! Hostile input: paths and port lists that must be refused, and paths that look like
//! profile syntax and must not change the profile.

use std::ffi::OsStr;
use std::os::unix::ffi::OsStrExt;

use js_mac_pool::seatbelt::{
    AbsPath, Caps, MAX_PATH_BYTES, MAX_PORTS, Net, PathError, Policy, Ports, PortsError, Profile,
    ReadModel, SlotLayout,
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
        assert_eq!(AbsPath::new(text), Err(expected), "{text:?}");
    }
    assert_eq!(AbsPath::new(&long), Err(PathError::TooLong));
}

#[test]
fn a_path_that_is_not_utf8_is_refused() {
    let bytes = OsStr::from_bytes(b"/Volumes/a\xffb");
    assert_eq!(AbsPath::new(bytes), Err(PathError::NotUtf8));
}

#[test]
fn the_longest_path_and_non_ascii_text_are_accepted() {
    let longest = format!("/{}", "a".repeat(MAX_PATH_BYTES - 1));
    assert_eq!(
        AbsPath::new(&longest).expect("at the limit").as_str(),
        longest
    );
    let accented = AbsPath::new("/Volumes/w\u{f6}rk/\u{fc}ber").expect("text");
    assert_eq!(accented.as_str(), "/Volumes/w\u{f6}rk/\u{fc}ber");
    assert_eq!(
        AbsPath::new("/private/var/folders/ab")
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
        deny_egress: vec![],
    };
    let layout = |run: &str, entry: &str, input: &str, deny: &str| SlotLayout {
        run: AbsPath::new(run).expect("run"),
        uid_entry: AbsPath::new(entry).expect("entry"),
        inputs: vec![AbsPath::new(input).expect("input")],
        read_denies: vec![AbsPath::new(deny).expect("deny")],
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
    assert_eq!(Ports::new([]), Err(PortsError::Empty));
    assert_eq!(Ports::new([80, 0]), Err(PortsError::Zero));
    let many = u16::try_from(MAX_PORTS).expect("small") + 1;
    assert_eq!(Ports::new(1..=many), Err(PortsError::TooMany));
    let at_bound = Ports::new(1..many).expect("at the bound");
    assert_eq!(at_bound.iter().count(), MAX_PORTS);
}

#[test]
fn a_port_list_keeps_each_port_once_lowest_first() {
    let ports = Ports::new([9000, 80, 9000, 443]).expect("ports");
    assert_eq!(ports.iter().collect::<Vec<u16>>(), [80, 443, 9000]);
}
