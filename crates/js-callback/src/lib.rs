//! A job's completion callback as the server sends it
//! (`docs/design/job-retry-and-completion-callback.md`, section 5): one bare POST with an
//! empty body, signed in the Standard Webhooks form with the key the job's submitter gave, to
//! the URL it gave, once the job reaches its final state.
//!
//! This release holds the form check of a callback URL ([`check_url`], which gives a
//! [`CallbackUrl`] or says why not, [`UrlError`]) and the words the store and the sender share,
//! which `js-types` defines and this crate re-exports: the key ([`SigningKey`]) and what a try
//! came to ([`CallbackAnswer`], [`FailureClass`]). Nothing here signs or sends a call yet, and
//! the server refuses a job that carries a callback.

mod url;

pub use js_types::{
    CallbackAnswer, FailureClass, MAX_SIGNING_KEY_LEN, MIN_SIGNING_KEY_LEN, SigningKey,
    SigningKeyError,
};
pub use url::{CallbackUrl, MAX_URL_LEN, UrlError, check_url};
