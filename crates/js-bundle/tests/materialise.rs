//! Materialising a bundle: where trees are, the environment order, locks and
//! placeholders in a job's own values.

mod common;

use std::collections::BTreeMap;
use std::ffi::OsStr;
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};

use common::{edited, golden, minimal, set};
use js_bundle::{
    BundleConfig, ConfigError, DiffId, EnvNameError, ExpandError, Inputs, MAX_ENV_BYTES,
    MaterialiseError, ReadGrantKind, SYSTEM_PATH, TemplateError, TreeName, materialise, tree_path,
};
use serde_json::{Value, json};

const ROOT: &str = "/store";

fn dir(root: &str, c: char) -> String {
    format!("{root}/v1/objects/trees/{}", c.to_string().repeat(64))
}

fn s(text: &str) -> String {
    text.to_string()
}

struct Run {
    root: PathBuf,
    writable: BTreeMap<TreeName, PathBuf>,
    developer_dir: Option<PathBuf>,
    base: Vec<(String, String)>,
    job_env: BTreeMap<String, String>,
    job_args: Vec<String>,
}

impl Run {
    fn new() -> Run {
        Run {
            root: PathBuf::from(ROOT),
            writable: BTreeMap::new(),
            developer_dir: None,
            base: Vec::new(),
            job_env: BTreeMap::new(),
            job_args: Vec::new(),
        }
    }

    fn go(&self, config: &BundleConfig) -> Result<js_bundle::Materialised, MaterialiseError> {
        materialise(
            config,
            &Inputs {
                store_root: &self.root,
                writable: &self.writable,
                developer_dir: self.developer_dir.as_deref(),
                base_env: &self.base,
                job_env: &self.job_env,
                job_args: &self.job_args,
            },
        )
    }
}

fn env_of(config: &BundleConfig, run: &Run) -> Vec<(String, String)> {
    run.go(config).unwrap().env
}

fn with_env(rules: Value) -> BundleConfig {
    edited(|v| {
        set(v, "/env", rules);
        set(v, "/system_path", json!(false));
    })
    .unwrap()
}

fn rule(name: &str, op: &str, values: Value, locked: bool) -> Value {
    json!({"name": name, "op": op, "values": values, "locked": locked})
}

/// The golden bundle resolves: trees below the store root by diff id, the base then
/// the bundle's rules then the system directories, the reads, and entrypoints.
#[test]
fn golden_bundle_resolves() {
    let mut run = Run::new();
    run.base = vec![(s("HOME"), s("/run/home"))];
    run.job_args = vec![s("--sysroot=${tree:zig}/lib"), s("plain")];
    let got = run.go(&golden()).unwrap();
    let (job, zig) = (dir(ROOT, 'a'), dir(ROOT, 'b'));
    assert_eq!(
        got.trees[&TreeName::parse("job").unwrap()],
        PathBuf::from(&job)
    );
    assert_eq!(
        got.trees[&TreeName::parse("zig").unwrap()],
        PathBuf::from(&zig)
    );
    assert_eq!(
        got.env,
        vec![
            (s("HOME"), s("/run/home")),
            (s("PATH"), format!("{job}/bin:{zig}:{SYSTEM_PATH}")),
            (s("ZIG_LIB_DIR"), format!("{zig}/lib")),
        ]
    );
    assert_eq!(got.args, vec![format!("--sysroot={zig}/lib"), s("plain")]);
    assert_eq!(
        got.reads,
        vec![ReadGrantKind::SystemCerts, ReadGrantKind::Timezone]
    );
    assert_eq!(
        got.entrypoints["kbf-job"],
        PathBuf::from(format!("{job}/bin/kbf-job"))
    );
    assert_eq!(got.entrypoints["zig"], PathBuf::from(format!("{zig}/zig")));
    assert_eq!(got.entrypoints.len(), 2);
}

/// Resolution depends on the config, the store root and the inputs and on nothing
/// else: the same inputs give the same result, another root moves every path.
#[test]
fn resolution_is_a_function_of_its_inputs() {
    let run = Run::new();
    assert_eq!(run.go(&golden()), run.go(&golden()));
    let mut other = Run::new();
    other.root = PathBuf::from("/elsewhere");
    let moved = other.go(&golden()).unwrap();
    assert_eq!(
        moved.trees[&TreeName::parse("job").unwrap()],
        PathBuf::from(dir("/elsewhere", 'a'))
    );
    let id = DiffId::parse(&common::hex('a')).unwrap();
    assert_eq!(
        tree_path(Path::new(ROOT), &id),
        PathBuf::from(dir(ROOT, 'a'))
    );
}

/// The order of the environment is the base, the bundle's rules, `DEVELOPER_DIR`, then
/// the job's; the job's value replaces a bundle value that is not locked.
#[test]
fn env_order_is_base_bundle_developer_dir_job() {
    let config = with_env(json!([
        rule("A", "set", json!(["from-bundle"]), false),
        rule("B", "set", json!(["${developer_dir}/b"]), false),
    ]));
    let mut run = Run::new();
    run.base = vec![(s("TMPDIR"), s("/t"))];
    run.developer_dir = Some(PathBuf::from("/Xcode/Developer"));
    run.job_env = BTreeMap::from([(s("A"), s("from-job")), (s("Z"), s("${tree:job}/z"))]);
    let got = run.go(&config).unwrap();
    assert_eq!(
        got.env,
        vec![
            (s("TMPDIR"), s("/t")),
            (s("A"), s("from-job")),
            (s("B"), s("/Xcode/Developer/b")),
            (s("DEVELOPER_DIR"), s("/Xcode/Developer")),
            (s("Z"), format!("{}/z", dir(ROOT, 'a'))),
        ]
    );
}

/// Each operation against an absent, a present and an empty variable.
#[test]
fn operations_combine_with_the_existing_value() {
    let config = with_env(json!([
        rule("SET", "set", json!(["new"]), false),
        rule("DEF_PRESENT", "default", json!(["new"]), false),
        rule("DEF_ABSENT", "default", json!(["new"]), false),
        rule("PRE", "prepend", json!(["p1", "p2"]), false),
        rule("PRE_EMPTY", "prepend", json!(["p"]), false),
        rule("PRE_ABSENT", "prepend", json!(["p"]), false),
        rule("APP", "append", json!(["a"]), false),
        rule("APP_ABSENT", "append", json!(["a"]), false),
    ]));
    let mut run = Run::new();
    run.base = vec![
        (s("SET"), s("old")),
        (s("DEF_PRESENT"), s("old")),
        (s("PRE"), s("old")),
        (s("PRE_EMPTY"), s("")),
        (s("APP"), s("old")),
    ];
    let env: BTreeMap<_, _> = env_of(&config, &run).into_iter().collect();
    let want = BTreeMap::from([
        (s("SET"), s("new")),
        (s("DEF_PRESENT"), s("old")),
        (s("DEF_ABSENT"), s("new")),
        (s("PRE"), s("p1:p2:old")),
        (s("PRE_EMPTY"), s("p")),
        (s("PRE_ABSENT"), s("p")),
        (s("APP"), s("old:a")),
        (s("APP_ABSENT"), s("a")),
    ]);
    assert_eq!(env, want);
}

/// `system_path` adds the four directories after the bundle's entries; without it a
/// bundle with no `PATH` rule has no `PATH`.
#[test]
fn system_path_follows_the_flag() {
    let mut run = Run::new();
    let with = edited(|v| set(v, "/env", json!([]))).unwrap();
    assert_eq!(env_of(&with, &run), vec![(s("PATH"), s(SYSTEM_PATH))]);
    run.base = vec![(s("PATH"), s("/base"))];
    assert_eq!(
        env_of(&with, &run),
        vec![(s("PATH"), format!("/base:{SYSTEM_PATH}"))]
    );
    let without = edited(|v| {
        set(v, "/env", json!([]));
        set(v, "/system_path", json!(false));
    })
    .unwrap();
    assert_eq!(env_of(&without, &Run::new()), Vec::new());
}

/// A job cannot set a variable the bundle locked, nor one the agent reserves.
#[test]
fn job_env_respects_locks_and_reserved_names() {
    let mut run = Run::new();
    run.job_env = BTreeMap::from([(s("ZIG_LIB_DIR"), s("/evil"))]);
    assert_eq!(
        run.go(&golden()),
        Err(MaterialiseError::Locked(s("ZIG_LIB_DIR")))
    );
    run.job_env = BTreeMap::from([(s("HOME"), s("/evil"))]);
    assert_eq!(
        run.go(&golden()),
        Err(MaterialiseError::JobEnvName {
            name: s("HOME"),
            why: EnvNameError::Forbidden
        })
    );
    run.job_env = BTreeMap::from([(s("1BAD"), s("x"))]);
    assert_eq!(
        run.go(&golden()),
        Err(MaterialiseError::JobEnvName {
            name: s("1BAD"),
            why: EnvNameError::Malformed
        })
    );
    run.job_env = BTreeMap::from([(s("PATH"), s("/job/bin"))]);
    let env = env_of(&golden(), &run);
    assert!(env.contains(&(s("PATH"), s("/job/bin"))));
}

/// Placeholders in a job's own values follow the config's rules.
#[test]
fn job_values_with_bad_placeholders_are_refused() {
    let cases = [
        ("${tree:job}/../x", true, TemplateError::ParentDir),
        ("${tree:job}/../x", false, TemplateError::ParentDir),
        ("a\nb", true, TemplateError::Newline),
    ];
    for (text, in_env, err) in cases {
        let mut run = Run::new();
        let what = if in_env {
            run.job_env = BTreeMap::from([(s("X"), s(text))]);
            s("env X")
        } else {
            run.job_args = vec![s("ok"), s(text)];
            s("arg 1")
        };
        let want = MaterialiseError::Expand {
            what,
            source: ExpandError::Template(err),
        };
        assert_eq!(run.go(&golden()), Err(want), "{text:?}");
    }
    let mut run = Run::new();
    run.job_args = vec![s("${tree:nope}/x")];
    assert_eq!(
        run.go(&golden()),
        Err(MaterialiseError::Expand {
            what: s("arg 0"),
            source: ExpandError::UnknownTree(TreeName::parse("nope").unwrap())
        })
    );
}

/// `${developer_dir}` with no Xcode selected is refused, in a bundle value and in a
/// job argument.
#[test]
fn developer_dir_needs_a_selected_xcode() {
    let config = with_env(json!([rule(
        "SDK",
        "set",
        json!(["${developer_dir}/sdk"]),
        false
    )]));
    let err = Run::new().go(&config).unwrap_err();
    assert_eq!(
        err,
        MaterialiseError::Expand {
            what: s("env SDK"),
            source: ExpandError::NoDeveloperDir
        }
    );
    let mut run = Run::new();
    run.job_args = vec![s("${developer_dir}")];
    assert!(matches!(
        run.go(&golden()),
        Err(MaterialiseError::Expand {
            source: ExpandError::NoDeveloperDir,
            ..
        })
    ));
}

/// A writable tree resolves to its clone; a missing or an extra clone is refused.
#[test]
fn writable_trees_resolve_to_their_clones() {
    let config = minimal();
    let tool = TreeName::parse("tool").unwrap();
    let mut run = Run::new();
    assert_eq!(
        run.go(&config),
        Err(MaterialiseError::MissingWritable(tool.clone()))
    );
    run.writable
        .insert(tool.clone(), PathBuf::from("/run/1/bundle-w/tool"));
    let got = run.go(&config).unwrap();
    assert_eq!(got.trees[&tool], PathBuf::from("/run/1/bundle-w/tool"));
    assert_eq!(got.env, Vec::new());
    assert!(got.entrypoints.is_empty());
    assert!(got.reads.is_empty());

    let mut run = Run::new();
    let job = TreeName::parse("job").unwrap();
    run.writable
        .insert(job.clone(), PathBuf::from("/run/1/job"));
    assert_eq!(
        run.go(&golden()),
        Err(MaterialiseError::UnexpectedWritable(job))
    );

    let mut run = Run::new();
    run.writable.insert(tool, PathBuf::from("relative/clone"));
    assert_eq!(
        run.go(&config),
        Err(MaterialiseError::BadPath(PathBuf::from("relative/clone")))
    );
}

/// Paths the caller gives must be absolute, normalised UTF-8 without control
/// characters.
#[test]
fn caller_paths_must_be_normalised() {
    let bad: Vec<PathBuf> = vec![
        PathBuf::from("relative"),
        PathBuf::from("/a/../b"),
        PathBuf::from("/a/./b"),
        PathBuf::from("/a//b"),
        PathBuf::from("/a/"),
        PathBuf::from("/"),
        PathBuf::from("/a\nb"),
        PathBuf::from(OsStr::from_bytes(b"/a\xffb")),
    ];
    for path in bad {
        let mut run = Run::new();
        run.root = path.clone();
        assert_eq!(
            run.go(&golden()),
            Err(MaterialiseError::BadPath(path.clone())),
            "{path:?}"
        );
        let mut run = Run::new();
        run.developer_dir = Some(path.clone());
        assert_eq!(
            run.go(&golden()),
            Err(MaterialiseError::BadPath(path.clone())),
            "{path:?}"
        );
    }
}

/// A config that does not validate is refused before anything is resolved.
#[test]
fn invalid_config_is_refused() {
    let mut config = golden();
    config.os = s("ios");
    assert!(matches!(
        Run::new().go(&config),
        Err(MaterialiseError::Config(ConfigError::Invalid { .. }))
    ));
}

/// Errors say which value failed.
#[test]
fn errors_read_as_text() {
    let err = MaterialiseError::Expand {
        what: s("arg 1"),
        source: ExpandError::NoDeveloperDir,
    };
    assert_eq!(
        err.to_string(),
        "arg 1: no Xcode is selected for ${developer_dir}"
    );
    assert_eq!(
        MaterialiseError::Locked(s("X")).to_string(),
        "job env \"X\" is locked by the bundle"
    );
}

/// A colon in a caller path is refused: the paths are joined into `PATH`-like values
/// with `:`, so a `:` would add a directory. The store root, a writable clone and the
/// developer directory are all caller paths.
#[test]
fn colon_in_a_caller_path_is_refused() {
    let bad = PathBuf::from("/x:/evil");
    let mut run = Run::new();
    run.root = bad.clone();
    assert_eq!(
        run.go(&golden()),
        Err(MaterialiseError::BadPath(bad.clone()))
    );

    let mut run = Run::new();
    run.developer_dir = Some(bad.clone());
    assert_eq!(
        run.go(&golden()),
        Err(MaterialiseError::BadPath(bad.clone()))
    );

    let mut run = Run::new();
    run.writable
        .insert(TreeName::parse("tool").unwrap(), bad.clone());
    assert_eq!(run.go(&minimal()), Err(MaterialiseError::BadPath(bad)));
}

/// The job's names go through the shared reserved list: allocator, Objective-C runtime,
/// launchd and agent names are refused, with the name in the error.
#[test]
fn job_env_names_use_the_shared_reserved_list() {
    for name in ["MallocStackLogging", "OBJC_DEBUG", "__XPC_X", "JS_FOO"] {
        let mut run = Run::new();
        run.job_env = BTreeMap::from([(s(name), s("1"))]);
        assert_eq!(
            run.go(&golden()),
            Err(MaterialiseError::JobEnvName {
                name: s(name),
                why: EnvNameError::Forbidden
            }),
            "{name}"
        );
    }
}

/// The environment handed to the program is bounded: exactly `MAX_ENV_BYTES` (names,
/// values, and an `=` and a NUL for each) is accepted, one byte more is refused with
/// the size, so the failure is here and not an `E2BIG` at exec.
#[test]
fn materialised_environment_is_bounded() {
    let mut run = Run::new();
    run.writable
        .insert(TreeName::parse("tool").unwrap(), PathBuf::from("/run/c"));
    let mut env: BTreeMap<String, String> = (0..63)
        .map(|i| (format!("K{i:03}"), "v".repeat(4096)))
        .collect();
    let used: usize = env.iter().map(|(k, v)| k.len() + v.len() + 2).sum();
    let last = MAX_ENV_BYTES - used - 2 - 2;
    assert!(last <= 4096);
    env.insert(s("KZ"), "v".repeat(last));
    run.job_env = env.clone();
    assert_eq!(run.go(&minimal()).unwrap().env.len(), 64);

    env.insert(s("KZ"), "v".repeat(last + 1));
    run.job_env = env;
    assert_eq!(
        run.go(&minimal()),
        Err(MaterialiseError::EnvTooLarge {
            len: MAX_ENV_BYTES + 1,
            max: MAX_ENV_BYTES
        })
    );
}

/// The agent's own base environment counts toward the same bound.
#[test]
fn base_environment_counts_toward_the_bound() {
    let mut run = Run::new();
    run.writable
        .insert(TreeName::parse("tool").unwrap(), PathBuf::from("/run/c"));
    run.base = vec![(s("BIG"), "b".repeat(MAX_ENV_BYTES))];
    assert!(matches!(
        run.go(&minimal()),
        Err(MaterialiseError::EnvTooLarge { .. })
    ));
}
