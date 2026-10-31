//! TLS reload: a renewed certificate and key are served without a restart.
//!
//! Every test makes its certificates fresh, tells them apart by serial number, and writes
//! them under the target directory; nothing here is a real secret.

use std::net::SocketAddr;
use std::path::PathBuf;
use std::process::Stdio;

use rcgen::{CertificateParams, KeyPair, SerialNumber, date_time_ymd};
use rustix::process::{Pid, Signal, kill_process};
use rustls::pki_types::pem::PemObject;
use rustls::pki_types::{CertificateDer, ServerName};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

use crate::binary::launch;
use crate::support::{
    Harness, LogCapture, Scratch, TOKEN, args_with, base_args, config, guard, lease_body,
};
use crate::tls::{connector, tls_request};

/// A self-signed certificate for 127.0.0.1 with this serial number and validity, and its
/// key, as PEM text.
fn identity(serial: u64, from: (i32, u8, u8), to: (i32, u8, u8)) -> (String, String) {
    let names = vec!["127.0.0.1".to_string(), "localhost".to_string()];
    let mut params = CertificateParams::new(names).unwrap();
    params.serial_number = Some(SerialNumber::from(serial));
    params.not_before = date_time_ymd(from.0, from.1, from.2);
    params.not_after = date_time_ymd(to.0, to.1, to.2);
    let key = KeyPair::generate().unwrap();
    let cert = params.self_signed(&key).unwrap();
    (cert.pem(), key.serialize_pem())
}

/// A certificate that is valid now and for a long time.
fn current(serial: u64) -> (String, String) {
    identity(serial, (2020, 1, 1), (3000, 1, 1))
}

/// The certificate and key files of one test server, and the PEM of what they hold.
struct Files {
    cert: PathBuf,
    key: PathBuf,
}

impl Files {
    /// Replaces both files with `pair`, the key owner-only.
    fn put(&self, scratch: &Scratch, pair: &(String, String)) {
        self.put_text(scratch, Some(&pair.0), Some(&pair.1), 0o600);
    }

    /// Replaces the files with these texts (`None` removes the file), the key with `mode`.
    fn put_text(&self, scratch: &Scratch, cert: Option<&str>, key: Option<&str>, mode: u32) {
        for (path, text, mode) in [(&self.cert, cert, 0o644), (&self.key, key, mode)] {
            match text {
                Some(text) => {
                    let name = path.file_name().unwrap().to_str().unwrap();
                    scratch.write(name, text, mode);
                }
                None => {
                    let _ = std::fs::remove_file(path);
                }
            }
        }
    }
}

/// A TLS server on the pair with serial 1.
async fn server(name: &str) -> (Harness, Files, (String, String)) {
    let scratch = Scratch::new(name);
    let first = current(1);
    let files = Files {
        cert: scratch.path("test-only-cert.pem"),
        key: scratch.path("test-only-key.pem"),
    };
    files.put(&scratch, &first);
    let args = args_with(
        &scratch,
        &[
            "--tls-cert",
            files.cert.to_str().unwrap(),
            "--tls-key",
            files.key.to_str().unwrap(),
        ],
    );
    let h = Harness::over(scratch, config(args).unwrap()).await;
    (h, files, first)
}

/// Handshakes with `addr`, trusting `trusted`, and returns the certificate the server
/// presented, or `None` when the handshake failed.
async fn presented(addr: SocketAddr, trusted: &str) -> Option<Vec<u8>> {
    let tcp = tokio::net::TcpStream::connect(addr).await.unwrap();
    let name = ServerName::IpAddress(addr.ip().into());
    let stream = connector(trusted).connect(name, tcp).await.ok()?;
    let (_, session) = stream.get_ref();
    let chain = session.peer_certificates().unwrap();
    Some(chain[0].as_ref().to_vec())
}

fn der(pem: &str) -> Vec<u8> {
    CertificateDer::from_pem_slice(pem.as_bytes())
        .unwrap()
        .as_ref()
        .to_vec()
}

/// Asserts that after a reload a new handshake presents the new certificate (the client
/// trusts only that one, and sees its DER), and that the old certificate is no longer
/// served.
#[tokio::test]
async fn a_new_handshake_gets_the_renewed_certificate() {
    let (h, files, first) = server("tls_reload_new").await;
    assert_eq!(presented(h.addr(), &first.0).await, Some(der(&first.0)));

    let second = current(2);
    files.put(&h.scratch, &second);
    h.running.reload_tls().unwrap();

    assert_eq!(presented(h.addr(), &second.0).await, Some(der(&second.0)));
    assert_eq!(presented(h.addr(), &first.0).await, None);
}

/// Sends a keep-alive GET `/healthz` on `stream` and reads the whole reply by its length.
async fn health<S: AsyncReadExt + AsyncWriteExt + Unpin>(stream: &mut S) -> String {
    stream
        .write_all(b"GET /healthz HTTP/1.1\r\nHost: test\r\n\r\n")
        .await
        .unwrap();
    let mut raw = Vec::new();
    loop {
        let mut chunk = [0u8; 256];
        let n = guard(stream.read(&mut chunk)).await.unwrap();
        assert!(
            n > 0,
            "the connection closed: {}",
            String::from_utf8_lossy(&raw)
        );
        raw.extend_from_slice(&chunk[..n]);
        if raw.ends_with(b"ok") {
            return String::from_utf8_lossy(&raw).into_owned();
        }
    }
}

/// Asserts that a connection opened before the reload keeps answering after it, on the
/// certificate it was opened with.
#[tokio::test]
async fn a_connection_opened_before_the_reload_keeps_working() {
    let (h, files, first) = server("tls_reload_open").await;
    let addr = h.addr();
    let tcp = tokio::net::TcpStream::connect(addr).await.unwrap();
    let name = ServerName::IpAddress(addr.ip().into());
    let mut open = connector(&first.0).connect(name, tcp).await.unwrap();
    assert!(health(&mut open).await.starts_with("HTTP/1.1 200"));

    files.put(&h.scratch, &current(2));
    h.running.reload_tls().unwrap();

    assert!(health(&mut open).await.starts_with("HTTP/1.1 200"));
}

/// Asserts that each bad pair is refused naming the flag of the file at fault, that the
/// certificate first served is still the one served, and that a pair that fails after
/// its certificate was good (a key that does not belong to it) does not swap the
/// certificate alone.
#[tokio::test]
async fn a_bad_pair_is_refused_and_the_old_certificate_is_still_served() {
    let (h, files, first) = server("tls_reload_bad").await;
    let new = current(2);
    let other = current(3);
    let expired = identity(4, (2020, 1, 1), (2021, 1, 1));
    let not_yet = identity(5, (3000, 1, 1), (3001, 1, 1));
    let s = &h.scratch;
    let cases: Vec<(&str, &str, Box<dyn Fn()>)> = vec![
        (
            "--tls-cert",
            "absent certificate",
            Box::new(|| files.put_text(s, None, Some(&new.1), 0o600)),
        ),
        (
            "--tls-cert",
            "empty certificate",
            Box::new(|| files.put_text(s, Some(""), Some(&new.1), 0o600)),
        ),
        (
            "--tls-cert",
            "junk certificate",
            Box::new(|| files.put_text(s, Some("not pem"), Some(&new.1), 0o600)),
        ),
        (
            "--tls-key",
            "absent key",
            Box::new(|| files.put_text(s, Some(&new.0), None, 0o600)),
        ),
        (
            "--tls-key",
            "junk key",
            Box::new(|| files.put_text(s, Some(&new.0), Some("not pem"), 0o600)),
        ),
        (
            "--tls-key",
            "loose key",
            Box::new(|| files.put_text(s, Some(&new.0), Some(&new.1), 0o644)),
        ),
        (
            "--tls-key",
            "mismatched key",
            Box::new(|| files.put_text(s, Some(&new.0), Some(&other.1), 0o600)),
        ),
        (
            "--tls-cert",
            "expired certificate",
            Box::new(|| files.put_text(s, Some(&expired.0), Some(&expired.1), 0o600)),
        ),
        (
            "--tls-cert",
            "not yet valid certificate",
            Box::new(|| files.put_text(s, Some(&not_yet.0), Some(&not_yet.1), 0o600)),
        ),
    ];
    for (flag, what, plant) in cases {
        plant();
        let err = h.running.reload_tls().unwrap_err();
        assert_eq!(err.flag(), flag, "{what}: {err}");
        assert_eq!(
            presented(h.addr(), &first.0).await,
            Some(der(&first.0)),
            "{what}: the old certificate must still be served"
        );
        assert_eq!(presented(h.addr(), &new.0).await, None, "{what}");
    }
    // The files are good again: the next reload takes them.
    files.put(s, &new);
    h.running.reload_tls().unwrap();
    assert_eq!(presented(h.addr(), &new.0).await, Some(der(&new.0)));
}

/// Asserts that a refused reload logs one line saying so, naming the flag, and that a
/// reload that worked logs one line saying that.
#[tokio::test]
async fn a_reload_logs_one_line() {
    let (h, files, _) = server("tls_reload_log").await;
    let logs = LogCapture::start();
    files.put_text(&h.scratch, Some("not pem"), Some("not pem"), 0o600);
    assert!(h.running.reload_tls().is_err());
    let text = logs.text();
    let lines: Vec<_> = text
        .lines()
        .filter(|l| l.contains("tls reload refused"))
        .collect();
    assert_eq!(lines.len(), 1, "{text}");
    assert!(lines[0].contains("--tls-cert"), "{text}");
    assert!(!text.contains("tls reloaded"), "{text}");

    files.put(&h.scratch, &current(2));
    h.running.reload_tls().unwrap();
    assert_eq!(
        logs.text()
            .lines()
            .filter(|l| l.contains("tls reloaded"))
            .count(),
        1
    );
}

/// Asserts that a server without TLS refuses a reload instead of ignoring it.
#[tokio::test]
async fn a_server_without_tls_refuses_a_reload() {
    let h = Harness::new("tls_reload_none").await;
    let err = h.running.reload_tls().unwrap_err();
    assert_eq!(err.flag(), "--tls-cert");
}

/// Asserts that the lease table survives a reload: a lease made before it is listed,
/// with the same id, after it, over the renewed certificate.
#[tokio::test]
async fn leases_survive_a_reload() {
    let (h, files, first) = server("tls_reload_state").await;
    let body = lease_body().to_string().into_bytes();
    let made = tls_request(
        h.addr(),
        &connector(&first.0),
        "POST",
        "/v1/leases",
        Some(TOKEN),
        Some(&body),
    )
    .await;
    assert_eq!(made.status, 201, "{}", made.body);
    let id = made.json()["id"].as_str().unwrap().to_string();

    let second = current(2);
    files.put(&h.scratch, &second);
    h.running.reload_tls().unwrap();

    let listed = tls_request(
        h.addr(),
        &connector(&second.0),
        "GET",
        "/v1/leases",
        Some(TOKEN),
        None,
    )
    .await;
    assert_eq!(listed.status, 200);
    let ids: Vec<String> = listed.json()["leases"]
        .as_array()
        .unwrap()
        .iter()
        .map(|l| l["id"].as_str().unwrap().to_string())
        .collect();
    assert_eq!(ids, vec![id]);
}

/// Asserts that SIGHUP to the real program reloads: it serves the renewed certificate
/// afterwards, the lease made before is still there, and the program is still running.
/// The signal is handled asynchronously, so the test asks until the new certificate is
/// served, within a bound.
#[tokio::test]
async fn sighup_reloads_the_program() {
    let scratch = Scratch::new("tls_reload_sighup");
    let first = current(1);
    let files = Files {
        cert: scratch.path("test-only-cert.pem"),
        key: scratch.path("test-only-key.pem"),
    };
    files.put(&scratch, &first);
    let mut args = base_args(&scratch);
    args.extend(
        [
            "--tls-cert",
            files.cert.to_str().unwrap(),
            "--tls-key",
            files.key.to_str().unwrap(),
        ]
        .map(Into::into),
    );
    let (mut child, addr) = launch(&args, Stdio::null());
    let body = lease_body().to_string().into_bytes();
    let made = tls_request(
        addr,
        &connector(&first.0),
        "POST",
        "/v1/leases",
        Some(TOKEN),
        Some(&body),
    )
    .await;
    assert_eq!(made.status, 201, "{}", made.body);

    let second = current(2);
    files.put(&scratch, &second);
    kill_process(Pid::from_child(&child.0), Signal::HUP).unwrap();
    let mut served = false;
    for _ in 0..400 {
        if presented(addr, &second.0).await.is_some() {
            served = true;
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
    assert!(
        served,
        "the renewed certificate was never served after SIGHUP"
    );
    assert!(
        child.0.try_wait().unwrap().is_none(),
        "the program stopped on SIGHUP"
    );
    let listed = tls_request(
        addr,
        &connector(&second.0),
        "GET",
        "/v1/leases",
        Some(TOKEN),
        None,
    )
    .await;
    assert_eq!(listed.json()["leases"].as_array().unwrap().len(), 1);
}
