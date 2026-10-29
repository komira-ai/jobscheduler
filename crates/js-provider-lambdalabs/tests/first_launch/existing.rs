//! `--go` refuses while instances with this runner's ownership marks exist.

use js_elastic::HostStatus;
use js_provider_lambdalabs::{
    ACQUIRE_TAG, ApiError, Call, Instance, SCHEDULER_TAG, Tag, TerminateEffect,
};

use crate::rig::{FIRST, Rig, id};

fn row(id_text: &str, name: Option<&str>, tags: Vec<Tag>, status: HostStatus) -> Instance {
    Instance {
        id: id(id_text),
        name: name.map(str::to_owned),
        status,
        region: "region-a".to_owned(),
        instance_type: "gpu_1x_cheap".to_owned(),
        ip: None,
        private_ip: None,
        hostname: None,
        first_healthy: None,
        tags,
    }
}

fn tag(key: &str, value: &str) -> Tag {
    Tag {
        key: key.to_owned(),
        value: value.to_owned(),
    }
}

/// An instance with the scheduler tag and nothing else: half the marks.
fn half_marked(status: HostStatus) -> Instance {
    row(
        "other001",
        Some("renamed-by-hand"),
        vec![tag(SCHEDULER_TAG, "first-launch")],
        status,
    )
}

fn assert_refused(rig: &Rig, status: i32, out: &str) {
    assert_eq!(status, 1, "{out}");
    assert!(out.contains("nothing was launched"), "{out}");
    assert!(
        out.lines().any(|l| l.starts_with("STOP: run teardown")),
        "{out}"
    );
    assert_eq!(rig.fake.count(Call::Launch), 0, "{out}");
    assert!(!rig.state.exists(), "no state file is made");
}

#[test]
fn a_half_marked_instance_refuses_go() {
    let rig = Rig::new();
    let args = rig.go_args(&[]);
    rig.fake.add_instance(half_marked(HostStatus::Active));
    let (status, out) = rig.exec(&args);
    assert_refused(&rig, status, &out);
    assert!(
        out.contains("other001 name=renamed-by-hand status=Active marks=suspect"),
        "{out}"
    );
}

#[test]
fn an_instance_with_an_acquire_tag_alone_is_half_marked_too() {
    let rig = Rig::new();
    let args = rig.go_args(&[]);
    rig.fake.add_instance(row(
        "other001",
        None,
        vec![tag(ACQUIRE_TAG, "x")],
        HostStatus::Booting,
    ));
    let (status, out) = rig.exec(&args);
    assert_refused(&rig, status, &out);
    assert!(out.contains("other001 name=- status=Booting"), "{out}");
}

#[test]
fn an_instance_this_runner_launched_refuses_a_second_go() {
    let rig = Rig::new();
    // The first run is killed in effect: its instance stays and its state file says so.
    rig.fake.set_terminate_effect(TerminateEffect::Nothing);
    let (status, _) = rig.exec(&rig.go_args(&["--boot-deadline-secs", "10"]));
    assert_eq!(status, 1);
    let kept = rig.state_text();
    let (status, out) = rig.exec(&rig.go_args(&["--boot-deadline-secs", "10"]));
    assert_eq!(status, 1, "{out}");
    assert!(out.contains("nothing was launched"), "{out}");
    assert!(
        out.contains(&format!("{FIRST} name=js-")) && out.contains("marks=ours"),
        "{out}"
    );
    assert!(out.contains("STOP: run teardown"), "{out}");
    assert_eq!(
        rig.fake.count(Call::Launch),
        1,
        "only the first run launched"
    );
    assert_eq!(rig.state_text(), kept, "the state file is not touched");
}

#[test]
fn a_list_that_fails_refuses_go() {
    let rig = Rig::new();
    let args = rig.go_args(&[]);
    rig.fake
        .fail_next(Call::ListInstances, ApiError::Transport("down".to_owned()));
    let (status, out) = rig.exec(&args);
    assert_refused(&rig, status, &out);
    assert!(
        out.contains("the instances cannot be listed, so none can be ruled out"),
        "{out}"
    );
}

#[test]
fn a_foreign_instance_does_not_refuse() {
    let rig = Rig::new();
    let args = rig.go_args(&[]);
    rig.fake.add_instance(row(
        "other001",
        Some("someone-elses"),
        vec![tag("team", "x")],
        HostStatus::Active,
    ));
    let (status, out) = rig.exec_active(&args, 1);
    assert_eq!(status, 0, "{out}");
    assert!(!out.contains("already exist"), "{out}");
    assert_eq!(rig.fake.count(Call::Launch), 1);
}

#[test]
fn a_terminated_instance_with_our_marks_does_not_count() {
    let rig = Rig::new();
    let (status, out) = rig.exec(&rig.args(&[]));
    assert_eq!(status, 0, "{out}");
    rig.fake.add_instance(half_marked(HostStatus::Terminated));
    let (status, out) = rig.exec(&rig.args(&[]));
    assert_eq!(status, 0, "{out}");
    assert!(!out.contains("warning"), "{out}");
}

#[test]
fn a_dry_run_shows_the_same_finding_as_a_warning_and_still_succeeds() {
    let rig = Rig::new();
    let clean = rig.hash(&[]);
    rig.fake.add_instance(half_marked(HostStatus::Active));
    let (status, out) = rig.exec(&rig.args(&[]));
    assert_eq!(status, 0, "{out}");
    assert!(
        out.contains(
            "warning: instances with this runner's ownership marks already exist; --go would refuse"
        ),
        "{out}"
    );
    assert!(out.contains("other001 name=renamed-by-hand"), "{out}");
    assert!(out.contains("dry run: nothing was launched."), "{out}");
    assert_eq!(
        crate::rig::hash_of(&out),
        clean,
        "the finding is not part of the plan"
    );
    assert_eq!(rig.fake.count(Call::Launch), 0);
}

#[test]
fn a_dry_run_whose_list_fails_warns() {
    let rig = Rig::new();
    rig.fake
        .fail_next(Call::ListInstances, ApiError::Transport("down".to_owned()));
    let (status, out) = rig.exec(&rig.args(&[]));
    assert_eq!(status, 0, "{out}");
    assert!(
        out.contains("warning: the instances cannot be listed, so none can be ruled out"),
        "{out}"
    );
}
