//! Checks on the fixture directory itself: every file is named by a test, listed in the README, and free of real values.

use super::*;

/// The directory that holds every fixture and the README.
fn fixtures_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures")
}

/// Every file under `dir`, as its path relative to `root` with `/` between the parts.
fn walk(root: &Path, dir: &Path, found: &mut Vec<String>) {
    let mut entries: Vec<PathBuf> = fs::read_dir(dir)
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .collect();
    entries.sort();
    for path in entries {
        if path.is_dir() {
            walk(root, &path, found);
        } else {
            let relative = path.strip_prefix(root).unwrap();
            let parts: Vec<String> = relative
                .components()
                .map(|part| part.as_os_str().to_string_lossy().into_owned())
                .collect();
            found.push(parts.join("/"));
        }
    }
}

/// Every fixture file, by its path under `tests/fixtures`: the README is not one.
fn fixture_files() -> Vec<String> {
    let root = fixtures_root();
    let mut found = Vec::new();
    walk(&root, &root, &mut found);
    found.retain(|path| path != "README.md");
    found
}

/// Whether `text` holds `name` as a whole file name: not as the end of a longer one.
fn mentions(text: &str, name: &str) -> bool {
    text.match_indices(name).any(|(at, _)| {
        text[..at].chars().next_back().is_none_or(|before| {
            !(before.is_ascii_alphanumeric() || before == '_' || before == '-')
        })
    })
}

/// The fixtures (paths under `tests/fixtures`) that none of `texts` names, by the path or
/// by the bare file name.
fn unnamed<'a>(fixtures: &'a [String], texts: &[String]) -> Vec<&'a str> {
    fixtures
        .iter()
        .filter(|path| {
            let bare = path.rsplit('/').next().unwrap_or(path);
            !texts
                .iter()
                .any(|text| mentions(text, path) || mentions(text, bare))
        })
        .map(String::as_str)
        .collect()
}

/// The text of every Rust source under `tests`.
fn test_sources() -> Vec<String> {
    let tests = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests");
    let mut found = Vec::new();
    walk(&tests, &tests, &mut found);
    found
        .iter()
        .filter(|path| path.ends_with(".rs"))
        .map(|path| fs::read_to_string(tests.join(path)).unwrap())
        .collect()
}

/// Asserts that every fixture file is named by a test source, by path or by file name, so
/// that a fixture no test reads cannot sit in the directory unchecked. Catches a fixture
/// added with no test.
#[test]
fn fixtures_are_all_named_by_a_test() {
    let fixtures = fixture_files();
    assert!(fixtures.len() > 40, "{fixtures:?}");
    assert_eq!(unnamed(&fixtures, &test_sources()), Vec::<&str>::new());
}

/// Asserts that the check reports a fixture no source names, once by path and once by bare
/// name, and does not take the end of a longer file name for the name. Catches a check
/// that passes everything.
#[test]
fn an_unnamed_fixture_is_reported() {
    let fixtures = vec![
        "api/read_by_a_test.xml".to_owned(),
        "api/never_read.xml".to_owned(),
        "other/ad.xml".to_owned(),
    ];
    let texts = vec![
        "fixture(\"read_by_a_test.xml\") and fixture(\"broad.xml\")".to_owned(),
        "include_str!(\"../fixtures/other/ad.xml\")".to_owned(),
    ];
    assert_eq!(unnamed(&fixtures, &texts), ["api/never_read.xml"]);
    let longer = vec!["api/ad.xml".to_owned()];
    assert_eq!(
        unnamed(&longer, &["fixture(\"broad.xml\")".to_owned()]),
        ["api/ad.xml"]
    );
}

/// Asserts that the README lists every fixture file by name. Catches a fixture with no
/// provenance row.
#[test]
fn fixtures_are_all_listed_in_the_readme() {
    let readme = fs::read_to_string(fixtures_root().join("README.md")).unwrap();
    assert_eq!(unnamed(&fixture_files(), &[readme]), Vec::<&str>::new());
}

/// The only IPv4 ranges a fixture may hold: the documentation ranges of RFC 5737.
const DOCUMENTATION_PREFIXES: [&str; 3] = ["192.0.2.", "198.51.100.", "203.0.113."];

/// The one account number a fixture may hold.
const PLACEHOLDER_ACCOUNT: &str = "123456789012";

/// The one access key id a fixture may hold: the one the AWS documentation uses.
const PLACEHOLDER_KEY: &str = "AKIAIOSFODNN7EXAMPLE";

/// The fixtures the placeholder check does not read, and why.
const UNCHECKED: [(&str, &str); 3] = [
    (
        "identity/doc.json",
        "a signed identity document: the signature covers its text, which names the made-up account 000000000000",
    ),
    (
        "identity/other.json",
        "a signed identity document: the signature covers its text, which names the made-up account 000000000000",
    ),
    (
        "identity/region.json",
        "a signed identity document: the signature covers its text, which names the made-up account 000000000000",
    ),
];

/// The values in `text` that a fixture may not hold: an IPv4 address outside the
/// documentation ranges, a twelve-digit number other than the placeholder account, and an
/// access key id other than the placeholder.
fn strays(text: &str) -> Vec<String> {
    let mut found = Vec::new();
    for word in text.split(|c: char| !(c.is_ascii_digit() || c == '.')) {
        let parts: Vec<&str> = word.trim_matches('.').split('.').collect();
        let octets = parts.len() == 4
            && parts
                .iter()
                .all(|part| !part.is_empty() && part.len() <= 3 && part.parse::<u8>().is_ok());
        if octets
            && !DOCUMENTATION_PREFIXES
                .iter()
                .any(|prefix| word.starts_with(prefix))
        {
            found.push(word.to_owned());
        }
    }
    let chars: Vec<(usize, char)> = text.char_indices().collect();
    let mut at = 0;
    while at < chars.len() {
        if !chars[at].1.is_ascii_digit() {
            at += 1;
            continue;
        }
        let start = at;
        while at < chars.len() && chars[at].1.is_ascii_digit() {
            at += 1;
        }
        let joins = |c: char| c.is_ascii_alphanumeric() || c == '-' || c == '.';
        let before = start.checked_sub(1).map(|i| chars[i].1);
        let after = chars.get(at).map(|(_, c)| *c);
        let alone = !before.is_some_and(joins) && !after.is_some_and(joins);
        let number = &text[chars[start].0..chars.get(at).map_or(text.len(), |(i, _)| *i)];
        if alone && number.len() == 12 && number != PLACEHOLDER_ACCOUNT {
            found.push(number.to_owned());
        }
    }
    for prefix in ["AKIA", "ASIA"] {
        for (at, _) in text.match_indices(prefix) {
            let key: String = text[at..].chars().take(20).collect();
            let whole = key.len() == 20
                && key
                    .chars()
                    .all(|c| c.is_ascii_uppercase() || c.is_ascii_digit());
            if whole && key != PLACEHOLDER_KEY {
                found.push(key);
            }
        }
    }
    found
}

/// Asserts that no fixture holds an IPv4 address outside the documentation ranges, a
/// twelve-digit number other than the placeholder account, or an access key id other than
/// the placeholder; the files not read are listed with a reason. Catches a real address,
/// account or key pasted in from a capture.
#[test]
fn fixtures_carry_only_placeholder_values() {
    let unchecked: BTreeMap<&str, &str> = UNCHECKED.into_iter().collect();
    let mut bad = Vec::new();
    for path in fixture_files() {
        if unchecked.contains_key(path.as_str()) {
            continue;
        }
        let bytes = fs::read(fixtures_root().join(&path)).unwrap();
        let text = String::from_utf8(bytes).unwrap_or_else(|_| panic!("{path} is not text"));
        bad.extend(
            strays(&text)
                .into_iter()
                .map(|value| format!("{path}: {value}")),
        );
    }
    assert_eq!(bad, Vec::<String>::new());
}

/// Asserts that the placeholder check reports an address outside the documentation ranges,
/// a twelve-digit account, and an access key id, and passes the placeholders themselves,
/// instance ids of seventeen digits, the zero request id and a price with many decimals.
/// Catches a check that finds nothing.
#[test]
fn a_stray_value_is_reported() {
    assert_eq!(strays("<ip>10.1.2.3</ip>"), ["10.1.2.3"]);
    assert_eq!(strays("<ip>192.0.3.7</ip>"), ["192.0.3.7"]);
    assert_eq!(strays("owner 210987654321 end"), ["210987654321"]);
    assert_eq!(
        strays("key AKIAABCDEFGHIJKLMNOP and ASIAABCDEFGHIJKLMNOP"),
        ["AKIAABCDEFGHIJKLMNOP", "ASIAABCDEFGHIJKLMNOP"]
    );
    let clean = "192.0.2.5 198.51.100.9 203.0.113.250 arn:aws:iam::123456789012:user/x \
                 AKIAIOSFODNN7EXAMPLE i-00000000000000001 0.123456789012345 \
                 00000000-0000-0000-0000-000000000000 1794483000 2026-11-05T01:01:01Z";
    assert_eq!(strays(clean), Vec::<String>::new());
}
