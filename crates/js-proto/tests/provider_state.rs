//! What `Provider` tells of a provider's state (its catalog, the instance types passed
//! over, the offers declined, the brake, the cool-down and the launches out), held to the
//! evolution rule of the package: the fields are additions, numbered after the shipped ones,
//! and a reader of the previous release meets them as unknown fields.

use js_proto::FILE_DESCRIPTOR_SET;
use js_proto::v1::{
    Availability, DeclinedOffer, Machine, PassedOver, PendingLaunch, Provider, ProviderBrake,
    ProviderCatalog, ProviderCheck,
};
use prost::Message;
use prost_types::FileDescriptorSet;

/// The previous release's `Provider`: name, kind, state, checks and machines. A fixture
/// standing for readers already deployed; it must not follow later changes.
#[derive(Clone, PartialEq, Message)]
struct PreviousProvider {
    #[prost(string, tag = "1")]
    name: String,
    #[prost(string, tag = "2")]
    kind: String,
    #[prost(enumeration = "Availability", tag = "3")]
    state: i32,
    #[prost(message, repeated, tag = "4")]
    checks: Vec<ProviderCheck>,
    #[prost(message, repeated, tag = "5")]
    machines: Vec<Machine>,
}

/// The golden rows of the fields `Provider` gained, as the golden writes them.
const ADDED_ROWS: [&str; 7] = [
    "Provider.catalog = 6 : ProviderCatalog",
    "Provider.passed_over = 7 : repeated PassedOver",
    "Provider.declined = 8 : repeated DeclinedOffer",
    "Provider.brake = 9 : ProviderBrake",
    "Provider.cooldown_until_ms = 10 : optional uint64",
    "Provider.cooldown_reason = 11 : string",
    "Provider.launching = 12 : repeated PendingLaunch",
];

/// A provider as the previous release told it.
fn previous() -> PreviousProvider {
    PreviousProvider {
        name: "burst".to_owned(),
        kind: "lambdalabs".to_owned(),
        state: Availability::Available as i32,
        checks: vec![ProviderCheck {
            name: "credentials".to_owned(),
            ok: true,
            checked_at_ms: 7,
            ..ProviderCheck::default()
        }],
        machines: vec![Machine {
            host: "js-burst-1".to_owned(),
            state: "registered".to_owned(),
            offer: "gpu_1x_a10".to_owned(),
            price_cents_per_hour: Some(75),
            ..Machine::default()
        }],
    }
}

/// The same provider as this release tells it, with none of the new fields set.
fn current() -> Provider {
    let old = previous();
    Provider {
        name: old.name,
        kind: old.kind,
        state: old.state,
        checks: old.checks,
        machines: old.machines,
        ..Provider::default()
    }
}

/// Asserts that the new fields of `Provider` are additions in the schema and on the wire:
/// the golden holds their rows and the descriptor numbers them 6 to 12, after the shipped
/// five; a provider with the new fields unset encodes byte for byte as the previous
/// release's and decodes in its reader; the previous release's bytes decode in this reader
/// with every new field unset; and a provider with every new field set decodes in the
/// previous reader with every old field as sent, the new ones skipped. Catches a renumbered
/// or re-meant field, a default that is written on the wire, and a previous reader that
/// cannot read this release's answer.
#[test]
fn the_provider_state_fields_are_additive_on_the_wire_and_in_the_proto() {
    let golden = include_str!("golden/fields.txt");
    for row in ADDED_ROWS {
        let held = golden.lines().filter(|line| *line == row).count();
        assert_eq!(held, 1, "{row}");
    }
    let set = FileDescriptorSet::decode(FILE_DESCRIPTOR_SET).expect("the descriptor set decodes");
    let provider = set
        .file
        .iter()
        .flat_map(|file| &file.message_type)
        .find(|message| message.name() == "Provider")
        .expect("Provider is in the schema");
    let numbers: Vec<(&str, i32)> = provider
        .field
        .iter()
        .map(|field| (field.name(), field.number()))
        .collect();
    assert_eq!(
        numbers,
        [
            ("name", 1),
            ("kind", 2),
            ("state", 3),
            ("checks", 4),
            ("machines", 5),
            ("catalog", 6),
            ("passed_over", 7),
            ("declined", 8),
            ("brake", 9),
            ("cooldown_until_ms", 10),
            ("cooldown_reason", 11),
            ("launching", 12),
        ]
    );

    let old_bytes = previous().encode_to_vec();
    assert_eq!(current().encode_to_vec(), old_bytes);
    let read = PreviousProvider::decode(current().encode_to_vec().as_slice());
    assert_eq!(read.expect("the previous reader decodes"), previous());
    let read = Provider::decode(old_bytes.as_slice()).expect("this reader decodes");
    assert_eq!(read, current());

    let full = Provider {
        catalog: Some(ProviderCatalog {
            fetched_at_ms: 1,
            offer_count: 3,
        }),
        passed_over: vec![PassedOver {
            instance_type: "gpu_8x_h100".to_owned(),
            region: "us-east-1".to_owned(),
            reason: "quota".to_owned(),
            until_ms: 2,
        }],
        declined: vec![DeclinedOffer {
            request: "job-1".to_owned(),
            offer: "gpu_1x_a100".to_owned(),
            zone: "us-west-1".to_owned(),
            reason: "capacity".to_owned(),
            declined_at_ms: 3,
        }],
        brake: Some(ProviderBrake {
            engaged: true,
            engaged_at_ms: Some(4),
            failures: 3,
        }),
        cooldown_until_ms: Some(5),
        cooldown_reason: "boot_failed".to_owned(),
        launching: vec![PendingLaunch {
            host: "js-burst-2".to_owned(),
            offer: "gpu_1x_a100".to_owned(),
            zone: "us-west-1".to_owned(),
            started_at_ms: 6,
            age_ms: 7,
        }],
        ..current()
    };
    let bytes = full.encode_to_vec();
    let old_reader = PreviousProvider::decode(bytes.as_slice()).expect("the previous reader");
    assert_eq!(old_reader, previous());
    assert_eq!(
        Provider::decode(bytes.as_slice()).expect("this reader"),
        full
    );
}
