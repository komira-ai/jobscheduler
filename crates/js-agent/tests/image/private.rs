//! A private image through the crun runtime's image client: the `--registry-credentials`
//! flags, read by the agent's own table, give the `--image-allow` entry the credentials
//! its registry's token realm asks for, and nothing else the client sends carries them.

use std::fs;
use std::os::unix::fs::PermissionsExt as _;
use std::path::{Path, PathBuf};

use clap::Parser as _;
use js_agent::image::RegistryImages;
use js_agent::{Cli, RegistryCredentials};
use js_contain::oci::image::{FetchError, ImageSource as _, PinnedImage};
use js_oci::Digest;
use js_testkit::dir::TestDir;
use js_testkit::oci_registry::{Artifact, Fixture, REGISTRY};

use crate::config::config_json;

/// The header of `puller:s3cret-pw` (worked out by hand).
const BASIC: &str = "Basic cHVsbGVyOnMzY3JldC1wdw==";

fn me() -> u32 {
    rustix::process::geteuid().as_raw()
}

fn image() -> Artifact {
    let layers = vec![b"base layer ".repeat(40), b"top layer ".repeat(60)];
    let config = config_json("{}", layers.len());
    Artifact::image(layers, &config)
}

/// A credentials file holding `text`, owner-only, in a private directory of `dir`.
fn credentials_file(dir: &TestDir, text: &str) -> PathBuf {
    fs::set_permissions(dir.path(), fs::Permissions::from_mode(0o700)).expect("dir mode");
    let path = dir.path().join("registry");
    fs::write(&path, text).expect("write");
    fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).expect("mode");
    path
}

/// The agent's flags for `fx`: one `--image-allow` entry and, when `file` is given, its
/// credentials.
fn cli(fx: &Fixture, file: Option<&Path>) -> Cli {
    let entry = format!("{}/team", fx.at(REGISTRY));
    let mut args = vec![
        "js-agent".to_owned(),
        "--server=https://leases.example".to_owned(),
        "--pool=default".to_owned(),
        format!("--image-allow={entry}"),
    ];
    args.extend(file.map(|path| format!("--registry-credentials={entry}={}", path.display())));
    Cli::try_parse_from(args).expect("flags")
}

/// The crun runtime's image source over the allow-list the agent makes of `cli`. The
/// test client trusts the fixture and may reach its loopback address; the entries,
/// credentials included, are the agent's.
fn images(fx: &Fixture, cli: &Cli) -> RegistryImages {
    let table = RegistryCredentials::from_cli(cli, me()).expect("credentials");
    let allow = table.image_allow(cli).expect("image allow");
    let allow = allow.into_iter().map(js_oci::Allow::loopback).collect();
    RegistryImages::new(fx.client_with(allow), "x86_64").expect("a platform")
}

fn pin(fx: &Fixture, artifact: &Artifact) -> PinnedImage {
    PinnedImage {
        reference: format!("{}/team/app@{}", fx.at(REGISTRY), artifact.digest),
        digest: artifact.digest.to_string(),
    }
}

/// A registry holding `artifact` whose realm issues `tok` only to [`BASIC`].
async fn private_registry(artifact: &Artifact) -> Fixture {
    let fx = Fixture::start().await;
    fx.publish(artifact);
    fx.behave(|b| {
        b.token = Some("tok".to_owned());
        b.token_basic = Some(BASIC.to_owned());
    });
    fx
}

/// The requests of a pull of `artifact` from `team/app`, after the first manifest
/// request: the token request with `auth`, then the manifest, config and layers with the
/// token.
fn pull_log(fx: &Fixture, artifact: &Artifact, auth: &str) -> Vec<String> {
    let host = fx.at(REGISTRY);
    let blob = |d: &Digest| format!("{host} GET /v2/team/app/blobs/{d} [Bearer tok]");
    let mut log = vec![
        format!("{host} GET /v2/team/app/manifests/{} []", artifact.digest),
        format!(
            "{host} GET /token?service=fake-registry&scope=repository:team/bundle:pull [{auth}]"
        ),
        format!(
            "{host} GET /v2/team/app/manifests/{} [Bearer tok]",
            artifact.digest
        ),
        blob(&Digest::of(&artifact.config)),
    ];
    log.extend(artifact.layers.iter().map(|l| blob(&Digest::of(l))));
    log
}

/// Proves a private image is pulled through the agent's allow-list: the token request
/// carries the credentials of the flag's file, and the manifest, config and layers only
/// the token. Catches credentials dropped between the flags and the image client, and
/// credentials sent with the image's own requests.
#[tokio::test]
async fn a_private_image_is_pulled_with_the_flag_credentials() {
    let artifact = image();
    let fx = private_registry(&artifact).await;
    let dir = TestDir::under(env!("CARGO_TARGET_TMPDIR"), "image-private");
    let file = credentials_file(&dir, "puller:s3cret-pw\n");
    let blobs = TestDir::under(env!("CARGO_TARGET_TMPDIR"), "image-private-blobs");
    let names = images(&fx, &cli(&fx, Some(&file)))
        .fetch(&pin(&fx, &artifact), blobs.path())
        .await
        .expect("a private pull");
    assert_eq!(names, ["0.tar.gz", "1.tar.gz"]);
    assert_eq!(fx.requests(), pull_log(&fx, &artifact, BASIC));
}

/// Proves what a failed private pull reports, which is what the run's record and the
/// agent's log get: without credentials, and with a wrong password, the failure is the
/// realm's refusal, word for word, and holds neither the user, the password nor the
/// header. Catches an image refused for another reason, and a failure that echoes the
/// request.
#[tokio::test]
async fn a_refused_private_pull_reports_the_realm_and_no_secret() {
    let artifact = image();
    let fx = private_registry(&artifact).await;
    let dir = TestDir::under(env!("CARGO_TARGET_TMPDIR"), "image-private-refused");
    let wrong = credentials_file(&dir, "puller:wrong-pw\n");
    let want = format!(
        "https://{}/token: the registry answered 401",
        fx.at(REGISTRY)
    );
    for (case, file) in [("no credentials", None), ("a wrong password", Some(&wrong))] {
        let blobs = TestDir::under(env!("CARGO_TARGET_TMPDIR"), "image-private-none");
        let refused = images(&fx, &cli(&fx, file.map(PathBuf::as_path)))
            .fetch(&pin(&fx, &artifact), blobs.path())
            .await
            .expect_err(case);
        match refused {
            FetchError::Failed(why) => assert_eq!(why, want, "{case}"),
            FetchError::Index => panic!("{case}: named an index"),
        }
    }
    let host = fx.at(REGISTRY);
    let token =
        format!("{host} GET /token?service=fake-registry&scope=repository:team/bundle:pull");
    let manifest = format!("{host} GET /v2/team/app/manifests/{} []", artifact.digest);
    assert_eq!(
        fx.requests(),
        [
            manifest.clone(),
            format!("{token} []"),
            manifest,
            format!("{token} [Basic cHVsbGVyOndyb25nLXB3]"),
        ]
    );
}
