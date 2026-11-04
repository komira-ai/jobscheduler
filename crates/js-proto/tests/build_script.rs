//! A source check of `build.rs`: it names no way to start a process and no `protoc`. The
//! property it stands for, that the crate builds where no protobuf compiler is installed,
//! is shown by every CI run, whose runners have none.

/// The words a build script that starts a process or a protobuf compiler would hold.
const SPAWN_WORDS: [&str; 3] = ["Command", "std::process", "protoc"];

/// The words of [`SPAWN_WORDS`] that `source` holds, comments included.
fn spawn_words(source: &str) -> Vec<&'static str> {
    SPAWN_WORDS
        .into_iter()
        .filter(|word| source.contains(word))
        .collect()
}

/// Asserts that `build.rs` holds none of the words, and that the check finds each one in
/// a build script that would start `protoc`.
#[test]
fn build_script_spawns_no_process() {
    assert_eq!(spawn_words(include_str!("../build.rs")), Vec::<&str>::new());
    assert_eq!(
        spawn_words("let out = std::process::Command::new(\"protoc\").output();"),
        SPAWN_WORDS
    );
}
