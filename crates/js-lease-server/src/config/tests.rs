//! Unit tests of the configuration: the token and its file, the default limits and a pool's
//! prepare timeout.

use std::ffi::OsString;
use std::os::unix::fs::PermissionsExt;

use js_lease::Booking;
use js_testkit::dir::TestDir;

use super::*;
use crate::capacity::Seated;

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

/// Asserts that a token file is trusted only when it is a regular file, owner-only
/// in mode, and owned by the user running the server: each condition alone refuses.
#[test]
fn a_token_file_must_be_the_servers_own_private_file() {
    assert!(trusted_private_file(true, 0o600, 1000, 1000));
    assert!(trusted_private_file(true, 0o400, 1000, 1000));
    assert!(!trusted_private_file(false, 0o600, 1000, 1000));
    assert!(!trusted_private_file(true, 0o640, 1000, 1000));
    assert!(!trusted_private_file(true, 0o600, 0, 1000));
    assert!(!trusted_private_file(true, 0o600, 1000, 0));
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
/// disturbs the ttls.
#[test]
fn a_pools_prepare_timeout_replaces_only_the_prepare_timeout() {
    let secs = Duration::from_secs;
    let flags = Settings::new(secs(60), secs(3600)).unwrap();
    assert_eq!(with_whole_node(flags, None).unwrap(), flags);
    let mut whole = WholeNodeConfig::dedicated(2);
    whole.prepare_timeout_ms = 1_200_000;
    let set = with_whole_node(flags, Some(&whole)).unwrap();
    assert_eq!(set.prepare_timeout(), secs(1200));
    assert_eq!((set.default_ttl(), set.max_ttl()), (secs(60), secs(3600)));
    whole.prepare_timeout_ms = 0;
    let err = with_whole_node(flags, Some(&whole)).err().unwrap();
    assert!(err.to_string().contains("--agent-pool-config"), "{err}");
}

/// A simulated source that builds nothing: asking it for a provider is refused naming
/// `--stub-hosts`, which tells it apart from any other source.
struct Stub;

impl CapacitySource for Stub {
    fn provider(&self, _bookings: &[Booking]) -> Result<Seated, StartupError> {
        Err(StartupError::new("--stub-hosts", "stub"))
    }
}

/// `text` as a TOML string.
fn quoted(text: impl std::fmt::Display) -> String {
    format!("{:?}", text.to_string())
}

/// Asserts that a configuration file read through the flags has no simulated source of
/// its own, and that [`Config::simulated`] over the same file serves the source it is
/// given beside the hosts the file's registering pool takes. It catches a source dropped
/// when the file is loaded, and one that appears with no caller to hand it in.
#[test]
fn a_config_file_is_served_beside_the_simulated_source_it_is_given() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../target/config_unit");
    let dir = TestDir::under(root, "simulated_beside_file");
    let token = dir.path().join("client-token");
    std::fs::write(&token, "x".repeat(40)).unwrap();
    std::fs::set_permissions(&token, std::fs::Permissions::from_mode(0o600)).unwrap();
    let text = format!(
        "[server]\nstate_file = {}\nboot = \"initialise\"\nclient_token_file = {}\nmax_lease_wall = \"2h\"\nlisten = \"127.0.0.1:0\"\nagent_listen = \"127.0.0.2:0\"\n\n[[pool]]\nkind = \"static\"\nname = \"burst\"\nclass = \"whole-node\"\nexec = \"none\"\nmax_machines = 6\nshape = {{ cpu_millis = 8000, memory_bytes = 17179869184 }}\n",
        quoted(dir.path().join("leases.toml").display()),
        quoted(token.display()),
    );
    let file = dir.path().join("js.toml");
    std::fs::write(&file, text).unwrap();
    let cli = || {
        let args: Vec<OsString> = vec![
            "js-lease-server".into(),
            "--config".into(),
            file.clone().into_os_string(),
        ];
        Cli::try_parse_from(args).unwrap()
    };

    let plain = Config::from_cli(cli()).unwrap();
    assert!(matches!(
        plain.hosts,
        Hosts::Layered {
            simulated: None,
            ..
        }
    ));

    let config = Config::simulated(cli(), Box::new(Stub)).unwrap();
    let served = match &config.hosts {
        Hosts::Layered {
            simulated: Some(source),
            ..
        } => source.provider(&[]).err().unwrap().to_string(),
        _ => String::from("not layered with a simulated source"),
    };
    assert_eq!(served, "--stub-hosts: stub");
}
