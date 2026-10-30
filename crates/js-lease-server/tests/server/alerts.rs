//! The alert keys of the configuration file, and the scaler's alerts reaching the sink
//! they name. No alert leaves the machine: the sink is the fake one.

use std::collections::BTreeMap;
use std::sync::Arc;

use js_alert::FakeSink;
use js_elastic::AcquireId;
use js_lease_server::{Config, OwnerOnlyFiles, RecordsFile};
use js_scaler::{Millis, Phase, Record, Records, StoredOffer};

use crate::config_support::{PoolText, clock, file, load, refusal, server};
use crate::scaler_support::{account, cloud_kinds, records_path, text};
use crate::support::{Harness, Scratch, T0};

/// `lines` as the `[server]` keys of a file with one static pool, loaded.
fn loaded(name: &str, lines: &str) -> (Scratch, Result<Config, String>) {
    let scratch = Scratch::new(name);
    let stat = PoolText::static_pool(&scratch, "hosts");
    let text = file(&server(&scratch, lines), &[&stat]);
    let config = load(&scratch, &text).map_err(|err| err.to_string());
    (scratch, config)
}

/// Asserts that the two keys are accepted alone and together (the token file read through
/// the server's owner-only reader), and refused with the rule that broke: a token file
/// without the URL, a URL that would carry the token in clear to a remote address, a token
/// file others can read, an empty one, and values of the wrong type. A refusal names the
/// key and repeats neither the URL nor the token. It catches a token that crosses a network
/// unencrypted and a typo that leaves the server silent.
#[test]
fn the_alert_keys_are_read_and_refused_with_their_rules() {
    let url = "alert_webhook_url = \"https://hooks.example.org/PLANTED-HOOK-PATH\"";
    assert!(loaded("alert_url", url).1.is_ok());
    assert!(
        loaded(
            "alert_loopback",
            "alert_webhook_url = \"http://127.0.0.1:9/x\""
        )
        .1
        .is_ok()
    );

    let scratch = Scratch::new("alert_token");
    let good = scratch.write("hook-token", "PLANTED-HOOK-TOKEN\n", 0o600);
    let open = scratch.write("open-token", "PLANTED-HOOK-TOKEN\n", 0o644);
    let empty = scratch.write("empty-token", "\n", 0o600);
    let stat = PoolText::static_pool(&scratch, "hosts");
    let with = |lines: &str| file(&server(&scratch, lines), &[&stat]);
    let token = |path: &std::path::Path| {
        format!(
            "alert_webhook_token_file = {:?}",
            path.display().to_string()
        )
    };
    assert!(load(&scratch, &with(&format!("{url}\n{}", token(&good)))).is_ok());

    let cases = [
        (
            token(&good),
            "server.alert_webhook_token_file: needs server.alert_webhook_url".to_owned(),
        ),
        (
            "alert_webhook_url = \"http://10.0.0.5/PLANTED-HOOK-PATH\"".to_owned(),
            "server.alert_webhook_url: plain http is accepted for a loopback address only"
                .to_owned(),
        ),
        (
            "alert_webhook_url = \"ftp://x\"".to_owned(),
            "server.alert_webhook_url: the URL must start with https:// or http://".to_owned(),
        ),
        (
            "alert_webhook_url = 5".to_owned(),
            "server.alert_webhook_url: must be a string".to_owned(),
        ),
    ];
    for (lines, said) in cases {
        let said_now = refusal(&scratch, &with(&lines));
        assert_eq!(said_now, said, "{lines}");
        assert!(!said_now.contains("PLANTED"), "{said_now}");
    }
    let unreadable = refusal(&scratch, &with(&format!("{url}\n{}", token(&open))));
    assert!(
        unreadable.starts_with("server.alert_webhook_token_file: "),
        "{unreadable}"
    );
    assert!(unreadable.contains("0600"), "{unreadable}");
    assert!(!unreadable.contains("PLANTED-HOOK-TOKEN"), "{unreadable}");
    let blank = refusal(&scratch, &with(&format!("{url}\n{}", token(&empty))));
    assert!(
        blank.starts_with("server.alert_webhook_token_file: "),
        "{blank}"
    );
    assert!(blank.ends_with("is empty"), "{blank}");
}

/// A record of the pool `burst`: an orphan, a machine of ours that no record knows.
fn orphan() -> Record {
    Record {
        pool: "burst".to_owned(),
        acquire: Some(AcquireId::new("burst-js-orphan").unwrap()),
        name: "js-orphan".to_owned(),
        offer: StoredOffer::nothing("gpu_1x_a100", Some("us-west-1".to_owned())),
        price_cents_per_hour: 129,
        phase: Phase::Orphan,
        since: Millis::new(T0 + 10),
        launched_at: Millis::new(T0 + 10),
        token_expires: None,
        cloud_id: Some("i-stray-alert".to_owned()),
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

/// Asserts the sink through a whole running server: a file that names a webhook and a
/// server given the fake sink in its place (`with_alert_sink` replaces what the file
/// names), a restored orphan record, and ticks. The orphan reaches the sink as one alert
/// with the provider's id in its text, however many ticks it holds for, and the status
/// shows the same text and the heartbeat. It catches a sink that is not handed to the
/// scaler the server builds, which only a running server shows.
#[tokio::test]
async fn a_running_server_sends_its_alerts_to_the_sink() {
    let scratch = Scratch::new("alerts_running");
    account(&scratch);
    let first = text(&scratch, "live", "initialise");
    let path = scratch.write("js.toml", &first, 0o644);
    let config = Config::from_config_file(&path, &cloud_kinds(), &OwnerOnlyFiles, clock());
    let h = Harness::over(scratch, config.unwrap()).await;
    let Harness {
        running, scratch, ..
    } = h;
    running.shutdown().await.unwrap();
    let records = Records {
        instances: vec![orphan()],
        cooldowns: BTreeMap::new(),
        ..Default::default()
    };
    RecordsFile::new(records_path(&scratch))
        .save(&records)
        .unwrap();

    let again = text(&scratch, "live", "resume");
    let again = again.replace(
        "[server]\n",
        "[server]\nalert_webhook_url = \"http://127.0.0.1:9/never\"\n",
    );
    let path = scratch.write("js.toml", &again, 0o644);
    let sink = Arc::new(FakeSink::new());
    let config = Config::from_config_file(&path, &cloud_kinds(), &OwnerOnlyFiles, clock())
        .unwrap()
        .with_alert_sink(sink.clone());
    let h = Harness::over(scratch, config).await;
    h.checks.step().await;
    for _ in 0..3 {
        h.ticks.step().await;
    }
    assert_eq!(sink.keys(), ["orphan:burst:js-orphan:i-stray-alert"]);
    assert!(sink.sent()[0].text.contains("i-stray-alert"));
    let reply = h.call("GET", "/v1/pools", None).await;
    let pools = reply.json();
    assert_eq!(pools["pools"][0]["alerts"][0], sink.sent()[0].text);
    assert_eq!(pools["pools"][0]["last_tick_ms"], T0);
}

/// Asserts that a file that names a webhook but has no elastic pool starts and serves
/// (there is no scaler to give the sink to). It catches a start that refuses a server for
/// having a place to send alerts it can never raise.
#[tokio::test]
async fn a_server_without_a_scaler_starts_with_a_webhook_named() {
    let (scratch, config) = loaded(
        "alerts_no_scaler",
        "alert_webhook_url = \"http://127.0.0.1:9/never\"",
    );
    let h = Harness::over(scratch, config.unwrap()).await;
    let health = h.call("GET", "/healthz", None).await;
    assert_eq!(health.status, 200, "{}", health.body);
}
