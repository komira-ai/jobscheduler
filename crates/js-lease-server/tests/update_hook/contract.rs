//! The contract of `js-lease-server update-hook` with the updater: the command line, the params
//! file, the declaration and its bounds, the record's rules, the reason tokens, the verb's
//! `run` and the exit codes with their last lines.

use std::ffi::OsString;
use std::fs;
use std::net::SocketAddr;
use std::os::unix::ffi::OsStringExt;
use std::path::PathBuf;

use js_lease_server::update_hook::{
    ArgsError, DEFAULT_UNIT, Declaration, DeclarationError, Flags, Invocation, MIN_TIMEOUTS,
    Params, ParamsError, Step, Timeouts, healthz_address,
};
use js_testkit::dir::TestDir;

fn os(words: &[&str]) -> Vec<OsString> {
    words.iter().map(OsString::from).collect()
}

fn parse(words: &[&str]) -> Result<Invocation, ArgsError> {
    Invocation::parse(os(words))
}

fn refusal(words: &[&str]) -> ArgsError {
    parse(words).expect_err("a refused command line")
}

// ---------------------------------------------------------------------------------------
// args.rs

/// Asserts each refusal of the command line is its variant and its exact words: nothing after
/// the verb, an unknown step, `prepare` without its directory, an extra argument, `--params`
/// as the last word, twice, or absent.
///
/// Catches a refusal reworded or a missing one reported as another.
#[test]
fn the_command_line_refusals_say_exactly_what_is_wrong() {
    let cases: Vec<(Vec<&str>, ArgsError, &str)> = vec![
        (vec![], ArgsError::MissingStep, "no step given"),
        (
            vec!["frob"],
            ArgsError::UnknownStep("frob".to_owned()),
            "unknown step \"frob\"",
        ),
        (
            vec!["prepare", "--params", "p"],
            ArgsError::MissingArtifact,
            "prepare needs the staged release directory",
        ),
        (
            vec!["cutover", "extra", "--params", "p"],
            ArgsError::Unexpected(OsString::from("extra")),
            "unexpected argument \"extra\"",
        ),
        (
            vec!["prepare", "/a", "/b", "--params", "p"],
            ArgsError::Unexpected(OsString::from("/b")),
            "unexpected argument \"/b\"",
        ),
        (
            vec!["cleanup", "--params"],
            ArgsError::MissingParamsFile,
            "--params needs a file",
        ),
        (
            vec!["cleanup", "--params", "a", "--params", "b"],
            ArgsError::DuplicateParams,
            "--params given twice",
        ),
        (
            vec!["cleanup"],
            ArgsError::MissingParams,
            "--params <file> is required",
        ),
        (
            vec!["--bogus=1", "cleanup", "--params", "p"],
            ArgsError::UnknownFlag("--bogus=1".to_owned()),
            "unknown flag \"--bogus=1\" before the step",
        ),
        (
            vec!["--nope", "cleanup"],
            ArgsError::UnknownFlag("--nope".to_owned()),
            "unknown flag \"--nope\" before the step",
        ),
    ];
    for (words, error, text) in cases {
        let got = refusal(&words);
        assert_eq!(got, error, "{words:?}");
        assert_eq!(got.to_string(), text, "{words:?}");
    }
}

/// Asserts that a flag with no `=value` or an empty value is `FlagValue`, one given twice is
/// `DuplicateFlag`, a path flag that is relative is `RelativeFlag`, with their exact words.
#[test]
fn a_flag_without_value_twice_or_relative_is_refused() {
    let no_value = refusal(&["--unit=", "cleanup", "--params", "p"]);
    assert_eq!(no_value, ArgsError::FlagValue("--unit"));
    assert_eq!(
        no_value.to_string(),
        "--unit needs a value, written --unit=<value>"
    );
    // A flag word with no `=` at all.
    let bare = refusal(&["--store", "cleanup", "--params", "p"]);
    assert_eq!(bare, ArgsError::FlagValue("--store"));
    let twice = refusal(&["--unit=a", "--unit=b", "cleanup", "--params", "p"]);
    assert_eq!(twice, ArgsError::DuplicateFlag("--unit"));
    assert_eq!(twice.to_string(), "--unit given twice");
    for name in ["--release-link", "--store"] {
        let flag = format!("{name}=relative/path");
        let relative = refusal(&[&flag, "cleanup", "--params", "p"]);
        assert_eq!(
            relative.to_string(),
            format!("{name} must be an absolute path")
        );
    }
    let dup_link = refusal(&[
        "--release-link=/a",
        "--release-link=/b",
        "cleanup",
        "--params",
        "p",
    ]);
    assert_eq!(dup_link, ArgsError::DuplicateFlag("--release-link"));
    let dup_store = refusal(&["--store=/a", "--store=/b", "cleanup", "--params", "p"]);
    assert_eq!(dup_store, ArgsError::DuplicateFlag("--store"));
    let dup_health = refusal(&[
        "--healthz-url=http://127.0.0.1:1",
        "--healthz-url=http://127.0.0.1:2",
        "cleanup",
        "--params",
        "p",
    ]);
    assert_eq!(dup_health, ArgsError::DuplicateFlag("--healthz-url"));
}

/// Asserts `--unit` is refused with `BadUnit` and its exact words when it holds a `/` or
/// starts with `-`, and accepted otherwise.
///
/// Catches a unit name that `systemctl` would read as an option or a path.
#[test]
fn a_unit_with_a_slash_or_a_leading_dash_is_refused() {
    for bad in ["a/b", "/etc/x.service", "-x.service", "--now"] {
        let flag = format!("--unit={bad}");
        // A value starting with `--` is still a value after `=`.
        let got = refusal(&[&flag, "cleanup", "--params", "p"]);
        assert_eq!(got, ArgsError::BadUnit(bad.to_owned()), "{bad}");
        assert_eq!(
            got.to_string(),
            format!("--unit {bad:?} is not a unit name")
        );
    }
    let ok = parse(&["--unit=other.service", "cleanup", "--params", "p"]).unwrap();
    assert_eq!(ok.flags.unit(), "other.service");
}

/// Asserts `Flags::unit` is the default unit with no flag and the flag's value with one.
#[test]
fn the_unit_defaults_when_the_flag_is_absent() {
    assert_eq!(Flags::default().unit(), DEFAULT_UNIT);
    assert_eq!(DEFAULT_UNIT, "js-lease-server.service");
    let given = Flags {
        unit: Some("x.service".to_owned()),
        ..Flags::default()
    };
    assert_eq!(given.unit(), "x.service");
}

/// Asserts `healthz_address` accepts `http://127.0.0.1:N` and `http://[::1]:N` and refuses
/// https, a non-loopback address, port 0, a hostname, a path suffix and the empty string, each
/// as `BadHealthzUrl` with the exact words.
///
/// Catches a health check that could be pointed off the machine.
#[test]
fn the_healthz_address_is_plain_http_to_the_loopback_only() {
    assert_eq!(
        healthz_address("http://127.0.0.1:7878"),
        Ok("127.0.0.1:7878".parse::<SocketAddr>().unwrap())
    );
    assert_eq!(
        healthz_address("http://[::1]:9"),
        Ok("[::1]:9".parse::<SocketAddr>().unwrap())
    );
    for bad in [
        "https://127.0.0.1:7878",
        "http://192.0.2.1:7878",
        "http://[2001:db8::1]:7878",
        "http://127.0.0.1:0",
        "http://localhost:7878",
        "http://127.0.0.1:7878/",
        "http://127.0.0.1:7878/healthz",
        "http://127.0.0.1",
        "127.0.0.1:7878",
        "",
    ] {
        let got = healthz_address(bad).expect_err(bad);
        assert_eq!(got, ArgsError::BadHealthzUrl(bad.to_owned()));
        assert_eq!(
            got.to_string(),
            format!(
                "--healthz-url {bad:?} must be http:// and a loopback address and port, such as http://127.0.0.1:7878"
            )
        );
    }
    let through_flag = refusal(&[
        "--healthz-url=https://127.0.0.1:1",
        "verify",
        "--params",
        "p",
    ]);
    assert_eq!(
        through_flag,
        ArgsError::BadHealthzUrl("https://127.0.0.1:1".to_owned())
    );
}

/// Asserts a full command line parses to its flags, step, artifact and params file, for
/// `prepare` with an artifact on either side of `--params`, and for the other steps with none.
#[test]
fn a_good_command_line_parses_into_flags_step_artifact_and_params() {
    let full = parse(&[
        "--release-link=/opt/link",
        "--unit=u.service",
        "--store=/etc/store=x",
        "--healthz-url=http://[::1]:80",
        "prepare",
        "/opt/releases/2.0.0",
        "--params",
        "/run/p.toml",
    ])
    .unwrap();
    assert_eq!(
        full,
        Invocation {
            flags: Flags {
                release_link: Some(PathBuf::from("/opt/link")),
                unit: Some("u.service".to_owned()),
                store: Some(PathBuf::from("/etc/store=x")),
                healthz: Some("[::1]:80".parse().unwrap()),
            },
            step: Step::Prepare,
            artifact: Some(PathBuf::from("/opt/releases/2.0.0")),
            params: PathBuf::from("/run/p.toml"),
        }
    );
    let params_first = parse(&["prepare", "--params", "p", "/art"]).unwrap();
    assert_eq!(params_first.artifact, Some(PathBuf::from("/art")));
    assert_eq!(params_first.flags, Flags::default());
    for step in [Step::Cutover, Step::Verify, Step::Rollback, Step::Cleanup] {
        let got = parse(&[step.name(), "--params", "p"]).unwrap();
        assert_eq!(got.step, step);
        assert_eq!(got.artifact, None);
    }
}

/// Asserts a flag word that is not UTF-8 is refused as `UnknownFlag` with the word shown
/// lossily (never taken as a changed value), a non-UTF-8 step word is an unknown step, and a
/// non-UTF-8 artifact and params path are kept byte for byte.
///
/// Catches a value that is silently rewritten to fit text.
#[test]
fn a_flag_word_that_is_not_utf8_is_refused_and_paths_keep_their_bytes() {
    let mut words = os(&["--params"]);
    words.clear();
    words.push(OsString::from_vec(b"--unit=\xff.service".to_vec()));
    words.extend(os(&["cleanup", "--params", "p"]));
    let got = Invocation::parse(words).expect_err("refused");
    assert_eq!(
        got,
        ArgsError::UnknownFlag("--unit=\u{fffd}.service".to_owned())
    );
    assert_eq!(
        got.to_string(),
        "unknown flag \"--unit=\u{fffd}.service\" before the step"
    );

    let bad_step = Invocation::parse(vec![OsString::from_vec(b"cl\xffan".to_vec())]);
    assert_eq!(
        bad_step,
        Err(ArgsError::UnknownStep("cl\u{fffd}an".to_owned()))
    );

    let art = OsString::from_vec(b"/r/\xfe".to_vec());
    let par = OsString::from_vec(b"/p/\xfd".to_vec());
    let kept = Invocation::parse(vec![
        OsString::from("prepare"),
        art.clone(),
        OsString::from("--params"),
        par.clone(),
    ])
    .unwrap();
    assert_eq!(kept.artifact, Some(PathBuf::from(art)));
    assert_eq!(kept.params, PathBuf::from(par));

    let extra = OsString::from_vec(b"\xff".to_vec());
    let unexpected = Invocation::parse(vec![
        OsString::from("cleanup"),
        extra.clone(),
        OsString::from("--params"),
        OsString::from("p"),
    ]);
    assert_eq!(unexpected, Err(ArgsError::Unexpected(extra)));
}

// ---------------------------------------------------------------------------------------
// params.rs

const FULL_PARAMS: &str = r#"
"updater.staged_release" = "/opt/releases/2.0.0"
"updater.previous_release" = "/opt/releases/1.0.0"
"updater.target_version" = "2.0.0"
"updater.previous_version" = "1.0.0"
defer_retry_secs = 60
"updater.something_new" = 5
unrelated = "x"
"#;

fn params_err(text: &str) -> ParamsError {
    Params::parse(text).expect_err("a refused params table")
}

/// Asserts a full table reads into every field with the flags' fields at their defaults, a
/// first-install table has no previous release or version, zero seconds is allowed and keys the
/// hook does not read are ignored.
#[test]
fn a_params_table_reads_into_its_fields() {
    let got = Params::parse(FULL_PARAMS).unwrap();
    assert_eq!(
        got,
        Params {
            staged_release: PathBuf::from("/opt/releases/2.0.0"),
            previous_release: Some(PathBuf::from("/opt/releases/1.0.0")),
            target_version: "2.0.0".to_owned(),
            previous_version: Some("1.0.0".to_owned()),
            defer_retry_secs: 60,
            release_link: None,
            unit: DEFAULT_UNIT.to_owned(),
        }
    );
    let first = Params::parse(
        "\"updater.staged_release\" = \"/r/2\"\n\"updater.target_version\" = \"2\"\ndefer_retry_secs = 0\n",
    )
    .unwrap();
    assert_eq!(first.previous_release, None);
    assert_eq!(first.previous_version, None);
    assert_eq!(first.defer_retry_secs, 0);
}

/// Asserts every refusal of a params table is its variant with its exact words: not TOML, a
/// missing key (the retry seconds first, then the staged release, then the target version), a
/// key of the wrong type, a relative directory, and seconds that are negative, not whole or not
/// a number.
#[test]
fn every_params_refusal_names_its_key() {
    let syntax = params_err("this is = = not toml");
    assert!(matches!(syntax, ParamsError::Syntax(_)), "{syntax:?}");
    assert!(
        syntax
            .to_string()
            .starts_with("the params file is not a TOML table: "),
        "{syntax}"
    );

    let missing = params_err("");
    assert!(matches!(missing, ParamsError::Missing("defer_retry_secs")));
    assert_eq!(
        missing.to_string(),
        "params key \"defer_retry_secs\" is required"
    );

    let staged = params_err("defer_retry_secs = 1\n\"updater.target_version\" = \"2\"\n");
    assert_eq!(
        staged.to_string(),
        "params key \"updater.staged_release\" is required"
    );
    let target = params_err("defer_retry_secs = 1\n\"updater.staged_release\" = \"/r\"\n");
    assert_eq!(
        target.to_string(),
        "params key \"updater.target_version\" is required"
    );

    let base = "\"updater.staged_release\" = \"/r\"\n\"updater.target_version\" = \"2\"\n";
    for (extra, text) in [
        (
            "defer_retry_secs = -1\n",
            "params key \"defer_retry_secs\" must be a whole number of seconds, not negative",
        ),
        (
            "defer_retry_secs = 1.5\n",
            "params key \"defer_retry_secs\" must be a whole number of seconds, not negative",
        ),
        (
            "defer_retry_secs = \"60\"\n",
            "params key \"defer_retry_secs\" must be a whole number of seconds, not negative",
        ),
        (
            "defer_retry_secs = 1\n\"updater.previous_version\" = 1\n",
            "params key \"updater.previous_version\" must be a string",
        ),
        (
            "defer_retry_secs = 1\n\"updater.previous_release\" = \"rel/1\"\n",
            "params key \"updater.previous_release\" must be an absolute path",
        ),
        (
            "defer_retry_secs = 1\n\"updater.previous_release\" = 7\n",
            "params key \"updater.previous_release\" must be a string",
        ),
    ] {
        let got = params_err(&format!("{base}{extra}"));
        assert_eq!(got.to_string(), text, "{extra}");
    }
    let not_text_staged = params_err(
        "defer_retry_secs = 1\n\"updater.staged_release\" = 5\n\"updater.target_version\" = \"2\"\n",
    );
    assert!(matches!(
        not_text_staged,
        ParamsError::NotText("updater.staged_release")
    ));
    let relative_staged = params_err(
        "defer_retry_secs = 1\n\"updater.staged_release\" = \"r\"\n\"updater.target_version\" = \"2\"\n",
    );
    assert!(matches!(
        relative_staged,
        ParamsError::Relative("updater.staged_release")
    ));
    let not_text_target = params_err(
        "defer_retry_secs = 1\n\"updater.staged_release\" = \"/r\"\n\"updater.target_version\" = 2\n",
    );
    assert_eq!(
        not_text_target.to_string(),
        "params key \"updater.target_version\" must be a string"
    );
}

/// Asserts `Params::read` reads a file from disk, and a missing file is `Read` with the
/// operating system's words after the fixed prefix.
#[test]
fn a_params_file_is_read_from_disk_and_a_missing_one_is_refused() {
    let dir = TestDir::under(env!("CARGO_TARGET_TMPDIR"), "contract-params-read");
    let file = dir.path().join("params.toml");
    fs::write(&file, FULL_PARAMS).unwrap();
    assert_eq!(
        Params::read(&file).unwrap(),
        Params::parse(FULL_PARAMS).unwrap()
    );

    let missing = dir.path().join("absent.toml");
    let os_words = fs::read_to_string(&missing).unwrap_err().to_string();
    let got = Params::read(&missing).expect_err("no file");
    assert!(matches!(got, ParamsError::Read(_)));
    assert_eq!(
        got.to_string(),
        format!("cannot read the params file: {os_words}")
    );
}

/// Asserts `with_flags` puts the release link and the unit in, and with no flags leaves no link
/// and the default unit (not a unit left from before).
#[test]
fn the_flags_are_put_into_the_params() {
    let base = Params::parse(FULL_PARAMS).unwrap();
    let flags = Flags {
        release_link: Some(PathBuf::from("/opt/link")),
        unit: Some("u.service".to_owned()),
        ..Flags::default()
    };
    let with = base.clone().with_flags(&flags);
    assert_eq!(with.release_link, Some(PathBuf::from("/opt/link")));
    assert_eq!(with.unit, "u.service");
    assert_eq!(with.target_version, "2.0.0");
    let without = with.with_flags(&Flags::default());
    assert_eq!(without.release_link, None);
    assert_eq!(without.unit, DEFAULT_UNIT);
}

// ---------------------------------------------------------------------------------------
// declaration.rs

struct Spec {
    state_dir: &'static str,
    paths: Vec<&'static str>,
    units: Vec<&'static str>,
    timeouts: [u64; 5],
    deadline: Option<u64>,
}

fn min_array() -> [u64; 5] {
    let t = MIN_TIMEOUTS;
    [t.prepare, t.cutover, t.verify, t.rollback, t.cleanup]
}

impl Spec {
    fn good() -> Spec {
        Spec {
            state_dir: "/srv/state",
            paths: vec!["/srv/releases", "/srv/link", "/srv/state"],
            units: vec!["js-lease-server.service"],
            timeouts: min_array(),
            deadline: None,
        }
    }

    fn text(&self) -> String {
        let list = |items: &[&str]| {
            items
                .iter()
                .map(|s| format!("{s:?}"))
                .collect::<Vec<_>>()
                .join(", ")
        };
        let [p, c, v, r, k] = self.timeouts;
        let deadline = self
            .deadline
            .map(|d| format!("verify_deadline_secs = {d}\n"))
            .unwrap_or_default();
        format!(
            "state_dir = {:?}\n{deadline}extra_key = true\n[hook]\nunits = [{}]\nwrite_paths = [{}]\nnew_key = 1\n[hook.timeout_secs]\nprepare = {p}\ncutover = {c}\nverify = {v}\nrollback = {r}\ncleanup = {k}\n",
            self.state_dir,
            list(&self.units),
            list(&self.paths),
        )
    }

    fn parse(&self) -> Result<Declaration, DeclarationError> {
        Declaration::parse(&self.text())
    }

    fn refused(&self) -> DeclarationError {
        self.parse().expect_err("a refused declaration")
    }
}

/// Asserts a good declaration reads into its fields, keys it does not read are ignored, and
/// `Timeouts::by_step` lists the five in the contract's order with their names.
#[test]
fn a_good_declaration_reads_into_its_fields() {
    let got = Spec::good().parse().unwrap();
    assert_eq!(got.state_dir, PathBuf::from("/srv/state"));
    assert_eq!(got.unit, "js-lease-server.service");
    assert_eq!(
        got.write_paths,
        [
            PathBuf::from("/srv/releases"),
            PathBuf::from("/srv/link"),
            PathBuf::from("/srv/state")
        ]
    );
    assert_eq!(got.timeouts, MIN_TIMEOUTS);
    let t = Timeouts {
        prepare: 1,
        cutover: 2,
        verify: 3,
        rollback: 4,
        cleanup: 5,
    };
    assert_eq!(
        t.by_step(),
        [
            ("prepare", 1),
            ("cutover", 2),
            ("verify", 3),
            ("rollback", 4),
            ("cleanup", 5)
        ]
    );
}

/// Asserts a declaration file is read from disk, a missing one is `Read` with the operating
/// system's words, and text that is not TOML or lacks a key is `Parse`.
#[test]
fn a_declaration_file_is_read_and_a_missing_or_malformed_one_is_refused() {
    let dir = TestDir::under(env!("CARGO_TARGET_TMPDIR"), "contract-decl-read");
    let file = dir.path().join("decl.toml");
    fs::write(&file, Spec::good().text()).unwrap();
    assert_eq!(
        Declaration::read(&file).unwrap(),
        Spec::good().parse().unwrap()
    );

    let missing = dir.path().join("absent.toml");
    let os_words = fs::read_to_string(&missing).unwrap_err().to_string();
    let got = Declaration::read(&missing).expect_err("no file");
    assert!(matches!(got, DeclarationError::Read(_)));
    assert_eq!(
        got.to_string(),
        format!("cannot read the declaration: {os_words}")
    );

    for text in ["not = = toml", "state_dir = \"/a\"\n"] {
        let got = Declaration::parse(text).expect_err("unusable");
        assert!(matches!(got, DeclarationError::Parse(_)), "{got:?}");
        assert!(
            got.to_string()
                .starts_with("the declaration is not usable: "),
            "{got}"
        );
    }
}

/// Asserts a state directory or write path that is relative, has `..`, `.`, a doubled or a
/// trailing slash is `NotPlain` with its exact words.
#[test]
fn a_path_that_is_not_plain_is_refused() {
    let words = "state_dir and every write path must be absolute paths of plain components";
    for bad in [
        "rel/state",
        "/srv/../state",
        "/srv/./state",
        "/srv//state",
        "/srv/state/",
    ] {
        let mut spec = Spec::good();
        spec.state_dir = bad;
        spec.paths = vec![bad, "/srv/link"];
        let got = spec.refused();
        assert!(matches!(got, DeclarationError::NotPlain), "{bad}: {got:?}");
        assert_eq!(got.to_string(), words);

        let mut spec = Spec::good();
        spec.paths = vec!["/srv/state", bad];
        assert!(
            matches!(spec.refused(), DeclarationError::NotPlain),
            "write path {bad}"
        );
    }
}

/// Asserts a state directory missing from the write paths is `StateDirNotListed`.
#[test]
fn a_state_dir_that_is_not_a_write_path_is_refused() {
    let mut spec = Spec::good();
    spec.paths = vec!["/srv/releases", "/srv/link"];
    let got = spec.refused();
    assert!(matches!(got, DeclarationError::StateDirNotListed));
    assert_eq!(
        got.to_string(),
        "state_dir is not listed in hook.write_paths"
    );
}

/// Asserts a write path inside another, or listed twice, is `Nested` naming both, and that a
/// sibling that merely shares a name prefix is accepted.
///
/// Catches a release directory that could reach the record or the link.
#[test]
fn a_write_path_inside_another_or_twice_is_refused() {
    let mut spec = Spec::good();
    spec.paths = vec![
        "/srv/state",
        "/srv/releases",
        "/srv/releases/inner",
        "/srv/link",
    ];
    let got = spec.refused();
    match &got {
        DeclarationError::Nested { inner, outer } => {
            assert_eq!(inner, "/srv/releases/inner");
            assert_eq!(outer, "/srv/releases");
        }
        other => panic!("{other:?}"),
    }
    assert_eq!(
        got.to_string(),
        "hook.write_paths /srv/releases/inner lies inside /srv/releases"
    );

    let mut twice = Spec::good();
    twice.paths = vec!["/srv/state", "/srv/state", "/srv/link"];
    assert_eq!(
        twice.refused().to_string(),
        "hook.write_paths /srv/state lies inside /srv/state"
    );

    let mut sibling = Spec::good();
    sibling.paths = vec!["/srv/state", "/srv/releases", "/srv/releases-old"];
    assert!(sibling.parse().is_ok());
}

/// Asserts `hook.units` with none or with two units is `Units(n)` with its exact words, and
/// with one is accepted.
#[test]
fn the_declaration_must_name_exactly_one_unit() {
    for (units, n) in [(vec![], 0), (vec!["a.service", "b.service"], 2)] {
        let mut spec = Spec::good();
        spec.units = units;
        let got = spec.refused();
        assert!(
            matches!(got, DeclarationError::Units(m) if m == n),
            "{got:?}"
        );
        assert_eq!(
            got.to_string(),
            format!("hook.units must name exactly one unit, not {n}")
        );
    }
}

/// Asserts MIN_TIMEOUTS is the documented sum of each step's constants, as numbers.
///
/// Catches a wait that grew without the least timeout growing with it.
#[test]
fn the_minimum_timeouts_are_the_sums_of_the_waits() {
    use js_lease_server::update_hook::{
        CLEANUP_SECS, LOCK_WAIT_SECS, PREPARE_WORK_SECS, SERVE_WAIT_SECS, STOP_WAIT_SECS,
        VERIFY_WAIT_SECS,
    };
    use js_update_hook::{
        CUTOVER_MARGIN_SECS, PREPARE_MARGIN_SECS, ROLLBACK_MARGIN_SECS, START_WAIT_SECS,
        VERIFY_MARGIN_SECS,
    };
    assert_eq!(
        MIN_TIMEOUTS.prepare,
        PREPARE_WORK_SECS + PREPARE_MARGIN_SECS
    );
    assert_eq!(
        MIN_TIMEOUTS.cutover,
        STOP_WAIT_SECS + LOCK_WAIT_SECS + START_WAIT_SECS + CUTOVER_MARGIN_SECS
    );
    assert_eq!(MIN_TIMEOUTS.verify, VERIFY_WAIT_SECS + VERIFY_MARGIN_SECS);
    assert_eq!(
        MIN_TIMEOUTS.rollback,
        STOP_WAIT_SECS + LOCK_WAIT_SECS + START_WAIT_SECS + SERVE_WAIT_SECS + ROLLBACK_MARGIN_SECS
    );
    assert_eq!(MIN_TIMEOUTS.cleanup, CLEANUP_SECS);
    assert_eq!(
        (
            STOP_WAIT_SECS,
            LOCK_WAIT_SECS,
            VERIFY_WAIT_SECS,
            SERVE_WAIT_SECS,
            PREPARE_WORK_SECS,
            CLEANUP_SECS
        ),
        (90, 30, 285, 120, 30, 60)
    );
    assert_eq!(min_array(), [90, 300, 300, 420, 60]);
}

/// Asserts for each of the five steps a timeout one second under its minimum is `Timeout`
/// naming the step, the declared and the minimum, and one at the minimum is accepted; with two
/// steps short, the first in the contract's order is named.
#[test]
fn a_step_timeout_under_its_minimum_is_refused() {
    let names = ["prepare", "cutover", "verify", "rollback", "cleanup"];
    for (i, name) in names.iter().enumerate() {
        let min = min_array()[i];
        let mut spec = Spec::good();
        spec.timeouts[i] = min - 1;
        let got = spec.refused();
        match &got {
            DeclarationError::Timeout {
                step,
                declared,
                min: least,
            } => assert_eq!((*step, *declared, *least), (*name, min - 1, min)),
            other => panic!("{other:?}"),
        }
        assert_eq!(
            got.to_string(),
            format!(
                "the declared {name} timeout {} s is shorter than the hook's {min} s",
                min - 1
            )
        );
        spec.timeouts[i] = min;
        assert!(spec.parse().is_ok(), "{name} at its minimum");
    }
    let mut two = Spec::good();
    two.timeouts = [1, 1, min_array()[2], min_array()[3], 1];
    assert!(matches!(
        two.refused(),
        DeclarationError::Timeout {
            step: "prepare",
            ..
        }
    ));
}

/// Asserts `verify_deadline_secs` under the declared verify plus rollback timeouts is
/// `VerifyDeadline` with its exact words, equal to them is accepted, and absent is accepted;
/// the sum is of the declared values, not of the minimums.
///
/// Catches an updater deadline that would roll back a verify still allowed to run.
#[test]
fn the_verify_deadline_must_cover_verify_and_rollback() {
    let sum = min_array()[2] + min_array()[3];
    let mut spec = Spec::good();
    spec.deadline = Some(sum - 1);
    let got = spec.refused();
    match &got {
        DeclarationError::VerifyDeadline {
            deadline,
            verify,
            rollback,
        } => assert_eq!(
            (*deadline, *verify, *rollback),
            (sum - 1, min_array()[2], min_array()[3])
        ),
        other => panic!("{other:?}"),
    }
    assert_eq!(
        got.to_string(),
        format!(
            "verify_deadline_secs {} is less than the verify bound {} plus the rollback bound {}",
            sum - 1,
            min_array()[2],
            min_array()[3]
        )
    );
    spec.deadline = Some(sum);
    assert!(spec.parse().is_ok());
    spec.deadline = None;
    assert!(spec.parse().is_ok());

    spec.timeouts[2] += 100;
    spec.deadline = Some(sum);
    assert!(matches!(
        spec.refused(),
        DeclarationError::VerifyDeadline { .. }
    ));
}
