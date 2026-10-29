//! The host credential on the wire: the optional `Welcome.host_credential`, the hello's
//! `accepts_host_credential`, and the type that keeps the secret out of every printed
//! form.

mod common;

use common::{hello, welcome};
use js_agent_wire::{Hello, HelloReply, HostCredential, Rule, Welcome};
use serde_json::json;

const SECRET: &str = "c3ed1c0f0000000000000000000000000000000000000000000000000000beef";

fn credential() -> HostCredential {
    HostCredential::parse(SECRET).unwrap()
}

/// A hello that asks for a credential carries `accepts_host_credential: true`, and one
/// that does not carries no such field at all, so it is byte for byte the hello of an
/// agent from before the field. A welcome with a credential carries it as a bare string
/// beside the other fields, and one without carries nothing, so the welcome of a server
/// that issued none is the welcome it always was. It catches a renamed field, a `false`
/// that is written out, and a credential written as an object.
#[test]
fn the_two_fields_are_optional_on_the_wire() {
    let mut asking = hello();
    asking.accepts_host_credential = true;
    let sent = serde_json::to_value(&asking).unwrap();
    assert_eq!(sent["accepts_host_credential"], json!(true));
    let quiet = serde_json::to_value(hello()).unwrap();
    assert!(quiet.get("accepts_host_credential").is_none(), "{quiet}");

    let mut issued = welcome();
    issued.host_credential = Some(credential());
    let sent = serde_json::to_value(&issued).unwrap();
    assert_eq!(sent["host_credential"], json!(SECRET));
    let plain = serde_json::to_value(welcome()).unwrap();
    assert!(plain.get("host_credential").is_none(), "{plain}");

    let back: Welcome = serde_json::from_value(sent).unwrap();
    assert_eq!(back, issued);
    let back: Hello = serde_json::from_value(serde_json::to_value(&asking).unwrap()).unwrap();
    assert!(back.accepts_host_credential);
}

/// A welcome inside a tagged hello reply carries the credential in the same place. It
/// catches a reply type that drops the field when the welcome is wrapped.
#[test]
fn a_hello_reply_carries_the_credential() {
    let mut issued = welcome();
    issued.host_credential = Some(credential());
    let reply = serde_json::to_value(HelloReply::Welcome(issued)).unwrap();
    assert_eq!(reply["type"], json!("welcome"));
    assert_eq!(reply["host_credential"], json!(SECRET));
}

/// Neither the credential's `Debug` nor the welcome's prints the secret, also inside a
/// hello reply, and the type has no other printed form. It catches a derived `Debug` on
/// either type, which would put the credential in a log line or a panic message.
#[test]
fn the_secret_is_in_no_debug_text() {
    let mut issued = welcome();
    issued.host_credential = Some(credential());
    let texts = [
        format!("{:?}", credential()),
        format!("{issued:?}"),
        format!("{issued:#?}"),
        format!("{:?}", HelloReply::Welcome(issued)),
    ];
    for text in texts {
        assert!(!text.contains(SECRET), "{text}");
        assert!(!text.contains("beef"), "{text}");
    }
    assert_eq!(format!("{:?}", credential()), "HostCredential(***)");
    assert!(format!("{:?}", welcome()).contains("host_credential: None"));
}

/// A credential outside the session alphabet, an empty one and one over the cap are
/// refused where the welcome is decoded, by the same rules as a session secret, and the
/// refusal names the field and the rule and nothing of the text. It catches a decoder that
/// accepts a credential that cannot be a header value, and an error that repeats the
/// secret.
#[test]
fn a_credential_is_checked_where_it_is_decoded() {
    let cases = [
        ("", Rule::Empty),
        ("has space", Rule::BadChars),
        ("-leading-dash", Rule::BadChars),
        ("ünicode", Rule::BadChars),
        (&"a".repeat(129), Rule::TooLong),
    ];
    for (text, rule) in cases {
        let refused = HostCredential::parse(text).unwrap_err();
        assert_eq!(refused.rule(), rule, "{text}");
        assert_eq!(refused.field(), "host_credential");
        assert_eq!(refused.code(), "invalid-welcome");
        let mut body = serde_json::to_value(welcome()).unwrap();
        body["host_credential"] = json!(text);
        let said = serde_json::from_value::<Welcome>(body)
            .unwrap_err()
            .to_string();
        assert!(said.contains("host_credential"), "{said}");
        assert!(text.is_empty() || !said.contains(text), "{said}");
    }
    assert!(HostCredential::parse(&"a".repeat(128)).is_ok());
}

/// Made from 256 random bits, the credential is 64 lower-case hex digits that parse
/// back to themselves, and `expose` is the text. It catches a hex that is upper case, short
/// or reversed.
#[test]
fn a_credential_made_of_bits_is_64_lower_case_hex_digits() {
    let mut bytes = [0u8; 32];
    bytes[0] = 0xab;
    bytes[1] = 0x01;
    bytes[31] = 0xff;
    let made = HostCredential::from_bytes(&bytes);
    let text = made.expose();
    assert_eq!(text.len(), 64);
    assert!(text.starts_with("ab01"), "{}", &text[..4]);
    assert!(text.ends_with("ff"));
    assert!(
        text.bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
    );
    assert_eq!(HostCredential::parse(text).unwrap(), made);
}

/// The welcome of a server from before the field, and a hello of an agent from before
/// it, decode with the defaults, and a welcome that carries a credential still passes the
/// agent's checks. It catches a field that is required, and a validation that rejects a
/// welcome for carrying one.
#[test]
fn the_fields_are_defaults_for_a_peer_that_lacks_them() {
    let mut body = serde_json::to_value(welcome()).unwrap();
    assert!(
        body.as_object_mut()
            .unwrap()
            .remove("host_credential")
            .is_none()
    );
    let old: Welcome = serde_json::from_value(body).unwrap();
    assert_eq!(old.host_credential, None);
    let mut issued = welcome();
    issued.host_credential = Some(credential());
    assert_eq!(issued.validate(), Ok(()));
}
