//! The `AssumeRole` call against the fake store's STS: its shape, its signature, and the
//! credential it brings back.

use std::time::Duration;

use js_logmint::{MintError, Minter, POLICY_VERSION, Refusal, STS_API_VERSION, StsEndpoint};
use js_logstore::sigv4::{Scope, Unsigned, sha256_hex, sign, uri_encode};
use js_logstore::{Credential, EndpointError, Method};

use crate::support::{HOUR, REGION, ROLE, Rig, START, URL, at, form, request};

/// An agent id longer than STS takes, with characters it refuses.
const LONG_HOST: &str =
    "host-1.example/with spaces:and*stars and far more than sixty-four bytes in all";

/// Asserts the one request a mint sends: a `POST` to `{sts_endpoint}/` with the content type
/// of a form; the form fields `Action`, `Version`, `RoleArn`, `RoleSessionName`,
/// `DurationSeconds` and `Policy`, in that order, with exactly the values of the call (the
/// API version, the role, `js-` and the first 32 hex digits of the agent id's SHA-256, the
/// lifetime in seconds and the session policy), every value percent-encoded so that the body
/// holds only unreserved bytes, `=`, `&` and `%`; and an `Authorization` that is exactly the
/// Signature Version 4 of that request for the service `sts` in the region, with the mint key,
/// which the fake checks before it issues. It catches an unsigned request or one signed for
/// `s3`, an unencoded `Policy` and a wrong action or version.
#[test]
fn the_sts_request_is_signed_and_well_formed() {
    let rig = Rig::new();
    let credential = rig.minter().mint(&request("a/b", LONG_HOST)).unwrap();
    assert_eq!(credential.expires_at, at(START + 3600));
    let sent = rig.sent();
    assert_eq!(sent.len(), 1);
    let call = &sent[0];
    assert_eq!(call.method, Method::Post);
    assert_eq!(call.url, format!("{URL}/"));
    assert_eq!(
        call.header("content-type"),
        Some("application/x-www-form-urlencoded")
    );
    assert_eq!(call.header("host"), Some("store.example.com"));
    assert_eq!(call.header("x-amz-content-sha256"), None);
    assert_eq!(call.header("x-amz-security-token"), None);

    let session = format!("js-{}", &sha256_hex(LONG_HOST.as_bytes())[..32]);
    assert_eq!(session.len(), 35);
    let policy = js_logmint::policy(&request("a/b", "x").bucket, &request("a/b", "x").prefix);
    assert!(policy.contains(POLICY_VERSION));
    let fields = form(call);
    let expected = [
        ("Action", "AssumeRole"),
        ("Version", STS_API_VERSION),
        ("RoleArn", ROLE),
        ("RoleSessionName", session.as_str()),
        ("DurationSeconds", "3600"),
        ("Policy", policy.as_str()),
    ];
    let expected: Vec<(String, String)> = expected
        .iter()
        .map(|(name, value)| (name.to_string(), value.to_string()))
        .collect();
    assert_eq!(fields, expected);
    let body = String::from_utf8(call.body.clone()).unwrap();
    assert!(body.contains(&format!("&Policy={}", uri_encode(&policy, false))));
    assert!(body.contains("RoleArn=arn%3Aaws%3Aiam%3A%3Aexample%3Arole%2Fjs-logs&"));
    let unreserved = |b: u8| b.is_ascii_alphanumeric() || b"-._~=&%".contains(&b);
    assert!(body.bytes().all(unreserved), "{body}");

    let headers = [(
        "content-type".to_owned(),
        "application/x-www-form-urlencoded".to_owned(),
    )];
    let unsigned = Unsigned {
        method: "POST",
        host: "store.example.com",
        path: "/",
        query: &[],
        headers: &headers,
        sign_also: &["content-type"],
        body: &call.body,
    };
    let signed_for = |service: &str| {
        let scope = Scope {
            region: REGION,
            service,
        };
        sign(&unsigned, &rig.mint_key, &scope, at(START))
            .header("authorization")
            .unwrap()
            .to_owned()
    };
    assert_eq!(
        call.header("authorization"),
        Some(signed_for("sts").as_str())
    );
    assert_ne!(signed_for("sts"), signed_for("s3"));
    assert!(
        signed_for("sts")
            .contains("/us-east-1/sts/aws4_request, SignedHeaders=content-type;host;x-amz-date,")
    );
}

/// Asserts that a request the fake's STS refuses comes back `Refused` with its status and
/// code: a key that is not a mint key (`AccessDenied`) and a lifetime STS does not take
/// (`ValidationError`, for 14 minutes). It catches a refusal read as a credential.
#[test]
fn a_refused_call_is_refused() {
    let rig = Rig::new();
    let not_a_mint_key = rig.fake.issue(js_logstore::fake::Policy {
        actions: Vec::new(),
        prefix: String::new(),
    });
    let refused = |status: u16, code: &str| -> Result<Credential, MintError> {
        Err(MintError::Refused(Refusal::Status {
            status,
            code: code.to_owned(),
        }))
    };
    let ask = request("a/b", "host-1");
    assert_eq!(
        rig.minter_with(not_a_mint_key).mint(&ask),
        refused(403, "AccessDenied")
    );
    let short = js_logmint::MintRequest {
        ttl: Duration::from_secs(14 * 60),
        ..ask
    };
    assert_eq!(rig.minter().mint(&short), refused(400, "ValidationError"));
}

/// Asserts that a credential shorter than asked, or already expiring, is refused: the fake
/// answers a 5-minute credential to a 1-hour request, and one expiring in 30 seconds, and both
/// are `Refused`; one of exactly half the hour is taken, as is one of the hour asked. It catches
/// a minter that hands out a credential about to die.
#[test]
fn a_credential_shorter_than_asked_or_already_expiring_is_refused() {
    let rig = Rig::new();
    let minter = rig.minter();
    let ask = request("a/b", "host-1");
    let too_short: Result<Credential, MintError> = Err(MintError::Refused(Refusal::Answer(
        "the credential expires within a minute or lives less than half of what was asked",
    )));
    for seconds in [300, 30, 1799] {
        rig.fake
            .answer_sts_with_lifetime(Some(Duration::from_secs(seconds)));
        assert_eq!(minter.mint(&ask), too_short, "{seconds} s");
    }
    rig.fake
        .answer_sts_with_lifetime(Some(Duration::from_secs(1800)));
    assert_eq!(minter.mint(&ask).unwrap().expires_at, at(START + 1800));
    rig.fake.answer_sts_with_lifetime(None);
    assert_eq!(minter.mint(&ask).unwrap().expires_at, at(START) + HOUR);
}

/// Asserts the STS endpoint: its URL is the base and `/`, it carries the region, a URL the
/// endpoint rules refuse is refused with their reason, and a minter's `Debug` shows the key id
/// and never the mint key's secret. It catches a call sent to a path or a secret printed.
#[test]
fn the_sts_endpoint_and_the_minter_print_no_secret() {
    let endpoint = StsEndpoint::parse("https://sts.example.com:8443/", REGION).unwrap();
    assert_eq!(endpoint.url(), "https://sts.example.com:8443/");
    assert_eq!(endpoint.region(), REGION);
    assert_eq!(
        StsEndpoint::parse("ftp://sts.example.com", REGION),
        Err(EndpointError::Scheme)
    );
    assert_eq!(
        StsEndpoint::parse("https://sts.example.com/path", REGION),
        Err(EndpointError::Path)
    );
    assert_eq!(
        StsEndpoint::parse("https://sts.example.com", "US"),
        Err(EndpointError::Region)
    );
    let rig = Rig::new();
    let shown = format!("{:?}", rig.minter());
    assert!(shown.contains(&rig.mint_key.access_key_id), "{shown}");
    assert!(shown.contains(ROLE), "{shown}");
    assert!(!shown.contains(&rig.mint_key.secret_access_key), "{shown}");
}
