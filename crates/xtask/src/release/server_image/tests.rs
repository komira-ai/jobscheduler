//! The js-lease-server image: its bytes, its layout and its refusals.

use std::collections::BTreeMap;
use std::io::Read;
use std::path::{Path, PathBuf};

use js_testkit::dir::TestDir;

use super::*;

const VERSION: Version = Version {
    major: 1,
    minor: 2,
    patch: 3,
};

/// A scratch directory of its own for one test.
fn scratch(name: &str) -> TestDir {
    TestDir::under(std::env::temp_dir(), name)
}

/// The repository root, two levels above this crate.
fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../..")
}

/// A binary that holds the text of [`VERSION`] as a program's `--version` does.
fn binary() -> Vec<u8> {
    b"\x7fELF....Serves the job scheduler\x001.2.3\x00crun".to_vec()
}

/// One entry of a tar: path, type, mode, owner ids and names, time and content.
#[derive(Debug, PartialEq, Eq)]
struct Member {
    path: String,
    directory: bool,
    mode: u32,
    ids: (u64, u64),
    names: (String, String),
    mtime: u64,
    content: Vec<u8>,
}

/// The entries of a tar.
fn read_tar(bytes: &[u8]) -> Vec<Member> {
    let mut tar = tar::Archive::new(bytes);
    tar.entries()
        .expect("entries")
        .map(|entry| {
            let mut entry = entry.expect("entry");
            let header = entry.header().clone();
            let mut content = Vec::new();
            entry.read_to_end(&mut content).expect("content");
            Member {
                path: String::from_utf8(entry.path_bytes().to_vec()).expect("path"),
                directory: header.entry_type().is_dir(),
                mode: header.mode().expect("mode"),
                ids: (header.uid().expect("uid"), header.gid().expect("gid")),
                names: (
                    header
                        .username()
                        .expect("user")
                        .unwrap_or_default()
                        .to_owned(),
                    header
                        .groupname()
                        .expect("group")
                        .unwrap_or_default()
                        .to_owned(),
                ),
                mtime: header.mtime().expect("mtime"),
                content,
            }
        })
        .collect()
}

/// The uncompressed bytes of a gzip stream.
fn gunzip(bytes: &[u8]) -> Vec<u8> {
    let mut out = Vec::new();
    flate2::read::GzDecoder::new(bytes)
        .read_to_end(&mut out)
        .expect("gunzip");
    out
}

/// The image of `binary` for `target`, built.
fn image(binary: &[u8], target: Target) -> Image {
    build(VERSION, target, binary).expect("the image builds")
}

/// A parsed JSON blob of the layout.
fn json_of(members: &[Member], path: &str) -> Value {
    let member = members.iter().find(|m| m.path == path).expect(path);
    serde_json::from_slice(&member.content).expect("json")
}

/// The blob of the layout with this `sha256:<hex>` digest.
fn blob<'a>(members: &'a [Member], digest: &str) -> &'a [u8] {
    let path = format!("blobs/sha256/{}", digest.trim_start_matches("sha256:"));
    &members
        .iter()
        .find(|m| m.path == path)
        .expect(&path)
        .content
}

/// Asserts that the same inputs give the same bytes, and different inputs different ones: the
/// image carries no time, no name and no host detail of its own.
#[test]
fn the_image_is_the_same_bytes_for_the_same_inputs() {
    let bin = binary();
    let first = image(&bin, Target::X86_64);
    assert_eq!(first, image(&bin, Target::X86_64));
    assert_ne!(first, image(&bin, Target::Aarch64));
    let mut other = bin.clone();
    other.push(b'.');
    assert_ne!(first, image(&other, Target::X86_64));
}

/// Asserts the layout as a loader reads it: `oci-layout` 1.0.0, an index of one manifest
/// descriptor with the platform and no name annotation, a manifest whose config and layer
/// descriptors give the digest and size of the blobs, the digest the image is deployed by being
/// the manifest's, and no blob that nothing names.
#[test]
fn the_layout_is_consistent_from_the_index_to_the_blobs() {
    let built = image(&binary(), Target::Aarch64);
    let members = read_tar(&built.tar);
    assert_eq!(
        json_of(&members, "oci-layout"),
        json!({"imageLayoutVersion": "1.0.0"})
    );

    let index = json_of(&members, "index.json");
    assert_eq!(index["schemaVersion"], 2);
    assert_eq!(
        index["mediaType"],
        "application/vnd.oci.image.index.v1+json"
    );
    let manifests = index["manifests"].as_array().expect("manifests");
    assert_eq!(manifests.len(), 1);
    let entry = &manifests[0];
    assert_eq!(
        entry["platform"],
        json!({"architecture": "arm64", "os": "linux"})
    );
    assert!(entry.get("annotations").is_none(), "named by digest only");
    assert_eq!(entry["digest"], format!("sha256:{}", built.manifest_digest));
    let manifest_bytes = blob(&members, entry["digest"].as_str().expect("digest"));
    assert_eq!(entry["size"], manifest_bytes.len());
    assert_eq!(
        entry["mediaType"],
        "application/vnd.oci.image.manifest.v1+json"
    );
    assert_eq!(sha256_hex(manifest_bytes), built.manifest_digest);

    let manifest: Value = serde_json::from_slice(manifest_bytes).expect("manifest");
    assert_eq!(manifest["schemaVersion"], 2);
    assert_eq!(
        manifest["mediaType"],
        "application/vnd.oci.image.manifest.v1+json"
    );
    let config = &manifest["config"];
    assert_eq!(
        config["mediaType"],
        "application/vnd.oci.image.config.v1+json"
    );
    let config_bytes = blob(&members, config["digest"].as_str().expect("digest"));
    assert_eq!(config["size"], config_bytes.len());
    assert_eq!(
        sha256_hex(config_bytes),
        config["digest"].as_str().expect("d")[7..]
    );
    let layers = manifest["layers"].as_array().expect("layers");
    assert_eq!(layers.len(), 1);
    assert_eq!(
        layers[0]["mediaType"],
        "application/vnd.oci.image.layer.v1.tar+gzip"
    );
    let layer = blob(&members, layers[0]["digest"].as_str().expect("digest"));
    assert_eq!(layers[0]["size"], layer.len());
    assert_eq!(
        sha256_hex(layer),
        layers[0]["digest"].as_str().expect("d")[7..]
    );

    // The config's diff id is the digest of the uncompressed layer.
    let config: Value = serde_json::from_slice(config_bytes).expect("config");
    assert_eq!(
        config["rootfs"],
        json!({"type": "layers", "diff_ids": [format!("sha256:{}", sha256_hex(&gunzip(layer)))]})
    );

    // The files of the layout are these and the three blobs, with their directories.
    let paths: Vec<&str> = members.iter().map(|m| m.path.as_str()).collect();
    let expected_blobs: Vec<String> = BTreeMap::from([
        (sha256_hex(config_bytes), ()),
        (sha256_hex(layer), ()),
        (sha256_hex(manifest_bytes), ()),
    ])
    .into_keys()
    .map(|hex| format!("blobs/sha256/{hex}"))
    .collect();
    let mut expected = vec!["blobs/".to_owned(), "blobs/sha256/".to_owned()];
    expected.extend(expected_blobs);
    expected.extend(["index.json", "manifest.json", "oci-layout"].map(str::to_owned));
    assert_eq!(paths, expected);
}

/// Asserts the Docker-compatible `manifest.json`: one image whose `Config` and one `Layers` path
/// are the blobs the manifest names, with `RepoTags` null, so an engine that reads only this
/// file loads the same image.
#[test]
fn the_docker_manifest_names_the_same_config_and_layer() {
    let members = read_tar(&image(&binary(), Target::X86_64).tar);
    let index = json_of(&members, "index.json");
    let manifest: Value = serde_json::from_slice(blob(
        &members,
        index["manifests"][0]["digest"].as_str().expect("digest"),
    ))
    .expect("manifest");
    let path = |descriptor: &Value| {
        format!(
            "blobs/sha256/{}",
            descriptor["digest"].as_str().expect("digest")[7..].to_owned()
        )
    };
    assert_eq!(
        json_of(&members, "manifest.json"),
        json!([{
            "Config": path(&manifest["config"]),
            "RepoTags": null,
            "Layers": [path(&manifest["layers"][0])],
        }])
    );
}

/// Asserts the config exactly, for both targets: the architecture, `linux`, the numeric user,
/// the entrypoint with no command, `/` as the working directory, `SIGTERM`, no exposed port, no
/// volume, the three labels, and no creation time or history.
#[test]
fn the_config_runs_the_server_as_a_numeric_user_and_stops_on_sigterm() {
    for (target, architecture) in [(Target::X86_64, "amd64"), (Target::Aarch64, "arm64")] {
        let members = read_tar(&image(&binary(), target).tar);
        let index = json_of(&members, "index.json");
        let manifest: Value = serde_json::from_slice(blob(
            &members,
            index["manifests"][0]["digest"].as_str().expect("digest"),
        ))
        .expect("manifest");
        let config: Value = serde_json::from_slice(blob(
            &members,
            manifest["config"]["digest"].as_str().expect("digest"),
        ))
        .expect("config");
        let mut without_rootfs = config.clone();
        without_rootfs
            .as_object_mut()
            .expect("object")
            .remove("rootfs");
        assert_eq!(
            without_rootfs,
            json!({
                "architecture": architecture,
                "os": "linux",
                "config": {
                    "User": "65532:65532",
                    "Entrypoint": ["/usr/local/bin/js-lease-server"],
                    "WorkingDir": "/",
                    "StopSignal": "SIGTERM",
                    "Labels": {
                        "org.opencontainers.image.version": "1.2.3",
                        "org.opencontainers.image.source": "https://github.com/komira-ai/jobscheduler",
                        "org.opencontainers.image.licenses": "Apache-2.0",
                    },
                },
            })
        );
    }
}

/// Asserts that the licence label is the workspace's licence, so a change of the repository's
/// licence is a change here too.
#[test]
fn the_licence_label_is_the_workspace_licence() {
    let manifest = std::fs::read_to_string(repo_root().join("Cargo.toml")).expect("Cargo.toml");
    assert!(
        manifest.contains(&format!("license = \"{LICENSES}\"")),
        "the workspace licence is not {LICENSES}"
    );
}

/// Asserts the layer: the directories above the two files (each once, mode 0755) before what is
/// in it, the program at mode 0755 with the binary's bytes and the repository's `LICENSE` at
/// mode 0644, all owned by `root` 0:0 at the fixed time, nothing else.
#[test]
fn the_layer_holds_the_program_and_the_licence_owned_by_root() {
    let bin = binary();
    let members = read_tar(&image(&bin, Target::X86_64).tar);
    let index = json_of(&members, "index.json");
    let manifest: Value = serde_json::from_slice(blob(
        &members,
        index["manifests"][0]["digest"].as_str().expect("digest"),
    ))
    .expect("manifest");
    let layer = gunzip(blob(
        &members,
        manifest["layers"][0]["digest"].as_str().expect("digest"),
    ));
    let entries = read_tar(&layer);
    let listing: Vec<(&str, u32)> = entries.iter().map(|e| (e.path.as_str(), e.mode)).collect();
    assert_eq!(
        listing,
        [
            ("usr/", 0o755),
            ("usr/local/", 0o755),
            ("usr/local/bin/", 0o755),
            ("usr/local/bin/js-lease-server", 0o755),
            ("usr/share/", 0o755),
            ("usr/share/doc/", 0o755),
            ("usr/share/doc/js-lease-server/", 0o755),
            ("usr/share/doc/js-lease-server/LICENSE", 0o644),
        ]
    );
    for entry in &entries {
        assert_eq!(
            (entry.ids, entry.names.clone(), entry.mtime),
            (
                (0, 0),
                ("root".to_owned(), "root".to_owned()),
                u64::from(MTIME)
            ),
            "{}",
            entry.path
        );
        assert_eq!(entry.directory, entry.path.ends_with('/'), "{}", entry.path);
    }
    assert_eq!(entries[3].content, bin);
    assert_eq!(
        entries[7].content,
        std::fs::read(repo_root().join("LICENSE")).expect("LICENSE")
    );
}

/// Asserts the layout tar's own entries: every one owned by `root` 0:0 at the fixed time, the
/// directories 0755 and the files 0644.
#[test]
fn the_layout_tar_is_owned_by_root_at_the_fixed_time() {
    let members = read_tar(&image(&binary(), Target::X86_64).tar);
    for member in &members {
        assert_eq!(
            (member.ids, member.names.clone(), member.mtime),
            (
                (0, 0),
                ("root".to_owned(), "root".to_owned()),
                u64::from(MTIME)
            ),
            "{}",
            member.path
        );
        assert_eq!(
            member.mode,
            if member.directory { 0o755 } else { 0o644 },
            "{}",
            member.path
        );
    }
}

/// Asserts that the layer's gzip stream names no file and carries no time: flags and modification
/// time in the header are zero, so the same layer is the same bytes on any runner and any day.
#[test]
fn the_layer_stream_has_no_name_and_no_time() {
    let members = read_tar(&image(&binary(), Target::X86_64).tar);
    let index = json_of(&members, "index.json");
    let manifest: Value = serde_json::from_slice(blob(
        &members,
        index["manifests"][0]["digest"].as_str().expect("digest"),
    ))
    .expect("manifest");
    let layer = blob(
        &members,
        manifest["layers"][0]["digest"].as_str().expect("digest"),
    );
    assert_eq!(&layer[..2], &[0x1f, 0x8b]);
    assert_eq!(layer[3], 0, "no flags: no file name");
    assert_eq!(&layer[4..8], &[0, 0, 0, 0], "no time");
}

/// Asserts the version test: the version as a whole (at either end of the bytes, between
/// non-digits), and not as the end of a longer number, the start of a longer one or the tail of a
/// dotted run.
#[test]
fn a_version_is_held_only_as_a_whole() {
    let held = |haystack: &[u8]| holds_version(haystack, VERSION);
    assert!(held(b"1.2.3"));
    assert!(held(b"x\x001.2.3\x00y"));
    assert!(held(b"js-lease-server 1.2.3"));
    assert!(held(b"1.2.3 "));
    assert!(held(b"1.2.30 and then 1.2.3"));
    assert!(!held(b""));
    assert!(!held(b"1.2"));
    assert!(!held(b"1.2.30"));
    assert!(!held(b"11.2.3"));
    assert!(!held(b"0.1.2.3"));
    assert!(!held(b"1.2.4 1.2.2"));
}

/// Asserts the targets' spellings, architectures and the file names.
#[test]
fn a_target_has_its_spelling_architecture_and_file_name() {
    let all: Vec<(&str, &str)> = Target::ALL
        .iter()
        .map(|t| (t.name(), t.architecture()))
        .collect();
    assert_eq!(
        all,
        [
            ("aarch64-unknown-linux-musl", "arm64"),
            ("x86_64-unknown-linux-musl", "amd64")
        ]
    );
    assert_eq!(
        file_name(VERSION, Target::X86_64),
        "js-lease-server-1.2.3-x86_64-unknown-linux-musl.oci.tar"
    );
    assert_eq!(
        file_name(VERSION, Target::Aarch64),
        "js-lease-server-1.2.3-aarch64-unknown-linux-musl.oci.tar"
    );
}

/// The arguments for a run in `dir`.
fn args(dir: &TestDir, bin: &str, target: Target) -> ImageArgs {
    ImageArgs {
        binary: dir.join(bin),
        version: VERSION,
        target,
        out: dir.join("out"),
    }
}

/// Asserts a run: the image is written under its name, the printed line gives the file's SHA-256
/// and size and the image's digest, and a second run into the same directory is refused and
/// leaves the first's bytes.
#[test]
fn a_run_writes_the_image_and_refuses_to_replace_it() {
    let dir = scratch("server-image-run");
    std::fs::write(dir.join("js-lease-server"), binary()).expect("binary");
    let args = args(&dir, "js-lease-server", Target::X86_64);
    let line = run(&args).expect("the run passes");
    let name = "js-lease-server-1.2.3-x86_64-unknown-linux-musl.oci.tar";
    let written = std::fs::read(dir.join("out").join(name)).expect("the image");
    let built = image(&binary(), Target::X86_64);
    assert_eq!(written, built.tar);
    assert_eq!(
        line,
        format!(
            "{name}: sha256 {}, {} bytes, image sha256:{}",
            sha256_hex(&written),
            written.len(),
            built.manifest_digest
        )
    );
    let Err(Error::Io { path, why }) = run(&args) else {
        panic!("a second run must fail");
    };
    assert!(path.ends_with(name), "{path}");
    assert!(why.contains("exists"), "{why}");
    assert_eq!(
        std::fs::read(dir.join("out").join(name)).expect("image"),
        written
    );
}

/// Asserts that a binary that does not hold the version (built as another one) is refused by
/// name, before the output directory is made, and the refusal's text.
#[test]
fn a_binary_of_another_version_is_refused_and_nothing_is_written() {
    let dir = scratch("server-image-version");
    std::fs::write(dir.join("js-lease-server"), b"x 1.2.30 y").expect("binary");
    let result = run(&args(&dir, "js-lease-server", Target::Aarch64));
    let error = Error::NotOfVersion {
        path: dir.join("js-lease-server").display().to_string(),
        version: "1.2.3".to_owned(),
    };
    assert_eq!(result, Err(error));
    assert!(!dir.join("out").exists());
    assert_eq!(
        Error::NotOfVersion {
            path: "target/release/js-lease-server".to_owned(),
            version: "1.2.3".to_owned(),
        }
        .to_string(),
        "target/release/js-lease-server does not hold the version 1.2.3: it is not a build of that version"
    );
}

/// Asserts the I/O failures: a missing binary and an output that is a file are [`Error::Io`]
/// naming the path.
#[test]
fn a_missing_binary_and_an_unmakeable_directory_are_io_errors() {
    let dir = scratch("server-image-io");
    let Err(Error::Io { path, .. }) = run(&args(&dir, "absent", Target::X86_64)) else {
        panic!("a missing binary must fail");
    };
    assert_eq!(path, dir.join("absent").display().to_string());
    std::fs::write(dir.join("js-lease-server"), binary()).expect("binary");
    std::fs::write(dir.join("out"), "a file").expect("file");
    let Err(Error::Io { path, .. }) = run(&args(&dir, "js-lease-server", Target::X86_64)) else {
        panic!("an output that is a file must fail");
    };
    assert_eq!(path, dir.join("out").display().to_string());
}

/// A sink that takes `room` bytes and then refuses every write, as a full disk does.
struct Full {
    room: usize,
}

impl Write for Full {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        if self.room == 0 {
            return Err(std::io::Error::other("the sink is full"));
        }
        let taken = bytes.len().min(self.room);
        self.room -= taken;
        Ok(taken)
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

/// Asserts that a sink that refuses the tar is an error naming what was being written: the
/// entry when the sink takes nothing, and the end of the archive when it takes the entry (a
/// header block and one block of content) and no more.
#[test]
fn a_tar_sink_that_fails_is_an_error_naming_what_was_written() {
    let entries: Vec<Entry> = vec![("a".to_owned(), 0o644, Some(b"text".to_vec()))];
    let Err(Error::Io { path, why }) = tar_to(Full { room: 0 }, &entries) else {
        panic!("a full sink must fail");
    };
    assert_eq!((path.as_str(), why.as_str()), ("a", "the sink is full"));
    let Err(Error::Io { path, why }) = tar_to(Full { room: 1024 }, &entries) else {
        panic!("a sink that fills at the end must fail");
    };
    assert_eq!(
        (path.as_str(), why.as_str()),
        ("the tar archive", "the sink is full")
    );
}

/// Asserts that a sink that refuses the gzip stream is an error naming the layer: when the sink
/// takes nothing, and when it takes the gzip header and no more (the compressed bytes are only
/// flushed at the end).
#[test]
fn a_gzip_sink_that_fails_is_an_error_naming_the_layer() {
    for room in [0, 10] {
        let Err(Error::Io { path, why }) = gzip_to(Full { room }, b"text") else {
            panic!("a sink that fills must fail");
        };
        assert_eq!(
            (path.as_str(), why.as_str()),
            ("the layer", "the sink is full")
        );
    }
}

/// Catches an image written although one of its three streams could not be made: the failure of
/// the layer's tar, of its compression and of the layout's tar each comes back as the error, and
/// nothing is made after it.
#[test]
fn a_stream_that_cannot_be_made_fails_the_image() {
    let bin = binary();
    for fail_at in 0..3 {
        let calls = std::cell::Cell::new(0);
        let step = |fails: bool, made: Vec<u8>| {
            calls.set(calls.get() + 1);
            if fails {
                Err(Error::Io {
                    path: format!("stream {}", calls.get() - 1),
                    why: "broken".to_owned(),
                })
            } else {
                Ok(made)
            }
        };
        let result = build_with(
            VERSION,
            Target::X86_64,
            &bin,
            &|entries| {
                step(
                    fail_at == calls.get(),
                    tar_to(Vec::new(), entries).expect("tar"),
                )
            },
            &|bytes| {
                step(
                    fail_at == calls.get(),
                    gzip_to(Vec::new(), bytes).expect("gzip"),
                )
            },
        );
        assert_eq!(
            result,
            Err(Error::Io {
                path: format!("stream {fail_at}"),
                why: "broken".to_owned(),
            })
        );
        assert_eq!(
            calls.get(),
            fail_at + 1,
            "nothing is made after the failure"
        );
    }
}
