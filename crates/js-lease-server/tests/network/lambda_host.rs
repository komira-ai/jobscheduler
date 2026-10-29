//! The script a Lambda machine runs: the service's renderer over the Lambda provider's own
//! host steps. The provider and the renderer are written apart; this is where they meet.

use std::num::NonZeroUsize;
use std::sync::Arc;

use js_elastic::{ElasticProvider, GpuClass, Offer, PriceUnit, SchedulerId};
use js_lease_server::bootstrap::{Fill, render};
use js_lease_server::network::{DefaultMode, NetworkMode};
use js_provider_lambdalabs::{ElasticConfig, FakeLambda, LambdaProvider, ManualClock, OptionTable};
use js_types::ExecMode;

use crate::support::{TEN_MINUTES, ctx, host, plan};

fn provider() -> LambdaProvider {
    let config = ElasticConfig::new(
        SchedulerId::new("sched").unwrap(),
        "a-key",
        NonZeroUsize::MIN,
    )
    .unwrap();
    let table = OptionTable::parse(
        "[[option]]\ninstance_type = \"gpu_1x_a10\"\ngpu_class = \"nvidia-a10\"\ngpus = 1\n",
    )
    .unwrap();
    LambdaProvider::new(
        Arc::new(FakeLambda::new()),
        Arc::new(ManualClock::new()),
        table,
        config,
    )
}

fn offer(disk_gib: u64) -> Offer {
    Offer {
        id: "gpu_1x_a10".to_owned(),
        zone: Some("us-west-1".to_owned()),
        cpu_millis: 30_000,
        memory_bytes: 200 << 30,
        disk_bytes: disk_gib << 30,
        gpus: 1,
        gpu_class: Some(GpuClass::parse("nvidia-a10").unwrap()),
        os: "linux".to_owned(),
        arch: "x86_64".to_owned(),
        exec: ExecMode::Container,
        price_cents_per_hour: Some(129),
        price_unit: PriceUnit::UsCents,
        available: true,
    }
}

fn script(disk_gib: u64) -> String {
    let host = host();
    let mut plan = plan(&host);
    plan.host_prep = provider().host_prep(&offer(disk_gib));
    plan.network = DefaultMode::new("https://sched.example.org:7301", Vec::new())
        .describe(&ctx(&host, TEN_MINUTES));
    render(&plan, Fill::Mask)
        .unwrap()
        .replace(plan.host.as_str(), "<HOST>")
}

/// Asserts the whole masked script of a Lambda machine on a 512 GiB disk against a file:
/// the provider's steps come first, before the join token is written, and the agent's unit
/// that follows requires the scratch mount the steps made, at the path they made it.
#[test]
fn the_lambda_script_matches_its_golden_file() {
    assert_eq!(
        script(512),
        include_str!("golden/lambda_default_masked.sh"),
        "the lambda script changed"
    );
}

/// Asserts that on a disk too small for the scratch image and the reserve the script
/// logs why and fails before the join token is written, so the agent never starts.
#[test]
fn a_disk_too_small_fails_the_script_before_the_token_is_written() {
    let text = script(100);
    let lines: Vec<&str> = text.lines().collect();
    let failed = lines.iter().position(|line| *line == "false").unwrap();
    let identity = lines.iter().position(|line| *line == "# identity").unwrap();
    assert!(failed < identity, "{text}");
    assert!(lines[failed - 1].starts_with("logger -p user.err -t js-bootstrap "));
    assert!(!lines.iter().any(|line| line.starts_with("mount ")));
}
