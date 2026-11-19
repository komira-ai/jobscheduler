# Keeping jobs off one node for a window (`js-agent cordon`)

A measurement needs a node that no job lands on for an hour or two, and that goes back into
service afterwards. The lease server lets a host cordon itself: while a cordon is in force the host
is placed no new job (a job it runs goes on). `js-agent cordon` is the client for it,
for an operator's script, on Linux and on macOS.

A cordon keeps the host out of every placement: with the job service on, the job store holds it,
so no server over that store places a job on the host while it is in force (each pass leaves the
host out, and the placement's own transaction refuses it), whichever server took the cordon.

## Taking a host out of placement for hours or days

For "keep jobs off this host for the next 48 hours" there is no need to stop the agent or to keep a
process running: an operator's cordon, set through the job service with an operator's token,
holds for the whole length asked for (up to seven days per call) with nothing renewing it. It is
kept over restarts and updates of the host's agent (the agent's update hook never lifts a cordon it
did not place) and over restarts of the lease server, and the jobs already on the host go on:

```text
js host cordon --server https://<job service> --token-file <operator token> --host-id <id> --cordon-secs 48h
```

`js host list` shows the cordon's end (`cordon_expires_at_ms`) while it holds. To put the host back
into placement before then:

```text
js host uncordon --server https://<job service> --token-file <operator token> --host-id <id>
```

The call extends a cordon already in force (it never shortens one) and lifts any holder's. A
`js-agent cordon` run on the host meanwhile finds the operator's cordon, prints `kept` and leaves it
alone. A cordon asked for longer than seven days is cut to seven days; repeat the call to go on.

## What it does

On a host the agent runs on, as root, name the agent's own flags file and its state root:

```text
sudo js-agent cordon --flags-file /etc/js-agent/flags --state-root /var/lib/js-agent/state --for 2h
```

`--flags-file` is read as the agent reads it (the same owner and mode rules, the same flags), and
the verb takes from it the lease server (`--server`), the certificate to trust (`--ca-cert`), the
host id (`--host-id` or `--host-id-from-facts`) and the host's certificate and key (`--host-cert`
and `--host-key`). It therefore cordons the host the agent registers, with what the agent
presents. `--state-root` and `--token-file` are given beside it, as the agent's unit gives them
beside the file; a flag the file names may not be given again, and `--server`, `--ca-cert`,
`--host-id`, `--host-id-from-facts`, `--host-cert` and `--host-key` may not be given with
`--flags-file` at all.

Without a flags file the verb takes the same values as its own flags:

```text
js-agent cordon --server <url> --state-root <dir> --for 2h
```

It cordons the host under the id the agent registers with. Without a flag that is the
default derived from the machine id; a host whose agent is started with `--host-id <id>` is
cordoned with the same `--host-id <id>`, and one whose agent takes its id from hostkeeper's facts
file (`--host-id-from-facts`) is cordoned with
`--host-id-from-facts /var/lib/hostkeeper/facts.json`.

1. It cordons the host with the host's own credential, chosen as the agent chooses it: the host
   certificate that hostkeeper keeps (`--host-cert` and `--host-key`; the key must be readable by
   the user running the verb, so that user is root or belongs to the key's group), or the one the
   agent keeps in its state root, or `--token-file` for a host that has not swapped its one-time
   token yet. The pool's shared join token is refused by the server on these routes.
2. It renews the cordon every third of `--lease-secs` (default 300, at most 3600) until `--for` has
   passed. The cordon is a lease the verb renews: a server that is not asked again lets it lapse.
3. It lifts the cordon at the end, or at once on SIGTERM, SIGINT or SIGHUP (the end of an ssh
   session, so a dropped connection does not leave the cordon to lapse).

The verb is a named holder of the cordon, and names it on every call (`--holder <name>`, 1 to 63
characters of `[a-z0-9._-]`; without it a name made for the run, `cordon-<pid>-<unix millis>`,
which the `cordoned` line prints). The server therefore says whose a cordon is, and the verb never
guesses it from a clock. It needs a lease server of this release: an earlier server refuses the
holder field with `invalid-cordon`.

### When the server refuses the credential

A host whose agent presents its host certificate reads no `host-credential` file, so a file left in
its state root by an earlier registration is not what the agent uses. Given that state root and no
host certificate, the verb sends that file, and the server refuses it: `join-token-not-admitted`
when the earlier registration used another host id than the one asked for; `join-token-expired`
once the credential has gone unused for its bound (24 hours with no hello or heartbeat), and
`http-401` once a later mint of a join token has dropped that lapsed credential. `join-token-not-admitted` also
comes when the verb is told another host id than the one the agent registered with (a
`--host-id-from-facts` on a host whose agent registered with `--host-id`). Standard error then
says what was sent and for which host:

```text
js-agent cordon: credential error: cannot cordon the host: the server refused the credential (join-token-not-admitted)
js-agent cordon: it sent the host credential in /var/lib/js-agent/state/host-credential for host <id>; to cordon with what the agent on this host presents, give the agent's flags file with --flags-file (such as /etc/js-agent/flags)
```

Run it again with `--flags-file` as above: the verb then takes the host id and the credential from
the agent's own flags, so it asks for the host the agent registers, with what the agent presents.

It extends and lifts only its own cordon:

- With no cordon in force, the first call places one, held by the verb's holder.
- When a cordon is in force that another holder holds (an operator's, which names no holder, or a
  requester's with another name), the server leaves it exactly as it was: the verb prints `kept
  host=<id> reason=already_cordoned holder=<holder, or -> expires_in_ms=<n>`, renews nothing, lifts
  nothing and exits 3. A cordon that is found is no longer extended, so a requester that died is
  not kept alive by the next one: its cordon lapses within its last `--lease-secs`, and the next
  run then places its own.
- `--take-over` makes the cordon in force the verb's, whoever holds it, and the verb holds and lifts
  it as its own (the first call only asks for it; the renewals do not).
- A renewal that finds another holder's cordon (the verb's own lapsed, for instance because the
  process was stopped for longer than `--lease-secs`, and another holder's took its place, or it was
  taken over) prints `kept host=<id> reason=taken_over holder=<holder, or ->`, lifts nothing and
  exits 3. A renewal that places the cordon again (it lapsed and nobody took its place) says
  `cordoned` again, and the cordon is held and lifted as before.
- The lift names the holder, so the server ends the verb's own cordon only. When it finds another
  holder's, the verb prints the same `taken_over` line and exits 3.
- It also lifts nothing when a reply names a host other than the one it cordoned.

To recover from a cordon whose holder died (a `kept ... reason=already_cordoned` that should not be
there), wait for it to lapse (at most that holder's `--lease-secs`, which `expires_in_ms` says), or
run the verb again with `--take-over`, or lift it as the operator with
`js host uncordon --host-id <id>`.

## What it prints

One line per state change on standard output, to be read by a script:

| line | when |
|---|---|
| `cordoned host=<id> holder=<h> expires_in_ms=<n> leases=<n>` | the cordon was placed or taken over, or placed again by a renewal after a lapse of the verb's own |
| `idle host=<id>` | once: the host reads no lease (`lease` absent, `leases` 0), is not `releasing`, and has no `mark` |
| `lifted host=<id>` | the cordon was lifted |
| `lapsed host=<id>` | the cordon had lapsed already at the end, nothing was left to lift |
| `kept host=<id> reason=already_cordoned holder=<h or -> expires_in_ms=<n>` | another holder's cordon was found when the verb asked; it is not extended and not lifted |
| `kept host=<id> reason=taken_over holder=<h or ->` | the cordon became another holder's during the window (a lapse and another's cordon, or a take-over); at a renewal or at the lift |

`idle` means what a script's drain gate checks on `GET /v1/hosts`: `lease` null, `releasing` false
and `mark` null. Until it is printed the verb reads the host every 5 seconds, so a script that waits
for it is told within seconds of the last lease ending.

| exit status | meaning |
|---|---|
| 0 | held to the end (or a stop) and lifted, or had lapsed |
| 1 | a server or credential error; the message on standard error says which and never holds the credential. The verb's cordon is lifted if the server can still be asked |
| 2 | a wrong flag or a file it names |
| 3 | another holder's cordon was found, when the verb asked or later, and is left in place |

A process killed with SIGKILL lifts nothing: the cordon lapses by itself within `--lease-secs` of the
last renewal (at most an hour).

## A measurement window

```sh
log=$(mktemp)
js-agent cordon --flags-file /etc/js-agent/flags --state-root /var/lib/js-agent/state \
    --for 2h > "$log" &
cordon=$!
# Wait for the node to be quiet: no lease, nothing releasing, not set aside.
until grep -q '^idle ' "$log"; do
    kill -0 "$cordon" 2> /dev/null || { echo "cordon ended: $(cat "$log")"; exit 1; }
    sleep 5
done
measure_the_node
kill -TERM "$cordon"
wait "$cordon"          # prints `lifted`, exits 0
```

A script that exits early without the `kill` leaves the cordon to lapse; run the verb under the
script's own cleanup so that a failure lifts it.
