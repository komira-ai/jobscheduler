//! The lease body's accelerator: the `resources.accelerator` object, its agreement with
//! `gpus` and `gpu_class`, the `accelerator.*` capabilities a body names, and what a body
//! without either still reads as.

use super::*;

/// The 400 reason of a body with these `resources` and `capabilities`.
fn refused(resources: &str, capabilities: &str) -> String {
    let text = format!(
        r#"{{"class":"c","resources":{resources},"capabilities":{capabilities},"max_wall_seconds":60}}"#
    );
    let err = parse::<AcquireBody>(text.as_bytes())
        .unwrap()
        .into_parts()
        .unwrap_err();
    assert_eq!(err.status, StatusCode::BAD_REQUEST, "{resources}");
    assert_eq!(err.code, "bad-request");
    err.reason.unwrap()
}

/// The request of a body with these `resources` and `capabilities`.
fn accepted(resources: &str, capabilities: &str) -> Request {
    let text = format!(
        r#"{{"class":"c","resources":{resources},"capabilities":{capabilities},"max_wall_seconds":60}}"#
    );
    let body = parse::<AcquireBody>(text.as_bytes()).unwrap();
    body.into_parts().unwrap().0
}

/// Asserts that an `accelerator` object that names another model than `gpu_class`, or
/// another count than `gpus`, is a 400 whose reason names `accelerator` and both
/// values, and that one that agrees (the same family, the same count) is taken.
/// Catches a lease booked as one model and matched as another.
#[test]
fn a_body_with_gpu_class_and_a_conflicting_accelerator_is_a_400_naming_accelerator() {
    let pair = r#""gpus":4,"gpu_class":"nvidia-a100""#;
    assert_eq!(
        refused(
            &format!(r#"{{{pair},"accelerator":{{"model":"nvidia-h100"}}}}"#),
            "{}"
        ),
        "accelerator.model \"nvidia-h100\" names another model than gpu_class \"nvidia-a100\""
    );
    assert_eq!(
        refused(&format!(r#"{{{pair},"accelerator":{{"count":2}}}}"#), "{}"),
        "accelerator.count 2 differs from gpus 4"
    );
    let agreed = accepted(
        &format!(r#"{{{pair},"accelerator":{{"model":"nvidia-a100","count":4}}}}"#),
        "{}",
    );
    assert_eq!(agreed.resources.gpus(), 4);
    assert_eq!(agreed.resources.gpu_class(), Some("nvidia-a100"));
}

/// Asserts that a body with no accelerator object and no GPUs parses to the request it
/// parsed to before the accelerator existed (the resources and capabilities as written,
/// no `accelerator.*` capability added), that the previous release's reader takes the
/// same body, that a body with the object reads every field of it, and that a body that
/// books GPUs as `gpus` and `gpu_class` keeps its booking. Catches a capability added to
/// a request that asked for no accelerator, and an object field read and dropped.
#[test]
fn a_body_without_accelerator_or_gpu_class_is_byte_identical_to_today() {
    let request = accepted(
        r#"{"cpu_millis":1000,"memory_bytes":2}"#,
        r#"{"os":"linux","isa":"x86-64-v3"}"#,
    );
    let expected = Request {
        class: ClassId::new("c").unwrap(),
        resources: Resources::new(1000, 2, 0, None).unwrap(),
        capabilities: [("os", "linux"), ("isa", "x86-64-v3")]
            .into_iter()
            .map(|(k, v)| (k.to_owned(), v.to_owned()))
            .collect(),
        max_wall: Duration::from_secs(60),
        labels: BTreeMap::new(),
        validation_run: None,
        probe_host: None,
    };
    assert_eq!(request, expected);
    let text =
        r#"{"class":"c","resources":{"cpu_millis":1000,"memory_bytes":2},"max_wall_seconds":60}"#;
    assert!(parse::<PreviousAcquireBody>(text.as_bytes()).is_ok());
    let body = parse::<AcquireBody>(text.as_bytes()).unwrap();
    assert!(body.resources.accelerator.is_none());
    let full = r#"{"class":"c","resources":{"gpus":4,"gpu_class":"nvidia-a100","accelerator":{"vendor":"nvidia","model":"nvidia-a100","count":4,"memory_per_device_gib":80,"interconnect":"nvlink-domain>=8","form_factor":"sxm"}},"max_wall_seconds":60}"#;
    let read = parse::<AcquireBody>(full.as_bytes()).unwrap();
    let shown = format!("{:?}", read.resources);
    assert!(shown.contains("count: Some(4)"), "{shown}");
    assert!(shown.contains("memory_per_device_gib: Some(80)"), "{shown}");
    let booked = accepted(r#"{"gpus":2,"gpu_class":"nvidia-a100-sxm4-80gb"}"#, "{}");
    assert_eq!(booked.resources.gpus(), 2);
    assert_eq!(booked.resources.gpu_class(), Some("nvidia-a100-sxm4-80gb"));
}

/// Asserts that the accelerator object is read in every form the JSON reader takes: as
/// a map with its fields in any order, as a sequence of its six fields in order (missing
/// trailing ones defaulted), `null` in the resources as no object, and refused as a
/// value of another type, with a field named twice, or with a field it does not know.
/// Catches an object read in one form and dropped or misread in another.
#[test]
fn the_accelerator_object_is_read_in_every_form_json_takes() {
    let read = |text: &str| serde_json::from_str::<AcceleratorBody>(text);
    let map = read(r#"{"form_factor":"sxm","count":2,"model":"m"}"#).unwrap();
    assert_eq!((map.count, map.model.as_deref()), (Some(2), Some("m")));
    assert_eq!(map.form_factor.as_deref(), Some("sxm"));
    let seq = read(r#"["nvidia","m",4,80,"nvlink-domain>=8","pcie"]"#).unwrap();
    let fields = (seq.vendor.as_deref(), seq.count, seq.memory_per_device_gib);
    assert_eq!(fields, (Some("nvidia"), Some(4), Some(80)));
    let fields = (seq.interconnect.as_deref(), seq.form_factor.as_deref());
    assert_eq!(fields, (Some("nvlink-domain>=8"), Some("pcie")));
    let short = read(r#"["nvidia"]"#).unwrap();
    assert_eq!(
        (short.vendor.as_deref(), short.model, short.count),
        (Some("nvidia"), None, None)
    );
    for refused in [
        "5",
        r#""x""#,
        r#"{"count":1,"count":2}"#,
        r#"{"cores":1}"#,
        "[1]",
    ] {
        assert!(read(refused).is_err(), "{refused}");
    }
    let none: ResourcesBody = serde_json::from_str(r#"{"accelerator":null}"#).unwrap();
    assert!(none.accelerator.is_none());
    assert!(serde_json::from_str::<ResourcesBody>(r#"{"accelerator":7}"#).is_err());
}

/// Asserts that a count with no model, in the object or as an `accelerator.count`
/// capability, is the 400 that names `accelerator.model`; that capabilities asking for
/// devices the resources do not book are a 400 naming the count; and that a vendor alone
/// (which books no GPU) and a count key beside the pair that books it are taken. Catches
/// GPUs booked with no class, which a previous release would read back as a whole host,
/// and devices required but not booked, which a sharing class would book twice.
#[test]
fn a_count_only_accelerator_is_a_400_naming_accelerator_model() {
    let said = "accelerator.model is required when accelerator.count is above zero";
    assert_eq!(refused(r#"{"accelerator":{"count":4}}"#, "{}"), said);
    assert_eq!(refused("{}", r#"{"accelerator.count":"4"}"#), said);
    let vendor = accepted(r#"{"accelerator":{"vendor":"nvidia"}}"#, "{}");
    assert_eq!(vendor.resources.gpus(), 0);
    assert_eq!(vendor.capabilities.len(), 1);
    assert_eq!(vendor.capabilities["accelerator.vendor"], "nvidia");
    let unbooked = r#"{"accelerator.model":"nvidia-a100","accelerator.count":"4"}"#;
    assert_eq!(
        refused("{}", unbooked),
        "accelerator.count 4 is more than the 0 gpus the request books"
    );
    let pair = r#"{"gpus":1,"gpu_class":"nvidia-a100"}"#;
    let counted = accepted(pair, r#"{"accelerator.count":"1"}"#);
    assert_eq!(counted.resources.gpus(), 1);
}

/// Asserts that an accelerator object alone books its count of its model and states
/// every field as an `accelerator.*` capability (memory in GiB), that `gpus` and
/// `gpu_class` alone add no capability (the request is stored as the previous release
/// stores it), and that an unknown key in the object is the parser's 400. Catches a field
/// read and dropped, or a key added to today's body that would make a repeated acquire
/// differ from the lease the previous release stored.
#[test]
fn an_accelerator_object_books_its_devices_and_states_its_need() {
    let object = r#"{"accelerator":{"vendor":"nvidia","model":"nvidia-a100","count":4,"memory_per_device_gib":80,"interconnect":"nvlink-domain>=8","form_factor":"sxm"}}"#;
    let request = accepted(object, "{}");
    assert_eq!(request.resources.gpus(), 4);
    assert_eq!(request.resources.gpu_class(), Some("nvidia-a100"));
    let stated: Vec<(&str, &str)> = request
        .capabilities
        .iter()
        .map(|(k, v)| (k.as_str(), v.as_str()))
        .collect();
    assert_eq!(
        stated,
        [
            ("accelerator.count", "4"),
            ("accelerator.form-factor", "sxm"),
            ("accelerator.interconnect", "nvlink-domain>=8"),
            ("accelerator.memory-gib", "80"),
            ("accelerator.model", "nvidia-a100"),
            ("accelerator.vendor", "nvidia"),
        ]
    );
    let pair = accepted(r#"{"gpus":2,"gpu_class":"nvidia-a100-sxm4-80gb"}"#, "{}");
    assert!(pair.capabilities.is_empty());
    let text = r#"{"class":"c","resources":{"accelerator":{"cores":4}},"max_wall_seconds":60}"#;
    let unknown = parse::<AcquireBody>(text.as_bytes()).unwrap_err();
    assert_eq!(unknown.status, StatusCode::BAD_REQUEST);
}

/// Asserts that a client capability naming another model or count than `gpus` and
/// `gpu_class` book is a 400 naming it, that one that agrees (the same family, the same
/// count) is taken, that one naming another value than the object states is a 400
/// naming the key, and that an `accelerator.*` capability that is not valid is a 400
/// naming `accelerator`. Catches a lease whose booking and requirement disagree.
#[test]
fn an_accelerator_capability_must_agree_with_the_resources_and_be_valid() {
    let pair = r#"{"gpus":1,"gpu_class":"nvidia-a100"}"#;
    assert_eq!(
        refused(pair, r#"{"accelerator.model":"nvidia-h100"}"#),
        "accelerator.model \"nvidia-h100\" names another model than gpu_class \"nvidia-a100\""
    );
    assert_eq!(
        refused(pair, r#"{"accelerator.count":"2"}"#),
        "accelerator.count 2 differs from gpus 1"
    );
    let same = accepted(pair, r#"{"accelerator.model":"nvidia-a100"}"#);
    assert_eq!(same.capabilities["accelerator.model"], "nvidia-a100");
    let object = r#"{"accelerator":{"model":"nvidia-a100","count":1}}"#;
    assert_eq!(
        refused(object, r#"{"accelerator.model":"nvidia-h100"}"#),
        "capability accelerator.model differs from the accelerator the resources state"
    );
    let reason = refused("{}", r#"{"accelerator.form-factor":"tpu"}"#);
    assert!(reason.starts_with("accelerator: "), "{reason}");
}
