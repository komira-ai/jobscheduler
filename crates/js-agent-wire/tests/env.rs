//! The one environment check, [`js_agent_wire::check_env`]: a job start's `env` and a
//! native body's `env` meet the same rules, with the same verdicts, and only the field a
//! refusal names differs.

mod common;

use std::collections::BTreeMap;

use common::{expect, job, long, map};
use js_agent_wire::limits::{MAX_ENV, MAX_ENV_KEY_LEN, MAX_ENV_TOTAL_BYTES, MAX_ENV_VALUE_LEN};
use js_agent_wire::{
    BookedResources, Command, Isolation, JobSpec, JobStartSpec, NativeSpec, NetworkLevel, Rule,
    ValidationError, check_env, refusal,
};
use serde_json::json;

const DIGEST: &str = "sha256:0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";

/// A job start with the environment `env` and nothing else that could be refused.
fn start(env: BTreeMap<String, String>) -> JobStartSpec {
    JobStartSpec {
        job_id: "job-1".into(),
        allocation_id: "alloc-1".into(),
        image: format!("ubuntu:24.04@{DIGEST}"),
        command: Vec::new(),
        run: None,
        os: None,
        arch: None,
        env,
        files: Vec::new(),
        booked: BookedResources {
            cpu_millis: 1000,
            memory_bytes: 1 << 30,
            disk_bytes: None,
            gpu_indices: Vec::new(),
        },
        wall_ms: 60_000,
        network: Default::default(),
        ports: Vec::new(),
        log_sink: None,
        kind: Default::default(),
        isolation: Default::default(),
    }
}

/// A run start whose native body has the environment `env` and nothing else that could be
/// refused.
fn native(env: BTreeMap<String, String>) -> JobSpec {
    let mut spec = job();
    spec.image = String::new();
    spec.args = Vec::new();
    spec.gpu_indices = Vec::new();
    spec.mounts = Vec::new();
    spec.isolation = Isolation::Contained;
    spec.native = Some(NativeSpec {
        program: "/usr/bin/clang".into(),
        args: Vec::new(),
        env,
        cwd: String::new(),
        inputs: Vec::new(),
        outputs: Vec::new(),
        network: NetworkLevel::Off,
        ports: Vec::new(),
    });
    spec
}

fn validate_start(env: &BTreeMap<String, String>) -> Result<(), ValidationError> {
    start(env.clone()).validate()
}

fn validate_native(env: &BTreeMap<String, String>) -> Result<(), ValidationError> {
    native(env.clone()).validate()
}

/// `count` entries `V0`, `V1`, ... each with the value `value`.
fn entries(count: usize, value: &str) -> BTreeMap<String, String> {
    (0..count)
        .map(|n| (format!("V{n}"), value.to_owned()))
        .collect()
}

/// The environments at and over each bound, each with the rule it breaks (`None` for one
/// at the bound, which passes). Every limit is read from `limits`.
fn table() -> Vec<(&'static str, BTreeMap<String, String>, Option<Rule>)> {
    // Entries each within the value bound that together pass the total by one byte.
    let full = long(MAX_ENV_VALUE_LEN);
    let per_entry = 2 + MAX_ENV_VALUE_LEN;
    let fits = MAX_ENV_TOTAL_BYTES / per_entry;
    let mut at_total = entries(fits, &full);
    let used = fits * per_entry;
    at_total.insert("Z".into(), long(MAX_ENV_TOTAL_BYTES - used - 1));
    let mut over_total = at_total.clone();
    over_total.insert("Z".into(), long(MAX_ENV_TOTAL_BYTES - used));
    vec![
        ("empty", BTreeMap::new(), None),
        ("count at the bound", entries(MAX_ENV, "x"), None),
        (
            "count over the bound",
            entries(MAX_ENV + 1, "x"),
            Some(Rule::TooMany),
        ),
        (
            "name at the bound",
            BTreeMap::from([(long(MAX_ENV_KEY_LEN), "x".into())]),
            None,
        ),
        (
            "name over the bound",
            BTreeMap::from([(long(MAX_ENV_KEY_LEN + 1), "x".into())]),
            Some(Rule::TooLong),
        ),
        (
            "value at the bound",
            BTreeMap::from([("A".into(), long(MAX_ENV_VALUE_LEN))]),
            None,
        ),
        (
            "value over the bound",
            BTreeMap::from([("A".into(), long(MAX_ENV_VALUE_LEN + 1))]),
            Some(Rule::TooLong),
        ),
        ("total at the bound", at_total, None),
        ("total over the bound", over_total, Some(Rule::TooLong)),
        (
            "NUL in a value",
            map(&[("A", "x\0y")]),
            Some(Rule::BadChars),
        ),
        ("empty name", map(&[("", "x")]), Some(Rule::Empty)),
        (
            "name led by a digit",
            map(&[("1A", "x")]),
            Some(Rule::BadChars),
        ),
        (
            "name with a dash",
            map(&[("A-B", "x")]),
            Some(Rule::BadChars),
        ),
        (
            "name with an equals sign",
            map(&[("A=B", "x")]),
            Some(Rule::BadChars),
        ),
        ("name led by an underscore", map(&[("_A1", "x")]), None),
        (
            "reserved name",
            map(&[("LD_PRELOAD", "x")]),
            Some(Rule::Reserved),
        ),
        (
            "value with what a shell or a flag parser reads",
            map(&[("A", "-x=y\n$(z)")]),
            None,
        ),
    ]
}

/// Asserts that a job start's environment over each bound is refused `invalid-env` naming
/// `env`, and one at each bound passes: the count, the name length, the value length, the
/// total (made of entries each within the value bound), a NUL in a value, and names that
/// break the grammar. It catches a bound that is missing or off by one.
#[test]
fn an_env_above_the_bounds_is_refused() {
    let total: usize = table()
        .iter()
        .filter(|(case, ..)| *case == "total over the bound")
        .flat_map(|(_, env, _)| env.iter().map(|(k, v)| k.len() + v.len()))
        .sum();
    assert_eq!(total, MAX_ENV_TOTAL_BYTES + 1);
    for (case, env, verdict) in table() {
        match verdict {
            None => assert_eq!(validate_start(&env), Ok(()), "{case}"),
            Some(rule) => expect(
                validate_start(&env).unwrap_err(),
                "env",
                rule,
                refusal::INVALID_ENV,
            ),
        }
    }
}

/// Asserts that one table of environments gets the same verdict as a job start's `env`, a
/// native body's `env` and the public check itself, each naming its own field. It catches
/// two copies of the check drifting apart.
#[test]
fn native_and_job_env_share_one_validation() {
    for (case, env, verdict) in table() {
        let verdict_of = |result: Result<(), ValidationError>| {
            result.map_err(|err| (err.rule(), err.code())).err()
        };
        let want = verdict.map(|rule| (rule, refusal::INVALID_ENV));
        assert_eq!(verdict_of(validate_start(&env)), want, "job start: {case}");
        assert_eq!(verdict_of(validate_native(&env)), want, "native: {case}");
        assert_eq!(verdict_of(check_env("x", &env)), want, "check_env: {case}");
        if let Err(err) = validate_native(&env) {
            assert_eq!(err.field(), "native.env", "{case}");
        }
        if let Err(err) = check_env("x", &env) {
            assert_eq!(err.field(), "x", "{case}");
        }
    }
}

/// Asserts that a job start naming a reserved variable is refused `invalid-env` naming
/// `env`: the loader's `LD_PRELOAD`, the agent's own `JS_AGENT_TOKEN`, and a name under
/// each reserved prefix of `js_types`. It catches a job start that could steer the
/// loader or pose as the agent in its container.
#[test]
fn job_start_with_a_reserved_env_name_is_refused() {
    let mut names = vec!["LD_PRELOAD".to_owned(), "JS_AGENT_TOKEN".to_owned()];
    names.extend(
        js_types::env::RESERVED_ENV_PREFIXES
            .iter()
            .map(|prefix| format!("{prefix}X")),
    );
    for name in names {
        let env = BTreeMap::from([(name.clone(), "v".to_owned())]);
        let err = validate_start(&env).unwrap_err();
        expect(err, "env", Rule::Reserved, refusal::INVALID_ENV);
    }
}

/// Asserts that a job start with no environment encodes without an `env` key, and one
/// with an environment encodes it as an object. It catches an empty environment that is
/// sent anyway, which an agent that predates the field would still read but which the
/// goldens do not hold.
#[test]
fn a_job_with_no_env_is_unchanged() {
    let bare = serde_json::to_value(Command::JobStart(Box::new(start(BTreeMap::new())))).unwrap();
    assert!(bare.get("env").is_none(), "{bare}");
    let set = serde_json::to_value(Command::JobStart(Box::new(start(map(&[
        ("A", "1"),
        ("B", "two words"),
    ])))))
    .unwrap();
    assert_eq!(set["env"], json!({"A": "1", "B": "two words"}));
}
