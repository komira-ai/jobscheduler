//! No error, `Debug` output or request record shows a secret, a session token, a signature or a
//! URL's query.

use std::io;
use std::sync::Arc;
use std::time::Duration;

use js_logstore::{
    Credential, ManualClock, Metadata, Store, TransportError, Unavailable, UreqTransport,
};

use crate::support::{START, Scripted, at, endpoint, reply, s3_error};

const SECRET: &str = "SECRET-wJalrXUtnFEMI";
const TOKEN: &str = "TOKEN-AQoDYXdzEPT";

/// A credential holding [`SECRET`] and [`TOKEN`].
fn secret_credential() -> Credential {
    Credential {
        access_key_id: "AKIDVISIBLE".to_owned(),
        secret_access_key: SECRET.to_owned(),
        session_token: Some(TOKEN.to_owned()),
        expires_at: at(START + 3600),
    }
}

/// Asserts, by exact equality, the text of every error variant as the client produces it while
/// signing with a secret and a token, the `Debug` of the credential, the endpoint, the store,
/// the transport and of the requests sent (a listing with a query among them); and that none
/// of them holds the secret, the token, the signature or the listing's query.
#[test]
fn errors_and_debug_never_show_a_secret() {
    let transport = Scripted::new(vec![
        Err(TransportError::Timeout),
        Err(TransportError::Io(io::ErrorKind::ConnectionRefused)),
        Err(TransportError::Request),
        Err(TransportError::Exchange),
        s3_error(503, "SlowDown"),
        s3_error(403, "AccessDenied"),
        s3_error(400, "ExpiredToken"),
        s3_error(404, "NoSuchBucket"),
        reply(416, &[], b""),
        reply(206, &[], b"x"),
        reply(200, &[], b"<ListBucketResult></ListBucketResult>"),
    ]);
    let clock = Arc::new(ManualClock::new(at(START)));
    let store = Store::new(
        transport.clone(),
        clock.clone(),
        endpoint(true),
        secret_credential(),
    );
    let key = "js/job-1/stdout.log";
    let put = || store.put(key, b"x".to_vec(), &Metadata::new()).unwrap_err();

    let mut errors = vec![put(), put(), put(), put(), put(), put(), put(), put()];
    errors.push(store.get_range(key, 0, 1).unwrap_err());
    errors.push(store.get_range(key, 0, 1).unwrap_err());
    errors.push(
        store
            .list("js/secret-prefix/", Some("js/after"), 10)
            .unwrap_err(),
    );
    errors.push(
        store
            .put(key, vec![0; 64 * 1024 * 1024 + 1], &Metadata::new())
            .unwrap_err(),
    );
    errors.push(store.list("", None, 0).unwrap_err());
    clock.advance(Duration::from_secs(3600));
    errors.push(put());

    let texts: Vec<String> = errors.iter().map(ToString::to_string).collect();
    assert_eq!(
        texts,
        [
            "the store is unavailable: the store did not answer in time",
            "the store is unavailable: the connection failed (ConnectionRefused)",
            "the store is unavailable: the request cannot be sent as given",
            "the store is unavailable: the exchange failed",
            "the store is unavailable: status 503, code `SlowDown`",
            "the store refused the request (status 403, code `AccessDenied`)",
            "the credential has expired (code `ExpiredToken`)",
            "the store has no such bucket or object (code `NoSuchBucket`)",
            "the range starts past the object's end",
            "the store's answer is malformed: a ranged answer has no usable content-range",
            "the store's answer is malformed: no IsTruncated of true or false",
            "the body is 67108865 bytes, more than the 67108864 one put carries",
            "the call is refused before sending: max_keys is 1 to MAX_LIST_KEYS",
            "the credential has expired (code ``)",
        ]
    );
    assert_eq!(
        format!("{:?}", errors[4]),
        "Unavailable(Status { status: 503, code: \"SlowDown\" })"
    );
    assert_eq!(
        TransportError::TooLarge.to_string(),
        "the answer is longer than the transport reads"
    );
    assert_eq!(
        Unavailable::Transport(TransportError::Timeout).to_string(),
        "the store did not answer in time"
    );

    let credential = secret_credential();
    assert_eq!(
        format!("{credential:?}"),
        "Credential { access_key_id: \"AKIDVISIBLE\", .. }"
    );
    assert_eq!(
        format!("{:?}", endpoint(false)),
        "Endpoint { https: true, authority: \"store.example.com\", region: \"us-east-1\", bucket: \"logs\", path_style: false }"
    );
    assert_eq!(
        format!("{store:?}"),
        "Store { endpoint: Endpoint { https: true, authority: \"store.example.com\", region: \"us-east-1\", bucket: \"logs\", path_style: true }, credential: Credential { access_key_id: \"AKIDVISIBLE\", .. }, .. }"
    );
    assert_eq!(
        format!("{:?}", UreqTransport::new(Duration::from_secs(1))),
        "UreqTransport { .. }"
    );

    let sent = transport.sent();
    let listing = &sent[10];
    assert!(listing.url.contains("?list-type=2"));
    let signature = listing
        .header("authorization")
        .and_then(|value| value.split_once("Signature="))
        .map(|(_, signature)| signature.to_owned())
        .unwrap();
    assert_eq!(
        format!("{listing:?}"),
        "HttpRequest { method: Get, url: \"https://store.example.com/logs\", headers: [\"host\", \"x-amz-date\", \"x-amz-content-sha256\", \"x-amz-security-token\", \"authorization\"], body_len: 0 }"
    );
    assert_eq!(
        format!("{:?}", sent[0]),
        "HttpRequest { method: Put, url: \"https://store.example.com/logs/js/job-1/stdout.log\", headers: [\"content-type\", \"host\", \"x-amz-date\", \"x-amz-content-sha256\", \"x-amz-security-token\", \"authorization\"], body_len: 1 }"
    );

    let mut shown: Vec<String> = texts;
    shown.extend(errors.iter().map(|error| format!("{error:?}")));
    shown.extend(sent.iter().map(|request| format!("{request:?}")));
    shown.push(format!("{store:?}"));
    shown.push(format!("{credential:?}"));
    for text in shown {
        for hidden in [
            SECRET,
            TOKEN,
            signature.as_str(),
            "secret-prefix",
            "list-type",
        ] {
            assert!(!text.contains(hidden), "{text} holds {hidden}");
        }
    }
}

/// Asserts that a request record's `Debug` with no query shows the whole URL, and an answer's
/// headers are found by name.
#[test]
fn a_record_without_a_query_shows_its_url() {
    let transport = Scripted::new(vec![reply(200, &[("etag", "\"e\"")], b"")]);
    let store = Store::new(
        transport.clone(),
        Arc::new(ManualClock::new(at(START))),
        endpoint(false),
        secret_credential(),
    );
    store.delete("k").unwrap();
    let sent = transport.sent();
    assert!(format!("{:?}", sent[0]).contains("url: \"https://logs.store.example.com/k\""));
    let answer = reply(200, &[("etag", "\"e\"")], b"").unwrap();
    assert_eq!(answer.header("etag"), Some("\"e\""));
    assert_eq!(answer.header("missing"), None);
}
