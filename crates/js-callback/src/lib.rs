//! A job's completion callback as the server sends it
//! (`docs/design/job-retry-and-completion-callback.md`, section 5): one bare POST with an
//! empty body, signed in the Standard Webhooks form with the key the job's submitter gave, to
//! the URL it gave, once the job reaches its final state.
//!
//! - The form check of a callback URL at submit: [`check_url`], which gives a [`CallbackUrl`]
//!   or says why not ([`UrlError`]).
//! - The signature and the `webhook-id`: [`sign`] and [`mint_webhook_id`].
//! - Where a call may go: the [`AddressRule`], which judges every address a try would connect
//!   to, and the [`Resolve`] that names a host's addresses ([`SystemResolver`] in the server).
//! - One try: the [`Sender`] (no proxy, no redirect, no body read, a time limit), trusting
//!   [`TlsRoots`], and what its answer means ([`CallbackAnswer`], [`FailureClass`]). When a
//!   failed call is tried again is the store's to decide (`js-jobs`).
//!
//! The key ([`SigningKey`]) and the answer words are defined in `js-types`, which the store
//! shares, and re-exported here.

mod address;
mod resolve;
mod sender;
mod sign;
mod url;

pub use address::AddressRule;
pub use js_types::{
    CallbackAnswer, FailureClass, MAX_SIGNING_KEY_LEN, MIN_SIGNING_KEY_LEN, SigningKey,
    SigningKeyError,
};
pub use resolve::{Resolve, ResolveError, SystemResolver};
pub use sender::{Sender, TlsRoots};
pub use sign::{WEBHOOK_ID_BYTES, mint_webhook_id, sign};
pub use url::{CallbackUrl, MAX_URL_LEN, UrlError, check_url};
