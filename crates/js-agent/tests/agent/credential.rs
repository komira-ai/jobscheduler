//! The host credential in the agent: kept by the session, written to the state root before
//! it is used, read again at start, and sent with every call in place of the token.

use std::fs;
use std::io;
use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;
use std::time::Duration;

use js_agent::{
    CREDENTIAL_FILE, CREDENTIAL_TEMP_FILE, Connector, CredentialFile, CredentialState,
    CredentialStore, Dialer, HEARTBEAT_PATH, HELLO_PATH, Note, Session, Step,
};
use js_agent_wire::HostCredential;
use serde_json::{Value, json};

use crate::jobs::private;
use crate::support::{
    ADOPTED, CountingSleeper, Host, Lowest, Scratch, Script, ScriptConnector, Sent, beat_ok, code,
    flags, hello, lowest, mac_host, reply, run, run_scripted, welcome, welcome_issuing,
};

/// The credential of the scripted welcomes.
const CREDENTIAL: &str = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";

/// The path under which [`Store`] records a save in the log it shares with the transport.
const SAVED: &str = "(save-credential)";

/// A store that logs its saves into the transport's log, so the order of the save, the
/// switch of the bearer and the first call is one list, and that fails when told to.
struct Store {
    log: Sent,
    fails: bool,
}

impl CredentialStore for Store {
    fn save(&self, credential: &HostCredential) -> io::Result<()> {
        self.log.borrow_mut().push((
            SAVED.to_owned(),
            Value::String(credential.expose().to_owned()),
        ));
        match self.fails {
            true => Err(io::Error::other("the disk is full")),
            false => Ok(()),
        }
    }
}

fn paths(sent: &Sent) -> Vec<String> {
    sent.borrow().iter().map(|(path, _)| path.clone()).collect()
}

fn note_of(step: &Step) -> Option<Note> {
    match step {
        Step::Wait { note, .. } => note.clone(),
        other => panic!("{other:?}"),
    }
}

fn registered(credential: CredentialState) -> Option<Note> {
    Some(Note::Registered {
        epoch: 1,
        interval: Duration::from_secs(5),
        credential,
    })
}

/// A welcome that carries a credential is written to the store, then made the bearer, and
/// only then is the first heartbeat sent; the note says it is saved, and the credential is
/// in no message body, step or note text. It catches a credential used before it is on disk
/// (a crash between the two would strand the host: its token is dead), a heartbeat sent
/// with the old bearer, and a credential that reaches a body or a log line.
#[test]
fn a_credential_is_saved_then_adopted_then_used() {
    let (mut script, sent) = Script::new(vec![welcome_issuing(1, CREDENTIAL), beat_ok()]);
    let store = Store {
        log: sent.clone(),
        fails: false,
    };
    let mut session =
        Session::new(hello("h1"), &mut script, lowest()).with_credential_store(&store);
    let first = session.tick();
    let second = session.tick();
    assert_eq!(note_of(&first), registered(CredentialState::Saved));
    assert_eq!(paths(&sent), [HELLO_PATH, SAVED, ADOPTED, HEARTBEAT_PATH]);
    for (path, body) in sent.borrow().iter() {
        let carries = body.to_string().contains(CREDENTIAL);
        assert_eq!(carries, path == SAVED || path == ADOPTED, "{path}");
    }
    let shown = format!("{first:?} {second:?} {}", note_of(&first).unwrap());
    assert!(!shown.contains(CREDENTIAL), "{shown}");
}

/// After the session is lost the agent says hello again; that welcome carries no credential
/// (the server issues one once), and the agent keeps the one it has: nothing is saved or
/// adopted a second time and the note says nothing of a credential. It catches an agent
/// that drops its bearer at a new session.
#[test]
fn a_later_welcome_without_a_credential_keeps_the_one_adopted() {
    let lost = json!({ "error": "stale-epoch" });
    let replies = vec![
        welcome_issuing(1, CREDENTIAL),
        reply(409, lost),
        welcome(2),
        beat_ok(),
    ];
    let (mut script, sent) = Script::new(replies);
    let store = Store {
        log: sent.clone(),
        fails: false,
    };
    let mut session =
        Session::new(hello("h1"), &mut script, lowest()).with_credential_store(&store);
    let steps: Vec<Step> = (0..4).map(|_| session.tick()).collect();
    assert_eq!(
        note_of(&steps[2]),
        Some(Note::Registered {
            epoch: 2,
            interval: Duration::from_secs(5),
            credential: CredentialState::None,
        })
    );
    let calls = paths(&sent);
    assert_eq!(calls.iter().filter(|p| *p == SAVED).count(), 1);
    assert_eq!(calls.iter().filter(|p| *p == ADOPTED).count(), 1);
}

/// A store that cannot write does not stop the session: the credential is adopted for this
/// process and the note says that a restart cannot register the host again. It catches an
/// agent that drops a credential it could not save (its token is dead, so it would be
/// refused at once) and one that says nothing about it.
#[test]
fn a_credential_that_cannot_be_saved_is_still_used_and_the_note_says_so() {
    let (mut script, sent) = Script::new(vec![welcome_issuing(1, CREDENTIAL), beat_ok()]);
    let store = Store {
        log: sent.clone(),
        fails: true,
    };
    let mut session =
        Session::new(hello("h1"), &mut script, lowest()).with_credential_store(&store);
    let first = session.tick();
    assert_eq!(note_of(&first), registered(CredentialState::Unsaved));
    assert_eq!(paths(&sent), [HELLO_PATH, SAVED, ADOPTED]);
    let said = note_of(&first).unwrap().to_string();
    assert!(!said.contains("disk is full"), "{said}");
    assert!(!said.contains(CREDENTIAL), "{said}");
}

/// A transport that will not take the credential is reported in the note, after the credential
/// was saved. It catches a session that says "saved" and carries on as if the bearer had
/// changed.
#[test]
fn a_transport_that_refuses_the_credential_is_reported() {
    let (script, sent) = Script::new(vec![welcome_issuing(1, CREDENTIAL)]);
    let mut script = script.refusing_credentials();
    let store = Store {
        log: sent.clone(),
        fails: false,
    };
    let mut session =
        Session::new(hello("h1"), &mut script, lowest()).with_credential_store(&store);
    assert_eq!(
        note_of(&session.tick()),
        registered(CredentialState::Refused)
    );
    assert_eq!(paths(&sent), [HELLO_PATH, SAVED]);
}

/// A welcome without a credential changes nothing, with a store given (a new agent that meets
/// a server that issues none: an older server, or a hand-joined pool), and a credential sent
/// to a session with no store is ignored: nothing is saved or adopted, the note is plain, the
/// heartbeats go on. It catches an agent that adopts a credential it cannot keep, which a
/// restart would lose.
#[test]
fn without_a_credential_or_without_a_store_the_token_path_is_unchanged() {
    let (mut script, sent) = Script::new(vec![welcome(1), beat_ok()]);
    let store = Store {
        log: sent.clone(),
        fails: false,
    };
    let mut session =
        Session::new(hello("h1"), &mut script, lowest()).with_credential_store(&store);
    assert_eq!(note_of(&session.tick()), registered(CredentialState::None));
    session.tick();
    assert_eq!(paths(&sent), [HELLO_PATH, HEARTBEAT_PATH]);

    let (mut script, sent) = Script::new(vec![welcome_issuing(1, CREDENTIAL), beat_ok()]);
    let mut session = Session::new(hello("h1"), &mut script, lowest());
    assert_eq!(note_of(&session.tick()), registered(CredentialState::None));
    session.tick();
    assert_eq!(paths(&sent), [HELLO_PATH, HEARTBEAT_PATH]);
}

/// A credential outside the alphabet makes the welcome unusable: it is not saved, not
/// adopted, and the hello is tried again. It catches an agent that writes a server's bytes
/// to disk and into a header unchecked.
#[test]
fn a_credential_with_odd_bytes_makes_the_welcome_unusable() {
    let (mut script, sent) = Script::new(vec![welcome_issuing(1, "has space")]);
    let store = Store {
        log: sent.clone(),
        fails: false,
    };
    let mut session =
        Session::new(hello("h1"), &mut script, lowest()).with_credential_store(&store);
    let Step::Wait { note, .. } = session.tick() else {
        panic!("the hello is tried again");
    };
    let said = note.unwrap().to_string();
    assert!(said.contains("invalid-welcome"), "{said}");
    assert_eq!(paths(&sent), [HELLO_PATH]);
}

fn state_dir(name: &str) -> (Scratch, PathBuf) {
    let scratch = Scratch::new(name);
    let dir = private(&scratch, "state");
    (scratch, dir)
}

/// The file is written with the credential's text and nothing else, owner-only whatever the
/// temporary file had before, replaced whole by a later save, and no temporary file stays. It
/// catches a file readable by others, one with a stray newline or partial text, and a
/// leftover temporary file that holds a copy.
#[test]
fn the_credential_file_is_written_whole_and_owner_only() {
    let (_scratch, dir) = state_dir("credential-file");
    let file = CredentialFile::in_dir(&dir);
    assert_eq!(file.path(), dir.join(CREDENTIAL_FILE));
    let stale = dir.join(CREDENTIAL_TEMP_FILE);
    fs::write(&stale, "old").unwrap();
    fs::set_permissions(&stale, fs::Permissions::from_mode(0o644)).unwrap();
    let first = HostCredential::parse(CREDENTIAL).unwrap();
    file.save(&first).unwrap();
    assert_eq!(fs::read_to_string(file.path()).unwrap(), CREDENTIAL);
    let mode = fs::metadata(file.path()).unwrap().permissions().mode();
    assert_eq!(mode & 0o777, 0o600);
    assert!(!stale.exists());
    let second = HostCredential::parse("another-credential").unwrap();
    file.save(&second).unwrap();
    assert_eq!(
        fs::read_to_string(file.path()).unwrap(),
        "another-credential"
    );
    assert_eq!(file.load().unwrap(), Some(second));
}

/// A directory that is not there, or a credential file that is a directory, fails the save
/// with an error: the session then reports it as unsaved. It catches a save that reports
/// success when nothing was written.
#[test]
fn a_save_that_cannot_complete_is_an_error() {
    let (_scratch, dir) = state_dir("credential-save-fails");
    let missing = CredentialFile::in_dir(&dir.join("nowhere"));
    let credential = HostCredential::parse(CREDENTIAL).unwrap();
    assert!(missing.save(&credential).is_err());
    let blocked = CredentialFile::in_dir(&dir);
    fs::create_dir(blocked.path()).unwrap();
    fs::write(blocked.path().join("held"), "x").unwrap();
    assert!(blocked.save(&credential).is_err());
}

/// Loading gives nothing for a missing file, the credential for a good one (a hand-written
/// trailing line break is dropped), and a refusal that names the file and never repeats its
/// content for a file that cannot be read, is not text, holds characters outside the
/// alphabet or is too long. It catches a start that takes a damaged file for an absent one,
/// which would fall back to a token that is dead, and an error that prints the secret.
#[test]
fn loading_distinguishes_absent_from_damaged() {
    let (_scratch, dir) = state_dir("credential-load");
    let file = CredentialFile::in_dir(&dir);
    assert_eq!(file.load(), Ok(None));
    fs::write(file.path(), format!("{CREDENTIAL}\n")).unwrap();
    let loaded = file.load().unwrap().unwrap();
    assert_eq!(loaded.expose(), CREDENTIAL);
    for damaged in [
        b"secret with space".to_vec(),
        vec![0xff, 0xfe, b'a'],
        vec![b'a'; 129],
        Vec::new(),
    ] {
        fs::write(file.path(), &damaged).unwrap();
        let said = file.load().unwrap_err();
        assert!(
            said.starts_with(&file.path().display().to_string()),
            "{said}"
        );
        assert!(said.ends_with("does not hold a host credential"), "{said}");
        assert!(!said.contains("secret"), "{said}");
    }
    fs::remove_file(file.path()).unwrap();
    fs::create_dir(file.path()).unwrap();
    let said = file.load().unwrap_err();
    assert!(said.contains("cannot be read"), "{said}");
}

/// Runs the program with flags it must refuse before anything is sent; returns stderr.
fn refused_run(extra: &[&str], host: &Host) -> String {
    let (ran, sent, waits) = run_scripted(extra, host, vec![welcome(1)], 1);
    assert_eq!(ran.status(), code(2), "{}", ran.err);
    assert!(sent.borrow().is_empty(), "something was sent: {:?}", sent);
    assert!(waits.is_empty());
    ran.err
}

fn flags_with(state: &str) -> Vec<String> {
    ["--host-id", "h1", "--state-root", state]
        .map(str::to_owned)
        .to_vec()
}

/// A hello says it keeps a credential exactly when the agent has a state root. It catches a
/// hello that asks for a credential when there is nowhere to put it (the server would then
/// retire the token and the host could not restart), and one that never asks.
#[test]
fn the_hello_asks_only_with_a_state_root() {
    let (_scratch, dir) = state_dir("credential-asks");
    let state = dir.display().to_string();
    let host = mac_host();
    let with = flags_with(&state);
    let with: Vec<&str> = with.iter().map(String::as_str).collect();
    let (ran, sent, _) = run_scripted(&with, &host, vec![welcome(1)], 1);
    assert_eq!(ran.status(), code(0), "{}", ran.err);
    assert_eq!(sent.borrow()[0].1["accepts_host_credential"], json!(true));
    let (ran, sent, _) = run_scripted(&["--host-id", "h1"], &host, vec![welcome(1)], 1);
    assert_eq!(ran.status(), code(0), "{}", ran.err);
    assert!(sent.borrow()[0].1.get("accepts_host_credential").is_none());
}

/// A whole run: the credential in the welcome lands in the state root with mode 0600, the
/// log line says so and holds no credential, and the next heartbeat goes out after the
/// switch. A second run over the same state root adopts the file before its first hello,
/// before any message; one with a damaged file, or a transport that refuses what the file
/// holds, ends with the usage status before it says anything. It catches a credential that
/// does not survive a restart and a start that ignores a file it cannot use.
#[test]
fn a_run_saves_the_credential_and_the_next_run_adopts_it() {
    let (_scratch, dir) = state_dir("credential-run");
    let state = dir.display().to_string();
    let flags_ = flags_with(&state);
    let flags_: Vec<&str> = flags_.iter().map(String::as_str).collect();
    let host = mac_host();
    let replies = vec![welcome_issuing(1, CREDENTIAL), beat_ok()];
    let (ran, sent, _) = run_scripted(&flags_, &host, replies, 1);
    assert_eq!(ran.status(), code(0), "{}", ran.err);
    assert!(
        ran.err.contains("the host credential is saved"),
        "{}",
        ran.err
    );
    assert!(!ran.err.contains(CREDENTIAL) && !ran.out.contains(CREDENTIAL));
    let file = dir.join(CREDENTIAL_FILE);
    assert_eq!(fs::read_to_string(&file).unwrap(), CREDENTIAL);
    assert_eq!(
        fs::metadata(&file).unwrap().permissions().mode() & 0o777,
        0o600
    );
    assert_eq!(paths(&sent), [HELLO_PATH, ADOPTED, HEARTBEAT_PATH]);

    let (ran, sent, _) = run_scripted(&flags_, &host, vec![welcome(2)], 1);
    assert_eq!(ran.status(), code(0), "{}", ran.err);
    assert_eq!(paths(&sent), [ADOPTED, HELLO_PATH, HEARTBEAT_PATH]);
    assert_eq!(sent.borrow()[0].1, json!(CREDENTIAL));

    let (script, sent) = Script::new(vec![welcome(2)]);
    let refusing = ScriptConnector::of(script.refusing_credentials());
    let mut sleeper = CountingSleeper::stopping_at(1);
    let ran = run(&flags(&flags_), &host, &refusing, &mut sleeper, &mut Lowest);
    assert_eq!(ran.status(), code(2), "{}", ran.err);
    assert!(
        ran.err.contains("the script takes no credential"),
        "{}",
        ran.err
    );
    assert!(!ran.err.contains(CREDENTIAL));
    assert!(sent.borrow().is_empty());

    fs::write(&file, "not a credential").unwrap();
    let said = refused_run(&flags_, &host);
    assert!(said.contains(&file.display().to_string()), "{said}");
    assert!(said.contains("does not hold a host credential"), "{said}");
    assert!(!said.contains("not a credential"), "{said}");
}

/// The real transport takes a credential it can send as a bearer and keeps what it had when
/// given text it cannot (the message names the problem and never repeats the text). The
/// header itself is checked in the client's own tests, and the whole path in the end-to-end
/// tests. It catches a transport that reports success for a bearer it did not set.
#[test]
fn the_real_transport_takes_a_credential_or_says_why_not() {
    let scratch = Scratch::new("credential-transport");
    let token = scratch.write("token", "tok-1\n", 0o600);
    let mut transport = Dialer.connect("http://127.0.0.1:1", &token, None).unwrap();
    assert_eq!(transport.adopt_credential(CREDENTIAL), Ok(()));
    let said = transport.adopt_credential("has space").unwrap_err();
    assert!(said.contains("printable ASCII"), "{said}");
    assert!(!said.contains("has space"), "{said}");
}
