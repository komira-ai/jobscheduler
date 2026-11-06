//! The `ec2` kind: a provider table checked offline and turned into a provider. Every
//! provider here is built over the in-memory account; the one test that builds the
//! cloud's client never calls it.

use std::num::NonZeroUsize;
use std::sync::Arc;

use js_elastic::{
    AcquireId, AcquireRequest, Acquired, BootPayload, CapacityProvider, HostName, Offer, Purchase,
    Reason, RefusalCode, SchedulerId,
};
use js_provider_ec2::kind::{self, Built, Context, Refused};
use js_provider_ec2::{
    ArchName, FakeEc2, ImageId, InstanceType, InstanceTypeInfo, ManualClock, SCHEDULER_TAG,
};

const IMAGE_X86: &str = "ami-0123456789abcdef0";
const IMAGE_ARM: &str = "ami-0fedcba987654321";

/// A table with the region, an image for each architecture, and `rest`.
fn table(rest: &str) -> toml::Table {
    format!(
        "region = \"region-a\"\nimages = {{ x86_64 = \"{IMAGE_X86}\", arm64 = \"{IMAGE_ARM}\" }}\n{rest}"
    )
    .parse()
    .unwrap()
}

fn context(live: bool) -> Context {
    Context {
        scheduler: SchedulerId::new("sched-one").unwrap(),
        max_machines: NonZeroUsize::new(4).unwrap(),
        live,
        clock: Arc::new(ManualClock::new()),
    }
}

/// An account that offers `m7.large` (x86_64) and `c7g.large` (arm64) in one zone, on
/// demand and on spot. Neither has a GPU.
fn account() -> Arc<FakeEc2> {
    let fake = Arc::new(FakeEc2::new());
    for (name, arch) in [("m7.large", "x86_64"), ("c7g.large", "arm64")] {
        fake.add_instance_type(InstanceTypeInfo {
            name: InstanceType(name.to_owned()),
            vcpus: 2,
            memory_mib: 8192,
            architectures: vec![ArchName(arch.to_owned())],
            gpus: Vec::new(),
            instance_storage_gb: None,
            bare_metal: false,
        });
        fake.add_zone_offering(name, "zone-a1")
            .add_on_demand_price(name, "0.10")
            .add_spot_price(name, "zone-a1", "0.04");
    }
    fake
}

fn built(table: &toml::Table, live: bool, fake: &Arc<FakeEc2>) -> Built {
    match kind::build_over(table, &context(live), fake.clone()) {
        Ok(built) => built,
        Err(refused) => panic!("{}: {}", refused.field, refused.reason),
    }
}

fn refusal(table: &toml::Table) -> (String, String) {
    let refused: Refused = match kind::build_over(table, &context(false), account()) {
        Ok(_) => panic!("the table should be refused"),
        Err(refused) => refused,
    };
    (refused.field, refused.reason)
}

/// The offers the provider lists, read on a runtime.
fn offers(provider: &impl CapacityProvider) -> Vec<Offer> {
    let runtime = tokio::runtime::Builder::new_multi_thread().build().unwrap();
    runtime.block_on(provider.catalog()).unwrap()
}

/// The request to launch `offer` for the acquire `acquire`, named for `scheduler`.
fn request(scheduler: &str, acquire: &str, offer: &Offer) -> AcquireRequest {
    let scheduler = SchedulerId::new(scheduler).unwrap();
    let acquire = AcquireId::new(acquire).unwrap();
    AcquireRequest {
        name: HostName::derive(&scheduler, &acquire),
        acquire,
        pool: "pool".to_owned(),
        lifetime: None,
        offer: offer.clone(),
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

fn offer_of(offers: &[Offer], arch: &str) -> Offer {
    offers
        .iter()
        .find(|offer| offer.arch == arch)
        .unwrap()
        .clone()
}

/// Asserts that the `images` table is checked when the table is parsed, each fault named
/// by its field: missing, not a table, empty, an architecture that is not `arm64`,
/// `aarch64` or `x86_64`, a value that is not a string, a value that is not an image id
/// (wrong prefix, upper case, too short, too long), and `arm64` given together with its
/// alias `aarch64`. It catches an image that is only found wrong when a machine is
/// launched.
#[test]
fn images_config_is_validated_at_parse() {
    let region = "region = \"region-a\"\n";
    let images = |rest: &str| -> toml::Table { format!("{region}{rest}").parse().unwrap() };
    let digits = "ami-0123456789abcdef0";
    let cases = [
        ("", "images", "required"),
        (
            "images = \"ami-01234567\"",
            "images",
            "must be a table from architecture to image id",
        ),
        (
            "images = {}",
            "images",
            "must name an image for arm64 or x86_64",
        ),
        (
            "images = { sparc = \"ami-01234567\" }",
            "images.sparc",
            "unknown architecture; known: arm64, aarch64, x86_64",
        ),
        (
            "images = { x86_64 = 7 }",
            "images.x86_64",
            "must be a string",
        ),
        (
            "images = { arm64 = \"img-01234567\" }",
            "images.arm64",
            "must be ami- followed by 8 to 17 lower-case hexadecimal digits",
        ),
        (
            "images = { x86_64 = \"ami-0123ABCD\" }",
            "images.x86_64",
            "must be ami- followed by 8 to 17 lower-case hexadecimal digits",
        ),
        (
            "images = { x86_64 = \"ami-0123456\" }",
            "images.x86_64",
            "must be ami- followed by 8 to 17 lower-case hexadecimal digits",
        ),
        (
            "images = { aarch64 = \"ami-012345678901234567\" }",
            "images.aarch64",
            "must be ami- followed by 8 to 17 lower-case hexadecimal digits",
        ),
    ];
    for (text, field, reason) in cases {
        let (got_field, got_reason) = refusal(&images(text));
        assert_eq!((got_field.as_str(), got_reason.as_str()), (field, reason));
    }
    let both = images(&format!(
        "images = {{ arm64 = \"{digits}\", aarch64 = \"{digits}\" }}"
    ));
    assert_eq!(
        refusal(&both),
        (
            "images.aarch64".to_owned(),
            "arm64 is given twice".to_owned()
        )
    );
}

/// Asserts that each architecture is launched from its own image, that `aarch64` is read
/// as `arm64`, and that an architecture with no image is refused as not allowed with no
/// instance launched. It catches an image used for the wrong architecture, an alias that
/// is dropped, and a missing image that launches anyway.
#[test]
fn each_architecture_gets_its_image_and_aarch64_is_arm64() {
    let fake = account();
    let aliased: toml::Table = format!(
        "region = \"region-a\"\nimages = {{ aarch64 = \"{IMAGE_ARM}\", x86_64 = \"{IMAGE_X86}\" }}\n\
         allowed_instance_types = [\"m7.large\", \"c7g.large\"]"
    )
    .parse()
    .unwrap();
    let provider = built(&aliased, true, &fake).provider;
    let offers = offers(&provider);
    let x86 = offer_of(&offers, "x86_64");
    let arm = offer_of(&offers, "arm64");
    for (acquire, offer) in [("acquire-1", &x86), ("acquire-2", &arm)] {
        let answer = provider.acquire(&request("sched-one", acquire, offer));
        assert!(matches!(answer, Acquired::Accepted { .. }), "{answer:?}");
    }
    let images: Vec<ImageId> = fake
        .run_requests()
        .into_iter()
        .map(|run| run.image)
        .collect();
    assert_eq!(
        images,
        vec![
            ImageId::new(IMAGE_X86).unwrap(),
            ImageId::new(IMAGE_ARM).unwrap()
        ]
    );
    let only_x86: toml::Table =
        format!("region = \"region-a\"\nimages = {{ x86_64 = \"{IMAGE_X86}\" }}")
            .parse()
            .unwrap();
    let bare = account();
    let provider = built(&only_x86, true, &bare).provider;
    let answer = provider.acquire(&request("sched-one", "acquire-3", &arm));
    assert_eq!(why(answer), (RefusalCode::Shape, Reason::NotAllowed));
    assert_eq!(bare.launched(), 0);
}

/// Asserts that a pool built `live` launches over the given account, on demand by default
/// since the table has no `purchase`, and tags the instance with its scheduler, and that
/// a pool built not live refuses the same acquire as disabled and launches nothing. It
/// catches a pool that ignores `launch`, a default that lists spot, and a provider built
/// for another scheduler.
#[test]
fn launch_decides_the_second_lock_and_the_default_is_on_demand() {
    let fake = account();
    let table = table("allowed_instance_types = [\"m7.large\", \"c7g.large\"]");
    let live = built(&table, true, &fake).provider;
    let listed = offers(&live);
    assert!(!listed.is_empty());
    assert!(
        listed
            .iter()
            .all(|offer| offer.purchase == Some(Purchase::OnDemand)),
        "{listed:?}"
    );
    let offer = offer_of(&listed, "x86_64");
    let answer = live.acquire(&request("sched-one", "acquire-1", &offer));
    assert!(matches!(answer, Acquired::Accepted { .. }), "{answer:?}");
    let runs = fake.run_requests();
    assert_eq!(runs.len(), 1);
    assert!(
        runs[0]
            .tags
            .iter()
            .any(|tag| tag.key == SCHEDULER_TAG && tag.value == "sched-one")
    );
    let foreign = live.acquire(&request("sched-two", "acquire-2", &offer));
    assert_eq!(why(foreign), (RefusalCode::Shape, Reason::Name));
    let idle = account();
    let plan = built(&table, false, &idle).provider;
    let answer = plan.acquire(&request("sched-one", "acquire-1", &offer));
    assert_eq!(why(answer), (RefusalCode::Denied, Reason::Disabled));
    assert_eq!(idle.launched(), 0);
}

/// Asserts that `allowed_instance_types` limits both the catalog (it lists a non-GPU type
/// only when the list names it) and the launch (an offer of a type outside the list is
/// refused as not allowed, with nothing launched). It catches a list that reaches only one
/// of the two.
#[test]
fn allowed_instance_types_limit_the_catalog_and_the_launch() {
    let fake = account();
    let none = built(&table(""), true, &fake).provider;
    assert_eq!(offers(&none), Vec::new());
    let only = built(
        &table("allowed_instance_types = [\"m7.large\"]"),
        true,
        &fake,
    )
    .provider;
    let listed = offers(&only);
    assert!(!listed.is_empty());
    assert!(listed.iter().all(|offer| offer.arch == "x86_64"));
    let offer = offer_of(&listed, "x86_64");
    let mut outside = offer.clone();
    outside.id = offer.id.replace("m7.large", "c7g.large");
    assert_ne!(outside.id, offer.id);
    let answer = only.acquire(&request("sched-one", "acquire-1", &outside));
    assert_eq!(why(answer), (RefusalCode::Shape, Reason::NotAllowed));
    assert_eq!(fake.launched(), 0);
    let answer = only.acquire(&request("sched-one", "acquire-1", &offer));
    assert!(matches!(answer, Acquired::Accepted { .. }), "{answer:?}");
    assert_eq!(fake.launched(), 1);
}

/// Asserts that `purchase` and `capacity_block_hours` reach the catalog: a spot-only
/// table lists only spot offers, and a table with `capacity-block` asks the account for
/// blocks of the length it gives. It catches a purchase list that is ignored and a block
/// length that is lost.
#[test]
fn purchase_and_block_hours_reach_the_catalog() {
    let fake = account();
    let spot = table("purchase = [\"spot\"]\nallowed_instance_types = [\"m7.large\"]");
    let provider = built(&spot, true, &fake).provider;
    let listed = offers(&provider);
    assert!(!listed.is_empty());
    assert!(
        listed
            .iter()
            .all(|offer| offer.purchase == Some(Purchase::Spot)),
        "{listed:?}"
    );
    assert_eq!(fake.block_hours_asked(), Vec::<u32>::new());
    let blocks = table(
        "purchase = [\"on-demand\", \"capacity-block\"]\ncapacity_block_hours = 24\nallowed_instance_types = [\"m7.large\"]",
    );
    let provider = built(&blocks, true, &fake).provider;
    let _ = offers(&provider);
    assert_eq!(fake.block_hours_asked(), vec![24]);
}

/// Asserts that every fault of the table is refused naming its field: an unknown field
/// (a key and a secret key are unknown, there is no credential key; the refusal lists the
/// known fields), a missing or mistyped `region`, a region that is not a region code, a
/// `purchase` that is not an array of strings or is empty, unknown or has a duplicate,
/// `allowed_instance_types` that is not an array of strings, `capacity_block_hours` that
/// is not a whole number of hours, and `capacity-block` without hours. It catches a typo
/// read as a default, a refusal that names nothing, and a wrong type that is ignored.
#[test]
fn each_fault_of_the_table_names_its_field() {
    let known = "region, purchase, images, allowed_instance_types, capacity_block_hours";
    for key in ["api_key", "secret_access_key"] {
        let (field, reason) = refusal(&table(&format!("{key} = \"x\"")));
        assert_eq!(field, key);
        assert_eq!(reason, format!("unknown field; known: {known}"));
    }
    let no_region: toml::Table = format!("images = {{ x86_64 = \"{IMAGE_X86}\" }}")
        .parse()
        .unwrap();
    assert_eq!(
        refusal(&no_region),
        ("region".to_owned(), "required".to_owned())
    );
    let cases = [
        ("region = 3", "region", "must be a string"),
        (
            "purchase = \"spot\"",
            "purchase",
            "must be an array of strings",
        ),
        (
            "purchase = [\"spot\", 3]",
            "purchase",
            "must be an array of strings",
        ),
        ("purchase = []", "purchase", "lists no purchase option"),
        (
            "purchase = [\"free\"]",
            "purchase",
            "\"free\" is not one of on-demand, spot, capacity-block",
        ),
        (
            "purchase = [\"spot\", \"spot\"]",
            "purchase",
            "\"spot\" is listed twice",
        ),
        (
            "allowed_instance_types = \"m7.large\"",
            "allowed_instance_types",
            "must be an array of strings",
        ),
        (
            "allowed_instance_types = [7]",
            "allowed_instance_types",
            "must be an array of strings",
        ),
        (
            "capacity_block_hours = \"day\"",
            "capacity_block_hours",
            "must be a whole number of hours",
        ),
        (
            "capacity_block_hours = -1",
            "capacity_block_hours",
            "must be a whole number of hours",
        ),
        (
            "purchase = [\"capacity-block\"]",
            "capacity_block_hours",
            "capacity-block needs a length of at least one hour",
        ),
        (
            "purchase = [\"capacity-block\"]\ncapacity_block_hours = 0",
            "capacity_block_hours",
            "capacity-block needs a length of at least one hour",
        ),
    ];
    for (rest, field, reason) in cases {
        let text = match rest.starts_with("region") {
            true => format!("{rest}\nimages = {{ x86_64 = \"{IMAGE_X86}\" }}"),
            false => {
                format!("region = \"region-a\"\nimages = {{ x86_64 = \"{IMAGE_X86}\" }}\n{rest}")
            }
        };
        let parsed: toml::Table = text.parse().unwrap();
        let (got_field, got_reason) = refusal(&parsed);
        assert_eq!(
            (got_field.as_str(), got_reason.as_str()),
            (field, reason),
            "{rest}"
        );
    }
    let bad_region: toml::Table =
        format!("region = \"Region A\"\nimages = {{ x86_64 = \"{IMAGE_X86}\" }}")
            .parse()
            .unwrap();
    let (field, reason) = refusal(&bad_region);
    assert_eq!(field, "region");
    assert_eq!(
        reason,
        "not a region code (lower-case letters, digits and '-', 1 to 32)"
    );
}

/// Asserts that `build` checks the same rules as `build_over` before it makes any client
/// (a faulty table is refused with the same field) and that a good table builds the
/// provider over the cloud's client, which is made and never called. It catches a `build`
/// that skips the checks and a client that cannot be made from a good table.
#[test]
fn build_checks_the_table_then_makes_the_cloud_client() {
    let faulty = table("api_key = \"x\"");
    let refused = match kind::build(&faulty, &context(false)) {
        Ok(_) => panic!("the table should be refused"),
        Err(refused) => refused,
    };
    assert_eq!(refused.field, "api_key");
    let good = table("");
    let live = kind::build(&good, &context(true)).is_ok();
    let plan = kind::build(&good, &context(false)).is_ok();
    assert!(live && plan);
    assert_eq!(kind::API_TIMEOUT.as_secs(), 30);
}
