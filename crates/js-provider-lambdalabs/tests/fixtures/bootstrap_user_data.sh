#!/bin/bash
# js-agent instance bootstrap, rendered by js-provider-lambdalabs.
# It holds no token and no key. The join token is delivered later, into the
# token file named below; the agent's unit starts only once that file exists.
set -euo pipefail
umask 022

AGENT_URL='https://github.com/example-org/jobscheduler/releases/download/v0.1.0/js-agent-x86_64-linux'
AGENT_SHA256='0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef'
SERVER_URL='https://lease.example.org:8443'
POOL='lambda-gpu'
CA_CERT='/etc/js-agent/ca.pem'
TOKEN_FILE='/etc/js-agent/token'
STATE_ROOT='/var/lib/js-agent/state'
SCRATCH_ROOT='/mnt/scratch/js-agent'
CONTAINER_BIN='/usr/bin/docker'
AGENT_BIN=/usr/local/bin/js-agent
UNIT_DIR=/etc/systemd/system

fail() {
    echo "js-agent bootstrap: $*" >&2
    exit 1
}

[ "$(id -u)" -eq 0 ] || fail "must run as root"

# The image is expected to carry the driver, the container engine and the NVIDIA
# container toolkit. Nothing is installed here: a missing piece ends the bootstrap.
for tool in curl sha256sum systemctl stat mktemp cmp install; do
    command -v "$tool" >/dev/null 2>&1 || fail "$tool is not installed"
done
command -v nvidia-smi >/dev/null 2>&1 || fail "nvidia-smi is not installed: there is no NVIDIA driver"
nvidia-smi >/dev/null 2>&1 || fail "nvidia-smi fails: the GPU driver does not work"
[ -x "$CONTAINER_BIN" ] || fail "there is no container engine at $CONTAINER_BIN"
command -v nvidia-ctk >/dev/null 2>&1 || command -v nvidia-container-runtime >/dev/null 2>&1 ||
    fail "the NVIDIA container toolkit is not installed (neither nvidia-ctk nor nvidia-container-runtime)"
engine_ready=no
engine_info=
for _ in $(seq 1 30); do
    if engine_info=$("$CONTAINER_BIN" info 2>&1); then
        engine_ready=yes
        break
    fi
    sleep 2
done
[ "$engine_ready" = yes ] || fail "the container engine does not answer: $CONTAINER_BIN info fails"
case "$engine_info" in
*nvidia*) ;;
*) fail "the container engine has no nvidia runtime: configure the NVIDIA container toolkit for it" ;;
esac

# private_dir <path> <mode bits that must be clear>: creates the directory when it is
# missing (mode 0700) and refuses one that exists with another owner or an open mode.
# It never changes the mode of a directory that exists.
private_dir() {
    mkdir -p -m 0700 "$1"
    [ "$(stat -c %u "$1")" = 0 ] || fail "$1 is not owned by root"
    [ $((0$(stat -c %a "$1") & $2)) -eq 0 ] || fail "$1 is open to other users (mode $(stat -c %a "$1"))"
}
private_dir "$STATE_ROOT" 077
private_dir "$SCRATCH_ROOT" 077
private_dir "$(dirname "$TOKEN_FILE")" 022
private_dir "$(dirname "$CA_CERT")" 022
if [ "$(stat -c %d "$STATE_ROOT")" = "$(stat -c %d "$SCRATCH_ROOT")" ]; then
    echo "js-agent bootstrap: warning: $STATE_ROOT and $SCRATCH_ROOT are on one file system; js-agent refuses a state root that shares a device with the scratch root" >&2
fi

# The agent: kept when it is already the expected build, otherwise downloaded and
# checked before it is moved into place.
installed=$(sha256sum "$AGENT_BIN" 2>/dev/null | cut -d' ' -f1 || true)
if [ "$installed" = "$AGENT_SHA256" ]; then
    echo "js-agent bootstrap: $AGENT_BIN is already the expected build"
else
    tmp=$(mktemp "$AGENT_BIN.XXXXXX")
    trap 'rm -f "$tmp"' EXIT
    curl --fail --silent --show-error --location --proto '=https' --proto-redir '=https' --tlsv1.2 \
        --connect-timeout 20 --max-time 600 --retry 3 --output "$tmp" "$AGENT_URL" ||
        fail "the download of $AGENT_URL failed"
    got=$(sha256sum "$tmp" | cut -d' ' -f1)
    [ "$got" = "$AGENT_SHA256" ] ||
        fail "sha256 mismatch for $AGENT_URL: expected $AGENT_SHA256, got $got; nothing was installed"
    chmod 0755 "$tmp"
    mv -f "$tmp" "$AGENT_BIN"
    trap - EXIT
fi

# put <path>: replaces the file with standard input when the two differ.
put() {
    local new
    new=$(mktemp "$1.XXXXXX")
    cat >"$new"
    chmod 0644 "$new"
    if cmp -s "$new" "$1"; then
        rm -f "$new"
    else
        mv -f "$new" "$1"
    fi
}

# The service does not start without the token file and the CA file; the path unit
# starts it when the token file appears.
put "$UNIT_DIR/js-agent.service" <<EOF
[Unit]
Description=js-agent: offers this machine to the lease server
After=network-online.target docker.service
Wants=network-online.target
ConditionPathExists=$TOKEN_FILE
ConditionPathExists=$CA_CERT

[Service]
Type=simple
ExecStart=$AGENT_BIN --server $SERVER_URL --pool $POOL --ca-cert $CA_CERT --token-file $TOKEN_FILE --container-bin $CONTAINER_BIN --state-root $STATE_ROOT --scratch-root $SCRATCH_ROOT
Restart=on-failure
RestartSec=10
EOF

put "$UNIT_DIR/js-agent.path" <<EOF
[Unit]
Description=starts js-agent once its join token is delivered

[Path]
PathExists=$TOKEN_FILE
Unit=js-agent.service

[Install]
WantedBy=multi-user.target
EOF

systemctl daemon-reload
systemctl enable --now js-agent.path
echo "js-agent bootstrap: done; js-agent starts when $TOKEN_FILE exists"
