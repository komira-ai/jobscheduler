//! The check of a submitted job's completion callback (`spec.callback`), for `SubmitJob`
//! (`docs/design/job-retry-and-completion-callback.md`, sections 5.2 and 5.3). Pure, as the rest
//! of `validate.rs` is: it reads the request and the address rule, and looks no name up.
//!
//! | rule | refused as |
//! |---|---|
//! | the server takes callbacks: until the release that accepts one, it refuses any, an empty one included | `field: spec.callback: callback-not-enabled` |
//! | the URL is https, names a host, has no userinfo and no fragment, and is at most 2048 bytes of printable ASCII with no space, its port 1 to 65535 ([`js_callback::check_url`]) | `field: spec.callback.url: <code>`, the code one of `too-long`, `not-printable-ascii`, `not-https`, `fragment`, `userinfo`, `no-host`, `bad-port` |
//! | a host that is an address literal (`203.0.113.9`, `[2001:db8::1]`, `2130706433`, `0x7f.1`) names an address the address rule admits: not loopback, link-local, a metadata address, the cluster's own, or any other the rule denies | `field: spec.callback.url: refused-address` |
//! | the signing key is 32 to 64 bytes | `field: spec.callback.signingKey: bad-length` |
//!
//! A host that is a name is not resolved here: the sender resolves it and judges every address
//! at each try, so a name that turns inward later is refused then. The deadline is not
//! checked: absent is a day, and any value is cut to the job's retention end.
//!
//! A refusal repeats nothing of the callback: not the URL, whose path or query may carry the
//! receiver's own reference, and not the key or its length. The callback recorded holds the
//! key as given; the store keeps it in the job's `callback/` row alone, and its digest in the
//! job's row.

use js_callback::{AddressRule, SigningKey, check_url};
use js_jobs::{CallbackKey, JobCallback};
use js_proto::v1;

use super::validate::Fault;

/// The field a refused URL is named by.
const URL_FIELD: &str = "spec.callback.url";

/// The callback `given` asks for, checked against `rule`: none when the job asks for none, and
/// refused whatever it holds while `rule` is `None` (the server takes no callback).
///
/// # Errors
/// The [`Fault`] of the first rule of the module docs that `given` breaks.
pub(super) fn callback(
    given: Option<&v1::Callback>,
    rule: Option<&AddressRule>,
) -> Result<Option<JobCallback>, Fault> {
    let Some(given) = given else {
        return Ok(None);
    };
    let Some(rule) = rule else {
        return Err(Fault::Wire("spec.callback", "callback-not-enabled"));
    };
    let url = check_url(&given.url).map_err(|fault| Fault::Wire(URL_FIELD, fault.code()))?;
    if url.literal().is_some_and(|address| !rule.admits(address)) {
        return Err(Fault::Wire(URL_FIELD, "refused-address"));
    }
    let key = SigningKey::new(given.signing_key.clone())
        .map_err(|_| Fault::Wire("spec.callback.signingKey", "bad-length"))?;
    Ok(Some(JobCallback {
        url: given.url.clone(),
        key: CallbackKey::Key(key),
        deadline_seconds: given.deadline_seconds,
    }))
}
