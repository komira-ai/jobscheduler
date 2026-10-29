//! Ownership marks, tags made from labels, and the ids.

use js_elastic::{
    Acquired, Claim, ElasticProvider, HostStatus, Reason, RefusalCode, Released, SchedulerId,
};
use js_provider_lambdalabs::{ACQUIRE_TAG, LabelError, MAX_TAGS, POOL_TAG, SCHEDULER_TAG};

use crate::rig::{SCHEDULER, ask, ask_as, host, live, rig, tag};

/// What the fake holds after one live launch with `labels`.
fn launched_with(labels: &[(&str, &str)], pool: &str) -> Result<Vec<(String, String)>, String> {
    let rig = rig(live(3));
    let mut request = ask("acq-1");
    request.pool = pool.to_owned();
    request.labels = labels
        .iter()
        .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
        .collect();
    match rig.provider.acquire(&request) {
        Acquired::Accepted { .. } => Ok(rig.fake.instances()[0]
            .tags
            .iter()
            .map(|t| (t.key.clone(), t.value.clone()))
            .collect()),
        Acquired::Refused(refusal) => match refusal.reason {
            Reason::Label(why) => {
                assert_eq!(rig.fake.launched(), 0, "a refused label launched something");
                Err(why)
            }
            other => panic!("refused for {other:?}"),
        },
        other => panic!("{other:?}"),
    }
}

#[test]
fn a_launch_is_tagged_with_scheduler_acquire_and_pool_and_one_tag_per_label() {
    let tags = launched_with(&[("team", "blue"), ("tier-2", "x")], "pool-a").unwrap();
    assert_eq!(
        tags,
        [
            (SCHEDULER_TAG.to_owned(), SCHEDULER.to_owned()),
            (ACQUIRE_TAG.to_owned(), "acq-1".to_owned()),
            (POOL_TAG.to_owned(), "pool-a".to_owned()),
            ("jobscheduler:team".to_owned(), "blue".to_owned()),
            ("jobscheduler:tier-2".to_owned(), "x".to_owned()),
        ]
    );
}

#[test]
fn the_name_is_a_prefix_per_scheduler_and_a_hash_per_acquire() {
    let one = rig(live(3));
    let two = rig(js_provider_lambdalabs::ElasticConfig::new(
        SchedulerId::new("sched-two").unwrap(),
        "k",
        std::num::NonZeroUsize::new(3).unwrap(),
    )
    .unwrap()
    .enable()
    .live()
    .launch_interval(crate::rig::INTERVAL));
    for (r, scheduler) in [(&one, SCHEDULER), (&two, "sched-two")] {
        r.provider.acquire(&ask_as(scheduler, "acq-1"));
        r.clock.advance(crate::rig::INTERVAL);
        r.provider.acquire(&ask_as(scheduler, "acq-2"));
    }
    let name = |r: &crate::rig::Rig, n: usize| r.fake.instances()[n].name.clone().unwrap();
    let (a1, a2, b1) = (name(&one, 0), name(&one, 1), name(&two, 0));
    assert!(a1.starts_with("js-") && a1.len() == 3 + 12 + 1 + 12, "{a1}");
    assert_eq!(a1[..16], a2[..16], "one scheduler, one prefix");
    assert_ne!(a1[16..], a2[16..], "two acquires, two names");
    assert_ne!(a1[..16], b1[..16], "two schedulers, two prefixes");
    assert_eq!(a1[16..], b1[16..], "the suffix depends on the acquire only");
    // The instance carries the name the request gave it.
    assert_eq!(a1, ask("acq-1").name.as_str());
}

#[test]
fn a_host_name_without_this_schedulers_prefix_is_refused_and_nothing_is_launched() {
    let rig = rig(live(3));
    // Named by another scheduler: an instance launched under it would not be recognised
    // as ours afterwards, and the cost guard would not count it.
    let request = ask_as("sched-two", "acq-1");
    let Acquired::Refused(refusal) = rig.provider.acquire(&request) else {
        panic!("a foreign name was launched");
    };
    assert_eq!(refusal.code, RefusalCode::Shape);
    assert_eq!(refusal.reason, Reason::Name);
    assert_eq!(rig.fake.launched(), 0);
}

#[test]
fn labels_that_could_collide_or_overflow_are_refused_not_rewritten() {
    let key = |k: &str| LabelError::Key(k.to_owned()).to_string();
    // a.b and a-b must not both become jobscheduler:a-b.
    assert_eq!(launched_with(&[("a.b", "x")], "p"), Err(key("a.b")));
    assert_eq!(launched_with(&[("A", "x")], "p"), Err(key("A")));
    assert_eq!(launched_with(&[("", "x")], "p"), Err(key("")));
    assert_eq!(launched_with(&[("1a", "x")], "p"), Err(key("1a")));
    assert_eq!(launched_with(&[("a_b", "x")], "p"), Err(key("a_b")));
    let long = "a".repeat(43);
    assert_eq!(launched_with(&[(&long, "x")], "p"), Err(key(&long)));
    let longest = "a".repeat(42);
    assert!(launched_with(&[(&longest, "x")], "p").is_ok());
    for reserved in ["scheduler", "acquire", "pool"] {
        assert_eq!(
            launched_with(&[(reserved, "x")], "p"),
            Err(LabelError::Reserved(reserved.to_owned()).to_string())
        );
    }
    assert_eq!(
        launched_with(&[("a", "x"), ("a", "y")], "p"),
        Err(LabelError::Twice("a".to_owned()).to_string())
    );
    assert_eq!(
        launched_with(&[("a", "")], "p"),
        Err(LabelError::Value("a".to_owned()).to_string())
    );
    assert_eq!(
        launched_with(&[("a", "has space")], "p"),
        Err(LabelError::Value("a".to_owned()).to_string())
    );
    assert_eq!(
        launched_with(&[("a", &"v".repeat(129))], "p"),
        Err(LabelError::Value("a".to_owned()).to_string())
    );
    assert!(launched_with(&[("a", &"v".repeat(128))], "p").is_ok());
    assert_eq!(
        launched_with(&[], ""),
        Err(LabelError::Value("pool".to_owned()).to_string())
    );
}

#[test]
fn the_number_of_tags_is_capped() {
    let labels: Vec<(String, String)> = (0..MAX_TAGS - 2)
        .map(|n| (format!("k{n}"), "v".to_owned()))
        .collect();
    let pairs = |n: usize| -> Vec<(&str, &str)> {
        labels[..n]
            .iter()
            .map(|(k, v)| (k.as_str(), v.as_str()))
            .collect()
    };
    assert!(launched_with(&pairs(MAX_TAGS - 3), "p").is_ok());
    assert_eq!(
        launched_with(&pairs(MAX_TAGS - 2), "p"),
        Err(LabelError::TooMany(MAX_TAGS + 1).to_string())
    );
    assert!(LabelError::TooMany(17).to_string().contains("17"));
}

/// What the provider makes of an instance with these marks: the answer of `list` (ours
/// only) and of a release (refused with the claim).
fn claim_of(name: Option<&str>, tags: Vec<js_provider_lambdalabs::Tag>) -> Claim {
    let rig = rig(live(3));
    // The name the provider would give acquire "x".
    rig.provider.acquire(&ask("x"));
    let own_name = rig.fake.instances()[0].name.clone().unwrap();
    let name = name.map(|n| n.replace("{own}", &own_name));
    rig.fake.update_instance(&crate::rig::id("fake0001"), &|i| {
        i.name = name.clone();
        i.tags = tags.clone();
    });
    match rig.provider.release(&host("fake0001")) {
        Released::Refused(claim) => claim,
        Released::Terminating(HostStatus::Terminating) => Claim::Ours,
        other => panic!("{other:?}"),
    }
}

#[test]
fn an_instance_is_ours_only_with_the_scheduler_tag_an_acquire_tag_and_the_name_prefix() {
    let ours = || vec![tag(SCHEDULER_TAG, SCHEDULER), tag(ACQUIRE_TAG, "x")];
    let cases: Vec<(&str, Option<&str>, Vec<js_provider_lambdalabs::Tag>, Claim)> = vec![
        ("all three", Some("{own}"), ours(), Claim::Ours),
        ("no name", None, ours(), Claim::Suspect),
        ("renamed", Some("renamed"), ours(), Claim::Suspect),
        (
            "no acquire tag",
            Some("{own}"),
            vec![tag(SCHEDULER_TAG, SCHEDULER)],
            Claim::Suspect,
        ),
        (
            "no scheduler tag",
            Some("{own}"),
            vec![tag(ACQUIRE_TAG, "x")],
            Claim::Suspect,
        ),
        ("no tags, our name", Some("{own}"), vec![], Claim::Suspect),
        (
            "scheduler tag only",
            Some("renamed"),
            vec![tag(SCHEDULER_TAG, SCHEDULER)],
            Claim::Suspect,
        ),
        (
            "acquire tag only",
            Some("renamed"),
            vec![tag(ACQUIRE_TAG, "x")],
            Claim::Suspect,
        ),
        (
            "another scheduler's tag, our name",
            Some("{own}"),
            vec![tag(SCHEDULER_TAG, "sched-two"), tag(ACQUIRE_TAG, "x")],
            Claim::Suspect,
        ),
        (
            "another scheduler's tags and name",
            Some("js-ffffffffffff-000000000000"),
            vec![tag(SCHEDULER_TAG, "sched-two"), tag(ACQUIRE_TAG, "x")],
            Claim::Foreign,
        ),
        ("nothing", Some("someone"), vec![], Claim::Foreign),
        ("no name, no tags", None, vec![], Claim::Foreign),
    ];
    for (what, name, tags, want) in cases {
        assert_eq!(claim_of(name, tags), want, "{what}");
    }
}
