//! The `[[machine_provider]]` and `[[platform]]` tables, a pool's `platforms`, its queue
//! settings and its `provider_limits`, read through [`Config::resolve_file`] and, where a
//! file is refused at start, [`Config::from_config_file`].

use std::time::Duration;

use js_lease_server::{
    BuildCtx, Built, Config, KindError, Kinds, OwnerOnlyFiles, PlatformKind, Resolved,
    StartupError, StaticPoolSource,
};
use js_platform::PlatformId;

use crate::config_support::{
    API_KEY, PoolText, clock, file, kinds, load, quote, quoted, server_with_agents,
};
use crate::support::Scratch;

/// A provider kind that builds a pool of listed hosts, as the agentless `static` kind
/// does, under another name.
fn listed_kind(_table: &toml::Table, _ctx: &BuildCtx<'_>) -> Result<Built, KindError> {
    Ok(Built::Static(StaticPoolSource::new("hosts.toml")))
}

/// The kinds of the tests, a hostless platform kind and a listed-hosts provider kind
/// added.
fn platform_kinds() -> Kinds {
    kinds()
        .with_platform(PlatformKind::hostless("test-hostless"))
        .with("listed", listed_kind)
}

fn resolve(scratch: &Scratch, text: &str) -> Result<Resolved, StartupError> {
    let path = scratch.write("js.toml", text, 0o644);
    Config::resolve_file(&path, &platform_kinds(), &OwnerOnlyFiles, clock())
}

fn resolved(scratch: &Scratch, text: &str) -> Resolved {
    match resolve(scratch, text) {
        Ok(resolved) => resolved,
        Err(err) => panic!("the file should resolve: {err}"),
    }
}

fn refused(scratch: &Scratch, text: &str) -> String {
    match resolve(scratch, text) {
        Ok(_) => panic!("the file should be refused"),
        Err(err) => err.to_string(),
    }
}

/// `items` as a TOML array of strings.
fn names(items: &[&str]) -> String {
    let quoted: Vec<String> = items.iter().map(|item| quote(item)).collect();
    format!("[{}]", quoted.join(", "))
}

fn rack() -> String {
    "[[machine_provider]]\nname = \"rack\"\nkind = \"static\"\n".to_owned()
}

/// A `lambdalabs` machine provider `name` whose key file holds [`API_KEY`].
fn lambda(scratch: &Scratch, name: &str) -> String {
    let key = scratch.write(&format!("key-{name}"), API_KEY, 0o600);
    format!(
        "[[machine_provider]]\nname = {}\nkind = \"lambdalabs\"\napi_key_file = {}\n",
        quote(name),
        quoted(&key)
    )
}

fn platform(name: &str, providers: &[&str]) -> String {
    format!(
        "[[platform]]\nname = {}\nkind = \"default\"\nmachine_providers = {}\n",
        quote(name),
        names(providers)
    )
}

/// The elastic pool `gpu` naming `platforms` instead of its provider.
fn on(platforms: &[&str], scratch: &Scratch) -> PoolText {
    PoolText::elastic(scratch)
        .without("provider")
        .set("platforms", &names(platforms))
}

/// A file: the server, then `tables`, then `pools`.
fn doc(scratch: &Scratch, tables: &[String], pools: &[&PoolText]) -> String {
    let head = format!("{}\n{}", server_with_agents(scratch), tables.concat());
    file(&head, pools)
}

/// The declared tables of the explicit spelling of `PoolText::elastic`: `rack`, `lambda`
/// and the platform `ours` naming them in that order.
fn ours(scratch: &Scratch) -> Vec<String> {
    vec![
        rack(),
        lambda(scratch, "lambda"),
        platform("ours", &["rack", "lambda"]),
    ]
}

/// The file of the golden `config_two_elastic.txt`: two `lambdalabs` machine providers
/// with their own key files, two platforms, the elastic pools `gpu` (settings and limits
/// given, `catalog_refresh` among them) and `cpu`, and an agentless static pool.
fn two_elastic(scratch: &Scratch) -> String {
    let tables = [
        rack(),
        lambda(scratch, "lambda-a"),
        lambda(scratch, "lambda-b"),
        platform("east", &["rack", "lambda-a"]),
        platform("west", &["rack", "lambda-b"]),
    ];
    let gpu = on(&["east"], scratch)
        .set("max_queue_wait", &quote("30m"))
        .set("start_timeout", &quote("90s"))
        .set("path_grace", &quote("250ms"))
        .set(
            "provider_limits",
            "{ lambda-a = { allowed_instance_types = [\"gpu_1x_a10\", \"gpu_8x_h100\"], max_price_cents_per_hour = 5000, max_machines = 3, max_spot_machines = 2, catalog_refresh = \"30m\" } }",
        );
    let cpu = on(&["west"], scratch)
        .set("name", &quote("cpu"))
        .set("max_instances", "2");
    let hosts = PoolText::static_pool(scratch, "hosts");
    doc(scratch, &tables, &[&gpu, &cpu, &hosts])
}

/// Asserts that a pool written with `provider = { kind = "lambdalabs", ... }` resolves to
/// what the explicit tables (`static` then `lambdalabs` machine providers, a `default`
/// platform naming them in that order, `platforms` on the pool) resolve to, line for line
/// once the loader's names are replaced by the operator's, and that the loader's names
/// carry the `+` no file name may hold. It catches a shorthand that drops `static`,
/// reverses the order, or loses a key of the provider table.
#[test]
fn provider_shorthand_equals_the_explicit_tables() {
    let scratch = Scratch::new("platform_shorthand");
    let short = resolved(
        &scratch,
        &doc(&scratch, &[], &[&PoolText::elastic(&scratch)]),
    );
    let explicit = resolved(
        &scratch,
        &doc(&scratch, &ours(&scratch), &[&on(&["ours"], &scratch)]),
    );
    let renamed = short
        .describe()
        .replace("+gpu.lambdalabs", "lambda")
        .replace("+static", "rack")
        .replace("+gpu", "ours");
    assert_eq!(renamed, explicit.describe());
    let pool = short.pool("gpu").unwrap();
    assert_eq!(pool.platforms, vec![PlatformId::new("+gpu")]);
    assert_eq!(
        short.platforms[0].machine_providers,
        ["+static", "+gpu.lambdalabs"]
    );
    assert!(!short.machine_providers[0].launches());
    assert!(short.machine_providers[1].launches());
    assert_eq!(short.machine_providers[1].kind, "lambdalabs");
    assert!(short.pool("absent").is_none());
}

/// Asserts that two pools written with equal provider tables share one made machine
/// provider (and the one made `static`), each with its own platform, and that a pool with
/// another table gets a provider of its own. It catches a loader that makes a provider per
/// pool, or folds two different tables into one.
#[test]
fn shorthand_pools_share_an_equal_provider_table() {
    let scratch = Scratch::new("platform_shorthand_share");
    let gpu = PoolText::elastic(&scratch);
    let twin = PoolText::elastic(&scratch).set("name", &quote("twin"));
    let other_key = quoted(&scratch.write("other-key", API_KEY, 0o600));
    let other = PoolText::elastic(&scratch)
        .set("name", &quote("other"))
        .set(
            "provider",
            &format!("{{ kind = \"lambdalabs\", api_key_file = {other_key} }}"),
        );
    let both = resolved(&scratch, &doc(&scratch, &[], &[&gpu, &twin, &other]));
    let providers: Vec<&str> = both
        .machine_providers
        .iter()
        .map(|provider| provider.name.as_str())
        .collect();
    assert_eq!(
        providers,
        ["+static", "+gpu.lambdalabs", "+other.lambdalabs"]
    );
    assert_eq!(
        both.platforms[1].machine_providers,
        ["+static", "+gpu.lambdalabs"]
    );
    assert_eq!(both.platforms[1].id, PlatformId::new("+twin"));
}

/// Asserts that a pool naming a platform no table declares is refused naming the pool and
/// the platform. It catches a loader that skips the cross-reference.
#[test]
fn a_pool_naming_an_undefined_platform_is_refused() {
    let scratch = Scratch::new("platform_undefined");
    let text = doc(
        &scratch,
        &ours(&scratch),
        &[&on(&["ours", "nowhere"], &scratch)],
    );
    assert_eq!(
        refused(&scratch, &text),
        "pool \"gpu\": platforms: \"nowhere\" is not a declared platform"
    );
}

/// Asserts that a platform naming a machine provider no table declares is refused naming
/// the platform and the provider. It catches a loader that skips the cross-reference.
#[test]
fn a_platform_naming_an_undefined_machine_provider_is_refused() {
    let scratch = Scratch::new("platform_undefined_provider");
    let tables = [rack(), platform("ours", &["rack", "gone"])];
    let text = doc(&scratch, &tables, &[&on(&["ours"], &scratch)]);
    assert_eq!(
        refused(&scratch, &text),
        "platform \"ours\": machine_providers: \"gone\" is not a declared machine provider"
    );
}

/// Asserts that a pool giving both `provider` and `platforms` is refused naming
/// `provider`, and one giving neither is refused as before. It catches a loader that
/// accepts both spellings at once and picks one.
#[test]
fn provider_and_platforms_on_one_pool_are_refused() {
    let scratch = Scratch::new("platform_both");
    let both = PoolText::elastic(&scratch).set("platforms", &names(&["ours"]));
    assert_eq!(
        refused(&scratch, &doc(&scratch, &ours(&scratch), &[&both])),
        "pool \"gpu\": provider: and platforms both say where the pool's hosts come from; give one"
    );
    let neither = PoolText::elastic(&scratch).without("provider");
    assert_eq!(
        refused(&scratch, &doc(&scratch, &[], &[&neither])),
        "pool \"gpu\": provider: required"
    );
}

/// Asserts that one file with two `lambdalabs` machine providers (each with its own key
/// file), two `default` platforms, two elastic pools and an agentless static pool resolves
/// (the refusals of a second registering pool are gone), with each pool's platforms, cap,
/// settings and limits as written.
#[test]
fn two_elastic_providers_in_one_server_parse() {
    let scratch = Scratch::new("platform_two_elastic");
    let both = resolved(&scratch, &two_elastic(&scratch));
    let gpu = both.pool("gpu").unwrap();
    assert_eq!(gpu.platforms, vec![PlatformId::new("east")]);
    assert_eq!(gpu.max_machines, Some(4));
    assert_eq!(gpu.settings.path_grace, Duration::from_millis(250));
    let limits = &gpu.provider_limits["lambda-a"];
    assert_eq!(limits.max_machines, Some(3));
    let cpu = both.pool("cpu").unwrap();
    assert_eq!(cpu.platforms, vec![PlatformId::new("west")]);
    assert_eq!(cpu.max_machines, Some(2));
    assert!(cpu.provider_limits.is_empty());
    assert!(both.pool("hosts").is_none());
}

/// Asserts the exact refusal to start on a file with two elastic pools, naming the second:
/// this version runs one pool that registers agents. It catches a
/// server that starts and serves only one of the two.
#[test]
fn starting_several_elastic_pools_is_refused_until_the_walk_lands() {
    let scratch = Scratch::new("platform_two_elastic_start");
    let said = load(&scratch, &two_elastic(&scratch))
        .err()
        .unwrap()
        .to_string();
    assert_eq!(
        said,
        "pool \"cpu\": several elastic pools are parsed; running them needs the ordered walk of design 5.6"
    );
}

/// A pool `jobs` that names only the hostless platform `run`.
fn hostless_pool(scratch: &Scratch) -> PoolText {
    PoolText::elastic(scratch)
        .set("name", &quote("jobs"))
        .set("platforms", &names(&["run"]))
        .without("provider")
        .without("class")
        .without("max_instances")
        .without("max_spend_per_hour")
        .without("max_spend_per_day")
        .without("idle")
        .without("max_age")
        .without("min_host_lifetime")
}

/// The tables of the hostless tests: the explicit `ours` tables and a hostless platform
/// `run`.
fn with_run(scratch: &Scratch) -> Vec<String> {
    let mut tables = ours(scratch);
    tables.push("[[platform]]\nname = \"run\"\nkind = \"test-hostless\"\n".to_owned());
    tables
}

/// Asserts that a pool naming only a hostless platform (reached through a test kind:
/// none is built) is refused for `shape`, `max_machines` and `max_queue_wait`, naming each
/// key; that it is accepted with `path_grace`, with no cap and the default queue settings;
/// that an unknown key and a hostless platform's `machine_providers` are refused; and that
/// a pool naming the hostless platform and a `default` one takes the keys that describe
/// hosts. It catches host-describing keys leaking through to a pool that has no host.
#[test]
fn hostless_only_pools_reject_shape_and_max_machines() {
    let scratch = Scratch::new("platform_hostless");
    let tables = with_run(&scratch);
    let gpu = on(&["ours"], &scratch);
    for (key, value) in [
        ("shape", quote("linux-x86-16")),
        ("max_machines", "4".to_owned()),
        ("max_queue_wait", quote("30m")),
    ] {
        let pool = hostless_pool(&scratch).set(key, &value);
        assert_eq!(
            refused(&scratch, &doc(&scratch, &tables, &[&gpu, &pool])),
            format!(
                "pool \"jobs\": {key}: describes hosts; the pool names only hostless platforms"
            )
        );
    }
    let unknown = hostless_pool(&scratch).set("class", &quote("whole-node"));
    assert_eq!(
        refused(&scratch, &doc(&scratch, &tables, &[&gpu, &unknown])),
        "pool \"jobs\": class: unknown field; known: name, platforms, path_grace"
    );
    let grace = hostless_pool(&scratch).set("path_grace", &quote("5s"));
    let mixed = on(&["run", "ours"], &scratch)
        .without("max_instances")
        .set("max_machines", "4")
        .set("max_queue_wait", &quote("30m"));
    let both = resolved(&scratch, &doc(&scratch, &tables, &[&mixed, &grace]));
    let mixed = both.pool("gpu").unwrap();
    assert_eq!(mixed.max_machines, Some(4));
    assert_eq!(mixed.settings.max_queue_wait, Duration::from_secs(1800));
    let run = PlatformId::new("run");
    assert_eq!(mixed.platforms, vec![run, PlatformId::new("ours")]);
    let only = both.pool("jobs").unwrap();
    assert_eq!(only.max_machines, None);
    assert_eq!(only.settings.path_grace, Duration::from_secs(5));
    let described = both.describe();
    let lines = [
        "platform run: kind test-hostless; hosts no; machine_providers -",
        "pool jobs: platforms run; max_machines -; max_queue_wait 1h; start_timeout 15m; path_grace 5s",
    ];
    for line in lines {
        assert!(described.contains(line), "{described}");
    }
    let mut providers = with_run(&scratch);
    providers.pop();
    providers.push(
        "[[platform]]\nname = \"run\"\nkind = \"test-hostless\"\nmachine_providers = [\"rack\"]\n"
            .to_owned(),
    );
    assert_eq!(
        refused(&scratch, &doc(&scratch, &providers, &[&gpu, &grace])),
        "platform \"run\": machine_providers: unknown field; known: name, kind"
    );
}

/// A file with `table` beside the explicit `ours` tables and its pool.
fn beside(scratch: &Scratch, table: &str) -> String {
    let mut tables = ours(scratch);
    tables.push(table.to_owned());
    doc(scratch, &tables, &[&on(&["ours"], scratch)])
}

/// Asserts that each platform kind that is planned and not built is refused as `not
/// built` naming the platform; that a platform or machine provider kind that is not a kind
/// is refused as `unknown` with the known kinds; and that the two words never stand for
/// each other. It catches a kind silently accepted, and unknown and unbuilt conflated.
#[test]
fn a_platform_kind_not_built_is_refused() {
    let scratch = Scratch::new("platform_kinds");
    for kind in ["kubernetes", "cloud_run", "ecs"] {
        let table = format!("[[platform]]\nname = \"later\"\nkind = {}\n", quote(kind));
        assert_eq!(
            refused(&scratch, &beside(&scratch, &table)),
            format!("platform \"later\": kind: {kind:?} is not built in this version")
        );
    }
    let table = "[[platform]]\nname = \"later\"\nkind = \"nonsense\"\n";
    assert_eq!(
        refused(&scratch, &beside(&scratch, table)),
        "platform \"later\": kind: unknown platform kind \"nonsense\"; known: default, test-hostless"
    );
    let table = "[[machine_provider]]\nname = \"cloud\"\nkind = \"nonsense\"\n";
    assert_eq!(
        refused(&scratch, &beside(&scratch, table)),
        "machine_provider \"cloud\": kind: unknown machine provider kind \"nonsense\"; known: any-mode, broken-floor, ec2, gce, lambdalabs, listed, static"
    );
}

/// The `ec2` machine provider `cloud` with the region and the one image the kind needs,
/// then the platform `ours` naming `rack` and it.
fn ec2_tables(extra: &str) -> Vec<String> {
    vec![
        rack(),
        format!(
            "[[machine_provider]]\nname = \"cloud\"\nkind = \"ec2\"\nregion = \"region-a\"\n\
             images = {{ arm64 = \"ami-0123456789abcdef0\" }}\n{extra}"
        ),
        platform("ours", &["rack", "cloud"]),
    ]
}

/// Asserts that a pool on an `ec2` machine provider is refused without `idle`, `max_age`,
/// `min_host_lifetime` and the server `max_lease_wall`, as a pool with GPUs is, though the
/// kind's floor reports no GPU. It catches an `ec2` pool that can launch GPU instances
/// with no bound on how long a host lives.
#[test]
fn ec2_pool_needs_the_lifetime_keys() {
    let scratch = Scratch::new("platform_ec2_lifetime");
    for key in ["idle", "max_age", "min_host_lifetime"] {
        let pool = on(&["ours"], &scratch).without(key);
        let text = doc(&scratch, &ec2_tables(""), &[&pool]);
        let path = scratch.write("js.toml", &text, 0o644);
        let said = Config::resolve_file(&path, &Kinds::standard(), &OwnerOnlyFiles, clock())
            .err()
            .unwrap()
            .to_string();
        assert_eq!(
            said,
            format!(
                "pool \"gpu\": {key}: required of a pool with accelerators: an idle host costs money by the hour, so the file names how long it may live"
            )
        );
    }
}

/// Loads the file of the pool `gpu` on the platform `ours` with `tables`, through the
/// kinds the server ships.
fn through_standard_kinds(scratch: &Scratch, tables: &[String]) -> Result<Resolved, StartupError> {
    let text = doc(scratch, tables, &[&on(&["ours"], scratch)]);
    let path = scratch.write("js.toml", &text, 0o644);
    Config::resolve_file(&path, &Kinds::standard(), &OwnerOnlyFiles, clock())
}

/// Asserts that a `[[machine_provider]]` of kind `ec2` with a region and an image is built
/// by the kinds the server ships and launches machines, that a key the kind does not know
/// (a credential among them) is refused naming the key and the known ones. It catches an
/// `ec2` kind that is not registered, or one still on the list of kinds not built, and a
/// credential key that is read as nothing.
#[test]
fn kind_ec2_builds_and_refuses_an_unknown_key() {
    let scratch = Scratch::new("platform_ec2");
    let built = match through_standard_kinds(&scratch, &ec2_tables("")) {
        Ok(resolved) => resolved,
        Err(err) => panic!("the ec2 kind should build: {err}"),
    };
    let cloud = &built.machine_providers[1];
    assert_eq!((cloud.name.as_str(), cloud.kind.as_str()), ("cloud", "ec2"));
    assert!(cloud.launches());
    assert_eq!(
        built.pool("gpu").unwrap().platforms,
        vec![PlatformId::new("ours")]
    );
    let said = through_standard_kinds(&scratch, &ec2_tables("secret_access_key = \"x\"\n"))
        .err()
        .unwrap()
        .to_string();
    assert_eq!(
        said,
        "machine_provider \"cloud\": secret_access_key: unknown field; known: region, purchase, images, allowed_instance_types, capacity_block_hours"
    );
}

/// The `gce` machine provider `cloud` with the project, the zones, the network, the
/// subnetwork and the one image the kind needs, then the platform `ours` naming `rack` and
/// it.
fn gce_tables(extra: &str) -> Vec<String> {
    vec![
        rack(),
        format!(
            "[[machine_provider]]\nname = \"cloud\"\nkind = \"gce\"\nproject = \"sample-project\"\n\
             zones = [\"region-one-a\"]\nnetwork = \"global/networks/sample-net\"\n\
             subnetwork = \"regions/region-one/subnetworks/sample-subnet\"\n\
             images = {{ x86_64 = \"projects/sample-project/global/images/sample-image\" }}\n{extra}"
        ),
        platform("ours", &["rack", "cloud"]),
    ]
}

/// Asserts that a `[[machine_provider]]` of kind `gce` with a project, zones, a network, a
/// subnetwork and an image is built by the kinds the server ships and launches machines,
/// that a key the kind does not know (a credential among them) is refused naming the key
/// and the known ones, and that `kubernetes`, `cloud_run` and `ecs` are still refused as
/// not built. Building the kind through the standard kinds makes the real client, whose
/// constructor reads `GOOGLE_APPLICATION_CREDENTIALS` and, when it is unset, takes the
/// metadata server's credential without a call, as the `ec2` kind's test reads `AWS_*`. It
/// catches a `gce` kind that is not registered, or one still on the list of kinds not
/// built, and a credential key that is read as nothing.
#[test]
fn kind_gce_builds_and_only_the_platform_kinds_are_refused() {
    let scratch = Scratch::new("platform_gce");
    let built = match through_standard_kinds(&scratch, &gce_tables("")) {
        Ok(resolved) => resolved,
        Err(err) => panic!("the gce kind should build: {err}"),
    };
    let cloud = &built.machine_providers[1];
    assert_eq!((cloud.name.as_str(), cloud.kind.as_str()), ("cloud", "gce"));
    assert!(cloud.launches());
    assert_eq!(
        built.pool("gpu").unwrap().platforms,
        vec![PlatformId::new("ours")]
    );
    let said = through_standard_kinds(&scratch, &gce_tables("private_key = \"x\"\n"))
        .err()
        .unwrap()
        .to_string();
    assert_eq!(
        said,
        "machine_provider \"cloud\": private_key: unknown field; known: project, zones, purchase, network, subnetwork, images, allowed_machine_types"
    );
    for kind in ["kubernetes", "cloud_run", "ecs"] {
        let table = format!("[[platform]]\nname = \"later\"\nkind = {}\n", quote(kind));
        assert_eq!(
            refused(&scratch, &beside(&scratch, &table)),
            format!("platform \"later\": kind: {kind:?} is not built in this version")
        );
    }
}

/// Asserts that a pool on a `gce` machine provider is refused without `idle`, `max_age` and
/// `min_host_lifetime`, as a pool with GPUs is, though the kind's floor reports no GPU. It
/// catches a `gce` pool that can launch GPU instances with no bound on how long a host
/// lives.
#[test]
fn gce_pool_needs_the_lifetime_keys() {
    let scratch = Scratch::new("platform_gce_lifetime");
    for key in ["idle", "max_age", "min_host_lifetime"] {
        let pool = on(&["ours"], &scratch).without(key);
        let text = doc(&scratch, &gce_tables(""), &[&pool]);
        let path = scratch.write("js.toml", &text, 0o644);
        let said = Config::resolve_file(&path, &Kinds::standard(), &OwnerOnlyFiles, clock())
            .err()
            .unwrap()
            .to_string();
        assert_eq!(
            said,
            format!(
                "pool \"gpu\": {key}: required of a pool with accelerators: an idle host costs money by the hour, so the file names how long it may live"
            )
        );
    }
}

/// Asserts the names and the other keys of the two tables: a name missing, outside the
/// grammar (a `+` included, so no name can be one the loader makes) or taken by an earlier
/// table of its kind; a `static` machine provider with a key of its own; a platform
/// without `machine_providers`; a table that is not an array of tables; and a provider
/// kind that builds listed hosts named as a machine provider. Each is refused naming the
/// table and the key. It catches a collision with a name the loader makes and a key read
/// as nothing.
#[test]
fn the_tables_check_their_names_and_keys() {
    let scratch = Scratch::new("platform_names");
    let cases = [
        (
            "[[machine_provider]]\nkind = \"static\"\n",
            "machine_provider #3: name: required",
        ),
        (
            "[[machine_provider]]\nname = \"+gpu.lambdalabs\"\nkind = \"static\"\n",
            "machine_provider #3: name: must be 1 to 63 characters of [a-z0-9._-]",
        ),
        (
            "[[machine_provider]]\nname = \"rack\"\nkind = \"static\"\n",
            "machine_provider \"rack\": name: also the name of an earlier machine provider",
        ),
        (
            "[[machine_provider]]\nname = \"more\"\nkind = \"static\"\nhosts_file = \"x\"\n",
            "machine_provider \"more\": hosts_file: unknown field; known: name, kind",
        ),
        (
            "[[platform]]\nname = \"ours\"\nkind = \"default\"\nmachine_providers = [\"rack\"]\n",
            "platform \"ours\": name: also the name of an earlier platform",
        ),
        (
            "[[platform]]\nname = \"bare\"\nkind = \"default\"\n",
            "platform \"bare\": machine_providers: required",
        ),
    ];
    for (table, message) in cases {
        assert_eq!(refused(&scratch, &beside(&scratch, table)), message);
    }
    let text = format!("machine_provider = 3\n{}", doc(&scratch, &[], &[]));
    assert_eq!(
        refused(&scratch, &text),
        "machine_provider: must be an array of tables, written [[machine_provider]]"
    );
    let tables = [
        rack(),
        "[[machine_provider]]\nname = \"odd\"\nkind = \"listed\"\n".to_owned(),
        platform("ours", &["rack", "odd"]),
    ];
    assert_eq!(
        refused(
            &scratch,
            &doc(&scratch, &tables, &[&on(&["ours"], &scratch)])
        ),
        "machine_provider \"odd\": kind: builds a pool of listed hosts, not a machine provider"
    );
}

/// Asserts that a declared machine provider no platform names and a declared platform no
/// pool names are refused naming them: their keys would never reach a check. It catches a
/// typo in a list that leaves a provider silently unused.
#[test]
fn a_table_nothing_names_is_refused() {
    let scratch = Scratch::new("platform_unnamed");
    let spare = "[[machine_provider]]\nname = \"spare\"\nkind = \"static\"\n";
    assert_eq!(
        refused(&scratch, &beside(&scratch, spare)),
        "machine_provider \"spare\": named by no platform"
    );
    let idle = platform("idle", &["rack"]);
    assert_eq!(
        refused(&scratch, &beside(&scratch, &idle)),
        "platform \"idle\": named by no pool"
    );
}

/// Asserts the queue settings: with none given they are the defaults (`max_queue_wait`
/// 1h, `start_timeout` 15m, `path_grace` 15s); given they are read as written; and a zero
/// or a word is refused naming the key. On an agentless static pool each is refused as a
/// key of an elastic pool. It catches a default that drifts and a setting read as zero.
#[test]
fn pool_settings_default_and_parse() {
    let scratch = Scratch::new("platform_settings");
    let plain = resolved(
        &scratch,
        &doc(&scratch, &[], &[&PoolText::elastic(&scratch)]),
    );
    let settings = plain.pool("gpu").unwrap().settings;
    assert_eq!(settings.max_queue_wait, Duration::from_secs(3600));
    assert_eq!(settings.start_timeout, Duration::from_secs(900));
    assert_eq!(settings.path_grace, Duration::from_secs(15));
    let given = PoolText::elastic(&scratch)
        .set("max_queue_wait", &quote("30m"))
        .set("start_timeout", &quote("90s"))
        .set("path_grace", &quote("5s"));
    let settings = resolved(&scratch, &doc(&scratch, &[], &[&given]))
        .pool("gpu")
        .unwrap()
        .settings;
    assert_eq!(settings.max_queue_wait, Duration::from_secs(1800));
    assert_eq!(settings.start_timeout, Duration::from_secs(90));
    assert_eq!(settings.path_grace, Duration::from_secs(5));
    for key in ["max_queue_wait", "start_timeout", "path_grace"] {
        for bad in ["0s", "soon"] {
            let pool = PoolText::elastic(&scratch).set(key, &quote(bad));
            assert_eq!(
                refused(&scratch, &doc(&scratch, &[], &[&pool])),
                format!("pool \"gpu\": {key}: must be a positive duration such as 600s, 10m or 1h")
            );
        }
        let hosts = PoolText::static_pool(&scratch, "hosts").set(key, &quote("1m"));
        assert_eq!(
            refused(&scratch, &doc(&scratch, &[], &[&hosts])),
            format!("pool \"hosts\": {key}: only for an elastic pool")
        );
    }
}

/// The pool `gpu` on `ours` with `max_machines = 4` and `limits` as its
/// `provider_limits`.
fn limited(scratch: &Scratch, limits: &str) -> String {
    limited_beside(scratch, limits, &[])
}

/// [`limited`], with `more` tables declared beside `ours`.
fn limited_beside(scratch: &Scratch, limits: &str, more: &[String]) -> String {
    let mut tables = ours(scratch);
    tables.extend_from_slice(more);
    let pool = on(&["ours"], scratch)
        .without("max_instances")
        .set("max_machines", "4")
        .set("provider_limits", limits);
    doc(scratch, &tables, &[&pool])
}

/// Asserts the limits of design 4.2 (without the keys left to their own changes) read to
/// their typed values, a share without `max_machines` included; and that each of these is
/// refused naming the pool and the key: a limit for an undeclared provider, for a declared
/// one the pool's platforms do not reach, and for a `static` one; `max_machines` above the
/// pool's; `max_spot_machines` above `max_machines`; a repeated, empty, missing or
/// mistyped instance type; a zero price or share; an entry or a table that is not a table;
/// and an unknown key (`catalog_ttl`; the message lists `quota_backoff`, `acquire_retry`, `catalog_refresh`, `offer_cooldown` and `size_tolerance_percent` among the known ones). It catches a
/// limit that is not checked against the pool, or a key read as nothing.
#[test]
fn provider_limits_are_validated() {
    let scratch = Scratch::new("platform_limits");
    let accepted = "{ lambda = { allowed_instance_types = [\"gpu_1x_a10\", \"gpu_8x_h100\"], max_price_cents_per_hour = 5000, max_machines = 4, max_spot_machines = 2 } }";
    let read = resolved(&scratch, &limited(&scratch, accepted));
    let limits = &read.pool("gpu").unwrap().provider_limits["lambda"];
    let types = limits.allowed_instance_types.as_deref().unwrap();
    assert_eq!(types, ["gpu_1x_a10", "gpu_8x_h100"]);
    assert_eq!(limits.max_price_cents_per_hour, Some(5000));
    assert_eq!(limits.max_machines, Some(4));
    assert_eq!(limits.max_spot_machines, Some(2));
    let spot_only = resolved(
        &scratch,
        &limited(&scratch, "{ lambda = { max_spot_machines = 9 } }"),
    );
    let limits = &spot_only.pool("gpu").unwrap().provider_limits["lambda"];
    assert_eq!(limits.max_spot_machines, Some(9));
    assert_eq!(limits.allowed_instance_types, None);
    let at = "pool \"gpu\": provider_limits.";
    let cases = [
        (
            "{ aws = { max_machines = 1 } }",
            "aws: not a machine provider of the pool's platforms",
        ),
        (
            "{ rack = { max_machines = 1 } }",
            "rack: a static machine provider launches nothing, so it has nothing to limit",
        ),
        (
            "{ lambda = { max_machines = 5 } }",
            "lambda.max_machines: must not be above the pool's max_machines (4)",
        ),
        (
            "{ lambda = { max_machines = 2, max_spot_machines = 3 } }",
            "lambda.max_spot_machines: must not be above max_machines (2)",
        ),
        (
            "{ lambda = { allowed_instance_types = [\"a\", \"a\"] } }",
            "lambda.allowed_instance_types: names \"a\" twice",
        ),
        (
            "{ lambda = { allowed_instance_types = [\"\"] } }",
            "lambda.allowed_instance_types: holds an empty name",
        ),
        (
            "{ lambda = { allowed_instance_types = [] } }",
            "lambda.allowed_instance_types: must name at least one",
        ),
        (
            "{ lambda = { allowed_instance_types = \"a\" } }",
            "lambda.allowed_instance_types: must be an array of strings",
        ),
        (
            "{ lambda = { allowed_instance_types = [1] } }",
            "lambda.allowed_instance_types: must be an array of strings",
        ),
        (
            "{ lambda = { max_price_cents_per_hour = 0 } }",
            "lambda.max_price_cents_per_hour: must be at least 1",
        ),
        (
            "{ lambda = { max_machines = 0 } }",
            "lambda.max_machines: must be at least 1",
        ),
        ("{ lambda = 3 }", "lambda: must be a table"),
        (
            "{ lambda = { catalog_ttl = \"1h\" } }",
            "lambda.catalog_ttl: unknown field; known: allowed_instance_types, max_price_cents_per_hour, max_machines, max_spot_machines, quota_backoff, acquire_retry, catalog_refresh, offer_cooldown, size_tolerance_percent",
        ),
    ];
    for (limits, message) in cases {
        assert_eq!(
            refused(&scratch, &limited(&scratch, limits)),
            format!("{at}{message}")
        );
    }
    let west = [
        lambda(&scratch, "lambda-b"),
        platform("west", &["rack", "lambda-b"]),
    ];
    let unreachable = limited_beside(&scratch, "{ lambda-b = { max_machines = 1 } }", &west);
    assert_eq!(
        refused(&scratch, &unreachable),
        format!("{at}lambda-b: not a machine provider of the pool's platforms")
    );
    assert_eq!(
        refused(&scratch, &limited(&scratch, "3")),
        "pool \"gpu\": provider_limits: must be a table"
    );
    let hosts = PoolText::static_pool(&scratch, "hosts").set("provider_limits", "{}");
    assert_eq!(
        refused(&scratch, &doc(&scratch, &[], &[&hosts])),
        "pool \"hosts\": provider_limits: only for an elastic pool"
    );
}

/// Asserts that `max_machines` and `max_instances` are one cap: either spelling resolves
/// to the same cap and the same description, giving both is refused naming both, a zero
/// is refused under the spelling given, and an agentless static pool refuses the new
/// spelling as it refuses the old. It catches a second cap that is read and ignored.
#[test]
fn max_instances_and_max_machines_are_the_same_cap() {
    let scratch = Scratch::new("platform_cap");
    let old = resolved(
        &scratch,
        &doc(&scratch, &[], &[&PoolText::elastic(&scratch)]),
    );
    let renamed = PoolText::elastic(&scratch)
        .without("max_instances")
        .set("max_machines", "4");
    let new = resolved(&scratch, &doc(&scratch, &[], &[&renamed]));
    assert_eq!(old.pool("gpu").unwrap().max_machines, Some(4));
    assert_eq!(old.describe(), new.describe());
    let both = PoolText::elastic(&scratch).set("max_machines", "4");
    assert_eq!(
        refused(&scratch, &doc(&scratch, &[], &[&both])),
        "pool \"gpu\": max_machines: and max_instances are one cap by two names; give one"
    );
    let zero = renamed.set("max_machines", "0");
    assert_eq!(
        refused(&scratch, &doc(&scratch, &[], &[&zero])),
        "pool \"gpu\": max_machines: must be at least 1"
    );
    let hosts = PoolText::static_pool(&scratch, "hosts").set("max_machines", "2");
    assert_eq!(
        refused(&scratch, &doc(&scratch, &[], &[&hosts])),
        "pool \"hosts\": max_machines: only for an elastic pool"
    );
}

/// Asserts that a platform may list its machine providers in any order, a launching one
/// before `static` included, and a pool its platforms in any order, each resolved as
/// written (the job pass walks them in that order); and that a pool whose platforms reach no
/// launching provider, or two, is refused. It catches an order the file asks for that is
/// refused or reordered, and a pool with no launching provider, or two, that starts.
#[test]
fn any_machine_provider_order_resolves_as_written() {
    let scratch = Scratch::new("platform_order");
    let reversed = [
        rack(),
        lambda(&scratch, "lambda"),
        platform("ours", &["lambda", "rack"]),
    ];
    let text = doc(&scratch, &reversed, &[&on(&["ours"], &scratch)]);
    let ours = resolved(&scratch, &text);
    assert_eq!(ours.platforms[0].machine_providers, ["lambda", "rack"]);
    let split = [
        rack(),
        lambda(&scratch, "lambda"),
        platform("cloud", &["lambda"]),
        platform("ours", &["rack"]),
    ];
    let text = doc(&scratch, &split, &[&on(&["cloud", "ours"], &scratch)]);
    let pool = resolved(&scratch, &text).pools[0].platforms.clone();
    assert_eq!(pool, [PlatformId::new("cloud"), PlatformId::new("ours")]);
    let one = "a pool's platforms reach exactly one machine provider that launches in this version; a pool of registered hosts alone is kind = \"joined\"";
    let text = doc(&scratch, &split, &[&on(&["ours"], &scratch)]);
    let cloud_unnamed = refused(&scratch, &text);
    assert_eq!(cloud_unnamed, format!("pool \"gpu\": platforms: {one}"));
    let two = [
        rack(),
        lambda(&scratch, "lambda-a"),
        lambda(&scratch, "lambda-b"),
        platform("east", &["rack", "lambda-a"]),
        platform("west", &["rack", "lambda-b"]),
    ];
    let text = doc(&scratch, &two, &[&on(&["east", "west"], &scratch)]);
    assert_eq!(
        refused(&scratch, &text),
        format!("pool \"gpu\": platforms: {one}")
    );
    let cloud = [lambda(&scratch, "lambda"), platform("cloud", &["lambda"])];
    let alone = resolved(
        &scratch,
        &doc(&scratch, &cloud, &[&on(&["cloud"], &scratch)]),
    );
    assert_eq!(alone.platforms[0].machine_providers, ["lambda"]);
}

/// Asserts that a machine provider's key file is read when a pool is built from it, as a
/// pool's provider table is: a missing file and one others can read are refused naming
/// `api_key_file` under the machine provider, and the key in the file (a sentinel) is in
/// neither refusal nor in any description. It catches a secret echoed by the new tables.
#[test]
fn the_secret_paths_are_read_not_values() {
    let scratch = Scratch::new("platform_secret");
    let missing = format!(
        "[[machine_provider]]\nname = \"lambda\"\nkind = \"lambdalabs\"\napi_key_file = {}\n",
        quoted(&scratch.path("absent-key"))
    );
    let tables = [rack(), missing, platform("ours", &["rack", "lambda"])];
    let said = refused(
        &scratch,
        &doc(&scratch, &tables, &[&on(&["ours"], &scratch)]),
    );
    assert!(
        said.starts_with("machine_provider \"lambda\": api_key_file: "),
        "{said}"
    );
    let open_key = scratch.write("open-key", API_KEY, 0o644);
    let open = format!(
        "[[machine_provider]]\nname = \"lambda\"\nkind = \"lambdalabs\"\napi_key_file = {}\n",
        quoted(&open_key)
    );
    let tables = [rack(), open, platform("ours", &["rack", "lambda"])];
    let said = refused(
        &scratch,
        &doc(&scratch, &tables, &[&on(&["ours"], &scratch)]),
    );
    assert!(
        said.starts_with("machine_provider \"lambda\": api_key_file: "),
        "{said}"
    );
    assert!(!said.contains(API_KEY), "{said}");
    let described = [
        resolved(
            &scratch,
            &doc(&scratch, &ours(&scratch), &[&on(&["ours"], &scratch)]),
        ),
        resolved(
            &scratch,
            &doc(&scratch, &[], &[&PoolText::elastic(&scratch)]),
        ),
        resolved(&scratch, &two_elastic(&scratch)),
    ];
    for one in described {
        let text = one.describe();
        assert!(!text.contains(API_KEY), "{text}");
        assert!(!text.contains(scratch.path("").to_str().unwrap()), "{text}");
    }
}

/// Asserts the descriptions of the shorthand file, the explicit-tables file and the file
/// with two elastic providers against their goldens, which are reviewed with the change
/// that alters them. It catches any change in what is resolved or in how it reads.
#[test]
fn describe_goldens_match() {
    let scratch = Scratch::new("platform_goldens");
    let cases = [
        (
            doc(&scratch, &[], &[&PoolText::elastic(&scratch)]),
            include_str!("golden/config_shorthand.txt"),
        ),
        (
            doc(&scratch, &ours(&scratch), &[&on(&["ours"], &scratch)]),
            include_str!("golden/config_explicit.txt"),
        ),
        (
            two_elastic(&scratch),
            include_str!("golden/config_two_elastic.txt"),
        ),
    ];
    for (text, golden) in cases {
        assert_eq!(resolved(&scratch, &text).describe(), golden);
    }
}
