//! `--host-id-from-facts`: the host registers under the `host_id` of hostkeeper's facts
//! file, and a facts file that cannot be used stops the agent.

use std::ffi::OsString;
use std::io;
use std::path::Path;

use clap::Parser;
use js_agent::flags_file::{FileFacts, expand_with};
use js_agent::{Cli, FactsError, resolve_host_id};
use serde_json::Value;

use crate::support::{
    CountingSleeper, Lowest, Scratch, Script, ScriptConnector, code, linux_host, run, welcome,
};

const FACTS: &str = "/var/lib/hostkeeper/facts.json";
const EUID: u32 = 1000;

/// A facts file as the reader seam gives it: regular, mode 0644, owned by root.
fn file(text: &str) -> FileFacts {
    FileFacts {
        regular: true,
        mode: 0o644,
        uid: 0,
        bytes: text.as_bytes().to_vec(),
    }
}

fn cli(extra: &[&str]) -> Cli {
    let mut words = vec![
        "js-agent",
        "--server=https://leases.example:7300",
        "--pool=p",
    ];
    words.extend_from_slice(extra);
    Cli::try_parse_from(words).unwrap()
}

/// What the flags `--host-id-from-facts FACTS` resolve to when the file is `found`.
fn resolved(found: io::Result<FileFacts>) -> Result<Option<String>, FactsError> {
    let found = std::cell::Cell::new(Some(found));
    resolve_host_id(&cli(&["--host-id-from-facts", FACTS]), EUID, &|path| {
        assert_eq!(path, Path::new(FACTS));
        found.take().expect("read once")
    })
}

fn refused(found: io::Result<FileFacts>) -> String {
    resolved(found).unwrap_err().to_string()
}

fn reason(text: &str) -> String {
    refused(Ok(file(text)))
}

/// The id is the file's `host_id`; fields this agent does not know, at the top and inside
/// `labels`, are ignored, and a file owned by the agent's own user is as good as root's.
/// It catches a reader that refuses a newer hostkeeper's extra fields.
#[test]
fn the_id_is_the_host_id_of_the_file_and_unknown_fields_are_ignored() {
    let text = r#"{"schema":1,"host_id":"web-04","host_group":"web","platform":"linux-x86_64","labels":{"a":"b"},"later":[1,2]}"#;
    assert_eq!(resolved(Ok(file(text))).unwrap().as_deref(), Some("web-04"));
    let own = FileFacts {
        uid: EUID,
        mode: 0o600,
        ..file(text)
    };
    assert_eq!(resolved(Ok(own)).unwrap().as_deref(), Some("web-04"));
}

/// With neither flag there is no override (the machine-id default applies); `--host-id` is
/// the override as given and reads no file.
/// It catches a default that reads a facts file nobody named.
#[test]
fn without_the_facts_flag_the_override_is_the_host_id_flag_or_none() {
    let none = |_: &Path| -> io::Result<FileFacts> { panic!("no file is read") };
    assert_eq!(resolve_host_id(&cli(&[]), EUID, &none).unwrap(), None);
    let named = cli(&["--host-id", "node-5"]);
    assert_eq!(
        resolve_host_id(&named, EUID, &none).unwrap().as_deref(),
        Some("node-5")
    );
}

/// Every way a facts file is refused says the flag, the file and the reason, by exact text.
/// It catches an arm of the reader that is missing or says nothing of the file.
#[test]
fn each_way_a_facts_file_is_refused_says_why() {
    let head = format!("--host-id-from-facts {FACTS}: ");
    let good = r#"{"schema":1,"host_id":"web-04"}"#;
    let with = |change: &dyn Fn(&mut FileFacts)| {
        let mut facts = file(good);
        change(&mut facts);
        refused(Ok(facts))
    };
    assert_eq!(
        refused(Err(io::Error::from(io::ErrorKind::NotFound))),
        format!("{head}cannot be read: entity not found")
    );
    assert_eq!(
        with(&|f| f.regular = false),
        format!("{head}not a regular file")
    );
    assert_eq!(
        with(&|f| f.mode = 0o664),
        format!("{head}writable by group or others (mode 0664)")
    );
    assert_eq!(
        with(&|f| f.mode = 0o602),
        format!("{head}writable by group or others (mode 0602)")
    );
    assert_eq!(
        with(&|f| f.uid = 4242),
        format!("{head}owned by uid 4242, not root or the agent's user")
    );
    assert_eq!(
        with(&|f| f.bytes = vec![b' '; 64 * 1024 + 1]),
        format!("{head}larger than 65536 bytes")
    );
    assert_eq!(
        with(&|f| f.bytes = vec![0xff, 0xfe]),
        format!("{head}not UTF-8")
    );
    assert_eq!(
        reason("host_id=web"),
        format!("{head}not JSON: expected value at line 1 column 1")
    );
    for text in ["[]", "\"web\"", "7", "null"] {
        assert_eq!(reason(text), format!("{head}not a JSON object"), "{text}");
    }
    for text in [
        "{}",
        r#"{"schema":"1","host_id":"a"}"#,
        r#"{"schema":1.0,"host_id":"a"}"#,
        r#"{"schema":-1,"host_id":"a"}"#,
    ] {
        assert_eq!(
            reason(text),
            format!("{head}schema missing or not an integer"),
            "{text}"
        );
    }
    for (text, found) in [
        (r#"{"schema":2,"host_id":"a"}"#, "2"),
        (r#"{"schema":0,"host_id":"a"}"#, "0"),
    ] {
        assert_eq!(
            reason(text),
            format!("{head}schema {found}, this agent reads 1")
        );
    }
    assert_eq!(reason(r#"{"schema":1}"#), format!("{head}host_id missing"));
    for text in [
        r#"{"schema":1,"host_id":7}"#,
        r#"{"schema":1,"host_id":null}"#,
    ] {
        assert_eq!(reason(text), format!("{head}host_id is not a string"));
    }
    for (text, shown) in [
        (r#"{"schema":1,"host_id":"Web 04"}"#, "Web 04"),
        (r#"{"schema":1,"host_id":""}"#, ""),
    ] {
        assert_eq!(
            reason(text),
            format!("{head}host_id: host id {shown:?} must be 1 to 63 characters of [a-z0-9._-]")
        );
    }
}

/// A flags file that carries `--host-id-from-facts=PATH` gives the id: the expanded words
/// parse as the agent's flags and resolve to the file's id.
/// It catches a flag that works on the command line and not from a flags file.
#[test]
fn a_flags_file_can_carry_the_facts_flag() {
    let flags =
        "--server=https://leases.example:7300\n--pool=p\n--host-id-from-facts=/f/facts.json\n";
    let words = expand_with(
        vec![
            OsString::from("js-agent"),
            OsString::from("--flags-file"),
            OsString::from("/f/flags"),
        ],
        EUID,
        &|path| {
            Ok(FileFacts {
                uid: EUID,
                mode: 0o600,
                ..file(if path == Path::new("/f/flags") {
                    flags
                } else {
                    unreachable!()
                })
            })
        },
    )
    .unwrap();
    let cli = Cli::try_parse_from(words).unwrap();
    let id = resolve_host_id(&cli, EUID, &|path| {
        assert_eq!(path, Path::new("/f/facts.json"));
        Ok(file(r#"{"schema":1,"host_id":"mac-build-2"}"#))
    });
    assert_eq!(id.unwrap().as_deref(), Some("mac-build-2"));
}

/// The two flags that choose the id together are a usage error that sends nothing.
/// It catches an agent that takes one flag over the other without saying so.
#[test]
fn the_host_id_flag_and_the_facts_flag_conflict() {
    let words = [
        "js-agent",
        "--server=https://leases.example:7300",
        "--pool=p",
        "--host-id=a",
        "--host-id-from-facts=/f",
    ];
    let said = Cli::try_parse_from(words).unwrap_err().to_string();
    assert!(said.contains("cannot be used with"), "{said}");
    assert!(said.contains("--host-id"), "{said}");
}

/// Runs the agent to its first wait with the facts file of the host `text`; returns the
/// status, what it said and the hello it sent (if any).
fn started(text: Option<FileFacts>, extra: &[&str]) -> (String, String, Option<Value>) {
    let mut host = linux_host();
    if let Some(facts) = text {
        host.files = host.files.with_facts(FACTS, facts);
    }
    let (script, sent) = Script::new(vec![welcome(1)]);
    let mut sleeper = CountingSleeper::stopping_at(1);
    let args = [
        "--server",
        "http://127.0.0.1:1",
        "--host-cert",
        "/nonexistent/host.crt",
        "--host-key",
        "/nonexistent/host.key",
        "--pool",
        "farm",
    ];
    let all: Vec<&str> = args.iter().chain(extra).copied().collect();
    let ran = run(
        &all,
        &host,
        &ScriptConnector::of(script),
        &mut sleeper,
        &mut Lowest,
    );
    let hello = sent.borrow().first().map(|sent| sent.1.clone());
    (ran.status(), ran.err, hello)
}

/// The agent registers under the id of the facts file: the hello carries it, not the
/// machine-id default.
/// It catches an agent that reads the flag and keeps the default id.
#[test]
fn the_hello_carries_the_id_of_the_facts_file() {
    let facts = file(r#"{"schema":1,"host_id":"web-04","host_group":"web"}"#);
    let (status, err, hello) = started(Some(facts), &["--host-id-from-facts", FACTS]);
    assert_eq!(status, code(0), "{err}");
    assert_eq!(hello.unwrap()["host_id"], "web-04");
}

/// A facts file that is missing or refused stops the agent with status 2 and the text of the
/// reason, before any hello; the machine-id default is not used instead.
/// It catches a silent fall back to an id the operator did not ask for.
#[test]
fn a_facts_file_that_cannot_be_used_stops_the_agent_before_the_hello() {
    let (status, err, hello) = started(None, &["--host-id-from-facts", FACTS]);
    assert_eq!(status, code(2), "{err}");
    assert!(hello.is_none());
    assert!(
        err.contains(&format!(
            "--host-id-from-facts {FACTS}: cannot be read: entity not found"
        )),
        "{err}"
    );
    let (status, err, hello) = started(
        Some(file(r#"{"schema":3,"host_id":"web-04"}"#)),
        &["--host-id-from-facts", FACTS],
    );
    assert_eq!(status, code(2), "{err}");
    assert!(hello.is_none());
    assert!(err.contains("schema 3, this agent reads 1"), "{err}");
}

/// Both flags together end the agent with status 2 before any hello.
/// It catches a conflict that clap does not enforce on the agent's real command line.
#[test]
fn both_id_flags_end_the_agent_with_a_usage_status() {
    let facts = file(r#"{"schema":1,"host_id":"web-04"}"#);
    let (status, err, hello) = started(
        Some(facts),
        &["--host-id", "a", "--host-id-from-facts", FACTS],
    );
    assert_eq!(status, code(2), "{err}");
    assert!(hello.is_none());
}

/// The system's own files give the facts of a file as it is: a regular file with its mode and
/// owner and its bytes (read through a link), and an absent file is an error.
/// It catches a system reader that reports a link or a directory as the file.
#[test]
fn the_system_files_look_at_a_facts_file_through_links() {
    use js_agent::{FileReader, SystemFiles};
    let scratch = Scratch::new("host-id-facts-system");
    let target = scratch.write("facts.json", r#"{"schema":1,"host_id":"a"}"#, 0o644);
    let link = scratch.path("link.json");
    std::os::unix::fs::symlink(&target, &link).unwrap();
    for path in [&target, &link] {
        let seen = SystemFiles.facts(path).unwrap();
        assert!(seen.regular);
        assert_eq!(seen.mode, 0o644);
        assert_eq!(seen.bytes, br#"{"schema":1,"host_id":"a"}"#);
    }
    let missing = SystemFiles.facts(&scratch.path("absent")).unwrap_err();
    assert_eq!(missing.kind(), io::ErrorKind::NotFound);
    let boxed: Box<dyn FileReader> = Box::new(SystemFiles);
    assert!(boxed.facts(&target).unwrap().regular);
}
