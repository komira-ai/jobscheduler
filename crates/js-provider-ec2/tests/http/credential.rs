//! Where the signing key comes from: given keys, the provider-standard variables, and the
//! role of the machine from the instance metadata service.

use std::cell::RefCell;
use std::collections::BTreeMap;
use std::io;
use std::time::Duration;

use js_logstore::{HttpRequest, TransportError};
use js_provider_ec2::{ApiError, Credentials, Ec2Api, StaticKeys};

use crate::script::{Answer, KEY_ID, REGION, answer, clock, over, region, status, xml};

/// The variables a lookup answers from.
fn lookup(vars: &'static [(&'static str, &'static str)]) -> impl Fn(&str) -> Option<String> {
    let map: BTreeMap<&str, &str> = vars.iter().copied().collect();
    move |name| map.get(name).map(|value| (*value).to_owned())
}

/// Asserts that the provider-standard variables give a key when the key id and the secret
/// are set, with a session token when that is set too, and give none when either is
/// absent or empty; and that an empty token counts as absent. It catches a key built from
/// an empty variable.
#[test]
fn the_standard_variables_give_a_key_when_the_id_and_secret_are_set() {
    let full = StaticKeys::from_lookup(lookup(&[
        ("AWS_ACCESS_KEY_ID", "AKIAFROMVARIABLES"),
        ("AWS_SECRET_ACCESS_KEY", "secret-from-variables"),
        ("AWS_SESSION_TOKEN", "token-from-variables"),
    ]))
    .unwrap();
    let text = format!("{full:?}");
    assert!(text.contains("AKIAFROMVARIABLES"));
    assert!(!text.contains("secret-from-variables") && !text.contains("token-from-variables"));
    let plain = StaticKeys::from_lookup(lookup(&[
        ("AWS_ACCESS_KEY_ID", "AKIAFROMVARIABLES"),
        ("AWS_SECRET_ACCESS_KEY", "secret-from-variables"),
        ("AWS_SESSION_TOKEN", ""),
    ]));
    assert!(plain.is_some());
    let missing: [&'static [(&'static str, &'static str)]; 4] = [
        &[],
        &[("AWS_ACCESS_KEY_ID", "AKIAFROMVARIABLES")],
        &[
            ("AWS_ACCESS_KEY_ID", ""),
            ("AWS_SECRET_ACCESS_KEY", "secret-from-variables"),
        ],
        &[
            ("AWS_ACCESS_KEY_ID", "AKIAFROMVARIABLES"),
            ("AWS_SECRET_ACCESS_KEY", ""),
        ],
    ];
    for vars in missing {
        assert!(StaticKeys::from_lookup(lookup(vars)).is_none(), "{vars:?}");
    }
}

/// Asserts that the lookup is asked for the three provider-standard names and no other
/// (`AWS_REGION` among them is not read). It catches a client that takes its region from
/// the environment.
#[test]
fn only_the_three_standard_names_are_read() {
    let asked = RefCell::new(Vec::new());
    let keys = StaticKeys::from_lookup(|name| {
        asked.borrow_mut().push(name.to_owned());
        Some("value".to_owned())
    });
    assert!(keys.is_some());
    let mut names = asked.into_inner();
    names.sort();
    assert_eq!(
        names,
        [
            "AWS_ACCESS_KEY_ID",
            "AWS_SECRET_ACCESS_KEY",
            "AWS_SESSION_TOKEN"
        ]
    );
}

/// Asserts that reading the process environment and choosing the standard credential give
/// one of the two sources and never fail, whatever the process has set (the result is not
/// compared: a test does not set the process environment).
#[test]
fn the_standard_credential_is_keys_or_the_role() {
    let keys = StaticKeys::from_env();
    let standard = Credentials::standard();
    assert!(matches!(
        (keys.is_some(), standard),
        (true, Credentials::Keys(_)) | (false, Credentials::InstanceRole)
    ));
}

/// Asserts that given keys are what a request is signed with: the key id is in the
/// credential scope of the authorization, a key with a session token sends it in
/// `x-amz-security-token`, and a key without one sends no such header. It catches a key
/// that drops its token.
#[test]
fn given_keys_sign_the_request_with_or_without_a_token() {
    for token in [Some("session-token-one"), None] {
        let keys = StaticKeys::new("AKIAGIVEN", "given-secret", token);
        let (client, script) = over(
            Credentials::Keys(keys),
            vec![answer(&xml("DescribeInstanceTypesResponse", ""))],
            clock(),
        );
        client.instance_types(&region(REGION)).unwrap();
        let request = &script.sent()[0];
        assert!(
            request
                .header("authorization")
                .unwrap()
                .contains("Credential=AKIAGIVEN/")
        );
        assert_eq!(request.header("x-amz-security-token"), token);
    }
}

/// The base of the instance metadata service.
const IMDS: &str = "http://169.254.169.254";

/// The role document of the metadata service, expiring at `expiration`.
fn role_document(key: &str, expiration: &str) -> String {
    format!(
        "{{\"Code\":\"Success\",\"Type\":\"AWS-HMAC\",\"AccessKeyId\":\"{key}\",\"SecretAccessKey\":\"role-secret-value\",\"Token\":\"role-token-{key}\",\"Expiration\":\"{expiration}\"}}"
    )
}

/// The three answers of a successful role fetch.
fn role_answers(key: &str, expiration: &str) -> Vec<Answer> {
    vec![
        answer("imds-session-token"),
        answer("role-name\nother-role"),
        answer(&role_document(key, expiration)),
    ]
}

fn ec2_page() -> Answer {
    answer(&xml("DescribeInstanceTypesResponse", ""))
}

fn is_imds(request: &HttpRequest) -> bool {
    request.url.starts_with(IMDS)
}

/// Asserts that with the role of the machine the client asks the metadata service for a
/// session token (a PUT naming its lifetime), then for the role's name with that token,
/// then for the role's credential, and signs the request with the credential it gets: the
/// role's key id in the authorization and its token in `x-amz-security-token`. It catches
/// a fetch that skips the session token, which the service refuses, and a request signed
/// with some other key.
#[test]
fn the_role_of_the_machine_is_fetched_over_imdsv2_and_signs_the_request() {
    let mut answers = role_answers("AKIAROLEONE", "2026-11-05T03:00:00Z");
    answers.push(ec2_page());
    let (client, script) = over(Credentials::InstanceRole, answers, clock());
    client.instance_types(&region(REGION)).unwrap();
    let sent = script.sent();
    assert_eq!(sent.len(), 4);
    assert_eq!(sent[0].method.as_str(), "PUT");
    assert_eq!(sent[0].url, format!("{IMDS}/latest/api/token"));
    assert_eq!(
        sent[0].header("x-aws-ec2-metadata-token-ttl-seconds"),
        Some("21600")
    );
    assert_eq!(sent[1].method.as_str(), "GET");
    assert_eq!(
        sent[1].url,
        format!("{IMDS}/latest/meta-data/iam/security-credentials/")
    );
    assert_eq!(
        sent[1].header("x-aws-ec2-metadata-token"),
        Some("imds-session-token")
    );
    assert_eq!(
        sent[2].url,
        format!("{IMDS}/latest/meta-data/iam/security-credentials/role-name")
    );
    assert_eq!(
        sent[2].header("x-aws-ec2-metadata-token"),
        Some("imds-session-token")
    );
    assert!(!is_imds(&sent[3]));
    assert!(
        sent[3]
            .header("authorization")
            .unwrap()
            .contains("Credential=AKIAROLEONE/")
    );
    assert_eq!(
        sent[3].header("x-amz-security-token"),
        Some("role-token-AKIAROLEONE")
    );
}

/// Asserts that a role credential is kept while it has more than five minutes to live (a
/// second call makes no metadata request), and fetched again once it has five minutes or
/// less, and that the new credential then signs. It catches a fetch on every call and a
/// credential used up to its last second.
#[test]
fn a_role_credential_is_kept_until_five_minutes_before_it_expires() {
    let wall = clock();
    let mut answers = role_answers("AKIAROLEONE", "2026-11-05T02:01:01Z");
    answers.extend([ec2_page(), ec2_page()]);
    answers.extend(role_answers("AKIAROLETWO", "2026-11-05T05:00:00Z"));
    answers.push(ec2_page());
    let (client, script) = over(Credentials::InstanceRole, answers, wall.clone());
    client.instance_types(&region(REGION)).unwrap();
    wall.advance(Duration::from_secs(54 * 60));
    client.instance_types(&region(REGION)).unwrap();
    assert_eq!(script.sent().len(), 5, "kept with six minutes left");
    wall.advance(Duration::from_secs(60));
    client.instance_types(&region(REGION)).unwrap();
    let sent = script.sent();
    assert_eq!(sent.len(), 9, "fetched again with five minutes left");
    assert!(
        sent[8]
            .header("authorization")
            .unwrap()
            .contains("Credential=AKIAROLETWO/")
    );
}

/// Asserts that every way the metadata service can fail to hand out a credential is
/// `NoCredential` with no EC2 request sent: the session token refused or unreachable, the
/// role list failing or empty, the credential failing, answers that are not UTF-8, a
/// document that is not JSON, one missing each field, and one with an expiry that is not a
/// time; and that a failure is not kept, so the next call tries again and succeeds. It
/// catches a failed fetch that signs with an empty key.
#[test]
fn every_failure_of_the_metadata_service_is_no_credential_and_is_retried_next_call() {
    let document = |field: &str| {
        let mut value: serde_json::Value =
            serde_json::from_str(&role_document("AKIAROLEONE", "2026-11-05T03:00:00Z")).unwrap();
        value.as_object_mut().unwrap().remove(field);
        value.to_string()
    };
    let bytes = |body: &[u8]| {
        Ok(js_logstore::HttpResponse {
            status: 200,
            headers: Vec::new(),
            body: body.to_vec(),
        })
    };
    let ok_token = || answer("imds-session-token");
    let ok_roles = || answer("role-name");
    let failures: Vec<Vec<Answer>> = vec![
        vec![Err(TransportError::Io(io::ErrorKind::TimedOut))],
        vec![status(404, "")],
        vec![bytes(&[0xff])],
        vec![ok_token(), status(500, "")],
        vec![ok_token(), answer("")],
        vec![ok_token(), ok_roles(), Err(TransportError::Timeout)],
        vec![ok_token(), ok_roles(), status(404, "")],
        vec![ok_token(), ok_roles(), answer("not json")],
        vec![ok_token(), ok_roles(), answer(&document("AccessKeyId"))],
        vec![ok_token(), ok_roles(), answer(&document("SecretAccessKey"))],
        vec![ok_token(), ok_roles(), answer(&document("Token"))],
        vec![ok_token(), ok_roles(), answer(&document("Expiration"))],
        vec![
            ok_token(),
            ok_roles(),
            answer(&role_document("AKIAROLEONE", "tomorrow")),
        ],
    ];
    for failure in failures {
        let used = failure.len();
        let (client, script) = over(Credentials::InstanceRole, failure, clock());
        assert_eq!(
            client.instance_types(&region(REGION)),
            Err(ApiError::NoCredential)
        );
        let sent = script.sent();
        assert_eq!(sent.len(), used);
        assert!(sent.iter().all(is_imds));
        script.push(role_answers("AKIAROLEONE", "2026-11-05T03:00:00Z"));
        script.push(vec![ec2_page()]);
        assert_eq!(client.instance_types(&region(REGION)), Ok(vec![]));
    }
}

/// Asserts that the key id of given keys is in their debug text and the secret is not.
#[test]
fn the_debug_text_of_keys_names_only_the_key_id() {
    let keys = StaticKeys::new(KEY_ID, "very-secret-value", Some("very-secret-token"));
    let text = format!("{keys:?} {:?}", Credentials::Keys(keys.clone()));
    assert!(text.contains(KEY_ID));
    assert!(!text.contains("very-secret"));
    assert_eq!(format!("{:?}", Credentials::InstanceRole), "InstanceRole");
}
