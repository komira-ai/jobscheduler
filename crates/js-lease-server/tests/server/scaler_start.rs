//! A server with a launching pool, started from a configuration file, over an account in
//! memory: a pool in `plan` launches nothing, a `live` pool launches once and not again
//! after a restart, and a missing records file is a first start only when the operator says
//! so. Time is the test's clock and the ticks are stepped; nothing sleeps and no network is
//! touched.

use std::os::unix::fs::PermissionsExt;

use js_lease_server::RecordsFile;
use js_provider_lambdalabs::Call;
use js_scaler::{Blocked, Phase, Records};
use js_state::Store;

use crate::scaler_support::{
    RECORDS_KEY, account, kept_records, open_store, put_in_store, records_path, start,
    start_refused, submit, text, text_in, text_with,
};
use crate::support::{Harness, Kept, Scratch};

/// The phase of the first record the scaler of `h` holds.
fn phase(h: &Harness) -> Phase {
    h.running.scaler().unwrap().state().records.instances[0].phase
}

/// Asserts that a pool in `plan` mode launches nothing however often it ticks: the account
/// sees reads and no launch, the scaler says it is in plan, no record is written, and a
/// queued job gets no machine. It catches a launch from a pool
/// whose `launch` is not `live`.
#[tokio::test]
async fn a_plan_pool_launches_nothing() {
    let scratch = Scratch::new("scaler_plan");
    let account = account(&scratch);
    let text = text(&scratch, "plan", "initialise");
    let h = start(scratch, &text).await;
    for _ in 0..3 {
        h.ticks.step().await;
    }
    let queued = submit(&h, "j1").await;
    assert_eq!(queued.status, 201, "{}", queued.body);
    h.ticks.step().await;

    assert_eq!(account.launched(), 0);
    assert_eq!(account.count(Call::Launch), 0);
    assert!(account.count(Call::InstanceTypes) >= 1);
    assert!(!records_path(&h.scratch).exists());
    let state = h.running.scaler().unwrap().state();
    assert_eq!(state.blocked["burst"], Blocked::Plan);
    assert!(state.records.instances.is_empty());
}

/// Asserts that a pool in `plan` mode starts without the pin of the agent a live pool
/// needs, and ticks without launching: the scaler holds it in plan. It catches a plan pool
/// that needs what only a launch downloads.
#[tokio::test]
async fn a_plan_pool_starts_without_an_agent_pin() {
    let scratch = Scratch::new("scaler_plan_no_agent");
    account(&scratch);
    let source = text_with(&scratch, "plan", "initialise", &[("agent", "")]);
    let h = start(scratch, &source).await;
    h.ticks.step().await;
    let state = h.running.scaler().unwrap().state();
    assert_eq!(state.blocked["burst"], Blocked::Plan);
}

/// Asserts that a `live` pool launches once for a queued job and not again after a restart:
/// one tick after the job is queued the machine is launched
/// with its user data (which names no token file), the records are in a file that only the owner can use (or, with a store, in
/// the store and in no file), and the account holds one machine; the ticks that follow add
/// none. The server is stopped and another started over the same files or store, the job
/// is still queued, and the account still holds one machine, the scaler one record, and no
/// second machine is launched for it. It runs with the records in their file and in the state
/// store. It catches a launch that is forgotten at a restart, a store mode that writes the
/// records file.
#[tokio::test]
async fn a_live_pool_launches_once_and_not_again_after_a_restart() {
    for kept in Kept::BOTH {
        launches_once_in(kept).await;
    }
}

async fn launches_once_in(kept: Kept) {
    let scratch = Scratch::new(&kept.named("scaler_live"));
    let account = account(&scratch);
    let first = text_in(&scratch, "live", "initialise", kept);
    let h = start(scratch, &first).await;
    assert_eq!(Kept::of(&h), kept);
    let queued = submit(&h, "j1").await;
    assert_eq!(queued.status, 201, "{}", queued.body);
    h.ticks.step().await;
    h.ticks.step().await;
    assert_eq!(account.launched(), 1);
    let path = records_path(&h.scratch);
    match kept {
        Kept::Files => assert_eq!(
            std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600
        ),
        Kept::Store => assert!(!path.exists(), "a store mode wrote the records file"),
    }
    let written = Records::decode(&kept_records(&h).unwrap()).unwrap();
    assert_eq!(written.instances.len(), 1);
    assert_eq!(written.instances[0].pool, "burst");
    // The acquire id carries 128 bits from the operating system's source; the
    // deterministic source of the tests ends in a long run of zero digits.
    let acquire = written.instances[0].acquire.as_ref().unwrap().to_string();
    assert!(acquire.starts_with("burst"), "{acquire}");
    assert!(!acquire.ends_with("0000000000000000"), "{acquire}");
    let script = account.launches()[0].user_data().unwrap().to_owned();
    let name = written.instances[0].name.clone();
    assert!(
        script.contains(&format!("--host-id \"{name}\"")),
        "{script}"
    );
    assert!(script.contains("--pool \"burst\""), "{script}");
    assert!(!script.contains("--token-file"), "{script}");

    h.ticks.step().await;
    h.ticks.step().await;
    assert_eq!(account.launched(), 1);
    assert_eq!(phase(&h), Phase::Booting);

    let Harness {
        running, scratch, ..
    } = h;
    running.shutdown().await.unwrap();
    let again = text_in(&scratch, "live", "resume", kept);
    let h = start(scratch, &again).await;
    h.ticks.step().await;
    for _ in 0..3 {
        h.ticks.step().await;
    }
    assert_eq!(account.launched(), 1);
    assert_eq!(account.count(Call::Launch), 1);
    let state = h.running.scaler().unwrap().state();
    assert_eq!(state.records.instances.len(), 1);
    assert_eq!(state.records.instances[0].name, name);
    assert_eq!(state.records.instances[0].phase, Phase::Booting);
}

/// Asserts that a missing records file is a first start only when the operator says so
/// (`--boot initialise`). A live pool started with `initialise` and no file starts, and
/// writes no file until it has something to record; started again with `resume` and still
/// no file it is refused, and the refusal names the file and the remedy; a pool in `plan`
/// mode starts with `resume` and no file; and a live pool with the file present starts with
/// `resume`. It runs with the records in their file and in the state store, where the
/// refusal names the store's setting and the records key, and the records are put in the
/// store. It catches a deleted or moved records file (or a missing key) read as an empty
/// memory, which would launch again for machines in flight and reset the day's spend.
#[tokio::test]
async fn a_live_pool_with_no_records_file_starts_only_at_a_first_start() {
    for kept in Kept::BOTH {
        starts_without_records_in(kept).await;
    }
}

async fn starts_without_records_in(kept: Kept) {
    let scratch = Scratch::new(&kept.named("scaler_missing_records"));
    account(&scratch);
    let first = text_in(&scratch, "live", "initialise", kept);
    let h = start(scratch, &first).await;
    h.ticks.step().await;
    let Harness {
        running, scratch, ..
    } = h;
    running.shutdown().await.unwrap();
    let path = records_path(&scratch);
    assert!(!path.exists());
    if kept == Kept::Store {
        assert!(open_store(&scratch).get(RECORDS_KEY).unwrap().is_none());
    }

    let resumed = text_in(&scratch, "live", "resume", kept);
    let said = start_refused(&scratch, &resumed).await;
    match kept {
        Kept::Files => assert!(said.contains(&path.display().to_string()), "{said}"),
        Kept::Store => assert!(
            said.starts_with("server.store: acquires/scaler/records is missing from the store"),
            "{said}"
        ),
    }
    assert!(said.contains("is missing"), "{said}");
    assert!(said.contains("--boot initialise"), "{said}");

    let planned = text_in(&scratch, "plan", "resume", kept);
    let h = start(scratch, &planned).await;
    let Harness {
        running, scratch, ..
    } = h;
    running.shutdown().await.unwrap();

    match kept {
        Kept::Files => RecordsFile::new(path).save(&Records::default()).unwrap(),
        Kept::Store => {
            let text = Records::default().encode().unwrap();
            put_in_store(&scratch, RECORDS_KEY, &text);
        }
    }
    let present = text_in(&scratch, "live", "resume", kept);
    let h = start(scratch, &present).await;
    assert!(
        h.running
            .scaler()
            .unwrap()
            .state()
            .records
            .instances
            .is_empty()
    );
}
