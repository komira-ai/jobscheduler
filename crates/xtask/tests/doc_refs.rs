//! The documentation check (`xtask::doc_refs`): the runbooks and the gateway's page name only
//! `js` commands, flags and `/v1/` routes that exist; and the check's own rules, each on a
//! small tree, text or source directory made here.

use std::path::Path;

use clap::{Arg, ArgAction, Command};
use js_testkit::dir::TestDir;
use xtask::doc_refs::{
    DocError, Mention, Stale, check, commands, documents, resolve, route_exists, routes,
    server_routes,
};

/// The repository root.
const ROOT: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/../..");

/// A scratch directory under the target directory, removed when the test ends.
fn scratch(name: &str) -> TestDir {
    TestDir::under(
        Path::new(env!("CARGO_TARGET_TMPDIR")).join("doc-refs"),
        name,
    )
}

/// Writes `text` to `path` under `dir`, making its directory.
fn put(dir: &TestDir, path: &str, text: &str) {
    let path = dir.path().join(path);
    std::fs::create_dir_all(path.parent().expect("a parent")).expect("the directory");
    std::fs::write(path, text).expect("the file");
}

/// The templates of the gateway's routes.
fn gateway() -> Vec<&'static str> {
    js_api_gateway::ROUTES
        .iter()
        .map(|route| route.template)
        .collect()
}

/// Asserts that every `js` command the runbooks show resolves against the tree `js` parses
/// with, and every `/v1/` route they and `docs/http-gateway.md` name is a route of the
/// gateway or of the lease server, or the start of one. On a failure it lists each place as
/// `file:line: token: why`.
#[test]
fn the_runbooks_name_only_commands_and_routes_that_exist() {
    let stale = check(Path::new(ROOT), &js_cli::command(), &gateway()).expect("the check runs");
    let shown: Vec<String> = stale.iter().map(Stale::to_string).collect();
    assert!(shown.is_empty(), "{}", shown.join("\n"));
}

/// Asserts that the check reads the runbooks and the page of the repository, and that the
/// server's own routes it reads hold the lease, agent, host, pool and usage routes.
#[test]
fn the_check_reads_the_repositorys_documents_and_routes() {
    let read = documents(Path::new(ROOT)).expect("the runbooks are listed");
    for file in [
        "docs/runbooks/cordon-a-node.md",
        "docs/runbooks/first-live-launch.md",
        "docs/http-gateway.md",
    ] {
        assert!(read.iter().any(|name| name == file), "{file}: {read:?}");
    }
    assert_eq!(
        read.last().map(String::as_str),
        Some("docs/http-gateway.md")
    );
    let server = server_routes(&Path::new(ROOT).join("crates/js-lease-server/src"))
        .expect("the server's source reads");
    for route in [
        "/v1/leases",
        "/v1/agent/hello",
        "/v1/hosts/{target}",
        "/v1/pools/{pool}/brake/reset",
        "/v1/usage/nodes/{*id}",
    ] {
        assert!(
            server.iter().any(|known| known == route),
            "{route}: {server:?}"
        );
    }
}

/// A small `js`: the global `--server` (with a value) and `--json`, `lease acquire
/// --probe-host HOST` and `lease list`, `job get --job-id ID` and `job logs ID`.
fn tree() -> Command {
    let global = |name: &'static str, action: ArgAction| {
        Arg::new(name).long(name).global(true).action(action)
    };
    let mut tree = Command::new("js")
        .arg(global("server", ArgAction::Set))
        .arg(global("json", ArgAction::SetTrue))
        .subcommand(
            Command::new("lease")
                .subcommand(Command::new("acquire").arg(Arg::new("probe-host").long("probe-host")))
                .subcommand(Command::new("list")),
        )
        .subcommand(
            Command::new("job")
                .subcommand(Command::new("get").arg(Arg::new("job-id").long("job-id")))
                .subcommand(Command::new("logs").arg(Arg::new("id"))),
        );
    tree.build();
    tree
}

/// Asserts the words `resolve` accepts: global flags before and after the words, a flag with
/// its value given after `=` or as the next word (which is then not read as a command), a
/// flag without a value, a placeholder with a space in it, a group named alone, an argument
/// after a command without subcommands, white space at the end, and every word after `--`,
/// `...` or a shell operator.
#[test]
fn a_command_of_the_tree_resolves() {
    let tree = tree();
    for command in [
        "js --server <url with spaces> --json lease list",
        "js lease list --server=https://x --json",
        "js --server job job get --job-id 'a b'",
        "js lease acquire --probe-host host",
        "js lease",
        "js job logs some-id more",
        "js lease list -- nvidia-smi --not-a-flag",
        "js lease list ... --anything",
        "js lease list | jq --no-such",
        "js lease list || true",
        "js lease list && frob",
        "js lease list ; frob",
        "js lease list > out --x",
        "js lease list 2>/dev/null --x",
        "js lease list # --x",
        "js lease list   ",
    ] {
        assert_eq!(resolve(&tree, command), Ok(()), "{command}");
    }
}

/// Asserts that `resolve` refuses, naming the word and what is missing: a group or command
/// that does not exist (also a placeholder where a command is due), a flag the command does
/// not have (a lease flag on a job command, a misspelt global), a flag given with `=`, and a
/// quoted word, which is read with its quotes.
#[test]
fn a_command_that_is_not_in_the_tree_is_refused_by_its_word() {
    let tree = tree();
    let refused = |command: &str| resolve(&tree, command).expect_err(command);
    let pair = |word: &str, why: &str| (word.to_owned(), why.to_owned());
    assert_eq!(
        refused("js host list"),
        pair("host", "`js` has no command host")
    );
    assert_eq!(
        refused("js \"lease\" list"),
        pair("\"lease\"", "`js` has no command \"lease\"")
    );
    assert_eq!(
        refused("js lease release <id>"),
        pair("release", "`js lease` has no command release")
    );
    assert_eq!(
        refused("js job <verb>"),
        pair("<verb>", "`js job` has no command <verb>")
    );
    assert_eq!(
        refused("js job get --probe-host h"),
        pair("--probe-host", "`js job get` has no flag --probe-host")
    );
    assert_eq!(
        refused("js --sever x lease list"),
        pair("--sever", "`js` has no flag --sever")
    );
    assert_eq!(
        refused("js lease acquire --probe=h"),
        pair("--probe=h", "`js lease acquire` has no flag --probe")
    );
}

/// Asserts which text `commands` reads as a command, with its line: a fenced line starting
/// `js ` (also indented, after a `$ ` prompt, and joined across `\` continuations, at its
/// first line, also when a fence closes it), and an inline span starting `js `; and which
/// it does not: a fenced line of
/// another program, `js-agent`, a span that is not a command, text outside a span, and
/// a fence's own lines.
#[test]
fn commands_are_read_from_fences_and_spans() {
    let text = "Run `js lease list` or `js-agent cordon`, not js job get.\n\
                ```sh\n\
                \x20  js --server x \\\n\
                \x20     lease acquire --probe-host h\n\
                $ js job get --job-id a\n\
                js-agent cordon --for 2h\n\
                curl /v1/pools\n\
                ```\n\
                `js` and `jsx` and `js job logs a` here.\n\
                ```\n\
                js lease list --json \\\n\
                ```\n";
    let mention = |line: usize, text: &str| Mention {
        line,
        text: text.to_owned(),
    };
    assert_eq!(
        commands(text),
        [
            mention(1, "js lease list"),
            mention(3, "js --server x        lease acquire --probe-host h"),
            mention(5, "js job get --job-id a"),
            mention(9, "js job logs a"),
            mention(11, "js lease list --json"),
        ]
    );
}

/// Asserts which text `routes` reads as a route, with its line: in a URL, in code and in
/// prose, several on one line, up to the first character that cannot be in a template (a
/// space, a backtick, `?`, `)`), without a final `.`, `:` or `,`, custom verbs and
/// wildcards kept; and that `/v1` without a slash is no route.
#[test]
fn routes_are_read_anywhere_in_the_text() {
    let text = "curl https://h:7300/v1/pools?x=1 and `/v1/hosts/{id}:cordon`.\n\
                Under /v1/, (/v1/hosts/{*rest}) and /v1/leases...\n\
                Ends /v1/jobs: then /v1/usage, /v1 alone.\n";
    let named: Vec<(usize, String)> = routes(text)
        .into_iter()
        .map(|mention| (mention.line, mention.text))
        .collect();
    let want = [
        (1, "/v1/pools"),
        (1, "/v1/hosts/{id}:cordon"),
        (2, "/v1/"),
        (2, "/v1/hosts/{*rest}"),
        (2, "/v1/leases"),
        (3, "/v1/jobs"),
        (3, "/v1/usage"),
    ];
    let want: Vec<(usize, String)> = want
        .into_iter()
        .map(|(line, route)| (line, route.to_owned()))
        .collect();
    assert_eq!(named, want);
}

/// Asserts how a named route meets the templates: a literal path, a value in a variable's
/// place (also empty, a percent escape, or a placeholder), a custom verb only on a template
/// with that verb (a whole variable takes `a:stop` as a value), a wildcard in the template or in the name, the start of a route, and a
/// final `/`; and that a different literal, a missing or wrong verb, a literal where the
/// template has a verb on a literal, and a path longer than every template are refused.
#[test]
fn a_route_is_matched_segment_by_segment() {
    let known: Vec<String> = [
        "/v1/jobs/{job_id}",
        "/v1/jobs/{job_id}/result",
        "/v1/jobs/{job_id}:cancel",
        "/v1/pools:reset",
        "/v1/usage/nodes/{*id}",
        "/v1/agent/hello",
    ]
    .iter()
    .map(|route| (*route).to_owned())
    .collect();
    for route in [
        "/v1/jobs/a",
        "/v1/jobs/%61",
        "/v1/jobs/{id}",
        "/v1/jobs//result",
        "/v1/jobs/x:cancel",
        "/v1/jobs/{id}:cancel",
        "/v1/pools:reset",
        "/v1/usage/nodes/a/b/c",
        "/v1/usage/{*rest}",
        "/v1/agent",
        "/v1/",
        "/v1/jobs/",
    ] {
        assert!(route_exists(route, &known), "{route}");
    }
    for route in [
        "/v1/leases",
        "/v1/pools:other",
        "/v1/other:reset",
        "/v1/pools",
        "/v1/agent/hello/again",
        "/v1/jobs/a/logs",
    ] {
        assert!(!route_exists(route, &known), "{route}");
    }
}

/// A source tree of a server: modules in a file of their own (`a.rs`, with a child in
/// `a/` and a `#[path]` one beside it), in a directory (`b/mod.rs`, with a child `b/c.rs`),
/// at a `#[path]` (whose own child is beside it), and inline (with a child and a `#[path]`
/// one in the inline module's directory); modules under `cfg(not(test))` (kept);
/// modules under a `cfg` that names `test` (left out: their files do not exist); and calls
/// of `route` with a `/v1/` literal, another literal, a constant, and `/v1/` literals given
/// to another method, in a function or `impl` under `cfg(test)` and in `#[test]` functions
/// (left out), and in a method of a plain `impl` (kept).
fn server(dir: &TestDir) {
    put(
        dir,
        "lib.rs",
        r#"
        mod a;
        mod b;
        #[path = "elsewhere/d.rs"]
        mod d;
        #[cfg(test)]
        mod tests;
        #[cfg(all(unix, test))]
        mod more_tests;
        #[cfg(feature = "x")]
        #[allow(dead_code)]
        #[doc = "a module"]
        #[path = 3]
        #[automatically_derived]
        mod e;
        #[cfg(not(test))]
        mod f;
        #[cfg(any(not(test), unix))]
        mod g;
        mod inline {
            mod nested;
            #[path = "p.rs"]
            mod pathed;
            fn f(r: R) { r.route("/v1/inline", x).route(NAME, y).route("/healthz", z); }
        }
        fn g(r: R) { r.other("/v1/not-a-route"); }
        #[cfg(test)]
        fn t(r: R) { r.route("/v1/test-fn", x); }
        #[test]
        fn u(r: R) { r.route("/v1/test-attr", x); }
        #[cfg(test)]
        impl Y { fn q(r: R) { r.route("/v1/test-impl", x); } }
        impl X {
            #[test]
            fn m(r: R) { r.route("/v1/test-method", x); }
            #[inline]
            fn p(r: R) { r.route("/v1/method", x); }
        }
        "#,
    );
    put(
        dir,
        "a.rs",
        "mod child;\n#[path = \"sib.rs\"]\nmod sib;\nfn f(r: R) { r.route(\"/v1/a\", x); }\n",
    );
    put(dir, "sib.rs", "fn f(r: R) { r.route(\"/v1/sib\", x); }\n");
    put(
        dir,
        "a/child.rs",
        "fn f(r: R) { r.route(\"/v1/a/child\", x); }\n",
    );
    put(dir, "b/mod.rs", "mod c;\n");
    put(
        dir,
        "b/c.rs",
        "fn f(r: R) { r.route(\"/v1/c\", x).route(\"/v1/a\", y); }\n",
    );
    put(
        dir,
        "elsewhere/d.rs",
        "mod deep;\nfn f(r: R) { r.route(\"/v1/d\", x); }\n",
    );
    put(
        dir,
        "elsewhere/deep.rs",
        "fn f(r: R) { r.route(\"/v1/deep\", x); }\n",
    );
    put(dir, "e.rs", "fn f(r: R) { r.route(\"/v1/e\", x); }\n");
    put(dir, "f.rs", "fn f(r: R) { r.route(\"/v1/f\", x); }\n");
    put(dir, "g.rs", "fn f(r: R) { r.route(\"/v1/g\", x); }\n");
    put(
        dir,
        "inline/nested.rs",
        "fn f(r: R) { r.route(\"/v1/nested\", x); }\n",
    );
    put(
        dir,
        "inline/p.rs",
        "fn f(r: R) { r.route(\"/v1/pathed\", x); }\n",
    );
}

/// Asserts that `server_routes` gives each `/v1/` literal of a `route` call once, in order,
/// from every production module however it is declared, and nothing from a module under a
/// test `cfg`, test code inside a production file, a non-`/v1/` or non-literal route, or
/// another method.
#[test]
fn the_servers_routes_are_read_from_its_production_modules() {
    let dir = scratch("server");
    server(&dir);
    assert_eq!(
        server_routes(dir.path()).expect("the tree reads"),
        [
            "/v1/a",
            "/v1/a/child",
            "/v1/c",
            "/v1/d",
            "/v1/deep",
            "/v1/e",
            "/v1/f",
            "/v1/g",
            "/v1/inline",
            "/v1/method",
            "/v1/nested",
            "/v1/pathed",
            "/v1/sib"
        ]
    );
}

/// Asserts that `server_routes` fails naming a module whose file is missing and a file that
/// does not parse.
#[test]
fn a_server_source_that_cannot_be_read_is_named() {
    let dir = scratch("broken");
    put(&dir, "lib.rs", "mod gone;\n");
    let said = server_routes(dir.path())
        .expect_err("a missing module")
        .to_string();
    let gone = dir.path().join("gone").join("mod.rs");
    assert!(
        said.starts_with(&format!("cannot read {}: ", gone.display())),
        "{said}"
    );
    put(&dir, "lib.rs", "fn (\n");
    let said = server_routes(dir.path()).expect_err("Rust that does not parse");
    assert!(matches!(said, DocError::Parse { .. }), "{said}");
    let lib = dir.path().join("lib.rs");
    assert!(
        said.to_string()
            .starts_with(&format!("cannot parse {}: ", lib.display()))
    );
}

/// Asserts the whole check on a repository made here: every stale place of every document
/// (runbooks in name order, a file that is not `.md` left out, then the gateway's page), each
/// document's places in line order with the file, line, word and reason, as the test prints
/// them; and that a repository without runbooks is refused naming the directory.
#[test]
fn the_check_names_each_stale_place_by_file_and_line() {
    let dir = scratch("repo");
    let src = dir.path().join("crates/js-lease-server/src");
    std::fs::create_dir_all(&src).expect("the source directory");
    std::fs::write(
        src.join("lib.rs"),
        "fn f(r: R) { r.route(\"/v1/leases\", x); }\n",
    )
    .expect("lib.rs");
    put(
        &dir,
        "docs/runbooks/b.md",
        "Use `js host list` on /v1/hosts.\nThen `js lease list`.\n",
    );
    put(
        &dir,
        "docs/runbooks/a.md",
        "```\njs lease acquire --probe-host h\njs lease acquire --class x\n```\nSee /v1/leases.\n",
    );
    put(&dir, "docs/runbooks/notes.txt", "`js nothing`\n");
    put(
        &dir,
        "docs/http-gateway.md",
        "Routes /v1/jobs and /v1/frob.\n",
    );
    let stale = check(dir.path(), &tree(), &["/v1/jobs/{job_id}"]).expect("the check runs");
    let shown: Vec<String> = stale.iter().map(Stale::to_string).collect();
    assert_eq!(
        shown,
        [
            "docs/runbooks/a.md:3: --class: `js lease acquire` has no flag --class",
            "docs/runbooks/b.md:1: /v1/hosts: no route of the gateway or of the server starts so",
            "docs/runbooks/b.md:1: host: `js` has no command host",
            "docs/http-gateway.md:1: /v1/frob: no route of the gateway or of the server starts so",
        ]
    );
    let bare = scratch("bare");
    let said = check(bare.path(), &tree(), &[])
        .expect_err("no source")
        .to_string();
    assert!(said.starts_with("cannot read "), "{said}");
    std::fs::create_dir_all(bare.path().join("crates/js-lease-server/src")).expect("src");
    std::fs::write(bare.path().join("crates/js-lease-server/src/lib.rs"), "").expect("lib.rs");
    let said = check(bare.path(), &tree(), &[])
        .expect_err("no runbooks")
        .to_string();
    let runbooks = bare.path().join("docs/runbooks");
    assert!(
        said.starts_with(&format!("cannot read {}: ", runbooks.display())),
        "{said}"
    );
    put(&bare, "docs/runbooks/a.md", "");
    let said = check(bare.path(), &tree(), &[])
        .expect_err("no page")
        .to_string();
    let page = bare.path().join("docs/http-gateway.md");
    assert!(
        said.starts_with(&format!("cannot read {}: ", page.display())),
        "{said}"
    );
}
