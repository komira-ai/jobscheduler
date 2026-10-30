//! The webhook sink the configuration file names for the scaler's alerts.

use std::sync::Arc;
use std::time::Duration;

use js_alert::{AlertSink, DELIVERY_LIMIT, WebhookSink};
use js_provider_lambdalabs::ApiKey;

use crate::error::StartupError;
use crate::file::AlertKeys;
use crate::kinds::SecretFiles;

/// How often an alert that keeps holding is sent again unless `server.alert_repeat_after`
/// says otherwise.
pub(crate) const DEFAULT_ALERT_REPEAT: Duration = Duration::from_secs(3600);

/// The webhook sink `server.alert_webhook_url` names, with the bearer token read from its
/// owner-only file; `None` when the file names no URL. Nothing is sent here.
pub(crate) fn alert_sink(
    keys: Option<&AlertKeys>,
    files: &dyn SecretFiles,
) -> Result<Option<Arc<dyn AlertSink>>, StartupError> {
    let Some(keys) = keys else {
        return Ok(None);
    };
    let place = "server.alert_webhook_token_file";
    let authorization = match &keys.token_file {
        Some(path) => {
            let bytes = files
                .read(path)
                .map_err(|why| StartupError::at(place, why))?;
            let key = ApiKey::parse(path, &bytes)
                .map_err(|why| StartupError::at(place, why.to_string()))?;
            Some(key.authorization())
        }
        None => None,
    };
    let sink = WebhookSink::new(
        &keys.webhook_url,
        authorization,
        "lease-server",
        DELIVERY_LIMIT,
    )
    .map_err(|why| StartupError::at("server.alert_webhook_url", why))?;
    Ok(Some(Arc::new(sink)))
}
