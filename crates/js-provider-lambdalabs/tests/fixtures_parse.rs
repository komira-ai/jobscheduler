//! Every file of `tests/fixtures` through the same client the provider runs on. Each
//! answer is served by the loopback server of `tests/http/support.rs` to a real
//! `HttpApi`, so the bytes go through the crate's own parsers; nothing here parses them a
//! second time. The results are asserted field by field, errors by their exact variant and
//! by what the provider makes of them at launch, and two checks keep the directory honest:
//! every file is named by a test and carries only placeholder values.
//!
//! None of the files is a recording. They are written from the Lambda Cloud API reference
//! (`tests/fixtures/README.md` says so, file by file).

// The support module serves other test binaries too; this one uses a part of it.
#[allow(dead_code)]
#[path = "http/support.rs"]
mod support;

use std::collections::BTreeSet;
use std::fs;
use std::num::NonZeroUsize;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use js_elastic::{
    AcquireId, AcquireRequest, Acquired, BootPayload, CapacityProvider, Held, HostName, HostRef,
    HostStatus, Observed, Offer, PriceUnit, ProviderError, Purchase, Reason, Refusal, RefusalCode,
    SchedulerId,
};
use js_provider_lambdalabs::{
    ApiError, ApiKey, Architecture, ElasticConfig, ErrorCode, FirewallRuleset, HttpApi, Instance,
    InstanceId, InstanceTypeOffer, LambdaApi, LambdaProvider, LaunchRequest, ManualClock,
    OptionTable, Region, SshKey, Sweep, Tag,
};
use js_types::{ExecMode, FormFactor, Model, accelerator_of};
use support::{PATIENT, Reply, Scratch, Server, fixture};
use time::{Date, Month, OffsetDateTime};

#[path = "fixtures_parse/directory.rs"]
mod directory;
#[path = "fixtures_parse/errors.rs"]
mod errors;
#[path = "fixtures_parse/instance_types.rs"]
mod instance_types;
#[path = "fixtures_parse/instances.rs"]
mod instances;

const TYPES: &str = "GET /api/v1/instance-types";
const LIST: &str = "GET /api/v1/instances";
const LAUNCH: &str = "POST /api/v1/instance-operations/launch";
const TERMINATE: &str = "POST /api/v1/instance-operations/terminate";
const RULESETS: &str = "GET /api/v1/firewall-rulesets";
const MAKE_RULESET: &str = "POST /api/v1/firewall-rulesets";
const KEYS: &str = "GET /api/v1/ssh-keys";
const MAKE_KEY: &str = "POST /api/v1/ssh-keys";

const FIRST: &str = "a1b2c3d4e5f60718293a4b5c6d7e8f90";
const LAUNCHED: &str = "c0ffee00c0ffee00c0ffee00c0ffee01";

fn one(id: &str) -> String {
    format!("GET /api/v1/instances/{id}")
}

fn id(text: &str) -> InstanceId {
    InstanceId::new(text).unwrap()
}

/// A real client of `server`, holding a key made for the test.
fn connect(server: &Server) -> HttpApi {
    let scratch = Scratch::new("fixtures-parse");
    let key = ApiKey::from_file(&scratch.key_file("key-one")).unwrap();
    HttpApi::connect(&server.url("http"), &key, None, PATIENT).unwrap()
}

/// A server that answers `route` with the fixture `file`, status 200.
fn serving(route: &str, file: &str) -> Server {
    let server = Server::plain();
    server.on(route, Reply::json(200, &fixture(file)));
    server
}

fn region(name: &str, description: &str) -> Region {
    Region {
        name: name.to_owned(),
        description: description.to_owned(),
    }
}

fn tag(key: &str, value: &str) -> Tag {
    Tag {
        key: key.to_owned(),
        value: value.to_owned(),
    }
}

/// An instant in UTC, to the millisecond.
fn at(year: i32, month: Month, day: u8, hms: (u8, u8, u8), milli: u16) -> OffsetDateTime {
    Date::from_calendar_date(year, month, day)
        .unwrap()
        .with_hms_milli(hms.0, hms.1, hms.2, milli)
        .unwrap()
        .assume_utc()
}

/// A provider for `scheduler` on the real client against `server`, live, with room for
/// `max_machines`, holding the allow list `table`.
fn provider(server: &Server, table: &str, max_machines: usize) -> LambdaProvider {
    let key = ApiKey::from_file(&Scratch::new("fixtures-provider").key_file("key-one")).unwrap();
    let api = HttpApi::connect(&server.url("http"), &key, None, PATIENT).unwrap();
    let config = ElasticConfig::new(
        SchedulerId::new("sched-one").unwrap(),
        "fixture-key",
        NonZeroUsize::new(max_machines).unwrap(),
    )
    .unwrap()
    .enable()
    .live();
    LambdaProvider::new(
        Arc::new(api),
        Arc::new(ManualClock::new()),
        OptionTable::parse(table).unwrap(),
        config,
    )
}

fn rows(types: &[&str]) -> String {
    types
        .iter()
        .map(|t| format!("[[option]]\ninstance_type = \"{t}\"\n"))
        .collect()
}

const SCHEDULER: &str = "sched-one";

fn scheduler() -> SchedulerId {
    SchedulerId::new(SCHEDULER).unwrap()
}

fn acquire(text: &str) -> AcquireId {
    AcquireId::new(text).unwrap()
}

fn request() -> LaunchRequest {
    LaunchRequest {
        region: "region-a".to_owned(),
        instance_type: "gpu_1x_h100_sxm5".to_owned(),
        ssh_key: "fixture-key".to_owned(),
        name: "js-0123456789ab-000000000001".to_owned(),
        tags: vec![tag("jobscheduler:pool", "fixture")],
        user_data: None,
        firewall_ruleset_ids: Vec::new(),
    }
}
