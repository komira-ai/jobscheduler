//! No key, token, assertion or payload reaches a debug text, an error or a log line.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use js_elastic::{AcquireId, SchedulerId};
use js_logstore::TransportError;
use js_provider_gce::{
    ApiError, CredentialError, Credentials, GceApi, HostMaintenance, ImagePath, InsertRequest,
    InstanceKey, InstanceName, InstanceRef, ListFilter, MachineTypeName, NetworkInterface,
    OperationRef, Payload, Project, ProvisioningModel, RequestId, Scheduling, ServiceAccountKey,
    Zone,
};
use tracing_subscriber::fmt::MakeWriter;

use crate::script::{TOKEN, answer, clock, error_body, over, over_with, status};

/// The payload of the insert: a made-up join token.
const PAYLOAD: &str = "TOPSECRETPAYLOAD0123456789";

/// The access token the service account's exchange answers.
const SERVICE_TOKEN: &str = "TOPSECRETSERVICETOKEN0123456789";

/// The access token the metadata server answers in a malformed answer.
const METADATA_TOKEN: &str = "TOPSECRETMETADATATOKEN0123456789";

/// A writer into a buffer the test reads back.
#[derive(Clone, Default)]
struct Lines(Arc<Mutex<Vec<u8>>>);

impl std::io::Write for Lines {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(bytes);
        Ok(bytes.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl<'a> MakeWriter<'a> for Lines {
    type Writer = Self;

    fn make_writer(&'a self) -> Self {
        self.clone()
    }
}

fn fixture() -> Vec<u8> {
    std::fs::read(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/tests/fixtures/credential/test-key.json"
    ))
    .unwrap()
}

/// A stretch of the fixture's private key, in the form it has in the key file.
fn key_material() -> String {
    let value: serde_json::Value = serde_json::from_slice(&fixture()).unwrap();
    let pem = value["private_key"].as_str().unwrap();
    pem.lines().nth(3).unwrap().to_owned()
}

fn insert_request() -> InsertRequest {
    InsertRequest {
        project: Project::new("sample-project").unwrap(),
        zone: Zone::new("region-one-a").unwrap(),
        name: InstanceName::new("js-vm-one").unwrap(),
        machine_type: MachineTypeName("q9-standard-2".to_owned()),
        scheduling: Scheduling {
            provisioning_model: ProvisioningModel::Standard,
            on_host_maintenance: HostMaintenance::Terminate,
            automatic_restart: false,
            termination_action: None,
            max_run_duration: Some(Duration::from_secs(60)),
        },
        image: ImagePath::new("projects/image-project/global/images/js-agent-x86").unwrap(),
        disk_size_gb: None,
        accelerators: None,
        labels: Vec::new(),
        payload: Payload::new(PAYLOAD.to_owned()),
        network: NetworkInterface {
            network: "global/networks/default".to_owned(),
            subnetwork: "regions/region-one/subnetworks/default".to_owned(),
        },
        request_id: RequestId::derive(
            &AcquireId::new("acquire-1").unwrap(),
            &SchedulerId::new("scheduler-1").unwrap(),
        ),
    }
}

/// Asserts that the client, the credentials, the key, every kind of `CredentialError` and
/// `ApiError` (printed with `{:?}` and `{}`), the errors the client returns for a success,
/// a denial, an unreadable answer, a timeout, a rejected exchange and a malformed token,
/// the sent requests, the insert request, and everything logged at TRACE while those
/// calls run, hold none of the private key, the access tokens, the assertion and its
/// signature, or the boot payload. It catches a debug text, an error or a log line that
/// prints a key, a token or the payload.
#[test]
fn credentials_never_appear_in_debug_or_logs() {
    let lines = Lines::default();
    let subscriber = tracing_subscriber::fmt()
        .with_ansi(false)
        .with_max_level(tracing::Level::TRACE)
        .with_writer(lines.clone())
        .finish();
    let exchange = format!(
        "{{\"access_token\":\"{SERVICE_TOKEN}\",\"expires_in\":3600,\"token_type\":\"Bearer\"}}"
    );
    let operation =
        "{\"name\":\"operation-one\",\"zone\":\"zones/region-one-a\",\"status\":\"PENDING\"}";
    let echoing = format!("{{\"error\":{{\"message\":\"{PAYLOAD} {SERVICE_TOKEN}\"}}}}");
    let key = ServiceAccountKey::from_json(&fixture()).unwrap();
    let (service, service_script) = over_with(
        Credentials::ServiceAccount(key),
        vec![
            answer(&exchange),
            answer(operation),
            status(403, &error_body(403, "forbidden")),
            status(500, &echoing),
            Err(TransportError::Timeout),
            answer(&format!("{PAYLOAD} {SERVICE_TOKEN} is not json")),
        ],
        clock(),
    );
    let (metadata, metadata_script) = over(vec![status(401, &echoing)]);
    let (malformed, malformed_script) = over_with(
        Credentials::MetadataServer,
        vec![answer(&format!(
            "{{\"access_token\":\"{METADATA_TOKEN}\"}}"
        ))],
        clock(),
    );
    let (rejected_key, rejected_script) = over_with(
        Credentials::ServiceAccount(ServiceAccountKey::from_json(&fixture()).unwrap()),
        vec![status(400, &echoing)],
        clock(),
    );
    let instance = InstanceRef {
        project: Project::new("sample-project").unwrap(),
        zone: Zone::new("region-one-a").unwrap(),
        instance: InstanceKey::Name(InstanceName::new("js-vm-one").unwrap()),
    };
    let filter = ListFilter {
        project: Project::new("sample-project").unwrap(),
        labels: Vec::new(),
        page_token: None,
    };
    let reference = OperationRef {
        project: Project::new("sample-project").unwrap(),
        zone: Zone::new("region-one-a").unwrap(),
        name: "operation-one".to_owned(),
    };

    let mut shown = Vec::new();
    tracing::subscriber::with_default(subscriber, || {
        let results = [
            service.insert_instance(&insert_request()).map(|_| ()),
            service.get_instance(&instance).map(|_| ()),
            service.delete_instance(&instance).map(|_| ()),
            service.get_operation(&reference).map(|_| ()),
            service.list_instances(&filter).map(|_| ()),
            metadata.get_instance(&instance).map(|_| ()),
            malformed.get_instance(&instance).map(|_| ()),
            rejected_key.get_instance(&instance).map(|_| ()),
        ];
        // The insert succeeds; every other call fails in its own way.
        assert!(results[0].is_ok());
        for error in results.iter().filter_map(|result| result.as_ref().err()) {
            shown.push(format!("{error:?} {error}"));
        }
    });
    shown.push(format!("{service:?} {metadata:?} {malformed:?}"));
    shown.push(format!("{:?}", insert_request()));
    let key = ServiceAccountKey::from_json(&fixture()).unwrap();
    shown.push(format!("{key:?}"));
    shown.push(format!("{:?}", Credentials::ServiceAccount(key)));
    shown.push(format!("{:?}", Credentials::MetadataServer));
    let every_error = [
        ApiError::answered("rateLimitExceeded"),
        ApiError::answered("forbidden"),
        ApiError::answered("notFound"),
        ApiError::Transport("the connection failed".to_owned()),
        ApiError::Unreadable("a field is missing".to_owned()),
        ApiError::Incomplete,
        ApiError::Timeout,
        ApiError::NoCredential,
    ];
    for error in &every_error {
        shown.push(format!("{error:?} {error}"));
    }
    let every_credential_error = [
        CredentialError::Read("no such file".to_owned()),
        CredentialError::NotJson,
        CredentialError::NotServiceAccount,
        CredentialError::Missing("client_email"),
        CredentialError::BadPrivateKey,
    ];
    for error in &every_credential_error {
        shown.push(format!("{error:?} {error}"));
    }
    let scripts = [
        &service_script,
        &metadata_script,
        &malformed_script,
        &rejected_script,
    ];
    for script in scripts {
        for request in script.sent() {
            shown.push(format!("{request:?}"));
        }
    }

    let body = String::from_utf8(service_script.sent()[0].body.clone()).unwrap();
    let assertion = body
        .strip_prefix("assertion=")
        .unwrap()
        .split('&')
        .next()
        .unwrap()
        .to_owned();
    let signature = assertion.rsplit('.').next().unwrap().to_owned();
    assert!(signature.len() > 100, "the assertion was signed");
    let logged = String::from_utf8(lines.0.lock().unwrap().clone()).unwrap();
    assert!(
        logged.contains("the cloud answered"),
        "the calls were logged"
    );
    shown.push(logged);
    let secrets = [
        key_material(),
        PAYLOAD.to_owned(),
        SERVICE_TOKEN.to_owned(),
        METADATA_TOKEN.to_owned(),
        TOKEN.to_owned(),
        assertion,
        signature,
    ];
    for text in &shown {
        for secret in &secrets {
            assert!(
                !text.contains(secret.as_str()),
                "{secret} appears in {text}"
            );
        }
    }
}
