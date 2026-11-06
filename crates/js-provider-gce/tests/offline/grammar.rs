//! The cloud's grammar for names and labels: what the provider derives from a scheduler id
//! and an acquire id always fits it, and two ids never share a label value.

use std::collections::BTreeSet;
use std::num::NonZeroUsize;
use std::sync::Arc;

use js_elastic::{Acquired, CapacityProvider, SchedulerId};
use js_provider_gce::{
    GceConfig, InstanceName, Project, is_label_key, is_label_value, is_resource_name, label_value,
};

use crate::rig::{
    Account, PROJECT, ask, ask_as, images, live, marks, name_of, network, rig, rig_over,
};

/// The config of a live provider for the scheduler `scheduler`, with room for `machines`.
fn live_for(scheduler: &str, machines: usize) -> GceConfig {
    GceConfig::new(
        SchedulerId::new(scheduler).unwrap(),
        Project::new(PROJECT).unwrap(),
        images(),
        network(),
        NonZeroUsize::new(machines).unwrap(),
    )
    .enable()
    .live()
}

/// Asserts that for scheduler ids and acquire ids of every length from one character to the
/// longest an id has, and of every character an id may hold (upper case, `.`, `_`, `-` and a
/// digit first), the name of the insert is a name of the cloud's grammar, every label key
/// and value of it is one of the label grammar, the labels are the marks of the scheduler
/// and the acquire, and a second acquire of the same id finds the instance again by those
/// labels and inserts nothing. It catches an id written into a name or label as it is, which
/// the cloud would refuse, and a lookup that cannot find what the insert labelled.
#[test]
fn the_name_and_labels_fit_the_cloud_grammar() {
    let long_scheduler = "x".repeat(64);
    let upper_scheduler = "Y".repeat(64);
    let schedulers = [
        "s",
        "A",
        "9lives",
        "Sched.One_two-three",
        long_scheduler.as_str(),
        upper_scheduler.as_str(),
    ];
    let prefix_63 = "q".repeat(63);
    let prefix_64 = "q".repeat(64);
    let longest = "q".repeat(128);
    let upper_longest = "Z".repeat(128);
    let digits = "0".repeat(128);
    let acquires = [
        "a",
        "9",
        "A",
        "Up.per_Case-1",
        prefix_63.as_str(),
        prefix_64.as_str(),
        longest.as_str(),
        upper_longest.as_str(),
        digits.as_str(),
    ];
    for scheduler in schedulers {
        for acquire in acquires {
            let lab = rig_over(Arc::new(Account::new()), live_for(scheduler, 1));
            let request = ask_as(scheduler, acquire);
            let first = lab.provider.acquire(&request);
            let Acquired::Accepted { held, .. } = first else {
                panic!("{scheduler} {acquire}: not accepted: {first:?}");
            };
            let inserts = lab.account.inserts();
            let insert = &inserts[0];
            assert!(is_resource_name(insert.name.as_str()), "{}", insert.name);
            assert_eq!(insert.name.as_str(), name_of(scheduler, acquire));
            assert_eq!(insert.labels, marks(scheduler, acquire));
            for label in &insert.labels {
                assert!(is_label_key(&label.key), "{}", label.key);
                assert!(is_label_value(&label.value), "{}", label.value);
            }
            let Acquired::Accepted { held: again, .. } = lab.provider.acquire(&request) else {
                panic!("{scheduler} {acquire}: the repeat is not accepted");
            };
            assert_eq!(again.id, held.id, "{scheduler} {acquire}");
            assert_eq!(lab.account.inserted(), 1, "{scheduler} {acquire}");
        }
    }
}

/// Asserts that the label value of an id that is at most 63 lower-case letters, digits and
/// `-` is the id; that any other id (upper case, `.`, `_`, 64 characters or more) has a
/// value that starts with `_`, is 63 characters, and is of the label grammar; that no two
/// of a set of ids that differ only in case, in their last character past a shared 63
/// character prefix, or in being the hashed text of another share a value; and that the
/// value of a value is a third value. It catches a hash cut so short that long ids collide,
/// and a plain value that could be mistaken for a hashed one.
#[test]
fn label_values_stay_distinct_per_id() {
    for plain in ["a", "a1", "abc-123", "9", "-", "a".repeat(63).as_str()] {
        assert_eq!(label_value(plain), plain);
    }
    let hashed_of_a1 = label_value("A1");
    let ids = [
        "a1".to_owned(),
        "A1".to_owned(),
        "a.1".to_owned(),
        "a_1".to_owned(),
        "a".repeat(63),
        "a".repeat(64),
        "a".repeat(128),
        format!("{}1", "p".repeat(63)),
        format!("{}2", "p".repeat(63)),
        hashed_of_a1.clone(),
        label_value(&hashed_of_a1),
    ];
    let values: BTreeSet<String> = ids.iter().map(|id| label_value(id)).collect();
    assert_eq!(values.len(), ids.len(), "{values:?}");
    for id in &ids[2..] {
        let value = label_value(id);
        assert!(is_label_value(&value), "{value}");
        if id != &"a".repeat(63) {
            assert!(value.starts_with('_'), "{id}: {value}");
            assert_eq!(value.len(), 63, "{id}: {value}");
        }
    }
    assert_ne!(label_value(&hashed_of_a1), hashed_of_a1);
}

/// Asserts that acquires whose ids differ only in case, and one whose id is the hashed text
/// another id has as its label value, are three acquires: three instances are inserted, each
/// with a name of its own, and a list shows all three. It catches a lookup that takes one
/// acquire's instance for another's.
#[test]
fn ids_that_differ_only_in_case_are_different_acquires() {
    let lab = rig(live(5));
    let hashed = label_value("A1");
    for acquire in ["a1", "A1", hashed.as_str()] {
        let request = ask(acquire);
        assert!(
            matches!(lab.provider.acquire(&request), Acquired::Accepted { .. }),
            "{acquire}"
        );
    }
    assert_eq!(lab.account.inserted(), 3);
    let listed = lab.provider.list().unwrap();
    let names: BTreeSet<Option<String>> = listed.into_iter().map(|held| held.name).collect();
    assert_eq!(names.len(), 3);
}

/// Asserts that two schedulers whose ids differ only in case, working in one project, each
/// launch their own instance for the acquire `a1` and list only their own. It catches a
/// scheduler label that two ids share.
#[test]
fn schedulers_that_differ_only_in_case_do_not_see_each_other() {
    let account = Arc::new(Account::new());
    let lower = rig_over(Arc::clone(&account), live_for("sched", 2));
    let upper = rig_over(Arc::clone(&account), live_for("SCHED", 2));
    for (lab, scheduler) in [(&lower, "sched"), (&upper, "SCHED")] {
        assert!(matches!(
            lab.provider.acquire(&ask_as(scheduler, "a1")),
            Acquired::Accepted { .. }
        ));
    }
    assert_eq!(account.inserted(), 2);
    for (lab, scheduler) in [(&lower, "sched"), (&upper, "SCHED")] {
        let listed = lab.provider.list().unwrap();
        assert_eq!(listed.len(), 1, "{scheduler}");
        assert_eq!(listed[0].name, Some(name_of(scheduler, "a1")));
    }
}

/// Asserts the edges of the three grammars: a label value is at most 63 lower-case letters,
/// digits, `_` and `-`, and may be empty; a label key is the same but starts with a letter
/// and is not empty; a resource name is 1 to 63 lower-case letters, digits and `-`, starts
/// with a letter and does not end with `-`; and an instance name is a resource name. It
/// catches a bound that is off by one and a character class that is too wide.
#[test]
fn the_grammar_predicates_hold_at_their_edges() {
    let long = "a".repeat(63);
    let too_long = "a".repeat(64);
    for (text, value, key, name) in [
        ("", true, false, false),
        ("a", true, true, true),
        ("a1", true, true, true),
        ("1a", true, false, false),
        ("_a", true, false, false),
        ("-a", true, false, false),
        ("a-", true, true, false),
        ("a_b", true, true, false),
        ("a--b", true, true, true),
        ("a-b_c9", true, true, false),
        ("A", false, false, false),
        ("aB", false, false, false),
        ("a.b", false, false, false),
        ("a b", false, false, false),
        ("é", false, false, false),
        (long.as_str(), true, true, true),
        (too_long.as_str(), false, false, false),
    ] {
        assert_eq!(is_label_value(text), value, "value {text:?}");
        assert_eq!(is_label_key(text), key, "key {text:?}");
        assert_eq!(is_resource_name(text), name, "name {text:?}");
        assert_eq!(InstanceName::new(text).is_some(), name, "instance {text:?}");
    }
    assert_eq!(InstanceName::new("a-b").unwrap().as_str(), "a-b");
    assert_eq!(InstanceName::new("a-b").unwrap().to_string(), "a-b");
}
