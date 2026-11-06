//! What every call shares: how a request is signed and sent, how an error answer is told,
//! how a transport failure is told, and how the XML of an answer is read.

use std::io;
use std::time::{Duration, SystemTime};

use js_logstore::sigv4::{Scope, Unsigned, sign};
use js_logstore::{Credential, TransportError};
use js_provider_ec2::{ApiError, DescribeFilter, Ec2Api, InstanceId, Tag};

use crate::script::{
    KEY_ID, NOW_SECONDS, REGION, SECRET, SESSION, answer, param, region, rig, status, xml,
};

/// Asserts that the signature of a request is the Signature Version 4 of exactly the
/// method, host, path, headers and body the request carries, for the fixed key, the
/// service and region, and the clock's time: the same call signed again here gives the same
/// headers. It catches a request signed over other bytes than it sends.
#[test]
fn a_request_carries_the_signature_of_what_it_sends() {
    let (client, script) = rig(vec![answer(&xml("DescribeInstanceTypesResponse", ""))]);
    client.instance_types(&region(REGION)).unwrap();
    let request = &script.sent()[0];
    let content_type = (
        "content-type".to_owned(),
        "application/x-www-form-urlencoded; charset=utf-8".to_owned(),
    );
    let credential = Credential {
        access_key_id: KEY_ID.to_owned(),
        secret_access_key: SECRET.to_owned(),
        session_token: Some(SESSION.to_owned()),
        expires_at: SystemTime::UNIX_EPOCH + Duration::from_secs(u64::from(u32::MAX)),
    };
    let signed = sign(
        &Unsigned {
            method: "POST",
            host: "ec2.region-a.amazonaws.com",
            path: "/",
            query: &[],
            headers: &[content_type],
            sign_also: &["content-type"],
            body: &request.body,
        },
        &credential,
        &Scope {
            region: REGION,
            service: "ec2",
        },
        SystemTime::UNIX_EPOCH + Duration::from_secs(NOW_SECONDS),
    );
    for (name, value) in &signed.headers {
        assert_eq!(request.header(name), Some(value.as_str()), "{name}");
    }
}

fn error_body(code: &str) -> String {
    format!(
        "<Response><Errors><Error><Code>{code}</Code><Message>private text</Message></Error></Errors><RequestID>0</RequestID></Response>"
    )
}

/// Asserts that an error answer is told by the cloud's code alone, whatever its HTTP
/// status (a 4xx and a 5xx alike), in both XML shapes (`Response/Errors/Error` and
/// `ErrorResponse/Error`): a throttle is throttled, a denial denied, any other code
/// refused, and the message of the answer is in no error text. It catches an error mapped
/// from the status and one that keeps the cloud's message.
#[test]
fn an_xml_error_answer_is_its_code_whatever_the_status() {
    let other = "<ErrorResponse><Error><Type>Sender</Type><Code>Blocked</Code><Message>private text</Message></Error></ErrorResponse>";
    let cases = [
        (400, error_body("InsufficientInstanceCapacity")),
        (500, error_body("InsufficientInstanceCapacity")),
        (503, error_body("RequestLimitExceeded")),
        (403, error_body("UnauthorizedOperation")),
        (403, other.to_owned()),
    ];
    for (code, body) in cases {
        let (client, _) = rig(vec![status(code, &body)]);
        let error = client.instance_types(&region(REGION)).unwrap_err();
        let given = body
            .split("<Code>")
            .nth(1)
            .unwrap()
            .split("</Code>")
            .next()
            .unwrap();
        assert_eq!(error, ApiError::answered(given));
        assert!(!format!("{error:?}{error}").contains("private text"));
    }
    let (client, _) = rig(vec![status(503, &error_body("RequestLimitExceeded"))]);
    assert!(matches!(
        client.instance_types(&region(REGION)),
        Err(ApiError::Throttled { .. })
    ));
    let (client, _) = rig(vec![status(403, &error_body("AuthFailure"))]);
    assert!(matches!(
        client.instance_types(&region(REGION)),
        Err(ApiError::Denied { .. })
    ));
}

/// Asserts that an error answer with no code (a body that is not XML, an empty body, an
/// XML document with no `Code`, or an empty `Code`) is unreadable and names its status.
/// It catches an unreadable error turned into a refusal or a success.
#[test]
fn an_error_answer_with_no_code_is_unreadable_with_its_status() {
    let bodies = [
        "<html><body>Bad Gateway</body></html>",
        "",
        "<Response><Errors/></Response>",
        "<Response><Errors><Error><Code> </Code></Error></Errors></Response>",
        "plain text <",
    ];
    for body in bodies {
        let (client, _) = rig(vec![status(502, body)]);
        assert_eq!(
            client.instance_types(&region(REGION)),
            Err(ApiError::Unreadable(
                "an error answer with no code (HTTP 502)".to_owned()
            )),
            "{body}"
        );
    }
}

/// Asserts that a timeout of the transport is `ApiError::Timeout` and that any other
/// transport failure is a `Transport` error that holds the client's own words and no
/// request detail, and that neither is retried: one request is sent, and the answers
/// left in the script are not used. It catches a retry and a timeout told as a refusal.
#[test]
fn a_transport_failure_is_told_by_kind_and_never_retried() {
    let (client, script) = rig(vec![Err(TransportError::Timeout), answer("")]);
    assert_eq!(
        client.instance_types(&region(REGION)),
        Err(ApiError::Timeout)
    );
    assert_eq!(script.sent().len(), 1);
    assert_eq!(script.left(), 1);

    for failure in [
        TransportError::Io(io::ErrorKind::ConnectionReset),
        TransportError::TooLarge,
        TransportError::Request,
        TransportError::Exchange,
    ] {
        let words = failure.to_string();
        let (client, _) = rig(vec![Err(failure)]);
        assert_eq!(
            client.instance_types(&region(REGION)),
            Err(ApiError::Transport(words))
        );
    }
}

/// Asserts that a success answer that is not a document (bytes that are not UTF-8, text
/// that is not XML, an unclosed element, a mismatched end tag, an undefined entity, an
/// empty body) is unreadable, never an empty list. It catches an answer that fails to
/// parse read as nothing found.
#[test]
fn a_success_answer_that_is_not_a_document_is_unreadable() {
    let bodies: [&[u8]; 6] = [
        &[0xff, 0xfe, 0x3c],
        b"just text",
        b"<a><b>",
        b"<a><b></a>",
        b"<a>&nothing;</a>",
        b"",
    ];
    for body in bodies {
        let (client, _) = rig(vec![Ok(js_logstore::HttpResponse {
            status: 200,
            headers: Vec::new(),
            body: body.to_vec(),
        })]);
        let result = client.describe_instances(&DescribeFilter::Tags(vec![]));
        assert!(
            matches!(result, Err(ApiError::Unreadable(_))),
            "{body:?}: {result:?}"
        );
    }
}

/// Asserts that a tag filter with a value of characters a form must escape (space, `&`,
/// `=`, `%`, `+`, a non-ASCII letter) arrives intact. It catches a body built without
/// escaping, which would split one parameter into two.
#[test]
fn a_filter_value_with_special_characters_arrives_intact() {
    let (client, script) = rig(vec![answer(&xml("DescribeInstancesResponse", ""))]);
    let value = "a b&c=d%e+f\u{e9}";
    let filter = DescribeFilter::Tags(vec![Tag {
        key: "k".to_owned(),
        value: value.to_owned(),
    }]);
    assert_eq!(client.describe_instances(&filter), Ok(vec![]));
    assert_eq!(
        param(&script.sent()[0], "Filter.1.Value.1").as_deref(),
        Some(value)
    );
}

/// Asserts that the debug text of the client names its region and nothing else, and that a
/// terminate of no ids still sends one request. It catches a debug text that prints the
/// transport or the key.
#[test]
fn the_debug_text_of_the_client_is_its_region() {
    let (client, script) = rig(vec![answer("")]);
    assert_eq!(
        format!("{client:?}"),
        "HttpEc2 { region: Region(\"region-a\"), .. }"
    );
    let none: [InstanceId; 0] = [];
    assert_eq!(client.terminate_instances(&none), Ok(()));
    assert_eq!(script.sent().len(), 1);
}
