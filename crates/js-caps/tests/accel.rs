//! The accelerator vocabulary as `js-caps` serves it: the alias table's rows, the one
//! conversion of a request's accelerator ([`request_need`]), and the `accelerator.*` keys
//! a request and a node report are matched on.

use std::fs;
use std::path::{Path, PathBuf};

use js_caps::{
    ALIASES, AccelError, AcceleratorMiss, AcceleratorNeed, AcceleratorRequest, CpuCaps, FAMILIES,
    Model, NodeCaps, ReportError, Request, RequestError, RequestNeed, RequestNeedError, Unmet,
    Vendor, check_stated, request_need,
};
use js_types::Resources;

const CPUINFO: &str = include_str!("fixtures/skylake_sp_platinum_8180.cpuinfo");

/// A node reporting `entries` beside an `x86_64` CPU.
fn node(entries: &[(&str, &str)]) -> NodeCaps {
    let mut report = vec![("arch", "x86_64")];
    report.extend_from_slice(entries);
    NodeCaps::from_report(report).unwrap()
}

/// A node of eight GPUs reported as `spelling`, as the server renders a host whose agent
/// reported that class.
fn gpu_node(spelling: &str) -> NodeCaps {
    node(&[("accelerator.model", spelling), ("accelerator.count", "8")])
}

/// Asserts that every row of the table is well formed: families and spellings sorted and
/// unique, every spelling a model name that is not itself a family, every row's family a
/// family of the table that resolves to itself, every family's vendor a vendor token, and
/// no two spellings sharing a family, memory and form factor. Catches a row added out of
/// order (binary search then misses it), duplicated, or pointing at a family the table
/// does not define.
#[test]
fn every_alias_row_is_well_formed() {
    assert!(FAMILIES.windows(2).all(|w| w[0].name < w[1].name));
    assert!(ALIASES.windows(2).all(|w| w[0].spelling < w[1].spelling));
    for family in FAMILIES {
        assert!(Vendor::parse(family.vendor).is_ok(), "{}", family.vendor);
        let resolved = Model::parse(family.name).unwrap().canonical();
        assert_eq!(resolved.model.as_str(), family.name);
        assert_eq!((resolved.variant, resolved.known), (None, true));
    }
    for row in ALIASES {
        let resolved = Model::parse(row.spelling).unwrap().canonical();
        assert_eq!(resolved.model.as_str(), row.family, "{}", row.spelling);
        assert!(resolved.known);
        assert!(
            FAMILIES.iter().any(|f| f.name == row.family),
            "{}",
            row.spelling
        );
        assert!(
            FAMILIES.iter().all(|f| f.name != row.spelling),
            "{}",
            row.spelling
        );
        let twins = ALIASES.iter().filter(|other| {
            (other.family, other.memory_gib, other.form_factor)
                == (row.family, row.memory_gib, row.form_factor)
        });
        assert_eq!(twins.count(), 1, "{} has a synonym", row.spelling);
    }
}

/// Every `.rs` file under `dir`, recursively, in a stable order.
fn rust_files(dir: &Path, out: &mut Vec<PathBuf>) {
    let mut entries: Vec<PathBuf> = fs::read_dir(dir)
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .collect();
    entries.sort();
    for path in entries {
        if path.is_dir() {
            rust_files(&path, out);
        } else if path.extension().is_some_and(|ext| ext == "rs") {
            out.push(path);
        }
    }
}

/// The lines of `text` that spell a canonical family as a string literal, outside
/// comments and before the first `#[cfg(test)]`.
fn spelt_families(text: &str) -> Vec<String> {
    let mut found = Vec::new();
    for line in text.lines() {
        if line.contains("#[cfg(test)]") {
            break;
        }
        if line.trim_start().starts_with("//") {
            continue;
        }
        for family in FAMILIES {
            if line.contains(&format!("\"{}\"", family.name)) {
                found.push(line.trim().to_owned());
            }
        }
    }
    found
}

/// Asserts that no production source of the workspace spells a canonical family as a
/// string literal, except the table itself in `js-types/src/accel.rs`: a provider or the
/// agent that needs a family names it through the table, and translates its own source
/// spellings to one. Test code is not scanned (files named `*_tests.rs`, and a file's lines
/// from its first `#[cfg(test)]`), nor `xtask`, whose site demo records the text a user
/// types. Checks the scan itself on a line that spells a family, so an empty scan cannot
/// pass by reading nothing. Catches a provider or the agent defining a family of its own.
#[test]
fn no_canonical_name_is_defined_outside_js_caps() {
    assert_eq!(
        spelt_families("const X: &str = \"nvidia-a100\";\n#[cfg(test)]\n\"nvidia-h100\"\n"),
        ["const X: &str = \"nvidia-a100\";"]
    );
    assert!(spelt_families("// \"nvidia-a100\" in a comment\n\"nvidia-a100x\"\n").is_empty());
    let crates = Path::new(env!("CARGO_MANIFEST_DIR")).join("..");
    let mut files = Vec::new();
    let mut members: Vec<PathBuf> = fs::read_dir(&crates)
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .filter(|path| path.file_name().is_some_and(|name| name != "xtask"))
        .collect();
    members.sort();
    for member in members {
        let src = member.join("src");
        if src.is_dir() {
            rust_files(&src, &mut files);
        }
    }
    assert!(files.len() > 100, "the scan reads the workspace's sources");
    let table = Path::new("js-types").join("src").join("accel.rs");
    let mut spelt = Vec::new();
    for file in files {
        let name = file.file_name().unwrap().to_string_lossy().into_owned();
        if file.ends_with(&table) || name.ends_with("_tests.rs") {
            continue;
        }
        for line in spelt_families(&fs::read_to_string(&file).unwrap()) {
            spelt.push(format!("{}: {line}", file.display()));
        }
    }
    assert!(
        spelt.is_empty(),
        "canonical names spelt outside the table: {spelt:#?}"
    );
}

/// Asserts the matcher on the `accelerator.*` keys: a family need is met by every
/// variant the table knows, a full spelling only by its own variant (the 40 GB variant
/// misses on memory), an unknown name only by an identical unknown name, never by a known
/// family or by a name that starts like it, and the reverse. Catches a matcher that
/// compares spellings, drops an unknown name, or matches by prefix.
#[test]
fn an_unknown_name_matches_only_an_identical_one() {
    let need = |model: &str| Request::parse([("accelerator.model", model)]).unwrap();
    let family = need("nvidia-a100");
    assert!(family.matches(&gpu_node("nvidia-a100-sxm4-80gb")));
    assert!(family.matches(&gpu_node("nvidia-a100-sxm4-40gb")));
    let full = need("nvidia-a100-sxm4-80gb");
    assert!(full.matches(&gpu_node("nvidia-a100-sxm4-80gb")));
    assert_eq!(
        full.unmet(&gpu_node("nvidia-a100-sxm4-40gb")),
        [Unmet::Accelerator(AcceleratorMiss::Memory)]
    );
    let gb300 = need("nvidia-gb300");
    assert!(gb300.matches(&gpu_node("nvidia-gb300")));
    for other in ["nvidia-a100-sxm4-80gb", "nvidia-gb300-x", "nvidia-gb30"] {
        assert_eq!(
            gb300.unmet(&gpu_node(other)),
            [Unmet::Accelerator(AcceleratorMiss::Model)],
            "{other}"
        );
    }
    assert!(!family.matches(&gpu_node("nvidia-a100x")));
    let shown = [
        (AcceleratorMiss::Vendor, "accelerator.vendor"),
        (AcceleratorMiss::Model, "accelerator.model"),
        (AcceleratorMiss::Count, "accelerator.count"),
        (AcceleratorMiss::Memory, "accelerator.memory-gib"),
        (AcceleratorMiss::Interconnect, "accelerator.interconnect"),
        (AcceleratorMiss::FormFactor, "accelerator.form-factor"),
    ];
    for (miss, key) in shown {
        assert_eq!(Unmet::Accelerator(miss).to_string(), key);
    }
    assert!(!family.matches(&gpu_node("nvidia-gb300")));
    assert!(!need("nvidia-a1").matches(&gpu_node("nvidia-a100-sxm4-80gb")));
}

/// Asserts the rest of the keys on the matcher: count, vendor and memory are compared,
/// a node that reports no accelerator meets a need with nothing set and misses any other,
/// and the miss reads as the key that names it. Catches a node report whose accelerator
/// is dropped, or a miss reported under another key.
#[test]
fn a_node_reports_its_accelerator_and_a_request_names_its_need() {
    let host = node(&[
        ("accelerator.model", "nvidia-h100-80gb-hbm3"),
        ("accelerator.count", "8"),
        ("accelerator.interconnect", "nvlink-domain>=8"),
    ]);
    let accel = host.accelerator.as_ref().unwrap();
    assert_eq!(
        (accel.model.as_str(), accel.vendor.as_str()),
        ("nvidia-h100", "nvidia")
    );
    let request = |pairs: &[(&str, &str)]| Request::parse(pairs.iter().copied()).unwrap();
    assert!(
        request(&[("accelerator.count", "8"), ("accelerator.vendor", "nvidia")]).matches(&host)
    );
    let cases = [
        (
            ("accelerator.count", "9"),
            AcceleratorMiss::Count,
            "accelerator.count",
        ),
        (
            ("accelerator.vendor", "amd"),
            AcceleratorMiss::Vendor,
            "accelerator.vendor",
        ),
        (
            ("accelerator.memory-gib", "81"),
            AcceleratorMiss::Memory,
            "accelerator.memory-gib",
        ),
        (
            ("accelerator.interconnect", "nvlink-domain>=72"),
            AcceleratorMiss::Interconnect,
            "accelerator.interconnect",
        ),
        (
            ("accelerator.form-factor", "pcie"),
            AcceleratorMiss::FormFactor,
            "accelerator.form-factor",
        ),
    ];
    for (pair, miss, key) in cases {
        let asked = request(&[pair]);
        let unmet = asked.unmet(&host);
        assert_eq!(unmet, [Unmet::Accelerator(miss)], "{key}");
        assert_eq!(unmet[0].to_string(), key);
    }
    assert!(request(&[("accelerator.memory-gib", "80")]).matches(&host));
    let bare = NodeCaps::new(CpuCaps::from_linux_cpuinfo(CPUINFO).unwrap());
    assert_eq!(bare.accelerator, None);
    let nothing = request(&[("accelerator.count", "0")]);
    assert!(nothing.matches(&bare));
    assert_eq!(nothing.accelerator(), None, "a need for nothing is no need");
    assert_eq!(
        Request::parse(nothing.properties().iter().map(|(k, v)| (*k, v.as_str()))),
        Ok(nothing.clone())
    );
    assert_eq!(
        request(&[("accelerator.vendor", "nvidia")]).unmet(&bare),
        [Unmet::Accelerator(AcceleratorMiss::Vendor)]
    );
    assert_eq!(
        request(&[("accelerator.model", "nvidia-a100")]).unmet(&bare),
        [Unmet::Accelerator(AcceleratorMiss::Model)]
    );
}

/// Asserts that the keys round trip through a request's properties, that a key named twice
/// or holding a bad value is refused naming it, and that a node report refuses the same,
/// and reads entries naming no model as no accelerator. Catches a request whose
/// accelerator is lost when written back, or a bad value read as no constraint.
#[test]
fn accelerator_keys_are_validated_and_round_trip() {
    let pairs = [
        ("accelerator.vendor", "nvidia"),
        ("accelerator.model", "nvidia-a100"),
        ("accelerator.count", "4"),
        ("accelerator.memory-gib", "80"),
        ("accelerator.interconnect", "nvlink-domain>=72"),
        ("accelerator.form-factor", "sxm"),
    ];
    let request = Request::parse(pairs).unwrap();
    let written: Vec<(&str, String)> = request.properties();
    let want: Vec<(&str, String)> = pairs.iter().map(|(k, v)| (*k, (*v).to_owned())).collect();
    assert_eq!(written, want);
    assert_eq!(
        Request::parse(written.iter().map(|(k, v)| (*k, v.as_str()))),
        Ok(request.clone())
    );
    assert_eq!(request.accelerator().map(|need| need.count), Some(4));
    assert_eq!(
        Request::parse([("arch", "x86_64")]).unwrap().accelerator(),
        None
    );
    assert_eq!(
        Request::parse([("accelerator.count", "1"), ("accelerator.count", "2")]),
        Err(RequestError::Repeated("accelerator.count".to_owned()))
    );
    assert_eq!(
        Request::parse([("accelerator.form-factor", "oam")]),
        Err(RequestError::Accelerator(AccelError::BadFormFactor(
            "oam".to_owned()
        )))
    );
    let report = |entries: &[(&str, &str)]| {
        let mut all = vec![("arch", "x86_64")];
        all.extend_from_slice(entries);
        NodeCaps::from_report(all)
    };
    assert_eq!(
        report(&[("accelerator.model", "a"), ("accelerator.model", "b")]),
        Err(ReportError::Repeated("accelerator.model".to_owned()))
    );
    assert_eq!(
        report(&[("accelerator.model", "A100")]),
        Err(ReportError::Accelerator(AccelError::BadModel(
            "A100".to_owned()
        )))
    );
    assert_eq!(
        report(&[("accelerator.count", "8")]).unwrap().accelerator,
        None
    );
}

/// `request_need` of a body with the pair `gpus`/`gpu_class` and an optional object.
fn convert(
    gpus: u64,
    class: Option<&str>,
    object: Option<AcceleratorRequest>,
) -> Result<RequestNeed, RequestNeedError> {
    request_need(gpus, class, object.as_ref())
}

/// The capability entries of a conversion, as owned pairs for comparing.
fn entries(need: &RequestNeed) -> Vec<(&str, &str)> {
    need.capabilities
        .iter()
        .map(|(key, value)| (*key, value.as_str()))
        .collect()
}

/// Asserts the conversion of a request that carries only today's pair, or nothing: both
/// book what they say and add no capability key, so the request is stored and compared as
/// the previous release stores it, and the need the pair states is derived when matching
/// (a full spelling as its model and `gpus` as its count). Catches a key added to today's
/// body, which would make a repeated acquire differ from the lease the previous release
/// stored, or a pair whose class stops constraining placement.
#[test]
fn todays_pair_converts_to_the_same_booking_and_its_keys() {
    let none = convert(0, None, None).unwrap();
    assert_eq!(
        (none.gpus, none.gpu_class, none.capabilities),
        (0, None, vec![])
    );
    let full = convert(2, Some("nvidia-a100-sxm4-80gb"), None).unwrap();
    assert_eq!(
        (full.gpus, full.gpu_class.as_deref()),
        (2, Some("nvidia-a100-sxm4-80gb"))
    );
    assert!(full.capabilities.is_empty());
    let booked = Resources::new(1000, 1, 2, full.gpu_class.clone()).unwrap();
    let need = AcceleratorNeed::booked(&booked).unwrap();
    assert_eq!(
        need.model.as_ref().map(Model::as_str),
        Some("nvidia-a100-sxm4-80gb")
    );
    assert_eq!(need.count, 2);
    let odd = convert(1, Some("A100_X"), None).unwrap();
    assert_eq!((odd.gpus, odd.gpu_class.as_deref()), (1, Some("A100_X")));
    assert!(odd.capabilities.is_empty());
    let odd = Resources::new(1000, 1, 1, Some("A100_X".into())).unwrap();
    assert_eq!(AcceleratorNeed::booked(&odd), None);
    assert_eq!(
        AcceleratorNeed::booked(&Resources::new(1000, 1, 0, None).unwrap()),
        None
    );
}

/// Asserts that `accelerator.*` capabilities a request names itself must agree with the
/// GPUs it books: another family or variant, or another count, is refused naming
/// `accelerator`; the same family and count, a vendor only, or a request that books no GPU
/// agree; a key that is not valid is refused. Catches a request booked as one model and
/// matched as another through its own capabilities.
#[test]
fn stated_capabilities_must_agree_with_the_booked_pair() {
    let stated = |pairs: &[(&str, &str)]| {
        pairs
            .iter()
            .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
            .collect::<std::collections::BTreeMap<String, String>>()
    };
    let class = Some("nvidia-a100-sxm4-80gb");
    let check = |pairs: &[(&str, &str)]| check_stated(&stated(pairs), 4, class);
    assert_eq!(check(&[]), Ok(()));
    assert_eq!(
        check(&[("accelerator.model", "nvidia-a100-sxm4-80gb")]),
        Ok(())
    );
    assert_eq!(check(&[("accelerator.count", "4")]), Ok(()));
    assert_eq!(check(&[("accelerator.vendor", "nvidia")]), Ok(()));
    assert_eq!(
        check(&[("accelerator.model", "nvidia-a100")]),
        Err(RequestNeedError::ModelDisagrees {
            accelerator: "nvidia-a100".into(),
            gpu_class: "nvidia-a100-sxm4-80gb".into()
        })
    );
    assert_eq!(
        check(&[("accelerator.count", "8")]),
        Err(RequestNeedError::CountDisagrees {
            accelerator: 8,
            gpus: 4
        })
    );
    assert!(matches!(
        check(&[("accelerator.form-factor", "oam")]),
        Err(RequestNeedError::Field(_))
    ));
    let cpu = stated(&[("accelerator.model", "nvidia-h100")]);
    assert_eq!(check_stated(&cpu, 0, None), Ok(()));
}

/// Asserts the conversion of a request that carries only the new object: it books its
/// count of its model (the class is the model as spelt), a vendor-only or model-only need
/// books nothing, every field becomes its key, and a count without a model or a bad field
/// is refused naming `accelerator`. Catches a count-only need that would be stored as GPUs
/// of no class (read as whole-host by the previous release), or a field dropped.
#[test]
fn the_object_converts_to_its_booking_and_keys() {
    let object = AcceleratorRequest {
        vendor: Some("nvidia".into()),
        model: Some("nvidia-a100".into()),
        count: Some(4),
        memory_per_device_gib: Some(80),
        interconnect: Some("nvlink-domain>=72".into()),
        form_factor: Some("sxm".into()),
    };
    let need = convert(0, None, Some(object)).unwrap();
    assert_eq!(
        (need.gpus, need.gpu_class.as_deref()),
        (4, Some("nvidia-a100"))
    );
    assert_eq!(
        entries(&need),
        [
            ("accelerator.vendor", "nvidia"),
            ("accelerator.model", "nvidia-a100"),
            ("accelerator.count", "4"),
            ("accelerator.memory-gib", "80"),
            ("accelerator.interconnect", "nvlink-domain>=72"),
            ("accelerator.form-factor", "sxm"),
        ]
    );
    let vendor_only = AcceleratorRequest {
        vendor: Some("nvidia".into()),
        ..AcceleratorRequest::default()
    };
    let need = convert(0, None, Some(vendor_only)).unwrap();
    assert_eq!((need.gpus, need.gpu_class.as_deref()), (0, None));
    assert_eq!(entries(&need), [("accelerator.vendor", "nvidia")]);
    let model_only = AcceleratorRequest {
        model: Some("nvidia-h100".into()),
        ..AcceleratorRequest::default()
    };
    let need = convert(0, None, Some(model_only)).unwrap();
    assert_eq!((need.gpus, need.gpu_class.as_deref()), (0, None));
    let count_only = AcceleratorRequest {
        count: Some(4),
        ..AcceleratorRequest::default()
    };
    let refused = convert(0, None, Some(count_only)).unwrap_err();
    assert_eq!(refused, RequestNeedError::CountWithoutModel);
    assert_eq!(
        refused.to_string(),
        "accelerator.model is required when accelerator.count is above zero"
    );
    let bad = AcceleratorRequest {
        vendor: Some("NVIDIA".into()),
        ..AcceleratorRequest::default()
    };
    let refused = convert(0, None, Some(bad)).unwrap_err();
    assert_eq!(
        refused,
        RequestNeedError::Field(AccelError::BadVendor("NVIDIA".into()))
    );
    assert_eq!(
        refused.to_string(),
        "accelerator: vendor must be 1 to 32 of a-z, 0-9 and '-', not \"NVIDIA\""
    );
    for bad in [
        AcceleratorRequest {
            model: Some("A100".into()),
            ..AcceleratorRequest::default()
        },
        AcceleratorRequest {
            interconnect: Some("nvlink".into()),
            ..AcceleratorRequest::default()
        },
        AcceleratorRequest {
            form_factor: Some("oam".into()),
            ..AcceleratorRequest::default()
        },
    ] {
        assert!(matches!(
            convert(0, None, Some(bad)),
            Err(RequestNeedError::Field(_))
        ));
    }
}

/// Asserts the conversion of a request that carries both: they agree when the object's
/// model resolves to the same family and variant as the class and its count equals
/// `gpus`, the pair then stays the booking and the object adds its other fields; another
/// model, another variant of the same family, a class that is not a model name, or
/// another count is refused naming `accelerator`. Catches two sources trusted
/// independently, so a body books one model and asks for another.
#[test]
fn a_pair_and_an_object_must_agree() {
    let object = |model: Option<&str>, count: Option<u32>| AcceleratorRequest {
        model: model.map(str::to_owned),
        count,
        memory_per_device_gib: Some(80),
        ..AcceleratorRequest::default()
    };
    let agreed = convert(2, Some("nvidia-a100-sxm4-80gb"), Some(object(None, None))).unwrap();
    assert_eq!(
        (agreed.gpus, agreed.gpu_class.as_deref()),
        (2, Some("nvidia-a100-sxm4-80gb"))
    );
    assert_eq!(
        entries(&agreed),
        [
            ("accelerator.model", "nvidia-a100-sxm4-80gb"),
            ("accelerator.count", "2"),
            ("accelerator.memory-gib", "80"),
        ]
    );
    let same = Some(object(Some("nvidia-a100-sxm4-80gb"), Some(2)));
    assert!(convert(2, Some("nvidia-a100-sxm4-80gb"), same).is_ok());
    for (model, class) in [
        ("nvidia-h100", "nvidia-a100"),
        ("nvidia-a100", "nvidia-a100-sxm4-80gb"),
        ("nvidia-a100-sxm4-40gb", "nvidia-a100-sxm4-80gb"),
        ("nvidia-a100", "A100_X"),
    ] {
        let refused = convert(1, Some(class), Some(object(Some(model), None))).unwrap_err();
        assert_eq!(
            refused,
            RequestNeedError::ModelDisagrees {
                accelerator: model.to_owned(),
                gpu_class: class.to_owned()
            }
        );
        assert!(refused.to_string().starts_with("accelerator.model "));
    }
    let refused = convert(2, Some("nvidia-a100"), Some(object(None, Some(4)))).unwrap_err();
    assert_eq!(
        refused,
        RequestNeedError::CountDisagrees {
            accelerator: 4,
            gpus: 2
        }
    );
    assert_eq!(
        refused.to_string(),
        "accelerator.count 4 differs from gpus 2"
    );
}
