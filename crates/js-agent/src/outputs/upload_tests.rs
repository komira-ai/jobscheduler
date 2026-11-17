//! The upload of a job's outputs against the in-memory store, with a manual clock: every put
//! and the manifest's exact JSON, a store that is down, a credential that expired or is not
//! held, a refusal, and a budget that runs out. Nothing sleeps: each step is one call, and the
//! thread of [`Plan::start`] runs on a manual wake.

use std::fs::{self, File};
use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use js_agent_wire::LogGrant;
use js_logstore::fake::{Action as Can, FakeStore, Policy, Status};
use js_logstore::outputs::{Manifest, OutputFailure, OutputFile, OutputPath};
use js_logstore::{
    Credential, Endpoint, HttpRequest, HttpResponse, JobPrefix, ManualClock, Transport,
    TransportError,
};
use js_testkit::dir::TestDir;
use sha2::{Digest as _, Sha256};

use super::gather::{Caps, Gathered, Gotten, gather};
use super::upload::{Limits, Next, Upload};
use super::{Plan, Uploading};
use crate::logs::{HostGrant, LiveLogs, ManualWake, Settled, Sink, Wake};

const ROOMY: Caps = Caps {
    file_bytes: 1 << 20,
    total_bytes: 1 << 30,
    files: 100,
    entries: 1000,
    failures: 100,
    depth: 16,
};

fn start_time() -> SystemTime {
    UNIX_EPOCH + Duration::from_secs(1_800_000_000)
}

fn endpoint() -> Endpoint {
    Endpoint::parse("http://127.0.0.1:9000", "test-1", "logs", true).unwrap()
}

fn prefix() -> JobPrefix {
    JobPrefix::new("js", "j-1").unwrap()
}

/// A limits with a one-second first backoff and a minute's budget.
fn limits() -> Limits {
    Limits {
        budget: Duration::from_secs(60),
        backoff_first: Duration::from_secs(1),
        backoff_max: Duration::from_secs(4),
        max_put: 1 << 20,
    }
}

/// A store, its clock and the host's credential cell.
struct Rig {
    dir: TestDir,
    clock: Arc<ManualClock>,
    store: FakeStore,
    grant: Arc<HostGrant>,
}

impl Rig {
    /// A rig whose host holds a credential that may put under `js/`.
    fn new(name: &str) -> Self {
        let rig = Self::bare(name);
        rig.renew("js/");
        rig
    }

    /// A rig whose host holds no credential.
    fn bare(name: &str) -> Self {
        let clock = Arc::new(ManualClock::new(start_time()));
        Self {
            dir: TestDir::under(std::env::temp_dir(), name),
            store: FakeStore::new(endpoint(), clock.clone()),
            grant: Arc::new(HostGrant::new(clock.clone())),
            clock,
        }
    }

    /// A new credential that may put under `prefix`, in the cell as a reply's grant would be.
    fn renew(&self, prefix: &str) {
        let credential = self.store.issue(Policy {
            actions: vec![Can::Put],
            prefix: prefix.to_owned(),
        });
        self.grant.replace(&grant_of(&credential));
    }

    /// Writes `text` at `path` below the working directory.
    fn put(&self, path: &str, text: &str) {
        let at = self.dir.path().join(path);
        fs::create_dir_all(at.parent().unwrap()).unwrap();
        fs::write(at, text).unwrap();
    }

    /// The upload of what `declared` gathers.
    fn upload(&self, declared: &[&str], limits: Limits) -> Upload {
        let declared: Vec<String> = declared.iter().map(|path| (*path).to_owned()).collect();
        let gathered = gather(self.dir.path(), &declared, ROOMY);
        self.upload_of(gathered, limits)
    }

    fn upload_of(&self, gathered: Gathered, limits: Limits) -> Upload {
        Upload::new(
            (prefix(), endpoint()),
            gathered,
            (
                Arc::new(self.store.clone()),
                self.clock.clone(),
                self.grant.clone(),
            ),
            limits,
        )
    }

    /// The object at `key`, as text.
    fn object(&self, key: &str) -> Option<String> {
        let objects = self.store.objects();
        objects
            .get(key)
            .map(|bytes| String::from_utf8(bytes.clone()).unwrap())
    }

    /// The keys of every object.
    fn keys(&self) -> Vec<String> {
        self.store.objects().into_keys().collect()
    }

    fn at(&self, secs: u64) -> SystemTime {
        start_time() + Duration::from_secs(secs)
    }
}

/// The grant a heartbeat reply carries for `credential`.
fn grant_of(credential: &Credential) -> LogGrant {
    let ms = credential
        .expires_at
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_millis();
    LogGrant {
        access_key_id: credential.access_key_id.clone(),
        secret_access_key: credential.secret_access_key.clone(),
        session_token: credential.session_token.clone(),
        expires_at_ms: u64::try_from(ms).unwrap(),
    }
}

fn sha(text: &str) -> String {
    Sha256::digest(text.as_bytes())
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

fn file(path: &str, text: &str) -> OutputFile {
    OutputFile {
        path: path.to_owned(),
        bytes: u64::try_from(text.len()).unwrap(),
        sha256: sha(text),
    }
}

fn failure(path: &str, reason: &str) -> OutputFailure {
    OutputFailure {
        path: path.to_owned(),
        reason: reason.to_owned(),
    }
}

/// The manifest at the job's key, read back.
fn manifest(rig: &Rig) -> Manifest {
    let text = rig.object("js/j-1/outputs.json").expect("a manifest");
    serde_json::from_str(&text).unwrap()
}

/// Asserts that one step puts each output file at `P/outputs/<path>` with its bytes, then the
/// manifest at `P/outputs.json`, whose exact JSON lists each file with its length and SHA-256,
/// the missing path and the gathering's refusal, and that the upload is then over. It catches
/// a file put under another key, a manifest put before a file, and a digest of other bytes.
#[test]
fn every_file_then_the_manifest_is_put() {
    let rig = Rig::new("outputs-upload-all");
    rig.put("a.txt", "alpha");
    rig.put("out/b", "beta");
    rig.put("out/a b", "spaced");
    let mut upload = rig.upload(&["a.txt", "out", "missing"], limits());
    assert_eq!(upload.step(), Next::Done);
    assert_eq!(
        rig.keys(),
        [
            "js/j-1/outputs.json",
            "js/j-1/outputs/a.txt",
            "js/j-1/outputs/out/b",
        ]
    );
    assert_eq!(rig.object("js/j-1/outputs/a.txt").unwrap(), "alpha");
    assert_eq!(rig.object("js/j-1/outputs/out/b").unwrap(), "beta");
    assert_eq!(
        rig.object("js/j-1/outputs.json").unwrap(),
        format!(
            r#"{{"version":1,"files":[{{"path":"a.txt","bytes":5,"sha256":"{}"}},{{"path":"out/b","bytes":4,"sha256":"{}"}}],"missing":["missing"],"failed":[{{"path":"out/a?b","reason":"name"}}]}}"#,
            sha("alpha"),
            sha("beta")
        )
    );
}

/// Asserts that a store that is down is asked again after the backoff, doubled for each failure
/// in a row up to its most, and that nothing is put early: three `SlowDown`s hold the upload one,
/// two and four seconds, and it then finishes. It catches an upload that gives up on a store
/// that comes back, and one that asks again at once.
#[test]
fn a_store_that_is_down_is_asked_again_after_a_growing_wait() {
    let rig = Rig::new("outputs-upload-down");
    rig.put("a.txt", "alpha");
    let mut upload = rig.upload(&["a.txt"], limits());
    rig.store.fail_next(3, Status::SlowDown);
    assert_eq!(upload.step(), Next::Sleep(rig.at(1)));
    assert_eq!(upload.step(), Next::Sleep(rig.at(1)), "not yet");
    rig.clock.advance(Duration::from_secs(1));
    assert_eq!(upload.step(), Next::Sleep(rig.at(3)));
    rig.clock.advance(Duration::from_secs(2));
    assert_eq!(upload.step(), Next::Sleep(rig.at(7)));
    assert!(rig.keys().is_empty());
    rig.clock.advance(Duration::from_secs(4));
    assert_eq!(upload.step(), Next::Done);
    assert_eq!(manifest(&rig).files, [file("a.txt", "alpha")]);
}

/// Asserts that a file is read again from its first byte for each try, and that one whose bytes
/// changed between two tries is failed `read`, the next taken and the manifest put. It catches
/// a retry that sends what the manifest's digest does not describe.
#[test]
fn a_file_that_changed_between_tries_is_failed_read() {
    let rig = Rig::new("outputs-upload-changed");
    rig.put("a.txt", "alpha");
    rig.put("b.txt", "beta");
    let mut upload = rig.upload(&["a.txt", "b.txt"], limits());
    rig.store.fail_next(1, Status::SlowDown);
    assert_eq!(upload.step(), Next::Sleep(rig.at(1)));
    rig.put("a.txt", "omega");
    rig.clock.advance(Duration::from_secs(1));
    assert_eq!(upload.step(), Next::Done);
    let written = manifest(&rig);
    assert_eq!(written.files, [file("b.txt", "beta")]);
    assert_eq!(written.failed, [failure("a.txt", "read")]);
}

/// Asserts that a credential the store answers as expired is reported refused to the host's
/// cell (which then says it is stale), that the upload waits, and that a newer credential ends
/// the wait at once, with no time passing. It catches an upload that keeps signing with a dead
/// credential, and one that waits out its backoff though a new one came.
#[test]
fn an_expired_credential_waits_for_the_next() {
    let rig = Rig::new("outputs-upload-expired");
    rig.put("a.txt", "alpha");
    let mut upload = rig.upload(&["a.txt"], limits());
    rig.store.fail_next(1, Status::ExpiredToken);
    assert_eq!(upload.step(), Next::Sleep(rig.at(1)));
    assert!(rig.grant.state().stale);
    rig.renew("js/");
    assert_eq!(upload.step(), Next::Done);
    assert_eq!(manifest(&rig).files, [file("a.txt", "alpha")]);
}

/// Asserts that with no credential held nothing is asked of the store and the upload looks
/// again a backoff later, finishing once one is held. It catches an unsigned request.
#[test]
fn with_no_credential_the_upload_waits() {
    let rig = Rig::bare("outputs-upload-none");
    rig.put("a.txt", "alpha");
    let mut upload = rig.upload(&["a.txt"], limits());
    assert_eq!(upload.step(), Next::Sleep(rig.at(1)));
    assert!(rig.store.operations().is_empty());
    rig.renew("js/");
    assert_eq!(upload.step(), Next::Done);
    assert_eq!(manifest(&rig).files, [file("a.txt", "alpha")]);
}

/// Asserts that a file the store refuses for good is listed failed `store` and the next file and
/// the manifest are still put; that a file over one put, and a file that cannot be read, are
/// failed `too-large` and `read`; and that a manifest the store refuses ends the upload with no
/// manifest. It catches one refused file losing every output, and a refused manifest asked for
/// again and again.
#[test]
fn a_refused_file_is_failed_and_a_refused_manifest_ends_the_upload() {
    let rig = Rig::new("outputs-upload-refused");
    rig.put("a.txt", "alpha");
    rig.put("b.txt", "beta");
    rig.put("big", "four");
    let mut small = limits();
    small.max_put = 3;
    let mut gathered = gather(
        rig.dir.path(),
        &["a.txt".to_owned(), "b.txt".to_owned(), "big".to_owned()],
        ROOMY,
    );
    let unreadable = rig.dir.path().join("w");
    gathered.files.push(Gotten {
        path: OutputPath::parse("w").unwrap(),
        file: File::create(&unreadable).unwrap(),
    });
    let mut upload = rig.upload_of(gathered, small);
    assert_eq!(upload.step(), Next::Done);
    let got = manifest(&rig);
    assert_eq!(got.files, []);
    assert_eq!(
        got.failed,
        [
            failure("a.txt", "too-large"),
            failure("b.txt", "too-large"),
            failure("big", "too-large"),
            failure("w", "read"),
        ]
    );
    let rig = Rig::new("outputs-upload-refused-file");
    rig.put("a.txt", "alpha");
    rig.put("b.txt", "beta");
    let mut upload = rig.upload(&["a.txt", "b.txt"], limits());
    rig.store.fail_next(1, Status::AccessDenied);
    assert_eq!(upload.step(), Next::Done);
    let got = manifest(&rig);
    assert_eq!(got.files, [file("b.txt", "beta")]);
    assert_eq!(got.failed, [failure("a.txt", "store")]);
    let rig = Rig::bare("outputs-upload-refused-manifest");
    rig.renew("js/j-1/outputs/");
    rig.put("a.txt", "alpha");
    let mut upload = rig.upload(&["a.txt"], limits());
    assert_eq!(upload.step(), Next::Done);
    assert_eq!(rig.keys(), ["js/j-1/outputs/a.txt"]);
}

/// Asserts that once the budget is spent the upload stops before its next request and puts no
/// manifest, and that a wait never reaches past the budget's end. It catches an upload that
/// runs on forever against a store that never comes back.
#[test]
fn past_its_budget_the_upload_stops_with_no_manifest() {
    let rig = Rig::new("outputs-upload-budget");
    rig.put("a.txt", "alpha");
    let mut short = limits();
    short.budget = Duration::from_secs(2);
    let mut upload = rig.upload(&["a.txt"], short);
    rig.store.fail_next(10, Status::InternalError);
    assert_eq!(upload.step(), Next::Sleep(rig.at(1)));
    rig.clock.advance(Duration::from_secs(1));
    assert_eq!(upload.step(), Next::Sleep(rig.at(2)), "held to the budget");
    rig.clock.advance(Duration::from_secs(1));
    assert_eq!(upload.step(), Next::Done);
    assert!(rig.keys().is_empty());
    let rig = Rig::bare("outputs-upload-budget-none");
    let mut upload = rig.upload(&[], short);
    assert_eq!(upload.step(), Next::Sleep(rig.at(1)));
    rig.clock.advance(Duration::from_secs(2));
    assert_eq!(upload.step(), Next::Done);
}

/// The live-log settings of `rig` over `transport`, waking on `wake`.
fn logs(rig: &Rig, transport: Arc<dyn Transport>, wake: Arc<ManualWake>) -> LiveLogs {
    LiveLogs {
        file_cap: 1 << 20,
        grant: rig.grant.clone(),
        transport,
        clock: rig.clock.clone(),
        wakes: Arc::new(move || wake.clone() as Arc<dyn Wake>),
        limits: crate::logs::Limits::default(),
    }
}

fn sink() -> Sink {
    Sink::new(endpoint(), prefix())
}

/// Starts the plan of `declared` below `root` and waits for its thread.
fn started(plan: Plan, root: &Path, wake: &ManualWake) -> Uploading {
    let uploading = plan.start(root);
    assert_eq!(wake.settled(), Settled::Ended);
    uploading
}

/// Asserts the plan of a run: none for a start that declared no outputs, none for a runtime
/// without live logs or a start without a sink; and that a plan gathers below the directory it
/// is started with and its own thread puts the files and the manifest, then ends. It catches a
/// run whose outputs are dropped though it had all it needed, and a removal that waits.
#[test]
fn a_plan_gathers_at_its_start_and_its_thread_puts_the_outputs() {
    let rig = Rig::new("outputs-plan");
    rig.put("a.txt", "alpha");
    let wake = ManualWake::new();
    let live = logs(&rig, Arc::new(rig.store.clone()), wake.clone());
    let declared = vec!["a.txt".to_owned()];
    assert!(Plan::asked(Some(&live), Some(&sink()), &[]).is_none());
    assert!(Plan::asked(None, Some(&sink()), &declared).is_none());
    assert!(Plan::asked(Some(&live), None, &declared).is_none());
    let plan = Plan::asked(Some(&live), Some(&sink()), &declared).unwrap();
    let uploading = started(plan.with_limits(limits()), rig.dir.path(), &wake);
    assert!(uploading.join());
    assert_eq!(manifest(&rig).files, [file("a.txt", "alpha")]);
}

/// A transport that panics.
struct Panics;

impl Transport for Panics {
    fn send(&self, _: HttpRequest) -> Result<HttpResponse, TransportError> {
        panic!("a transport that fails its caller");
    }
}

/// Asserts that a panic in the upload is caught by its thread, which says it has ended and
/// leaves the agent running. It catches a panic that would end the agent's process.
#[test]
fn a_panic_in_the_upload_ends_its_thread_only() {
    let rig = Rig::new("outputs-plan-panic");
    rig.put("a.txt", "alpha");
    let wake = ManualWake::new();
    let live = logs(&rig, Arc::new(Panics), wake.clone());
    let plan = Plan::asked(Some(&live), Some(&sink()), &["a.txt".to_owned()]).unwrap();
    let uploading = started(plan, rig.dir.path(), &wake);
    assert!(uploading.join());
    assert!(rig.keys().is_empty());
}
