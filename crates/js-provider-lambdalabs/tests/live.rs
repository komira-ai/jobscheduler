//! The live conformance suite of the Lambda Cloud provider: the cases of
//! `js_elastic::conformance::live` run against the real Lambda Cloud API, through the
//! provider the `lambdalabs` kind builds.
//!
//! It reads the catalog, chooses the offer of the instance type it is given, launches three
//! instances one at a time (each released before the next), and ends with a cleanup pass that
//! terminates whatever the run's scheduler still holds. It is a dry run unless it is given
//! `--spend` and the plan hash a dry run printed: a dry run launches nothing and spends
//! nothing, and a spend run refuses when the plan it computes now hashes to another value.
//! Run it through `tools/held/provider-live/provider_live.sh` or the `provider-live`
//! workflow, as `docs/runbooks/live-conformance.md` says; it is built only with the cargo
//! feature `live`.
//!
//! ```text
//! --run-id ID            required: 1 to 40 characters of a-z, 0-9 and '-'; names the scheduler
//!                        `live-<ID>` and the tag `validation-run` every instance carries
//! --region REGION        required: the region to launch in, such as us-east-1
//! --shape TYPE           required, as in every provider's suite: the instance type to
//!                        launch, such as gpu_1x_a10; the suite launches only the shape it is
//!                        told, and the provider's table names that type before it is built
//! --key-file PATH        required: the file holding the API key, which only its owner may
//!                        read or write
//! --spend --plan-hash H  launch for real; H is the plan hash a dry run printed
//! ```
//!
//! The API key comes from the key file and from nowhere else; no flag or variable holds it.
//! There is no capture flag: the provider's client has no seam for one, so the answers of a
//! live run are captured with any HTTP client and the same key file (see
//! `tests/fixtures/README.md`). A launch also needs a firewall ruleset and an ssh key, which
//! the provider finds or creates itself and which cost nothing; the suite leaves them in the
//! account. The exit status is 0 for a pass, 1 for a failed case, a left-over instance or a
//! changed plan, and 2 for a usage error.
//!
//! This file is only the `main` of the suite. The flags' account, its provider table and the
//! reader of the key file are in `tests/live/cloud.rs`, which `tests/live_cloud.rs` tests
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
