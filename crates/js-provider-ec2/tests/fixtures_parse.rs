//! The answers of the cloud as its API reference documents them, read by the real client.
//!
//! Every file under `tests/fixtures/api` is the body of one answer. Each test serves
//! them through a scripted transport to [`HttpEc2`], the client the provider runs on, so
//! the parser under test is the one that ships. None of the files is a capture: they are
//! written from the documented response shapes (see `tests/fixtures/README.md`).
//!
//! The last tests check the fixtures themselves: every file is named by a test and listed
//! in the README, and none holds a real address, account or key.

use std::collections::BTreeMap;
use std::fs;
use std::num::NonZeroUsize;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;
use js_caps::{Model, accelerator_of};
use js_elastic::{
    AcquireId, AcquireRequest, Acquired, BlockTerms, BootPayload, CapacityProvider, Held, HostName,
    HostRef, HostStatus, Observed, Offer, PriceUnit, Purchase, Quota, QuotaLimit, Reason, Refusal,
    RefusalCode, Released, SchedulerId,
};
use js_logstore::HttpRequest;
use js_provider_ec2::{
    ApiError, ArchName, BlockOffering, ClientToken, Credentials, DescribeFilter, Ec2Api,
    Ec2Catalog, Ec2CatalogSettings, Ec2Config, Ec2Provider, ErrorCode, GpuInfo, HttpEc2, ImageId,
    Images, Instance, InstanceId, InstanceState, InstanceType, InstanceTypeInfo, LaunchPurchase,
    ManualClock, PurchaseKind, QUOTAS_TARGET, QuotaFamily, Region, RunRequest, StaticKeys, Tag,
    TypePrice, UserData, VcpuQuota, Zone, ZoneOffering, ZonePrice, launch_answer,
};
use js_types::ExecMode;

// The scripted transport the `http` binary shares; this binary uses only part of it.
#[allow(dead_code)]
#[path = "http/script.rs"]
mod script;

/// The region of every fixture.
const REGION: &str = "us-east-1";

/// The scheduler the tags of the instance fixtures name.
const SCHEDULER: &str = "sched-one";

/// The page token the first page of each paged fixture carries.
const TOKEN: &str = "EXAMPLE-PAGE-TOKEN-1";

/// The directory of the fixtures of the API answers.
fn api_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/api")
}

/// The text of the fixture `name`.
fn fixture(name: &str) -> String {
    fs::read_to_string(api_dir().join(name)).unwrap_or_else(|why| panic!("{name}: {why}"))
}

/// The answer with status 200 that serves the fixture `name`.
fn ok(name: &str) -> script::Answer {
    script::answer(&fixture(name))
}

/// The answer with `status` that serves the fixture `name`.
fn failing(status: u16, name: &str) -> script::Answer {
    script::status(status, &fixture(name))
}

/// A client of the fixtures' region with the fixed keys over a script of `answers`.
fn client(answers: Vec<script::Answer>) -> (HttpEc2, Arc<script::Script>) {
    let keys = StaticKeys::new(script::KEY_ID, script::SECRET, Some(script::SESSION));
    let script = script::Script::new(answers);
    let api = HttpEc2::new(
        script::region(REGION),
        Credentials::Keys(keys),
        script.clone(),
        script::clock(),
    );
    (api, script)
}

fn region() -> Region {
    script::region(REGION)
}

/// The value of the form parameter `name` of `request`.
fn param(request: &HttpRequest, name: &str) -> Option<String> {
    script::param(request, name)
}

/// The refusal an acquire gets from the cloud's error code `code`.
fn refused_launch(constant: RefusalCode, code: &str) -> Acquired {
    Acquired::Refused(Refusal {
        code: constant,
        reason: Reason::Api(code.to_owned()),
    })
}

fn refused(code: &str) -> ApiError {
    ApiError::Refused {
        code: ErrorCode::new(code),
    }
}

fn denied(code: &str) -> ApiError {
    ApiError::Denied {
        code: ErrorCode::new(code),
    }
}

fn throttled(code: &str) -> ApiError {
    ApiError::Throttled {
        code: ErrorCode::new(code),
    }
}

fn names(list: &[&str]) -> Vec<ArchName> {
    list.iter()
        .map(|name| ArchName((*name).to_owned()))
        .collect()
}

fn typed(name: &str) -> InstanceType {
    InstanceType(name.to_owned())
}

fn zone(name: &str) -> Zone {
    Zone(name.to_owned())
}

#[path = "fixtures_parse/catalog.rs"]
mod catalog;
#[path = "fixtures_parse/errors.rs"]
mod errors;
#[path = "fixtures_parse/hygiene.rs"]
mod hygiene;
#[path = "fixtures_parse/launch.rs"]
mod launch;
#[path = "fixtures_parse/provider.rs"]
mod provider;
