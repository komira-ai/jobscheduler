//! The command line of js-agent's start mode: what `Profile::start_argv` writes is what
//! `StartArgs::parse` reads back, and every way an argument list can be wrong has its own
//! error with its own message.

use std::ffi::{OsStr, OsString};
use std::os::unix::ffi::{OsStrExt as _, OsStringExt as _};

use js_mac_pool::seatbelt::{
    AbsPath, Caps, InitError, MountGrant, MountTrees, Net, Policy, Ports, Profile, ReadModel,
    START_VERB, SlotLayout, StartArgs, StartError,
};

fn path(text: &str) -> AbsPath {
    AbsPath::parse(text).expect("a good path")
}

fn os(text: &str) -> OsString {
    OsString::from(text)
}

fn all(list: &[&str]) -> Vec<OsString> {
    list.iter().map(|text| os(text)).collect()
}

fn profile_of(run: &str, mounts: Vec<MountGrant>, trees: Option<MountTrees>, net: Net) -> Profile {
    let layout = SlotLayout {
        run: path(run),
        uid_entry: None,
        inputs: vec![],
        read_denies: vec![],
        mounts,
        trees,
    };
    let caps = Caps {
        net,
        read_model: ReadModel::Open,
    };
    Profile::for_policy(&Policy::base(layout, caps).expect("a policy")).expect("a profile")
}

fn job_all() -> Profile {
    let root = path("/Volumes/store/fs");
    let trees = MountTrees::new(
        root,
        vec![
            path("/Volumes/store/fs/mount-provider/aa/r1/0"),
            path("/Volumes/store/fs/mount-provider/aa/r1/1"),
        ],
    )
    .expect("trees");
    let put = MountGrant {
        path: path("/Volumes/store/put/cas/b/build1/0011223344556677"),
        write: true,
        no_exec: false,
    };
    profile_of(
        "/private/var/js-mac-job/jobs/j1",
        vec![put],
        Some(trees),
        Net::Remote(Ports::new(&[443, 8980]).expect("ports")),
    )
}

/// What `parse` reads back from the part of a start vector after the verb.
fn read(argv: &[OsString]) -> StartArgs {
    assert_eq!(argv[1], os(START_VERB));
    StartArgs::parse(&argv[2..]).expect("a start line")
}

/// A start vector of a job with trees, a put directory and ports reads back as the same
/// profile text and parameters, program and arguments.
///
/// Catches a writer and a reader that drift apart: a parameter dropped, reordered or cut
/// at the wrong `=`, or arguments after `--` lost.
#[test]
fn a_start_vector_of_a_full_job_profile_reads_back_as_it_was_written() {
    let profile = job_all();
    let args = all(&["-c", "echo hi", "--profile", "--"]);
    let argv = profile.start_argv(&path("/opt/js-agent"), "/bin/sh", &args);
    let parsed = read(&argv);
    assert_eq!(parsed.init(), &profile.sandbox_init().expect("no NUL"));
    assert_eq!(parsed.program(), OsStr::new("/bin/sh"));
    assert_eq!(parsed.args(), args.as_slice());
}

/// Values with `=`, spaces and quotes in a path, and an argument that is not UTF-8 after
/// `--`, survive the round trip.
///
/// Catches a reader that splits a value at a later `=`, splits on spaces, or insists that
/// the program's arguments are text.
#[test]
fn hostile_values_survive_the_round_trip() {
    let profile = profile_of(
        "/private/var/jobs/a=b c \"d\" 'e' (f)",
        vec![],
        None,
        Net::Off,
    );
    let args = vec![
        OsString::from_vec(vec![0x66, 0xff, 0xfe, 0x67]),
        os("--param"),
        os("A=B"),
        os(""),
        os("x=y z"),
    ];
    let argv = profile.start_argv(&path("/opt/js-agent"), "/bin/echo", &args);
    let parsed = read(&argv);
    assert_eq!(parsed.init(), &profile.sandbox_init().expect("no NUL"));
    let flat: Vec<&str> = parsed
        .init()
        .params()
        .iter()
        .map(|c| c.to_str().expect("text"))
        .collect();
    assert_eq!(flat, ["RUN", "/private/var/jobs/a=b c \"d\" 'e' (f)"]);
    assert_eq!(parsed.program(), OsStr::new("/bin/echo"));
    assert_eq!(parsed.args(), args.as_slice());
    assert_eq!(parsed.args()[0].as_bytes(), [0x66, 0xff, 0xfe, 0x67]);
}

/// A parameter is split at its first `=`: the value keeps the rest, and options may come in
/// either order, with no arguments after the program.
///
/// Catches a split at the last `=` and a reader that needs `--profile` first.
#[test]
fn a_parameter_splits_at_its_first_equals_and_options_come_in_any_order() {
    let parsed = StartArgs::parse(&all(&[
        "--param",
        "NAME=a=b=",
        "--profile",
        "(version 1)",
        "--param",
        "EMPTY=",
        "--",
        "/bin/true",
    ]))
    .expect("parsed");
    let flat: Vec<&str> = parsed
        .init()
        .params()
        .iter()
        .map(|c| c.to_str().expect("text"))
        .collect();
    assert_eq!(flat, ["NAME", "a=b=", "EMPTY", ""]);
    assert_eq!(parsed.init().text().to_bytes(), b"(version 1)");
    assert_eq!(parsed.program(), OsStr::new("/bin/true"));
    assert!(parsed.args().is_empty());
}

/// Each way the arguments are wrong gives its own error and its own words, by exact
/// equality.
///
/// Catches an error that is the wrong variant, or a message that no longer says what is
/// wrong.
#[test]
fn every_error_has_its_own_variant_and_message() {
    let non_text = OsString::from_vec(vec![0xff]);
    let cases: Vec<(Vec<OsString>, StartError, &str)> = vec![
        (
            all(&["--", "/bin/true"]),
            StartError::NoProfile,
            "no --profile was given",
        ),
        (
            all(&["--profile", "a", "--profile", "b", "--", "/bin/true"]),
            StartError::TwoProfiles,
            "--profile was given twice",
        ),
        (
            all(&["--profile"]),
            StartError::MissingValue("--profile".to_owned()),
            "--profile has no value",
        ),
        (
            all(&["--profile", "a", "--param"]),
            StartError::MissingValue("--param".to_owned()),
            "--param has no value",
        ),
        (
            all(&["--profile", "a", "--param", "=x", "--", "/bin/true"]),
            StartError::ParamWithoutName("=x".to_owned()),
            "--param =x is not NAME=VALUE with a name",
        ),
        (
            all(&["--profile", "a", "--param", "name", "--", "/bin/true"]),
            StartError::ParamWithoutName("name".to_owned()),
            "--param name is not NAME=VALUE with a name",
        ),
        (
            vec![os("--profile"), non_text.clone()],
            StartError::NotText("--profile".to_owned()),
            "the value of --profile is not UTF-8 text",
        ),
        (
            vec![os("--profile"), os("a"), os("--param"), non_text.clone()],
            StartError::NotText("--param".to_owned()),
            "the value of --param is not UTF-8 text",
        ),
        (
            all(&["--profile", "a", "--sandbox", "--", "/bin/true"]),
            StartError::UnknownOption("--sandbox".to_owned()),
            "unknown option --sandbox",
        ),
        (
            all(&["/bin/true"]),
            StartError::UnknownOption("/bin/true".to_owned()),
            "unknown option /bin/true",
        ),
        (
            all(&["--profile", "a"]),
            StartError::NoSeparator,
            "no -- before the program",
        ),
        (
            all(&[]),
            StartError::NoSeparator,
            "no -- before the program",
        ),
        (
            all(&["--profile", "a", "--"]),
            StartError::NoProgram,
            "no program after --",
        ),
    ];
    for (args, error, message) in cases {
        let got = StartArgs::parse(&args).expect_err("refused");
        assert_eq!(got, error, "{args:?}");
        assert_eq!(got.to_string(), message, "{args:?}");
    }
}

/// An option that is not UTF-8 is unknown, spelled with the replacement character.
///
/// Catches a reader that panics or hides the bad argument.
#[test]
fn an_option_that_is_not_text_is_unknown() {
    let args = vec![OsString::from_vec(vec![b'-', 0xff])];
    assert_eq!(
        StartArgs::parse(&args),
        Err(StartError::UnknownOption("-\u{fffd}".to_owned()))
    );
}

/// A NUL byte in the profile or in a parameter is the in-process apply's refusal, carried
/// as is, with the message of the inner error.
///
/// Catches a NUL that reaches the C call, which would cut the text short.
#[test]
fn a_nul_in_the_profile_or_a_parameter_is_an_init_error() {
    let text = StartArgs::parse(&all(&["--profile", "(version 1)\0(x)", "--", "/bin/true"]))
        .expect_err("refused");
    assert_eq!(text, StartError::Init(InitError::TextNul));
    assert_eq!(text.to_string(), "the profile text holds a NUL byte");
    let param = StartArgs::parse(&all(&[
        "--profile",
        "(version 1)",
        "--param",
        "MT_0=/a\0b",
        "--",
        "/bin/true",
    ]))
    .expect_err("refused");
    assert_eq!(
        param,
        StartError::Init(InitError::ParamNul("MT_0".to_owned()))
    );
    assert_eq!(
        param.to_string(),
        "the profile parameter MT_0 holds a NUL byte"
    );
}
