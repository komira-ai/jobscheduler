//! What `GET /v1/pools` shows of a launching pool comes from the scaler's records: the
//! machines, the figures of what they cost, the orphans and the last launch. Time is the
//! test's clock and the ticks are stepped; nothing sleeps and no network is touched.

use std::collections::BTreeMap;

use js_elastic::AcquireId;
use js_lease_server::RecordsFile;
use js_provider::Provider;
use js_scaler::{Millis, Phase, Record, Records, StoredOffer};
use serde_json::{Value, json};

use crate::scaler_support::{account, records_path, spec, start, text, token_in};
use crate::support::{Harness, Scratch, T0};

/// The one pool of `GET /v1/pools`.
async fn pool(h: &Harness) -> (Value, String) {
    let reply = h.call("GET", "/v1/pools", None).await;
    assert_eq!(reply.status, 200, "{}", reply.body);
    let mut pools = reply.json();
    (pools["pools"][0].take(), reply.body)
}

/// A record of the pool `pool` for the machine `name`, in `phase`, launched `at` ms after
/// the start of the clock; `cloud` is the provider's id for it, once known.
fn record(pool: &str, (name, cloud): (&str, Option<&str>), phase: Phase, at: u64) -> Record {
    Record {
        pool: pool.to_owned(),
        acquire: Some(AcquireId::new(&format!("{pool}-{name}")).unwrap()),
        name: name.to_owned(),
        offer: StoredOffer::nothing("gpu_1x_a100", Some("us-west-1".to_owned())),
        price_cents_per_hour: 129,
        phase,
        since: Millis::new(T0 + at),
        launched_at: Millis::new(T0 + at),
        token_expires: None,
        cloud_id: cloud.map(str::to_owned),
        retry_at: None,
        attempt: 0,
        gone_at: None,
        registered: false,
        condemned: false,
        clean_lists: 0,
        last_clean: None,
        seen_live: false,
    }
}

/// Asserts the status of a pool that has launched a machine: before it, no machine, no
/// spend and no last launch; after it, the machine in `booting` with its offer, zone and
/// price, the figures that count it, and the launch as accepted, at the time the record
/// names; and no join token anywhere in the reply. It catches a status that is not wired
/// to the scaler, and one that shows the token of the launch.
#[tokio::test]
async fn a_launched_machine_is_in_the_status_and_its_token_is_not() {
    let scratch = Scratch::new("scaler_status_launch");
    let account = account(&scratch);
    let source = text(&scratch, "live", "initialise");
    let h = start(scratch, &source).await;
    h.ticks.step().await;
    let (before, _) = pool(&h).await;
    assert_eq!(before["machines"], json!([]));
    // The heartbeat a watcher outside the server reads: the time of the tick that ran.
    assert_eq!(before["last_tick_ms"], T0);
    assert_eq!(before["scaler"], true);
    assert_eq!(before["last_launch"], Value::Null);
    assert_eq!(before["instances"]["n"], 0);

    h.running
        .registry()
        .unwrap()
        .provider()
        .start(spec(1, "r1"));
    h.ticks.step().await;
    h.ticks.step().await;
    let held = h.running.scaler().unwrap().state().records.instances[0].clone();
    let (after, body) = pool(&h).await;
    let machine = json!({
        "host": held.name, "state": "booting", "offer": "gpu_1x_a100",
        "zone": "us-west-1", "price_cents_per_hour": 129,
        "provider_id": held.cloud_id
    });
    assert_eq!(after["machines"], json!([machine]));
    assert_eq!(after["orphans"], json!([]));
    assert_eq!(after["instances"]["n"], 1);
    assert_eq!(after["instances"]["spend_cents_per_hour"], 129);
    let launch = json!({
        "at_ms": held.launched_at.as_millis(), "outcome": "accepted", "host": held.name
    });
    assert_eq!(after["last_launch"], launch);
    let token = token_in(account.launches()[0].user_data().unwrap());
    assert!(!body.contains(&token), "{body}");
}

/// Asserts what the status takes from the records of a restarted server: the machines that
/// still count (a gone one and a record of another pool do not), an orphan listed on its
/// own and left out of the machines and the spend, and the newest launch of the pool
/// (a record of another pool is not it), shown as pending while the provider has not
/// given an id for it. It catches an orphan counted as a machine, a gone machine still
/// listed, and the launch of another pool shown as this one's.
#[tokio::test]
async fn the_status_lists_the_machines_the_orphans_and_the_newest_launch_of_the_pool() {
    let scratch = Scratch::new("scaler_status_records");
    account(&scratch);
    let first = text(&scratch, "live", "initialise");
    let h = start(scratch, &first).await;
    let Harness {
        running, scratch, ..
    } = h;
    running.shutdown().await.unwrap();
    let records = Records {
        instances: vec![
            record("burst", ("js-a", Some("i-1")), Phase::Registered, 1000),
            record("burst", ("js-old", Some("i-2")), Phase::Gone, 500),
            record("burst", ("js-orphan", Some("i-3")), Phase::Orphan, 3000),
            record("burst", ("js-new", None), Phase::Planned, 2000),
            record(
                "elsewhere",
                ("js-other", Some("i-4")),
                Phase::Registered,
                9000,
            ),
        ],
        cooldowns: BTreeMap::new(),
        ..Default::default()
    };
    RecordsFile::new(records_path(&scratch))
        .save(&records)
        .unwrap();

    let again = text(&scratch, "live", "resume");
    let h = start(scratch, &again).await;
    let (view, _) = pool(&h).await;
    let one = |host: &str, state: &str, id: Option<&str>| {
        json!({
            "host": host, "state": state, "offer": "gpu_1x_a100",
            "zone": "us-west-1", "price_cents_per_hour": 129, "provider_id": id
        })
    };
    let machines = json!([
        one("js-a", "registered", Some("i-1")),
        one("js-new", "planned", None)
    ]);
    assert_eq!(view["machines"], machines);
    assert_eq!(
        view["orphans"],
        json!([{ "host": "js-orphan", "provider_id": "i-3" }])
    );
    assert_eq!(view["instances"]["n"], 2);
    assert_eq!(view["instances"]["spend_cents_per_hour"], 258);
    let launch = json!({ "at_ms": T0 + 2000, "outcome": "pending", "host": "js-new" });
    assert_eq!(view["last_launch"], launch);
}

/// Asserts that what the scaler cannot clear itself is on the status, as JSON and as the
/// page: a machine of ours that no record knows (an orphan) is an alert on the pool, with
/// the provider's id, and the page shows it as a row. It catches alerts that only a log
/// line carries, which an operator who does not read the log never sees.
#[tokio::test]
async fn the_alerts_are_on_the_status() {
    let scratch = Scratch::new("scaler_status_alerts");
    account(&scratch);
    let first = text(&scratch, "live", "initialise");
    let h = start(scratch, &first).await;
    let Harness {
        running, scratch, ..
    } = h;
    running.shutdown().await.unwrap();
    let orphan = record(
        "burst",
        ("js-orphan", Some("i-stray-alert")),
        Phase::Orphan,
        10,
    );
    let records = Records {
        instances: vec![orphan],
        cooldowns: BTreeMap::new(),
        ..Default::default()
    };
    RecordsFile::new(records_path(&scratch))
        .save(&records)
        .unwrap();

    let again = text(&scratch, "live", "resume");
    let h = start(scratch, &again).await;
    let (before, _) = pool(&h).await;
    assert_eq!(before["alerts"], json!([]));
    h.ticks.step().await;
    let (after, body) = pool(&h).await;
    let alerts = after["alerts"].as_array().unwrap();
    assert_eq!(alerts.len(), 1, "{body}");
    assert!(
        alerts[0].as_str().unwrap().contains("i-stray-alert"),
        "{body}"
    );
    let page = h.call("GET", "/status", None).await;
    assert_eq!(page.status, 200, "{}", page.body);
    assert!(
        page.body.contains("<th align=\"left\">alert</th>"),
        "{}",
        page.body
    );
    assert!(page.body.contains("i-stray-alert"), "{}", page.body);
}
