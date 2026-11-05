//! The pulls of the crun runtime: an image made sure of in the store within a time limit.

use std::sync::Arc;
use std::time::Duration;

use js_contain::oci::Oci;
use js_contain::oci::image::PinnedImage;

use super::spec::pinned;
use crate::lifecycle::PullFuture;

/// `image` in the store, fetched and unpacked if it is not, within `limit` for the whole.
pub(super) async fn ensure_image(
    oci: &Oci,
    image: &PinnedImage,
    limit: Duration,
) -> Result<(), String> {
    let ensured = tokio::time::timeout(limit, oci.check_pinned(image)).await;
    ensured
        .map_err(|_| format!("pulling {} took longer than {limit:?}", image.reference))?
        .map_err(|why| why.to_string())
}

/// The pull and unpack of `image`, with the limit of the whole.
pub(super) fn pull(oci: Arc<Oci>, image: String, limit: Duration) -> PullFuture {
    Box::pin(async move {
        let pinned = pinned(&image)?;
        ensure_image(&oci, &pinned, limit).await
    })
}
