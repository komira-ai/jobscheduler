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

#[path = "live/cloud.rs"]
mod cloud;

fn main() {
    let raw: Vec<String> = std::env::args().skip(1).collect();
    let mut out = std::io::stdout();
    let status = cloud::main_with(&raw, &mut out, &mut std::io::stderr());
    let _ = out.flush();
    std::process::exit(status);
}
