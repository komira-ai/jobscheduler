# Keeping jobs off one node for a window (`js-agent cordon`)

A measurement needs a node that no job lands on for an hour or two, and that goes back into
service afterwards. The lease server lets a host cordon itself: while a cordon is in force the host
is placed no new job (a job it runs goes on). `js-agent cordon` is the client for it,
for an operator's script, on Linux and on macOS.

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
3. It lifts the cordon at the end, or at once on SIGTERM or SIGINT.

### When the server refuses the credential

A host whose agent presents its host certificate reads no `host-credential` file, so a file left in
its state root by an earlier registration is not what the agent uses. Given that state root and no
host certificate, the verb sends that file, and the server refuses it: `join-token-not-admitted`
when the earlier registration used another host id than the one asked for, `join-token-expired` or
`http-401` once the server has let the unused credential lapse. `join-token-not-admitted` also
comes when the verb is told another host id than the one the agent registered with (a
`--host-id-from-facts` on a host whose agent registered with `--host-id`). Standard error then
says what was sent and for which host:

```text
js-agent cordon: credential error: cannot cordon the host: the server refused the credential (join-token-not-admitted)
js-agent cordon: it sent the host credential in /var/lib/js-agent/state/host-credential for host <id>; to cordon with what the agent on this host presents, give the agent's flags file with --flags-file (such as /etc/js-agent/flags)
```

Run it again with `--flags-file` as above: the verb then takes the host id and the credential from
the agent's own flags, so it asks for the host the agent registers, with what the agent presents.

It lifts only a cordon it placed. The server cannot say who placed a cordon, and an uncordon lifts
any, so the verb relies on the `already_cordoned` flag of its first reply: when that says `true`,
someone else holds a cordon, the verb prints `kept`, renews nothing, lifts nothing and exits 3. (That
first call extended the other holder's cordon to at least `--lease-secs` from now; it never
shortens one.) It also lifts nothing when a reply names a host other than the one it cordoned.

Two limits come from there being one cordon per host and no record of who placed it:

- A cordon that someone else places during the window (an admin's request, or a tool that does
  not look at `already_cordoned`) merges with the verb's, and the verb lifts it at the end of its
  window. Only its first reply can tell it that the host was cordoned before it.
- If the process is stopped (a suspended process, a frozen machine) for longer than
  `--lease-secs`, its cordon lapses. The next renewal then says again whose the cordon is: one that
  was there already is another holder's, and the verb prints `kept host=<id>
  reason=taken_after_lapse`, lifts nothing and exits 3; one that the renewal placed is the verb's
  again, said with a new `cordoned` line, and held and lifted as before. The verb counts its cordon from when its request left and the server from when it arrived, so
  a renewal stalled only just past `--lease-secs` can find the verb's own cordon still in force and
  call it `taken_after_lapse`: an exit 3 after a stall may leave the verb's own cordon, which lapses
  by itself within `--lease-secs`.

## What it prints

One line per state change on standard output, to be read by a script:

| line | when |
|---|---|
| `cordoned host=<id> expires_in_ms=<n> leases=<n>` | the cordon was placed |
| `idle host=<id>` | once: the host reads no lease (`lease` absent, `leases` 0), is not `releasing`, and has no `mark` |
| `lifted host=<id>` | the cordon was lifted |
| `lapsed host=<id>` | the cordon had lapsed already at the end, nothing was left to lift |
| `kept host=<id> reason=already_cordoned` | another holder's cordon was found |
| `kept host=<id> reason=taken_after_lapse` | the verb's cordon lapsed and another holder's took its place |

`idle` means what a script's drain gate checks on `GET /v1/hosts`: `lease` null, `releasing` false
and `mark` null. Until it is printed the verb reads the host every 5 seconds, so a script that waits
for it is told within seconds of the last lease ending.

| exit status | meaning |
|---|---|
| 0 | held to the end (or a stop) and lifted, or had lapsed |
| 1 | a server or credential error; the message on standard error says which and never holds the credential. A cordon the verb placed is lifted if the server can still be asked |
| 2 | a wrong flag or a file it names |
| 3 | another holder's cordon was found and is left in place |

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
