//! What a launch carries besides its type and name: the payload as user data, the
//! no-inbound firewall ruleset of its region, and one ssh key.

use std::num::NonZeroUsize;

use js_elastic::{
    Acquired, BootPayload, ElasticProvider, Reason, Refusal, RefusalCode, SchedulerId,
};
use js_provider_lambdalabs::{ApiError, Call, ElasticConfig, ErrorCode, PrepareError};

use crate::rig::{INTERVAL, KEY, SCHEDULER, ask, ask_as, candidate, live, rig};

fn refused(code: RefusalCode, reason: Reason) -> Acquired {
    Acquired::Refused(Refusal { code, reason })
}

fn accepted(answer: &Acquired) -> bool {
    matches!(answer, Acquired::Accepted { .. })
}

fn ruleset_name() -> String {
    format!("js-{SCHEDULER}-noinbound")
}

fn key_name() -> String {
    format!("js-{SCHEDULER}")
}

/// A live config that names no ssh key.
fn keyless() -> ElasticConfig {
    keyless_for(SCHEDULER)
}

fn keyless_for(scheduler: &str) -> ElasticConfig {
    ElasticConfig::with_account_key(
        SchedulerId::new(scheduler).unwrap(),
        NonZeroUsize::new(5).unwrap(),
    )
    .enable()
    .live()
    .launch_interval(INTERVAL)
}

fn down() -> ApiError {
    ApiError::Transport("down".to_owned())
}

#[test]
fn the_payload_is_delivered_as_user_data_and_an_empty_payload_sends_none() {
    let rig = rig(live(5));
    let mut request = ask("a");
    request.payload = BootPayload::new("#!/bin/sh\necho hello\n".to_owned());
    assert!(accepted(&rig.provider.acquire(&request)));
    rig.clock.advance(INTERVAL);
    assert!(accepted(&rig.provider.acquire(&ask("b"))));
    let sent = rig.fake.launches();
    assert_eq!(sent[0].user_data(), Some("#!/bin/sh\necho hello\n"));
    assert_eq!(sent[1].user_data(), None);
}

#[test]
fn every_launch_attaches_the_no_inbound_ruleset_of_its_own_region() {
    let rig = rig(live(5));
    assert!(accepted(&rig.provider.acquire(&ask("a"))));
    rig.clock.advance(INTERVAL);
    let mut elsewhere = ask("b");
    elsewhere.offer = candidate("region-b");
    assert!(accepted(&rig.provider.acquire(&elsewhere)));
    let sets = rig.fake.firewall_rulesets();
    assert_eq!(sets.len(), 2);
    assert_eq!(
        sets.iter().map(|s| s.region.as_str()).collect::<Vec<_>>(),
        ["region-a", "region-b"]
    );
    for set in &sets {
        assert_eq!(set.name, ruleset_name());
        assert_eq!(set.rules, 0);
    }
    assert_ne!(sets[0].id, sets[1].id);
    let sent = rig.fake.launches();
    assert_eq!(sent[0].firewall_ruleset_ids, [sets[0].id.clone()]);
    assert_eq!(sent[1].firewall_ruleset_ids, [sets[1].id.clone()]);
}

#[test]
fn a_ruleset_is_found_before_one_is_made_and_settled_once_per_region() {
    let rig = rig(live(5));
    let ours = rig
        .fake
        .add_firewall_ruleset(&ruleset_name(), "region-a", 0);
    // Decoys: our name in another region (holding rules), and another name here.
    rig.fake
        .add_firewall_ruleset(&ruleset_name(), "region-b", 3);
    rig.fake.add_firewall_ruleset("web", "region-a", 1);
    assert!(accepted(&rig.provider.acquire(&ask("a"))));
    rig.clock.advance(INTERVAL);
    assert!(accepted(&rig.provider.acquire(&ask("b"))));
    for sent in rig.fake.launches() {
        assert_eq!(sent.firewall_ruleset_ids, [ours.clone()]);
    }
    assert_eq!(rig.fake.count(Call::CreateFirewallRuleset), 0);
    assert_eq!(rig.fake.count(Call::ListFirewallRulesets), 1);
}

#[test]
fn a_ruleset_of_our_name_that_holds_rules_is_never_used() {
    let rig = rig(live(5));
    rig.fake
        .add_firewall_ruleset(&ruleset_name(), "region-a", 2);
    let text = format!(
        "firewall: the ruleset {} holds 2 rules; it must hold none",
        ruleset_name()
    );
    assert_eq!(
        rig.provider.acquire(&ask("a")),
        refused(RefusalCode::Denied, Reason::Firewall(text))
    );
    assert_eq!(rig.fake.count(Call::Launch), 0);
    assert_eq!(rig.fake.count(Call::CreateFirewallRuleset), 0);
    assert_eq!(
        rig.provider.ensure_firewall("region-a"),
        Err(PrepareError::FirewallRules {
            name: ruleset_name(),
            rules: 2
        })
    );
}

#[test]
fn a_failed_ruleset_call_sends_no_launch_and_a_retry_goes_through() {
    let rig = rig(live(5));
    rig.fake.fail_next(Call::ListFirewallRulesets, down());
    // No answer from the API about the ruleset: nothing was launched, so it is a wait, not
    // an unknown launch.
    assert_eq!(
        rig.provider.acquire(&ask("a")),
        Acquired::Deferred { wait: INTERVAL }
    );
    assert_eq!(rig.fake.count(Call::Launch), 0);
    assert!(accepted(&rig.provider.acquire(&ask("a"))));

    let rig = crate::rig::rig(live(5));
    rig.fake
        .fail_next(Call::CreateFirewallRuleset, api(403, ErrorCode::Forbidden));
    assert_eq!(
        rig.provider.acquire(&ask("a")),
        refused(
            RefusalCode::Denied,
            Reason::Api("global/forbidden".to_owned())
        )
    );
    assert_eq!(rig.fake.count(Call::Launch), 0);

    let rig = crate::rig::rig(live(5));
    rig.fake
        .fail_next(Call::ListFirewallRulesets, ApiError::Incomplete);
    assert_eq!(
        rig.provider.ensure_firewall("region-a"),
        Err(PrepareError::Api(ApiError::Incomplete))
    );
}

fn api(status: u16, code: ErrorCode) -> ApiError {
    ApiError::Api {
        status,
        code,
        request_id: None,
    }
}

#[test]
fn a_scheduler_id_too_long_for_the_ruleset_name_is_a_shape_refusal_that_calls_nothing() {
    let long = "s".repeat(52);
    let config = ElasticConfig::new(
        SchedulerId::new(&long).unwrap(),
        KEY,
        NonZeroUsize::new(5).unwrap(),
    )
    .unwrap()
    .enable()
    .live();
    let rig = rig(config);
    assert_eq!(
        rig.provider.acquire(&ask_as(&long, "a")),
        refused(
            RefusalCode::Shape,
            Reason::Firewall(
                "firewall: the scheduler id is too long for a ruleset name of 64 characters"
                    .to_owned()
            )
        )
    );
    assert!(rig.fake.calls().is_empty());
}

#[test]
fn a_dry_run_makes_no_ruleset_and_no_key() {
    let dry = ElasticConfig::with_account_key(
        SchedulerId::new(SCHEDULER).unwrap(),
        NonZeroUsize::new(5).unwrap(),
    )
    .enable();
    let rig = rig(dry);
    assert_eq!(
        rig.provider.acquire(&ask("a")),
        refused(RefusalCode::Denied, Reason::DryRun)
    );
    assert!(rig.fake.calls().is_empty());
    assert!(rig.fake.firewall_rulesets().is_empty());
    assert!(rig.fake.ssh_keys().is_empty());
}

#[test]
fn a_named_key_is_used_as_it_is_without_asking_the_account() {
    let rig = rig(live(5));
    assert!(accepted(&rig.provider.acquire(&ask("a"))));
    assert_eq!(rig.fake.launches()[0].ssh_key, KEY);
    assert_eq!(rig.fake.count(Call::ListSshKeys), 0);
    assert_eq!(rig.provider.ssh_key(), Ok(KEY.to_owned()));
}

#[test]
fn the_accounts_only_key_is_used_and_asked_for_once() {
    let rig = rig(keyless());
    rig.fake.add_ssh_key("mine");
    assert!(accepted(&rig.provider.acquire(&ask("a"))));
    rig.clock.advance(INTERVAL);
    assert!(accepted(&rig.provider.acquire(&ask("b"))));
    for sent in rig.fake.launches() {
        assert_eq!(sent.ssh_key, "mine");
    }
    assert_eq!(rig.fake.count(Call::ListSshKeys), 1);
    assert_eq!(rig.fake.count(Call::CreateSshKey), 0);
}

#[test]
fn among_several_keys_the_one_named_for_the_scheduler_is_used() {
    let rig = rig(keyless());
    for name in ["alpha", &key_name(), "beta"] {
        rig.fake.add_ssh_key(name);
    }
    assert!(accepted(&rig.provider.acquire(&ask("a"))));
    assert_eq!(rig.fake.launches()[0].ssh_key, key_name());
    assert_eq!(rig.fake.count(Call::CreateSshKey), 0);
}

#[test]
fn an_account_with_no_key_gets_one_named_for_the_scheduler_once() {
    let rig = rig(keyless());
    assert!(accepted(&rig.provider.acquire(&ask("a"))));
    rig.clock.advance(INTERVAL);
    assert!(accepted(&rig.provider.acquire(&ask("b"))));
    let keys = rig.fake.ssh_keys();
    assert_eq!(keys.len(), 1);
    assert_eq!(keys[0].name, key_name());
    for sent in rig.fake.launches() {
        assert_eq!(sent.ssh_key, key_name());
    }
    assert_eq!(rig.fake.count(Call::ListSshKeys), 1);
    assert_eq!(rig.fake.count(Call::CreateSshKey), 1);
}

#[test]
fn several_keys_and_none_to_pick_is_refused_until_one_is_named_and_is_not_remembered() {
    let rig = rig(keyless());
    rig.fake.add_ssh_key("alpha");
    rig.fake.add_ssh_key("beta");
    assert_eq!(
        rig.provider.acquire(&ask("a")),
        refused(
            RefusalCode::Denied,
            Reason::SshKey("provider.ssh_key: account has 2 keys; name one".to_owned())
        )
    );
    assert_eq!(rig.fake.count(Call::Launch), 0);
    assert_eq!(rig.fake.count(Call::CreateSshKey), 0);
    assert_eq!(
        rig.provider.ssh_key(),
        Err(PrepareError::SshKeys { count: 2 })
    );
    // The operator makes the key; the next acquire finds it.
    rig.fake.add_ssh_key(&key_name());
    assert!(accepted(&rig.provider.acquire(&ask("a"))));
    assert_eq!(rig.fake.launches()[0].ssh_key, key_name());
}

#[test]
fn a_failed_key_list_is_a_wait_and_launches_nothing() {
    let rig = rig(keyless());
    rig.fake.fail_next(Call::ListSshKeys, down());
    assert_eq!(
        rig.provider.acquire(&ask("a")),
        Acquired::Deferred { wait: INTERVAL }
    );
    assert_eq!(rig.fake.count(Call::Launch), 0);
    assert_eq!(rig.fake.count(Call::CreateFirewallRuleset), 0);
}

#[test]
fn a_scheduler_id_too_long_to_name_a_key_makes_none_and_still_uses_the_only_key() {
    let long = "s".repeat(62);
    let rig = rig(keyless_for(&long));
    assert_eq!(rig.provider.ssh_key(), Err(PrepareError::KeyName));
    assert_eq!(rig.fake.count(Call::CreateSshKey), 0);
    rig.fake.add_ssh_key("only");
    assert_eq!(rig.provider.ssh_key(), Ok("only".to_owned()));
}

#[test]
fn the_catalog_and_the_launch_keep_to_the_configured_regions() {
    let rig = rig(live(5).regions(&["region-a"]).unwrap());
    let zones: Vec<String> = rig
        .provider
        .catalog()
        .unwrap()
        .into_iter()
        .filter_map(|offer| offer.zone)
        .collect();
    assert_eq!(zones, ["region-a", "region-a"]);
    let mut elsewhere = ask("a");
    elsewhere.offer = candidate("region-b");
    assert_eq!(
        rig.provider.acquire(&elsewhere),
        refused(RefusalCode::Shape, Reason::NotAllowed)
    );
    assert_eq!(rig.fake.count(Call::ListFirewallRulesets), 0);
    assert_eq!(rig.fake.count(Call::Launch), 0);
}

#[test]
fn a_failed_key_creation_is_a_wait_and_launches_nothing() {
    let rig = rig(keyless());
    rig.fake.fail_next(Call::CreateSshKey, down());
    assert_eq!(
        rig.provider.acquire(&ask("a")),
        Acquired::Deferred { wait: INTERVAL }
    );
    assert_eq!(rig.fake.count(Call::CreateSshKey), 1);
    assert_eq!(rig.fake.count(Call::Launch), 0);
    // Nothing was kept from the failure: the next acquire makes the key and launches.
    assert!(accepted(&rig.provider.acquire(&ask("a"))));
    assert_eq!(rig.fake.ssh_keys().len(), 1);
}

#[test]
fn every_prepare_error_says_what_to_do() {
    let texts = [
        (
            PrepareError::Api(ApiError::Incomplete),
            ApiError::Incomplete.to_string(),
        ),
        (
            PrepareError::FirewallName,
            "firewall: the scheduler id is too long for a ruleset name of 64 characters".to_owned(),
        ),
        (
            PrepareError::FirewallRules {
                name: "n".to_owned(),
                rules: 4,
            },
            "firewall: the ruleset n holds 4 rules; it must hold none".to_owned(),
        ),
        (
            PrepareError::KeyName,
            "provider.ssh_key: the scheduler id is too long to name a key; name one".to_owned(),
        ),
        (
            PrepareError::SshKeys { count: 3 },
            "provider.ssh_key: account has 3 keys; name one".to_owned(),
        ),
    ];
    for (error, text) in texts {
        assert_eq!(error.to_string(), text);
    }
}
