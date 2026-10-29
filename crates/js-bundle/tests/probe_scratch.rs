//! Scratch probes of current behaviour.

mod common;

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use common::{MINIMAL, minimal};
use js_bundle::{
    BundleConfig, ConfigError, Inputs, Template, TemplateError, TreeName, check_env_name,
    materialise,
};

fn parse(text: &str) -> Result<BundleConfig, ConfigError> {
    BundleConfig::parse(text.as_bytes())
}

#[test]
fn probe_duplicate_entrypoint_last_wins() {
    let text = MINIMAL.replace(
        "\"entrypoints\":{}",
        "\"entrypoints\":{\"a\":{\"path\":\"${tree:tool}/one\"},\"a\":{\"path\":\"${tree:tool}/two\"}}",
    );
    let config = parse(&text).unwrap();
    assert_eq!(config.entrypoints.len(), 1);
    assert_eq!(config.entrypoints["a"].path.as_str(), "${tree:tool}/two");
}

#[test]
fn probe_unknown_field_error_echoes_raw_key() {
    let text = MINIMAL.replace("\"schema\":1,", "\"sch\\nema\\u001b[31m\":1,\"schema\":1,");
    match parse(&text) {
        Err(ConfigError::Json(msg)) => {
            assert!(msg.contains('\n') && msg.contains('\u{1b}'), "{msg:?}")
        }
        other => panic!("{other:?}"),
    }
}

#[test]
fn probe_unknown_field_error_is_as_large_as_the_key() {
    let key = "k".repeat(900_000);
    let text = MINIMAL.replace("\"schema\":1,", &format!("\"{key}\":1,\"schema\":1,"));
    match parse(&text) {
        Err(ConfigError::Json(msg)) => assert!(msg.len() > 900_000, "{}", msg.len()),
        other => panic!("{other:?}"),
    }
}

#[test]
fn probe_deep_nesting_is_an_error_not_a_crash() {
    for depth in [200, 100_000, 500_000] {
        let text = format!("{{\"layers\":{}", "[".repeat(depth));
        assert!(matches!(parse(&text), Err(ConfigError::Json(_))));
    }
}

#[test]
fn probe_struct_level_duplicates_are_refused() {
    let layer = MINIMAL.replace("\"tree\":\"tool\",", "\"tree\":\"tool\",\"tree\":\"tool\",");
    let seat = MINIMAL.replace(
        "\"network_max\":\"off\"",
        "\"network_max\":\"off\",\"network_max\":\"on\"",
    );
    let xcode = MINIMAL.replace(
        "\"built\":",
        "\"xcode\":{\"min\":\"15.0\",\"components\":[]},\"xcode\":null,\"built\":",
    );
    for text in [layer, seat, xcode] {
        match parse(&text) {
            Err(ConfigError::Json(msg)) => assert!(msg.contains("duplicate field"), "{msg}"),
            other => panic!("{other:?}"),
        }
    }
}

#[test]
fn probe_control_characters_other_than_nul_and_newline_pass_a_template() {
    for text in [
        "a\u{202e}b",
        "a\tb",
        "a\u{1b}[31mb",
        "a\u{2028}b",
        "a\u{85}b",
        "$${tree:a}/x",
    ] {
        assert!(Template::parse(text).is_ok(), "{text:?}");
    }
    assert!(matches!(
        Template::parse("${tree:${tree:a}}"),
        Err(TemplateError::BadTreeName(_))
    ));
}

#[test]
fn probe_job_absolute_literal_paths_with_a_trailing_slash_are_refused() {
    assert_eq!(
        Template::parse("/tmp/out/"),
        Err(TemplateError::EmptyComponent)
    );
    assert_eq!(Template::parse("/"), Err(TemplateError::EmptyComponent));
    assert!(Template::parse("..").is_ok());
}

#[test]
fn probe_env_names() {
    let long = "A".repeat(100_000);
    assert!(check_env_name(&long).is_ok());
    for name in [
        "MALLOC_X",
        "MallocStackLogging",
        "OBJC_DEBUG",
        "__XPC_X",
        "JS",
        "JSX",
    ] {
        assert!(check_env_name(name).is_ok(), "{name}");
    }
    for name in ["JS_X", "JS_AGENT_X", "DYLD_X", "LD_X", "HOME", "dyld_x"] {
        let got = check_env_name(name);
        assert_eq!(got.is_ok(), name == "dyld_x", "{name}");
    }
}

#[test]
fn probe_canonical_bytes_follow_the_order_of_set_like_lists() {
    let one = MINIMAL.replace("\"reads\":[]", "\"reads\":[\"timezone\",\"locale\"]");
    let two = MINIMAL.replace("\"reads\":[]", "\"reads\":[\"locale\",\"timezone\"]");
    let (a, b) = (parse(&one).unwrap(), parse(&two).unwrap());
    assert_ne!(a.to_canonical_json(), b.to_canonical_json());
    let null = MINIMAL.replace("\"built\":", "\"xcode\":null,\"built\":");
    assert_eq!(
        parse(&null).unwrap().to_canonical_json(),
        minimal().to_canonical_json()
    );
}

#[test]
fn probe_colon_in_a_caller_path_splits_path() {
    let text = MINIMAL.replace(
        "\"env\":[]",
        "\"env\":[{\"name\":\"PATH\",\"op\":\"prepend\",\"values\":[\"${developer_dir}/usr/bin\"],\"locked\":true}]",
    );
    let config = parse(&text).unwrap();
    let mut writable = BTreeMap::new();
    writable.insert(TreeName::parse("tool").unwrap(), PathBuf::from("/w/tool"));
    let job_env = BTreeMap::new();
    let out = materialise(
        &config,
        &Inputs {
            store_root: Path::new("/store"),
            writable: &writable,
            developer_dir: Some(Path::new("/x:/evil")),
            base_env: &[],
            job_env: &job_env,
            job_args: &[],
        },
    )
    .unwrap();
    let path = &out.env.iter().find(|e| e.0 == "PATH").unwrap().1;
    assert_eq!(path, "/x:/evil/usr/bin");
}
