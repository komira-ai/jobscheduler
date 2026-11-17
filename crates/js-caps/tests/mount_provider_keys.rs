//! The capability keys of node-local mount providers, `mount-provider.<name>`: a family of
//! keys, not a fixed name, so none is in `DETECTED_KEYS`. A request names one as an exact
//! key, a node's report keeps one, and the bare prefix names no provider.

use js_caps::{
    DETECTED_KEYS, FLAG_SET, MOUNT_PROVIDER_PREFIX, NodeCaps, ReportError, Request, RequestError,
    Unmet, is_mount_provider_key, mount_provider_key, property_name,
};

fn req(props: &[(&str, &str)]) -> Result<Request, RequestError> {
    Request::parse(props.iter().copied())
}

/// A node reporting `entries` beside an arch.
fn node(entries: &[(&str, &str)]) -> Result<NodeCaps, ReportError> {
    let arch = [("arch", "x86_64")];
    NodeCaps::from_report(arch.iter().chain(entries).copied())
}

/// Asserts the spelling of the prefix and of the key made from a name, and which keys
/// `is_mount_provider_key` accepts: the prefix and at least one more byte, case and all. It
/// catches a key built without its dot, and a bare prefix, an empty name, a prefix in
/// another case or another key that merely contains the prefix read as a provider.
#[test]
fn a_mount_provider_key_is_the_prefix_and_a_name() {
    assert_eq!(MOUNT_PROVIDER_PREFIX, "mount-provider.");
    assert_eq!(mount_provider_key("tierstore"), "mount-provider.tierstore");
    assert_eq!(mount_provider_key(""), "mount-provider.");
    let table = [
        ("mount-provider.tierstore", true),
        ("mount-provider.a", true),
        ("mount-provider.a.b", true),
        ("mount-provider.", false),
        ("mount-provider", false),
        ("mount-provider-tierstore", false),
        ("Mount-Provider.tierstore", false),
        ("x.mount-provider.tierstore", false),
        ("layered-rootfs", false),
        ("", false),
    ];
    for (key, expected) in table {
        assert_eq!(is_mount_provider_key(key), expected, "{key:?}");
    }
}

/// Asserts that no fixed key of `DETECTED_KEYS` is in the family, and that no key made from
/// a name is in the list: the family is told apart by its prefix, which the agent checks a
/// `--capability` flag against. It catches a provider key added to the fixed list, which
/// would make the list name a provider that no agent was given.
#[test]
fn the_family_is_not_among_the_detected_keys() {
    assert!(DETECTED_KEYS.iter().all(|key| !is_mount_provider_key(key)));
    assert!(!DETECTED_KEYS.contains(&mount_provider_key("tierstore").as_str()));
}

/// Asserts that a request naming `mount-provider.tierstore=true` parses and reads back as the
/// same pair, and that a request naming the same key twice is refused naming it, and a
/// request naming two providers keeps both. It catches the family missing from the exact
/// keys, where a job with a provider's tree is refused as an unknown key and can never be
/// placed.
#[test]
fn a_request_for_a_mount_provider_parses() {
    let key = mount_provider_key("tierstore");
    let request = req(&[(key.as_str(), FLAG_SET)]).unwrap();
    assert_eq!(request.exact("mount-provider.tierstore"), Some("true"));
    assert_eq!(
        request.properties(),
        [("mount-provider.tierstore", "true".to_owned())]
    );
    let twice = req(&[(key.as_str(), "true"), (key.as_str(), "true")]);
    assert_eq!(
        twice,
        Err(RequestError::Repeated("mount-provider.tierstore".into()))
    );
    let two = req(&[("mount-provider.b", "true"), ("mount-provider.a", "true")]).unwrap();
    assert_eq!(
        two.properties(),
        [
            ("mount-provider.a", "true".to_owned()),
            ("mount-provider.b", "true".to_owned())
        ]
    );
    let properties = two.properties();
    let again = Request::parse(properties.iter().map(|(key, value)| (*key, value.as_str())));
    assert_eq!(again, Ok(two));
}

/// Asserts that a node reporting `mount-provider.tierstore=true` meets the request for it,
/// and that a node without the key, with another provider's, or with another value does
/// not: the miss is `Unmet::Exact` naming the key and printing as `key=true`. It catches a
/// match that ignores the key, which would place a job on a host that has no provider of
/// that name, and one that takes another provider's key for it.
#[test]
fn a_mount_provider_is_matched_exactly() {
    let wanted = req(&[("mount-provider.tierstore", "true")]).unwrap();
    let reporting = node(&[("mount-provider.tierstore", "true")]).unwrap();
    assert!(wanted.matches(&reporting));
    let unmet = [Unmet::Exact {
        key: "mount-provider.tierstore",
        want: "true",
    }];
    let silent = node(&[]).unwrap();
    assert_eq!(wanted.unmet(&silent), unmet);
    assert_eq!(
        wanted.unmet(&silent)[0].to_string(),
        "mount-provider.tierstore=true"
    );
    let other = node(&[("mount-provider.scratch", "true")]).unwrap();
    assert_eq!(wanted.unmet(&other), unmet);
    let off = node(&[("mount-provider.tierstore", "false")]).unwrap();
    assert_eq!(wanted.unmet(&off), unmet);
    assert!(Request::default().matches(&silent));
}

/// Asserts that a node report keeps every provider key it names, each under its own key, and
/// that a report naming one twice is refused naming it. It catches a report reader that
/// drops the family, which would leave every provider job unplaceable, and one whose second
/// entry silently replaces the first.
#[test]
fn a_report_keeps_its_mount_providers() {
    let caps = node(&[
        ("mount-provider.tierstore", "true"),
        ("mount-provider.scratch", "true"),
    ])
    .unwrap();
    assert_eq!(caps.exact.get("mount-provider.tierstore").unwrap(), "true");
    assert_eq!(caps.exact.get("mount-provider.scratch").unwrap(), "true");
    assert_eq!(caps.exact.len(), 2);
    let twice = node(&[
        ("mount-provider.tierstore", "true"),
        ("mount-provider.tierstore", "true"),
    ]);
    assert_eq!(
        twice,
        Err(ReportError::Repeated("mount-provider.tierstore".into()))
    );
}

/// Asserts that the bare prefix, with and without its dot, stays an unknown key: a request
/// naming it is refused `UnknownKey` naming it, and a node report skips it. It catches a
/// prefix test that takes an empty name for a provider, which would let a job require a
/// provider that nothing can be named.
#[test]
fn the_bare_prefix_is_still_an_unknown_key() {
    for key in ["mount-provider.", "mount-provider", "mount-provider-x"] {
        assert_eq!(
            req(&[(key, "true")]),
            Err(RequestError::UnknownKey(key.to_owned())),
            "{key:?}"
        );
        let skipped = node(&[(key, "true")]).unwrap();
        assert!(skipped.exact.is_empty(), "{key:?}");
    }
}

/// Asserts that an action's platform property named like a provider key is read as that key
/// (in lower case, as every own key is) and becomes the request's exact requirement, and that
/// the bare prefix is not read. It catches a platform that drops a provider's key, which
/// would place an action that needs the provider's tree on any host.
#[test]
fn a_platform_property_naming_a_mount_provider_is_read_as_its_key() {
    let name = |text: &str| property_name(text).map(|key| key.into_owned());
    assert_eq!(
        name("mount-provider.tierstore").as_deref(),
        Some("mount-provider.tierstore")
    );
    assert_eq!(
        name("Mount-Provider.Tierstore").as_deref(),
        Some("mount-provider.tierstore")
    );
    assert_eq!(name("mount-provider."), None);
    let request = Request::from_platform([("mount-provider.tierstore", "true")]).unwrap();
    assert_eq!(request.exact("mount-provider.tierstore"), Some("true"));
}
