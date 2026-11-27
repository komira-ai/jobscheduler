//! `js-agent cordon --flags-file`: the verb reads the agent's own flags file and cordons the
//! host the agent registers, with what the agent presents; and a refused certificate is told
//! with what was sent and for which host. Over the counted fake lease server, and over a real
//! lease server in the test process for the refusal a certificate of another host gets.

use std::cell::Cell;
use std::path::{Path, PathBuf};

use js_agent::Login;
use js_agent::flags_file::FileFacts;
use js_agent::own_host::LeaseError;
use js_client::HostIdentity;

use super::fakes::{Call, Effect, FakeLease, FakePace, Kind};
use super::http::{enrol, real};
use super::verb::{Ran, Seen, drive_with};
use crate::support::{Host, code, linux_host};

/// Where the agent's package keeps its flags file.
const FLAGS: &str = "/etc/js-agent/flags";
/// hostkeeper's facts file.
const FACTS: &str = "/var/lib/hostkeeper/facts.json";
/// The host certificate and key hostkeeper keeps.
const CERT: &str = "/var/lib/hostkeeper/identity/host.crt";
const KEY: &str = "/var/lib/hostkeeper/identity/host.key";

/// A root-owned file of `mode` holding `text`, as the flags file and the facts file are read.
fn root_file(text: &str, mode: u32) -> FileFacts {
    FileFacts {
        regular: true,
        mode,
        uid: 0,
        bytes: text.as_bytes().to_vec(),
    }
}

/// A Linux host whose agent flags file (at [`FLAGS`], mode `mode`) holds `flags`, and whose
/// facts file names the host `web-04`.
fn host_with(flags: &str, mode: u32) -> Host {
    let mut host = linux_host();
    host.files = host
        .files
        .with_facts(FLAGS, root_file(flags, mode))
        .with_facts(
            FACTS,
            root_file(
                r#"{"schema":1,"host_id":"web-04","host_group":"web"}"#,
                0o644,
            ),
        );
    host
}

/// The flags file of a host hostkeeper manages: its id from the facts file, its certificate and
/// key, and the lease server and pool.
fn certified_flags() -> String {
    format!(
        "# the agent's flags\n--server=https://leases.example:7300\n--pool=farm\n\
         --host-id-from-facts={FACTS}\n--ca-cert=/etc/js-agent/ca.pem\n\
         --host-cert={CERT}\n--host-key={KEY}\n"
    )
}

/// Runs the verb on `flags` over `host` against `lease`.
fn drive(flags: &[&str], host: &Host, lease: &FakeLease) -> Ran {
    let lease = lease.clone();
    drive_with(
        flags,
        host,
        &move |_| Ok(Box::new(lease.clone())),
        &mut FakePace::new(),
    )
}

fn text(path: &Path) -> String {
    path.display().to_string()
}

/// Asserts that with `--flags-file` the verb connects to the file's server, trusts the file's
/// certificate, cordons the host of the file's `--host-id-from-facts` and presents the file's
/// host certificate and key, with `--state-root` and `--token-file` given beside it and
/// ignored; and that the window then runs as always: one cordon, the lift, the lines. It
/// catches a verb that takes another identity than the certificate the file names.
#[test]
fn the_agents_flags_file_names_the_server_the_host_and_the_host_certificate() {
    let host = host_with(&certified_flags(), 0o644);
    let lease = FakeLease::new();
    lease.set(|state| state.id = "web-04".to_owned());
    let flags = [
        "--flags-file",
        FLAGS,
        "--state-root",
        "/elsewhere",
        "--token-file",
        "/token",
        "--for",
        "1m",
    ];
    let ran = drive(&flags, &host, &lease);
    assert_eq!(ran.status, code(0), "{}", ran.err);
    assert_eq!(
        ran.seen,
        [Seen {
            server: "https://leases.example:7300".to_owned(),
            login: Login::HostCertificate(HostIdentity::new(CERT, KEY)),
            ca_cert: Some(PathBuf::from("/etc/js-agent/ca.pem")),
            host: "web-04".to_owned(),
        }]
    );
    assert_eq!(lease.calls(), [Call::Cordon(300), Call::Uncordon]);
    assert_eq!(
        ran.out,
        "cordoned host=web-04 holder=cordon-4242-1700000000000 expires_in_ms=300000 leases=0\nidle host=web-04\nlifted host=web-04\n"
    );
    assert!(ran.err.is_empty(), "{}", ran.err);
}

/// Asserts that a verb with no host certificate and key ends with status 2 naming both flags
/// and connects to nothing, whether the flags file names none, or the verb's own flags do, and
/// whatever `--state-root` and `--token-file` are given beside: they identify no host. It
/// catches a verb that cordons with some other identity.
#[test]
fn without_a_host_certificate_the_verb_ends_with_status_2_naming_both_flags() {
    let plain = "--server=https://leases.example:7300\n--pool=farm\n--host-id=node-5\n";
    let host = host_with(plain, 0o644);
    let said = "js-agent cordon: --host-cert and --host-key: required, here or in the \
                --flags-file that names them\n";
    let cases: [&[&str]; 2] = [
        &["--flags-file", FLAGS, "--for", "1m"],
        &[
            "--flags-file",
            FLAGS,
            "--state-root",
            "/state",
            "--token-file",
            "/token",
            "--for",
            "1m",
        ],
    ];
    for flags in cases {
        let ran = drive(flags, &host, &FakeLease::new());
        assert_eq!(ran.status, code(2), "{flags:?}: {}", ran.err);
        assert_eq!(ran.err, said, "{flags:?}");
        assert!(ran.seen.is_empty(), "{flags:?}");
    }
    let own = [
        "--server",
        "https://leases.example:7300",
        "--host-id",
        "node-5",
        "--state-root",
        "/state",
        "--for",
        "1m",
    ];
    let ran = drive(&own, &host, &FakeLease::new());
    assert_eq!(ran.status, code(2), "{}", ran.err);
    assert_eq!(ran.err, said);
    assert!(ran.seen.is_empty());
}

/// Asserts that a flags file the agent would refuse, and a flag the verb may not take beside
/// it, end the verb with status 2 and the reason on standard error, and connect to nothing: a
/// file group or others can write; a file that is not there; a file without the agent's
/// required `--pool`; a file naming one of the host flags without the other; `--server` beside
/// `--flags-file`; and neither `--flags-file` nor `--server`. It catches a verb that trusts a file the agent would not,
/// and one where a second value silently wins.
#[test]
fn a_flags_file_the_agent_would_refuse_or_a_second_value_is_a_usage_error() {
    let good = "--server=https://leases.example:7300\n--pool=farm\n--host-id=node-5\n";
    let cases: [(String, u32, Vec<&str>, &str); 6] = [
        (
            good.to_owned(),
            0o664,
            vec![],
            "js-agent cordon: --flags-file /etc/js-agent/flags: writable by group or others \
             (mode 0664)\n",
        ),
        (
            "--server=https://leases.example:7300\n--host-id=node-5\n".to_owned(),
            0o644,
            vec![],
            "js-agent cordon: error: the following required arguments were not provided:   \
             --pool <NAME>\n",
        ),
        (
            format!("{good}--host-cert={CERT}\n"),
            0o644,
            vec![],
            "js-agent cordon: error: the following required arguments were not provided:   \
             --host-key <PATH>\n",
        ),
        (
            good.to_owned(),
            0o644,
            vec!["--server", "https://other:7300"],
            "cannot be used with",
        ),
        (
            good.to_owned(),
            0o644,
            vec!["--host-id", "node-6"],
            "cannot be used with",
        ),
        (
            good.to_owned(),
            0o644,
            vec!["--ca-cert", "/ca.pem"],
            "cannot be used with",
        ),
    ];
    for (file, mode, beside, said) in cases {
        let host = host_with(&file, mode);
        let mut flags = vec!["--flags-file", FLAGS, "--for", "1m"];
        flags.extend(beside);
        let ran = drive(&flags, &host, &FakeLease::new());
        assert_eq!(ran.status, code(2), "{flags:?}: {}", ran.err);
        match said.ends_with('\n') {
            true => assert_eq!(ran.err, said, "{flags:?}"),
            false => assert!(ran.err.contains(said), "{flags:?}: {}", ran.err),
        }
        assert!(ran.seen.is_empty(), "{flags:?}");
    }

    let host = linux_host();
    let missing = ["--flags-file", FLAGS, "--for", "1m"];
    let ran = drive(&missing, &host, &FakeLease::new());
    assert_eq!(ran.status, code(2));
    assert!(
        ran.err
            .starts_with("js-agent cordon: --flags-file /etc/js-agent/flags: cannot be read: "),
        "{}",
        ran.err
    );
    assert!(ran.seen.is_empty());
    let neither = ["--host-id", "h-1", "--for", "1m"];
    let ran = drive(&neither, &host, &FakeLease::new());
    assert_eq!(ran.status, code(2));
    assert!(
        ran.err
            .contains("the following required arguments were not provided:\n  --server <URL>"),
        "{}",
        ran.err
    );
    assert!(ran.seen.is_empty());
}

/// Asserts what standard error says when the server refuses the host certificate: the refusal,
/// then what was sent and for which host. Without `--flags-file` it adds how to send what the
/// agent sends; with it, it does not. A refusal that is not of the certificate (`invalid-cordon`)
/// gets no such line. Each run makes the one cordon call that was refused and lifts nothing. It
/// catches a refusal that leaves an operator guessing which identity the verb read.
#[test]
fn a_refused_certificate_is_told_with_what_was_sent_and_for_which_host() {
    let host = host_with(&certified_flags(), 0o644);
    let refused = |code: &str| {
        let lease = FakeLease::new();
        let why = LeaseError::Refused(code.to_owned());
        lease.on(Kind::Cordon, 0, Effect::Fail(why));
        lease
    };
    let own = [
        "--server",
        "https://leases.example:7300",
        "--host-cert",
        CERT,
        "--host-key",
        KEY,
        "--host-id-from-facts",
        FACTS,
        "--for",
        "15m",
    ];
    let from_file = ["--flags-file", FLAGS, "--for", "15m"];
    let refusal = |code: &str| {
        format!(
            "js-agent cordon: credential error: cannot cordon the host: the server refused \
             the host certificate ({code})\n"
        )
    };
    let how = "; to cordon with what the agent on this host presents, give the agent's flags \
               file with --flags-file (such as /etc/js-agent/flags)";
    let cases = [
        (
            &own[..],
            "certificate-host-mismatch",
            format!(
                "{}js-agent cordon: it sent the host certificate {CERT} for host web-04{how}\n",
                refusal("certificate-host-mismatch")
            ),
        ),
        (
            &own[..],
            "http-401",
            format!(
                "{}js-agent cordon: it sent the host certificate {CERT} for host web-04{how}\n",
                refusal("http-401")
            ),
        ),
        (
            &from_file[..],
            "host-certificate-required",
            format!(
                "{}js-agent cordon: it sent the host certificate {CERT} for host web-04\n",
                refusal("host-certificate-required")
            ),
        ),
        (
            &own[..],
            "invalid-cordon",
            "js-agent cordon: server error: cannot cordon the host: the server refused \
             (invalid-cordon)\n"
                .to_owned(),
        ),
    ];
    for (flags, code_given, said) in cases {
        let lease = refused(code_given);
        let ran = drive(flags, &host, &lease);
        assert_eq!(ran.status, code(1), "{flags:?}");
        assert_eq!(ran.err, said, "{flags:?}");
        assert!(ran.out.is_empty(), "{flags:?}: {}", ran.out);
        assert_eq!(lease.calls(), [Call::Cordon(300)], "{flags:?}");
    }
}

/// Asserts both answers of a real lease server to a host enrolled as `mac-1` with its host
/// certificate: the verb told the host by a facts file that names another id (`web-04`) sends
/// the certificate of `mac-1` and is refused 403 `certificate-host-mismatch`, exits 1 naming the
/// certificate file and the host, and places no cordon; the verb given the agent's flags file
/// (which names `--host-id=mac-1`) cordons `mac-1`, holds the cordon on the server through the
/// window and lifts it. It catches a refusal said without its cause, and a flags file whose host
/// or certificate is not the one the server registered.
#[test]
fn on_a_real_server_the_agents_flags_file_cordons_the_host_another_host_id_cannot() {
    let enrolled = enrol("cordon_flags_file_real");
    let server = &enrolled.server;
    let identities = server.identities();
    let identity = identities.of("mac-1");
    let (cert, key) = (text(identity.cert_path()), text(identity.key_path()));
    let ca = text(identities.server_cert());
    let url = server.url();
    let agent_flags = format!(
        "--server={url}\n--pool=default\n--host-id=mac-1\n--host-cert={cert}\n--host-key={key}\n\
         --ca-cert={ca}\n"
    );
    let host = host_with(&agent_flags, 0o644);

    let other_id = [
        "--server",
        &url,
        "--host-cert",
        &cert,
        "--host-key",
        &key,
        "--ca-cert",
        &ca,
        "--host-id-from-facts",
        FACTS,
        "--for",
        "15m",
    ];
    let ran = drive_with(&other_id, &host, &real, &mut FakePace::new());
    assert_eq!(ran.status, code(1), "{}", ran.err);
    assert_eq!(
        ran.err,
        format!(
            "js-agent cordon: credential error: cannot cordon the host: the server refused the \
             host certificate (certificate-host-mismatch)\njs-agent cordon: it sent the host \
             certificate {cert} for host web-04; to cordon with what the agent on this host \
             presents, give the agent's flags file with --flags-file (such as \
             /etc/js-agent/flags)\n"
        )
    );
    assert!(server.host("mac-1").get("cordon").is_none());

    let renewals = Cell::new(0_u32);
    let mut pace = FakePace::new().on_wait(|_, span| {
        server.clock.advance(span);
        assert!(server.host("mac-1")["cordon"]["expires_at_ms"].is_u64());
        renewals.set(renewals.get() + 1);
    });
    let from_file = ["--flags-file", FLAGS, "--for", "15m"];
    let ran = drive_with(&from_file, &host, &real, &mut pace);
    assert_eq!(ran.status, code(0), "{}", ran.err);
    assert_eq!(
        ran.out,
        "cordoned host=mac-1 holder=cordon-4242-1700000000000 expires_in_ms=300000 leases=0\nidle host=mac-1\nlifted host=mac-1\n"
    );
    assert_eq!(renewals.get(), 9, "a renewal every 100 s of 900");
    assert!(server.host("mac-1").get("cordon").is_none());
}
