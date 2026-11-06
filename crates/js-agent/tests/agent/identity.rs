//! `--identity-source`: the cloud's signed identity the agent reads from a metadata service
//! at start and sends in its hello. The service is a stand-in on a loopback listener of port
//! 0, in the test process, that serves the paths of IMDSv2 or of the Compute Engine
//! metadata server and records every request; the agent reaches it through
//! `--identity-metadata-url`.

use std::collections::BTreeMap;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;

use js_agent_wire::limits::MAX_IDENTITY_DOCUMENT_LEN;
use js_agent_wire::{Hello, SignedIdentity};
use js_provider_ec2::{AwsIdentityVerifier, IdentityDocument, IdentityVerifier as _};
use js_provider_gce::{GoogleIdentityVerifier, IdentityClaims, IdentityVerifier as _};
use serde_json::{Value, json};

use crate::support::{code, mac_host, run_scripted, welcome};

/// The hello the agent sent with the default flags before `--identity-source` existed,
/// from the fake Mac host and the flags of [`DEFAULT_FLAGS`].
const DEFAULT_HELLO: &str = include_str!(concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../js-agent-wire/tests/fixtures/agent/default_hello.json"
));
const DEFAULT_FLAGS: [&str; 4] = ["--host-id", "build-1", "--label", "rack=r1"];

/// The document and signature the EC2 provider's tests sign with their throwaway key, and
/// that key's modulus (`crates/js-provider-ec2/tests/offline/identity.rs`).
const EC2_DOC: &str = include_str!(concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../js-provider-ec2/tests/fixtures/identity/doc.json"
));
const EC2_SIG: &str = include_str!(concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../js-provider-ec2/tests/fixtures/identity/doc.sig"
));
const EC2_TEST_MODULUS_HEX: &str = "B6C8460E14E6B4919F1A8741ADD7C0C0735B67A95027DE80451183F0A00EB6ABF1F2CAF2BAE681A7F643D5F43917C3D1ECE1F44BA57385713D07CF77D9C399147CD392A4BDBD6A0D492E2ECC9902D40ADB3EE08FA125EA24FB98E0039165983D1FE41F1DCCE2FDA40DA1D5918B730B0C7AE31CCF94E5BD2AE8995E962430FA9F";

/// A token the Compute Engine provider's tests sign with their throwaway key, and the key's
/// modulus (`crates/js-provider-gce/tests/offline/identity.rs`).
const GCE_TOKEN: &str = include_str!(concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../js-provider-gce/tests/fixtures/identity/good.jwt"
));
const GCE_MODULUS_HEX: &str = include_str!(concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../js-provider-gce/tests/fixtures/identity/modulus.hex"
));

/// The IMDSv2 session token the stand-in issues.
const IMDS_TOKEN: &str = "AQAEAtok-0123456789";

/// One request the stand-in received: the method, the target (path and query) and the
/// headers, their names in lower case.
#[derive(Debug, Clone)]
struct Seen {
    method: String,
    target: String,
    headers: BTreeMap<String, String>,
}

impl Seen {
    fn header(&self, name: &str) -> Option<&str> {
        self.headers.get(name).map(String::as_str)
    }
}

type Answer = Box<dyn Fn(&Seen) -> (u16, String) + Send + Sync>;

/// A metadata service on a loopback listener: answers every connection with its answer and
/// records the request, or, made by [`StandIn::closing`], closes every connection without
/// reading or answering it. Its thread ends when the stand-in is dropped.
struct StandIn {
    base: String,
    seen: Arc<Mutex<Vec<Seen>>>,
    address: SocketAddr,
    stop: Arc<AtomicBool>,
}

impl StandIn {
    fn start(answer: Answer) -> Self {
        Self::spawn(Some(answer))
    }

    /// A service that went away mid-request.
    fn closing() -> Self {
        Self::spawn(None)
    }

    fn spawn(answer: Option<Answer>) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let seen = Arc::new(Mutex::new(Vec::new()));
        let stop = Arc::new(AtomicBool::new(false));
        let (record, stopped) = (seen.clone(), stop.clone());
        thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(stream) = stream else { return };
                if stopped.load(Ordering::SeqCst) {
                    return;
                }
                match &answer {
                    Some(answer) => serve(stream, answer, &record),
                    None => drop(stream),
                }
            }
        });
        Self {
            base: format!("http://{address}"),
            seen,
            address,
            stop,
        }
    }

    fn seen(&self) -> Vec<Seen> {
        self.seen.lock().unwrap().clone()
    }
}

/// Ends the thread: the flag is set, then one connection wakes the listener to read it.
impl Drop for StandIn {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        let _ = TcpStream::connect(self.address);
    }
}

fn serve(stream: TcpStream, answer: &Answer, record: &Mutex<Vec<Seen>>) {
    let mut reader = BufReader::new(stream.try_clone().unwrap());
    let mut line = String::new();
    if reader.read_line(&mut line).is_err() {
        return;
    }
    let mut parts = line.split_whitespace();
    let method = parts.next().unwrap_or_default().to_owned();
    let target = parts.next().unwrap_or_default().to_owned();
    let mut headers = BTreeMap::new();
    loop {
        let mut header = String::new();
        if reader.read_line(&mut header).is_err() {
            return;
        }
        let header = header.trim_end();
        if header.is_empty() {
            break;
        }
        if let Some((name, value)) = header.split_once(':') {
            headers.insert(name.trim().to_ascii_lowercase(), value.trim().to_owned());
        }
    }
    let length: usize = headers
        .get("content-length")
        .map_or(0, |text| text.parse().unwrap());
    let mut body = vec![0; length];
    if reader.read_exact(&mut body).is_err() {
        return;
    }
    let seen = Seen {
        method,
        target,
        headers,
    };
    let (status, text) = answer(&seen);
    record.lock().unwrap().push(seen);
    let mut stream = stream;
    let reply = format!(
        "HTTP/1.1 {status} X\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{text}",
        text.len()
    );
    // The agent may hang up before the whole answer is written (it reads at most 64 KiB):
    // that is its answer to give, not the stand-in's failure.
    let _ = stream.write_all(reply.as_bytes());
}

/// The paths of IMDSv2: a `PUT` of the token path with a lifetime issues [`IMDS_TOKEN`];
/// the document and signature paths answer only a `GET` that carries it (401 otherwise).
fn imds(document: &str, signature: &str) -> Answer {
    let (document, signature) = (document.to_owned(), signature.to_owned());
    Box::new(move |seen: &Seen| {
        let token = seen.header("x-aws-ec2-metadata-token") == Some(IMDS_TOKEN);
        match (seen.method.as_str(), seen.target.as_str()) {
            ("PUT", "/latest/api/token")
                if seen
                    .header("x-aws-ec2-metadata-token-ttl-seconds")
                    .is_some() =>
            {
                (200, IMDS_TOKEN.to_owned())
            }
            ("GET", "/latest/dynamic/instance-identity/document") if token => {
                (200, document.clone())
            }
            ("GET", "/latest/dynamic/instance-identity/signature") if token => {
                (200, signature.clone())
            }
            _ => (401, String::new()),
        }
    })
}

/// The identity token path of the metadata server: a `GET` with `Metadata-Flavor: Google`
/// is answered `token`, any other request 403.
fn gce(token: &str) -> Answer {
    let token = token.to_owned();
    Box::new(move |seen: &Seen| {
        let path = "/computeMetadata/v1/instance/service-accounts/default/identity";
        let flavored = seen.header("metadata-flavor") == Some("Google");
        match seen.target.split_once('?') {
            Some((at, _)) if at == path && flavored && seen.method == "GET" => (200, token.clone()),
            _ => (403, String::new()),
        }
    })
}

/// The decoded query parameters of a request target.
fn query(target: &str) -> BTreeMap<String, String> {
    let decode = |text: &str| {
        let bytes = text.as_bytes();
        let mut out = Vec::new();
        let mut at = 0;
        while at < bytes.len() {
            if bytes[at] == b'%' {
                out.push(u8::from_str_radix(&text[at + 1..at + 3], 16).unwrap());
                at += 3;
            } else {
                out.push(bytes[at]);
                at += 1;
            }
        }
        String::from_utf8(out).unwrap()
    };
    let (_, query) = target.split_once('?').unwrap_or_default();
    query
        .split('&')
        .filter_map(|pair| pair.split_once('='))
        .map(|(key, value)| (decode(key), decode(value)))
        .collect()
}

/// Runs the agent to its first wait with a welcome and returns the hello it sent.
fn hello_of(extra: &[&str]) -> Value {
    let (ran, sent, _) = run_scripted(extra, &mac_host(), vec![welcome(1)], 1);
    assert_eq!(ran.status(), code(0), "{}", ran.err);
    sent.borrow()[0].1.clone()
}

/// Runs the agent with flags or a service it must stop on before anything is sent; returns
/// what it printed.
fn refused(extra: &[&str]) -> String {
    let (ran, sent, waits) = run_scripted(extra, &mac_host(), vec![welcome(1)], 1);
    assert_eq!(ran.status(), code(2), "{}", ran.err);
    assert!(sent.borrow().is_empty(), "something was sent: {sent:?}");
    assert!(waits.is_empty());
    ran.err
}

/// The signed identity a sent hello carries, read with the wire's own type.
fn identity_of(hello: Value) -> Option<SignedIdentity> {
    serde_json::from_value::<Hello>(hello).unwrap().identity
}

fn hex_bytes(text: &str) -> Vec<u8> {
    let text = text.trim();
    (0..text.len())
        .step_by(2)
        .map(|at| u8::from_str_radix(&text[at..at + 2], 16).unwrap())
        .collect()
}

/// Asserts that with `--identity-source aws` the agent asks IMDSv2 for a session token
/// with a `PUT` that states a lifetime, reads the document and the signature with `GET`s
/// that carry that token (the stand-in refuses either without it), and sends both bodies
/// byte for byte as served: a document over several lines and a signature wrapped over
/// lines with a closing line break, nothing trimmed. It catches an agent that sends no
/// identity, that uses IMDSv1 (no token), and one that rewrites what the cloud signed.
#[test]
fn aws_source_sends_document_and_signature() {
    let document = "{\n  \"instanceId\" : \"i-0abc\",\n  \"region\" : \"region-a\"\n}";
    let signature = "c2lnbmF0dXJl\nb3ZlciBsaW5lcw==\n";
    let service = StandIn::start(imds(document, signature));
    let hello = hello_of(
        &[
            &DEFAULT_FLAGS[..],
            &[
                "--identity-source",
                "aws",
                "--identity-metadata-url",
                &service.base,
            ],
        ]
        .concat(),
    );
    assert_eq!(
        hello["identity"],
        json!({ "document": document, "signature": signature })
    );
    let seen = service.seen();
    let asked: Vec<(&str, &str)> = seen
        .iter()
        .map(|seen| (seen.method.as_str(), seen.target.as_str()))
        .collect();
    assert_eq!(
        asked,
        [
            ("PUT", "/latest/api/token"),
            ("GET", "/latest/dynamic/instance-identity/document"),
            ("GET", "/latest/dynamic/instance-identity/signature"),
        ]
    );
    assert_eq!(
        seen[0].header("x-aws-ec2-metadata-token-ttl-seconds"),
        Some("60")
    );
    assert_eq!(seen[0].header("x-aws-ec2-metadata-token"), None);
}

/// Asserts that with `--identity-source gce` the agent asks the identity token path once,
/// with `Metadata-Flavor: Google`, `format=full` and the audience of `--identity-audience`
/// (one with `:`, `/`, `?`, `&` and `=`, which must arrive whole), and sends the token as
/// the identity's document with an empty signature. It catches an audience cut at its first
/// `&`, a missing flavor header (the server refuses such a request) and a token sent in the
/// wrong field.
#[test]
fn gce_source_requests_configured_audience() {
    let audience = "https://leases.example/agent?pool=a&x=1";
    let service = StandIn::start(gce("header.claims.signature"));
    let hello = hello_of(
        &[
            &DEFAULT_FLAGS[..],
            &[
                "--identity-source",
                "gce",
                "--identity-audience",
                audience,
                "--identity-metadata-url",
                &service.base,
            ],
        ]
        .concat(),
    );
    assert_eq!(
        hello["identity"],
        json!({ "document": "header.claims.signature", "signature": "" })
    );
    let seen = service.seen();
    assert_eq!(seen.len(), 1, "{seen:?}");
    assert_eq!(seen[0].method, "GET");
    assert_eq!(seen[0].header("metadata-flavor"), Some("Google"));
    let asked = query(&seen[0].target);
    assert_eq!(
        asked,
        BTreeMap::from([
            ("audience".to_owned(), audience.to_owned()),
            ("format".to_owned(), "full".to_owned()),
        ])
    );
}

/// Asserts that with the default flags, and with `--identity-source none` given, the hello
/// is byte for byte the one the agent sent before the flag existed (the checked-in fixture):
/// no `identity` key, nothing else moved. It catches an agent that sends an empty identity
/// by default, and any other change to the default hello.
#[test]
fn no_source_hello_is_unchanged() {
    for extra in [
        DEFAULT_FLAGS.to_vec(),
        [&DEFAULT_FLAGS[..], &["--identity-source", "none"]].concat(),
    ] {
        let encoded = serde_json::to_string_pretty(&hello_of(&extra)).unwrap() + "\n";
        assert_eq!(encoded, DEFAULT_HELLO, "{extra:?}");
    }
}

/// Asserts that a configured source whose service fails stops the agent at start with
/// status 2 and nothing sent, and that the error names the source, the URL that failed and
/// what was wrong: an IMDS that answers 500 to the token request, one that refuses the
/// document (401) after issuing a token, one that answers the signature with an empty body,
/// one that closes the connection without an answer, one whose token is longer than the
/// 64 KiB the agent reads, and a metadata server that answers 500. It catches a source that falls back to a hello
/// without an identity, and an error that does not say where to look.
#[test]
fn fetch_failure_is_startup_error() {
    let failing: Answer = Box::new(|_: &Seen| (500, "boom".to_owned()));
    let service = StandIn::start(failing);
    let url = &service.base;
    let aws = ["--identity-source", "aws", "--identity-metadata-url", url];
    let said = refused(&aws);
    assert!(
        said.contains(&format!(
            "--identity-source aws: cannot read the signed identity from {url}/latest/api/token: it answered 500"
        )),
        "{said}"
    );

    let no_document: Answer = Box::new(|seen: &Seen| match seen.method.as_str() {
        "PUT" => (200, IMDS_TOKEN.to_owned()),
        _ => (401, String::new()),
    });
    let service = StandIn::start(no_document);
    let url = &service.base;
    let said = refused(&["--identity-source", "aws", "--identity-metadata-url", url]);
    assert!(
        said.contains(&format!(
            "--identity-source aws: cannot read the signed identity from {url}/latest/dynamic/instance-identity/document: it answered 401"
        )),
        "{said}"
    );

    let service = StandIn::start(imds("{}", ""));
    let url = &service.base;
    let said = refused(&["--identity-source", "aws", "--identity-metadata-url", url]);
    assert!(
        said.contains(&format!(
            "--identity-source aws: cannot read the signed identity from {url}/latest/dynamic/instance-identity/signature: it answered an empty body"
        )),
        "{said}"
    );

    // Closed without an answer, the request reads as an end of stream or as a reset,
    // whichever the kernel reports first; either names the source and the URL.
    let service = StandIn::closing();
    let url = &service.base;
    let said = refused(&["--identity-source", "aws", "--identity-metadata-url", url]);
    assert!(
        said.contains(&format!(
            "--identity-source aws: cannot read the signed identity from {url}/latest/api/token: io: "
        )),
        "{said}"
    );

    let huge: Answer = Box::new(|_: &Seen| (200, "t".repeat(64 * 1024 + 1)));
    let service = StandIn::start(huge);
    let url = &service.base;
    let said = refused(&["--identity-source", "aws", "--identity-metadata-url", url]);
    assert!(
        said.contains(&format!(
            "--identity-source aws: cannot read the signed identity from {url}/latest/api/token: the response body is larger than request limit: 65536"
        )),
        "{said}"
    );

    let failing: Answer = Box::new(|_: &Seen| (500, String::new()));
    let service = StandIn::start(failing);
    let url = &service.base;
    let gce = [
        "--identity-source",
        "gce",
        "--identity-audience",
        "a b",
        "--identity-metadata-url",
        url,
    ];
    let said = refused(&gce);
    assert!(
        said.contains(&format!(
            "--identity-source gce: cannot read the signed identity from {url}/computeMetadata/v1/instance/service-accounts/default/identity?audience=a%20b&format=full: it answered 500"
        )),
        "{said}"
    );
}

/// Asserts that a document the cloud served over the wire's cap stops the agent at start,
/// nothing sent, with the field named; one at the cap is sent. It catches an agent that
/// sends a hello every server refuses, over and over.
#[test]
fn a_document_over_the_cap_is_a_startup_error() {
    let at_cap = "d".repeat(MAX_IDENTITY_DOCUMENT_LEN);
    let service = StandIn::start(imds(&at_cap, "sig"));
    let url = &service.base;
    let flags = ["--identity-source", "aws", "--identity-metadata-url", url];
    let identity = identity_of(hello_of(&flags)).unwrap();
    assert_eq!(identity.document.len(), MAX_IDENTITY_DOCUMENT_LEN);

    let over = "d".repeat(MAX_IDENTITY_DOCUMENT_LEN + 1);
    let service = StandIn::start(imds(&over, "sig"));
    let url = &service.base;
    let said = refused(&["--identity-source", "aws", "--identity-metadata-url", url]);
    assert!(
        said.contains("the hello is not acceptable: identity.document "),
        "{said}"
    );
}

/// Asserts the flag rules: `--identity-metadata-url` and `--identity-audience` without a
/// source that takes them, `gce` without an audience or with an empty one, an unknown
/// source, and a metadata URL that is not `http://` with a host are refused before anything
/// is read or sent, naming the flag; a URL with a closing `/` is used without it. It catches
/// a flag that is silently ignored, and a request sent to a URL the operator did not mean.
#[test]
fn the_identity_flags_are_checked() {
    for (flags, says) in [
        (
            vec!["--identity-metadata-url", "http://127.0.0.1:1"],
            "--identity-metadata-url: needs --identity-source aws or gce",
        ),
        (
            vec!["--identity-audience", "a"],
            "--identity-audience: needs --identity-source gce",
        ),
        (
            vec!["--identity-source", "aws", "--identity-audience", "a"],
            "--identity-audience: needs --identity-source gce",
        ),
        (
            vec!["--identity-source", "gce"],
            "--identity-audience: required with --identity-source gce",
        ),
        (
            vec!["--identity-source", "gce", "--identity-audience", ""],
            "--identity-audience: required with --identity-source gce",
        ),
        (
            vec![
                "--identity-source",
                "aws",
                "--identity-metadata-url",
                "https://127.0.0.1:1",
            ],
            "--identity-metadata-url: not an http:// URL of a host (with no user, query or fragment)",
        ),
        (
            vec![
                "--identity-source",
                "aws",
                "--identity-metadata-url",
                "http:///x",
            ],
            "--identity-metadata-url: not an http:// URL of a host (with no user, query or fragment)",
        ),
    ] {
        let said = refused(&flags);
        assert!(said.contains(says), "{flags:?}: {said}");
    }
    let (ran, sent, _) = run_scripted(
        &["--identity-source", "azure"],
        &mac_host(),
        vec![welcome(1)],
        1,
    );
    assert_eq!(ran.status(), code(2), "{}", ran.err);
    assert!(ran.err.contains("--identity-source"), "{}", ran.err);
    assert!(sent.borrow().is_empty());

    let service = StandIn::start(imds("doc", "sig"));
    let slashed = format!("{}/", service.base);
    let flags = [
        "--identity-source",
        "aws",
        "--identity-metadata-url",
        &slashed,
    ];
    assert_eq!(identity_of(hello_of(&flags)).unwrap().document, "doc");
    assert_eq!(service.seen()[0].target, "/latest/api/token");
}

/// Asserts that the hello the agent builds from what each cloud signs passes that cloud's
/// verifier in the providers: a document and signature of the EC2 provider's test key,
/// served through IMDSv2, verify under `AwsIdentityVerifier` with that key and read as the
/// signed instance; a token of the Compute Engine provider's test key, served by the
/// metadata server, verifies under `GoogleIdentityVerifier` with that key and reads as its
/// claims. It catches an agent that sends the identity in a form the providers cannot
/// verify (a signature or token changed on the way, or put in the other field).
#[test]
fn the_identity_sent_passes_the_providers_verifiers() {
    let service = StandIn::start(imds(EC2_DOC, EC2_SIG));
    let url = &service.base;
    let flags = ["--identity-source", "aws", "--identity-metadata-url", url];
    let identity = identity_of(hello_of(&flags)).unwrap();
    let verifier = AwsIdentityVerifier::with_modulus(hex_bytes(EC2_TEST_MODULUS_HEX));
    assert_eq!(
        verifier.verify(&identity),
        Some(IdentityDocument {
            instance_id: "i-0123456789abcdef0".to_owned(),
            region: "region-a".to_owned(),
            image_id: "ami-0123456789abcdef0".to_owned(),
        })
    );

    let service = StandIn::start(gce(GCE_TOKEN));
    let url = &service.base;
    let flags = [
        "--identity-source",
        "gce",
        "--identity-audience",
        "https://sample-scheduler.example",
        "--identity-metadata-url",
        url,
    ];
    let identity = identity_of(hello_of(&flags)).unwrap();
    let verifier = GoogleIdentityVerifier::with_keys(vec![(
        "test-key-1".to_owned(),
        hex_bytes(GCE_MODULUS_HEX),
    )]);
    assert_eq!(
        verifier.verify(&identity),
        Some(IdentityClaims {
            project_id: "sample-project".to_owned(),
            zone: "region-one-a".to_owned(),
            instance_id: "1".to_owned(),
            instance_name: "js-865c0ebee0c3-1c34f88707b5".to_owned(),
        })
    );
}
