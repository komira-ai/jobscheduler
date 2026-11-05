//! Source checks of `job_api`, read from the crate's own source tree: the presented token is
//! compared in one place, and the job service reads no clock of its own and names no
//! provider. Each check is also run on a text that breaks it, so it is shown to fail.

use std::fs;
use std::path::Path;

/// The sources of `job_api` that are not tests (every `.rs` file of the directory whose name
/// does not end in `tests.rs`), as (file name, text), by file name.
fn sources() -> Vec<(String, String)> {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("src/job_api");
    let mut sources: Vec<(String, String)> = fs::read_dir(&dir)
        .expect("the module's directory reads")
        .map(|entry| entry.expect("the directory entry reads").path())
        .filter(|path| !path.to_string_lossy().ends_with("tests.rs"))
        .map(|path| {
            let name = path.file_name().expect("a file has a name");
            let text = fs::read_to_string(&path).expect("the source reads");
            (name.to_string_lossy().into_owned(), text)
        })
        .collect();
    sources.sort();
    sources
}

/// The number of calls of `Token::matches` in `source`.
fn token_comparisons(source: &str) -> usize {
    source.matches(".matches(").count()
}

/// What another comparison of a presented token would be written with: the constant-time
/// equality the token type uses, or the token's comparison with another token.
const OTHER_COMPARISONS: [&str; 4] = ["ct_eq", "ConstantTimeEq", "subtle", "same_as("];

/// The words of [`OTHER_COMPARISONS`] that `source` holds.
fn other_comparisons(source: &str) -> Vec<&'static str> {
    OTHER_COMPARISONS
        .into_iter()
        .filter(|word| source.contains(word))
        .collect()
}

/// The text of the function `name` in `source`, from its `fn` to the first line that is a
/// lone `}`; empty when `source` has no such function.
fn function<'a>(source: &'a str, name: &str) -> &'a str {
    let Some(start) = source.find(&format!("fn {name}(")) else {
        return "";
    };
    let rest = &source[start..];
    let end = rest.find("\n}\n").map_or(rest.len(), |end| end + 2);
    &rest[..end]
}

/// Asserts that the bearer layer (`auth.rs`, its `admitted`) and the lease routes' middleware
/// (`api.rs`, its `authenticate`) each read the header with `bearer` and compare the token
/// with `Token::matches`, and that no file of `job_api` holds another call of it or any
/// other comparison of a token. Each check is shown to fail on a text that copies the
/// comparison or writes its own.
#[test]
fn one_comparison_serves_both_transports_and_the_lease_routes() {
    let sources = sources();
    let names: Vec<&str> = sources.iter().map(|(name, _)| name.as_str()).collect();
    assert!(
        names.contains(&"auth.rs") && names.contains(&"mod.rs"),
        "{names:?}"
    );
    let auth = function(include_str!("auth.rs"), "admitted");
    assert!(auth.contains(".and_then(bearer)"), "{auth}");
    assert!(auth.contains("token.matches(given.as_bytes())"), "{auth}");
    let lease = function(include_str!("../api.rs"), "authenticate");
    assert!(lease.contains(".and_then(bearer)"), "{lease}");
    assert!(lease.contains("token.matches(given.as_bytes())"), "{lease}");
    let calls: Vec<(&str, usize)> = sources
        .iter()
        .map(|(name, text)| (name.as_str(), token_comparisons(text)))
        .filter(|(_, count)| *count > 0)
        .collect();
    assert_eq!(calls, [("auth.rs", 1)]);
    for (name, text) in &sources {
        assert_eq!(other_comparisons(text), Vec::<&str>::new(), "{name}");
    }

    let copied = "fn a() { token.matches(x) }\nfn b() { token.matches(y) }\n";
    assert_eq!(token_comparisons(copied), 2);
    let own = "let ok = given.ct_eq(&token.0).into() || token.same_as(&other);";
    assert_eq!(other_comparisons(own), ["ct_eq", "same_as("]);
    assert_eq!(function("fn other() {\n}\n", "admitted"), "");
    assert_eq!(
        function("fn admitted() {\n    x\n}", "admitted"),
        "fn admitted() {\n    x\n}"
    );
}

/// What the job service must not name: a clock read of its own (it takes the server's
/// clock) and the provider types (it reads no provider catalog).
const FORBIDDEN: [&str; 4] = [
    "SystemTime",
    "Instant::now",
    "CapacityProvider",
    "ElasticProvider",
];

/// The words of [`FORBIDDEN`] that `source` holds.
fn forbidden(source: &str) -> Vec<&'static str> {
    FORBIDDEN
        .into_iter()
        .filter(|word| source.contains(word))
        .collect()
}

/// Asserts that no file of `job_api` names `SystemTime`, `Instant::now`, `CapacityProvider`
/// or `ElasticProvider`, and that the check finds each in a text that uses it.
#[test]
fn the_job_api_reads_no_clock_and_names_no_provider() {
    for (name, text) in sources() {
        assert_eq!(forbidden(&text), Vec::<&str>::new(), "{name}");
    }
    let text = "let t = SystemTime::now(); let i = Instant::now(); \
                fn p(c: &dyn CapacityProvider, e: &dyn ElasticProvider) {}";
    assert_eq!(forbidden(text), FORBIDDEN);
}
