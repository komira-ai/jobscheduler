//! `--job-listen` and `server.job_listen`: the setting, the TLS rule it shares with
//! `--listen`, an address in use, and the job store the server opens beside the state file
//! when the job service is on (and only then).

use std::ffi::OsString;
use std::fs;
use std::net::SocketAddr;
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::PathBuf;

use js_lease_server::{Cli, Config, SharedClock, StartupError, channel_ticker, start};
use js_types::ManualClock;

use crate::agents_support::{agent_args, join, start_agents};
use crate::config_support::{PoolText, file, load, refusal, server};
use crate::jobs_support::job_args;
use crate::support::{Harness, Scratch, config, without_agent_pool};

/// A `[server]` table whose listeners let a pool that registers agents start, with `extra`
/// lines after them.
fn server_lines(scratch: &Scratch, extra: &str) -> String {
    let listeners = "listen = \"127.0.0.1:0\"\nagent_listen = \"127.0.0.2:0\"";
    server(scratch, &format!("{listeners}\n{extra}"))
}

/// A file whose one pool registers agents (a joined pool), with `extra` in `[server]`.
fn joined_file(scratch: &Scratch, extra: &str) -> String {
    file(&server_lines(scratch, extra), &[&PoolText::joined("burst")])
}

/// What the refusal of a start says: its flag and its message.
fn refused<T>(result: Result<T, StartupError>) -> (&'static str, String) {
    let err = result.err().expect("the start should be refused");
    (err.flag(), err.to_string())
}

/// Starts the server `config` describes on a clock that stands still.
async fn started(config: Config) -> Result<js_lease_server::Running, StartupError> {
    let (_, ticker) = channel_ticker();
    start(config, SharedClock::new(ManualClock::default()), ticker).await
}

/// The files under `dir` whose names start with `prefix`, sorted.
fn named(dir: &std::path::Path, prefix: &str) -> Vec<PathBuf> {
    let mut found: Vec<PathBuf> = fs::read_dir(dir)
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .filter(|path| {
            path.file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|name| name.starts_with(prefix))
        })
        .collect();
    found.sort();
    found
}

/// Asserts that the flag and the file key give the same address, that neither given is
/// off, that the flag needs `--agent-pool-config` and is refused naming itself without it,
/// that the key must be an
/// address, and that the flag beside `--config` is refused naming the key that takes its
/// place, as every flag is. It catches a key read and dropped, and a flag let through beside
/// the file.
#[test]
fn job_listen_flag_and_config_key_agree() {
    let scratch = Scratch::new("job_listen_agree");
    let want: SocketAddr = "127.0.0.1:7302".parse().unwrap();
    let by_flag = config(job_args(&scratch, "127.0.0.1:7302")).unwrap();
    assert_eq!(by_flag.job_listen(), Some(want));
    let by_key = load(
        &scratch,
        &joined_file(&scratch, "job_listen = \"127.0.0.1:7302\""),
    )
    .unwrap();
    assert_eq!(by_key.job_listen(), Some(want));
    assert_eq!(config(agent_args(&scratch)).unwrap().job_listen(), None);
    assert_eq!(
        load(&scratch, &joined_file(&scratch, ""))
            .unwrap()
            .job_listen(),
        None
    );

    let mut alone = without_agent_pool(&scratch);
    alone.extend(["--job-listen", "127.0.0.1:7302"].map(OsString::from));
    assert_eq!(
        refused(config(alone)).1,
        "--job-listen: requires --agent-pool-config"
    );
    assert_eq!(
        refusal(&scratch, &joined_file(&scratch, "job_listen = 7302")),
        "server.job_listen: must be a string"
    );

    let path = scratch.write("beside.toml", "", 0o644);
    let beside = Cli::try_parse_from([
        OsString::from("js-lease-server"),
        "--config".into(),
        path.into_os_string(),
        "--job-listen".into(),
        "127.0.0.1:0".into(),
    ])
    .unwrap();
    assert_eq!(
        refused(Config::from_cli(beside)).1,
        "--job-listen: not allowed with --config; set server.job_listen"
    );
}

/// Asserts the TLS rule of `--listen` on `--job-listen`: an address off the loopback
/// without `--tls-cert` and `--tls-key` is refused naming `--job-listen`, with the whole
/// reason; a loopback one (v4 or v6) is not, nor one off the loopback with both TLS flags.
/// The file's key is held to the same rule naming `server.job_listen` and the file's pair,
/// and binding all interfaces with TLS is a warning naming the key. It catches a job port
/// that carries a bearer token across a network in clear.
#[test]
fn job_listen_on_a_network_address_requires_tls() {
    let scratch = Scratch::new("job_listen_tls");
    for wide in ["0.0.0.0:7302", "192.0.2.1:7302", "[::]:7302"] {
        assert_eq!(
            refused(config(job_args(&scratch, wide))),
            (
                "--job-listen",
                format!(
                    "--job-listen: {wide} is not a loopback address, so --tls-cert and --tls-key are required: a bearer token never crosses a network in clear"
                )
            ),
            "{wide}"
        );
    }
    for loopback in ["127.0.0.1:7302", "[::1]:7302", "127.9.9.9:1"] {
        assert!(config(job_args(&scratch, loopback)).is_ok(), "{loopback}");
    }
    let mut with_tls = job_args(&scratch, "0.0.0.0:7302");
    with_tls.extend(["--tls-cert", "c.pem", "--tls-key", "k.pem"].map(OsString::from));
    assert!(config(with_tls).is_ok());

    let clear = "is not a loopback address, so server.tls_cert and server.tls_key are required: a bearer token never crosses a network in clear";
    assert_eq!(
        refusal(
            &scratch,
            &joined_file(&scratch, "job_listen = \"10.1.2.3:7302\"")
        ),
        format!("server.job_listen: 10.1.2.3:7302 {clear}")
    );
    let wide = "job_listen = \"0.0.0.0:7302\"\ntls_cert = \"/c\"\ntls_key = \"/k\"";
    let config = load(&scratch, &joined_file(&scratch, wide)).unwrap();
    let warning = "server: warning: binds all interfaces (server.job_listen 0.0.0.0:7302)";
    assert!(
        config.warnings().contains(&warning.to_owned()),
        "{:?}",
        config.warnings()
    );
}

/// Asserts that a job service address already in use refuses the start naming the flag
/// that gave it, `--job-listen` or `server.job_listen`, and that the refused start leaves
/// the lease file free: a server on the same files starts afterwards. It catches a bind
/// failure reported as another flag's, or one that leaves the server half started.
#[tokio::test]
async fn job_listen_in_use_is_refused_naming_the_flag() {
    let busy = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let taken = busy.local_addr().unwrap().to_string();

    let scratch = Scratch::new("job_listen_in_use");
    let flag = config(job_args(&scratch, &taken)).unwrap();
    assert_eq!(refused(started(flag).await).0, "--job-listen");
    let again = config(job_args(&scratch, "127.0.0.1:0")).unwrap();
    let running = started(again).await.unwrap();
    running.shutdown().await.unwrap();

    let other = Scratch::new("job_listen_in_use_file");
    let key = load(
        &other,
        &joined_file(&other, &format!("job_listen = {taken:?}")),
    )
    .unwrap();
    assert_eq!(refused(started(key).await).0, "server.job_listen");
}

/// Asserts that without `--job-listen` no job store is opened: after a start, an agent's
/// join and a stop, no file of the job store is beside the state file, and the server
/// serves no job port. It catches a store opened (and a file made) by a server whose job
/// service is off.
#[tokio::test]
async fn job_listen_off_opens_no_job_store_and_creates_no_file() {
    let h = start_agents("job_listen_off_store").await;
    assert_eq!(h.running.job_addr(), None);
    let _node = join(&h, "node-a").await;
    let Harness {
        running, scratch, ..
    } = h;
    running.shutdown().await.unwrap();
    assert_eq!(
        named(&scratch.dir, "leases.toml.jobs"),
        Vec::<PathBuf>::new()
    );
}

/// Asserts the job store's file beside the state file's path: made by the start, owned by the
/// owner of the directory and owner-only (mode 0600), as is every file of the store beside it
/// (its lock and SQLite's own); and a file already there with a looser mode is kept and
/// made owner-only. It catches a store file that others on the host can read, which would
/// show them the jobs' specs.
#[tokio::test]
async fn job_listen_creates_the_store_file_with_the_neighbouring_mode() {
    for loose in [false, true] {
        let scratch = Scratch::new(&format!("job_listen_store_mode_{loose}"));
        if loose {
            scratch.write("leases.toml.jobs", "", 0o644);
        }
        let args = job_args(&scratch, "127.0.0.1:0");
        let h = Harness::with(scratch, args).await;
        let owner = fs::metadata(&h.scratch.dir).unwrap().uid();
        let files = named(&h.scratch.dir, "leases.toml.jobs");
        assert!(
            files.contains(&h.scratch.path("leases.toml.jobs")),
            "{files:?}"
        );
        assert!(
            files.contains(&h.scratch.path("leases.toml.jobs.lock")),
            "{files:?}"
        );
        for path in &files {
            let meta = fs::metadata(path).unwrap();
            assert_eq!(
                meta.permissions().mode() & 0o777,
                0o600,
                "{path:?} ({loose})"
            );
            assert_eq!(meta.uid(), owner, "{path:?}");
        }
        let Harness { running, .. } = h;
        running.shutdown().await.unwrap();
    }
}

/// Asserts that a job store that cannot be opened refuses the start naming `--job-listen`,
/// with the file in the reason: a directory where the store's file goes (which no user,
/// root included, can open as a file). It catches a start that goes on without the job
/// store it was asked for, or that names another flag.
#[tokio::test]
async fn job_listen_without_a_usable_store_is_refused_naming_the_flag() {
    let scratch = Scratch::new("job_listen_unusable");
    fs::create_dir(scratch.path("leases.toml.jobs")).unwrap();
    let config = config(job_args(&scratch, "127.0.0.1:0")).unwrap();
    let (flag, said) = refused(started(config).await);
    assert_eq!(flag, "--job-listen");
    assert!(said.contains("leases.toml.jobs"), "{said}");
}
