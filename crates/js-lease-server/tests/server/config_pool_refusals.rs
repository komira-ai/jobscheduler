//! What start-up refuses in a `[[pool]]` table (design 4.4): each refusal with its exact
//! message, naming the pool and the field, and no message or `Debug` holding a secret.

use std::os::unix::fs::PermissionsExt;

use js_lease_server::{Config, Kinds, OwnerOnlyFiles};

use crate::config_support::{
    API_KEY, OAUTH_SECRET, PoolText, clock, file, load, oauth_file, quote, quoted, refusal, server,
    server_with_agents,
};
use crate::support::Scratch;

/// The refusal of a file whose elastic pool is `pool`.
fn elastic(scratch: &Scratch, pool: &PoolText) -> String {
    refusal(scratch, &file(&server_with_agents(scratch), &[pool]))
}

/// The refusal of a file whose static pool is `pool`.
fn hosts(scratch: &Scratch, pool: &PoolText) -> String {
    refusal(scratch, &file(&server(scratch, ""), &[pool]))
}

/// Asserts that a pool without a usable name, class or provider is refused: before the
/// name is known the pool is "#n", a name outside the id alphabet is refused with the
/// alphabet and without printing it, and a provider table needs a `kind`.
#[test]
fn a_pool_needs_a_name_a_class_and_a_provider() {
    let scratch = Scratch::new("pool_basics");
    let pool = PoolText::elastic(&scratch);
    let said = elastic(&scratch, &pool.without("name"));
    assert_eq!(said, "pool #1: name: required");
    let said = elastic(&scratch, &PoolText::elastic(&scratch).set("name", "5"));
    assert_eq!(said, "pool #1: name: must be a string");
    let upper = PoolText::elastic(&scratch).set("name", &quote("PLANTED_NAME"));
    let said = elastic(&scratch, &upper);
    assert_eq!(
        said,
        "pool #1: name: must be 1 to 63 characters of [a-z0-9._-]"
    );
    let bad_class = PoolText::elastic(&scratch).set("class", &quote("Whole Node"));
    let said = elastic(&scratch, &bad_class);
    assert_eq!(
        said,
        "pool \"gpu\": class: must be 1 to 63 characters of [a-z0-9._-]"
    );
    let no_class = PoolText::elastic(&scratch).without("class");
    assert_eq!(
        elastic(&scratch, &no_class),
        "pool \"gpu\": class: required"
    );
    let no_provider = PoolText::elastic(&scratch).without("provider");
    assert_eq!(
        elastic(&scratch, &no_provider),
        "pool \"gpu\": provider: required"
    );
    let wrong = PoolText::elastic(&scratch).set("provider", "5");
    assert_eq!(
        elastic(&scratch, &wrong),
        "pool \"gpu\": provider: must be a table"
    );
    let no_kind = PoolText::elastic(&scratch).set("provider", "{ ssh_key = \"k\" }");
    assert_eq!(
        elastic(&scratch, &no_kind),
        "pool \"gpu\": provider.kind: required"
    );
    let kind_number = PoolText::elastic(&scratch).set("provider", "{ kind = 5 }");
    assert_eq!(
        elastic(&scratch, &kind_number),
        "pool \"gpu\": provider.kind: must be a string"
    );
}

/// Asserts that a kind the server does not have is refused with the kinds it has, for the
/// provider and for the network mode (`none` and `script` are not modes), and that the
/// server as shipped has `lambdalabs` and `static`. It catches a kind name read as a default.
#[test]
fn an_unknown_kind_is_refused_with_the_known_ones() {
    let scratch = Scratch::new("pool_kinds");
    let key = scratch.path("api-key");
    let lambda = PoolText::elastic(&scratch).set(
        "provider",
        &format!("{{ kind = \"lambda\", api_key_file = {} }}", quoted(&key)),
    );
    assert_eq!(
        elastic(&scratch, &lambda),
        "pool \"gpu\": provider.kind: \"lambda\" is not a kind; known: any-mode, broken-floor, lambdalabs, static"
    );
    for mode in ["none", "script"] {
        let pool = PoolText::elastic(&scratch).set("network", &format!("{{ kind = {mode:?} }}"));
        assert_eq!(
            elastic(&scratch, &pool),
            format!(
                "pool \"gpu\": network.kind: \"{mode}\" is not a kind; known: default, tailscale"
            )
        );
    }
    let no_kind = PoolText::elastic(&scratch).set("network", "{ tag = \"tag:js\" }");
    assert_eq!(
        elastic(&scratch, &no_kind),
        "pool \"gpu\": network.kind: required"
    );
    let wrong = PoolText::elastic(&scratch).set("network", "5");
    assert_eq!(
        elastic(&scratch, &wrong),
        "pool \"gpu\": network: must be a table"
    );
    let pool = PoolText::elastic(&scratch);
    let text = file(&server_with_agents(&scratch), &[&pool]);
    let path = scratch.write("js.toml", &text, 0o644);
    let shipped = Config::from_config_file(&path, &Kinds::standard(), &OwnerOnlyFiles, clock());
    assert_eq!(
        shipped.err().unwrap().to_string(),
        "pool \"gpu\": provider.types: required"
    );
    let mut names = Kinds::standard().names();
    names.sort_unstable();
    assert_eq!(names, ["lambdalabs", "static"]);
}

/// Asserts the unknown fields of a provider table and of a network table, each naming
/// the fields of that table, and that a secret written inline has no key to go in: the
/// refusal names the key and holds none of the secret.
#[test]
fn unknown_provider_and_network_fields_are_refused_and_no_secret_has_a_key() {
    let scratch = Scratch::new("pool_unknown");
    let key = quoted(&scratch.path("api-key"));
    let typo = format!("{{ kind = \"lambdalabs\", api_keyfile = {key} }}");
    let pool = PoolText::elastic(&scratch).set("provider", &typo);
    assert_eq!(
        elastic(&scratch, &pool),
        "pool \"gpu\": provider.api_keyfile: unknown field; known: api_key_file, regions, types, ssh_key, api_url"
    );
    let inline =
        format!("{{ kind = \"lambdalabs\", api_key_file = {key}, api_key = \"{API_KEY}\" }}");
    let said = elastic(
        &scratch,
        &PoolText::elastic(&scratch).set("provider", &inline),
    );
    assert!(
        said.starts_with("pool \"gpu\": provider.api_key: unknown field; known: "),
        "{said}"
    );
    assert!(!said.contains(API_KEY), "{said}");
    let oauth = oauth_file(&scratch);
    let status = format!(
        "{{ kind = \"tailscale\", oauth_client_file = {oauth}, tag = \"tag:js\", status_token_file = \"x\" }}"
    );
    let pool = PoolText::elastic(&scratch).set("network", &status);
    assert_eq!(
        elastic(&scratch, &pool),
        "pool \"gpu\": network.status_token_file: unknown field; known: kind, oauth_client_file, tag, binary, api_url"
    );
    let extra =
        PoolText::elastic(&scratch).set("network", "{ kind = \"default\", tag = \"tag:js\" }");
    assert_eq!(
        elastic(&scratch, &extra),
        "pool \"gpu\": network.tag: unknown field; known: kind"
    );
}

/// Asserts the credential files of a pool: a key file that is missing, or that others can
/// read or write, or that is not a regular file, is refused naming the pool and
/// `provider.api_key_file` with the rule (mode 0600 or 0400); a provider's own field error
/// is placed under `provider` with its index, as the type names of a table are.
#[test]
fn a_credential_file_must_be_the_services_own_private_file() {
    let scratch = Scratch::new("pool_credentials");
    let path = scratch.path("api-key");
    let pool = PoolText::elastic(&scratch);
    assert!(load(&scratch, &file(&server_with_agents(&scratch), &[&pool])).is_ok());
    for mode in [0o400, 0o600] {
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(mode)).unwrap();
        assert!(load(&scratch, &file(&server_with_agents(&scratch), &[&pool])).is_ok());
    }
    let prefix = format!(
        "pool \"gpu\": provider.api_key_file: {} must be a regular file owned by the user running the server and readable only by that user (mode 0600 or 0400), not mode ",
        path.display()
    );
    for mode in [0o644, 0o640, 0o660, 0o604, 0o700, 0o777] {
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(mode)).unwrap();
        let said = elastic(&scratch, &pool);
        assert!(said.starts_with(&prefix), "{said}");
        assert!(
            said.contains(&format!("not mode {mode:04o} owned by uid ")),
            "{said}"
        );
        assert!(!said.contains(API_KEY), "{said}");
    }
    let absent = scratch.path("absent-key");
    let missing = PoolText::elastic(&scratch).set(
        "provider",
        &format!(
            "{{ kind = \"lambdalabs\", api_key_file = {} }}",
            quoted(&absent)
        ),
    );
    assert_eq!(
        elastic(&scratch, &missing),
        format!(
            "pool \"gpu\": provider.api_key_file: {}: No such file or directory (os error 2)",
            absent.display()
        )
    );
    let dir = scratch.path("a-dir");
    std::fs::create_dir(&dir).unwrap();
    std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o600)).unwrap();
    let not_file = PoolText::elastic(&scratch).set(
        "provider",
        &format!(
            "{{ kind = \"lambdalabs\", api_key_file = {} }}",
            quoted(&dir)
        ),
    );
    assert!(elastic(&scratch, &not_file).contains("must be a regular file"));
    let no_file = PoolText::elastic(&scratch).set("provider", "{ kind = \"lambdalabs\" }");
    assert_eq!(
        elastic(&scratch, &no_file),
        "pool \"gpu\": provider.api_key_file: required"
    );
    let rows = "{ kind = \"lambdalabs\", api_key_file = KEY, types = [{ name = \"gpu_1x_a10\" }, { name = \"Gpu-2x\" }] }";
    let rows = rows.replace("KEY", &quoted(&scratch.path("api-key")));
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
    let bad_types = PoolText::elastic(&scratch).set("provider", &rows);
    assert_eq!(
        elastic(&scratch, &bad_types),
        "pool \"gpu\": provider.types[2].name: must be 1 to 64 lower case letters, digits or '_'"
    );
}

/// Asserts the OAuth client file of the Tailscale mode and its tag: a world-readable
/// file, a file that is not a client file and a missing file are refused naming
/// `network.oauth_client_file`, without the secret; the tag must be `tag:` and a name; the
/// file and the tag are required.
#[test]
fn the_tailscale_mode_needs_a_private_client_file_and_a_tag() {
    let scratch = Scratch::new("pool_tailscale");
    let oauth = oauth_file(&scratch);
    let path = scratch.path("oauth");
    let with = |network: String| {
        let pool = PoolText::elastic(&scratch).set("network", &network);
        elastic(&scratch, &pool)
    };
    let tailscale = |file: &str, tag: &str| {
        format!("{{ kind = \"tailscale\", oauth_client_file = {file}, tag = \"{tag}\" }}")
    };
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();
    let said = with(tailscale(&oauth, "tag:js"));
    let head = "pool \"gpu\": network.oauth_client_file: network: oauth client file: ";
    assert!(said.starts_with(head), "{said}");
    assert!(said.contains("not mode 0644"), "{said}");
    assert!(!said.contains(OAUTH_SECRET), "{said}");
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
    std::fs::write(
        &path,
        format!("client_id = 5 PLANTED-AFTER {OAUTH_SECRET}\n"),
    )
    .unwrap();
    let said = with(tailscale(&oauth, "tag:js"));
    assert_eq!(
        said,
        format!(
            "{head}{}: is not a valid client file (line 1)",
            path.display()
        )
    );
    let absent = quoted(&scratch.path("absent-oauth"));
    let said = with(tailscale(&absent, "tag:js"));
    assert!(said.starts_with(head), "{said}");
    assert!(!said.contains("PLANTED"), "{said}");
    let oauth = oauth_file(&scratch);
    let said = with(tailscale(&oauth, "js-agent"));
    assert_eq!(
        said,
        "pool \"gpu\": network.tag: must be \"tag:\" and letters, digits or '-'"
    );
    let said = with("{ kind = \"tailscale\", tag = \"tag:js\" }".to_owned());
    assert_eq!(said, "pool \"gpu\": network.oauth_client_file: required");
    let said = with(format!(
        "{{ kind = \"tailscale\", oauth_client_file = {oauth} }}"
    ));
    assert_eq!(said, "pool \"gpu\": network.tag: required");
}

/// Asserts the limits and the spend caps of an elastic pool, each with its message: a
/// missing `max_instances`, `max_spend_per_hour` or `max_spend_per_day`; an instance
/// count of zero, of 256 and of 300 (the join-token table holds 255 waiting tokens);
/// spend values that are not a decimal with at most two places, or are zero; a day cap
/// below the hour cap; a value of the wrong type. 255 and a day cap equal to the hour cap
/// are accepted.
#[test]
fn an_elastic_pool_has_bounded_instances_and_spend() {
    let scratch = Scratch::new("pool_limits");
    for key in ["max_instances", "max_spend_per_hour", "max_spend_per_day"] {
        let pool = PoolText::elastic(&scratch).without(key);
        assert_eq!(
            elastic(&scratch, &pool),
            format!("pool \"gpu\": {key}: required")
        );
    }
    let instances = [
        ("0", "must be at least 1"),
        ("-1", "must not be negative"),
        ("256", "256 exceeds 255"),
        ("300", "300 exceeds 255"),
        ("\"4\"", "must be an integer"),
    ];
    for (value, reason) in instances {
        let pool = PoolText::elastic(&scratch).set("max_instances", value);
        assert_eq!(
            elastic(&scratch, &pool),
            format!("pool \"gpu\": max_instances: {reason}")
        );
    }
    let places = "must be a decimal with at most two places, such as \"12.00\"";
    let spends = [
        ("\"12.345\"", places),
        ("\"abc\"", places),
        ("\"\"", places),
        ("\"-1\"", places),
        ("\".5\"", places),
        ("\"12.\"", places),
        ("\"99999999999999999999\"", places),
        ("\"0\"", "must be above zero"),
        ("\"0.00\"", "must be above zero"),
        ("12", "must be a string"),
    ];
    for key in ["max_spend_per_hour", "max_spend_per_day"] {
        for (value, reason) in spends {
            let pool = PoolText::elastic(&scratch).set(key, value);
            assert_eq!(
                elastic(&scratch, &pool),
                format!("pool \"gpu\": {key}: {reason}"),
                "{value}"
            );
        }
    }
    let low_day = PoolText::elastic(&scratch).set("max_spend_per_day", &quote("11.99"));
    assert_eq!(
        elastic(&scratch, &low_day),
        "pool \"gpu\": max_spend_per_day: must not be below max_spend_per_hour"
    );
    let equal = PoolText::elastic(&scratch).set("max_spend_per_day", &quote("12.50"));
    let largest = equal
        .set("max_instances", "255")
        .set("max_spend_per_hour", &quote("12.5"));
    let config = load(&scratch, &file(&server_with_agents(&scratch), &[&largest])).unwrap();
    let plan = config.elastic_pool().unwrap();
    assert_eq!(plan.max_instances, 255);
    assert_eq!(plan.max_spend_per_hour_cents, 1250);
    assert_eq!(plan.max_spend_per_day_cents, 1250);
}

/// Asserts the `exec`, `launch`, `idle`, `boot_timeout` and `join_token_file` keys of an
/// elastic pool: an unknown mode names the three choices, a mode the provider kind cannot
/// launch is refused naming the kind, `launch` is `plan` or `live`, a duration that is not
/// one is refused, and a join token file is read owner-only. It catches a macOS pool on a
/// provider that cannot launch one.
#[test]
fn exec_launch_and_timing_keys_are_checked() {
    let scratch = Scratch::new("pool_exec");
    let case =
        |key: &str, value: &str| elastic(&scratch, &PoolText::elastic(&scratch).set(key, value));
    assert_eq!(
        case("exec", "\"vm\""),
        "pool \"gpu\": exec: must be container, sandbox-exec or none"
    );
    assert_eq!(case("exec", "5"), "pool \"gpu\": exec: must be a string");
    assert_eq!(
        case("exec", "\"sandbox-exec\""),
        "pool \"gpu\": exec: sandbox-exec hosts cannot be launched by provider kind \"lambdalabs\""
    );
    assert_eq!(
        case("launch", "\"now\""),
        "pool \"gpu\": launch: must be plan or live"
    );
    assert_eq!(
        case("launch", "true"),
        "pool \"gpu\": launch: must be a string"
    );
    let duration = "must be a positive duration such as 600s, 10m or 1h";
    assert_eq!(
        case("idle", "\"later\""),
        format!("pool \"gpu\": idle: {duration}")
    );
    assert_eq!(
        case("boot_timeout", "\"0s\""),
        format!("pool \"gpu\": boot_timeout: {duration}")
    );
    let absent = scratch.path("absent-join");
    let said = case("join_token_file", &quoted(&absent));
    assert_eq!(
        said,
        format!(
            "pool \"gpu\": join_token_file: {}: No such file or directory (os error 2)",
            absent.display()
        )
    );
    let same = scratch.write("same-token", crate::support::TOKEN, 0o600);
    let said = case("join_token_file", &quoted(&same));
    assert!(
        said.starts_with("pool \"gpu\": join_token_file: "),
        "{said}"
    );
    assert!(said.ends_with("the two must differ"), "{said}");
    let joins = scratch.write("joins", "join-token-A-0123456789-abcdefghijklmnop\n", 0o600);
    let pool = PoolText::elastic(&scratch).set("join_token_file", &quoted(&joins));
    assert!(load(&scratch, &file(&server_with_agents(&scratch), &[&pool])).is_ok());
}

/// Asserts that a kind's floor that no pool can have (GPUs and no class of them) is
/// refused naming `provider`, so a pool that could admit nobody does not start.
#[test]
fn a_floor_that_no_host_can_meet_is_refused() {
    let scratch = Scratch::new("pool_floor");
    let pool = PoolText::elastic(&scratch).set("provider", "{ kind = \"broken-floor\" }");
    let said = elastic(&scratch, &pool);
    assert!(said.starts_with("pool \"gpu\": provider: "), "{said}");
}

/// Asserts that pools may not share a name, that a second static pool and a second
/// registering pool are refused with the design's message, and that a static pool refuses
/// every key that belongs to an elastic one, naming the key; and that `launch = "live"`
/// on a pool whose provider launches nothing is refused naming `launch`.
#[test]
fn pools_are_counted_and_a_static_pool_launches_nothing() {
    let scratch = Scratch::new("pool_counts");
    let server = server_with_agents(&scratch);
    let one = PoolText::static_pool(&scratch, "dup");
    let two = PoolText::static_pool(&scratch, "dup");
    assert_eq!(
        refusal(&scratch, &file(&server, &[&one, &two])),
        "pool \"dup\": name: also the name of an earlier pool"
    );
    let other = PoolText::static_pool(&scratch, "other");
    assert_eq!(
        refusal(&scratch, &file(&server, &[&one, &other])),
        "pool \"other\": only one static pool is served in this version"
    );
    let first = PoolText::elastic(&scratch);
    let second = PoolText::elastic(&scratch).set("name", &quote("b"));
    assert_eq!(
        refusal(&scratch, &file(&server, &[&first, &second])),
        "pool \"b\": only one pool may register agents in this version"
    );
    let live = PoolText::static_pool(&scratch, "hosts").set("launch", "\"live\"");
    assert_eq!(
        hosts(&scratch, &live),
        "pool \"hosts\": launch: live needs an elastic provider; this kind launches nothing"
    );
    let plan = PoolText::static_pool(&scratch, "hosts").set("launch", "\"plan\"");
    assert!(load(&scratch, &file(&server_with_agents(&scratch), &[&plan])).is_ok());
    let elastic_keys = [
        ("exec", "\"none\""),
        ("max_instances", "4"),
        ("max_spend_per_hour", "\"1.00\""),
        ("max_spend_per_day", "\"2.00\""),
        ("idle", "\"1m\""),
        ("max_age", "\"6h\""),
        ("min_host_lifetime", "\"1m\""),
        ("boot_timeout", "\"1m\""),
        ("join_token_file", "\"/x\""),
        ("network", "{ kind = \"default\" }"),
    ];
    for (key, value) in elastic_keys {
        let pool = PoolText::static_pool(&scratch, "hosts").set(key, value);
        assert_eq!(
            hosts(&scratch, &pool),
            format!("pool \"hosts\": {key}: only for an elastic pool")
        );
    }
}

/// Asserts the static kind's table: a missing or mistyped `hosts_file`, an unknown field,
/// a file that is not there and a file that lists no host are each refused naming the pool
/// and `provider.hosts_file` (or the unknown field).
#[test]
fn the_static_kind_checks_its_hosts_file() {
    let scratch = Scratch::new("pool_static");
    let set = |provider: &str| {
        let pool = PoolText::static_pool(&scratch, "hosts").set("provider", provider);
        hosts(&scratch, &pool)
    };
    assert_eq!(
        set("{ kind = \"static\" }"),
        "pool \"hosts\": provider.hosts_file: required"
    );
    assert_eq!(
        set("{ kind = \"static\", hosts_file = 5 }"),
        "pool \"hosts\": provider.hosts_file: must be a string"
    );
    assert_eq!(
        set("{ kind = \"static\", hosts_fil = \"/x\" }"),
        "pool \"hosts\": provider.hosts_fil: unknown field; known: hosts_file"
    );
    let absent = scratch.path("no-hosts.toml");
    let said = set(&format!(
        "{{ kind = \"static\", hosts_file = {} }}",
        quoted(&absent)
    ));
    assert_eq!(
        said,
        format!(
            "pool \"hosts\": provider.hosts_file: {}: No such file or directory (os error 2)",
            absent.display()
        )
    );
    let empty = scratch.write("empty-hosts.toml", "", 0o644);
    let said = set(&format!(
        "{{ kind = \"static\", hosts_file = {} }}",
        quoted(&empty)
    ));
    assert_eq!(
        said,
        format!(
            "pool \"hosts\": provider.hosts_file: {}: the pool lists no host",
            empty.display()
        )
    );
}

/// Asserts that no refusal and no `Debug` holds a secret the test planted: the content of
/// the key and OAuth files, a secret written inline where a path belongs, and a syntax
/// error whose value is the secret. The plan of a pool that did build prints its OAuth
/// client as an id and `***`. It catches an error that quotes a value, as the TOML parser's
/// own messages do.
#[test]
fn no_message_and_no_debug_holds_a_planted_secret() {
    let scratch = Scratch::new("pool_secrets");
    let server = server_with_agents(&scratch);
    let planted = "PLANTED-INLINE-VALUE";
    let texts = [
        format!(
            "{server}\n[[pool]]\nname = \"gpu\"\nclass = \"whole-node\"\nmax_instances = \"{planted}\"\n"
        ),
        format!("{server}\n[[pool]]\nname = \"gpu\" {planted}\n"),
        format!("{server}\n[[pool]]\nname = \"gpu\"\nclass = \"{planted}\"\n"),
        format!(
            "{server}\n[[pool]]\nname = \"gpu\"\nclass = \"whole-node\"\nlaunch = \"{planted}\"\n"
        ),
        format!(
            "{server}\n[[pool]]\nname = \"gpu\"\nclass = \"whole-node\"\nexec = \"{planted}\"\nprovider = {{}}\n"
        ),
        format!(
            "{server}\n[[pool]]\nname = \"gpu\"\nclass = \"whole-node\"\napi_key = \"{planted}\"\n"
        ),
        format!("{server}\n[[pool]]\nname = \"gpu\"\nnetwork = \"{planted}\"\n"),
        format!("[server]\nboot = \"{planted}\"\n"),
        format!("[server]\ntick = \"{planted}\"\n"),
        format!("[server]\nstate_file = 5\nclient_token_file = \"{planted}\"\n"),
    ];
    for text in texts {
        let said = refusal(&scratch, &text);
        assert!(!said.contains(planted), "{said}");
    }
    let oauth = oauth_file(&scratch);
    let tailscale =
        format!("{{ kind = \"tailscale\", oauth_client_file = {oauth}, tag = \"tag:js-agent\" }}");
    let pool = PoolText::elastic(&scratch).set("network", &tailscale);
    let config = load(&scratch, &file(&server, &[&pool])).unwrap();
    let shown = format!("{:?}", config.elastic_pool().unwrap());
    assert!(shown.contains("tag:js-agent"), "{shown}");
    assert!(shown.contains("***"), "{shown}");
    assert!(!shown.contains(OAUTH_SECRET), "{shown}");
    assert!(!shown.contains(API_KEY), "{shown}");
}
