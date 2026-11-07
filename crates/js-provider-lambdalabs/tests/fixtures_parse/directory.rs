//! The fixture directory itself: every file is named by a test, the README lists every
//! file, and no file carries a value that is not a placeholder.

use crate::*;

// ---------------------------------------------------------- the directory itself

fn fixtures_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures")
}

/// Every file under `dir`, recursively, as paths relative to `dir` with `/` separators.
fn files_under(dir: &Path) -> BTreeSet<String> {
    fn walk(dir: &Path, prefix: &str, into: &mut BTreeSet<String>) {
        for entry in fs::read_dir(dir).unwrap() {
            let entry = entry.unwrap();
            let name = format!("{prefix}{}", entry.file_name().to_str().unwrap());
            if entry.file_type().unwrap().is_dir() {
                walk(&entry.path(), &format!("{name}/"), into);
            } else {
                into.insert(name);
            }
        }
    }
    let mut into = BTreeSet::new();
    walk(dir, "", &mut into);
    into
}

/// The fixture files: everything under `tests/fixtures` except the README.
fn fixture_names() -> Vec<String> {
    files_under(&fixtures_dir())
        .into_iter()
        .filter(|name| name != "README.md")
        .collect()
}

/// Whether `text` names the file `name`: as a whole name, not as the tail of a longer one.
fn mentions(text: &str, name: &str) -> bool {
    text.match_indices(name).any(|(at, _)| {
        text[..at]
            .chars()
            .next_back()
            .is_none_or(|before| !(before.is_ascii_alphanumeric() || before == '_'))
    })
}

/// The names in `names` that none of `texts` mentions, by relative path or bare file name.
fn unnamed<'a>(names: &'a [String], texts: &[String]) -> Vec<&'a str> {
    names
        .iter()
        .filter(|name| {
            let bare = name.rsplit('/').next().unwrap_or(name);
            !texts
                .iter()
                .any(|text| mentions(text, name) || mentions(text, bare))
        })
        .map(String::as_str)
        .collect()
}

fn test_sources() -> Vec<String> {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests");
    files_under(&root)
        .into_iter()
        .filter(|name| name.ends_with(".rs"))
        .map(|name| fs::read_to_string(root.join(name)).unwrap())
        .collect()
}

#[test]
fn fixtures_are_all_named_by_a_test() {
    let names = fixture_names();
    assert!(!names.is_empty());
    let missing = unnamed(&names, &test_sources());
    assert!(
        missing.is_empty(),
        "no test names these fixtures: {missing:?}"
    );
}

#[test]
fn a_fixture_no_test_names_is_reported() {
    let names: Vec<String> = [
        "named.json",
        "sub/deep.json",
        "orphan.json",
        "pre_named.json",
    ]
    .map(str::to_owned)
    .to_vec();
    let texts: Vec<String> = [
        "let a = fixture(\"named.json\");",
        "include_str!(\"../fixtures/sub/deep.json\")",
        // Only the tail of another name: not a mention.
        "fixture(\"pre_named.json\")",
    ]
    .map(str::to_owned)
    .to_vec();
    // `named.json` also ends `pre_named.json`, but the first text names it whole.
    assert_eq!(unnamed(&names, &texts), ["orphan.json"]);
    let only_tail = vec!["xnamed.json".to_owned()];
    assert_eq!(unnamed(&names[..1], &only_tail), ["named.json"]);
    assert_eq!(unnamed(&names[..1], &[]), ["named.json"]);
}

#[test]
fn the_readme_has_a_row_for_every_fixture() {
    let readme = fs::read_to_string(fixtures_dir().join("README.md")).unwrap();
    let names = fixture_names();
    let missing = unnamed(&names, &[readme]);
    assert!(missing.is_empty(), "the README omits: {missing:?}");
}

/// The one account id the fixtures may carry.
const ACCOUNT_ID: &str = "123456789012";

/// The one access key id the fixtures may carry.
const ACCESS_KEY_ID: &str = "AKIAIOSFODNN7EXAMPLE";

/// Addresses outside the documentation ranges that a fixture may carry, each with why.
const ALLOWED_ADDRESSES: &[(&str, &str)] = &[(
    "0.0.0.0",
    "the 'any source' network 0.0.0.0/0 of a firewall rule: it names no host",
)];

/// Twelve-digit numbers other than the account id that a fixture may carry, each with why.
const ALLOWED_NUMBERS: &[(&str, &str)] = &[(
    "000000000001",
    "the counter that ends an instance name such as js-0123abcd-000000000001: not an account",
)];

/// Whether `a.b.c.d` lies in 192.0.2.0/24, 198.51.100.0/24 or 203.0.113.0/24.
fn documentation_address(octets: [u8; 3]) -> bool {
    matches!(octets, [192, 0, 2] | [198, 51, 100] | [203, 0, 113])
}

/// What in `text` is not a placeholder: an IPv4 address outside the documentation ranges,
/// a twelve-digit number that is not [`ACCOUNT_ID`] and an AWS access key id that is not
/// [`ACCESS_KEY_ID`]. The exemptions are the constants above.
fn placeholder_faults(text: &str) -> Vec<String> {
    let mut faults = Vec::new();
    // Runs of digits and dots: an address is four numbers of at most 255 between dots.
    for run in text.split(|c: char| !(c.is_ascii_digit() || c == '.')) {
        let parts: Vec<&str> = run.trim_matches('.').split('.').collect();
        let octets: Vec<u8> = parts
            .iter()
            .filter(|p| !p.is_empty() && p.len() <= 3)
            .filter_map(|p| p.parse().ok())
            .collect();
        if parts.len() == 4 && octets.len() == 4 {
            let address = parts.join(".");
            let allowed = ALLOWED_ADDRESSES.iter().any(|(a, _)| *a == address);
            if !allowed && !documentation_address([octets[0], octets[1], octets[2]]) {
                faults.push(format!(
                    "address {address} is outside the documentation ranges"
                ));
            }
        }
    }
    // Maximal runs of digits that no letter touches.
    let bytes = text.as_bytes();
    let mut start = 0;
    while start < bytes.len() {
        if !bytes[start].is_ascii_digit() {
            start += 1;
            continue;
        }
        let mut end = start;
        while end < bytes.len() && bytes[end].is_ascii_digit() {
            end += 1;
        }
        let touches_letter = (start > 0 && bytes[start - 1].is_ascii_alphabetic())
            || (end < bytes.len() && bytes[end].is_ascii_alphabetic());
        let number = &text[start..end];
        let allowed = number == ACCOUNT_ID || ALLOWED_NUMBERS.iter().any(|(n, _)| *n == number);
        if number.len() == 12 && !touches_letter && !allowed {
            faults.push(format!("number {number} looks like an account id"));
        }
        start = end;
    }
    for prefix in ["AKIA", "ASIA"] {
        for (at, _) in text.match_indices(prefix) {
            let key: String = text[at..]
                .chars()
                .take_while(|c| c.is_ascii_uppercase() || c.is_ascii_digit())
                .take(20)
                .collect();
            if key.len() == 20 && key != ACCESS_KEY_ID {
                faults.push(format!("{key} looks like an access key id"));
            }
        }
    }
    faults
}

#[test]
fn fixtures_carry_only_placeholder_values() {
    let mut faults = Vec::new();
    for name in fixture_names() {
        let text = fs::read_to_string(fixtures_dir().join(&name)).unwrap();
        faults.extend(
            placeholder_faults(&text)
                .into_iter()
                .map(|fault| format!("{name}: {fault}")),
        );
    }
    assert!(faults.is_empty(), "{faults:#?}");
}

#[test]
fn text_with_a_real_looking_address_account_or_key_is_reported() {
    let bad = "ip 10.0.0.7 and 198.51.101.9, account 210987654321, key AKIAABCDEFGHIJKLMNOP, \
               and ASIAABCDEFGHIJKLMNOP";
    assert_eq!(
        placeholder_faults(bad),
        [
            "address 10.0.0.7 is outside the documentation ranges",
            "address 198.51.101.9 is outside the documentation ranges",
            "number 210987654321 looks like an account id",
            "AKIAABCDEFGHIJKLMNOP looks like an access key id",
            "ASIAABCDEFGHIJKLMNOP looks like an access key id",
        ]
    );
    let good = "192.0.2.1 198.51.100.255 203.0.113.0/24 0.0.0.0/0 123456789012 000000000001 \
                AKIAIOSFODNN7EXAMPLE js-0123abcd-000000000001 2026-10-20T12:30:45.5Z \
                a1b2c3d4e5f60718293a4b5c6d7e8f90 999.1.1.1 1.2.3";
    assert_eq!(placeholder_faults(good), Vec::<String>::new());
}
