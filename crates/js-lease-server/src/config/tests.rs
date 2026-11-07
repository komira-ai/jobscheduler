//! Unit tests of the configuration: the token and its file, the default limits and a pool's
//! prepare timeout.

use js_registry::WholeNodeConfig;

use super::agent_setup::with_whole_node;
use super::*;

/// Asserts that a token compares equal only to itself and prints without its bytes.
#[test]
fn a_token_matches_itself_and_hides_itself() {
    let token = Token(b"s3cret".to_vec());
    assert!(token.matches(b"s3cret"));
    assert!(!token.matches(b"s3creT"));
    assert!(!token.matches(b"s3cret!"));
    assert!(!token.matches(b""));
    assert_eq!(format!("{token:?}"), "Token(..)");
}

/// Asserts that a token is the content without trailing whitespace, and that empty
/// content and any space, line break, control or non-ASCII byte inside the token
/// is refused: no header value could ever equal such a token.
#[test]
fn a_token_is_one_run_of_printable_ascii() {
    let good = "abcdefghijklmnopqrstuvwxyz-0123456789";
    let trailing = format!("{good}\r\n");
    assert!(
        parse_token(trailing.as_bytes())
            .unwrap()
            .matches(good.as_bytes())
    );
    assert_eq!(parse_token(b" \r\n").err().as_deref(), Some("is empty"));
    for bad in [
        &b"two\nlines\n"[..],
        b"has space",
        b" leading",
        b"tab\tinside",
        b"nul\0byte",
        "caf\u{e9}".as_bytes(),
    ] {
        let reason = parse_token(bad).err().unwrap();
        assert!(reason.contains("printable ASCII"), "{bad:?}");
    }
}

/// Asserts that a token of 31 bytes is refused and one of 32 is accepted, counting
/// the bytes without the trailing whitespace, and that the refusal says how long
/// the token was and how long it must be.
#[test]
fn a_token_must_have_at_least_32_bytes() {
    let short = "x".repeat(MIN_TOKEN_BYTES - 1);
    let reason = parse_token(format!("{short}\n\n").as_bytes())
        .err()
        .unwrap();
    assert!(reason.contains("31 bytes"), "{reason}");
    assert!(reason.contains("at least 32"), "{reason}");
    let enough = "x".repeat(MIN_TOKEN_BYTES);
    assert!(parse_token(enough.as_bytes()).is_ok());
}

/// Asserts that the default limits are what the flags default to, so the two
/// cannot drift apart.
#[test]
fn the_default_limits_are_the_flag_defaults() {
    let cli = Cli::try_parse_from([
        "js-lease-server",
        "--state-file=a",
        "--boot=resume",
        "--pool-config=b",
        "--token-file=c",
    ])
    .unwrap();
    assert_eq!(Limits::from(&cli), Limits::default());
}

/// Asserts how a pool's whole-node settings reach the lease table's settings: none
/// leaves the flags' settings untouched (every figure), a pool's `prepare_timeout`
/// replaces only the prepare timeout, and a zero one is a startup error naming the
/// pool flag. It catches a pool key that is dropped on the way, and one that also
/// disturbs the ceiling on the wall time.
#[test]
fn a_pools_prepare_timeout_replaces_only_the_prepare_timeout() {
    let secs = Duration::from_secs;
    let flags = Settings::new().with_max_wall(secs(3600)).unwrap();
    assert_eq!(with_whole_node(flags, None).unwrap(), flags);
    let mut whole = WholeNodeConfig::dedicated(2);
    whole.prepare_timeout_ms = 1_200_000;
    let set = with_whole_node(flags, Some(&whole)).unwrap();
    assert_eq!(set.prepare_timeout(), secs(1200));
    assert_eq!(set.max_wall(), Some(secs(3600)));
    whole.prepare_timeout_ms = 0;
    let err = with_whole_node(flags, Some(&whole)).err().unwrap();
    assert!(err.to_string().contains("--agent-pool-config"), "{err}");
}

/// The startup error of the command line `args` (the program name left out).
fn refused_flag(args: &[&str]) -> StartupError {
    let args = std::iter::once("js-lease-server").chain(args.iter().copied());
    let cli = Cli::try_parse_from(args).unwrap();
    Config::from_cli(cli).err().unwrap()
}

/// Asserts that `--default-ttl` and `--max-ttl`, which still parse (hidden) so that they
/// can be refused by name, are startup errors with the exact reason, whatever value they
/// are given, and that the refusal comes before every other check: with no other flag at
/// all, and beside `--config`. It catches a flag that is accepted and ignored (an
/// operator would think a ttl is in force), one refused as unknown by the parser, and a
/// refusal that is hidden behind another one.
#[test]
fn a_removed_ttl_flag_is_refused_by_name_before_anything_else() {
    for flag in ["--default-ttl", "--max-ttl"] {
        for value in ["60s", "not-a-duration"] {
            let given = format!("{flag}={value}");
            for args in [
                vec![given.as_str()],
                vec!["--config=x.toml", given.as_str()],
            ] {
                let err = refused_flag(&args);
                assert_eq!(err.flag(), flag);
                assert_eq!(
                    err.to_string(),
                    format!(
                        "{flag}: was removed: a lease has no time to live (it ends when \
                         released or at its maximum wall time); remove the flag"
                    )
                );
            }
        }
    }
}

/// Asserts that the removed ttl flags are hidden options of the command line, with no
/// default, so that `--help` does not offer them. It catches a flag left visible.
#[test]
fn the_removed_ttl_flags_are_hidden_and_have_no_default() {
    let command = <Cli as clap::CommandFactory>::command();
    for name in ["default_ttl", "max_ttl"] {
        let arg = command.get_arguments().find(|arg| arg.get_id() == name);
        let arg = arg.unwrap();
        assert!(arg.is_hide_set(), "{name}");
        assert!(arg.get_default_values().is_empty(), "{name}");
    }
}
