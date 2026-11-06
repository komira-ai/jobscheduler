//! No key, token or signature reaches a debug text, an error or a log line.

use std::sync::{Arc, Mutex};

use js_provider_ec2::{ApiError, Credentials, Ec2Api, StaticKeys};
use tracing_subscriber::fmt::MakeWriter;

use crate::script::{Answer, answer, clock, over, region, status, xml};

/// The secret access key of the test key.
const SECRET: &str = "TOPSECRETACCESSKEY0123456789";

/// The session token of the test key.
const TOKEN: &str = "TOPSECRETSESSIONTOKEN0123456789";

/// A writer into a buffer the test reads back.
#[derive(Clone, Default)]
struct Lines(Arc<Mutex<Vec<u8>>>);

impl std::io::Write for Lines {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(bytes);
        Ok(bytes.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl<'a> MakeWriter<'a> for Lines {
    type Writer = Self;

    fn make_writer(&'a self) -> Self {
        self.clone()
    }
}

/// Asserts that the client, the keys, the credentials source, every kind of `ApiError`
/// (printed with `{:?}` and `{}`), the errors the client returns for a success, a refusal,
/// a throttle, a timeout and an unreadable answer, a role credential fetched from the
/// metadata service, and everything logged at TRACE while those calls run, hold none of
/// the secret access key, the session token, the role's secret and token, or the signature
/// of a request. It catches a debug text, an error or a log line that prints a key.
#[test]
fn credentials_never_appear_in_debug_or_logs() {
    let lines = Lines::default();
    let subscriber = tracing_subscriber::fmt()
        .with_ansi(false)
        .with_max_level(tracing::Level::TRACE)
        .with_writer(lines.clone())
        .finish();
    let keys = StaticKeys::new("AKIASECRETTEST", SECRET, Some(TOKEN));
    let throttle = "<Response><Errors><Error><Code>RequestLimitExceeded</Code><Message>TOPSECRETACCESSKEY0123456789</Message></Error></Errors></Response>";
    let answers: Vec<Answer> = vec![
        answer(&xml("DescribeInstanceTypesResponse", "")),
        status(503, throttle),
        status(
            400,
            "<Response><Errors><Error><Code>AuthFailure</Code></Error></Errors></Response>",
        ),
        Err(js_logstore::TransportError::Timeout),
        answer("<unclosed>"),
    ];
    let (client, script) = over(Credentials::Keys(keys.clone()), answers, clock());
    let (role_client, role_script) = over(
        Credentials::InstanceRole,
        vec![
            answer("ROLESESSIONTOKEN"),
            answer("role-name"),
            answer(
                "{\"AccessKeyId\":\"AKIAROLETEST\",\"SecretAccessKey\":\"ROLESECRETVALUE\",\"Token\":\"ROLETOKENVALUE\",\"Expiration\":\"2026-11-05T03:00:00Z\"}",
            ),
            answer(&xml("DescribeInstanceTypesResponse", "")),
        ],
        clock(),
    );

    let mut shown = Vec::new();
    tracing::subscriber::with_default(subscriber, || {
        let results = [
            client.instance_types(&region("region-a")).map(|_| ()),
            client.instance_types(&region("region-a")).map(|_| ()),
            client.instance_types(&region("region-a")).map(|_| ()),
            client.instance_types(&region("region-a")).map(|_| ()),
            client.instance_types(&region("region-a")).map(|_| ()),
            role_client.instance_types(&region("region-a")).map(|_| ()),
        ];
        for result in results {
            if let Err(error) = result {
                shown.push(format!("{error:?} {error}"));
            }
        }
        shown.push(format!("{client:?} {role_client:?}"));
        assert_eq!(
            format!("{client:?}"),
            "HttpEc2 { region: Region(\"region-a\"), .. }"
        );
        assert_eq!(
            format!("{keys:?}"),
            "StaticKeys { access_key_id: \"AKIASECRETTEST\", .. }"
        );
        shown.push(format!("{keys:?} {:?}", Credentials::Keys(keys.clone())));
        shown.push(format!("{:?}", Credentials::InstanceRole));
    });
    let every_error = [
        ApiError::answered("RequestLimitExceeded"),
        ApiError::answered("AuthFailure"),
        ApiError::answered("InvalidParameterValue"),
        ApiError::Transport("the connection failed".to_owned()),
        ApiError::Unreadable("a field is missing".to_owned()),
        ApiError::Incomplete,
        ApiError::Timeout,
        ApiError::NoCredential,
    ];
    for error in &every_error {
        shown.push(format!("{error:?} {error}"));
    }
    for request in script.sent().iter().chain(role_script.sent().iter()) {
        shown.push(format!("{request:?}"));
    }

    let signatures: Vec<String> = script
        .sent()
        .iter()
        .map(|request| {
            let authorization = request.header("authorization").unwrap();
            authorization
                .rsplit("Signature=")
                .next()
                .unwrap()
                .to_owned()
        })
        .collect();
    assert_eq!(signatures.len(), 5);
    let logged = String::from_utf8(lines.0.lock().unwrap().clone()).unwrap();
    assert!(
        logged.contains("the cloud answered"),
        "the calls were logged"
    );
    shown.push(logged);
    for text in &shown {
        for secret in [
            SECRET,
            TOKEN,
            "ROLESESSIONTOKEN",
            "ROLESECRETVALUE",
            "ROLETOKENVALUE",
        ] {
            assert!(!text.contains(secret), "{secret} appears in {text}");
        }
        for signature in &signatures {
            assert!(
                !text.contains(signature.as_str()),
                "a signature appears in {text}"
            );
        }
    }
}
