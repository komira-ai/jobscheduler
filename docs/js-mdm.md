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

  Operator paths must not cross a symlinked directory: `--config`, `--bin-dir` and the
  archives in it, and `MDM_STATE`, `MDM_ETC`, `MDM_BIN` and everything below them. The host
  layer opens every directory on the way without following links, because the service user
  owns directories under `MDM_STATE` and could otherwise point root at any directory of the
  host (to chmod, chown, write or read there). A refusal names the component, for example
  `refused: /etc/js-mdm/x: js-mdm is a symbolic link; give the real path`; give the real path
  instead. A directory on the way owned by a user other than root, the effective user or the
  owner the step asks for is refused too, naming the directory and the uid. `fetch --dir` is
  the one path that is resolved through links first (it is your own path, and the git work tree
  check needs the real one).
* `js-mdm install --config FILE --uninstall [--apply]`: root. Stops and disables the units and
  moves their files to `MDM_STATE/removed/<unix seconds>/`. The state, the TLS certificate, the
  SCEP CA, the secrets, the binaries and the user stay.
* `js-mdm tls --config FILE [--apply]`: root. lego over DNS-01 with the configured provider
  (GoDaddy, Route 53 or Cloudflare). The provider's credentials are lines of the root-only
  `MDM_ETC/dns.env`, which the certificate unit loads itself; they are never read into this
  program's output or any argument list. Only the checks before the plan refuse without
  changing anything: the plan runs in two parts (the proxy can be planned only once the
  certificate is there), so when the certificate is missing after the issuing unit ran, the run
  fails with exit 1 and the units and the first issuance attempt stay in place. Fix the cause
  and run it again.
* `js-mdm verify --config FILE [--ignore TEXT]...`: read only. Health checks (units active,
  listeners, certificate lifetime, modes), then one line per installed file against the render
  of the same settings:

  ```
  same       /etc/systemd/system/js-mdm-scep.service
  differs    /etc/systemd/system/js-mdm-proxy.service: installed sha256 ..., rendered sha256 ...
  missing    /etc/systemd/system/js-mdm-cert.timer
  ```

  `wrong` is printed for a file that is not a regular file (a link is never followed) or does not
  have the mode and owner `install` gives it, whatever its text. Comment lines are left out of
  both sides before hashing. `--ignore LINE` leaves out, from both sides, the lines equal to
  LINE, whole (a line that holds LINE and more is kept; an empty LINE or one with a line break
  is refused), and every line left out is printed under its file as `ignored`. A host
  installed by another tool that words one line differently is compared with one `--ignore` for
  its wording and one for the render's. A file this user may not read is `unreadable`;
  world-readable files need no root, the checks of root-only files do. A unit an admin has
  masked (a link to `/dev/null`) is reported as `wrong`, a link, not as `same` or `missing`.

* `js-mdm dep define-profile --config FILE [--replace] [--apply]`: root, even as a dry run.
  Renders the ADE profile (`js-mdm-core`'s `AdeProfile`), defines it at Apple through
  depserver (`POST /proxy/<MDM_DEP_NAME>/profile`), makes it the assigner's
  (`PUT /v1/assigner/<MDM_DEP_NAME>?profile_uuid=...`, which depsyncer gives to new devices),
  records it in `MDM_STATE/dep-profile-uuid` and `MDM_STATE/dep-profile.sha256` (0644 root;
  `<uuid>\n` and `<sha256 hex of the profile JSON>\n`) and enables and starts the depsyncer.
  A dry run prints the profile JSON and the calls and makes none. The records decide a rerun:

  * the recorded sha256 is this render's: `ADE profile: unchanged`, no call (the depsyncer is
    still brought up if it is not);
  * a profile is recorded with another sha256 (or none): refused, exit 2, before any call. The
    `MDM_ADE_*`, `MDM_LANGUAGE` or `MDM_REGION` settings changed, or another tool defined the
    profile with other values. `--replace` defines a second profile (Apple keeps the old one)
    and moves the assigner to it; with no record the first define needs no flag, and with a
    recorded profile that is this render `--replace` changes nothing.

  The API key `NANODEP_API` is read from `MDM_ETC/nanodep.env` (0600, root; surrounding quotes
  and trailing blanks are removed, as systemd does for the service). It travels only as the
  `Authorization` header of calls to `127.0.0.1:MDM_DEP_PORT`, and a call is made only when:

  * the running binary, its canonical path and every directory above it are owned by root and
    writable by neither group nor others (`REFUSED` otherwise): a binary a user could swap
    would be run by the next root run;
  * every socket listening on `MDM_DEP_PORT` (IPv4 or IPv6, read from `/proc/net/tcp` and
    `tcp6`) is owned by root or by `MDM_USER`: a local user who holds the port while depserver
    is down gets no key and cannot hand back a profile uuid. The check is made just before each
    call, so a user who takes the port in the instant between the check and the connect is not
    caught (the port is taken, or depserver, which holds it, is not).
  * the whole call, from the connect to the last byte of the answer, takes at most 120 s, so a
    server that sends a byte at a time ends the call.

  Calls are printed with their size, never their body. Text of an answer quoted in an error
  (300 characters at most) has the key, its base64 forms and every line naming an
  `Authorization` header replaced, and control characters written out.

  The define is two calls, the profile and then the assigner, and the records are written after
  both. If the assigner call fails, Apple holds a profile this host did not record; the next run
  defines it again (a second profile at Apple), so set the assigner by hand or delete the stray
  profile in Apple Business Manager. A define followed by a failed record write is the same.

  Compatibility with `kbf-deploy mdm dep define-profile`: it writes the same two files in the
  same formats, so a box it ran on is adopted: the render hashes the same as kbf-deploy's only
  if `MDM_ADE_PROFILE_NAME` (kbf-deploy: `<MDM_ORG> build farm`), `MDM_ADE_ORG_MAGIC`
  (`kbf-mdm`), `MDM_LANGUAGE` (`en`), `MDM_REGION` (`US`) and `MDM_ADE_SKIP_ITEMS` (the
  default list) equal kbf-deploy's values and `MDM_DNS_NAME` is the same. With other values
  the sha256 differs, and the verb refuses to define a second profile unless `--replace` is
  given. To keep the profile Apple already holds, set those keys to kbf-deploy's values.
* `js-mdm dep assign --config FILE --serial SERIAL... [--apply]`: root. Assigns the recorded
  profile to devices already listed in Apple Business Manager (`POST
  /proxy/<MDM_DEP_NAME>/profile/devices`; the depsyncer covers devices that sync in later). A
  serial is 8 to 14 upper-case letters and digits. One line `SERIAL: STATUS` per device; exit 1
  unless Apple answers `SUCCESS` for each (a serial missing from the answer is `no answer`).
  Refused (exit 2) when no profile is recorded, or when the recorded uuid is not 16 to 64 hex
  digits: run `dep define-profile --apply` first. Apple's status text is printed with control
  characters written out.

Neither `dep` verb touches `www/enroll.mobileconfig`, and `install` does not write it: an
enrollment profile written by another tool stays as it is. (Its payload UUIDs are derived from
`MDM_PROFILE_ID` here and are fixed in kbf-deploy's render, so a Mac that took one would hold
two profiles if the file were replaced.) Writing it is the job of the `push-cert` verb, which
is not in this slice.

## Not in this slice

`push-cert` (the APNs push certificate and the enrollment profile), `dep get-cert` and
`dep upload-token` (the Apple Business Manager token), and `erase`, and the checks that dial
the proxy over HTTPS. The enrollment profile is therefore not yet written or compared.
Security notes for the profile are in `js-mdm-core`'s crate documentation.

## Tests

Tests run on `js_host_step::fake::FakeHost`: a file tree in memory and a command runner that
answers only the lines a test registered (anything else is an error, and every line is
recorded). The real host is tested under a scratch directory standing in for `/`.
