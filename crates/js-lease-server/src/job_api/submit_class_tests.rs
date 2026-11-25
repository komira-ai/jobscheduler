//! `SubmitJob` of a job that names its size class (`spec.size_class`, `sizeClass` over JSON),
//! over both transports through the mounted handler: a class of up to 512 bytes of printable
//! ASCII is recorded and answered byte for byte, and any other is refused naming the field
//! and the fault, never echoing the class.

use super::*;

/// A build target's full name as Buck2 writes it, with the kind of its step: exactly `len`
/// bytes, padded with words.
fn target(len: usize) -> String {
    let name = format!("komira//{}json:json mojo_compile build", "src/".repeat(40));
    let name = format!("{name}{}", " x".repeat((len - name.len()) / 2));
    let name = format!("{name}{}", "y".repeat(len - name.len()));
    assert_eq!(name.len(), len);
    name
}

/// The job `id` naming the class `class`.
fn with_class(id: &str, class: &str) -> v1::JobSpec {
    v1::JobSpec {
        size_class: class.to_owned(),
        ..wire(id)
    }
}

/// Asserts that a 200-byte class (slashes, a colon, spaces, upper and lower case) and a
/// 512-byte one are recorded byte for byte and answered as sent, over gRPC and over JSON, and
/// that a job that names none records none. Catches a class lowered, cut, hashed or dropped
/// between the request and the record or the answer, and an empty field taken as a class.
#[tokio::test]
async fn a_class_of_up_to_512_printable_bytes_is_recorded_and_answered_byte_for_byte() {
    let (_, writes) = over_fake(Fixed::new(true)).await;
    let long = format!("Komira//Src:{}", target(200)[12..].to_owned());
    let cases = [
        (Via::Grpc, "class-grpc", target(200)),
        (Via::Json, "class-json", long),
        (Via::Grpc, "class-max", target(512)),
    ];
    for (via, named, class) in cases {
        assert_eq!(class.len(), if named == "class-max" { 512 } else { 200 });
        let job = writes
            .submit(via, &with_class(named, &class))
            .await
            .unwrap();
        assert_eq!(job.spec.unwrap().size_class, class, "{via:?}");
        let stored = writes.jobs.get_job(&id(named)).unwrap().unwrap();
        let recorded = stored.spec.size_class.expect("a class");
        assert_eq!(recorded.as_str().as_bytes(), class.as_bytes(), "{via:?}");
    }
    let job = writes.submit(Via::Json, &wire("class-none")).await.unwrap();
    assert_eq!(job.spec.unwrap().size_class, "");
    let stored = writes.jobs.get_job(&id("class-none")).unwrap().unwrap();
    assert_eq!(stored.spec.size_class, None);
    assert_eq!(rows(&writes.jobs), 4);
}

/// Asserts that the gateway takes the class under its JSON name `sizeClass`, as a client
/// that writes the body by hand sends it. Catches the field reachable over gRPC alone.
#[tokio::test]
async fn the_gateway_takes_the_class_as_size_class_in_json() {
    let (_, writes) = over_fake(Fixed::new(true)).await;
    let request = v1::SubmitJobRequest {
        spec: Some(wire("class-raw")),
    };
    let mut body = to_json("SubmitJobRequest", &request);
    body["spec"]["sizeClass"] = json!("cell//pkg:name rust_compile build");
    let (status, _) = writes.post("/v1/jobs", &body).await;
    assert_eq!(status, StatusCode::CREATED);
    let stored = writes.jobs.get_job(&id("class-raw")).unwrap().unwrap();
    assert_eq!(
        stored.spec.size_class.as_ref().map(|class| class.as_str()),
        Some("cell//pkg:name rust_compile build")
    );
}

/// Asserts that a class one byte over 512 is refused `bad-length`, and one holding a line
/// break, a tab, DEL or a byte above ASCII `not-printable-ascii`, over gRPC
/// (`INVALID_ARGUMENT`, `field: spec.sizeClass: <code>`) and over JSON (400 naming
/// `spec.sizeClass: <code>`), and that nothing is recorded. Catches a class that would split a
/// log line or overrun the field kept, and a refusal that does not say why.
#[tokio::test]
async fn a_class_over_512_bytes_or_with_a_byte_outside_printable_ascii_is_refused_with_why() {
    let (_, writes) = over_fake(Fixed::new(true)).await;
    let refused = [
        (target(513), "bad-length"),
        ("a\nb".to_owned(), "not-printable-ascii"),
        ("a\tb".to_owned(), "not-printable-ascii"),
        ("a\u{7f}".to_owned(), "not-printable-ascii"),
        ("caf\u{e9}".to_owned(), "not-printable-ascii"),
    ];
    for (class, code) in refused {
        let spec = with_class("class-bad", &class);
        let status = writes.submit_grpc(&spec).await.unwrap_err();
        assert_eq!(status.code(), Code::InvalidArgument, "{code}");
        assert_eq!(status.message(), format!("field: spec.sizeClass: {code}"));
        let (http, body) = writes.submit(Via::Json, &spec).await.unwrap_err();
        assert_eq!(
            (http, body["field"].clone()),
            (
                StatusCode::BAD_REQUEST,
                json!(format!("spec.sizeClass: {code}"))
            )
        );
        let start = class.get(..3).unwrap_or(&class);
        assert!(!body.to_string().contains(start), "{body}");
    }
    assert_eq!(rows(&writes.jobs), 0);
}
