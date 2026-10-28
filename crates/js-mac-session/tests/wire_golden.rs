//! The bytes the helper's socket protocol and its ledger put on the wire and on disk, one
//! exact string per message. The helper and its clients are built and deployed apart (the
//! daemon of another repository is one), so a change to any string here is a protocol
//! change that must be made on both sides, not an edit to this file.
//!
//! The rows were written from the protocol as it stood when the crate moved into this
//! repository; the lease id's text form (`<term>.<seq>`, where `js_types::LeaseId` displays
//! `<term>-<seq>`) is the part this crate adds a type for, so it has rows of its own.

use std::os::fd::AsFd as _;
use std::os::unix::net::UnixStream;

use js_mac_session::lease::{Dotted, parse_lease, user_name};
use js_mac_session::ledger::Ledger;
use js_mac_session::proto::{self, Call, Reply, Request, Session};
use js_testkit::dir::TestDir;
use js_types::LeaseId;

/// Every request, as the daemon sends it, and the text that must parse back to it.
fn requests() -> Vec<(Request, &'static str)> {
    let s = |text: &str| text.to_owned();
    vec![
        (
            Request::UserCreate {
                lease: s("3.7"),
                grant: None,
            },
            r#"{"verb":"user-create","lease":"3.7"}"#,
        ),
        (
            Request::UserCreate {
                lease: s("3.7"),
                grant: Some(s("a.b")),
            },
            r#"{"verb":"user-create","lease":"3.7","grant":"a.b"}"#,
        ),
        (
            Request::Run {
                lease: s("3.7"),
                argv: vec![s("/bin/true")],
                env: vec![(s("A"), s("b"))],
                session: Session::Background,
            },
            r#"{"verb":"run","lease":"3.7","argv":["/bin/true"],"env":[["A","b"]]}"#,
        ),
        (
            Request::Run {
                lease: s("3.7"),
                argv: vec![s("/bin/true")],
                env: Vec::new(),
                session: Session::Gui,
            },
            r#"{"verb":"run","lease":"3.7","argv":["/bin/true"],"env":[],"session":"gui"}"#,
        ),
        (
            Request::SessionReady { lease: s("3.7") },
            r#"{"verb":"session-ready","lease":"3.7"}"#,
        ),
        (
            Request::KillUid { lease: s("3.7") },
            r#"{"verb":"kill-uid","lease":"3.7"}"#,
        ),
        (
            Request::UserDelete { lease: s("3.7") },
            r#"{"verb":"user-delete","lease":"3.7"}"#,
        ),
        (
            Request::SessionLogin { lease: s("3.7") },
            r#"{"verb":"session-login","lease":"3.7"}"#,
        ),
        (Request::SessionIdle, r#"{"verb":"session-idle"}"#),
        (
            Request::RoleRun {
                argv: vec![s("/v/vmm"), s("vm-run"), s("--spec"), s("/i/spec")],
                env: Vec::new(),
            },
            r#"{"verb":"role-run","argv":["/v/vmm","vm-run","--spec","/i/spec"],"env":[]}"#,
        ),
        (
            Request::RoleSessionReady,
            r#"{"verb":"role-session-ready"}"#,
        ),
        (Request::RoleKillUid, r#"{"verb":"role-kill-uid"}"#),
        (
            Request::RoleReap {
                pgid: 4242,
                start: 1_700_000_000_123_456,
                boot: s("B0"),
            },
            r#"{"verb":"role-reap","pgid":4242,"start":1700000000123456,"boot":"B0"}"#,
        ),
        (
            Request::ImageSetup { bundle: s("/i/b") },
            r#"{"verb":"image-setup","bundle":"/i/b"}"#,
        ),
        (
            Request::ImageHarvest {
                bundle: s("/i/b"),
                out: s("/i/o"),
            },
            r#"{"verb":"image-harvest","bundle":"/i/b","out":"/i/o"}"#,
        ),
        (
            Request::BootReset {
                held: vec![s("1.1"), s("2.9")],
            },
            r#"{"verb":"boot-reset","held":["1.1","2.9"]}"#,
        ),
        (Request::Scan, r#"{"verb":"scan"}"#),
        (Request::Baseline, r#"{"verb":"baseline"}"#),
        (Request::RebootDirty, r#"{"verb":"reboot-dirty"}"#),
    ]
}

/// Every reply, as the helper sends it, and the text that must parse back to it.
fn replies() -> Vec<(Reply, &'static str)> {
    let s = |text: &str| text.to_owned();
    vec![
        (
            Reply::Hello { nonce: s("n0") },
            r#"{"hello":{"nonce":"n0"}}"#,
        ),
        (Reply::Created { uid: 650 }, r#"{"created":{"uid":650}}"#),
        (Reply::Started { pid: 77 }, r#"{"started":{"pid":77}}"#),
        (
            Reply::Exited {
                code: Some(0),
                signal: None,
            },
            r#"{"exited":{"code":0,"signal":null}}"#,
        ),
        (
            Reply::Exited {
                code: None,
                signal: Some(9),
            },
            r#"{"exited":{"code":null,"signal":9}}"#,
        ),
        (Reply::Killed, r#""killed""#),
        (
            Reply::Deleted { existed: false },
            r#"{"deleted":{"existed":false}}"#,
        ),
        (Reply::Restarting, r#""restarting""#),
        (
            Reply::SessionReady { ready: true },
            r#"{"session-ready":{"ready":true}}"#,
        ),
        (
            Reply::Reset {
                kept: Some(s("1.1")),
                deleted: vec![s("1.2")],
            },
            r#"{"reset":{"kept":"1.1","deleted":["1.2"]}}"#,
        ),
        (
            Reply::Reset {
                kept: None,
                deleted: Vec::new(),
            },
            r#"{"reset":{"kept":null,"deleted":[]}}"#,
        ),
        (
            Reply::Scanned {
                clean: false,
                added: vec![s("a")],
                removed: vec![s("r")],
                errors: vec![s("e")],
            },
            r#"{"scanned":{"clean":false,"added":["a"],"removed":["r"],"errors":["e"]}}"#,
        ),
        (
            Reply::Baselined {
                classes: vec![(s("lease-users"), 1)],
            },
            r#"{"baselined":{"classes":[["lease-users",1]]}}"#,
        ),
        (
            Reply::Imaged { report: s("{}") },
            r#"{"imaged":{"report":"{}"}}"#,
        ),
        (
            Reply::Refused { reason: s("no") },
            r#"{"refused":{"reason":"no"}}"#,
        ),
    ]
}

/// Catches: a verb renamed, a field renamed or reordered, a default that is now sent or
/// no longer left out: any change of the bytes the daemon's requests are written as.
#[test]
fn every_request_has_its_bytes() {
    for (request, text) in requests() {
        assert_eq!(serde_json::to_string(&request).unwrap(), text);
        assert_eq!(serde_json::from_str::<Request>(text).unwrap(), request);
    }
}

/// Catches: a reply variant renamed or reshaped, which a daemon would read as a refusal
/// or not at all.
#[test]
fn every_reply_has_its_bytes() {
    for (reply, text) in replies() {
        assert_eq!(serde_json::to_string(&reply).unwrap(), text);
        assert_eq!(serde_json::from_str::<Reply>(text).unwrap(), reply);
    }
}

/// Catches: a frame header that is not four big-endian length bytes, or a call that is
/// not the nonce and the request in that order.
#[test]
fn a_call_is_a_length_prefix_and_its_json() {
    let (client, helper) = UnixStream::pair().unwrap();
    let call = Call {
        nonce: "n0".to_owned(),
        request: Request::Scan,
    };
    proto::send(client.as_fd(), &call, &[]).unwrap();
    drop(client);
    let mut bytes = Vec::new();
    std::io::Read::read_to_end(&mut &helper, &mut bytes).unwrap();
    let body = br#"{"nonce":"n0","request":{"verb":"scan"}}"#;
    let mut want = u32::try_from(body.len()).unwrap().to_be_bytes().to_vec();
    want.extend_from_slice(body);
    assert_eq!(bytes, want);
}

/// Catches: a lease id spelled with the dash `js_types::LeaseId` displays instead of the
/// dot the protocol, the ledger and the logs use; a second spelling of one id; the lease
/// user's account name changing (an account the helper made would then never be found).
#[test]
fn a_lease_id_is_spelled_term_dot_sequence() {
    let lease = LeaseId::new(3, 41);
    assert_eq!(Dotted(lease).to_string(), "3.41");
    assert_eq!(parse_lease("3.41"), Ok(lease));
    assert!(parse_lease("3-41").is_err());
    assert!(parse_lease("03.41").is_err());
    assert_eq!(user_name(lease), "kbf-lease-3-41");
    assert_eq!(
        Dotted(LeaseId::new(u64::MAX, 0)).to_string(),
        "18446744073709551615.0"
    );
}

/// Catches: a ledger line that is not `created <term>.<seq> <uid>` or `deleted
/// <term>.<seq>`, which a ledger written before the move would then fail to read, or
/// that is read back as another lease.
#[test]
fn the_ledger_file_is_one_dotted_line_per_record() {
    let dir = TestDir::under(env!("CARGO_TARGET_TMPDIR"), "wire-golden-ledger");
    let path = dir.join("ledger");
    let mut ledger = Ledger::open(&path).unwrap();
    ledger.record_created(LeaseId::new(3, 7), 650).unwrap();
    ledger.record_created(LeaseId::new(3, 8), 651).unwrap();
    ledger.record_deleted(LeaseId::new(3, 7)).unwrap();
    drop(ledger);
    assert_eq!(
        std::fs::read_to_string(&path).unwrap(),
        "created 3.7 650\ncreated 3.8 651\ndeleted 3.7\n"
    );
    let again = Ledger::open(&path).unwrap();
    assert_eq!(again.live_leases(), vec![LeaseId::new(3, 8)]);
}

/// Catches: the old ledger text of a lease that was written before the move, in the form
/// the kbf-built helper wrote it, no longer being read.
#[test]
fn a_ledger_written_before_the_move_is_read() {
    let dir = TestDir::under(env!("CARGO_TARGET_TMPDIR"), "wire-golden-old-ledger");
    let path = dir.join("ledger");
    std::fs::write(&path, "created 1.1 650\ncreated 1.2 651\ndeleted 1.1\n").unwrap();
    let ledger = Ledger::open(&path).unwrap();
    assert_eq!(ledger.live_leases(), vec![LeaseId::new(1, 2)]);
    assert_eq!(ledger.last_uid(), Some(651));
}
