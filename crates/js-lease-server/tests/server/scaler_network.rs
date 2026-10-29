//! The network mode of a launching pool is built when the server starts: the default mode
//! needs nothing, the Tailscale mode needs its client and a usable address for the API (the
//! default, or the file's), and a pool in `plan` mode starts without the pins a live pool
//! needs. No test calls the Tailscale API: a mode makes no call until a launch asks it to.

use js_lease::{ManualClock, Timestamp};
use js_lease_server::{
    Config, OwnerOnlyFiles, Running, SharedClock, StartupError, channel_ticker, start,
};

use crate::config_support::{clock, oauth_file};
use crate::scaler_support::{account, cloud_kinds, text_with};
use crate::support::{Scratch, T0};

/// Starts a server whose pool `burst` is in `plan` mode with this `network` table.
async fn started(name: &str, network: &str) -> Result<(Running, Scratch), StartupError> {
    let scratch = Scratch::new(name);
    account(&scratch);
    let table = network.replace("OAUTH", &oauth_file(&scratch));
    let source = text_with(&scratch, "plan", "initialise", &[("network", &table)]);
    let path = scratch.write("js.toml", &source, 0o644);
    let config = Config::from_config_file(&path, &cloud_kinds(), &OwnerOnlyFiles, clock());
    let (_ticks, ticker) = channel_ticker();
    let now = SharedClock::new(ManualClock::new(Timestamp::from_millis(T0)));
    let running = start(config.unwrap(), now, ticker).await?;
    Ok((running, scratch))
}

const TAILSCALE: &str = "{ kind = \"tailscale\", oauth_client_file = OAUTH, tag = \"tag:js-agent\"";

/// Asserts that a pool in `plan` mode starts with the Tailscale mode and no pins (the
/// default API address, no binary) and with the file's address and binary, and that a
/// Tailscale address the client cannot use stops the start naming the pool and the field.
/// It catches a plan pool that needs what only a launch uses, a mode built over an address
/// nobody checked, and a refusal that does not say where.
#[tokio::test]
async fn the_tailscale_mode_is_built_at_start() {
    let bare = format!("{TAILSCALE} }}");
    assert!(started("scaler_net_bare", &bare).await.is_ok());

    let pinned = format!(
        "{TAILSCALE}, api_url = \"http://127.0.0.1:9\", binary = {{ url = \"https://pkgs.example.org/ts.tgz\", sha256 = \"{}\" }} }}",
        "ab".repeat(32)
    );
    assert!(started("scaler_net_pinned", &pinned).await.is_ok());

    let bad = format!("{TAILSCALE}, api_url = \"ftp://tailscale.example.org\" }}");
    let said = started("scaler_net_bad", &bad)
        .await
        .err()
        .unwrap()
        .to_string();
    assert!(
        said.starts_with("pool \"burst\": network.api_url: "),
        "{said}"
    );
    assert!(said.contains("must be https://host"), "{said}");
}
