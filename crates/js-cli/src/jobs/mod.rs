//! The job commands of `js`: `job`, `host`, `pool`, `provider` and `usage`, one command per
//! call of the job service. The table of commands, [`surface`], and the dispatcher that
//! sends a request through the client are generated from the service definition by
//! `cargo xtask gen-job-surface` (`generated.rs`); this module interprets the table and
//! names no call, so a call added to the definition becomes a command with no code here.
//!
//! A command's flags are built into its request one flag at a time: each flag's value is
//! put at its field's path in a JSON object, read as the request message (proto3 JSON, by
//! the descriptor set of `js-proto`) and encoded, and the encodings are joined, which in
//! protobuf is the request with every field set. A value the message refuses is a usage
//! error naming its flag.

mod text;

pub(crate) use text::lines as text_lines;

mod generated;

use std::io::Write;
use std::path::PathBuf;

use clap::builder::{PossibleValuesParser, TypedValueParser, ValueParser};
use clap::{Arg, ArgAction, ArgMatches};
use js_client::REQUEST_LIMIT;
use js_client::jobs::{Dialer, JobClient};
use prost::Message as _;
use prost_reflect::{DescriptorPool, DynamicMessage, MessageDescriptor};
use serde_json::{Map, Value as Json};

use crate::cli::parse_duration;
use crate::failure::{Failure, exit};
use crate::render::Render;

/// The generated table: one command per call of the job service, in the order the
/// definition names the calls.
#[must_use]
pub fn surface() -> &'static [Command] {
    generated::SURFACE
}

/// One command of the table: a call of the job service, the words that name it and the flags
/// its request is built from.
#[derive(Debug)]
pub struct Command {
    /// The words after `js`: `["job", "submit"]`, or `["usage"]` alone.
    pub path: &'static [&'static str],
    /// The call's name in the definition, such as `SubmitJob`.
    pub rpc: &'static str,
    /// The call's comment, as one paragraph: its first sentence is the command's summary.
    pub doc: &'static str,
    /// What the definition says about repeating the call.
    pub idempotency: Idempotency,
    /// The full name of the request message.
    pub request: &'static str,
    /// The full name of the response message.
    pub response: &'static str,
    /// The server streams its answers; each is printed as it comes.
    pub streaming: bool,
    /// The path of the field the call is idempotent on, when it has one. When that field's
    /// flag is not given, `js` fills it with 128 random bits in hexadecimal and prints the
    /// value before it sends the request, so a run whose answer was lost can be repeated
    /// with the same id (even where the service would mint an id for an empty one).
    pub retry_key: Option<&'static [&'static str]>,
    /// One flag per leaf of the request.
    pub flags: &'static [Flag],
}

/// One flag of a command: a field of the request.
#[derive(Debug)]
pub struct Flag {
    /// The flag without its dashes: `spec.needs.cpu-millis`, `spec.ports-file`, `id`.
    pub name: &'static str,
    /// The path of the field from the request, by field name: `["spec", "needs",
    /// "cpu_millis"]`.
    pub field: &'static [&'static str],
    /// What the flag takes.
    pub kind: Kind,
    /// The flag may be given more than once; each value is one more element (or entry).
    pub repeated: bool,
    /// The flag must be given.
    pub required: bool,
    /// The field's comment, as one paragraph.
    pub doc: &'static str,
}

/// What a flag takes.
#[derive(Debug)]
pub enum Kind {
    /// `true` or `false`.
    Bool,
    /// A 32-bit integer.
    I32,
    /// A 64-bit integer.
    I64,
    /// A whole number below 2^32.
    U32,
    /// A whole number below 2^64.
    U64,
    /// A 32-bit floating point number.
    F32,
    /// A 64-bit floating point number.
    F64,
    /// Text.
    Text,
    /// A whole number of seconds, written `90`, `90s`, `15m` or `2h`.
    Seconds,
    /// One of an enum's values by its short name, each with the value's own name.
    Enum(&'static [(&'static str, &'static str)]),
    /// `KEY=VALUE` of a map: what the key and the value take.
    Pair(&'static Kind, &'static Kind),
    /// A path to a file holding the field's value in proto3 JSON.
    File,
}

/// What the definition says about repeating a call (its `idempotency_level`).
#[derive(Debug, Clone, Copy)]
pub enum Idempotency {
    /// Not stated.
    Unknown,
    /// The call changes nothing.
    NoSideEffects,
    /// Repeating the call is the same as making it once.
    Idempotent,
}

/// Where the dispatcher hands each answer, encoded.
pub(crate) type Sink<'a> = &'a mut dyn FnMut(&[u8]) -> Result<(), Failure>;

/// The command groups of `surface` for the `js` command line: one per first word, in the
/// order the table first names it. A one-word command (`usage`) is a group of its own with
/// the call's flags.
#[must_use]
pub fn commands(surface: &'static [Command]) -> Vec<clap::Command> {
    let mut groups: Vec<&'static str> = Vec::new();
    for command in surface {
        let first = command.path.first().copied().unwrap_or_default();
        match groups.contains(&first) {
            true => {}
            false => groups.push(first),
        }
    }
    groups
        .into_iter()
        .map(|group| group_command(group, surface))
        .collect()
}

/// The group `group`: its subcommands, or the one-word command itself.
fn group_command(group: &'static str, surface: &'static [Command]) -> clap::Command {
    let members = surface
        .iter()
        .filter(|command| command.path.first() == Some(&group));
    let about = format!("The {group} calls of the job service");
    let parent = clap::Command::new(group).about(about);
    members.fold(parent, |parent, command| match command.path.get(1) {
        Some(word) => parent
            .subcommand_required(true)
            .arg_required_else_help(true)
            .subcommand(with_flags(clap::Command::new(*word), command)),
        None => with_flags(parent, command),
    })
}

/// `target` with the summary, the long help and the flags of `command`.
fn with_flags(target: clap::Command, command: &'static Command) -> clap::Command {
    let summary = command.doc.split_inclusive(". ").next().unwrap_or_default();
    target
        .about(summary.trim_end().to_owned())
        .long_about(command.doc)
        .args(command.flags.iter().map(arg))
}

/// Where the call's own flags stand in a command's help: after the four global flags (whose
/// places are their declaration order), each in the order the table gives.
const FLAG_ORDER: usize = 100;

/// The argument of `flag`.
fn arg(flag: &'static Flag) -> Arg {
    let action = match flag.repeated {
        true => ArgAction::Append,
        false => ArgAction::Set,
    };
    Arg::new(flag.name)
        .long(flag.name)
        .help(help(flag))
        .value_name(value_name(&flag.kind))
        .required(flag.required)
        // After the global flags, in the order of the request's fields.
        .display_order(FLAG_ORDER)
        // A text value may start with a dash (`--spec.args --fast`), a number may be
        // negative (`--spec.priority -3`).
        .allow_hyphen_values(matches!(flag.kind, Kind::Text))
        .allow_negative_numbers(matches!(
            flag.kind,
            Kind::I32 | Kind::I64 | Kind::F32 | Kind::F64
        ))
        .action(action)
        .value_parser(parser(&flag.kind))
}

/// The help of `flag`: the field's comment and how the flag is written.
fn help(flag: &Flag) -> String {
    let how = match flag.kind {
        Kind::Seconds => " A duration: 90, 90s, 15m or 2h.",
        Kind::Pair(..) => " KEY=VALUE; repeat for more.",
        Kind::File => " Read from a file holding the field's value in proto3 JSON.",
        _ => "",
    };
    let repeat = match (flag.repeated, &flag.kind) {
        (true, Kind::Pair(..)) | (false, _) => "",
        (true, _) => " Repeat for more.",
    };
    format!("{}{how}{repeat}", flag.doc).trim_start().to_owned()
}

/// The placeholder of a flag's value in the help.
fn value_name(kind: &Kind) -> &'static str {
    match kind {
        Kind::Bool => "BOOL",
        Kind::I32 | Kind::I64 | Kind::U32 | Kind::U64 => "N",
        Kind::F32 | Kind::F64 => "X",
        Kind::Text => "TEXT",
        Kind::Seconds => "DURATION",
        Kind::Enum(_) => "NAME",
        Kind::Pair(..) => "KEY=VALUE",
        Kind::File => "PATH",
    }
}

/// The parser of a flag's value: the value as proto3 JSON, or the usage error that names
/// what the flag takes.
fn parser(kind: &'static Kind) -> ValueParser {
    match kind {
        Kind::Enum(choices) => {
            let shorts = choices.iter().map(|(short, _)| *short);
            let named = PossibleValuesParser::new(shorts).map(|short| enum_value(choices, &short));
            ValueParser::new(named)
        }
        _ => ValueParser::new(move |text: &str| value(kind, text)),
    }
}

/// The JSON of the enum value whose short name is `short`.
fn enum_value(choices: &[(&str, &str)], short: &str) -> Json {
    let found = choices.iter().find(|(name, _)| *name == short);
    Json::from(found.map_or(short, |(_, value)| *value))
}

/// `text` as a value of `kind` in proto3 JSON, or what is wrong with it.
fn value(kind: &Kind, text: &str) -> Result<Json, String> {
    let bad = |want: &str| format!("{text:?} is not {want}");
    match kind {
        Kind::Bool => text
            .parse::<bool>()
            .map(Json::Bool)
            .map_err(|_| bad("true or false")),
        Kind::I32 => text
            .parse::<i32>()
            .map(Json::from)
            .map_err(|_| bad("a 32-bit integer")),
        Kind::I64 => checked::<i64>(text).ok_or_else(|| bad("a 64-bit integer")),
        Kind::U32 => text
            .parse::<u32>()
            .map(Json::from)
            .map_err(|_| bad("a whole number below 2^32")),
        Kind::U64 => checked::<u64>(text).ok_or_else(|| bad("a whole number below 2^64")),
        Kind::F32 => checked::<f32>(text).ok_or_else(|| bad("a number")),
        Kind::F64 => checked::<f64>(text).ok_or_else(|| bad("a number")),
        Kind::Text | Kind::File => Ok(Json::from(text)),
        Kind::Seconds => parse_duration(text).map(|duration| Json::from(duration.as_secs())),
        Kind::Enum(choices) => {
            let found = choices.iter().find(|(short, _)| *short == text);
            let names: Vec<&str> = choices.iter().map(|(short, _)| *short).collect();
            let want = format!("one of {}", names.join(", "));
            found
                .map(|(_, name)| Json::from(*name))
                .ok_or_else(|| bad(&want))
        }
        Kind::Pair(key, item) => {
            let (left, right) = text.split_once('=').ok_or_else(|| bad("KEY=VALUE"))?;
            let key = value(key, left)
                .map(|key| key.as_str().map_or_else(|| key.to_string(), str::to_owned))?;
            Ok(Json::Object(Map::from_iter([(key, value(item, right)?)])))
        }
    }
}

/// `text` as JSON text when it parses as a `T` (64-bit integers and floats are strings in
/// proto3 JSON).
fn checked<T: std::str::FromStr>(text: &str) -> Option<Json> {
    text.parse::<T>().ok().map(|_| Json::from(text))
}

/// The command of `matches` (the parsed command line) when it is a job command, with the
/// matches of its own words.
pub(crate) fn chosen(matches: &ArgMatches) -> Option<(&'static Command, &ArgMatches)> {
    let (words, own) = words(matches);
    let found = surface()
        .iter()
        .find(|command| command.path == words.as_slice());
    found.map(|command| (command, own))
}

/// The subcommand words of `matches` and the matches of the last.
fn words(matches: &ArgMatches) -> (Vec<&str>, &ArgMatches) {
    match matches.subcommand() {
        Some((name, sub)) => {
            let (mut words, own) = words(sub);
            words.insert(0, name);
            (words, own)
        }
        None => (Vec::new(), matches),
    }
}

/// Runs a job command: builds the request from the flags (drawing the retry key when it is
/// not given, and printing it on `err` before anything is sent), sends it, and prints each
/// answer on `out`.
pub(crate) fn run(
    command: &'static Command,
    globals: &ArgMatches,
    args: &ArgMatches,
    render: Render,
    out: &mut dyn Write,
    err: &mut dyn Write,
    dialer: &Dialer,
) -> Result<u8, Failure> {
    let server = globals
        .get_one::<String>("server")
        .ok_or_else(|| Failure::flag("--server", "is required"))?;
    let token_file = globals
        .get_one::<PathBuf>("token_file")
        .ok_or_else(|| Failure::flag("--token-file", "is required"))?;
    let ca_cert = globals.get_one::<PathBuf>("ca_cert").map(PathBuf::as_path);
    let pool = pool()?;
    let request = message(&pool, command.request)?;
    let mut encoded = Vec::new();
    for flag in command.flags {
        let given = given(flag, args)?;
        encoded.extend(
            given
                .map(|value| field(&request, flag, value))
                .transpose()?
                .unwrap_or_default(),
        );
    }
    let key = retry_key(command, args)?;
    let drawn = key
        .as_ref()
        .filter(|(flag, _)| args.get_one::<Json>(flag.name).is_none());
    encoded.extend(
        drawn
            .map(|(flag, id)| field(&request, flag, Json::from(id.as_str())))
            .transpose()?
            .unwrap_or_default(),
    );
    drawn.into_iter().for_each(|(_, id)| {
        let _ = err.write_all(
            render
                .drawn(command.path.first().copied().unwrap_or_default(), id)
                .as_bytes(),
        );
    });
    let response = message(&pool, command.response)?;
    let mut sink = |answer: &[u8]| print(&response, answer, render, out);
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|why| Failure::Server(format!("cannot start the client: {why}")))?;
    let sent = runtime.block_on(async {
        let mut client =
            JobClient::connect_via(server, token_file, ca_cert, REQUEST_LIMIT, dialer.clone())
                .await?;
        generated::dispatch(&mut client, command.rpc, &encoded, &mut sink).await
    });
    sent.map_err(|failure| unanswered(failure, command, key.as_ref()))?;
    Ok(exit::OK)
}

/// The values given to `flag`, as proto3 JSON: the field's value, or `None` when the flag
/// is not given. A `-file` flag's file is read here.
fn given(flag: &Flag, args: &ArgMatches) -> Result<Option<Json>, Failure> {
    let values: Vec<Json> = args
        .get_many::<Json>(flag.name)
        .map_or_else(Vec::new, |values| values.cloned().collect());
    match (&flag.kind, flag.repeated, values.as_slice()) {
        (_, _, []) => Ok(None),
        (Kind::File, _, [path, ..]) => read_file(flag, path.as_str().unwrap_or_default()).map(Some),
        (Kind::Pair(..), _, _) => Ok(Some(Json::Object(
            values
                .iter()
                .filter_map(Json::as_object)
                .flatten()
                .map(|(key, item)| (key.clone(), item.clone()))
                .collect(),
        ))),
        (_, true, _) => Ok(Some(Json::Array(values))),
        (_, false, [.., last]) => Ok(Some(last.clone())),
    }
}

/// The JSON in the file a `-file` flag names.
fn read_file(flag: &Flag, path: &str) -> Result<Json, Failure> {
    let refuse = |why: String| Failure::Usage(format!("--{}: {path}: {why}", flag.name));
    let text = std::fs::read_to_string(path).map_err(|why| refuse(why.to_string()))?;
    serde_json::from_str(&text).map_err(|why| refuse(format!("is not JSON: {why}")))
}

/// The encoding of a request that holds only `value` at `flag`'s field.
fn field(request: &MessageDescriptor, flag: &Flag, value: Json) -> Result<Vec<u8>, Failure> {
    let json = flag.field.iter().rev().fold(value, |inner, name| {
        Json::Object(Map::from_iter([((*name).to_owned(), inner)]))
    });
    let message = DynamicMessage::deserialize(request.clone(), json);
    let message = message.map_err(|why| Failure::Usage(format!("--{}: {why}", flag.name)))?;
    Ok(message.encode_to_vec())
}

/// The command's retry key and its value: the one given, or 128 random bits in
/// hexadecimal.
fn retry_key(
    command: &'static Command,
    args: &ArgMatches,
) -> Result<Option<(&'static Flag, String)>, Failure> {
    let flag = command
        .retry_key
        .and_then(|key| command.flags.iter().find(|flag| flag.field == key));
    let given = flag
        .and_then(|flag| args.get_one::<Json>(flag.name))
        .and_then(Json::as_str)
        .map(str::to_owned);
    match (flag, given) {
        (None, _) => Ok(None),
        (Some(flag), Some(id)) => Ok(Some((flag, id))),
        (Some(flag), None) => fresh_id(draw()).map(|id| Some((flag, id))),
    }
}

/// 128 random bits from the system's random source.
fn draw() -> Result<[u8; 16], getrandom::Error> {
    let mut bytes = [0u8; 16];
    getrandom::getrandom(&mut bytes).map(|()| bytes)
}

/// The id of these random bytes, in lower-case hexadecimal, or the failure of the draw.
fn fresh_id(drawn: Result<[u8; 16], getrandom::Error>) -> Result<String, Failure> {
    let id = drawn.map(|bytes| bytes.iter().map(|byte| format!("{byte:02x}")).collect());
    id.map_err(|why| Failure::Server(format!("cannot draw a random id: {why}")))
}

/// What to say when the answer was lost after the request was sent: with a retry key, that
/// the thing may exist and the command is run again with that key; without one, whether the
/// definition says the call is safe to repeat. Any other failure is unchanged.
fn unanswered(failure: Failure, command: &Command, key: Option<&(&Flag, String)>) -> Failure {
    let group = command.path.first().copied().unwrap_or_default();
    match (failure, key, command.idempotency) {
        (Failure::Unanswered(_), Some((flag, id)), _) => Failure::Unanswered(format!(
            "the {group} may exist; run the same command again with --{} {id}",
            flag.name
        )),
        (Failure::Unanswered(why), None, Idempotency::Unknown) => {
            Failure::Unanswered(format!("{why}; the call may have taken effect"))
        }
        (Failure::Unanswered(why), None, Idempotency::NoSideEffects | Idempotency::Idempotent) => {
            Failure::Unanswered(format!("{why}; the call is safe to repeat"))
        }
        (other, _, _) => other,
    }
}

/// Prints one encoded answer of the message `response`.
fn print(
    response: &MessageDescriptor,
    answer: &[u8],
    render: Render,
    out: &mut dyn Write,
) -> Result<(), Failure> {
    let message = DynamicMessage::decode(response.clone(), answer)
        .map_err(|why| mismatch(response.full_name(), &why))?;
    let _ = out.write_all(render.answer(&message).as_bytes());
    Ok(())
}

/// The descriptor set of the job service.
fn pool() -> Result<DescriptorPool, Failure> {
    DescriptorPool::decode(js_proto::FILE_DESCRIPTOR_SET)
        .map_err(|why| mismatch("the descriptor set", &why))
}

/// The message `name` of the descriptor set.
fn message(pool: &DescriptorPool, name: &str) -> Result<MessageDescriptor, Failure> {
    pool.get_message_by_name(name)
        .ok_or_else(|| mismatch(name, &"no such message"))
}

/// The failure of a table that does not match the descriptor set it should have been
/// generated from.
fn mismatch(what: &str, why: &dyn std::fmt::Display) -> Failure {
    Failure::Surface(format!("{what}: {why}"))
}

/// The failure of a call the dispatcher does not know (the table and the dispatcher are
/// generated together, so this is a table that is not current).
pub(crate) fn unknown_call(rpc: &str) -> Failure {
    mismatch(rpc, &"no such call")
}

/// The message `T` of an encoded request the flags built.
pub(crate) fn decode<T: prost::Message + Default>(request: &[u8]) -> Result<T, Failure> {
    T::decode(request).map_err(|why| mismatch(std::any::type_name::<T>(), &why))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Asserts every kind's value in proto3 JSON and its refusal, including the kinds the
    /// job service's table has no flag of today (64-bit signed, floats, an enum as a map
    /// value, a map with a number for its key). It catches a kind spelt in a way the
    /// message would not read.
    #[test]
    fn every_kind_reads_its_value_or_says_what_it_takes() {
        const SHADES: &[(&str, &str)] = &[("dark-grey", "SHADE_DARK_GREY")];
        let read = |kind: &'static Kind, text: &str| match value(kind, text) {
            Ok(json) => json.to_string(),
            Err(why) => why,
        };
        let cases: [(&'static Kind, &str, &str); 15] = [
            (&Kind::Bool, "true", "true"),
            (&Kind::Bool, "yes", r#""yes" is not true or false"#),
            (&Kind::I32, "-4", "-4"),
            (&Kind::I64, "-9000000000", r#""-9000000000""#),
            (&Kind::I64, "x", r#""x" is not a 64-bit integer"#),
            (&Kind::U32, "7", "7"),
            (&Kind::U64, "7", r#""7""#),
            (&Kind::F32, "1.5", r#""1.5""#),
            (&Kind::F64, "z", r#""z" is not a number"#),
            (&Kind::Text, "a b", r#""a b""#),
            (&Kind::Seconds, "2m", "120"),
            (&Kind::Enum(SHADES), "dark-grey", r#""SHADE_DARK_GREY""#),
            (
                &Kind::Enum(SHADES),
                "pink",
                r#""pink" is not one of dark-grey"#,
            ),
            (
                &Kind::Pair(&Kind::U32, &Kind::Enum(SHADES)),
                "3=dark-grey",
                r#"{"3":"SHADE_DARK_GREY"}"#,
            ),
            (
                &Kind::Pair(&Kind::Text, &Kind::Text),
                "no-equals",
                r#""no-equals" is not KEY=VALUE"#,
            ),
        ];
        for (kind, text, want) in cases {
            assert_eq!(read(kind, text), want, "{kind:?} {text}");
        }
        let names: Vec<&str> = [
            Kind::Bool,
            Kind::I64,
            Kind::F32,
            Kind::F64,
            Kind::Text,
            Kind::Seconds,
            Kind::File,
        ]
        .iter()
        .map(value_name)
        .collect();
        assert_eq!(names, ["BOOL", "N", "X", "X", "TEXT", "DURATION", "PATH"]);
    }

    /// A command of `idempotency` with no flags, for the failures below.
    fn bare(idempotency: Idempotency) -> Command {
        Command {
            path: &["thing", "watch"],
            rpc: "WatchThings",
            doc: "",
            idempotency,
            request: "jobscheduler.v1.ListHostsRequest",
            response: "jobscheduler.v1.ListHostsResponse",
            streaming: false,
            retry_key: None,
            flags: &[],
        }
    }

    /// Asserts what a lost answer says by what the definition states of the call: may have
    /// taken effect when it states nothing, safe to repeat when it states the call changes
    /// nothing or is idempotent; and that any other failure passes unchanged.
    #[test]
    fn a_lost_answer_says_whether_the_call_is_safe_to_repeat() {
        let lost = || Failure::Unanswered("lost".to_owned());
        let said =
            |command: &Command, failure: Failure| unanswered(failure, command, None).to_string();
        assert_eq!(
            said(&bare(Idempotency::Unknown), lost()),
            "lost; the call may have taken effect"
        );
        assert_eq!(
            said(&bare(Idempotency::NoSideEffects), lost()),
            "lost; the call is safe to repeat"
        );
        assert_eq!(
            said(&bare(Idempotency::Idempotent), lost()),
            "lost; the call is safe to repeat"
        );
        assert_eq!(
            said(
                &bare(Idempotency::Unknown),
                Failure::NotSent("down".to_owned())
            ),
            "down"
        );
    }

    /// Asserts the failures of a table that does not match the job service: a call the
    /// dispatcher does not know, a message the descriptor set lacks, a request that does not
    /// decode, and a random source that fails; each says what, and the first three as
    /// `internal` (exit 1).
    #[test]
    fn a_stale_table_and_a_failed_draw_are_named() {
        let stale = "the job command table does not match the job service";
        let surface = unknown_call("WatchThings");
        assert_eq!(
            surface.to_string(),
            format!(
                "{stale} (WatchThings: no such call); regenerate it with cargo xtask gen-job-surface"
            )
        );
        assert_eq!(surface.parts(), (exit::SERVER, "internal"));
        let pool = pool().expect("the job service decodes");
        let missing = message(&pool, "jobscheduler.v1.Nothing")
            .err()
            .map(|f| f.to_string());
        assert_eq!(
            missing.unwrap_or_default(),
            format!(
                "{stale} (jobscheduler.v1.Nothing: no such message); regenerate it with cargo xtask gen-job-surface"
            )
        );
        let undecoded = decode::<js_proto::v1::GetJobRequest>(&[0xff])
            .err()
            .map(|f| f.to_string());
        assert!(
            undecoded
                .unwrap_or_default()
                .starts_with(&format!("{stale} (js_proto::v1::GetJobRequest: "))
        );
        let response =
            message(&pool, "jobscheduler.v1.GetJobResponse").expect("a message of the service");
        let mut out = Vec::new();
        let printed = print(&response, &[0xff], Render::new(false), &mut out)
            .err()
            .map(|f| f.to_string());
        assert!(
            printed
                .unwrap_or_default()
                .starts_with(&format!("{stale} (jobscheduler.v1.GetJobResponse: "))
        );
        let failed = fresh_id(Err(getrandom::Error::UNSUPPORTED))
            .err()
            .map(|f| f.to_string());
        assert!(
            failed
                .unwrap_or_default()
                .starts_with("cannot draw a random id: ")
        );
        assert_eq!(
            fresh_id(Ok([0xab; 16])).unwrap_or_default(),
            "ab".repeat(16)
        );
    }
}
