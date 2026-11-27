//! tierstore as a farm build node runs it: its package's unit `tierstore@a`, over a file in the
//! shape of the build nodes' committed `/etc/tierstore/config.toml`, with the test's values in
//! place of the farm's names, addresses and keys, and `trees.max_placements = 2`.
//!
//! The farm's file this mirrors has: an `s3` interface; the `mount-provider` interface on
//! `/var/lib/tierstore/provider.sock` with the mountpoint of the `fs` interface; the `fs`
//! interface at `/run/tierstore/fs`, its control group `tierstore`, its owner the job user's
//! subordinate id 1000 and its put directory over the pack prefix; `[server]` with a handoff
//! socket, the process's name from hostkeeper's facts, a bucket and the full key's file;
//! `[local_cache]` written through to the cold store; the cold store an S3 server; a cached
//! `cas/` policy and an uncached rest. Each value here is the test's: a loopback address the
//! kernel picked in place of the node's port, a bucket of the workflow's S3 server, keys made
//! by the test, and a budget a hosted runner's disk holds. The farm's file sets no `[trees]`;
//! this one sets the cap the test drives at.

use std::net::SocketAddr;
use std::path::Path;
use std::time::{Duration, Instant};

use crate::host::{TIERSTORE_UNIT, chown, make_dir, put, run, start, stop};
use crate::http;
use crate::upstream::{BUCKET, Credentials};

/// tierstore's file.
const CONFIG: &str = "/etc/tierstore/config.toml";

/// The provider's socket, the farm's.
pub const SOCKET: &str = "/var/lib/tierstore/provider.sock";

/// The `fs` interface's mountpoint, the farm's.
pub const MOUNT: &str = "/run/tierstore/fs";

/// The cache's directory.
const CACHE: &str = "/var/lib/tierstore-cache/cas";

/// The marker the unit starts only with.
const MARKER: &str = "/var/lib/tierstore/run-a";

/// How long a start or restart may take before the test fails.
const READY_LIMIT: Duration = Duration::from_secs(10 * 60);

/// The placements tierstore holds at once.
pub const MAX_PLACEMENTS: u32 = 2;

/// The running instance.
pub struct Tierstore {
    /// Its `s3` interface, where `/readyz` and `/metricz` are served.
    s3: SocketAddr,
}

/// The file, with the `s3` interface on `s3` and the cold store at `cold`.
fn config(s3: SocketAddr, cold: SocketAddr) -> String {
    format!(
        r#"# A farm build node's file, in shape (see the test's tierstore.rs).
[[interface]]
type   = "s3"
params = {{ addresses = ["{s3}"] }}

[[interface]]
type   = "mount-provider"
params = {{ socket = "{SOCKET}", mountpoint = "{MOUNT}" }}

[[interface]]
type = "fs"
[interface.params]
mountpoint   = "{MOUNT}"
client_group = "tierstore"
owner        = {{ subids_of = "jsjob", id = 1000 }}
[interface.params.put]
enabled   = true
prefixes  = ["cas/b/"]
stage_dir = "{CACHE}/put-stage"

[server]
handoff_socket = "/var/lib/tierstore/handoff.sock"
proxy_id_from_facts = {{ path = "/var/lib/hostkeeper/facts.json", prefix = "tierstore-" }}
bucket = "{BUCKET}"
access_key_file = "/etc/tierstore/access.key"

[local_cache]
path = "{CACHE}"
layout = "digest-v1"
budget = "4GiB"
write_through = true

[cold_store]
kind = "s3"
bucket = "{BUCKET}"
endpoint = "http://{cold}"
credentials_file = "/etc/tierstore/cold.credentials"

[[policy]]
prefix = "cas/"
cache = true

[[policy]]
prefix = ""
cache = false

[trees]
max_placements = {MAX_PLACEMENTS}
"#
    )
}

impl Tierstore {
    /// Stops any instance an earlier test left, forgets its cache, writes the file with the `s3`
    /// interface on `s3` and the cold store at `cold` (reached with `credentials`), starts the
    /// unit and waits until it is ready.
    pub fn start(s3: SocketAddr, cold: SocketAddr, credentials: &Credentials) -> Tierstore {
        stop(TIERSTORE_UNIT);
        let _ = std::fs::remove_dir_all(CACHE);
        make_dir(Path::new(CACHE), 0o700);
        chown(Path::new(CACHE), "tierstore:tierstore");
        put(Path::new(CONFIG), &config(s3, cold), 0o640);
        chown(Path::new(CONFIG), "root:tierstore");
        let key = |name: &str, text: &str| {
            let path = Path::new("/etc/tierstore").join(name);
            put(&path, text, 0o600);
            chown(&path, "tierstore:tierstore");
        };
        key(
            "access.key",
            "blackboxtestkey00001\nblackbox-test-secret-0123456789abcdefghij\n",
        );
        key(
            "cold.credentials",
            &format!("{}\n{}\n", credentials.key_id, credentials.secret),
        );
        run(
            "/usr/libexec/tierstore/tierstore",
            &["--config", CONFIG, "--check-config"],
        );
        make_dir(Path::new("/var/lib/tierstore"), 0o700);
        chown(Path::new("/var/lib/tierstore"), "tierstore:tierstore");
        put(Path::new(MARKER), "", 0o644);
        start(TIERSTORE_UNIT);
        let tierstore = Tierstore { s3 };
        tierstore.wait_ready();
        tierstore
    }

    /// Restarts the unit, as an operator or an update does (`systemctl restart`: a drain, then
    /// a new process), and waits until it is ready again.
    pub fn restart(&self) {
        run("systemctl", &["restart", TIERSTORE_UNIT]);
        self.wait_ready();
    }

    /// Waits until `/readyz` answers 200 and the provider's socket is there.
    fn wait_ready(&self) {
        let limit = Instant::now() + READY_LIMIT;
        loop {
            let ready = http::get(self.s3, "/readyz").is_ok_and(|answer| answer.status == 200);
            if ready && Path::new(SOCKET).exists() {
                return;
            }
            assert!(
                Instant::now() < limit,
                "tierstore was not ready within {} s:\n{}",
                READY_LIMIT.as_secs(),
                crate::host::journal()
            );
            std::thread::sleep(Duration::from_millis(500));
        }
    }

    /// The calls tierstore's provider has answered unavailable since it started, its own count
    /// (`tierstore_provider_unavailable_total`, every reason).
    pub fn unavailable_answers(&self) -> u64 {
        let answer = http::get(self.s3, "/metricz").unwrap_or_else(|e| panic!("/metricz: {e}"));
        assert_eq!(answer.status, 200, "/metricz: {}", answer.text());
        sum("tierstore_provider_unavailable_total", &answer.text())
    }
}

/// The sum of the samples of the counter `name` in a Prometheus text.
fn sum(name: &str, text: &str) -> u64 {
    text.lines()
        .filter(|line| {
            line.strip_prefix(name)
                .is_some_and(|rest| rest.starts_with([' ', '{']))
        })
        .filter_map(|line| line.rsplit(' ').next()?.parse::<f64>().ok())
        .map(|value| value as u64)
        .sum()
}
