//! The provider's own live checks in a running server: the Lambda provider over the
//! in-memory account, behind the pool's check loop, with time a manual clock. Nothing
//! reaches a real cloud, and no check writes to the account.

use std::num::NonZeroUsize;
use std::sync::Arc;

use js_elastic::{ElasticProvider, SchedulerId};
use js_provider_lambdalabs::{
    Architecture, Call, ElasticConfig, FakeLambda, InstanceTypeOffer, LambdaProvider, ManualClock,
    OptionTable, Region,
};
use serde_json::{Value, json};

use crate::pools_support::{FakeCloud, Options, Rig};

const TABLE: &str =
    "[[option]]\ninstance_type = \"gpu_1x_a10\"\ngpu_class = \"nvidia-a10\"\ngpus = 1\n";
const RULESET: &str = "js-sched-noinbound";

/// A server whose pool is the Lambda provider over an account that offers one A10 in one
/// region, with `table` as its `types` rows.
struct Account {
    fake: Arc<FakeLambda>,
    rig: Rig,
}

fn config() -> ElasticConfig {
    ElasticConfig::with_account_key(
        SchedulerId::new("sched").unwrap(),
        NonZeroUsize::new(4).unwrap(),
    )
}

async fn start(name: &str, table: &str) -> Account {
    let fake = Arc::new(FakeLambda::new());
    fake.set_offers(vec![InstanceTypeOffer {
        name: "gpu_1x_a10".to_owned(),
        description: String::new(),
        gpu_description: String::new(),
        price_cents_per_hour: 129,
        vcpus: 30,
        memory_gib: 200,
        storage_gib: 512,
        gpus: 1,
        architecture: Architecture::X86_64,
        regions: vec![Region {
            name: "us-west-1".to_owned(),
            description: String::new(),
        }],
    }]);
    let provider = LambdaProvider::new(
        fake.clone(),
        Arc::new(ManualClock::new()),
        OptionTable::parse(table).unwrap(),
        config(),
    );
    let cloud = FakeCloud::new(Vec::new());
    let provider: Arc<dyn ElasticProvider> = Arc::new(provider);
    cloud.delegate(provider);
    let rig = Rig::start_over(name, cloud, Options::default()).await;
    Account { fake, rig }
}

/// The check `name` of the pool, from `provider_checks`.
fn check(pool: &Value, name: &str) -> Value {
    let all = pool["provider_checks"].as_array().unwrap();
    let found: Vec<&Value> = all.iter().filter(|c| c["name"] == name).collect();
    assert_eq!(found.len(), 1, "{name} in {all:?}");
    found[0].clone()
}

/// Asserts that a ruleset that holds a rule makes the pool unavailable on its first pass,
/// before any acquire: the reason is the design's `firewall: ...` line, the firewall
/// check shows `ok: false`, the other two hold, the pool is `blocked`, and nothing is
/// launched or made. Emptying the ruleset makes the pool available at the retry 10 s
/// later, by the injected clock. It catches a ruleset with rules that only shows on the
/// first launch, and a failure that never clears.
#[tokio::test]
async fn a_ruleset_with_a_rule_makes_the_pool_unavailable_before_any_acquire() {
    let account = start("provider_checks_rule", TABLE).await;
    account.fake.add_ssh_key("only");
    let id = account.fake.add_firewall_ruleset(RULESET, "us-west-1", 1);
    let rig = &account.rig;

    rig.step().await;
    let pool = rig.pool().await;
    let reason = format!(
        "firewall: the ruleset {RULESET} holds 1 rules; it must hold none (region us-west-1)"
    );
    assert_eq!(pool["state"], "unavailable");
    assert_eq!(pool["blocked"], "unavailable");
    assert_eq!(pool["reasons"], json!([reason]));
    assert_eq!(check(&pool, "firewall")["ok"], false);
    assert_eq!(check(&pool, "firewall")["reason"], reason);
    assert_eq!(check(&pool, "types")["ok"], true);
    assert_eq!(check(&pool, "ssh_key")["ok"], true);
    assert_eq!(account.fake.launched(), 0);
    assert_eq!(account.fake.count(Call::Launch), 0);

    account.fake.set_firewall_rules(&id, 0);
    rig.advance(10);
    rig.step().await;
    let pool = rig.pool().await;
    assert_eq!(pool["state"], "available");
    assert_eq!(check(&pool, "firewall")["ok"], true);
    assert_eq!(pool["reasons"], json!([]));
    rig.assert_clean();
}

/// Asserts that a typo'd `types` row is named in the pool's reasons and on its check, in
/// the design's words, and that the pool stays unavailable until the row is right. It
/// catches a typo the pool silently ignores.
#[tokio::test]
async fn a_typod_types_row_is_named_in_the_pool_reasons() {
    let table = format!(
        "{TABLE}[[option]]\ninstance_type = \"gpu_9x_x\"\ngpu_class = \"nvidia-a10\"\ngpus = 9\n"
    );
    let account = start("provider_checks_typo", &table).await;
    account.fake.add_ssh_key("only");
    let rig = &account.rig;
    rig.step().await;
    let pool = rig.pool().await;
    let reason = "catalog: gpu_9x_x is not offered";
    assert_eq!(pool["state"], "unavailable");
    assert_eq!(pool["reasons"], json!([reason]));
    assert_eq!(check(&pool, "types")["reason"], reason);
    rig.assert_clean();
}

/// Asserts the ssh key check in a running server. An account with three keys and none of
/// ours makes the pool unavailable with `provider.ssh_key: account has 3 keys; name one`;
/// when it has none, the pool is available, the check says a launch will create the key
/// and the firewall check says the ruleset will be made, on the JSON and on the page, and
/// the account is left as it was: no key, no ruleset, no create call. It catches a check
/// that makes what a launch would make, and a note that does not reach the status.
#[tokio::test]
async fn the_ssh_key_and_firewall_checks_show_and_make_nothing() {
    let account = start("provider_checks_key", TABLE).await;
    for key in ["a", "b", "c"] {
        account.fake.add_ssh_key(key);
    }
    let rig = &account.rig;
    rig.step().await;
    let pool = rig.pool().await;
    let reason = "provider.ssh_key: account has 3 keys; name one";
    assert_eq!(pool["state"], "unavailable");
    assert_eq!(pool["reasons"], json!([reason]));
    assert_eq!(check(&pool, "ssh_key")["reason"], reason);
    assert_eq!(check(&pool, "ssh_key")["note"], Value::Null);
    let page = rig.get("/status").await.body;
    assert!(page.contains("check ssh_key"), "{page}");
    assert!(page.contains(reason), "{page}");
    assert_eq!(account.fake.count(Call::CreateSshKey), 0);

    let empty = start("provider_checks_none", TABLE).await;
    let rig = &empty.rig;
    rig.step().await;
    let pool = rig.pool().await;
    assert_eq!(pool["state"], "available");
    let key_note = "account has 0 keys; a launch will create js-sched";
    let fire_note = format!("{RULESET} will be created in us-west-1");
    assert_eq!(check(&pool, "ssh_key")["note"], key_note);
    assert_eq!(check(&pool, "firewall")["note"], fire_note);
    let page = rig.get("/status").await.body;
    assert!(
        page.contains(&format!("ok at 1000000 ms: {key_note}")),
        "{page}"
    );
    assert!(page.contains(&fire_note), "{page}");
    assert!(empty.fake.ssh_keys().is_empty());
    assert!(empty.fake.firewall_rulesets().is_empty());
    assert_eq!(empty.fake.count(Call::CreateSshKey), 0);
    assert_eq!(empty.fake.count(Call::CreateFirewallRuleset), 0);
    assert_eq!(empty.fake.launched(), 0);
    rig.assert_clean();
}

/// Asserts that a pool whose provider has no checks of its own (the default of the hook)
/// shows an empty list and passes on the catalog alone, as before.
#[tokio::test]
async fn a_provider_with_no_checks_of_its_own_shows_none() {
    let rig = Rig::start(
        "provider_checks_default",
        vec![crate::pools_support::a10("gpu_1x_a10", 1, 129)],
        Options::default(),
    )
    .await;
    rig.step().await;
    let pool = rig.pool().await;
    assert_eq!(pool["state"], "available");
    assert_eq!(pool["provider_checks"], json!([]));
    rig.assert_clean();
}
