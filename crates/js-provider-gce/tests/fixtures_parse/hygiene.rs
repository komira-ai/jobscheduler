//! Checks on the fixture directory itself: every file is named by a test, and none carries
//! a value that is not a placeholder.

use super::*;

// ---- the directory of fixtures

/// The directory of the fixtures.
fn fixtures_dir() -> std::path::PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures")
}

/// Every file under `dir`, as a path relative to `base` written with `/`, in order.
fn files_under(dir: &Path, base: &Path) -> Vec<String> {
    let mut entries: Vec<_> = fs::read_dir(dir)
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .collect();
    entries.sort();
    let mut found = Vec::new();
    for path in entries {
        if path.is_dir() {
            found.extend(files_under(&path, base));
        } else {
            let relative = path.strip_prefix(base).unwrap();
            let parts: Vec<String> = relative
                .components()
                .map(|part| part.as_os_str().to_string_lossy().into_owned())
                .collect();
            found.push(parts.join("/"));
        }
    }
    found
}

/// The fixture files, as paths relative to `tests/fixtures`, without the README.
fn fixture_names() -> Vec<String> {
    let base = fixtures_dir();
    files_under(&base, &base)
        .into_iter()
        .filter(|name| name.rsplit('/').next() != Some("README.md"))
        .collect()
}

/// The text of every Rust source under `tests`.
fn test_sources() -> Vec<String> {
    let base = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests");
    files_under(&base, &base)
        .into_iter()
        .filter(|name| name.ends_with(".rs"))
        .map(|name| fs::read_to_string(base.join(name)).unwrap())
        .collect()
}

/// Whether `source` has `name` as a whole file name: not as the end or the start of a
/// longer one.
fn mentions(source: &str, name: &str) -> bool {
    let part = |c: char| c.is_ascii_alphanumeric() || c == '_' || c == '-';
    source.match_indices(name).any(|(at, _)| {
        let before = source[..at].chars().next_back();
        let after = source[at + name.len()..].chars().next();
        !before.is_some_and(|c| part(c) || c == '.') && !after.is_some_and(part)
    })
}

/// The fixtures of `names` that none of `sources` names, by their relative path or their
/// bare file name.
fn unnamed<'a>(names: &'a [String], sources: &[String]) -> Vec<&'a str> {
    names
        .iter()
        .filter(|name| {
            let bare = name.rsplit('/').next().unwrap_or(name);
            !sources
                .iter()
                .any(|source| mentions(source, name) || mentions(source, bare))
        })
        .map(String::as_str)
        .collect()
}

/// Asserts that every file under `tests/fixtures` (the API answers, the credential and the
/// identity tokens) is named by some Rust source under `tests`, so no file lingers that a
/// test does not read. The failure names each unnamed file.
#[test]
fn fixtures_are_all_named_by_a_test() {
    let names = fixture_names();
    assert!(names.len() > 40, "{names:?}");
    let missing = unnamed(&names, &test_sources());
    assert!(missing.is_empty(), "no test names: {missing:?}");
}

/// Asserts that the check above reports a file no source names, by its path, and accepts
/// one named by its relative path or by its bare name; and that a longer file name that
/// begins or ends with the same words does not count as naming it. It catches a check that passes
/// whatever the sources say.
#[test]
fn a_fixture_no_test_names_is_reported() {
    let names: Vec<String> = [
        "api/named_by_path.json",
        "api/named_by_name.json",
        "api/orphan.json",
        "api/short.json",
    ]
    .map(str::to_owned)
    .to_vec();
    let sources = vec![
        "const A: &str = include_str!(\"fixtures/api/named_by_path.json\");".to_owned(),
        "fixture!(\"named_by_name.json\")".to_owned(),
        "fixture!(\"not-short.json\") // and the words orphan.jsonl".to_owned(),
    ];
    assert_eq!(
        unnamed(&names, &sources),
        ["api/orphan.json", "api/short.json"]
    );
    assert_eq!(unnamed(&names, &[]).len(), 4);
}

/// The IPv4 addresses in `text` outside the three documentation ranges (192.0.2.0/24,
/// 198.51.100.0/24, 203.0.113.0/24), as written.
fn foreign_addresses(text: &str) -> Vec<String> {
    let mut found = Vec::new();
    let mut rest = text;
    while let Some(start) = rest.find(|c: char| c.is_ascii_digit()) {
        let token_end = rest[start..]
            .find(|c: char| !(c.is_ascii_digit() || c == '.'))
            .map_or(rest.len(), |end| start + end);
        let groups: Vec<&str> = rest[start..token_end].split('.').collect();
        for window in groups.windows(4) {
            let octets: Option<Vec<u8>> = window
                .iter()
                .map(|group| {
                    (!group.is_empty() && group.len() <= 3)
                        .then(|| group.parse::<u8>().ok())
                        .flatten()
                })
                .collect();
            if let Some(octets) = octets {
                let documentation =
                    matches!(octets[..3], [192, 0, 2] | [198, 51, 100] | [203, 0, 113]);
                if !documentation {
                    found.push(window.join("."));
                }
            }
        }
        rest = &rest[token_end..];
    }
    found
}

/// The numbers of twelve digits in `text` other than the placeholder account number
/// 123456789012.
fn foreign_account_numbers(text: &str) -> Vec<String> {
    text.split(|c: char| !c.is_ascii_digit())
        .filter(|run| run.len() == 12 && *run != "123456789012")
        .map(str::to_owned)
        .collect()
}

/// The access key ids in `text` (`AKIA` or `ASIA` and sixteen capital letters or digits)
/// other than the placeholder AKIAIOSFODNN7EXAMPLE.
fn foreign_key_ids(text: &str) -> Vec<String> {
    let mut found = Vec::new();
    for prefix in ["AKIA", "ASIA"] {
        for (at, _) in text.match_indices(prefix) {
            let id: String = text[at..]
                .chars()
                .take(20)
                .take_while(|c| c.is_ascii_uppercase() || c.is_ascii_digit())
                .collect();
            if id.len() == 20 && id != "AKIAIOSFODNN7EXAMPLE" {
                found.push(id);
            }
        }
    }
    found
}

/// Everything in `text` that is not a placeholder value, labelled by its kind.
fn foreign_values(text: &str) -> Vec<String> {
    let mut found = Vec::new();
    found.extend(
        foreign_addresses(text)
            .into_iter()
            .map(|value| format!("address {value}")),
    );
    found.extend(
        foreign_account_numbers(text)
            .into_iter()
            .map(|value| format!("number {value}")),
    );
    found.extend(
        foreign_key_ids(text)
            .into_iter()
            .map(|value| format!("key id {value}")),
    );
    found
}

/// Asserts that no fixture carries an IPv4 address outside the documentation ranges, a
/// twelve-digit number other than the placeholder account number, or an access key id other
/// than the placeholder, so a real capture cannot be committed unsanitised. No file is
/// exempt: the key material and tokens of `credential` and `identity` pass it as they are.
/// The failure names each file and value.
#[test]
fn fixtures_carry_only_placeholder_values() {
    let base = fixtures_dir();
    let mut offending = Vec::new();
    for name in fixture_names() {
        let bytes = fs::read(base.join(&name)).unwrap();
        let text = String::from_utf8(bytes).unwrap_or_else(|_| panic!("{name} is not text"));
        offending.extend(
            foreign_values(&text)
                .into_iter()
                .map(|value| format!("{name}: {value}")),
        );
    }
    assert!(offending.is_empty(), "{offending:#?}");
}

/// Asserts that the placeholder check reports an address outside the documentation ranges
/// (a private one, a loopback one, one just outside a documentation range), a twelve-digit
/// number that is not the placeholder, and an AKIA or ASIA key id that is not the
/// placeholder, each as written; and reports none of the placeholders, nor numbers of
/// another length, nor a version number. It catches a check that reports nothing.
#[test]
fn values_that_are_not_placeholders_are_reported() {
    let bad = "ip 10.0.0.1 and 127.0.0.1 and 203.0.114.1 and 192.0.3.5 \
               account 111111111111 keys AKIAABCDEFGHIJKLMNOP ASIAABCDEFGHIJKLMNOP";
    assert_eq!(
        foreign_values(bad),
        [
            "address 10.0.0.1",
            "address 127.0.0.1",
            "address 203.0.114.1",
            "address 192.0.3.5",
            "number 111111111111",
            "key id AKIAABCDEFGHIJKLMNOP",
            "key id ASIAABCDEFGHIJKLMNOP",
        ]
    );
    let good = "ip 192.0.2.10 198.51.100.7 203.0.113.250 account 123456789012 \
                key AKIAIOSFODNN7EXAMPLE sizes 1234567890123 12345678901 version 1.2.3 \
                price 24.0 1.5.7.x AKIASHORT";
    assert_eq!(foreign_values(good), Vec::<String>::new());
}
