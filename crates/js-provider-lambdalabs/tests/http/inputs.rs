//! The calls a launch needs besides launching: firewall rulesets and ssh keys. What is
//! sent, what is read, and what is not read.

use js_provider_lambdalabs::{ApiError, FirewallRuleset, LambdaApi, SshKey};
use serde_json::{Value, json};

use crate::reads::client;
use crate::support::{Reply, Server, fixture};

const RULESETS: &str = "GET /api/v1/firewall-rulesets";
const MAKE_RULESET: &str = "POST /api/v1/firewall-rulesets";
const KEYS: &str = "GET /api/v1/ssh-keys";
const MAKE_KEY: &str = "POST /api/v1/ssh-keys";

fn body(server: &Server, index: usize) -> Value {
    serde_json::from_slice(&server.seen()[index].body).unwrap()
}

#[test]
fn rulesets_are_read_with_their_region_and_how_many_rules_they_hold() {
    let server = Server::plain();
    server.on(
        RULESETS,
        Reply::json(200, &fixture("firewall_rulesets.json")),
    );
    let found = client(&server, "k").list_firewall_rulesets().unwrap();
    assert_eq!(
        found,
        [
            FirewallRuleset {
                id: "rs-noinbound-a".to_owned(),
                name: "js-sched-one-noinbound".to_owned(),
                region: "region-a".to_owned(),
                rules: 0,
            },
            FirewallRuleset {
                id: "rs-web".to_owned(),
                name: "web".to_owned(),
                region: "region-a".to_owned(),
                rules: 1,
            },
        ]
    );
}

#[test]
fn a_ruleset_that_does_not_say_what_it_holds_is_malformed_not_empty() {
    let cases = [
        r#"{"data": [{"id": "r", "name": "n", "region": {"name": "x"}}]}"#,
        r#"{"data": [{"id": "r", "name": "n", "region": {"name": "x"}, "rules": 3}]}"#,
        r#"{"data": [{"name": "n", "region": {"name": "x"}, "rules": []}]}"#,
        r#"{"data": {}}"#,
    ];
    for text in cases {
        let server = Server::plain();
        server.on(RULESETS, Reply::json(200, text));
        let result = client(&server, "k").list_firewall_rulesets();
        assert!(matches!(result, Err(ApiError::Malformed(_))), "{text}");
    }
}

#[test]
fn a_ruleset_is_created_with_a_name_a_region_and_no_rules() {
    let server = Server::plain();
    server.on(
        MAKE_RULESET,
        Reply::json(200, &fixture("firewall_ruleset.json")),
    );
    let made = client(&server, "k")
        .create_firewall_ruleset("region-a", "js-sched-one-noinbound")
        .unwrap();
    assert_eq!(made.id, "rs-new");
    assert_eq!(made.rules, 0);
    let seen = &server.seen()[0];
    assert_eq!(seen.header("content-type"), Some("application/json"));
    assert_eq!(
        body(&server, 0),
        json!({"name": "js-sched-one-noinbound", "region": "region-a", "rules": []})
    );
}

#[test]
fn keys_are_read_by_id_and_name() {
    let server = Server::plain();
    server.on(KEYS, Reply::json(200, &fixture("ssh_keys.json")));
    let found = client(&server, "k").list_ssh_keys().unwrap();
    assert_eq!(
        found,
        [
            SshKey {
                id: "k1".to_owned(),
                name: "alpha".to_owned()
            },
            SshKey {
                id: "k2".to_owned(),
                name: "beta".to_owned()
            },
        ]
    );
    let server = Server::plain();
    server.on(KEYS, Reply::json(200, r#"{"data": [{"id": "k"}]}"#));
    assert!(matches!(
        client(&server, "k").list_ssh_keys(),
        Err(ApiError::Malformed(_))
    ));
}

#[test]
fn a_key_is_made_by_name_alone_and_the_private_half_the_answer_carries_is_not_read() {
    let server = Server::plain();
    server.on(
        MAKE_KEY,
        Reply::json(200, &fixture("ssh_key_generated.json")),
    );
    let made = client(&server, "k").create_ssh_key("js-sched-one").unwrap();
    // Only a name goes out: with no public key, the API makes the pair.
    assert_eq!(body(&server, 0), json!({"name": "js-sched-one"}));
    assert_eq!(made.name, "js-sched-one");
    assert!(!format!("{made:?}").contains("PRIVATE-CANARY"));
}

#[test]
fn a_key_answer_of_the_wrong_shape_does_not_quote_the_secret_it_holds() {
    // `data` is a string here, where an object belongs: the parser's own message would
    // quote the string, and the string holds a private key.
    let secret = "-----BEGIN PRIVATE KEY-----\nPLANTED-SECRET-5d2f\n-----END PRIVATE KEY-----\n";
    let answer = json!({ "data": secret }).to_string();
    let server = Server::plain();
    server.on(MAKE_KEY, Reply::json(200, &answer));
    let err = client(&server, "k")
        .create_ssh_key("js-sched-one")
        .unwrap_err();
    assert!(matches!(err, ApiError::Malformed(_)), "{err:?}");
    for shown in [format!("{err}"), format!("{err:?}")] {
        assert!(!shown.contains("PLANTED-SECRET"), "{shown}");
        assert!(!shown.contains("PRIVATE KEY"), "{shown}");
        assert!(shown.contains("unexpected answer shape"), "{shown}");
    }
}

#[test]
fn a_refused_call_is_the_error_of_the_api_for_each_of_the_four() {
    // A server with no route answers every request 500.
    let server = Server::plain();
    let api = client(&server, "k");
    let errors = [
        api.list_firewall_rulesets().map(|_| ()).unwrap_err(),
        api.create_firewall_ruleset("region-a", "n")
            .map(|_| ())
            .unwrap_err(),
        api.list_ssh_keys().map(|_| ()).unwrap_err(),
        api.create_ssh_key("n").map(|_| ()).unwrap_err(),
    ];
    for error in errors {
        assert!(
            matches!(error, ApiError::Api { status: 500, .. }),
            "{error:?}"
        );
    }
}

#[test]
fn a_made_ruleset_or_key_that_is_not_the_documented_shape_is_malformed() {
    let server = Server::plain();
    server.on(MAKE_RULESET, Reply::json(200, r#"{"data": {"id": "r"}}"#));
    server.on(MAKE_KEY, Reply::json(200, "not json"));
    let api = client(&server, "k");
    assert!(matches!(
        api.create_firewall_ruleset("region-a", "n"),
        Err(ApiError::Malformed(_))
    ));
    assert!(matches!(
        api.create_ssh_key("n"),
        Err(ApiError::Malformed(_))
    ));
}
