//! The command table of `js`: how a call becomes a command and a request field a flag, and
//! the text of [`super::CLI_FILE`].
//!
//! **Commands.** A call's name is a verb and a noun (`SubmitJob`, `ListJobs`, `GetUsage`).
//! A request's *subject* is its one singular, non-`optional` scalar field (`string`, `bytes`,
//! `bool`, an integer or a float, not in a `oneof`; enums, messages, repeated fields, maps
//! and `optional` fields do not count), when it has exactly one such field; its flag is
//! required. The rule has one limit: a request whose one such field is really optional
//! gets a required flag, and the fix is to make the field `optional` in the definition, an
//! additive change. When the subject is
//! named `<owner>_id`, the command is `<owner> <verb>` if the noun is the owner and
//! `<owner> <noun>` otherwise (`GetJob` is `job get`, `GetResult` is `job result`,
//! `CordonHost` is `host cordon`). Without such a subject the command is
//! `<noun> <verb>`, the noun without a trailing `s` (`SubmitJob` is `job submit`,
//! `ListHosts` is `host list`), and a `Get` is the noun alone (`GetUsage` is `usage`).
//!
//! **Flags.** One flag per leaf of the request, named by the field names in kebab case joined
//! by dots: a nested message (singular or `optional`) is walked, so `--spec.needs.cpu-millis`.
//! A scalar or enum takes one value, or repeats when the field is repeated (an enum by the
//! short name of any of its values, the one numbered 0 too); a map takes
//! `KEY=VALUE` and repeats; an integer field named `*_seconds` or `*_secs` takes a duration.
//! A field the flags cannot spell (`bytes`, a repeated message, a map of messages, a `oneof`
//! member (of one `oneof`, one flag at most may be given), a recursive message, a well-known
//! type of `google.protobuf`, a message without fields) is `--<name>-file <path>`, a file holding the field's value in
//! proto3 JSON, so no call is out of reach. The field a call is idempotent on
//! ([`super::surface::RETRY_KEYS`]) gets the entry's flag in place of its dotted name
//! (`--id` for `spec.job_id` of `SubmitJob`), is not required, and is the command's
//! `retry_key`, which the CLI fills with a random id when the flag is not given.

use super::GenError;
use super::surface::{
    Enumeration, Field, Idempotency, Kind, Label, Message, RetryKey, Rpc, Scalar, Surface,
};
use super::words::{camel_words, kebab, lower_words, short_names, singular, snake};

/// The flags `js` itself has, and clap's own: no request field may take one.
pub const RESERVED: [&str; 6] = ["server", "token-file", "ca-cert", "json", "help", "version"];

/// The first words of the commands `js` writes by hand, and clap's own.
pub const TAKEN: [&str; 2] = ["lease", "help"];

/// One command of the table.
#[derive(Debug, Clone, PartialEq)]
pub struct Command {
    /// The words after `js`.
    pub path: Vec<String>,
    /// The call.
    pub rpc: String,
    /// The call's comment as one paragraph.
    pub doc: String,
    /// What the definition says about repeating the call.
    pub idempotency: Idempotency,
    /// The full name of the request message.
    pub request: String,
    /// The full name of the response message.
    pub response: String,
    /// The server streams its answers.
    pub streaming: bool,
    /// The path of the field the call is idempotent on, when it has one.
    pub retry_key: Option<Vec<String>>,
    /// One flag per leaf of the request.
    pub flags: Vec<Flag>,
}

/// One flag of a command.
#[derive(Debug, Clone, PartialEq)]
pub struct Flag {
    /// The flag without its dashes.
    pub name: String,
    /// The path of the field from the request, by field name.
    pub field: Vec<String>,
    /// What the flag takes.
    pub kind: FlagKind,
    /// The flag may be given more than once.
    pub repeated: bool,
    /// The flag must be given (the request's subject).
    pub required: bool,
    /// The `oneof` of the field, named by its path from the request (`spec.how`): of the
    /// flags of one `oneof`, at most one may be given.
    pub oneof: Option<String>,
    /// The field's comment as one paragraph.
    pub doc: String,
}

/// What a flag takes.
#[derive(Debug, Clone, PartialEq)]
pub enum FlagKind {
    /// A scalar other than `bytes`.
    Scalar(Scalar),
    /// A whole number of seconds, written as a duration.
    Seconds,
    /// One of an enum's values by its short name, each with the value's own name.
    Enum(Vec<(String, String)>),
    /// `KEY=VALUE` of a map.
    Pair(Box<FlagKind>, Box<FlagKind>),
    /// A file holding the field's value in proto3 JSON.
    File,
}

/// What a request field becomes.
enum Leaf<'a> {
    /// A message whose fields are flags in turn.
    Nested(&'a Message),
    /// One flag.
    Flag(FlagKind),
    /// A `-file` flag.
    File,
}

/// The commands of `surface`, in the order of its calls.
///
/// # Errors
///
/// [`GenError::Path`] for a command without words of its own, on a taken word, or where
/// another command is; [`GenError::Flag`] for a flag that is reserved or given twice.
pub fn table(surface: &Surface) -> Result<Vec<Command>, GenError> {
    let commands: Vec<Command> = surface.rpcs.iter().map(command).collect();
    commands
        .iter()
        .enumerate()
        .try_for_each(|(at, command)| check(command, commands.get(..at).unwrap_or_default()))?;
    Ok(commands)
}

/// The command of one call.
fn command(rpc: &Rpc) -> Command {
    let subject = subject(&rpc.request);
    let mut flags = Vec::new();
    let walk = Walk {
        subject,
        key: rpc.retry_key.as_ref(),
    };
    walk.message(&rpc.request, &[], &mut flags);
    Command {
        path: path(&rpc.name, subject),
        rpc: rpc.name.clone(),
        doc: paragraph(&rpc.doc),
        idempotency: rpc.idempotency,
        request: rpc.request.name.clone(),
        response: rpc.response.clone(),
        streaming: rpc.server_streaming,
        retry_key: rpc.retry_key.as_ref().map(|key| key.field.clone()),
        flags,
    }
}

/// A comment's lines as one paragraph.
fn paragraph(lines: &[String]) -> String {
    let words: Vec<&str> = lines
        .iter()
        .map(|line| line.trim())
        .filter(|line| !line.is_empty())
        .collect();
    words.join(" ")
}

/// The request's subject: its one singular, non-`optional` scalar field outside a `oneof`,
/// when it has exactly one.
fn subject(request: &Message) -> Option<&Field> {
    let candidates: Vec<&Field> = request
        .fields
        .iter()
        .filter(|field| {
            let singular = matches!(field.label, Label::Singular) & field.oneof.is_none();
            singular & matches!(field.kind, Kind::Scalar(_))
        })
        .collect();
    match candidates.as_slice() {
        [one] => Some(one),
        _ => None,
    }
}

/// The words of the call `name`'s command (the module docs give the rule).
fn path(name: &str, subject: Option<&Field>) -> Vec<String> {
    let words = lower_words(&camel_words(name));
    let verb = words.first().cloned().unwrap_or_default();
    let noun = words.get(1..).unwrap_or_default().join("-");
    let owner = subject
        .and_then(|field| field.name.strip_suffix("_id"))
        .map(kebab);
    match owner {
        Some(owner) => {
            let leaf = match singular(&noun) == owner {
                true => verb,
                false => noun,
            };
            vec![owner, leaf]
        }
        None => {
            let group = singular(&noun).to_owned();
            match verb.as_str() {
                "get" => vec![group],
                _ => vec![group, verb],
            }
        }
    }
}

/// Refuses a command whose words are empty or taken, that lands where one of `before`
/// is (the same words, or the words of a group), or with a flag that is reserved or given
/// twice.
fn check(command: &Command, before: &[Command]) -> Result<(), GenError> {
    let path = command.path.join(" ");
    let empty = command.path.iter().any(String::is_empty);
    let taken = command
        .path
        .first()
        .is_some_and(|first| TAKEN.contains(&first.as_str()));
    let clash = before
        .iter()
        .find(|other| overlaps(&other.path, &command.path));
    let refuse = |why: String| GenError::Path {
        rpc: command.rpc.clone(),
        why,
    };
    match (empty, taken, clash) {
        (true, _, _) => Err(refuse("its name has no noun after the verb".to_owned())),
        (false, true, _) => Err(refuse(format!(
            "`js {path}` is a command js writes by hand"
        ))),
        (false, false, Some(other)) => {
            Err(refuse(format!("`js {path}` is where {} is", other.rpc)))
        }
        (false, false, None) => flags_are_free(command),
    }
}

/// Whether one of two commands' words are the start of the other's.
fn overlaps(a: &[String], b: &[String]) -> bool {
    let shared = a.len().min(b.len());
    a.get(..shared) == b.get(..shared)
}

/// Refuses a flag that `js` has, or that the command has twice.
fn flags_are_free(command: &Command) -> Result<(), GenError> {
    command.flags.iter().enumerate().try_for_each(|(at, flag)| {
        let reserved = RESERVED.contains(&flag.name.as_str());
        let twice = command
            .flags
            .get(..at)
            .unwrap_or_default()
            .iter()
            .any(|other| other.name == flag.name);
        let refuse = |why: &str| GenError::Flag {
            rpc: command.rpc.clone(),
            flag: flag.name.clone(),
            why: why.to_owned(),
        };
        match (reserved, twice) {
            (true, _) => Err(refuse("is a flag of js itself")),
            (false, true) => Err(refuse("is the flag of two fields")),
            (false, false) => Ok(()),
        }
    })
}

/// The walk over a request: its subject and the call's retry key.
struct Walk<'a> {
    subject: Option<&'a Field>,
    key: Option<&'a RetryKey>,
}

impl Walk<'_> {
    /// Adds the flags of `message`, whose path from the request is `at`.
    fn message(&self, message: &Message, at: &[String], flags: &mut Vec<Flag>) {
        for field in &message.fields {
            let mut path = at.to_vec();
            path.push(field.name.clone());
            let name = path
                .iter()
                .map(|part| kebab(part))
                .collect::<Vec<_>>()
                .join(".");
            match leaf(field) {
                Leaf::Nested(inner) => self.message(inner, &path, flags),
                Leaf::Flag(kind) => flags.push(self.flag(field, at, path, name, kind)),
                Leaf::File => {
                    flags.push(self.flag(field, at, path, format!("{name}-file"), FlagKind::File))
                }
            }
        }
    }

    /// The flag of `field`.
    fn flag(
        &self,
        field: &Field,
        at: &[String],
        path: Vec<String>,
        name: String,
        kind: FlagKind,
    ) -> Flag {
        let key = self.key.filter(|key| key.field == path);
        let subject = self
            .subject
            .is_some_and(|subject| subject.name == field.name)
            & at.is_empty();
        let repeated = matches!(field.label, Label::Repeated) & !matches!(kind, FlagKind::File);
        Flag {
            name: key.map_or(name, |key| key.flag.clone()),
            field: path,
            kind,
            repeated,
            required: subject,
            oneof: field.oneof.as_ref().map(|oneof| {
                let mut path = at.to_vec();
                path.push(oneof.clone());
                path.join(".")
            }),
            doc: paragraph(&field.doc),
        }
    }
}

/// What `field` becomes (the module docs give the rule).
fn leaf(field: &Field) -> Leaf<'_> {
    // A well-known type (its proto3 JSON is not an object of its fields) and a message
    // without fields (whose presence is all it says) are read from a file.
    let special = match &field.kind {
        Kind::Message(inner) => well_known_or_empty(inner),
        _ => false,
    };
    match (&field.kind, field.label, field.oneof.is_some() | special) {
        (_, _, true) => Leaf::File,
        (Kind::Message(inner), Label::Singular | Label::Optional, false) => Leaf::Nested(inner),
        (Kind::Map(key, value), _, false) => {
            value_kind(value, &field.name).map_or(Leaf::File, |value| {
                Leaf::Flag(FlagKind::Pair(
                    Box::new(FlagKind::Scalar(*key)),
                    Box::new(value),
                ))
            })
        }
        (kind, _, false) => value_kind(kind, &field.name).map_or(Leaf::File, Leaf::Flag),
    }
}

/// Whether `message` is a well-known type (package `google.protobuf`) or has no fields.
fn well_known_or_empty(message: &Message) -> bool {
    message.name.starts_with("google.protobuf.") | message.fields.is_empty()
}

/// What a flag of one value of `kind` takes, or `None` when a flag cannot spell it.
fn value_kind(kind: &Kind, name: &str) -> Option<FlagKind> {
    match kind {
        Kind::Scalar(Scalar::Bytes) | Kind::Message(_) | Kind::Map(..) | Kind::Cycle => None,
        Kind::Scalar(scalar) => Some(seconds_or(*scalar, name)),
        Kind::Enum(enumeration) => Some(FlagKind::Enum(choices(enumeration))),
    }
}

/// A duration for an integer field named `*_seconds` or `*_secs`, the scalar otherwise.
fn seconds_or(scalar: Scalar, name: &str) -> FlagKind {
    let integer = matches!(
        scalar,
        Scalar::I32 | Scalar::I64 | Scalar::U32 | Scalar::U64
    );
    let seconds = name.ends_with("_seconds") | name.ends_with("_secs");
    match integer & seconds {
        true => FlagKind::Seconds,
        false => FlagKind::Scalar(scalar),
    }
}

/// The values a flag of `enumeration` takes: every value, the one numbered 0 too (a
/// repeated, map or `optional` enum can hold it), by its short name, each with its own name.
fn choices(enumeration: &Enumeration) -> Vec<(String, String)> {
    let names: Vec<&str> = enumeration
        .values
        .iter()
        .map(|(name, _)| name.as_str())
        .collect();
    let short = short_names(&names);
    enumeration
        .values
        .iter()
        .zip(short)
        .map(|((name, _), short)| (short, name.clone()))
        .collect()
}

/// The text of [`super::CLI_FILE`] for `surface`, whose commands are `table`.
pub fn render(surface: &Surface, table: &[Command]) -> String {
    let mut out = super::client::header(surface);
    out.push_str(&format!(
        "\nuse js_client::jobs::JobClient;\nuse prost::Message;\n\nuse super::{{Command, Flag, Idempotency, Kind, Sink, decode}};\nuse crate::failure::Failure;\n\n/// The commands, one per call of `{}.{}`, in the order the definition names them.\npub(crate) const SURFACE: &[Command] = &[\n",
        surface.package, surface.service
    ));
    table
        .iter()
        .for_each(|command| out.push_str(&command_text(command)));
    out.push_str("];\n\n/// Sends `request`, the encoded request of the call `rpc` names, through `client`, and hands\n/// each answer, encoded, to `sink`. `rpc` is the `rpc` of an entry of `SURFACE`: the table and\n/// this match are generated together from the same calls, so the last call's arm is the\n/// match's fallback.\npub(crate) async fn dispatch(client: &mut JobClient, rpc: &str, request: &[u8], sink: Sink<'_>) -> Result<(), Failure> {\n    match rpc {\n");
    let last = surface.rpcs.len().saturating_sub(1);
    surface
        .rpcs
        .iter()
        .enumerate()
        .for_each(|(at, rpc)| out.push_str(&dispatch_arm(rpc, at == last)));
    out.push_str("    }\n}\n");
    out
}

/// One entry of `SURFACE`.
fn command_text(command: &Command) -> String {
    let path: Vec<String> = command
        .path
        .iter()
        .map(|word| format!("{word:?}"))
        .collect();
    let flags: String = command.flags.iter().map(flag_text).collect();
    let retry_key = command
        .retry_key
        .as_ref()
        .map_or("None".to_owned(), |field| {
            let parts: Vec<String> = field.iter().map(|part| format!("{part:?}")).collect();
            format!("Some(&[{}])", parts.join(", "))
        });
    let flags = match flags.is_empty() {
        true => "&[]".to_owned(),
        false => format!("&[\n{flags}        ]"),
    };
    format!(
        "    Command {{\n        path: &[{}],\n        rpc: {:?},\n        doc: {:?},\n        idempotency: Idempotency::{:?},\n        request: {:?},\n        response: {:?},\n        streaming: {},\n        retry_key: {},\n        flags: {flags},\n    }},\n",
        path.join(", "),
        command.rpc,
        command.doc,
        command.idempotency,
        command.request,
        command.response,
        command.streaming,
        retry_key,
    )
}

/// One flag of a `SURFACE` entry.
fn flag_text(flag: &Flag) -> String {
    let field: Vec<String> = flag.field.iter().map(|part| format!("{part:?}")).collect();
    format!(
        "            Flag {{ name: {:?}, field: &[{}], kind: {}, repeated: {}, required: {}, oneof: {}, doc: {:?} }},\n",
        flag.name,
        field.join(", "),
        kind_text(&flag.kind),
        flag.repeated,
        flag.required,
        flag.oneof
            .as_ref()
            .map_or("None".to_owned(), |oneof| format!("Some({oneof:?})")),
        flag.doc,
    )
}

/// A `Kind` of js-cli, as Rust.
fn kind_text(kind: &FlagKind) -> String {
    match kind {
        FlagKind::Scalar(scalar) => format!("Kind::{}", scalar_name(*scalar)),
        FlagKind::Seconds => "Kind::Seconds".to_owned(),
        FlagKind::Enum(choices) => {
            let choices: Vec<String> = choices
                .iter()
                .map(|(short, name)| format!("({short:?}, {name:?})"))
                .collect();
            format!("Kind::Enum(&[{}])", choices.join(", "))
        }
        FlagKind::Pair(key, value) => {
            format!("Kind::Pair(&{}, &{})", kind_text(key), kind_text(value))
        }
        FlagKind::File => "Kind::File".to_owned(),
    }
}

/// The name of a scalar's `Kind` variant in js-cli.
fn scalar_name(scalar: Scalar) -> &'static str {
    match scalar {
        Scalar::Bool => "Bool",
        Scalar::I32 => "I32",
        Scalar::I64 => "I64",
        Scalar::U32 => "U32",
        Scalar::U64 => "U64",
        Scalar::F32 => "F32",
        Scalar::F64 => "F64",
        Scalar::Text | Scalar::Bytes => "Text",
    }
}

/// The arm of `dispatch` for one call: the last call's arm matches any name (`last`).
fn dispatch_arm(rpc: &Rpc, last: bool) -> String {
    let method = snake(&rpc.name);
    let pattern = match last {
        true => format!("_ /* {} */", rpc.name),
        false => format!("{:?}", rpc.name),
    };
    match rpc.server_streaming {
        true => format!(
            "        {pattern} => {{\n            let mut answers = client.{method}(decode(request)?).await?;\n            while let Some(answer) = answers.next().await {{\n                sink(&answer?.encode_to_vec())?;\n            }}\n            Ok(())\n        }}\n"
        ),
        false => format!(
            "        {pattern} => sink(&client.{method}(decode(request)?).await?.encode_to_vec()),\n"
        ),
    }
}
