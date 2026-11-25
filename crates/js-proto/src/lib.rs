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
//! implements it: the calls of a group it does not serve yet answer `UNIMPLEMENTED`, and
//! nothing mounts it yet.
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
//!
//! [`provider::v1`] is a second, separate package: `jobscheduler.provider.v1`
//! (`proto/jobscheduler/provider/v1/provider.proto`), the service
//! [`provider::v1::mount_provider_server::MountProvider`] a node-local mount provider
//! serves to the agent of its host over a Unix socket (`docs/design/mount-providers.md`),
//! with its client [`provider::v1::mount_provider_client::MountProviderClient`]. It is not in
//! [`FILE_DESCRIPTOR_SET`] and grows by additions alone, as the job service does.
//!
//! [`request_need`] converts a job's needs and constraints, with its optional accelerator,
//! into the booking and the `accelerator.*` capability keys, through `js_caps`.

use std::fmt;

pub mod refusal;

/// The messages, enums, client and server of package `jobscheduler.v1`, generated from the
/// `.proto` files by the build script. Every message, field and enum value carries the
/// comment of its `.proto` file as its documentation.
pub mod v1 {
    include!(concat!(env!("OUT_DIR"), "/jobscheduler.v1.rs"));
}

/// The service a node-local mount provider serves to the agent of its host.
pub mod provider {
    /// The messages, enum, client and server of package `jobscheduler.provider.v1`,
    /// generated from `provider.proto` by the build script, each documented by the comment
    /// of its `.proto` file.
    pub mod v1 {
        include!(concat!(env!("OUT_DIR"), "/jobscheduler.provider.v1.rs"));
    }
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

/// A callback's signing key is secret (whoever holds it can sign calls the receiver trusts),
/// so `Debug` prints the URL's scheme and host, the length of the key and the deadline, and
/// nothing else: no log line or panic message that prints a job leaks the key, nor the URL's
/// path or query, which may carry the receiver's own reference.
impl fmt::Debug for v1::Callback {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Callback")
            .field("url_origin", &url_origin(&self.url))
            .field("signing_key_len", &self.signing_key.len())
            .field("deadline_seconds", &self.deadline_seconds)
            .finish_non_exhaustive()
    }
}

/// The scheme and the host (with its port) of `url`: the text before its path, query or
/// fragment, with any userinfo left out. A text that does not start with a scheme (a letter,
/// then letters, digits, `+`, `-` or `.`) followed by `://` gives the empty text, since
/// nothing in it is known to be a host: a `://` later in the text, in a path or a query, is
/// not taken for the end of a scheme.
fn url_origin(url: &str) -> String {
    let Some((scheme, rest)) = url.split_once("://") else {
        return String::new();
    };
    let mut letters = scheme.bytes();
    let starts = letters
        .next()
        .is_some_and(|first| first.is_ascii_alphabetic());
    let rest_ok = letters.all(|byte| byte.is_ascii_alphanumeric() || b"+-.".contains(&byte));
    if !(starts && rest_ok) {
        return String::new();
    }
    let authority = rest.split(['/', '?', '#']).next().unwrap_or_default();
    let host = authority.rsplit('@').next().unwrap_or_default();
    format!("{scheme}://{host}")
}

/// The booking and the capability keys a job's needs and constraints state: the one
/// conversion of `js_caps::request_need`, called with what the lease body passes it, the
/// GPU count, the class when it is not empty, and the accelerator object field by field.
/// A job that sets only `gpus` and `gpu_class` converts as the pair always did.
///
/// # Errors
///
/// [`js_caps::RequestNeedError`] when a field of the accelerator is not valid, when it asks
/// for a count and names no model, or when it disagrees with the pair.
pub fn request_need(
    needs: &v1::Needs,
    constraints: Option<&v1::Constraints>,
) -> Result<js_caps::RequestNeed, js_caps::RequestNeedError> {
    let class = (!needs.gpu_class.is_empty()).then_some(needs.gpu_class.as_str());
    let object = constraints
        .and_then(|constraints| constraints.accelerator.as_ref())
        .map(accelerator_request);
    js_caps::request_need(u64::from(needs.gpus), class, object.as_ref())
}

/// The request object `js_caps` converts, field by field from the message.
fn accelerator_request(accelerator: &v1::Accelerator) -> js_caps::AcceleratorRequest {
    js_caps::AcceleratorRequest {
        vendor: accelerator.vendor.clone(),
        model: accelerator.model.clone(),
        count: accelerator.count,
        memory_per_device_gib: accelerator.memory_per_device_gib,
        interconnect: accelerator.interconnect.clone(),
        form_factor: accelerator.form_factor.clone(),
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
    use super::v1::{Callback, JobFile, JobSpec, SubmitJobRequest};

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
                r#"network: Unspecified, ports: [], host_ports: [], files: [JobFile { "#,
                r#"name: "upload-url", contents_len: 16, .. }], priority: 0, labels: {}, "#,
                r#"history_key: "", layers: [], mounts: [], retry: None, callback: None }) }"#,
            )
        );
    }

    /// A callback whose key is 32 bytes and whose URL is `url`.
    fn secret_callback(url: &str) -> Callback {
        Callback {
            url: url.to_owned(),
            signing_key: b"k3y-k3y-k3y-k3y-k3y-k3y-k3y-k3y!".to_vec(),
            deadline_seconds: Some(60),
        }
    }

    /// Asserts the whole `Debug` text of a callback and of a submit that carries one: the
    /// URL's scheme and host with its port, the key's length and the deadline, never the key,
    /// nor the URL's userinfo, path, query or fragment. A text that does not start with a scheme
    /// and `://` (none at all, one only inside its query, an empty scheme, one starting with a
    /// digit) shows no part of itself. Catches the generated `Debug`, which prints the key and the whole URL, coming
    /// back, and an origin that keeps the userinfo or the receiver's reference.
    #[test]
    fn debug_of_a_callback_shows_the_origin_and_key_length_only() {
        let cases = [
            (
                "https://hooks.example:8443/js-done/ref-7?x=1#f",
                "https://hooks.example:8443",
            ),
            ("https://user:pw@hooks.example/p", "https://hooks.example"),
            ("https://hooks.example", "https://hooks.example"),
            ("https://hooks.example?ref=7", "https://hooks.example"),
            ("hooks.example/js-done/ref-7", ""),
            ("hooks.example/js-done/ref-7?next=https://x", ""),
            ("user:pw@h.example/p?u=https://x", ""),
            ("://h.example/ref-7", ""),
            ("1https://h.example/ref-7", ""),
        ];
        for (url, origin) in cases {
            assert_eq!(
                format!("{:?}", secret_callback(url)),
                format!(
                    "Callback {{ url_origin: {origin:?}, signing_key_len: 32, \
                     deadline_seconds: Some(60), .. }}"
                ),
                "{url}"
            );
        }
        let request = SubmitJobRequest {
            spec: Some(JobSpec {
                callback: Some(secret_callback("https://h.example/ref")),
                ..JobSpec::default()
            }),
        };
        assert_eq!(
            format!("{request:?}"),
            concat!(
                r#"SubmitJobRequest { spec: Some(JobSpec { job_id: "", image: "", command: "", "#,
                r#"args: [], env: {}, needs: None, constraints: None, wall_limit_seconds: None, "#,
                r#"network: Unspecified, ports: [], host_ports: [], files: [], priority: 0, "#,
                r#"labels: {}, history_key: "", layers: [], mounts: [], retry: None, "#,
                r#"callback: Some(Callback { url_origin: "https://h.example", "#,
                r#"signing_key_len: 32, deadline_seconds: Some(60), .. }) }) }"#,
            )
        );
    }
}
