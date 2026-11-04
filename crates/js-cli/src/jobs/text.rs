//! An answer as text: one `path: value` line per field that is set, in the order of the
//! field numbers, a nested message's fields under its name with a dot (`job.spec.image`),
//! a list's elements by index (`jobs[0].id`) and a map's entries by key, in key order
//! (`job.spec.labels.team`). An enum value is its short name (`queued`); a `bytes` field
//! (a log tail) is its label on a line of its own and then its text, so `GetLogs` prints the
//! standard output tail and then the standard error tail, each under its label, and a
//! `truncated: true` line when a stream was cut. Control characters are left out of what
//! the server sent, except the line breaks and tabs of a log tail.

use prost_reflect::{DynamicMessage, Kind, MapKey, Value};

/// The lines of `message`.
pub(crate) fn lines(message: &DynamicMessage) -> String {
    let mut out = String::new();
    fields(message, "", &mut out);
    out
}

/// The lines of the fields of `message`, each path starting with `prefix`.
fn fields(message: &DynamicMessage, prefix: &str, out: &mut String) {
    for (field, value) in message.fields() {
        put(
            &format!("{prefix}{}", field.name()),
            &field.kind(),
            value,
            out,
        );
    }
}

/// The lines of `value`, of `kind`, at `path`.
fn put(path: &str, kind: &Kind, value: &Value, out: &mut String) {
    match value {
        Value::Message(inner) => fields(inner, &format!("{path}."), out),
        Value::List(items) => {
            for (at, item) in items.iter().enumerate() {
                put(&format!("{path}[{at}]"), kind, item, out);
            }
        }
        Value::Map(entries) => {
            let item_kind = kind
                .as_message()
                .map(|entry| entry.map_entry_value_field().kind());
            let item_kind = item_kind.unwrap_or_else(|| kind.clone());
            let mut sorted: Vec<(&MapKey, &Value)> = entries.iter().collect();
            sorted.sort_by(|a, b| a.0.cmp(b.0));
            for (key, item) in sorted {
                let key = scalar(&Value::from(key.clone()));
                put(&format!("{path}.{key}"), &item_kind, item, out);
            }
        }
        Value::Bytes(bytes) => {
            let text: String = String::from_utf8_lossy(bytes)
                .chars()
                .filter(|ch| !ch.is_control() | matches!(ch, '\n' | '\t'))
                .collect();
            let text = text.strip_suffix('\n').unwrap_or(&text);
            out.push_str(&format!("{path}:\n{text}\n"));
        }
        Value::EnumNumber(number) => line(path, &choice(kind, *number), out),
        other => line(path, &scalar(other), out),
    }
}

/// One `path: value` line.
fn line(path: &str, value: &str, out: &mut String) {
    out.push_str(&format!("{path}: {value}\n"));
}

/// A scalar as text: a string without its control characters and without quotes, anything
/// else as the protobuf text format writes it.
fn scalar(value: &Value) -> String {
    match value {
        Value::String(text) => text.chars().filter(|ch| !ch.is_control()).collect(),
        other => other.to_string(),
    }
}

/// The short name of the value `number` of the enum `kind`: its name without the prefix the
/// enum's value names share, in lower case with dashes (`JOB_STATE_FAILED_TO_START` is
/// `failed-to-start`); the number for a value the enum does not have.
fn choice(kind: &Kind, number: i32) -> String {
    let enumeration = kind.as_enum();
    let named = enumeration.and_then(|enumeration| {
        let value = enumeration.get_value(number)?;
        let names: Vec<String> = enumeration
            .values()
            .map(|value| value.name().to_owned())
            .collect();
        let prefix = names
            .into_iter()
            .reduce(|shared, name| common_prefix(&shared, &name))
            .unwrap_or_default();
        let cut = prefix.rfind('_').map_or(0, |at| at + 1);
        Some(
            value
                .name()
                .get(cut..)
                .unwrap_or_default()
                .to_lowercase()
                .replace('_', "-"),
        )
    });
    named.unwrap_or_else(|| number.to_string())
}

/// The longest prefix `a` and `b` share.
fn common_prefix(a: &str, b: &str) -> String {
    a.chars()
        .zip(b.chars())
        .take_while(|(x, y)| x == y)
        .map(|(x, _)| x)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Asserts that an enum value the enum does not have (one a newer server added) prints
    /// as its number, and a known one as its short name.
    #[test]
    fn an_unknown_enum_value_prints_its_number() {
        let pool = prost_reflect::DescriptorPool::decode(js_proto::FILE_DESCRIPTOR_SET)
            .expect("the job service decodes");
        let state = Kind::Enum(
            pool.get_enum_by_name("jobscheduler.v1.JobState")
                .expect("the job state"),
        );
        assert_eq!(
            (choice(&state, 99), choice(&state, 4)),
            ("99".to_owned(), "running".to_owned())
        );
    }
}
