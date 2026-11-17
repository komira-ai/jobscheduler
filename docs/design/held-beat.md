# Place on submit, start on the held heartbeat

Design issue: komira-ai/jobscheduler#903. Goal: a job submitted while a host has room starts on that
host without waiting for a server tick or a heartbeat interval.

## Before

A submit committed the job as queued and answered. The tick's pass (`Shared::tick`, then
`Agents::place_jobs`) or a heartbeat's pass placed it, and the host learned of the start in the reply to
its next heartbeat (`starts_for`, `per_job.rs`). Submit to start took up to one tick plus one heartbeat
interval: up to 10 s with the defaults (`--tick 5s`, interval 5 s), up to 2 s at 1 s each.

## After

1. **Place in the call.** `SubmitJob` reads the queue before it commits. When no queued job has the new
   job's priority or a higher one, the call runs the server's pass over the queued jobs after the commit
   and answers the job as it then stands: `placed` with its host, or `queued` when no host had room. The
   pass is the one the tick runs: it claims the host's room in the store's own transaction
   (`place_claimed`), so it stays right with several servers, and one pass of a server runs at a time
   (`Agents::placing`). A job with a queued job of its priority or above ahead of it, and a retry of a job
   already recorded, is only committed; the next pass places it, as before.
2. **Hold the heartbeat.** An agent that reports the hello capability `held-beat` is one whose session
   sends its next heartbeat at once after a reply that says `beat_now`. The server watches such a host's
   heartbeat from before it reads the store for the reply (`held_beat::Wakes::watch`). When the reply has
   nothing to send (no command, acknowledgement, taint acknowledgement or log credential), the route holds
   it open for the pool's heartbeat interval, at most 5 s, and answers it with `beat_now`. Everything the
   beat reported was committed and counted before the hold.
3. **Wake on placement.** Every placement a pass commits wakes the watched heartbeat of its host
   (`JobStarts::placed`). The held reply's stops and starts are built again from the store over the
   listing the beat brought (`per_job::commands_now`) and the reply is answered at once with the start (or,
   for a job that books the whole host, with the allocation to prepare for). A wake that comes between the
   store read and the hold is kept, so none is lost. A wake that brings nothing to send holds on.

Submit to start on an idle host is then one pass, one store read and one reply, with no tick and no
interval in it. The agent beats again as soon as a held reply reaches it, so a heartbeat of the host is
waiting at the server nearly all the time.

## Compatibility

- `beat_now` is an optional reply field, absent when false; an agent from before it ignores it. A server
  sends it, and holds a reply, only to an agent that reported `held-beat`, so an older agent, which waits
  the interval after every reply, never has its interval stretched. A newer agent with an older server
  never sees `beat_now` and waits the interval as before. Tested both ways in
  `crates/js-agent-wire/tests/previous_release_v0_1_208.rs`
  (`a_held_reply_reads_in_the_v0_1_208_structs_and_an_old_reply_is_not_held`).
- Servers upgrade before agents (the wire's rule); the order does not matter here.

## Failure modes

- **Several servers.** The wakes are the process's own. A placement made by another server than the one
  holding the host's heartbeat is delivered on the host's next heartbeat, at most one hold later.
- **Silence.** A held heartbeat is recorded when it arrives, and the next is sent as soon as its reply
  returns, so beats arrive at most one interval apart, as before; the grace is unchanged.
- **Connections.** A held heartbeat keeps its connection for up to the hold, so each host holds about
  one connection of `max_connections` (256 by default) at all times. A fleet that nears it raises
  `--max-connections` or gives agents their own port (`server.agent_listen`).
- **Stop.** A held reply ends within the hold (at most 5 s), inside the default `--drain-timeout` of
  10 s.
- **Store read fails while held.** The reply is answered as it was (empty, `beat_now`); the agent's next
  heartbeat, sent at once, is served in full and answers `state-write-failed` if the store still fails.
- **Reports during a hold.** A job's state change on the host reaches the server in the agent's next
  heartbeat, which leaves when the held reply returns: at most one hold later, as it was at most one
  interval later before.
