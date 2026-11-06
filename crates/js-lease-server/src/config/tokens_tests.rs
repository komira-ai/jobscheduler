//! The client tokens: the set of clients and its comparison, and the clients the flags and
//! the configuration file give, with every refusal of a client entry.

use std::ffi::OsString;

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

/// `default`, `ci` and `ops`, in that order.
fn three() -> Clients {
    let mut clients = Clients::only(&token(DEFAULT_TOKEN), "the client of --token-file");
    let ci = clients.add(name("ci"), &token(CI), "--client-token ci".to_owned());
    let ops = clients.add(name("ops"), &token(OPS), "--client-token ops".to_owned());
    assert_eq!((ci, ops), (Ok(()), Ok(())));
    clients
}

/// The name `clients` give `given`, as text.
fn named(clients: &Clients, given: &str) -> Option<String> {
    clients
        .identify(given.as_bytes())
        .map(|name| name.as_str().to_owned())
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
        let found = found.map(|name| name.as_str().to_owned());
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
/// entry holding a token is found by the token; and that the set prints its names only,
/// with no token and no digest. Catches a second client under one name (the record could
/// not tell them apart), two clients sharing a token (revoking one would leave it open),
/// and a token or its digest printed by `Debug`.
#[test]
fn a_client_has_a_name_and_a_token_of_its_own() {
    let mut clients = three();
    let taken = [
        (
            clients.add(name("ci"), &token(STRANGER), "ci again".to_owned()),
            Taken::Name("--client-token ci".to_owned()),
            "name",
            "also the name of --client-token ci",
        ),
        (
            clients.add(name("default"), &token(STRANGER), "x".to_owned()),
            Taken::Name("the client of --token-file".to_owned()),
            "name",
            "also the name of the client of --token-file",
        ),
        (
            clients.add(name("new"), &token(OPS), "x".to_owned()),
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
    assert_eq!(printed, r#"Clients(["default", "ci", "ops"])"#);
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
            r#"Clients(["default", "ci", "ops.team_1-A"])"#
        );
        for (given, wants) in [
            (DEFAULT_TOKEN, Some("default")),
            (CI, Some("ci")),
            (OPS, Some("ops.team_1-A")),
            (JOIN, None),
        ] {
            assert_eq!(named(&config.clients, given).as_deref(), wants);
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
        assert_eq!(format!("{:?}", config.clients), r#"Clients(["default"])"#);
        assert_eq!(
            config.clients.identify(DEFAULT_TOKEN.as_bytes()),
            Some(ClientName::default_client())
        );
        assert_eq!(named(&config.clients, CI), None);
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
            "entry 1: must be NAME=PATH, with NAME 1 to 40 bytes of letters, digits, '.', '_' and '-'"
                .to_owned(),
        ),
        (
            both(rig.client_flags(&[("ci", CI)]), vec![flag.into(), CI.into()]),
            "entry 2: must be NAME=PATH, with NAME 1 to 40 bytes of letters, digits, '.', '_' and '-'"
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
            "client \"ci\": token: unknown field; known: name, token_file".to_owned(),
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
