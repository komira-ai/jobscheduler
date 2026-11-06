//! The `gce` kind: a provider table checked offline and turned into a provider. Every
//! provider here is built over the in-memory project; the one test that builds the cloud's
//! client never calls it.

use std::num::NonZeroUsize;
use std::sync::Arc;

use js_elastic::{
    AcquireRequest, Acquired, CapacityProvider, Offer, Purchase, Reason, RefusalCode, SchedulerId,
};
use js_provider_gce::kind::{self, Built, Context};
use js_provider_gce::{
    ManualClock, PAYLOAD_LIMIT, PROVIDER_ID, ProvisioningModel, TerminationAction,
};

use crate::rig::{
    ARM_IMAGE, ARM_TYPE, Account, CPU_TYPE, NETWORK, PROJECT, SCHEDULER, SUBNETWORK, X86_IMAGE,
    ZONE, ask, ask_as, image, label, machine, network, offer, set,
};

/// The known fields of a table, as a refusal of an unknown one lists them.
const KNOWN: &str = "project, zones, purchase, network, subnetwork, images, allowed_machine_types";

/// Why a network value is refused.
const NETWORK_FORM: &str =
    "must be global/networks/<name> or projects/<project>/global/networks/<name>";

/// Why a subnetwork value is refused.
const SUBNETWORK_FORM: &str = "must be regions/<region>/subnetworks/<name> or projects/<project>/regions/<region>/subnetworks/<name>";

/// Why an image value is refused.
const IMAGE_FORM: &str = "must be projects/<project>/global/images/<image> or projects/<project>/global/images/family/<family>";

/// A good table: the rig's project, zone, network, subnetwork and an image for each
/// architecture.
fn base() -> toml::Table {
    format!(
        "project = \"{PROJECT}\"\nzones = [\"{ZONE}\"]\nnetwork = \"{NETWORK}\"\n\
         subnetwork = \"{SUBNETWORK}\"\n\
         images = {{ x86_64 = \"{X86_IMAGE}\", arm64 = \"{ARM_IMAGE}\" }}\n"
    )
    .parse()
    .unwrap()
}

/// The good table with the keys `edit` writes replaced or added.
fn table(edit: &str) -> toml::Table {
    let mut table = base();
    let edit: toml::Table = edit.parse().unwrap();
    for (key, value) in edit {
        table.insert(key, value);
    }
    table
}

/// The good table without the key `key`.
fn without(key: &str) -> toml::Table {
    let mut table = base();
    table.remove(key);
    table
}

/// The good table, allowing the rig's two CPU machine types, with `edit` applied.
fn allowing(edit: &str) -> toml::Table {
    let mut table = table("allowed_machine_types = [\"q9-standard-2\", \"q9a-standard-2\"]");
    let edit: toml::Table = edit.parse().unwrap();
    for (key, value) in edit {
        table.insert(key, value);
    }
    table
}

fn context(live: bool) -> Context {
    Context {
        scheduler: SchedulerId::new(SCHEDULER).unwrap(),
        max_machines: NonZeroUsize::new(4).unwrap(),
        live,
        clock: Arc::new(ManualClock::new()),
    }
}

fn built(table: &toml::Table, live: bool, account: &Arc<Account>) -> Built {
    match kind::build_over(table, &context(live), account.clone()) {
        Ok(built) => built,
        Err(refused) => panic!("{}: {}", refused.field, refused.reason),
    }
}

/// The field and reason the table is refused with.
fn refusal(table: &toml::Table) -> (String, String) {
    match kind::build_over(table, &context(false), Arc::new(Account::new())) {
        Ok(_) => panic!("the table should be refused: {table:?}"),
        Err(refused) => (refused.field, refused.reason),
    }
}

/// The offers the provider lists, read on a runtime.
fn offers(provider: &impl CapacityProvider) -> Vec<Offer> {
    let runtime = tokio::runtime::Builder::new_multi_thread().build().unwrap();
    runtime.block_on(provider.catalog()).unwrap()
}

fn why(answer: Acquired) -> (RefusalCode, Reason) {
    match answer {
        Acquired::Refused(refusal) => (refusal.code, refusal.reason),
        other => panic!("expected a refusal, got {other:?}"),
    }
}

/// The first of `offers` of the architecture `arch` and the purchase `purchase`.
fn offer_of(offers: &[Offer], arch: &str, purchase: &Purchase) -> Offer {
    offers
        .iter()
        .find(|offer| (offer.arch == arch) & (offer.purchase.as_ref() == Some(purchase)))
        .unwrap()
        .clone()
}

/// The request to launch `offer` for the acquire `acquire`, as the rig's scheduler.
fn request(acquire: &str, offer: &Offer) -> AcquireRequest {
    AcquireRequest {
        offer: offer.clone(),
        ..ask(acquire)
    }
}

/// Asserts that every fault of the table is refused at parse time naming its field and why,
/// with the exact text: an unknown field, which includes anything that looks like a
/// credential and which lists the known fields; each required field missing; each field of
/// the wrong type; a project that is not a project id; zones that are empty, not a zone name
/// or listed twice; purchase options that are empty, unknown or listed twice;
/// `allowed_machine_types` that is not an array of strings; a network, a subnetwork and an
/// image in every malformed form; a subnetwork of a region that holds none of the zones;
/// and an `images` table that is missing, not a table, empty, names an architecture that
/// does not exist, or gives `arm64` twice as `aarch64` too. Every accepted form is built.
/// It catches a fault that is found only when a machine is launched, and a refusal that
/// names nothing.
#[test]
fn config_is_validated_at_parse() {
    let unknown = format!("unknown field; known: {KNOWN}");
    for key in [
        "api_key",
        "private_key",
        "credentials_file",
        "key_file",
        "client_secret",
    ] {
        let (field, reason) = refusal(&table(&format!("{key} = \"x\"")));
        assert_eq!((field.as_str(), reason.as_str()), (key, unknown.as_str()));
    }
    for key in ["project", "zones", "network", "subnetwork", "images"] {
        assert_eq!(
            refusal(&without(key)),
            (key.to_owned(), "required".to_owned())
        );
    }
    let project = "not a project id (6 to 30 lower-case letters, digits and '-', starting with a letter and not ending with '-')";
    let array = "must be an array of strings";
    let mut cases: Vec<(String, &str, String)> = vec![
        (
            "project = 3".to_owned(),
            "project",
            "must be a string".to_owned(),
        ),
        (
            "project = \"Bad\"".to_owned(),
            "project",
            project.to_owned(),
        ),
        (
            "zones = \"region-one-a\"".to_owned(),
            "zones",
            array.to_owned(),
        ),
        ("zones = [1]".to_owned(), "zones", array.to_owned()),
        ("zones = []".to_owned(), "zones", "lists no zone".to_owned()),
        (
            "zones = [\"Bad Zone\"]".to_owned(),
            "zones",
            "\"Bad Zone\" is not a zone name (a region name, '-' and a zone letter)".to_owned(),
        ),
        (
            "zones = [\"region-one-a\", \"region-one-a\"]".to_owned(),
            "zones",
            "\"region-one-a\" is listed twice".to_owned(),
        ),
        (
            "purchase = \"spot\"".to_owned(),
            "purchase",
            array.to_owned(),
        ),
        (
            "purchase = [\"spot\", 3]".to_owned(),
            "purchase",
            array.to_owned(),
        ),
        (
            "purchase = []".to_owned(),
            "purchase",
            "lists no purchase option".to_owned(),
        ),
        (
            "purchase = [\"free\"]".to_owned(),
            "purchase",
            "\"free\" is not one of on-demand, spot".to_owned(),
        ),
        (
            "purchase = [\"spot\", \"spot\"]".to_owned(),
            "purchase",
            "\"spot\" is listed twice".to_owned(),
        ),
        (
            "allowed_machine_types = \"q9-standard-2\"".to_owned(),
            "allowed_machine_types",
            array.to_owned(),
        ),
        (
            "allowed_machine_types = [7]".to_owned(),
            "allowed_machine_types",
            array.to_owned(),
        ),
        (
            "network = 3".to_owned(),
            "network",
            "must be a string".to_owned(),
        ),
        (
            "subnetwork = 3".to_owned(),
            "subnetwork",
            "must be a string".to_owned(),
        ),
        (
            "zones = [\"region-one-a\", \"region-two-a\"]".to_owned(),
            "subnetwork",
            "is in the region \"region-one\", and the zone \"region-two-a\" is not".to_owned(),
        ),
        (
            "zones = [\"region-two-a\"]".to_owned(),
            "subnetwork",
            "is in the region \"region-one\", and the zone \"region-two-a\" is not".to_owned(),
        ),
        (
            "images = \"x86\"".to_owned(),
            "images",
            "must be a table from architecture to image".to_owned(),
        ),
        (
            "images = {}".to_owned(),
            "images",
            "must name an image for arm64 or x86_64".to_owned(),
        ),
        (
            format!("images = {{ sparc = \"{X86_IMAGE}\" }}"),
            "images.sparc",
            "unknown architecture; known: arm64, aarch64, x86_64".to_owned(),
        ),
        (
            "images = { x86_64 = 7 }".to_owned(),
            "images.x86_64",
            "must be a string".to_owned(),
        ),
        (
            format!("images = {{ arm64 = \"{ARM_IMAGE}\", aarch64 = \"{ARM_IMAGE}\" }}"),
            "images.aarch64",
            "arm64 is given twice".to_owned(),
        ),
    ];
    for bad in [
        "default",
        "global/networks/",
        "global/networks/Default",
        "global/network/default",
        "projects/sample-project/networks/default",
        "projects/short/global/networks/default",
        "projects/sample-project",
        "projects/sample-project/global/networks/",
    ] {
        cases.push((
            format!("network = \"{bad}\""),
            "network",
            NETWORK_FORM.to_owned(),
        ));
    }
    for bad in [
        "default",
        "regions/region-one/default",
        "regions/region-one/subnetworks/",
        "regions//subnetworks/default",
        "regions/Region/subnetworks/default",
        "global/networks/default",
        "projects/short/regions/region-one/subnetworks/default",
    ] {
        cases.push((
            format!("subnetwork = \"{bad}\""),
            "subnetwork",
            SUBNETWORK_FORM.to_owned(),
        ));
    }
    for (key, field) in [
        ("x86_64", "images.x86_64"),
        ("arm64", "images.arm64"),
        ("aarch64", "images.aarch64"),
    ] {
        for bad in [
            "js-agent",
            "projects/image-project/global/images/",
            "projects/image-project/global/images/family/",
            "projects/Bad/global/images/js-agent",
            "projects/image-project/images/js-agent",
            "projects/image-project/global/images/Js-Agent",
        ] {
            cases.push((
                format!("images = {{ {key} = \"{bad}\" }}"),
                field,
                IMAGE_FORM.to_owned(),
            ));
        }
    }
    for (edit, field, reason) in cases {
        let (got_field, got_reason) = refusal(&table(&edit));
        assert_eq!((got_field.as_str(), got_reason), (field, reason), "{edit}");
    }

    let account = Arc::new(Account::new());
    for good in [
        "network = \"projects/host-project/global/networks/shared\"",
        "subnetwork = \"projects/host-project/regions/region-one/subnetworks/shared\"",
        "zones = [\"region-one-a\", \"region-one-b\"]",
        "purchase = [\"spot\", \"on-demand\"]",
        "allowed_machine_types = []",
        "images = { x86_64 = \"projects/image-project/global/images/family/js-agent-x86\" }",
        "images = { arm64 = \"projects/image-project/global/images/js-agent-arm\" }",
        "images = { aarch64 = \"projects/image-project/global/images/js-agent-arm\" }",
    ] {
        built(&table(good), false, &account);
    }
}

/// Asserts that each architecture is launched from its own image, that `aarch64` is read as
/// `arm64`, and that an architecture with no image is refused as not allowed with no
/// instance inserted. It catches an image used for the wrong architecture, an alias that
/// is dropped, and a missing image that launches anyway.
#[test]
fn each_architecture_gets_its_image_and_aarch64_is_arm64() {
    let account = Arc::new(Account::new());
    let aliased = allowing(&format!(
        "images = {{ aarch64 = \"{ARM_IMAGE}\", x86_64 = \"{X86_IMAGE}\" }}"
    ));
    let provider = built(&aliased, true, &account).provider;
    let arm = offer(&format!("{ARM_TYPE}/on-demand"), "arm64");
    let x86 = offer(&format!("{CPU_TYPE}/on-demand"), "x86_64");
    for (acquire, wanted) in [("a1", &arm), ("a2", &x86)] {
        let answer = provider.acquire(&request(acquire, wanted));
        assert!(matches!(answer, Acquired::Accepted { .. }), "{answer:?}");
    }
    let images: Vec<_> = account
        .inserts()
        .into_iter()
        .map(|insert| insert.image)
        .collect();
    assert_eq!(images, [image(ARM_IMAGE), image(X86_IMAGE)]);

    let only_x86 = allowing(&format!("images = {{ x86_64 = \"{X86_IMAGE}\" }}"));
    let bare = Arc::new(Account::new());
    let provider = built(&only_x86, true, &bare).provider;
    let answer = provider.acquire(&request("a3", &arm));
    assert_eq!(why(answer), (RefusalCode::Shape, Reason::NotAllowed));
    assert_eq!(bare.inserted(), 0);
}

/// Asserts that a pool built `live` inserts over the given project, on demand by default
/// since the table has no `purchase`, into the table's project and network, labelled with
/// its scheduler; that it refuses the name of another scheduler; and that a pool built not
/// live refuses the same acquire as disabled and inserts nothing. It catches a pool that
/// ignores `launch`, a default that lists Spot, and a provider built for another
/// scheduler.
#[test]
fn launch_decides_the_second_lock_and_the_default_is_on_demand() {
    let account = Arc::new(Account::new());
    let table = allowing("");
    let live = built(&table, true, &account).provider;
    let listed = offers(&live);
    assert!(!listed.is_empty());
    assert!(
        listed
            .iter()
            .all(|listed| listed.purchase == Some(Purchase::OnDemand)),
        "{listed:?}"
    );
    let wanted = offer_of(&listed, "x86_64", &Purchase::OnDemand);
    let answer = live.acquire(&request("a1", &wanted));
    assert!(matches!(answer, Acquired::Accepted { .. }), "{answer:?}");
    let inserts = account.inserts();
    assert_eq!(inserts.len(), 1);
    assert_eq!(inserts[0].project.as_str(), PROJECT);
    assert_eq!(inserts[0].network, network());
    assert_eq!(
        inserts[0].scheduling.provisioning_model,
        ProvisioningModel::Standard
    );
    assert!(
        inserts[0]
            .labels
            .contains(&label("js-scheduler", SCHEDULER))
    );
    let foreign = AcquireRequest {
        offer: wanted.clone(),
        ..ask_as("sched-two", "a2")
    };
    assert_eq!(
        why(live.acquire(&foreign)),
        (RefusalCode::Shape, Reason::Name)
    );
    let idle = Arc::new(Account::new());
    let plan = built(&table, false, &idle).provider;
    let answer = plan.acquire(&request("a1", &wanted));
    assert_eq!(why(answer), (RefusalCode::Denied, Reason::Disabled));
    assert_eq!(idle.inserted(), 0);
}

/// Asserts that `purchase` reaches the catalog and the launch: a Spot-only table lists only
/// Spot offers and inserts a Spot instance that is deleted when the cloud ends it, and a
/// table that lists both lists both. It catches a purchase list that is ignored.
#[test]
fn purchase_reaches_the_catalog_and_the_launch() {
    let account = Arc::new(Account::new());
    let spot = allowing("purchase = [\"spot\"]");
    let provider = built(&spot, true, &account).provider;
    let listed = offers(&provider);
    assert!(!listed.is_empty());
    assert!(
        listed
            .iter()
            .all(|listed| listed.purchase == Some(Purchase::Spot)),
        "{listed:?}"
    );
    let wanted = offer_of(&listed, "x86_64", &Purchase::Spot);
    let answer = provider.acquire(&request("a1", &wanted));
    assert!(matches!(answer, Acquired::Accepted { .. }), "{answer:?}");
    let inserts = account.inserts();
    assert_eq!(
        inserts[0].scheduling.provisioning_model,
        ProvisioningModel::Spot
    );
    assert_eq!(
        inserts[0].scheduling.termination_action,
        Some(TerminationAction::Delete)
    );

    let both = allowing("purchase = [\"spot\", \"on-demand\"]");
    let listed = offers(&built(&both, true, &account).provider);
    for purchase in [Purchase::Spot, Purchase::OnDemand] {
        assert!(
            listed
                .iter()
                .any(|listed| listed.purchase.as_ref() == Some(&purchase)),
            "{purchase:?}"
        );
    }
}

/// Asserts that `allowed_machine_types` limits both the catalog (it lists a CPU type only
/// when the list names it; without a list only the offers with an accelerator are listed)
/// and the launch (an offer of a type outside the list is refused as not allowed, with
/// nothing inserted). It catches a list that reaches only one of the two.
#[test]
fn allowed_machine_types_limit_the_catalog_and_the_launch() {
    let account = Arc::new(Account::new());
    let none = built(&table(""), true, &account).provider;
    let listed = offers(&none);
    assert!(!listed.is_empty());
    assert!(listed.iter().all(|listed| listed.accelerator.is_some()));

    let only = built(
        &table("allowed_machine_types = [\"q9-standard-2\"]"),
        true,
        &account,
    )
    .provider;
    let listed = offers(&only);
    assert!(!listed.is_empty());
    assert!(listed.iter().all(|listed| listed.arch == "x86_64"));
    let wanted = offer_of(&listed, "x86_64", &Purchase::OnDemand);
    let mut outside = wanted.clone();
    outside.id = wanted.id.replace(CPU_TYPE, ARM_TYPE);
    assert_ne!(outside.id, wanted.id);
    let answer = only.acquire(&request("a1", &outside));
    assert_eq!(why(answer), (RefusalCode::Shape, Reason::NotAllowed));
    assert_eq!(account.inserted(), 0);
    let answer = only.acquire(&request("a1", &wanted));
    assert!(matches!(answer, Acquired::Accepted { .. }), "{answer:?}");
    assert_eq!(account.inserted(), 1);
}

/// Asserts what the provider says it is and carries: its id is the kind's name, it can
/// launch, it limits the payload to 256 KiB, adds no boot step to any offer, has no
/// unresolved acquire, and states Linux on `x86_64` and `arm64` machines that can be had
/// whole and host ports; and that once a catalog has carried an accelerator the translation
/// table does not name, the capabilities name its model too. It catches capabilities that
/// leave out an offer the catalog has returned.
#[test]
fn the_provider_states_what_it_is_and_what_it_carries() {
    let account = Arc::new(Account::new());
    account.add_machine_type(machine("g2-standard-4", 4, &[("nvidia-l4", 1)], "X86_64"));
    let provider = built(
        &table("allowed_machine_types = [\"g2-standard-4\"]"),
        true,
        &account,
    )
    .provider;
    assert_eq!(provider.id(), PROVIDER_ID);
    assert!(provider.can_launch());
    assert_eq!(provider.payload_limit(), PAYLOAD_LIMIT);
    assert_eq!(PAYLOAD_LIMIT, 256 * 1024);
    assert!(provider.unresolved().is_empty());
    assert!(
        provider
            .host_prep(&offer("g2-standard-4/on-demand", "x86_64"))
            .is_empty()
    );
    let before = provider.capabilities();
    assert_eq!(before.os, set(&["linux"]));
    assert_eq!(before.arch, set(&["arm64", "x86_64"]));
    assert!(before.whole_host);
    assert!(before.host_ports);
    assert!(!before.accelerators.is_empty());

    let listed = offers(&provider);
    let models: Vec<_> = listed
        .iter()
        .filter_map(|listed| listed.accelerator.as_ref())
        .map(|accelerator| accelerator.model.clone())
        .collect();
    assert!(!models.is_empty());
    assert!(
        models
            .iter()
            .all(|model| !before.accelerators.contains(model))
    );
    let after = provider.capabilities();
    assert!(
        models
            .iter()
            .all(|model| after.accelerators.contains(model))
    );
    assert_eq!(after.arch, set(&["arm64", "x86_64"]));
}

/// Asserts that `build` checks the same rules as `build_over` before it makes any client (a
/// faulty table is refused with the same field) and that a good table builds the provider
/// over the cloud's client, which is made and never called. It reads the credential
/// settings of the environment the way a service does, as the EC2 kind's test does, and
/// the test environment names none. It catches a `build` that skips the checks and a client
/// that cannot be made from a good table.
#[test]
fn build_checks_the_table_then_makes_the_cloud_client() {
    let faulty = table("api_key = \"x\"");
    let refused = match kind::build(&faulty, &context(false)) {
        Ok(_) => panic!("the table should be refused"),
        Err(refused) => refused,
    };
    assert_eq!(refused.field, "api_key");
    let good = base();
    let live = kind::build(&good, &context(true)).is_ok();
    let plan = kind::build(&good, &context(false)).is_ok();
    assert!(live && plan);
    assert_eq!(kind::API_TIMEOUT.as_secs(), 30);
}
