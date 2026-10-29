//! The JSON the API sends, reduced to the fields this crate uses. Unknown fields are
//! ignored, so the API may add fields; a field the documentation marks optional may be
//! missing.

use std::collections::BTreeMap;

use js_elastic::{HostStatus, Secret};
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use time::OffsetDateTime;
use time::format_description::well_known::Rfc3339;

use crate::error::{ApiError, ErrorCode, plain};
use crate::model::{
    Architecture, FirewallRuleset, Instance, InstanceId, InstanceTypeOffer, LaunchRequest, Region,
    SshKey, Tag,
};

/// The longest `request_id` kept.
const REQUEST_ID: usize = 80;

#[derive(Deserialize)]
struct Envelope<T> {
    data: T,
    #[serde(default)]
    page_token: Option<String>,
}

#[derive(Deserialize)]
struct WireRegion {
    name: String,
    #[serde(default)]
    description: String,
}

#[derive(Deserialize)]
struct WireSpecs {
    vcpus: u32,
    memory_gib: u32,
    storage_gib: u64,
    gpus: u32,
}

#[derive(Deserialize)]
struct WireType {
    name: String,
    #[serde(default)]
    description: String,
    #[serde(default)]
    gpu_description: String,
    price_cents_per_hour: u64,
    specs: WireSpecs,
    #[serde(default)]
    architecture: String,
}

#[derive(Deserialize)]
struct WireOffer {
    instance_type: WireType,
    #[serde(default)]
    regions_with_capacity_available: Vec<WireRegion>,
}

#[derive(Deserialize)]
struct WireName {
    name: String,
}

#[derive(Deserialize)]
struct WireTag {
    key: String,
    value: String,
}

#[derive(Deserialize)]
struct WireInstance {
    id: String,
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    ip: Option<String>,
    #[serde(default)]
    private_ip: Option<String>,
    status: String,
    region: WireName,
    instance_type: WireName,
    #[serde(default)]
    hostname: Option<String>,
    #[serde(default)]
    first_healthy: Option<String>,
    #[serde(default)]
    tags: Option<Vec<WireTag>>,
}

#[derive(Deserialize)]
struct ErrorEnvelope {
    error: ErrorBody,
}

#[derive(Deserialize)]
struct ErrorBody {
    code: String,
    #[serde(default)]
    request_id: Option<String>,
}

/// Reads a body. The error is fixed text and a position, never the parser's message: a
/// type error quotes the value it met, and the answer to a key made by the API holds a
/// private key.
fn decode<T: DeserializeOwned>(body: &[u8]) -> Result<Envelope<T>, ApiError> {
    serde_json::from_slice(body).map_err(|err| {
        ApiError::Malformed(format!(
            "unexpected answer shape (line {}, column {})",
            err.line(),
            err.column()
        ))
    })
}

/// The instance types of a `GET /instance-types` body, ordered by name.
pub(crate) fn offers(body: &[u8]) -> Result<Vec<InstanceTypeOffer>, ApiError> {
    let found: Envelope<BTreeMap<String, WireOffer>> = decode(body)?;
    let mut offers: Vec<InstanceTypeOffer> = found.data.into_values().map(offer).collect();
    offers.sort_by(|a, b| a.name.cmp(&b.name));
    Ok(offers)
}

fn offer(wire: WireOffer) -> InstanceTypeOffer {
    let kind = wire.instance_type;
    InstanceTypeOffer {
        name: kind.name,
        description: kind.description,
        gpu_description: kind.gpu_description,
        price_cents_per_hour: kind.price_cents_per_hour,
        vcpus: kind.specs.vcpus,
        memory_gib: kind.specs.memory_gib,
        storage_gib: kind.specs.storage_gib,
        gpus: kind.specs.gpus,
        architecture: match kind.architecture.as_str() {
            "x86_64" => Architecture::X86_64,
            "arm64" => Architecture::Arm64,
            other => Architecture::Other(plain(other, 40)),
        },
        regions: wire
            .regions_with_capacity_available
            .into_iter()
            .map(|region| Region {
                name: region.name,
                description: region.description,
            })
            .collect(),
    }
}

/// The instances of a `GET /instances` body. A page token means the answer is one page of
/// several: [`ApiError::Incomplete`], never a short list.
pub(crate) fn instances(body: &[u8]) -> Result<Vec<Instance>, ApiError> {
    let found: Envelope<Vec<WireInstance>> = decode(body)?;
    if found.page_token.is_some_and(|token| !token.is_empty()) {
        return Err(ApiError::Incomplete);
    }
    found.data.into_iter().map(instance).collect()
}

/// The instance of a `GET /instances/{id}` body.
pub(crate) fn one_instance(body: &[u8]) -> Result<Instance, ApiError> {
    let found: Envelope<WireInstance> = decode(body)?;
    instance(found.data)
}

fn instance(wire: WireInstance) -> Result<Instance, ApiError> {
    let id = InstanceId::new(&wire.id)
        .map_err(|_| ApiError::Malformed("an instance id of an unexpected form".to_owned()))?;
    Ok(Instance {
        id,
        name: wire.name,
        status: HostStatus::parse(&wire.status),
        region: wire.region.name,
        instance_type: wire.instance_type.name,
        ip: wire.ip,
        private_ip: wire.private_ip,
        hostname: wire.hostname,
        first_healthy: wire
            .first_healthy
            .and_then(|text| OffsetDateTime::parse(&text, &Rfc3339).ok()),
        tags: wire
            .tags
            .unwrap_or_default()
            .into_iter()
            .map(|tag| Tag {
                key: tag.key,
                value: tag.value,
            })
            .collect(),
    })
}

/// The code and request id of an error body; a body that is not one reads as
/// [`ErrorCode::Unparsed`]. The `message` and `suggestion` are not read.
pub(crate) fn error_parts(body: &[u8]) -> (ErrorCode, Option<String>) {
    match serde_json::from_slice::<ErrorEnvelope>(body) {
        Ok(found) => (
            ErrorCode::parse(&found.error.code),
            found.error.request_id.map(|id| plain(&id, REQUEST_ID)),
        ),
        Err(_) => (ErrorCode::Unparsed, None),
    }
}

#[derive(Serialize)]
struct WireTagOut<'a> {
    key: &'a str,
    value: &'a str,
}

#[derive(Serialize)]
struct WireRulesetRef<'a> {
    id: &'a str,
}

#[derive(Serialize)]
struct LaunchBody<'a> {
    region_name: &'a str,
    instance_type_name: &'a str,
    ssh_key_names: [&'a str; 1],
    name: &'a str,
    tags: Vec<WireTagOut<'a>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    user_data: Option<&'a str>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    firewall_rulesets: Vec<WireRulesetRef<'a>>,
}

#[derive(Serialize)]
struct TerminateBody<'a> {
    instance_ids: [&'a str; 1],
}

#[derive(Deserialize)]
struct Launched {
    instance_ids: Vec<String>,
}

#[derive(Deserialize)]
struct Terminated {
    #[serde(default)]
    terminated_instances: Vec<WireId>,
}

#[derive(Deserialize)]
struct WireId {
    id: String,
}

/// The body of a launch call.
pub(crate) fn launch_body(request: &LaunchRequest) -> Vec<u8> {
    let body = LaunchBody {
        region_name: &request.region,
        instance_type_name: &request.instance_type,
        ssh_key_names: [&request.ssh_key],
        name: &request.name,
        tags: request
            .tags
            .iter()
            .map(|tag| WireTagOut {
                key: &tag.key,
                value: &tag.value,
            })
            .collect(),
        user_data: request.user_data.as_ref().map(Secret::expose),
        firewall_rulesets: request
            .firewall_ruleset_ids
            .iter()
            .map(|id| WireRulesetRef { id })
            .collect(),
    };
    // A struct of strings always serializes.
    serde_json::to_vec(&body).unwrap_or_default()
}

/// The body of a terminate call for one instance.
pub(crate) fn terminate_body(id: &InstanceId) -> Vec<u8> {
    let body = TerminateBody {
        instance_ids: [id.as_str()],
    };
    serde_json::to_vec(&body).unwrap_or_default()
}

/// The ids of a launch answer.
pub(crate) fn launched(body: &[u8]) -> Result<Vec<InstanceId>, ApiError> {
    let found: Envelope<Launched> = decode(body)?;
    ids(found.data.instance_ids)
}

/// The ids a terminate answer says were terminated.
pub(crate) fn terminated(body: &[u8]) -> Result<Vec<InstanceId>, ApiError> {
    let found: Envelope<Terminated> = decode(body)?;
    ids(found.data.terminated_instances.into_iter().map(|w| w.id))
}

fn ids(found: impl IntoIterator<Item = String>) -> Result<Vec<InstanceId>, ApiError> {
    found
        .into_iter()
        .map(|id| {
            InstanceId::new(&id)
                .map_err(|_| ApiError::Malformed("an instance id of an unexpected form".to_owned()))
        })
        .collect()
}

#[derive(Deserialize)]
struct WireRuleset {
    id: String,
    name: String,
    region: WireName,
    // No default: a ruleset that does not say what it holds is not read as empty.
    rules: Vec<serde::de::IgnoredAny>,
}

#[derive(Deserialize)]
struct WireKey {
    id: String,
    name: String,
}

#[derive(Serialize)]
struct RulesetBody<'a> {
    name: &'a str,
    region: &'a str,
    rules: [(); 0],
}

#[derive(Serialize)]
struct KeyBody<'a> {
    name: &'a str,
}

fn ruleset(wire: WireRuleset) -> FirewallRuleset {
    FirewallRuleset {
        id: wire.id,
        name: wire.name,
        region: wire.region.name,
        rules: wire.rules.len(),
    }
}

/// The rulesets of a `GET /firewall-rulesets` body.
pub(crate) fn rulesets(body: &[u8]) -> Result<Vec<FirewallRuleset>, ApiError> {
    let found: Envelope<Vec<WireRuleset>> = decode(body)?;
    Ok(found.data.into_iter().map(ruleset).collect())
}

/// The ruleset of a `POST /firewall-rulesets` body.
pub(crate) fn one_ruleset(body: &[u8]) -> Result<FirewallRuleset, ApiError> {
    let found: Envelope<WireRuleset> = decode(body)?;
    Ok(ruleset(found.data))
}

/// The body of a create-ruleset call: a name, a region and no rules.
pub(crate) fn ruleset_body(region: &str, name: &str) -> Vec<u8> {
    let body = RulesetBody {
        name,
        region,
        rules: [],
    };
    serde_json::to_vec(&body).unwrap_or_default()
}

/// The keys of a `GET /ssh-keys` body.
pub(crate) fn keys(body: &[u8]) -> Result<Vec<SshKey>, ApiError> {
    let found: Envelope<Vec<WireKey>> = decode(body)?;
    Ok(found.data.into_iter().map(key).collect())
}

/// The key of a `POST /ssh-keys` body. The answer of a made key pair carries the private
/// half too; no field of it is read, so it is not kept.
pub(crate) fn one_key(body: &[u8]) -> Result<SshKey, ApiError> {
    let found: Envelope<WireKey> = decode(body)?;
    Ok(key(found.data))
}

fn key(wire: WireKey) -> SshKey {
    SshKey {
        id: wire.id,
        name: wire.name,
    }
}

/// The body of an add-key call: a name and no public key, which has the API make a pair.
pub(crate) fn key_body(name: &str) -> Vec<u8> {
    serde_json::to_vec(&KeyBody { name }).unwrap_or_default()
}
