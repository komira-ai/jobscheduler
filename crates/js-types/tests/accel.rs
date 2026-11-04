//! The accelerator vocabulary: the validated tokens, the alias table and its resolution,
//! the one way an accelerator is built, and the one comparison of a need with it.

use std::collections::BTreeMap;

use js_types::{
    ACCEL_KEYS, AccelError, Accelerator, AcceleratorMiss, AcceleratorNeed, FormFactor,
    Interconnect, MAX_MEMORY_GIB, Model, Vendor, accelerator_of, vendor_of,
};

const GIB: u64 = 1 << 30;

fn model(text: &str) -> Model {
    Model::parse(text).unwrap()
}

fn vendor(text: &str) -> Vendor {
    Vendor::parse(text).unwrap()
}

fn caps(pairs: &[(&str, &str)]) -> BTreeMap<String, String> {
    pairs
        .iter()
        .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
        .collect()
}

/// An accelerator with every field set, to vary one at a time.
fn have() -> Accelerator {
    Accelerator {
        vendor: vendor("nvidia"),
        model: model("nvidia-a100"),
        variant: None,
        count: 8,
        memory_per_device: Some(80 * GIB),
        form_factor: Some(FormFactor::Sxm),
        interconnect: Some(Interconnect::new("nvlink-domain", 8).unwrap()),
    }
}

/// Asserts the vendor token: 1 to 32 of `a-z`, `0-9` and `-`, any such text accepted
/// (the set is open), and everything else refused naming the text. Catches a vendor that
/// is upper case, empty or unbounded reaching the matcher.
#[test]
fn a_vendor_is_a_bounded_lower_case_token() {
    let (longest, too_long) = ("v".repeat(32), "v".repeat(33));
    for good in ["nvidia", "amd", "x", "a-1", longest.as_str()] {
        assert_eq!(Vendor::parse(good).unwrap().as_str(), good);
        assert_eq!(Vendor::parse(good).unwrap().to_string(), good);
    }
    for bad in ["", "NVIDIA", "nv idia", "nv_idia", too_long.as_str()] {
        assert_eq!(
            Vendor::parse(bad),
            Err(AccelError::BadVendor(bad.to_owned()))
        );
    }
    assert_eq!(
        AccelError::BadVendor("X".into()).to_string(),
        "vendor must be 1 to 32 of a-z, 0-9 and '-', not \"X\""
    );
}

/// Asserts the model name: lower-case words joined by single `-` of any length (a GPU
/// class of 65 or 300 characters, which the previous release took, is still a model), and
/// everything else refused naming the text. Catches a model that the agent's spelling rule
/// could never print reaching the table or the matcher, and a length bound that would
/// refuse a configuration the previous release accepted.
#[test]
fn a_model_is_lower_case_words_joined_by_single_dashes() {
    let (long, longer) = ("m".repeat(65), format!("nvidia-{}", "a".repeat(300)));
    for good in [
        "nvidia-a100-sxm4-80gb",
        "a100",
        "x-1",
        long.as_str(),
        longer.as_str(),
    ] {
        assert_eq!(model(good).as_str(), good);
        assert_eq!(model(good).to_string(), good);
    }
    for bad in [
        "",
        "-a",
        "a-",
        "a--b",
        "A100",
        "NVIDIA-A10",
        "nvidia a10",
        "nvidia-a10 ",
        "a_b",
        "a.b",
        "nvidia-\u{e9}",
    ] {
        assert_eq!(Model::parse(bad), Err(AccelError::BadModel(bad.to_owned())));
    }
}

/// Asserts the form factor's two names round trip and anything else is refused naming
/// the text. Catches a form factor read case-blind or a third one accepted that no table
/// row carries.
#[test]
fn a_form_factor_is_sxm_or_pcie() {
    for ff in [FormFactor::Sxm, FormFactor::Pcie] {
        assert_eq!(ff.name().parse::<FormFactor>(), Ok(ff));
        assert_eq!(ff.to_string(), ff.name());
    }
    for bad in ["SXM", "oam", ""] {
        assert_eq!(
            bad.parse::<FormFactor>(),
            Err(AccelError::BadFormFactor(bad.to_owned()))
        );
    }
}

/// Asserts the interconnect: `kind>=size` parses into its parts and prints back, it is
/// met by the same kind with a domain at least as large (the boundary included) and by no
/// other kind, and text without `>=`, a size or a valid kind is refused naming the text.
/// Catches a size compared the wrong way round or an unknown kind matched by prefix.
#[test]
fn an_interconnect_is_a_kind_and_a_least_size() {
    let want = Interconnect::parse("nvlink-domain>=72").unwrap();
    assert_eq!((want.kind(), want.size()), ("nvlink-domain", 72));
    assert_eq!(want.to_string(), "nvlink-domain>=72");
    let at = |size| Interconnect::new("nvlink-domain", size).unwrap();
    assert!(at(72).satisfies(&want));
    assert!(at(144).satisfies(&want));
    assert!(!at(71).satisfies(&want));
    assert!(
        !Interconnect::new("nvlink-domain-x", 72)
            .unwrap()
            .satisfies(&want)
    );
    assert!(!Interconnect::new("nvlink", 72).unwrap().satisfies(&want));
    for bad in ["nvlink", "nvlink>=", "nvlink>=x", ">=4", "NV>=4", "a>=-1"] {
        assert_eq!(
            Interconnect::parse(bad),
            Err(AccelError::BadInterconnect(bad.to_owned()))
        );
    }
    assert_eq!(
        Interconnect::new("a--b", 2),
        Err(AccelError::BadInterconnect("a--b>=2".to_owned()))
    );
    assert!(Interconnect::new(&"k".repeat(33), 1).is_err());
}

/// Asserts resolution: a variant spelling becomes its family with that variant's nominal
/// memory and form factor; a family stays itself; an unknown name stays itself with
/// `known: false` and no variant, and is never matched to a known family by its prefix.
/// Catches a lookup that widens, drops or rewrites an unknown name.
#[test]
fn a_name_resolves_to_its_family_and_variant_or_passes_through() {
    let a100 = model("nvidia-a100-sxm4-80gb").canonical();
    assert_eq!(a100.model.as_str(), "nvidia-a100");
    let variant = a100.variant.unwrap();
    assert_eq!(variant.memory_per_device, 80 * GIB);
    assert_eq!(variant.form_factor, FormFactor::Sxm);
    assert!(a100.known);
    let h100 = model("nvidia-h100-80gb-hbm3").canonical();
    assert_eq!(h100.model.as_str(), "nvidia-h100");
    assert_eq!(h100.variant.unwrap().memory_per_device, 80 * GIB);
    for unknown in ["nvidia-gb300", "nvidia-a100-sxm4-96gb", "nvidia-a1000"] {
        let resolved = model(unknown).canonical();
        assert_eq!(resolved.model.as_str(), unknown);
        assert_eq!((resolved.variant, resolved.known), (None, false));
    }
}

/// Asserts the vendor derivation in its order: the table's vendor for a family or a
/// variant spelling, the first word of an unknown dashed name when that is a vendor token,
/// and `unknown` for an undashed name (a legacy class such as `a100`) or a first word too
/// long to be a vendor. Catches a new accelerator that fails `vendor=nvidia` before the
/// table learns it, or a vendor guessed from text that names none.
#[test]
fn the_vendor_comes_from_the_table_then_the_first_word_then_unknown() {
    assert_eq!(
        vendor_of(&model("nvidia-a100-sxm4-80gb")).as_str(),
        "nvidia"
    );
    assert_eq!(vendor_of(&model("apple-m")).as_str(), "apple");
    assert_eq!(vendor_of(&model("nvidia-gb300")).as_str(), "nvidia");
    assert_eq!(vendor_of(&model("amd-mi300x")).as_str(), "amd");
    assert_eq!(vendor_of(&model("a100")).as_str(), Vendor::UNKNOWN);
    let long = format!("{}-x", "v".repeat(33));
    assert_eq!(vendor_of(&model(&long)).as_str(), Vendor::UNKNOWN);
}

/// Asserts the one builder: a known variant takes its nominal memory even when less was
/// measured (81559 MiB on an H100 80GB HBM3 is below 80 GiB), an unknown name takes the
/// measured memory, a family name with no variant takes the measured memory, and the
/// spelling is kept as the variant only when it is not the family. Catches a measured
/// compare that refuses the very device a request names, or measured memory dropped for an
/// accelerator the table does not know.
#[test]
fn memory_is_the_variants_nominal_size_else_what_was_measured() {
    let measured = 81_559 * (1 << 20);
    let h100 = accelerator_of(&model("nvidia-h100-80gb-hbm3"), 8, Some(measured));
    assert_eq!(h100.memory_per_device, Some(80 * GIB));
    assert_eq!(h100.model.as_str(), "nvidia-h100");
    assert_eq!(
        h100.variant.as_ref().map(Model::as_str),
        Some("nvidia-h100-80gb-hbm3")
    );
    assert_eq!((h100.count, h100.form_factor), (8, Some(FormFactor::Sxm)));
    let need = AcceleratorNeed {
        memory_per_device: Some(80 * GIB),
        ..AcceleratorNeed::default()
    };
    assert_eq!(need.met_by(Some(&h100)), Ok(()));
    let unknown = accelerator_of(&model("nvidia-gb300"), 4, Some(measured));
    assert_eq!(unknown.memory_per_device, Some(measured));
    assert_eq!((unknown.variant.clone(), unknown.form_factor), (None, None));
    assert_eq!(need.met_by(Some(&unknown)), Err(AcceleratorMiss::Memory));
    let a10 = accelerator_of(&model("nvidia-a10"), 1, Some(24 * GIB));
    assert_eq!(a10.memory_per_device, Some(24 * GIB));
    assert_eq!(a10.variant, None);
    assert_eq!(
        accelerator_of(&model("nvidia-a10"), 1, None).memory_per_device,
        None
    );
}

/// Asserts the comparison in its order, one field wrong at a time and then several, with
/// each boundary exactly: vendor, model, count at least, memory at least (unknown memory
/// fails a fixed one), interconnect at least, form factor equal; and that a need with
/// nothing set is met by a host with no accelerator while a need for any device is not.
/// Catches `==` for an at-least field, a reversed compare, an off-by-one, or a changed
/// order (the first miss is reported to clients).
#[test]
fn the_comparison_reports_the_first_miss_in_a_fixed_order() {
    let full = AcceleratorNeed {
        vendor: Some(vendor("nvidia")),
        model: Some(model("nvidia-a100")),
        count: 8,
        memory_per_device: Some(80 * GIB),
        interconnect: Some(Interconnect::new("nvlink-domain", 8).unwrap()),
        form_factor: Some(FormFactor::Sxm),
    };
    assert_eq!(full.met_by(Some(&have())), Ok(()));
    let with = |change: fn(&mut Accelerator)| {
        let mut host = have();
        change(&mut host);
        full.met_by(Some(&host))
    };
    assert_eq!(
        with(|h| h.vendor = vendor("amd")),
        Err(AcceleratorMiss::Vendor)
    );
    assert_eq!(
        with(|h| h.model = model("nvidia-h100")),
        Err(AcceleratorMiss::Model)
    );
    assert_eq!(with(|h| h.count = 7), Err(AcceleratorMiss::Count));
    assert_eq!(with(|h| h.count = 9), Ok(()));
    assert_eq!(
        with(|h| h.memory_per_device = Some(80 * GIB - 1)),
        Err(AcceleratorMiss::Memory)
    );
    assert_eq!(
        with(|h| h.memory_per_device = None),
        Err(AcceleratorMiss::Memory)
    );
    assert_eq!(with(|h| h.memory_per_device = Some(141 * GIB)), Ok(()));
    assert_eq!(
        with(|h| h.interconnect = Some(Interconnect::new("nvlink-domain", 7).unwrap())),
        Err(AcceleratorMiss::Interconnect)
    );
    assert_eq!(
        with(|h| h.interconnect = None),
        Err(AcceleratorMiss::Interconnect)
    );
    assert_eq!(
        with(|h| h.form_factor = Some(FormFactor::Pcie)),
        Err(AcceleratorMiss::FormFactor)
    );
    assert_eq!(
        with(|h| h.form_factor = None),
        Err(AcceleratorMiss::FormFactor)
    );
    assert_eq!(
        with(|h| {
            h.count = 1;
            h.memory_per_device = None;
            h.form_factor = None;
        }),
        Err(AcceleratorMiss::Count)
    );
    assert_eq!(full.met_by(None), Err(AcceleratorMiss::Vendor));
    assert_eq!(AcceleratorNeed::default().met_by(None), Ok(()));
    let one = AcceleratorNeed {
        count: 1,
        ..AcceleratorNeed::default()
    };
    assert_eq!(one.met_by(None), Err(AcceleratorMiss::Count));
    assert_eq!(one.met_by(Some(&have())), Ok(()));
}

/// Asserts that a need naming a full spelling is met only by that variant of the family:
/// the 40 GB SXM4 spelling is met by a 40 GB SXM4 host and missed, by memory, by the 80 GB
/// one (more memory is not that variant) and the reverse; a 40 GB PCIe host misses it by
/// form factor; a memory the need also asks for is still at least; and a family name or an
/// unknown name pins nothing. Catches a full spelling widened to its family or to any
/// larger variant.
#[test]
fn a_full_spelling_need_pins_its_variant() {
    let host = |spelling: &str| accelerator_of(&model(spelling), 8, None);
    let named = |spelling: &str| AcceleratorNeed {
        model: Some(model(spelling)),
        count: 2,
        ..AcceleratorNeed::default()
    };
    let sxm40 = named("nvidia-a100-sxm4-40gb");
    assert_eq!(sxm40.met_by(Some(&host("nvidia-a100-sxm4-40gb"))), Ok(()));
    assert_eq!(
        sxm40.met_by(Some(&host("nvidia-a100-sxm4-80gb"))),
        Err(AcceleratorMiss::Memory)
    );
    assert_eq!(
        named("nvidia-a100-sxm4-80gb").met_by(Some(&host("nvidia-a100-sxm4-40gb"))),
        Err(AcceleratorMiss::Memory)
    );
    let pcie40 = Accelerator {
        vendor: vendor("nvidia"),
        model: model("nvidia-a100"),
        variant: None,
        count: 8,
        memory_per_device: Some(40 * GIB),
        form_factor: Some(FormFactor::Pcie),
        interconnect: None,
    };
    assert_eq!(
        sxm40.met_by(Some(&pcie40)),
        Err(AcceleratorMiss::FormFactor)
    );
    let contradicted = AcceleratorNeed {
        form_factor: Some(FormFactor::Pcie),
        ..sxm40.clone()
    };
    assert_eq!(
        contradicted.met_by(Some(&host("nvidia-a100-sxm4-40gb"))),
        Err(AcceleratorMiss::FormFactor),
        "a form factor the need names is not dropped for the variant's"
    );
    let more = AcceleratorNeed {
        memory_per_device: Some(64 * GIB),
        ..sxm40.clone()
    };
    assert_eq!(
        more.met_by(Some(&host("nvidia-a100-sxm4-40gb"))),
        Err(AcceleratorMiss::Memory)
    );
    let family = named("nvidia-a100");
    for spelling in ["nvidia-a100-sxm4-40gb", "nvidia-a100-sxm4-80gb"] {
        assert_eq!(family.met_by(Some(&host(spelling))), Ok(()), "{spelling}");
    }
    assert_eq!(family.met_by(Some(&pcie40)), Ok(()));
    let gb300 = named("nvidia-gb300");
    let measured = accelerator_of(&model("nvidia-gb300"), 8, Some(279 * GIB));
    assert_eq!(gb300.met_by(Some(&measured)), Ok(()));
}

/// Asserts that a need round trips through its capability keys (memory in whole GiB,
/// rounded up so it never asks for less), that no key reads as no need and a zero count
/// is not written, that a bad value is refused naming its key, and that the memory key
/// reads every GiB up to `MAX_MEMORY_GIB` and refuses the next, while any byte count a need
/// holds (to `u64::MAX`) is written as a key that reads back. Catches a need whose keys
/// lose a field, whose memory is rounded down into asking for less, or whose memory is
/// written as a value its own reader refuses.
#[test]
fn a_need_round_trips_through_its_capability_keys() {
    let need = AcceleratorNeed {
        vendor: Some(vendor("nvidia")),
        model: Some(model("nvidia-a100")),
        count: 4,
        memory_per_device: Some(80 * GIB),
        interconnect: Some(Interconnect::new("nvlink-domain", 72).unwrap()),
        form_factor: Some(FormFactor::Pcie),
    };
    let written = need.to_capabilities();
    let keys: Vec<&str> = written.iter().map(|(key, _)| *key).collect();
    assert_eq!(keys, ACCEL_KEYS);
    let read: BTreeMap<String, String> = written
        .into_iter()
        .map(|(key, value)| (key.to_owned(), value))
        .collect();
    assert_eq!(AcceleratorNeed::from_capabilities(&read), Ok(Some(need)));
    assert_eq!(
        AcceleratorNeed::from_capabilities(&caps(&[("os", "linux")])),
        Ok(None)
    );
    assert_eq!(AcceleratorNeed::default().to_capabilities(), vec![]);
    let odd = AcceleratorNeed {
        memory_per_device: Some(80 * GIB + 1),
        ..AcceleratorNeed::default()
    };
    assert_eq!(
        odd.to_capabilities(),
        vec![("accelerator.memory-gib", "81".to_owned())]
    );
    let largest = MAX_MEMORY_GIB.to_string();
    let read =
        AcceleratorNeed::from_capabilities(&caps(&[("accelerator.memory-gib", largest.as_str())]));
    assert_eq!(
        read.unwrap().unwrap().memory_per_device,
        Some(MAX_MEMORY_GIB * GIB),
        "the largest GiB whose bytes fit is read"
    );
    let first_refused = (MAX_MEMORY_GIB + 1).to_string();
    assert_eq!(
        AcceleratorNeed::from_capabilities(&caps(&[(
            "accelerator.memory-gib",
            first_refused.as_str()
        )])),
        Err(AccelError::BadNumber {
            key: "accelerator.memory-gib",
            value: first_refused.clone()
        })
    );
    for bytes in [1_u64 << 62, MAX_MEMORY_GIB * GIB, u64::MAX] {
        let huge = AcceleratorNeed {
            memory_per_device: Some(bytes),
            ..AcceleratorNeed::default()
        };
        let written: BTreeMap<String, String> = huge
            .to_capabilities()
            .into_iter()
            .map(|(key, value)| (key.to_owned(), value))
            .collect();
        let back = AcceleratorNeed::from_capabilities(&written)
            .unwrap()
            .unwrap();
        let memory = back.memory_per_device.unwrap();
        assert!(memory >= bytes.min(MAX_MEMORY_GIB * GIB), "{bytes}");
    }
    assert_eq!(
        AcceleratorNeed::from_capabilities(&caps(&[("accelerator.memory-gib", "lots")])),
        Err(AccelError::BadNumber {
            key: "accelerator.memory-gib",
            value: "lots".into()
        })
    );
    assert_eq!(
        AccelError::BadNumber {
            key: "accelerator.count",
            value: "x".into()
        }
        .to_string(),
        "accelerator.count must be a whole number, not \"x\""
    );
}

/// Asserts that a host's accelerator round trips through the entries it reports (memory
/// rounded down, so a host never claims more), that a vendor left out is derived from the
/// model, that entries naming no model read as none, and that a variant spelling is read
/// as its family with the spelling kept. Catches a host report that loses its count or
/// memory, or claims more memory than it has.
#[test]
fn a_hosts_accelerator_round_trips_through_its_report() {
    let host = Accelerator {
        variant: None,
        ..have()
    };
    let read: BTreeMap<String, String> = host
        .to_capabilities()
        .into_iter()
        .map(|(key, value)| (key.to_owned(), value))
        .collect();
    assert_eq!(Accelerator::from_capabilities(&read), Ok(Some(host)));
    let low = Accelerator {
        memory_per_device: Some(80 * GIB - 1),
        interconnect: None,
        form_factor: None,
        ..have()
    };
    assert!(
        low.to_capabilities()
            .contains(&("accelerator.memory-gib", "79".to_owned()))
    );
    let derived = Accelerator::from_capabilities(&caps(&[
        ("accelerator.model", "nvidia-a100-sxm4-80gb"),
        ("accelerator.count", "2"),
    ]))
    .unwrap()
    .unwrap();
    assert_eq!(derived.vendor.as_str(), "nvidia");
    assert_eq!(derived.model.as_str(), "nvidia-a100");
    assert_eq!(derived.variant, Some(model("nvidia-a100-sxm4-80gb")));
    assert_eq!(derived.count, 2);
    assert_eq!(
        Accelerator::from_capabilities(&caps(&[("accelerator.count", "2")])),
        Ok(None)
    );
    assert_eq!(Accelerator::from_capabilities(&caps(&[])), Ok(None));
    assert!(Accelerator::from_capabilities(&caps(&[("accelerator.model", "A")])).is_err());
}
