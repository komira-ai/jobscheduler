//! What the server still accepts and no longer reads: the pool tokens (`--agent-token-file`,
//! a pool's `join_token_file`) and `--require-host-credential` (`server.require_host_credential`),
//! which an installed unit or a deployed file may still pass. An agent is admitted by its host
//! certificate alone, so each of them leaves one line at start and does nothing else.

/// The line a given `item` (a flag, or a key with the file that holds it) leaves at start: it
/// names the item and says why it is ignored.
pub(crate) fn ignored(item: &str) -> String {
    format!("{item} is ignored: agents are admitted by host certificate only (drop it)")
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The line names the item and says it is ignored because a host is admitted by its
    /// certificate only, as the operator reads it at start.
    #[test]
    fn the_line_names_the_item_and_why_it_is_ignored() {
        assert_eq!(
            ignored("--agent-token-file"),
            "--agent-token-file is ignored: agents are admitted by host certificate only (drop it)"
        );
    }
}
