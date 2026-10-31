# Keeping leases off one node for a window (`js-agent cordon`)

A measurement needs a node that no lease lands on for an hour or two, and that goes back into
service afterwards. The lease server lets a host cordon itself: while a cordon is in force the host
takes no new lease (a lease it holds, and its runs, go on). `js-agent cordon` is the client for it,
for an operator's script, on Linux and on macOS.

## What it does

```text
js-agent cordon --server <url> --state-root <dir> --for 2h
```

1. It cordons the host with the host's own credential (the one the agent keeps in its state root,
   or `--token-file` for a host that has not swapped its one-time token yet). The pool's shared join
   token is refused by the server on these routes.
2. It renews the cordon every third of `--lease-secs` (default 300, at most 3600) until `--for` has
   passed. The cordon is a lease the verb renews: a server that is not asked again lets it lapse.
3. It lifts the cordon at the end, or at once on SIGTERM or SIGINT.

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
  again, said with a new `cordoned` line, and held and lifted as before.

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
js-agent cordon --server https://leases.example:7300 --state-root /var/lib/js-agent \
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
