#!/usr/bin/env bash
# The live update drill of js-server (docs/runbooks/live-update-drill.md), by hand only: it
# updates the Swarm service of the server with `docker service update --force` while a real
# build runs on the farm, and checks that the update lost nothing: no job ended lost, failed or
# failed to start while it ran, and every host that took jobs before it takes jobs after it.
# It is a held step: owner-only, run on the manager node of the Swarm.
#
# It is a dry run by default. A dry run reads the hosts and the running jobs through the job
# service, prints what it found and the update it would make, and changes nothing. It updates
# the service only with `--update`, and refuses to when no job is running (the drill needs a
# build under way). It never forces anything but the update it is for, and removes no file.
#
# Usage:
#   live_update_drill.sh --server URL --token-file F --out-dir D [--ca-cert F]
#                        [--service NAME] [--settle-samples N] [--interval SECONDS] [--update]
#
# Every reading goes to --out-dir, a directory that must exist (the workspace scratch
# directory): the hosts before, each sample during the update, the update's output and the
# jobs that ended. It needs `js`, `docker` and `jq` on PATH.
set -euo pipefail

usage() {
  cat >&2 <<'USAGE'
usage: live_update_drill.sh --server URL --token-file F --out-dir D [--ca-cert F]
                            [--service NAME] [--settle-samples N] [--interval SECONDS]
                            [--update]
  dry run by default; it updates the service only with --update, while a build runs
  --service         the Swarm service of the server (js-server_server)
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
    --server | --token-file | --ca-cert | --out-dir | --service | --settle-samples | --interval)
      [ "$#" -ge 2 ] || fail "$1 needs a value"
      case "$1" in
        --server) server=$2 ;;
        --token-file) token_file=$2 ;;
        --ca-cert) ca_cert=$2 ;;
        --out-dir) out_dir=$2 ;;
        --service) service=$2 ;;
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
hosts_before=$(wc -l <"$out_dir/hosts-before.txt")
running_before=$(jobs_in running | wc -l)
printf 'hosts taking jobs: %s (in %s)\n' "$hosts_before" "$out_dir/hosts-before.txt"
printf 'jobs running: %s\n' "$running_before"
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

# The hosts get --settle-samples readings to take jobs again: each agent says hello to the
# new server once its next heartbeat reaches it.
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
if [ "$verdict" -eq 0 ]; then
  printf 'PASS: every host takes jobs again and no job ended lost, failed or failed to start\n'
fi
exit "$verdict"
