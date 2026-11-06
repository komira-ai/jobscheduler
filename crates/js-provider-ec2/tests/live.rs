//! The live conformance suite of the EC2 provider: the cases of
//! `js_elastic::conformance::live` run against the real EC2, Pricing and Service Quotas
//! APIs, through the provider the `ec2` kind builds.
//!
//! It reads the catalog, chooses one offer, launches three instances one at a time (each
//! released before the next), and ends with a cleanup pass that terminates whatever the
//! run's scheduler still holds. It is a dry run unless it is given `--spend` and the plan
//! hash a dry run printed: a dry run launches nothing and spends nothing, and a spend run
//! refuses when the plan it computes now hashes to another value. Run it through
//! `tools/held/provider-live/provider_live.sh` or the `provider-live` workflow, as
//! `docs/runbooks/live-conformance.md` says; it is built only with the cargo feature `live`.
//!
//! ```text
//! --run-id ID            required: 1 to 40 characters of a-z, 0-9 and '-'; names the scheduler
//!                        `live-<ID>` and the label `validation-run` every instance carries
//! --region REGION        required: the region to launch in, such as us-east-1
//! --image AMI            required: the machine image, ami- and lower-case hexadecimal digits
//! --arch ARCH            x86_64 (the default) or arm64: the architecture `--image` boots; only
//!                        offers of that architecture are chosen
//! --shape TYPE           required: the instance type to launch, such as t3.micro; the
//!                        catalog is read for this type alone. It is required because with
//!                        no type allowed the provider's catalog holds only the types with
//!                        a GPU, and the cheapest of those is no test machine. A name with
//!                        no '/' is read as the on-demand offer of the type, t3.micro as
//!                        t3.micro/on-demand, which is how the catalog names it
//! --capture-dir DIR      write the body of each answer of the EC2, Pricing and Service Quotas
//!                        APIs to a file in DIR (requests, which carry signatures, are never
//!                        written); DIR is created if it is missing
//! --spend --plan-hash H  launch for real; H is the plan hash a dry run printed
//! ```
//!
//! The credentials come from the provider's usual chain, never from a flag: the variables
//! `AWS_ACCESS_KEY_ID`, `AWS_SECRET_ACCESS_KEY` and `AWS_SESSION_TOKEN` when they are set,
//! else the instance role of the machine it runs on. The exit status is 0 for a pass, 1 for
//! a failed case, a left-over instance or a changed plan, and 2 for a usage error.
//!
//! This file is only the `main` of the suite. The flags' account, its provider table and the
//! recording transport are in `tests/live/cloud.rs`, which `tests/live_cloud.rs` tests
//! without a network.

use std::io::Write;

use js_elastic::conformance::live::{LiveArgs, run};

#[path = "live/cloud.rs"]
mod cloud;

use cloud::{EXTRA, Ec2Cloud, offer_id};

/// The usage line.
const USAGE: &str = "usage: live --run-id ID --region REGION --image AMI --shape TYPE \
                     [--arch x86_64|arm64] [--capture-dir DIR] [--spend --plan-hash HASH]\n\
                     --shape is required: with no type allowed the catalog holds only the types \
                     with a GPU";

/// Runs the suite for `raw` (the arguments without the program name): the exit status.
fn main_with(raw: &[String]) -> i32 {
    let usage_error = |message: &str| {
        eprintln!("{message}\n{USAGE}");
        2
    };
    let mut args = match LiveArgs::parse(raw, EXTRA) {
        Ok(args) => args,
        Err(message) => return usage_error(&message),
    };
    args.shape = offer_id(&args.shape);
    let cloud = match Ec2Cloud::from_args(&args, raw) {
        Ok(cloud) => cloud,
        Err(message) => return usage_error(&message),
    };
    // The catalog runs its blocking calls on tokio's blocking pool, and the driver waits for
    // it without a runtime of its own.
    let runtime = match tokio::runtime::Builder::new_multi_thread()
        .worker_threads(1)
        .build()
    {
        Ok(runtime) => runtime,
        Err(why) => {
            eprintln!("cannot start the runtime: {why}");
            return 1;
        }
    };
    let entered = runtime.enter();
    let mut out = std::io::stdout();
    let status = run(&args, &cloud, &mut out);
    let _ = out.flush();
    drop(entered);
    drop(runtime);
    status
}

fn main() {
    let raw: Vec<String> = std::env::args().skip(1).collect();
    std::process::exit(main_with(&raw));
}
