//! A private bundle through the store's registry client: the `--registry-credentials`
//! flags, read by the agent's own table and handed to the bundle settings, give a
//! `--bundle-registry-allow` or `--bundle-registry-allow-private` entry the credentials
//! its registry's token realm asks for, and only the token request carries them.

use std::fs;
use std::os::unix::fs::PermissionsExt as _;
use std::path::Path;

use clap::Parser as _;
use js_agent::bundle::{PlaceError, Settings};
use js_agent::{Cli, RegistryCredentials};
use js_oci::{Allow, Credentials};
use js_testkit::oci_registry::{Fixture, REGISTRY, REPO};

use super::fake::FakeHost;
use super::fixture::{Bundle, Spec, placer};
use crate::support::{Scratch, platform};

/// The header of `puller:s3cret-pw` (worked out by hand).
const BASIC: &str = "Basic cHVsbGVyOnMzY3JldC1wdw==";

fn me() -> u32 {
    rustix::process::geteuid().as_raw()
}

/// A bundle of the standard spec, built in `dir`.
fn standard(dir: &Scratch) -> Bundle {
    let src = dir.path("src");
    fs::create_dir_all(&src).unwrap();
    Spec::standard().build(&src)
}

/// A registry holding `bundle` whose realm issues `tok` only to [`BASIC`], and the
/// runtime it runs on (kept alive by the caller).
fn private_registry(bundle: &Bundle) -> (tokio::runtime::Runtime, Fixture) {
    let runtime = tokio::runtime::Runtime::new().unwrap();
    let fixture = runtime.block_on(Fixture::start());
    fixture.publish(&bundle.artifact);
    fixture.behave(|b| {
        b.token = Some("tok".to_owned());
        b.token_basic = Some(BASIC.to_owned());
    });
    (runtime, fixture)
}

/// A credentials file holding `puller:s3cret-pw`, owner-only, in a private directory.
fn credentials_file(dir: &Scratch) -> std::path::PathBuf {
    let secrets = dir.path("secrets");
    fs::create_dir_all(&secrets).unwrap();
    fs::set_permissions(&secrets, fs::Permissions::from_mode(0o700)).unwrap();
    let path = secrets.join("registry");
    fs::write(&path, "puller:s3cret-pw\n").unwrap();
    fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).unwrap();
    path
}

/// The bundle settings of an agent whose registry flag `flag` names `entry`, with the
/// credentials of `file` when it is given, as the agent makes them at start: the flags
/// checked, then the credentials table handed over.
fn settings(dir: &Scratch, flag: &str, entry: &str, file: Option<&Path>) -> Settings {
    let (store, scratch) = (dir.path("store"), dir.path("scratch"));
    for path in [&store, &scratch] {
        fs::create_dir_all(path).unwrap();
        fs::set_permissions(path, fs::Permissions::from_mode(0o700)).unwrap();
    }
    let mut args = vec![
        "js-agent".to_owned(),
        "--server=http://127.0.0.1:1".to_owned(),
        "--pool=macs".to_owned(),
        format!("--scratch-root={}", scratch.display()),
        format!("--bundle-store={}", store.display()),
        "--bundle-budget-gib=10".to_owned(),
        "--bundle-trust=digest".to_owned(),
        format!("{flag}={entry}"),
    ];
    args.extend(file.map(|path| format!("--registry-credentials={entry}={}", path.display())));
    let cli = Cli::try_parse_from(args).unwrap();
    let host = FakeHost::new();
    let checked = Settings::from_cli(&cli, &platform("macos", "aarch64"), &*host);
    let table = RegistryCredentials::from_cli(&cli, me()).unwrap();
    checked.unwrap().unwrap().with_credentials(table)
}

/// The requests of `fx` that carried basic auth.
fn with_basic(fx: &Fixture) -> Vec<String> {
    let lines = fx.requests().into_iter();
    lines.filter(|line| line.contains(" [Basic ")).collect()
}

/// Proves a private bundle is fetched through the store's allow-list, from a public and
/// from a private registry entry: the entry carries the flag's credentials (and the
/// private exemption when it came from the private flag), the token request is the one
/// request that carries them, and the bundle is held. Catches credentials dropped
/// between the flags, the bundle settings and the store's client.
#[test]
fn a_private_bundle_is_fetched_with_the_flag_credentials() {
    for (flag, private) in [
        ("--bundle-registry-allow", false),
        ("--bundle-registry-allow-private", true),
    ] {
        let dir = Scratch::new("bundle-private");
        let bundle = standard(&dir);
        let (_runtime, fx) = private_registry(&bundle);
        let entry = format!("{}/team", fx.at(REGISTRY));
        let file = credentials_file(&dir);
        let settings = settings(&dir, flag, &entry, Some(&file));
        let allow = settings.registry_allow().unwrap();
        let mut want = Allow::parse(&entry)
            .unwrap()
            .with_credentials(Credentials::new("puller", "s3cret-pw").unwrap());
        if private {
            want = want.private();
        }
        assert_eq!(allow, [want], "{flag}");
        assert_eq!(
            format!("{:?}", settings.credentials),
            format!(
                "RegistryCredentials {{ entries: [(Allow {{ authority: Authority {{ host: \
                 \"registry.test\", port: {} }}, prefix: \"team\", private: false, loopback: \
                 false, plain_http: false, port_named: true, credentials: None }}, \
                 Credentials(<redacted>))] }}",
                fx.port
            ),
            "{flag}"
        );
        let client = fx.client_with(allow.into_iter().map(Allow::loopback).collect());
        let store = placer(&dir, client);
        store
            .ensure_registry(&format!("{entry}/bundle"), &bundle.digest())
            .unwrap();
        assert_eq!(
            with_basic(&fx),
            [format!(
                "{} GET /token?service=fake-registry&scope=repository:{REPO}:pull [{BASIC}]",
                fx.at(REGISTRY)
            )],
            "{flag}"
        );
    }
}

/// Proves a bundle entry the flags gave no credentials asks the realm anonymously, and the
/// fetch fails with the realm's refusal word for word. Catches credentials attached to an
/// entry no flag named.
#[test]
fn a_bundle_entry_without_credentials_is_fetched_anonymously() {
    let dir = Scratch::new("bundle-anonymous");
    let bundle = standard(&dir);
    let (_runtime, fx) = private_registry(&bundle);
    let entry = format!("{}/team", fx.at(REGISTRY));
    let settings = settings(&dir, "--bundle-registry-allow", &entry, None);
    let allow = settings.registry_allow().unwrap();
    assert_eq!(allow, [Allow::parse(&entry).unwrap()]);
    let store = placer(
        &dir,
        fx.client_with(allow.into_iter().map(Allow::loopback).collect()),
    );
    let refused = store
        .ensure_registry(&format!("{entry}/bundle"), &bundle.digest())
        .unwrap_err();
    assert!(matches!(refused, PlaceError::Source(_)), "{refused}");
    assert_eq!(
        refused.to_string(),
        format!(
            "https://{}/token: the registry answered 401",
            fx.at(REGISTRY)
        )
    );
    assert!(with_basic(&fx).is_empty(), "{:?}", fx.requests());
}
