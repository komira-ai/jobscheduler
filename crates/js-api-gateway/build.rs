//! Checks the `google.api.http` option of every method of `jobscheduler.v1.JobService` in
//! js-proto's descriptor set (`js_proto::FILE_DESCRIPTOR_SET`) and that `src/gateway.rs`, the
//! enum `Rpc`, the table `ROUTES` and the function `call` that `src/lib.rs` includes, is the
//! Rust `src/codegen.rs` writes for them.
//!
//! The options are read and checked by `src/annotation.rs` and the Rust is written by
//! `src/codegen.rs`, the modules the crate's tests run over test descriptors. A method whose
//! option the gateway does not serve fails the build with the method's name and the fault, a
//! service without one annotated method fails it with the service's name, and a
//! `src/gateway.rs` that is not what the options give fails it with the text it should hold.
//! The generated file is checked in, rather than written to `OUT_DIR`, so that the coverage
//! report measures it as part of the crate.

use std::error::Error;

use prost_reflect::DescriptorPool;

#[allow(
    dead_code,
    reason = "the library and its tests use items of the module the build script does not"
)]
#[path = "src/annotation.rs"]
mod annotation;

#[allow(
    dead_code,
    reason = "the library and its tests use items of the module the build script does not"
)]
#[path = "src/codegen.rs"]
mod codegen;

/// The service the gateway serves.
const SERVICE: &str = "jobscheduler.v1.JobService";

/// The checked-in generated Rust, from the package directory (a build script's working
/// directory).
const GENERATED: &str = "src/gateway.rs";

fn main() -> Result<(), Box<dyn Error>> {
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-changed=src/annotation.rs");
    println!("cargo:rerun-if-changed=src/codegen.rs");
    println!("cargo:rerun-if-changed={GENERATED}");
    let pool = DescriptorPool::decode(js_proto::FILE_DESCRIPTOR_SET)?;
    let routes = annotation::routes(&pool, SERVICE).map_err(|err| err.to_string())?;
    let expected = codegen::render(SERVICE, &routes);
    let checked_in = std::fs::read_to_string(GENERATED)?;
    if checked_in != expected {
        return Err(format!(
            "{GENERATED} is not the Rust the google.api.http options of {SERVICE} give; it \
             must hold exactly:\n{expected}"
        )
        .into());
    }
    Ok(())
}
