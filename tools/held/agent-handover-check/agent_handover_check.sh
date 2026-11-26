#!/usr/bin/env bash
# The agent hand-over check (docs/runbooks/agent-handover-check.md): it watches named hosts
# through the job service while a js-agent release rolls onto them, and says whether each one
# handed its running jobs to the new agent instead of draining. It is read-only: it runs only
# `js host list`, `js job list` and `js job get`, and changes nothing. It is a held step only
# because it needs a client token of the job service; the roll itself is started elsewhere.
#
# It reads the hosts and the jobs running on them once, then reads the hosts every
# --interval seconds, at most --samples times, until every named host lists an agent version
# other than the one it listed first. Then it reads the running jobs again and asks the
# service about each job that ran before and does not run now on the same host and attempt.
#
# A host passes when it moved to a new version, was never seen cordoned or tainted while it
# moved (an update that drains cordons the host first; one that hands over does not), and every
# job that ran on it before runs on it still in the same attempt or ended succeeded or
# cancelled. The check fails when a host did not move, was seen cordoned or tainted, or a job
# of a named host ended lost, failed or failed to start, was started again, or could not be
# read. A run in which every host passed but one ran no job before ends inconclusive: that
# host's hand-over of running jobs was not shown.
#
# Usage:
#   agent_handover_check.sh --server URL --token-file F --out-dir D [--ca-cert F]
#                           --host ID [--host ID...] [--samples N] [--interval SECONDS]
#
# Every reading goes to --out-dir, a directory that must exist (the workspace scratch
# directory): the hosts and running jobs before, each reading of the hosts (timeline.tsv), the
# running jobs after, and each job's outcome (outcomes.tsv). It needs `js` and `jq` on PATH.
# Exit status: 0 pass, 1 fail, 2 refused (nothing watched), 3 inconclusive.
set -euo pipefail

usage() {
  cat >&2 <<'USAGE'
usage: agent_handover_check.sh --server URL --token-file F --out-dir D [--ca-cert F]
                               --host ID [--host ID...] [--samples N] [--interval SECONDS]
  read-only: it runs js host list, js job list and js job get, and changes nothing
  --host      a host to watch, as js host list names it; give it once per host
  --samples   the most readings of the hosts while the release rolls (360)
  --interval  seconds between two readings of the hosts (20)
USAGE
  exit 2
}

fail() {
  printf 'agent_handover_check.sh: %s\n' "$1" >&2
  usage
}

# refuse MESSAGE: the check cannot start; nothing was watched.
refuse() {
  printf 'agent_handover_check.sh: %s\n' "$1" >&2
  exit 2
}

server=''
token_file=''
ca_cert=''
out_dir=''
samples=360
interval=20
hosts=()

while [ "$#" -gt 0 ]; do
  case "$1" in
    --help | -h)
      usage
      ;;
    --server | --token-file | --ca-cert | --out-dir | --host | --samples | --interval)
      [ "$#" -ge 2 ] || fail "$1 needs a value"
      case "$1" in
        --server) server=$2 ;;
        --token-file) token_file=$2 ;;
        --ca-cert) ca_cert=$2 ;;
        --out-dir) out_dir=$2 ;;
        --host) hosts+=("$2") ;;
        --samples) samples=$2 ;;
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
[ "${#hosts[@]}" -gt 0 ] || fail "--host is required, once per host"
for host in "${hosts[@]}"; do
  [[ $host =~ ^[A-Za-z0-9_.:-]{1,253}$ ]] || fail "--host $host is not a host id"
done
if [ "$(printf '%s\n' "${hosts[@]}" | sort -u | wc -l)" -ne "${#hosts[@]}" ]; then
  fail "a --host is given twice"
fi
[[ $samples =~ ^[1-9][0-9]{0,4}$ ]] || fail "--samples is a count from 1"
[[ $interval =~ ^[1-9][0-9]{0,3}$ ]] || fail "--interval is a whole number of seconds from 1"

js_flags=(--server "$server" --token-file "$token_file" --json)
if [ -n "$ca_cert" ]; then
  js_flags+=(--ca-cert "$ca_cert")
fi
names=$(printf '%s\n' "${hosts[@]}" | jq -R . | jq -cs .)

# Every job in one of the states given, on a named host, one JSON object per line, every page:
# its id, its host, its attempt (0, a job recorded before attempts were counted, reads as 1)
# and when it ended.
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
    page=$(js "${js_flags[@]}" job list "${states[@]}" "${paging[@]}" </dev/null) || return 1
    jq -c --argjson names "$names" '
      .jobs[]?
      | select((.hostId // "") as $h | $names | index($h))
      | {id, host: .hostId, attempt: (if (.attempt // 0) == 0 then 1 else .attempt end),
         state, ended: ((.times.endedAtMs // "0") | tonumber)}' <<<"$page"
    token=$(jq -r '.nextPageToken // ""' <<<"$page")
    [ -n "$token" ] || return 0
  done
}

# row FILE HOST: the host's reading in a listing, tab-separated: its agent version, whether it
# is cordoned, its mark, its state and its taint (`-` for none). `absent` when it is not listed.
row() {
  jq -r --arg h "$2" '
    [.hosts[]? | select(.id == $h)][0]
    | if . == null then "absent"
      else [((.agentVersion // "") | if . == "" then "-" else . end),
            (if .cordonExpiresAtMs == null then "-" else "cordoned" end),
            (.mark // "HOST_MARK_UNSPECIFIED"),
            (.state // "HOST_STATE_UNSPECIFIED"),
            ((.taintReason // "") | if . == "" then "-" else . end)] | @tsv
      end' "$1"
}

# ids FILE HOST: the ids of the jobs of FILE on HOST, space-separated, `none` when there are none.
ids() {
  local list
  list=$(jq -r --arg h "$2" 'select(.host == $h) | .id' "$1" | sort | tr '\n' ' ')
  printf '%s' "${list:-none }" | sed 's/ $//'
}

started_ms=$(($(date +%s) * 1000))
js "${js_flags[@]}" host list >"$out_dir/hosts-before.json" </dev/null ||
  refuse "the hosts cannot be read"
jobs_in running >"$out_dir/running-before.jsonl" ||
  refuse "the running jobs cannot be read"

before=()
for host in "${hosts[@]}"; do
  reading=$(row "$out_dir/hosts-before.json" "$host")
  [ "$reading" != absent ] || refuse "host $host is not listed by the job service"
  version=${reading%%$'\t'*}
  [ "$version" != - ] ||
    refuse "host $host lists no agent version, so whether it moves cannot be told; read the hosts again once its agent has said hello"
  cordon=$(cut -f2 <<<"$reading")
  [ "$cordon" = - ] ||
    refuse "host $host is cordoned already, so a drain could not be told from it; start the check once the cordon is gone"
  before+=("$version")
  printf '%s: agent %s; running before: %s\n' "$host" "$version" \
    "$(ids "$out_dir/running-before.jsonl" "$host")"
done

# The readings while the release rolls: one line per host per reading in timeline.tsv.
timeline="$out_dir/timeline.tsv"
printf 'reading\thost\tversion\tcordon\tmark\tstate\ttaint\n' >"$timeline"
readings=0
for ((reading = 1; reading <= samples; reading++)); do
  readings=$reading
  sample="$out_dir/hosts-reading.json"
  if ! js "${js_flags[@]}" host list >"$sample" </dev/null; then
    printf '%s\t*\tunreadable\t-\t-\t-\t-\n' "$reading" >>"$timeline"
    [ "$reading" -eq "$samples" ] || sleep "$interval"
    continue
  fi
  moved=0
  for i in "${!hosts[@]}"; do
    line=$(row "$sample" "${hosts[$i]}")
    printf '%s\t%s\t%s\n' "$reading" "${hosts[$i]}" "$line" >>"$timeline"
    version=${line%%$'\t'*}
    if [ "$version" != absent ] && [ "$version" != - ] && [ "$version" != "${before[$i]}" ]; then
      moved=$((moved + 1))
    fi
  done
  [ "$moved" -lt "${#hosts[@]}" ] || break
  [ "$reading" -eq "$samples" ] || sleep "$interval"
done
printf 'readings of the hosts: %s (in %s)\n' "$readings" "$timeline"

if ! jobs_in running >"$out_dir/running-after.jsonl"; then
  printf 'FAIL: the running jobs cannot be read after the roll; run the check again\n'
  exit 1
fi

# Each job that ran before: kept (same host, same attempt, running now), ended (succeeded or
# cancelled since), or a failure: lost, failed, failed to start, started again or unreadable.
outcomes="$out_dir/outcomes.tsv"
: >"$outcomes"
while IFS= read -r job; do
  id=$(jq -r .id <<<"$job")
  host=$(jq -r .host <<<"$job")
  attempt=$(jq -r .attempt <<<"$job")
  if jq -e --arg id "$id" --arg h "$host" --argjson a "$attempt" \
    'select(.id == $id and .host == $h and .attempt == $a)' \
    "$out_dir/running-after.jsonl" >/dev/null; then
    printf '%s\t%s\tkept\n' "$host" "$id" >>"$outcomes"
    continue
  fi
  if ! got=$(js "${js_flags[@]}" job get --job-id "$id" </dev/null) ||
    ! jq -e '.job.id' <<<"$got" >/dev/null 2>&1; then
    printf '%s\t%s\tunreadable\n' "$host" "$id" >>"$outcomes"
    continue
  fi
  outcome=$(jq -r --arg h "$host" --argjson a "$attempt" '
    .job
    | (if (.attempt // 0) == 0 then 1 else .attempt end) as $now
    | if .state == "JOB_STATE_SUCCEEDED" or .state == "JOB_STATE_CANCELLED" then "ended"
      elif .state == "JOB_STATE_LOST" then "lost"
      elif .state == "JOB_STATE_FAILED" then "failed"
      elif .state == "JOB_STATE_FAILED_TO_START" then "failed-to-start"
      elif $now != $a or (.hostId // "") != $h then "started-again"
      else "not-running:" + (.state // "JOB_STATE_UNSPECIFIED")
      end' <<<"$got")
  printf '%s\t%s\t%s\n' "$host" "$id" "$outcome" >>"$outcomes"
done <"$out_dir/running-before.jsonl"

# The jobs of the named hosts that ended badly since the check began, before-jobs or not.
if ! jobs_in lost failed failed-to-start >"$out_dir/ended-badly-all.jsonl"; then
  printf 'FAIL: the jobs that ended lost, failed or failed to start cannot be read; run the check again\n'
  exit 1
fi
jq -c --argjson since "$started_ms" 'select(.ended >= $since) | {id, host, state}' \
  "$out_dir/ended-badly-all.jsonl" >"$out_dir/jobs-ended-badly.jsonl"

verdict=0
idle=0
for i in "${!hosts[@]}"; do
  host=${hosts[$i]}
  last=$(awk -F'\t' -v h="$host" '$2 == h { v = $3 } END { print v }' "$timeline")
  if [ -n "$last" ] && [ "$last" != absent ] && [ "$last" != - ] && [ "$last" != "${before[$i]}" ]; then
    printf '%s: moved %s -> %s\n' "$host" "${before[$i]}" "$last"
  else
    printf '%s: FAIL: did not move from %s (last listed: %s)\n' "$host" "${before[$i]}" "${last:-unread}"
    verdict=1
  fi
  cordoned=$(awk -F'\t' -v h="$host" '$2 == h && $4 == "cordoned" { print $1; exit }' "$timeline")
  if [ -n "$cordoned" ]; then
    printf '%s: FAIL: cordoned at reading %s: it drained instead of handing over\n' "$host" "$cordoned"
    verdict=1
  fi
  tainted=$(awk -F'\t' -v h="$host" '$2 == h && $7 != "-" && $7 != "" { print $1 " (" $7 ")"; exit }' "$timeline")
  if [ -n "$tainted" ]; then
    printf '%s: FAIL: tainted at reading %s\n' "$host" "$tainted"
    verdict=1
  fi
  marks=$(awk -F'\t' -v h="$host" '$2 == h && $5 != "HOST_MARK_UNSPECIFIED" && $5 != "" { print $5 }' "$timeline" | sort -u | tr '\n' ' ')
  if [ -n "$marks" ]; then
    printf '%s: marks seen: %s\n' "$host" "${marks% }"
  fi
  unheard=$(awk -F'\t' -v h="$host" '$2 == h && $6 != "HOST_STATE_LIVE" { n++ } END { print n + 0 }' "$timeline")
  if [ "$unheard" -gt 0 ]; then
    printf '%s: not heard as live in %s reading(s)\n' "$host" "$unheard"
  fi
  ran=$(awk -F'\t' -v h="$host" '$1 == h { n++ } END { print n + 0 }' "$outcomes")
  kept=$(awk -F'\t' -v h="$host" '$1 == h && $3 == "kept" { n++ } END { print n + 0 }' "$outcomes")
  ended=$(awk -F'\t' -v h="$host" '$1 == h && $3 == "ended" { n++ } END { print n + 0 }' "$outcomes")
  printf '%s: running before: %s; kept: %s; ended succeeded or cancelled: %s; running after: %s\n' \
    "$host" "$ran" "$kept" "$ended" "$(ids "$out_dir/running-after.jsonl" "$host")"
  if [ "$ran" -eq 0 ]; then
    idle=1
  fi
  while IFS=$'\t' read -r _ id outcome; do
    printf '%s: FAIL: job %s %s\n' "$host" "$id" "$outcome"
    verdict=1
  done < <(awk -F'\t' -v h="$host" '$1 == h && $3 != "kept" && $3 != "ended"' "$outcomes")
done
if [ -s "$out_dir/jobs-ended-badly.jsonl" ]; then
  printf 'FAIL: jobs of these hosts that ended lost, failed or failed to start since the check began:\n'
  sed 's/^/  /' "$out_dir/jobs-ended-badly.jsonl"
  verdict=1
fi
if [ "$verdict" -ne 0 ]; then
  exit 1
fi
if [ "$idle" -ne 0 ]; then
  printf 'INCONCLUSIVE: every host moved and lost nothing, but a host ran no job before, so its hand-over of running jobs was not shown\n'
  exit 3
fi
printf 'PASS: every host moved and handed its running jobs over: none cordoned, tainted, lost, failed or started again\n'
