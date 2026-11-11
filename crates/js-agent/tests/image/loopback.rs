//! An image from a registry on the host's own loopback, as `--image-allow-loopback` names it:
//! the agent's allow-list over the crun runtime's image client, against a registry this test
//! serves over plain http on `127.0.0.1` at a port the kernel chose.

use clap::Parser as _;
use js_agent::image::RegistryImages;
use js_agent::{Cli, RegistryCredentials};
use js_contain::oci::image::{ImageSource as _, PinnedImage};
use js_oci::{Client, Policy};
use js_testkit::dir::TestDir;
use js_testkit::oci_registry::{Artifact, Fixture};

use crate::config::config_json;

/// Proves an image (as an image layer's is) is pulled over plain http from a registry on
/// `127.0.0.1` with a port, given by `--image-allow-loopback` alone, through the client the
/// crun runtime makes of the agent's allow-list: no certificate, no other entry. Catches a
/// loopback entry pulled over https (it would fail with no certificate), and one the client
/// is not let reach.
#[tokio::test]
async fn an_image_on_a_loopback_registry_is_pulled_over_plain_http() {
    let layers = vec![b"a layer ".repeat(30)];
    let artifact = Artifact::image(layers, &config_json("{}", 1));
    let fx = Fixture::start_plain().await;
    fx.publish(&artifact);
    let entry = format!("127.0.0.1:{}/team", fx.port);
    let cli = Cli::try_parse_from([
        "js-agent",
        "--server=https://leases.example",
        "--pool=default",
        &format!("--image-allow-loopback={entry}"),
    ])
    .expect("flags");
    let table = RegistryCredentials::from_cli(&cli, 0).expect("no credentials");
    let allow = table.image_allow(&cli).expect("the allow-list");
    let images = RegistryImages::new(Client::new(Policy::new(allow)), "x86_64").expect("images");
    let pin = PinnedImage {
        reference: format!("{entry}/app@{}", artifact.digest),
        digest: artifact.digest.to_string(),
    };
    let blobs = TestDir::under(env!("CARGO_TARGET_TMPDIR"), "image-loopback");
    let names = images.fetch(&pin, blobs.path()).await.expect("pulled");
    assert_eq!(names, ["0.tar.gz"]);
}
