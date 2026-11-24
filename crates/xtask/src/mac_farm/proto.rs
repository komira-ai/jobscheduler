//! The Remote Execution API v2 and the googleapis packages it uses, as `build.rs` generates them
//! from the vendored files of `proto/third_party` (their licences and upstream commits are in its
//! `PINS`). Modules mirror the proto packages, because generated code names other packages by
//! relative paths (`super::super::...`).

/// `build.bazel.*`: the Remote Execution API and its semantic version message.
#[allow(
    missing_docs,
    clippy::all,
    clippy::pedantic,
    reason = "generated from the vendored .proto files, whose comments are not on every item"
)]
pub mod build {
    /// `build.bazel.*`.
    pub mod bazel {
        /// `build.bazel.remote.*`.
        pub mod remote {
            /// `build.bazel.remote.execution.*`.
            pub mod execution {
                /// `build.bazel.remote.execution.v2`: the Remote Execution API.
                pub mod v2 {
                    tonic::include_proto!("build.bazel.remote.execution.v2");
                }
            }
        }
        /// `build.bazel.semver`: the API's version message.
        pub mod semver {
            tonic::include_proto!("build.bazel.semver");
        }
    }
}

/// `google.*`: the googleapis packages the Remote Execution API imports, and ByteStream.
#[allow(
    missing_docs,
    clippy::all,
    clippy::pedantic,
    reason = "generated from the vendored .proto files, whose comments are not on every item"
)]
pub mod google {
    /// `google.api`: the annotations the API's methods carry.
    pub mod api {
        tonic::include_proto!("google.api");
    }
    /// `google.bytestream`: the streamed reads and writes of large blobs.
    pub mod bytestream {
        tonic::include_proto!("google.bytestream");
    }
    /// `google.longrunning`: the operation an Execute answers with.
    pub mod longrunning {
        tonic::include_proto!("google.longrunning");
    }
    /// `google.rpc`: the status of a failed call and its details.
    pub mod rpc {
        tonic::include_proto!("google.rpc");
    }
}

/// The Remote Execution API v2, under a short name.
pub use build::bazel::remote::execution::v2 as reapi;
