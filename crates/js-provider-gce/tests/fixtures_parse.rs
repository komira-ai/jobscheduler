//! The documented answers of the Compute Engine and Cloud Billing APIs, one file each in
//! `tests/fixtures/api`, read by the crate's own client: the public `HttpGce` over a
//! scripted transport sends each call and parses the file's bytes with the code that parses
//! a live answer. Nothing here copies a parser.
//!
//! The files are written from the providers' published response shapes, not captured from
//! an account (`tests/fixtures/README.md` says so per file and how a real capture replaces
//! one). Two checks keep the directory honest: every file is named by a test, and no file
//! carries an address, account number or key id that is not a placeholder.

use std::fs;
use std::path::Path;
use std::sync::Arc;

use js_elastic::{
    AcquireId, Acquired, HostStatus, Offer, PriceUnit, Purchase, Quota, QuotaLimit, Reason,
    Refusal, RefusalCode, SchedulerId,
};
use js_logstore::HttpRequest;
use js_provider_gce::{
    AcceleratorTypeInfo, AcceleratorTypeName, ApiError, ArchName, AttachedAccelerator,
    CatalogAnswer, ErrorReason, GceApi, GceCatalog, GceCatalogSettings, HostMaintenance, ImagePath,
    InsertRequest, Instance, InstanceId, InstanceKey, InstanceName, InstanceRef, InstanceStatus,
    Label, ListFilter, MachineTypeInfo, MachineTypeName, Money, NetworkInterface, Operation,
    OperationRef, OperationStatus, Payload, PricedItem, Project, ProvisioningModel, QuotaMetric,
    RequestId, Scheduling, SeriesName, SkuPrice, UsageType, Zone, launch_answer,
};

// The scripted transport the `http` binary shares; this binary uses only part of it.
#[allow(dead_code)]
#[path = "http/script.rs"]
mod script;

use script::{Answer, answer, api_calls, over, status};

/// The text of the fixture `name` in `tests/fixtures/api`, compiled in.
macro_rules! fixture {
    ($name:literal) => {
        include_str!(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/tests/fixtures/api/",
            $name
        ))
    };
}

const PROJECT: &str = "example-project";
const COMPUTE: &str = "https://compute.googleapis.com/compute/v1/projects/example-project";
const SKUS_URL: &str = "https://cloudbilling.googleapis.com/v1/services/6F81-5844-456A/skus";
const ZONE_A: &str = "us-central1-a";
const ZONE_B: &str = "us-central1-b";
const ZONE_F: &str = "us-central1-f";
const INSTANCE_ID: &str = "1234567890123456789";
const INSTANCE_NAME: &str = "js-0123456789ab-fedcba987654";
const OPERATION: &str = "operation-1700000000000-6000000000001-00000001-0000a001";

fn project() -> Project {
    Project::new(PROJECT).unwrap()
}

fn zone(name: &str) -> Zone {
    Zone::new(name).unwrap()
}

fn region() -> js_provider_gce::Region {
    zone(ZONE_A).region()
}

/// The URL without its query, and the query as sorted, percent-decoded pairs.
fn split(request: &HttpRequest) -> (&str, Vec<(String, String)>) {
    let (base, query) = request
        .url
        .split_once('?')
        .unwrap_or((request.url.as_str(), ""));
    let mut pairs: Vec<(String, String)> = query
        .split('&')
        .filter(|pair| !pair.is_empty())
        .map(|pair| {
            let (name, value) = pair.split_once('=').unwrap();
            (decode(name), decode(value))
        })
        .collect();
    pairs.sort();
    (base, pairs)
}

/// `text` with its `%XX` escapes decoded.
fn decode(text: &str) -> String {
    let bytes = text.as_bytes();
    let mut out = Vec::new();
    let mut at = 0;
    while at < bytes.len() {
        if bytes[at] == b'%' {
            let hex = std::str::from_utf8(&bytes[at + 1..at + 3]).unwrap();
            out.push(u8::from_str_radix(hex, 16).unwrap());
            at += 3;
        } else {
            out.push(bytes[at]);
            at += 1;
        }
    }
    String::from_utf8(out).unwrap()
}

/// `pairs` as sorted owned pairs, to compare with a request's query.
fn query(pairs: &[(&str, &str)]) -> Vec<(String, String)> {
    let mut owned: Vec<(String, String)> = pairs
        .iter()
        .map(|(name, value)| ((*name).to_owned(), (*value).to_owned()))
        .collect();
    owned.sort();
    owned
}

/// The URLs (without query) and the sorted queries of every request to the API.
fn calls(script: &script::Script) -> Vec<(String, Vec<(String, String)>)> {
    api_calls(script)
        .iter()
        .map(|request| {
            let (base, pairs) = split(request);
            (base.to_owned(), pairs)
        })
        .collect()
}

fn info(
    name: &str,
    zone_name: &str,
    vcpus: u32,
    memory_mib: u64,
    accelerators: &[(&str, u32)],
    arch: Option<&str>,
) -> MachineTypeInfo {
    MachineTypeInfo {
        name: MachineTypeName(name.to_owned()),
        zone: zone(zone_name),
        vcpus,
        memory_mib,
        accelerators: accelerators
            .iter()
            .map(|(type_name, count)| AttachedAccelerator {
                type_name: AcceleratorTypeName((*type_name).to_owned()),
                count: *count,
            })
            .collect(),
        arch: arch.map(|name| ArchName(name.to_owned())),
    }
}

/// `rows` ordered by name and zone, which is not the order the cloud lists them in.
fn by_name_and_zone(mut rows: Vec<MachineTypeInfo>) -> Vec<MachineTypeInfo> {
    rows.sort_by(|a, b| (&a.name, &a.zone).cmp(&(&b.name, &b.zone)));
    rows
}

fn machine_pages() -> Vec<Answer> {
    vec![
        answer(fixture!("machine_types_aggregated_page1.json")),
        answer(fixture!("machine_types_aggregated_page2.json")),
    ]
}

fn sku_pages() -> Vec<Answer> {
    vec![
        answer(fixture!("billing_skus_page1.json")),
        answer(fixture!("billing_skus_page2.json")),
    ]
}

#[path = "fixtures_parse/catalog.rs"]
mod catalog;
#[path = "fixtures_parse/errors.rs"]
mod errors;
#[path = "fixtures_parse/hygiene.rs"]
mod hygiene;
#[path = "fixtures_parse/launch.rs"]
mod launch;
