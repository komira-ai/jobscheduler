#!/usr/bin/env bash
# Reproduces site/test/demo.transcript: builds the lease server and the client, leases a host from a static host file on
# this machine, lists the lease and releases it. It writes the transcript to the file named by the first argument
# (default: demo.transcript in the current directory). Run it from anywhere; it needs cargo and a free port 7300 on
# 127.0.0.1. Lease ids and times differ on every run, so a new recording differs from the committed one in those.
set -euo pipefail

repo="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
out="$(realpath -m "${1:-demo.transcript}")"

cd "$repo"
cargo build --release -p js-lease-server -p js-cli >&2
bin="$repo/target/release"

work="$(mktemp -d)"
server_pid=""
cleanup() {
  [ -z "$server_pid" ] || kill "$server_pid" 2>/dev/null || true
  rm -rf "$work"
}
trap cleanup EXIT
mkdir "$work/bin"
cp "$bin/js-lease-server" "$bin/js" "$work/bin/"
cd "$work"
export PATH="$work/bin:$PATH"

: > "$out"
# Prints a command the way a reader types it, runs it, and prints what it wrote.
show() {
  printf '$ %s\n' "$1" >> "$out"
  { eval "$1"; } >> "$out" 2>&1
}

cat > hosts.toml <<'TOML'
[[host]]
id = "gpu-01"
gpu_class = "nvidia-a100"

[host.resources]
cpu_millis = 64000
memory_bytes = 549755813888
gpus = 8

[host.capabilities]
isa = "x86-64-v3"
driver = "550"
TOML

{ echo '# hosts.toml: one host, listed by hand; there is no agent'; cat hosts.toml; } >> "$out"
show "printf 'a-local-token-of-at-least-32-bytes\n' > client-token && chmod 600 client-token"
printf '$ %s\n' "js-lease-server --state-file leases.toml --boot initialise --token-file client-token --pool-config hosts.toml &" >> "$out"
js-lease-server --state-file leases.toml --boot initialise --token-file client-token --pool-config hosts.toml >server.log 2>&1 &
server_pid=$!
js_args="--server http://127.0.0.1:7300 --token-file client-token"
for _ in $(seq 1 50); do
  js $js_args lease list >/dev/null 2>&1 && break
  sleep 0.2
done

show "js $js_args lease acquire --class gpu-host --gpus 1 --gpu-class nvidia-a100 --max-wall 1h"
id="$(grep -o 'lease [^ ]*' "$out" | tail -1 | cut -d' ' -f2)"
show "js $js_args lease list"
show "js $js_args lease release $id"
show "js $js_args lease list"
