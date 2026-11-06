//! What the tests share: a fixed clock, the fake store with its STS, a transport that records
//! what goes to the fake, a transport that answers scripted replies, and the requests.

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use js_logmint::{
    Bucket, LogPrefix, MintRequest, PresignedPrefixMinter, RoleArn, StsEndpoint, StsMinter,
};
use js_logstore::fake::FakeStore;
use js_logstore::{
    Credential, Endpoint, HttpRequest, HttpResponse, ManualClock, Store, StoreError, Transport,
    TransportError,
};

/// The time every test starts at, in seconds since the Unix epoch.
pub const START: u64 = 1_800_000_000;

/// The region of every test endpoint.
pub const REGION: &str = "us-east-1";

/// The bucket of every test.
pub const BUCKET: &str = "logs";

/// The role every test assumes.
pub const ROLE: &str = "arn:aws:iam::example:role/js-logs";

/// The base URL of the fake store and its STS.
pub const URL: &str = "https://store.example.com";

/// The lifetime every test asks for.
pub const HOUR: Duration = Duration::from_secs(3600);

/// The instant `seconds` after the Unix epoch.
pub fn at(seconds: u64) -> SystemTime {
    UNIX_EPOCH + Duration::from_secs(seconds)
}

/// The request of a credential for `host` under the log prefix `prefix`, for an hour.
pub fn request(prefix: &str, host: &str) -> MintRequest {
    MintRequest {
        bucket: Bucket::new(BUCKET).unwrap(),
        prefix: LogPrefix::new(prefix).unwrap(),
        host: host.to_owned(),
        ttl: HOUR,
    }
}

/// A transport to the fake store that keeps a copy of every request.
#[derive(Clone)]
pub struct Recording {
    pub fake: FakeStore,
    pub sent: Arc<Mutex<Vec<HttpRequest>>>,
}

impl Transport for Recording {
    fn send(&self, request: HttpRequest) -> Result<HttpResponse, TransportError> {
        self.sent.lock().unwrap().push(request.clone());
        self.fake.send(request)
    }
}

/// The fake store, the clock it shares with its clients, and a minter calling its STS.
pub struct Rig {
    pub clock: Arc<ManualClock>,
    pub fake: FakeStore,
    pub transport: Recording,
    pub mint_key: Credential,
}

/// The minter of a rig.
pub type RigMinter = StsMinter<Recording, Arc<ManualClock>>;

impl Rig {
    pub fn new() -> Self {
        let clock = Arc::new(ManualClock::new(at(START)));
        let endpoint = Endpoint::parse(URL, REGION, BUCKET, true).unwrap();
        let fake = FakeStore::new(endpoint, clock.clone());
        let mint_key = fake.minting_key();
        let transport = Recording {
            fake: fake.clone(),
            sent: Arc::default(),
        };
        Self {
            clock,
            fake,
            transport,
            mint_key,
        }
    }

    /// A minter that signs with the rig's mint key.
    pub fn minter(&self) -> RigMinter {
        self.minter_with(self.mint_key.clone())
    }

    /// A prefix-grant minter deriving from a new signing key of the fake store, on the rig's
    /// clock.
    pub fn presigned_minter(&self) -> PresignedPrefixMinter<Arc<ManualClock>> {
        PresignedPrefixMinter::new(self.clock.clone(), self.fake.prefix_signing_key())
    }

    /// A minter that signs with `key`.
    pub fn minter_with(&self, key: Credential) -> RigMinter {
        StsMinter::new(
            self.transport.clone(),
            self.clock.clone(),
            StsEndpoint::parse(URL, REGION).unwrap(),
            RoleArn::new(ROLE).unwrap(),
            key,
        )
    }

    /// What the minter sent, in order.
    pub fn sent(&self) -> Vec<HttpRequest> {
        self.transport.sent.lock().unwrap().clone()
    }

    /// A client of the fake store signing with `credential`.
    pub fn client(&self, credential: Credential) -> Store<FakeStore, Arc<ManualClock>> {
        let endpoint = Endpoint::parse(URL, REGION, BUCKET, true).unwrap();
        Store::new(self.fake.clone(), self.clock.clone(), endpoint, credential)
    }
}

/// A transport that answers the replies it was given, in order.
#[derive(Clone, Default)]
pub struct Scripted {
    replies: Arc<Mutex<VecDeque<Result<HttpResponse, TransportError>>>>,
}

impl Scripted {
    /// A transport answering `replies`.
    pub fn new(replies: Vec<Result<HttpResponse, TransportError>>) -> Self {
        Self {
            replies: Arc::new(Mutex::new(replies.into())),
        }
    }
}

impl Transport for Scripted {
    fn send(&self, _request: HttpRequest) -> Result<HttpResponse, TransportError> {
        self.replies.lock().unwrap().pop_front().unwrap()
    }
}

/// A minter over `transport`, at [`START`].
pub fn scripted_minter(transport: Scripted) -> StsMinter<Scripted, Arc<ManualClock>> {
    StsMinter::new(
        transport,
        Arc::new(ManualClock::new(at(START))),
        StsEndpoint::parse(URL, REGION).unwrap(),
        RoleArn::new(ROLE).unwrap(),
        Credential {
            access_key_id: "AKIDMINT".to_owned(),
            secret_access_key: "mint-secret-access-key".to_owned(),
            session_token: None,
            expires_at: at(START + 86_400),
        },
    )
}

/// An answer of `status` with `body`.
pub fn reply(status: u16, body: &str) -> Result<HttpResponse, TransportError> {
    Ok(HttpResponse {
        status,
        headers: Vec::new(),
        body: body.as_bytes().to_vec(),
    })
}

/// An STS error answer of `status`, `code` and `message`.
pub fn sts_error(status: u16, code: &str, message: &str) -> Result<HttpResponse, TransportError> {
    let body = format!(
        "<ErrorResponse><Error><Type>Sender</Type><Code>{code}</Code><Message>{message}</Message></Error></ErrorResponse>"
    );
    reply(status, &body)
}

/// The body of a successful answer whose `<Credentials>` holds `inner`.
pub fn credentials(inner: &str) -> String {
    format!(
        "<AssumeRoleResponse><AssumeRoleResult><Credentials>{inner}</Credentials></AssumeRoleResult></AssumeRoleResponse>"
    )
}

/// The four fields of a credential expiring at `expiration`.
pub fn fields(expiration: &str) -> String {
    format!(
        "<AccessKeyId>ASIAEXAMPLE</AccessKeyId><SecretAccessKey>minted-secret</SecretAccessKey><SessionToken>minted-token</SessionToken><Expiration>{expiration}</Expiration>"
    )
}

/// The form fields of `request`'s body, decoded, in their order.
pub fn form(request: &HttpRequest) -> Vec<(String, String)> {
    let text = String::from_utf8(request.body.clone()).unwrap();
    text.split('&')
        .map(|pair| {
            let (name, value) = pair.split_once('=').unwrap();
            (decode(name), decode(value))
        })
        .collect()
}

/// `text` with each `%XX` replaced by its byte.
pub fn decode(text: &str) -> String {
    let mut out = Vec::new();
    let mut bytes = text.bytes();
    while let Some(byte) = bytes.next() {
        match byte {
            b'%' => {
                let hex: String = [bytes.next().unwrap(), bytes.next().unwrap()]
                    .iter()
                    .map(|b| char::from(*b))
                    .collect();
                out.push(u8::from_str_radix(&hex, 16).unwrap());
            }
            other => out.push(other),
        }
    }
    String::from_utf8(out).unwrap()
}

/// Whether `result` is a refusal of the credential (403 `AccessDenied`).
pub fn denied<T>(result: Result<T, StoreError>) -> bool {
    matches!(result, Err(StoreError::Denied { status: 403, ref code }) if code == "AccessDenied")
}
