//! SCRATCH, never merged: pulls the private GHCR probe bundle by digest, authenticating
//! to the token realm with Basic auth from the environment. Run with `--ignored`.

use js_oci::{Allow, BundleSource, Client, Digest, Kind, Location, Policy};

const DIGEST: &str = "sha256:422599f15d5578a584ab9bedaa02462bf1bf16dc6e4ed4b9d280b0ff155902dc";

#[tokio::test]
#[ignore = "needs GHCR credentials and the network"]
async fn pull_probe_bundle() {
    let user = std::env::var("GHCR_PROBE_USER").unwrap();
    let token = std::env::var("GHCR_PROBE_TOKEN").unwrap();
    let out = std::env::var("GHCR_PROBE_OUT").unwrap();
    let client = Client::new(Policy::new(vec![Allow::parse("ghcr.io/komira-ai/").unwrap()]))
        .with_basic_auth(&user, &token);
    let repo = client
        .repository(Location::parse("ghcr.io/komira-ai/js-bundle-probe").unwrap())
        .unwrap();
    let pin = Digest::parse(DIGEST).unwrap();
    let kind = Kind::bundle();
    let manifest = match repo.fetch_manifest(&pin, &kind).await {
        Ok(m) => m,
        Err(e) => panic!("PULL REFUSED: {e} / {e:?}"),
    };
    println!("manifest-digest: {}", manifest.digest().as_str());
    assert_eq!(manifest.digest(), &pin);
    println!("artifact-type: {}", manifest.artifact_type().unwrap_or_default());
    println!(
        "config: {} {}",
        manifest.config().digest().as_str(),
        manifest.config().media_type()
    );
    for (k, v) in manifest.annotations() {
        println!("annotation: {k}={v}");
    }
    let config = repo.fetch_config(&manifest, &kind).await.unwrap();
    println!("config-bytes: {}", config.len());
    for (i, layer) in manifest.layers().iter().enumerate() {
        let mut bytes = Vec::new();
        repo.fetch_blob(layer, &mut bytes).await.unwrap();
        println!(
            "layer {i}: {} {} {} bytes (verified)",
            layer.digest().as_str(),
            layer.media_type(),
            bytes.len()
        );
        std::fs::write(format!("{out}/layer-{i}.tar.zst"), &bytes).unwrap();
    }
}
