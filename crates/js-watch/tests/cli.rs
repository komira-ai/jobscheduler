//! The command line and the configuration file, against loopback servers that stand in
//! for the cloud, the status page and the alert receiver. Nothing leaves 127.0.0.1.

use std::ffi::OsString;
use std::fs;
use std::net::TcpListener;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::sync::Mutex;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use js_alert::Receiver;
use js_elastic::{AcquireId, HostName, SchedulerId};
use js_watch::{Config, ConfigError, run};

/// A directory of files for one test, removed when it is dropped.
struct Scratch {
    dir: PathBuf,
}

impl Scratch {
    fn new(name: &str) -> Self {
        static NEXT: AtomicUsize = AtomicUsize::new(0);
        let dir = Path::new(env!("CARGO_TARGET_TMPDIR"))
            .join("js-watch")
            .join(format!("{name}-{}", NEXT.fetch_add(1, Ordering::SeqCst)));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        Self { dir }
    }

    fn file(&self, name: &str, text: &str, mode: u32) -> PathBuf {
        let path = self.dir.join(name);
        fs::write(&path, text).unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(mode)).unwrap();
        path
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.dir);
    }
}

/// An address nothing listens on.
fn closed() -> String {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    format!("http://{}", listener.local_addr().unwrap())
}

struct Ran {
    code: String,
    out: String,
    err: String,
    sleeps: Vec<Duration>,
}

impl Ran {
    fn exited(&self, code: u8) -> bool {
        self.code == format!("{:?}", ExitCode::from(code))
    }
}

fn exec(args: &[&str]) -> Ran {
    let args: Vec<OsString> = std::iter::once("js-watch")
        .chain(args.iter().copied())
        .map(OsString::from)
        .collect();
    let (mut out, mut err) = (Vec::new(), Vec::new());
    let sleeps = Mutex::new(Vec::new());
    let sleep = |pause: Duration| sleeps.lock().unwrap().push(pause);
    let code = run(args, &mut out, &mut err, &sleep);
    Ran {
        code: format!("{code:?}"),
        out: String::from_utf8(out).unwrap(),
        err: String::from_utf8(err).unwrap(),
        sleeps: sleeps.into_inner().unwrap(),
    }
}

/// The URLs and files of one configuration.
struct Parts<'a> {
    scratch: &'a Scratch,
    lambda: String,
    status: String,
    hook: String,
    extra: &'a str,
}

fn config(parts: &Parts<'_>) -> PathBuf {
    let key = parts.scratch.file("key", "fixture-key\n", 0o600);
    let token = parts.scratch.file("token", "hook-token\n", 0o600);
    let text = format!(
        r#"scheduler_id = "sched-one"
cloud = "lambda"
status_url = "{}"
{}
[lambda]
api_key_file = "{}"
api_url = "{}"

[alerts]
webhook_url = "{}"
webhook_token_file = "{}"
"#,
        parts.status,
        parts.extra,
        key.display(),
        parts.lambda,
        parts.hook,
        token.display(),
    );
    parts.scratch.file("watch.toml", &text, 0o600)
}

fn offline(scratch: &Scratch, hook: &str, extra: &str) -> PathBuf {
    config(&Parts {
        scratch,
        lambda: closed(),
        status: closed(),
        hook: hook.to_owned(),
        extra,
    })
}

fn path(file: &Path) -> &str {
    file.to_str().unwrap()
}

/// Asserts the help text and the two usage failures: `--help` prints usage on stdout and
/// succeeds, and a missing subcommand and an unknown cloud both exit 2 with the usage on
/// stderr. It catches a command line that panics or exits 0 on a typo.
#[test]
fn help_succeeds_and_a_usage_error_exits_2() {
    let help = exec(&["--help"]);
    assert!(help.exited(0), "{}", help.code);
    assert!(
        help.out.contains("run") && help.out.contains("abort"),
        "{}",
        help.out
    );
    let none = exec(&[]);
    assert!(
        none.exited(2) && none.err.contains("Usage"),
        "{} {}",
        none.code,
        none.err
    );
    let cloud = exec(&[
        "abort",
        "--config",
        "x",
        "--cloud",
        "nowhere",
        "--scheduler-id",
        "s",
    ]);
    assert!(cloud.exited(2), "{}", cloud.code);
}

/// Asserts the defaults and the explicit values of the timing keys, and the refusal of a
/// zero. It catches a default that changed unnoticed (the heartbeat window is an
/// operational promise) and a zero interval that would spin.
#[test]
fn the_timing_keys_default_and_refuse_zero() {
    let scratch = Scratch::new("timing");
    let plain = Config::load(&offline(&scratch, "https://hook.example.org/x", "")).unwrap();
    let settings = plain.settings();
    assert_eq!(plain.interval(), Duration::from_secs(60));
    assert_eq!(settings.stale_after, Duration::from_secs(300));
    assert_eq!(settings.confirm_ticks, 2);
    assert_eq!(settings.sweep_every, Duration::from_secs(1800));
    let extra = "interval_secs = 5\nstale_after_secs = 6\nconfirm_ticks = 3\nsweep_every_secs = 7";
    let set = Config::load(&offline(&scratch, "https://hook.example.org/x", extra)).unwrap();
    assert_eq!(set.interval(), Duration::from_secs(5));
    assert_eq!(set.settings().stale_after, Duration::from_secs(6));
    assert_eq!(set.settings().confirm_ticks, 3);
    assert_eq!(set.settings().sweep_every, Duration::from_secs(7));
    for key in [
        "interval_secs",
        "stale_after_secs",
        "confirm_ticks",
        "sweep_every_secs",
    ] {
        let file = offline(
            &scratch,
            "https://hook.example.org/x",
            &format!("{key} = 0"),
        );
        let refused = Config::load(&file).unwrap_err();
        assert_eq!(refused.to_string(), format!("{key} must be at least 1"));
    }
}

/// Asserts the refusals of the file itself: a file that is not there, a key it does not
/// know, a missing `[lambda]` table; each named, none repeating a value. It catches a
/// typo in a key silently ignored (a watcher that never alarms).
#[test]
fn a_bad_file_is_refused_by_name() {
    let scratch = Scratch::new("file");
    let missing = Config::load(&scratch.dir.join("none.toml")).unwrap_err();
    assert!(matches!(missing, ConfigError::Read { .. }), "{missing}");
    let typo = offline(
        &scratch,
        "https://hook.example.org/x",
        "stale_after_sec = 1",
    );
    let refused = Config::load(&typo).unwrap_err();
    assert!(matches!(refused, ConfigError::Syntax { .. }), "{refused}");
    assert!(refused.to_string().contains("stale_after_sec"), "{refused}");
    let bare = scratch.file(
        "bare.toml",
        "scheduler_id = \"s\"\ncloud = \"lambda\"\nstatus_url = \"http://127.0.0.1:1\"\n[alerts]\nwebhook_url = \"https://h.example.org\"\n",
        0o600,
    );
    let config = Config::load(&bare).unwrap();
    assert_eq!(config.lambda().unwrap_err(), ConfigError::NoLambda);
    let ran = exec(&["run", "--config", path(&bare)]);
    assert!(
        ran.exited(2) && ran.err.contains("[lambda] is required"),
        "{}",
        ran.err
    );
}

/// Asserts that `run` refuses each unusable input before it watches anything, with a
/// message that names the problem and repeats no secret: a file that is not there, a key
/// file others can read, a missing key file, a status URL or API URL or webhook URL the
/// rule refuses, a missing token file, a bad scheduler id. It catches a watcher that
/// starts half-configured and never alarms.
#[test]
fn run_refuses_what_it_cannot_use_before_it_starts() {
    let scratch = Scratch::new("refuse");
    let hook = "https://hook.example.org/SECRET";
    let good = offline(&scratch, hook, "");
    let text = fs::read_to_string(&good).unwrap();
    let broken = |name: &str, from: &str, to: &str| {
        let changed = text.replacen(from, to, 1);
        assert_ne!(changed, text, "{from} not found");
        scratch.file(name, &changed, 0o600)
    };
    let cases = [
        broken(
            "a.toml",
            "scheduler_id = \"sched-one\"",
            "scheduler_id = \"bad id!\"",
        ),
        broken("b.toml", "http://", "ftp://"),
        broken("c.toml", hook, "http://10.0.0.5/hook"),
        broken("e.toml", "/key", "/no-such-key"),
        broken("f.toml", "/token", "/no-such-token"),
    ];
    for file in &cases {
        let ran = exec(&["run", "--config", path(file), "--ticks", "1"]);
        assert!(ran.exited(2), "{}: {}", file.display(), ran.err);
        assert!(!ran.err.contains("SECRET"), "{}", ran.err);
        assert!(ran.sleeps.is_empty());
    }
    // A key file that others can read is refused like the lease server's.
    let open = scratch.dir.join("key");
    fs::set_permissions(&open, fs::Permissions::from_mode(0o644)).unwrap();
    let ran = exec(&["run", "--config", path(&good), "--ticks", "1"]);
    assert!(ran.exited(2), "{}", ran.err);
    fs::set_permissions(&open, fs::Permissions::from_mode(0o600)).unwrap();
    // And a plain-http API address that is not loopback.
    let remote = broken(
        "g.toml",
        "api_url = \"http://127.0.0.1",
        "api_url = \"http://10.0.0.5",
    );
    let ran = exec(&["run", "--config", path(&remote), "--ticks", "1"]);
    assert!(ran.exited(2), "{}", ran.err);
}

/// Asserts the whole `run` path with nothing real behind it: the cloud and the status page
/// are closed ports, the receiver is a loopback server. Two ticks happen with one pause
/// between them (and none after the last); the unreachable cloud reaches the receiver as
/// two alerts (the list and the sweep) with the bearer token read from its file, once each. It catches a
/// `run` that is wired to nothing, a token that is not sent, a loop that pauses after its
/// last tick, and a dedupe that is lost between ticks.
#[test]
fn run_wires_the_cloud_the_status_and_the_sink_and_stops_after_its_ticks() {
    let scratch = Scratch::new("run");
    let receiver = Receiver::start(200);
    let hook = format!("http://{}/alert", receiver.addr());
    let file = offline(&scratch, &hook, "interval_secs = 9");
    let ran = exec(&["run", "--config", path(&file), "--ticks", "2"]);
    assert!(ran.exited(0), "{} {}", ran.code, ran.err);
    assert_eq!(ran.sleeps, [Duration::from_secs(9)]);
    let seen = receiver.requests();
    assert_eq!(seen.len(), 2);
    let head = seen[0].head.to_ascii_lowercase();
    assert!(head.contains("authorization: bearer hook-token"), "{head}");
    let keys: Vec<String> = seen
        .iter()
        .map(|each| {
            let body: serde_json::Value = serde_json::from_str(&each.body).unwrap();
            assert_eq!(body["source"], "js-watch");
            body["key"].as_str().unwrap().to_owned()
        })
        .collect();
    assert_eq!(keys, ["list:lambda", "sweep:lambda"]);

    let one = exec(&["run", "--config", path(&file), "--ticks", "1"]);
    assert!(one.exited(0) && one.sleeps.is_empty());

    // Without a token file the alert goes out with no authorization header.
    let text = fs::read_to_string(&file).unwrap();
    let bare: Vec<&str> = text
        .lines()
        .filter(|line| !line.starts_with("webhook_token_file"))
        .collect();
    let file = scratch.file("bare.toml", &bare.join("\n"), 0o600);
    let before = receiver.requests().len();
    let ran = exec(&["run", "--config", path(&file), "--ticks", "1"]);
    assert!(ran.exited(0), "{}", ran.err);
    let seen = receiver.requests();
    assert_eq!(seen.len(), before + 2);
    let head = seen[before].head.to_ascii_lowercase();
    assert!(!head.contains("authorization"), "{head}");
}

/// The arguments of `abort` for `sched-one` with the configuration at `file`.
fn abort_args<'a>(file: &'a str, extra: &[&'a str]) -> Vec<&'a str> {
    let base = [
        "abort",
        "--config",
        file,
        "--cloud",
        "lambda",
        "--scheduler-id",
        "sched-one",
    ];
    base.into_iter().chain(extra.iter().copied()).collect()
}

/// The body of `GET /instances` with one running instance of `sched-one` in `region-a`.
fn one_instance() -> String {
    let name = HostName::derive(
        &SchedulerId::new("sched-one").unwrap(),
        &AcquireId::new("a").unwrap(),
    );
    format!(
        r#"{{"data":[{{"id":"i-aaa","name":"{name}","status":"active","region":{{"name":"region-a"}},"instance_type":{{"name":"gpu_1x_sample"}},"tags":[{{"key":"jobscheduler:scheduler","value":"sched-one"}},{{"key":"jobscheduler:acquire","value":"a"}}]}}],"page_token":null}}"#,
        name = name.as_str()
    )
}

/// Asserts the abort command end to end against a loopback stand-in for Lambda's API: with
/// an empty account it succeeds and says so; with one instance a dry run lists it and
/// succeeds, the account having been asked only to list (every request the stand-in saw is
/// a GET); and `--apply` against a stand-in that never confirms a termination exits 1, says
/// which instance failed, and says how many. It catches an abort whose exit status hides a
/// failed stop, and a dry run that sends anything but reads.
#[test]
fn abort_prints_a_plan_and_fails_loudly_when_a_stop_is_not_confirmed() {
    let scratch = Scratch::new("abort");
    let account = Receiver::start_with(200, r#"{"data":[],"page_token":null}"#);
    let file = config(&Parts {
        scratch: &scratch,
        lambda: format!("http://{}", account.addr()),
        status: closed(),
        hook: "https://hook.example.org/x".to_owned(),
        extra: "",
    });
    let empty = exec(&abort_args(path(&file), &[]));
    assert!(empty.exited(0), "{}", empty.err);
    assert!(empty.out.contains("total: 0 instance(s)"), "{}", empty.out);

    let account = Receiver::start_with(200, &one_instance());
    let file = config(&Parts {
        scratch: &scratch,
        lambda: format!("http://{}", account.addr()),
        status: closed(),
        hook: "https://hook.example.org/x".to_owned(),
        extra: "",
    });
    let dry = exec(&abort_args(path(&file), &[]));
    assert!(dry.exited(0), "{}", dry.err);
    assert!(
        dry.out.contains("would stop i-aaa gpu_1x_sample region-a"),
        "{}",
        dry.out
    );
    for request in account.requests() {
        assert!(request.head.starts_with("GET "), "{}", request.head);
    }

    let apply = exec(&abort_args(path(&file), &["--apply"]));
    assert!(apply.exited(1), "{}", apply.code);
    assert!(apply.out.contains("FAILED i-aaa"), "{}", apply.out);
    assert!(
        apply.err.contains("1 instance(s) were not stopped"),
        "{}",
        apply.err
    );
}

/// Asserts that an abort whose account cannot be listed exits 1 with the reason and has
/// stopped nothing, and that a bad scheduler id or a file without credentials is refused
/// with 2. It catches an abort that treats an unreadable account as an empty one.
#[test]
fn abort_refuses_a_bad_id_and_fails_when_the_account_cannot_be_listed() {
    let scratch = Scratch::new("abort_fail");
    let file = offline(&scratch, "https://hook.example.org/x", "");
    let down = exec(&[
        "abort",
        "--config",
        path(&file),
        "--cloud",
        "lambda",
        "--scheduler-id",
        "sched-one",
        "--apply",
    ]);
    assert!(down.exited(1), "{}", down.code);
    assert!(down.out.is_empty(), "{}", down.out);
    let bad = exec(&[
        "abort",
        "--config",
        path(&file),
        "--cloud",
        "lambda",
        "--scheduler-id",
        "bad id!",
    ]);
    assert!(bad.exited(2), "{}", bad.err);
    let bare = scratch.file(
        "bare.toml",
        "scheduler_id = \"s\"\ncloud = \"lambda\"\nstatus_url = \"http://127.0.0.1:1\"\n[alerts]\nwebhook_url = \"https://h.example.org\"\n",
        0o600,
    );
    let none = exec(&[
        "abort",
        "--config",
        path(&bare),
        "--cloud",
        "lambda",
        "--scheduler-id",
        "s",
    ]);
    assert!(none.exited(2), "{}", none.err);

    let absent = scratch.dir.join("absent.toml");
    let missing = exec(&abort_args(path(&absent), &[]));
    assert!(missing.exited(2), "{}", missing.err);
    let text = fs::read_to_string(&file).unwrap();
    let no_key = scratch.file(
        "no-key.toml",
        &text.replacen("/key", "/no-such-key", 1),
        0o600,
    );
    let nokey = exec(&abort_args(path(&no_key), &[]));
    assert!(nokey.exited(2), "{}", nokey.err);
}
