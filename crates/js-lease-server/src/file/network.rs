//! The `network` table of an elastic pool: how a launched host reaches the server.

use super::pins::read_artifact;
use super::{NetworkPlan, Section};
use crate::error::StartupError;
use crate::network::{OAuthClient, valid_tag};

pub(super) fn read_network(
    table: Option<toml::Table>,
    head: String,
) -> Result<NetworkPlan, StartupError> {
    let Some(table) = table else {
        return Ok(NetworkPlan::Default);
    };
    let mut s = Section::open(table, head);
    let kind = s.required_string("kind")?;
    match kind.as_str() {
        "default" => {
            s.only(&["kind"])?;
            Ok(NetworkPlan::Default)
        }
        "tailscale" => {
            s.only(&["kind", "oauth_client_file", "tag", "binary", "api_url"])?;
            let file = s.required_path("oauth_client_file")?;
            let tag = s.required_string("tag")?;
            if !valid_tag(&tag) {
                return Err(s.fail("tag", "must be \"tag:\" and letters, digits or '-'"));
            }
            let binary = match s.table("binary")? {
                Some(table) => Some(read_artifact(table, format!("{}binary.", s.head))?),
                None => None,
            };
            let api_url = s.string("api_url")?;
            match OAuthClient::from_file(&file) {
                Ok(client) => Ok(NetworkPlan::Tailscale {
                    client,
                    tag,
                    binary,
                    api_url,
                }),
                Err(err) => Err(s.fail("oauth_client_file", &err.to_string())),
            }
        }
        _ => {
            let reason = format!("{kind:?} is not a kind; known: default, tailscale");
            Err(s.fail("kind", &reason))
        }
    }
}
