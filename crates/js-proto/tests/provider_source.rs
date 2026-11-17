//! The provider source of a layer and of a mount (`ProviderSource`, `Layer.provider = 3`,
//! `Mount.provider = 4`), held to the evolution rule of the package: each is an addition, and a
//! reader of the previous release meets it as an unknown field.

use js_proto::FILE_DESCRIPTOR_SET;
use js_proto::v1::layer::Source as LayerSource;
use js_proto::v1::mount::Source as MountSource;
use js_proto::v1::{JobSpec, Layer, Mount, MountMode, ProviderSource};
use prost::Message;
use prost_types::{DescriptorProto, FileDescriptorSet};

/// The previous release's `Layer`: the two sources it had, as plain fields. A fixture standing
/// for readers already deployed; it must not follow later changes.
#[derive(Clone, PartialEq, Message)]
struct PreviousLayer {
    #[prost(string, tag = "1")]
    host_path: String,
    #[prost(string, tag = "2")]
    image: String,
}

/// The previous release's `Mount`: the one source it had, and the fields after it.
#[derive(Clone, PartialEq, Message)]
struct PreviousMount {
    #[prost(string, tag = "1")]
    host_path: String,
    #[prost(string, tag = "10")]
    target: String,
    #[prost(int32, tag = "11")]
    mode: i32,
    #[prost(string, tag = "12")]
    sub_path: String,
    #[prost(bool, tag = "13")]
    no_exec: bool,
}

/// The previous release's `JobSpec`, as far as layers and mounts go.
#[derive(Clone, PartialEq, Message)]
struct PreviousJobSpec {
    #[prost(string, tag = "1")]
    job_id: String,
    #[prost(int32, tag = "50")]
    priority: i32,
    #[prost(message, repeated, tag = "60")]
    layers: Vec<PreviousLayer>,
    #[prost(message, repeated, tag = "61")]
    mounts: Vec<PreviousMount>,
}

/// The golden rows that name the provider additions, as the golden writes them.
const ADDED_ROWS: [&str; 4] = [
    "Layer.provider = 3 : ProviderSource (oneof source)",
    "Mount.provider = 4 : ProviderSource (oneof source)",
    "ProviderSource.provider = 1 : string",
    "ProviderSource.ref = 2 : string",
];

/// The bytes of `ProviderSource { "tierstore", "toolchains/clang-18" }`, written by hand: the
/// provider as field 1 and the ref as field 2, each length-delimited.
const SOURCE_BYTES: &str = "0a 09 74 69 65 72 73 74 6f 72 65 12 13 74 6f 6f 6c 63 68 61 69 6e 73 \
                            2f 63 6c 61 6e 67 2d 31 38";

/// The bytes of the layer with that source: the source as field 3.
const LAYER_BYTES: &str = "1a 20 0a 09 74 69 65 72 73 74 6f 72 65 12 13 74 6f 6f 6c 63 68 61 69 \
                           6e 73 2f 63 6c 61 6e 67 2d 31 38";

/// The bytes of the mount with that source and the target `/data`: the source as field 4 and
/// the target as field 10.
const MOUNT_BYTES: &str = "22 20 0a 09 74 69 65 72 73 74 6f 72 65 12 13 74 6f 6f 6c 63 68 61 69 \
                           6e 73 2f 63 6c 61 6e 67 2d 31 38 52 05 2f 64 61 74 61";

/// The bytes of a hex text of pairs separated by white space.
fn bytes(hex: &str) -> Vec<u8> {
    hex.split_whitespace()
        .map(|pair| u8::from_str_radix(pair, 16).expect("a hex pair"))
        .collect()
}

/// The descriptor set of the package, decoded.
fn schema() -> FileDescriptorSet {
    FileDescriptorSet::decode(FILE_DESCRIPTOR_SET).expect("the descriptor set decodes")
}

/// The message called `name` in the schema.
fn message<'a>(set: &'a FileDescriptorSet, name: &str) -> &'a DescriptorProto {
    set.file
        .iter()
        .flat_map(|file| &file.message_type)
        .find(|message| message.name() == name)
        .unwrap_or_else(|| panic!("{name} is in the schema"))
}

/// The fields of the message `name` as (field name, number, oneof name) rows.
fn fields(set: &FileDescriptorSet, name: &str) -> Vec<(String, i32, Option<String>)> {
    let one = message(set, name);
    one.field
        .iter()
        .map(|field| {
            let oneof = field.oneof_index.map(|at| {
                one.oneof_decl[usize::try_from(at).expect("a non-negative index")]
                    .name()
                    .to_owned()
            });
            (field.name().to_owned(), field.number(), oneof)
        })
        .collect()
}

/// The source the bytes above encode.
fn source() -> ProviderSource {
    ProviderSource {
        provider: "tierstore".to_owned(),
        r#ref: "toolchains/clang-18".to_owned(),
    }
}

/// A spec with a host-path layer and a provider layer, a provider mount and its old fields.
fn spec() -> JobSpec {
    JobSpec {
        job_id: "job-1".to_owned(),
        priority: 3,
        layers: vec![
            Layer {
                source: Some(LayerSource::HostPath("/srv/base".to_owned())),
            },
            Layer {
                source: Some(LayerSource::Provider(source())),
            },
        ],
        mounts: vec![Mount {
            source: Some(MountSource::Provider(source())),
            target: "/data".to_owned(),
            mode: MountMode::ReadWrite as i32,
            ..Mount::default()
        }],
        ..JobSpec::default()
    }
}

/// Asserts that every row of the provider additions is in the golden exactly once. Catches an
/// addition that the golden does not pin, and a row written twice.
#[test]
fn the_golden_holds_each_provider_row_once() {
    let golden = include_str!("golden/fields.txt");
    for row in ADDED_ROWS {
        let held = golden.lines().filter(|line| *line == row).count();
        assert_eq!(held, 1, "{row}");
    }
}

/// Asserts the numbers the schema gives the provider source, read by name from the descriptor
/// set: `provider` and `ref` of the message, and the member `provider` of the oneof `source`
/// of `Layer` (3) and of `Mount` (4), after the members they already had. Catches a renumbered
/// or renamed field and a source that left its oneof.
#[test]
fn the_schema_numbers_the_provider_source() {
    let set = schema();
    assert_eq!(
        fields(&set, "ProviderSource"),
        [
            ("provider".to_owned(), 1, None),
            ("ref".to_owned(), 2, None)
        ]
    );
    let source = || Some("source".to_owned());
    assert!(fields(&set, "Layer").contains(&("provider".to_owned(), 3, source())));
    assert!(fields(&set, "Mount").contains(&("provider".to_owned(), 4, source())));
}

/// Asserts the exact bytes of a provider source, of a layer and of a mount that hold it, and
/// that each decodes back to what was encoded with the source in its oneof. Catches a changed
/// number, wire type or order, which every provider and every stored job would meet.
#[test]
fn a_provider_source_has_the_documented_bytes_and_reads_back() {
    assert_eq!(source().encode_to_vec(), bytes(SOURCE_BYTES));
    let layer = Layer {
        source: Some(LayerSource::Provider(source())),
    };
    assert_eq!(layer.encode_to_vec(), bytes(LAYER_BYTES));
    let read = Layer::decode(bytes(LAYER_BYTES).as_slice()).expect("the layer decodes");
    assert_eq!(read, layer);
    let mount = Mount {
        source: Some(MountSource::Provider(source())),
        target: "/data".to_owned(),
        ..Mount::default()
    };
    assert_eq!(mount.encode_to_vec(), bytes(MOUNT_BYTES));
    let read = Mount::decode(bytes(MOUNT_BYTES).as_slice()).expect("the mount decodes");
    assert_eq!(read, mount);
    let alone = ProviderSource::decode(bytes(SOURCE_BYTES).as_slice()).expect("the source decodes");
    assert_eq!(alone, source());
}

/// Asserts that a new spec with a provider layer and a provider mount decodes in the previous
/// reader with every old field as sent, the host-path layer whole, the provider layer as a
/// layer with no source it knows, and the mount with its target, mode and options but no
/// source; and that what the previous reader writes back reads in the new type with no source
/// where the provider's was. Catches a source that a previous reader cannot decode, one that
/// disturbs the fields beside it, and a reader that would take the provider for another kind.
#[test]
fn a_previous_reader_skips_the_provider_source_and_keeps_the_rest() {
    let bytes = spec().encode_to_vec();
    let old = PreviousJobSpec::decode(bytes.as_slice()).expect("the previous reader decodes");
    assert_eq!(old.job_id, "job-1");
    assert_eq!(old.priority, 3);
    assert_eq!(
        old.layers,
        [
            PreviousLayer {
                host_path: "/srv/base".to_owned(),
                image: String::new(),
            },
            PreviousLayer::default(),
        ]
    );
    assert_eq!(
        old.mounts,
        [PreviousMount {
            host_path: String::new(),
            target: "/data".to_owned(),
            mode: MountMode::ReadWrite as i32,
            sub_path: String::new(),
            no_exec: false,
        }]
    );

    let back = JobSpec::decode(old.encode_to_vec().as_slice()).expect("the new reader decodes");
    assert_eq!(
        back.layers,
        [
            Layer {
                source: Some(LayerSource::HostPath("/srv/base".to_owned())),
            },
            Layer { source: None },
        ]
    );
    assert_eq!(back.mounts[0].source, None);
    assert_eq!(back.mounts[0].target, "/data");

    let same = JobSpec::decode(bytes.as_slice()).expect("the new reader decodes");
    assert_eq!(same, spec());
}

/// Asserts that bytes of the previous release's layers and mounts, which hold no provider,
/// decode in the new type with their sources as sent and encode byte-identical. Catches a
/// default that is written for a spec that names no provider.
#[test]
fn a_spec_without_a_provider_is_the_previous_bytes() {
    let old = PreviousJobSpec {
        job_id: "job-1".to_owned(),
        priority: 3,
        layers: vec![PreviousLayer {
            host_path: "/srv/base".to_owned(),
            image: String::new(),
        }],
        mounts: vec![PreviousMount {
            host_path: "/srv/data".to_owned(),
            target: "/data".to_owned(),
            mode: MountMode::ReadOnly as i32,
            sub_path: String::new(),
            no_exec: true,
        }],
    };
    let old_bytes = old.encode_to_vec();
    let read = JobSpec::decode(old_bytes.as_slice()).expect("the new reader decodes");
    assert_eq!(
        read.layers[0].source,
        Some(LayerSource::HostPath("/srv/base".to_owned()))
    );
    assert_eq!(
        read.mounts[0].source,
        Some(MountSource::HostPath("/srv/data".to_owned()))
    );
    assert_eq!(read.encode_to_vec(), old_bytes);
}
