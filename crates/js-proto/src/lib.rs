//! The job service as protobuf: package `jobscheduler.v1`, the one definition of every call
//! a client may make (design section 3.6).
//!
//! The `.proto` files are in this crate's `proto/jobscheduler/v1/`: `job.proto` (the job,
//! its states, its result and why a submit is refused), `fleet.proto` (hosts with the
//! resources booked on them, pools, machine providers), `usage.proto` (ended bookings) and
//! `service.proto` (`service JobService`). The header of `service.proto` states how the
//! package may change: field numbers are never reused or renumbered, a shipped field is
//! never removed or given another meaning, new fields are optional and new enum values are
//! appended. The crate's schema test holds the shipped fields to it.
//!
//! [`v1`] is the Rust the build script generates from them: a struct for each message, an
//! enum for each protobuf enum, the client [`v1::job_service_client::JobServiceClient`] and
//! the server trait [`v1::job_service_server::JobService`] with its service
//! [`v1::job_service_server::JobServiceServer`]. A method a server does not implement
//! answers `UNIMPLEMENTED`. The crate neither listens nor connects: a caller brings the
//! transport. The lease server's job service (`js-lease-server`, `src/job_api/`)
//! implements it, every call answering `UNIMPLEMENTED` so far, and nothing serves it yet.
//!
//! Each method of the service carries a `google.api.http` option, its route under the
//! HTTP/JSON gateway (crate `js-api-gateway`, which generates its routes from them). The two
//! files that define the option, `google/api/annotations.proto` and `google/api/http.proto`,
//! are copied from googleapis (Apache-2.0) into `proto/google/api/`, with the long comments
//! of `http.proto` shortened and every definition unchanged.
//!
//! A refused submit is a [`tonic::Status`] carrying a [`v1::Refusal`] in its details;
//! [`refusal`] makes one and reads one back. [`FILE_DESCRIPTOR_SET`] is the encoded
//! descriptor set of the package, with its comments, for reflection and for the tests.

use std::fmt;

pub mod refusal;

/// The messages, enums, client and server of package `jobscheduler.v1`, generated from the
/// `.proto` files by the build script. Every message, field and enum value carries the
/// comment of its `.proto` file as its documentation.
pub mod v1 {
    include!(concat!(env!("OUT_DIR"), "/jobscheduler.v1.rs"));
}

/// A job file's contents are secret (they can hold presigned URLs or tokens), so `Debug`
/// prints the name and the length of the contents and nothing else, as the agent's
/// `RunFile` does: no log line or panic message that prints a job leaks them.
impl fmt::Debug for v1::JobFile {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("JobFile")
            .field("name", &self.name)
            .field("contents_len", &self.contents.len())
            .finish_non_exhaustive()
    }
}

/// The encoded `google.protobuf.FileDescriptorSet` of package `jobscheduler.v1`: its four
/// files, with their comments, and the files they import (`google/api/annotations.proto`,
/// `google/api/http.proto` and `google/protobuf/descriptor.proto`), so that a reader of the
/// set resolves the `google.api.http` option each method of the service carries.
pub const FILE_DESCRIPTOR_SET: &[u8] = include_bytes!(concat!(
    env!("OUT_DIR"),
    "/jobscheduler_v1_descriptor_set.bin"
));

#[cfg(test)]
mod tests {
    use super::v1::{JobFile, JobSpec, SubmitJobRequest};

    fn secret_file() -> JobFile {
        JobFile {
            name: "upload-url".to_owned(),
            contents: "s3cr3t-presigned".to_owned(),
        }
    }

    /// Asserts the whole `Debug` text of a job file and of a submit that carries one: the
    /// name and the length of the contents, never the contents. Catches the generated
    /// `Debug`, which prints every field, coming back.
    #[test]
    fn debug_of_a_job_file_shows_the_name_and_length_only() {
        assert_eq!(
            format!("{:?}", secret_file()),
            r#"JobFile { name: "upload-url", contents_len: 16, .. }"#
        );
        let request = SubmitJobRequest {
            spec: Some(JobSpec {
                files: vec![secret_file()],
                ..JobSpec::default()
            }),
        };
        assert_eq!(
            format!("{request:?}"),
            concat!(
                r#"SubmitJobRequest { spec: Some(JobSpec { job_id: "", image: "", command: "", "#,
                r#"args: [], env: {}, needs: None, constraints: None, wall_limit_seconds: None, "#,
                r#"network: Unspecified, ports: [], files: [JobFile { name: "upload-url", "#,
                r#"contents_len: 16, .. }], priority: 0, labels: {} }) }"#,
            )
        );
    }
}
