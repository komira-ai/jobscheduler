//! The client tokens: the set of clients and its comparison, and the clients the flags and
//! the configuration file give, with every refusal of a client entry.

use std::ffi::OsString;
use std::path::Path;

use js_jobs::ClientName;

use super::*;
use crate::config::Config;
use crate::config::clients_rig::{ClientsRig, DEFAULT_TOKEN, JOIN};

/// The token of the client `ci`.
const CI: &str = "ci-token-0123456789-abcdefghijklmnopqrst";

/// The token of the client `ops`.
const OPS: &str = "ops-token-0123456789-abcdefghijklmnopqrs";

/// A token no client has.
const STRANGER: &str = "stranger-token-0123456789-abcdefghijklmn";

fn token(text: &str) -> Token {
    parse_token(text.as_bytes()).expect("a valid token")
}

fn name(text: &str) -> ClientName {
    ClientName::parse(text).expect("a client name")
}

/// The entry of the client `text`, a `client`, named in refusals by `place`; its file is
/// never read here.
fn entry(text: &str, place: &str) -> Entry {
    Entry {
        name: name(text),
        role: Role::Client,
        place: place.to_owned(),
        file: std::path::PathBuf::new(),
    }
}

/// `default`, `ci` and `ops`, in that order.
fn three() -> Clients {
    let mut clients = Clients::only(&token(DEFAULT_TOKEN), "the client of --token-file");
    let ci = clients.add(entry("ci", "--client-token ci"), &token(CI));
    let ops = clients.add(entry("ops", "--client-token ops"), &token(OPS));
    assert_eq!((ci, ops), (Ok(()), Ok(())));
    clients
}

/// The name `clients` give `given`, as text.
fn named(clients: &Clients, given: &str) -> Option<String> {
    clients
        .identify(given.as_bytes())
        .map(|(name, _)| name.as_str().to_owned())
}

/// Asserts, through the seam that counts the comparisons of digests, that a token of the
/// last client, a token of the first, a token of no client and an empty one each cost one
/// comparison per client (three), and name the client they are or none, as the comparison
/// without the seam does. Catches a loop that returns at the first match (the first
/// client's token would cost one) or at the first mismatch (the last client's would), which
/// would let the time of a refusal tell how far a guess got.
#[test]
fn the_comparison_checks_every_client() {
    let clients = three();
    for (given, wants) in [
        (OPS, Some("ops")),
        (DEFAULT_TOKEN, Some("default")),
        (STRANGER, None),
        ("", None),
    ] {
        let mut made = 0;
        let found = clients.identify_with(given.as_bytes(), |known, presented| {
            made += 1;
            bool::from(known.ct_eq(presented))
        });
        let found = found.map(|(name, _)| name.as_str().to_owned());
        assert_eq!(
            (found.as_deref(), made),
            (wants, 3),
            "{given:?} must be compared with every client"
        );
        assert_eq!(named(&clients, given), found, "{given:?}");
    }
}

/// Asserts that a client is added only with a name and a token of its own: a name taken by
/// an earlier client (`default` among them) and a token an earlier client holds are each
/// refused naming that client's entry and the key at fault, never the token; that the
/// entry holding a token is found by the token; and that the set prints its names and roles only,
/// with no token and no digest. Catches a second client under one name (the record could
/// not tell them apart), two clients sharing a token (revoking one would leave it open),
/// and a token or its digest printed by `Debug`.
#[test]
fn a_client_has_a_name_and_a_token_of_its_own() {
    let mut clients = three();
    let taken = [
        (
            clients.add(entry("ci", "ci again"), &token(STRANGER)),
            Taken::Name("--client-token ci".to_owned()),
            "name",
            "also the name of --client-token ci",
        ),
        (
            clients.add(entry("default", "x"), &token(STRANGER)),
            Taken::Name("the client of --token-file".to_owned()),
            "name",
            "also the name of the client of --token-file",
        ),
        (
            clients.add(entry("new", "x"), &token(OPS)),
            Taken::Token("--client-token ops".to_owned()),
            "token_file",
            "holds the same token as --client-token ops; each client needs its own token",
        ),
    ];
    for (added, wants, key, said) in taken {
        let taken = added.expect_err("refused");
        assert_eq!(
            (&taken, taken.key(), taken.to_string()),
            (&wants, key, said.to_owned())
        );
    }
    assert_eq!(clients.holder_of(&token(CI)), Some("--client-token ci"));
    assert_eq!(clients.holder_of(&token(STRANGER)), None);
    assert_eq!(
        named(&clients, STRANGER),
        None,
        "a refused client was added"
    );
    let printed = format!("{clients:?}");
    assert_eq!(
        printed,
        r#"Clients([("default", "operator"), ("ci", "client"), ("ops", "client")])"#
    );
    for secret in [DEFAULT_TOKEN, CI, OPS] {
        let digest: String = digest(secret.as_bytes())
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect();
        assert!(!printed.contains(secret) && !printed.contains(&digest));
    }
}

/// Asserts that `--client-token NAME=PATH` and a `[[client]]` table give the same clients:
/// the same names in the same order, each named by its own token, `default` by the token of
/// `--token-file` (`server.client_token_file`), and no client by the join token; and that
/// the flag beside `--config` is refused naming the table that takes its place. Catches a
/// table or flag read and dropped, a name given to the wrong token, and a flag let through
/// beside the file.
#[test]
fn flag_and_config_table_agree() {
    let rig = ClientsRig::new("agree");
    let clients = [("ci", CI), ("ops.team_1-A", OPS)];
    let by_flag = rig
        .config_of_flags(&rig.client_flags(&clients))
        .expect("the flags start");
    let tables = rig.client_tables(&clients);
    let text = rig.file("job_listen = \"127.0.0.1:0\"", &tables);
    let by_file = rig.config_of_file(&text).expect("the file starts");
    for config in [&by_flag, &by_file] {
        assert_eq!(
            format!("{:?}", config.clients),
            r#"Clients([("default", "operator"), ("ci", "client"), ("ops.team_1-A", "client")])"#
        );
        for (given, wants) in [
            (DEFAULT_TOKEN, Some("default")),
            (CI, Some("ci")),
            (OPS, Some("ops.team_1-A")),
            (JOIN, None),
        ] {
            assert_eq!(named(&config.clients.now(), given).as_deref(), wants);
        }
    }

    let mut beside: Vec<OsString> = ["js-lease-server", "--config", "js.toml"]
        .map(OsString::from)
        .into();
    beside.extend(rig.client_flags(&[("ci", CI)]));
    let refused = Config::from_cli(Cli::try_parse_from(beside).expect("parses")).err();
    assert_eq!(
        refused.map(|err| err.to_string()).as_deref(),
        Some("--client-token: not allowed with --config; add a [[client]] table")
    );
}

/// Asserts that `--token-file` (and `server.client_token_file`) alone is the one client
/// `default`, named by that token and by no other, and that it is still the token of the
/// lease routes: a server configured as before has the one token it had, so every existing
/// test of the bearer layer runs unchanged against the client `default`. Catches a server
/// that needs a named client to serve jobs, or whose lease routes lost their token.
#[test]
fn token_file_alone_behaves_as_before() {
    let rig = ClientsRig::new("alone");
    let by_flag = rig.config_of_flags(&[]).expect("the flags start");
    let text = rig.file("job_listen = \"127.0.0.1:0\"", "");
    let by_file = rig.config_of_file(&text).expect("the file starts");
    for config in [&by_flag, &by_file] {
        assert_eq!(
            format!("{:?}", config.clients),
            r#"Clients([("default", "operator")])"#
        );
        assert_eq!(
            config.clients.now().identify(DEFAULT_TOKEN.as_bytes()),
            Some((ClientName::default_client(), Role::Operator))
        );
        assert_eq!(named(&config.clients.now(), CI), None);
        assert!(config.token.matches(DEFAULT_TOKEN.as_bytes()));
    }
}

/// The flag and the message of the refusal of a start from the flags with `extra`.
fn flag_refusal(rig: &ClientsRig, extra: &[OsString]) -> (&'static str, String) {
    let err = rig
        .config_of_flags(extra)
        .err()
        .expect("the start is refused");
    (err.flag(), err.to_string())
}

/// The message of the refusal of a start from the file with `server` lines and `tables`.
fn file_refusal(rig: &ClientsRig, server: &str, tables: &str) -> String {
    let text = rig.file(server, tables);
    let err = rig
        .config_of_file(&text)
        .err()
        .expect("the start is refused");
    assert_eq!(err.flag(), "--config");
    err.to_string()
}

/// Asserts each refusal of a client entry at start, from the flags and from the file: two
/// clients of one name, a client named `default` beside `--token-file`, a name that is not
/// one (a flag entry without `=` is named by its position and not repeated, so a token
/// pasted in its place is never printed), a short token, a token file others can read, the
/// same token in two entries, a client whose token is the join token (refused by the join
/// token file, naming the client), and named clients without the job service; and, in the
/// file, a table without a name or without a token file, with a field it does not know, or
/// not written as tables. Each names the flag or the table and the key at fault, and no
/// message holds a token. Catches any of them accepted, a refusal that names the wrong
/// entry, and a token printed in a refusal.
#[test]
fn config_is_validated_naming_the_entry() {
    let rig = ClientsRig::new("refusals");
    let short = rig.token_file("short", "short-token");
    let open = rig.write("open.token", CI, 0o644);
    let entry = |name: &str, path: &std::path::Path| -> Vec<OsString> {
        let value = format!("{name}={}", path.display());
        vec!["--client-token".into(), value.into()]
    };
    let both = |first: Vec<OsString>, second: Vec<OsString>| [first, second].concat();
    let mut refused = Vec::new();

    let flag = "--client-token";
    for (extra, said) in [
        (
            rig.client_flags(&[("ci", CI), ("ci", OPS)]),
            "ci: also the name of --client-token ci".to_owned(),
        ),
        (
            rig.client_flags(&[("default", CI)]),
            "default: also the name of the client of --token-file".to_owned(),
        ),
        (
            rig.client_flags(&[("bad name", CI)]),
            "entry 1: must be NAME=PATH or NAME:ROLE=PATH, with NAME 1 to 40 bytes of letters, digits, '.', '_' and '-' and ROLE \"client\" or \"operator\""
                .to_owned(),
        ),
        (
            both(rig.client_flags(&[("ci", CI)]), vec![flag.into(), CI.into()]),
            "entry 2: must be NAME=PATH or NAME:ROLE=PATH, with NAME 1 to 40 bytes of letters, digits, '.', '_' and '-' and ROLE \"client\" or \"operator\""
                .to_owned(),
        ),
        (
            entry("ci", &short),
            format!(
                "ci: {} holds a token of 11 bytes; a token must have at least 32 (use a random one)",
                short.display()
            ),
        ),
        (
            rig.client_flags(&[("ci", CI), ("ops", CI)]),
            "ops: holds the same token as --client-token ci; each client needs its own token"
                .to_owned(),
        ),
        (
            rig.client_flags(&[("ci", DEFAULT_TOKEN)]),
            "ci: holds the same token as the client of --token-file; each client needs its own token"
                .to_owned(),
        ),
    ] {
        let (at, message) = flag_refusal(&rig, &extra);
        assert_eq!((at, message), (flag, format!("{flag}: {said}")));
        refused.push(format!("{flag}: {said}"));
    }
    let (at, message) = flag_refusal(&rig, &entry("ci", &open));
    let prefix = format!("{flag}: ci: {} must be a regular file", open.display());
    assert_eq!(at, flag);
    assert!(message.starts_with(&prefix), "{message}");
    assert!(message.contains("not mode 0644"), "{message}");
    refused.push(message);

    let (at, message) = flag_refusal(&rig, &rig.client_flags(&[("ci", JOIN)]));
    let joins = rig.path("joins");
    let said = format!(
        "--agent-token-file: {} holds the client token of --client-token ci: the two must differ",
        joins.display()
    );
    assert_eq!(
        (at, message.as_str()),
        ("--agent-token-file", said.as_str())
    );
    refused.push(message);

    let mut no_job = rig.flags(&rig.client_flags(&[("ci", CI)]));
    let at = no_job
        .iter()
        .position(|arg| arg == "--job-listen")
        .expect("the rig serves jobs");
    no_job.drain(at..at + 2);
    let err = Config::from_cli(Cli::try_parse_from(no_job).expect("parses")).err();
    let said = err.map(|err| (err.flag(), err.to_string()));
    let wants = (flag, "--client-token: requires --job-listen".to_owned());
    assert_eq!(said, Some(wants));

    let serves = "job_listen = \"127.0.0.1:0\"";
    let table = |lines: &str| format!("[[client]]\n{lines}\n");
    let short_line = format!("token_file = {:?}", short.display().to_string());
    let ci_line = format!(
        "token_file = {:?}",
        rig.token_file("ci", CI).display().to_string()
    );
    for (server, tables, said) in [
        (
            serves,
            rig.client_tables(&[("ci", CI), ("ci", OPS)]),
            "client \"ci\": name: also the name of [[client]] \"ci\"".to_owned(),
        ),
        (
            serves,
            rig.client_tables(&[("default", CI)]),
            "client \"default\": name: also the name of the client of server.client_token_file"
                .to_owned(),
        ),
        (
            serves,
            rig.client_tables(&[("ci", CI), ("bad name", OPS)]),
            "client #2: name: must be 1 to 40 bytes of letters, digits, '.', '_' and '-'".to_owned(),
        ),
        (
            serves,
            table(&ci_line),
            "client #1: name: required".to_owned(),
        ),
        (
            serves,
            table(&format!("name = 7\n{ci_line}")),
            "client #1: name: must be a string".to_owned(),
        ),
        (
            serves,
            table("name = \"ci\""),
            "client \"ci\": token_file: required".to_owned(),
        ),
        (
            serves,
            table(&format!("name = \"ci\"\n{ci_line}\ntoken = \"x\"")),
            "client \"ci\": token: unknown field; known: name, token_file, role".to_owned(),
        ),
        (
            serves,
            table(&format!("name = \"ci\"\n{short_line}")),
            format!(
                "client \"ci\": token_file: {} holds a token of 11 bytes; a token must have at least 32 (use a random one)",
                short.display()
            ),
        ),
        (
            serves,
            rig.client_tables(&[("ci", CI), ("ops", CI)]),
            "client \"ops\": token_file: holds the same token as [[client]] \"ci\"; each client needs its own token"
                .to_owned(),
        ),
        (
            serves,
            rig.client_tables(&[("ci", JOIN)]),
            format!(
                "pool \"burst\": join_token_file: {} holds the client token of [[client]] \"ci\": the two must differ",
                joins.display()
            ),
        ),
        (
            "",
            rig.client_tables(&[("ci", CI)]),
            "client: needs server.job_listen, the job service it serves".to_owned(),
        ),
    ] {
        let message = file_refusal(&rig, server, &tables);
        assert_eq!(message, said);
        refused.push(message);
    }
    let untabled = format!("client = 7\n{}", rig.file(serves, ""));
    let err = rig
        .config_of_file(&untabled)
        .err()
        .map(|err| err.to_string());
    let said = "client: must be an array of tables, written [[client]]";
    assert_eq!(err.as_deref(), Some(said));
    let open_line = format!("token_file = {:?}", open.display().to_string());
    let message = file_refusal(&rig, serves, &table(&format!("name = \"ci\"\n{open_line}")));
    let prefix = format!(
        "client \"ci\": token_file: {} must be a regular file",
        open.display()
    );
    assert!(message.starts_with(&prefix), "{message}");
    refused.push(message);

    for message in &refused {
        for secret in [DEFAULT_TOKEN, CI, OPS, JOIN] {
            assert!(!message.contains(secret), "{message}");
        }
    }
}

/// Asserts that a named client is a `client` unless its entry says `operator`, by flag
/// (`NAME=PATH`, `NAME:client=PATH`, `NAME:operator=PATH`) and by table (no `role`,
/// `role = "client"`, `role = "operator"`), and that `default` is an operator; and that a
/// role that is neither is refused at start, by flag at the entry's position and by table at
/// its `role` key, never repeating the value, as is a role that is no string. Catches an entry
/// with no role read as an operator, a role read and dropped, a misspelt role let through as
/// either role, and a refusal that echoes what was given in a token's place.
#[test]
fn a_named_client_is_a_client_unless_its_entry_says_operator() {
    let rig = ClientsRig::new("roles");
    let flag = |name: &str, token: &str| -> Vec<OsString> {
        let path = rig.token_file("role", token);
        vec![
            "--client-token".into(),
            format!("{name}={}", path.display()).into(),
        ]
    };
    let flags = [
        flag("ci", CI),
        flag("rel:client", STRANGER),
        flag("ops:operator", OPS),
    ]
    .concat();
    let by_flag = rig.config_of_flags(&flags).expect("the flags start");
    let table = |name: &str, token: &str, role: &str| {
        let path = rig.token_file(name, token);
        format!(
            "[[client]]\nname = {name:?}\ntoken_file = {:?}\n{role}\n",
            path.display().to_string()
        )
    };
    let tables = [
        table("ci", CI, ""),
        table("rel", STRANGER, "role = \"client\""),
        table("ops", OPS, "role = \"operator\""),
    ]
    .concat();
    let text = rig.file("job_listen = \"127.0.0.1:0\"", &tables);
    let by_file = rig.config_of_file(&text).expect("the file starts");
    for config in [&by_flag, &by_file] {
        assert_eq!(
            format!("{:?}", config.clients),
            r#"Clients([("default", "operator"), ("ci", "client"), ("rel", "client"), ("ops", "operator")])"#
        );
        let now = config.clients.now();
        assert_eq!(
            now.identify(OPS.as_bytes()).map(|(_, role)| role),
            Some(Role::Operator)
        );
        assert_eq!(
            now.identify(CI.as_bytes()).map(|(_, role)| role),
            Some(Role::Client)
        );
    }

    let wrong = flag("ci:admin", CI);
    let refused = rig
        .config_of_flags(&wrong)
        .err()
        .expect("refused")
        .to_string();
    assert!(
        refused.starts_with("--client-token: entry 1: must be"),
        "{refused}"
    );
    assert!(!refused.contains("admin"), "{refused}");
    for (role, wants) in [
        (
            "role = \"admin\"",
            "client \"ci\": role: must be \"client\" or \"operator\"",
        ),
        (
            "role = \"Operator\"",
            "client \"ci\": role: must be \"client\" or \"operator\"",
        ),
        ("role = 3", "client \"ci\": role:"),
    ] {
        let text = rig.file("job_listen = \"127.0.0.1:0\"", &table("ci", CI, role));
        let refused = rig
            .config_of_file(&text)
            .err()
            .expect("refused")
            .to_string();
        assert!(refused.contains(wants), "{role}: {refused}");
        assert!(
            !refused.contains("admin") && !refused.contains("Operator\""),
            "{refused}"
        );
    }
}

/// Asserts the reload of the set itself, with the files read by a function the test gives:
/// every named client's file is read again in order (`default`'s never, it has none), the
/// names, roles and order are kept, a new token replaces the old one and the call says the
/// tokens changed, a second reload with the same files says they did not, and any refusal
/// (a file the reader cannot read, a token `taken` refuses) keeps the set in force whole.
/// Catches a reload that reads `default`'s token again, drops a role, reorders the clients,
/// replaces part of the set before a later file fails, or reports every reload as a change.
#[test]
fn a_reload_replaces_the_whole_set_or_nothing() {
    let mut clients = Clients::only(&token(DEFAULT_TOKEN), "the client of --token-file");
    for (text, role, path) in [("ci", Role::Client, "ci"), ("ops", Role::Operator, "ops")] {
        let entry = Entry {
            name: name(text),
            role,
            place: format!("--client-token {text}"),
            file: std::path::PathBuf::from(path),
        };
        let first = if text == "ci" { CI } else { OPS };
        assert_eq!(clients.add(entry, &token(first)), Ok(()));
    }
    let live = LiveClients::new(clients);
    let files = std::cell::RefCell::new(std::collections::BTreeMap::from([
        ("ci", STRANGER.to_owned()),
        ("ops", OPS.to_owned()),
    ]));
    let read_paths = std::cell::RefCell::new(Vec::new());
    let read = |path: &Path| -> Result<Vec<u8>, String> {
        let key = path.to_str().unwrap_or_default();
        read_paths.borrow_mut().push(key.to_owned());
        files
            .borrow()
            .get(key)
            .map(|text| text.as_bytes().to_vec())
            .ok_or_else(|| format!("{key}: gone"))
    };
    let free = |_: &Token| None;
    assert_eq!(live.reload(&read, &free), Ok(true));
    assert_eq!(*read_paths.borrow(), ["ci", "ops"]);
    assert_eq!(
        format!("{live:?}"),
        r#"Clients([("default", "operator"), ("ci", "client"), ("ops", "operator")])"#
    );
    assert_eq!(named(&live.now(), STRANGER).as_deref(), Some("ci"));
    assert_eq!(named(&live.now(), CI), None);
    assert_eq!(
        named(&live.now(), DEFAULT_TOKEN).as_deref(),
        Some("default")
    );
    assert_eq!(live.reload(&read, &free), Ok(false));

    files.borrow_mut().insert("ci", CI.to_owned());
    files.borrow_mut().remove("ops");
    assert_eq!(
        live.reload(&read, &free),
        Err("--client-token ops: ops: gone".to_owned())
    );
    assert_eq!(
        named(&live.now(), STRANGER).as_deref(),
        Some("ci"),
        "a part was replaced"
    );
    files.borrow_mut().insert("ops", OPS.to_owned());
    let join = |token: &Token| {
        token
            .matches(CI.as_bytes())
            .then(|| "is the join token".to_owned())
    };
    assert_eq!(
        live.reload(&read, &join),
        Err("--client-token ci: is the join token".to_owned())
    );
    assert_eq!(named(&live.now(), STRANGER).as_deref(), Some("ci"));
}
