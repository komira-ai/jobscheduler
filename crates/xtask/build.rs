//! Compiles the vendored Remote Execution API and the googleapis files it uses
//! (`proto/third_party`, pinned in its `PINS`) into Rust in `OUT_DIR`, which
//! `src/mac_farm/proto.rs` includes: the messages, and the clients and servers of the
//! Execution, ContentAddressableStorage and ByteStream services. `mac-farm` is their client;
//! the server traits are for the fake farm of its tests.
//!
//! protox, a compiler written in Rust, parses the files, so no `protoc` is installed or run.
//! Map fields are `BTreeMap`s, so a message encodes the same bytes every time.

use std::error::Error;
use std::path::PathBuf;

/// The files to generate code for; their imports are generated too, and the well-known
/// `google.protobuf` types are `prost-types`.
const FILES: [&str; 4] = [
    "build/bazel/remote/execution/v2/remote_execution.proto",
    "google/bytestream/bytestream.proto",
    "google/longrunning/operations.proto",
    "google/rpc/error_details.proto",
];

fn main() -> Result<(), Box<dyn Error>> {
    let root = PathBuf::from(std::env::var_os("CARGO_MANIFEST_DIR").ok_or("no manifest dir")?)
        .join("proto/third_party");
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-changed={}", root.display());
    let descriptors = protox::compile(FILES, [root.join("remote-apis"), root.join("googleapis")])?;
    tonic_prost_build::configure()
        .build_client(true)
        .build_server(true)
        .btree_map(".")
        .compile_fds(descriptors)?;
    Ok(())
}
