//! The names a mint is made of: the bucket, the log prefix and the role.

use js_logmint::{Bucket, LogPrefix, NameError, RoleArn};
use js_logstore::LayoutError;

/// Asserts the bucket rule at its bounds: 3 and 63 bytes of `[a-z0-9.-]` are taken, 2 and 64
/// bytes, an upper-case letter, a `_`, a `*` and a `/` are refused, and the refusal does not
/// repeat the name. It catches a bucket that could widen the resource pattern or add a path.
#[test]
fn a_bucket_is_three_to_sixty_three_bytes_of_its_alphabet() {
    for good in ["abc", &"b".repeat(63), "logs.example-1"] {
        assert_eq!(Bucket::new(good).unwrap().as_str(), good);
    }
    for bad in ["ab", &"b".repeat(64), "Logs", "lo_gs", "logs*", "lo/gs", ""] {
        assert_eq!(Bucket::new(bad), Err(NameError::Bucket), "{bad:?}");
    }
    assert_eq!(
        NameError::Bucket.to_string(),
        "the bucket must be 3 to 63 bytes of [a-z0-9.-]"
    );
}

/// Asserts that a log prefix is held to the object layout's rules and gets its trailing `/`:
/// the empty prefix stays empty, `a/b` becomes `a/b/`, a prefix of exactly 512 bytes is taken,
/// and a leading, trailing or double `/`, a `.` or `..` segment, each of `*`, `?`, `$`, `{`, a
/// quote, a backslash, a space, a control character and 513 bytes are refused with the
/// layout's reason. It catches a prefix that could widen the session policy (`a/b` without its
/// `/` would match `a/bc/...`) or break out of its JSON string.
#[test]
fn a_log_prefix_is_held_to_the_layout_and_ends_in_a_slash() {
    assert_eq!(LogPrefix::new("").unwrap().as_str(), "");
    assert_eq!(LogPrefix::new("a/b").unwrap().as_str(), "a/b/");
    assert_eq!(LogPrefix::new("A.z_0-9").unwrap().as_str(), "A.z_0-9/");
    let longest = format!("{}/{}", "a".repeat(255), "b".repeat(256));
    assert_eq!(longest.len(), 512);
    assert_eq!(
        LogPrefix::new(&longest).unwrap().as_str(),
        format!("{longest}/")
    );
    assert_eq!(
        LogPrefix::new(&format!("{longest}c")),
        Err(NameError::Prefix(LayoutError::PrefixTooLong))
    );
    for bad in [
        "/a", "a/", "a//b", ".", "..", "a/./b", "a/../b", "a*", "a?", "a$", "a{b}", "a\"b", "a\\b",
        "a b", "a\u{1}b", "a/b\n",
    ] {
        assert_eq!(
            LogPrefix::new(bad),
            Err(NameError::Prefix(LayoutError::Prefix)),
            "{bad:?}"
        );
    }
    assert_eq!(
        NameError::Prefix(LayoutError::PrefixTooLong).to_string(),
        "the log prefix is longer than 512 bytes"
    );
}

/// Asserts the role rule: an `arn:` value of printable ASCII of up to 2,048 bytes is taken, and
/// one without `arn:`, of 2,049 bytes, or holding a space or a control character is refused
/// without repeating it. It catches a role that would be sent as something other than the ARN
/// the operator wrote.
#[test]
fn a_role_is_an_arn_of_printable_ascii() {
    let longest = format!("arn:{}", "r".repeat(2044));
    for good in ["arn:aws:iam::example:role/js-logs", longest.as_str()] {
        assert_eq!(RoleArn::new(good).unwrap().as_str(), good);
    }
    let too_long = format!("{longest}r");
    for bad in [
        "role/js-logs",
        "",
        too_long.as_str(),
        "arn:aws:iam::example:role/js logs",
        "arn:aws:iam::example:role/js\tlogs",
    ] {
        assert_eq!(RoleArn::new(bad), Err(NameError::RoleArn), "{bad:?}");
    }
    assert_eq!(
        NameError::RoleArn.to_string(),
        "the role must be an arn: value of printable ASCII, at most 2048 bytes"
    );
}
