//! The `[[client]]` tables of the configuration file: the named clients of the job service,
//! each a name and the file of its own bearer token.
//!
//! ```toml
//! [[client]]
//! name = "ci"                                   # 1 to 40 letters, digits, '.', '_', '-'
//! token_file = "/run/credentials/js/ci-token"   # owner-only, one token as --token-file's
//! ```
//!
//! They are `--client-token NAME=PATH` in the file, and the client of
//! `server.client_token_file` is the one named `default`. A refusal names the table by its
//! name when the name is valid, else by its number, and never repeats the token or a name
//! that is not valid.

use js_jobs::ClientName;

use super::{Load, Section, Server};
use crate::config::{Clients, Taken, Token, token_at};
use crate::error::StartupError;

/// The fields of a `[[client]]` table.
const CLIENT_KEYS: &[&str] = &["name", "token_file"];

/// What the `name` of a `[[client]]` table must be.
const NAME_MUST: &str = "must be 1 to 40 bytes of letters, digits, '.', '_' and '-'";

/// The clients of the file: the client named `default`, whose token is `default` (the token
/// of `server.client_token_file`), and one per `[[client]]` table of `root`, in order, each
/// token read through the load's secret files. The tables need the job service, which alone
/// takes them: without `server.job_listen` they are refused.
pub(super) fn read(
    root: &mut Section,
    default: &Token,
    server: &Server,
    ld: &Load<'_>,
) -> Result<Clients, StartupError> {
    let mut clients = Clients::only(default, "the client of server.client_token_file");
    let tables = root.tables("client")?;
    if !tables.is_empty() && server.agents.job_listen().is_none() {
        return Err(root.fail(
            "client",
            "needs server.job_listen, the job service it serves",
        ));
    }
    for (index, table) in tables.into_iter().enumerate() {
        let named = table
            .get("name")
            .and_then(toml::Value::as_str)
            .and_then(|name| ClientName::parse(name).ok());
        let head = match &named {
            Some(name) => format!("client {:?}: ", name.as_str()),
            None => format!("client #{}: ", index + 1),
        };
        let mut s = Section::open(table, head);
        s.only(CLIENT_KEYS)?;
        s.required_string("name")?;
        let Some(name) = named else {
            return Err(s.fail("name", NAME_MUST));
        };
        let path = s.required_path("token_file")?;
        let token = token_at(&path, |path| ld.files.read(path))
            .map_err(|reason| s.fail("token_file", &reason))?;
        let place = format!("[[client]] {:?}", name.as_str());
        let refused = |taken: Taken| s.fail(taken.key(), &taken.to_string());
        clients.add(name, &token, place).map_err(refused)?;
    }
    Ok(clients)
}
