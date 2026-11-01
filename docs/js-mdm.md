# js-mdm: the host layer

`js-mdm` is jobscheduler's MDM for enrolling Macs as compute hosts. It runs NanoHUB, a SCEP
server, NanoDEP and Caddy as capped systemd services on one Linux host. Three crates:

| crate | layer | holds |
|---|---|---|
| `js-mdm-core` | `pure` | settings, units, profiles, pins: bytes from settings text, no I/O |
| `js-host-step` | `runtime` | the planner of host steps (file, directory, user, unit, command), the real host and a fake |
| `js-mdm` | `app` | the binary: `fetch`, `install`, `tls`, `verify` |

The settings file (`crates/js-mdm-core/config.example` documents every key) holds no secret and
no site name; `js-mdm` hard-codes nothing of a site.

## Verbs

Every verb that changes the host prints its plan by default (`DRY-RUN:` lines naming paths,
modes, owners and sizes, never contents) and acts with `--apply`. A second run finds
`nothing to do`. Exit 0 done, 1 a step, download or check failed, 2 refused (`REFUSED: ...` on
stderr).

* `js-mdm fetch --dir DIR [--only TOOL]... [--apply]`: the pinned archives, downloaded over
  HTTPS (redirects must stay on `https://`) as your own user, never root, into a directory
  outside every git work tree. Each is checked against its sha256 pin; a download that is not the
  pin is written to `<file>.rejected` and fails the run.
* `js-mdm install --config FILE --bin-dir DIR [--apply]`: root. Creates the service user, the
  directories (state 0700, etc 0750, bin 0755), unpacks the binaries from the checked archives
  (the old one kept as `<name>.previous`), makes the three secrets once from 32 random bytes in
  0600 root files (never printed, never on a command line), makes the SCEP CA unless `ca.pem`
  is there, writes the slice, the units and the Caddyfile as `js-mdm-core` renders them, and
  starts the services. The proxy waits for the TLS certificate.
* `js-mdm install --config FILE --uninstall [--apply]`: root. Stops and disables the units and
  moves their files to `MDM_STATE/removed/<unix seconds>/`. The state, the TLS certificate, the
  SCEP CA, the secrets, the binaries and the user stay.
* `js-mdm tls --config FILE [--apply]`: root. lego over DNS-01 with the configured provider
  (GoDaddy, Route 53 or Cloudflare). The provider's credentials are lines of the root-only
  `MDM_ETC/dns.env`, which the certificate unit loads itself; they are never read into this
  program's output or any argument list.
* `js-mdm verify --config FILE [--ignore TEXT]...`: read only. Health checks (units active,
  listeners, certificate lifetime, modes), then one line per installed file against the render
  of the same settings:

  ```
  same       /etc/systemd/system/js-mdm-scep.service
  differs    /etc/systemd/system/js-mdm-proxy.service: installed sha256 ..., rendered sha256 ...
  missing    /etc/systemd/system/js-mdm-cert.timer
  ```

  Comment lines are left out of both sides before hashing. `--ignore TEXT` removes TEXT from the
  installed text first, so a host installed by another tool that words one line differently can
  be compared (`--ignore " (local note)"`). A file this user may not read is `unreadable`;
  world-readable files need no root, the checks of root-only files do.

## Not in this slice

`push-cert` (the APNs push certificate and the enrollment profile), `dep` (Apple Business
Manager) and `erase`, and the checks that dial the proxy over HTTPS. The enrollment profile is
therefore not yet written or compared. Security notes for the profile are in `js-mdm-core`'s
crate documentation.

## Tests

Tests run on `js_host_step::fake::FakeHost`: a file tree in memory and a command runner that
answers only the lines a test registered (anything else is an error, and every line is
recorded). The real host is tested under a scratch directory standing in for `/`.
