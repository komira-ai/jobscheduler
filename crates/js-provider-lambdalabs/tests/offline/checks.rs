//! The live checks of the provider over the in-memory account: each one holds and fails
//! with the design's words, and none of them writes. The clock is the rig's manual one,
//! which no check reads: a check has no time of its own.

use std::num::NonZeroUsize;

use js_elastic::{CheckResult, ElasticProvider, SchedulerId};
use js_provider_lambdalabs::{ApiError, Call, ElasticConfig, ErrorCode, FakeLambda};

use crate::rig::{Rig, SCHEDULER, TABLE, config, rig, rig_with};

const RULESET: &str = "js-sched-one-noinbound";

fn account_key() -> ElasticConfig {
    ElasticConfig::with_account_key(
        SchedulerId::new(SCHEDULER).unwrap(),
        NonZeroUsize::new(5).unwrap(),
    )
}

/// The result named `name` among `results`.
fn named(results: &[CheckResult], name: &str) -> CheckResult {
    let found: Vec<_> = results.iter().filter(|r| r.name == name).collect();
    assert_eq!(found.len(), 1, "{name} in {results:?}");
    found[0].clone()
}

fn failed(name: &'static str, reason: &str) -> CheckResult {
    CheckResult::failed(name, reason)
}

fn api_error() -> ApiError {
    ApiError::Api {
        status: 403,
        code: ErrorCode::Forbidden,
        request_id: None,
    }
}

/// Counts of the two calls that write to the account.
fn writes(fake: &FakeLambda) -> (usize, usize) {
    (
        fake.count(Call::CreateSshKey),
        fake.count(Call::CreateFirewallRuleset),
    )
}

/// Asserts that a ready account passes all three checks, in the design's order, with no
/// note, and that the checks made list calls only. It catches a check that is missing from
/// the hook, one that fails a good account, and one that calls a write.
#[test]
fn a_ready_account_passes_every_check() {
    let rig = rig(config(5));
    rig.fake.add_ssh_key("fixture-key");
    rig.fake.add_firewall_ruleset(RULESET, "region-a", 0);
    rig.fake.add_firewall_ruleset(RULESET, "region-b", 0);
    assert_eq!(
        rig.provider.checks(),
        [
            CheckResult::held("types"),
            CheckResult::held("ssh_key"),
            CheckResult::held("firewall"),
        ]
    );
    assert_eq!(writes(&rig.fake), (0, 0));
}

/// Asserts that a `types` row that names no type of the catalog (a typo) is named with
/// the design's words, that a row whose GPU count the catalog contradicts is named too,
/// that a good row is not, and that a failed catalog call fails the check with the error.
/// It catches a typo that passes, a message without the row's name, and a good row blamed.
#[test]
fn a_typod_types_row_is_named() {
    let table = format!(
        "{TABLE}\n[[option]]\ninstance_type = \"gpu_9x_x\"\ngpu_class = \"nvidia-sample\"\ngpus = 9\n"
    );
    let rig = rig_with(config(5), &table);
    assert_eq!(
        named(&rig.provider.checks(), "types"),
        failed("types", "catalog: gpu_9x_x is not offered")
    );

    let table = TABLE.replace("gpus = 8", "gpus = 4");
    let rig = rig_with(config(5), &table);
    assert_eq!(
        named(&rig.provider.checks(), "types"),
        failed(
            "types",
            "catalog: gpu_8x_example has 8 gpus in the catalog, not 4"
        )
    );

    let rig = self::rig(config(5));
    rig.fake.fail_next(Call::InstanceTypes, api_error());
    let result = named(&rig.provider.checks(), "types");
    assert_eq!(
        result,
        failed("types", &format!("catalog: {}", api_error()))
    );
}

/// Asserts the key rule of a launch, read with a list only. A configured name must be on
/// the account; with none configured, `js-<scheduler>` wins among several, the only key
/// passes, several with none of our name fail with the design's words, and an account with
/// none passes with a note that a launch will make one while nothing is made. It catches
/// a rule that differs from `ssh_key`, a message that differs from the design's, and a
/// check that creates the key.
#[test]
fn the_ssh_key_check_applies_the_rule_of_a_launch_and_makes_nothing() {
    let ssh = |rig: &Rig| named(&rig.provider.checks(), "ssh_key");

    let rig = rig(config(5));
    assert_eq!(
        ssh(&rig),
        failed(
            "ssh_key",
            "provider.ssh_key: key fixture-key is not on the account"
        )
    );
    rig.fake.add_ssh_key("fixture-key");
    assert_eq!(ssh(&rig), CheckResult::held("ssh_key"));

    let rig = self::rig(account_key());
    assert_eq!(
        ssh(&rig),
        CheckResult::held_noting(
            "ssh_key",
            "account has 0 keys; a launch will create js-sched-one"
        )
    );
    assert!(rig.fake.ssh_keys().is_empty());
    assert_eq!(writes(&rig.fake), (0, 0));

    rig.fake.add_ssh_key("only");
    assert_eq!(ssh(&rig), CheckResult::held("ssh_key"));
    rig.fake.add_ssh_key("second");
    rig.fake.add_ssh_key("third");
    assert_eq!(
        ssh(&rig),
        failed("ssh_key", "provider.ssh_key: account has 3 keys; name one")
    );
    rig.fake.add_ssh_key("js-sched-one");
    assert_eq!(ssh(&rig), CheckResult::held("ssh_key"));

    let rig = self::rig(account_key());
    rig.fake.fail_next(Call::ListSshKeys, api_error());
    assert_eq!(
        ssh(&rig),
        failed("ssh_key", &format!("provider.ssh_key: {}", api_error()))
    );
    assert_eq!(writes(&rig.fake), (0, 0));
}

/// Asserts the firewall check per region. An absent ruleset passes with a note naming the
/// regions it will be made in; an empty one passes; one that holds rules fails, naming
/// the ruleset, the count and the region; a ruleset with rules in a region the pool does
/// not use is not looked at when regions are configured; a failed list fails the check.
/// Nothing is created. It catches a ruleset with rules passing, a check that creates the
/// ruleset, and one that reads a region outside the pool.
#[test]
fn the_firewall_check_lists_rulesets_per_region_and_makes_nothing() {
    let firewall = |rig: &Rig| named(&rig.provider.checks(), "firewall");

    let rig = rig(config(5));
    assert_eq!(
        firewall(&rig),
        CheckResult::held_noting(
            "firewall",
            format!("{RULESET} will be created in region-a, region-b")
        )
    );
    assert!(rig.fake.firewall_rulesets().is_empty());
    assert_eq!(writes(&rig.fake), (0, 0));

    rig.fake.add_firewall_ruleset(RULESET, "region-a", 0);
    assert_eq!(
        firewall(&rig),
        CheckResult::held_noting("firewall", format!("{RULESET} will be created in region-b"))
    );
    let id = rig.fake.add_firewall_ruleset(RULESET, "region-b", 0);
    assert_eq!(firewall(&rig), CheckResult::held("firewall"));

    rig.fake.set_firewall_rules(&id, 2);
    assert_eq!(
        firewall(&rig),
        failed(
            "firewall",
            &format!(
                "firewall: the ruleset {RULESET} holds 2 rules; it must hold none (region region-b)"
            )
        )
    );
    assert_eq!(writes(&rig.fake), (0, 0));

    let only_a = config(5).regions(&["region-a"]).unwrap();
    let rig = self::rig(only_a);
    rig.fake.add_firewall_ruleset(RULESET, "region-a", 0);
    rig.fake.add_firewall_ruleset(RULESET, "region-b", 3);
    assert_eq!(firewall(&rig), CheckResult::held("firewall"));

    let rig = self::rig(config(5));
    rig.fake.fail_next(Call::ListFirewallRulesets, api_error());
    assert_eq!(
        firewall(&rig),
        failed("firewall", &format!("firewall: {}", api_error()))
    );
}

/// Asserts that on an empty account, where every check has something to make, a pass of
/// the checks makes nothing: no key, no ruleset, no launch, and the only calls are reads.
/// It catches any check that writes.
#[test]
fn a_pass_of_checks_on_an_empty_account_only_reads() {
    let rig = rig(account_key());
    let results = rig.provider.checks();
    assert_eq!(results.len(), 3);
    assert!(results.iter().all(|r| r.failure.is_none()), "{results:?}");
    assert_eq!(writes(&rig.fake), (0, 0));
    assert_eq!(rig.fake.launched(), 0);
    assert!(rig.fake.ssh_keys().is_empty());
    assert!(rig.fake.firewall_rulesets().is_empty());
    assert!(rig.fake.calls().iter().all(|call| matches!(
        call,
        Call::InstanceTypes | Call::ListSshKeys | Call::ListFirewallRulesets
    )));
}

/// A writer into a buffer the test reads back.
#[derive(Clone, Default)]
struct Lines(std::sync::Arc<std::sync::Mutex<Vec<u8>>>);

impl std::io::Write for Lines {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(bytes);
        Ok(bytes.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for Lines {
    type Writer = Self;

    fn make_writer(&'a self) -> Self {
        self.clone()
    }
}

/// Asserts that the catalog leaves a typo'd `types` row out with a log line that names the
/// row, as the design says ("skipped with a log line"), and still returns the offers of
/// the good rows. It catches a row dropped without a trace.
#[test]
fn a_types_row_the_catalog_drops_is_logged_by_name() {
    let table = format!(
        "{TABLE}\n[[option]]\ninstance_type = \"gpu_9x_x\"\ngpu_class = \"nvidia-sample\"\ngpus = 9\n"
    );
    let rig = rig_with(config(5), &table);
    let lines = Lines::default();
    let subscriber = tracing_subscriber::fmt()
        .with_ansi(false)
        .with_max_level(tracing::Level::WARN)
        .with_writer(lines.clone())
        .finish();
    let offers = tracing::subscriber::with_default(subscriber, || rig.provider.catalog().unwrap());
    assert_eq!(offers.len(), 3);
    let text = String::from_utf8(lines.0.lock().unwrap().clone()).unwrap();
    assert!(text.contains("a types row is skipped"), "{text}");
    assert!(text.contains("gpu_9x_x is not offered"), "{text}");
}
