//! js-agent as a farm Linux node runs it: the package's unit `js-agent-crun`, which reads
//! `/etc/js-agent/flags`, with the files hostkeeper and the node's config package put beside it.
//!
//! The flags mirror the farm's committed Linux flags file (the HostGroup of the Linux build
//! nodes): the same flags with the same kinds of value, the test's in place of the farm's names,
//! addresses and keys. Each line below is one of the farm's, in its order:
//!
//! - `--server`, the lease server by a host name and port (here `localhost`);
//! - `--pool`, `--host-id-from-facts` (hostkeeper's facts file), `--ca-cert` (the server's CA),
//!   `--host-cert` and `--host-key` (hostkeeper's copy of the host identity);
//! - `--image-allow` of two registry prefixes;
//! - `--egress=allow-list` and one `--egress-allow` of the node's tierstore S3 address;
//! - `--mount-root-ro=trees=` and `--mount-root=put=` (tierstore's mount's trees and put roots);
//! - `--mount-provider=tierstore=` (here the counting proxy's socket, which passes every call
//!   on to tierstore's own), `--mount-provider-user=tierstore=tierstore` and
//!   `--mount-root-ro=tierstore-fs=` (tierstore's mount);
//! - `--cpu-series`, `--host-second-samples` and `--memory-series`.
//!
//! One line is the test's alone: `--image-allow=auth.docker.io/token`. The jobs run busybox from Docker
//! Hub (`docker.io/library/`, a prefix the farm allows too), whose pull tokens come from
//! `auth.docker.io`, and the agent asks a token host only when it is the registry's own or that
//! of another allowed prefix; the farm's jobs pull from `ghcr.io/komira-ai/`, which issues its
//! own tokens and has no public image with a shell.
//!
//! No `--mount-provider-wait`: the farm takes the default.

use std::net::SocketAddr;
use std::path::Path;

use crate::host::put;
use crate::identity::{FACTS_FILE, IDENTITY_DIR};
use crate::tierstore::MOUNT;

/// The flags file the unit reads.
const FLAGS_FILE: &str = "/etc/js-agent/flags";

/// The token file the unit names, which an agent with a host certificate does not read.
const TOKEN_FILE: &str = "/etc/js-agent/token";

/// The lease server's CA, beside the flags.
const CA_FILE: &str = "/etc/js-agent/ca.pem";

/// The socket the agent reaches tierstore's provider on: the counting proxy's.
pub const PROVIDER_SOCKET: &str = "/run/js-tierstore-blackbox/provider.sock";

/// Writes the flags for the lease server at `server` (whose certificate is `server_cert`, PEM)
/// and the node's tierstore S3 address `s3`, and the files the unit names.
pub fn write(server: &str, server_cert: &str, pool: &str, s3: SocketAddr) {
    put(Path::new(CA_FILE), server_cert, 0o644);
    put(
        Path::new(TOKEN_FILE),
        "unread-with-a-host-certificate-0123456789\n",
        0o600,
    );
    let lines = [
        "# js-agent flags in the shape of the farm's Linux nodes' (see the test's agent.rs)."
            .to_owned(),
        format!("--server={server}"),
        format!("--pool={pool}"),
        format!("--host-id-from-facts={FACTS_FILE}"),
        format!("--ca-cert={CA_FILE}"),
        format!("--host-cert={IDENTITY_DIR}/host.crt"),
        format!("--host-key={IDENTITY_DIR}/host.key"),
        "--image-allow=ghcr.io/komira-ai/".to_owned(),
        "--image-allow=docker.io/library/".to_owned(),
        // Not the farm's: the token host of the job image's registry (see the module's docs).
        "--image-allow=auth.docker.io/token".to_owned(),
        "--egress=allow-list".to_owned(),
        format!("--egress-allow={s3}"),
        format!("--mount-root-ro=trees={MOUNT}/trees"),
        format!("--mount-root=put={MOUNT}/put"),
        format!("--mount-provider=tierstore={PROVIDER_SOCKET}"),
        "--mount-provider-user=tierstore=tierstore".to_owned(),
        format!("--mount-root-ro=tierstore-fs={MOUNT}"),
        "--cpu-series".to_owned(),
        "--host-second-samples".to_owned(),
        "--memory-series".to_owned(),
        String::new(),
    ];
    put(Path::new(FLAGS_FILE), &lines.join("\n"), 0o644);
}
