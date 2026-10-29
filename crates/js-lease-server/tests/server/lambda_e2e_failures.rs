//! The Lambda path when something goes wrong, through the real server and an account in
//! memory (see `lambda_e2e`): a host that never says hello, a welcome that is lost, a
//! refused launch, a launch whose answer is lost, a server that stops while a machine is
//! being released, and a record that cannot be written. Time is the test's clock and the
//! ticks are stepped; nothing sleeps.

use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;

use js_provider::StartAck;
use js_provider_lambdalabs::{ApiError, Call, ErrorCode};
use js_scaler::{Blocked, Phase};

use crate::agents_support::session;
use crate::lambda_e2e_support::{DELETE_DEVICE, DEVICES, KEYS, Rig, joins_held};
use crate::scaler_support::records_path;

/// Asserts a machine whose host never says hello (F5). It is Booting one minute before
/// `boot_timeout`, with nothing terminated; at `boot_timeout` the record goes Releasing,
/// the join token is revoked (a hello with it is the 401 of an unknown token) and the
/// machine is terminated; the next tick reads it gone, removes the host and deletes its
/// tailnet device. The pool launches nothing for `boot_fail_cooldown` while the request
/// keeps waiting, and launches a machine of another name once the cooldown has run. It
/// catches a machine kept past its boot time, a token left alive, a device left behind, a
/// pool that relaunches at once, and one that never launches again.
#[tokio::test]
async fn a_host_that_never_says_hello_is_released_at_boot_timeout_and_the_pool_cools_off() {
    let rig = Rig::start("e2e_boot_timeout").await;
    rig.complete_view(&[]).await;
    let (name, token) = rig.launch().await;

    rig.advance(14 * 60);
    rig.wait();
    rig.tick().await;
    assert_eq!(rig.phase(), Phase::Booting);
    assert_eq!(rig.account.count(Call::Terminate), 0);
    assert_eq!(rig.account.launched(), 1);

    rig.advance(60);
    rig.wait();
    rig.tick().await;
    assert_eq!(rig.phase(), Phase::Releasing);
    assert_eq!(rig.account.count(Call::Terminate), 1);
    assert_eq!(rig.hello(&token, &name, &name).await.status, 401);
    assert_eq!(rig.asked(DELETE_DEVICE), 0);

    rig.wait();
    rig.tick().await;
    assert_eq!(rig.phase(), Phase::Gone);
    assert_eq!(rig.asked(DELETE_DEVICE), 1);
    assert_eq!(rig.state().blocked["burst"], Blocked::BootFailed);
    assert_eq!(rig.account.launched(), 1);

    rig.advance(9 * 60);
    rig.wait();
    rig.tick().await;
    assert_eq!(rig.state().blocked["burst"], Blocked::BootFailed);
    assert_eq!(
        rig.account.launched(),
        1,
        "no launch while the pool cools off"
    );
    rig.advance(61);
    rig.wait();
    rig.tick().await;
    assert_eq!(rig.account.launched(), 2);
    let records = rig.state().records.instances;
    assert_eq!(records.len(), 2);
    assert_ne!(records[1].name, name);
    assert_eq!(rig.asked(KEYS), 2);
}

/// Walks a lost welcome (7.2): the machine is launched, its agent says hello and is
/// admitted with the token, and the welcome that carries the host credential never
/// reaches it. The token was retired by that hello, so a retry from the same host id and
/// one from another get the 401 of an unknown token, and the registry holds one host.
async fn lose_the_welcome(rig: &Rig) -> (String, String) {
    let (name, token) = rig.launch().await;
    rig.withdraw();
    let lost = rig.agent_says_hello(&token, &name).await;
    assert_eq!(lost.status, 200, "{}", lost.body);
    drop(lost);
    for host in [name.as_str(), "burst-second-host"] {
        let retry = rig.hello(&token, host, &name).await;
        assert_eq!(retry.status, 401, "{}", retry.body);
    }
    assert_eq!(rig.registry().hosts().len(), 1);
    (name, token)
}

/// Asserts what must hold however long a lost welcome takes to be given up on: the
/// machine is terminated once, after its token is revoked; the host is removed and its
/// device deleted; nothing else was launched and no second host ever registered.
async fn assert_released_once(rig: &Rig, name: &str, token: &str) {
    assert_eq!(rig.account.count(Call::Terminate), 1);
    assert_eq!(rig.hello(token, name, name).await.status, 401);
    rig.tick().await;
    assert_eq!(rig.phase(), Phase::Gone);
    assert!(rig.registry().hosts().is_empty());
    assert_eq!(rig.asked(DELETE_DEVICE), 1);
    assert_eq!(rig.account.launched(), 1);
    assert_eq!(rig.account.count(Call::Launch), 1);
    assert_eq!(rig.asked(KEYS), 1);
}

/// Asserts the design's rule for a lost welcome: the host holds no credential and the
/// token is dead, so it can never be heard from, and `boot_timeout` after the launch the
/// machine is released (7.2, 13.3). The host is in the registry (the hello was admitted)
/// and silent; one minute before `boot_timeout` nothing is terminated, at `boot_timeout`
/// the record is Releasing and the machine is terminated. It catches a machine kept and
/// billed past its boot time because its host registered and then went silent.
#[tokio::test]
#[ignore = "needs #387: the registry does not yet report heard-since-hello"]
async fn a_lost_welcome_is_released_at_boot_timeout() {
    let rig = Rig::start("e2e_lost_welcome").await;
    let (name, token) = lose_the_welcome(&rig).await;
    rig.tick().await;
    rig.advance(14 * 60);
    rig.tick().await;
    assert_eq!(rig.account.count(Call::Terminate), 0);
    rig.advance(60);
    rig.tick().await;
    assert_eq!(rig.phase(), Phase::Releasing);
    assert_released_once(&rig, &name, &token).await;
}

/// Asserts that a lost welcome is given up on at last, whatever the delay: the host is
/// silent from its hello on, the clock moves a minute at a time, and the machine is
/// terminated once, its token having been revoked first; then the host is removed, the
/// device deleted, and no second machine was launched and no second host registered. It
/// catches a machine that a registered, silent host keeps for ever.
#[tokio::test]
async fn a_lost_welcome_is_released_in_the_end_and_nothing_else_registers() {
    let rig = Rig::start("e2e_lost_welcome_end").await;
    let (name, token) = lose_the_welcome(&rig).await;
    for _ in 0..200 {
        rig.advance(60);
        rig.tick().await;
        if rig.account.count(Call::Terminate) > 0 {
            break;
        }
    }
    assert_released_once(&rig, &name, &token).await;
}

/// Asserts a refused acquire. The provider answers the launch with a 403 (denied): no
/// machine exists, the join token minted for it is revoked and the Tailscale API is told
/// to forget the host's device; the refused record is dropped, in memory and in the file;
/// the pool launches nothing while it cools off, and launches once it has. It catches a
/// refused launch kept as a machine that is counted for ever, a token left alive, and a
/// pool that asks again at once.
#[tokio::test]
async fn a_refused_launch_leaves_no_machine_no_token_and_a_cooling_pool() {
    let rig = Rig::start("e2e_refused").await;
    let denied = ApiError::Api {
        status: 403,
        code: ErrorCode::Forbidden,
        request_id: None,
    };
    rig.account.fail_next(Call::Launch, denied);
    rig.tick().await;
    rig.wait();
    rig.tick().await;
    assert_eq!(rig.account.launched(), 0);
    assert_eq!(rig.phase(), Phase::Launching);
    assert_eq!(joins_held(&rig.h), 1);
    assert_eq!(rig.asked(KEYS), 1);

    rig.wait();
    rig.tick().await;
    assert!(rig.state().records.instances.is_empty());
    assert!(rig.file().instances.is_empty());
    assert_eq!(joins_held(&rig.h), 0);
    assert_eq!(
        rig.asked(DEVICES),
        1,
        "the host's device is looked for and forgotten"
    );
    assert_eq!(rig.state().blocked["burst"], Blocked::Denied);
    assert!(rig.account.instances().is_empty());

    rig.advance(9 * 60);
    rig.wait();
    rig.tick().await;
    assert_eq!(
        rig.account.count(Call::Launch),
        1,
        "no second try while cooling off"
    );
    rig.advance(61);
    rig.wait();
    rig.tick().await;
    assert_eq!(rig.account.launched(), 1);
    assert_eq!(rig.account.count(Call::Launch), 2);
    assert_eq!(rig.asked(KEYS), 2);
}

/// Walks a launch whose answer is lost: the machine is launched, the answer is an
/// error, `ticks` more ticks pass, and the server stops. Another starts over the same
/// files.
async fn crash_after_a_lost_answer(name: &str, ticks: usize) -> (Rig, String, String) {
    let rig = Rig::start(name).await;
    let lost = ApiError::Transport("connection reset".to_owned());
    rig.account.lose_response(Call::Launch, lost);
    rig.tick().await;
    rig.wait();
    rig.tick().await;
    assert_eq!(rig.account.launched(), 1);
    assert_eq!(rig.phase(), Phase::Launching);
    for _ in 0..ticks {
        rig.tick().await;
    }
    let (machine, token) = (rig.name(), rig.token(0));
    assert_eq!(rig.asked(KEYS), 1);
    (rig.restart().await, machine, token)
}

/// What must hold after the restart: the machine is the record's, Booting, found by the
/// list; the request is presented again and nothing more is launched, minted or asked of
/// Tailscale; and the token minted before the crash is the one that admits the host.
async fn assert_adopted(rig: &Rig, name: &str, token: &str) {
    rig.tick().await;
    assert_eq!(rig.phase(), Phase::Booting);
    for _ in 0..3 {
        rig.wait();
        rig.tick().await;
    }
    assert_eq!(rig.account.launched(), 1);
    assert_eq!(rig.account.count(Call::Launch), 1);
    assert_eq!(rig.asked(KEYS), 1, "no second key");
    let records = rig.state().records.instances;
    assert_eq!(records.len(), 1);
    assert_eq!(records[0].name, name);
    let (_, credential) = rig.join(token, name).await;
    assert_eq!(credential.len(), 64);
    rig.tick().await;
    assert_eq!(rig.phase(), Phase::Registered);
}

/// Asserts a crash after the call and before its answer (F1). The server stops after the
/// tick that launched, with the record Launching on disk and the lost answer only in
/// memory. The new server adopts the record, its list shows the machine and the record is
/// Booting; no second machine is launched, no token or key is minted again, and the
/// first token admits the host. It catches a restart that forgets a launch and buys a
/// second machine, and one that mints again and locks the booted host out.
#[tokio::test]
async fn a_crash_after_the_launch_does_not_launch_again() {
    let (rig, name, token) = crash_after_a_lost_answer("e2e_crash_launching", 0).await;
    assert_adopted(&rig, &name, &token).await;
}

/// Asserts the same when the record had already become Unresolved (F2) before the crash:
/// the new server's list settles it, nothing is launched or minted again.
#[tokio::test]
async fn a_crash_while_a_launch_is_unresolved_does_not_launch_again() {
    let (rig, name, token) = crash_after_a_lost_answer("e2e_crash_unresolved", 1).await;
    assert_adopted(&rig, &name, &token).await;
}

/// The path of the token file of `rig`.
fn joins_path(rig: &Rig) -> PathBuf {
    let mut joins = rig.h.scratch.state().into_os_string();
    joins.push(".joins");
    PathBuf::from(joins)
}

/// Asserts the order of a release and a crash in the middle of it. The host is idle, the
/// registry marks it releasing, and the token file cannot be written, so the revoke
/// fails: the record goes Releasing and is saved, and the machine is not terminated, tick
/// after tick, while the credential is not revoked. The server then stops. The new
/// server marks the machine ending before it admits any hello: the host that says hello
/// again is listed `releasing`, takes no lease and no reservation; the first tick revokes
/// its credential and terminates the machine, the next reads it gone, removes the host and
/// deletes its device. Exactly one machine was ever launched and it is terminated once.
/// It catches a machine terminated while its credential works, a host that rejoins
/// unmarked and could be booked before its machine is gone, and a release that is
/// forgotten at a restart.
#[tokio::test]
async fn a_machine_is_not_terminated_before_its_token_is_revoked_and_a_restart_finishes_the_release()
 {
    let rig = Rig::start("e2e_crash_releasing").await;
    let (name, token) = rig.launch().await;
    rig.withdraw();
    let (me, credential) = rig.join(&token, &name).await;
    rig.tick().await;
    assert_eq!(rig.phase(), Phase::Registered);
    for _ in 0..40 {
        rig.advance(10);
        assert_eq!(rig.beat(&credential, &me).await.status, 200);
        rig.tick().await;
        if rig.mark(&name).await == "releasing" {
            break;
        }
    }
    assert_eq!(rig.mark(&name).await, "releasing");
    assert_eq!(rig.phase(), Phase::Draining);
    assert_eq!(rig.account.count(Call::Terminate), 0);

    let joins = joins_path(&rig);
    let kept = std::fs::read(&joins).unwrap();
    std::fs::remove_file(&joins).unwrap();
    std::fs::create_dir(&joins).unwrap();
    for _ in 0..2 {
        rig.tick().await;
        assert_eq!(rig.phase(), Phase::Releasing);
        assert_eq!(rig.file().instances[0].phase, Phase::Releasing);
        assert_eq!(rig.account.count(Call::Terminate), 0, "revoke comes first");
        assert_eq!(rig.account.instances().len(), 1);
    }

    std::fs::remove_dir(&joins).unwrap();
    std::fs::write(&joins, kept).unwrap();
    std::fs::set_permissions(&joins, std::fs::Permissions::from_mode(0o600)).unwrap();
    let rig = rig.restart().await;
    let again = rig.agent_says_hello(&credential, &name).await;
    let me = session(&again, &name);
    assert_eq!(rig.mark(&name).await, "releasing");
    assert!(!matches!(rig.present(), StartAck::Started { .. }));
    let counts = rig.registry().whole_counts();
    assert_eq!((counts.reserved, counts.held), (0, 0));
    rig.withdraw();
    assert_eq!(rig.account.count(Call::Terminate), 0);

    rig.tick().await;
    assert_eq!(rig.account.count(Call::Terminate), 1);
    assert_eq!(rig.beat(&credential, &me).await.status, 401);
    rig.tick().await;
    assert_eq!(rig.phase(), Phase::Gone);
    assert!(rig.registry().hosts().is_empty());
    assert_eq!(rig.asked(DELETE_DEVICE), 1);
    assert_eq!(rig.account.launched(), 1);
    assert_eq!(rig.account.count(Call::Terminate), 1);
}

/// Asserts that the record is written before anything is done for it. The file of the
/// records cannot be written: the tick that would launch counts the failure and does
/// nothing else, so no join token is minted, no Tailscale key is made and nothing is
/// launched, however often it is tried; once the file can be written the next tick
/// launches. It catches a launch, a mint or a key that does not wait for its record.
#[tokio::test]
async fn nothing_is_launched_while_the_record_cannot_be_written() {
    let rig = Rig::start("e2e_records_first").await;
    rig.tick().await;
    let path = records_path(&rig.h.scratch);
    std::fs::create_dir(&path).unwrap();
    for _ in 0..3 {
        rig.wait();
        rig.tick().await;
    }
    let scaler = rig.h.running.scaler().unwrap();
    assert_eq!(scaler.save_errors(), 3);
    assert_eq!(rig.account.launched(), 0);
    assert_eq!(rig.account.count(Call::Launch), 0);
    assert_eq!(rig.asked(KEYS), 0);
    assert_eq!(joins_held(&rig.h), 0);

    std::fs::remove_dir(&path).unwrap();
    rig.wait();
    rig.tick().await;
    assert_eq!(rig.account.launched(), 1);
    assert_eq!(rig.phase(), Phase::Launching);
    assert_eq!(rig.file().instances.len(), 1);
    assert_eq!(rig.h.running.scaler().unwrap().save_errors(), 3);
}
