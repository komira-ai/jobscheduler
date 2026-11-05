//! `cargo xtask gen-job-surface`: the two files it renders, held to goldens for the test
//! definition `tests/support/surface.proto` and to the checked-in files for the job service;
//! the commands the js-cli interpreter builds from a rendered table; and the CLI and the
//! client rendered from one surface.
//!
//! On a difference a test prints the text it rendered as one JSON string, so the golden can
//! be copied from the log exactly.

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;
use std::process::Command as Process;

use js_cli::jobs::{Command, Flag, Idempotency, Kind};
use js_testkit::dir::TestDir;
use prost::Message as _;
use prost_types::field_descriptor_proto::{Label, Type};
use prost_types::{DescriptorProto, FileDescriptorSet};
use syn::{Expr, Item, Lit};
use xtask::job_surface::{CLI_FILE, CLIENT_FILE, Generated, generate};

/// The repository root.
const ROOT: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/../..");

/// The goldens of the test definition's rendering.
const GOLDENS: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/goldens/job_surface");

/// The test definition, compiled by protox (as js-proto's build script compiles the job
/// service) with its comments; with `calls`, only its first that many calls.
fn definition(calls: usize) -> FileDescriptorSet {
    let dir = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/support");
    let mut compiler = protox::Compiler::new([dir]).expect("the include directory");
    compiler.include_source_info(true).include_imports(true);
    compiler
        .open_files(["surface.proto"])
        .expect("the test definition compiles");
    let mut set = compiler.file_descriptor_set();
    let file = set
        .file
        .iter_mut()
        .find(|file| !file.service.is_empty())
        .expect("the service's file");
    file.service[0].method.truncate(calls);
    set
}

/// The rendering of the test definition's first `calls` calls.
fn rendered(calls: usize) -> Generated {
    generate(&definition(calls).encode_to_vec()).expect("the test definition renders")
}

/// `text` as one JSON string, for a failure message.
fn shown(text: &str) -> String {
    serde_json::to_string(text).expect("a string is JSON")
}

/// A scratch directory under the target directory, removed when the test ends.
fn scratch(name: &str) -> TestDir {
    TestDir::under(
        Path::new(env!("CARGO_TARGET_TMPDIR")).join("job-surface"),
        name,
    )
}

/// Runs the xtask program with `args`: its exit status, stdout and stderr.
fn xtask(args: &[&str]) -> (Option<i32>, String, String) {
    let out = Process::new(env!("CARGO_BIN_EXE_xtask"))
        .args(args)
        .output()
        .expect("xtask runs");
    (
        out.status.code(),
        String::from_utf8_lossy(&out.stdout).into_owned(),
        String::from_utf8_lossy(&out.stderr).into_owned(),
    )
}

/// Asserts that the rendering of the test definition (a scalar, a repeated and a map field;
/// a streamed answer; a nested message, an enum, a map of enums, a map of messages, a
/// `oneof`, a recursive message, `bytes`, every scalar group; one request with a single
/// scalar, whose flag is required, and two with several, whose flags are not) equals the
/// goldens byte for byte. A change to a rule of the generator moves these goldens.
#[test]
fn generated_output_matches_the_golden() {
    let got = rendered(3);
    for (name, text) in [("client.rs.txt", &got.client), ("cli.rs.txt", &got.cli)] {
        let want = std::fs::read_to_string(Path::new(GOLDENS).join(name)).unwrap_or_default();
        assert!(
            *text == want,
            "{name} differs from what renders: {}",
            shown(text)
        );
    }
}

/// Asserts that the checked-in client and CLI files are what the generator renders from the
/// job service js-proto builds, through the verb's own `--check`. It fails when the `.proto`
/// files change and the files were not rendered again.
#[test]
fn generated_files_are_current() {
    let (code, out, err) = xtask(&["gen-job-surface", "--check", "--out", ROOT]);
    let want = generate(js_proto::FILE_DESCRIPTOR_SET).expect("the job service renders");
    assert_eq!(
        (code, out.as_str(), err.as_str()),
        (
            Some(0),
            format!("{CLIENT_FILE} and {CLI_FILE} are current\n").as_str(),
            ""
        ),
        "\n{CLIENT_FILE}: {}\n{CLI_FILE}: {}",
        shown(&want.client),
        shown(&want.cli)
    );
}

/// Asserts that `--check` against a copy of the files with one byte of the CLI file changed
/// exits 1 and names that file and only it (T15: the check fails on a stale file).
#[test]
fn check_names_the_stale_file() {
    let dir = scratch("stale");
    for path in [CLIENT_FILE, CLI_FILE] {
        let mut text =
            std::fs::read_to_string(Path::new(ROOT).join(path)).expect("the checked-in file");
        if path == CLI_FILE {
            text.replace_range(0..1, "#");
        }
        let copy = dir.path().join(path);
        std::fs::create_dir_all(copy.parent().expect("a parent")).expect("the copy's directory");
        std::fs::write(copy, text).expect("the copy");
    }
    let root = dir.path().to_str().expect("a UTF-8 path");
    let (code, out, err) = xtask(&["gen-job-surface", "--check", "--out", root]);
    assert_eq!(
        (code, out.as_str(), err.as_str()),
        (
            Some(1),
            "",
            "xtask: not what `cargo xtask gen-job-surface --out .` renders from the job service: crates/js-cli/src/jobs/generated.rs\n"
        )
    );
}

/// Asserts that the verb, given a descriptor file it cannot read or that does not decode,
/// exits 1 naming the file or the decoding and writes nothing under `--out`.
#[test]
fn the_verb_refuses_a_descriptor_it_cannot_use() {
    let dir = scratch("refused");
    let out = dir.path().join("out");
    let missing = dir.path().join("missing.bin");
    let undecodable = dir.path().join("text.bin");
    // `//` is a field tag of wire type 7, which no message has.
    std::fs::write(&undecodable, b"// not a descriptor set\n").expect("the file");
    let run = |descriptor: &Path| {
        xtask(&[
            "gen-job-surface",
            "--descriptor",
            descriptor.to_str().expect("UTF-8"),
            "--out",
            out.to_str().expect("UTF-8"),
        ])
    };
    let (code, said, err) = run(&missing);
    assert_eq!((code, said.as_str()), (Some(1), ""));
    assert!(
        err.starts_with(&format!("xtask: cannot read {}: ", missing.display())),
        "{err}"
    );
    let (code, said, err) = run(&undecodable);
    assert_eq!((code, said.as_str()), (Some(1), ""));
    assert!(
        err.starts_with("xtask: the descriptor set does not decode: "),
        "{err}"
    );
    assert!(!out.exists());
}

/// Asserts that the verb, given an `--out` it cannot write under (a file), exits 1 naming
/// the first file it could not write.
#[test]
fn the_verb_names_a_file_it_cannot_write() {
    let dir = scratch("unwritable");
    let not_a_dir = dir.path().join("not-a-dir");
    std::fs::write(&not_a_dir, b"").expect("the file");
    let (code, said, err) = xtask(&[
        "gen-job-surface",
        "--out",
        not_a_dir.to_str().expect("UTF-8"),
    ]);
    assert_eq!((code, said.as_str()), (Some(1), ""));
    assert!(
        err.starts_with(&format!(
            "xtask: cannot write {}: ",
            not_a_dir.join(CLIENT_FILE).display()
        )),
        "{err}"
    );
}

/// Asserts that the verb, given a descriptor file, writes both files under `--out` (making
/// their directories) as the generator renders them, and says so.
#[test]
fn the_verb_writes_both_files_from_a_descriptor() {
    let dir = scratch("write");
    let descriptor = dir.path().join("surface.bin");
    std::fs::write(&descriptor, definition(3).encode_to_vec()).expect("the descriptor file");
    let out = dir.path().join("out");
    let (code, said, err) = xtask(&[
        "gen-job-surface",
        "--descriptor",
        descriptor.to_str().expect("UTF-8"),
        "--out",
        out.to_str().expect("UTF-8"),
    ]);
    assert_eq!((code, err.as_str()), (Some(0), ""));
    assert_eq!(said, format!("wrote {CLIENT_FILE} and {CLI_FILE}\n"));
    let want = rendered(3);
    let read = |path: &str| std::fs::read_to_string(out.join(path)).expect("a written file");
    assert_eq!((read(CLIENT_FILE), read(CLI_FILE)), (want.client, want.cli));
}

/// Asserts that a call added to the definition, and nothing else, becomes a command: the
/// table of the test definition's first two calls and of all three, each built into commands
/// by js-cli's interpreter, give the same commands for the first two calls, and the third
/// command with exactly one flag per leaf of its request. It catches a call that needs
/// hand-written code and a new call that reorders or alters another command.
#[test]
fn a_new_rpc_appears_as_a_subcommand() {
    let before = tree(&rendered(2).cli);
    let after = tree(&rendered(3).cli);
    assert_eq!(
        order(&rendered(2).cli),
        ["thing", "thing put", "thing watch"]
    );
    assert_eq!(
        order(&rendered(3).cli),
        ["thing", "thing put", "thing watch", "thing shape"]
    );
    assert_eq!(
        before.keys().collect::<Vec<_>>(),
        ["thing put", "thing watch"]
    );
    assert_eq!(
        after.keys().collect::<Vec<_>>(),
        ["thing put", "thing shape", "thing watch"]
    );
    for name in ["thing put", "thing watch"] {
        assert_eq!(before[name], after[name], "{name}");
    }
    assert_eq!(
        after["thing shape"].0,
        [
            "thing-id required",
            "size.width",
            "size.height",
            "size.inner-file",
            "shade",
            "sides repeated",
            "notes-file",
            "tool-file",
            "machine-file",
        ]
    );
}

/// The groups and commands js-cli's interpreter builds from a rendered table, in the order
/// it builds them.
fn order(cli: &str) -> Vec<String> {
    let mut found = Vec::new();
    for group in js_cli::jobs::commands(surface_of(cli)) {
        found.push(group.get_name().to_owned());
        for sub in group.get_subcommands() {
            found.push(format!("{} {}", group.get_name(), sub.get_name()));
        }
    }
    found
}

/// The commands js-cli's interpreter builds from a rendered table, by their words: each
/// command's flags (with `required` and `repeated` marks) and its whole help text.
fn tree(cli: &str) -> BTreeMap<String, (Vec<String>, String)> {
    let mut found = BTreeMap::new();
    for group in js_cli::jobs::commands(surface_of(cli)) {
        for sub in group.get_subcommands() {
            let flags = sub
                .get_arguments()
                .map(|arg| {
                    let mut text = arg.get_id().to_string();
                    if arg.is_required_set() {
                        text.push_str(" required");
                    }
                    if matches!(arg.get_action(), clap::ArgAction::Append) {
                        text.push_str(" repeated");
                    }
                    text
                })
                .collect();
            let help = sub.clone().render_long_help().to_string();
            found.insert(
                format!("{} {}", group.get_name(), sub.get_name()),
                (flags, help),
            );
        }
    }
    found
}

/// Asserts that the CLI and the client come from one surface, reading the two rendered
/// files and the descriptor (not the generator's model): every command dispatches to a
/// client method whose request type is the command's request, every flag names a field of
/// that request (through nested messages), and every leaf field of the request has a flag
/// (a `-file` flag for what the flags cannot spell). It catches a flag the client has no
/// field for and a field the CLI dropped.
#[test]
fn cli_and_client_are_generated_from_the_same_surface() {
    let set = definition(3);
    let got = rendered(3);
    let methods = client_methods(&got.client);
    let messages = messages(&set);
    for command in surface_of(&got.cli) {
        let method = dispatched(&got.cli, command.rpc);
        let request = command.request.rsplit('.').next().unwrap_or_default();
        assert_eq!(
            methods.get(&method).map(String::as_str),
            Some(request),
            "{}",
            command.rpc
        );
        let mut leaves = BTreeSet::new();
        leaf_fields(
            &messages,
            &format!(".{}", command.request),
            &[],
            &mut Vec::new(),
            &mut leaves,
        );
        let flags: BTreeSet<Vec<String>> = command
            .flags
            .iter()
            .map(|flag| flag.field.iter().map(|part| (*part).to_owned()).collect())
            .collect();
        assert_eq!(flags, leaves, "{}", command.rpc);
    }
}

/// The request type of each method of `JobClient` in a rendered client file, by method.
fn client_methods(client: &str) -> BTreeMap<String, String> {
    let file = syn::parse_file(client).expect("the client file parses");
    let mut methods = BTreeMap::new();
    for item in file.items {
        let Item::Impl(block) = item else { continue };
        for member in block.items {
            let syn::ImplItem::Fn(method) = member else {
                continue;
            };
            let request = method.sig.inputs.iter().find_map(|input| match input {
                syn::FnArg::Typed(typed) => match &*typed.ty {
                    syn::Type::Path(path) => path.path.segments.last().map(|s| s.ident.to_string()),
                    _ => None,
                },
                syn::FnArg::Receiver(_) => None,
            });
            // The stream reader's `next` takes no request.
            let Some(request) = request else { continue };
            methods.insert(method.sig.ident.to_string(), request);
        }
    }
    methods
}

/// The client method the dispatcher of a rendered CLI file calls for `rpc`: its own arm, or
/// the fallback arm that the last call takes (`_ /* Name */`).
fn dispatched(cli: &str, rpc: &str) -> String {
    let arm = cli
        .find(&format!("{rpc:?} =>"))
        .or_else(|| cli.find(&format!("_ /* {rpc} */ =>")))
        .expect("a dispatch arm");
    let call = cli[arm..].find("client.").expect("a client call") + arm + "client.".len();
    cli[call..]
        .chars()
        .take_while(|ch| ch.is_alphanumeric() || *ch == '_')
        .collect()
}

/// Every message of `set`, by full name with its leading dot.
fn messages(set: &FileDescriptorSet) -> BTreeMap<String, DescriptorProto> {
    fn add(scope: &str, message: &DescriptorProto, into: &mut BTreeMap<String, DescriptorProto>) {
        let name = format!("{scope}.{}", message.name());
        for nested in &message.nested_type {
            add(&name, nested, into);
        }
        into.insert(name, message.clone());
    }
    let mut all = BTreeMap::new();
    for file in &set.file {
        for message in &file.message_type {
            add(&format!(".{}", file.package()), message, &mut all);
        }
    }
    all
}

/// The paths of the leaf fields of the message `name`, from the request: a singular
/// message (not a map entry, not one already being walked, not a `oneof` member) is walked;
/// every other field is a leaf.
fn leaf_fields(
    messages: &BTreeMap<String, DescriptorProto>,
    name: &str,
    at: &[String],
    walking: &mut Vec<String>,
    leaves: &mut BTreeSet<Vec<String>>,
) {
    walking.push(name.to_owned());
    for field in &messages[name].field {
        let mut path = at.to_vec();
        path.push(field.name().to_owned());
        let inner = messages.get(field.type_name());
        let map = inner.is_some_and(|m| m.options.as_ref().is_some_and(|o| o.map_entry()));
        // A well-known type and a message without fields are read from a file.
        let file = inner.is_some_and(|m| m.field.is_empty())
            || field.type_name().starts_with(".google.protobuf.");
        let walk = field.r#type() == Type::Message
            && field.label() != Label::Repeated
            && !map
            && !file
            && !walking.iter().any(|w| w == field.type_name())
            && (field.oneof_index.is_none() || field.proto3_optional());
        if walk {
            leaf_fields(messages, field.type_name(), &path, walking, leaves);
        } else {
            leaves.insert(path);
        }
    }
    walking.pop();
}

/// The `SURFACE` table of a rendered CLI file, as js-cli's own types (leaked, as the table
/// is `'static`).
fn surface_of(cli: &str) -> &'static [Command] {
    let file = syn::parse_file(cli).expect("the CLI file parses");
    let table = file
        .items
        .iter()
        .find_map(|item| match item {
            Item::Const(item) if item.ident == "SURFACE" => Some((*item.expr).clone()),
            _ => None,
        })
        .expect("a SURFACE table");
    let commands: Vec<Command> = elements(&table).iter().map(command_of).collect();
    Box::leak(commands.into_boxed_slice())
}

/// The elements of `&[...]`.
fn elements(expr: &Expr) -> Vec<Expr> {
    match expr {
        Expr::Reference(reference) => elements(&reference.expr),
        Expr::Array(array) => array.elems.iter().cloned().collect(),
        other => panic!("not an array: {}", kind_name(other)),
    }
}

/// The fields of a struct literal, by name.
fn fields_of(expr: &Expr) -> BTreeMap<String, Expr> {
    let Expr::Struct(literal) = expr else {
        panic!("not a struct: {}", kind_name(expr))
    };
    literal
        .fields
        .iter()
        .map(|field| match &field.member {
            syn::Member::Named(name) => (name.to_string(), field.expr.clone()),
            syn::Member::Unnamed(_) => panic!("a tuple field"),
        })
        .collect()
}

/// What kind of expression `expr` is, for a failure message.
fn kind_name(expr: &Expr) -> &'static str {
    match expr {
        Expr::Lit(_) => "a literal",
        Expr::Path(_) => "a path",
        Expr::Call(_) => "a call",
        _ => "another expression",
    }
}

/// A string literal, leaked.
fn text(expr: &Expr) -> &'static str {
    match expr {
        Expr::Lit(syn::ExprLit {
            lit: Lit::Str(text),
            ..
        }) => Box::leak(text.value().into_boxed_str()),
        other => panic!("not a string: {}", kind_name(other)),
    }
}

/// A boolean literal.
fn boolean(expr: &Expr) -> bool {
    match expr {
        Expr::Lit(syn::ExprLit {
            lit: Lit::Bool(value),
            ..
        }) => value.value,
        other => panic!("not a bool: {}", kind_name(other)),
    }
}

/// `&["a", "b"]`, leaked.
fn texts(expr: &Expr) -> &'static [&'static str] {
    let all: Vec<&'static str> = elements(expr).iter().map(text).collect();
    Box::leak(all.into_boxed_slice())
}

/// The last segment of a path expression.
fn last_segment(expr: &Expr) -> String {
    match expr {
        Expr::Path(path) => path
            .path
            .segments
            .last()
            .map(|s| s.ident.to_string())
            .unwrap_or_default(),
        Expr::Call(call) => last_segment(&call.func),
        Expr::Reference(reference) => last_segment(&reference.expr),
        other => panic!("not a path: {}", kind_name(other)),
    }
}

/// The arguments of a call expression.
fn arguments(expr: &Expr) -> Vec<Expr> {
    match expr {
        Expr::Call(call) => call.args.iter().cloned().collect(),
        Expr::Reference(reference) => arguments(&reference.expr),
        other => panic!("not a call: {}", kind_name(other)),
    }
}

/// One `Command { .. }` of the table.
fn command_of(expr: &Expr) -> Command {
    let fields = fields_of(expr);
    let flags: Vec<Flag> = elements(&fields["flags"]).iter().map(flag_of).collect();
    Command {
        path: texts(&fields["path"]),
        rpc: text(&fields["rpc"]),
        doc: text(&fields["doc"]),
        idempotency: match last_segment(&fields["idempotency"]).as_str() {
            "NoSideEffects" => Idempotency::NoSideEffects,
            "Idempotent" => Idempotency::Idempotent,
            _ => Idempotency::Unknown,
        },
        request: text(&fields["request"]),
        response: text(&fields["response"]),
        streaming: boolean(&fields["streaming"]),
        retry_key: match last_segment(&fields["retry_key"]).as_str() {
            "Some" => Some(texts(&arguments(&fields["retry_key"])[0])),
            _ => None,
        },
        flags: Box::leak(flags.into_boxed_slice()),
    }
}

/// One `Flag { .. }` of the table.
fn flag_of(expr: &Expr) -> Flag {
    let fields = fields_of(expr);
    Flag {
        name: text(&fields["name"]),
        field: texts(&fields["field"]),
        kind: kind_of(&fields["kind"]),
        repeated: boolean(&fields["repeated"]),
        required: boolean(&fields["required"]),
        oneof: match last_segment(&fields["oneof"]).as_str() {
            "Some" => Some(text(&arguments(&fields["oneof"])[0])),
            _ => None,
        },
        doc: text(&fields["doc"]),
    }
}

/// One `Kind::..` of the table.
fn kind_of(expr: &Expr) -> Kind {
    match last_segment(expr).as_str() {
        "Bool" => Kind::Bool,
        "I32" => Kind::I32,
        "I64" => Kind::I64,
        "U32" => Kind::U32,
        "U64" => Kind::U64,
        "F32" => Kind::F32,
        "F64" => Kind::F64,
        "Text" => Kind::Text,
        "Seconds" => Kind::Seconds,
        "File" => Kind::File,
        "Enum" => {
            let choices: Vec<(&'static str, &'static str)> = elements(&arguments(expr)[0])
                .iter()
                .map(|pair| match pair {
                    Expr::Tuple(tuple) => (text(&tuple.elems[0]), text(&tuple.elems[1])),
                    other => panic!("not a pair: {}", kind_name(other)),
                })
                .collect();
            Kind::Enum(Box::leak(choices.into_boxed_slice()))
        }
        "Pair" => {
            let args = arguments(expr);
            let key: &'static Kind = Box::leak(Box::new(kind_of(&args[0])));
            let value: &'static Kind = Box::leak(Box::new(kind_of(&args[1])));
            Kind::Pair(key, value)
        }
        other => panic!("no kind {other}"),
    }
}

/// Asserts the required flags and the retry key the job service's own definition gives:
/// exactly the subject ids of the six calls on one job or host, and `--id` (not required,
/// in place of `--spec.job-id`) as the retry key of `job submit`. It catches a change of
/// the subject rule and a retry-key entry that no longer reaches its field.
#[test]
fn the_job_service_requires_six_flags_and_keys_submit_on_its_id() {
    let table = surface_of(
        &generate(js_proto::FILE_DESCRIPTOR_SET)
            .expect("renders")
            .cli,
    );
    let required: Vec<String> = table
        .iter()
        .flat_map(|command| {
            let words = command.path.join(" ");
            command
                .flags
                .iter()
                .filter(|flag| flag.required)
                .map(move |flag| format!("{words} --{}", flag.name))
        })
        .collect();
    assert_eq!(
        required,
        [
            "job get --job-id",
            "job cancel --job-id",
            "job result --job-id",
            "job logs --job-id",
            "host cordon --host-id",
            "host uncordon --host-id",
        ]
    );
    let keyed: Vec<(String, Vec<&str>, Vec<&str>)> = table
        .iter()
        .filter_map(|command| {
            let key = command.retry_key?;
            let flags = command
                .flags
                .iter()
                .filter(|flag| flag.field == key)
                .map(|flag| flag.name)
                .collect();
            Some((command.path.join(" "), key.to_vec(), flags))
        })
        .collect();
    assert_eq!(
        keyed,
        [("job submit".to_owned(), vec!["spec", "job_id"], vec!["id"])]
    );
}
