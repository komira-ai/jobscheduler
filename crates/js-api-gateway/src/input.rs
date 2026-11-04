//! A call's request message, made from an HTTP request: the path variables, then either the
//! JSON body (a route whose body is the request) or the query string (any other route),
//! decoded by the proto3 JSON mapping from the request's descriptor.

use std::borrow::Cow;
use std::collections::BTreeMap;

use percent_encoding::percent_decode_str;
use prost_reflect::{DynamicMessage, Kind, MessageDescriptor, Value};
use serde_json::{Map, Value as Json};

use crate::answer::Answer;

/// The most bytes in a job id.
const MAX_JOB_ID: usize = 63;

/// The reason of a 400 for a body that is not JSON.
const NOT_JSON: &str = "the body is not valid JSON";
/// The reason of a 400 for a body that is JSON but not an object.
const NOT_AN_OBJECT: &str = "the body is not a JSON object";
/// The reason of a 400 for a body sent to a route that takes none.
const NO_BODY: &str = "this route takes no body; its fields are set by the path and the query";
/// The reason of a 400 for a query string sent to a route whose body is the request.
const NO_QUERY: &str = "this route takes its fields in the body, not in the query";
/// The reason of a 400 for a query string that does not decode as UTF-8 text.
const NOT_TEXT: &str = "the query string does not decode as UTF-8 text";
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
/// text), its `query` string and its `body`. `takes_body` says the route's body is the
/// request; otherwise the query sets the fields and the body must be empty.
///
/// Checks, in order, each answering a 400 that names the field when one is to blame: the
/// path variables against their grammar; the body (JSON object; an empty body is `{}`) or
/// the query; the decoding by the proto3 JSON mapping (an unknown field, a value of the
/// wrong kind); every `job_id` the message holds outside lists and maps; and
/// no field the path sets set again.
pub(crate) fn build(
    input: &MessageDescriptor,
    takes_body: bool,
    variables: &[(String, String)],
    query: Option<&str>,
    body: &[u8],
) -> Result<DynamicMessage, Answer> {
    let mut path = Vec::new();
    for (name, value) in variables {
        let field = input.get_field_by_name(name).ok_or_else(Answer::internal)?;
        if !path_value_ok(name, value) {
            return Err(Answer::invalid(Some(field.json_name().to_owned()), BAD_ID));
        }
        path.push((field, value));
    }
    let query = query.unwrap_or_default();
    let object = if takes_body {
        body_object(query, body)?
    } else {
        query_object(input, query, body)?
    };
    let mut message = decode(input, &object)?;
    if let Some(field) = first_bad_id(&message, "") {
        return Err(Answer::invalid(Some(field), BAD_ID));
    }
    for (field, value) in path {
        if message.has_field(&field) {
            return Err(Answer::invalid(
                Some(field.json_name().to_owned()),
                SET_BY_PATH,
            ));
        }
        message.set_field(&field, Value::String(value.clone()));
    }
    Ok(message)
}

/// The JSON object of a route whose body is the request. Its query string must be empty.
fn body_object(query: &str, body: &[u8]) -> Result<Map<String, Json>, Answer> {
    if let Some((key, _)) = pairs(query)?.into_iter().next() {
        return Err(Answer::invalid(Some(key), NO_QUERY));
    }
    if body.is_empty() {
        return Ok(Map::new());
    }
    match serde_json::from_slice::<Json>(body) {
        Ok(Json::Object(object)) => Ok(object),
        Ok(_) => Err(Answer::invalid(None, NOT_AN_OBJECT)),
        Err(_) => Err(Answer::invalid(None, NOT_JSON)),
    }
}

/// The JSON object of a route that takes no body, from its query string: `name=value` for a
/// singular field (once), `name=value` repeated for a list, and `name[key]=value` for each
/// entry of a map. A name is the field's JSON name or its proto name. A value is a JSON
/// string, which the proto3 JSON mapping reads for numbers, enums and bytes too, but for a
/// bool, whose `true` and `false` are JSON booleans.
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
        let Some(field) = input
            .get_field_by_json_name(name)
            .or_else(|| input.get_field_by_name(name))
        else {
            return Err(Answer::invalid(Some(key), UNKNOWN_FIELD));
        };
        let json_name = field.json_name().to_owned();
        let entry = entry.map(str::to_owned);
        match (field.is_map(), field.is_list(), entry) {
            (true, _, Some(entry)) => {
                let kind = field
                    .kind()
                    .as_message()
                    .map_or(Kind::String, |map| map.map_entry_value_field().kind());
                let entries = maps.entry(json_name).or_default();
                if entries.insert(entry, scalar(&kind, text)).is_some() {
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

/// A query key split into a field name and a map key: `labels[team]` is `("labels",
/// Some("team"))`, `pageSize` is `("pageSize", None)`.
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

/// The `name=value` pairs of a query string, each side percent-decoded with `+` read as a
/// space. Empty pairs (`a=1&&b=2`) are skipped; a pair without `=` has an empty value.
fn pairs(query: &str) -> Result<Vec<(String, String)>, Answer> {
    query
        .split('&')
        .filter(|pair| !pair.is_empty())
        .map(|pair| {
            let (key, value) = pair.split_once('=').unwrap_or((pair, ""));
            Ok((unescape(key)?, unescape(value)?))
        })
        .collect()
}

/// One side of a query pair, decoded.
fn unescape(text: &str) -> Result<String, Answer> {
    let spaced = text.replace('+', " ");
    percent_decode_str(&spaced)
        .decode_utf8()
        .map(Cow::into_owned)
        .map_err(|_| Answer::invalid(None, NOT_TEXT))
}

/// The message of `input` that `object` holds by the proto3 JSON mapping, refusing an
/// unknown field. The 400 names the first offending field ([`locate`]).
fn decode(input: &MessageDescriptor, object: &Map<String, Json>) -> Result<DynamicMessage, Answer> {
    DynamicMessage::deserialize(input.clone(), Json::Object(object.clone()))
        .map_err(|_| Answer::invalid(locate(input, object, ""), WRONG_VALUE))
}

/// The path, by JSON names from `prefix`, of the first field of `object` (in key order) that
/// does not decode on its own as a field of `message`: an unknown key, or a value of the
/// wrong kind. Inside a singular message field the search goes on into the message, and
/// names the message field itself when every field inside decodes on its own. `None` when
/// every field decodes on its own but not all together (the two names of one field of a
/// oneof).
fn locate(message: &MessageDescriptor, object: &Map<String, Json>, prefix: &str) -> Option<String> {
    for (key, item) in object {
        let mut single = Map::new();
        single.insert(key.clone(), item.clone());
        if DynamicMessage::deserialize(message.clone(), Json::Object(single)).is_ok() {
            continue;
        }
        let path = join(prefix, key);
        let field = message
            .get_field_by_json_name(key)
            .or_else(|| message.get_field_by_name(key));
        return match (field.map(|field| (field.kind(), field.is_map())), item) {
            (Some((Kind::Message(inner), false)), Json::Object(fields)) => {
                locate(&inner, fields, &path).or(Some(path))
            }
            _ => Some(path),
        };
    }
    None
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
