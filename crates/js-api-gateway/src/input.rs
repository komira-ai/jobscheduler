//! A call's request message, made from an HTTP request: the path variables, then either the
//! JSON body (a route whose body is the request) or the query string (any other route),
//! decoded by the proto3 JSON mapping from the request's descriptor.

use std::borrow::Cow;
use std::collections::{BTreeMap, BTreeSet};
use std::fmt;

use percent_encoding::percent_decode_str;
use prost_reflect::{DynamicMessage, FieldDescriptor, Kind, MessageDescriptor, Value};
use serde::de::{Deserialize, Deserializer, Error as _, MapAccess, SeqAccess, Visitor};
use serde_json::{Map, Number, Value as Json};

use crate::answer::Answer;

/// The most bytes in a job id.
const MAX_JOB_ID: usize = 63;

/// The reason of a 400 for a body that is not JSON.
const NOT_JSON: &str = "the body is not valid JSON";
/// The reason of a 400 for a body that is JSON but not an object.
const NOT_AN_OBJECT: &str = "the body is not a JSON object";
/// The reason of a 400 for a body with an object that gives one member twice.
const MEMBER_TWICE: &str = "an object of the body gives a member twice";
/// The reason of a 400 for a body sent to a route that takes none.
const NO_BODY: &str = "this route takes no body; its fields are set by the path and the query";
/// The reason of a 400 for a query string sent to a route whose body is the request.
const NO_QUERY: &str = "this route takes its fields in the body, not in the query";
/// The reason of a 400 for path or query text with a malformed percent escape, or that does
/// not decode as UTF-8.
const NOT_TEXT: &str = "the text has a malformed percent escape or does not decode as UTF-8";
/// The reason of a 400 for a query parameter or JSON key that names no field.
const UNKNOWN_FIELD: &str = "the request has no such field";
/// The reason of a 400 for a query parameter of a map that names no key, or of another
/// field that names one.
const MALFORMED_KEY: &str = "a map field is set as name[key]=value, any other field as name=value";
/// The reason of a 400 for a field set twice.
const TWICE: &str = "the field is set twice";
/// The reason of a 400 for a body or query field the request does not have, or that holds a
/// value of the wrong kind.
const WRONG_VALUE: &str = "the field is unknown, or does not hold a value of its type";
/// The reason of a 400 for an id outside its grammar.
const BAD_ID: &str = "the field is not a valid id";
/// The reason of a 400 for a body or query field that the path sets.
const SET_BY_PATH: &str = "the field is set by the path";

/// Whether `text` is a job id: 1 to 63 bytes of letters, digits, dot, underscore and dash
/// (design section 5.3, `JobSpec.job_id` in `job.proto`).
fn is_job_id(text: &str) -> bool {
    (1..=MAX_JOB_ID).contains(&text.len())
        && text
            .bytes()
            .all(|b| matches!(b, b'a'..=b'z' | b'A'..=b'Z' | b'0'..=b'9' | b'.' | b'_' | b'-'))
}

/// Whether `value` may set the path variable `field`: a job id (`job_id`) in its grammar,
/// any other field (such as `host_id`, which the service judges) any text but the empty
/// one.
pub(crate) fn path_value_ok(field: &str, value: &str) -> bool {
    match field {
        "job_id" => is_job_id(value),
        _ => !value.is_empty(),
    }
}

/// The request message of `input` for an HTTP request: its path `variables` (field name and
/// text as written in the path), its `query` string and its `body`. `takes_body` says the
/// route's body is the request; otherwise the query sets the fields and the body must be
/// empty.
///
/// Checks, in order, each answering a 400 that names the field (by its JSON-name path)
/// when one is to blame:
///
/// - each path variable, percent-decoded once, against its grammar;
/// - the body (a JSON object whose objects give no member twice; an empty body is `{}`) or
///   the query;
/// - no field the path sets also named by the body or the query, under either of its
///   names, whatever its value (`null` and the default too);
/// - no field given under both its names, and every integer field that holds a JSON number
///   holding an exact integer (a fraction, an exponent or a number past the 64-bit range
///   would otherwise be rounded);
/// - the decoding by the proto3 JSON mapping (an unknown field, a value of the wrong kind);
/// - every `job_id` the message holds outside lists and maps.
pub(crate) fn build(
    input: &MessageDescriptor,
    takes_body: bool,
    variables: &[(String, String)],
    query: Option<&str>,
    body: &[u8],
) -> Result<DynamicMessage, Answer> {
    let mut path = Vec::new();
    for (name, raw) in variables {
        let field = input.get_field_by_name(name).ok_or_else(Answer::internal)?;
        let at = Some(field.json_name().to_owned());
        let Some(value) = unescape(raw, false) else {
            return Err(Answer::invalid(at, NOT_TEXT));
        };
        if !path_value_ok(name, &value) {
            return Err(Answer::invalid(at, BAD_ID));
        }
        path.push((field, value));
    }
    let query = query.unwrap_or_default();
    let object = if takes_body {
        body_object(query, body)?
    } else {
        query_object(input, query, body)?
    };
    for (field, _) in &path {
        if object.keys().any(|key| names(field, key)) {
            let at = Some(field.json_name().to_owned());
            return Err(Answer::invalid(at, SET_BY_PATH));
        }
    }
    if let Some((field, reason)) = check(input, &object, "") {
        return Err(Answer::invalid(Some(field), reason));
    }
    let mut message = decode(input, &object)?;
    if let Some(field) = first_bad_id(&message, "") {
        return Err(Answer::invalid(Some(field), BAD_ID));
    }
    for (field, value) in path {
        message.set_field(&field, Value::String(value));
    }
    Ok(message)
}

/// Whether `key` names `field`, by its JSON name or its proto name.
fn names(field: &FieldDescriptor, key: &str) -> bool {
    key == field.json_name() || key == field.name()
}

/// The field of `message` that `key` names, by its JSON name or its proto name.
fn lookup(message: &MessageDescriptor, key: &str) -> Option<FieldDescriptor> {
    message
        .get_field_by_json_name(key)
        .or_else(|| message.get_field_by_name(key))
}

/// The JSON object of a route whose body is the request. Its query string must be empty.
fn body_object(query: &str, body: &[u8]) -> Result<Map<String, Json>, Answer> {
    if let Some((key, _)) = pairs(query)?.into_iter().next() {
        return Err(Answer::invalid(Some(key), NO_QUERY));
    }
    if body.is_empty() {
        return Ok(Map::new());
    }
    match serde_json::from_slice::<Strict>(body) {
        Ok(Strict(Json::Object(object))) => Ok(object),
        Ok(_) => Err(Answer::invalid(None, NOT_AN_OBJECT)),
        Err(err) if err.is_data() => Err(Answer::invalid(None, MEMBER_TWICE)),
        Err(_) => Err(Answer::invalid(None, NOT_JSON)),
    }
}

/// A JSON value, parsed as `serde_json::Value` is but refusing an object that gives a member
/// twice (which `serde_json::Value` would keep the last of).
pub(crate) struct Strict(Json);

impl<'de> Deserialize<'de> for Strict {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        deserializer.deserialize_any(StrictVisitor)
    }
}

/// The visitor of [`Strict`].
pub(crate) struct StrictVisitor;

impl<'de> Visitor<'de> for StrictVisitor {
    type Value = Strict;

    fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("a JSON value")
    }

    fn visit_bool<E: serde::de::Error>(self, v: bool) -> Result<Strict, E> {
        Ok(Strict(Json::Bool(v)))
    }

    fn visit_i64<E: serde::de::Error>(self, v: i64) -> Result<Strict, E> {
        Ok(Strict(Json::from(v)))
    }

    fn visit_u64<E: serde::de::Error>(self, v: u64) -> Result<Strict, E> {
        Ok(Strict(Json::from(v)))
    }

    fn visit_f64<E: serde::de::Error>(self, v: f64) -> Result<Strict, E> {
        Ok(Strict(Number::from_f64(v).map_or(Json::Null, Json::Number)))
    }

    fn visit_str<E: serde::de::Error>(self, v: &str) -> Result<Strict, E> {
        Ok(Strict(Json::String(v.to_owned())))
    }

    fn visit_unit<E: serde::de::Error>(self) -> Result<Strict, E> {
        Ok(Strict(Json::Null))
    }

    fn visit_seq<A: SeqAccess<'de>>(self, mut seq: A) -> Result<Strict, A::Error> {
        let mut items = Vec::new();
        while let Some(Strict(item)) = seq.next_element()? {
            items.push(item);
        }
        Ok(Strict(Json::Array(items)))
    }

    fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<Strict, A::Error> {
        let mut object = Map::new();
        while let Some((key, Strict(value))) = map.next_entry::<String, Strict>()? {
            if object.insert(key, value).is_some() {
                return Err(A::Error::custom(MEMBER_TWICE));
            }
        }
        Ok(Strict(Json::Object(object)))
    }
}

/// The JSON object of a route that takes no body, from its query string: `name=value` for a
/// singular field (once), `name=value` repeated for a list, and `name[key]=value` for each
/// entry of a map. A name is the field's JSON name or its proto name, after decoding. A
/// value is a JSON string, which the proto3 JSON mapping reads for numbers, enums and bytes
/// too, but for a bool, whose `true` and `false` are JSON booleans.
fn query_object(
    input: &MessageDescriptor,
    query: &str,
    body: &[u8],
) -> Result<Map<String, Json>, Answer> {
    if !body.is_empty() {
        return Err(Answer::invalid(None, NO_BODY));
    }
    let mut singles = Map::new();
    let mut lists: BTreeMap<String, Vec<Json>> = BTreeMap::new();
    let mut maps: BTreeMap<String, Map<String, Json>> = BTreeMap::new();
    for (key, text) in pairs(query)? {
        let (name, entry) = split_key(&key);
        let Some(field) = lookup(input, name) else {
            return Err(Answer::invalid(Some(key), UNKNOWN_FIELD));
        };
        let json_name = field.json_name().to_owned();
        let entry = entry.map(str::to_owned);
        match (field.is_map(), field.is_list(), entry) {
            (true, _, Some(entry)) => {
                let entries = maps.entry(json_name).or_default();
                if entries
                    .insert(entry, scalar(&map_value_kind(&field), text))
                    .is_some()
                {
                    return Err(Answer::invalid(Some(key), TWICE));
                }
            }
            (false, true, None) => lists
                .entry(json_name)
                .or_default()
                .push(scalar(&field.kind(), text)),
            (false, false, None) => {
                if singles
                    .insert(json_name, scalar(&field.kind(), text))
                    .is_some()
                {
                    return Err(Answer::invalid(Some(key), TWICE));
                }
            }
            _ => return Err(Answer::invalid(Some(key), MALFORMED_KEY)),
        }
    }
    let mut object = singles;
    object.extend(
        lists
            .into_iter()
            .map(|(name, list)| (name, Json::Array(list))),
    );
    object.extend(
        maps.into_iter()
            .map(|(name, map)| (name, Json::Object(map))),
    );
    Ok(object)
}

/// The kind of the values of the map field `field`.
fn map_value_kind(field: &FieldDescriptor) -> Kind {
    field
        .kind()
        .as_message()
        .map_or(Kind::String, |map| map.map_entry_value_field().kind())
}

/// A decoded query key split into a field name and a map key: `labels[team]` is
/// `("labels", Some("team"))`, `labels[]` is `("labels", Some(""))`, `labels[a[b]]` is
/// `("labels", Some("a[b]"))` and `pageSize` is `("pageSize", None)`.
fn split_key(key: &str) -> (&str, Option<&str>) {
    key.strip_suffix(']')
        .and_then(|inner| inner.split_once('['))
        .map_or((key, None), |(name, entry)| (name, Some(entry)))
}

/// The JSON value of query text `text` for a field of kind `kind`.
fn scalar(kind: &Kind, text: String) -> Json {
    match (kind, text.as_str()) {
        (Kind::Bool, "true") => return Json::Bool(true),
        (Kind::Bool, "false") => return Json::Bool(false),
        _ => {}
    }
    Json::String(text)
}

/// The `name=value` pairs of a query string, each side percent-decoded once with `+` read
/// as a space. Empty pairs (`a=1&&b=2`) are skipped; a pair without `=` has an empty value.
/// A name that does not decode is a 400 naming no field; a value that does not, a 400
/// naming its decoded name.
fn pairs(query: &str) -> Result<Vec<(String, String)>, Answer> {
    query
        .split('&')
        .filter(|pair| !pair.is_empty())
        .map(|pair| {
            let (key, value) = pair.split_once('=').unwrap_or((pair, ""));
            let Some(key) = unescape(key, true) else {
                return Err(Answer::invalid(None, NOT_TEXT));
            };
            let Some(value) = unescape(value, true) else {
                return Err(Answer::invalid(Some(key), NOT_TEXT));
            };
            Ok((key, value))
        })
        .collect()
}

/// `text` percent-decoded once, with `+` read as a space when `plus_is_space` (a query, not
/// a path). `None` when a `%` is not followed by two hexadecimal digits or the decoded
/// bytes are not UTF-8.
fn unescape(text: &str, plus_is_space: bool) -> Option<String> {
    let well_formed = text.split('%').skip(1).all(|after| {
        after
            .get(..2)
            .is_some_and(|two| two.bytes().all(|b| b.is_ascii_hexdigit()))
    });
    if !well_formed {
        return None;
    }
    let spaced = if plus_is_space {
        text.replace('+', " ")
    } else {
        text.to_owned()
    };
    percent_decode_str(&spaced)
        .decode_utf8()
        .ok()
        .map(Cow::into_owned)
}

/// Whether `kind` is an integer kind, which the proto3 JSON mapping reads from a JSON
/// number only when it is an exact integer.
fn is_integer(kind: &Kind) -> bool {
    matches!(
        kind,
        Kind::Int32
            | Kind::Sint32
            | Kind::Sfixed32
            | Kind::Int64
            | Kind::Sint64
            | Kind::Sfixed64
            | Kind::Uint32
            | Kind::Fixed32
            | Kind::Uint64
            | Kind::Fixed64
    )
}

/// The first field of `object` (a message of `message`), by its JSON-name path from
/// `prefix`, that decoding would read wrongly, with the reason: a field given under both its
/// names, or an integer field holding a JSON number that is not an exact integer (in a list,
/// the element is named as `name[index]`; in a map, the map field). Keys that name no field
/// are left to the decoding.
fn check(
    message: &MessageDescriptor,
    object: &Map<String, Json>,
    prefix: &str,
) -> Option<(String, &'static str)> {
    let mut seen = BTreeSet::new();
    for (key, item) in object {
        let Some(field) = lookup(message, key) else {
            continue;
        };
        let path = join(prefix, field.json_name());
        if !seen.insert(field.number()) {
            return Some((path, TWICE));
        }
        let found = match item {
            Json::Array(items) if field.is_list() => {
                items.iter().enumerate().find_map(|(index, element)| {
                    check_element(&field.kind(), element, &format!("{path}[{index}]"))
                })
            }
            Json::Object(entries) if field.is_map() => {
                let kind = map_value_kind(&field);
                entries
                    .values()
                    .find_map(|entry| check_element(&kind, entry, &path))
            }
            other => check_element(&field.kind(), other, &path),
        };
        if found.is_some() {
            return found;
        }
    }
    None
}

/// [`check`] of one value of kind `kind` at `path`.
fn check_element(kind: &Kind, item: &Json, path: &str) -> Option<(String, &'static str)> {
    match (kind, item) {
        (Kind::Message(inner), Json::Object(fields)) => check(inner, fields, path),
        (kind, Json::Number(number))
            if is_integer(kind) && !(number.is_i64() || number.is_u64()) =>
        {
            Some((path.to_owned(), WRONG_VALUE))
        }
        _ => None,
    }
}

/// The message of `input` that `object` holds by the proto3 JSON mapping, refusing an
/// unknown field. The 400 names the first offending field ([`locate`]).
fn decode(input: &MessageDescriptor, object: &Map<String, Json>) -> Result<DynamicMessage, Answer> {
    DynamicMessage::deserialize(input.clone(), Json::Object(object.clone()))
        .map_err(|_| Answer::invalid(locate(input, object, ""), WRONG_VALUE))
}

/// Whether the member `key: item` alone decodes as a field of `message`.
fn decodes(message: &MessageDescriptor, key: &str, item: &Json) -> bool {
    let mut single = Map::new();
    single.insert(key.to_owned(), item.clone());
    DynamicMessage::deserialize(message.clone(), Json::Object(single)).is_ok()
}

/// The path, by JSON names from `prefix`, of the first field of `object` (in key order) that
/// does not decode on its own as a field of `message`: an unknown key (named as it was
/// written), or a value of the wrong kind. The search goes on into a singular message field
/// and into each element of a list (`name[index]`), and stops at a map field, whose keys are
/// values of the request. `None` when every field decodes on its own but not all together
/// (two fields of one oneof).
fn locate(message: &MessageDescriptor, object: &Map<String, Json>, prefix: &str) -> Option<String> {
    for (key, item) in object {
        if decodes(message, key, item) {
            continue;
        }
        let Some(field) = lookup(message, key) else {
            return Some(join(prefix, key));
        };
        let path = join(prefix, field.json_name());
        let deeper = match item {
            Json::Array(items) if field.is_list() => {
                first_bad_element(message, key, &field, items, &path)
            }
            _ if field.is_map() => None,
            _ => within(&field.kind(), item, &path),
        };
        return Some(deeper.unwrap_or(path));
    }
    None
}

/// The path of the first element of the list `items` (the value of `key` in `message`) that
/// does not decode, as `path[index]`, or of the field inside it that does not.
fn first_bad_element(
    message: &MessageDescriptor,
    key: &str,
    field: &FieldDescriptor,
    items: &[Json],
    path: &str,
) -> Option<String> {
    items.iter().enumerate().find_map(|(index, element)| {
        if decodes(message, key, &Json::Array(vec![element.clone()])) {
            return None;
        }
        let at = format!("{path}[{index}]");
        Some(within(&field.kind(), element, &at).unwrap_or(at))
    })
}

/// The path inside a message value `item` of kind `kind` at `path` that does not decode.
fn within(kind: &Kind, item: &Json, path: &str) -> Option<String> {
    match (kind, item) {
        (Kind::Message(inner), Json::Object(fields)) => locate(inner, fields, path),
        _ => None,
    }
}

/// The path of the first `job_id` of `message`, outside lists and maps, that is set and is
/// not a job id, by JSON names from `prefix`.
fn first_bad_id(message: &DynamicMessage, prefix: &str) -> Option<String> {
    message.fields().find_map(|(field, value)| {
        let path = join(prefix, field.json_name());
        match value {
            Value::String(text) => (field.name() == "job_id" && !is_job_id(text)).then_some(path),
            Value::Message(inner) => first_bad_id(inner, &path),
            _ => None,
        }
    })
}

/// `name` under `prefix`: `spec.jobId`, or `name` alone at the top.
fn join(prefix: &str, name: &str) -> String {
    if prefix.is_empty() {
        name.to_owned()
    } else {
        format!("{prefix}.{name}")
    }
}
