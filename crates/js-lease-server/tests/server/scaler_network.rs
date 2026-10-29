//! The network mode of a launching pool is built when the server starts: the default mode
//! needs nothing, the Tailscale mode needs its client and a usable address for the API (the
//! default, or the file's), and a pool in `plan` mode starts without the pins a live pool
//! needs. No test calls the Tailscale API: a mode makes no call until a launch asks it to.

use js_lease::{ManualClock, Timestamp};
use js_lease_server::{
    Config, OwnerOnlyFiles, Running, SharedClock, StartupError, TickHandle, channel_ticker,
    start_with_checks,
};

use crate::config_support::{clock, oauth_file};
use crate::scaler_support::{account, cloud_kinds, text_with};
use crate::support::{Scratch, T0};

/// A server whose pool `burst` is in `launch` mode with this `network` table, and the
/// handles of its tickers: neither is stepped, so no check reaches a network.
type Started = (Running, Scratch, (TickHandle, TickHandle));

/// Starts a server whose pool `burst` is in `launch` mode with this `network` table.
async fn started(name: &str, launch: &str, network: &str) -> Result<Started, StartupError> {
    let scratch = Scratch::new(name);
    account(&scratch);
    let table = network.replace("OAUTH", &oauth_file(&scratch));
    let source = text_with(&scratch, launch, "initialise", &[("network", &table)]);
    let path = scratch.write("js.toml", &source, 0o644);
    let config = Config::from_config_file(&path, &cloud_kinds(), &OwnerOnlyFiles, clock());
    let (ticks, ticker) = channel_ticker();
    let (checks, check_ticker) = channel_ticker();
    let now = SharedClock::new(ManualClock::new(Timestamp::from_millis(T0)));
    let running = start_with_checks(config.unwrap(), now, ticker, check_ticker).await?;
    Ok((running, scratch, (ticks, checks)))
}

const TAILSCALE: &str = "{ kind = \"tailscale\", oauth_client_file = OAUTH, tag = \"tag:js-agent\"";

/// The `binary` pin of a Tailscale build.
fn binary() -> String {
    format!(
        "binary = {{ url = \"https://pkgs.example.org/ts.tgz\", sha256 = \"{}\" }}",
        "ab".repeat(32)
    )
}

/// Asserts that a pool in `plan` mode starts with the Tailscale mode and no pins (the
/// default API address, no binary), with the file's address and binary, and with an
/// address the client cannot use, which its checks show and nothing launches over. A
/// `live` pool whose network cannot be built does not start: the refusal names the pool
/// and the network. It catches a plan pool that needs what only a launch uses, a live pool
/// that starts over a network it cannot use, and a refusal that does not say where.
#[tokio::test]
async fn the_tailscale_mode_is_built_at_start() {
    let bare = format!("{TAILSCALE} }}");
    assert!(started("scaler_net_bare", "plan", &bare).await.is_ok());

    let pinned = format!(
        "{TAILSCALE}, api_url = \"http://127.0.0.1:9\", {} }}",
        binary()
    );
    assert!(started("scaler_net_pinned", "plan", &pinned).await.is_ok());

    let bad = format!(
        "{TAILSCALE}, api_url = \"ftp://tailscale.example.org\", {} }}",
        binary()
    );
    assert!(started("scaler_net_bad_plan", "plan", &bad).await.is_ok());
    let said = started("scaler_net_bad_live", "live", &bad)
        .await
        .err()
        .unwrap()
        .to_string();
    assert!(said.starts_with("pool \"burst\": network: "), "{said}");
    assert!(said.contains("must be https://host"), "{said}");
}
