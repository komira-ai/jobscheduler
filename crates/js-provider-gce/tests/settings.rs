//! The settings of the catalog and the errors of the port: what each configuration key
//! accepts and refuses, and how a call's error is classified. The project and zones are
//! made up.

use js_elastic::Purchase;
use js_provider_gce::{
    AcceleratorTypeName, ApiError, ErrorReason, GceCatalogSettings, MachineTypeName, PurchaseKind,
    SettingsError, Zone,
};

const PROJECT: &str = "sample-project";
const ZONES: [&str; 3] = ["region-one-a", "region-one-b", "region-one-c"];
const OTHER_ZONE: &str = "region-two-a";

fn zone(name: &str) -> Zone {
    Zone::new(name).unwrap()
}

/// Asserts that the settings are checked once, each refusal naming its key in words: a
/// project that is not an id (too short or long, a leading digit, a trailing `-`, upper
/// case, `_`), no zone, a zone that is not a zone name (no `-`, upper case, an empty word,
/// too long) or is listed twice, no purchase option, an unknown one or one listed twice;
/// the accessors and the spelling of each option both ways; and the names of a project, a
/// zone and its region. Catches a key that accepts what it should refuse, or the other
/// way.
#[test]
fn settings_are_checked_once_and_name_their_key() {
    let new = |project: &str, zones: &[&str], purchase: &[&str]| {
        GceCatalogSettings::new(project, zones, purchase, None)
    };
    for project in [
        "",
        "short",
        "1sample",
        "sample-",
        "Sample-project",
        "sample_project",
        &"a".repeat(31),
    ] {
        assert_eq!(
            new(project, &ZONES, &["spot"]),
            Err(SettingsError::Project),
            "{project:?}"
        );
    }
    for project in ["sample", "a1-b2-c3", &"a".repeat(30)] {
        assert_eq!(
            new(project, &ZONES, &["spot"]).unwrap().project().as_str(),
            project
        );
    }
    for bad in [
        "",
        "zone",
        "Zone-a",
        "zone--a",
        "-zone-a",
        "zone-a-",
        "zone_one-a",
    ] {
        assert_eq!(
            new(PROJECT, &[bad], &["spot"]),
            Err(SettingsError::Zone(bad.to_owned())),
            "{bad:?}"
        );
    }
    let long = format!("{}-a", "z".repeat(62));
    assert_eq!(Zone::new(&long), Err(SettingsError::Zone(long.clone())));
    assert_eq!(new(PROJECT, &[], &["spot"]), Err(SettingsError::NoZone));
    assert_eq!(
        new(PROJECT, &[ZONES[0], ZONES[1], ZONES[0]], &["spot"]),
        Err(SettingsError::ZoneTwice(ZONES[0].to_owned()))
    );
    assert_eq!(new(PROJECT, &ZONES, &[]), Err(SettingsError::NoPurchase));
    assert_eq!(
        new(PROJECT, &ZONES, &["spot", "on-demand", "spot"]),
        Err(SettingsError::PurchaseTwice("spot".to_owned()))
    );
    let messages = [
        (
            SettingsError::Project,
            "project: not a project id (6 to 30 lower-case letters, digits and '-', starting \
             with a letter and not ending with '-')",
        ),
        (SettingsError::NoZone, "zones: lists no zone"),
        (
            SettingsError::Zone("x".to_owned()),
            "zones: \"x\" is not a zone name (a region name, '-' and a zone letter)",
        ),
        (
            SettingsError::ZoneTwice("x".to_owned()),
            "zones: \"x\" is listed twice",
        ),
        (
            SettingsError::NoPurchase,
            "purchase: lists no purchase option",
        ),
        (
            SettingsError::Purchase("x".to_owned()),
            "purchase: \"x\" is not one of on-demand, spot",
        ),
        (
            SettingsError::PurchaseTwice("spot".to_owned()),
            "purchase: \"spot\" is listed twice",
        ),
    ];
    for (error, text) in messages {
        assert_eq!(error.to_string(), text);
    }
    let kept = new(PROJECT, &[ZONES[1], OTHER_ZONE], &["spot", "on-demand"]).unwrap();
    assert_eq!(
        kept.purchase(),
        [PurchaseKind::Spot, PurchaseKind::OnDemand]
    );
    assert_eq!(kept.zones(), [zone(ZONES[1]), zone(OTHER_ZONE)]);
    assert_eq!(kept.project().to_string(), PROJECT);
    for (kind, text, purchase) in [
        (PurchaseKind::OnDemand, "on-demand", Purchase::OnDemand),
        (PurchaseKind::Spot, "spot", Purchase::Spot),
    ] {
        assert_eq!(kind.name(), text);
        assert_eq!(PurchaseKind::parse(text), Ok(kind));
        assert_eq!(kind.purchase(), purchase);
    }
    let named = zone("us-sample1-b");
    assert_eq!(
        (named.as_str(), named.to_string()),
        ("us-sample1-b", "us-sample1-b".to_owned())
    );
    let region = named.region();
    assert_eq!(
        (region.as_str(), region.to_string()),
        ("us-sample1", "us-sample1".to_owned())
    );
}

/// Asserts that an error is classified by the cloud's reason alone: each documented
/// throttle reason is throttled, each denial reason denied, any other refused, each
/// carrying the reason; that an error with no reason says so; and that a reason keeps only
/// printable ASCII, at most 80 characters. Catches a classification that reads message
/// text or drops the reason.
#[test]
fn an_error_is_classified_by_its_reason() {
    for reason in ["rateLimitExceeded", "RATE_LIMIT_EXCEEDED"] {
        assert!(
            matches!(ApiError::answered(reason), ApiError::Throttled { .. }),
            "{reason}"
        );
    }
    for reason in ["forbidden", "PERMISSION_DENIED"] {
        assert!(
            matches!(ApiError::answered(reason), ApiError::Denied { .. }),
            "{reason}"
        );
    }
    let other = ApiError::answered("ZONE_RESOURCE_POOL_EXHAUSTED");
    assert_eq!(
        other.reason().map(ErrorReason::as_str),
        Some("ZONE_RESOURCE_POOL_EXHAUSTED")
    );
    assert!(matches!(other, ApiError::Refused { .. }));
    for reason in ["rateLimitExceeded", "forbidden"] {
        assert_eq!(
            ApiError::answered(reason).reason().unwrap().to_string(),
            reason
        );
    }
    for reasonless in [
        ApiError::Transport(String::new()),
        ApiError::Unreadable(String::new()),
        ApiError::Incomplete,
    ] {
        assert_eq!(reasonless.reason(), None);
    }
    assert_eq!(ErrorReason::new("bad reason\u{e9}\n").as_str(), "badreason");
    assert_eq!(ErrorReason::new(&"x".repeat(100)).as_str().len(), 80);
    // The names of the model types.
    let name = MachineTypeName("a3-megagpu-8g".to_owned());
    assert_eq!(
        (name.as_str(), name.to_string()),
        ("a3-megagpu-8g", "a3-megagpu-8g".to_owned())
    );
    let accelerator = AcceleratorTypeName("nvidia-l4".to_owned());
    assert_eq!(accelerator.as_str(), "nvidia-l4");
}
