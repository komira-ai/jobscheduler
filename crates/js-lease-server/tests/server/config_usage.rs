//! The usage settings in the configuration file: `server.usage_db`, `server.usage_off` and
//! `server.profile_store_bytes`, under the names, defaults and rules of the flags
//! `--usage-db`, `--usage-off` and `--profile-store-bytes`.

use std::path::Path;

use crate::config_support::{PoolText, file, load, refusal, server};
use crate::support::Scratch;

/// `lines` as the `[server]` keys of a file with one static pool, loaded.
fn loaded(name: &str, lines: &str) -> (Scratch, Result<js_lease_server::Config, String>) {
    let scratch = Scratch::new(name);
    let stat = PoolText::static_pool(&scratch, "hosts");
    let text = file(&server(&scratch, lines), &[&stat]);
    let config = load(&scratch, &text).map_err(|err| err.to_string());
    (scratch, config)
}

/// Asserts the defaults of a file with no usage key: usage on, in `usage.db` beside the
/// lease file, with a budget of 256 MiB, as with no usage flag. It catches a default
/// that lands in the working directory or a budget of zero.
#[test]
fn a_file_without_usage_keys_has_the_flags_defaults() {
    let (scratch, config) = loaded("usage_file_defaults", "");
    let config = config.unwrap();
    assert_eq!(config.usage_db(), Some(scratch.path("usage.db").as_path()));
    assert_eq!(config.profile_store_bytes(), 268_435_456);
}

/// Asserts that the keys set usage as the flags did: a given path is kept, the budget is
/// kept, and `usage_off = true` leaves no file. It catches a key the reader takes and
/// drops, and an off switch that leaves the file named.
#[test]
fn the_usage_keys_set_usage_as_the_flags_do() {
    let lines = "usage_db = \"/var/lib/js/u.db\"\nprofile_store_bytes = 1";
    let config = loaded("usage_file_given", lines).1.unwrap();
    assert_eq!(config.usage_db(), Some(Path::new("/var/lib/js/u.db")));
    assert_eq!(config.profile_store_bytes(), 1);

    let config = loaded("usage_file_off", "usage_off = true").1.unwrap();
    assert_eq!(config.usage_db(), None);

    let (scratch, config) = loaded("usage_file_not_off", "usage_off = false");
    let config = config.unwrap();
    assert_eq!(config.usage_db(), Some(scratch.path("usage.db").as_path()));
}

/// Asserts the refusals, each with the rule of its flag: a budget of zero, a negative or
/// text budget, a value of the wrong type, and `usage_off = true` beside `usage_db`
/// (the flags conflict). They are refused naming the key and never repeat the value.
/// It catches a zero budget that would evict every profile it learns, and two keys that
/// contradict.
#[test]
fn the_usage_keys_are_refused_with_the_flags_rules() {
    let max = u64::MAX;
    for (lines, said) in [
        (
            "profile_store_bytes = 0".to_owned(),
            format!("server.profile_store_bytes: must be from 1 to {max}"),
        ),
        (
            "profile_store_bytes = -1".to_owned(),
            "server.profile_store_bytes: must not be negative".to_owned(),
        ),
        (
            "profile_store_bytes = \"lots\"".to_owned(),
            "server.profile_store_bytes: must be an integer".to_owned(),
        ),
        (
            "usage_off = \"yes\"".to_owned(),
            "server.usage_off: must be true or false".to_owned(),
        ),
        (
            "usage_db = 5".to_owned(),
            "server.usage_db: must be a string".to_owned(),
        ),
        (
            "usage_db = \"u.db\"\nusage_off = true".to_owned(),
            "server.usage_db: not allowed with server.usage_off".to_owned(),
        ),
    ] {
        let scratch = Scratch::new("usage_file_refused");
        let stat = PoolText::static_pool(&scratch, "hosts");
        let text = file(&server(&scratch, &lines), &[&stat]);
        assert_eq!(refusal(&scratch, &text), said, "{lines}");
    }
}
