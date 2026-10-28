//! The readers of cgroup file text.

use js_agent::usage::{
    cpu_limited, parse_active_file, parse_inactive_file, parse_single_u64, parse_stat_key,
    parse_usage_usec, working_set,
};

const CPU_STAT: &str = "usage_usec 123456\nuser_usec 100000\nsystem_usec 23456\nnr_periods 0\n";

const MEMORY_STAT: &str = "anon 1\nfile 2\nactive_anon 11\ninactive_anon 12\n\
inactive_file 7000\nactive_file 3000\nworkingset_activate_file 5\n";

/// Asserts the reading of a `key value` line over real layouts and every malformed form:
/// a missing key, a sign, a non-number, a value past `u64`, no value, a third token, a
/// longer key, a leading space, an empty text and a duplicate (the first line wins even
/// when it is malformed). It catches a `starts_with` or `contains` match of the key, a
/// lenient number parse, an overflow that wraps and a line that accepts extra tokens.
#[test]
fn stat_key_table() {
    let key = "usage_usec";
    let cases: [(&str, Option<u64>); 17] = [
        (CPU_STAT, Some(123_456)),
        ("user_usec 5\n", None),
        ("usage_usec abc\n", None),
        ("usage_usec -5\n", None),
        ("usage_usec +5\n", None),
        ("usage_usec 18446744073709551615\n", Some(u64::MAX)),
        ("usage_usec 18446744073709551616\n", None),
        ("usage_usec\n", None),
        ("usage_usec \n", None),
        ("usage_usec 1 2\n", None),
        ("usage_usec_x 9\n", None),
        ("xusage_usec 9\n", None),
        (" usage_usec 9\n", None),
        ("", None),
        ("usage_usec 1\nusage_usec 2\n", Some(1)),
        ("usage_usec abc\nusage_usec 2\n", None),
        ("usage_usec 42", Some(42)),
    ];
    for (text, expected) in cases {
        assert_eq!(parse_stat_key(text, key), expected, "{text:?}");
    }
    assert_eq!(parse_stat_key("usage_usec 9\n", "usage"), None);
}

/// Asserts that `inactive_file` and `active_file` are read apart, with `inactive_anon`,
/// `active_anon` and a `workingset_activate_file` line beside them. It catches swapped
/// keys and a prefix or suffix match.
#[test]
fn inactive_and_active_file_are_not_mixed() {
    assert_eq!(parse_inactive_file(MEMORY_STAT), Some(7000));
    assert_eq!(parse_active_file(MEMORY_STAT), Some(3000));
    assert_eq!(parse_usage_usec(CPU_STAT), Some(123_456));
    assert_eq!(parse_inactive_file(CPU_STAT), None);
}

/// Asserts the one-number reader: a trailing newline and surrounding spaces are trimmed,
/// and an empty text, `max`, two numbers and a sign are refused. It catches a missing trim
/// and a lenient parse.
#[test]
fn single_u64_table() {
    assert_eq!(parse_single_u64("12345\n"), Some(12_345));
    assert_eq!(parse_single_u64(" 12 \n"), Some(12));
    assert_eq!(parse_single_u64(""), None);
    assert_eq!(parse_single_u64("max"), None);
    assert_eq!(parse_single_u64("1 2"), None);
    assert_eq!(parse_single_u64("+3"), None);
}

/// Asserts the working set below, at and above the inactive cache. It catches an
/// unchecked subtraction, which panics in a debug build when the cache is larger.
#[test]
fn working_set_saturates() {
    assert_eq!(working_set(5, 9), 0);
    assert_eq!(working_set(9, 9), 0);
    assert_eq!(working_set(9, 5), 4);
}

/// Asserts `cpu.max` readings: a `max` quota is no limit, a number (zero too) is a limit,
/// and an empty text, a lone `max` and a word are not read at all. It catches an inverted
/// test and garbage read as a limit.
#[test]
fn cpu_limited_table() {
    assert_eq!(cpu_limited("max 100000\n"), Some(false));
    assert_eq!(cpu_limited("200000 100000\n"), Some(true));
    assert_eq!(cpu_limited("0 100000\n"), Some(true));
    assert_eq!(cpu_limited("max"), Some(false));
    assert_eq!(cpu_limited(""), None);
    assert_eq!(cpu_limited("abc 1"), None);
}
