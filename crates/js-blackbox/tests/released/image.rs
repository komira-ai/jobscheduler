//! The job image, and the registry on the runner's loopback the agent pulls it from.
//!
//! The image is the runner's own `/usr/bin/sleep` with the libraries `ldd` says it loads, at the
//! paths `ldd` names, in one gzipped layer: a job of the image runs `/bin/sleep N` and ends 0
//! after N seconds. Nothing is pulled from the internet for it, and the agent reaches the
//! registry through its `--image-allow-loopback` entry.

use std::io::Write;

use flate2::Compression;
use flate2::write::GzEncoder;
use js_testkit::oci_registry::{Artifact, Fixture};
use sha2::{Digest, Sha256};
use tokio::runtime::Runtime;

use crate::host::run;

/// The repository of the image on the registry, under the agent's allow-list entry.
const REPOSITORY: &str = "blackbox/sleep";

/// The registry, serving on its own runtime for as long as the test process lives.
pub struct Registry {
    /// The port it listens on, on 127.0.0.1.
    pub port: u16,
    /// The job image's reference, pinned by digest.
    pub image: String,
    _fixture: Fixture,
    _runtime: Runtime,
}

impl Registry {
    /// Starts the registry and publishes the job image on it.
    pub fn start() -> Registry {
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()
            .expect("the registry's runtime");
        let fixture = runtime.block_on(Fixture::start_plain());
        let (layer, diff_id) = layer();
        let config = format!(
            r#"{{"architecture":"amd64","os":"linux","config":{{"Env":["PATH=/bin"],"Cmd":["/bin/sleep","1"]}},"rootfs":{{"type":"layers","diff_ids":["sha256:{diff_id}"]}}}}"#
        );
        let artifact = Artifact::image(vec![layer], &config);
        fixture.publish(&artifact);
        let port = fixture.port;
        Registry {
            port,
            image: format!("127.0.0.1:{port}/{REPOSITORY}@{}", artifact.digest),
            _fixture: fixture,
            _runtime: runtime,
        }
    }
}

/// The layer, gzipped, and the hex SHA-256 of its tar (the config's `diff_id`).
fn layer() -> (Vec<u8>, String) {
    let mut files = vec![("/bin/sleep".to_owned(), "/usr/bin/sleep".to_owned())];
    for library in libraries(&run("ldd", &["/usr/bin/sleep"])) {
        files.push((library.clone(), library));
    }
    let mut tar = tar::Builder::new(Vec::new());
    let mut made = std::collections::BTreeSet::new();
    for (inside, from) in &files {
        let inside = inside.trim_start_matches('/');
        // Every directory above the file, once, searchable by the job's unprivileged id.
        let mut above = String::new();
        for part in inside
            .split('/')
            .collect::<Vec<_>>()
            .split_last()
            .map_or(&[][..], |(_, d)| d)
        {
            above.push_str(part);
            above.push('/');
            if made.insert(above.clone()) {
                let mut header = tar::Header::new_gnu();
                header.set_entry_type(tar::EntryType::Directory);
                header.set_mode(0o755);
                header.set_size(0);
                tar.append_data(&mut header, &above, std::io::empty())
                    .expect("a directory of the layer");
            }
        }
        let bytes = std::fs::read(from).unwrap_or_else(|e| panic!("read {from}: {e}"));
        let mut header = tar::Header::new_gnu();
        header.set_entry_type(tar::EntryType::Regular);
        header.set_mode(0o755);
        header.set_size(bytes.len() as u64);
        tar.append_data(&mut header, inside, bytes.as_slice())
            .expect("a file of the layer");
    }
    let tar = tar.into_inner().expect("the layer's tar");
    let diff_id = hex::encode(Sha256::digest(&tar));
    let mut gz = GzEncoder::new(Vec::new(), Compression::default());
    gz.write_all(&tar).expect("gzip the layer");
    (gz.finish().expect("gzip the layer"), diff_id)
}

/// The absolute paths of the libraries `ldd` printed: the path after `=>`, or the line's
/// first word when it is a path (the program interpreter).
fn libraries(ldd: &str) -> Vec<String> {
    ldd.lines()
        .filter_map(|line| {
            let line = line.trim();
            let path = line
                .split_once("=>")
                .map_or(line, |(_, after)| after.trim());
            let path = path.split_whitespace().next()?;
            path.starts_with('/').then(|| path.to_owned())
        })
        .collect()
}

/// The flags of `released fake-registry`.
#[derive(Debug, clap::Args)]
pub struct Flags {
    /// The file to write, once the registry serves: its port on the first line, the job
    /// image's reference on the second.
    #[arg(long)]
    pub out: std::path::PathBuf,
}

/// Serves the job image until the process is stopped.
pub fn serve(flags: &Flags) {
    let registry = Registry::start();
    // Written aside and renamed, so the wrapper never reads half of it.
    let aside = flags.out.with_extension("part");
    crate::host::put(
        &aside,
        format!("{}\n{}\n", registry.port, registry.image),
        0o644,
        (0, 0),
    );
    std::fs::rename(&aside, &flags.out)
        .unwrap_or_else(|e| panic!("rename {}: {e}", aside.display()));
    eprintln!(
        "fake registry: serving {} on 127.0.0.1:{}",
        registry.image, registry.port
    );
    loop {
        std::thread::park();
    }
}
