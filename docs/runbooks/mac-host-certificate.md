# Moving a Mac from its join token to its host certificate

A Mac that joined the lease server by a join token (its agent runs with `--token-file
/etc/js-agent/token` and nothing in its flags file names a certificate) moves onto hostkeeper's
host certificate when its group's config package names the certificate. Nothing else changes on
the Mac: the agent's package, its plist (which goes on naming `--token-file`), the token file
and the host id stay as they are. A Mac that has moved sends no token; the Macs that have not
moved go on joining by the token, so the Macs of a group can move one at a time. The join
tokens are deleted from the lease server only after every host has moved
([docs/design/multi-instance.md](../design/multi-instance.md), G3).

Examples use the host id `mac-1` and the group `mac-build`. Replace them.

## 1. Check the Mac and the server first

- **The certificate names the Mac's host id.** The lease server reads the host from the
  certificate's one DNS name (`crates/js-lease-server/src/host_cert.rs`) and refuses every
  request about another host `403 certificate-host-mismatch`
  (`crates/js-lease-server/src/agent_api/host_cert.rs`). The host id the Mac registers under is
  the one `js host list` shows for it, and the one its flags file gives (`--host-id`, or
  `--host-id-from-facts`). On the Mac:

  ```
  openssl x509 -noout -text -in /var/lib/hostkeeper/identity/host.crt | grep -A1 'Subject Alternative Name'
  ```

  must print `DNS:mac-1` and no other DNS name. If it names another host, stop: the flags file's
  host id and hostkeeper's name for the Mac must agree before the move.
- **The key's owner and mode.** `/var/lib/hostkeeper/identity/host.key` after following links is a
  regular file owned by root (or by the account the agent runs as), mode 0600, 0400, 0640 or
  0440 (`--host-key` in `crates/js-agent/src/cli.rs`). The agent refuses any other, naming the
  flag and the file.
- **The server trusts hostkeeper's CA.** The lease server is started with `--agent-client-ca`
  naming the CA that signs the host certificates, as it is for the Linux hosts that join by
  certificate.

## 2. Name the certificate in the group's flags

The group's config package (`js-agent-config-mac-build`,
[packaging/js-agent-macos/README.md](../../packaging/js-agent-macos/README.md), "Config
packages") adds two lines to `/etc/js-agent/flags` and keeps the others:

```
--host-cert=/var/lib/hostkeeper/identity/host.crt
--host-key=/var/lib/hostkeeper/identity/host.key
```

With both flags the agent presents the certificate in the TLS handshake and sends no bearer: it
reads neither the token file nor a host credential kept in its state root, and keeps no new one
(`crates/js-agent/src/login.rs`). It reads the two files again before each request, so
hostkeeper's renewal in place needs no restart.

## 3. What the Mac does when the package lands

The config hook restarts the agent so that it reads the new flags. The lease server still holds
the session the agent opened on the token, so the first hello by certificate is refused `409
host-live`, and the agent tries again with its backoff. Once that session has lapsed (its grace,
15 seconds unless the pool's configuration says otherwise), the hello is admitted at the next
epoch and the Mac's binding moves from the token to the certificate
(`crates/js-registry/src/registry/admit.rs`). The config hook's `verify` passes when it sees that
new session.

Check it with `js host list --server https://<job service> --token-file <operator token>`: the
Mac is `live` under the same id.

## 4. There is no way back to the token

Once a Mac has said hello by its certificate, a token never takes that host id again: the token's
hello for it is refused `403 host-bound`, live or lost. `host-bound` is fatal to the agent, so
an agent put back on its token flags exits, and launchd starts it again into the same refusal.

The config hook's `rollback` can do that. If `verify` fails after the Mac has already said hello
by its certificate (the lease server cannot be reached for the rest of `verify`'s time), the
rollback puts the token flags back and restarts the agent. A missing hello never fails a
rollback, so the step still ends succeeded, and its detail says that no hello came
(`crates/js-agent/src/update_hook/config/steps.rs`, `rollback`). To bring such a Mac back,
roll the config package that names the certificate forward again. Do not take the certificate
lines out of the flags of a Mac that has moved.

## What the tests show

- `a_token_host_moves_onto_its_certificate_while_another_stays_on_the_token`
  (`crates/js-lease-server/tests/server/host_certs_agent.rs`): js-agent's own transport joins by
  the pool's token, then by the host certificate after its token session lapsed; the token is
  refused for that host, and a second host goes on joining and beating by the token.
- `a_lost_token_host_is_rebound_to_its_certificate_by_its_first_hello` and
  `the_move_onto_a_certificate_keeps_the_cordon_across_a_restart`
  (`crates/js-lease-server/tests/server/host_certs_hosts.rs`): the binding the server keeps
  moves to the certificate, a cordon is kept across the move and a server restart.
- `an_agent_given_its_host_certificate_and_key_connects_with_them`
  (`crates/js-agent/tests/agent/credential.rs`): with the certificate flags beside a kept host
  credential, the agent logs in by the certificate and leaves the kept file as it is.
