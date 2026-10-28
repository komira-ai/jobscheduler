//! The dry run, the plan it prints, and the command line.

use std::fs;
use std::os::unix::fs::PermissionsExt;

use js_provider_lambdalabs::{ApiError, Call, SetupError};

use crate::rig::{BASE, KEY_BYTES, Rig, hash_of, offer};

#[test]
fn a_dry_run_launches_nothing_and_prints_the_plan_with_its_hash() {
    let rig = Rig::new();
    let (status, out) = rig.exec(&rig.args(&[]));
    assert_eq!(status, 0, "{out}");
    for needle in [
        "instance type: gpu_1x_cheap",
        "region: region-a",
        "price: 75 cents per hour (from the API)",
        "boot deadline: 900 s",
        "wall-clock cap: 1800 s",
        "spend cap: 100 cents, reached 4800 s after the launch at this price",
        "ssh check: none",
        "choice: the cheapest x86_64 GPU type with capacity",
        "ssh key name on the account: my-key",
        "dry run: nothing was launched.",
        "STOP: paste back this output",
    ] {
        assert!(out.contains(needle), "{needle:?} missing:\n{out}");
    }
    assert!(out.contains(&format!("api: {BASE}")));
    assert_eq!(hash_of(&out).len(), 16);
    assert_eq!(rig.fake.launched(), 0);
    assert_eq!(rig.fake.count(Call::Launch), 0);
    assert!(!rig.state.exists());
    assert_eq!(rig.connected.borrow().as_slice(), [BASE]);
}

#[test]
fn the_cheaper_arm_type_and_the_type_without_capacity_are_passed_over() {
    let rig = Rig::new();
    let (_, out) = rig.exec(&rig.args(&[]));
    assert!(
        !out.contains("gpu_1x_arm") && !out.contains("gpu_1x_nocap"),
        "{out}"
    );
}

#[test]
fn flags_name_the_type_and_the_region_and_appear_in_the_plan_and_its_hash() {
    let rig = Rig::new();
    let plain = rig.hash(&[]);
    let (status, out) = rig.exec(&rig.args(&["--instance-type", "gpu_8x_big"]));
    assert_eq!(status, 0, "{out}");
    assert!(out.contains("instance type: gpu_8x_big") && out.contains("choice: named by flag"));
    assert_ne!(hash_of(&out), plain);
    let (_, out) = rig.exec(&rig.args(&["--region", "region-b"]));
    assert!(out.contains("region: region-b"), "{out}");
    let (status, out) =
        rig.exec(&rig.args(&["--instance-type", "gpu_8x_big", "--region", "region-b"]));
    assert_eq!(status, 1, "{out}");
    assert!(out.contains("error: no x86_64 GPU instance type with capacity fits the flags"));
}

#[test]
fn the_ssh_check_and_every_limit_are_in_the_plan() {
    let rig = Rig::new();
    let (_, out) = rig.exec(&rig.args(&[
        "--ssh-key",
        "/id",
        "--ssh-user",
        "ubuntu",
        "--ssh-command",
        "uptime",
        "--boot-deadline-secs",
        "60",
        "--wall-cap-secs",
        "120",
        "--spend-cap-cents",
        "3",
    ]));
    for needle in [
        "ssh check: ubuntu@instance: uptime",
        "boot deadline: 60 s",
        "wall-clock cap: 120 s",
        "spend cap: 3 cents, reached 144 s after the launch",
    ] {
        assert!(out.contains(needle), "{needle:?} missing:\n{out}");
    }
    let (_, out) = rig.exec(&rig.args(&["--ssh-key", "/id", "--ssh-user", "ubuntu"]));
    assert!(out.contains("ubuntu@instance: nvidia-smi"), "{out}");
}

#[test]
fn a_price_that_changed_since_the_dry_run_changes_the_hash() {
    let rig = Rig::new();
    let before = rig.hash(&[]);
    let mut offers = crate::rig::offers();
    offers[0].price_cents_per_hour = 80;
    offers[5].price_cents_per_hour = 80;
    rig.fake.set_offers(offers);
    assert_ne!(rig.hash(&[]), before);
}

#[test]
fn a_dry_run_with_a_state_file_in_place_is_still_a_dry_run() {
    let rig = Rig::new();
    fs::write(&rig.state, "x").unwrap();
    let (status, _) = rig.exec(&rig.args(&[]));
    assert_eq!(status, 0);
    assert_eq!(fs::read_to_string(&rig.state).unwrap(), "x");
}

#[test]
fn the_plan_is_refused_when_no_type_fits_or_the_api_gives_no_price_or_no_types() {
    let rig = Rig::new();
    rig.fake.set_offers(vec![offer(
        "gpu_1x_a",
        0,
        1,
        js_provider_lambdalabs::Architecture::X86_64,
        &["r"],
    )]);
    let (status, out) = rig.exec(&rig.args(&[]));
    assert_eq!(status, 1);
    assert!(out.contains("the API gave no price"), "{out}");
    rig.fake.set_offers(Vec::new());
    let (status, out) = rig.exec(&rig.args(&[]));
    assert_eq!(status, 1);
    assert!(out.contains("no usable GPU instance type"), "{out}");
    rig.fake.set_offers(vec![offer(
        "gpu_1x_a",
        5,
        1,
        js_provider_lambdalabs::Architecture::X86_64,
        &[],
    )]);
    let (status, out) = rig.exec(&rig.args(&[]));
    assert_eq!(status, 1);
    assert!(
        out.contains("no x86_64 GPU instance type with capacity"),
        "{out}"
    );
    assert!(out.contains("STOP: nothing was launched"));
}

#[test]
fn an_api_failure_reading_the_types_is_reported_whichever_read_it_hits() {
    let rig = Rig::new();
    let down = || ApiError::Transport("down".to_owned());
    rig.fake.fail_next(Call::InstanceTypes, down());
    let (status, out) = rig.exec(&rig.args(&[]));
    assert_eq!(status, 1);
    assert!(out.contains("the instance types cannot be read"), "{out}");
    rig.fake.pass_next(Call::InstanceTypes);
    rig.fake.fail_next(Call::InstanceTypes, down());
    let (status, out) = rig.exec(&rig.args(&[]));
    assert_eq!(status, 1);
    assert!(out.contains("the instance types cannot be read"), "{out}");
}

#[test]
fn a_key_name_the_account_cannot_hold_is_refused() {
    let rig = Rig::new();
    let long = "k".repeat(65);
    let mut args = rig.args(&[]);
    let at = args.iter().position(|a| a == "my-key").unwrap();
    args[at] = long;
    let (status, out) = rig.exec(&args);
    assert_eq!(status, 1);
    assert!(
        out.contains("the ssh key name must be 1 to 64 printable ASCII"),
        "{out}"
    );
}

#[test]
fn a_missing_key_name_or_state_file_is_a_usage_error() {
    let rig = Rig::new();
    let args = vec![
        "--base-url".to_owned(),
        BASE.to_owned(),
        "--key-file".to_owned(),
        rig.key.to_str().unwrap().to_owned(),
    ];
    let (status, out) = rig.exec(&args);
    assert_eq!(status, 2);
    assert!(
        out.contains("--key-name and --state-file are required"),
        "{out}"
    );
}

#[test]
fn every_malformed_command_line_is_refused_with_status_2() {
    let rig = Rig::new();
    let cases: &[(&[&str], &str)] = &[
        (&["stray"], "unexpected argument"),
        (&["--bogus", "1"], "unknown flag --bogus"),
        (&["--region"], "--region needs a value"),
        (
            &["--wall-cap-secs", "0"],
            "--wall-cap-secs must be a whole number above zero",
        ),
        (
            &["--boot-deadline-secs", "x"],
            "--boot-deadline-secs must be",
        ),
        (&["--spend-cap-cents", "-1"], "--spend-cap-cents must be"),
        (
            &["--ssh-key", "/id"],
            "--ssh-key and --ssh-user are given together",
        ),
        (
            &["--ssh-user", "u"],
            "--ssh-key and --ssh-user are given together",
        ),
        (
            &["--ssh-command", "ls"],
            "--ssh-key and --ssh-user are given together",
        ),
        (&["--plan-hash", "abc"], "--plan-hash goes with --go"),
        (
            &["--teardown-only", "f", "--go"],
            "--teardown-only stands alone",
        ),
        (
            &["--teardown-only", "f", "--plan-hash", "h"],
            "--teardown-only stands alone",
        ),
    ];
    for (extra, message) in cases {
        let (status, out) = rig.exec(&rig.args(extra));
        assert_eq!(status, 2, "{extra:?}: {out}");
        assert!(out.contains(message), "{extra:?}: {out}");
        assert!(out.contains("STOP:"));
    }
    for required in ["--base-url", "--key-file"] {
        let mut args = rig.args(&[]);
        let at = args.iter().position(|a| a == required).unwrap();
        args.drain(at..at + 2);
        let (status, out) = rig.exec(&args);
        assert_eq!(status, 2);
        assert!(out.contains(&format!("{required} is required")), "{out}");
    }
}

#[test]
fn a_stray_argument_is_never_echoed() {
    let rig = Rig::new();
    let (_, out) = rig.exec(&rig.args(&["a-secret-typed-as-an-argument"]));
    assert!(!out.contains("a-secret-typed"), "{out}");
}

#[test]
fn a_key_file_others_can_read_is_refused_without_quoting_it() {
    let rig = Rig::new();
    fs::set_permissions(&rig.key, fs::Permissions::from_mode(0o644)).unwrap();
    let (status, out) = rig.exec(&rig.args(&[]));
    assert_eq!(status, 1);
    assert!(out.contains("nobody else can read or write"), "{out}");
    assert!(!out.contains(KEY_BYTES));
    assert_eq!(rig.fake.calls(), Vec::new());
    assert!(rig.connected.borrow().is_empty());
}

#[test]
fn a_client_that_cannot_be_built_is_reported_before_any_call() {
    let rig = Rig::new();
    *rig.connect_error.borrow_mut() = Some(SetupError::CaBundleRequired);
    let (status, out) = rig.exec(&rig.args(&[]));
    assert_eq!(status, 1);
    assert!(out.contains("an https base URL needs a CA bundle"), "{out}");
    assert_eq!(rig.fake.calls(), Vec::new());
}
