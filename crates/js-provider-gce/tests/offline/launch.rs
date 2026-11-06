//! The records of the launch side: names, ids, references, images, statuses, payloads,
//! request ids, operations, and the words of the errors and methods that go with them.

use js_elastic::{AcquireId, HostRef, HostStatus, SchedulerId};
use js_provider_gce::{
    ACQUIRE_LABEL, ApiError, HostMaintenance, ImagePath, InstanceId, InstanceKey, InstanceName,
    InstanceStatus, LABEL_PREFIX, Method, Operation, OperationRef, OperationStatus, PAYLOAD_KEY,
    Payload, Project, ProvisioningModel, RequestId, SCHEDULER_LABEL, TerminationAction, host_ref,
    is_label_key, is_label_value, is_resource_name, label_value, parse_host_ref,
};

use crate::rig::{PROJECT, ZONE, id, label, row, zone};

/// Asserts that the names that mark an instance are spelled as they are on instances that
/// already exist: the scheduler label, the acquire label, the prefix of an acquire's own
/// labels and the metadata key of the payload. It catches a rename, which would make the
/// instances already launched look foreign.
#[test]
fn the_marking_names_are_spelled_as_documented() {
    assert_eq!(SCHEDULER_LABEL, "js-scheduler");
    assert_eq!(ACQUIRE_LABEL, "js-acquire-id");
    assert_eq!(LABEL_PREFIX, "js-label-");
    assert_eq!(PAYLOAD_KEY, "user-data");
}

/// Asserts that a label value is the id itself when the id is at most 63 lower-case
/// letters, digits and dashes, and otherwise `_` and the first 62 hexadecimal digits of the
/// SHA-256 of the id (pinned for one id), always a value of the label grammar and 63
/// characters long when hashed; and that an id with an underscore or an upper-case letter
/// is hashed, so no hashed value can equal a plain one. It catches a value that is cut or
/// rewritten instead of hashed, and one whose hash changes between releases.
#[test]
fn a_label_value_is_the_id_or_a_hash_of_it() {
    assert_eq!(label_value("sched-one"), "sched-one");
    assert_eq!(label_value(""), "");
    let longest = "a".repeat(63);
    assert_eq!(label_value(&longest), longest);
    assert_eq!(
        label_value("Sched-One"),
        "_4e800461ecea78c889e2e3f36a9cd2423e6b53995aac7087bb65454b9d5e9e"
    );
    for hashed in [
        label_value("Sched-One"),
        label_value(&"a".repeat(64)),
        label_value("has_underscore"),
        label_value("\u{e9}"),
    ] {
        assert!(hashed.starts_with('_'), "{hashed}");
        assert_eq!(hashed.len(), 63);
        assert!(is_label_value(&hashed));
    }
    assert_ne!(label_value("has_underscore"), label_value("has-underscore"));
    assert_ne!(label_value(&"a".repeat(64)), label_value(&"a".repeat(65)));
}

/// Asserts the edges of the three grammars: a label value may be empty and holds lower-case
/// letters, digits, `_` and `-` up to 63; a label key is the same but non-empty and starts
/// with a letter; a resource name is non-empty, starts with a letter, does not end with `-`
/// and has no `_`. It catches a grammar that is off by one at either end or lets a
/// character through.
#[test]
fn the_grammars_have_their_edges_where_the_cloud_has_them() {
    let sixty_three = "a".repeat(63);
    let sixty_four = "a".repeat(64);
    for (text, value, key, name) in [
        ("", true, false, false),
        ("a", true, true, true),
        ("a_b", true, true, false),
        ("a-b", true, true, true),
        ("a-", true, true, false),
        ("-a", true, false, false),
        ("_a", true, false, false),
        ("1a", true, false, false),
        ("A", false, false, false),
        ("a b", false, false, false),
        ("\u{e9}", false, false, false),
        (sixty_three.as_str(), true, true, true),
        (sixty_four.as_str(), false, false, false),
    ] {
        assert_eq!(is_label_value(text), value, "value {text:?}");
        assert_eq!(is_label_key(text), key, "key {text:?}");
        assert_eq!(is_resource_name(text), name, "name {text:?}");
    }
}

/// Asserts that an instance name is accepted exactly when it is a resource name, and reads
/// back as the text it was made from. It catches a name that is rewritten.
#[test]
fn an_instance_name_is_a_resource_name() {
    let name = InstanceName::new("js-abc-0123").unwrap();
    assert_eq!(name.as_str(), "js-abc-0123");
    assert_eq!(name.to_string(), "js-abc-0123");
    for bad in ["", "Js", "js-", "1js", "js_abc"] {
        assert_eq!(InstanceName::new(bad), None, "{bad:?}");
    }
}

/// Asserts that an instance id is 1 to 20 decimal digits with no leading zero that fit 64
/// bits: the largest 64-bit number is accepted and one more is not, nor are 21 digits, a
/// leading zero, a sign, a letter or nothing. It catches an id that a number cannot hold
/// and an id that is not the text the cloud wrote.
#[test]
fn an_instance_id_is_a_decimal_number_of_64_bits() {
    for good in ["1", "42", "9000001", "18446744073709551615"] {
        let parsed = InstanceId::new(good).unwrap();
        assert_eq!(parsed.as_str(), good);
        assert_eq!(parsed.to_string(), good);
    }
    for bad in [
        "",
        "0",
        "012",
        "-1",
        "+1",
        "12a",
        "1 2",
        "18446744073709551616",
        "123456789012345678901",
    ] {
        assert_eq!(InstanceId::new(bad), None, "{bad:?}");
    }
}

/// Asserts that a key names the instance by its name or by its id, and writes it as a call
/// does. It catches a key that writes the wrong one of the two.
#[test]
fn an_instance_key_writes_the_name_or_the_id() {
    let by_name = InstanceKey::Name(InstanceName::new("js-abc-0123").unwrap());
    let by_id = InstanceKey::Id(id("42"));
    assert_eq!(by_name.as_str(), "js-abc-0123");
    assert_eq!(by_id.as_str(), "42");
}

/// Asserts that the scheduler's reference is the zone and the id joined by `/`, that it
/// reads back as the same pair, and that text of any other form is no reference: no `/`, an
/// empty zone or id, an id with a leading zero or letters, a second `/`, and a zone that is
/// not a zone name. It catches a reference that reads back as another instance.
#[test]
fn a_host_reference_is_the_zone_and_the_id() {
    let reference = host_ref(&zone(ZONE), &id("42"));
    assert_eq!(reference.as_str(), "region-one-a/42");
    assert_eq!(parse_host_ref(&reference), Some((zone(ZONE), id("42"))));
    for bad in [
        "region-one-a",
        "/42",
        "region-one-a/",
        "region-one-a/042",
        "region-one-a/abc",
        "region-one-a/4/2",
        "Bad/42",
        "nodash/42",
        "42",
        "",
    ] {
        assert_eq!(parse_host_ref(&HostRef(bad.to_owned())), None, "{bad:?}");
    }
}

/// Asserts that an image path is either `projects/<project>/global/images/<image>` or the
/// same with `family/` before the name, that it reads back as written, and that each part
/// is checked: a missing `projects/`, a project that is not a project id, another middle,
/// a name that is not a resource name, and an empty image or family. It catches a path the
/// cloud would refuse at the first insert.
#[test]
fn an_image_path_is_an_image_or_a_family_of_a_project() {
    for good in [
        "projects/image-project/global/images/js-agent-x86",
        "projects/image-project/global/images/family/js-agent-arm",
    ] {
        let parsed = ImagePath::new(good).unwrap();
        assert_eq!(parsed.as_str(), good);
        assert_eq!(parsed.to_string(), good);
    }
    for bad in [
        "global/images/js-agent",
        "projects/image-project",
        "projects/x/global/images/js-agent",
        "projects/image-project/regional/images/js-agent",
        "projects/image-project/global/images/",
        "projects/image-project/global/images/Js-Agent",
        "projects/image-project/global/images/family/",
        "projects/image-project/global/images/family/Js-Agent",
        "",
    ] {
        assert_eq!(ImagePath::new(bad), None, "{bad:?}");
    }
}

/// Asserts that each of the nine statuses the cloud documents parses to its own variant and
/// reads as the scheduler's status it is documented to be (provisioning, staging and
/// repairing are booting; running is active; stopping and suspending are terminating;
/// terminated, stopped and suspended are terminated), and that a status this release does
/// not know, which includes a known one in another case, is kept as printable ASCII cut to
/// 40 characters and reads as the scheduler's unknown. It catches a status mapped to the
/// wrong word and an unknown status that is read as gone or that carries any character.
#[test]
fn a_status_reads_as_the_scheduler_reads_it() {
    let known = [
        (
            "PROVISIONING",
            InstanceStatus::Provisioning,
            HostStatus::Booting,
        ),
        ("STAGING", InstanceStatus::Staging, HostStatus::Booting),
        ("REPAIRING", InstanceStatus::Repairing, HostStatus::Booting),
        ("RUNNING", InstanceStatus::Running, HostStatus::Active),
        (
            "STOPPING",
            InstanceStatus::Stopping,
            HostStatus::Terminating,
        ),
        (
            "SUSPENDING",
            InstanceStatus::Suspending,
            HostStatus::Terminating,
        ),
        (
            "TERMINATED",
            InstanceStatus::Terminated,
            HostStatus::Terminated,
        ),
        ("STOPPED", InstanceStatus::Stopped, HostStatus::Terminated),
        (
            "SUSPENDED",
            InstanceStatus::Suspended,
            HostStatus::Terminated,
        ),
    ];
    for (text, status, host) in known {
        assert_eq!(InstanceStatus::parse(text), status, "{text}");
        assert_eq!(status.status(), host, "{text}");
    }
    assert_eq!(
        InstanceStatus::parse("running"),
        InstanceStatus::Other("running".to_owned())
    );
    let noisy = format!("a b\n{}", "c".repeat(60));
    let kept = InstanceStatus::parse(&noisy);
    assert_eq!(kept, InstanceStatus::Other(format!("ab{}", "c".repeat(38))));
    assert_eq!(
        kept.status(),
        HostStatus::Unknown(format!("ab{}", "c".repeat(38)))
    );
    assert_eq!(
        InstanceStatus::parse("WEIRD").status(),
        HostStatus::Unknown("WEIRD".to_owned())
    );
}

/// Asserts that an instance answers the value of a label it has, nothing for one it lacks,
/// and the scheduler's reference made of its zone and id. It catches a label read by the
/// wrong key and a reference that is not the one a release parses.
#[test]
fn an_instance_answers_its_labels_and_its_reference() {
    let instance = row(
        "42",
        InstanceStatus::Running,
        "js-abc-0123",
        vec![label("k", "v"), label("k2", "v2")],
    );
    assert_eq!(instance.label("k"), Some("v"));
    assert_eq!(instance.label("k2"), Some("v2"));
    assert_eq!(instance.label("missing"), None);
    assert_eq!(instance.host_ref(), host_ref(&zone(ZONE), &id("42")));
}

/// Asserts the words the API takes for the provisioning model, the host maintenance setting
/// and the termination action. It catches a spelling the cloud would refuse.
#[test]
fn the_scheduling_words_are_the_apis() {
    assert_eq!(ProvisioningModel::Standard.name(), "STANDARD");
    assert_eq!(ProvisioningModel::Spot.name(), "SPOT");
    assert_eq!(HostMaintenance::Terminate.name(), "TERMINATE");
    assert_eq!(TerminationAction::Delete.name(), "DELETE");
}

/// Asserts that a payload hands back the text it was made from, counts its bytes, knows
/// when it is empty, and that its `Debug` text says how many bytes there are and never what
/// they are. It catches a payload that can leak a join token into a log.
#[test]
fn a_payload_never_prints_what_it_holds() {
    let payload = Payload::new("join-token-123".to_owned());
    assert_eq!(payload.expose(), "join-token-123");
    assert_eq!(payload.len(), 14);
    assert!(!payload.is_empty());
    assert_eq!(format!("{payload:?}"), "Payload(14 bytes)");
    assert!(Payload::new(String::new()).is_empty());
    assert_eq!(Payload::new(String::new()).len(), 0);
}

/// Asserts that a request id is the first 16 bytes of the SHA-256 of the scheduler id, a
/// newline and the acquire id, as a version 8 UUID (pinned for one pair): 36 characters in
/// the groups 8-4-4-4-12, the version digit 8 and a variant digit of 8, 9, a or b; that the
/// same pair gives the same id and another scheduler or another acquire gives another. It
/// catches a request id that changes between retries or releases, or is shared by two
/// acquires.
#[test]
fn a_request_id_is_derived_from_the_scheduler_and_the_acquire() {
    let scheduler = SchedulerId::new("one").unwrap();
    let acquire = AcquireId::new("a1").unwrap();
    let derived = RequestId::derive(&acquire, &scheduler);
    assert_eq!(derived.as_str(), "1baffed8-0255-8930-8b60-e70b6aaccd36");
    let text = derived.as_str();
    assert_eq!(text.len(), 36);
    assert_eq!(text.as_bytes()[14], b'8');
    assert!("89ab".contains(text.as_bytes()[19] as char));
    for dash in [8, 13, 18, 23] {
        assert_eq!(text.as_bytes()[dash], b'-');
    }
    assert_eq!(derived, RequestId::derive(&acquire, &scheduler));
    let other_scheduler = SchedulerId::new("two").unwrap();
    assert_ne!(derived, RequestId::derive(&acquire, &other_scheduler));
    let other_acquire = AcquireId::new("a2").unwrap();
    assert_ne!(derived, RequestId::derive(&other_acquire, &scheduler));
}

/// Asserts that an operation's status parses from the three words the cloud uses and from
/// nothing else, and that an operation has failed only when it is done and carries an error
/// code: a running operation with a code has not finished, and a done one with none went
/// through. It catches an operation read as failed before it ends or as good with a code.
#[test]
fn an_operation_has_failed_only_when_it_is_done_with_an_error() {
    assert_eq!(
        OperationStatus::parse("PENDING"),
        Some(OperationStatus::Pending)
    );
    assert_eq!(
        OperationStatus::parse("RUNNING"),
        Some(OperationStatus::Running)
    );
    assert_eq!(OperationStatus::parse("DONE"), Some(OperationStatus::Done));
    assert_eq!(OperationStatus::parse("done"), None);
    assert_eq!(OperationStatus::parse(""), None);
    let operation = |status, codes: &[&str]| Operation {
        reference: OperationRef {
            project: Project::new(PROJECT).unwrap(),
            zone: zone(ZONE),
            name: "operation-1".to_owned(),
        },
        status,
        target_id: None,
        error_codes: codes
            .iter()
            .map(|code| js_provider_gce::ErrorReason::new(code))
            .collect(),
    };
    assert!(operation(OperationStatus::Done, &["QUOTA_EXCEEDED"]).failed());
    assert!(!operation(OperationStatus::Done, &[]).failed());
    assert!(!operation(OperationStatus::Running, &["QUOTA_EXCEEDED"]).failed());
    assert!(!operation(OperationStatus::Pending, &[]).failed());
}

/// Asserts that only an answer with the reason `notFound` is a not-found and only one with
/// `alreadyExists` is an already-exists, whatever else the error says; that the reasons
/// `authError` and `UNAUTHENTICATED` are denials; that a timeout and a missing credential
/// have no reason and are neither; and the words of those two errors. It catches "gone"
/// read from a timeout or from message text.
#[test]
fn only_the_clouds_reason_says_not_found_or_already_exists() {
    let missing = ApiError::answered("notFound");
    assert!(missing.is_not_found());
    assert!(!missing.is_already_exists());
    let taken = ApiError::answered("alreadyExists");
    assert!(taken.is_already_exists());
    assert!(!taken.is_not_found());
    for other in [
        ApiError::Timeout,
        ApiError::NoCredential,
        ApiError::Incomplete,
        ApiError::Transport("notFound".to_owned()),
        ApiError::Unreadable("alreadyExists".to_owned()),
        ApiError::answered("rateLimitExceeded"),
        ApiError::answered("forbidden"),
        ApiError::answered("backendError"),
    ] {
        assert!(!other.is_not_found(), "{other}");
        assert!(!other.is_already_exists(), "{other}");
    }
    assert_eq!(ApiError::Timeout.reason(), None);
    assert_eq!(ApiError::NoCredential.reason(), None);
    assert_eq!(ApiError::Timeout.to_string(), "the call timed out");
    assert_eq!(
        ApiError::NoCredential.to_string(),
        "no credential is available"
    );
    for reason in ["authError", "UNAUTHENTICATED"] {
        assert!(
            matches!(ApiError::answered(reason), ApiError::Denied { .. }),
            "{reason}"
        );
    }
}

/// Asserts the name of each launch call as the cloud spells its API method, as written by
/// `name` and by `Display`. It catches a method that is logged or matched under another
/// name.
#[test]
fn the_launch_calls_carry_the_clouds_method_names() {
    for (method, name) in [
        (Method::InsertInstance, "instances.insert"),
        (Method::GetInstance, "instances.get"),
        (Method::DeleteInstance, "instances.delete"),
        (Method::ListInstances, "instances.aggregatedList"),
        (Method::GetOperation, "zoneOperations.get"),
    ] {
        assert_eq!(method.name(), name);
        assert_eq!(method.to_string(), name);
    }
}
