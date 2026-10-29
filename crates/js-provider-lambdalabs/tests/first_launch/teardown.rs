//! `--teardown-only` from a state file, after a crash or a kill.

use std::fs;

use js_provider_lambdalabs::{ApiError, Call, Status, TerminateEffect};

use crate::rig::{FIRST, Rig};

/// A run that leaves its instance behind, as a killed process does: the instance exists
/// and the state file names it.
fn left_behind(rig: &Rig) {
    rig.fake.set_terminate_effect(TerminateEffect::Nothing);
    let args = rig.go_args(&["--boot-deadline-secs", "10"]);
    let (status, _) = rig.exec(&args);
    assert_eq!(status, 1);
    assert_ne!(rig.status_of(FIRST), Status::Terminating);
    rig.fake.set_terminate_effect(TerminateEffect::Terminating);
}

fn teardown(rig: &Rig) -> (i32, String) {
    let file = rig.state.to_str().unwrap().to_owned();
    rig.exec(&rig.args_without_plan(&["--teardown-only", &file]))
}

#[test]
fn teardown_only_terminates_what_the_state_file_names() {
    let rig = Rig::new();
    left_behind(&rig);
    let (status, out) = teardown(&rig);
    assert_eq!(status, 0, "{out}");
    assert!(out.contains("tearing down: fake0001"), "{out}");
    assert!(out.contains("result: terminated"), "{out}");
    assert_eq!(rig.status_of(FIRST), Status::Terminating);
    assert_eq!(rig.fake.launched(), 1, "nothing is launched");
}

#[test]
fn teardown_only_finds_an_instance_a_crash_left_out_of_the_state_file() {
    let rig = Rig::new();
    // The launch lands, its answer is lost, and the list that would find it fails: the
    // run stops with the state file holding no instance.
    rig.fake
        .lose_response(Call::Launch, ApiError::Transport("timeout".to_owned()));
    let args = rig.go_args(&[]);
    rig.fake.pass_next(Call::ListInstances);
    rig.fake.pass_next(Call::ListInstances);
    rig.fake
        .fail_next(Call::ListInstances, ApiError::Transport("down".to_owned()));
    // The first list is the check for instances already there, the second the launch
    // guard's; the fault falls on the one after the lost answer.
    let (status, out) = rig.exec(&args);
    assert_eq!(status, 1, "{out}");
    assert!(rig.state_text().contains("instances=\n"));
    assert_eq!(rig.fake.launched(), 1);
    let (status, out) = teardown(&rig);
    assert_eq!(status, 0, "{out}");
    assert_eq!(rig.status_of(FIRST), Status::Terminating);
}

#[test]
fn teardown_only_goes_on_when_the_list_fails_and_the_file_names_the_instance() {
    let rig = Rig::new();
    left_behind(&rig);
    rig.fake
        .fail_next(Call::ListInstances, ApiError::Transport("down".to_owned()));
    let (status, out) = teardown(&rig);
    assert_eq!(status, 0, "{out}");
    assert!(out.contains("the instances could not be listed"), "{out}");
    assert_eq!(rig.status_of(FIRST), Status::Terminating);
}

#[test]
fn teardown_only_with_nothing_to_find_says_so_and_fails() {
    let rig = Rig::new();
    rig.fake
        .fail_next(Call::Launch, ApiError::Transport("timeout".to_owned()));
    let (status, _) = rig.exec(&rig.go_args(&[]));
    assert_eq!(status, 1);
    let (status, out) = teardown(&rig);
    assert_eq!(status, 1, "{out}");
    assert!(
        out.contains("no instance of this state file was found"),
        "{out}"
    );
    assert!(out.contains("STOP: check the provider console"), "{out}");
}

#[test]
fn teardown_only_that_cannot_confirm_says_the_instance_may_still_bill() {
    let rig = Rig::new();
    left_behind(&rig);
    rig.fake.set_terminate_effect(TerminateEffect::Nothing);
    let (status, out) = teardown(&rig);
    assert_eq!(status, 1, "{out}");
    assert_eq!(
        rig.fake.count(Call::Terminate),
        10,
        "the first run tried five times too"
    );
    assert!(
        out.contains("STOP: AN INSTANCE MAY STILL BE RUNNING AND BILLING"),
        "{out}"
    );
}

#[test]
fn teardown_only_does_not_need_the_launch_flags() {
    let rig = Rig::new();
    left_behind(&rig);
    let args = vec![
        "--base-url".to_owned(),
        crate::rig::BASE.to_owned(),
        "--key-file".to_owned(),
        rig.key.to_str().unwrap().to_owned(),
        "--teardown-only".to_owned(),
        rig.state.to_str().unwrap().to_owned(),
    ];
    let before = rig.fake.count(Call::InstanceTypes);
    let (status, out) = rig.exec(&args);
    assert_eq!(status, 0, "{out}");
    assert_eq!(
        rig.fake.count(Call::InstanceTypes),
        before,
        "no type is read"
    );
}

#[test]
fn a_state_file_that_is_missing_or_not_ours_is_refused() {
    let rig = Rig::new();
    let (status, out) = teardown(&rig);
    assert_eq!(status, 1);
    assert!(out.contains("state: "), "{out}");
    assert_eq!(rig.fake.calls(), Vec::new());
    let good = "first-launch-state=1\nacquire=first-launch-abc\nlaunched_unix=5\nkey_name=k\ninstance_type=gpu_1x_cheap\nregion=r\nprice_cents_per_hour=75\ninstances=fake0001\n";
    let lines: Vec<&str> = good.lines().collect();
    // Cut after each line: every field is required.
    for keep in 0..lines.len() {
        fs::write(&rig.state, lines[..keep].join("\n")).unwrap();
        let (status, out) = teardown(&rig);
        assert_eq!(status, 1, "{keep}: {out}");
        assert!(
            out.contains("is not a state file of this runner"),
            "{keep}: {out}"
        );
    }
    for (from, to) in [
        ("first-launch-state=1", "first-launch-state=2"),
        ("acquire=first-launch-abc", "acquire=a b"),
        ("acquire=first-launch-abc", "acquire first-launch-abc"),
        ("launched_unix=5", "launched_unix=soon"),
        ("price_cents_per_hour=75", "price_cents_per_hour=-1"),
        ("instances=fake0001", "instances=fake 1"),
        ("key_name=k", "region=r"),
    ] {
        fs::write(&rig.state, good.replace(from, to)).unwrap();
        let (status, out) = teardown(&rig);
        assert_eq!(status, 1, "{to}: {out}");
        assert!(
            out.contains("is not a state file of this runner"),
            "{to}: {out}"
        );
    }
    assert_eq!(rig.fake.calls(), Vec::new());
}

#[test]
fn a_state_file_whose_key_name_or_type_is_unusable_is_refused() {
    let rig = Rig::new();
    let good = "first-launch-state=1\nacquire=first-launch-abc\nlaunched_unix=5\nkey_name=k\ninstance_type=gpu_1x_cheap\nregion=r\nprice_cents_per_hour=75\ninstances=fake0001\n";
    for bad in ["key_name=k", "instance_type=gpu_1x_cheap"] {
        let replaced = match bad {
            "key_name=k" => "key_name=",
            _ => "instance_type=Not Valid",
        };
        fs::write(&rig.state, good.replace(bad, replaced)).unwrap();
        let (status, out) = teardown(&rig);
        assert_eq!(status, 1, "{out}");
        assert_eq!(rig.fake.count(Call::Terminate), 0);
    }
}
