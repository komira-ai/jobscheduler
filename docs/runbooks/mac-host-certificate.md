# Giving a Mac its host certificate

A Mac joins the lease server by hostkeeper's host certificate and by nothing else: the one-time
join tokens and the pool's token are gone ([docs/design/multi-instance.md](../design/multi-instance.md),
G3), and the move of the Macs from the token to the certificate is done. A Mac's group config
package names the certificate in `/etc/js-agent/flags`, and the agent's plist names only
`--flags-file /etc/js-agent/flags`. This page is the check to make before a Mac, or a new group
of Macs, is given that package.

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
  naming the CA that signs the host certificates, as it is for every other host.

## 2. Name the certificate in the group's flags

The group's config package (`js-agent-config-mac-build`,
[packaging/js-agent-macos/README.md](../../packaging/js-agent-macos/README.md), "Config
packages") adds two lines to `/etc/js-agent/flags` and keeps the others:

```
--host-cert=/var/lib/hostkeeper/identity/host.crt
--host-key=/var/lib/hostkeeper/identity/host.key
```

With both flags the agent presents the certificate in the TLS handshake and sends no bearer. An
agent started without both flags refuses to start, naming them, and the lease server refuses a
request with no verified host certificate (`401 host-certificate-required`). It reads the two files again before each request, so
hostkeeper's renewal in place needs no restart.

## 3. What the Mac does when the package lands

The config hook restarts the agent so that it reads the new flags, and the agent says hello with
its certificate. Check it with `js host list --server https://<job service> --token-file <operator
token>`: the Mac is `live` under its host id. An agent that holds a flags file naming `--token-file`
or `--instance-file` still starts: it reads neither file and warns once for each.
