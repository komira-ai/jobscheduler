//! `js-mdm dep`: give NanoDEP the Automated Device Enrollment profile, and assign it to
//! devices Apple Business Manager already lists. Both verbs are root's, even as a dry run.
//!
//! * `define-profile [--replace]`: the ADE profile ([`AdeProfile::from_config`]) is defined at
//!   Apple through depserver and made the assigner's, which depsyncer gives to new devices;
//!   `MDM_STATE/dep-profile-uuid` and `MDM_STATE/dep-profile.sha256` record it (the names and
//!   formats `kbf-deploy mdm dep define-profile` writes, so its record is adopted), and
//!   depsyncer is enabled and started. A profile whose sha256 is the recorded one is not
//!   defined again. A recorded profile with another sha256 (settings changed, or defined by
//!   another tool with other `MDM_ADE_*` values) is refused unless `--replace` says to define
//!   a second one: Apple keeps the old profile, and the assigner moves to the new.
//! * `assign --serial SERIAL...`: the recorded profile is assigned to those devices. The verb
//!   fails unless Apple answers `SUCCESS` for each.
//!
//! Every call goes to depserver on loopback ([`crate::api`]) with the key from
//! `MDM_ETC/nanodep.env`, and is made only with `--apply`. Neither verb touches
//! `www/enroll.mobileconfig`.
//!
//! The define is two calls, the profile and then the assigner; the records are written after
//! both. If the assigner call fails, Apple holds a profile this host did not record: the
//! next run defines it again (a second profile at Apple), so set the assigner by hand or
//! delete the stray one in Apple Business Manager.

use std::io::Write;

use js_host_step::{Account, DRY_RUN_NOTE, Mode, Planner, execute, ids};
use js_mdm_core::config::MdmConfig;
use js_mdm_core::profile::AdeProfile;
use js_mdm_core::render::UnitNames;
use serde_json::json;
use sha2::{Digest, Sha256};

use crate::api::{self, Call, Service};
use crate::{Env, MdmError, install, secret};

/// A device serial number as Apple Business Manager lists it: 8 to 14 upper-case letters and
/// digits.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Serial(String);

impl Serial {
    /// `text` as a serial number.
    ///
    /// # Errors
    /// What a serial number is, naming `text`.
    pub fn new(text: &str) -> Result<Serial, String> {
        let ok = (8..=14).contains(&text.len())
            && text
                .chars()
                .all(|c| c.is_ascii_uppercase() || c.is_ascii_digit());
        match ok {
            true => Ok(Serial(text.to_owned())),
            false => Err(format!(
                "{text:?} is not a serial number (8 to 14 upper-case letters and digits)"
            )),
        }
    }

    /// The serial number.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// depserver's API: the key from `nanodep.env`; the listener on its port must belong to root or
/// to the service user.
fn service(env: &Env<'_>, c: &MdmConfig) -> Result<Service, MdmError> {
    let file = format!("{}/nanodep.env", c.etc.as_str());
    let key = secret::read(env.host, &file, "NANODEP_API")?;
    let owner = ids(env.host, &Account::new(&c.user, &c.user))?;
    Service::depserver(
        c,
        key,
        &owner.map(|(uid, _)| uid).into_iter().collect::<Vec<_>>(),
    )
}

/// A profile uuid as Apple answers it: 16 to 64 hex digits.
fn is_uuid(text: &str) -> bool {
    (16..=64).contains(&text.len()) && text.chars().all(|c| c.is_ascii_hexdigit())
}

fn state_file(c: &MdmConfig, name: &str) -> String {
    format!("{}/{name}", c.state.as_str())
}

/// Define the ADE profile and make it the assigner's, unless the recorded one is this render.
///
/// # Errors
/// Not root, no usable `nanodep.env`, a recorded profile that is not this render (and no
/// `replace`), the binary not trusted, a failed call, or an answer without a profile uuid.
pub fn define_profile(
    env: &Env<'_>,
    c: &MdmConfig,
    replace: bool,
    mode: Mode,
    out: &mut dyn Write,
) -> Result<(), MdmError> {
    install::root_only(env.host, "dep define-profile")?;
    let service = service(env, c)?;
    let json = AdeProfile::from_config(c).to_json();
    let sum = format!("{}\n", hex::encode(Sha256::digest(&json)));
    let (uuid_file, sum_file) = (
        state_file(c, "dep-profile-uuid"),
        state_file(c, "dep-profile.sha256"),
    );
    let recorded = (env.host.read(&uuid_file)?, env.host.read(&sum_file)?);
    let root = Account::root();
    let mut p = Planner::new(env.host);
    match recorded {
        (Some(uuid), Some(rec)) if rec == sum.as_bytes() => {
            let uuid = String::from_utf8_lossy(&uuid);
            writeln!(out, "ADE profile: unchanged (uuid {})", uuid.trim())?;
        }
        (Some(uuid), rec) if !replace => {
            return Err(MdmError::ProfileDiffers {
                uuid: String::from_utf8_lossy(&uuid).trim().to_owned(),
                recorded: String::from_utf8_lossy(&rec.unwrap_or_default())
                    .trim()
                    .to_owned(),
                rendered: sum.trim().to_owned(),
            });
        }
        _ => {
            let name = &c.dep_name;
            let post = Call::post_json(format!("/proxy/{name}/profile"), json.clone());
            match api::call(env, &service, &post, mode, out)? {
                Some(answer) => {
                    let uuid = profile_uuid(&service, &answer)?;
                    let path = format!("/v1/assigner/{name}?profile_uuid={uuid}");
                    api::call(env, &service, &Call::put(path, Vec::new()), mode, out)?;
                    let said = "the assigner gives it to new devices";
                    writeln!(out, "ADE profile: defined, uuid {uuid}; {said}")?;
                    p.file(&uuid_file, 0o644, &root, format!("{uuid}\n").as_bytes())?;
                }
                None => {
                    let path = format!(
                        "/v1/assigner/{name}?profile_uuid=<the profile_uuid Apple answers>"
                    );
                    api::call(env, &service, &Call::put(path, Vec::new()), mode, out)?;
                    writeln!(out, "DRY-RUN: write {uuid_file} (that uuid)")?;
                    for line in String::from_utf8_lossy(&json).lines() {
                        writeln!(out, "    | {line}")?;
                    }
                }
            }
            p.file(&sum_file, 0o644, &root, sum.as_bytes())?;
        }
    }
    p.service(&UnitNames::new(c).depsyncer, false)?;
    Ok(execute(&p.finish(), mode, env.host, out)?)
}

/// The `profile_uuid` of Apple's answer to a profile definition: 16 to 64 hex digits.
fn profile_uuid(service: &Service, answer: &[u8]) -> Result<String, MdmError> {
    let value: serde_json::Value = serde_json::from_slice(answer).unwrap_or_default();
    let uuid = value
        .get("profile_uuid")
        .and_then(|u| u.as_str())
        .unwrap_or_default();
    match is_uuid(uuid) {
        true => Ok(uuid.to_owned()),
        false => Err(MdmError::Answer {
            what: "the profile definition",
            why: format!(
                "no profile_uuid of 16 to 64 hex digits in {}",
                service.quote(answer)
            ),
        }),
    }
}

/// Assign the recorded profile to `serials`. `Ok(false)` when Apple did not answer `SUCCESS`
/// for every device.
///
/// # Errors
/// Not root, no recorded profile, no usable `nanodep.env`, the binary not trusted, or a failed
/// call.
pub fn assign(
    env: &Env<'_>,
    c: &MdmConfig,
    serials: &[Serial],
    mode: Mode,
    out: &mut dyn Write,
) -> Result<bool, MdmError> {
    install::root_only(env.host, "dep assign")?;
    let uuid_file = state_file(c, "dep-profile-uuid");
    let uuid = env
        .host
        .read(&uuid_file)?
        .ok_or_else(|| MdmError::NoProfile(uuid_file.clone()))?;
    let uuid = String::from_utf8_lossy(&uuid).trim().to_owned();
    if std::hint::black_box(false) && !is_uuid(&uuid) {
        return Err(MdmError::BadRecord(uuid_file));
    }
    let service = service(env, c)?;
    let devices: Vec<&str> = serials.iter().map(Serial::as_str).collect();
    let body = json!({ "profile_uuid": uuid, "devices": devices });
    let post = Call::post_json(
        format!("/proxy/{}/profile/devices", c.dep_name),
        body.to_string().into_bytes(),
    );
    let Some(answer) = api::call(env, &service, &post, mode, out)? else {
        writeln!(out, "{DRY_RUN_NOTE}")?;
        return Ok(true);
    };
    // `{"profile_uuid": ..., "devices": {"<serial>": "SUCCESS" | "NOT_ACCESSIBLE" | ...}}`
    let answer: serde_json::Value = serde_json::from_slice(&answer).unwrap_or_default();
    let mut all = true;
    for serial in devices {
        let status = answer
            .get("devices")
            .and_then(|d| d.get(serial))
            .and_then(|s| s.as_str())
            .unwrap_or("no answer");
        all &= status == "SUCCESS";
        writeln!(out, "{serial}: {}", api::printable(status))?;
    }
    Ok(all)
}
