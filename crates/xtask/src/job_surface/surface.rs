//! The [`Surface`] of a service, read from its encoded `FileDescriptorSet`: per call its name,
//! its documentation, its request message with every field (scalar, enum, repeated, map,
//! nested message, each with the comment the `.proto` file gives it), its response, whether
//! the response streams, and the idempotency level the definition states.

use std::collections::BTreeMap;

use prost_types::field_descriptor_proto::{Label as ProtoLabel, Type};
use prost_types::method_options::IdempotencyLevel;
use prost_types::{
    DescriptorProto, EnumDescriptorProto, FieldDescriptorProto, FileDescriptorProto,
    FileDescriptorSet, MethodDescriptorProto,
};

use super::GenError;

/// One service, as the generator renders it.
#[derive(Debug, Clone, PartialEq)]
pub struct Surface {
    /// The protobuf package, such as `jobscheduler.v1`.
    pub package: String,
    /// The service's name, such as `JobService`.
    pub service: String,
    /// The calls, in the order the definition names them.
    pub rpcs: Vec<Rpc>,
}

/// One call of the service.
#[derive(Debug, Clone, PartialEq)]
pub struct Rpc {
    /// The call's name, such as `SubmitJob`.
    pub name: String,
    /// The comment of the call, one entry per line.
    pub doc: Vec<String>,
    /// The request message.
    pub request: Message,
    /// The full name of the response message, such as `jobscheduler.v1.SubmitJobResponse`.
    pub response: String,
    /// The server streams its answers.
    pub server_streaming: bool,
    /// What the definition says about repeating the call (read when the definition sets it;
    /// it does not decide the retry key).
    pub idempotency: Idempotency,
    /// The field the call is idempotent on, from [`RETRY_KEYS`]: `js` fills it with a random
    /// id when its flag is not given.
    pub retry_key: Option<RetryKey>,
}

/// The field a call is idempotent on, and the flag that sets it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RetryKey {
    /// The path of the field from the request, by field name.
    pub field: Vec<String>,
    /// The flag of the field, without its dashes.
    pub flag: String,
}

/// One entry of [`RETRY_KEYS`].
#[derive(Debug, Clone, Copy)]
pub struct KeyEntry {
    /// The full name of the service.
    pub service: &'static str,
    /// The call.
    pub rpc: &'static str,
    /// The path of a singular `string` field of the request, joined by dots.
    pub field: &'static str,
    /// The flag the field gets in place of its dotted name.
    pub flag: &'static str,
}

/// The calls that are idempotent on a client-chosen id. The definition has no option that
/// says which field that is (design section 5.3 and the comment of `JobSpec.job_id` say it
/// in words), so this one entry holds it; every other rule reads the definition. Reading a
/// descriptor set of the named service fails when the call or the field is gone
/// ([`GenError::RetryKey`]), so the entry cannot fall out of date unnoticed. An additive
/// option in the definition would replace it.
pub const RETRY_KEYS: [KeyEntry; 1] = [KeyEntry {
    service: "jobscheduler.v1.JobService",
    rpc: "SubmitJob",
    field: "spec.job_id",
    flag: "id",
}];

/// The `idempotency_level` option of a call.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Idempotency {
    /// Not stated.
    Unknown,
    /// The call changes nothing.
    NoSideEffects,
    /// Repeating the call is the same as making it once.
    Idempotent,
}

/// A message and its fields.
#[derive(Debug, Clone, PartialEq)]
pub struct Message {
    /// The full name, without the leading dot.
    pub name: String,
    /// The fields, in the order the definition names them.
    pub fields: Vec<Field>,
}

/// One field of a message.
#[derive(Debug, Clone, PartialEq)]
pub struct Field {
    /// The field's name, such as `cpu_millis`.
    pub name: String,
    /// The comment of the field, one entry per line.
    pub doc: Vec<String>,
    /// Singular, optional (proto3 `optional`) or repeated. A map is repeated.
    pub label: Label,
    /// What the field holds.
    pub kind: Kind,
    /// The real `oneof` the field is a member of (not the one proto3 makes for `optional`).
    pub oneof: Option<String>,
}

/// How many values a field holds.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Label {
    /// One value, with no presence of its own.
    Singular,
    /// One value or none (proto3 `optional`).
    Optional,
    /// Any number, or a map.
    Repeated,
}

/// What a field holds.
#[derive(Debug, Clone, PartialEq)]
pub enum Kind {
    /// A scalar.
    Scalar(Scalar),
    /// An enum, with its values.
    Enum(Enumeration),
    /// A message, with its fields.
    Message(Message),
    /// A map from a scalar key to a value.
    Map(Scalar, Box<Kind>),
    /// A message already being read further up (a recursive type).
    Cycle,
}

/// The scalar types, grouped as Rust holds them.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Scalar {
    /// `bool`.
    Bool,
    /// `int32`, `sint32`, `sfixed32`.
    I32,
    /// `int64`, `sint64`, `sfixed64`.
    I64,
    /// `uint32`, `fixed32`.
    U32,
    /// `uint64`, `fixed64`.
    U64,
    /// `float`.
    F32,
    /// `double`.
    F64,
    /// `string`.
    Text,
    /// `bytes`.
    Bytes,
}

/// An enum and its values.
#[derive(Debug, Clone, PartialEq)]
pub struct Enumeration {
    /// Each value's name and number, in the order the definition names them.
    pub values: Vec<(String, i32)>,
}

/// The tag of `message_type` in `FileDescriptorProto`, of `nested_type` and `field` in
/// `DescriptorProto`, of `service` in the file and `method` in `ServiceDescriptorProto`: the
/// path elements of `SourceCodeInfo`.
const MESSAGE_TYPE: i32 = 4;
const NESTED_TYPE: i32 = 3;
const FIELD: i32 = 2;
const SERVICE: i32 = 6;
const METHOD: i32 = 2;

/// Reads the surface of the one service in `set`.
///
/// # Errors
///
/// [`GenError::Services`] unless `set` holds exactly one service; [`GenError::Unresolved`]
/// for a type name that names no message or enum of the set;
/// [`GenError::ClientStreaming`] for a call whose request streams;
/// [`GenError::Foreign`] for a call whose request or response is in another package;
/// [`GenError::RetryKey`] when an entry of [`RETRY_KEYS`] names this service and its call or
/// field is not there.
pub fn read(set: &FileDescriptorSet) -> Result<Surface, GenError> {
    let index = Index::new(set);
    let services: Vec<_> = set
        .file
        .iter()
        .flat_map(|file| {
            file.service
                .iter()
                .enumerate()
                .map(move |(at, s)| (file, at, s))
        })
        .collect();
    let (file, at, service) = match services.as_slice() {
        [one] => *one,
        other => return Err(GenError::Services(other.len())),
    };
    let comments = comments(file);
    let mut rpcs = service
        .method
        .iter()
        .enumerate()
        .map(|(number, method)| {
            let path = [SERVICE, index_of(at), METHOD, index_of(number)];
            index.rpc(method, doc(&comments, &path))
        })
        .collect::<Result<Vec<_>, _>>()?;
    let inside = format!("{}.", file.package());
    let foreign = rpcs.iter().find_map(|rpc| {
        let names = [rpc.request.name.as_str(), rpc.response.as_str()];
        names
            .into_iter()
            .find(|name| !name.starts_with(&inside))
            .map(|name| (rpc, name))
    });
    foreign.map_or(Ok(()), |(rpc, name)| {
        Err(GenError::Foreign {
            rpc: rpc.name.clone(),
            message: name.to_owned(),
        })
    })?;
    let full = format!("{}.{}", file.package(), service.name());
    let mut keys = RETRY_KEYS.iter().filter(|entry| entry.service == full);
    keys.try_for_each(|entry| attach(&mut rpcs, entry))?;
    Ok(Surface {
        package: file.package().to_owned(),
        service: service.name().to_owned(),
        rpcs,
    })
}

/// Gives the call `entry` names its retry key, or fails naming what is missing.
fn attach(rpcs: &mut [Rpc], entry: &KeyEntry) -> Result<(), GenError> {
    let missing = |why: String| GenError::RetryKey {
        rpc: format!("{}.{}", entry.service, entry.rpc),
        why,
    };
    let rpc = rpcs.iter_mut().find(|rpc| rpc.name == entry.rpc);
    let rpc = rpc.ok_or_else(|| missing("is not in the definition".to_owned()))?;
    let field: Vec<String> = entry.field.split('.').map(str::to_owned).collect();
    let found = field_at(&rpc.request, &field);
    let text = found.is_some_and(|field| {
        matches!(
            (field.label, &field.kind, field.oneof.is_some()),
            (Label::Singular, Kind::Scalar(Scalar::Text), false)
        )
    });
    match text {
        true => {
            rpc.retry_key = Some(RetryKey {
                field,
                flag: entry.flag.to_owned(),
            });
            Ok(())
        }
        false => Err(missing(format!(
            "has no singular string field {}",
            entry.field
        ))),
    }
}

/// The field at `path` from `message`, through nested messages.
fn field_at<'a>(message: &'a Message, path: &[String]) -> Option<&'a Field> {
    let (first, rest) = path.split_first()?;
    let field = message.fields.iter().find(|field| field.name == *first)?;
    match (rest.is_empty(), &field.kind) {
        (true, _) => Some(field),
        // The flags reach a nested field only through a singular message outside a `oneof`.
        (false, Kind::Message(inner)) => match (field.label, field.oneof.is_some()) {
            (Label::Singular | Label::Optional, false) => field_at(inner, rest),
            _ => None,
        },
        (false, _) => None,
    }
}

/// A position in a repeated field of a descriptor, as a path element.
fn index_of(at: usize) -> i32 {
    i32::try_from(at).unwrap_or(i32::MAX)
}

/// The comments of a file's elements, by the path of the element.
type Comments = BTreeMap<Vec<i32>, Vec<String>>;

/// The comment of every element of `file` that has one: its leading comment, or its trailing
/// one (`int32 limit = 2; // ...`) when it has no leading comment; each line trimmed and the
/// trailing blank lines dropped.
fn comments(file: &FileDescriptorProto) -> Comments {
    let locations = file.source_code_info.iter().flat_map(|info| &info.location);
    locations
        .map(|location| {
            let leading = lines(location.leading_comments());
            let trailing = lines(location.trailing_comments());
            let comment = match leading.is_empty() {
                true => trailing,
                false => leading,
            };
            (location.path.clone(), comment)
        })
        .collect()
}

/// The lines of a comment, trimmed, without the blank lines at its end.
fn lines(comment: &str) -> Vec<String> {
    let mut lines: Vec<String> = comment.lines().map(|line| line.trim().to_owned()).collect();
    let kept = lines
        .iter()
        .rposition(|line| !line.is_empty())
        .map_or(0, |at| at + 1);
    lines.truncate(kept);
    lines
}

/// The comment at `path`, or none.
fn doc(comments: &Comments, path: &[i32]) -> Vec<String> {
    comments.get(path).cloned().unwrap_or_default()
}

/// A message as the index holds it: the message, its path in its file and the position of
/// that file's comments in [`Index::comments`].
type Entry<'a> = (&'a DescriptorProto, Vec<i32>, usize);

/// Every message and enum of a set, by full name with its leading dot.
struct Index<'a> {
    messages: BTreeMap<String, Entry<'a>>,
    enums: BTreeMap<String, &'a EnumDescriptorProto>,
    /// The comments of each file of the set, in the set's order.
    comments: Vec<Comments>,
}

impl<'a> Index<'a> {
    fn new(set: &'a FileDescriptorSet) -> Self {
        let mut index = Self {
            messages: BTreeMap::new(),
            enums: BTreeMap::new(),
            comments: set.file.iter().map(comments).collect(),
        };
        for (number, file) in set.file.iter().enumerate() {
            let scope = format!(".{}", file.package());
            index.add_enums(&scope, &file.enum_type);
            for (at, message) in file.message_type.iter().enumerate() {
                index.add_message(&scope, message, vec![MESSAGE_TYPE, index_of(at)], number);
            }
        }
        index
    }

    /// Adds `message`, its nested messages and its nested enums.
    fn add_message(
        &mut self,
        scope: &str,
        message: &'a DescriptorProto,
        path: Vec<i32>,
        file: usize,
    ) {
        let name = format!("{scope}.{}", message.name());
        self.add_enums(&name, &message.enum_type);
        for (at, nested) in message.nested_type.iter().enumerate() {
            let mut nested_path = path.clone();
            nested_path.extend([NESTED_TYPE, index_of(at)]);
            self.add_message(&name, nested, nested_path, file);
        }
        self.messages.insert(name, (message, path, file));
    }

    fn add_enums(&mut self, scope: &str, enums: &'a [EnumDescriptorProto]) {
        for item in enums {
            self.enums.insert(format!("{scope}.{}", item.name()), item);
        }
    }

    /// The call `method`, whose comment is `doc`.
    fn rpc(&self, method: &MethodDescriptorProto, doc: Vec<String>) -> Result<Rpc, GenError> {
        let refused = GenError::ClientStreaming(method.name().to_owned());
        let request = match method.client_streaming() {
            true => return Err(refused),
            false => self.message(method.input_type(), &mut Vec::new())?,
        };
        self.resolve(method.output_type())?;
        let idempotency = method
            .options
            .as_ref()
            .map(|options| options.idempotency_level());
        Ok(Rpc {
            name: method.name().to_owned(),
            doc,
            request,
            response: method.output_type().trim_start_matches('.').to_owned(),
            server_streaming: method.server_streaming(),
            retry_key: None,
            idempotency: match idempotency.unwrap_or(IdempotencyLevel::IdempotencyUnknown) {
                IdempotencyLevel::IdempotencyUnknown => Idempotency::Unknown,
                IdempotencyLevel::NoSideEffects => Idempotency::NoSideEffects,
                IdempotencyLevel::Idempotent => Idempotency::Idempotent,
            },
        })
    }

    /// The message `type_name` names.
    fn resolve(&self, type_name: &str) -> Result<&Entry<'a>, GenError> {
        let found = self.messages.get(type_name);
        found.ok_or_else(|| GenError::Unresolved(type_name.to_owned()))
    }

    /// The message `type_name` names, with its fields; `reading` is the messages being read
    /// further up, so a recursive type ends in [`Kind::Cycle`].
    fn message(&self, type_name: &str, reading: &mut Vec<String>) -> Result<Message, GenError> {
        let (message, path, file) = self.resolve(type_name)?;
        let comments = self.comments.get(*file).cloned().unwrap_or_default();
        reading.push(type_name.to_owned());
        let fields = message
            .field
            .iter()
            .enumerate()
            .map(|(at, field)| {
                let mut field_path = path.clone();
                field_path.extend([FIELD, index_of(at)]);
                let oneof = field.oneof_index.and_then(|at| {
                    message
                        .oneof_decl
                        .get(usize::try_from(at).unwrap_or(usize::MAX))
                });
                let oneof = oneof.map(|decl| decl.name().to_owned());
                self.field(field, oneof, doc(&comments, &field_path), reading)
            })
            .collect::<Result<Vec<_>, _>>();
        reading.pop();
        Ok(Message {
            name: type_name.trim_start_matches('.').to_owned(),
            fields: fields?,
        })
    }

    /// One field, whose comment is `doc`.
    fn field(
        &self,
        field: &FieldDescriptorProto,
        oneof: Option<String>,
        doc: Vec<String>,
        reading: &mut Vec<String>,
    ) -> Result<Field, GenError> {
        let optional = field.proto3_optional();
        let label = match (field.label(), optional) {
            (ProtoLabel::Repeated, _) => Label::Repeated,
            (_, true) => Label::Optional,
            (_, false) => Label::Singular,
        };
        Ok(Field {
            name: field.name().to_owned(),
            doc,
            label,
            kind: self.kind(field, reading)?,
            oneof: oneof.filter(|_| !optional),
        })
    }

    /// What `field` holds.
    fn kind(
        &self,
        field: &FieldDescriptorProto,
        reading: &mut Vec<String>,
    ) -> Result<Kind, GenError> {
        let type_name = field.type_name();
        match (scalar(field.r#type()), field.r#type()) {
            (Some(scalar), _) => Ok(Kind::Scalar(scalar)),
            (None, Type::Enum) => self.enumeration(type_name).map(Kind::Enum),
            (None, _) => self.message_kind(type_name, reading),
        }
    }

    /// The kind of a message-typed field: a map, a cycle or a message.
    fn message_kind(&self, type_name: &str, reading: &mut Vec<String>) -> Result<Kind, GenError> {
        let (message, _, _) = self.resolve(type_name)?;
        let is_map = message
            .options
            .as_ref()
            .is_some_and(|options| options.map_entry());
        let cycle = reading.iter().any(|name| name == type_name);
        match (is_map, cycle) {
            (true, _) => self.map(message, reading),
            (false, true) => Ok(Kind::Cycle),
            (false, false) => self.message(type_name, reading).map(Kind::Message),
        }
    }

    /// A map entry message as a map: its key (field 1, a scalar) and its value (field 2).
    fn map(&self, entry: &DescriptorProto, reading: &mut Vec<String>) -> Result<Kind, GenError> {
        let unresolved = || {
            GenError::Unresolved(format!(
                "{}: a map entry without a key and a value",
                entry.name()
            ))
        };
        let (key, value) = match entry.field.as_slice() {
            [key, value] => (key, value),
            _ => return Err(unresolved()),
        };
        let key = scalar(key.r#type()).ok_or_else(unresolved)?;
        Ok(Kind::Map(key, Box::new(self.kind(value, reading)?)))
    }

    /// The enum `type_name` names, with its values.
    fn enumeration(&self, type_name: &str) -> Result<Enumeration, GenError> {
        let found = self.enums.get(type_name);
        let item = found.ok_or_else(|| GenError::Unresolved(type_name.to_owned()))?;
        Ok(Enumeration {
            values: item
                .value
                .iter()
                .map(|value| (value.name().to_owned(), value.number()))
                .collect(),
        })
    }
}

/// The scalar a field type is, or `None` for an enum, a message or a group.
fn scalar(kind: Type) -> Option<Scalar> {
    match kind {
        Type::Bool => Some(Scalar::Bool),
        Type::Int32 | Type::Sint32 | Type::Sfixed32 => Some(Scalar::I32),
        Type::Int64 | Type::Sint64 | Type::Sfixed64 => Some(Scalar::I64),
        Type::Uint32 | Type::Fixed32 => Some(Scalar::U32),
        Type::Uint64 | Type::Fixed64 => Some(Scalar::U64),
        Type::Float => Some(Scalar::F32),
        Type::Double => Some(Scalar::F64),
        Type::String => Some(Scalar::Text),
        Type::Bytes => Some(Scalar::Bytes),
        Type::Enum | Type::Message | Type::Group => None,
    }
}
