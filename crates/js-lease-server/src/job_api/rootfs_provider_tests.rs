//! The provider sources of a job's layers and mounts (`rootfs.rs`), called directly: the
//! bound of the name and of the ref at both sides, the fault each refusal names, the form a
//! source is stored in and the provider names a job requires.

use super::*;

/// The provider source `name` with the ref `reference`.
fn source(name: &str, reference: &str) -> v1::ProviderSource {
    v1::ProviderSource {
        provider: name.to_owned(),
        r#ref: reference.to_owned(),
    }
}

/// A layer from the provider `name`, asked for `reference`.
fn provider_layer(name: &str, reference: &str) -> v1::Layer {
    v1::Layer {
        source: Some(v1::layer::Source::Provider(source(name, reference))),
    }
}

/// A mount at `target` from the provider `name`, asked for `reference`, every other field
/// unset.
fn provider_mount(name: &str, reference: &str, target: &str) -> v1::Mount {
    v1::Mount {
        source: Some(v1::mount::Source::Provider(source(name, reference))),
        target: target.to_owned(),
        ..v1::Mount::default()
    }
}

/// The layer a provider layer is stored as.
fn stored_layer(name: &str, reference: &str) -> Layer {
    Layer::Provider {
        provider: name.to_owned(),
        reference: reference.to_owned(),
    }
}

/// Asserts that a ref of 1024 bytes is taken and one byte more refused, for a layer and for a
/// mount, counting bytes and not characters (512 two-byte characters are taken, 513 refused),
/// and that a ref of one byte is taken. Catches a bound that counts characters, one that is
/// off by one, and a ref bound left out of either list.
#[test]
fn a_ref_of_1024_bytes_is_accepted_and_one_byte_more_refused() {
    let cases = [
        ("k".repeat(1024), true),
        ("k".repeat(1025), false),
        ("é".repeat(512), true),
        ("é".repeat(513), false),
        ("k".to_owned(), true),
    ];
    for (reference, taken) in cases {
        let text = format!("{} bytes", reference.len());
        let layer = layers(&[provider_layer("tierstore", &reference)]);
        let mount = mounts(&[provider_mount("tierstore", &reference, "/in")]);
        match taken {
            true => {
                assert_eq!(
                    layer,
                    Ok(vec![stored_layer("tierstore", &reference)]),
                    "{text}"
                );
                let source = MountSource::Provider {
                    provider: "tierstore".to_owned(),
                    reference: reference.clone(),
                };
                assert_eq!(
                    mount.map(|list| list[0].source.clone()),
                    Ok(source),
                    "{text}"
                );
            }
            false => {
                assert_eq!(
                    layer,
                    Err(Fault::Entry("layers", 0, "provider.ref")),
                    "{text}"
                );
                let fault = Fault::Entry("mounts", 0, "provider.ref");
                assert_eq!(mount.map(|_| ()), Err(fault), "{text}");
            }
        }
    }
}

/// Asserts that an empty ref, and a ref with a control character (a newline, a NUL, a DEL, a
/// C1 control, a tab) or a format character (a right-to-left override, the line separator), is
/// refused naming `provider.ref` of its entry in a layer list and in a mount list, while a ref
/// with spaces, slashes and characters beyond ASCII is taken and stored untouched. Catches a
/// ref that can hide text from a log, one with a rule of its own that differs from the wire's,
/// and a ref normalised on the way in.
#[test]
fn a_ref_with_a_control_character_is_refused_naming_the_field() {
    let bad = [
        "",
        "a\nb",
        "a\0b",
        "\u{7f}",
        "a\u{85}b",
        "tab\there",
        "a\u{202e}b",
        "a\u{2028}b",
    ];
    for reference in bad {
        assert_eq!(
            layers(&[host_layer("/a"), provider_layer("tierstore", reference)]),
            Err(Fault::Entry("layers", 1, "provider.ref")),
            "{reference:?}"
        );
        assert_eq!(
            mounts(&[provider_mount("tierstore", reference, "/in")]).map(|_| ()),
            Err(Fault::Entry("mounts", 0, "provider.ref")),
            "{reference:?}"
        );
    }
    for reference in ["dir/a file.json", "ключ/ключ", "  padded  ", "a=b&c=d"] {
        assert_eq!(
            layers(&[provider_layer("tierstore", reference)]),
            Ok(vec![stored_layer("tierstore", reference)]),
            "{reference:?}"
        );
    }
}

/// A layer whose source is [`host_layer`] of `path`.
fn host_layer(path: &str) -> v1::Layer {
    v1::Layer {
        source: Some(v1::layer::Source::HostPath(path.to_owned())),
    }
}

/// Asserts that a provider name of 1 to 32 bytes of lower-case letters, digits and `-`, the
/// first a letter, is taken, and that an empty name, an upper-case letter, a leading digit or
/// `-`, an underscore, a dot, a space, a non-ASCII letter and a name of 33 bytes are each
/// refused naming `provider.provider`, for a layer and for a mount, and before the ref is
/// read. Catches a grammar looser than the wire's (a name the agent's flag or the capability
/// key would not carry) and a name checked after the ref.
#[test]
fn a_provider_name_is_one_to_32_bytes_of_lowercase_letters_digits_and_dashes() {
    let longest = "a".repeat(32);
    for name in ["a", "tierstore", "tier-store-2", longest.as_str()] {
        assert_eq!(
            layers(&[provider_layer(name, "key")]),
            Ok(vec![stored_layer(name, "key")]),
            "{name}"
        );
        assert!(
            mounts(&[provider_mount(name, "key", "/in")]).is_ok(),
            "{name}"
        );
    }
    let long = "a".repeat(33);
    let bad = [
        "",
        "Tier",
        "1tier",
        "-tier",
        "tier_store",
        "tier.store",
        "tier store",
        "tíer",
        &long[..],
    ];
    for name in bad {
        assert_eq!(
            layers(&[provider_layer(name, "key")]),
            Err(Fault::Entry("layers", 0, "provider.provider")),
            "{name:?}"
        );
        assert_eq!(
            mounts(&[provider_mount(name, "key", "/in")]).map(|_| ()),
            Err(Fault::Entry("mounts", 0, "provider.provider")),
            "{name:?}"
        );
    }
    assert_eq!(
        layers(&[provider_layer("Tier", "")]),
        Err(Fault::Entry("layers", 0, "provider.provider"))
    );
}

/// Asserts that provider layers are stored beside host-path and image layers in the order
/// given, that two layers of one provider are two layers, and that a layer with no source
/// among them is still refused at its index as having no source this server knows. Catches a
/// provider layer that displaces or merges with another, and a source this server does not
/// know taken as a provider.
#[test]
fn a_provider_layer_is_stored_in_its_place_among_the_layers() {
    let pinned = format!("registry.example/app@sha256:{}", "c".repeat(64));
    let list = [
        host_layer("/a"),
        provider_layer("tierstore", "one"),
        v1::Layer {
            source: Some(v1::layer::Source::Image(pinned.clone())),
        },
        provider_layer("tierstore", "two"),
    ];
    assert_eq!(
        layers(&list),
        Ok(vec![
            Layer::HostPath {
                path: "/a".to_owned()
            },
            stored_layer("tierstore", "one"),
            Layer::Image { image: pinned },
            stored_layer("tierstore", "two"),
        ])
    );
    let unknown = v1::Layer { source: None };
    assert_eq!(
        layers(&[provider_layer("tierstore", "one"), unknown]),
        Err(Fault::NoSource("layers", 1))
    );
}

/// Asserts that a mount from a provider takes a target, a mode, a `subPath` and `noExec` as a
/// mount of a host path does and is stored in the explicit form, with its provider and ref as
/// given. Catches a provider mount that drops an option or is stored with a default left
/// implicit.
#[test]
fn a_provider_mount_is_stored_whole() {
    let mount = v1::Mount {
        mode: v1::MountMode::ReadWrite.into(),
        sub_path: "sub/dir".to_owned(),
        no_exec: true,
        ..provider_mount("tierstore", "the/key", "/data")
    };
    assert_eq!(
        mounts(&[mount, provider_mount("cache", "other", "/cache")]),
        Ok(vec![
            JobMount {
                source: MountSource::Provider {
                    provider: "tierstore".to_owned(),
                    reference: "the/key".to_owned(),
                },
                target: "/data".to_owned(),
                mode: MountMode::ReadWrite,
                sub_path: Some("sub/dir".to_owned()),
                no_exec: true,
                propagation: Propagation::Private,
            },
            JobMount {
                source: MountSource::Provider {
                    provider: "cache".to_owned(),
                    reference: "other".to_owned(),
                },
                target: "/cache".to_owned(),
                mode: MountMode::ReadOnly,
                sub_path: None,
                no_exec: false,
                propagation: Propagation::Private,
            },
        ])
    );
}

/// Asserts that the source of a provider mount is checked before its target, as a host path
/// is, and that the rules of the target still hold for it: a relative target and a target
/// equal in lower case to an earlier mount's are refused naming `target`, and a mount with no
/// source is refused as having none this server knows. Catches a provider mount that skips
/// the target rules and a source checked in another order than a host path's.
#[test]
fn a_provider_mount_is_held_to_the_rules_of_every_mount() {
    let both_bad = provider_mount("Tier", "key", "relative");
    assert_eq!(
        mounts(&[both_bad]).map(|_| ()),
        Err(Fault::Entry("mounts", 0, "provider.provider"))
    );
    assert_eq!(
        mounts(&[provider_mount("tierstore", "key", "relative")]).map(|_| ()),
        Err(Fault::Entry("mounts", 0, "target"))
    );
    let twice = [
        provider_mount("tierstore", "one", "/Data"),
        provider_mount("tierstore", "two", "/data"),
    ];
    assert_eq!(
        mounts(&twice).map(|_| ()),
        Err(Fault::Entry("mounts", 1, "target"))
    );
    let none = v1::Mount {
        source: None,
        ..provider_mount("tierstore", "key", "/in")
    };
    assert_eq!(
        mounts(&[none]).map(|_| ()),
        Err(Fault::NoSource("mounts", 0))
    );
}

/// Asserts that the provider names a job requires are those of its provider layers and its
/// provider mounts, sorted and each once, and none for layers and mounts of other kinds.
/// Catches a name listed twice, a name missed from either list, and a host path read as a
/// provider.
#[test]
fn the_providers_a_job_requires_are_listed_sorted_and_once() {
    let layer_list = [
        provider_layer("tierstore", "one"),
        host_layer("/a"),
        provider_layer("cache", "two"),
        provider_layer("tierstore", "three"),
    ];
    let wire_layers = layers(&layer_list).unwrap();
    let mount_list = [
        provider_mount("zeta", "x", "/z"),
        mount_at("/host"),
        provider_mount("cache", "y", "/c"),
    ];
    let wire_mounts = mounts(&mount_list).unwrap();
    assert_eq!(
        providers(&wire_layers, &wire_mounts),
        ["cache", "tierstore", "zeta"]
    );
    assert!(providers(&wire_layers[1..2], &wire_mounts[1..2]).is_empty());
    assert!(providers(&[], &[]).is_empty());
}

/// A mount of the host path `/src` at `target`, every other field unset.
fn mount_at(target: &str) -> v1::Mount {
    v1::Mount {
        source: Some(v1::mount::Source::HostPath("/src".to_owned())),
        target: target.to_owned(),
        ..v1::Mount::default()
    }
}
