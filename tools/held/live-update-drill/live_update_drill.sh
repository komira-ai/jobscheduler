#!/usr/bin/env bash
# The live update drill of js-server (docs/runbooks/live-update-drill.md), by hand only: it
# updates the Swarm service of the server with `docker service update --force` while a real
# build runs on the farm, and checks that the update lost nothing: no job ended lost, failed or
# failed to start while it ran, and every host that took jobs before it takes jobs after it.
# The service runs two tasks (`--replicas`), updated `order: start-first`, `parallelism: 1`,
# `failure_action: rollback`, so the one update rolls the tasks one at a time: each new task
# turns ready, then its old task gets SIGTERM. The drill also checks that both tasks were
# replaced and that the service's update ended `completed` (a rollback does not pass).
# It is a held step: owner-only, run on the manager node of the Swarm.
#
# It is a dry run by default. A dry run reads the hosts and the running jobs through the job
# service, reads the service's spec and running tasks (read-only), prints what it found and
# the update it would make, and changes nothing. It updates the service only with `--update`,
# and refuses to when no job is running (the drill needs a build under way), when no host
# takes jobs, when the spec is not the one above with a stop grace period of at least 40 s, or
# when the running tasks are not exactly `--replicas`, all Running. It never forces anything
# but the update it is for, and removes no file.
#
# Usage:
#   live_update_drill.sh --server URL --token-file F --out-dir D [--ca-cert F]
#                        [--service NAME] [--replicas N] [--settle-samples N]
#                        [--interval SECONDS] [--update]
#
# Every reading goes to --out-dir, a directory that must exist (the workspace scratch
# directory): the service and its tasks before and after, the hosts before, each sample during
# the update, the update's output and the jobs that ended. It needs `js`, `docker` and `jq` on PATH.
set -euo pipefail

usage() {
  cat >&2 <<'USAGE'
usage: live_update_drill.sh --server URL --token-file F --out-dir D [--ca-cert F]
                            [--service NAME] [--replicas N] [--settle-samples N]
                            [--interval SECONDS] [--update]
  dry run by default; it updates the service only with --update, while a build runs
  --service         the Swarm service of the server (js-server_server)
  --replicas        how many tasks the service runs, rolled one at a time (2)
  --settle-samples  how many readings, --interval apart, the hosts get to take jobs again
                    after the update (30)
  --interval        seconds between two readings of the hosts (2)
USAGE
  exit 2
}

fail() {
  printf 'live_update_drill.sh: %s\n' "$1" >&2
  usage
}

server=''
token_file=''
ca_cert=''
out_dir=''
service='js-server_server'
replicas=2
settle_samples=30
interval=2
update=0

while [ "$#" -gt 0 ]; do
  case "$1" in
    --help | -h)
      usage
      ;;
    --update)
      update=1
      shift
      ;;
    --server | --token-file | --ca-cert | --out-dir | --service | --replicas | --settle-samples | --interval)
      [ "$#" -ge 2 ] || fail "$1 needs a value"
      case "$1" in
        --server) server=$2 ;;
        --token-file) token_file=$2 ;;
        --ca-cert) ca_cert=$2 ;;
        --out-dir) out_dir=$2 ;;
        --service) service=$2 ;;
        --replicas) replicas=$2 ;;
        --settle-samples) settle_samples=$2 ;;
        --interval) interval=$2 ;;
      esac
      shift 2
      ;;
    *)
      fail "unknown argument: $1"
      ;;
  esac
done

[ -n "$server" ] || fail "--server is required"
[ -n "$token_file" ] || fail "--token-file is required"
[ -n "$out_dir" ] || fail "--out-dir is required"
[ -d "$out_dir" ] || fail "--out-dir $out_dir is not a directory"
[[ $service =~ ^[A-Za-z0-9_.-]{1,63}$ ]] || fail "--service is a Swarm service name"
[[ $replicas =~ ^[1-9][0-9]{0,2}$ ]] || fail "--replicas is a count from 1"
[[ $settle_samples =~ ^[1-9][0-9]{0,3}$ ]] || fail "--settle-samples is a count from 1"
[[ $interval =~ ^[1-9][0-9]{0,2}$ ]] || fail "--interval is a whole number of seconds from 1"

js_flags=(--server "$server" --token-file "$token_file" --json)
if [ -n "$ca_cert" ]; then
  js_flags+=(--ca-cert "$ca_cert")
fi

# The hosts that take jobs now, one id per line in id order: heard, not tainted, not
# cordoned and not set aside by a mark.
placeable() {
  js "${js_flags[@]}" host list | jq -r '
    .hosts[]?
    | select(.state == "HOST_STATE_LIVE")
    | select((.taintReason // "") == "")
    | select(.cordonExpiresAtMs == null)
    | select((.mark // "HOST_MARK_UNSPECIFIED") == "HOST_MARK_UNSPECIFIED")
    | .id' | sort
}

# Every job in one of the states given, one JSON object per line, every page.
jobs_in() {
  local token='' page
  local states=()
  local state
  for state in "$@"; do
    states+=(--states "$state")
  done
  while :; do
    local paging=(--page-size 500)
    if [ -n "$token" ]; then
      paging+=(--page-token "$token")
    fi
    page=$(js "${js_flags[@]}" job list "${states[@]}" "${paging[@]}")
    jq -c '.jobs[]?' <<<"$page"
    token=$(jq -r '.nextPageToken // ""' <<<"$page")
    [ -n "$token" ] || return 0
  done
}

placeable >"$out_dir/hosts-before.txt"
# `wc -l` on macOS pads its count with spaces; `$((...))` makes each count a bare number.
hosts_before=$(wc -l <"$out_dir/hosts-before.txt")
hosts_before=$((hosts_before))
running_before=$(jobs_in running | wc -l)
running_before=$((running_before))
printf 'hosts taking jobs: %s (in %s)\n' "$hosts_before" "$out_dir/hosts-before.txt"
printf 'jobs running: %s\n' "$running_before"

# The service's spec, read-only: the drill needs the replicas it was told, a start-first
# update one task at a time that rolls back, and a stop grace period that covers the drain
# (--drain-hold 15 s + --drain-timeout 10 s + the final tick, rounded to 40 s).
min_grace_ns=40000000000
spec_ok=1
spec_json="$out_dir/service-before.json"
spec_field() {
  jq -r "$1"' // "none"' "$spec_json" 2>/dev/null || printf 'none'
}
# spec_check LABEL GOT WANT OK: one finding; OK is 1 when GOT is acceptable.
spec_check() {
  if [ "$4" -eq 1 ]; then
    printf 'service %s: %s (ok)\n' "$1" "$2"
  else
    printf 'service %s: %s (want %s): FAIL\n' "$1" "$2" "$3"
    spec_ok=0
  fi
}
if docker service inspect --format '{{json .}}' "$service" >"$spec_json"; then
  got=$(spec_field '.Spec.Mode.Replicated.Replicas')
  ok=0
  [ "$got" = "$replicas" ] && ok=1
  spec_check replicas "$got" "$replicas" "$ok"
  got=$(spec_field '.Spec.UpdateConfig.Order')
  ok=0
  [ "$got" = start-first ] && ok=1
  spec_check update-order "$got" start-first "$ok"
  got=$(spec_field '.Spec.UpdateConfig.Parallelism')
  ok=0
  [ "$got" = 1 ] && ok=1
  spec_check update-parallelism "$got" 1 "$ok"
  got=$(spec_field '.Spec.UpdateConfig.FailureAction')
  ok=0
  [ "$got" = rollback ] && ok=1
  spec_check update-failure-action "$got" rollback "$ok"
  got=$(spec_field '.Spec.TaskTemplate.ContainerSpec.StopGracePeriod')
  ok=0
  if [[ $got =~ ^[0-9]{1,18}$ ]] && [ "$got" -ge "$min_grace_ns" ]; then ok=1; fi
  spec_check stop-grace-period-ns "$got" "at least $min_grace_ns" "$ok"
else
  printf 'service %s: cannot be read: FAIL\n' "$service"
  spec_ok=0
fi

# The tasks that run now, to tell after the update that each was replaced.
tasks_ok=1
tasks_ps() {
  docker service ps --filter desired-state=running \
    --format '{{.ID}} {{.CurrentState}}' "$service"
}
if tasks_ps >"$out_dir/tasks-before.txt"; then
  tasks_before=$(wc -l <"$out_dir/tasks-before.txt")
  tasks_before=$((tasks_before))
  tasks_idle=$(awk '$2 !~ /^Running/' "$out_dir/tasks-before.txt" | wc -l)
  tasks_idle=$((tasks_idle))
  if [ "$tasks_before" -ne "$replicas" ] || [ "$tasks_idle" -ne 0 ]; then
    tasks_ok=0
  fi
else
  tasks_before='unreadable'
  tasks_ok=0
fi
printf 'tasks running: %s (want %s, all Running)\n' "$tasks_before" "$replicas"
printf 'update: docker service update --force --detach=false %s\n' "$service"

if [ "$update" -eq 0 ]; then
  printf 'dry run: nothing was changed; run again with --update while a build runs\n'
  exit 0
fi
if [ "$running_before" -eq 0 ]; then
  printf 'live_update_drill.sh: no job is running: start the build first\n' >&2
  exit 2
fi
if [ "$hosts_before" -eq 0 ]; then
  printf 'live_update_drill.sh: no host takes jobs: the farm is not serving\n' >&2
  exit 2
fi

if [ "$spec_ok" -eq 0 ]; then
  printf 'live_update_drill.sh: the service is not as the drill needs it (see above)\n' >&2
  exit 2
fi
if [ "$tasks_ok" -eq 0 ]; then
  printf 'live_update_drill.sh: the running tasks are not %s, all Running\n' "$replicas" >&2
  exit 2
fi

started_ms=$(($(date +%s) * 1000))
# `--force` is Docker's flag to roll the service's task when nothing in its spec changed: the
# update is what the drill exercises, and it forces nothing else.
docker service update --force --detach=false "$service" >"$out_dir/update.log" 2>&1 &
updating=$!
sample=0
while kill -0 "$updating" 2>/dev/null; do
  sample=$((sample + 1))
  placeable >"$out_dir/hosts-during-$sample.txt" || true
  sleep "$interval"
done
updated=0
wait "$updating" || updated=$?
if [ "$updated" -ne 0 ]; then
  printf 'FAIL: the update ended with status %s (see %s)\n' "$updated" "$out_dir/update.log"
  exit 1
fi
printf 'the update converged after %s readings of the hosts\n' "$sample"

# Every task must be new, and the service's own update must have completed (a rollback ends
# `rollback_completed`).
tasks_ps >"$out_dir/tasks-after.txt" || true
docker service inspect --format '{{json .}}' "$service" >"$out_dir/service-after.json" || true
tasks_after=$(wc -l <"$out_dir/tasks-after.txt")
tasks_after=$((tasks_after))
tasks_kept="$out_dir/tasks-kept.txt"
awk 'NR == FNR { old[$1] = 1; next } ($1 in old)' \
  "$out_dir/tasks-before.txt" "$out_dir/tasks-after.txt" >"$tasks_kept"
update_state=$(jq -r '.UpdateStatus.State // "none"' "$out_dir/service-after.json" 2>/dev/null ||
  printf 'none')

# The hosts get --settle-samples readings to take jobs again: each agent's next heartbeat
# reaches a new task, which serves its session from the host's row.
missing="$out_dir/hosts-missing.txt"
for ((reading = 1; reading <= settle_samples; reading++)); do
  placeable >"$out_dir/hosts-after.txt"
  comm -23 "$out_dir/hosts-before.txt" "$out_dir/hosts-after.txt" >"$missing"
  [ -s "$missing" ] || break
  sleep "$interval"
done

jobs_in lost failed failed-to-start |
  jq -c --argjson since "$started_ms" \
    'select(((.times.endedAtMs // "0") | tonumber) >= $since) | {id, state}' \
    >"$out_dir/jobs-ended-badly.txt"

verdict=0
if [ -s "$missing" ]; then
  printf 'FAIL: hosts that took jobs before the update and do not now:\n'
  sed 's/^/  /' "$missing"
  verdict=1
fi
if [ -s "$out_dir/jobs-ended-badly.txt" ]; then
  printf 'FAIL: jobs that ended lost, failed or failed to start during the update:\n'
  sed 's/^/  /' "$out_dir/jobs-ended-badly.txt"
  verdict=1
fi
if [ "$tasks_after" -ne "$replicas" ] || [ -s "$tasks_kept" ]; then
  printf 'FAIL: the update did not replace every task\n'
  printf '  running tasks: %s (want %s); kept from before:\n' "$tasks_after" "$replicas"
  sed 's/^/  /' "$tasks_kept"
  verdict=1
fi
if [ "$update_state" != completed ]; then
  printf 'FAIL: the service update ended %s, not completed (see %s)\n' \
    "$update_state" "$out_dir/service-after.json"
  verdict=1
fi
if [ "$verdict" -eq 0 ]; then
  printf 'PASS: every host takes jobs again and no job ended lost, failed or failed to start\n'
fi
exit "$verdict"
