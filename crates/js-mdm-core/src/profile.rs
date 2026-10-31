//! The two profiles the kit hands Apple's side: the enrollment profile a Mac installs
//! ([`enrollment`], a `.mobileconfig` built with the `plist` crate) and the Automated
//! Device Enrollment profile NanoDEP defines at Apple ([`AdeProfile`]).
//!
//! The enrollment profile holds two payloads: a SCEP payload, through which the Mac
//! gets its identity certificate from `https://<MDM_DNS_NAME>/scep` with the SCEP
//! challenge, and the MDM payload, which names that certificate (its
//! `IdentityCertificateUUID` is the SCEP payload's `PayloadUUID`), the server
//! `https://<MDM_DNS_NAME>/mdm` and the push topic. Its identifiers are
//! `MDM_PROFILE_ID` (`.scep` and `.mdm` for the payloads) and its payload UUIDs are
//! fixed, so a render with the same settings is the same profile, not a second one.
//!
//! The ADE profile is built from the settings, and [`AdeProfile::parse`] reads one back
//! and refuses an unknown key and a profile a device could leave: one that is not
//! supervised, not mandatory, removable, or that sends the device anywhere but
//! `https://<MDM_DNS_NAME>/enroll`.

use std::fmt;

use plist::{Dictionary, Value};
use serde::{Deserialize, Serialize};

use crate::config::MdmConfig;
use crate::secret::Secret;

/// The SCEP payload's UUID, which the MDM payload names as its identity.
const SCEP_UUID: &str = "071DB849-C11F-4A24-B1E8-69113460B678";
const MDM_UUID: &str = "25B0DB54-2EDA-4965-B430-24DB8BFC06DD";
const PROFILE_UUID: &str = "5F652B4C-C5C9-4D69-B44E-493CB00C698E";
/// Every MDM right (`AccessRights`, bits 1 to 4096).
const ACCESS_RIGHTS: i64 = 8191;
/// The SCEP key: RSA 2048, for signing and encryption (`Key Usage` 1 | 4).
const KEY_SIZE: i64 = 2048;
const KEY_USAGE: i64 = 5;
const CAPABILITIES: [&str; 3] = [
    "com.apple.mdm.per-user-connections",
    "com.apple.mdm.bootstraptoken",
    "com.apple.mdm.token",
];

/// An MDM push topic, `com.apple.mgmt.External.<uuid>`: the subject UID of an APNs MDM
/// push certificate.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Topic(String);

impl Topic {
    /// `text` as a topic, or why it is not one.
    pub fn new(text: &str) -> Result<Topic, ProfileError> {
        let _ = text;
        todo!("red run: not implemented yet")
    }

    /// The topic as text.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for Topic {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// Why a profile, or a part of one, was refused.
#[derive(Debug, PartialEq, Eq, thiserror::Error)]
pub enum ProfileError {
    /// Not a push topic.
    #[error("topic {0:?} is not com.apple.mgmt.External.<uuid>")]
    Topic(String),
    /// Not the JSON the kit reads.
    #[error("the ADE profile is not the JSON this kit reads: {0}")]
    Json(String),
    /// Sends devices to another server.
    #[error("the ADE profile sends devices to {got:?}, not {want:?}")]
    Url {
        /// The URL in the profile.
        got: String,
        /// The URL it must hold.
        want: String,
    },
    /// Not supervised.
    #[error(
        "the ADE profile is not supervised (is_supervised): an unsupervised Mac takes no erase or lock"
    )]
    NotSupervised,
    /// Skippable.
    #[error(
        "the ADE profile is not mandatory (is_mandatory): Setup Assistant would let a person skip it"
    )]
    NotMandatory,
    /// The MDM can be removed from the Mac.
    #[error(
        "the ADE profile is removable (is_mdm_removable): a person could remove the MDM from the Mac"
    )]
    Removable,
}

/// The enrollment profile for the MDM at `MDM_DNS_NAME`, named `MDM_PROFILE_ID` and
/// `MDM_PROFILE_DISPLAY_NAME`, as the bytes of an XML property list ending in a newline.
pub fn enrollment(c: &MdmConfig, topic: &Topic, challenge: &Secret) -> Vec<u8> {
    let _ = (c, topic, challenge);
    todo!("red run: not implemented yet")
}

fn text(s: &str) -> Value {
    Value::String(s.to_owned())
}

fn dict<const N: usize>(pairs: [(&str, Value); N]) -> Value {
    let mut d = Dictionary::new();
    for (key, value) in pairs {
        d.insert(key.to_owned(), value);
    }
    Value::Dictionary(d)
}

/// The Automated Device Enrollment profile, as Apple's `define profile` call takes it.
/// The fields are in the order they are sent.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AdeProfile {
    /// The profile's name at Apple.
    pub profile_name: String,
    /// Where a device is sent to enroll.
    pub url: String,
    /// Whether the device is supervised.
    pub is_supervised: bool,
    /// Whether a person must take the profile.
    pub is_mandatory: bool,
    /// Whether a person may remove the MDM.
    pub is_mdm_removable: bool,
    /// Whether Setup Assistant waits for the MDM to finish configuring the device.
    pub await_device_configured: bool,
    /// Whether Setup Assistant advances by itself.
    pub auto_advance_setup: bool,
    /// The `org_magic`.
    pub org_magic: String,
    /// The language.
    pub language: String,
    /// The region.
    pub region: String,
    /// Certificates the device trusts during enrollment.
    pub anchor_certs: Vec<String>,
    /// The Setup Assistant panes skipped.
    pub skip_setup_items: Vec<String>,
}

impl AdeProfile {
    /// The profile for the settings `c`: supervised, mandatory, not removable, sending
    /// devices to `https://<MDM_DNS_NAME>/enroll`.
    pub fn from_config(c: &MdmConfig) -> AdeProfile {
        let _ = c;
        todo!("red run: not implemented yet")
    }

    /// `text` read as a profile for the MDM at `dns_name`, refused unless a device it
    /// enrolls is supervised, cannot skip it, cannot remove the MDM, and is sent to
    /// this MDM.
    pub fn parse(text: &str, dns_name: &str) -> Result<AdeProfile, ProfileError> {
        let _ = (text, dns_name);
        todo!("red run: not implemented yet")
    }

    /// The bytes sent to Apple: pretty JSON in field order, ending in a newline.
    pub fn to_json(&self) -> Vec<u8> {
        let mut out = serde_json::to_vec_pretty(self).expect("a profile serializes");
        out.push(b'\n');
        out
    }
}
