//! Source checks of `job_api`, read from the crate's own source tree: the presented token is
//! compared in one place, the job service reads no clock of its own and names no provider,
//! and the read calls take no lock. Each check is also run on a text that breaks it, so it is
//! shown to fail.

use std::fs;
use std::path::Path;

/// The sources of `job_api` that are not tests (every `.rs` file of the directory and of its
/// subdirectories whose name does not end in `tests.rs`), as (path under the directory,
/// text), by path.
fn sources() -> Vec<(String, String)> {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("src/job_api");
    let mut sources = Vec::new();
    let mut dirs = vec![dir.clone()];
    while let Some(next) = dirs.pop() {
        for entry in fs::read_dir(next).expect("the module's directory reads") {
            let path = entry.expect("the directory entry reads").path();
            let name = path.strip_prefix(&dir).expect("under the directory");
            let name = name.to_string_lossy().into_owned();
            if path.is_dir() {
                dirs.push(path);
            } else if !name.ends_with("tests.rs") {
                let text = fs::read_to_string(&path).expect("the source reads");
                sources.push((name, text));
            }
        }
    }
    sources.sort();
    sources
}

/// The number of calls of `Token::matches` in `source`.
fn token_comparisons(source: &str) -> usize {
    source.matches(".matches(").count()
}

/// The number of calls of `Clients::identify` in `source`.
fn client_comparisons(source: &str) -> usize {
    source.matches(".identify(").count()
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

/// Asserts that the bearer layer (`auth.rs`, its `client`) reads the header with `bearer`
/// and compares the token with every client's by `Clients::identify`, as the lease routes'
/// middleware (`api.rs`, its `authenticate`) reads it with `bearer` and compares it by
/// `Token::matches`; and that no file of `job_api` holds another call of either or any other
/// comparison of a token, so both transports go through the one comparison. Each check is
/// shown to fail on a text that copies the comparison or writes its own.
#[test]
fn one_comparison_serves_both_transports() {
    let sources = sources();
    let names: Vec<&str> = sources.iter().map(|(name, _)| name.as_str()).collect();
    assert!(
        names.contains(&"auth.rs") && names.contains(&"mod.rs"),
        "{names:?}"
    );
    let auth = function(include_str!("auth.rs"), "client");
    assert!(auth.contains(".and_then(bearer)"), "{auth}");
    assert!(
        auth.contains("clients.identify(given.as_bytes())"),
        "{auth}"
    );
    let lease = function(include_str!("../api.rs"), "authenticate");
    assert!(lease.contains(".and_then(bearer)"), "{lease}");
    assert!(lease.contains("token.matches(given.as_bytes())"), "{lease}");
    let calls: Vec<(&str, usize, usize)> = sources
        .iter()
        .map(|(name, text)| {
            let counts = (token_comparisons(text), client_comparisons(text));
            (name.as_str(), counts.0, counts.1)
        })
        .filter(|(_, tokens, clients)| tokens + clients > 0)
        .collect();
    assert_eq!(calls, [("auth.rs", 0, 1)]);
    for (name, text) in &sources {
        assert_eq!(other_comparisons(text), Vec::<&str>::new(), "{name}");
    }

    let copied = "fn a() { token.matches(x) }\nfn b() { token.matches(y) }\n";
    assert_eq!(token_comparisons(copied), 2);
    let twice = "fn a() { c.identify(x) }\nfn b() { c.identify(y) }\n";
    assert_eq!(client_comparisons(twice), 2);
    let own = "let ok = given.ct_eq(&token.0).into() || token.same_as(&other);";
    assert_eq!(other_comparisons(own), ["ct_eq", "same_as("]);
    assert_eq!(function("fn other() {\n}\n", "admitted"), "");
    assert_eq!(
        function("fn admitted() {\n    x\n}", "admitted"),
        "fn admitted() {\n    x\n}"
    );
}

/// What the job service must not name: a clock read of its own (it takes the server's
/// clock) and the provider types (it reaches a catalog only through the submit group's
/// admission trait, which the server implements outside `job_api`).
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

/// What the read calls must not name: a lock, or the lease table or the registry that the
/// server guards with one.
const LOCKS: [&str; 5] = ["Mutex", "RwLock", ".lock(", "Registry", "LeaseTable"];

/// The words of [`LOCKS`] that `source` holds.
fn locks(source: &str) -> Vec<&'static str> {
    LOCKS
        .into_iter()
        .filter(|word| source.contains(word))
        .collect()
}

/// Asserts that the read calls (`read.rs` and `read/view.rs`, both found among the sources)
/// name no lock, no lease table and no registry, so none of them holds a lock of the server
/// across a read of the store; and that the check finds each word in a text that uses it.
#[test]
fn the_read_calls_take_no_lock() {
    let sources = sources();
    let read: Vec<&(String, String)> = sources
        .iter()
        .filter(|(name, _)| name == "read.rs" || name == "read/view.rs")
        .collect();
    assert_eq!(
        read.len(),
        2,
        "{:?}",
        sources.iter().map(|(name, _)| name).collect::<Vec<_>>()
    );
    for (name, text) in read {
        assert_eq!(locks(text), Vec::<&str>::new(), "{name}");
    }
    let text = "let t: Mutex<LeaseTable<P, C>> = x; let r: RwLock<Registry> = y; t.lock();";
    assert_eq!(locks(text), LOCKS);
}
