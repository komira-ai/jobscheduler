//! What `GET /v1/pools` shows of a launching pool comes from the scaler's records: the
//! machines, the figures of what they cost, the orphans and the last launch. Time is the
//! test's clock and the ticks are stepped; nothing sleeps and no network is touched.

use std::collections::BTreeMap;

use js_elastic::AcquireId;
use js_lease_server::RecordsFile;
use js_provider::Provider;
use js_scaler::{Millis, Phase, Record, Records, StoredOffer};
use serde_json::{Value, json};

use crate::scaler_support::{account, records_path, spec, start, text, text_with, token_in};
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
        "zone": "us-west-1", "price_cents_per_hour": 129
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
    let one = |host: &str, state: &str| {
        json!({
            "host": host, "state": state, "offer": "gpu_1x_a100",
            "zone": "us-west-1", "price_cents_per_hour": 129
        })
    };
    let machines = json!([one("js-a", "registered"), one("js-new", "planned")]);
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

/// Asserts that a launch a server-wide total refuses is shown, for each of the three totals:
/// two requests wait, the first launches within a total of 50 cents (a launch is billed at
/// least `boot_timeout`, 33 cents of an offer at 129 cents an hour), and the second would
/// take it to 66, so the pool shows `max_spend_total` as what blocks it and has launched one
/// machine, not two. It catches a total that is read and not enforced, and a cap the status
/// does not show.
#[tokio::test]
async fn a_launch_the_server_total_refuses_shows_max_spend_total() {
    for key in [
        "max_spend_per_day_total",
        "max_spend_per_month_total",
        "max_spend_since_go_total",
    ] {
        let scratch = Scratch::new(&format!("scaler_status_{key}"));
        account(&scratch);
        let source = text(&scratch, "live", "initialise").replacen(
            "[server]\n",
            &format!("[server]\n{key} = \"0.50\"\n"),
            1,
        );
        let h = start(scratch, &source).await;
        h.ticks.step().await;
        for (lease, id) in [(1, "r1"), (2, "r2")] {
            h.running
                .registry()
                .unwrap()
                .provider()
                .start(spec(lease, id));
        }
        for _ in 0..3 {
            h.ticks.step().await;
        }
        let (view, body) = pool(&h).await;
        assert_eq!(view["blocked"], "max_spend_total", "{key}: {body}");
        assert_eq!(view["instances"]["n"], 1, "{key}: {body}");
        assert_eq!(
            view["machines"].as_array().unwrap().len(),
            1,
            "{key}: {body}"
        );
    }
}

/// Asserts, at run time and not only at load, that the status shows the mode the pool is in
/// and when its go ends: a live pool with a `live_until` an hour ahead shows `live` and the
/// instant; once the clock passes it, the same pool shows `plan` (the file still says live),
/// and no machine is launched for a request that waits. It catches a status that keeps
/// showing the file's mode after the go has ended.
#[tokio::test]
async fn the_status_shows_a_go_that_has_ended() {
    let scratch = Scratch::new("scaler_status_expired");
    account(&scratch);
    let until = T0 + 3_600_000;
    let wall = iso(until);
    let source = text_with(&scratch, "live", "initialise", &[("live_until", &wall)]);
    let h = start(scratch, &source).await;
    h.ticks.step().await;
    let (before, _) = pool(&h).await;
    assert_eq!(before["launch"], "live");
    assert_eq!(before["live_until_ms"], until);

    h.clock.advance(std::time::Duration::from_secs(3601));
    h.running
        .registry()
        .unwrap()
        .provider()
        .start(spec(1, "r1"));
    h.ticks.step().await;
    h.ticks.step().await;
    let (after, body) = pool(&h).await;
    assert_eq!(after["launch"], "plan", "{body}");
    assert_eq!(after["live_until_ms"], until);
    assert_eq!(after["instances"]["n"], 0, "{body}");
}

/// The instant `ms` milliseconds after the epoch as a UTC date-time, for a file.
fn iso(ms: u64) -> String {
    let secs = ms / 1000;
    let (days, rest) = (secs / 86_400, secs % 86_400);
    // Civil date from days since the epoch (the inverse of the file reader's conversion).
    let z = i64::try_from(days).unwrap() + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + i64::from(month <= 2);
    format!(
        "{year:04}-{month:02}-{day:02}T{:02}:{:02}:{:02}Z",
        rest / 3600,
        rest % 3600 / 60,
        rest % 60
    )
}
