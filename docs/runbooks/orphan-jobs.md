# Jobs a host runs that the job store does not know

A host lists every job it runs in each heartbeat. When the job store holds no record of a listed job
(no placement of it on that host, no end, no cancel, and no earlier attempt of it there), the server
does not tell the host to stop it: the store may be the one that is wrong. That happens after a restore
of an older store, a failed migration, a server pointed at the wrong database, or a store copy that is
behind; a server that stopped such jobs would stop every job of the farm within one heartbeat. Each
such job is an *orphan* of its host:

- the heartbeat reply names it in `unknown_jobs`, and the agent keeps it running and logs a line
  `job: UnknownToServer { run_ids: [...] }` when the list changes;
- the host's row keeps it with the time a heartbeat first listed it, so its age is the same on every
  server over the store;
- the host takes no new job while it lists one, so the work the store does not count is never booked
  over;
- it ends by itself at its own wall limit on the agent; if it is still listed after `--max-job-wall`
  (`server.max_job_wall`, 30 days unless set), the server tells the host to stop it (the stop's reason
  is `orphan_expired`), one orphan of a host at a time.

The jobs the store does hold records of are stopped for those records, each stop carrying its reason
(`cancelled`, `ended`, `replaced`), and the agent logs the reason (`job: StopReasoned { ... }`).

## 1. See the orphans

`js_unknown_jobs_found_total{host="..."}` counts each orphan once, when a heartbeat first lists it. To
see a host's orphans and how long it has listed each:

```
curl -s -H "Authorization: Bearer $(cat client-token)" \
  https://js.example.ts.net:<port>/v1/hosts/host-a/orphans
```

```
{"host": "host-a", "orphans": [{"job_id": "j1", "listed_for_secs": 42, "end_asked": false}]}
```

A host the store holds no row of is `404 unknown-host`.

## 2. When most of a host's work is unknown

When more than half of what a host lists is orphans, the server tells that host to stop nothing at all,
not even a job the store records a cancel of: the store is more likely wrong than the host. Each such
heartbeat counts in `js_unknown_listing_guarded_total{host="..."}`, and the server logs an error the
first time (`more than half of what the host lists is unknown to the store`). Alert on any rise of that
counter. It means the store lost or rolled back its tables, or the server reads the wrong store: check
`--store` and the last restore (`docs/runbooks/state-database.md`) before anything else. Once the store
is right again, the jobs it holds are judged by its records on the next heartbeat.

## 3. End an orphan

When the store is right and the job is work nobody wants (a job a client cancelled before a restore,
say), end it on its host:

```
curl -s -X POST -H "Authorization: Bearer $(cat client-token)" \
  https://js.example.ts.net:<port>/v1/hosts/host-a/orphans/j1/end
```

```
{"host": "host-a", "job_id": "j1", "end": "asked"}
```

The `202` means the end is asked: the host's row marks it, and every heartbeat of the host after that,
on whichever server takes it, tells the host to stop the job (reason `operator`) while it lists it,
whatever share of its listing is unknown. The listing shows `"end_asked": true` until the host has
stopped the job. Asking again is harmless. A job that is not one of the host's orphans is
`404 unknown-orphan`; `409 row-moved` means a heartbeat of the host wrote its row first, so ask again;
`503 state-write-failed` means the store could not be read or written.
