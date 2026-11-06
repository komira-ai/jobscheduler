//! The arguments of a live run: every flag read, every mistake refused by name.

use std::collections::BTreeMap;

use js_elastic::SchedulerId;
use js_elastic::conformance::live::LiveArgs;

use crate::mem::*;

/// Asserts that every flag is read into its field, that a zone is a location as a region
/// is, that the scheduler is `live-` and the run id, and that an extra flag is read and
#[test]
fn parse_reads_every_flag() {
    let args = LiveArgs::parse(
        &strings(&[
            "--run-id",
            "run-1",
            "--zone",
            "us-central1-a",
            "--shape",
            "small",
            "--key-file",
            "keys/one",
            "--spend",
            "--plan-hash",
            &"ab01".repeat(16),
        ]),
        &["key-file", "other"],
    )
    .unwrap();
    assert_eq!(args.run_id, "run-1");
    assert_eq!(args.location, "us-central1-a");
    assert_eq!(args.shape, "small");
    assert_eq!(args.plan_hash, Some("ab01".repeat(16)));
    assert_eq!(
        args.extra,
        BTreeMap::from([("key-file".to_owned(), "keys/one".to_owned())])
    );
    assert_eq!(args.scheduler(), SchedulerId::new("live-run-1").unwrap());
    assert_eq!(args.required("key-file"), Ok("keys/one"));
    assert_eq!(
        args.required("other"),
        Err("--other is required".to_owned())
    );
    let plain = args_of(&[]);
    assert_eq!(
        (
            plain.location.as_str(),
            plain.shape.as_str(),
            plain.plan_hash
        ),
        ("us-east-1", "cheap", None)
    );
    assert!(plain.extra.is_empty());
}

/// Asserts that a run id of 1 and of 40 characters, with inner dashes and digits, is
/// accepted.
#[test]
fn parse_accepts_run_ids_at_the_limits() {
    for id in ["a", "0", "a-b-0", &"z".repeat(40)] {
        let args = LiveArgs::parse(
            &strings(&["--run-id", id, "--region", "r", "--shape", "s"]),
            &[],
        )
        .unwrap();
        assert_eq!(args.run_id, id);
    }
}

/// Asserts the exact message of each way the arguments are wrong, naming the flag.
#[test]
fn parse_refuses_bad_arguments_naming_the_flag() {
    let hash = "0".repeat(64);
    let bad_run =
        "--run-id must be 1 to 40 characters of a-z, 0-9 and '-', not starting or ending with '-'";
    let long = "a".repeat(41);
    let cases: Vec<(Vec<&str>, &str)> = vec![
        (vec![], "--run-id is required"),
        (vec!["--region", "r"], "--run-id is required"),
        (
            vec!["stray"],
            "unexpected argument stray: every argument is a --flag",
        ),
        (
            vec!["-x"],
            "unexpected argument -x: every argument is a --flag",
        ),
        (vec!["--bogus", "1"], "unknown flag --bogus"),
        (vec!["--"], "unknown flag --"),
        (
            vec!["--run-id", "a", "--run-id", "b"],
            "--run-id is given twice",
        ),
        (vec!["--spend", "--spend"], "--spend is given twice"),
        (
            vec!["--key-file", "a", "--key-file", "b"],
            "--key-file is given twice",
        ),
        (vec!["--run-id"], "--run-id needs a value"),
        (vec!["--run-id", ""], "--run-id needs a value"),
        (vec!["--run-id", "--region", "r"], "--run-id needs a value"),
        (vec!["--run-id", "Abc", "--region", "r"], bad_run),
        (vec!["--run-id", "a_b", "--region", "r"], bad_run),
        (vec!["--run-id", "-a", "--region", "r"], bad_run),
        (vec!["--run-id", "a-", "--region", "r"], bad_run),
        (vec!["--run-id", &long, "--region", "r"], bad_run),
        (
            vec!["--run-id", "a"],
            "one of --region or --zone is required",
        ),
        (
            vec!["--run-id", "a", "--region", "r", "--zone", "z"],
            "--region and --zone cannot both be given",
        ),
        (
            vec!["--run-id", "a", "--region", "r"],
            "--shape is required",
        ),
        (
            vec![
                "--run-id",
                "a",
                "--zone",
                "z",
                "--spend",
                "--plan-hash",
                &hash,
            ],
            "--shape is required",
        ),
        (
            vec!["--run-id", "a", "--region", "r", "--shape"],
            "--shape needs a value",
        ),
        (
            vec![
                "--run-id", "a", "--region", "r", "--shape", "s", "--shape", "t",
            ],
            "--shape is given twice",
        ),
        (
            vec!["--run-id", "a", "--region", "r", "--shape", "s", "--spend"],
            "--spend needs --plan-hash HASH, the hash a dry run printed",
        ),
        (
            vec![
                "--run-id",
                "a",
                "--region",
                "r",
                "--shape",
                "s",
                "--plan-hash",
                &hash,
            ],
            "--plan-hash is only valid with --spend",
        ),
    ];
    for (args, message) in cases {
        let got = LiveArgs::parse(&strings(&args), &["key-file"]);
        assert_eq!(got, Err(message.to_owned()), "{args:?}");
    }
    let short = "0".repeat(63);
    let upper = "A".repeat(64);
    let letters = "g".repeat(64);
    for hash in [short, upper, letters] {
        let got = LiveArgs::parse(
            &strings(&[
                "--run-id",
                "a",
                "--region",
                "r",
                "--shape",
                "s",
                "--spend",
                "--plan-hash",
                &hash,
            ]),
            &[],
        );
        assert_eq!(
            got,
            Err("--plan-hash must be 64 lowercase hexadecimal digits".to_owned())
        );
    }
}

/// Asserts that a flag nobody declared is unknown, and that an extra named like a flag of
/// the driver is read as that flag.
#[test]
fn parse_knows_only_the_declared_extras() {
    let undeclared = LiveArgs::parse(&strings(&["--run-id", "a", "--key-file", "k"]), &[]);
    assert_eq!(undeclared, Err("unknown flag --key-file".to_owned()));
    let shadow = LiveArgs::parse(
        &strings(&["--run-id", "a", "--region", "r", "--shape", "s"]),
        &["run-id"],
    );
    assert!(shadow.unwrap().extra.is_empty());
}
