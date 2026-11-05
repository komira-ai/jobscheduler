//! The fake store's STS, driven by raw requests: what `AssumeRole` issues, what it refuses and
//! in which order, and the session policies it reads.

use std::time::Duration;

use js_logstore::fake::{
    Action, FakeStore, MAX_DURATION_SECS, MIN_DURATION_SECS, MINTING_KEY_TTL, Policy, Status,
};
use js_logstore::sigv4::uri_encode;
use js_logstore::{
    Clock, Credential, HttpRequest, HttpResponse, Metadata, Method, StoreError, Transport,
};

use crate::support::{ALL, Raw, Rig, START, at, code, policy};

/// The content type of a form.
const FORM: &str = "application/x-www-form-urlencoded";

/// A session policy that allows put and delete under `a/b/` of the bucket `logs`.
const POLICY: &str = r#"{"Version":"v","Statement":[{"Effect":"Allow","Action":["s3:PutObject","s3:DeleteObject"],"Resource":"arn:aws:s3:::logs/a/b/*"}]}"#;

/// The form fields of a good call for `seconds`.
fn fields(seconds: &str) -> Vec<(&'static str, String)> {
    vec![
        ("Action", "AssumeRole".to_owned()),
        ("Version", "v".to_owned()),
        ("RoleArn", "arn:aws:iam::example:role/js-logs".to_owned()),
        ("RoleSessionName", "js-0123456789abcdef".to_owned()),
        ("DurationSeconds", seconds.to_owned()),
        ("Policy", POLICY.to_owned()),
    ]
}

/// `fields` as a form body, each name and value percent-encoded.
fn form(fields: &[(&str, String)]) -> Vec<u8> {
    fields
        .iter()
        .map(|(name, value)| format!("{}={}", uri_encode(name, false), uri_encode(value, false)))
        .collect::<Vec<_>>()
        .join("&")
        .into_bytes()
}

/// A `POST /` with `body` and the content type `kind`, signed for `sts` with `key` at `now`.
fn call(
    key: &Credential,
    kind: Option<&str>,
    body: Vec<u8>,
    now: std::time::SystemTime,
) -> HttpRequest {
    let raw = Raw {
        sign_also: &["content-type"],
        body,
        ..Raw::new(Method::Post, "/")
    };
    let raw = match kind {
        Some(kind) => raw.header("content-type", kind),
        None => raw,
    };
    raw.sign_for(key, now, "sts")
}

/// The fake's answer to `request`.
fn answer(fake: &FakeStore, request: HttpRequest) -> HttpResponse {
    fake.send(request).unwrap()
}

/// The text of the element `name` of an answer's body.
fn element(response: &HttpResponse, name: &str) -> String {
    let text = String::from_utf8(response.body.clone()).unwrap();
    let open = format!("<{name}>");
    let (_, rest) = text.split_once(&open).unwrap();
    rest.split_once(&format!("</{name}>")).unwrap().0.to_owned()
}

/// The credential an `AssumeRole` answer carries.
fn issued(response: &HttpResponse, expires_at: std::time::SystemTime) -> Credential {
    Credential {
        access_key_id: element(response, "AccessKeyId"),
        secret_access_key: element(response, "SecretAccessKey"),
        session_token: Some(element(response, "SessionToken")),
        expires_at,
    }
}

/// How many credentials the fake holds, as its `Debug` says.
fn held(fake: &FakeStore) -> String {
    let shown = format!("{fake:?}");
    let (_, rest) = shown.split_once("credentials: ").unwrap();
    rest.split(|c: char| !c.is_ascii_digit())
        .next()
        .unwrap()
        .to_owned()
}

/// Asserts what `AssumeRole` issues: a minting key has no session token and lives
/// `MINTING_KEY_TTL`, and may not call S3; a call signed with it for `sts` answers 200 with an
/// XML credential that has a session token and expires `DurationSeconds` after the fake's
/// clock (900 and 43,200 at the bounds), or after what `answer_sts_with_lifetime` says until it
/// is cleared; the credential may do exactly what the call's session policy says (put under
/// `a/b/`, not under `a/bc/`, no read); `expire` makes it answer `ExpiredToken` and ignores an
/// unknown id; and `issue_for` gives a credential the lifetime asked. It catches a fake that
/// issues a credential wider than the policy sent, or for another lifetime.
#[test]
fn an_assume_role_call_issues_a_credential_with_the_policy_it_names() {
    let rig = Rig::new();
    let now = rig.clock.now();
    let key = rig.fake.minting_key();
    assert_eq!(key.session_token, None);
    assert_eq!(key.expires_at, now + MINTING_KEY_TTL);
    let as_s3 = rig
        .client(key.clone())
        .put("a/b/x", b"x".to_vec(), &Metadata::new());
    assert_eq!(
        as_s3,
        Err(StoreError::Denied {
            status: 403,
            code: "AccessDenied".to_owned()
        })
    );

    let body = form(&fields("900"));
    let response = answer(&rig.fake, call(&key, Some(FORM), body, now));
    assert_eq!(
        response.status,
        200,
        "{}",
        String::from_utf8_lossy(&response.body)
    );
    assert_eq!(response.header("content-type"), Some("application/xml"));
    assert_eq!(element(&response, "Expiration"), "2027-01-15T08:15:00.000Z");
    let credential = issued(&response, at(START + 900));
    assert!(
        credential
            .session_token
            .as_deref()
            .is_some_and(|token| !token.is_empty())
    );
    let client = rig.client(credential.clone());
    let none = Metadata::new();
    client
        .put("a/b/job-1/stdout.log", b"x".to_vec(), &none)
        .unwrap();
    client.delete("a/b/job-1/stdout.log").unwrap();
    let denied = Err(StoreError::Denied {
        status: 403,
        code: "AccessDenied".to_owned(),
    });
    assert_eq!(client.put("a/bc/x", b"x".to_vec(), &none), denied);
    assert_eq!(client.get_range("a/b/x", 0, 1).map(|_| ()), denied);

    let longest = answer(
        &rig.fake,
        call(&key, Some(FORM), form(&fields("43200")), now),
    );
    assert_eq!(element(&longest, "Expiration"), "2027-01-15T20:00:00.000Z");
    rig.fake
        .answer_sts_with_lifetime(Some(Duration::from_secs(300)));
    let short = answer(
        &rig.fake,
        call(&key, Some(FORM), form(&fields("3600")), now),
    );
    assert_eq!(element(&short, "Expiration"), "2027-01-15T08:05:00.000Z");
    rig.fake.answer_sts_with_lifetime(None);
    let asked = answer(
        &rig.fake,
        call(&key, Some(FORM), form(&fields("3600")), now),
    );
    assert_eq!(element(&asked, "Expiration"), "2027-01-15T09:00:00.000Z");

    let before = held(&rig.fake);
    rig.fake.expire("FAKEKEY-NONE");
    assert_eq!(held(&rig.fake), before);
    assert!(client.put("a/b/x", b"x".to_vec(), &none).is_ok());
    rig.fake.expire(&credential.access_key_id);
    assert_eq!(
        client.put("a/b/x", b"x".to_vec(), &none),
        Err(StoreError::Expired {
            code: "ExpiredToken".to_owned()
        })
    );

    let brief = rig
        .fake
        .issue_for(policy(&ALL, ""), Duration::from_secs(10));
    assert_eq!(brief.expires_at, now + Duration::from_secs(10));
    assert!(brief.session_token.is_some());
}

/// Asserts the refusals of `AssumeRole`, in the fake's order, each changing nothing (the fake
/// holds as many credentials after as before): a scripted failure first; then the signature (an
/// unknown key, a body changed after signing); a key that is not a minting key
/// (`AccessDenied`); a body that is not a form (no content type, another one, a pair without
/// `=`, a bad escape, bytes that are not UTF-8: `MalformedQueryString`); an action other than
/// `AssumeRole` or none (`InvalidAction`); a missing or empty version, a role that is not an
/// ARN or is missing, a session name of 1 or 65 bytes or with a `/`, and a duration of 899,
/// 43,201, a word or none (`ValidationError`); and a policy that is missing or that the fake
/// cannot read (`MalformedPolicyDocument`). A `POST` that is not to `/` is not an STS call. It
/// catches a fake that issues for a call STS would refuse.
#[test]
fn an_assume_role_call_is_refused_in_order() {
    let rig = Rig::new();
    let now = rig.clock.now();
    let key = rig.fake.minting_key();
    let good = || form(&fields("900"));
    let before = held(&rig.fake);
    let refusal = |request: HttpRequest| {
        let response = answer(&rig.fake, request);
        (response.status, code(&response))
    };
    let said = |status: u16, code: &str| (status, code.to_owned());

    rig.fake.fail_next(1, Status::SlowDown);
    assert_eq!(
        refusal(call(&key, Some(FORM), good(), now)),
        said(503, "SlowDown")
    );
    let stranger = Credential {
        access_key_id: "AKIDUNKNOWN".to_owned(),
        ..key.clone()
    };
    assert_eq!(
        refusal(call(&stranger, Some(FORM), good(), now)),
        said(403, "InvalidAccessKeyId")
    );
    let mut changed = call(&key, Some(FORM), good(), now);
    changed.body.push(b'x');
    assert_eq!(refusal(changed), said(403, "SignatureDoesNotMatch"));
    let not_minting = rig.fake.issue(policy(&ALL, ""));
    assert_eq!(
        refusal(call(&not_minting, Some(FORM), good(), now)),
        said(403, "AccessDenied")
    );

    let malformed = said(400, "MalformedQueryString");
    assert_eq!(refusal(call(&key, None, good(), now)), malformed);
    assert_eq!(
        refusal(call(&key, Some("text/plain"), good(), now)),
        malformed
    );
    for body in [
        b"Action".to_vec(),
        b"Action=AssumeRole&Version".to_vec(),
        b"Action=%zz".to_vec(),
        b"%zz=AssumeRole".to_vec(),
        b"Action=\xff".to_vec(),
        Vec::new(),
    ] {
        assert_eq!(refusal(call(&key, Some(FORM), body, now)), malformed);
    }

    let with = |name: &str, value: Option<&str>| {
        let mut fields = fields("900");
        fields.retain(|(have, _)| *have != name);
        if let Some(value) = value {
            let name = fields_name(name);
            fields.push((name, value.to_owned()));
        }
        call(&key, Some(FORM), form(&fields), now)
    };
    let invalid_action = said(400, "InvalidAction");
    assert_eq!(
        refusal(with("Action", Some("GetCallerIdentity"))),
        invalid_action
    );
    assert_eq!(refusal(with("Action", None)), invalid_action);
    let validation = said(400, "ValidationError");
    let long_name = "n".repeat(65);
    for (name, value) in [
        ("Version", None),
        ("Version", Some("")),
        ("RoleArn", None),
        ("RoleArn", Some("role/js-logs")),
        ("RoleSessionName", None),
        ("RoleSessionName", Some("j")),
        ("RoleSessionName", Some(long_name.as_str())),
        ("RoleSessionName", Some("js/host")),
        ("DurationSeconds", None),
        ("DurationSeconds", Some("899")),
        ("DurationSeconds", Some("43201")),
        ("DurationSeconds", Some("an hour")),
    ] {
        assert_eq!(refusal(with(name, value)), validation, "{name} {value:?}");
    }
    let unreadable = said(400, "MalformedPolicyDocument");
    assert_eq!(refusal(with("Policy", None)), unreadable);
    assert_eq!(refusal(with("Policy", Some("{}"))), unreadable);
    assert_eq!(
        held(&rig.fake),
        format!("{}", before.parse::<u64>().unwrap() + 1)
    );

    let names_ok = ["js", "js-+=,.@_", &"n".repeat(64)];
    for name in names_ok {
        let response = answer(&rig.fake, with("RoleSessionName", Some(name)));
        assert_eq!(response.status, 200, "{name}");
    }
    assert_eq!(MIN_DURATION_SECS, 900);
    assert_eq!(MAX_DURATION_SECS, 43_200);

    let mut elsewhere = call(&key, Some(FORM), good(), now);
    elsewhere.url = "store.example.com/".to_owned();
    assert_eq!(refusal(elsewhere), said(400, "InvalidURI"));
}

/// The `'static` spelling of a form field's name.
fn fields_name(name: &str) -> &'static str {
    [
        "Action",
        "Version",
        "RoleArn",
        "RoleSessionName",
        "DurationSeconds",
        "Policy",
    ]
    .into_iter()
    .find(|have| *have == name)
    .unwrap()
}

/// The document of one statement with `statement`'s members, for the bucket `logs`.
fn one(statement: &str) -> String {
    format!(r#"{{"Version":"v","Statement":[{statement}]}}"#)
}

/// A statement with `effect`, `actions` and `resource`.
fn statement(effect: &str, actions: &str, resource: &str) -> String {
    format!(r#"{{"Effect":{effect},"Action":{actions},"Resource":{resource}}}"#)
}

/// Asserts which session policies the fake reads and what it reads them as: put and delete
/// under `a/b/`, every action it knows, and the whole bucket for `logs/*`; and that it reads
/// nothing (so grants nothing) for text that is not JSON, a document that is not an object or
/// has another member or lacks or mistypes `Version` or `Statement`, no statement or two, a
/// statement that is not an object or has another member (`Condition`) or `NotResource` in
/// place of `Resource`, an effect other than `Allow`, actions that are not an array of known
/// names or are none, a resource that is not a string, of another bucket, of a bucket whose
/// name only begins with `logs`, without the `/` after the bucket or the trailing `*`, or with
/// another `*`, `?` or `$`. It catches a fake that grants more than the text says.
#[test]
fn a_session_policy_is_read_exactly() {
    let put_delete = r#"["s3:PutObject","s3:DeleteObject"]"#;
    let read = |text: &str| Policy::from_session_policy(text, "logs");
    assert_eq!(
        read(POLICY),
        Some(policy(&[Action::Put, Action::Delete], "a/b/"))
    );
    let every = r#"["s3:PutObject","s3:DeleteObject","s3:GetObject","s3:ListBucket"]"#;
    let all = one(&statement(r#""Allow""#, every, r#""arn:aws:s3:::logs/*""#));
    assert_eq!(read(&all), Some(policy(&ALL, "")));

    let resource = r#""arn:aws:s3:::logs/a/*""#;
    let allow = r#""Allow""#;
    let refused = [
        "not json".to_owned(),
        format!("[{}]", one(&statement(allow, put_delete, resource))),
        format!(
            r#"{{"Version":"v","Id":"x","Statement":[{}]}}"#,
            statement(allow, put_delete, resource)
        ),
        format!(
            r#"{{"Statement":[{}]}}"#,
            statement(allow, put_delete, resource)
        ),
        format!(
            r#"{{"Version":1,"Statement":[{}]}}"#,
            statement(allow, put_delete, resource)
        ),
        format!(
            r#"{{"Version":"v","Statement":{}}}"#,
            statement(allow, put_delete, resource)
        ),
        r#"{"Version":"v","Statement":[]}"#.to_owned(),
        format!(
            r#"{{"Version":"v","Statement":[{0},{0}]}}"#,
            statement(allow, put_delete, resource)
        ),
        one(r#""Allow""#),
        one(&format!(
            r#"{{"Effect":"Allow","Action":{put_delete},"Resource":{resource},"Condition":{{}}}}"#
        )),
        one(&format!(
            r#"{{"Effect":"Allow","Action":{put_delete},"NotResource":{resource}}}"#
        )),
        one(&statement(r#""Deny""#, put_delete, resource)),
        one(&statement("true", put_delete, resource)),
        one(&statement(allow, r#""s3:PutObject""#, resource)),
        one(&statement(allow, "[]", resource)),
        one(&statement(allow, r#"["s3:*"]"#, resource)),
        one(&statement(allow, "[7]", resource)),
        one(&statement(
            allow,
            put_delete,
            r#"["arn:aws:s3:::logs/a/*"]"#,
        )),
        one(&statement(allow, put_delete, r#""arn:aws:s3:::other/a/*""#)),
        one(&statement(allow, put_delete, r#""arn:aws:s3:::logsx/a/*""#)),
        one(&statement(allow, put_delete, r#""arn:aws:s3:::logs*""#)),
        one(&statement(allow, put_delete, r#""arn:aws:s3:::logs/a/""#)),
        one(&statement(allow, put_delete, r#""arn:aws:s3::logs/a/*""#)),
        one(&statement(allow, put_delete, r#""arn:aws:s3:::logs/a*/*""#)),
        one(&statement(allow, put_delete, r#""arn:aws:s3:::logs/a?/*""#)),
        one(&statement(allow, put_delete, r#""arn:aws:s3:::logs/a$/*""#)),
    ];
    for text in &refused {
        assert_eq!(read(text), None, "{text}");
    }
}
