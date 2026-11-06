//! Where the access token comes from: the metadata server, or the key file of a service
//! account that `GOOGLE_APPLICATION_CREDENTIALS` names.

use std::cell::RefCell;
use std::io;
use std::path::{Path, PathBuf};
use std::time::Duration;

use base64::Engine as _;
use base64::engine::general_purpose::{STANDARD, URL_SAFE_NO_PAD};
use js_logstore::TransportError;
use js_provider_gce::{
    ApiError, CredentialError, Credentials, GceApi, KEY_FILE_VARIABLE, ServiceAccountKey,
};
use ring::signature::{RSA_PKCS1_2048_8192_SHA256, RsaKeyPair, UnparsedPublicKey};
use serde_json::{Value, json};

use crate::launch::{ZONE, by_name, instance_json};
use crate::script::{
    Answer, NOW_SECONDS, TOKEN, TOKEN_URL, answer, clock, over_with, status, token_answer,
};

const TOKEN_URI: &str = "https://oauth2.googleapis.com/token";

fn fixture_path() -> PathBuf {
    PathBuf::from(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/tests/fixtures/credential/test-key.json"
    ))
}

fn fixture() -> Vec<u8> {
    std::fs::read(fixture_path()).unwrap()
}

fn fixture_value() -> Value {
    serde_json::from_slice(&fixture()).unwrap()
}

fn key_with(value: &Value) -> Result<ServiceAccountKey, CredentialError> {
    ServiceAccountKey::from_json(value.to_string().as_bytes())
}

fn instance() -> Answer {
    answer(&instance_json("1234567890123456789", "js-vm-one", "RUNNING", ZONE).to_string())
}

/// The DER of the fixture's private key.
fn fixture_der() -> Vec<u8> {
    let value = fixture_value();
    let pem = value["private_key"].as_str().unwrap();
    let body: String = pem
        .lines()
        .filter(|line| !line.starts_with("-----"))
        .collect();
    STANDARD.decode(body).unwrap()
}

fn lookup_of(value: Option<&'static str>) -> impl Fn(&str) -> Option<String> {
    move |name| {
        assert_eq!(name, KEY_FILE_VARIABLE);
        value.map(str::to_owned)
    }
}

fn refuse_to_read(_: &Path) -> Result<Vec<u8>, String> {
    panic!("no file is read");
}

/// Asserts that with the variable absent, or set to nothing, the credential is the metadata
/// server, and that the lookup is asked for the key file variable and no other name and no
/// file is read. It catches a client that reads an empty path as a file, or reads any other
/// part of the environment.
#[test]
fn without_a_key_file_the_credential_is_the_metadata_server() {
    for value in [None, Some("")] {
        let credentials = Credentials::from_lookup(lookup_of(value), refuse_to_read).unwrap();
        assert!(matches!(credentials, Credentials::MetadataServer));
        assert_eq!(format!("{credentials:?}"), "MetadataServer");
    }
}

/// Asserts that a named file is read at the path the variable holds, and that a good
/// service-account key gives the service-account credential whose debug text names the
/// account's email and holds no part of the private key. It catches a credential that
/// ignores the file, and a debug text that prints the key.
#[test]
fn a_named_key_file_gives_the_service_account_credential() {
    let asked = RefCell::new(Vec::new());
    let credentials = Credentials::from_lookup(lookup_of(Some("/sample/key.json")), |path| {
        asked.borrow_mut().push(path.to_path_buf());
        Ok(fixture())
    })
    .unwrap();
    assert_eq!(asked.into_inner(), [PathBuf::from("/sample/key.json")]);
    assert!(matches!(credentials, Credentials::ServiceAccount(_)));
    let text = format!("{credentials:?}");
    assert_eq!(
        text,
        "ServiceAccount(ServiceAccountKey { client_email: \"tester@sample-project.iam.gserviceaccount.com\", .. })"
    );
    assert!(!text.contains("MII") && !text.contains("PRIVATE"));
}

/// Asserts that a key file that cannot be read is `Read` with the reader's own words, a
/// file that is not JSON is `NotJson`, JSON of another type of credential (or with no type)
/// is `NotServiceAccount`, a key lacking `client_email`, `token_uri` or `private_key` names
/// the field, and a `private_key` that is not an RSA PKCS#8 PEM (no markers, no end marker,
/// bad base64, bytes that are no key) is `BadPrivateKey`; that every message names the
/// variable; and that no message holds what the file holds. It catches an error that echoes
/// the key file.
#[test]
fn a_key_file_that_cannot_be_used_is_named_by_the_variable_and_the_problem() {
    let secret = "SECRETCONTENT";
    let from = |bytes: Vec<u8>| {
        Credentials::from_lookup(lookup_of(Some("key.json")), move |_| Ok(bytes.clone()))
    };
    let altered = |key: &str, value: Value| {
        let mut body = fixture_value();
        body[key] = value;
        from(body.to_string().into_bytes())
    };
    let removed = |key: &str| {
        let mut body = fixture_value();
        body.as_object_mut().unwrap().remove(key);
        from(body.to_string().into_bytes())
    };
    let unreadable = Credentials::from_lookup(lookup_of(Some("key.json")), |_| {
        Err("no such file".to_owned())
    });
    let begin = "-----BEGIN PRIVATE KEY-----";
    let end = "-----END PRIVATE KEY-----";
    let cases = [
        (unreadable, CredentialError::Read("no such file".to_owned())),
        (from(secret.as_bytes().to_vec()), CredentialError::NotJson),
        (
            altered("type", json!("authorized_user")),
            CredentialError::NotServiceAccount,
        ),
        (removed("type"), CredentialError::NotServiceAccount),
        (
            removed("client_email"),
            CredentialError::Missing("client_email"),
        ),
        (removed("token_uri"), CredentialError::Missing("token_uri")),
        (
            removed("private_key"),
            CredentialError::Missing("private_key"),
        ),
        (
            altered("private_key", json!(secret)),
            CredentialError::BadPrivateKey,
        ),
        (
            altered("private_key", json!(format!("{begin}\nAAAA\n"))),
            CredentialError::BadPrivateKey,
        ),
        (
            altered("private_key", json!(format!("{begin}\n!!!!\n{end}"))),
            CredentialError::BadPrivateKey,
        ),
        (
            altered("private_key", json!(format!("{begin}\nAAAA\n{end}"))),
            CredentialError::BadPrivateKey,
        ),
    ];
    for (result, expected) in cases {
        let error = result.unwrap_err();
        assert_eq!(error, expected);
        let text = error.to_string();
        assert!(text.contains(KEY_FILE_VARIABLE), "{text}");
        assert!(!text.contains(secret), "{text}");
    }
}

/// Asserts that a key file whose lines end in CR LF and whose `private_key_id` is absent is
/// a good key. It catches a PEM reader that depends on the line ending.
#[test]
fn a_key_with_carriage_returns_and_no_key_id_is_good() {
    let mut body = fixture_value();
    body.as_object_mut().unwrap().remove("private_key_id");
    let pem = body["private_key"].as_str().unwrap().replace('\n', "\r\n");
    body["private_key"] = json!(pem);
    assert!(key_with(&body).is_ok());
}

/// Asserts that the standard credential and the file reader behind it work on the process
/// as it is: the standard credential is a credential or an error about the key file (the
/// result is not compared, because a test does not set the process environment), the
/// reader gives a file's bytes, and gives the reason in text for a file that is not there.
/// It catches a reader that panics on a missing file.
#[test]
fn the_standard_credential_reads_the_environment_and_the_file_system() {
    let _standard = Credentials::standard();
    assert_eq!(
        Credentials::read_key_file(&fixture_path()).unwrap(),
        fixture()
    );
    let absent = fixture_path().with_file_name("absent.json");
    assert!(!Credentials::read_key_file(&absent).unwrap_err().is_empty());
}

/// Asserts that the metadata server's token is asked for with a GET at the documented
/// link-local address carrying the `metadata-flavor: Google` header and no body, and is
/// then the bearer token of the API call. It catches a token request the server refuses.
#[test]
fn the_metadata_server_is_asked_for_the_token_with_the_flavor_header() {
    let (client, script) = over_with(
        Credentials::MetadataServer,
        vec![token_answer(3600), instance()],
        clock(),
    );
    client.get_instance(&by_name()).unwrap();
    let sent = script.sent();
    assert_eq!(sent.len(), 2);
    assert_eq!(sent[0].method.as_str(), "GET");
    assert_eq!(sent[0].url, TOKEN_URL);
    assert_eq!(sent[0].header("metadata-flavor"), Some("Google"));
    assert!(sent[0].body.is_empty());
    assert_eq!(
        sent[1].header("authorization"),
        Some(format!("Bearer {TOKEN}").as_str())
    );
}

/// Asserts that a token is kept while it has more than five minutes to live (a second call
/// makes no token request) and fetched again once it has five minutes or less; and that a
/// token whose answer states a lifetime of years is not kept for more than a day. It
/// catches a token fetched on every call and one used up to its last second.
#[test]
fn a_token_is_kept_until_five_minutes_before_it_expires() {
    let wall = clock();
    let (client, script) = over_with(
        Credentials::MetadataServer,
        vec![
            token_answer(3600),
            instance(),
            instance(),
            token_answer(u64::MAX),
            instance(),
            instance(),
            token_answer(3600),
            instance(),
        ],
        wall.clone(),
    );
    client.get_instance(&by_name()).unwrap();
    wall.advance(Duration::from_secs(54 * 60));
    client.get_instance(&by_name()).unwrap();
    assert_eq!(script.sent().len(), 3, "kept with six minutes left");
    wall.advance(Duration::from_secs(60));
    client.get_instance(&by_name()).unwrap();
    assert_eq!(
        script.sent().len(),
        5,
        "fetched again with five minutes left"
    );
    wall.advance(Duration::from_secs(23 * 3600));
    client.get_instance(&by_name()).unwrap();
    assert_eq!(script.sent().len(), 6, "kept with an hour left");
    wall.advance(Duration::from_secs(3600));
    client.get_instance(&by_name()).unwrap();
    assert_eq!(
        script.sent().len(),
        8,
        "a day is the longest a token is kept"
    );
    assert_eq!(script.left(), 0);
}

/// Asserts that every way the token call can fail gives an error and no API request: a
/// timeout is `Timeout`; a transport failure, an HTTP status other than 200, a body that is
/// not JSON, and one lacking or misstating `access_token` or `expires_in` are
/// `NoCredential`; and that a failure is not kept, so the next call asks again and
/// succeeds. It catches a failed fetch that calls with an empty token.
#[test]
fn every_failure_of_the_token_call_is_an_error_and_is_asked_again_next_call() {
    let ok = |body: &str| status(200, body);
    let cases: Vec<(Answer, ApiError)> = vec![
        (Err(TransportError::Timeout), ApiError::Timeout),
        (
            Err(TransportError::Io(io::ErrorKind::ConnectionRefused)),
            ApiError::NoCredential,
        ),
        (status(404, "{}"), ApiError::NoCredential),
        (
            status(
                500,
                &format!("{{\"access_token\":\"{TOKEN}\",\"expires_in\":3600}}"),
            ),
            ApiError::NoCredential,
        ),
        (ok("not json"), ApiError::NoCredential),
        (ok("{\"expires_in\":3600}"), ApiError::NoCredential),
        (
            ok("{\"access_token\":5,\"expires_in\":3600}"),
            ApiError::NoCredential,
        ),
        (ok("{\"access_token\":\"t\"}"), ApiError::NoCredential),
        (
            ok("{\"access_token\":\"t\",\"expires_in\":\"soon\"}"),
            ApiError::NoCredential,
        ),
    ];
    for (failure, expected) in cases {
        let (client, script) = over_with(
            Credentials::MetadataServer,
            vec![failure, token_answer(3600), instance()],
            clock(),
        );
        assert_eq!(client.get_instance(&by_name()), Err(expected));
        assert_eq!(script.sent().len(), 1);
        assert!(client.get_instance(&by_name()).is_ok());
        assert_eq!(script.sent().len(), 3);
    }
}

/// Splits a JWT into its three parts.
fn parts(jwt: &str) -> [String; 3] {
    let mut parts = jwt.split('.').map(str::to_owned);
    let found = [
        parts.next().unwrap(),
        parts.next().unwrap(),
        parts.next().unwrap(),
    ];
    assert!(parts.next().is_none());
    found
}

fn json_part(part: &str) -> Value {
    serde_json::from_slice(&URL_SAFE_NO_PAD.decode(part).unwrap()).unwrap()
}

/// The assertion a service-account exchange sent, with the request's other facts checked.
fn assertion_sent(request: &js_logstore::HttpRequest) -> String {
    assert_eq!(request.method.as_str(), "POST");
    assert_eq!(request.url, TOKEN_URI);
    assert_eq!(
        request.header("content-type"),
        Some("application/x-www-form-urlencoded")
    );
    let body = String::from_utf8(request.body.clone()).unwrap();
    let rest = body.strip_prefix("assertion=").unwrap();
    let (assertion, grant) = rest.split_once('&').unwrap();
    assert_eq!(
        grant,
        "grant_type=urn%3Aietf%3Aparams%3Aoauth%3Agrant-type%3Ajwt-bearer"
    );
    assertion.to_owned()
}

/// Asserts that a service-account credential exchanges a signed assertion at the key's
/// token address: a form POST of the JWT-bearer grant type and the assertion, whose header
/// names RS256, JWT and the key id, whose claims are the account, both scopes, the token
/// address, the clock's time and an hour later, and whose signature verifies under the
/// key's public key; that the same key and clock give the same assertion; and that the
/// token the exchange answers is the bearer token of the API call. It catches an assertion
/// with the wrong audience or lifetime, one signed over other bytes, and a token that is
/// not used.
#[test]
fn a_service_account_exchanges_a_signed_assertion_for_the_token() {
    let exchange =
        r#"{"access_token":"service-account-token","expires_in":3599,"token_type":"Bearer"}"#;
    let mut assertions = Vec::new();
    for _ in 0..2 {
        let key = ServiceAccountKey::from_json(&fixture()).unwrap();
        let (client, script) = over_with(
            Credentials::ServiceAccount(Box::new(key)),
            vec![answer(exchange), instance(), instance()],
            clock(),
        );
        client.get_instance(&by_name()).unwrap();
        client.get_instance(&by_name()).unwrap();
        let sent = script.sent();
        assert_eq!(sent.len(), 3, "the token is kept");
        assertions.push(assertion_sent(&sent[0]));
        for call in &sent[1..] {
            assert_eq!(
                call.header("authorization"),
                Some("Bearer service-account-token")
            );
        }
    }
    assert_eq!(assertions[0], assertions[1]);
    let [header, claims, signature] = parts(&assertions[0]);
    assert_eq!(
        json_part(&header),
        json!({"alg": "RS256", "typ": "JWT", "kid": "test-key-id"})
    );
    assert_eq!(
        json_part(&claims),
        json!({
            "iss": "tester@sample-project.iam.gserviceaccount.com",
            "scope": "https://www.googleapis.com/auth/compute https://www.googleapis.com/auth/cloud-billing.readonly",
            "aud": TOKEN_URI,
            "iat": NOW_SECONDS,
            "exp": NOW_SECONDS + 3600,
        })
    );
    let pair = RsaKeyPair::from_pkcs8(&fixture_der()).unwrap();
    let der: &[u8] = pair.public().as_ref();
    let public = UnparsedPublicKey::new(&RSA_PKCS1_2048_8192_SHA256, der);
    let signed = format!("{header}.{claims}");
    let bytes = URL_SAFE_NO_PAD.decode(signature).unwrap();
    assert!(public.verify(signed.as_bytes(), &bytes).is_ok());
    assert!(public.verify(b"other bytes", &bytes).is_err());
}

/// Asserts that a key with no `private_key_id` signs an assertion whose header has no
/// `kid`, and that an exchange the cloud refuses is `NoCredential` with no API request.
/// It catches a header that states a key id of null.
#[test]
fn an_assertion_without_a_key_id_has_no_kid_and_a_refused_exchange_is_no_credential() {
    let mut body = fixture_value();
    body.as_object_mut().unwrap().remove("private_key_id");
    let (client, script) = over_with(
        Credentials::ServiceAccount(Box::new(key_with(&body).unwrap())),
        vec![status(400, "{\"error\":\"invalid_grant\"}")],
        clock(),
    );
    assert_eq!(client.get_instance(&by_name()), Err(ApiError::NoCredential));
    let sent = script.sent();
    assert_eq!(sent.len(), 1);
    let [header, _, _] = parts(&assertion_sent(&sent[0]));
    assert_eq!(json_part(&header), json!({"alg": "RS256", "typ": "JWT"}));
}
