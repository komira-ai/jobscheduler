//! The generator's refusals, each on a small descriptor set made by hand: what it cannot
//! read, what it cannot render, and the retry-key entry that names a call or field the
//! definition lost.

use std::path::{Path, PathBuf};

use prost::Message as _;
use prost_types::field_descriptor_proto::{Label, Type};
use prost_types::method_options::IdempotencyLevel;
use prost_types::{
    DescriptorProto, FieldDescriptorProto, FileDescriptorProto, FileDescriptorSet, MessageOptions,
    MethodDescriptorProto, MethodOptions, ServiceDescriptorProto,
};

use super::docs::{page_with, route_table, served_paragraph, splice};
use super::{Args, generate, run};

/// A field `name` of `kind`; `type_name` names its enum or message, when it has one.
fn field(name: &str, number: i32, kind: Type, type_name: &str) -> FieldDescriptorProto {
    FieldDescriptorProto {
        name: Some(name.to_owned()),
        number: Some(number),
        label: Some(Label::Optional.into()),
        r#type: Some(kind.into()),
        type_name: Some(type_name.to_owned()),
        ..FieldDescriptorProto::default()
    }
}

/// A message `name` with `fields`.
fn message(name: &str, fields: Vec<FieldDescriptorProto>) -> DescriptorProto {
    DescriptorProto {
        name: Some(name.to_owned()),
        field: fields,
        ..DescriptorProto::default()
    }
}

/// A call `name` from `input` to the message `Empty`.
fn method(name: &str, input: &str) -> MethodDescriptorProto {
    MethodDescriptorProto {
        name: Some(name.to_owned()),
        input_type: Some(input.to_owned()),
        output_type: Some(".p.Empty".to_owned()),
        ..MethodDescriptorProto::default()
    }
}

/// The encoded set of one file of package `package` holding `messages`, the message
/// `Empty` and the service `Svc` with `methods`.
fn set(
    package: &str,
    methods: Vec<MethodDescriptorProto>,
    messages: Vec<DescriptorProto>,
) -> Vec<u8> {
    let mut messages = messages;
    messages.push(message("Empty", Vec::new()));
    let file = FileDescriptorProto {
        name: Some("p.proto".to_owned()),
        package: Some(package.to_owned()),
        message_type: messages,
        service: vec![ServiceDescriptorProto {
            name: Some("Svc".to_owned()),
            method: methods,
            ..ServiceDescriptorProto::default()
        }],
        ..FileDescriptorProto::default()
    };
    FileDescriptorSet { file: vec![file] }.encode_to_vec()
}

/// What `generate` says of `descriptor`, or "" when it renders.
fn refused(descriptor: &[u8]) -> String {
    generate(descriptor)
        .err()
        .map(|error| error.to_string())
        .unwrap_or_default()
}

/// One call `name` whose request is `Req` with `fields`.
fn one(name: &str, fields: Vec<FieldDescriptorProto>) -> String {
    refused(&set(
        "p",
        vec![method(name, ".p.Req")],
        vec![message("Req", fields)],
    ))
}

/// Asserts the refusals of what cannot be read: bytes that are not a descriptor set, a set
/// with no service or only a service with no call, a type that is not in the set (a request, a response, an enum), a map
/// entry without its two fields, and a call whose request streams.
#[test]
fn what_cannot_be_read_is_refused_by_name() {
    assert!(refused(&[0xff]).starts_with("the descriptor set does not decode: "));
    let empty = FileDescriptorSet {
        file: vec![FileDescriptorProto::default()],
    };
    assert_eq!(
        refused(&empty.encode_to_vec()),
        "the descriptor set must hold one service with calls, not 0"
    );
    assert_eq!(
        refused(&set("p", Vec::new(), Vec::new())),
        "the descriptor set must hold one service with calls, not 0"
    );
    assert_eq!(
        refused(&set(
            "p",
            vec![method("GetThing", ".p.Missing")],
            Vec::new()
        )),
        "the descriptor set has no type .p.Missing"
    );
    let mut lost = method("GetThing", ".p.Empty");
    lost.output_type = Some(".p.Gone".to_owned());
    assert_eq!(
        refused(&set("p", vec![lost], Vec::new())),
        "the descriptor set has no type .p.Gone"
    );
    assert_eq!(
        one("ListThings", vec![field("shade", 1, Type::Enum, ".p.Nope")]),
        "the descriptor set has no type .p.Nope"
    );
    let mut request = message(
        "Req",
        vec![field("pairs", 1, Type::Message, ".p.Req.Entry")],
    );
    let mut entry = message("Entry", vec![field("key", 1, Type::String, "")]);
    entry.options = Some(MessageOptions {
        map_entry: Some(true),
        ..MessageOptions::default()
    });
    request.nested_type = vec![entry];
    assert_eq!(
        refused(&set(
            "p",
            vec![method("ListThings", ".p.Req")],
            vec![request]
        )),
        "the descriptor set has no type Entry: a map entry without a key and a value"
    );
    let mut streaming = method("PutThings", ".p.Empty");
    streaming.client_streaming = Some(true);
    assert_eq!(
        refused(&set("p", vec![streaming], Vec::new())),
        "PutThings streams its requests; a generated command sends one request"
    );
}

/// Asserts the refusals of commands that cannot be: a name without a noun, a command on the
/// words `js` keeps for itself (`join-token`, `lease`), two calls on one command, a field whose flag is one of `js`'s
/// own, and two fields with one flag.
#[test]
fn a_command_that_cannot_be_is_refused_by_name() {
    assert_eq!(
        one("Ping", Vec::new()),
        "Ping: its name has no noun after the verb"
    );
    assert_eq!(
        one("ListLeases", Vec::new()),
        "ListLeases: `js lease list` starts with a word js keeps for itself"
    );
    assert_eq!(
        one("ListJoinTokens", Vec::new()),
        "ListJoinTokens: `js join-token list` starts with a word js keeps for itself"
    );
    let two = set(
        "p",
        vec![
            method("ListJob", ".p.Empty"),
            method("ListJobs", ".p.Empty"),
        ],
        Vec::new(),
    );
    assert_eq!(refused(&two), "ListJobs: `js job list` is where ListJob is");
    assert_eq!(
        one(
            "ListThings",
            vec![
                field("server", 1, Type::String, ""),
                field("json", 2, Type::Bool, "")
            ]
        ),
        "ListThings: the flag --server is a flag of js itself"
    );
    let mut ports = field("ports", 1, Type::Message, ".p.Empty");
    ports.label = Some(Label::Repeated.into());
    assert_eq!(
        one(
            "ListThings",
            vec![ports, field("ports_file", 2, Type::String, "")]
        ),
        "ListThings: the flag --ports-file is the flag of two fields"
    );
}

/// Asserts that the retry-key entry fails loudly when the job service loses what it names:
/// the call `SubmitJob`, the field `spec`, the message on the way to the id (`spec` is no
/// message, or a repeated one), or a `spec.job_id` that is not a string. It catches the entry dropped without a word, which
/// would leave `js job submit` without its drawn id.
#[test]
fn the_retry_key_entry_fails_when_the_definition_loses_it() {
    let package = "jobscheduler.v1";
    let service = |methods, messages| {
        let mut file = FileDescriptorSet::decode(set(package, methods, messages).as_slice())
            .unwrap_or_default();
        file.file.iter_mut().for_each(|file| {
            file.service
                .iter_mut()
                .for_each(|s| s.name = Some("JobService".to_owned()))
        });
        refused(&file.encode_to_vec())
    };
    let empty = ".jobscheduler.v1.Empty";
    let mut get = method("GetJob", empty);
    get.output_type = Some(empty.to_owned());
    assert_eq!(
        service(vec![get], Vec::new()),
        "jobscheduler.v1.JobService.SubmitJob is not in the definition, which the generator's retry-key entry names"
    );
    let mut submit = method("SubmitJob", ".jobscheduler.v1.Req");
    submit.output_type = Some(empty.to_owned());
    let flat = message("Req", vec![field("spec", 1, Type::String, "")]);
    let lost = "jobscheduler.v1.JobService.SubmitJob has no singular string field spec.job_id, which the generator's retry-key entry names";
    assert_eq!(service(vec![submit.clone()], vec![flat]), lost);
    assert_eq!(
        service(vec![submit.clone()], vec![message("Req", Vec::new())]),
        lost
    );
    let mut repeated = field("spec", 1, Type::Message, ".jobscheduler.v1.Spec");
    repeated.label = Some(Label::Repeated.into());
    let text_id = message("Spec", vec![field("job_id", 1, Type::String, "")]);
    assert_eq!(
        service(
            vec![submit.clone()],
            vec![message("Req", vec![repeated]), text_id]
        ),
        lost
    );
    let nested = message(
        "Req",
        vec![field("spec", 1, Type::Message, ".jobscheduler.v1.Spec")],
    );
    let spec = message("Spec", vec![field("job_id", 1, Type::Uint64, "")]);
    assert_eq!(service(vec![submit], vec![nested, spec]), lost);
}

/// Asserts that each call's `idempotency_level` option is read into the table: none set,
/// `NO_SIDE_EFFECTS` and `IDEMPOTENT`, in the order of the calls.
#[test]
fn a_calls_idempotency_level_is_read_from_its_options() {
    let with = |name: &str, level: IdempotencyLevel| {
        let mut call = method(name, ".p.Empty");
        call.options = Some(MethodOptions {
            idempotency_level: Some(level.into()),
            ..MethodOptions::default()
        });
        call
    };
    let calls = vec![
        method("ListWidgets", ".p.Empty"),
        with("GetGadget", IdempotencyLevel::NoSideEffects),
        with("PutThing", IdempotencyLevel::Idempotent),
    ];
    let cli = generate(&set("p", calls, Vec::new()))
        .map(|generated| generated.cli)
        .unwrap_or_default();
    let levels: Vec<&str> = cli
        .lines()
        .filter_map(|line| line.trim().strip_prefix("idempotency: "))
        .collect();
    assert_eq!(
        levels,
        [
            "Idempotency::Unknown,",
            "Idempotency::NoSideEffects,",
            "Idempotency::Idempotent,"
        ]
    );
}

/// Asserts that a call whose request is a message of another package is refused by name:
/// the client names the messages of the service's own package only.
#[test]
fn a_message_of_another_package_is_refused() {
    let one = set("p", vec![method("GetThing", ".q.Thing")], Vec::new());
    let mut file = FileDescriptorSet::decode(one.as_slice()).unwrap_or_default();
    file.file.push(FileDescriptorProto {
        name: Some("q.proto".to_owned()),
        package: Some("q".to_owned()),
        message_type: vec![message("Thing", Vec::new())],
        ..FileDescriptorProto::default()
    });
    assert_eq!(
        refused(&file.encode_to_vec()),
        "GetThing: q.Thing is not in the service's package"
    );
}

/// The directory of this crate, which holds no generated file.
fn here() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

/// Asserts the verb's own refusals: a descriptor file that cannot be read or does not
/// decode, a root the files cannot be written under (it is a file), and `--check` against a
/// root without them, which names all three.
#[test]
fn the_verb_names_what_it_cannot_read_write_or_match() {
    let missing = here().join("no-such-descriptor.bin");
    let args = |descriptor: Option<PathBuf>, out: &Path, check: bool| Args {
        descriptor,
        out: out.to_path_buf(),
        check,
    };
    let said = run(&args(Some(missing.clone()), &here(), false))
        .err()
        .map(|e| e.to_string());
    let want = format!("cannot read {}: ", missing.display());
    assert!(said.unwrap_or_default().starts_with(&want));
    // A Rust source file opens with `//`, a field tag of wire type 7, which no message has.
    let undecodable = here().join("src/job_surface/tests.rs");
    let said = run(&args(Some(undecodable), &here(), false))
        .err()
        .map(|e| e.to_string());
    assert!(
        said.unwrap_or_default()
            .starts_with("the descriptor set does not decode: ")
    );
    let file = here().join("Cargo.toml");
    let said = run(&args(None, &file, false)).err().map(|e| e.to_string());
    let want = format!("cannot write {}: ", file.join(super::CLIENT_FILE).display());
    assert!(said.unwrap_or_default().starts_with(&want));
    let said = run(&args(None, &here(), true)).err().map(|e| e.to_string());
    assert_eq!(
        said.unwrap_or_default(),
        "not what `cargo xtask gen-job-surface --out .` renders from the job service: crates/js-client/src/jobs/generated.rs, crates/js-cli/src/jobs/generated.rs, docs/http-gateway.md"
    );
}

/// A page with both blocks' markers around `routes` and `served` (each block's old text).
fn marked(routes: &str, served: &str) -> String {
    format!(
        "# Page\n\n<!-- gen-job-surface routes begin -->\n{routes}<!-- gen-job-surface routes end -->\n\nText.\n\n<!-- gen-job-surface served begin -->\n{served}<!-- gen-job-surface served end -->\n\nEnd.\n"
    )
}

/// Asserts that the route table is a header and one row per route of the gateway, in its
/// order, with the verb, the template, the method and the success status (201 for a
/// submit), and that a page's blocks are replaced by the table and the paragraph while the
/// text around them is kept byte for byte.
#[test]
fn the_blocks_are_rendered_between_their_markers() {
    let routes = &js_api_gateway::ROUTES[..2];
    assert_eq!(
        route_table(routes),
        "| route | method of `JobService` | success |\n|---|---|---|\n| `POST /v1/jobs` | `SubmitJob` | 201 |\n| `GET /v1/jobs/{job_id}` | `GetJob` | 200 |\n"
    );
    let served = [js_api_gateway::Rpc::GetJob];
    let page = page_with(&marked("stale\n", "old\nlines\n"), routes, &served);
    assert_eq!(
        page,
        Ok(marked(
            &route_table(routes),
            "The server serves `GetJob` (`js_lease_server::SERVED_CALLS`). Every other call answers `UNIMPLEMENTED` (over the gateway, 501 `unimplemented`) until the server serves it: `SubmitJob`.\n"
        ))
    );
    let again = page_with(&page.clone().unwrap_or_default(), routes, &served);
    assert_eq!(again, page, "rendering a current page changes nothing");
}

/// Asserts that the served paragraph names exactly the calls of the served list, in the
/// order of the routes, and every other call as unimplemented: the server's list, which holds
/// every call, says every call is served; a copy without `GetUsage` names the other eleven and
/// `GetUsage` as unimplemented; a copy without `ListHosts` and `GetUsage` names both, in the
/// order of the routes; and an empty list says no call is served.
#[test]
fn the_served_paragraph_follows_the_served_list() {
    use js_api_gateway::Rpc;
    let routes = js_api_gateway::ROUTES;
    assert_eq!(
        served_paragraph(routes, &js_lease_server::SERVED_CALLS),
        "The server serves every call of the service (`js_lease_server::SERVED_CALLS`).\n"
    );
    let without = |left_out: &[Rpc]| -> Vec<Rpc> {
        js_lease_server::SERVED_CALLS
            .into_iter()
            .filter(|rpc| !left_out.contains(rpc))
            .collect()
    };
    assert_eq!(
        served_paragraph(routes, &without(&[Rpc::GetUsage])),
        "The server serves `SubmitJob`, `GetJob`, `ListJobs`, `CancelJob`, `GetResult`, `GetLogs`, `ListHosts`, `CordonHost`, `UncordonHost`, `ListPools` and `ListProviders` (`js_lease_server::SERVED_CALLS`). Every other call answers `UNIMPLEMENTED` (over the gateway, 501 `unimplemented`) until the server serves it: `GetUsage`.\n"
    );
    assert_eq!(
        served_paragraph(routes, &without(&[Rpc::GetUsage, Rpc::ListHosts])),
        "The server serves `SubmitJob`, `GetJob`, `ListJobs`, `CancelJob`, `GetResult`, `GetLogs`, `CordonHost`, `UncordonHost`, `ListPools` and `ListProviders` (`js_lease_server::SERVED_CALLS`). Every other call answers `UNIMPLEMENTED` (over the gateway, 501 `unimplemented`) until the server serves it: `ListHosts` and `GetUsage`.\n"
    );
    assert_eq!(
        served_paragraph(routes, &[]),
        "The server serves no call yet (`js_lease_server::SERVED_CALLS`): every call answers `UNIMPLEMENTED` (over the gateway, 501 `unimplemented`).\n"
    );
}

/// Asserts each refusal of the markers by its exact text: a begin or an end line missing, a
/// marker there twice, and an end before its begin; none of them returns a page.
#[test]
fn wrong_markers_are_refused_by_name() {
    let begin = "<!-- gen-job-surface routes begin -->\n";
    let end = "<!-- gen-job-surface routes end -->\n";
    let cases = [
        (
            format!("a\n{end}"),
            "has no line <!-- gen-job-surface routes begin -->",
        ),
        (
            format!("{begin}a\n"),
            "has no line <!-- gen-job-surface routes end -->",
        ),
        (
            format!("{begin}{begin}{end}"),
            "has the line <!-- gen-job-surface routes begin --> more than once",
        ),
        (
            format!("{end}a\n{begin}"),
            "the line <!-- gen-job-surface routes end --> comes before <!-- gen-job-surface routes begin -->",
        ),
    ];
    for (text, why) in cases {
        assert_eq!(
            splice(&text, "routes", "x\n"),
            Err(why.to_owned()),
            "{text}"
        );
    }
    assert_eq!(
        splice(&format!("{begin}old\n{end}tail"), "routes", "new\n"),
        Ok(format!("{begin}new\n{end}tail"))
    );
}
