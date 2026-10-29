//! The `lambdalabs` kind: a provider table checked offline and turned into a provider.
//! The key file is read through an injected reader and the client is built and never
//! used, so no test touches a network.

use std::num::NonZeroUsize;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use js_elastic::{
    AcquireId, AcquireRequest, Acquired, BootPayload, ElasticProvider, HostName, Offer, PriceUnit,
    Reason, RefusalCode, SchedulerId,
};
use js_provider_lambdalabs::ManualClock;
use js_provider_lambdalabs::kind::{self, Built, Context, DEFAULT_API_URL, Refused, SecretFiles};
use js_types::ExecMode;

const KEY_FILE: &str = "/run/credentials/js/lambda-key";
const SECRET: &str = "PLANTED-LAMBDA-KEY-VALUE";

/// A reader of secret files over a fixed list.
struct Files(Vec<(PathBuf, Result<Vec<u8>, String>)>);

impl SecretFiles for Files {
    fn read(&self, path: &Path) -> Result<Vec<u8>, String> {
        let found = self.0.iter().find(|(each, _)| each == path);
        match found {
            Some((_, read)) => read.clone(),
            None => Err(format!("{}: no such file", path.display())),
        }
    }
}

fn files(key: &[u8]) -> Files {
    Files(vec![(PathBuf::from(KEY_FILE), Ok(key.to_vec()))])
}

fn context<'a>(files: &'a Files, live: bool) -> Context<'a> {
    Context {
        scheduler: SchedulerId::new("sched-one").unwrap(),
        max_machines: NonZeroUsize::new(4).unwrap(),
        live,
        clock: Arc::new(ManualClock::new()),
        files,
    }
}

fn table(text: &str) -> toml::Table {
    text.parse().unwrap()
}

/// A table with the key file and `rest` after it.
fn with_key(rest: &str) -> toml::Table {
    table(&format!("api_key_file = {KEY_FILE:?}\n{rest}"))
}

const TYPES: &str = "types = [{ name = \"gpu_1x_a10\", gpu = \"nvidia-a10\", gpus = 1 }]";

fn refusal(table: &toml::Table) -> (String, String) {
    let files = files(SECRET.as_bytes());
    let refused: Refused = match kind::build(table, &context(&files, false)) {
        Ok(_) => panic!("the table should be refused"),
        Err(refused) => refused,
    };
    (refused.field, refused.reason)
}

fn built(table: &toml::Table, live: bool) -> Built {
    let files = files(format!("{SECRET}\n").as_bytes());
    match kind::build(table, &context(&files, live)) {
        Ok(built) => built,
        Err(refused) => panic!("{}: {}", refused.field, refused.reason),
    }
}

/// The request for a machine of the scheduler `scheduler`, for `offer` in `zone`.
fn request(scheduler: &str, offer: &str, zone: &str) -> AcquireRequest {
    let scheduler = SchedulerId::new(scheduler).unwrap();
    let acquire = AcquireId::new("acquire-1").unwrap();
    AcquireRequest {
        name: HostName::derive(&scheduler, &acquire),
        acquire,
        pool: "gpu".to_owned(),
        offer: Offer {
            id: offer.to_owned(),
            zone: Some(zone.to_owned()),
            cpu_millis: 1000,
            memory_bytes: 1 << 30,
            disk_bytes: 1 << 30,
            gpus: 1,
            gpu_class: None,
            os: "linux".to_owned(),
            arch: "x86_64".to_owned(),
            exec: ExecMode::Container,
            price_cents_per_hour: Some(75),
            price_unit: PriceUnit::UsCents,
            available: true,
        },
        labels: Vec::new(),
        payload: BootPayload::new("payload".to_owned()),
    }
}

fn why(answer: Acquired) -> (RefusalCode, Reason) {
    match answer {
        Acquired::Refused(refusal) => (refusal.code, refusal.reason),
        other => panic!("expected a refusal, got {other:?}"),
    }
}

/// Asserts that the table of the design's first example builds, and that the facts the
/// service reads offline are the pool's: the fewest GPUs over the types and their one
/// class, with the types in either order. It catches a floor taken from the first row
/// instead of the least, and a class lost on the way.
#[test]
fn the_facts_of_a_pool_are_its_fewest_gpus_and_its_class() {
    let one = built(&with_key(TYPES), false);
    assert_eq!((one.gpus, one.gpu_class.as_str()), (1, "nvidia-a10"));
    let rows = "types = [{ name = \"gpu_2x_a10\", gpu = \"nvidia-a10\", gpus = 2 }, \
                { name = \"gpu_1x_a10\", gpu = \"nvidia-a10\", gpus = 1 }, \
                { name = \"gpu_4x_a10\", gpu = \"nvidia-a10\", gpus = 4 }]";
    let several = built(&with_key(rows), false);
    assert_eq!(several.gpus, 1);
    let first_is_least = "types = [{ name = \"gpu_1x_a10\", gpu = \"nvidia-a10\", gpus = 1 }, \
                { name = \"gpu_2x_a10\", gpu = \"nvidia-a10\", gpus = 2 }]";
    assert_eq!(built(&with_key(first_is_least), false).gpus, 1);
}

/// Asserts that a table with every optional key builds: `regions`, `ssh_key` and an
/// `api_url` on the loopback interface, which the client accepts over plain http. It
/// catches an optional key the kind reads and then refuses.
#[test]
fn every_optional_key_builds() {
    let text = format!(
        "{TYPES}\nregions = [\"us-west-1\", \"us-east-1\"]\nssh_key = \"my-key\"\napi_url = \"http://127.0.0.1:9\""
    );
    let built = built(&with_key(&text), false);
    assert_eq!(built.gpus, 1);
}

/// Asserts the second lock: a pool in `plan` builds a provider that is not enabled, so an
/// acquire is refused `Denied` for being disabled whatever the scaler does, and a pool in
/// `live` builds one that is, so the same acquire gets as far as the offer check and is
/// refused `Shape` for an offer outside `types`. The check that follows, on the name, tells
/// the scheduler apart: a machine named for another scheduler is refused for its name, and
/// one named for this pool's scheduler is not. A region outside `regions` is refused as
/// not allowed. It catches a plan pool that can launch, a live pool that cannot, a
/// provider built for another scheduler, and `types` and `regions` that never reach the
/// provider. Each refusal is before any call to the API.
#[test]
fn launch_decides_the_second_lock_and_the_rest_reaches_the_provider() {
    let text = format!("{TYPES}\nregions = [\"us-west-1\"]");
    let table = with_key(&text);
    let plan = built(&table, false).provider;
    let outside = request("sched-one", "gpu_9x_unknown", "us-west-1");
    assert_eq!(
        why(plan.acquire(&outside)),
        (RefusalCode::Denied, Reason::Disabled)
    );
    let live = built(&table, true).provider;
    assert_eq!(
        why(live.acquire(&outside)),
        (RefusalCode::Shape, Reason::NotAllowed)
    );
    let foreign = request("sched-two", "gpu_1x_a10", "us-west-1");
    assert_eq!(
        why(live.acquire(&foreign)),
        (RefusalCode::Shape, Reason::Name)
    );
    let elsewhere = request("sched-one", "gpu_1x_a10", "us-east-1");
    assert_eq!(
        why(live.acquire(&elsewhere)),
        (RefusalCode::Shape, Reason::NotAllowed)
    );
}

/// Asserts that every way the provider table can be wrong is refused naming its field:
/// an unknown field (with the known ones), a missing key file or `types`, a field of the
/// wrong type, and an `api_url` the client cannot use. It catches a typo read as a
/// default, a refusal that names nothing, and a wrong type that is ignored.
#[test]
fn each_fault_of_the_table_names_its_field() {
    let known = "api_key_file, regions, types, ssh_key, api_url";
    let (field, reason) = refusal(&with_key(&format!("{TYPES}\napi_keyfile = \"x\"")));
    assert_eq!(field, "api_keyfile");
    assert_eq!(reason, format!("unknown field; known: {known}"));
    let (field, reason) = refusal(&table(TYPES));
    assert_eq!(
        (field.as_str(), reason.as_str()),
        ("api_key_file", "required")
    );
    let (field, reason) = refusal(&table(&format!("api_key_file = 3\n{TYPES}")));
    assert_eq!(
        (field.as_str(), reason.as_str()),
        ("api_key_file", "must be a string")
    );
    let (field, reason) = refusal(&with_key(""));
    assert_eq!((field.as_str(), reason.as_str()), ("types", "required"));
    let (field, reason) = refusal(&with_key("types = \"gpu_1x_a10\""));
    assert_eq!(
        (field.as_str(), reason.as_str()),
        ("types", "must be an array of tables")
    );
    let (field, reason) = refusal(&with_key("types = []"));
    assert_eq!(
        (field.as_str(), reason.as_str()),
        ("types", "must list at least one type")
    );
    for (rest, field, reason) in [
        (
            format!("{TYPES}\nssh_key = 3"),
            "ssh_key",
            "must be a string",
        ),
        (
            format!("{TYPES}\nssh_key = \"two words\""),
            "ssh_key",
            "the ssh key name must be 1 to 64 printable ASCII characters",
        ),
        (
            format!("{TYPES}\nregions = \"us-west-1\""),
            "regions",
            "must be an array of strings",
        ),
        (
            format!("{TYPES}\nregions = [3]"),
            "regions",
            "must be an array of strings",
        ),
        (
            format!("{TYPES}\nregions = [\"US West\"]"),
            "regions",
            "a region is 1 to 64 lower case letters, digits or '-'",
        ),
        (
            format!("{TYPES}\nregions = []"),
            "regions",
            "a region is 1 to 64 lower case letters, digits or '-'",
        ),
        (
            format!("{TYPES}\napi_url = 3"),
            "api_url",
            "must be a string",
        ),
    ] {
        let (got_field, got_reason) = refusal(&with_key(&rest));
        assert_eq!((got_field.as_str(), got_reason.as_str()), (field, reason));
    }
    let (field, reason) = refusal(&with_key(&format!("{TYPES}\napi_url = \"ftp://x\"")));
    assert_eq!(field, "api_url");
    assert!(reason.contains("base URL"), "{reason}");
}

/// Asserts the rules of `types`, each naming the row and the field: a row that is not a
/// table, an unknown key in a row, a missing or mistyped `name`, `gpu` and `gpus`, a type
/// name that is empty, too long, in upper case or with a dash, a type listed twice, a class
/// not in the agent's spelling, a second class in one pool, and `gpus` of zero, negative,
/// too large or not a number. It catches each rule being skipped, and a refusal that
/// points at the wrong row.
#[test]
fn each_rule_of_the_types_names_its_row_and_field() {
    let row = |name: &str, gpu: &str, gpus: &str| {
        format!("{{ name = {name:?}, gpu = {gpu:?}, gpus = {gpus} }}")
    };
    let good = row("gpu_1x_a10", "nvidia-a10", "1");
    let long = "a".repeat(65);
    let cases: Vec<(String, &str, &str)> = vec![
        (
            "types = [3]".to_owned(),
            "types[1]",
            "must be a table with name, gpu and gpus",
        ),
        (
            format!(
                "types = [{good}, {{ name = \"x\", gpu = \"nvidia-a10\", gpus = 1, size = 2 }}]"
            ),
            "types[2].size",
            "unknown field; known: name, gpu, gpus",
        ),
        (
            "types = [{ gpu = \"nvidia-a10\", gpus = 1 }]".to_owned(),
            "types[1].name",
            "required",
        ),
        (
            "types = [{ name = 3, gpu = \"nvidia-a10\", gpus = 1 }]".to_owned(),
            "types[1].name",
            "must be a string",
        ),
        (
            format!("types = [{}]", row("", "nvidia-a10", "1")),
            "types[1].name",
            "must be 1 to 64 lower case letters, digits or '_'",
        ),
        (
            format!("types = [{}]", row(&long, "nvidia-a10", "1")),
            "types[1].name",
            "must be 1 to 64 lower case letters, digits or '_'",
        ),
        (
            format!("types = [{}]", row("GPU_1x", "nvidia-a10", "1")),
            "types[1].name",
            "must be 1 to 64 lower case letters, digits or '_'",
        ),
        (
            format!("types = [{}]", row("gpu-1x", "nvidia-a10", "1")),
            "types[1].name",
            "must be 1 to 64 lower case letters, digits or '_'",
        ),
        (
            format!("types = [{good}, {good}]"),
            "types[2].name",
            "\"gpu_1x_a10\" is listed twice",
        ),
        (
            "types = [{ name = \"x\", gpus = 1 }]".to_owned(),
            "types[1].gpu",
            "required",
        ),
        (
            "types = [{ name = \"x\", gpu = 3, gpus = 1 }]".to_owned(),
            "types[1].gpu",
            "must be a string",
        ),
        (
            format!("types = [{}]", row("x", "NVIDIA A10", "1")),
            "types[1].gpu",
            "must be lower case letters and digits joined by single '-', as the agent reports a GPU model",
        ),
        (
            format!("types = [{}]", row("x", "", "1")),
            "types[1].gpu",
            "is empty",
        ),
        (
            format!("types = [{good}, {}]", row("y", "nvidia-a100", "1")),
            "types[2].gpu",
            "a pool has one gpu class; \"nvidia-a10\" is the class of types[1], not \"nvidia-a100\"",
        ),
        (
            "types = [{ name = \"x\", gpu = \"nvidia-a10\" }]".to_owned(),
            "types[1].gpus",
            "required",
        ),
        (
            format!("types = [{}]", row("x", "nvidia-a10", "0")),
            "types[1].gpus",
            "must be a whole number from 1",
        ),
        (
            format!("types = [{}]", row("x", "nvidia-a10", "-2")),
            "types[1].gpus",
            "must be a whole number from 1",
        ),
        (
            format!("types = [{}]", row("x", "nvidia-a10", "5000000000")),
            "types[1].gpus",
            "must be a whole number from 1",
        ),
        (
            format!("types = [{}]", row("x", "nvidia-a10", "\"two\"")),
            "types[1].gpus",
            "must be a whole number from 1",
        ),
    ];
    for (text, field, reason) in cases {
        let (got_field, got_reason) = refusal(&with_key(&text));
        assert_eq!(
            (got_field.as_str(), got_reason.as_str()),
            (field, reason),
            "{text}"
        );
    }
}

/// Asserts that the key file is read through the injected reader and nothing else: a file
/// the reader refuses is refused naming `api_key_file` with the reader's own reason, a
/// file with no key, with two words or too long is refused for its content, and the key's
/// text is in no refusal. It catches a kind that opens the file itself (the reader exists
/// so that one rule decides which files are private), and a refusal that quotes the key.
#[test]
fn the_key_file_is_read_through_the_reader_and_never_quoted() {
    let table = with_key(TYPES);
    let unreadable = Files(vec![(
        PathBuf::from(KEY_FILE),
        Err("mode 0644, must be 0600 or 0400".to_owned()),
    )]);
    let refused = kind::build(&table, &context(&unreadable, false))
        .err()
        .unwrap();
    assert_eq!(refused.field, "api_key_file");
    assert_eq!(refused.reason, "mode 0644, must be 0600 or 0400");
    let missing = Files(Vec::new());
    let refused = kind::build(&table, &context(&missing, false))
        .err()
        .unwrap();
    assert_eq!(refused.field, "api_key_file");
    assert!(
        refused.reason.contains("no such file"),
        "{}",
        refused.reason
    );
    let bad: [&[u8]; 3] = [b"  \n", b"two words", &[b'k'; 5000]];
    for content in bad {
        let given = files(content);
        let refused = kind::build(&table, &context(&given, false)).err().unwrap();
        assert_eq!(refused.field, "api_key_file");
        assert!(!refused.reason.contains("two words"), "{}", refused.reason);
        assert!(refused.reason.contains(KEY_FILE), "{}", refused.reason);
    }
    let valid = files(SECRET.as_bytes());
    let fine = kind::build(&table, &context(&valid, false)).is_ok();
    assert!(fine);
}

/// Asserts the address the kind uses when the table names none is Lambda's, and that the
/// constant is a base URL the client accepts, so a table with no `api_url` builds.
#[test]
fn the_default_address_is_a_base_url() {
    assert_eq!(DEFAULT_API_URL, "https://cloud.lambdalabs.com");
    assert_eq!(built(&with_key(TYPES), false).gpus, 1);
}
