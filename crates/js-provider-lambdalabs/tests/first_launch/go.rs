//! The launch: its guards, the state file, the wait, the ssh check and the result.

use std::cell::RefCell;
use std::fs;
use std::path::PathBuf;
use std::time::Duration;

use js_provider_lambdalabs::{ACQUIRE_TAG, ApiError, Call, ErrorCode, SCHEDULER_TAG, Status, Tag};

use crate::rig::{FIRST, Rig, TestRemote, activate, id};

fn ssh_flags() -> [&'static str; 4] {
    ["--ssh-key", "/keys/id", "--ssh-user", "ubuntu"]
}

#[test]
fn go_without_a_plan_hash_or_with_another_launches_nothing() {
    let rig = Rig::new();
    let (status, out) = rig.exec(&rig.args(&["--go"]));
    assert_eq!(status, 1, "{out}");
    assert!(out.contains("--go needs --plan-hash"), "{out}");
    assert!(
        out.contains("plan hash: "),
        "the plan is shown so the hash can be compared"
    );
    let (status, out) = rig.exec(&rig.args(&["--go", "--plan-hash", "0123456789abcdef"]));
    assert_eq!(status, 1, "{out}");
    assert!(
        out.contains("does not match, so nothing was launched"),
        "{out}"
    );
    assert_eq!(rig.fake.count(Call::Launch), 0);
    assert!(!rig.state.exists());
}

#[test]
fn a_hash_of_a_plan_with_other_flags_is_refused() {
    let rig = Rig::new();
    let hash = rig.hash(&["--instance-type", "gpu_8x_big"]);
    let (status, _) = rig.exec(&rig.args(&["--go", "--plan-hash", &hash]));
    assert_eq!(status, 1);
    assert_eq!(rig.fake.count(Call::Launch), 0);
}

#[test]
fn a_price_that_moved_since_the_dry_run_refuses_the_launch() {
    let rig = Rig::new();
    let hash = rig.hash(&[]);
    let mut offers = crate::rig::offers();
    offers[0].price_cents_per_hour = 76;
    offers[5].price_cents_per_hour = 76;
    rig.fake.set_offers(offers);
    let (status, out) = rig.exec(&rig.args(&["--go", "--plan-hash", &hash]));
    assert_eq!(status, 1, "{out}");
    assert!(
        out.contains("76 cents per hour"),
        "the new plan is shown:\n{out}"
    );
    assert_eq!(rig.fake.count(Call::Launch), 0);
}

#[test]
fn the_state_file_exists_with_the_instance_before_the_first_wait() {
    let rig = Rig::new();
    let args = rig.go_args(&[]);
    let seen = RefCell::new(Vec::new());
    let remote = TestRemote::answering(Err("not asked".to_owned()));
    let fake = rig.fake.clone();
    let path = rig.state.clone();
    let (status, out) = rig.exec_with(
        &args,
        &|n| {
            if n == 1 {
                seen.borrow_mut().push(fs::read_to_string(&path).unwrap());
                activate(&fake);
            }
        },
        &remote,
    );
    assert_eq!(status, 0, "{out}");
    let seen = seen.into_inner();
    assert_eq!(seen.len(), 1);
    let text = &seen[0];
    assert!(text.contains(&format!("instances={FIRST}\n")), "{text}");
    assert!(text.contains("launched_unix=1000\n"), "{text}");
    assert!(text.contains("acquire=first-launch-"), "{text}");
    assert!(text.contains("key_name=my-key\n") && text.contains("price_cents_per_hour=75\n"));
}

#[test]
fn the_state_file_is_owner_only_and_names_the_acquire_the_instance_is_tagged_with() {
    let rig = Rig::new();
    let args = rig.go_args(&[]);
    let (status, _) = rig.exec_active(&args, 1, &TestRemote::answering(Ok(String::new())));
    assert_eq!(status, 0);
    use std::os::unix::fs::PermissionsExt;
    assert_eq!(
        fs::metadata(&rig.state).unwrap().permissions().mode() & 0o777,
        0o600
    );
    let acquire = rig
        .state_text()
        .lines()
        .find_map(|l| l.strip_prefix("acquire="))
        .unwrap()
        .to_owned();
    let tagged = rig.fake.instances().remove(0);
    assert!(tagged.tags.contains(&Tag {
        key: ACQUIRE_TAG.to_owned(),
        value: acquire,
    }));
    assert!(
        tagged
            .tags
            .iter()
            .any(|t| t.key == SCHEDULER_TAG && t.value == "first-launch")
    );
    assert!(tagged.name.unwrap().starts_with("js-"));
}

#[test]
fn a_launch_goes_active_runs_the_ssh_check_and_terminates() {
    let rig = Rig::new();
    let mut extra = ssh_flags().to_vec();
    extra.extend(["--ssh-command", "nvidia-smi -L"]);
    let args = rig.go_args(&extra);
    let remote = TestRemote::answering(Ok("GPU 0: Sample\n".to_owned()));
    let (status, out) = rig.exec_active(&args, 3, &remote);
    assert_eq!(status, 0, "{out}");
    assert_eq!(rig.fake.launched(), 1);
    let asked = remote.asked.borrow();
    assert_eq!(asked.len(), 1);
    let (host, user, identity, command, limit) = &asked[0];
    assert_eq!(
        (host.as_str(), user.as_str(), command.as_str()),
        ("203.0.113.7", "ubuntu", "nvidia-smi -L")
    );
    assert_eq!(identity, &PathBuf::from("/keys/id"));
    assert_eq!(*limit, Duration::from_secs(120));
    assert!(
        out.contains("ssh output of nvidia-smi -L:\nGPU 0: Sample"),
        "{out}"
    );
    assert!(out.contains("result: terminated after 30 s"), "{out}");
    assert!(
        out.contains("at most 1 cents spent at 75 cents per hour"),
        "{out}"
    );
    assert!(
        out.contains("STOP: paste back everything printed above"),
        "{out}"
    );
    assert_eq!(rig.status_of(FIRST), Status::Terminating);
    assert_eq!(rig.fake.count(Call::Terminate), 1);
}

#[test]
fn the_default_ssh_command_is_nvidia_smi_and_its_output_is_cut() {
    let rig = Rig::new();
    let args = rig.go_args(&ssh_flags());
    let long = format!("{}{}", "a".repeat(4000), "\u{2603}".repeat(10));
    let remote = TestRemote::answering(Ok(long));
    let (status, out) = rig.exec_active(&args, 1, &remote);
    assert_eq!(status, 0, "{out}");
    assert_eq!(remote.asked.borrow()[0].3, "nvidia-smi");
    assert!(!out.contains('\u{2603}'));
}

#[test]
fn without_ssh_flags_nothing_is_run_on_the_instance() {
    let rig = Rig::new();
    let args = rig.go_args(&[]);
    let remote = TestRemote::answering(Err("never".to_owned()));
    let (status, out) = rig.exec_active(&args, 1, &remote);
    assert_eq!(status, 0, "{out}");
    assert!(remote.asked.borrow().is_empty());
    assert!(
        !out.contains("ssh output") && !out.contains("ssh check failed"),
        "{out}"
    );
    assert_eq!(rig.status_of(FIRST), Status::Terminating);
}

#[test]
fn a_failed_ssh_check_still_terminates_and_fails_the_run() {
    let rig = Rig::new();
    let args = rig.go_args(&ssh_flags());
    let remote = TestRemote::answering(Err("the command ended with status 1".to_owned()));
    let (status, out) = rig.exec_active(&args, 1, &remote);
    assert_eq!(status, 1, "{out}");
    assert!(
        out.contains("ssh check failed: the command ended with status 1"),
        "{out}"
    );
    assert!(out.contains("result: terminated"));
    assert!(out.contains("STOP: the run did not finish its check; the instance is terminated"));
    assert_eq!(rig.status_of(FIRST), Status::Terminating);
}

#[test]
fn an_instance_without_a_public_address_cannot_be_asked() {
    let rig = Rig::new();
    let args = rig.go_args(&ssh_flags());
    let fake = rig.fake.clone();
    let remote = TestRemote::answering(Ok(String::new()));
    let (status, out) = rig.exec_with(
        &args,
        &|_| fake.set_status(&id(FIRST), Status::Active),
        &remote,
    );
    assert_eq!(status, 1, "{out}");
    assert!(out.contains("the instance has no public address"), "{out}");
    assert!(remote.asked.borrow().is_empty());
    assert_eq!(rig.status_of(FIRST), Status::Terminating);
}

#[test]
fn an_instance_that_is_not_ours_by_the_time_of_release_is_not_terminated() {
    let rig = Rig::new();
    let args = rig.go_args(&ssh_flags());
    let mut remote = TestRemote::answering(Ok(String::new()));
    let fake = rig.fake.clone();
    remote.on_run = Box::new(move || fake.update_instance(&id(FIRST), &|i| i.tags.clear()));
    let (status, out) = rig.exec_active(&args, 1, &remote);
    assert_eq!(status, 1, "{out}");
    assert_eq!(rig.fake.count(Call::Terminate), 0);
    assert!(out.contains("TERMINATION NOT CONFIRMED"), "{out}");
    assert!(
        out.contains("STOP: AN INSTANCE MAY STILL BE RUNNING AND BILLING"),
        "{out}"
    );
    assert!(out.contains(&rig.state.display().to_string()));
}

#[test]
fn a_launch_that_answers_with_several_instances_terminates_all_of_them() {
    let rig = Rig::new();
    let args = rig.go_args(&[]);
    rig.fake.launch_creates(2);
    let (status, out) = rig.exec_active(&args, 1, &TestRemote::answering(Ok(String::new())));
    assert_eq!(status, 0, "{out}");
    assert!(out.contains("launched: fake0001 fake0002"), "{out}");
    assert!(rig.state_text().contains("instances=fake0001,fake0002\n"));
    for instance in rig.fake.instances() {
        assert_eq!(instance.status, Status::Terminating, "{}", instance.id);
    }
}

#[test]
fn a_refused_launch_leaves_no_state_file_and_no_instance() {
    let rig = Rig::new();
    let args = rig.go_args(&[]);
    rig.fake.fail_next(
        Call::Launch,
        ApiError::Api {
            status: 400,
            code: ErrorCode::InsufficientCapacity,
            request_id: None,
        },
    );
    let (status, out) = rig.exec(&args);
    assert_eq!(status, 1, "{out}");
    assert!(out.contains("nothing was launched"), "{out}");
    assert!(!rig.state.exists());
    assert_eq!(rig.fake.launched(), 0);
}

#[test]
fn an_existing_state_file_stops_the_run_before_anything_is_launched() {
    let rig = Rig::new();
    let args = rig.go_args(&[]);
    fs::write(&rig.state, "earlier").unwrap();
    let (status, out) = rig.exec(&args);
    assert_eq!(status, 1, "{out}");
    assert_eq!(rig.fake.count(Call::Launch), 0);
    assert_eq!(fs::read_to_string(&rig.state).unwrap(), "earlier");
    assert!(out.contains("nothing was launched"));
}

#[test]
fn a_lost_launch_answer_is_resolved_by_listing_and_the_run_goes_on() {
    let rig = Rig::new();
    let args = rig.go_args(&[]);
    rig.fake
        .lose_response(Call::Launch, ApiError::Transport("timeout".to_owned()));
    let (status, out) = rig.exec_active(&args, 1, &TestRemote::answering(Ok(String::new())));
    assert_eq!(status, 0, "{out}");
    assert!(
        out.contains("the launch got no answer; looking for the instance"),
        "{out}"
    );
    assert_eq!(rig.fake.launched(), 1);
    assert_eq!(rig.status_of(FIRST), Status::Terminating);
}

#[test]
fn a_launch_with_no_answer_and_no_instance_keeps_the_state_file() {
    let rig = Rig::new();
    let args = rig.go_args(&[]);
    rig.fake
        .fail_next(Call::Launch, ApiError::Transport("timeout".to_owned()));
    let (status, out) = rig.exec(&args);
    assert_eq!(status, 1, "{out}");
    assert!(out.contains("the state file is kept"), "{out}");
    let text = rig.state_text();
    assert!(
        text.contains("instances=\n") && text.contains("acquire=first-launch-"),
        "{text}"
    );
}

#[test]
fn a_state_file_that_cannot_be_updated_tears_the_instance_down_at_once() {
    let rig = Rig::new();
    let args = rig.go_args(&[]);
    fs::create_dir(rig.dir.join("state.new")).unwrap();
    let (status, out) = rig.exec(&args);
    assert_eq!(status, 1, "{out}");
    assert!(
        out.contains("teardown forced: the state file could not be updated"),
        "{out}"
    );
    assert_eq!(rig.status_of(FIRST), Status::Terminating);
    assert!(rig.sleeps.borrow().is_empty(), "no wait happened");
}

#[test]
fn a_failing_list_while_booting_is_reported_and_the_wait_goes_on() {
    let rig = Rig::new();
    let args = rig.go_args(&[]);
    let fake = rig.fake.clone();
    let (status, out) = rig.exec_with(
        &args,
        &move |n| match n {
            1 => fake.fail_next(Call::ListInstances, ApiError::Transport("blip".to_owned())),
            2 => activate(&fake),
            _ => {}
        },
        &TestRemote::answering(Ok(String::new())),
    );
    assert_eq!(status, 0, "{out}");
    assert!(out.contains("t+10 s: list failed"), "{out}");
}

#[test]
fn an_instance_that_ends_by_itself_forces_teardown() {
    let rig = Rig::new();
    let args = rig.go_args(&[]);
    let fake = rig.fake.clone();
    let (status, out) = rig.exec_with(
        &args,
        &move |_| fake.set_status(&id(FIRST), Status::Preempted),
        &TestRemote::answering(Ok(String::new())),
    );
    assert_eq!(status, 1, "{out}");
    assert!(
        out.contains("teardown forced: the instance ended on its own"),
        "{out}"
    );
}
