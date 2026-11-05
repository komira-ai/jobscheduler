//! Compiles the `.proto` files under `proto/` and writes their Rust to `OUT_DIR`, where
//! `src/lib.rs` and `src/refusal.rs` include it.
//!
//! protox, a compiler written in Rust, parses the files into descriptor sets, so no
//! compiler binary is installed or run, and tonic-prost-build turns each set into Rust.
//!
//! - Package `jobscheduler.v1`, the job service: its descriptor set is also written out
//!   whole (with the comments, which become the rustdoc of the generated items, and with
//!   the files it imports, among them `google/api/annotations.proto` and
//!   `google/api/http.proto` for the `google.api.http` option of every method) for
//!   `FILE_DESCRIPTOR_SET`. The Rust of the imported files is written to `OUT_DIR` too and
//!   is not included anywhere. The messages, and the client and server traits of
//!   `JobService`: the client is generated without tonic's transport, so it cannot
//!   connect by itself, and the server trait has a default body for every method that
//!   answers `UNIMPLEMENTED`. `JobFile` gets no generated `Debug`, which would print its
//!   secret contents.
//! - Package `jobscheduler.rpc`, the wire form of `google.rpc.Status` that refusals are
//!   carried in. It is compiled on its own so that it is not in the service's descriptor
//!   set; `google.protobuf.Any` is `prost_types::Any`.
//!
//! Map fields are `BTreeMap`s, so a map encodes in key order.

use std::error::Error;
use std::path::{Path, PathBuf};

/// The directory the `.proto` files are imported from.
const INCLUDE: &str = "proto";

/// The files of package `jobscheduler.v1`, from [`INCLUDE`].
const SERVICE_FILES: [&str; 4] = [
    "jobscheduler/v1/job.proto",
    "jobscheduler/v1/fleet.proto",
    "jobscheduler/v1/usage.proto",
    "jobscheduler/v1/service.proto",
];

/// The file of package `jobscheduler.rpc`, from [`INCLUDE`].
const STATUS_FILE: &str = "jobscheduler/rpc/status.proto";

/// The message whose generated `Debug` is left out, because it would print secret file
/// contents; `src/lib.rs` writes one that prints the name and the length only.
const JOB_FILE: &str = ".jobscheduler.v1.JobFile";

/// The file in `OUT_DIR` that holds the encoded descriptor set of the service.
const DESCRIPTOR_SET: &str = "jobscheduler_v1_descriptor_set.bin";

fn main() -> Result<(), Box<dyn Error>> {
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-changed={INCLUDE}");
    let out = PathBuf::from(std::env::var_os("OUT_DIR").ok_or("OUT_DIR is not set")?);

    let mut service = protox::Compiler::new([INCLUDE])?;
    service.include_source_info(true).include_imports(true);
    service.open_files(SERVICE_FILES)?;
    std::fs::write(
        out.join(DESCRIPTOR_SET),
        service.encode_file_descriptor_set(),
    )?;
    builder(&out)
        .generate_default_stubs(true)
        .skip_debug([JOB_FILE])
        .compile_fds(service.file_descriptor_set())?;

    let mut status = protox::Compiler::new([INCLUDE])?;
    status.include_source_info(true);
    status.open_files([STATUS_FILE])?;
    builder(&out)
        .build_client(false)
        .build_server(false)
        .compile_fds(status.file_descriptor_set())?;
    Ok(())
}

/// The generator settings both packages share.
fn builder(out: &Path) -> tonic_prost_build::Builder {
    tonic_prost_build::configure()
        .build_transport(false)
        .btree_map(".")
        .out_dir(out)
}
