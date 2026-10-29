//! The rendered script, masked, against files: the text the status page shows and the
//! text a machine runs differ only at the secret bytes, so the file is the contract.
//! The host's name is a hash, so the files hold `<HOST>` in its place.

use js_lease_server::bootstrap::{Fill, render};
use js_lease_server::network::{DefaultMode, NetworkMode};

use crate::fake::Fake;
use crate::support::{TEN_MINUTES, clock, ctx, host, mode, plan};

fn masked(plan: &js_elastic::BootPlan) -> String {
    render(plan, Fill::Mask)
        .unwrap()
        .replace(plan.host.as_str(), "<HOST>")
}

/// Asserts the whole masked script of a plan under the default mode: the trap first, the
/// cleanup lists, the identity files, no network steps, the agent checked by hash and the
/// unit. A change to the script is a change to this file.
#[test]
fn the_default_mode_script_matches_its_golden_file() {
    let host = host();
    let mut plan = plan(&host);
    let mode = DefaultMode::new("https://sched.example.org:7301", Vec::new());
    plan.network = mode.describe(&ctx(&host, TEN_MINUTES));
    assert_eq!(
        masked(&plan),
        include_str!("golden/default_masked.sh"),
        "the default mode's script changed"
    );
}

/// Asserts the whole masked script of a plan under the tailscale mode: the archive
/// fetched and checked, the key file written and removed, and the key file in the
/// cleanup list.
#[test]
fn the_tailscale_script_matches_its_golden_file() {
    let host = host();
    let fake = Fake::empty();
    let (_, shared) = clock();
    let mode = mode(&fake, shared);
    let mut plan = plan(&host);
    plan.network = mode.describe(&ctx(&host, TEN_MINUTES));
    assert_eq!(
        masked(&plan),
        include_str!("golden/tailscale_masked.sh"),
        "the tailscale script changed"
    );
    assert!(fake.seen().is_empty(), "a masked render calls nothing");
}
