//! The schema of package `jobscheduler.v1`, read from [`FILE_DESCRIPTOR_SET`]: the methods
//! of `JobService`, the rule that no call or message is about holding capacity, and the
//! evolution rule of `service.proto` (every shipped field and enum value keeps its name,
//! number and type; new ones may be added).

use std::collections::{BTreeMap, BTreeSet};

use js_proto::FILE_DESCRIPTOR_SET;
use prost::Message;
use prost_types::field_descriptor_proto::{Label, Type};
use prost_types::{
    DescriptorProto, EnumDescriptorProto, EnumValueDescriptorProto, FieldDescriptorProto,
    FileDescriptorSet, MethodDescriptorProto,
};

/// The package every file of the service is in.
const PACKAGE: &str = "jobscheduler.v1";

/// Words a call or message about holding capacity would carry in its name.
const CAPACITY_WORDS: [&str; 7] = [
    "acquire", "renew", "release", "lease", "reserve", "allocat", "book",
];

/// The one type whose name may carry a capacity word: the read-only view of what is booked
/// on a host.
const READ_ONLY_TYPE: &str = "HostBooking";

/// The one field name that may carry a capacity word: the bookings a host or a usage
/// record shows.
const READ_ONLY_FIELD: &str = "booked";

fn descriptor_set() -> FileDescriptorSet {
    FileDescriptorSet::decode(FILE_DESCRIPTOR_SET).expect("the descriptor set decodes")
}

/// The full name `.jobscheduler.v1.Name` of a type of the package, as a field names it.
fn full(name: &str) -> String {
    format!(".{PACKAGE}.{name}")
}

/// A type name as the golden writes it: without the leading dot, and without the package
/// for a type of this package.
fn short(type_name: &str) -> &str {
    let name = type_name.trim_start_matches('.');
    name.strip_prefix(PACKAGE)
        .and_then(|rest| rest.strip_prefix('.'))
        .unwrap_or(name)
}

/// The messages and enums of the package, by full name, nested ones included.
#[derive(Default)]
struct Types<'a> {
    messages: BTreeMap<String, &'a DescriptorProto>,
    enums: BTreeMap<String, &'a EnumDescriptorProto>,
}

impl<'a> Types<'a> {
    fn of(set: &'a FileDescriptorSet) -> Self {
        let mut types = Self::default();
        for file in set.file.iter().filter(|f| f.package() == PACKAGE) {
            for message in &file.message_type {
                types.add_message(&format!(".{PACKAGE}"), message);
            }
            for e in &file.enum_type {
                types.enums.insert(format!(".{PACKAGE}.{}", e.name()), e);
            }
        }
        types
    }

    fn add_message(&mut self, scope: &str, message: &'a DescriptorProto) {
        let name = format!("{scope}.{}", message.name());
        for nested in &message.nested_type {
            self.add_message(&name, nested);
        }
        for e in &message.enum_type {
            self.enums.insert(format!("{name}.{}", e.name()), e);
        }
        self.messages.insert(name, message);
    }

    /// The map entry type `full_name` stands for, as `(key, value)`, if it is one.
    fn map_entry(
        &self,
        full_name: &str,
    ) -> Option<(&'a FieldDescriptorProto, &'a FieldDescriptorProto)> {
        let message: &'a DescriptorProto = self.messages.get(full_name).copied()?;
        let is_entry = message.options.as_ref().is_some_and(|o| o.map_entry());
        match (is_entry, message.field.as_slice()) {
            (true, [key, value]) => Some((key, value)),
            _ => None,
        }
    }
}

/// The type of `field` as the golden writes it.
fn field_type(types: &Types<'_>, field: &FieldDescriptorProto) -> String {
    if let Some((key, value)) = types.map_entry(field.type_name()) {
        return format!(
            "map<{}, {}>",
            field_type(types, key),
            field_type(types, value)
        );
    }
    let base = match field.r#type() {
        Type::Double => "double",
        Type::Float => "float",
        Type::Int64 => "int64",
        Type::Uint64 => "uint64",
        Type::Int32 => "int32",
        Type::Fixed64 => "fixed64",
        Type::Fixed32 => "fixed32",
        Type::Bool => "bool",
        Type::String => "string",
        Type::Group => "group",
        Type::Bytes => "bytes",
        Type::Uint32 => "uint32",
        Type::Sfixed32 => "sfixed32",
        Type::Sfixed64 => "sfixed64",
        Type::Sint32 => "sint32",
        Type::Sint64 => "sint64",
        Type::Message | Type::Enum => short(field.type_name()),
    };
    if field.label() == Label::Repeated {
        format!("repeated {base}")
    } else if field.proto3_optional() {
        format!("optional {base}")
    } else {
        base.to_owned()
    }
}

/// Every field and enum value of the package as golden rows: `Message.field` to
/// `number : type`, and `Enum.VALUE` to `number`. A map entry type is written as the map
/// field's type, not as rows of its own.
fn rows(set: &FileDescriptorSet) -> BTreeMap<String, String> {
    let types = Types::of(set);
    let mut rows = BTreeMap::new();
    for (name, message) in &types.messages {
        if types.map_entry(name).is_some() {
            continue;
        }
        for field in &message.field {
            rows.insert(
                format!("{}.{}", short(name), field.name()),
                format!("{} : {}", field.number(), field_type(&types, field)),
            );
        }
    }
    for (name, e) in &types.enums {
        for value in &e.value {
            rows.insert(
                format!("{}.{}", short(name), value.name()),
                value.number().to_string(),
            );
        }
    }
    rows
}

/// The rows of `golden/fields.txt`, refusing a malformed or repeated row.
fn golden_rows() -> BTreeMap<String, String> {
    let mut rows = BTreeMap::new();
    for line in include_str!("golden/fields.txt").lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let (key, value) = line.split_once(" = ").expect("a row is `key = value`");
        let repeated = rows.insert(key.to_owned(), value.to_owned());
        assert_eq!(repeated, None, "{key} is in the golden twice");
    }
    rows
}

/// What a schema `current` breaks of the shipped rows `shipped`: one line per shipped row
/// that is missing (removed or renamed) or whose number or type changed. A row `current`
/// has and `shipped` does not is an addition and is not reported.
fn broken_rows(
    shipped: &BTreeMap<String, String>,
    current: &BTreeMap<String, String>,
) -> Vec<String> {
    shipped
        .iter()
        .filter_map(|(key, was)| match current.get(key) {
            None => Some(format!("{key}: removed or renamed")),
            Some(now) if now != was => Some(format!("{key}: was `{was}`, now `{now}`")),
            Some(_) => None,
        })
        .collect()
}

/// A name seen on the service's surface: a method, a type a call reaches, or a field of
/// such a type.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
enum Name {
    Method(String),
    Type(String),
    Field(String, String),
}

impl Name {
    fn text(&self) -> &str {
        match self {
            Self::Method(n) | Self::Type(n) | Self::Field(_, n) => n,
        }
    }

    fn is_read_only_exception(&self) -> bool {
        match self {
            Self::Type(n) => n == READ_ONLY_TYPE,
            Self::Field(_, n) => n == READ_ONLY_FIELD,
            Self::Method(_) => false,
        }
    }
}

/// Every name on the surface of `JobService`: each method, and each message and enum a
/// request or response reaches, transitively, with the fields of those messages. Enum
/// values are states, not calls, and are not included.
fn surface(set: &FileDescriptorSet) -> BTreeSet<Name> {
    let types = Types::of(set);
    let service = set
        .file
        .iter()
        .filter(|f| f.package() == PACKAGE)
        .flat_map(|f| &f.service)
        .find(|s| s.name() == "JobService")
        .expect("the package defines JobService");
    let mut names = BTreeSet::new();
    let mut work: Vec<String> = Vec::new();
    for method in &service.method {
        names.insert(Name::Method(method.name().to_owned()));
        work.push(method.input_type().to_owned());
        work.push(method.output_type().to_owned());
    }
    let mut seen = BTreeSet::new();
    while let Some(type_name) = work.pop() {
        if !seen.insert(type_name.clone()) {
            continue;
        }
        let simple = type_name.rsplit('.').next().unwrap_or_default().to_owned();
        if let Some(message) = types.messages.get(&type_name) {
            names.insert(Name::Type(simple.clone()));
            for field in &message.field {
                names.insert(Name::Field(simple.clone(), field.name().to_owned()));
                if !field.type_name().is_empty() {
                    work.push(field.type_name().to_owned());
                }
            }
        } else if types.enums.contains_key(&type_name) {
            names.insert(Name::Type(simple));
        }
    }
    names
}

/// The names of `surface` that carry a capacity word, without the read-only exceptions.
fn capacity_names(surface: &BTreeSet<Name>) -> Vec<Name> {
    surface
        .iter()
        .filter(|name| !name.is_read_only_exception())
        .filter(|name| {
            let lower = name.text().to_lowercase();
            CAPACITY_WORDS.iter().any(|word| lower.contains(word))
        })
        .cloned()
        .collect()
}

fn message_mut<'a>(set: &'a mut FileDescriptorSet, name: &str) -> &'a mut DescriptorProto {
    set.file
        .iter_mut()
        .filter(|f| f.package() == PACKAGE)
        .flat_map(|f| &mut f.message_type)
        .find(|m| m.name() == name)
        .expect("the message exists")
}

fn string_field(name: &str, number: i32) -> FieldDescriptorProto {
    FieldDescriptorProto {
        name: Some(name.to_owned()),
        number: Some(number),
        label: Some(Label::Optional.into()),
        r#type: Some(Type::String.into()),
        ..FieldDescriptorProto::default()
    }
}

/// Asserts that the methods of `JobService`, in the order of `service.proto`, are the 12
/// of `golden/methods.txt`. Catches a method added, removed, renamed or moved without the
/// golden changing with it.
#[test]
fn method_list_matches_golden() {
    let set = descriptor_set();
    let service = set
        .file
        .iter()
        .flat_map(|f| &f.service)
        .find(|s| s.name() == "JobService")
        .expect("JobService is defined");
    let methods: Vec<&str> = service
        .method
        .iter()
        .map(MethodDescriptorProto::name)
        .collect();
    let golden: Vec<&str> = include_str!("golden/methods.txt").lines().collect();
    assert_eq!(golden.len(), 12);
    assert_eq!(methods, golden);
}

/// Asserts that no method, no type a request or response reaches and no field of such a
/// type is named for acquiring, renewing, releasing, leasing, reserving, allocating or
/// booking capacity, but for the read-only `HostBooking` and `booked`; and that the check
/// sees both exceptions and names a method, a message and a field that break the rule.
#[test]
fn no_method_acquires_renews_or_releases_capacity() {
    let set = descriptor_set();
    let names = surface(&set);
    assert_eq!(capacity_names(&names), Vec::<Name>::new());
    assert!(names.contains(&Name::Type(READ_ONLY_TYPE.to_owned())));
    assert!(names.contains(&Name::Field("Host".to_owned(), READ_ONLY_FIELD.to_owned())));
    assert!(names.contains(&Name::Field(
        "UsageRecord".to_owned(),
        READ_ONLY_FIELD.to_owned()
    )));

    let mut broken = set.clone();
    let service = broken
        .file
        .iter_mut()
        .flat_map(|f| &mut f.service)
        .find(|s| s.name() == "JobService")
        .expect("JobService is defined");
    service.method.push(MethodDescriptorProto {
        name: Some("AcquireCapacity".to_owned()),
        input_type: Some(full("GetJobRequest")),
        output_type: Some(full("GetJobResponse")),
        ..MethodDescriptorProto::default()
    });
    let mut allocation = DescriptorProto {
        name: Some("Allocation".to_owned()),
        ..DescriptorProto::default()
    };
    allocation.field.push(string_field("host_id", 1));
    let jobs_file = broken
        .file
        .iter_mut()
        .find(|f| f.name() == "jobscheduler/v1/job.proto")
        .expect("job.proto is in the set");
    jobs_file.message_type.push(allocation);
    let mut allocation_field = string_field("allocation", 100);
    allocation_field.r#type = Some(Type::Message.into());
    allocation_field.type_name = Some(full("Allocation"));
    message_mut(&mut broken, "GetJobResponse")
        .field
        .push(allocation_field);
    message_mut(&mut broken, "CordonHostRequest")
        .field
        .push(string_field("lease_secs", 100));
    assert_eq!(
        capacity_names(&surface(&broken)),
        [
            Name::Method("AcquireCapacity".to_owned()),
            Name::Type("Allocation".to_owned()),
            Name::Field("CordonHostRequest".to_owned(), "lease_secs".to_owned()),
            Name::Field("GetJobResponse".to_owned(), "allocation".to_owned()),
        ]
    );
}

/// Asserts that every row of `golden/fields.txt` holds in the descriptor set: each shipped
/// field keeps its name, number and type, and each shipped enum value its name and number.
/// This is the check of the evolution rule for readers of shipped data.
#[test]
fn shipped_fields_are_a_subset_of_current_fields() {
    let current = rows(&descriptor_set());
    assert_eq!(broken_rows(&golden_rows(), &current), Vec::<String>::new());
    // The golden is not empty and covers the four files.
    for row in [
        "JobSpec.job_id",
        "Host.booked",
        "UsageRecord.booked",
        "GetLogsResponse.truncated",
        "RefusalCode.REFUSAL_CODE_JOB_ID_CONFLICT",
    ] {
        assert!(golden_rows().contains_key(row), "{row}");
    }
}

/// Asserts that a field and an enum value added to the descriptor after the golden still
/// pass, so the check is not an equality that blocks additions, and that the added rows
/// are read (two more rows than the golden).
#[test]
fn a_field_added_after_the_golden_still_passes() {
    let mut set = descriptor_set();
    message_mut(&mut set, "Needs")
        .field
        .push(string_field("accelerator_model", 100));
    set.file
        .iter_mut()
        .flat_map(|f| &mut f.enum_type)
        .find(|e| e.name() == "JobState")
        .expect("JobState is defined")
        .value
        .push(EnumValueDescriptorProto {
            name: Some("JOB_STATE_PAUSED".to_owned()),
            number: Some(100),
            ..EnumValueDescriptorProto::default()
        });
    let current = rows(&set);
    let golden = golden_rows();
    assert_eq!(broken_rows(&golden, &current), Vec::<String>::new());
    assert_eq!(
        current.get("Needs.accelerator_model").map(String::as_str),
        Some("100 : string")
    );
    assert_eq!(
        current.get("JobState.JOB_STATE_PAUSED").map(String::as_str),
        Some("100")
    );
    assert_eq!(current.len(), rows(&descriptor_set()).len() + 2);
}

/// Asserts that a shipped field that is removed, renamed, renumbered or retyped, a shipped
/// map whose value type changes, and a shipped enum value that is renumbered are each named
/// by the check.
#[test]
fn each_breaking_change_of_a_shipped_field_is_named() {
    let golden = golden_rows();
    let breaks = |change: &dyn Fn(&mut FileDescriptorSet)| {
        let mut set = descriptor_set();
        change(&mut set);
        broken_rows(&golden, &rows(&set))
    };
    let field = |set: &mut FileDescriptorSet, message: &str, name: &str| -> FieldDescriptorProto {
        message_mut(set, message)
            .field
            .iter()
            .find(|f| f.name() == name)
            .cloned()
            .expect("the field exists")
    };
    let replace = |set: &mut FileDescriptorSet, message: &str, new: FieldDescriptorProto| {
        let fields = &mut message_mut(set, message).field;
        let at = fields
            .iter()
            .position(|f| f.number() == new.number() || f.name() == new.name())
            .expect("the field exists");
        fields[at] = new;
    };
    assert_eq!(
        breaks(&|set| message_mut(set, "Needs")
            .field
            .retain(|f| f.name() != "gpus")),
        ["Needs.gpus: removed or renamed"]
    );
    assert_eq!(
        breaks(&|set| {
            let mut gpus = field(set, "Needs", "gpus");
            gpus.name = Some("gpu_count".to_owned());
            replace(set, "Needs", gpus);
        }),
        ["Needs.gpus: removed or renamed"]
    );
    assert_eq!(
        breaks(&|set| {
            let mut gpus = field(set, "Needs", "gpus");
            gpus.number = Some(40);
            replace(set, "Needs", gpus);
        }),
        ["Needs.gpus: was `4 : uint32`, now `40 : uint32`"]
    );
    assert_eq!(
        breaks(&|set| {
            let mut gpus = field(set, "Needs", "gpus");
            gpus.r#type = Some(Type::Uint64.into());
            replace(set, "Needs", gpus);
        }),
        ["Needs.gpus: was `4 : uint32`, now `4 : uint64`"]
    );
    assert_eq!(
        breaks(&|set| {
            let mut gpus = field(set, "Needs", "gpus");
            gpus.label = Some(Label::Repeated.into());
            replace(set, "Needs", gpus);
        }),
        ["Needs.gpus: was `4 : uint32`, now `4 : repeated uint32`"]
    );
    assert_eq!(
        breaks(&|set| {
            let entry = message_mut(set, "JobSpec")
                .nested_type
                .iter_mut()
                .find(|m| m.name() == "EnvEntry")
                .expect("JobSpec.env is a map");
            entry.field[1].r#type = Some(Type::Bytes.into());
        }),
        ["JobSpec.env: was `5 : map<string, string>`, now `5 : map<string, bytes>`"]
    );
    assert_eq!(
        breaks(&|set| {
            let state = set
                .file
                .iter_mut()
                .flat_map(|f| &mut f.enum_type)
                .find(|e| e.name() == "JobState")
                .expect("JobState is defined");
            state.value[1].number = Some(11);
        }),
        ["JobState.JOB_STATE_QUEUED: was `1`, now `11`"]
    );
}
