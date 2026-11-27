#!/usr/bin/env bash
# Measures the SubmitJob round trip as the build service's frontends see it
# (docs/runbooks/submit-rollout.md): the `rpc` step of their histogram
# `kbf_executor_step_seconds`, the time from the frontend's call of SubmitJob to the job service's
# answer. It reads the histogram of every frontend on this node twice, a window apart, and gives
# the p50 and p90 of the submits in between, from the difference of the two readings.
#
# It only reads: `docker ps`, `docker inspect` and `docker network inspect` for the containers,
# and an HTTP GET of each frontend's metrics port at its address on the node's `docker_gwbridge`
# network. Run it on the Swarm node that runs the frontends and the js-server task. It changes
# nothing and needs no root, only a user that may run `docker`.
#
# Usage:
#   submit_rpc.sh snapshot FILE [--port N] [--frontend-service NAME] [--js-service NAME]
#   submit_rpc.sh compute FIRST SECOND
#   submit_rpc.sh measure --out-dir D [--minutes N] [--port N] [--frontend-service NAME]
#                         [--js-service NAME]
#
# `snapshot` writes one reading to FILE (`-` is standard output): the time, the js-server
# containers with their version and start time, and the `rpc` lines of each frontend's
# histogram. `compute` prints the numbers of the window between two readings. `measure` is a
# snapshot to D/first.txt, a wait of --minutes (30), a snapshot to D/second.txt, and `compute`
# of the two (also written to D/result.txt).
#
# `compute` refuses (exit 1) a window over which the js-server containers changed (a deploy or a
# restart: the window then measures two servers), one with no js-server container in either
# reading, and one with no submit. A frontend that is in
# one reading only, or whose counts went down (it restarted), is left out and named.
#
# The quantile is Prometheus's histogram_quantile: linear inside the bucket that holds the rank, from
# 0 for the first bucket. So it is as coarse as the buckets the frontends export, which the script
# reads from the readings; a quantile past the last finite bucket is printed as `>` that bound.
set -euo pipefail

usage() {
  cat >&2 <<'USAGE'
usage: submit_rpc.sh snapshot FILE [--port N] [--frontend-service NAME] [--js-service NAME]
       submit_rpc.sh compute FIRST SECOND
       submit_rpc.sh measure --out-dir D [--minutes N] [--port N] [--frontend-service NAME]
                             [--js-service NAME]
  read-only; run it on the node that runs the frontends and the js-server task
  --port              the frontends' metrics port (8982)
  --frontend-service  the Swarm service of the frontends (kbf-frontend_server)
  --js-service        the Swarm service of the job server (js-server_server)
  --minutes           the window of `measure` (30)
USAGE
  exit 2
}

fail() {
  printf 'submit_rpc.sh: %s\n' "$1" >&2
  usage
}

port=8982
frontend_service='kbf-frontend_server'
js_service='js-server_server'
minutes=30
out_dir=''

# Reads the flags shared by `snapshot` and `measure`; `measure` also takes --out-dir, --minutes.
parse_flags() {
  local verb=$1
  shift
  while [ "$#" -gt 0 ]; do
    case "$1" in
      --port | --frontend-service | --js-service | --out-dir | --minutes)
        [ "$#" -ge 2 ] || fail "$1 needs a value"
        if [ "$verb" = snapshot ] && { [ "$1" = --out-dir ] || [ "$1" = --minutes ]; }; then
          fail "snapshot does not take $1"
        fi
        case "$1" in
          --port) port=$2 ;;
          --frontend-service) frontend_service=$2 ;;
          --js-service) js_service=$2 ;;
          --out-dir) out_dir=$2 ;;
          --minutes) minutes=$2 ;;
        esac
        shift 2
        ;;
      *)
        fail "unknown argument: $1"
        ;;
    esac
  done
  if ! [[ "$port" =~ ^[1-9][0-9]{0,4}$ ]] || [ "$port" -gt 65535 ]; then
    fail "--port must be 1-65535: $port"
  fi
  if ! [[ "$minutes" =~ ^[1-9][0-9]{0,3}$ ]]; then
    fail "--minutes must be a whole number from 1 to 9999: $minutes"
  fi
  [ -n "$frontend_service" ] || fail '--frontend-service must not be empty'
  [ -n "$js_service" ] || fail '--js-service must not be empty'
}

# One reading, to standard output.
#   # at <UTC time> <unix seconds>
#   js <container id> <version> <started at>      (one per js-server container, sorted)
#   fe <container id>                             (then that frontend's rpc lines)
read_once() {
  local id ip line metrics cid address
  printf '# at %s %s\n' "$(date -u +%Y-%m-%dT%H:%M:%SZ)" "$(date -u +%s)"
  docker ps --no-trunc --filter "name=$js_service" --format '{{.ID}}' | while IFS= read -r id; do
    [ -n "$id" ] || continue
    printf 'js %s %s\n' "$id" \
      "$(docker inspect --format '{{index .Config.Labels "org.opencontainers.image.version"}} {{.State.StartedAt}}' "$id")"
  done | sort
  # The address of each container on docker_gwbridge, by full container id.
  local addresses
  addresses=$(docker network inspect docker_gwbridge \
    --format '{{range $id, $c := .Containers}}{{$id}} {{$c.IPv4Address}}{{println}}{{end}}')
  docker ps --no-trunc --filter "name=$frontend_service" --format '{{.ID}}' | sort | while IFS= read -r id; do
    [ -n "$id" ] || continue
    ip=''
    while IFS=' ' read -r cid address; do
      [ "$cid" = "$id" ] && ip=${address%/*}
    done <<< "$addresses"
    if [ -z "$ip" ]; then
      printf '# no docker_gwbridge address for frontend %s\n' "$id"
      continue
    fi
    if ! metrics=$(curl -fsS --max-time 10 "http://$ip:$port/metrics"); then
      printf '# frontend %s did not answer\n' "$id"
      continue
    fi
    printf 'fe %s\n' "$id"
    while IFS= read -r line; do
      case "$line" in
        'kbf_executor_step_seconds_'*'step="rpc"'*) printf '%s\n' "$line" ;;
      esac
    done <<< "$metrics"
  done
}

snapshot() {
  local file=$1
  if [ "$file" = - ]; then
    read_once
  else
    read_once > "$file"
  fi
}

compute() {
  local first=$1 second=$2
  [ -f "$first" ] || fail "no such reading: $first"
  [ -f "$second" ] || fail "no such reading: $second"
  local program
  program=$(cat <<'AWK'
function label(s, key,   r) {
  r = s
  if (index(r, key "=\"") == 0) return ""
  sub(".*" key "=\"", "", r)
  sub("\".*", "", r)
  return r
}
function bound(le) { return le == "+Inf" ? 1e300 : le + 0 }
function ms(seconds) { return sprintf("%.1f", seconds * 1000) }
# histogram_quantile over the aggregate `key` ("" is every form): linear inside the bucket.
function quantile(key, q,   total, rank, i, cum, prev, lo, hi) {
  total = agg[key, "+Inf"]
  rank = q * total
  prev = 0
  lo = 0
  for (i = 1; i <= n; i++) {
    cum = agg[key, les[i]]
    if (cum >= rank) {
      if (les[i] == "+Inf") return "> " ms(lo) " ms"
      hi = bound(les[i])
      return ms(lo + (hi - lo) * (rank - prev) / (cum - prev)) " ms"
    }
    prev = cum
    lo = bound(les[i])
  }
  return "> " ms(lo) " ms"
}
function line_of(key, name,   c) {
  c = agg[key, "+Inf"] + 0
  if (c == 0) return name ": count 0"
  return sprintf("%s: count %d (%.2f/s), p50 %s, p90 %s, mean %s ms", name, c, c / span,
    quantile(key, 0.5), quantile(key, 0.9), ms(sum[key] / c))
}
FNR == 1 { f++ }
/^# at / { at[f] = $3; epoch[f] = $4; next }
/^#/ { next }
$1 == "js" { js[f] = js[f] $2 " " $3 " " $4 "\n"; version[f] = $3; next }
$1 == "fe" { cur = $2; fe[f, cur] = 1; ids[cur] = 1; next }
/^kbf_executor_step_seconds_bucket/ {
  form = label($0, "form"); le = label($0, "le")
  forms[form] = 1; seen_le[le] = 1
  b[f, cur, form, le] = $NF; next
}
/^kbf_executor_step_seconds_sum/ { s[f, cur, label($0, "form")] = $NF; next }
END {
  if (f != 2) { print "submit_rpc.sh: compute needs two readings" > "/dev/stderr"; exit 1 }
  if (js[1] == "" && js[2] == "") {
    print "submit_rpc.sh: no js-server container in either reading: the window cannot say which server it measured" > "/dev/stderr"
    exit 1
  }
  if (js[1] != js[2]) {
    printf "submit_rpc.sh: the js-server containers changed during the window (a deploy or a restart):\n%s--\n%s", js[1], js[2] > "/dev/stderr"
    exit 1
  }
  span = epoch[2] - epoch[1]
  if (span <= 0) { print "submit_rpc.sh: the second reading is not later than the first" > "/dev/stderr"; exit 1 }
  n = 0
  for (le in seen_le) les[++n] = le
  for (i = 2; i <= n; i++) {
    v = les[i]
    for (j = i - 1; j >= 1 && bound(les[j]) > bound(v); j--) les[j + 1] = les[j]
    les[j + 1] = v
  }
  kept = 0; dropped = ""; m = 0
  for (id in ids) order[++m] = id
  for (i = 2; i <= m; i++) {
    v = order[i]
    for (j = i - 1; j >= 1 && order[j] > v; j--) order[j + 1] = order[j]
    order[j + 1] = v
  }
  for (k = 1; k <= m; k++) {
    id = order[k]
    short = substr(id, 1, 12)
    if (!((1, id) in fe) || !((2, id) in fe)) { dropped = dropped " " short "(one-reading)"; continue }
    bad = 0
    for (form in forms) for (i = 1; i <= n; i++)
      if (b[2, id, form, les[i]] - b[1, id, form, les[i]] < 0) bad = 1
    if (bad) { dropped = dropped " " short "(restarted)"; continue }
    kept++
    for (form in forms) {
      for (i = 1; i <= n; i++) {
        d = b[2, id, form, les[i]] - b[1, id, form, les[i]]
        agg[form, les[i]] += d
        agg["", les[i]] += d
      }
      d = s[2, id, form] - s[1, id, form]
      sum[form] += d
      sum[""] += d
    }
  }
  printf "window: %s to %s (%d s)\n", at[1], at[2], span
  printf "js-server: %s\n", version[1]
  printf "frontends: %d kept, dropped:%s\n", kept, (dropped == "" ? " none" : dropped)
  if (agg["", "+Inf"] + 0 == 0) {
    print line_of("", "rpc")
    print "submit_rpc.sh: no SubmitJob in the window" > "/dev/stderr"
    exit 1
  }
  print line_of("", "rpc")
  nf = 0
  for (form in forms) names[++nf] = form
  for (i = 2; i <= nf; i++) {
    v = names[i]
    for (j = i - 1; j >= 1 && names[j] > v; j--) names[j + 1] = names[j]
    names[j + 1] = v
  }
  for (i = 1; i <= nf; i++) print line_of(names[i], "rpc form=" names[i])
}
AWK
  )
  awk "$program" "$first" "$second"
}

[ "$#" -ge 1 ] || usage
verb=$1
shift
case "$verb" in
  --help | -h)
    usage
    ;;
  snapshot)
    [ "$#" -ge 1 ] || fail 'snapshot needs a FILE'
    file=$1
    shift
    parse_flags snapshot "$@"
    snapshot "$file"
    ;;
  compute)
    [ "$#" -eq 2 ] || fail 'compute needs FIRST and SECOND'
    compute "$1" "$2"
    ;;
  measure)
    parse_flags measure "$@"
    [ -n "$out_dir" ] || fail '--out-dir is required'
    [ -d "$out_dir" ] || fail "--out-dir is not a directory: $out_dir"
    snapshot "$out_dir/first.txt"
    printf 'first reading: %s; waiting %s minutes\n' "$out_dir/first.txt" "$minutes"
    sleep "$((minutes * 60))"
    snapshot "$out_dir/second.txt"
    compute "$out_dir/first.txt" "$out_dir/second.txt" | tee "$out_dir/result.txt"
    ;;
  *)
    fail "unknown verb: $verb"
    ;;
esac
