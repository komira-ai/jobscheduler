//! The Lambda path when something goes wrong, through the real server and an account in
//! memory (see `lambda_e2e`): a host that never says hello, a refused launch, a launch whose
//! answer is lost, and a record that cannot be written. Time is the test's clock and the
//! ticks are stepped; nothing sleeps.

use js_provider_lambdalabs::{ApiError, Call, ErrorCode};
use js_scaler::{Blocked, Phase};

use crate::lambda_e2e_support::{DELETE_DEVICE, DEVICES, KEYS, Rig};
use crate::scaler_support::records_path;

/// Asserts a machine whose host never says hello (F5). It is Booting one minute before
/// `boot_timeout`, with nothing terminated; at `boot_timeout` the record goes Releasing,
/// the machine is terminated; the next tick reads it gone, removes the host and deletes its
/// tailnet device. The pool launches nothing for `boot_fail_cooldown` while the request
/// keeps waiting, and launches a machine of another name once the cooldown has run. It
/// catches a machine kept past its boot time, a device left behind, a
/// pool that relaunches at once, and one that never launches again.
#[tokio::test]
async fn a_host_that_never_says_hello_is_released_at_boot_timeout_and_the_pool_cools_off() {
    let rig = Rig::start("e2e_boot_timeout").await;
    rig.complete_view().await;
    let name = rig.launch().await;

    rig.advance(14 * 60);
    rig.wait().await;
    rig.tick().await;
    assert_eq!(rig.phase(), Phase::Booting);
    assert_eq!(rig.account.count(Call::Terminate), 0);
    assert_eq!(rig.account.launched(), 1);

    rig.advance(60);
    rig.wait().await;
    rig.tick().await;
    assert_eq!(rig.phase(), Phase::Releasing);
    assert_eq!(rig.account.count(Call::Terminate), 1);
    assert_eq!(rig.asked(DELETE_DEVICE), 0);

    rig.wait().await;
    rig.tick().await;
    assert_eq!(rig.phase(), Phase::Gone);
    assert_eq!(rig.asked(DELETE_DEVICE), 1);
    assert_eq!(rig.state().blocked["burst"], Blocked::BootFailed);
    assert_eq!(rig.account.launched(), 1);

    rig.advance(9 * 60);
    rig.wait().await;
    rig.tick().await;
    assert_eq!(rig.state().blocked["burst"], Blocked::BootFailed);
    assert_eq!(
        rig.account.launched(),
        1,
        "no launch while the pool cools off"
    );
    rig.advance(61);
    rig.wait().await;
    rig.tick().await;
    assert_eq!(rig.account.launched(), 2);
    let records = rig.state().records.instances;
    assert_eq!(records.len(), 2);
    assert_ne!(records[1].name, name);
    assert_eq!(rig.asked(KEYS), 2);
}

/// Asserts a refused acquire. The provider answers the launch with a 403 (denied): no
/// machine exists, and the Tailscale API is told to forget the host's device; the refused
/// record is dropped, in memory and in the file; the pool launches nothing while it cools
/// off, and launches once it has. It catches a refused launch kept as a machine that is
/// counted for ever, and a pool that asks again at once.
#[tokio::test]
async fn a_refused_launch_leaves_no_machine_and_a_cooling_pool() {
    let rig = Rig::start("e2e_refused").await;
    let denied = ApiError::Api {
        status: 403,
        code: ErrorCode::Forbidden,
        request_id: None,
    };
    rig.account.fail_next(Call::Launch, denied);
    rig.tick().await;
    rig.wait().await;
    rig.tick().await;
    assert_eq!(rig.account.launched(), 0);
    assert_eq!(rig.phase(), Phase::Launching);
    assert_eq!(rig.asked(KEYS), 1);

    rig.wait().await;
    rig.tick().await;
    assert!(rig.state().records.instances.is_empty());
    assert!(rig.file().instances.is_empty());
    assert_eq!(
        rig.asked(DEVICES),
        1,
        "the host's device is looked for and forgotten"
    );
    assert_eq!(rig.state().blocked["burst"], Blocked::Denied);
    assert!(rig.account.instances().is_empty());

    rig.advance(9 * 60);
    rig.wait().await;
    rig.tick().await;
    assert_eq!(
        rig.account.count(Call::Launch),
        1,
        "no second try while cooling off"
    );
    rig.advance(61);
    rig.wait().await;
    rig.tick().await;
    assert_eq!(rig.account.launched(), 1);
    assert_eq!(rig.account.count(Call::Launch), 2);
    assert_eq!(rig.asked(KEYS), 2);
}

/// Walks a launch whose answer is lost: the machine is launched, the answer is an
/// error, `ticks` more ticks pass, and the server stops. Another starts over the same
/// files.
async fn crash_after_a_lost_answer(name: &str, ticks: usize) -> (Rig, String) {
    let rig = Rig::start(name).await;
    let lost = ApiError::Transport("connection reset".to_owned());
    rig.account.lose_response(Call::Launch, lost);
    rig.tick().await;
    rig.wait().await;
    rig.tick().await;
    assert_eq!(rig.account.launched(), 1);
    assert_eq!(rig.phase(), Phase::Launching);
    for _ in 0..ticks {
        rig.tick().await;
    }
    let machine = rig.name();
    assert_eq!(rig.asked(KEYS), 1);
    (rig.restart().await, machine)
}

/// What must hold after the restart: the machine is the record's, Booting, found by the
/// list; the request is presented again and nothing more is launched or asked of
/// Tailscale.
async fn assert_adopted(rig: &Rig, name: &str) {
    rig.tick().await;
    assert_eq!(rig.phase(), Phase::Booting);
    for _ in 0..3 {
        rig.wait().await;
        rig.tick().await;
    }
    assert_eq!(rig.account.launched(), 1);
    assert_eq!(rig.account.count(Call::Launch), 1);
    assert_eq!(rig.asked(KEYS), 1, "no second key");
    let records = rig.state().records.instances;
    assert_eq!(records.len(), 1);
    assert_eq!(records[0].name, name);
}

/// Asserts a crash after the call and before its answer (F1). The server stops after the
/// tick that launched, with the record Launching on disk and the lost answer only in
/// memory. The new server adopts the record, its list shows the machine and the record is
/// Booting; no second machine is launched and no key is minted again. It catches a restart
/// that forgets a launch and buys a second machine.
#[tokio::test]
async fn a_crash_after_the_launch_does_not_launch_again() {
    let (rig, name) = crash_after_a_lost_answer("e2e_crash_launching", 0).await;
    assert_adopted(&rig, &name).await;
}

/// Asserts the same when the record had already become Unresolved (F2) before the crash:
/// the new server's list settles it, nothing is launched or asked of Tailscale again.
#[tokio::test]
async fn a_crash_while_a_launch_is_unresolved_does_not_launch_again() {
    let (rig, name) = crash_after_a_lost_answer("e2e_crash_unresolved", 1).await;
    assert_adopted(&rig, &name).await;
}

/// Asserts that the record is written before anything is done for it. The file of the
/// records cannot be written: the tick that would launch counts the failure and does
/// nothing else, so no Tailscale key is made and nothing is launched, however often it is
/// tried; once the file can be written the next tick launches. It catches a launch or a key
/// that does not wait for its record.
#[tokio::test]
async fn nothing_is_launched_while_the_record_cannot_be_written() {
    let rig = Rig::start("e2e_records_first").await;
    rig.tick().await;
    let path = records_path(&rig.h.scratch);
    std::fs::create_dir(&path).unwrap();
    for _ in 0..3 {
        rig.wait().await;
        rig.tick().await;
    }
    let scaler = rig.h.running.scaler().unwrap();
    assert_eq!(scaler.save_errors(), 3);
    assert_eq!(rig.account.launched(), 0);
    assert_eq!(rig.account.count(Call::Launch), 0);
    assert_eq!(rig.asked(KEYS), 0);

    std::fs::remove_dir(&path).unwrap();
    rig.wait().await;
    rig.tick().await;
    assert_eq!(rig.account.launched(), 1);
    assert_eq!(rig.phase(), Phase::Launching);
    assert_eq!(rig.file().instances.len(), 1);
    assert_eq!(rig.h.running.scaler().unwrap().save_errors(), 3);
}
